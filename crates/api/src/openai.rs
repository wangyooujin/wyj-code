//! OpenAI Chat Completions API 供应商实现。
//! 协议依据：https://platform.openai.com/docs/api-reference/chat

use crate::{
    provider::{EventStream, Provider},
    types::{ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition},
};
use anyhow::Result;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wyj_config::Config;

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    stream_options: bool,
}

impl OpenAIProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::with_model(cfg, &cfg.active_profile().model.clone())
    }

    pub fn with_model(cfg: &Config, model: &str) -> Result<Self> {
        let api_key = cfg.api_key()?.to_string();
        let base_url = cfg.resolved_base_url().trim_end_matches('/').to_string();
        Ok(Self {
            client: Client::new(),
            api_key,
            base_url,
            model: model.to_string(),
            stream_options: cfg
                .active_profile()
                .effective_openai_stream_options_for_model(model),
        })
    }
}

// ── 请求/响应类型 ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: ApiFunctionCall,
}

#[derive(Serialize)]
struct ApiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ApiTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: ApiFunctionDef,
}

#[derive(Serialize)]
struct ApiFunctionDef {
    name: String,
    description: String,
    parameters: Value,
}

// SSE chunk 类型
#[derive(Deserialize, Debug)]
struct SseChunk {
    choices: Vec<Choice>,
    usage: Option<UsageData>,
}

#[derive(Deserialize, Debug)]
struct Choice {
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize, Debug)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize, Debug)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize, Debug)]
struct UsageData {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    /// OpenAI 服务端自动缓存的命中数（prompt_tokens 已包含它，需要扣减）
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize, Debug)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
}

#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    started: bool,
}

// ── 内部模型 → API 请求转换 ───────────────────────────────────────────────────

fn to_api_messages(messages: &[Message]) -> Vec<ApiMessage> {
    let mut out = vec![];
    for m in messages {
        match m.role {
            Role::User => {
                let mut text_parts = vec![];
                let mut tool_results = vec![];
                for block in &m.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            // OpenAI 的 tool 消息不支持图片块：Parts 走
                            // display_text 降级（图片以占位符表示），第一期接受此限制
                            let content_str = match content {
                                crate::types::ToolResultContent::Text(t) => t.clone(),
                                crate::types::ToolResultContent::Parts(_) => content.display_text(),
                                crate::types::ToolResultContent::Blocks(b) => {
                                    serde_json::to_string(b).unwrap_or_default()
                                }
                            };
                            tool_results.push(ApiMessage {
                                role: "tool".to_string(),
                                content: Some(Value::String(content_str)),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                        _ => {}
                    }
                }
                if !tool_results.is_empty() {
                    out.extend(tool_results);
                } else {
                    out.push(ApiMessage {
                        role: "user".to_string(),
                        content: Some(Value::String(text_parts.join(""))),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            Role::Assistant => {
                let mut text_parts = vec![];
                let mut tool_calls = vec![];
                for block in &m.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(ApiToolCall {
                                id: id.clone(),
                                call_type: "function",
                                function: ApiFunctionCall {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input).unwrap_or_default(),
                                },
                            });
                        }
                        _ => {}
                    }
                }
                out.push(ApiMessage {
                    role: "assistant".to_string(),
                    content: if text_parts.is_empty() {
                        None
                    } else {
                        Some(Value::String(text_parts.join("")))
                    },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                });
            }
        }
    }
    out
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

/// 将供应商返回的 usage 原样转换为内部账本事件。`prompt_tokens` 是供应商针对
/// 实际序列化请求计算的值，包含 system、tool schema 与消息包装，因此它比本地
/// 启发式估算更适合作为 MiniMax / GLM / DeepSeek 的精确用量来源。
fn usage_event(usage: UsageData) -> StreamEvent {
    // OpenAI 兼容 API 的 prompt_tokens 含缓存命中部分；内部账本沿用 Anthropic
    // 语义，把未缓存输入与 cache read 分开存，但两者之和仍是精确 prompt token 数。
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .unwrap_or(0);
    let prompt = usage.prompt_tokens.unwrap_or(0);
    StreamEvent::Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: usage.completion_tokens.unwrap_or(0),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    }
}

