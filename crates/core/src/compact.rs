//! 上下文自动压缩：估算 token 用量，超限时调用 LLM 生成摘要替换旧消息。

use anyhow::Result;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, Message, Role, ToolDefinition, ToolResultContent},
};

use crate::session::Session;

/// 压缩时保留给"最近尾部原文"的 token 预算。
///
/// 旧实现用固定条数（6 条），在两种场景下都是错的：工具密集
/// 回合里 6 条可能就是一整个巨型 tool_result（保留太多，压不下去）；纯对话回合
/// 里 6 条可能只有 1K token（保留太少，压完仍超标，才需要连跑 3 轮 pass）。
/// 改为随窗口伸缩的 token 预算后两种场景都对。
///
/// 比例与量级参照 Goose 的 `compute_tool_call_cutoff`（`3 × limit / 20000`
/// clamp 到 10–500）——保留量必须随窗口线性伸缩，否则小窗口被压垮、
/// 大窗口浪费。
///
/// **上限为什么从 32K 提到 128K**：旧上限在 1M 窗口下把 900K 历史压成
/// 约 32K + 16K 摘要，压缩比 18:1，实测后果是「压缩后模型明显失忆、重新
/// grep 刚 grep 过的目录、重新读刚读过的文件」——省下的 token 又以工具
/// 往返的形式花回去，还多付一次压缩的 LLM 往返。压缩保留量的目的不是
/// 「压到最小」，而是「压到阈值以下且不再丢现场」：窗口越大，能留的现场
/// 就该越多。200K 窗口仍是 25K（行为与旧版逐字节相同）。
fn compact_keep_budget(context_window: u32) -> u32 {
    (context_window / 8).clamp(8_000, 128_000)
}

/// 低于这个条数就不再压：没有足够的历史值得花一次 LLM 往返。
const COMPACT_MIN_MESSAGES: usize = 3;

/// `compact_session` 的两种「已无可压缩」信号。
///
/// 与真正的失败（LLM 调用报错、摘要为空）区分开，供
/// `compact_session_until_fit` 决定是把控制权还给调用方还是上报错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactSkip {
    /// 消息数不足（`< COMPACT_MIN_MESSAGES`），再压没有意义。
    TooFewMessages(usize),
    /// 找不到不拆散 tool_use / tool_result 配对的安全边界。
    NoSafeBoundary,
}

impl std::fmt::Display for CompactSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactSkip::TooFewMessages(n) => write!(f, "消息数量过少（{n}条），无需压缩"),
            CompactSkip::NoSafeBoundary => write!(f, "找不到安全的压缩边界，暂不压缩"),
        }
    }
}

impl std::error::Error for CompactSkip {}

/// 从 `anyhow::Error` 里取回 [`CompactSkip`]（若它是 compact 的良性跳过）。
pub fn compact_skip_of(error: &anyhow::Error) -> Option<CompactSkip> {
    error.downcast_ref::<CompactSkip>().copied()
}

/// 压缩触发时预留的 buffer（不参与压缩的窗口尾部空间）。
///
/// 必须同时满足两个约束，缺一不可：
///
/// 1. **不小于本轮输出预留 `max_tokens`**。压缩只是把历史压到阈值以下，真正
///    决定这次请求会不会撞穿窗口的是 `阈值 + max_tokens`。旧公式固定封顶
///    40K，在 `max_tokens = 64000` 的配置下会出现
///    `160K(阈值) + 64K(输出) > 200K(窗口)` 的**结构性必 400**。
///    这与 Claude Code 官方文档对 `CLAUDE_CODE_MAX_OUTPUT_TOKENS` 的说明
///    一致：调大输出上限会直接吃掉自动压缩前的可用窗口。
/// 2. **随窗口线性伸缩**。旧公式 `min(40K, cw/5)` 在 1M 窗口下只留 4%
///    余量（阈值 96%），窗口越大反而触发越晚。改为 `cw / 10` 后
///    200K → 90%、1M → 90%，与 Codex CLI 的 `window × 0.9` 对齐。
///
/// 小窗口靠 `max_tokens + 8_192` 的加法项兜底，不会被比例项压到过激。
pub fn compact_trigger_buffer(context_window: u32, max_output_tokens: u32) -> u32 {
    max_output_tokens
        .saturating_add(8_192)
        .max(context_window / 10)
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub messages_removed: usize,
    /// 压缩后保留原文的尾部消息条数（按 token 预算而非固定条数决定）。
    pub messages_kept: usize,
    pub tokens_saved_estimate: u32,
}

/// 一次模型请求的上下文占用分解。
///
/// 存在的意义是**消灭两套口径**：旧实现里压缩决策用
/// [`estimate_request_tokens`]（含 system + tools + 输出预留），而状态栏用
/// [`estimate_tokens`]（只有 messages），两者能差出 1 万+ token，在 200K
/// 窗口下就是 6 个以上百分点的系统性低报——进度条和压缩时机对不上号。
/// 现在压缩决策、展示、`/context` 面板全部共用这一个结构体。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextAudit {
    /// system prompt 合计（= `system_stable_tokens` + `system_volatile_tokens`）。
    pub system_tokens: u32,
    /// system prompt 的 **stable 段**（主提示 + env + 记忆快照 + 项目规约祖先链）。
    ///
    /// 单独拆出来是因为它决定 Anthropic 缓存前缀的一半：stable 段字节不变，
    /// `tools + stable` 整段命中缓存。另一段（volatile）一变，**messages 断点**
    /// 失效（system 位于 messages 之前，而该断点覆盖 tools + system + messages
    /// 前缀），整段历史全价重算——所以「哪一段在变」比「system 一共多大」
    /// 更有诊断价值。
    pub system_stable_tokens: u32,
    /// system prompt 的 **volatile 段**（模型兼容 suffix → 工具可用性名单 →
    /// 语言提醒）。
    ///
    /// 子目录规约 reminder 曾在这里累积，导致每命中一个新目录就整段历史
    /// 全价重算；它已改走历史注入，因此这一段现在是**每轮结构相同**的。
    pub system_volatile_tokens: u32,
    /// 本次实际发送的工具定义（已扣除 lazy tool 折叠的部分）。
    pub tool_schema_tokens: u32,
    /// 本次请求**实际发送**的工具个数。
    ///
    /// 与 `tool_schema_tokens` 分开是因为两者回答不同的问题：token 回答
    /// 「多贵」，个数回答「目录多大」。后者决定 tools 段的缓存前缀规模，
    /// 而 Anthropic 明确规定会话中途增删 tools 会作废 tools 缓存**及其后
    /// 的全部内容**——所以这个数字只该单调不减。
    pub tool_count: u32,
    /// 会话历史（messages）。
    pub history_tokens: u32,
    /// 输出预留 `max_tokens`。
    pub output_reserve: u32,
    /// 每条消息的包装开销 + 协议级固定开销。
    pub framing_tokens: u32,
    /// 历史里「超出最近 K 个工具调用」的 tool_result 合计。**只统计、
    /// 不删除**——第 3 批的 tool_result context editing 才真的回收它；
    /// 现在先让 `/context` 能如实告诉用户有多少是"可回收"的。
    pub reclaimable_tool_result_tokens: u32,
    /// 本次请求**按多少窗口做的判定**（`Session::effective_context_window`
    /// 的结果；通常等于配置值，端点拒绝过才会更小）。`0` = 未记录。
    ///
    /// 为什么放在度量结构里而不是另开一个字段：占用率、阈值、`/context`
    /// 面板必须与**同一次判定**同源。端点把一份 256K 窗口的请求拒了之后，
    /// 有效窗口会收紧到远小于配置值——此时若面板还按配置值算出「占用率
    /// 30% / 阈值 900K」，用户看到的就是一个永远触发不到的阈值，而这正是
    /// v0.2.26 消灭过的「同一个事实两套口径」换个地方复发。
    pub window: u32,
}

/// 保留全文的最近工具调用数，与 Anthropic `clear_tool_uses_20250919`
/// 的默认 `keep = 3` 对齐。
pub const RECLAIM_KEEP_RECENT_TOOL_CALLS: usize = 3;

impl ContextAudit {
    /// 本次请求实际占用的上下文（不含可回收项——那部分目前仍真实占用）。
    pub fn total(&self) -> u32 {
        self.system_tokens
            .saturating_add(self.tool_schema_tokens)
            .saturating_add(self.history_tokens)
            .saturating_add(self.output_reserve)
            .saturating_add(self.framing_tokens)
    }

    /// 用**实测校准系数**修正后的上下文占用。
    ///
    /// 只修正 prompt 部分（system / 工具 schema / 历史 / framing），
    /// `output_reserve` 是配置里的 `max_tokens`，不是猜出来的，不能被缩放。
    ///
    /// `ratio` 来自 [`crate::session::Session::input_estimate_ratio`]：每轮
    /// 请求成功后用 provider 上报的真实 prompt token 反推「估算偏低/偏高多少」。
    /// 为什么需要它：`estimate_text_tokens` 是字符启发式，对代码 / JSON /
    /// 多语言混排的偏差可达数十个百分点，而压缩阈值是一个**硬比例**——
    /// 估算偏低 30% 就意味着在 1M 窗口上要等到真实 910K 才触发压缩，
    /// 撞 400 的概率与「明明还早却已压缩」的误触发都在这个偏差里。
    pub fn calibrated_total(&self, ratio: f32) -> u32 {
        let prompt = self.total().saturating_sub(self.output_reserve);
        let scaled = (prompt as f32 * ratio.clamp(0.5, 2.0)).round() as u32;
        scaled.saturating_add(self.output_reserve)
    }

