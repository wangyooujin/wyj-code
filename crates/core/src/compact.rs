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

/// 摘要最多允许叠加多少代。
///
/// 每次压缩都会把「上一次的摘要消息」连同它后面的原文一起当成本次的输入
/// （`keep_from > 0` 时摘要必然落在 `to_compact` 段里），于是信息以
/// 「摘要的摘要」的形式逐代衰减：精确数值、否定知识、先后因果最先死。
/// `MAX_COMPACT_PASSES` 只挡单次请求内的重复压缩，挡不住跨回合、
/// 跨小时的长会话累积。
///
/// 超过这个代际后不再做「就地再压」，而是走 `compact_reset_note`：
/// 摘要消息里显式声明「本次是第 N 代」，让模型知道该去 CAS 归档里核原文，
/// 而不是把一份 N 代摘要当成事实基线继续推理。
pub const MAX_COMPACT_GENERATIONS: u32 = 4;

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

/// 压缩硬阈值：上下文估算高于它就触发自动压缩。
///
/// 两条不变量必须同时成立，缺一条都会变成用户可见的故障：
///
/// 1. **`阈值 + 输出预留 ≤ 窗口`**——否则压缩刚把历史压到阈值以下，下一次
///    请求仍然必 400（`compact_trigger_buffer` 的文档已解释过这一点）。
/// 2. **阈值显著大于 0**——否则 `estimated > 阈值` 永远成立。历史被压到只剩
///    一条摘要时估算仍然是正数，于是**每个回合都判定「超限」并再压一次**，
///    而每次都是它自己刚写下的摘要，压无可压——这就是「不停压缩」的死循环。
///
/// ## 比较口径：拿 [`ContextAudit::calibrated_prompt`]，不是 `calibrated_total`
///
/// 本函数返回的阈值**已经**通过 [`compact_trigger_buffer`] 扣掉了输出预留，
/// 所以左边的量必须**只含 prompt**。用 `calibrated_total()`（含 `output_reserve`）
/// 去比等于把 `max_tokens` 计两遍，实际约束退化成
/// `prompt + 2 × max_tokens + 8192 ≤ window`：
///
/// * 官方 Anthropic、`max_tokens = 32000` / 200K 窗口时，阈值 159,008，
///   但 prompt 只到 127,008（63.5%）就触发——而 buffer 的设计意图是 90%；
/// * `max_tokens = 128000` / 204,800 窗口时，阈值 68,608 **小于输出预留本身**，
///   于是 `128000 > 68608` 恒成立——历史压到只剩一条摘要也判定超限，
///   三轮回退一次、永不收敛（实测用户现象：每几轮「已自动压缩 · 保留历史 0 条」）。
///
/// 这两个数量级完全不同的故障是同一个缺陷的两个极端，改任一处都必须连带
/// 检查另一处：`agent.rs` 的进入判定、`compact_session_until_fit` 的收敛判定、
/// `elide_stale_tool_results` 的停手线，三处必须同口径。
///
/// 直接 `window.saturating_sub(buffer)` 在 `max_output_tokens + 8192 >= window`
/// 时静默返回 0（`saturating_sub` 不会报错），本函数因此加了下限，
/// 并用 `window - max_output` 把下限夹回不变量 1。
///
/// 调用方应在进入压缩之前先按 [`compact_trigger_buffer`] 校验输出预算；
/// 这里的下限是**最后一道兜底**，不是替代品。
pub fn compact_threshold(context_window: u32, max_output_tokens: u32) -> u32 {
    let floor = (context_window / 4).min(context_window.saturating_sub(max_output_tokens));
    context_window
        .saturating_sub(compact_trigger_buffer(context_window, max_output_tokens))
        .max(floor)
}

/// 官方 Anthropic 对 `thinking.budget_tokens` 的硬性下限（端点会拒绝更小的值）。
pub const MIN_THINKING_BUDGET_TOKENS: u32 = 1_024;

/// Anthropic provider 在 `max_tokens <= thinking_budget` 时会把输出预算
/// 抬到 `budget + 4096`（`budget_tokens < max_tokens` 是协议硬约束）。
pub const THINKING_BUDGET_HEADROOM: u32 = 4_096;

/// 一次请求**最终会发出去**的输出预算，以及被夹紧后的 `thinking_budget`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudget {
    /// 实际发送的 `max_tokens`（= 夹紧后的用户值 与 provider 下限取大）。
    pub max_tokens: u32,
    /// 实际发送的 `thinking_budget`；`None` = 本次不发思考预算。
    pub thinking_budget: Option<u32>,
}

/// 把「用户配置的输出预算」与「thinking_budget 撑出来的 provider 下限」
/// 一起夹进窗口，返回本次请求真正应该发的值。
///
/// ## 为什么必须一起算
///
/// provider 侧有一条硬约束：`max_tokens <= thinking_budget` 时会把预算抬到
/// `budget + 4096`。于是**真正的输出预留是 `max(max_tokens, budget + 4096)`**，
/// 两者只夹一个都会出错：
///
/// * 只夹 `max_tokens`：`budget` 可以任意大（实测有人填 190K / 200K 窗口），
///   夹紧结果被 provider 的下限整个作废，`compact_threshold` 塌到窗口的 3%，
///   任何历史都触发压缩——[`fit_max_output_tokens`] 形同不存在。
/// * 先合并再夹、把结果赋回 `max_tokens`：`budget` 大而 `max_tokens` 小时，
///   取到的是窗口 40% 而非用户配置值，等于**放大**用户预算，违背
///   「只往下夹、永不放大」。
///
/// 正确做法是先求窗口允许的输出上限（[`fit_max_output_tokens`]），再用它
/// **反过来夹 `thinking_budget`**：压到 `ceiling - 4096`，且不低于官方要求的
/// [`MIN_THINKING_BUDGET_TOKENS`]。压不下去就整个丢弃 budget——发一个必然挤爆
/// 窗口的组合不如不发。
pub fn fit_output_budget(
    context_window: u32,
    max_output_tokens: u32,
    thinking_budget: Option<u32>,
) -> OutputBudget {
    let provider_floor = thinking_budget
        .map(|budget| budget.saturating_add(THINKING_BUDGET_HEADROOM))
        .unwrap_or(0);
    // 用户配置值单独夹一次：这是「只往下夹、永不放大」的唯一执行点。
    let fitted_max = fit_max_output_tokens(context_window, max_output_tokens);
    // 窗口允许的输出上限：以「本次真实会发出的量」为输入再夹一次。
    let ceiling = fit_max_output_tokens(context_window, max_output_tokens.max(provider_floor));
    let clamped_budget = match thinking_budget {
        Some(_) if provider_floor > ceiling => {
            let squeezed = ceiling.saturating_sub(THINKING_BUDGET_HEADROOM);
            (squeezed >= MIN_THINKING_BUDGET_TOKENS).then_some(squeezed)
        }
        other => other,
    };
    let applied_floor = clamped_budget
        .map(|budget| budget.saturating_add(THINKING_BUDGET_HEADROOM))
        .unwrap_or(0);
    OutputBudget {
        max_tokens: fitted_max.max(applied_floor),
        thinking_budget: clamped_budget,
    }
}

