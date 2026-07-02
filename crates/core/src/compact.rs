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
///
/// 改进点（对比旧版 `chars/3`）：旧版对中文严重低估（中文约 1-2 token/字，
/// `/3` 只估 0.33，偏低 3-6 倍），导致压缩触发过晚、真实上下文可能溢出。
/// 现采用启发式：CJK 字符按 1.5 token/字，其余按 0.25 token/字（≈4 字符/token），
/// 对中文和代码混合场景更接近真实值，英文场景略微高估（安全方向，触发偏早）。
pub fn estimate_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } => estimate_text_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => {
                estimate_text_tokens(name) + estimate_text_tokens(&input.to_string())
            }
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => estimate_text_tokens(t),
                ToolResultContent::Blocks(v) => {
                    v.iter().map(|x| estimate_text_tokens(&x.to_string())).sum()
                }
            },
            ContentBlock::Image { data, .. } => data.len() / 3,
        })
        .sum::<usize>() as u32
}

/// 启发式 token 估算：CJK 字符按 1.5 token/字，其余按 0.25 token/字。
fn estimate_text_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (cjk * 3 / 2) + (other / 4)
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

    let keep_from = safe_keep_from(&session.messages, COMPACT_KEEP_RECENT)
        .ok_or_else(|| anyhow::anyhow!("找不到安全的压缩边界，暂不压缩"))?;

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

/// 判断消息是否为"真实用户发言"边界（而非工具结果回传）：
/// 只有此类消息可以安全作为压缩截断点，否则会拆散 tool_use/tool_result 配对。
fn is_user_turn_boundary(msg: &Message) -> bool {
    matches!(msg.role, Role::User)
        && !msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// 从「保留最近 N 条」的朴素截断点回退到最近的真实用户发言边界：按固定条数
/// 截断可能正好切在工具结果消息或助手消息上，这样拼接摘要
/// （[User(摘要), Assistant(确认), ...保留部分]）时要么破坏 user/assistant
/// 角色交替、要么拆散 tool_use/tool_result 配对，导致压缩后请求被 Provider
/// 拒绝或模型上下文残缺——表现为"压缩后模型不知道该做什么"。
/// 第一条消息必然是真实用户发言，故只要 messages 非空就不会返回 `None`
/// （除非连第一条都不满足，理论上不可能发生）。
fn safe_keep_from(messages: &[Message], keep_recent: usize) -> Option<usize> {
    let total = messages.len();
    let mut keep_from = total.saturating_sub(keep_recent);
    while keep_from > 0 && !is_user_turn_boundary(&messages[keep_from]) {
        keep_from -= 1;
    }
    if keep_from == 0 && !messages.is_empty() && !is_user_turn_boundary(&messages[0]) {
        return None;
    }
    Some(keep_from)
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
                        // 跳过历史中残留的 CLAUDE.md <system-reminder> 块（旧版本注入
                        // 到 user 消息的遗留），避免摘要里混入记忆文件碎片。
                        if text.contains("<system-reminder>") {
                            None
                        } else {
                            Some(truncate_chars(text, 600))
                        }
                    }
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

    /// 朴素按固定条数截断会正好切在 assistant(tool_use) 之后、
    /// user(tool_result) 之前——回退逻辑必须往前找到更早的真实用户发言，
    /// 而不是把 keep_from 落在工具结果消息或助手消息上。
    #[test]
    fn safe_keep_from_backs_off_tool_result_boundary() {
        let messages = vec![
            user_text("请帮我读一下这个文件"), // 0: 真实用户边界
            assistant_tool_use(),              // 1
            user_tool_result(),                // 2
            assistant_tool_use(),              // 3
            user_tool_result(),                // 4
            assistant_tool_use(),              // 5
            user_tool_result(),                // 6
        ];
        // 朴素 keep_from = 7 - 3 = 4，正好落在 user_tool_result 上（idx 4）
        let keep_from = safe_keep_from(&messages, 3).expect("应能找到安全边界");
        assert!(
            is_user_turn_boundary(&messages[keep_from]),
            "回退后的截断点必须是真实用户发言，而非 tool_result 或 assistant 消息"
        );
        assert_eq!(keep_from, 0, "本例中只有 idx 0 是真实用户发言边界");
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
}