    pub fn from_request(
        system: &wyj_api::SystemPrompt<'_>,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_output_tokens: u32,
    ) -> Self {
        const REQUEST_OVERHEAD_TOKENS: u32 = 64;
        const MESSAGE_OVERHEAD_TOKENS: u32 = 4;

        let system_stable_tokens = estimate_text_tokens(system.stable) as u32;
        let system_volatile_tokens = estimate_text_tokens(system.volatile) as u32;
        Self {
            system_tokens: estimate_system_tokens(system) as u32,
            system_stable_tokens,
            system_volatile_tokens,
            tool_schema_tokens: estimate_tools_tokens(tools),
            tool_count: tools.len() as u32,
            history_tokens: estimate_tokens(messages),
            output_reserve: max_output_tokens,
            framing_tokens: (messages.len() as u32)
                .saturating_mul(MESSAGE_OVERHEAD_TOKENS)
                .saturating_add(REQUEST_OVERHEAD_TOKENS),
            reclaimable_tool_result_tokens: reclaimable_tool_result_tokens(messages),
            window: 0,
        }
    }

    /// 记录本次判定使用的**有效窗口**（[`crate::session::Session::effective_context_window`]）。
    ///
    /// 做成 builder 而不是 `from_request` 的第五个参数：`from_request` 的
    /// 四项是「按内容算出来的量」，窗口是「按运行环境定的量」，两者的生命周期
    /// 不同——测试与若干调用点只关心前者，强行让它们传窗口只会得到一堆无意义
    /// 的 0。
    pub fn with_window(mut self, window: u32) -> Self {
        self.window = window;
        self
    }

    /// 取本次判定实际使用的窗口，`fallback` 用于「本会话还没发过请求」
    /// （`window == 0`）的场合。
    pub fn effective_window(&self, fallback: u32) -> u32 {
        if self.window > 0 {
            self.window
        } else {
            fallback
        }
    }

    /// 面板要显示的 system 明细 `(stable, volatile)`。
    ///
    /// `None` = **拆分信息缺失**，调用方应回退到 `system_tokens` 合计。两种
    /// 形态必须区分开：否则面板会显示 `stable = 0 / volatile = 0` 却仍报一个
    /// 非零的 system 合计。
    ///
    /// `ContextAudit` 是**进程内**状态、不随 `SessionFile` 落盘，所以实际
    /// 触发回退的是「本会话还没发过请求」（默认值全 0）与「system 本来就是
    /// 空的」这两种情形，而不是「老会话缺字段」。
    pub fn system_split(&self) -> Option<(u32, u32)> {
        if self.system_stable_tokens + self.system_volatile_tokens > 0 {
            Some((self.system_stable_tokens, self.system_volatile_tokens))
        } else {
            None
        }
    }

    /// 面板上**各明细行之和**，必须等于 [`Self::total`]。
    ///
    /// 存在的意义是让「合计」那行可验：用户把各行加起来必须得到同一个数，
    /// 否则第一反应是面板算错了。拆分形态与回退形态都要对上，因此这个方法
    /// 与面板的取数路径共用 [`Self::system_split`]——两处各写一半就会在
    /// 「stable + volatile 恰好为 0」这种边界上分叉。
    pub fn parts_sum(&self) -> u32 {
        let system = match self.system_split() {
            Some((stable, volatile)) => stable.saturating_add(volatile),
            None => self.system_tokens,
        };
        system
            .saturating_add(self.tool_schema_tokens)
            .saturating_add(self.history_tokens)
            .saturating_add(self.output_reserve)
            .saturating_add(self.framing_tokens)
    }
}

/// system prompt 的 token 估算，**不拼接 stable + volatile**。
///
/// 旧实现调 `system.combined()`：主提示词本身就有几万 token，一次工具轮里
/// `from_request` 最多被调用 10 次（context editing 的 measure 闭包 + 压缩判定
/// + 阈值判定），等于每轮把整个系统提示重新分配、逐字符扫 10 遍。
///
/// 分段估算与拼接后估算**完全等价**：`estimate_text_tokens` 是逐字符的纯
/// 函数，拼接只多出一个 `\n\n`（非 CJK，贡献 0）。
pub fn estimate_system_tokens(system: &wyj_api::SystemPrompt<'_>) -> usize {
    estimate_text_tokens(system.stable) + estimate_text_tokens(system.volatile)
}

/// 工具定义的 token 估算，按**工具名缓存**。
///
/// 每个工具的 `input_schema.to_string()` 是一次完整 JSON 序列化——本仓库
/// 工具集（未含 MCP）就能到几万字符，而 `from_request` 每轮最多调 10 次、
/// 每次遍历 `tools.len()` 个工具，几百次重复序列化同样的字符串。
///
/// 缓存键用工具名：同一进程内 schema 变了（`Agent::refresh_tool_definitions`、
/// MCP 重连）会重新算并覆盖旧值，代价只是「用新值覆盖旧值」，不会有陈旧读。
pub fn estimate_tools_tokens(tools: &[ToolDefinition]) -> u32 {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    tools.iter().fold(0u32, |total, tool| {
        let tokens = match cache.get(&tool.name) {
            Some(tokens) => *tokens,
            None => {
                let schema = tool.input_schema.to_string();
                let tokens = (estimate_text_tokens(&tool.name)
                    + estimate_text_tokens(&tool.description)
                    + estimate_text_tokens(&schema)) as u32;
                cache.insert(tool.name.clone(), tokens);
                tokens
            }
        };
        total.saturating_add(tokens)
    })
}

/// 历史中「超出最近 [`RECLAIM_KEEP_RECENT_TOOL_CALLS`] 个工具调用」的
/// tool_result 合计 token。
///
/// 从后往前数工具结果，跳过保留窗口内的，剩下的按出现顺序累加——与
/// `clear_tool_uses_20250919` "按时间顺序清最旧的" 语义一致。
pub fn reclaimable_tool_result_tokens(messages: &[Message]) -> u32 {
    let mut seen = 0usize;
    let mut reclaimable = 0usize;
    for block in messages.iter().rev().flat_map(|m| m.content.iter()) {
        if let ContentBlock::ToolResult { content, .. } = block {
            seen += 1;
            if seen > RECLAIM_KEEP_RECENT_TOOL_CALLS {
                reclaimable += match content {
                    ToolResultContent::Text(t) => estimate_text_tokens(t),
                    ToolResultContent::Parts(parts) => parts
                        .iter()
                        .map(|p| match p {
                            wyj_api::types::ToolResultPart::Text { text } => {
                                estimate_text_tokens(text)
                            }
                            wyj_api::types::ToolResultPart::Image { data, .. } => {
                                estimate_image_tokens(data.len())
                            }
                        })
                        .sum(),
                    ToolResultContent::Blocks(v) => {
                        v.iter().map(|x| estimate_text_tokens(&x.to_string())).sum()
                    }
                };
            }
        }
    }
    reclaimable as u32
}

/// 估算一整次模型请求占用的上下文，而不是只看会话历史。
///
/// 保留为薄封装：与压缩决策同源，见 [`ContextAudit`]。
pub fn estimate_request_tokens(
    system: &wyj_api::SystemPrompt<'_>,
    messages: &[Message],
    tools: &[ToolDefinition],
    max_output_tokens: u32,
) -> u32 {
    ContextAudit::from_request(system, messages, tools, max_output_tokens).total()
}

/// 粗略估算消息列表的 token 数。
///
/// 改进点（对比旧版 `chars/3`）：旧版对中文严重低估（中文约 1-2 token/字，
/// `/3` 只估 0.33，偏低 3-6 倍），导致压缩触发过晚、真实上下文可能溢出。
///
/// 当前启发式：CJK 字符按 1.5 token/字，其余按 0.33 token/字（≈3 字符/token）。
/// 旧版 0.25 (4 chars/token) 对含代码/JSON 的混合内容偏低约 2 倍——Claude BPE
/// 对 `{` `}` `(` `)` 关键字等会单独成 token，dev 项目里 tool result / Read 输出
/// 大多是这类内容，会让估算 < 真实 → 压缩触发过晚 → 撞穿上下文窗口。
/// 0.33 在英文 prose 略高估（安全方向）、代码 JSON 更贴近真实，混合场景触发更准。
pub fn estimate_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(estimate_block_tokens)
        .sum::<usize>() as u32
}

/// 单个 `ContentBlock` 的 token 估算。`context_edit` 需要按块度量
/// 「清理这一条省了多少」，所以从 `estimate_tokens` 里拆出这一层。
pub fn estimate_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => estimate_text_tokens(text),
        ContentBlock::ToolUse { name, input, .. } => {
            estimate_text_tokens(name) + estimate_text_tokens(&input.to_string())
        }
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => estimate_text_tokens(t),
            ToolResultContent::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    wyj_api::types::ToolResultPart::Text { text } => estimate_text_tokens(text),
                    wyj_api::types::ToolResultPart::Image { data, .. } => {
                        estimate_image_tokens(data.len())
                    }
                })
                .sum(),
            ToolResultContent::Blocks(v) => {
                v.iter().map(|x| estimate_text_tokens(&x.to_string())).sum()
            }
        },
        ContentBlock::Image { data, .. } => estimate_image_tokens(data.len()),
        // thinking 输出不占后续请求的 input（回传不计费），但估算宁多勿少
        ContentBlock::Thinking { thinking, .. } => estimate_text_tokens(thinking),
        ContentBlock::RedactedThinking { data } => data.len() / 4,
    }
}

