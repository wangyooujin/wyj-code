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
        })
    }
}

// ── 请求/响应类型 ────────────────────────────────────────────────────────────

/// prompt 缓存标记。Anthropic API 支持 `cache_control: {type: "ephemeral"}`
/// 标记 system / tools / 历史消息的前缀，命中后 input token 按 0.1x 计费。
/// 详见 https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
#[derive(Serialize, Clone, Copy)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

const EPHEMERAL: CacheControl = CacheControl { kind: "ephemeral" };

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    /// system 字段用数组形式以支持 `cache_control` 标记，使 system prompt
    /// 内容可被 prompt caching 缓存（首轮全价、后续轮次命中按 0.1x 计费）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<ApiSystemBlock<'a>>,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ApiSystemBlock<'a> {
    #[serde(rename = "type")]
    block_type: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: &'static str,
    content: Vec<ApiContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
    },
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
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
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
    ToolUse {
        id: String,
        name: String,
    },
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
    /// 命中 prompt 缓存的输入 token 数（按 0.1x 计费）
    #[allow(dead_code)]
    cache_read_input_tokens: Option<u32>,
    /// 写入 prompt 缓存的输入 token 数（按 1.25x 计费）
    #[allow(dead_code)]
    cache_creation_input_tokens: Option<u32>,
}

// ── 内部模型 → API 请求转换 ───────────────────────────────────────────────────

fn to_api_messages(messages: &[Message]) -> Vec<ApiMessage> {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content = m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => ApiContentBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    },
                    ContentBlock::ToolUse { id, name, input } => ApiContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        cache_control: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let val = match content {
                            crate::types::ToolResultContent::Text(t) => Value::String(t.clone()),
                            crate::types::ToolResultContent::Blocks(b) => Value::Array(b.clone()),
                        };
                        ApiContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: val,
                            is_error: *is_error,
                            cache_control: None,
                        }
                    }
                    ContentBlock::Image { media_type, data } => ApiContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64",
                            media_type: media_type.clone(),
                            data: data.clone(),
                        },
                    },
                })
                .collect();
            ApiMessage { role, content }
        })
        .collect()
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
        // ── 构建 system 块（带 cache_control，缓存 system prompt）──
        let system_blocks = if system.is_empty() {
            vec![]
        } else {
            vec![ApiSystemBlock {
                block_type: "text",
                text: system,
                cache_control: Some(EPHEMERAL),
            }]
        };

        // ── 构建 tools 块（最后一个工具打 cache_control，缓存全部工具定义）──
        let tool_count = tools.len();
        let api_tools: Vec<ApiTool> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| ApiTool {
                name: &t.name,
                description: &t.description,
                input_schema: &t.input_schema,
                cache_control: (tool_count > 0 && i == tool_count - 1).then_some(EPHEMERAL),
            })
            .collect();

        // ── 构建消息历史，在最后一个内容块打 cache_control 断点 ──
        // Anthropic 缓存按前缀匹配：把断点放在历史末尾，使「system + tools +
        // 既有历史」整体被缓存，后续轮次只有新增的 user/assistant 内容按全价。
        // 注意 breakpoint 总数上限为 4（system 1 + tools 1 + 历史 1 = 3，安全）。
        let mut api_messages = to_api_messages(messages);
        if let Some(last_msg) = api_messages.last_mut() {
            if let Some(last_block) = last_msg.content.last_mut() {
                match last_block {
                    ApiContentBlock::Text { cache_control, .. }
                    | ApiContentBlock::ToolUse { cache_control, .. }
                    | ApiContentBlock::ToolResult { cache_control, .. } => {
                        *cache_control = Some(EPHEMERAL);
                    }
                    ApiContentBlock::Image { .. } => {}
                }
            }
        }

        let body = ApiRequest {
            model: &self.model,
            max_tokens,
            system: system_blocks,
            messages: api_messages,
            tools: api_tools,
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
            // 启用 prompt caching：标记为 ephemeral 的 system/tools/历史前缀
            // 命中缓存后按 0.1x 计费，显著降低多轮对话的 input token 成本。
            .header("anthropic-beta", "prompt-caching-2024-07-31")
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

        // 用 flat_map 允许每个 SSE 事件 yield 多个 StreamEvent
        let stream = sse.flat_map(|item| {
            let events: Vec<Result<StreamEvent>> = parse_sse_item(item);
            futures::stream::iter(events)
        });

        Ok(Box::pin(stream))
    }
}

/// 将单个 SSE 原始事件解析为零或多个 StreamEvent
fn parse_sse_item(
    item: Result<eventsource_stream::Event, eventsource_stream::EventStreamError<reqwest::Error>>,
) -> Vec<Result<StreamEvent>> {
    let event = match item {
        Ok(e) => e,
        Err(e) => return vec![Err(anyhow::anyhow!("SSE 读取失败: {e}"))],
    };
    if event.data == "[DONE]" {
        return vec![];
    }
    let parsed: SseEvent = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("SSE 解析跳过: {e} data={}", event.data);
            return vec![];
        }
    };
    match parsed {
        SseEvent::MessageStart { message } => {
            if let Some(usage) = message.usage {
                let input = usage.input_tokens.unwrap_or(0);
                let output = usage.output_tokens.unwrap_or(0);
                let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
                if input > 0 || output > 0 || cache_read > 0 {
                    return vec![Ok(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_input_tokens: cache_read,
                    })];
                }
            }
            vec![]
        }
        SseEvent::ContentBlockStart { content_block, .. } => match content_block {
            ContentBlockStart::ToolUse { id, name } => {
                vec![Ok(StreamEvent::ToolUseStart { id, name })]
            }
            ContentBlockStart::Text { .. } => vec![],
        },
        SseEvent::ContentBlockDelta { delta, .. } => match delta {
            BlockDelta::TextDelta { text } => vec![Ok(StreamEvent::TextDelta(text))],
            BlockDelta::InputJsonDelta { partial_json } => {
                vec![Ok(StreamEvent::ToolUseDelta {
                    id: String::new(),
                    json_delta: partial_json,
                })]
            }
        },
        SseEvent::ContentBlockStop { .. } => vec![],
        SseEvent::MessageDelta { delta, usage } => {
            let stop_reason = delta
                .stop_reason
                .as_deref()
                .map(parse_stop_reason)
                .unwrap_or(StopReason::EndTurn);
            let mut out = vec![Ok(StreamEvent::MessageStop { stop_reason })];
            // message_delta.usage 携带本次调用的真实 input+output token 数
            // MiniMax 等供应商只在此处给出实际计数，message_start 里均为 0
            if let Some(u) = usage {
                let input = u.input_tokens.unwrap_or(0);
                let output = u.output_tokens.unwrap_or(0);
                let cache_read = u.cache_read_input_tokens.unwrap_or(0);
                if input > 0 || output > 0 || cache_read > 0 {
                    out.push(Ok(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_input_tokens: cache_read,
                    }));
                }
            }
            out
        }
        SseEvent::MessageStop | SseEvent::Ping => vec![],
        SseEvent::Error { error } => {
            vec![Err(anyhow::anyhow!("Anthropic 流式错误: {error}"))]
        }
    }
}
