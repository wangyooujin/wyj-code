//! 持久化前的 `ContentBlock` 字节截断与脱敏工具。
//!
//! 目标:在不破坏 `SessionFile` / `Checkpoint` struct 与 branch/rewind/resume
//! 协议的前提下,**同时约束磁盘序列化字节与运行时 LLM 请求体积**——
//! `truncate_messages` 是三条路径(磁盘 save、内存 runtime、resume load)
//! 共用的入口,避免长 `ContentBlock::Text` / `ToolResult` / `Thinking` 在
//! 任何路径上无界累积、撞穿模型上下文窗口。
//!
//! 截断策略:`wyj_config::PersistCapCfg` 任意字段 = 0 即关闭对应截断,
//! 保持旧行为(向后兼容)。
//!
//! 复用 `wyj_tools::textutil` 的 UTF-8 安全 head+tail 截断语义;为避免
//! `wyj-core` 反向依赖 `wyj-tools`,这里 inline 等价实现(同款
//! `is_char_boundary` 回退),与 `crates/tools/src/textutil.rs` 保持
//! 行为一致——`truncate_head_tail("中".repeat(1000), 100, 100)` 必须
//! 产生同样的"头 100 字节 + `[truncated N bytes]` + 尾 100 字节"。

use crate::session_store::SessionFile;
use crate::workspace_cas::WorkspaceCas;
use std::sync::{Arc, Mutex};
use wyj_api::types::{ContentBlock as ApiContentBlock, Message, ToolResultContent, ToolResultPart};
use wyj_config::PersistCapCfg;

/// 按字节上限截断，保证不切断多字节字符。`s.len() <= max_bytes` 时原样借用返回。
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 保头保尾截断：超限时保留开头 `head_bytes` + 结尾 `tail_bytes`,
/// 中间以标记替代。适合命令输出(报错信息几乎总在尾部)。
fn truncate_head_tail(s: &str, head_bytes: usize, tail_bytes: usize) -> String {
    if s.len() <= head_bytes + tail_bytes {
        return s.to_string();
    }
    let head = truncate_str(s, head_bytes);
    let mut start = s.len() - tail_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let omitted = s.len() - head.len() - (s.len() - start);
    format!("{head}\n…[truncated {omitted} bytes]…\n{}", &s[start..])
}

/// 单次 `shrink_json_to_budget` 调用的最大裁剪轮数。
const SHRINK_MAX_ROUNDS: usize = 32;

/// 每轮多砍的余量字节。避免"只差 1 字节"时反复逼近产生死循环。
const SHRINK_SLACK: usize = 64;

/// 返回 `v` 子树里最长的 string 叶子长度；没有 string 叶子返回 `None`。
fn max_string_len(v: &serde_json::Value) -> Option<usize> {
    match v {
        serde_json::Value::String(s) => Some(s.len()),
        serde_json::Value::Array(a) => a.iter().filter_map(max_string_len).max(),
        serde_json::Value::Object(o) => o.values().filter_map(max_string_len).max(),
        _ => None,
    }
}

/// 在 `v` 子树里定位最长的 string 叶子并原地 head 截断到 `<= keep` 字节。
/// 返回是否真的发生了截断（`false` 表示已无可砍，调用方据此终止）。
///
/// 用下标两段式（先只读定位、再按下标取 `&mut`）而不是直接返回 `&mut`，
/// 是为了在 `Vec` / `Map` 迭代里避开借用检查器的跨迭代可变借用冲突。
fn truncate_longest_string(v: &mut serde_json::Value, keep: usize) -> bool {
    match v {
        serde_json::Value::String(s) => {
            if s.len() > keep {
                *s = truncate_str(s, keep).to_string();
                true
            } else {
                false
            }
        }
        serde_json::Value::Array(a) => {
            let mut best: Option<(usize, usize)> = None;
            for (i, item) in a.iter().enumerate() {
                if let Some(len) = max_string_len(item) {
                    if !matches!(&best, Some((bl, _)) if len <= *bl) {
                        best = Some((len, i));
                    }
                }
            }
            let Some((_, i)) = best else { return false };
            truncate_longest_string(&mut a[i], keep)
        }
        serde_json::Value::Object(o) => {
            let mut best: Option<(usize, String)> = None;
            for (k, item) in o.iter() {
                if let Some(len) = max_string_len(item) {
                    if !matches!(&best, Some((bl, _)) if len <= *bl) {
                        best = Some((len, k.clone()));
                    }
                }
            }
            let Some((_, k)) = best else { return false };
            match o.get_mut(&k) {
                Some(child) => truncate_longest_string(child, keep),
                None => false,
            }
        }
        _ => false,
    }
}

