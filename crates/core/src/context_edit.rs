//! Tool-result context editing：把过期的工具输出外部化到 CAS 并在上下文里留占位。
//!
//! # 为什么不是「直接删」
//!
//! 整段摘要把 20K 的 Grep 输出压成一行 `[工具输出] xxx…`，而摘要最先丢的恰好是
//! **负知识**（被否决的假设）、**精确值**（路径/行号/常量）、**顺序因果**——见
//! `doc/plan/v0.2.26-context-management-plan.md` 的失败模式清单。清理比摘要便宜
//! （无 output token、不产生幻觉）**也更保真**（原文只是移出模型视野，没被转述）。
//! Claude Code 官方文档的措辞正是这个顺序：*"It clears older tool outputs first,
//! then summarizes the conversation if needed."*
//!
//! # 为什么必须外部化而不是就地占位
//!
//! `Session.messages` 同时是「发给 provider 的内容」和「落盘的 transcript」。
//! 就地占位会让 transcript 永久丢内容——而 Bash / WebFetch 这类命令**重跑也拿不回
//! 原文**。所以占位符里带一个 `cas://<hash>`，模型可用 `ContextRecall` 取回；
//! 会话 resume 时 [`materialize_elided`] 会把全文还原回内存（新会话有完整上下文，
//! 之后随增长重新被清理）。CAS 侧的 `intern()` 天然按内容去重，重复的 Grep 输出
//! 只存一份。
//!
//! # 保护规则（误删代价最高的部分）
//!
//! 只有**允许清单**里的只读/一次性工具的结果会被清理。写操作的回执
//! （`Edit`/`Write`/`NotebookEdit`）**永不清理**——丢了模型会以为自己没改过，从而重做。
//! 最近 [`ElideOptions::protect_recent`] 个工具结果同样受保护（模型刚读到的内容
//! 不能凭空消失）。

use anyhow::{Context, Result};
use wyj_api::types::{ContentBlock, Message, ToolResultContent, ToolResultPart};

use crate::session::Session;
use crate::workspace_cas::WorkspaceCas;

/// 占位符首行标记。既是给模型看的说明，也是 resume 时识别「这条是占位」的锚点。
pub const ELIDED_MARKER: &str = "[context-edited]";

/// 允许被外部化清理的工具白名单。
///
/// 用白名单而非黑名单是刻意的：新增工具默认**不**被清理，需要显式加进来。清理的
/// 误删代价（模型以为自己没做过某事）远高于漏删的收益。
///
/// - `Read` / `Grep` / `Glob` / `CodeSearch`：确定性只读，丢了可以重跑，但重跑也有
///   代价（时间 + token），能召回就比重跑好。
/// - `Bash` / `WebFetch` / `WebSearch`：一次性执行，**重跑拿不回原文**，是外部化
///   收益最高、也最需要 `ContextRecall` 的一类。
/// - `Agent`：子 Agent 的最终结论。重跑它等于让子 Agent 重做一遍调研；而它在
///   重度使用子 Agent 的会话里往往就是最大的几块内容。此前 `Agent` 不在名单里，
///   于是这类会话永远 `elided == 0`（见 [`elide_tool_results`] 的早退分支），
///   只能退到有损的 `compact` 摘要。
/// - `mcp__*`：MCP server 的返回值同样可能巨大且不可重跑。`MCP` 桥接工具的实际
///   名字统一带 `mcp_{server}__{tool}` 前缀（见 `mcp::bridge::build_bridges`），
///   这里按前缀匹配而不是枚举具体工具。
///
/// 仍**不**在名单里的：`Write` / `Edit` / `NotebookEdit`（写操作的回执重跑一次
/// 就会再次落盘，是有副作用的重复执行）、`BashOutput`（小结果为主）。
pub const ELIDABLE_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "CodeSearch",
    "Bash",
    "WebFetch",
    "WebSearch",
    "Agent",
];

/// 按前缀匹配的 MCP 工具名（MCP server 的返回值也可能巨大且不可重跑）。
pub const ELIDABLE_TOOL_PREFIXES: &[&str] = &["mcp__"];

pub fn is_elidable_tool(tool_name: &str) -> bool {
    ELIDABLE_TOOLS.contains(&tool_name)
        || ELIDABLE_TOOL_PREFIXES
            .iter()
            .any(|prefix| tool_name.starts_with(prefix))
}

/// 单次调用的清理参数。
#[derive(Debug, Clone, Copy)]
pub struct ElideOptions {
    /// 最近 N 个工具结果保留全文不动。对齐 Anthropic `clear_tool_uses_20250919`
    /// 的默认 `keep = 3`。
    pub protect_recent: usize,
    /// 单批处理条数。对齐 Goose 的 `TOOLCALL_SUMMARIZATION_BATCH_SIZE = 10`，
    /// 避免一次请求做掉一大手术、把 prompt cache 前缀反复击穿。
    pub batch_size: usize,
    /// 小于这个字节数的结果不值得外部化：省下的 token 还不够一条占位符本身。
    pub min_bytes: usize,
}

