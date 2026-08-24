//! Anthropic Messages API 供应商实现。
//! 协议依据：https://docs.anthropic.com/en/api/messages

use crate::{
    capabilities::ModelIdentity,
    provider::{EventStream, Provider},
    thinking::should_emit_interleaved_beta,
    types::{ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wyj_config::{Config, WireProtocol};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    /// 模型是否支持图片输入（Profile.vision）。false 时图片降级为占位文本，
    /// 避免非多模态端点收到 image 块直接 400 打断整轮对话。
    supports_vision: bool,
    prompt_cache: bool,
    /// vendor 名（anthropic / zhipu / minimax / moonshot / 等）。用于 thinking adapter
    /// dispatch，决定是否发 interleaved-thinking beta header。
    vendor: String,
    /// 是否官方 Anthropic 端点（profile.provider == Anthropic + base_url 为 api.anthropic.com）。
    /// 第三方兼容端点（GLM/MiniMax/Moonshot 的 /anthropic 路径）按"无 beta"对待。
    is_official_anthropic_endpoint: bool,
    /// Profile 与 catalog 能力对比后被静默丢弃的参数（如用户给 thinking_budget 但
    /// spec 不支持 budget_tokens）。stream() 入口 logging 一次，避免静默降级。
    dropped_parameters: Vec<crate::request_plan::DroppedParameter>,
}

impl AnthropicProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::with_model(cfg, &cfg.active_profile().model.clone())
    }

    pub fn with_model(cfg: &Config, model: &str) -> Result<Self> {
        let api_key = cfg.api_key()?.to_string();
        let base_url = cfg.resolved_base_url().trim_end_matches('/').to_string();
        let profile = cfg.active_profile();
        let vendor = profile
            .vendor
            .clone()
            .unwrap_or_else(|| infer_vendor(&profile.base_url, model).to_string());
        let dropped_parameters =
            crate::request_plan::RequestPlan::from_profile(profile, Some(model)).dropped_parameters;
        Ok(Self {
            client: Client::new(),
            api_key,
            base_url,
            model: model.to_string(),
            supports_vision: profile.vision,
            prompt_cache: profile.effective_prompt_cache(),
            vendor,
            is_official_anthropic_endpoint: profile.is_official_anthropic_endpoint(),
            dropped_parameters,
        })
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            vendor: self.vendor.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            wire_protocol: WireProtocol::AnthropicMessages,
        }
    }
}

/// vendor 推导（fallback）。当 profile 没声明 vendor 时按 base_url + model 名粗略回退。
fn infer_vendor(base_url: &str, model: &str) -> &'static str {
    let base_url = base_url.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    let haystack = format!("{} {}", base_url, model);
    for (needle, vendor) in [
        ("api.anthropic.com", "anthropic"),
        ("bigmodel", "zhipu"),
        ("z.ai", "zhipu"),
        ("glm", "zhipu"),
        ("minimax", "minimax"),
        ("minimaxi.com", "minimax"),
        ("moonshot", "moonshot"),
        ("kimi", "moonshot"),
    ] {
        if haystack.contains(needle) {
            return vendor;
        }
    }
    "anthropic" // 协议是 anthropic 但 vendor 未知
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
    tools: Vec<Value>,
    stream: bool,
    /// Extended thinking 配置（开启时携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingParam>,
}

#[derive(Serialize)]
struct ThinkingParam {
    #[serde(rename = "type")]
    kind: &'static str, // "enabled"
    budget_tokens: u32,
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
    /// thinking 块回传（签名必须原样携带）；不可打 cache_control
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: &'static str,
    media_type: String,
    data: String,
}

