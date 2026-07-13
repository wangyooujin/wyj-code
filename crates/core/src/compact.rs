//! 上下文自动压缩：估算 token 用量，超限时调用 LLM 生成摘要替换旧消息。

use anyhow::Result;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, Message, Role, ToolDefinition, ToolResultContent},
};

use crate::session::Session;

/// 压缩触发缓冲（距上限此 token 数时触发）
pub const COMPACT_TRIGGER_BUFFER: u32 = 40_000;
/// 保留最近 N 条消息不压缩，确保上下文连续性
const COMPACT_KEEP_RECENT: usize = 6;

pub fn compact_trigger_buffer(context_window: u32) -> u32 {
    40_000.min((context_window / 5).max(4_000))
}

pub struct CompactResult {
    pub messages_removed: usize,
    pub tokens_saved_estimate: u32,
}

/// 估算一整次模型请求占用的上下文，而不是只看会话历史。
///
/// 自动压缩的目标是避免下一次 `Provider::stream` 超出模型窗口；该请求实际还会
/// 携带 system prompt、工具定义，并要为输出预留 `max_tokens`。所有供应商的分词
/// 器和消息包装开销并不相同，因此这仍是保守估算，但覆盖面比只估 messages 完整。
pub fn estimate_request_tokens(
    system: &str,
    messages: &[Message],
    tools: &[ToolDefinition],
    max_output_tokens: u32,
) -> u32 {
    const REQUEST_OVERHEAD_TOKENS: u32 = 64;
    const MESSAGE_OVERHEAD_TOKENS: u32 = 4;

    let system_tokens = estimate_text_tokens(system) as u32;
    let message_overhead = (messages.len() as u32).saturating_mul(MESSAGE_OVERHEAD_TOKENS);
    let tool_tokens = tools.iter().fold(0u32, |total, tool| {
        let schema = tool.input_schema.to_string();
        total.saturating_add(
            (estimate_text_tokens(&tool.name)
                + estimate_text_tokens(&tool.description)
                + estimate_text_tokens(&schema)) as u32,
        )
    });

    estimate_tokens(messages)
        .saturating_add(system_tokens)
        .saturating_add(tool_tokens)
        .saturating_add(message_overhead)
        .saturating_add(REQUEST_OVERHEAD_TOKENS)
        .saturating_add(max_output_tokens)
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
        })
        .sum::<usize>() as u32
}

/// 图片 token 估算：Anthropic 按约 像素数/750 计 token。无尺寸信息时用解码
/// 后字节数近似（base64 长度 × 3/4 ÷ 750），1600 封顶（API 对大图会缩放）。
/// 旧公式 `data.len()/3` 对大图高估约百倍，会导致压缩过早触发。
fn estimate_image_tokens(b64_len: usize) -> usize {
    (b64_len * 3 / 4 / 750).min(1600)
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

    let conv_text = messages_to_text(to_compact);
    let prompt = crate::prompts::compact_prompt(&conv_text);

    let req = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: prompt }],
    }];

    // 摘要 token 上限：取上下文窗口的 1/10，但不超过 16000
    let summary_max_tokens = (context_window / 10).min(16_000);

    let result = provider
        .complete(
            crate::prompts::COMPACT_SYSTEM,
            &req,
            &[],
            &wyj_api::provider::RequestOptions::text_only(summary_max_tokens),
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

    let messages_removed = to_compact.len();

    if reset_entire_session {
        session.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: crate::prompts::compact_summary_message(messages_removed, &summary),
            }],
        }];
    } else {
        session.messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: crate::prompts::compact_summary_message(messages_removed, &summary),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: crate::prompts::COMPACT_ACK.to_string(),
                }],
            },
        ];
        session.messages.extend(to_keep);
    }

    let tokens_saved = before_tokens.saturating_sub(estimate_tokens(&session.messages));

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
                            ToolResultContent::Parts(_) => {
                                truncate_chars(&content.display_text(), 400)
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
                    // 思考内容不进摘要（内部推理，非对话事实）
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => None,
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

    #[test]
    fn compact_trigger_buffer_scales_with_context_window() {
        assert_eq!(compact_trigger_buffer(200_000), 40_000);
        assert_eq!(compact_trigger_buffer(32_000), 6_400);
        assert_eq!(compact_trigger_buffer(8_000), 4_000);
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
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<wyj_api::provider::EventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn complete(
            &self,
            _system: &str,
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
        }];

        let message_only = estimate_tokens(&messages);
        let full_request = estimate_request_tokens(
            "system instruction ".repeat(40).as_str(),
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
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000)
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
        let result = compact_session(&mut session, &StaticSummaryProvider, 200_000)
            .await
            .expect("normal multi-turn session should compact");
        let after = estimate_tokens(&session.messages);

        assert_eq!(result.tokens_saved_estimate, before - after);
        assert!(result.messages_removed > 0);
        assert!(after < before);
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