impl Default for ElideOptions {
    fn default() -> Self {
        Self {
            protect_recent: crate::compact::RECLAIM_KEEP_RECENT_TOOL_CALLS,
            batch_size: 10,
            min_bytes: 2_000,
        }
    }
}

/// Agent 侧的装配配置。
#[derive(Debug, Clone)]
pub struct ContextEditCfg {
    pub enabled: bool,
    pub opts: ElideOptions,
    /// CAS 池。`None` 时不清理（无处安放原文）。
    pub cas: std::sync::Arc<WorkspaceCas>,
    /// 单次请求最多连续清理几批，防"历史里全是可清理项"时一次做掉一大手术、
    /// 把 prompt cache 前缀反复击穿。
    pub max_batches: usize,
    /// 软阈值占窗口的比例（来自 `[context_edit].soft_limit_ratio`）。
    /// `0.0` = 关闭软阈值，退回「只在硬阈值清理」的旧行为。
    pub soft_limit_ratio: f64,
    /// 统计：本次会话累计清理条数 / 释放 token。
    pub totals: std::sync::Arc<ContextEditTotals>,
}

/// 进程内累计统计（跨 Agent 重建保留，供 TUI 展示）。
#[derive(Debug, Default)]
pub struct ContextEditTotals {
    pub elided: std::sync::atomic::AtomicU32,
    pub freed_tokens: std::sync::atomic::AtomicU32,
}

impl ContextEditTotals {
    pub fn snapshot(&self) -> (u32, u32) {
        (
            self.elided.load(std::sync::atomic::Ordering::Relaxed),
            self.freed_tokens.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// 一次清理的结果。
#[derive(Debug, Clone, Default)]
pub struct ElisionStats {
    /// 实际被替换成占位的条数。
    pub elided: usize,
    /// 释放的估算 token。
    pub freed_tokens: u32,
    /// 新 `intern` 出来的 blob hash。**同一 hash 可能重复出现**——那对应同一份
    /// 内容被两个不同的占位引用，`ref_count` 加两次、`release` 也减两次，是对的。
    pub blobs: Vec<String>,
}

/// 从消息列表里建立 `tool_use_id → 工具名` 映射。
///
/// `ContentBlock::ToolResult` **不携带工具名**（只有 `tool_use_id`），所以保护规则
/// 只能靠回查对应的 `ToolUse` 块拿到名字。
///
/// 返回 owned map 是刻意的：调用方随后要可变借用 `session.messages` 做替换，
/// 借用 `&str` 会让借用检查器直接拒绝。
fn tool_name_index(messages: &[Message]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        if let ContentBlock::ToolUse { id, name, .. } = block {
            map.insert(id.clone(), name.clone());
        }
    }
    map
}

/// 一条 tool_use 的「可弃性」信号来源。
///
/// 只记**能判定价值**的几个字段：工具名、目标路径（Read/Edit/Write 的
/// `file_path`）、命令（Bash 的 `command`）。
///
/// ## 为什么需要它
///
/// `protect_recent = 3` 是**纯位置**语义：它只知道「最近 3 个」，不知道
/// 「这 3 个里有没有值得留的」。两种失效场景都很常见：
///
///   - 工具密集任务里那 3 条恰好是三次巨型 `Read`（收益被吃掉）；
///   - 纯对话里那 3 条毫无价值（保护窗口被浪费）。
///
/// 有了调用参数就能判断更细的价值：被后续 `Edit` 消费过的文件，
/// 其 `Read` 结果价值已经兑现（模型已经从它那里改对了文件）；同一路径
/// 后来又被 `Read` 过，旧的那份是冗余的。
#[derive(Debug, Clone)]
struct ToolCallInfo {
    /// `Read` / `Edit` / `Write` / `NotebookEdit` 的 `file_path`。
    ///
    /// 只记路径不记工具名：工具名已由 `is_elidable_tool` 在清理循环里
    /// 判过一轮，价值判定只关心「这次结果指向哪个文件」。
    path: Option<String>,
}

fn tool_call_index(messages: &[Message]) -> std::collections::HashMap<String, ToolCallInfo> {
    let mut map = std::collections::HashMap::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        let ContentBlock::ToolUse { id, input, .. } = block else {
            continue;
        };
        let path = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        map.insert(id.clone(), ToolCallInfo { path });
    }
    map
}

/// 某条工具结果的「已兑现」程度：后面是否已经有工具消费了它的产物。
///
/// 返回 true 的两种情形（都意味着这条结果的信息**已经被用掉了**）：
///
///   1. **被写操作消费**：`Read(f)` 之后出现了 `Edit(f)` / `Write(f)`。
///      模型已经从那次读取里提取出需要的东西并落地了，原文不再有增量价值。
///   2. **同路径重复读**：`Read(f)` 之后又 `Read(f)`。后一次是前一次的
///      超集（文件可能变了，新内容才是准的），旧的可以被新内容替代。
///
/// 注意判据只看向**后**：向前看会把「后来被改掉的文件」误判成已兑现——
/// 那次读取恰好是改动前的基线，价值最高。
fn is_fulfilled(
    positions: &[(usize, usize)],
    index: usize,
    calls: &std::collections::HashMap<String, ToolCallInfo>,
    messages: &[Message],
) -> bool {
    let (mi, bi) = positions[index];
    let Some(call) = tool_use_for_result(&messages[mi].content[bi], calls) else {
        return false;
    };
    let Some(path) = call.path.as_deref() else {
        return false;
    };
    // 收集本块之后的所有工具调用。
    let mut seen_later = false;
    'outer: for (offset, message) in messages[mi..].iter().enumerate() {
        for (later_bi, block) in message.content.iter().enumerate() {
            if offset == 0 && later_bi <= bi {
                continue;
            }
            let ContentBlock::ToolUse { name, input, .. } = block else {
                continue;
            };
            let later_path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str());
            if later_path != Some(path) {
                continue;
            }
            // 写操作消费了这次读取；或又一次读取使旧结果冗余。
            if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit" | "Read") {
                seen_later = true;
                break 'outer;
            }
        }
    }
    seen_later
}