/// 图片 token 估算：Anthropic 按约 像素数/750 计 token。无尺寸信息时用解码
/// 后字节数近似（base64 长度 × 3/4 ÷ 750），1600 封顶（API 对大图会缩放）。
/// 旧公式 `data.len()/3` 对大图高估约百倍，会导致压缩过早触发。
fn estimate_image_tokens(b64_len: usize) -> usize {
    (b64_len * 3 / 4 / 750).min(1600)
}

/// 启发式 token 估算：CJK 字符按 1.5 token/字，其余按 0.33 token/字
/// （≈3 字符/token，对代码/JSON 内容更贴近 Claude BPE 真实值）。
pub fn estimate_text_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (cjk * 3 / 2) + (other / 3)
}

/// 浮点版文本 token 估算，供**按增量累加**的调用方使用（如 TUI 的流式速率）。
///
/// 不能拿 [`estimate_text_tokens`] 逐 delta 累加：那条路径按片段做整数除法，
/// 单个 ASCII 字符是 `1 / 3 = 0`，一帧几十个小 delta 加完仍是 0——整段消息的
/// 粗估没有这个问题（片段足够长），所以整数版保持原语义不动。
pub fn estimate_text_tokens_f64(text: &str) -> f64 {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk as f64 * 1.5 + other as f64 / 3.0
}

/// 判断字符是否为 CJK 统一表意文字或常见全角字符（中日韩）。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F |   // CJK 标点符号
        0x3040..=0x309F |   // 平假名
        0x30A0..=0x30FF |   // 片假名
        0x3400..=0x4DBF |   // CJK 扩展 A
        0x4E00..=0x9FFF |   // CJK 统一表意文字
        0xF900..=0xFAFF |   // CJK 兼容表意文字
        0xFF00..=0xFFEF |   // 全角 ASCII / 半角片假名
        0x20000..=0x2A6DF | // CJK 扩展 B
        0x2A700..=0x2B73F | // CJK 扩展 C
        0x2B740..=0x2B81F | // CJK 扩展 D
        0x2B820..=0x2CEAF   // CJK 扩展 E
    )
}

/// 压缩调用可复用的主对话前缀。
///
/// **为什么值得单独做一个结构体**：压缩请求若从零构造（`COMPACT_SYSTEM` +
/// 一条塞满整段历史的 user 消息），前缀与主对话毫无关系，provider 的前缀缓存
/// 一次都命中不了——而压缩恰恰发生在历史最长、最贵的那一刻。DeepSeek 的
/// 缓存命中价是 miss 的 1/50，Anthropic 是 1/10，一次 180K 的压缩差价就是
/// 数量级的。
///
/// 复用条件是「压缩请求的前缀与主对话**逐字节相同**」：
///   * `system` 必须是本轮真实发出去的 stable 段（记忆快照 / 规约链都在里面，
///     换成 `COMPACT_SYSTEM` 就从第一个 token 起分叉）；
///   * `tools` 必须是本轮真实发出去的 schema 列表（工具段在缓存前缀里排在
///     历史**之前**，少一个工具就整段失效）。
///
/// 消息前缀由 [`compact_session`] 自己从 `session.messages[..keep_from]` 切，
/// 不放进本结构体——否则调用方要同时持有 `&session.messages` 和 `&mut session`。
#[derive(Clone, Copy)]
pub struct CompactPrefix<'a> {
    pub system: wyj_api::SystemPrompt<'a>,
    pub tools: &'a [ToolDefinition],
}

/// 压缩会话：调用 LLM 生成摘要，替换旧消息，保留最近若干条。
///
/// `prefix` 为 `None` 时退回「独立请求」形态（`COMPACT_SYSTEM` + 文本化历史），
/// 行为与 v0.3.8 完全一致；给出 `Some` 时走前缀复用路径。
pub async fn compact_session(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    prefix: Option<CompactPrefix<'_>>,
) -> Result<CompactResult> {
    let total = session.messages.len();
    if total < COMPACT_MIN_MESSAGES {
        return Err(CompactSkip::TooFewMessages(total).into());
    }

    let keep_from = safe_keep_from_budget(&session.messages, compact_keep_budget(context_window))
        .ok_or(CompactSkip::NoSafeBoundary)?;

    // 工具密集型单回合的消息序列通常只有首条是真实 user 消息：
    // user → assistant(tool_use) → user(tool_result) → ...。此时安全边界会回退
    // 到 0。旧逻辑仍对空前缀生成摘要，再追加一对 user/assistant 确认消息，历史
    // 不但没变短，还会额外增长两条消息。退化为单条 user 摘要可保留角色合法性，
    // 也让下一次请求从一个可继续的 assistant 回合开始。
    let reset_entire_session = keep_from == 0;
    let (to_compact, to_keep) = if reset_entire_session {
        (&session.messages[..], Vec::new())
    } else {
        (
            &session.messages[..keep_from],
            session.messages[keep_from..].to_vec(),
        )
    };
    let before_tokens = estimate_tokens(&session.messages);
    let messages_removed = to_compact.len();
    let messages_kept = to_keep.len();

    // 摘要 token 上限：取上下文窗口的 1/10，但不超过 16000
    let summary_max_tokens = (context_window / 10).min(16_000);

    let result = match prefix {
        Some(prefix) => {
            // ── 前缀复用路径 ──
            // 请求 = 「主对话前缀」+ 尾部一条指令。前缀逐字节等于本轮真实请求
            // 的开头，因此 provider 的前缀缓存能命中到指令之前；指令本身按
            // 全价计，但它只有几百 token。
            //
            // 两个必须守住的点：
            //   1. `messages` 直接用 `session.messages[..keep_from]` 的切片，
            //      不重新序列化、不做任何改写——任何一处改写都会让前缀失配。
            //   2. 尾部指令走一条**新的 user 消息**，不合并进最后一条已有消息。
            //      合并会改写前缀的最后一个元素，缓存命中点随之后退到它之前。
            //
            // `reset_entire_session` 时前缀为空（`keep_from == 0`），复用没有
            // 意义但也没有害处：请求退化成「一条 user 指令」，与旧路径等价。
            let mut req: Vec<Message> = to_compact.to_vec();
            req.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: crate::prompts::compact_instructions().to_string(),
                }],
            });
            provider
                .complete(
                    &prefix.system,
                    &req,
                    prefix.tools,
                    &wyj_api::provider::RequestOptions::text_only(summary_max_tokens),
                )
                .await?
        }
        None => {
            // ── 独立请求路径（无前缀可复用） ──
            let conv_text = messages_to_text(to_compact);
            let prompt = crate::prompts::compact_prompt(&conv_text);
            let req = vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: prompt }],
            }];
            provider
                .complete(
                    &wyj_api::SystemPrompt::stable_only(crate::prompts::COMPACT_SYSTEM),
                    &req,
                    &[],
                    &wyj_api::provider::RequestOptions::text_only(summary_max_tokens),
                )
                .await?
        }
    };

    // 压缩本身是一次真实的 LLM 往返，必须计入用量。旧实现直接丢弃
    // `CompletionResult` 的 usage 字段（也不累加 `api_calls`），导致
    // `/cost` 与 `WYJ_STATS_JSON` 系统性低估真实花费，压缩越频繁偏差越大。
    // 计入时机放在空摘要检查之前：即使摘要生成失败，这轮 token 也已经花掉了。
    session.add_usage(result.input_tokens, result.output_tokens);
    session.add_cache_usage(
        result.cache_read_input_tokens,
        result.cache_creation_input_tokens,
    );
    session.api_calls += 1;

    let summary: String = result
        .content
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text { text } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let summary = summary.trim().to_string();
    if summary.is_empty() {
        anyhow::bail!("摘要生成失败：模型返回空输出");
    }

    if reset_entire_session {
        session.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: crate::prompts::compact_summary_message(messages_removed, &summary),
            }],
        }];
    } else {
        session.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: crate::prompts::compact_summary_message(messages_removed, &summary),
            }],
        }];
        // 确认语只在**保留段以 user 开头**时才需要：那时摘要(user) 后面直接
        // 跟 user 会破坏角色交替，得插一条 assistant 隔开。
        //
        // 保留段以 assistant(tool_use) 开头时（P0-1 新增的工具轮起点边界）
        // 绝不能插：那会变成 `summary(user) → ack(assistant) → tool_use
        // (assistant)` 两条连续 assistant，且 ack 与 tool_use 之间没有任何
        // user 内容——provider 侧既违反交替，`tool_result` 也不再紧跟其
        // `tool_use`。此时 `summary(user) → assistant(tool_use)` 本身就是
        // 合法的一轮。
        let tail_starts_with_assistant = to_keep
            .first()
            .is_some_and(|m| matches!(m.role, Role::Assistant));
        if !tail_starts_with_assistant {
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: crate::prompts::COMPACT_ACK.to_string(),
                }],
            });
        }
        session.messages.extend(to_keep);
    }
    // 压缩是「整段历史被替换」，增量日志必须收到一次全量快照——否则重放时
    // 会把压缩前的旧消息和压缩后的新消息拼在一起，拼出一个从未存在过的历史。
    session.emit_replaced();
    let tokens_saved = before_tokens.saturating_sub(estimate_tokens(&session.messages));

    Ok(CompactResult {
        messages_removed,
        messages_kept,
        tokens_saved_estimate: tokens_saved,
    })
}

