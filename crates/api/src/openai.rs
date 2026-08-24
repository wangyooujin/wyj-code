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
    /// 是否把 user message 中的 `ContentBlock::Image` 序列化为 OpenAI 的
    /// `image_url` 块（OpenAI Chat Completions 标准格式）。
    /// 当 `Profile.vision=false` 时走纯字符串降级，避免第三方兼容端点
    /// 对未知 content 数组字段返回 400。
    supports_vision: bool,
}

impl OpenAIProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::with_model(cfg, &cfg.active_profile().model.clone())
    }

    pub fn with_model(cfg: &Config, model: &str) -> Result<Self> {
        let api_key = cfg.api_key()?.to_string();
        let base_url = cfg.resolved_base_url().trim_end_matches('/').to_string();
        let profile = cfg.active_profile();
        Ok(Self {
            client: Client::new(),
            api_key,
            base_url,
            model: model.to_string(),
            stream_options: profile.effective_openai_stream_options_for_model(model),
            supports_vision: profile.vision,
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
    /// DeepSeek `deepseek-reasoner` 等 reasoning 模型在流式响应里通过
    /// `delta.reasoning_content` 字段返回推理过程。OpenAI Chat Completions
    /// 标准无此字段，serde 默认忽略未知字段；国产 reasoning 模型会回填。
    #[serde(default)]
    reasoning_content: Option<String>,
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

fn to_api_messages(messages: &[Message], supports_vision: bool) -> Vec<ApiMessage> {
    let mut out = vec![];
    for m in messages {
        match m.role {
            Role::User => {
                let mut text_parts: Vec<String> = vec![];
                let mut image_parts: Vec<Value> = vec![];
                let mut tool_results = vec![];
                for block in &m.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::Image { media_type, data } => {
                            // 仅当 supports_vision=true 时序列化 image_url；
                            // 否则丢弃（与原 `_ => {}` 行为一致）。
                            if supports_vision {
                                image_parts.push(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", media_type, data),
                                    }
                                }));
                            }
                        }
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
                } else if supports_vision && !image_parts.is_empty() {
                    // vision 模式：content 是 [{type:"text",...}, {type:"image_url",...}] 数组
                    let mut content_parts: Vec<Value> = text_parts
                        .into_iter()
                        .map(|t| serde_json::json!({"type": "text", "text": t}))
                        .collect();
                    content_parts.extend(image_parts);
                    out.push(ApiMessage {
                        role: "user".to_string(),
                        content: Some(Value::Array(content_parts)),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                } else {
                    // 普通文本 user 消息：content 是纯字符串
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
///
/// reasoning_content（DeepSeek `deepseek-reasoner` 等）独立于 finish_reason /
/// content / tool_calls，因为它常与 content 在同一 chunk 出现，且 reasoning
/// 应在 content 之前上屏以模拟 Claude 原生 thinking 体验。
fn stream_events_from_chunk(
    tool_map: &mut std::collections::HashMap<usize, PendingToolCall>,
    chunk: SseChunk,
) -> Vec<StreamEvent> {
    let mut events = vec![];

    if let Some(choice) = chunk.choices.into_iter().next() {
        // 1. reasoning_content 独立分支（DeepSeek 等 reasoning 模型）。
        //    若与 content 同 chunk，优先 reasoning 后 content，模拟 thinking-then-text。
        if let Some(text) = choice.delta.reasoning_content {
            if !text.is_empty() {
                events.push(StreamEvent::ThinkingDelta(text));
            }
        }

        // 2. finish_reason / content / tool_calls 走原互斥逻辑。
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

    // 3. usage 最后发：精确账本永远跟随 message stop 之后。
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
        api_messages.extend(to_api_messages(messages, self.supports_vision));

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
                    reasoning_content: None,
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

    /// DeepSeek `deepseek-reasoner` 等 reasoning 模型通过 `delta.reasoning_content`
    /// 字段返回推理过程。当前实现把它当作 `ThinkingDelta` 事件，让 reasoning
    /// 能落盘到 `ContentBlock::Thinking` 并上屏，与 Anthropic thinking 行为对齐。
    #[test]
    fn reasoning_chunk_emits_thinking_delta() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: None,
                    reasoning_content: Some("Step 1: read Cargo.toml".to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            StreamEvent::ThinkingDelta(t) if t == "Step 1: read Cargo.toml"
        ));
    }

    /// reasoning_content 与 content 可在同一 chunk 出现（DeepSeek 边界）。
    /// 实现要求：reasoning 在前，content 在后，模拟 thinking-then-text 体验。
    #[test]
    fn reasoning_and_content_in_same_chunk() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("Answer: 42".to_string()),
                    reasoning_content: Some("Compute 6*7".to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::ThinkingDelta(t) if t == "Compute 6*7"));
        assert!(matches!(&events[1], StreamEvent::TextDelta(t) if t == "Answer: 42"));
    }

    /// reasoning_content 为空字符串时不 emit（避免上屏空 thinking）。
    #[test]
    fn empty_reasoning_content_is_skipped() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("hi".to_string()),
                    reasoning_content: Some(String::new()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "hi"));
    }

    /// 当 supports_vision=true 时,user message 中的 Image 块应序列化为
    /// OpenAI Chat Completions 标准的 `image_url` 数组,与文本混合成
    /// `[{type:"text",...},{type:"image_url",...}]` 形式。
    #[test]
    fn vision_true_serializes_image_blocks_as_image_url() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "describe this image".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "iVBORw0KGgo=".to_string(),
                },
            ],
        }];

        let api = to_api_messages(&msgs, true);
        assert_eq!(api.len(), 1);
        let content = api[0].content.as_ref().expect("user content");
        let parts = content.as_array().expect("content must be an array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "describe this image");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    /// 当 supports_vision=false 时,Image 块走纯字符串降级（原 `_ => {}` 行为）。
    /// 这是 Profile.vision=false 时所有 OpenAI 兼容端点必须走的路径，避免
    /// 第三方代理对未知 content 数组字段返回 400。
    #[test]
    fn vision_false_drops_image_blocks_to_plain_string() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "hi".to_string(),
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "ignored".to_string(),
                },
            ],
        }];

        let api = to_api_messages(&msgs, false);
        assert_eq!(api.len(), 1);
        let content = api[0].content.as_ref().expect("user content");
        // 纯字符串，内容只包含文本段
        assert_eq!(content.as_str(), Some("hi"));
    }

    /// 无 Image 块时,即使 supports_vision=true 也走纯字符串路径（向后兼容）。
    #[test]
    fn vision_true_without_image_keeps_plain_string() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }];

        let api = to_api_messages(&msgs, true);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].content.as_ref().unwrap().as_str(), Some("hello"));
    }
}
