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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wyj_config::{Config, WireProtocol};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// prompt cache 生效状态取值。与
/// `wyj_core::session_store::prompt_cache_state` 的常量一一对应，但**不在此处
/// 依赖 core**（api 是 core 的下层依赖，core 依赖 api）。两处常量必须同步
/// 修改，语义由 `session_store` 侧的定义为准。
pub const PROMPT_CACHE_STATE_OFF: u8 = 0;
pub const PROMPT_CACHE_STATE_ON: u8 = 1;
pub const PROMPT_CACHE_STATE_DOWNGRADED: u8 = 2;

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    /// 模型是否支持图片输入（Profile.vision）。false 时图片降级为占位文本，
    /// 避免非多模态端点收到 image 块直接 400 打断整轮对话。
    supports_vision: bool,
    prompt_cache: bool,
    /// 是否把 `prompt-caching-2024-07-31` 放进 `anthropic-beta` 头。
    /// 只有官方 Anthropic 端点为 true；第三方兼容端点开启缓存时只发
    /// `cache_control` 而不发 beta 头，理由见 [`collect_beta_header`]。
    send_cache_beta: bool,
    /// 该端点已被实测判定不支持 `cache_control`（首次 400 后单调置位，
    /// 本进程内不再尝试缓存）。供 agent 层据此在 `/cost` 解释缓存为何为 0。
    cache_disabled: Arc<AtomicBool>,
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
        let is_official = profile.is_official_anthropic_endpoint();
        Ok(Self {
            client: Client::new(),
            api_key,
            base_url,
            model: model.to_string(),
            supports_vision: profile.vision,
            prompt_cache: profile.effective_prompt_cache(),
            // 第三方端点不发 caching beta 头：它们实现了 cache_control，但对
            // 未知 beta 头普遍直接 400。降级兜底见 `send_with_cache_fallback`。
            send_cache_beta: is_official,
            cache_disabled: Arc::new(AtomicBool::new(false)),
            vendor,
            is_official_anthropic_endpoint: is_official,
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

/// 按 `SystemPrompt` 的两段结构生成 system 块列表。
///
/// 拆成两块是有意为之：Anthropic 的缓存按**前缀**匹配，`cache_control` 断点
/// 打在哪块末尾，该块及其之前才进缓存。
///   * `stable`（主提示 / `<env>` / 模式段 / 记忆快照 / AGENTS.md 祖先链）
///     承载缓存，断点打在这块末尾；
///   * `volatile`（当前工具可用性 / 模型兼容 suffix / 子目录 AGENTS.md
///     reminder）每轮都可能变，放在断点**之后**且不打断点——这样它变化时
///     不会让 stable 整段（约 1.6k~5k token）全价重算。
///
/// 断点预算：system 1 + tools 1 + 历史 1 = 3，仍在 Anthropic 上限 4 以内。
fn build_system_blocks<'a>(
    system: &crate::provider::SystemPrompt<'a>,
    prompt_cache: bool,
) -> Vec<ApiSystemBlock<'a>> {
    let mut blocks: Vec<ApiSystemBlock<'a>> = Vec::new();
    if !system.stable.is_empty() {
        blocks.push(ApiSystemBlock {
            block_type: "text",
            text: system.stable,
            cache_control: prompt_cache.then_some(EPHEMERAL),
        });
    }
    if !system.volatile.is_empty() {
        blocks.push(ApiSystemBlock {
            block_type: "text",
            text: system.volatile,
            cache_control: None,
        });
    }
    blocks
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
///
/// `send_cache_beta` 与 `prompt_cache` **刻意解耦**：prompt caching 在
/// Anthropic 协议层是「请求体里的 `cache_control` 块」，beta 头只是官方
/// 在 GA 前的开关。第三方 Anthropic 兼容端点（MiniMax / GLM / Kimi …）
/// 普遍实现了前者却会对未知 beta 头直接 400，所以第三方开启缓存时只发
/// `cache_control`、不发 beta 头。官方端点两者都发（保持既有行为）。
fn collect_beta_header(
    prompt_cache: bool,
    send_cache_beta: bool,
    interleaved_thinking: bool,
    tools: &[ToolDefinition],
) -> Option<String> {
    let mut betas: Vec<&str> = vec![];
    if prompt_cache && send_cache_beta {
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

/// 发送一次 Anthropic Messages 请求（连接前阶段带指数退避重试）。
///
/// 单独成函数而不是内联闭包：降级重试需要用同一组连接参数发第二次，而
/// `RetryPolicy` 与 `url` 都是借用——内联闭包会让 future 借用临时值。
/// 429/5xx/连接错误的重试在这里统一处理，流未开始消费，重试对上层透明。
async fn send_anthropic_request(
    client: &Client,
    api_key: &str,
    url: &str,
    policy: &crate::retry::RetryPolicy,
    body_value: &Value,
    beta_header: Option<&str>,
) -> Result<reqwest::Response> {
    crate::retry::send_with_retry(policy, "Anthropic", || {
        let mut req = client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if let Some(beta) = beta_header {
            req = req.header("anthropic-beta", beta);
        }
        req.json(body_value)
    })
    .await
}

/// 该错误是否意味着「端点不接受 cache_control / caching beta 头」。
///
/// 必须排除 `ContextLengthExceeded`：它也是 400，但语义是
/// 「提示词太长」，正确反应是上层强制压缩（`agent.rs` 的
/// `ContextLengthExceeded` 恢复路径）而不是关缓存。若把它误判成缓存不支持，
/// 用户会遇到「上下文一满就再也不用缓存了」这种莫名其妙的降级。
///
/// 只认 `UnsupportedParameter` 与 `InvalidRequest` 两类：前者是端点明确说不认识
/// 该参数，后者是「无法解析请求」——`cache_control` 写坏或 beta 头不被接受时
/// 都落在这里。其余 400（如 tool schema 非法）不降级，避免把真实 bug 掩盖成
/// 「缓存不支持」。
fn is_cache_rejection(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::error::ProviderError>()
        .map(|e| {
            e.provider_status == Some(400)
                && matches!(
                    e.kind,
                    crate::error::ProviderErrorKind::UnsupportedParameter
                        | crate::error::ProviderErrorKind::InvalidRequest
                )
        })
        .unwrap_or(false)
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
    fn prompt_cache_state(&self) -> u8 {
        if !self.prompt_cache {
            PROMPT_CACHE_STATE_OFF
        } else if self.cache_disabled.load(Ordering::Relaxed) {
            PROMPT_CACHE_STATE_DOWNGRADED
        } else {
            PROMPT_CACHE_STATE_ON
        }
    }

    async fn stream(
        &self,
        system: &crate::provider::SystemPrompt<'_>,
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

        // ── 缓存是否生效 ──
        // profile 显式开启（第三方端点默认关，见 `effective_prompt_cache`），
        // 且本进程尚未因 400 判定该端点不支持。
        let cache_requested = self.prompt_cache;
        let cache_active = cache_requested && !self.cache_disabled.load(Ordering::Relaxed);

        // ── 按 cache_active 构建请求体与 beta 头 ──
        // 做成闭包而不是先建好再改，是因为 400 降级需要一份**完全重建**的
        // 请求：reqwest 的 RequestBuilder 不可复用，而 cache_control 是
        // 序列化进 body 的，不是可以事后摘掉的 header。
        let identity = self.identity();
        let interleaved_enabled = thinking_budget.is_some()
            && opts.interleaved
            && should_emit_interleaved_beta(&identity, self.is_official_anthropic_endpoint);
        // 官方端点 beta 与 cache_control 同生共死；第三方即使本次降级重试，
        // 也不应发 caching beta 头（那正是可能触发 400 的东西）。
        let send_cache_beta = self.send_cache_beta;

        let build_request = |cache_active: bool| -> Result<(Value, Option<String>)> {
            let system_blocks = build_system_blocks(system, cache_active);

            let tool_count = tools.len();
            let api_tools: Vec<Value> = tools
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let cc = (cache_active && tool_count > 0 && i == tool_count - 1)
                        .then_some(EPHEMERAL);
                    build_api_tool(t, cc)
                })
                .collect();

            // Anthropic 缓存按前缀匹配：把断点放在历史末尾，使「system + tools +
            // 既有历史」整体被缓存，后续轮次只有新增的 user/assistant 内容按全价。
            // 注意 breakpoint 总数上限为 4（system 1 + tools 1 + 历史 1 = 3，安全）。
            let mut api_messages = to_api_messages(messages, self.supports_vision);
            // 独立 Image 块不能承载 cache_control：从末尾向前回退到最近一个可打
            // 断点的块（旧实现直接放弃断点，以图片结尾的轮次会丢失缓存写入）。
            if cache_active {
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
            let beta =
                collect_beta_header(cache_active, send_cache_beta, interleaved_enabled, tools);
            Ok((body_value, beta))
        };

        let (body_value, beta_header) = build_request(cache_active)?;
        let url = format!("{}/v1/messages", self.base_url);
        let policy = crate::retry::RetryPolicy::default();
        let resp = match send_anthropic_request(
            &self.client,
            &self.api_key,
            &url,
            &policy,
            &body_value,
            beta_header.as_deref(),
        )
        .await
        {
            Ok(resp) => resp,
            // 端点不认 cache_control / caching beta 头 → 单调降级并重试一次。
            // 发生在 SSE 开始之前，尚未产出任何 delta，重放安全。
            Err(err) => {
                if !(cache_active && is_cache_rejection(&err)) {
                    return Err(err);
                }
                // compare_exchange 保证并发请求里只有一个执行降级，避免重复告警。
                if self
                    .cache_disabled
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    tracing::warn!(
                        vendor = %self.vendor,
                        model = %self.model,
                        base_url = %self.base_url,
                        "端点拒绝了 prompt cache（cache_control / anthropic-beta），本进程已停用缓存并重试；如需缓存请确认该模型是否支持"
                    );
                }
                let (plain_body, plain_beta) = build_request(false)?;
                send_anthropic_request(
                    &self.client,
                    &self.api_key,
                    &url,
                    &policy,
                    &plain_body,
                    plain_beta.as_deref(),
                )
                .await?
            }
        };

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
        let header = collect_beta_header(true, true, false, &tools).unwrap();
        assert_eq!(header, "prompt-caching-2024-07-31,computer-use-2025-11-24");
    }

    #[test]
    fn beta_header_is_none_without_any_beta_source() {
        assert_eq!(collect_beta_header(false, true, false, &[]), None);
    }

    /// 第三方兼容端点开启缓存时**只发 cache_control、不发 caching beta 头**。
    /// MiniMax / GLM / Kimi 的 /anthropic 路径实现了 cache_control，但对未知
    /// beta 头直接 400；把两者绑死会让「开启缓存」在第三方上必然失败。
    #[test]
    fn third_party_endpoint_sends_cache_control_without_caching_beta() {
        let header = collect_beta_header(true, false, false, &[]);
        assert_eq!(header, None, "第三方端点开启缓存不应产生 anthropic-beta 头");
    }

    /// 但 interleaved-thinking / 原生工具 beta 仍照发——只有 caching beta 被
    /// 按端点裁掉，不能把整个 header 一起吞掉。
    #[test]
    fn third_party_still_sends_unrelated_betas() {
        let tools = vec![computer_tool_def()];
        let header = collect_beta_header(true, false, true, &tools).unwrap();
        assert_eq!(
            header,
            "interleaved-thinking-2025-05-14,computer-use-2025-11-24"
        );
    }

    /// 400「提示词太长」不是「缓存不支持」。误判会让上下文一满就永久失去缓存，
    /// 而正确反应是上层强制压缩。
    #[test]
    fn context_overflow_400_is_not_a_cache_rejection() {
        let headers = reqwest::header::HeaderMap::new();
        let err = anyhow::Error::new(crate::error::ProviderError::from_http(
            reqwest::StatusCode::BAD_REQUEST,
            &headers,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        ));
        assert!(!is_cache_rejection(&err));
    }

    #[test]
    fn unsupported_parameter_400_triggers_cache_downgrade() {
        let headers = reqwest::header::HeaderMap::new();
        let err = anyhow::Error::new(crate::error::ProviderError::from_http(
            reqwest::StatusCode::BAD_REQUEST,
            &headers,
            r#"{"error":{"message":"unsupported parameter: anthropic-beta"}}"#,
        ));
        assert!(is_cache_rejection(&err));
    }

    #[test]
    fn non_400_never_triggers_cache_downgrade() {
        let headers = reqwest::header::HeaderMap::new();
        let err = anyhow::Error::new(crate::error::ProviderError::from_http(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &headers,
            r#"{"error":{"message":"unsupported parameter"}}"#,
        ));
        assert!(!is_cache_rejection(&err));
    }

    // ── system 分段与 prompt cache 断点 ─────────────────────────────────
    //
    // 回归背景：整个 system 曾被压成单个 text 块、断点打在块尾，导致
    // `<current-tool-availability>`、模型兼容 suffix、子目录 AGENTS.md
    // reminder、每轮重算的 Project Brief 中任何一项变化，都会让整段 system
    // 全价重算。拆成 stable / volatile 两块后断点只保护 stable。

    #[test]
    fn stable_and_volatile_map_to_separate_system_blocks() {
        let system = crate::provider::SystemPrompt {
            stable: "STABLE-PROMPT",
            volatile: "VOLATILE-TAIL",
        };
        let blocks = build_system_blocks(&system, true);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "STABLE-PROMPT");
        assert_eq!(blocks[1].text, "VOLATILE-TAIL");
    }

    #[test]
    fn cache_breakpoint_sits_only_on_the_stable_block() {
        let system = crate::provider::SystemPrompt {
            stable: "STABLE-PROMPT",
            volatile: "VOLATILE-TAIL",
        };
        let blocks = build_system_blocks(&system, true);
        assert!(
            blocks[0].cache_control.is_some(),
            "断点必须打在 stable 块末尾，否则 volatile 变化会让 stable 失效"
        );
        assert!(
            blocks[1].cache_control.is_none(),
            "volatile 块不应打断点（会白白多占一个 breakpoint 配额）"
        );
    }

    #[test]
    fn volatile_changes_do_not_disturb_the_stable_prefix() {
        // 同一份 stable + 不同 volatile → stable 块序列化结果必须逐字节相同，
        // 这正是 prompt cache 仍能命中的前提。
        let a = crate::provider::SystemPrompt {
            stable: "SAME",
            volatile: "turn-1 tool list: Read, Bash",
        };
        let b = crate::provider::SystemPrompt {
            stable: "SAME",
            volatile: "turn-2 tool list: Read, Bash, WebFetch",
        };
        // 只比较 stable 块本身（整份块列表当然会因 volatile 文本不同而不同）。
        let stable_a = serde_json::to_string(&build_system_blocks(&a, true)[0]).unwrap();
        let stable_b = serde_json::to_string(&build_system_blocks(&b, true)[0]).unwrap();
        assert_eq!(stable_a, stable_b, "volatile 变化不应改写 stable 块");
        // 断点仍在 stable 块末尾
        assert!(stable_a.contains("cache_control"));
    }

    #[test]
    fn empty_segments_are_omitted_and_prompt_cache_off_drops_breakpoint() {
        let only_stable = crate::provider::SystemPrompt::stable_only("S");
        let blocks = build_system_blocks(&only_stable, true);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_some());

        // prompt_cache = false → 一律不打断点
        let blocks = build_system_blocks(&only_stable, false);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_none());

        // 两段都空 → 不产出块（保持旧行为：system 字段 skip_serializing_if 空数组）
        let none = crate::provider::SystemPrompt::default();
        assert!(build_system_blocks(&none, true).is_empty());
    }
}