/// 从 tool_result 块找回它对应的 tool_use 信息。
fn tool_use_for_result<'a>(
    block: &ContentBlock,
    calls: &'a std::collections::HashMap<String, ToolCallInfo>,
) -> Option<&'a ToolCallInfo> {
    let ContentBlock::ToolResult { tool_use_id, .. } = block else {
        return None;
    };
    calls.get(tool_use_id.as_str())
}

/// 找出所有 tool_result 的位置（消息下标 + 块下标），按出现顺序。
fn tool_result_positions(messages: &[Message]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (mi, message) in messages.iter().enumerate() {
        for (bi, block) in message.content.iter().enumerate() {
            if matches!(block, ContentBlock::ToolResult { .. }) {
                out.push((mi, bi));
            }
        }
    }
    out
}

/// 该 tool_result 是否已经是占位符（resume 后已还原的除外，见 materialize）。
fn is_elided(content: &ToolResultContent) -> bool {
    match content {
        ToolResultContent::Text(t) => t.starts_with(ELIDED_MARKER),
        // `Parts` 形态（图片 + 文本）也必须认：清理时文本段被换成了占位符，
        // 而图片段保持不变。不认的话会有两个后果：
        //   1. resume 时 `materialize_elided` 跳过它，原文永久只剩占位符；
        //   2. 下一轮清理把它再当成「未清理」重新 intern 一次，每轮一个新
        //      blob（占位符里带大小/首行，内容每轮都变）→ CAS 无界增长。
        ToolResultContent::Parts(parts) => parts.iter().any(|part| match part {
            ToolResultPart::Text { text, .. } => text.starts_with(ELIDED_MARKER),
            ToolResultPart::Image { .. } => false,
        }),
        ToolResultContent::Blocks(_) => false,
    }
}

/// 从 `Parts` 里取出占位符的 `cas://<hash>`（未清理 / 只有图片时返回 `None`）。
fn elided_hash_in_parts(parts: &[ToolResultPart]) -> Option<&str> {
    parts.iter().find_map(|part| match part {
        ToolResultPart::Text { text, .. } if text.starts_with(ELIDED_MARKER) => elided_hash(text),
        _ => None,
    })
}

