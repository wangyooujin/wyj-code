//! Tool-result context editing：把过期的工具输出外部化到 CAS 并在上下文里留占位。
//!
//! # 为什么不是「直接删」
//!
//! 整段摘要把 20K 的 Grep 输出压成一行 `[工具输出] xxx…`，而摘要最先丢的恰好是
//! **负知识**（被否决的假设）、**精确值**（路径/行号/常量）、**顺序因果**——见
//! `doc/plan/v1.5.17-context-management-plan.md` 的失败模式清单。清理比摘要便宜
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
use wyj_api::types::{ContentBlock, Message, ToolResultContent};

use crate::session::Session;
use crate::workspace_cas::WorkspaceCas;

/// 占位符首行标记。既是给模型看的说明，也是 resume 时识别「这条是占位」的锚点。
pub const ELIDED_MARKER: &str = "[context-edited]";

/// 允许被外部化清理的工具白名单。
///
/// 用白名单而非黑名单是刻意的：新增工具默认**不**被清理，需要显式加进来。清理的
/// 误删代价（模型以为自己没做过某事）远高于漏删的收益。
///
/// - `Read` / `Grep` / `Glob`：确定性只读，丢了可以重跑，但重跑也有代价（时间 +
///   token），能召回就比重跑好。
/// - `Bash` / `WebFetch`：一次性执行，**重跑拿不回原文**，是外部化收益最高、
///   也最需要 `ContextRecall` 的一类。
pub const ELIDABLE_TOOLS: &[&str] = &["Read", "Grep", "Glob", "Bash", "WebFetch"];

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

/// 工具名是否在允许清理的白名单里。
pub fn is_elidable(tool_name: &str) -> bool {
    ELIDABLE_TOOLS.contains(&tool_name)
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
        _ => false,
    }
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
    let positions = tool_result_positions(&session.messages);
    // 最近 protect_recent 个工具结果受保护（按出现顺序，不是按消息下标）。
    let protected_from = positions.len().saturating_sub(opts.protect_recent);

    let mut batched = 0usize;
    // 倒着遍历不了——要按「最旧优先」清理（Anthropic 语义），所以正向收集。
    for (idx, &(mi, bi)) in positions.iter().enumerate() {
        if idx >= protected_from {
            break;
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
        if !is_elidable(tool_name) {
            continue;
        }

        let text = match content {
            ToolResultContent::Text(t) => t.clone(),
            ToolResultContent::Blocks(v) => serde_json::to_string(v).unwrap_or_default(),
            ToolResultContent::Parts(_) => continue,
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
        *content = ToolResultContent::Text(placeholder);
        let after = crate::compact::estimate_block_tokens(&session.messages[mi].content[bi]) as u32;

        stats.elided += 1;
        stats.freed_tokens = stats
            .freed_tokens
            .saturating_add(before.saturating_sub(after));
        stats.blobs.push(hash);
        batched += 1;
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
        let ToolResultContent::Text(text) = content else {
            continue;
        };
        let Some(hash) = elided_hash(text).map(str::to_string) else {
            continue;
        };
        match cas.get(&hash) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(full) => {
                    *text = full;
                    restored += 1;
                }
                Err(error) => {
                    tracing::warn!("CAS blob {hash} 不是合法 UTF-8，保留占位符: {error}");
                }
            },
            Err(error) => {
                // blob 已被 gc：保留占位符，模型仍可看到"这里曾经有过内容"的
                // 线索并自行重跑工具，而不是看到空洞。
                tracing::debug!("CAS blob {hash} 已不存在，保留占位符: {error}");
            }
        }
    }
    restored
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
        for tool in ["Read", "Grep", "Glob", "Bash", "WebFetch"] {
            assert!(is_elidable(tool), "{tool} 应在白名单里");
        }
        for tool in ["Edit", "Write", "Memory", "Agent"] {
            assert!(!is_elidable(tool), "{tool} 不应被清理");
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