/// 单次 `compact_session` 在工具密集型单回合里可能只腾出几十条消息的
/// 摘要体积；遇到『估算一直 > threshold』的死循环（heuristic 偏低、
/// 上一轮摘要自身偏长、reasoning 模型一轮回巨长）时,裸发会撞穿模型
/// 上下文窗口。本函数最多连续跑 [`MAX_COMPACT_PASSES`] 次,直到
/// `estimated_tokens <= threshold` 或压缩本身失败（消息数太少 / 找不到
/// 安全边界 / 模型摘要失败）,最终总会返回最后一次的 `CompactResult`
/// 或最后一次错误;调用方在只剩一两条工具结果消息、压缩已退化到极限
/// 时通过 fallthrough 把控制权还给 stream,由 `truncate_messages` 兜底
/// 截断单块超长内容。
///
/// 故意不引入指数退避或 sleep——compact 是 LLM round-trip,本来就要等,
/// 加 sleep 只会让失败时回退更慢。
///
/// `prefix` 透传给每一次 pass。**只在第一次 pass 有价值**（第二次起历史已被
/// 上一次的摘要替换，前缀自然不再与主对话一致），但仍然照传：多传一个
/// 已经失配的前缀不会让缓存变差，只会回到「全 miss」这个基线。
///
/// **`measure` 必须与调用方的进入条件同口径**。历史上这里用
/// [`estimate_tokens`]（messages-only）判断收敛，而进入条件是请求级估算
/// （`ContextAudit::total()`，含 system + 工具 schema + 输出预留）。两者能差
/// 4~5 万 token，于是「进入时判定超标 → 压一轮 → 用低口径判定还是超标 →
/// 再压一轮」空跑第 2、3 轮，而每一轮都会把上一轮刚保留的尾部原文再摘要
/// 一次——压缩次数多、现场丢得多，正是长会话「越压越失忆」的放大器之一。
///
/// **停滞即停手**：一轮压缩至少要把测量值降下来 5%，否则视为「压不动」
/// （只剩摘要 + 少量保留原文时必然如此）。没有这条，`MAX_COMPACT_PASSES`
/// 会对着同一段压不动的历史连开三次 LLM 往返，且每次都会再丢一点原文。
pub async fn compact_session_until_fit(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    threshold: u32,
    prefix: Option<CompactPrefix<'_>>,
    measure: impl Fn(&[Message]) -> u32,
) -> CompactUntilFitOutcome {
    const MAX_COMPACT_PASSES: usize = 3;
    /// 一轮压缩至少要把测量值降这么多比例，否则直接停手。
    const MIN_PROGRESS_RATIO: f64 = 0.05;

    let mut last_result: Option<Result<CompactResult>> = None;
    let mut before = measure(&session.messages);
    for pass in 0..MAX_COMPACT_PASSES {
        let current = measure(&session.messages);
        if current <= threshold {
            return CompactUntilFitOutcome {
                final_estimated: current,
                passes: pass,
                last_result: last_result.and_then(|r| r.ok()),
                last_error: None,
            };
        }
        match compact_session(session, provider, context_window, prefix).await {
            Ok(result) => {
                last_result = Some(Ok(result));
                let after = measure(&session.messages);
                let required = (before as f64 * (1.0 - MIN_PROGRESS_RATIO)).ceil() as u32;
                if after >= required {
                    tracing::debug!(
                        before,
                        after,
                        required,
                        pass,
                        "压缩进展不足 5%，停止继续压缩（剩下的只能交给单块截断兜底）"
                    );
                    return CompactUntilFitOutcome {
                        final_estimated: after,
                        passes: pass + 1,
                        last_result: last_result.and_then(|r| r.ok()),
                        last_error: None,
                    };
                }
                before = after;
            }
            Err(error) => {
                // 「消息数过少」/「找不到安全边界」是『已无可压缩』的合法
                // 信号——常见于连跑多轮 compact 后只剩 1~2 条 user/assistant
                // 配对,此时本来就压不动了;把控制权还给 caller,让它继续
                // stream（runtime `truncate_messages` 仍会兜底截断单块超长
                // 内容）。其它错误(LLM 调用失败 / 摘要为空) 才算真失败。
                if let Some(skip) = compact_skip_of(&error) {
                    tracing::debug!(
                        ?skip,
                        "compact 已无可压缩空间，把控制权交回调用方由 truncate 兜底"
                    );
                    return CompactUntilFitOutcome {
                        final_estimated: measure(&session.messages),
                        passes: pass,
                        last_result: last_result.and_then(|r| r.ok()),
                        last_error: None,
                    };
                }
                return CompactUntilFitOutcome {
                    final_estimated: measure(&session.messages),
                    passes: pass,
                    last_result: None,
                    last_error: Some(error.to_string()),
                };
            }
        }
    }
    // 直接用最新测量值，不能用进入时的 `estimated` 去 cap——那是压缩**前**的
    // 快照，用它取 min 会让「压完仍超标」的返回值假装已经降下来了。
    CompactUntilFitOutcome {
        final_estimated: measure(&session.messages),
        passes: MAX_COMPACT_PASSES,
        last_result: last_result.and_then(|r| r.ok()),
        last_error: None,
    }
}

/// `compact_session_until_fit` 的返回值。`passes == 0` 表示首次估算就
/// 已经在阈值内、根本没有跑过 compact；`last_error` 是最后一次压缩错误
/// （如『消息数太少』),调用方据此判断要不要继续 stream。
#[derive(Debug, Clone)]
pub struct CompactUntilFitOutcome {
    pub final_estimated: u32,
    pub passes: usize,
    pub last_result: Option<CompactResult>,
    pub last_error: Option<String>,
}