/// 把一批过期工具结果外部化到 CAS，原地替换为带 `cas://` 的占位符。
///
/// 只处理 `Text` 与 `Blocks`：带图片的 `Parts` 不动（图片本身已经由
/// `serialize::externalize_block` 走 CAS，且还原路径要保持 image 语义）。
///
/// 返回 `Vec<(mi, bi)>` 之外还要把新 `intern` 的 hash 交给调用方记到 session 上，
/// 否则会话结束时无从 `release`，`WorkspaceCas::gc` 只删 `ref_count == 0` 的 blob，
/// 会变成无界增长。
pub fn elide_tool_results(
    session: &mut Session,
    cas: &WorkspaceCas,
    opts: &ElideOptions,
) -> ElisionStats {
    let mut stats = ElisionStats::default();
    if opts.batch_size == 0 {
        return stats;
    }

    let names = tool_name_index(&session.messages);
    let calls = tool_call_index(&session.messages);
    let positions = tool_result_positions(&session.messages);
    // 最近 protect_recent 个工具结果受保护（按出现顺序，不是按消息下标）。
    let protected_from = positions.len().saturating_sub(opts.protect_recent);

    let mut batched = 0usize;
    // 倒着遍历不了——要按「最旧优先」清理（Anthropic 语义），所以正向收集。
    for (idx, &(mi, bi)) in positions.iter().enumerate() {
        // 保护窗口的**位置语义保留**，但对「已兑现」的结果放宽一格：
        // 窗口内的结果若已被后续 Edit/Write 消费（或又被读过一次），
        // 它的信息已经用掉了，没有理由再占着保护额度。
        //
        // 只放宽不收缩：窗口外的结果一定可清，窗口内的看价值。这样
        // `protect_recent` 仍然是最坏情况下的上界——不会出现「本该保护的
        // 三条全被清掉」。
        // 注意这里是 `continue` 而不是 `break`：保护窗口是**逐条**判定的，
        // 窗口内某个结果未兑现不代表后面那个也未兑现。用 `break` 会让
        // 「窗口内第 1 个未兑现」把后面已兑现的全部挡在门外。
        let in_window = idx >= protected_from;
        if in_window && !is_fulfilled(&positions, idx, &calls, &session.messages) {
            continue;
        }
        if batched >= opts.batch_size {
            break;
        }
        let block = &session.messages[mi].content[bi];
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        else {
            continue;
        };
        if is_elided(content) {
            continue;
        }
        let Some(tool_name) = names.get(tool_use_id.as_str()) else {
            // 找不到对应的 ToolUse（历史会话文件可能缺块）：保守不清理。
            continue;
        };
        if !is_elidable_tool(tool_name) {
            continue;
        }

        let text = match content {
            ToolResultContent::Text(t) => t.clone(),
            ToolResultContent::Blocks(v) => serde_json::to_string(v).unwrap_or_default(),
            // Parts 里混着图片与文本：图片必须留着（模型要能看见），只把其中
            // 的文本段拼起来走同一条外部化路径。拼出来的哈希取回时是纯文本，
            // 足够——`ContextRecall` 的用途是召回**可读内容**。
            ToolResultContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ToolResultPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        if text.len() < opts.min_bytes {
            continue;
        }

        let Ok(hash) = cas.intern(text.as_bytes()) else {
            // 外部化失败**不阻断**：这一轮就跳过这条，下次阈值还会再触发。
            continue;
        };
        let before = crate::compact::estimate_block_tokens(block) as u32;
        let placeholder = render_placeholder(tool_name, text.len(), &text, &hash);

        let ContentBlock::ToolResult { content, .. } = &mut session.messages[mi].content[bi] else {
            unreachable!("上面已匹配过 ToolResult");
        };
        match content {
            ToolResultContent::Parts(parts) => {
                // 保留图片段，把**所有**文本段收进同一个占位符：只换第一段
                // 会让剩下的文本留在上下文里（既没真正外化，`freed_tokens`
                // 还按整块差值计），而每轮清理都会再 intern 一个新 blob。
                let mut replaced = false;
                for part in parts.iter_mut() {
                    if let ToolResultPart::Text { text, .. } = part {
                        if replaced {
                            text.clear();
                        } else {
                            *text = placeholder.clone();
                            replaced = true;
                        }
                    }
                }
                if !replaced {
                    // 只有图片的 `Parts`：内容没被外化，刚 intern 的 blob
                    // 没有任何引用者，必须立刻归还，否则引用计数只增不减。
                    let _ = cas.release(&hash);
                    continue;
                }
            }
            _ => *content = ToolResultContent::Text(placeholder),
        }
        let after = crate::compact::estimate_block_tokens(&session.messages[mi].content[bi]) as u32;

        stats.elided += 1;
        stats.freed_tokens = stats
            .freed_tokens
            .saturating_add(before.saturating_sub(after));
        stats.blobs.push(hash);
        batched += 1;
    }

    // 外部化是**原地改写**历史中的消息内容，不是追加。不通知 sink，增量日志
    // 里会永远留着那段被移出的大文本，重放时既占空间又与运行时状态不符。
    if stats.elided > 0 {
        session.emit_replaced();
    }
    stats
}

/// 占位符正文。**必须足够短**——它替代的是可能上万 token 的原文。
///
/// 携带工具名 / 字节数 / 首行，是为了让模型自己判断「值不值得召回」，而不是
/// 只看到一句冷冰冰的 "removed"（Cursor 的 context ring 与 Anthropic 的
/// `clear_tool_uses` 都在占位里保留可判断的信息）。
fn render_placeholder(tool_name: &str, bytes: usize, original: &str, hash: &str) -> String {
    let first_line = original
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("(empty)");
    // 按字节截断会切在 CJK/emoji 中间，先回退到最近的 char boundary。
    let first_line = &first_line[..crate::textutil::floor_char_boundary(first_line, 200)];
    format!(
        "{ELIDED_MARKER} {tool_name} 的结果已移出上下文以节省窗口。\n\
         - 工具: {tool_name}\n\
         - 大小: {bytes} bytes\n\
         - 首行: {first_line}\n\
         - 原文: cas://{hash}\n\
         需要完整内容时调用 ContextRecall 工具，参数 {{\"hash\": \"{hash}\"}}。"
    )
}

/// 从占位符里解析出 CAS hash。供 [`materialize_elided`] 与 `ContextRecall` 使用。
///
/// 注意占位符里那行是 `- 原文: cas://<hash>`——`cas://` **不在行首**，所以不能
/// 用 `strip_prefix`，必须在行内找子串再截到空白。
pub fn elided_hash(text: &str) -> Option<&str> {
    if !text.starts_with(ELIDED_MARKER) {
        return None;
    }
    const PREFIX: &str = crate::serialize::CAS_URI_PREFIX;
    text.lines().find_map(|line| {
        let start = line.find(PREFIX)? + PREFIX.len();
        let hash = line[start..].split_whitespace().next()?;
        (!hash.is_empty()).then_some(hash)
    })
}

/// 把上下文里的占位符还原成原文。
///
/// 调用点：`SessionStore::load` / checkpoint 恢复之后。语义是「**新会话有完整
/// 上下文**」——resume 后模型一开始能看见全部历史内容，随着会话增长再逐步被重新
/// 清理。落盘的 session 文件保持占位符形态（体积小），完整内容始终在 CAS 里，
/// 因此 transcript 不存在不可逆的信息损失。
pub fn materialize_elided(messages: &mut [Message], cas: Option<&WorkspaceCas>) -> usize {
    let Some(cas) = cas else {
        return 0;
    };
    let mut restored = 0usize;
    for block in messages.iter_mut().flat_map(|m| m.content.iter_mut()) {
        let ContentBlock::ToolResult { content, .. } = block else {
            continue;
        };
        match content {
            ToolResultContent::Text(text) => {
                let Some(hash) = elided_hash(text).map(str::to_string) else {
                    continue;
                };
                if restore_from_cas(cas, &hash, text) {
                    restored += 1;
                }
            }
            // `Parts` 形态：占位符只占据其中一段文本，图片段原样保留。
            // 不处理这条分支的话，resume 后模型看到的是占位符而不是原文，
            // 而文件里明明写着「CAS 里有完整内容」——两套语义不一致。
            ToolResultContent::Parts(parts) => {
                let Some(hash) = elided_hash_in_parts(parts).map(str::to_string) else {
                    continue;
                };
                for part in parts.iter_mut() {
                    if let ToolResultPart::Text { text, .. } = part {
                        if text.starts_with(ELIDED_MARKER) && restore_from_cas(cas, &hash, text) {
                            restored += 1;
                            break;
                        }
                    }
                }
            }
            ToolResultContent::Blocks(_) => {}
        }
    }
    restored
}

/// 从 CAS 取回原文写回占位符。返回是否真的还原。
fn restore_from_cas(cas: &WorkspaceCas, hash: &str, slot: &mut String) -> bool {
    match cas.get(hash) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(full) => {
                *slot = full;
                true
            }
            Err(error) => {
                tracing::warn!("CAS blob {hash} 不是合法 UTF-8，保留占位符: {error}");
                false
            }
        },
        Err(error) => {
            // blob 已被 gc：保留占位符，模型仍可看到"这里曾经有过内容"的
            // 线索并自行重跑工具，而不是看到空洞。
            tracing::debug!("CAS blob {hash} 已不存在，保留占位符: {error}");
            false
        }
    }
}