/// 把中立 `ToolDefinition` 序列化为 Anthropic 请求体里的单个工具条目。
/// 原生工具（`native = Some`）按 `{"type", "name", ...extra}` 展开，不带
/// description/input_schema；普通工具沿用 `{name, description, input_schema}`。
fn build_api_tool(t: &ToolDefinition, cache_control: Option<CacheControl>) -> Value {
    let mut obj = match &t.native {
        Some(native) => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), Value::String(native.tool_type.clone()));
            obj.insert("name".to_string(), Value::String(t.name.clone()));
            if let Value::Object(extra) = &native.extra {
                for (k, v) in extra {
                    obj.insert(k.clone(), v.clone());
                }
            }
            obj
        }
        None => {
            let mut obj = serde_json::Map::new();
            obj.insert("name".to_string(), Value::String(t.name.clone()));
            obj.insert(
                "description".to_string(),
                Value::String(t.description.clone()),
            );
            obj.insert("input_schema".to_string(), t.input_schema.clone());
            obj
        }
    };
    if let Some(cc) = cache_control {
        obj.insert(
            "cache_control".to_string(),
            serde_json::json!({"type": cc.kind}),
        );
    }
    Value::Object(obj)
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
        #[serde(rename = "error")]
        _error: Value,
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
    Thinking {
        #[allow(dead_code)]
        #[serde(default)]
        thinking: String,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum BlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
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
    cache_read_input_tokens: Option<u32>,
    /// 写入 prompt 缓存的输入 token 数（按 1.25x 计费）
    cache_creation_input_tokens: Option<u32>,
}

// ── 内部模型 → API 请求转换 ───────────────────────────────────────────────────

fn to_api_messages(messages: &[Message], vision: bool) -> Vec<ApiMessage> {
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
                            // 结构化多块内容：text 原样、image 转 Anthropic 原生
                            // image source 结构（tool_result 内嵌图片块）；
                            // 非多模态模型（Profile.vision=false）降级为占位文本
                            crate::types::ToolResultContent::Parts(parts) => Value::Array(
                                parts
                                    .iter()
                                    .map(|p| match p {
                                        crate::types::ToolResultPart::Text { text } => {
                                            serde_json::json!({"type": "text", "text": text})
                                        }
                                        crate::types::ToolResultPart::Image {
                                            media_type,
                                            data,
                                        } if vision => serde_json::json!({
                                            "type": "image",
                                            "source": {
                                                "type": "base64",
                                                "media_type": media_type,
                                                "data": data,
                                            }
                                        }),
                                        crate::types::ToolResultPart::Image {
                                            media_type, ..
                                        } => serde_json::json!({
                                            "type": "text",
                                            "text": format!(
                                                "[image omitted: model does not support vision ({media_type})]"
                                            )
                                        }),
                                    })
                                    .collect(),
                            ),
                            crate::types::ToolResultContent::Blocks(b) => Value::Array(b.clone()),
                        };
                        ApiContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: val,
                            is_error: *is_error,
                            cache_control: None,
                        }
                    }
                    ContentBlock::Image { media_type, data } if vision => ApiContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64",
                            media_type: media_type.clone(),
                            data: data.clone(),
                        },
                    },
                    ContentBlock::Image { media_type, .. } => ApiContentBlock::Text {
                        text: format!(
                            "[image omitted: model does not support vision ({media_type})]"
                        ),
                        cache_control: None,
                    },
                    // thinking 块必须原样（含 signature）回传，否则工具调用续轮被拒
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                        ..
                    } => ApiContentBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: signature.clone(),
                    },
                    ContentBlock::RedactedThinking { data } => {
                        ApiContentBlock::RedactedThinking { data: data.clone() }
                    }
                })
                .collect();
            ApiMessage { role, content }
        })
        .collect()
}

