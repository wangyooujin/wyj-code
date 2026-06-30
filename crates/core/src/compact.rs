//! 上下文自动压缩：估算 token 用量，超限时调用 LLM 生成摘要替换旧消息。

use anyhow::Result;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, Message, Role, ToolResultContent},
};

use crate::session::Session;

/// 压缩触发缓冲（距上限此 token 数时触发）
pub const COMPACT_TRIGGER_BUFFER: u32 = 40_000;
/// 保留最近 N 条消息不压缩，确保上下文连续性
const COMPACT_KEEP_RECENT: usize = 6;

pub struct CompactResult {
    pub messages_removed: usize,
    pub tokens_saved_estimate: u32,
}

/// 粗略估算消息列表的 token 数。
/// 文本按字符数 / 3，工具 JSON 按字节数 / 4，图片 base64 按字节数 / 3。
pub fn estimate_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } => text.chars().count() / 3,
            ContentBlock::ToolUse { name, input, .. } => {
                name.chars().count() / 3 + input.to_string().len() / 4
            }
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => t.len() / 4,
                ToolResultContent::Blocks(v) => v.iter().map(|x| x.to_string().len() / 4).sum(),
            },
            ContentBlock::Image { data, .. } => data.len() / 3,
        })
        .sum::<usize>() as u32
}

/// 压缩会话：调用 LLM 生成摘要，替换旧消息，保留最近若干条。
pub async fn compact_session(
    session: &mut Session,
    provider: &dyn Provider,
    context_window: u32,
) -> Result<CompactResult> {
    let total = session.messages.len();
    if total <= COMPACT_KEEP_RECENT + 2 {
        anyhow::bail!("消息数量过少（{}条），无需压缩", total);
    }

    let keep_from = total.saturating_sub(COMPACT_KEEP_RECENT);
    let to_compact = &session.messages[..keep_from];
    let to_keep = session.messages[keep_from..].to_vec();

    let conv_text = messages_to_text(to_compact);
    let prompt = format!(
        "请为以下 AI 编程助手与用户的对话生成一份详细的中文摘要。\n\
        摘要需完整保留：已完成任务、关键技术决策、重要文件路径、\
        代码变更要点、当前进度和待完成事项，使后续对话可以无缝继续。\n\n\
        对话记录：\n{conv_text}"
    );

    let req = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: prompt }],
    }];

    // 摘要 token 上限：取上下文窗口的 1/10，但不超过 16000
    let summary_max_tokens = (context_window / 10).min(16_000);

    let result = provider
        .complete(
            "你是专业的技术对话摘要助手，输出结构清晰、信息完整的摘要。",
            &req,
            &[],
            summary_max_tokens,
        )
        .await?;

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

    let tokens_saved = estimate_tokens(to_compact);
    let messages_removed = to_compact.len();

    session.messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("[历史对话摘要 — 已压缩 {messages_removed} 条消息]\n\n{summary}"),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "已了解历史对话摘要，继续协助完成任务。".to_string(),
            }],
        },
    ];
    session.messages.extend(to_keep);

    Ok(CompactResult {
        messages_removed,
        tokens_saved_estimate: tokens_saved,
    })
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
                    ContentBlock::Text { text } => Some(truncate_chars(text, 600)),
                    ContentBlock::ToolUse { name, .. } => Some(format!("[工具调用: {name}]")),
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let text = match content {
                            ToolResultContent::Text(t) => truncate_chars(t, 400),
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
                })
                .collect();
            format!("[{role}]: {}", parts.join(" | "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}
