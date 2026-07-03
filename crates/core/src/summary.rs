//! 会话摘要生成：首轮对话结束后调用 LLM 生成简短标题，
//! 写回 SessionFile.title 并标记 title_generated = true。
//! 之后固定不再重新生成（对齐 Claude Code 行为）。

use std::sync::Arc;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, Message, Role},
};

use crate::session_store::SessionStore;

/// 会话标题生成器：持有 SessionStore 引用，生成后直接写盘。
pub struct SummaryGenerator {
    store: Arc<SessionStore>,
    provider: Arc<dyn Provider>,
}

impl SummaryGenerator {
    pub fn new(store: Arc<SessionStore>, provider: Arc<dyn Provider>) -> Self {
        Self { store, provider }
    }

    /// 首轮后生成标题并更新 session 文件的 title 字段。
    /// 若该会话已生成过标题（title_generated == true）则跳过。
    /// 返回生成的标题（Some）或 None（跳过/失败/无内容）。
    pub async fn generate_title(&self, session_id: &str, messages: &[Message]) -> Option<String> {
        // 加载现有 SessionFile，检查是否已生成过标题
        let mut file = self.store.load(session_id).ok()?;

        if file.title_generated {
            return None; // 已生成，跳过
        }

        // 提取首条 user 文字 + 末条 assistant 文字作为摘要输入
        let user_text = first_user_text(messages);
        let assistant_text = last_assistant_text(messages);
        if user_text.is_empty() && assistant_text.is_empty() {
            return None;
        }

        let title = self.call_llm(&user_text, &assistant_text).await?;

        // 写回 SessionFile（保留其他字段，仅更新 title + title_generated）
        file.title = title.clone();
        file.title_generated = true;
        if let Err(e) = self.store.save(&file) {
            tracing::debug!("标题写盘失败: {e}");
            return None;
        }

        Some(title)
    }

    /// 调用 LLM 生成标题，返回清理后的标题字符串。
    async fn call_llm(&self, user_text: &str, assistant_text: &str) -> Option<String> {
        let system = "你是对话标题生成器。根据用户的首条消息和助手回复，生成一个简洁的中文标题。\
                      要求：不超过20个字，不要引号、不要句号、不要换行，直接输出标题文本。";

        let prompt =
            format!("用户消息：{user_text}\n\n助手回复：{assistant_text}\n\n请生成对话标题：");

        let req = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }];

        let result = self
            .provider
            .complete(
                system,
                &req,
                &[],
                &wyj_api::provider::RequestOptions::text_only(256),
            )
            .await
            .ok()?;

        let raw: String = result
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
            .join("")
            .trim()
            .to_string();

        if raw.is_empty() {
            return None;
        }

        Some(clean_title(&raw))
    }
}

/// 取首条 user 文字消息（截断 500 字符）
fn first_user_text(messages: &[Message]) -> String {
    for msg in messages {
        if matches!(msg.role, Role::User) {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    let t = text.trim();
                    if !t.is_empty() {
                        return truncate_chars(t, 500);
                    }
                }
            }
        }
    }
    String::new()
}

/// 取末条 assistant 文字消息（截断 500 字符）
fn last_assistant_text(messages: &[Message]) -> String {
    for msg in messages.iter().rev() {
        if matches!(msg.role, Role::Assistant) {
            for block in msg.content.iter().rev() {
                if let ContentBlock::Text { text } = block {
                    let t = text.trim();
                    if !t.is_empty() {
                        return truncate_chars(t, 500);
                    }
                }
            }
        }
    }
    String::new()
}

/// 清理 LLM 输出的标题：去引号/句号/换行/首尾空白，截断 60 字符
fn clean_title(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| *c != '"' && *c != '\'' && *c != '\n' && *c != '\r' && *c != '。' && *c != '.')
        .collect();
    let trimmed = cleaned.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() > 60 {
        format!("{}…", chars[..59].iter().collect::<String>())
    } else {
        trimmed.to_string()
    }
}

/// 截断字符串到指定字符数
fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > max {
        chars[..max].iter().collect()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_title_removes_quotes() {
        assert_eq!(clean_title("\"修复表格边框\""), "修复表格边框");
        assert_eq!(clean_title("'添加单元测试'"), "添加单元测试");
    }

    #[test]
    fn test_clean_title_removes_period() {
        assert_eq!(clean_title("修复了bug。"), "修复了bug");
        assert_eq!(clean_title("Added feature."), "Added feature");
    }

    #[test]
    fn test_clean_title_removes_newlines() {
        assert_eq!(clean_title("标题\n第二行"), "标题第二行");
    }

    #[test]
    fn test_clean_title_truncates_long() {
        let long = "一".repeat(70);
        let result = clean_title(&long);
        assert_eq!(result.chars().count(), 60);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_clean_title_preserves_short() {
        assert_eq!(clean_title("简短标题"), "简短标题");
    }

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("你好世界", 2), "你好");
    }

    #[test]
    fn test_first_user_text() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "  ".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "帮我写冒泡排序".to_string(),
                }],
            },
        ];
        assert_eq!(first_user_text(&msgs), "帮我写冒泡排序");
    }

    #[test]
    fn test_first_user_text_empty() {
        let msgs: Vec<Message> = vec![];
        assert_eq!(first_user_text(&msgs), "");
    }

    #[test]
    fn test_last_assistant_text() {
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "第一轮回复".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "第二轮回复".to_string(),
                }],
            },
        ];
        assert_eq!(last_assistant_text(&msgs), "第二轮回复");
    }
}