/// 汇总本次请求需要的 `anthropic-beta` header 值（逗号分隔，去重）。
/// `prompt_cache`/`interleaved_thinking` 对应固定 beta；每个原生工具
/// （`ToolDefinition.native`）各自携带所需 beta，按声明顺序去重追加。
fn collect_beta_header(
    prompt_cache: bool,
    interleaved_thinking: bool,
    tools: &[ToolDefinition],
) -> Option<String> {
    let mut betas: Vec<&str> = vec![];
    if prompt_cache {
        betas.push("prompt-caching-2024-07-31");
    }
    if interleaved_thinking {
        betas.push("interleaved-thinking-2025-05-14");
    }
    for t in tools {
        if let Some(native) = &t.native {
            if !betas.contains(&native.beta.as_str()) {
                betas.push(native.beta.as_str());
            }
        }
    }
    (!betas.is_empty()).then(|| betas.join(","))
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
        opts: &crate::provider::RequestOptions,
    ) -> Result<EventStream> {
        if !self.dropped_parameters.is_empty() {
            for dropped in &self.dropped_parameters {
                tracing::info!(
                    vendor = %self.vendor,
                    model = %self.model,
                    parameter = %dropped.name,
                    reason = %dropped.reason,
                    "Profile 参数在当前 vendor/model 下被静默丢弃（catalog 阶段判定）"
                );
            }
        }
        // ── thinking 配置：budget 必须小于 max_tokens，不足时自动抬高 ──
        let thinking_budget = opts.thinking_budget.filter(|b| *b > 0);
        let max_tokens = match thinking_budget {
            Some(b) if opts.max_tokens <= b => {
                tracing::warn!(
                    "max_tokens ({}) <= thinking_budget ({b})，自动抬高到 budget+4096",
                    opts.max_tokens
                );
                b + 4096
            }
            _ => opts.max_tokens,
        };

        // ── 构建 system 块（带 cache_control，缓存 system prompt）──
        let system_blocks = if system.is_empty() {
            vec![]
        } else {
            vec![ApiSystemBlock {
                block_type: "text",
                text: system,
                cache_control: self.prompt_cache.then_some(EPHEMERAL),
            }]
        };

        // ── 构建 tools 块（最后一个工具打 cache_control，缓存全部工具定义）──
        let tool_count = tools.len();
        let api_tools: Vec<Value> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let cc = (self.prompt_cache && tool_count > 0 && i == tool_count - 1)
                    .then_some(EPHEMERAL);
                build_api_tool(t, cc)
            })
            .collect();

        // ── 构建消息历史，在最后一个内容块打 cache_control 断点 ──
        // Anthropic 缓存按前缀匹配：把断点放在历史末尾，使「system + tools +
        // 既有历史」整体被缓存，后续轮次只有新增的 user/assistant 内容按全价。
        // 注意 breakpoint 总数上限为 4（system 1 + tools 1 + 历史 1 = 3，安全）。
        let mut api_messages = to_api_messages(messages, self.supports_vision);
        // 独立 Image 块不能承载 cache_control：从末尾向前回退到最近一个可打
        // 断点的块（旧实现直接放弃断点，以图片结尾的轮次会丢失缓存写入）。
        if self.prompt_cache {
            'breakpoint: for msg in api_messages.iter_mut().rev() {
                for block in msg.content.iter_mut().rev() {
                    match block {
                        ApiContentBlock::Text { cache_control, .. }
                        | ApiContentBlock::ToolUse { cache_control, .. }
                        | ApiContentBlock::ToolResult { cache_control, .. } => {
                            *cache_control = Some(EPHEMERAL);
                            break 'breakpoint;
                        }
                        // Image/Thinking 块不可承载 cache_control，继续向前找
                        ApiContentBlock::Image { .. }
                        | ApiContentBlock::Thinking { .. }
                        | ApiContentBlock::RedactedThinking { .. } => {}
                    }
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
            thinking: thinking_budget.map(|b| ThinkingParam {
                kind: "enabled",
                budget_tokens: b,
            }),
        };
        // api_tools 借用 tools 的引用，不能随 body 一起 move，重新序列化
        let body_value = serde_json::to_value(&body).context("序列化请求失败")?;

        // beta 头：prompt caching 恒开；interleaved thinking 仅在 thinking 开启
        // 且 adapter 允许时追加——第三方 Anthropic 兼容端点（GLM/MiniMax/Moonshot
        // 的 /anthropic 路径）默认不发 interleaved-thinking beta header，因为该
        // header 不在它们的兼容范围内。原生工具（如 computer-use）各自携带
        // 所需 beta，按需去重追加。
        let identity = self.identity();
        let interleaved_enabled = thinking_budget.is_some()
            && opts.interleaved
            && should_emit_interleaved_beta(&identity, self.is_official_anthropic_endpoint);
        let beta_header = collect_beta_header(self.prompt_cache, interleaved_enabled, tools);

        let url = format!("{}/v1/messages", self.base_url);
        // 连接前阶段带指数退避重试（429/5xx/连接错误），流未开始消费，重试透明
        let resp = crate::retry::send_with_retry(
            &crate::retry::RetryPolicy::default(),
            "Anthropic",
            || {
                let mut req = self
                    .client
                    .post(&url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .header("content-type", "application/json");
                if let Some(beta) = &beta_header {
                    req = req.header("anthropic-beta", beta);
                }
                req.json(&body_value)
            },
        )
        .await?;

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

/// Anthropic 兼容供应商返回的 usage 是其对已序列化请求的实际计数。GLM 与
/// MiniMax 的兼容端点可能把它放在 `message_start` 或 `message_delta`，统一在
/// 此处转换，避免两个分支的语义漂移。
fn usage_event(usage: UsageData) -> Option<StreamEvent> {
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
    (input > 0 || output > 0 || cache_read > 0 || cache_write > 0).then_some(StreamEvent::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
    })
}

/// 将单个 SSE 原始事件解析为零或多个 StreamEvent
fn parse_sse_item(
    item: Result<eventsource_stream::Event, eventsource_stream::EventStreamError<reqwest::Error>>,
) -> Vec<Result<StreamEvent>> {
    let event = match item {
        Ok(e) => e,
        Err(_) => {
            return vec![Err(anyhow::Error::new(crate::error::ProviderError::new(
                crate::error::ProviderErrorKind::StreamTruncated,
                "provider SSE stream ended unexpectedly",
            )))];
        }
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
        SseEvent::MessageStart { message } => message
            .usage
            .and_then(usage_event)
            .map_or_else(Vec::new, |event| vec![Ok(event)]),
        SseEvent::ContentBlockStart { content_block, .. } => match content_block {
            ContentBlockStart::ToolUse { id, name } => {
                vec![Ok(StreamEvent::ToolUseStart { id, name })]
            }
            ContentBlockStart::Text { .. } => vec![],
            ContentBlockStart::Thinking { .. } => vec![Ok(StreamEvent::ThinkingStart)],
            ContentBlockStart::RedactedThinking { data } => {
                vec![Ok(StreamEvent::RedactedThinking(data))]
            }
        },
        SseEvent::ContentBlockDelta { delta, .. } => match delta {
            BlockDelta::TextDelta { text } => vec![Ok(StreamEvent::TextDelta(text))],
            BlockDelta::InputJsonDelta { partial_json } => {
                vec![Ok(StreamEvent::ToolUseDelta {
                    id: String::new(),
                    json_delta: partial_json,
                })]
            }
            BlockDelta::ThinkingDelta { thinking } => {
                vec![Ok(StreamEvent::ThinkingDelta(thinking))]
            }
            BlockDelta::SignatureDelta { signature } => {
                vec![Ok(StreamEvent::ThinkingSignatureDelta(signature))]
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
            if let Some(event) = usage.and_then(usage_event) {
                out.push(Ok(event));
            }
            out
        }
        SseEvent::MessageStop | SseEvent::Ping => vec![],
        SseEvent::Error { _error: _ } => {
            vec![Err(anyhow::Error::new(crate::error::ProviderError::new(
                crate::error::ProviderErrorKind::StreamTruncated,
                "provider emitted a stream error event",
            )))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolResultContent, ToolResultPart};

    fn image_tool_result_msg() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: ToolResultContent::Parts(vec![ToolResultPart::Image {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }]),
                is_error: false,
            }],
        }]
    }

    #[test]
    fn parts_image_serializes_as_native_image_block() {
        let api = to_api_messages(&image_tool_result_msg(), true);
        let json = serde_json::to_string(&api).unwrap();
        // tool_result.content 数组内嵌 Anthropic 原生 image source 结构
        assert!(json.contains(r#""type":"image""#));
        assert!(json.contains(r#""type":"base64""#));
        assert!(json.contains(r#""media_type":"image/png""#));
    }

    #[test]
    fn parts_image_degrades_to_text_without_vision() {
        let api = to_api_messages(&image_tool_result_msg(), false);
        let json = serde_json::to_string(&api).unwrap();
        assert!(!json.contains(r#""type":"image""#));
        assert!(json.contains("image omitted"));
    }

    #[test]
    fn standalone_image_degrades_without_vision() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        }];
        let json = serde_json::to_string(&to_api_messages(&msgs, false)).unwrap();
        assert!(!json.contains(r#""type":"image""#));
        assert!(json.contains("image omitted"));
    }

    #[test]
    fn thinking_blocks_serialize_with_signature() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: "sig".into(),
                    reasoning_details: None,
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
        }];
        let json = serde_json::to_string(&to_api_messages(&msgs, true)).unwrap();
        assert!(json.contains(r#""type":"thinking""#));
        assert!(json.contains(r#""signature":"sig""#));
        assert!(json.contains(r#""type":"redacted_thinking""#));
        assert!(json.contains(r#""data":"opaque""#));
    }

    #[test]
    fn anthropic_compatible_usage_is_preserved_as_exact_token_usage() {
        let event = usage_event(UsageData {
            input_tokens: Some(1_024),
            output_tokens: Some(256),
            cache_read_input_tokens: Some(128),
            cache_creation_input_tokens: Some(64),
        })
        .expect("non-empty provider usage should produce an event");
        assert!(matches!(
            event,
            StreamEvent::Usage {
                input_tokens: 1_024,
                output_tokens: 256,
                cache_read_input_tokens: 128,
                cache_creation_input_tokens: 64,
            }
        ));
    }

    fn computer_tool_def() -> ToolDefinition {
        ToolDefinition {
            name: "computer".to_string(),
            description: "ignored for native tools".to_string(),
            input_schema: serde_json::json!({"ignored": true}),
            native: Some(crate::types::NativeToolSpec {
                tool_type: "computer_20251124".to_string(),
                extra: serde_json::json!({
                    "display_width_px": 1280,
                    "display_height_px": 800,
                }),
                beta: "computer-use-2025-11-24".to_string(),
            }),
        }
    }

    #[test]
    fn native_tool_serializes_without_description_or_input_schema() {
        let value = build_api_tool(&computer_tool_def(), None);
        let obj = value
            .as_object()
            .expect("native tool must serialize as object");
        assert_eq!(
            obj.get("type").and_then(|v| v.as_str()),
            Some("computer_20251124")
        );
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("computer"));
        assert_eq!(
            obj.get("display_width_px").and_then(|v| v.as_i64()),
            Some(1280)
        );
        assert_eq!(
            obj.get("display_height_px").and_then(|v| v.as_i64()),
            Some(800)
        );
        // 原生工具不携带 description/input_schema —— schema 由供应商内置
        assert!(!obj.contains_key("description"));
        assert!(!obj.contains_key("input_schema"));
    }

    #[test]
    fn native_tool_carries_cache_control_when_requested() {
        let value = build_api_tool(&computer_tool_def(), Some(EPHEMERAL));
        let obj = value.as_object().unwrap();
        assert_eq!(
            obj.get("cache_control")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("ephemeral")
        );
    }

    #[test]
    fn custom_tool_serializes_with_name_description_input_schema() {
        let def = ToolDefinition {
            name: "Read".to_string(),
            description: "read a file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        };
        let value = build_api_tool(&def, None);
        let obj = value.as_object().unwrap();
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("Read"));
        assert_eq!(
            obj.get("description").and_then(|v| v.as_str()),
            Some("read a file")
        );
        assert!(obj.contains_key("input_schema"));
        // 普通工具不带 type 字段（那是原生工具专属）
        assert!(!obj.contains_key("type"));
    }

    #[test]
    fn beta_header_appends_native_tool_beta_and_dedupes() {
        let tools = vec![computer_tool_def(), computer_tool_def()];
        let header = collect_beta_header(true, false, &tools).unwrap();
        assert_eq!(header, "prompt-caching-2024-07-31,computer-use-2025-11-24");
    }

    #[test]
    fn beta_header_is_none_without_any_beta_source() {
        assert_eq!(collect_beta_header(false, false, &[]), None);
    }
}