/// 把 `v` 裁剪到 `serde_json::to_string(v)` 的字节数 `<= budget`，
/// 且**全程保持合法 JSON**。
///
/// 存在的理由：`ContentBlock::ToolUse.input` 与 `ToolResultContent::Blocks`
/// 都是 JSON 值。对它们做字符串级 head+tail 截断会在中间插入
/// `[truncated N bytes]` 标记，使 `from_str` 必然失败——旧实现正是这样，
/// 而失败分支是 `if let Ok(value) = ...`，于是 `tool_use_input_bytes`(默认
/// 64 KiB) 从上线起就是一个**静默 no-op**，超限的 `Edit`/`Write` 参数
/// 既不落盘截断也不运行时截断，直接进请求体。见本文件 `mod tests` 里
/// `tool_use_input_over_cap_is_actually_truncated` 这个回归测试。
///
/// 策略：每轮把当前最长的 string 叶子按「超出量 + 余量」head 截断，
/// 重新序列化度量，最多 `SHRINK_MAX_ROUNDS` 轮。
///
/// 已知边界：若 `v` 里没有 string 叶子（例如几万个数字组成的数组），
/// 继续裁剪只能删字段、会改变工具语义，故原样保留并放弃。
/// 真实输入的字节量几乎全部来自 string 叶子（`Edit.new_string`、
/// `Write.content`、MCP 入参的长文本等），这个边界不影响实际效果。
fn shrink_json_to_budget(v: &mut serde_json::Value, budget: usize) {
    for _ in 0..SHRINK_MAX_ROUNDS {
        let Ok(serialized) = serde_json::to_string(v) else {
            return;
        };
        let size = serialized.len();
        if size <= budget {
            return;
        }
        let Some(longest) = max_string_len(v) else {
            return;
        };
        let keep = longest.saturating_sub((size - budget) + SHRINK_SLACK);
        if !truncate_longest_string(v, keep) {
            return;
        }
    }
}

/// 在落盘前对 `SessionFile.messages` 内 `ContentBlock` 做原地截断。
/// `SessionFile` struct 字段签名不变。
///
/// 调用方约定:**先**调 `extract_title` / `extract_preview` 拿到完整文本,
/// **再**调本函数截断 messages。这样 title/preview 仍是完整内容,
/// resume/branch 协议稳定。
pub fn truncate_session_for_persistence(file: &mut SessionFile, cfg: &PersistCapCfg) {
    truncate_messages(&mut file.messages, cfg);
}

/// 对任意 `Vec<Message>` / slice 复用同一截断实现。运行时 API 发送路径
/// (`Agent::run_turn_with_injection_inner`) 与 `SessionStore::load` 都通过
/// 本入口走同一条截断链,与 `truncate_session_for_persistence` 行为完全
/// 一致——保证 disk / runtime / resume 三条路径共用同一上限。
pub fn truncate_messages(messages: &mut [Message], cfg: &PersistCapCfg) {
    if !is_persist_cap_active(cfg) {
        return;
    }
    for msg in messages {
        for block in &mut msg.content {
            truncate_content_block(block, cfg);
        }
    }
}