/// 判断消息是否为"真实用户发言"边界（而非工具结果回传）：
/// 只有此类消息可以安全作为压缩截断点，否则会拆散 tool_use/tool_result 配对。
fn is_user_turn_boundary(msg: &Message) -> bool {
    matches!(msg.role, Role::User)
        && !msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// 判断消息是否是一个**完整的工具轮起点**，可以作为压缩截断点。
///
/// 为什么需要这一类边界：一次提交跑几十轮工具时，消息序列是
/// `user → assistant(tool_use) → user(tool_result) × N`，中间**没有任何新的
/// 真实用户发言**。只认 [`is_user_turn_boundary`] 会让截断点一路回退到 0，
/// 触发 [`compact_session`] 的 `reset_entire_session`——连最近那截
/// `compact_keep_budget` 的工作现场也一起丢掉，压缩比退化成「全量重置」。
/// 这是「压缩后模型明显失忆、重新读文件」的直接机制。
///
/// 安全条件：assistant 消息里**所有** `tool_use` 的 id 都必须能在后续历史里
/// 找到对应的 `tool_result`。只要有一个悬空，从这条消息开始保留就会把
/// use/result 配对拆到摘要两侧，provider 直接拒绝。
fn is_tool_round_start(
    msg: &Message,
    later_result_ids: &std::collections::HashSet<String>,
) -> bool {
    if !matches!(msg.role, Role::Assistant) {
        return false;
    }
    let mut has_tool_use = false;
    for block in &msg.content {
        if let ContentBlock::ToolUse { id, .. } = block {
            has_tool_use = true;
            if !later_result_ids.contains(id) {
                return false;
            }
        }
    }
    has_tool_use
}

/// 从全部消息里收集 `tool_result` 的 `tool_use_id`。
///
/// 只建一次、供整个回退过程复用：回退是 O(条数) 的，每步重建集合会退化成
/// O(n²)，长会话（几千条消息）上足以让压缩前的准备阶段变成肉眼可见的停顿。
/// 有效历史里 `tool_result` 必然出现在其 `tool_use` 之后，所以用全集判定
/// 「配对完整」是安全的；顺序错乱的畸形历史不属于本函数要处理的范围
/// （那种历史本来就会被 `repair_dangling_tool_uses` 与 provider 拒绝）。
fn collect_tool_result_ids(messages: &[Message]) -> std::collections::HashSet<String> {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

/// 在 `idx` 处切开后，**被压缩的那一段**（`messages[..idx]`）是否自洽。
///
/// 唯一要排除的形态：被压缩段的最后一条是含 `tool_use` 的 assistant，而它的
/// `tool_result` 落在保留段里。摘要请求会带着一个悬空 tool_use 发给 provider，
/// 直接被判非法——压缩于是失败，而这条路径恰好发生在「历史已经很长」时。
/// 回退一格即可绕开（前一条是自己的 tool_result 或其之前的消息）。
fn can_cut_before(messages: &[Message], idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    !messages[idx - 1]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
}

/// 从「保留最近 N 条」的朴素截断点回退到最近的安全边界：按固定条数
/// 截断可能正好切在工具结果消息或助手消息上，这样拼接摘要
/// （[User(摘要), Assistant(确认), ...保留部分]）时要么破坏 user/assistant
/// 角色交替、要么拆散 tool_use/tool_result 配对，导致压缩后请求被 Provider
/// 拒绝或模型上下文残缺——表现为"压缩后模型不知道该做什么"。
///
/// 安全边界有两种：
///   1. 不含 tool_result 的 user 消息（真实用户发言）；
///   2. 一个**工具轮起点** assistant 消息，且它的全部 tool_use 都能在历史里
///      找到对应 tool_result（[`is_tool_round_start`]）。
///
/// 第二类是「单轮长任务」不再全量重置的关键：从工具轮中间切开虽然安全
/// （配对完整），但会丢掉这轮已经做过的工作；从工具轮起点切开则保住整轮。
fn safe_keep_from(messages: &[Message], keep_recent: usize) -> Option<usize> {
    let total = messages.len();
    if total == 0 {
        return Some(0);
    }
    let result_ids = collect_tool_result_ids(messages);
    let mut keep_from = total.saturating_sub(keep_recent);
    let is_boundary = |idx: usize| {
        let candidate = &messages[idx];
        (is_user_turn_boundary(candidate) || is_tool_round_start(candidate, &result_ids))
            && can_cut_before(messages, idx)
    };
    while keep_from > 0 && !is_boundary(keep_from) {
        keep_from -= 1;
    }
    if keep_from == 0 && !is_boundary(0) {
        return None;
    }
    Some(keep_from)
}

/// 预算版保留点：先按 token 预算从尾部往前累加定出朴素索引，再复用
/// [`safe_keep_from`] 做「回退到真实用户发言边界」。
///
/// 单条消息本身就超预算时（一条巨型 tool_result）至少退回"保留它之前"的
/// 边界——既不保留全部（压缩等于没做），也不越界访问 `messages[len()]`。
fn safe_keep_from_budget(messages: &[Message], budget: u32) -> Option<usize> {
    let mut used = 0u32;
    let mut keep_from = messages.len();
    while keep_from > 0 {
        let cost = estimate_tokens(std::slice::from_ref(&messages[keep_from - 1]));
        if used.saturating_add(cost) > budget {
            break;
        }
        used = used.saturating_add(cost);
        keep_from -= 1;
    }
    if keep_from >= messages.len() {
        keep_from = messages.len().saturating_sub(1);
    }
    safe_keep_from(messages, messages.len() - keep_from)
}

fn messages_to_text(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "用户",
                Role::Assistant => "助手",
            };
            let parts: Vec<String> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => {
                        // 跳过历史中残留的规约文件 <system-reminder> 块（旧版本注入
                        // 到 user 消息的遗留），避免摘要里混入记忆文件碎片。
                        if text.contains("<system-reminder>") {
                            None
                        } else {
                            Some(crate::textutil::truncate_chars(text, 600, true))
                        }
                    }
                    ContentBlock::ToolUse { name, .. } => Some(format!("[工具调用: {name}]")),
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let text = match content {
                            ToolResultContent::Text(t) => {
                                crate::textutil::truncate_chars(t, 400, true)
                            }
                            ToolResultContent::Parts(_) => {
                                crate::textutil::truncate_chars(&content.display_text(), 400, true)
                            }
                            ToolResultContent::Blocks(_) => "[复杂内容]".to_string(),
                        };
                        let prefix = if *is_error {
                            "[工具错误]"
                        } else {
                            "[工具输出]"
                        };
                        Some(format!("{prefix} {text}"))
                    }
                    ContentBlock::Image { .. } => Some("[图片]".to_string()),
                    // 思考内容进摘要：推理链是模型续跑的语言锚点，压缩时丢弃会让
                    // 后续轮次重新起跑（既丢失已付出的推理，也让输出语言漂移）。
                    // 长思考按 truncate_chars 截断，避免摘要被单轮 reasoning 撑爆。
                    ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                        Some(format!(
                            "[思考] {}",
                            crate::textutil::truncate_chars(thinking, 400, true)
                        ))
                    }
                    // 加密的 redacted 块没有可读内容，只能占位。
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                        Some("[思考]".to_string())
                    }
                })
                .collect();
            format!("[{role}]: {}", parts.join(" | "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_text(s: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: s.to_string(),
            }],
        }
    }

    fn assistant_with_thinking(thinking: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: thinking.to_string(),
                    signature: String::new(),
                    reasoning_details: None,
                },
                ContentBlock::Text {
                    text: "改完了".to_string(),
                },
            ],
        }
    }

    /// 推理内容必须进摘要文本：压缩后它是模型续跑唯一的语言/推理锚点，
    /// 丢弃会让后续轮次重新起跑。
    #[test]
    fn thinking_reaches_compaction_text() {
        let msgs = vec![assistant_with_thinking("先确认 grep.rs 的后端选择")];
        let text = messages_to_text(&msgs);
        assert!(
            text.contains("先确认 grep.rs 的后端选择"),
            "thinking 未进入摘要文本: {text}"
        );
        assert!(text.contains("改完了"), "普通文本不能被挤掉: {text}");
    }

    /// 长 reasoning 截断到 400 字符，避免单轮思考把摘要撑爆。
    #[test]
    fn long_thinking_is_truncated_in_compaction_text() {
        let long = "思".repeat(2000);
        let text = messages_to_text(&[assistant_with_thinking(&long)]);
        assert!(text.contains('…'), "超长 thinking 应被截断: {text}");
        assert!(text.chars().count() < 2000, "截断未生效");
    }

    /// 空 thinking 与 redacted 块只留占位，不产生空段。
    #[test]
    fn empty_and_redacted_thinking_produce_placeholder_only() {
        let msgs = [
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: String::new(),
                    reasoning_details: None,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::RedactedThinking {
                    data: "encrypted".to_string(),
                }],
            },
        ];
        for msg in &msgs {
            assert_eq!(
                messages_to_text(std::slice::from_ref(msg)),
                "[助手]: [思考]",
                "空/加密 thinking 应只剩占位"
            );
        }
    }

    #[test]
    fn compact_trigger_buffer_scales_with_context_window() {
        // 大窗口按比例留 10%，不再被 40K 绝对封顶顶成 96% 才触发。
        assert_eq!(compact_trigger_buffer(200_000, 8_192), 20_000);
        assert_eq!(compact_trigger_buffer(1_000_000, 8_192), 100_000);
        // 小窗口靠 `max_tokens + 8192` 兜底，不会被比例项压到过激。
        assert_eq!(compact_trigger_buffer(8_000, 8_192), 16_384);
    }

    /// 回归：`max_tokens > 40K` 时旧公式会算出
    /// `阈值(160K) + 输出(64K) > 窗口(200K)` 的结构性必 400。
    /// 新公式必须保证 `窗口 - buffer + max_tokens <= 窗口`。
    #[test]
    fn compact_trigger_buffer_always_covers_max_output_tokens() {
        for (window, max_output) in [
            (200_000u32, 64_000u32),
            (200_000, 128_000),
            (32_000, 8_192),
            (1_000_000, 128_000),
        ] {
            let buffer = compact_trigger_buffer(window, max_output);
            assert!(
                buffer >= max_output,
                "window={window} max_output={max_output} buffer={buffer}: \
                 压缩阈值 + 输出预留必须不超过窗口"
            );
            let threshold = window.saturating_sub(buffer);
            assert!(threshold + max_output <= window);
        }
    }

    fn assistant_tool_use() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    fn user_tool_result() -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: ToolResultContent::text("ok"),
                is_error: false,
            }],
        }
    }

    struct StaticSummaryProvider;

    #[async_trait::async_trait]
    impl Provider for StaticSummaryProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<wyj_api::provider::EventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn complete(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<wyj_api::types::CompletionResult> {
            Ok(wyj_api::types::CompletionResult {
                content: vec![ContentBlock::Text {
                    text: "## Task & Intent\n继续完成当前任务。".to_string(),
                }],
                stop_reason: wyj_api::types::StopReason::EndTurn,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }
    }

    #[test]
    fn request_estimate_includes_system_tools_and_output_reserve() {
        let messages = vec![user_text(&"x".repeat(400))];
        let tools = vec![ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({"path": {"type": "string"}}),
            native: None,
        }];

        let message_only = estimate_tokens(&messages);
        let full_request = estimate_request_tokens(
            &wyj_api::SystemPrompt::stable_only("system instruction ".repeat(40).as_str()),
            &messages,
            &tools,
            1024,
        );

        assert!(full_request >= message_only + 1024);
        assert!(full_request > message_only + 1200);
    }

    #[tokio::test]
    async fn compact_session_resets_a_tool_only_turn_instead_of_emitting_zero_compaction() {
        let mut session = Session::new();
        session.messages.push(user_text("请持续检查并修复问题"));
        for _ in 0..4 {
            session.messages.push(assistant_tool_use());
            session.messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: ToolResultContent::text("x".repeat(2_000)),
                    is_error: false,
                }],
            });
        }
        let original_len = session.messages.len();
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
            .await
            .expect("tool-only turn should compact safely");

        assert_eq!(result.messages_removed, original_len);
        assert!(result.tokens_saved_estimate > 0);
        assert_eq!(session.messages.len(), 1);
        assert!(matches!(session.messages[0].role, Role::User));
        assert!(session.messages[0]
            .text()
            .starts_with("[Conversation summary — 9 earlier messages"));
        assert!(!session.messages[0]
            .text()
            .contains(crate::prompts::COMPACT_ACK));
    }

    #[tokio::test]
    async fn compact_session_reports_the_actual_post_compaction_token_reduction() {
        let mut session = Session::new();
        for i in 0..6 {
            session
                .messages
                .push(user_text(&format!("任务 {i}: {}", "x".repeat(2_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "已记录".to_string(),
                }],
            });
        }
        let before = estimate_tokens(&session.messages);
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
            .await
            .expect("normal multi-turn session should compact");
        let after = estimate_tokens(&session.messages);

        assert_eq!(result.tokens_saved_estimate, before - after);
        assert!(result.messages_removed > 0);
        assert!(after < before);
    }

    /// 前缀复用路径：压缩请求的 system / tools / messages 必须**逐字节等于**
    /// 主对话前缀，否则 provider 的前缀缓存一次都命中不了。
    ///
    /// 这条测试钉的是三个具体失败形态（都会静默退化成全价、不报错）：
    ///   1. system 换回 `COMPACT_SYSTEM`（从第一个 token 起分叉）；
    ///   2. tools 传空（工具段排在历史之前，少一个就整段失效）；
    ///   3. messages 不是 `session.messages[..keep_from]` 而是重新文本化的历史。
    #[tokio::test]
    async fn compact_with_prefix_reuses_the_exact_main_conversation_prefix() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Capture {
            system: String,
            tools: Vec<String>,
            messages: Vec<Message>,
        }

        struct CapturingProvider {
            captured: Arc<Mutex<Capture>>,
        }

        #[async_trait::async_trait]
        impl Provider for CapturingProvider {
            async fn stream(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<wyj_api::provider::EventStream> {
                Ok(Box::pin(futures::stream::empty()))
            }

            async fn complete(
                &self,
                system: &wyj_api::SystemPrompt<'_>,
                messages: &[Message],
                tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<wyj_api::types::CompletionResult> {
                let mut guard = self.captured.lock().unwrap();
                guard.system = system.combined();
                guard.tools = tools.iter().map(|t| t.name.clone()).collect();
                guard.messages = messages.to_vec();
                Ok(wyj_api::types::CompletionResult {
                    content: vec![ContentBlock::Text {
                        text: "## Task & Intent\n摘要".to_string(),
                    }],
                    stop_reason: wyj_api::types::StopReason::EndTurn,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                })
            }
        }

        let mut session = Session::new();
        // 每条 20K 字符 ≈ 6.7K token，6 轮远超 keep 预算（200K 窗口下 25K），
        // 这样 `keep_from > 0`，才真正走到「前缀复用」而不是整体重置。
        for i in 0..6 {
            session
                .messages
                .push(user_text(&format!("任务 {i}: {}", "x".repeat(20_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "已记录".to_string(),
                }],
            });
        }
        let original = session.messages.clone();
        let keep_from = safe_keep_from_budget(&session.messages, compact_keep_budget(200_000))
            .expect("应有安全边界");
        assert!(keep_from > 0, "本用例必须落在前缀复用路径，而非整体重置");
        let expected_prefix = original[..keep_from].to_vec();

        let captured = Arc::new(Mutex::new(Capture::default()));
        let provider = CapturingProvider {
            captured: captured.clone(),
        };
        let tools = vec![ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({"path": {"type": "string"}}),
            native: None,
        }];
        let system_text = "main system prompt with memory snapshot";
        compact_session(
            &mut session,
            &provider,
            200_000,
            Some(CompactPrefix {
                system: wyj_api::SystemPrompt::stable_only(system_text),
                tools: &tools,
            }),
        )
        .await
        .expect("前缀复用路径应成功");

        let guard = captured.lock().unwrap();
        assert_eq!(guard.system, system_text, "system 必须是主对话的 stable 段");
        assert_eq!(guard.tools, vec!["Read".to_string()], "tools 必须原样传递");
        assert_eq!(
            guard.messages.len(),
            expected_prefix.len() + 1,
            "请求 = 主对话前缀 + 一条指令，不能把历史重新文本化"
        );
        // `Message` 没有 `PartialEq`，逐条比 role + 文本 + 块数即可覆盖
        // 「是否被重新文本化 / 是否被改写」这两个真实失败形态。
        for (index, (got, want)) in guard.messages[..expected_prefix.len()]
            .iter()
            .zip(expected_prefix.iter())
            .enumerate()
        {
            assert_eq!(got.role, want.role, "第 {index} 条角色被改写");
            assert_eq!(
                got.content.len(),
                want.content.len(),
                "第 {index} 条块数被改写"
            );
            assert_eq!(got.text(), want.text(), "第 {index} 条正文被改写");
        }
        let tail = guard.messages.last().unwrap();
        assert!(tail.text().contains("Summarize the conversation so far"));
        assert!(
            !tail.text().contains("Conversation:"),
            "复用路径不得再把对话文本拼进指令（那等于发两遍历史）"
        );
    }

    /// 朴素按固定条数截断会正好切在 assistant(tool_use) 之后、
    /// user(tool_result) 之前——回退逻辑不能把 keep_from 落在工具结果消息上
    /// （会把 use/result 拆到摘要两侧），但**可以**落在工具轮起点
    /// assistant(tool_use) 上。后者是 P0-1 新增的：单轮长任务里没有新的
    /// 真实 user 发言，只认 user 边界会让 keep_from 一路退到 0，把刚做完的
    /// 整轮工作也一起摘要掉。
    #[test]
    fn safe_keep_from_backs_off_tool_result_boundary() {
        let messages = vec![
            user_text("请帮我读一下这个文件"), // 0: 真实用户边界
            assistant_tool_use(),              // 1: 工具轮起点
            user_tool_result(),                // 2
            assistant_tool_use(),              // 3: 工具轮起点
            user_tool_result(),                // 4
            assistant_tool_use(),              // 5: 工具轮起点
            user_tool_result(),                // 6
        ];
        // 朴素 keep_from = 7 - 3 = 4，正好落在 user_tool_result 上（idx 4）
        let keep_from = safe_keep_from(&messages, 3).expect("应能找到安全边界");
        assert!(
            !matches!(
                messages[keep_from].content.first(),
                Some(ContentBlock::ToolResult { .. })
            ),
            "截断点不能落在 tool_result 上"
        );
        assert_eq!(
            keep_from, 3,
            "应回退到最近一个工具轮起点，而不是继续退到 0 把整轮丢掉"
        );
        assert!(is_tool_round_start(
            &messages[keep_from],
            &collect_tool_result_ids(&messages)
        ));
    }

    /// 核心回归：单轮长任务（只有首条真实 user 发言 + 几十轮工具）压缩后
    /// **不能**退化成 `reset_entire_session`，必须保住尾部工作现场。
    #[test]
    fn compaction_keeps_the_trailing_tool_round_instead_of_resetting() {
        let mut messages = vec![user_text("重构这个模块")];
        for i in 0..20 {
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: "Read".to_string(),
                    input: serde_json::json!({ "file_path": format!("src/f{i}.rs") }),
                }],
            });
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: ToolResultContent::text("x".repeat(400)),
                    is_error: false,
                }],
            });
        }
        let keep_from = safe_keep_from_budget(&messages, 2_000).expect("单轮长任务也必须有边界");
        assert!(keep_from > 0, "不能退化成全量重置");
        assert!(
            is_tool_round_start(&messages[keep_from], &collect_tool_result_ids(&messages)),
            "保留段必须从一个完整的工具轮开始"
        );
        // 保留段里的每个 tool_use 都能找到自己的 tool_result
        let kept = &messages[keep_from..];
        for (idx, msg) in kept.iter().enumerate() {
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    assert!(
                        kept[idx + 1..]
                            .iter()
                            .flat_map(|m| m.content.iter())
                            .any(|b| matches!(
                                b,
                                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                            )),
                        "保留段的 tool_use {id} 没有对应 tool_result"
                    );
                }
            }
        }
    }

    /// 尾部恰好以 assistant(tool_use) 开头时不能再插 `COMPACT_ACK`——那会
    /// 产生两条连续 assistant，provider 直接拒绝。
    #[tokio::test]
    async fn compaction_does_not_insert_ack_when_tail_starts_with_assistant() {
        let mut session = Session::new();
        session.push_user("先读文件");
        // 10 轮 × 约 1000 token 的 tool_result：总量必须超过 keep 预算
        // （8K 窗口下 clamp 到 8_000），否则会出现「全部保留 → keep_from=0
        // → 退化成整体重置」，测不到 ACK 条件化这条路径。
        for i in 0..10 {
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: "Read".to_string(),
                    input: serde_json::json!({}),
                }],
            });
            session.messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: ToolResultContent::text("y".repeat(3_000)),
                    is_error: false,
                }],
            });
        }
        // 小窗口 + 小预算 → 一定会保留尾部工具轮，且首条是 assistant
        compact_session(&mut session, &StaticSummaryProvider, 8_000, None)
            .await
            .expect("应能压缩");
        assert!(
            session.messages.len() >= 2,
            "应保留摘要 + 尾部工具轮，实际 {} 条",
            session.messages.len()
        );
        for pair in session.messages.windows(2) {
            assert!(
                pair[0].role != pair[1].role,
                "压缩后出现连续同角色消息：{:?} → {:?}",
                pair[0].role,
                pair[1].role
            );
        }
        assert!(
            !session.messages.iter().any(|m| {
                matches!(m.content.first(), Some(ContentBlock::Text { text }) if text == crate::prompts::COMPACT_ACK)
            }),
            "尾部以 assistant 开头时不应插入确认语"
        );
    }

    #[test]
    fn safe_keep_from_keeps_naive_cut_when_already_a_boundary() {
        let messages = vec![
            user_text("第一条"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "好的".to_string(),
                }],
            },
            user_text("第二条"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "收到".to_string(),
                }],
            },
            user_text("第三条"),
        ];
        // 朴素 keep_from = 5 - 2 = 3，落在 assistant 消息上，需回退到 idx 2（第二条用户发言）
        let keep_from = safe_keep_from(&messages, 2).expect("应能找到安全边界");
        assert_eq!(keep_from, 2);
        assert!(is_user_turn_boundary(&messages[keep_from]));
    }

    /// v0.2.21 修「366K text input 撞穿 262K 上下文」:首次估算就已在
    /// 阈值内时,`compact_session_until_fit` 应跑 0 轮 compact 直接返回,
    /// 不白白消耗 LLM round-trip 预算。
    #[tokio::test]
    async fn compact_session_until_fit_no_op_when_already_under_threshold() {
        let mut session = Session::new();
        session.messages.push(user_text("hello"));
        let estimated = estimate_tokens(&session.messages);
        let threshold = estimated + 10_000;
        let outcome = compact_session_until_fit(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            threshold,
            None,
            estimate_tokens,
        )
        .await;
        assert_eq!(outcome.passes, 0);
        assert!(outcome.last_result.is_none());
        assert!(outcome.last_error.is_none());
        assert_eq!(outcome.final_estimated, estimate_tokens(&session.messages));
    }

    /// 兜底回归:即便 compact 第一次没把消息压回阈值(heuristic 偏低 /
    /// 摘要本身偏长),`compact_session_until_fit` 必须继续重试而不是
    /// 裸发——这是 v0.2.19 之前『单次 compact 不够就 400 撞穿』的根因。
    #[tokio::test]
    async fn compact_session_until_fit_loops_until_under_threshold() {
        let mut session = Session::new();
        // 10 轮 user+assistant,每条 2000 字符,合计远超 threshold
        for i in 0..10 {
            session
                .messages
                .push(user_text(&format!("任务 {i}: {}", "x".repeat(2_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
            });
        }
        let estimated = estimate_tokens(&session.messages);
        let threshold = estimated / 4; // 强压到 1/4,单次 compact 不够
        let outcome = compact_session_until_fit(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            threshold,
            None,
            estimate_tokens,
        )
        .await;
        // 必须循环过 compact——v0.2.19 之前『0 轮就裸发』的回归根因。
        assert!(outcome.passes >= 1, "必须循环重试,不能裸发");
        assert!(outcome.passes <= 3);
        // 收敛目标:要嘛 final_estimated 落入阈值,要嘛 caller 拿到的是
        // 已尽力压缩的状态(loop 自洽终止 / 「无可压缩」信号)。
        // runtime `truncate_messages` 会在 caller 拿到控制权后兜底截断
        // 单块超长内容,所以这里不强制要求收敛——只要求不挂死、不裸发 366K。
        assert!(outcome.last_error.is_none());
        // final_estimated 必须远小于起始 estimated(说明 compact 真跑了),
        // 否则等于『阈值检测后 compact 啥都没干』的 bug 复现。
        assert!(
            outcome.final_estimated < estimated,
            "compact 必须实际减少 token; 起始 {estimated}, 最终 {}",
            outcome.final_estimated
        );
    }

    /// 口径回归：循环内的收敛判断必须与进入条件同口径。
    ///
    /// 旧实现用 messages-only 的 `estimate_tokens` 对比**请求级**阈值，两者差
    /// 的 system + 工具 schema + 输出预留有 4~5 万 token——于是一次压缩早已
    /// 把真实请求压进阈值，循环却判定「还超标」，白跑第 2、3 轮，每一轮都把
    /// 上一轮刚保留的尾部原文再摘要一次。
    ///
    /// 这里用「measure = messages 估算 + 5 万固定开销」的口径复现：真正被压到
    /// 阈值下之后必须是 0 次额外 pass。
    #[tokio::test]
    async fn until_fit_uses_the_callers_measurement_not_messages_only() {
        const FIXED_OVERHEAD: u32 = 50_000;
        let mut session = Session::new();
        for i in 0..6 {
            session
                .messages
                .push(user_text(&format!("任务 {i}: {}", "x".repeat(2_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
            });
        }
        let measure = |messages: &[Message]| estimate_tokens(messages) + FIXED_OVERHEAD;
        let threshold = measure(&session.messages);
        let outcome = compact_session_until_fit(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            threshold,
            None,
            &measure,
        )
        .await;
        assert_eq!(
            outcome.passes, 0,
            "首次测量已在阈值内时不该白跑任何一轮 compact"
        );
        assert_eq!(outcome.final_estimated, measure(&session.messages));
    }

    /// 停滞熔断：一轮压缩若压不下 5%，继续压只是对着同一段历史反复烧 LLM
    /// 往返（`MAX_COMPACT_PASSES` 上限 3 次），而且每次都会再丢一点原文。
    /// 构造一个「压完几乎不变」的场景：历史只剩摘要体量的内容，压到
    /// `reset_entire_session` 之后再压就只会撞上 `TooFewMessages`。
    #[tokio::test]
    async fn until_fit_stops_when_a_pass_makes_no_progress() {
        // 摘要 provider 返回一条固定的长摘要：压完的体积基本等于压之前的
        // 体积，天然构成「压不动」。
        struct BulkySummaryProvider;
        #[async_trait::async_trait]
        impl Provider for BulkySummaryProvider {
            async fn stream(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<wyj_api::provider::EventStream> {
                Ok(Box::pin(futures::stream::empty()))
            }
            async fn complete(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<wyj_api::types::CompletionResult> {
                Ok(wyj_api::types::CompletionResult {
                    content: vec![ContentBlock::Text {
                        text: "## Task & Intent\n".to_string() + &"摘要内容".repeat(50),
                    }],
                    stop_reason: wyj_api::types::StopReason::EndTurn,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                })
            }
        }

        let mut session = Session::new();
        session.push_user("重构整个模块");
        for i in 0..3 {
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: "Read".to_string(),
                    input: serde_json::json!({}),
                }],
            });
            session.messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: ToolResultContent::text("结果".repeat(200)),
                    is_error: false,
                }],
            });
        }
        let before = estimate_tokens(&session.messages);
        // 阈值定在「几乎不可能达到」的位置，逼它走满多轮。
        let outcome =
            compact_session_until_fit(&mut session, &BulkySummaryProvider, 200_000, 1, None, |m| {
                estimate_tokens(m)
            })
            .await;
        assert!(outcome.last_error.is_none(), "熔断不是错误");
        assert!(
            outcome.passes <= 2,
            "压不动时必须停手，实测跑了 {} 轮",
            outcome.passes
        );
        assert!(
            estimate_tokens(&session.messages) < before,
            "至少第一轮要真的压下去一点"
        );
    }

    /// 压缩失败(LLM 调用挂掉)时不应无限循环,而是把 last_error 透传,
    /// 把控制权还给 caller —— caller 此时仍有 `truncate_messages` 兜底。
    /// 用专门的「complete 永远失败」provider 触发真实的 LLM 错误路径。
    #[tokio::test]
    async fn compact_session_until_fit_propagates_real_compact_failure() {
        struct FailingProvider;
        #[async_trait::async_trait]
        impl Provider for FailingProvider {
            async fn stream(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<wyj_api::provider::EventStream> {
                Ok(Box::pin(futures::stream::empty()))
            }
            async fn complete(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _opts: &wyj_api::provider::RequestOptions,
            ) -> Result<wyj_api::types::CompletionResult> {
                Err(anyhow::anyhow!("synthetic LLM 503"))
            }
        }

        let mut session = Session::new();
        for i in 0..10 {
            session
                .messages
                .push(user_text(&format!("任务 {i}: {}", "x".repeat(2_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
            });
        }
        let estimated = estimate_tokens(&session.messages);
        let threshold = estimated / 4;
        let outcome = compact_session_until_fit(
            &mut session,
            &FailingProvider,
            200_000,
            threshold,
            None,
            estimate_tokens,
        )
        .await;
        assert!(outcome.last_error.is_some(), "LLM 失败必须透传");
        assert!(outcome.passes <= 1, "失败不应再重试,应立即透传错误");
        assert!(outcome.last_error.unwrap().contains("503"));
    }

    /// 「消息数过少」/「找不到安全边界」是『已无可压缩』的合法信号——
    /// 不应被 caller 当成错误处理。这种情况在多轮 compact 收敛后
    /// 会自然出现,本 test 锁住 loop 的宽容语义。
    #[tokio::test]
    async fn compact_session_until_fit_treats_already_compact_as_no_error() {
        let mut session = Session::new();
        // 总消息数 <= 8 → compact_session 会 bail "消息数量过少"
        session.messages.push(user_text("hello"));
        let threshold = 1; // 永远低于,触发 compact
        let outcome = compact_session_until_fit(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            threshold,
            None,
            estimate_tokens,
        )
        .await;
        assert!(outcome.last_error.is_none(), "已无可压缩不是错误");
        // 必须立即退出(不无限循环)
        assert!(outcome.passes <= 1);
    }

    /// 回归:estimate_text_tokens 对代码/JSON (BPE 2.5-3 chars/token) 不再
    /// 用 0.25 (4 chars/token) 偏低——这是 v0.2.21 之前 heuristic 让
    /// compact 触发过晚、最终撞穿 262K 上限的隐藏根因之一。
    /// 这里用 ASCII 长字符串(代表代码 / 路径 / JSON 文本)验证估算比例。
    #[test]
    fn estimate_text_tokens_uses_three_chars_per_token_for_ascii() {
        let text = "a".repeat(300); // 300 chars
        let tokens = estimate_text_tokens(&text);
        // 0.33 token/char = 100 tokens,允许 80~120 区间(防止未来微调)
        assert!(
            (80..=120).contains(&tokens),
            "300 ASCII chars 应估 ~100 tokens, 实测 {tokens}"
        );
    }

    /// CJK 仍是 1.5 token/字(中文 1 字 ≈ 1.5 token),不能因为修 ASCII 而
    /// 把中文场景也破坏。
    #[test]
    fn estimate_text_tokens_keeps_cjk_ratio() {
        let text = "中".repeat(100); // 100 CJK chars
        let tokens = estimate_text_tokens(&text);
        assert_eq!(tokens, 150, "100 CJK chars 应估 150 tokens");
    }

    /// 与 `StaticSummaryProvider` 相同，但返回非零 usage —— 用于验证压缩的
    /// LLM 往返确实被计入 Session 计数器。
    struct UsageSummaryProvider;

    #[async_trait::async_trait]
    impl Provider for UsageSummaryProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<wyj_api::provider::EventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn complete(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<wyj_api::types::CompletionResult> {
            Ok(wyj_api::types::CompletionResult {
                content: vec![ContentBlock::Text {
                    text: "## Task & Intent\n继续完成当前任务。".to_string(),
                }],
                stop_reason: wyj_api::types::StopReason::EndTurn,
                input_tokens: 1_234,
                output_tokens: 56,
                cache_read_input_tokens: 7,
                cache_creation_input_tokens: 8,
            })
        }
    }

    fn multi_turn_session() -> Session {
        let mut session = Session::new();
        for i in 0..6 {
            session
                .messages
                .push(user_text(&format!("任务 {i}: {}", "x".repeat(2_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "已记录".to_string(),
                }],
            });
        }
        session
    }

    #[tokio::test]
    async fn compact_session_counts_the_summary_round_trip_in_session_usage() {
        // 回归背景：旧实现直接丢弃 `CompletionResult` 的 usage，也不累加
        // `api_calls`，`/cost` 与 WYJ_STATS_JSON 因此系统性低估真实花费。
        let mut session = multi_turn_session();
        compact_session(&mut session, &UsageSummaryProvider, 200_000, None)
            .await
            .expect("normal multi-turn session should compact");
        assert_eq!(session.total_input_tokens, 1_234);
        assert_eq!(session.total_output_tokens, 56);
        assert_eq!(session.total_cache_read_tokens, 7);
        assert_eq!(session.total_cache_write_tokens, 8);
        assert_eq!(session.api_calls, 1);
    }

    #[test]
    fn compact_skip_is_typed_and_downcastable_not_string_matched() {
        // 回归背景：旧实现靠 `error.to_string().contains("消息数量过少")` 识别
        // 良性跳过。中文文案一旦被 i18n 本地化或改措辞，判定会静默变成「真失败」。
        let mut session = Session::new();
        session.push_user("hi");
        let err = futures::executor::block_on(compact_session(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            None,
        ))
        .expect_err("只有 1 条消息时应报跳过");
        assert_eq!(
            compact_skip_of(&err),
            Some(CompactSkip::TooFewMessages(1)),
            "必须是 typed error，不能靠字符串匹配识别"
        );
        // Display 仍输出原中文文案，保持既有日志/单测行为
        assert!(err.to_string().contains("消息数量过少"));
    }

    fn tool_result_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t".to_string(),
                content: ToolResultContent::text(text),
                is_error: false,
            }],
        }
    }

    /// 可回收 tool_result 只统计"超出最近 K 个"的部分——最近 3 个必须保留
    /// 全文，否则模型刚读到的文件/grep 结果会凭空消失。
    #[test]
    fn reclaimable_keeps_the_most_recent_tool_results() {
        let messages: Vec<Message> = (0..6).map(|_| tool_result_msg(&"x".repeat(600))).collect();
        let total_each = estimate_text_tokens(&"x".repeat(600)) as u32;
        let reclaimable = reclaimable_tool_result_tokens(&messages);
        assert_eq!(
            reclaimable,
            total_each * 3,
            "6 个工具结果里只有最早的 3 个可回收"
        );
        assert!(reclaimable < estimate_tokens(&messages));
    }

    #[test]
    fn reclaimable_is_zero_when_fewer_than_keep_window() {
        let messages: Vec<Message> = (0..2).map(|_| tool_result_msg(&"x".repeat(600))).collect();
        assert_eq!(reclaimable_tool_result_tokens(&messages), 0);
    }

    /// 保留量按 token 预算而非固定条数：短消息要多留，长消息要少留。
    #[test]
    fn safe_keep_from_budget_scales_with_message_size() {
        let short: Vec<Message> = (0..20).map(|i| user_text(&format!("{}", i))).collect();
        let long: Vec<Message> = (0..20)
            .map(|i| user_text(&format!("{} {}", i, "x".repeat(2000))))
            .collect();

        // 预算 2_000 token：短消息（每条约 1 token）能留很多条
        let keep_short = safe_keep_from_budget(&short, 2_000).unwrap();
        assert!(
            keep_short < 5,
            "20 条极短消息在 2K 预算下应保留几乎全部，实际保留起点 {keep_short}"
        );

        // 同预算下，每条约 667 token 的大消息只能留 2~3 条
        let keep_long = safe_keep_from_budget(&long, 2_000).unwrap();
        assert!(
            keep_long > 15,
            "20 条大消息在 2K 预算下应只保留末尾少数几条，实际保留起点 {keep_long}"
        );
    }

    /// 单条消息就超预算时不能越界，也不能退化成"保留全部"（等于没压）。
    #[test]
    fn safe_keep_from_budget_handles_single_oversized_message() {
        let messages = vec![
            user_text("first"),
            user_text(&"x".repeat(20_000)),
            user_text("last"),
        ];
        let keep_from = safe_keep_from_budget(&messages, 100).expect("必须有安全边界");
        assert!(keep_from < messages.len(), "必须真的丢掉点什么，不能全保留");
        assert!(is_user_turn_boundary(&messages[keep_from]));
    }

    /// `ContextAudit::total()` 必须与旧的 `estimate_request_tokens` 完全等价，
    /// 否则压缩阈值会因"引入分解结构体"而静默漂移。
    #[test]
    fn context_audit_total_matches_request_estimate() {
        let messages = vec![
            user_text("hello world"),
            assistant_tool_use(),
            user_tool_result(),
        ];
        let tools = vec![ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({"path": {"type": "string"}}),
            native: None,
        }];
        let system = wyj_api::SystemPrompt::stable_only("system instruction ");

        let audit = ContextAudit::from_request(&system, &messages, &tools, 8_192);
        let legacy = estimate_request_tokens(&system, &messages, &tools, 8_192);
        assert_eq!(audit.total(), legacy);
        assert_eq!(audit.output_reserve, 8_192);
        assert!(audit.system_tokens > 0);
        assert!(audit.tool_schema_tokens > 0);
        assert!(audit.history_tokens > 0);
    }

    /// `effective_window` 的语义：记录了就用记录的，没记录才回落——面板与
    /// 决策共用同一个分母就是靠它。
    #[test]
    fn effective_window_prefers_the_recorded_one() {
        let audit = ContextAudit::default().with_window(256_000);
        assert_eq!(audit.effective_window(1_000_000), 256_000);
        let never_requested = ContextAudit::default();
        assert_eq!(never_requested.window, 0);
        assert_eq!(never_requested.effective_window(200_000), 200_000);
    }

    /// `calibrated_total` 只缩放 prompt 部分——`output_reserve` 是配置里的
    /// `max_tokens`，不是估出来的，按真实 token 折算没有意义。
    #[test]
    fn calibrated_total_scales_prompt_but_never_the_output_reserve() {
        let audit = ContextAudit {
            system_tokens: 10_000,
            tool_schema_tokens: 5_000,
            history_tokens: 25_000,
            output_reserve: 32_000,
            framing_tokens: 100,
            window: 0,
            reclaimable_tool_result_tokens: 0,
            ..Default::default()
        };
        assert_eq!(audit.calibrated_total(1.0), audit.total());
        assert_eq!(audit.calibrated_total(1.3), 52_130 + 32_000);
        assert_eq!(audit.calibrated_total(0.5), 20_050 + 32_000);
    }

    /// 校准系数被 clamp 在 `[0.5, 2.0]`：超出这个区间几乎一定意味着端点
    /// 上报口径不同（把 cache 算进了 prompt、或干脆编数），照单全收会让
    /// 压缩阈值漂到不可预期的位置。
    #[test]
    fn calibrated_total_clamps_out_of_range_ratios() {
        let audit = ContextAudit {
            system_tokens: 10_000,
            tool_schema_tokens: 0,
            history_tokens: 30_000,
            output_reserve: 0,
            framing_tokens: 0,
            window: 0,
            reclaimable_tool_result_tokens: 0,
            ..Default::default()
        };
        assert_eq!(audit.calibrated_total(9.9), 80_000);
        assert_eq!(audit.calibrated_total(0.01), 20_000);
    }

    /// 面板各明细行之和必须等于合计行——用户把各行加起来得得到同一个数，
    /// 否则第一反应是「面板算错了」。拆分形态（stable + volatile 两行）要
    /// 对上，回退形态（system 合计单行）也要对上。
    #[test]
    fn the_panel_parts_add_up_to_the_total() {
        let audit = ContextAudit {
            system_tokens: 120,
            system_stable_tokens: 100,
            system_volatile_tokens: 20,
            tool_schema_tokens: 300,
            tool_count: 7,
            history_tokens: 400,
            output_reserve: 50,
            framing_tokens: 10,
            ..Default::default()
        };
        assert_eq!(audit.system_split(), Some((100, 20)));
        assert_eq!(audit.parts_sum(), audit.total());
    }

    /// 老会话 / 未发过请求的 audit 没有 stable / volatile 两个字段：面板必须
    /// 回退成 system 合计**单行**，而不是显示成 0 / 0 却仍报一个非零合计。
    #[test]
    fn an_audit_without_the_split_falls_back_to_the_system_total() {
        let audit = ContextAudit {
            system_tokens: 120,
            tool_schema_tokens: 300,
            history_tokens: 400,
            output_reserve: 50,
            framing_tokens: 10,
            ..Default::default()
        };
        assert_eq!(audit.system_split(), None);
        assert_eq!(audit.parts_sum(), audit.total());
    }

    /// 压缩保留预算必须随窗口伸缩——固定条数 / 固定绝对值在两端都会出错。
    ///
    /// 上限 128K 是 v0.3.10 起的新值：旧上限 32K 在 1M 窗口下把历史压成
    /// 约 48K（压缩比 18:1），实测表现为「压缩后模型明显失忆、重新读文件」。
    /// 200K 窗口保持 25K 不变，因此常见配置的行为与旧版一致。
    #[test]
    fn compact_keep_budget_scales_and_clamps() {
        assert_eq!(compact_keep_budget(8_000), 8_000);
        assert_eq!(compact_keep_budget(200_000), 25_000);
        assert_eq!(compact_keep_budget(1_000_000), 125_000);
        assert_eq!(compact_keep_budget(4_000_000), 128_000);
    }
}
