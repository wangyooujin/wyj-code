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
}