fn is_persist_cap_active(cfg: &PersistCapCfg) -> bool {
    cfg.tool_result_head_bytes != 0
        || cfg.tool_result_tail_bytes != 0
        || cfg.thinking_bytes != 0
        || cfg.reasoning_details_bytes != 0
        || cfg.tool_use_input_bytes != 0
        || cfg.text_head_bytes != 0
        || cfg.text_tail_bytes != 0
}

pub fn truncate_content_block(block: &mut ApiContentBlock, cfg: &PersistCapCfg) {
    match block {
        ApiContentBlock::ToolResult { content, .. } => {
            truncate_tool_result(content, cfg);
        }
        ApiContentBlock::Text { text } => {
            if cfg.text_head_bytes > 0 || cfg.text_tail_bytes > 0 {
                *text = truncate_head_tail(text, cfg.text_head_bytes, cfg.text_tail_bytes);
            }
        }
        ApiContentBlock::Thinking {
            thinking,
            reasoning_details,
            ..
        } => {
            if cfg.thinking_bytes > 0 {
                *thinking = truncate_head_tail(thinking, cfg.thinking_bytes, 0);
            }
            if cfg.reasoning_details_bytes > 0 {
                if let Some(details) = reasoning_details.as_mut() {
                    for item in details.iter_mut() {
                        if let Some(obj) = item.as_object_mut() {
                            let Some(field) = obj.get_mut("text") else {
                                continue;
                            };
                            let owned =
                                std::mem::replace(field, serde_json::Value::String(String::new()));
                            if let serde_json::Value::String(s) = owned {
                                if let serde_json::Value::String(target) = field {
                                    *target =
                                        truncate_head_tail(&s, cfg.reasoning_details_bytes, 0);
                                }
                            }
                        }
                    }
                }
            }
        }
        ApiContentBlock::ToolUse { input, .. } => {
            if cfg.tool_use_input_bytes > 0 {
                shrink_json_to_budget(input, cfg.tool_use_input_bytes);
            }
        }
        ApiContentBlock::Image { .. } | ApiContentBlock::RedactedThinking { .. } => {}
    }
}

fn truncate_tool_result(content: &mut ToolResultContent, cfg: &PersistCapCfg) {
    match content {
        ToolResultContent::Text(text) => {
            if cfg.tool_result_head_bytes > 0 || cfg.tool_result_tail_bytes > 0 {
                *text = truncate_head_tail(
                    text,
                    cfg.tool_result_head_bytes,
                    cfg.tool_result_tail_bytes,
                );
            }
        }
        ToolResultContent::Parts(parts) => {
            for part in parts.iter_mut() {
                if let ToolResultPart::Text { text } = part {
                    if cfg.tool_result_head_bytes > 0 || cfg.tool_result_tail_bytes > 0 {
                        *text = truncate_head_tail(
                            text,
                            cfg.tool_result_head_bytes,
                            cfg.tool_result_tail_bytes,
                        );
                    }
                }
                // ToolResultPart::Image 不替换 data —— 避免引入运行时去重逻辑。
                // 仅 `display_text` 占位由工具层(textutil)负责,本函数只动 disk 序列化字节。
            }
        }
        ToolResultContent::Blocks(values) => {
            if cfg.tool_result_head_bytes > 0 || cfg.tool_result_tail_bytes > 0 {
                // 与 ToolUse 同理：Blocks 是 JSON 值，必须整体保持合法，
                // 预算按 head+tail 之和计（语义上与 text 分支的合计量一致）。
                // `shrink_json_to_budget` 操作 `Value`，这里临时包成
                // `Value::Array` 再取回。
                let budget = cfg
                    .tool_result_head_bytes
                    .saturating_add(cfg.tool_result_tail_bytes);
                let mut wrapper = serde_json::Value::Array(std::mem::take(values));
                shrink_json_to_budget(&mut wrapper, budget);
                if let serde_json::Value::Array(arr) = wrapper {
                    *values = arr;
                }
            }
        }
    }
}