/// 一个 SSE chunk 可能同时携带 finish_reason 与 usage。旧实现只在 choices 为空
/// 时读取 usage，导致部分 OpenAI 兼容服务的精确 token 被静默丢弃。
fn stream_events_from_chunk(
    tool_map: &mut std::collections::HashMap<usize, PendingToolCall>,
    chunk: SseChunk,
) -> Vec<StreamEvent> {
    let mut events = vec![];

    if let Some(choice) = chunk.choices.into_iter().next() {
        if let Some(fr) = choice.finish_reason {
            events.push(StreamEvent::MessageStop {
                stop_reason: parse_stop_reason(&fr),
            });
        } else if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                events.push(StreamEvent::TextDelta(text));
            }
        } else if let Some(tcs) = choice.delta.tool_calls {
            for tc in tcs {
                let idx = tc.index;
                let pending = tool_map.entry(idx).or_default();
                if let Some(id) = tc.id {
                    pending.id = Some(id);
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        pending.name = Some(name);
                    }
                    if !pending.started {
                        if let (Some(id), Some(name)) = (pending.id.clone(), pending.name.clone()) {
                            pending.started = true;
                            events.push(StreamEvent::ToolUseStart { id, name });
                        }
                    }
                    if let Some(args) = func.arguments {
                        if let Some(id) = pending.id.clone() {
                            events.push(StreamEvent::ToolUseDelta {
                                id,
                                json_delta: args,
                            });
                        }
                    }
                }
            }
        }
    }

    if let Some(usage) = chunk.usage {
        events.push(usage_event(usage));
    }

    events
}

// ── Provider 实现 ─────────────────────────────────────────────────────────────

#[async_trait]
impl Provider for OpenAIProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        opts: &crate::provider::RequestOptions,
    ) -> Result<EventStream> {
        // OpenAI 格式不支持 Anthropic 式 thinking 参数，忽略 opts.thinking_*
        let max_tokens = opts.max_tokens;
        let mut api_messages = vec![ApiMessage {
            role: "system".to_string(),
            content: Some(Value::String(system.to_string())),
            tool_calls: None,
            tool_call_id: None,
        }];
        api_messages.extend(to_api_messages(messages));

        // 原生工具（如 Anthropic computer-use）无 description/input_schema，
        // 且 OpenAI Chat Completions 不支持该调用形态：过滤掉。这类工具只在
        // Anthropic profile 下注册，此处仅作防御性兜底。
        let api_tools: Vec<ApiTool> = tools
            .iter()
            .filter(|t| t.native.is_none())
            .map(|t| ApiTool {
                tool_type: "function",
                function: ApiFunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();

        let body = ApiRequest {
            model: self.model.clone(),
            messages: api_messages,
            tools: api_tools,
            max_tokens,
            stream: true,
            stream_options: self.stream_options.then_some(StreamOptions {
                include_usage: true,
            }),
        };

        let url = format!("{}/chat/completions", self.base_url);
        // 连接前阶段带指数退避重试（429/5xx/连接错误），流未开始消费，重试透明
        let resp =
            crate::retry::send_with_retry(&crate::retry::RetryPolicy::default(), "OpenAI", || {
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("content-type", "application/json")
                    .json(&body)
            })
            .await?;

        let byte_stream = resp.bytes_stream();
        let sse = byte_stream.eventsource();

        // scan 维护 tool_id_map；一个 SSE chunk 可拆成多个内部事件（例如终止原因
        // 与精确 usage 同时出现），随后 flat_map 按原顺序输出。
        let stream = sse
            .scan(
                std::collections::HashMap::<usize, PendingToolCall>::new(),
                |tool_map, item| {
                    let events: Option<Vec<Result<StreamEvent>>> = match item {
                        Ok(event) if event.data == "[DONE]" => None,
                        Ok(event) => match serde_json::from_str::<SseChunk>(&event.data) {
                            Ok(chunk) => Some(
                                stream_events_from_chunk(tool_map, chunk)
                                    .into_iter()
                                    .map(Ok)
                                    .collect(),
                            ),
                            Err(e) => {
                                tracing::debug!("OpenAI SSE 跳过: {e}");
                                Some(vec![])
                            }
                        },
                        Err(_) => Some(vec![Err(anyhow::Error::new(
                            crate::error::ProviderError::new(
                                crate::error::ProviderErrorKind::StreamTruncated,
                                "provider SSE stream ended unexpectedly",
                            ),
                        ))]),
                    };
                    futures::future::ready(events)
                },
            )
            .flat_map(futures::stream::iter);

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_chunk_keeps_usage_for_exact_token_accounting() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta::default(),
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(UsageData {
                prompt_tokens: Some(120),
                completion_tokens: Some(30),
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(40),
                }),
            }),
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert!(matches!(
            events[0],
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn
            }
        ));
        assert!(matches!(
            events[1],
            StreamEvent::Usage {
                input_tokens: 80,
                output_tokens: 30,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 0,
            }
        ));
    }

    #[test]
    fn one_chunk_can_emit_tool_start_and_arguments() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: None,
                    tool_calls: Some(vec![ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_string()),
                        function: Some(FunctionDelta {
                            name: Some("Read".to_string()),
                            arguments: Some(r#"{"path":"Cargo.toml"}"#.to_string()),
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert!(matches!(
            events[0],
            StreamEvent::ToolUseStart { ref id, ref name } if id == "call_1" && name == "Read"
        ));
        assert!(matches!(
            events[1],
            StreamEvent::ToolUseDelta { ref id, ref json_delta }
                if id == "call_1" && json_delta == r#"{"path":"Cargo.toml"}"#
        ));
    }
}
