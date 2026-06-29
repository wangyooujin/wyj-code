//! Anthropic Messages API 供应商实现。
//! 协议依据：https://docs.anthropic.com/en/api/messages

use crate::{
    provider::{EventStream, Provider},
    types::{ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wyj_config::Config;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        let api_key = cfg.api_key()?.to_string();
        let base_url = cfg.resolved_base_url().trim_end_matches('/').to_string();
        Ok(Self {
            client: Client::new(),
            api_key,
            base_url,
            model: cfg.model.clone(),
        })
    }
}

// ── 请求/响应类型 ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ApiMessage {
    role: &'static str,
    content: Vec<ApiContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: Value, is_error: bool },
    Image { source: ImageSource },
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: &'static str,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct ApiTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

// SSE 事件负载
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseEvent {
    MessageStart {
        message: MessageStartData,
    },
    ContentBlockStart {
        #[allow(dead_code)]
        index: usize,
        content_block: ContentBlockStart,
    },
    ContentBlockDelta {
        #[allow(dead_code)]
        index: usize,
        delta: BlockDelta,
    },
    ContentBlockStop {
        #[allow(dead_code)]
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaData,
        #[allow(dead_code)]
        usage: Option<UsageData>,
    },
    MessageStop,
    Ping,
    Error {
        error: Value,
    },
}

#[derive(Deserialize, Debug)]
struct MessageStartData {
    usage: Option<UsageData>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockStart {
    Text {
        #[allow(dead_code)]
        text: String,
    },
    ToolUse { id: String, name: String },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

#[derive(Deserialize, Debug)]
struct MessageDeltaData {
    stop_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct UsageData {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

// ── 内部模型 → API 请求转换 ───────────────────────────────────────────────────

fn to_api_messages(messages: &[Message]) -> Vec<ApiMessage> {
    messages.iter().map(|m| {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content = m.content.iter().map(|b| match b {
            ContentBlock::Text { text } => ApiContentBlock::Text { text: text.clone() },
            ContentBlock::ToolUse { id, name, input } => ApiContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                let val = match content {
                    crate::types::ToolResultContent::Text(t) => Value::String(t.clone()),
                    crate::types::ToolResultContent::Blocks(b) => Value::Array(b.clone()),
                };
                ApiContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: val,
                    is_error: *is_error,
                }
            }
            ContentBlock::Image { media_type, data } => ApiContentBlock::Image {
                source: ImageSource {
                    source_type: "base64",
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
            },
        }).collect();
        ApiMessage { role, content }
    }).collect()
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::Other,
    }
}

// ── Provider 实现 ─────────────────────────────────────────────────────────────

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<EventStream> {
        let body = ApiRequest {
            model: &self.model,
            max_tokens,
            system,
            messages: to_api_messages(messages),
            tools: tools
                .iter()
                .map(|t| ApiTool {
                    name: &t.name,
                    description: &t.description,
                    input_schema: &t.input_schema,
                })
                .collect(),
            stream: true,
        };
        // api_tools 借用 tools 的引用，不能随 body 一起 move，重新序列化
        let body_value = serde_json::to_value(&body).context("序列化请求失败")?;

        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body_value)
            .send()
            .await
            .context("发送 Anthropic 请求失败")?;

        let status = resp.status();
        if !status.is_success() {
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API 错误 {status}: {err}");
        }

        let byte_stream = resp.bytes_stream();
        let sse = byte_stream.eventsource();

        let stream = sse.filter_map(|item| async move {
            let event = match item {
                Ok(e) => e,
                Err(e) => return Some(Err(anyhow::anyhow!("SSE 读取失败: {e}"))),
            };
            if event.data == "[DONE]" {
                return None;
            }
            let parsed: SseEvent = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("SSE 解析跳过: {e} data={}", event.data);
                    return None;
                }
            };
            match parsed {
                SseEvent::MessageStart { message } => {
                    if let Some(usage) = message.usage {
                        return Some(Ok(StreamEvent::Usage {
                            input_tokens: usage.input_tokens.unwrap_or(0),
                            output_tokens: usage.output_tokens.unwrap_or(0),
                        }));
                    }
                    None
                }
                SseEvent::ContentBlockStart { content_block, .. } => match content_block {
                    ContentBlockStart::ToolUse { id, name } => {
                        Some(Ok(StreamEvent::ToolUseStart { id, name }))
                    }
                    ContentBlockStart::Text { .. } => None,
                },
                SseEvent::ContentBlockDelta { delta, .. } => match delta {
                    BlockDelta::TextDelta { text } => Some(Ok(StreamEvent::TextDelta(text))),
                    BlockDelta::InputJsonDelta { partial_json } => {
                        // id 需从外部跟踪；此处简化：delta 不携带 id，由消费者按顺序关联
                        // 实际 id 可通过 index 映射，这里用空字符串占位，消费者自行维护
                        Some(Ok(StreamEvent::ToolUseDelta {
                            id: String::new(),
                            json_delta: partial_json,
                        }))
                    }
                },
                SseEvent::ContentBlockStop { .. } => None,
                SseEvent::MessageDelta { delta, .. } => {
                    let stop_reason = delta
                        .stop_reason
                        .as_deref()
                        .map(parse_stop_reason)
                        .unwrap_or(StopReason::EndTurn);
                    Some(Ok(StreamEvent::MessageStop { stop_reason }))
                }
                SseEvent::MessageStop | SseEvent::Ping => None,
                SseEvent::Error { error } => {
                    Some(Err(anyhow::anyhow!("Anthropic 流式错误: {error}")))
                }
            }
        });

        Ok(Box::pin(stream))
    }
}