// ==================== Phase 3: ContentBlock 外部化到 CAS ====================

/// 全局 CAS 引用,供 `externalize_block` 在落盘前把 image / 长 thinking 数据
/// 移到 CAS blob pool。None 时 externalize 跳过(等价于旧行为)。
/// 由 CLI 装配阶段注入(参考 `set_session_persist_cap`)。生产代码 set 一次,
/// 内部用 Mutex 是为了允许单测重置(OnceLock 只能 set 一次,不够灵活)。
static EXTERNALIZE_CAS: Mutex<Option<Arc<WorkspaceCas>>> = Mutex::new(None);

pub fn set_externalize_cas(cas: Option<Arc<WorkspaceCas>>) {
    *EXTERNALIZE_CAS.lock().expect("EXTERNALIZE_CAS poisoned") = cas;
}

pub fn current_externalize_cas() -> Option<Arc<WorkspaceCas>> {
    EXTERNALIZE_CAS
        .lock()
        .expect("EXTERNALIZE_CAS poisoned")
        .clone()
}

/// CAS 引用占位符前缀。`ToolResultPart::Image.data == "cas://<hash>..."` 表示
/// 真实 base64 数据已存入 CAS,调用方(agent 读 session)需 `cas.get()` 还原。
pub const CAS_URI_PREFIX: &str = "cas://";

/// 落盘前对超大字段(image / thinking)做 CAS 外部化。
/// 失败时返回原 block 不变(不阻断序列化)。
/// 阈值:base64 image > 32KB 或 thinking > 16KB 时外置。
/// `cas == None` 时不外置,等价于旧行为。`Some(cas)` 时用传入的 CAS,
/// 不读 global 状态 —— 避免测试并发跑时 global 互相覆盖。
pub fn externalize_block_with(block: &mut ApiContentBlock, cas: Option<&WorkspaceCas>) {
    let Some(cas) = cas else {
        return;
    };
    match block {
        ApiContentBlock::ToolResult {
            content: ToolResultContent::Parts(parts),
            ..
        } => {
            for part in parts.iter_mut() {
                if let ToolResultPart::Image { data, .. } = part {
                    if data.len() <= 32 * 1024 {
                        continue; // 阈值下不外置
                    }
                    match cas.intern(data.as_bytes()) {
                        Ok(hash) => {
                            *data = format!("{CAS_URI_PREFIX}{hash}");
                        }
                        Err(error) => {
                            tracing::warn!(
                                "CAS image externalize 失败 ({} bytes): {error}",
                                data.len()
                            );
                        }
                    }
                }
            }
        }
        ApiContentBlock::ToolResult { .. } => {}
        ApiContentBlock::Thinking { thinking, .. } => {
            if thinking.len() <= 16 * 1024 {
                return;
            }
            let original_len = thinking.len();
            match cas.intern(thinking.as_bytes()) {
                Ok(hash) => {
                    *thinking = format!(
                        "[externalized to cas://{}, {} bytes]",
                        &hash[..12],
                        original_len
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        "CAS thinking externalize 失败 ({} bytes): {error}",
                        original_len
                    );
                }
            }
        }
        _ => {}
    }
}

/// 兼容 wrapper:从 global CAS 读取(生产代码使用)。
pub fn externalize_block(block: &mut ApiContentBlock) {
    externalize_block_with(block, current_externalize_cas().as_deref());
}