/// 从占位符取回原文（`ContextRecall` 工具的底层）。
pub fn recall_elided(cas: &WorkspaceCas, hash: &str) -> Result<String> {
    let bytes = cas
        .get(hash)
        .with_context(|| format!("recall elided tool result {hash}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 上下文里已占位化、且尚未还原的条数——供 `/context` 展示"已经省了多少"。
pub fn elided_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| match b {
            ContentBlock::ToolResult { content, .. } => is_elided(content),
            _ => false,
        })
        .count()
}

/// 释放本会话外部化过的 blob 引用。
///
/// 在会话真正被丢弃时调用（`/clear`、新会话、退出、resume 别的会话）。CAS 的
/// `gc` 只删 `ref_count == 0` 的 blob，不 release 就等于永不回收。
pub fn release_session_blobs(cas: Option<&WorkspaceCas>, blobs: &[String]) -> usize {
    let Some(cas) = cas else {
        return 0;
    };
    let mut released = 0usize;
    for hash in blobs {
        if let Err(error) = cas.release(hash) {
            tracing::warn!("释放 elided blob {hash} 失败: {error}");
            continue;
        }
        released += 1;
    }
    released
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_api::types::Role;

    fn temp_cas() -> (tempfile::TempDir, WorkspaceCas) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cas = WorkspaceCas::open(dir.path(), 16 * 1024 * 1024).expect("cas open");
        (dir, cas)
    }

    fn assistant_tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: ToolResultContent::text(text),
                is_error: false,
            }],
        }
    }

    /// 每条内容**互不相同**——CAS 按 SHA-256 去重，重复内容会塌成 1 个 blob，
    /// 那样就测不到多 blob 的释放路径了。
    fn big(i: usize) -> String {
        format!("match arm body #{i}\n").repeat(400) // ~6.4 KB，远超 min_bytes
    }

    fn session_with(tool: &str, count: usize) -> Session {
        let mut session = Session::new();
        for i in 0..count {
            session
                .messages
                .push(assistant_tool_use(&format!("t{i}"), tool));
            session
                .messages
                .push(tool_result(&format!("t{i}"), &big(i)));
        }
        session
    }

    /// 上下文里所有占位符的正文。
    fn placeholder_texts(session: &Session) -> Vec<String> {
        session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(t),
                    ..
                } if t.starts_with(ELIDED_MARKER) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn elides_only_the_oldest_beyond_protect_window() {
        let (_dir, cas) = temp_cas();
        let mut session = session_with("Grep", 6);
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());

        assert_eq!(stats.elided, 3, "6 个结果保留最近 3 个，其余 3 个被清理");
        assert!(stats.freed_tokens > 0);
        assert_eq!(stats.blobs.len(), 3);
        // 最近 3 个必须是原样
        for (_, content) in last_tool_results(&session, 3) {
            assert!(!is_elided(&content), "保护窗口内的结果不应被清理");
        }
    }

    fn last_tool_results(session: &Session, n: usize) -> Vec<(String, ToolResultContent)> {
        let mut all: Vec<(String, ToolResultContent)> = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some((tool_use_id.clone(), content.clone())),
                _ => None,
            })
            .collect();
        let start = all.len().saturating_sub(n);
        all.split_off(start)
    }

    /// 保护窗口内的结果，若它的信息**已被后续写操作消费**，可以不保护。
    ///
    /// 这条钉的是「位置语义」的具体失效场景：`Read(f)` → `Edit(f)` 之后，
    /// 那次读取的原文已经完成了它的价值（模型已经从它那里提取出要改的东西
    /// 并落地了），再占着 3 个保护名额只是在浪费清理额度。
    #[test]
    fn a_read_consumed_by_a_later_edit_leaves_the_protection_window() {
        let dir = tempfile::tempdir().unwrap();
        let cas = WorkspaceCas::open(dir.path(), 16 * 1024 * 1024).unwrap();
        let mut session = Session::new();

        // 3 条较早的工具结果（会被正常清掉）
        for i in 0..3 {
            push_read_pair(&mut session, &format!("old{i}.rs"), 5_000);
        }
        // 最近 3 个里的第 1 个：Read 后紧跟 Edit —— 已兑现
        push_read_pair(&mut session, "consumed.rs", 5_000);
        session.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "edit-1".into(),
                name: "Edit".into(),
                input: serde_json::json!({"file_path": "consumed.rs"}),
            }],
        });
        session.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "edit-1".into(),
                content: ToolResultContent::text("ok"),
                is_error: false,
            }],
        });
        // 最近 3 个里的另外 2 个：普通 Read，未兑现
        push_read_pair(&mut session, "keep1.rs", 5_000);
        push_read_pair(&mut session, "keep2.rs", 5_000);

        let opts = ElideOptions {
            protect_recent: 3,
            batch_size: 10,
            min_bytes: 1_000,
        };
        let stats = elide_tool_results(&mut session, &cas, &opts);

        // 3 条旧的 + 1 条已兑现的 = 4 条被清理。
        assert_eq!(
            stats.elided, 4,
            "旧的 3 条 + 已兑现的 1 条都应被清理，实际 {}",
            stats.elided
        );
        // 未兑现的两条必须原样保留（清理后文本会变成 `[context-edited]`
        // 占位符，所以「仍是原文」就等于「没被清理」）。
        let kept: Vec<String> = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(t),
                    ..
                } => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            kept.iter().any(|t| t.contains("keep1")),
            "未兑现的 keep1 不该被清理: {kept:?}"
        );
        assert!(
            kept.iter().any(|t| t.contains("keep2")),
            "未兑现的 keep2 不该被清理: {kept:?}"
        );
        // consumed.rs 的那次 Read 已兑现 → 原文被换成了占位符。
        let elided_count = kept.iter().filter(|t| t.starts_with(ELIDED_MARKER)).count();
        assert_eq!(elided_count, 4, "应有 4 条被占位化（3 旧 + 1 已兑现）");
    }

    /// 同路径重复读：后一次是前一次的超集，旧的那份冗余。
    #[test]
    fn a_reread_of_the_same_path_makes_the_older_result_redundant() {
        let dir = tempfile::tempdir().unwrap();
        let cas = WorkspaceCas::open(dir.path(), 16 * 1024 * 1024).unwrap();
        let mut session = Session::new();
        for i in 0..3 {
            push_read_pair(&mut session, &format!("old{i}.rs"), 5_000);
        }
        // 最近 3 个里：同路径读两次
        push_read_pair(&mut session, "twice.rs", 5_000);
        push_read_pair(&mut session, "twice.rs", 5_000);
        push_read_pair(&mut session, "solo.rs", 5_000);

        let opts = ElideOptions {
            protect_recent: 3,
            batch_size: 10,
            min_bytes: 1_000,
        };
        let stats = elide_tool_results(&mut session, &cas, &opts);
        assert_eq!(
            stats.elided, 4,
            "旧的 3 条 + 重复读的前一次应被清理，实际 {}",
            stats.elided
        );
    }

    /// 保护窗口内**没有**已兑现项时，行为必须与旧版逐字节一致——
    /// 不能因为引入价值判定就把保护额度悄悄缩小。
    #[test]
    fn an_unfulfilled_window_is_protected_exactly_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let cas = WorkspaceCas::open(dir.path(), 16 * 1024 * 1024).unwrap();
        let mut session = Session::new();
        for i in 0..3 {
            push_read_pair(&mut session, &format!("old{i}.rs"), 5_000);
        }
        for i in 0..3 {
            push_read_pair(&mut session, &format!("fresh{i}.rs"), 5_000);
        }

        let opts = ElideOptions {
            protect_recent: 3,
            batch_size: 10,
            min_bytes: 1_000,
        };
        let stats = elide_tool_results(&mut session, &cas, &opts);
        assert_eq!(stats.elided, 3, "只清旧的 3 条，最近 3 条全部保留");
    }

    /// 造一对 `Read(path)` → tool_result。
    fn push_read_pair(session: &mut Session, path: &str, bytes: usize) {
        let id = format!("read-{path}-{}", session.messages.len());
        session.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: "Read".into(),
                input: serde_json::json!({ "file_path": path }),
            }],
        });
        session.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: ToolResultContent::text(format!("{path} 的内容 {}", "x".repeat(bytes))),
                is_error: false,
            }],
        });
    }

    #[test]
    fn write_receipts_are_never_elided() {
        let (_dir, cas) = temp_cas();
        // 写操作回执被清理会让模型以为自己没改过 → 重做。
        for tool in ["Edit", "Write", "NotebookEdit"] {
            let mut session = session_with(tool, 6);
            let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
            assert_eq!(stats.elided, 0, "{tool} 的回执不应被清理");
        }
    }

    #[test]
    fn elidable_tools_are_covered() {
        for tool in [
            "Read",
            "Grep",
            "Glob",
            "CodeSearch",
            "Bash",
            "WebFetch",
            "WebSearch",
            "Agent",
            "mcp__postgres__query",
            "mcp__github__create_pr",
        ] {
            assert!(is_elidable_tool(tool), "{tool} 应在白名单里");
        }
        // 写操作回执被清理会让模型以为自己没改过 → 重做。
        for tool in ["Edit", "Write", "NotebookEdit", "Memory"] {
            assert!(!is_elidable_tool(tool), "{tool} 不应被清理");
        }
    }

    /// 核心往返：清理 → 占位可解析 → 原文能取回 → 还原后与原始完全一致。
    #[test]
    fn elide_recall_materialize_roundtrip() {
        let (_dir, cas) = temp_cas();
        let mut session = session_with("Bash", 6);
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(stats.elided, 3);

        // 占位符必须带可解析的 hash，且携带判断信息
        let placeholders = placeholder_texts(&session);
        assert_eq!(placeholders.len(), 3);
        let text = &placeholders[0];
        assert!(text.contains("Bash"), "占位符应标明工具名: {text}");
        assert!(
            text.contains("ContextRecall"),
            "占位符应告诉模型怎么召回: {text}"
        );
        let hash = elided_hash(text).expect("hash 应可解析").to_string();

        // 召回
        let recalled = recall_elided(&cas, &hash).expect("recall");
        assert!(recalled.contains("match arm body"));

        // 还原（resume 路径）
        let mut messages = session.messages.clone();
        let restored = materialize_elided(&mut messages, Some(&cas));
        assert_eq!(restored, 3);
        assert!(
            messages
                .iter()
                .flat_map(|m| m.content.iter())
                .any(|b| matches!(
                    b, ContentBlock::ToolResult { content: ToolResultContent::Text(t), .. }
                        if t.contains("match arm body")
                )),
            "还原后应出现完整原文"
        );
        assert!(
            placeholder_texts(&Session {
                messages,
                ..Default::default()
            })
            .is_empty(),
            "还原后不应再有占位符"
        );
    }

    /// 相同内容只存一份（CAS 按 SHA-256 去重），但 ref_count 仍然按引用次数记账。
    #[test]
    fn identical_contents_dedupe_but_still_count_refs() {
        let (_dir, cas) = temp_cas();
        let same = big(0);
        let mut session = Session::new();
        for i in 0..6 {
            session
                .messages
                .push(assistant_tool_use(&format!("t{i}"), "Grep"));
            session.messages.push(tool_result(&format!("t{i}"), &same));
        }
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(stats.elided, 3);
        assert_eq!(
            stats
                .blobs
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1,
            "内容相同应只产生 1 个 blob"
        );
        assert_eq!(
            stats.blobs.len(),
            3,
            "但 3 次引用都要记账，release 才对得上"
        );

        release_session_blobs(Some(&cas), &stats.blobs);
        assert_eq!(cas.gc(0).expect("gc").deleted_blobs, 1);
    }

    /// blob 已被 gc 时保留占位符（让模型知道"这里曾经有内容"并重跑），
    /// 而不是替换成空洞。
    #[test]
    fn materialize_keeps_placeholder_when_blob_is_gone() {
        let (_dir, cas) = temp_cas();
        let mut session = session_with("Read", 6);
        elide_tool_results(&mut session, &cas, &ElideOptions::default());
        let mut messages = session.messages.clone();

        // 手工删掉全部 blob 实体，模拟已被 gc 回收
        for hash in collect_elided_hashes(&messages) {
            let blob = cas
                .root()
                .join("sha256")
                .join(&hash[..2])
                .join(&hash[2..4])
                .join(format!("{hash}.blob"));
            std::fs::remove_file(&blob).expect("删除 blob 实体");
        }

        let restored = materialize_elided(&mut messages, Some(&cas));
        assert_eq!(restored, 0, "blob 不在时不应算作已还原");
        assert_eq!(
            collect_elided_hashes(&messages).len(),
            3,
            "blob 丢失时必须保留占位符，而不是变成空洞"
        );
    }

    fn collect_elided_hashes(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(t),
                    ..
                } => elided_hash(t).map(str::to_string),
                _ => None,
            })
            .collect()
    }

    /// 小结果不值得外部化：省下的 token 还不够占位符本身。
    #[test]
    fn skips_results_below_min_bytes() {
        let (_dir, cas) = temp_cas();
        let mut session = Session::new();
        for i in 0..6 {
            session
                .messages
                .push(assistant_tool_use(&format!("t{i}"), "Grep"));
            session
                .messages
                .push(tool_result(&format!("t{i}"), "3 matches"));
        }
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(stats.elided, 0);
    }

    /// 二次调用必须幂等：占位符不再被当成候选（否则会重复 intern、虚增 ref_count）。
    #[test]
    fn elide_is_idempotent() {
        let (_dir, cas) = temp_cas();
        let mut session = session_with("Grep", 6);
        let first = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        let second = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(first.elided, 3);
        assert_eq!(second.elided, 0, "占位符不应被再次清理");
        assert!(second.blobs.is_empty());
    }

    /// 找不到对应 ToolUse 块的历史会话必须被跳过，不能瞎清。
    #[test]
    fn skips_results_without_matching_tool_use() {
        let (_dir, cas) = temp_cas();
        let mut session = Session::new();
        for i in 0..6 {
            session
                .messages
                .push(tool_result(&format!("orphan{i}"), &big(i)));
        }
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(stats.elided, 0);
    }

    /// 策略级回归：超阈值时应能靠**清理**收敛，而不是必须去花一次摘要的
    /// LLM 往返。这条锁住"清理优先于摘要"这个顺序（Claude Code 的
    /// "clears older tool outputs first, then summarizes ... if needed"）。
    #[test]
    fn elision_alone_can_bring_a_history_back_under_threshold() {
        let (_dir, cas) = temp_cas();
        let mut session = Session::new();
        // 100 轮，每轮约 7.6KB / 2.5K token ≈ 250K token，远超 200K 窗口
        for i in 0..100 {
            session
                .messages
                .push(assistant_tool_use(&format!("t{i}"), "Bash"));
            session
                .messages
                .push(tool_result(&format!("t{i}"), &big(i)));
        }
        let window = 200_000u32;
        let threshold = window - crate::compact::compact_trigger_buffer(window, 8_192);
        let system = wyj_api::SystemPrompt::stable_only("system");
        let opts = ElideOptions::default();

        let before =
            crate::compact::ContextAudit::from_request(&system, &session.messages, &[], 8_192)
                .total();
        assert!(before > threshold, "构造的会话必须超阈值, 否则测试无意义");

        // 复刻 `Agent::elide_stale_tool_results`：单次请求最多 `max_batches` 批，
        // 每批后重估，达标即停。真实推理循环每个工具轮次都会重跑一次，所以这里
        // 模拟多次请求收敛。
        let max_batches = 3usize;
        let mut estimate = before;
        let mut requests = 0usize;
        while estimate > threshold && requests < 5 {
            for _ in 0..max_batches {
                if estimate <= threshold {
                    break;
                }
                let stats = elide_tool_results(&mut session, &cas, &opts);
                if stats.elided == 0 {
                    break;
                }
                estimate = crate::compact::ContextAudit::from_request(
                    &system,
                    &session.messages,
                    &[],
                    8_192,
                )
                .total();
            }
            requests += 1;
        }
        assert!(
            estimate <= threshold,
            "纯清理就应该能落回阈值内, 起始 {before} → 最终 {estimate} (阈值 {threshold}, \
             用了 {requests} 次请求)"
        );
        // 清理不该比摘要更贵：占位符总开销必须远小于省下的量
        let placeholders: usize = placeholder_texts(&session)
            .iter()
            .map(|t| crate::compact::estimate_text_tokens(t))
            .sum();
        assert!(
            placeholders * 10 < (before - estimate) as usize,
            "占位符开销 ({placeholders} tokens) 必须远小于释放量 ({})",
            before - estimate
        );
        // 保护窗口必须还在：模型刚读到的最后 3 条不能被清掉
        let tail = last_tool_results(&session, 3);
        assert!(
            tail.iter().all(|(_, c)| !is_elided(c)),
            "保护窗口内的最后 3 条工具结果必须保留全文"
        );
    }

    #[test]
    fn release_decrements_refcount_so_gc_can_reclaim() {
        let (_dir, cas) = temp_cas();
        let mut session = session_with("Bash", 6);
        let stats = elide_tool_results(&mut session, &cas, &ElideOptions::default());
        assert_eq!(stats.blobs.len(), 3);

        // 不 release：ref_count 仍为 1，gc(0) 删不掉
        let before = cas.gc(0).expect("gc");
        assert_eq!(before.deleted_blobs, 0, "未 release 的 blob 不该被回收");

        let released = release_session_blobs(Some(&cas), &stats.blobs);
        assert_eq!(released, 3);
        let after = cas.gc(0).expect("gc");
        assert_eq!(after.deleted_blobs, 3, "release 后应可被 gc 回收");
    }
}