/// 把输出预算夹到「窗口还容得下可压缩提示段」的范围内，返回本次请求
/// **实际应该发**的 `max_tokens`。未超限时原样返回，不做任何改动。
///
/// ## 为什么光统一比较口径还不够
///
/// 口径修好之后（阈值比的是 prompt，见 [`compact_threshold`]），剩下的问题是
/// **阈值本身可能低到不可用**：`阈值 = 窗口 - max(max_tokens + 8192, 窗口/10)`，
/// 输出预留直接正比地吃掉可压缩空间。实测用户配置
/// `context_window = 204,800` / `max_tokens = 128,000`（62.5% 的窗口）时阈值
/// 只剩 68,608——历史刚过 68K 就得压一次，虽然能收敛（压完确实落到阈值下），
/// 但表现就是「几乎每几轮弹一次压缩提示」，而且每次都要付一趟摘要往返。
///
/// ## 判据与取值
///
/// 要求压缩阈值至少占窗口的一半（`MIN_PROMPT_SHARE`）。不达标的按窗口的
/// 40% 夹紧——40% 与旧代码在「阈值被压成 0」那条极端路径上用的常量一致，
/// 因此小窗口（`max_tokens + 8192 >= 窗口`）的行为与改动前完全相同：
/// 阈值 0 一定小于窗口一半，直接落到同一个 40% 分支。
///
/// **只会往小夹，永不放大**：`.min(max_output_tokens)` 保证用户填得比 40% 还
/// 小时原值返回——那是「刻意省成本」的选择，不是需要修正的错配。
pub fn fit_max_output_tokens(context_window: u32, max_output_tokens: u32) -> u32 {
    // 阈值至少要留下的窗口份额：1/2。低于它压缩会频繁到失去意义。
    const MIN_PROMPT_SHARE_DENOM: u32 = 2;
    const CLAMPED_OUTPUT_SHARE: u32 = 2; // 分子
    const CLAMPED_OUTPUT_DENOM: u32 = 5; // 分母 → 40%
    if context_window == 0 {
        return max_output_tokens;
    }
    let min_threshold = context_window / MIN_PROMPT_SHARE_DENOM;
    if compact_threshold(context_window, max_output_tokens) >= min_threshold {
        return max_output_tokens;
    }
    let clamped =
        (context_window.saturating_mul(CLAMPED_OUTPUT_SHARE) / CLAMPED_OUTPUT_DENOM).max(1);
    clamped.min(max_output_tokens)
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub messages_removed: usize,
    /// 压缩后保留原文的尾部消息条数（按 token 预算而非固定条数决定）。
    pub messages_kept: usize,
    pub tokens_saved_estimate: u32,
    /// 被压缩段的归档 CAS hash（`Some` = 原文仍可取回）。
    ///
    /// 压缩是整条链路里**唯一的信息不可恢复点**：`context_edit` 把旧工具输出
    /// 外部化到 CAS 且可用 `ContextRecall` 召回，而压缩直接把消息丢掉。实测
    /// 有 14 个会话压缩过，用户事后想找「压缩前那段对话里到底写了什么」时
    /// 无从查起。
    ///
    /// 现在把被压缩段的完整 JSON 归档到 CAS，hash 记进 `Session.elided_blobs`
    /// 参与引用计数回收。`None` = 未接 CAS（测试路径 / CAS 不可写），此时
    /// 行为与旧版完全一致。
    pub archived_hash: Option<String>,
    /// 本次压缩后，摘要处于第几代（1 = 首代摘要，2 = 摘要的摘要……）。
    ///
    /// `compact_count` 只回答「压了几次」，而真正决定信息保真度的是
    /// 「当前这份摘要叠了多少层」。两者不等价：连续压 5 次但每次都
    /// `reset_entire_session`（keep_from = 0）时历史只剩一份全新摘要，
    /// 代际并不叠加。
    pub generation: u32,
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

    /// 本次请求的 **prompt 部分**（system + 工具 schema + 历史 + framing），
    /// **不含** `output_reserve`。
    ///
    /// 这是「实际会被发送上去、可能撞穿窗口的输入」——`output_reserve` 不是
    /// 已有内容，而是给模型留的输出空间。
    pub fn prompt_tokens(&self) -> u32 {
        self.total().saturating_sub(self.output_reserve)
    }

    /// 用**实测校准系数**修正后的 prompt 部分。
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
    pub fn calibrated_prompt(&self, ratio: f32) -> u32 {
        let prompt = self.prompt_tokens();
        (prompt as f32 * ratio.clamp(0.5, 2.0)).round() as u32
    }

    /// 用**实测校准系数**修正后的整次请求占用（prompt + 输出预留）。
    ///
    /// ⚠️ **不要拿它去和 [`compact_threshold`] / 软阈值比较**：那两个阈值
    /// 已经通过 [`compact_trigger_buffer`] 扣掉了输出预留，再比一次等于
    /// `max_tokens` 被计两遍（详见 [`compact_threshold`] 文档）。与阈值比较
    /// 一律用 [`Self::calibrated_prompt`]；本方法只用于「这次请求一共多大」
    /// 这类展示口径（`/context` 的合计行、状态栏占用率）。
    pub fn calibrated_total(&self, ratio: f32) -> u32 {
        self.calibrated_prompt(ratio)
            .saturating_add(self.output_reserve)
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

/// 工具定义的 token 估算，按**工具内容**缓存。
///
/// 每个工具的 `input_schema.to_string()` 是一次完整 JSON 序列化——本仓库
/// 工具集（未含 MCP）就能到几万字符，而 `from_request` 每轮最多调 10 次、
/// 每次遍历 `tools.len()` 个工具，几百次重复序列化同样的字符串。
///
/// ## 缓存键为什么不能只用工具名
///
/// 旧实现键是裸工具名、命中即返回，注释却写「schema 变了会重新算并覆盖旧值」
/// ——那句在实现里**不可能发生**：`Agent::refresh_tool_definitions` 与 MCP
/// 重连改的是 schema，工具名不变，于是缓存永远命中旧值。后果是 MCP 工具热更新
/// 后工具 token 估算长期陈旧，压缩阈值判定随之偏移。
///
/// 现在命中时**比对 description 与 schema 本体**（`serde_json::Value` 的
/// 结构比较，比 `to_string` 快且不分配）。内容一致才复用旧值，不一致就地
/// 重算覆盖——这才是注释描述的行为。
pub fn estimate_tools_tokens(tools: &[ToolDefinition]) -> u32 {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    struct Cached {
        description: String,
        schema: serde_json::Value,
        tokens: u32,
    }

    static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    tools.iter().fold(0u32, |total, tool| {
        let cached_tokens = cache
            .get(&tool.name)
            .filter(|c| c.description == tool.description && c.schema == tool.input_schema)
            .map(|c| c.tokens);
        let tokens = match cached_tokens {
            Some(tokens) => tokens,
            None => {
                let schema = tool.input_schema.to_string();
                let tokens = (estimate_text_tokens(&tool.name)
                    + estimate_text_tokens(&tool.description)
                    + estimate_text_tokens(&schema)) as u32;
                cache.insert(
                    tool.name.clone(),
                    Cached {
                        description: tool.description.clone(),
                        schema: tool.input_schema.clone(),
                        tokens,
                    },
                );
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
    compact_session_with_archive(session, provider, context_window, prefix, None).await
}

/// 同 [`compact_session`]，但把被压缩的原始消息归档进 CAS。
///
/// 归档动机见 [`CompactResult::archived_hash`]：压缩是全链路唯一的信息不可
/// 恢复点。归档失败**不阻断**压缩——它只是「事后能不能查原文」的增强，
/// 而压缩本身是防止请求撞穿窗口的必要动作，优先级更高。
pub async fn compact_session_with_archive(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    prefix: Option<CompactPrefix<'_>>,
    cas: Option<&crate::workspace_cas::WorkspaceCas>,
) -> Result<CompactResult> {
    compact_session_full(session, provider, context_window, prefix, cas, None).await
}

/// 同 [`compact_session_with_archive`]，但可以复用**会话滚动工作文档**
/// （[`crate::session_memory`]）代替一次全量摘要往返。
///
/// ## 为什么这条路径值得单独存在
///
/// 全量摘要的成本与历史长度成正比，而它恰好只在历史最长的时候触发。
/// 滚动文档把这份成本摊到每 N 轮，压缩时只是**读一个文件**：零 LLM 往返、
/// 零 output token，而且信息只被摘要过一次（不会「摘要的摘要」）。
///
/// ## 什么时候仍然走全量摘要
///
/// 文档不可用时（从未更新过、太短、或更新一直在失败）。此刻绝不能因为
/// 「配置里开了 session memory」就交出一份空摘要——那比压缩本身更伤。
pub async fn compact_session_with_session_memory(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    prefix: Option<CompactPrefix<'_>>,
    cas: Option<&crate::workspace_cas::WorkspaceCas>,
    session_memory: Option<&crate::session_memory::SessionMemory>,
) -> Result<CompactResult> {
    compact_session_full(
        session,
        provider,
        context_window,
        prefix,
        cas,
        session_memory,
    )
    .await
}

async fn compact_session_full(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    prefix: Option<CompactPrefix<'_>>,
    cas: Option<&crate::workspace_cas::WorkspaceCas>,
    session_memory: Option<&crate::session_memory::SessionMemory>,
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
    // `to_compact` 取 **owned** 副本而不是切片：归档要在压缩往返之后、
    // 改写历史之前序列化这段原文，而那时 `session` 需要可变借用（记账）。
    // 用切片会让 `session.add_usage` 与归档的不可变借用冲突。
    let (to_compact, to_keep) = if reset_entire_session {
        (session.messages.clone(), Vec::new())
    } else {
        (
            session.messages[..keep_from].to_vec(),
            session.messages[keep_from..].to_vec(),
        )
    };
    let before_tokens = estimate_tokens(&session.messages);
    let messages_removed = to_compact.len();
    let messages_kept = to_keep.len();

    // ── 摘要代际 ──
    // 判据是「被压段里有没有一份既有摘要」：有就说明这次是在摘要之上再摘要，
    // 代际 +1；没有则是首代。
    //
    // 为什么看 `to_compact` 而不是 `session.messages[0]`：`keep_from > 0` 时
    // 既有摘要必然落在被压段；而 `reset_entire_session` 时整个历史都被压，
    // 首条若无摘要说明这是从零开始的第一次压缩。两种情形用同一个判据即可，
    // 不必分叉。取最大值而不是首条，是因为极少数情况下历史里可能残留多份
    // 摘要（历史被手工编辑过），此时以最新那代为准。
    let previous_generation = to_compact
        .iter()
        .filter_map(crate::prompts::summary_generation)
        .max()
        .unwrap_or(0);
    let generation = previous_generation + 1;

    // 摘要 token 上限：取上下文窗口的 1/10，但不超过 16000
    let summary_max_tokens = (context_window / 10).min(16_000);

    // ── 优先复用滚动工作文档 ──
    // 走通时省掉一整次全量摘要往返：文档是每 N 轮增量维护出来的，
    // 信息只被摘要过一次，而且它的体量稳定（≤ MAX_DOC_BYTES），
    // 不像全量摘要那样「历史越长越贵、越压越薄」。
    //
    // **覆盖度是硬前提**：只有文档覆盖到 `keep_from`（= 本次真正会被丢弃的
    // 那一段）时才能 splice。否则文档里缺的正是即将被删掉的内容，拿它代替
    // 摘要等于**静默丢掉那段对话**——比多跑一次全量往返昂贵得多。
    //
    // 不复用时如实记录原因，而不是静默退化——「开了但没生效」和「没开」
    // 在 `/context` 上必须是两种不同的结论。
    let covered_before = session_memory
        .map(|memory| memory.read_state().covered_messages)
        .unwrap_or(0);
    let rolled_doc = session_memory
        .filter(|memory| memory.covers(keep_from))
        .and_then(|memory| memory.summary_text());
    if session_memory.is_some() && rolled_doc.is_none() {
        tracing::debug!(
            keep_from,
            covered = covered_before,
            "滚动工作文档未覆盖本次压缩段，退回全量摘要"
        );
    }
    // 「有没有发生 LLM 往返」必须由**路径**决定，不能靠 usage 数值反推：
    // 端点不一定上报 usage（`StaticSummaryProvider` 就返回 0），按数值
    // 判断会把真实发生过的往返记成 0 次调用。
    let mut used_rolled_doc = false;
    let result = match rolled_doc {
        Some(doc) => {
            used_rolled_doc = true;
            tracing::debug!(
                doc_bytes = doc.len(),
                "复用会话滚动工作文档，跳过全量摘要往返"
            );
            // 记账口径必须与真实往返区分开：这里**没有**发生 LLM 调用，
            // 所以不 add_usage、不 add_cache_usage、不 api_calls += 1。
            // 伪造一次用量会让 `/cost` 显示一笔并不存在的花费。
            wyj_api::types::CompletionResult {
                content: vec![ContentBlock::Text { text: doc }],
                stop_reason: wyj_api::types::StopReason::EndTurn,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            }
        }
        None => match prefix {
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
                let mut req: Vec<Message> = to_compact.clone();
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
                let conv_text = messages_to_text(&to_compact);
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
        },
    };

    // 压缩本身是一次真实的 LLM 往返，必须计入用量。旧实现直接丢弃
    // `CompletionResult` 的 usage 字段（也不累加 `api_calls`），导致
    // `/cost` 与 `WYJ_STATS_JSON` 系统性低估真实花费，压缩越频繁偏差越大。
    // 计入时机放在空摘要检查之前：即使摘要生成失败，这轮 token 也已经花掉了。
    //
    // 复用滚动文档的那条路径**没有发生调用**：不记 input/output，
    // 也不 `api_calls += 1`（否则 `/cost` 会显示一笔并不存在的往返）。
    // 判据是路径标志而非 usage 数值——端点不报 usage 时数值全为 0，
    // 按数值判断会把真实发生过的往返也记成 0 次。
    if !used_rolled_doc {
        session.add_usage(result.input_tokens, result.output_tokens);
        session.add_cache_usage(
            result.cache_read_input_tokens,
            result.cache_creation_input_tokens,
        );
        session.api_calls += 1;
        // 压缩往返也是一次真实的模型请求：它不经过 `provider.stream`
        // 那条路（所以 `api_attempts` 不会自动 +1），但它同样产生 token，
        // 必须同时进两个计数，否则「每次调用带多少上下文」的分母会漏它。
        session.api_attempts += 1;
    }

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

    // ── 重建现场：被压缩段里操作过的文件 ─────────────────────────────────
    //
    // 压缩后模型只看得到摘要，摘要里不会列出具体文件路径。对齐 Claude Code
    // 的「What survives compaction」：官方在压缩后按最近修改时间重读最多
    // 5 个文件。这里做轻量版——只把路径摆进摘要消息，不重读内容（重读要多付
    // token，且可能读到已变的文件，反而误导）。
    let touched_files = collect_touched_files(&to_compact);

    // ── 归档被压缩段 ─────────────────────────────────────────────────────
    //
    // 放在**改写历史之前**：一旦 `session.messages` 被替换，原文就只剩内存
    // 里这份临时切片，而它在函数返回时即失效。
    //
    // 归档的是 `to_compact`（即将被丢弃的那段）的完整 JSON，不是摘要——
    // 摘要里已经只有 600 字符正文和 400 字符工具结果（`messages_to_text`
    // 的截断），不足以回答「压缩前到底写了什么」。
    //
    // 失败只 warn：归档是增强项，压缩本身是防止请求撞穿窗口的必要动作。
    let archived_hash = match cas {
        Some(cas) => match serde_json::to_vec(&to_compact) {
            Ok(bytes) => match cas.intern(&bytes) {
                Ok(hash) => {
                    // 记进 session 的 blob 引用表，会话结束 / `/clear` 时统一
                    // release。不记的话 ref_count 永远 ≥1，`WorkspaceCas::gc`
                    // 永远删不掉这批归档——用「归档」换「CAS 无界增长」。
                    session.elided_blobs.push(hash.clone());
                    Some(hash)
                }
                Err(error) => {
                    tracing::warn!("压缩归档写入 CAS 失败: {error}");
                    None
                }
            },
            Err(error) => {
                tracing::warn!("压缩归档序列化失败: {error}");
                None
            }
        },
        None => None,
    };

    // 摘要必须带上归档 hash：否则 CAS 里那份原文没有任何入口能取回
    // （`ContextRecall` 只认 `[context-edited]` 占位符，而压缩是整段删除）。
    let archive_ref = archived_hash.as_deref();
    let summary_message = |removed: usize| Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: crate::prompts::compact_summary_message_full(
                removed,
                &summary,
                &touched_files,
                archive_ref,
                generation,
            ),
        }],
    };

    if reset_entire_session {
        session.messages = vec![summary_message(messages_removed)];
    } else {
        session.messages = vec![summary_message(messages_removed)];
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
    // 覆盖指针必须跟着历史一起重算：`covered_messages` 是**消息下标**，而
    // 压缩把历史从 N 条换成 M 条。不重置的话，下次 `SessionMemory::update`
    // 会算出 `covered.min(len) == len` → 增量恒空 → 文档永远不再更新，
    // 而压缩仍在拿这份越来越旧的文档当摘要。
    //
    // 置 0（= 下一次全量重读）而不是置成新长度：保留段（`to_keep`）可能
    // 尚未进文档，置成新长度会让那段**永远**进不去。
    if let Some(memory) = session_memory {
        memory.reset_coverage();
    }
    let tokens_saved = before_tokens.saturating_sub(estimate_tokens(&session.messages));

    Ok(CompactResult {
        messages_removed,
        messages_kept,
        tokens_saved_estimate: tokens_saved,
        archived_hash,
        generation,
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
    compact_session_until_fit_full(
        session,
        provider,
        context_window,
        threshold,
        prefix,
        measure,
        None,
        None,
    )
    .await
}

/// 同 [`compact_session_until_fit`]，但把每一轮被压缩的原文归档进 CAS。
///
/// 循环里每一轮都可能丢一段不同的原文，所以归档是**逐轮**发生的
/// （`compact_session_with_archive` 每次自己 intern 一个 blob）。
#[allow(clippy::too_many_arguments)]
pub async fn compact_session_until_fit_with_archive(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    threshold: u32,
    prefix: Option<CompactPrefix<'_>>,
    measure: impl Fn(&[Message]) -> u32,
    cas: Option<&crate::workspace_cas::WorkspaceCas>,
) -> CompactUntilFitOutcome {
    compact_session_until_fit_full(
        session,
        provider,
        context_window,
        threshold,
        prefix,
        measure,
        cas,
        None,
    )
    .await
}

/// 同 [`compact_session_until_fit_with_archive`]，并可在第一轮之后复用
/// 会话滚动工作文档。
///
/// 只有**第一轮**会用到文档：一旦压缩发生，历史已被替换成摘要，而
/// `covered_messages` 指向的是压缩前的条数——第二轮若再取文档，会得到
/// 与刚写入的摘要互相矛盾的两份叙述。第二轮起一律走全量摘要（此时
/// 历史已经很短，成本可忽略）。
#[allow(clippy::too_many_arguments)]
pub async fn compact_session_until_fit_with_session_memory(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    threshold: u32,
    prefix: Option<CompactPrefix<'_>>,
    measure: impl Fn(&[Message]) -> u32,
    cas: Option<&crate::workspace_cas::WorkspaceCas>,
    session_memory: Option<&crate::session_memory::SessionMemory>,
) -> CompactUntilFitOutcome {
    compact_session_until_fit_full(
        session,
        provider,
        context_window,
        threshold,
        prefix,
        measure,
        cas,
        session_memory,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn compact_session_until_fit_full(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
    threshold: u32,
    prefix: Option<CompactPrefix<'_>>,
    measure: impl Fn(&[Message]) -> u32,
    cas: Option<&crate::workspace_cas::WorkspaceCas>,
    session_memory: Option<&crate::session_memory::SessionMemory>,
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
        // 只有第一轮复用滚动文档：压缩发生后历史已被摘要替换，
        // 文档的 `covered_messages` 指向压缩前，再用会导致两份叙述互相矛盾。
        let doc_for_this_pass = if pass == 0 { session_memory } else { None };
        match compact_session_with_session_memory(
            session,
            provider,
            context_window,
            prefix,
            cas,
            doc_for_this_pass,
        )
        .await
        {
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
pub(crate) fn is_user_turn_boundary(msg: &Message) -> bool {
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

/// 从即将被压缩的消息里提取「操作过的文件路径」，按**出现顺序去重**。
///
/// 只认 `Read` / `Edit` / `Write` / `NotebookEdit` 的 `file_path` 参数——
/// 这四个是「模型确实碰过这个文件」的直接证据。`Grep` / `Glob` 的 `path`
/// 是目录或搜索根，列进「touched files」只会误导（模型会以为自己读过它）。
///
/// 上限 20 条：这个列表进摘要消息，而摘要消息本身要尽量短。按**倒序取**
/// （最近操作的在前面）——压缩后最需要的是「刚刚在改什么」，不是「一小时前
/// 读过什么」。
fn collect_touched_files(messages: &[Message]) -> Vec<String> {
    const MAX_FILES: usize = 20;
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    // 倒着走：最近的优先。
    for message in messages.iter().rev() {
        for block in message.content.iter().rev() {
            let ContentBlock::ToolUse { name, input, .. } = block else {
                continue;
            };
            if !matches!(name.as_str(), "Read" | "Edit" | "Write" | "NotebookEdit") {
                continue;
            }
            let Some(path) = input.get("file_path").and_then(|v| v.as_str()) else {
                continue;
            };
            let path = path.trim();
            if path.is_empty() || !seen.insert(path.to_string()) {
                continue;
            }
            out.push(path.to_string());
            if out.len() >= MAX_FILES {
                return out;
            }
        }
    }
    out
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
                reasoning_tokens: 0,
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

    /// 首代摘要就是第 1 代，且正文里**不**出现代际警示。
    #[tokio::test]
    async fn first_compaction_is_generation_one_without_a_warning() {
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
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
            .await
            .expect("应能压缩");
        assert_eq!(result.generation, 1);
        assert!(
            !session.messages[0]
                .text()
                .contains("generation 1 of a layered summary"),
            "第 1 代不该出现警示文案: {}",
            session.messages[0].text()
        );
    }

    /// 已有摘要再被压缩 → 代际 +1，且摘要正文里写出「第 N 代」。
    ///
    /// 这条钉的是「摘要的摘要」这个真实失败模式：信息逐代衰减
    /// （精确数值 / 否定知识 / 先后因果最先死），而模型对此毫无感知。
    #[tokio::test]
    async fn recompacting_a_summary_bumps_the_generation_and_warns() {
        let mut session = Session::new();
        // 先造一份「第 1 代摘要」，形式与生产环境一致（前缀 + 正文）。
        session
            .messages
            .push(user_text(&crate::prompts::compact_summary_message(
                9,
                "第一代摘要",
            )));
        for i in 0..6 {
            session
                .messages
                .push(user_text(&format!("后续任务 {i}: {}", "x".repeat(2_000))));
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "已记录".to_string(),
                }],
            });
        }
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
            .await
            .expect("应能压缩");
        assert_eq!(result.generation, 2, "在摘要之上再摘要必须升一代");
        let text = session.messages[0].text();
        assert!(
            text.contains("generation 2 of a layered summary"),
            "第 2 代必须在正文里声明: {text}"
        );
        assert!(
            text.contains("may have drifted"),
            "应说明细节可能已漂移: {text}"
        );
    }

    /// 超过代际上限后措辞要更强：从「可能漂移」升级成「当地图用」。
    #[tokio::test]
    async fn past_the_generation_cap_the_wording_gets_stronger() {
        let mut session = Session::new();
        let deep = crate::prompts::compact_summary_message_full(
            99,
            "第四代摘要",
            &[],
            None,
            MAX_COMPACT_GENERATIONS,
        );
        session.messages.push(user_text(&deep));
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
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
            .await
            .expect("应能压缩");
        assert_eq!(result.generation, MAX_COMPACT_GENERATIONS + 1);
        let text = session.messages[0].text();
        assert!(
            text.contains("past the point where"),
            "超过上限后应换成更强的措辞: {text}"
        );
    }

    /// `summary_generation` 只认摘要前缀：普通用户消息不该被误判成摘要。
    #[test]
    fn summary_generation_only_matches_real_summary_messages() {
        assert_eq!(
            crate::prompts::summary_generation(&user_text("普通的一轮对话")),
            None
        );
        assert_eq!(
            crate::prompts::summary_generation(&user_text(
                &crate::prompts::compact_summary_message(3, "老摘要")
            )),
            Some(1),
            "老格式（无代际字样）按第 1 代处理"
        );
        assert_eq!(
            crate::prompts::summary_generation(&user_text(
                &crate::prompts::compact_summary_message_full(3, "新摘要", &[], None, 5)
            )),
            Some(5)
        );
    }

    /// 滚动文档可用时，压缩**不得**发生 LLM 往返：这是它存在的全部理由。
    ///
    /// 顺带钉住记账口径——`api_calls` 不能因为「复用了文档」就 +1，
    /// 否则 `/cost` 会显示一笔并不存在的调用。
    #[tokio::test]
    async fn usable_session_memory_replaces_the_summary_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let memory = crate::session_memory::SessionMemory::new(dir.path(), "s1");
        let doc = format!(
            "## Task & Intent\n把压缩改成滚动文档。\n## Current State\n{}",
            "细节。".repeat(200)
        );
        std::fs::write(
            dir.path().join(format!(
                "{}-s1.md",
                crate::session_memory::SESSION_MEMORY_FILENAME_PREFIX
            )),
            &doc,
        )
        .unwrap();

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
        let api_calls_before = session.api_calls;
        let result = compact_session_with_session_memory(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            None,
            None,
            Some(&memory),
        )
        .await
        .expect("应能压缩");

        assert!(result.messages_removed > 0);
        assert_eq!(
            session.api_calls, api_calls_before,
            "复用滚动文档时零 LLM 往返，api_calls 不得增加"
        );
        let text = session.messages[0].text();
        assert!(
            text.contains("把压缩改成滚动文档"),
            "摘要正文必须来自滚动文档而不是模型输出: {text}"
        );
    }

    /// 文档不可用时必须**退回**全量摘要，而不是交出一份空摘要。
    ///
    /// 「开了 session memory 但一次都没成功更新」是真实会发生的状态
    /// （首轮就撞阈值 / 更新一直失败）。此时若直接 splice 空文档，
    /// 压缩会静默销毁整段历史。
    #[tokio::test]
    async fn unusable_session_memory_falls_back_to_a_full_summary() {
        let dir = tempfile::tempdir().unwrap();
        let memory = crate::session_memory::SessionMemory::new(dir.path(), "s1");
        // 只有几十字节，远低于 MIN_USABLE_BYTES。
        std::fs::write(
            dir.path().join(format!(
                "{}-s1.md",
                crate::session_memory::SESSION_MEMORY_FILENAME_PREFIX
            )),
            "太短",
        )
        .unwrap();

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
        let api_calls_before = session.api_calls;
        compact_session_with_session_memory(
            &mut session,
            &StaticSummaryProvider,
            200_000,
            None,
            None,
            Some(&memory),
        )
        .await
        .expect("应能压缩");

        assert_eq!(
            session.api_calls,
            api_calls_before + 1,
            "退回全量摘要时必须真的发生了往返"
        );
        assert!(
            session.messages[0].text().contains("继续完成当前任务"),
            "应使用模型输出的摘要: {}",
            session.messages[0].text()
        );
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
                    reasoning_tokens: 0,
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
                    reasoning_tokens: 0,
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
                reasoning_tokens: 0,
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

    /// 压缩后必须把「被压段里操作过的文件」列进摘要消息。
    ///
    /// 钉的是「压缩后模型不知道现场在哪」这个缺陷：摘要里只会写「改过 auth
    /// 模块」，模型要么重新 Read 一遍（多付一次完整文件 token），要么凭印象
    /// 改错地方。对齐 Claude Code 的「What survives compaction」表。
    #[test]
    fn compact_summary_lists_files_touched_before_compaction() {
        // 有文件操作 → 摘要消息里必须出现路径。
        let mut session = Session::new();
        session.push_user("把 auth 模块的错误处理修一下");
        for i in 0..8 {
            session.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: if i % 2 == 0 { "Edit" } else { "Read" }.to_string(),
                    input: serde_json::json!({
                        "file_path": format!("/repo/src/auth_{i}.rs"),
                        "content": "x".repeat(400),
                    }),
                }],
            });
            session.push_tool_result(format!("t{i}"), ToolResultContent::text("ok"), false);
        }
        futures::executor::block_on(async {
            compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
                .await
                .expect("should compact");
        });
        let summary_text = session
            .messages
            .first()
            .and_then(|m| match &m.content[0] {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            summary_text.contains("Files touched before compaction"),
            "有文件操作时必须列出这一段，实际摘要：{summary_text}"
        );
        assert!(
            summary_text.contains("/repo/src/auth_"),
            "必须列出具体路径，实际摘要：{summary_text}"
        );
    }

    /// 没有文件操作的历史不该凭空多出一段（不制造噪音）。
    #[test]
    fn compact_summary_omits_file_section_when_nothing_touched() {
        let mut session = multi_turn_session();
        futures::executor::block_on(async {
            compact_session(&mut session, &StaticSummaryProvider, 200_000, None)
                .await
                .expect("should compact");
        });
        let summary_text = session
            .messages
            .first()
            .and_then(|m| match &m.content[0] {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            !summary_text.contains("Files touched before compaction"),
            "没有文件操作时不该加这一段"
        );
    }

    /// `collect_touched_files` 只认真正的文件操作工具，且最近的排在前面。
    #[test]
    fn collect_touched_files_ignores_search_roots_and_orders_recent_first() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "a".to_string(),
                    name: "Read".to_string(),
                    input: serde_json::json!({ "file_path": "/repo/old.rs" }),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "b".to_string(),
                    name: "Grep".to_string(),
                    // 搜索根不是「读过的文件」，不该进列表。
                    input: serde_json::json!({ "path": "/repo/src", "pattern": "foo" }),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "c".to_string(),
                    name: "Edit".to_string(),
                    input: serde_json::json!({ "file_path": "/repo/new.rs" }),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "d".to_string(),
                    name: "Read".to_string(),
                    // 重复路径只记一次。
                    input: serde_json::json!({ "file_path": "/repo/old.rs" }),
                }],
            },
        ];
        let files = collect_touched_files(&messages);
        assert_eq!(
            files,
            vec!["/repo/old.rs".to_string(), "/repo/new.rs".to_string()],
            "最近的排前面、去重、且不含 Grep 的搜索根"
        );
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

    /// **工具 token 缓存必须随 schema 内容失效**。
    ///
    /// 回归背景：旧实现的缓存键是裸工具名、命中即返回，注释却写「schema 变了
    /// 会重新算并覆盖旧值」——那句在实现里不可能发生。`refresh_tool_definitions`
    /// 与 MCP 重连改的是 schema，工具名不变，于是估算永远停在旧值，压缩阈值
    /// 判定随之偏移。
    #[test]
    fn tool_token_cache_follows_schema_changes() {
        let tool = |description: &str, schema: serde_json::Value| ToolDefinition {
            name: "schema_mutation_probe".to_string(),
            description: description.to_string(),
            input_schema: schema,
            native: None,
        };
        let small = tool("d", serde_json::json!({"a": {"type": "string"}}));
        let before = estimate_tools_tokens(std::slice::from_ref(&small));
        // 同名、同 description，只把 schema 撑大：必须重算。
        let big = tool(
            "d",
            serde_json::json!({
                "a": {"type": "string"},
                "b": {"type": "string", "description": "x".repeat(2_000)},
            }),
        );
        let after = estimate_tools_tokens(std::slice::from_ref(&big));
        assert!(
            after > before,
            "schema 变大后估算必须跟着涨（before={before}, after={after}）"
        );
        // description 变化同样要重算。
        let renamed = tool(&"x".repeat(1_000), big.input_schema.clone());
        assert!(
            estimate_tools_tokens(std::slice::from_ref(&renamed)) > after,
            "description 变化同样必须触发重算"
        );
        // 内容回到原样时结果也回到原值（缓存不残留）。
        assert_eq!(
            estimate_tools_tokens(std::slice::from_ref(&small)),
            before,
            "回到同一份内容必须得到同一个估算"
        );
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

    /// 核心回归：阈值比较的量必须**不含**输出预留。
    ///
    /// 现场（用户实测）：`context_window = 204,800` / `max_tokens = 128,000`
    /// 时阈值只有 68,608，而 `calibrated_total` 含 128,000 的输出预留——
    /// `128000 > 68608` **恒成立**，于是历史压到只剩一条摘要仍然判超限，
    /// 每三轮回退一次、永不收敛（用户看到「已自动压缩 · 保留最近 0 条原文」）。
    ///
    /// 这条测试用「一条消息都没有」的空历史把该恒等式钉死：只要比较口径回退
    /// 到 `calibrated_total`，空历史的判定也会超阈值，立刻红。
    #[test]
    fn empty_history_never_exceeds_the_compact_threshold() {
        let window = 204_800;
        let max_tokens = 128_000;
        let threshold = compact_threshold(window, max_tokens);
        // 空历史 + 用户配置的输出预留：`output_reserve` 必须真的填成 128,000，
        // 否则 `calibrated_total` 里根本没有那个「凭空多出来的下界」。
        let audit = ContextAudit {
            output_reserve: max_tokens,
            window,
            ..Default::default()
        };

        assert_eq!(
            threshold, 68_608,
            "阈值 = window - (max_tokens + 8192)，这是用户现场的实测值"
        );
        assert!(
            threshold < max_tokens,
            "本用例的意义在于「阈值比输出预留还小」这一形态，参数必须先满足它"
        );
        assert_eq!(audit.prompt_tokens(), 0);
        assert!(
            audit.calibrated_prompt(1.0) <= threshold,
            "空历史不存在「已经超阈值」这回事：prompt 0 vs 阈值 {threshold}"
        );
        // 反证：同一个 audit 用 calibrated_total 就是恒超阈值。
        assert!(
            audit.calibrated_total(1.0) > threshold,
            "calibrated_total 含输出预留 {}，必然大于阈值 {threshold}——这正是旧缺陷",
            max_tokens
        );
    }

    /// 校准系数只作用于 prompt，`calibrated_prompt` 与 `calibrated_total`
    /// 必须严格相差一个未被缩放的输出预留。
    #[test]
    fn calibrated_prompt_and_total_differ_by_exactly_the_output_reserve() {
        let audit = ContextAudit {
            system_tokens: 10_000,
            tool_schema_tokens: 5_000,
            history_tokens: 25_000,
            output_reserve: 32_000,
            framing_tokens: 100,
            ..Default::default()
        };
        for ratio in [1.0_f32, 1.3, 0.5, 2.0] {
            assert_eq!(
                audit.calibrated_total(ratio),
                audit.calibrated_prompt(ratio) + 32_000,
                "ratio = {ratio}"
            );
        }
        assert_eq!(audit.prompt_tokens(), 40_100);
        assert_eq!(audit.calibrated_prompt(1.0), 40_100);
    }

    /// 输出预算挤占窗口时会被夹到可压缩范围内；不挤占时原值返回。
    ///
    /// 用户现场：204,800 窗口 + 128,000 输出（62.5%）→ 阈值只剩 68,608，
    /// 历史刚过 68K 就压一次。夹到 40% 后阈值回到 114,688，压缩频率正常。
    #[test]
    fn oversized_output_budget_is_clamped_to_keep_room_for_prompt() {
        let window = 204_800;
        let clamped = fit_max_output_tokens(window, 128_000);
        assert_eq!(
            clamped, 81_920,
            "应夹到窗口的 40%，否则可压缩提示段被输出预留吃掉"
        );
        assert!(
            compact_threshold(window, clamped) >= window / 2,
            "夹紧后阈值至少要占到窗口一半"
        );
        assert!(
            compact_threshold(window, 128_000) < window / 2,
            "夹紧前阈值确实不达标——否则这个用例没有验证任何东西"
        );
    }

    /// **`thinking_budget` 不能绕过输出预算夹紧**。
    ///
    /// 回归背景（用户现场）：`thinking_budget = 190_000` / 200K 窗口时，
    /// provider 会把 `max_tokens` 抬到 `budget + 4096 = 194_096`，而旧实现
    /// 用 `.max(provider_floor)` 把夹紧结果整个作废——`compact_threshold`
    /// 塌到窗口的 3%，任何历史都触发压缩，等于压缩死循环。
    #[test]
    fn thinking_budget_cannot_defeat_the_output_clamp() {
        let window = 200_000;
        for budget in [100_000, 150_000, 190_000] {
            let plan = fit_output_budget(window, 32_000, Some(budget));
            let applied_budget = plan.thinking_budget.unwrap_or(0);
            let floor = applied_budget.saturating_add(THINKING_BUDGET_HEADROOM);
            let effective_output = plan.max_tokens.max(floor);
            let threshold = compact_threshold(window, effective_output);
            assert!(
                threshold >= window / 4,
                "budget={budget}：阈值塌到 {threshold}（窗口 {}），压缩会死循环",
                window / 4
            );
            assert!(
                threshold + effective_output <= window,
                "budget={budget}：阈值 + 输出 {effective_output} 超过窗口"
            );
        }
    }

    /// 夹紧方向**只往下**：`budget` 撑大时压 budget，但绝不放大用户配置的
    /// `max_tokens`。
    #[test]
    fn thinking_budget_clamp_never_raises_the_configured_max_tokens() {
        let window = 200_000;
        // 用户只想要 8K 输出，却配了个很大的思考预算。
        let plan = fit_output_budget(window, 8_192, Some(150_000));
        assert!(
            plan.max_tokens
                <= 8_192.max(
                    plan.thinking_budget
                        .unwrap_or(0)
                        .saturating_add(THINKING_BUDGET_HEADROOM)
                ),
            "实际输出不该超过「用户配置」与「provider 下限」的较大者"
        );
        // 小 budget 完全不该被碰。
        let small = fit_output_budget(window, 32_000, Some(8_192));
        assert_eq!(small.thinking_budget, Some(8_192), "小预算原样保留");
        assert_eq!(small.max_tokens, 32_000, "用户配置值原样保留");
    }

    /// 窗口小到连「1024 思考预算 + 4096 余量」都容不下时，整个丢弃 budget
    /// ——发一个必然挤爆窗口的组合不如不发。
    #[test]
    fn a_budget_that_cannot_fit_the_window_is_dropped_entirely() {
        // 窗口 4K：40% 上限 = 1600，减掉 4096 余量后不足 1024。
        let plan = fit_output_budget(4_000, 1_000, Some(50_000));
        assert_eq!(plan.thinking_budget, None, "压不下去就必须整个丢弃");
        assert!(plan.max_tokens > 0);
    }

    /// 没有 thinking_budget 时行为必须与 `fit_max_output_tokens` **逐字节相同**
    /// ——绝大多数 profile 走的都是这条路。
    #[test]
    fn no_thinking_budget_matches_the_plain_clamp() {
        for (window, max_tokens) in [
            (200_000u32, 32_000u32),
            (204_800, 128_000),
            (1_000_000, 64_000),
            (0, 32_000),
        ] {
            let plan = fit_output_budget(window, max_tokens, None);
            assert_eq!(
                plan.max_tokens,
                fit_max_output_tokens(window, max_tokens),
                "window={window} max_tokens={max_tokens}"
            );
            assert_eq!(plan.thinking_budget, None);
        }
    }

    /// 不过度干预：用户刻意把输出预留填得很小时，原值返回，绝不放大。
    #[test]
    fn a_small_output_budget_is_never_raised() {
        let window = 200_000;
        for max_tokens in [8_192, 32_000, 64_000, 81_920] {
            assert_eq!(
                fit_max_output_tokens(window, max_tokens),
                max_tokens,
                "max_tokens = {max_tokens} 已经留够提示空间，必须原样返回"
            );
        }
        // 已夹紧的下限本身不应再被继续往下压。
        assert_eq!(fit_max_output_tokens(window, 40_000), 40_000);
        // 空窗口是「还没算出有效窗口」的哨兵，不能把预算归零。
        assert_eq!(fit_max_output_tokens(0, 32_000), 32_000);
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
