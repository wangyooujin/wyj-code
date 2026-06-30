//! OpenAI Chat Completions API 供应商实现。
//! 协议依据：https://platform.openai.com/docs/api-reference/chat

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

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenAIProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::with_model(cfg, &cfg.model.clone())
    }

    pub fn with_model(cfg: &Config, model: &str) -> Result<Self> {
        let api_key = cfg.api_key()?.to_string();
        let base_url = cfg.resolved_base_url().trim_end_matches('/').to_string();
        Ok(Self {
            client: Client::new(),
            api_key,
            base_url,
            model: model.to_string(),
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
    stream_options: StreamOptions,
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
                            let content_str = match content {
                                crate::types::ToolResultContent::Text(t) => t.clone(),
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

// ── Provider 实现 ─────────────────────────────────────────────────────────────

#[async_trait]
impl Provider for OpenAIProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<EventStream> {
        let mut api_messages = vec![ApiMessage {
            role: "system".to_string(),
            content: Some(Value::String(system.to_string())),
            tool_calls: None,
            tool_call_id: None,
        }];
        api_messages.extend(to_api_messages(messages));

        let api_tools: Vec<ApiTool> = tools
            .iter()
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
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("发送 OpenAI 请求失败")?;

        let status = resp.status();
        if !status.is_success() {
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API 错误 {status}: {err}");
        }

        let byte_stream = resp.bytes_stream();
        let sse = byte_stream.eventsource();

        // scan 维护 tool_id_map 状态：闭包始终返回 Some(Option<Event>) 防止提前终止流
        // filter_map 剥掉内层 None（跳过无关帧）
        let stream = sse
            .scan(
                std::collections::HashMap::<usize, String>::new(),
                |tool_id_map, item| {
                    let result: Option<Result<StreamEvent>> = (|| {
                        let event = match item {
                            Ok(e) => e,
                            Err(e) => return Some(Err(anyhow::anyhow!("SSE 读取失败: {e}"))),
                        };
                        if event.data == "[DONE]" {
                            return None;
                        }
                        let chunk: SseChunk = match serde_json::from_str(&event.data) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::debug!("OpenAI SSE 跳过: {e}");
                                return None;
                            }
                        };

                        if chunk.choices.is_empty() {
                            if let Some(usage) = chunk.usage {
                                return Some(Ok(StreamEvent::Usage {
                                    input_tokens: usage.prompt_tokens.unwrap_or(0),
                                    output_tokens: usage.completion_tokens.unwrap_or(0),
                                }));
                            }
                            return None;
                        }

                        let choice = chunk.choices.into_iter().next()?;

                        if let Some(fr) = choice.finish_reason {
                            return Some(Ok(StreamEvent::MessageStop {
                                stop_reason: parse_stop_reason(&fr),
                            }));
                        }

                        if let Some(text) = choice.delta.content {
                            if !text.is_empty() {
                                return Some(Ok(StreamEvent::TextDelta(text)));
                            }
                        }

                        if let Some(tcs) = choice.delta.tool_calls {
                            for tc in tcs {
                                let idx = tc.index;
                                if let Some(func) = tc.function {
                                    if let (Some(id), Some(name)) =
                                        (tc.id.clone(), func.name.clone())
                                    {
                                        tool_id_map.insert(idx, id.clone());
                                        return Some(Ok(StreamEvent::ToolUseStart { id, name }));
                                    }
                                    if let Some(args) = func.arguments {
                                        if let Some(id) = tool_id_map.get(&idx) {
                                            return Some(Ok(StreamEvent::ToolUseDelta {
                                                id: id.clone(),
                                                json_delta: args,
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                        None
                    })();
                    // 始终返回 Some(...) 确保 scan 不提前终止流
                    futures::future::ready(Some(result))
                },
            )
            .filter_map(|x| futures::future::ready(x));

        Ok(Box::pin(stream))
    }
}
