//! 持久化前的 `ContentBlock` 字节截断与脱敏工具。
//!
//! 目标:在不破坏 `SessionFile` / `Checkpoint` struct 与 branch/rewind/resume
//! 协议的前提下,**仅减小磁盘序列化字节**;内存中完整 messages 仍可用于下
//! 一次 LLM 请求,只在下一次 `save()` / `create()` 时再截断。
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

/// 在落盘前对 `SessionFile.messages` 内 `ContentBlock` 做原地截断。
/// `SessionFile` struct 字段签名不变。
///
/// 调用方约定:**先**调 `extract_title` / `extract_preview` 拿到完整文本,
/// **再**调本函数截断 messages。这样 title/preview 仍是完整内容,
/// resume/branch 协议稳定。
pub fn truncate_session_for_persistence(file: &mut SessionFile, cfg: &PersistCapCfg) {
    if cfg.tool_result_head_bytes == 0
        && cfg.tool_result_tail_bytes == 0
        && cfg.thinking_bytes == 0
        && cfg.reasoning_details_bytes == 0
        && cfg.tool_use_input_bytes == 0
    {
        return;
    }
    for msg in &mut file.messages {
        truncate_message(msg, cfg);
    }
}

fn truncate_message(msg: &mut Message, cfg: &PersistCapCfg) {
    for block in &mut msg.content {
        truncate_content_block(block, cfg);
    }
}

pub fn truncate_content_block(block: &mut ApiContentBlock, cfg: &PersistCapCfg) {
    match block {
        ApiContentBlock::ToolResult { content, .. } => {
            truncate_tool_result(content, cfg);
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
                let raw = serde_json::to_string(input).unwrap_or_default();
                if raw.len() > cfg.tool_use_input_bytes {
                    let truncated = truncate_head_tail(&raw, cfg.tool_use_input_bytes, 0);
                    if let Ok(value) = serde_json::from_str(&truncated) {
                        *input = value;
                    }
                }
            }
        }
        ApiContentBlock::Text { .. }
        | ApiContentBlock::Image { .. }
        | ApiContentBlock::RedactedThinking { .. } => {}
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
                if let Ok(serialized) = serde_json::to_string(values) {
                    if serialized.len() > cfg.tool_result_head_bytes + cfg.tool_result_tail_bytes {
                        let truncated = truncate_head_tail(
                            &serialized,
                            cfg.tool_result_head_bytes,
                            cfg.tool_result_tail_bytes,
                        );
                        if let Ok(parsed) =
                            serde_json::from_str::<Vec<serde_json::Value>>(&truncated)
                        {
                            *values = parsed;
                        }
                    }
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

fn current_externalize_cas() -> Option<Arc<WorkspaceCas>> {
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
                                tracing::warn!(
                                    "CAS image materialize 失败 (hash={hash}): {error}"
                                );
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
        assert!(data.starts_with("cas://"), "data 应该是 cas:// 引用: {data}");
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
}