/// 把 externalize 过的 block 还原(in-memory,resume 时用)。
/// `cas == None` 时不还原(等价于保持原样)。
pub fn materialize_block_with(block: &mut ApiContentBlock, cas: Option<&WorkspaceCas>) {
    let Some(cas) = cas else {
        return;
    };
    match block {
        ApiContentBlock::ToolResult {
            content: ToolResultContent::Parts(parts),
            ..
        } => {
            for part in parts.iter_mut() {
                if let ToolResultPart::Image { data, .. } = part {
                    if let Some(hash) = data.strip_prefix(CAS_URI_PREFIX) {
                        match cas.get(hash) {
                            Ok(bytes) => {
                                // 假定 data 是 base64 字符串;把原始字节重新 base64 编码
                                use base64::engine::general_purpose::STANDARD;
                                use base64::Engine as _;
                                *data = STANDARD.encode(&bytes);
                            }
                            Err(error) => {
                                tracing::warn!("CAS image materialize 失败 (hash={hash}): {error}");
                            }
                        }
                    }
                }
            }
        }
        ApiContentBlock::ToolResult { .. } => {}
        ApiContentBlock::Thinking { thinking, .. } => {
            if let Some(rest) = thinking.strip_prefix("[externalized to cas://") {
                if let Some(hash_end) = rest.find(',') {
                    let hash = &rest[..hash_end];
                    if let Ok(bytes) = cas.get(hash) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            *thinking = text;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// 兼容 wrapper:从 global CAS 读取(生产代码使用)。
pub fn materialize_block(block: &mut ApiContentBlock) {
    materialize_block_with(block, current_externalize_cas().as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_ascii() {
        assert_eq!(truncate_str("hello", 3), "hel");
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_multibyte_no_panic() {
        assert_eq!(truncate_str("中文", 4), "中");
        assert_eq!(truncate_str("中文", 2), "");
        assert_eq!(truncate_str("a中文", 3), "a");
        assert_eq!(truncate_str("🦀🦀", 5), "🦀");
    }

    #[test]
    fn head_tail_short_passthrough() {
        assert_eq!(truncate_head_tail("short", 100, 100), "short");
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        let input = "AAAA".repeat(100) + &"Z".repeat(100);
        let out = truncate_head_tail(&input, 40, 40);
        assert!(out.starts_with("AAAA"));
        assert!(out.ends_with(&"Z".repeat(40)));
        assert!(out.contains("[truncated"));
    }

    /// 回归 v1.5.12 修的「366K text input 撞穿 262K 上下文窗口」bug。
    /// 长 assistant 总结、用户粘贴日志在 `ContentBlock::Text` 里无界累积
    /// 是直接根因——这条 test 锁住 `truncate_content_block` 对 Text 块做
    /// head+tail 截断的行为,确保未来不会因为重构把 Text 重新漏出。
    #[test]
    fn truncate_text_block_applies_head_and_tail() {
        let mut block = ApiContentBlock::Text {
            text: "A".repeat(20_000) + &"Z".repeat(20_000),
        };
        let cfg = PersistCapCfg {
            tool_result_head_bytes: 0,
            tool_result_tail_bytes: 0,
            thinking_bytes: 0,
            reasoning_details_bytes: 0,
            tool_use_input_bytes: 0,
            text_head_bytes: 100,
            text_tail_bytes: 50,
        };
        truncate_content_block(&mut block, &cfg);
        match block {
            ApiContentBlock::Text { text } => {
                assert!(text.starts_with(&"A".repeat(100)));
                assert!(text.ends_with(&"Z".repeat(50)));
                assert!(text.contains("[truncated"));
                assert!(text.len() < 40_000, "Text 块必须被实际截断");
            }
            _ => panic!("expected Text block"),
        }
    }

    /// opt-out 行为:任一字段 = 0 即关闭对应截断(向后兼容)。
    /// 用户把 `text_head_bytes = 0` 时,长 Text 块必须原样保留——与
    /// v1.5.10 之前的行为完全一致,不破坏老用户的运行预期。
    #[test]
    fn truncate_text_block_skipped_when_caps_are_zero() {
        let original = "x".repeat(50_000);
        let mut block = ApiContentBlock::Text {
            text: original.clone(),
        };
        let cfg = PersistCapCfg {
            tool_result_head_bytes: 0,
            tool_result_tail_bytes: 0,
            thinking_bytes: 0,
            reasoning_details_bytes: 0,
            tool_use_input_bytes: 0,
            text_head_bytes: 0,
            text_tail_bytes: 0,
        };
        truncate_content_block(&mut block, &cfg);
        match block {
            ApiContentBlock::Text { text } => assert_eq!(text.len(), original.len()),
            _ => panic!("expected Text block"),
        }
    }

    /// `truncate_messages` (新增的 slice 入口) 与
    /// `truncate_session_for_persistence` 必须行为完全一致——disk /
    /// runtime / resume 三条路径共用同一上限的安全契约。
    #[test]
    fn truncate_messages_matches_session_path() {
        let mut session_file = SessionFile {
            session_id: "test".to_string(),
            title: "测试".to_string(),
            last_preview: "preview".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            turns: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            tool_schema_tokens: 0,
            tool_schema_tokens_saved: 0,
            prompt_cache_state: crate::session_store::prompt_cache_state::OFF,
            messages: vec![Message {
                role: wyj_api::types::Role::User,
                content: vec![ApiContentBlock::Text {
                    text: "Y".repeat(30_000),
                }],
            }],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            context_edit_freed_tokens: 0,
        };
        let mut messages = session_file.messages.clone();
        let cfg = PersistCapCfg {
            tool_result_head_bytes: 0,
            tool_result_tail_bytes: 0,
            thinking_bytes: 0,
            reasoning_details_bytes: 0,
            tool_use_input_bytes: 0,
            text_head_bytes: 64,
            text_tail_bytes: 32,
        };
        truncate_session_for_persistence(&mut session_file, &cfg);
        truncate_messages(&mut messages, &cfg);
        // 两条路径必须产出等价长度(head+tail+truncated marker 拼接)
        let from_file = match &session_file.messages[0].content[0] {
            ApiContentBlock::Text { text } => text.len(),
            _ => panic!(),
        };
        let from_slice = match &messages[0].content[0] {
            ApiContentBlock::Text { text } => text.len(),
            _ => panic!(),
        };
        assert_eq!(from_file, from_slice);
        assert!(from_file < 30_000);
    }

    #[test]
    fn head_tail_multibyte_no_panic() {
        let input = "中".repeat(1000);
        let out = truncate_head_tail(&input, 100, 100);
        assert!(out.contains("[truncated"));
        assert!(out.starts_with('中'));
        assert!(out.ends_with('中'));
    }

    // ==================== Phase 3 externalize tests ====================

    use super::{externalize_block_with, materialize_block_with};
    use crate::workspace_cas::WorkspaceCas;
    use wyj_api::types::{ContentBlock, ToolResultContent, ToolResultPart};

    fn make_test_cas() -> (tempfile::TempDir, std::sync::Arc<WorkspaceCas>) {
        let dir = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(WorkspaceCas::open(dir.path(), 1024 * 1024).unwrap());
        (dir, cas)
    }

    #[test]
    fn externalize_image_above_threshold_to_cas() {
        let (_dir, cas) = make_test_cas();
        let big_data = "A".repeat(64 * 1024); // 64KB,超过 32KB 阈值
        let mut block = ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: ToolResultContent::Parts(vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: big_data.clone(),
            }]),
            is_error: false,
        };
        externalize_block_with(&mut block, Some(cas.as_ref()));
        let ContentBlock::ToolResult { content, .. } = &block else {
            panic!()
        };
        let ToolResultContent::Parts(parts) = content else {
            panic!()
        };
        let ToolResultPart::Image { data, .. } = &parts[0] else {
            panic!()
        };
        assert!(
            data.starts_with("cas://"),
            "data 应该是 cas:// 引用: {data}"
        );
        let hash = &data[6..];
        assert_eq!(hash.len(), 64);
        // CAS 应有 1 个 blob
        let stats = cas.stats().unwrap();
        assert_eq!(stats.total_blobs, 1);
        assert_eq!(stats.total_bytes, 64 * 1024);
    }

    #[test]
    fn externalize_image_below_threshold_keeps_inline() {
        let (_dir, cas) = make_test_cas();
        let small_data = "B".repeat(1024); // 1KB,低于 32KB 阈值
        let mut block = ContentBlock::ToolResult {
            tool_use_id: "t2".to_string(),
            content: ToolResultContent::Parts(vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: small_data.clone(),
            }]),
            is_error: false,
        };
        externalize_block_with(&mut block, Some(cas.as_ref()));
        let ContentBlock::ToolResult { content, .. } = &block else {
            panic!()
        };
        let ToolResultContent::Parts(parts) = content else {
            panic!()
        };
        let ToolResultPart::Image { data, .. } = &parts[0] else {
            panic!()
        };
        assert_eq!(data, &small_data, "阈值下不外置,保持 inline");
    }

    #[test]
    fn externalize_thinking_above_threshold_to_cas() {
        let (_dir, cas) = make_test_cas();
        let long = "X".repeat(20 * 1024); // 20KB,超过 16KB
        let mut block = ContentBlock::Thinking {
            thinking: long.clone(),
            signature: String::new(),
            reasoning_details: None,
        };
        externalize_block_with(&mut block, Some(cas.as_ref()));
        let ContentBlock::Thinking { thinking, .. } = &block else {
            panic!()
        };
        assert!(
            thinking.starts_with("[externalized to cas://"),
            "thinking 应被外置,实际: {thinking}"
        );
    }

    #[test]
    fn materialize_roundtrip_restores_image() {
        let (_dir, cas) = make_test_cas();
        let original_data = "P".repeat(64 * 1024);
        let original_bytes = original_data.as_bytes().to_vec();
        let mut block = ContentBlock::ToolResult {
            tool_use_id: "t3".to_string(),
            content: ToolResultContent::Parts(vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: original_data.clone(),
            }]),
            is_error: false,
        };
        externalize_block_with(&mut block, Some(cas.as_ref()));
        // 此时 data 是 cas://<hash>
        let ContentBlock::ToolResult { content, .. } = block.clone() else {
            panic!()
        };
        let ToolResultContent::Parts(parts) = content else {
            panic!()
        };
        let ToolResultPart::Image { data, .. } = &parts[0] else {
            panic!()
        };
        assert!(data.starts_with("cas://"));
        // materialize 应从 CAS 还原
        materialize_block_with(&mut block, Some(cas.as_ref()));
        let ContentBlock::ToolResult { content, .. } = &block else {
            panic!()
        };
        let ToolResultContent::Parts(parts) = content else {
            panic!()
        };
        let ToolResultPart::Image { data: restored, .. } = &parts[0] else {
            panic!()
        };
        // 还原后是 base64 编码
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let decoded = STANDARD.decode(restored).unwrap();
        assert_eq!(decoded, original_bytes);
    }

    #[test]
    fn externalize_without_cas_is_noop() {
        // 传 None → externalize 跳过,保持 inline
        let mut block = ContentBlock::ToolResult {
            tool_use_id: "t4".to_string(),
            content: ToolResultContent::Parts(vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: "Z".repeat(64 * 1024),
            }]),
            is_error: false,
        };
        externalize_block_with(&mut block, None);
        let ContentBlock::ToolResult { content, .. } = &block else {
            panic!()
        };
        let ToolResultContent::Parts(parts) = content else {
            panic!()
        };
        let ToolResultPart::Image { data, .. } = &parts[0] else {
            panic!()
        };
        assert!(!data.starts_with("cas://"));
    }

    // ── shrink_json_to_budget：persist_cap 对 JSON 值的裁剪 ──────────────
    //
    // 背景：ToolUse.input 与 ToolResultContent::Blocks 都是 JSON 值。旧实现
    // 对它们做字符串级 head+tail 截断后再 from_str，中间插入的
    // `[truncated N bytes]` 标记使解析必然失败，而失败分支是
    // `if let Ok(...)`，于是 persist_cap 这两条上限从上线起就是静默 no-op。
    // 下面第一个测试是针对该缺陷的回归测试。

    #[test]
    fn shrink_json_keeps_valid_json_and_meets_budget() {
        let mut v = serde_json::json!({
            "file_path": "/tmp/a.rs",
            "new_string": "x".repeat(5000),
        });
        assert!(serde_json::to_string(&v).unwrap().len() > 512);
        shrink_json_to_budget(&mut v, 512);
        let out = serde_json::to_string(&v).unwrap();
        assert!(out.len() <= 512, "serialized = {}", out.len());
        // 关键断言：裁剪后必须仍是可解析的 Value，且非超大字段没被动过
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back["file_path"], "/tmp/a.rs");
        assert!(back["new_string"].as_str().unwrap().len() < 5000);
    }

    #[test]
    fn shrink_json_prefers_longest_string_and_is_char_boundary_safe() {
        let mut v = serde_json::json!({
            "short": "ab",
            "long": "中".repeat(2000),
            "nested": { "deep": "y".repeat(100) },
        });
        shrink_json_to_budget(&mut v, 800);
        let out = serde_json::to_string(&v).unwrap();
        assert!(out.len() <= 800, "serialized = {}", out.len());
        // 裁剪只该动最长的 string 叶子
        assert_eq!(v["short"], "ab");
        assert_eq!(v["nested"]["deep"], "y".repeat(100));
        // 头截断回退到 char boundary，to_string 不 panic 即说明没切断字符
        assert!(v["long"].as_str().unwrap().len() < 6000);
    }

    #[test]
    fn shrink_json_gives_up_when_no_string_leaves() {
        // 已知边界：全是数字的大数组无法在不删语义的前提下裁剪，应原样保留。
        let arr: Vec<serde_json::Value> = (0..5000u32).map(serde_json::Value::from).collect();
        let mut v = serde_json::Value::Array(arr);
        let before = serde_json::to_string(&v).unwrap().len();
        shrink_json_to_budget(&mut v, 100);
        let after = serde_json::to_string(&v).unwrap();
        assert_eq!(after.len(), before);
        assert!(serde_json::from_str::<serde_json::Value>(&after).is_ok());
    }

    #[test]
    fn tool_use_input_over_cap_is_actually_truncated() {
        let mut block = ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "Write".to_string(),
            input: serde_json::json!({ "content": "z".repeat(300_000) }),
        };
        let cfg = PersistCapCfg {
            tool_use_input_bytes: 4096,
            ..Default::default()
        };
        truncate_content_block(&mut block, &cfg);
        let ContentBlock::ToolUse { input, .. } = &block else {
            panic!("expected ToolUse")
        };
        let out = serde_json::to_string(input).unwrap();
        assert!(
            out.len() <= 4096,
            "tool_use input serialized = {}",
            out.len()
        );
        // 保留的头部内容仍在，不是被整个丢弃
        assert!(out.contains('z'));
    }

    #[test]
    fn tool_result_blocks_over_cap_is_actually_truncated() {
        let mut block = ContentBlock::ToolResult {
            tool_use_id: "t2".to_string(),
            content: ToolResultContent::Blocks(vec![serde_json::json!({
                "k": "q".repeat(200_000)
            })]),
            is_error: false,
        };
        let cfg = PersistCapCfg {
            tool_result_head_bytes: 2048,
            tool_result_tail_bytes: 1024,
            ..Default::default()
        };
        truncate_content_block(&mut block, &cfg);
        let ContentBlock::ToolResult { content, .. } = &block else {
            panic!("expected ToolResult")
        };
        let ToolResultContent::Blocks(values) = content else {
            panic!("expected Blocks")
        };
        let out = serde_json::to_string(values).unwrap();
        assert!(out.len() <= 3072, "blocks serialized = {}", out.len());
    }
}
