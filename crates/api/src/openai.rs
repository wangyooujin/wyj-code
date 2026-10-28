//! OpenAI Chat Completions API 供应商实现。
//! 协议依据：https://platform.openai.com/docs/api-reference/chat

use crate::{
    capabilities::ModelIdentity,
    provider::{EventStream, Provider},
    request_plan::{DroppedParameter, RequestPlan},
    thinking::apply_thinking_to_openai_body,
    types::{ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition},
};
use anyhow::Result;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use wyj_config::{Config, WireProtocol};

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
    /// vendor 名（deepseek / alibaba / zhipu / ...）；用于 thinking adapter dispatch
    vendor: String,
    /// 本实现**实际**使用的线协议。
    ///
    /// 恒为 `OpenAiChatCompletions`——`OpenAIProvider` 只会打
    /// `{base_url}/chat/completions`。profile 若声明了 `open_ai_responses`，
    /// 构造期会告警一次并在此归一到真实协议；不归一的话 identity 会把
    /// `ThinkingSpec::for_vendor` / `adapter_for` 引到没有实现的 `Responses`
    /// 分支（落到 generic → thinking 全关），形成「配置能选、行为静默变」。
    wire_protocol: WireProtocol,
    /// 端点是否会复用请求前缀（catalog 判定的 `PromptCacheMode`）。
    /// 用于 `prompt_cache_state()` 回报真实状态，见该方法的注释。
    prompt_cache_mode: crate::PromptCacheMode,
    /// Profile 与 catalog 能力对比后被静默丢弃的参数（用户在 Profile 里设了
    /// vendor 不识别的 thinking_budget/reasoning_effort）。每个请求 stream()
    /// 入口 logging 一次，避免静默降级对用户不可见。
    dropped_parameters: Vec<DroppedParameter>,
}

impl OpenAIProvider {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::with_model(cfg, &cfg.active_profile().model.clone())
    }

    pub fn with_model(cfg: &Config, model: &str) -> Result<Self> {
        let api_key = cfg.api_key()?.to_string();
        let base_url = cfg.resolved_base_url().trim_end_matches('/').to_string();
        let profile = cfg.active_profile();
        let vendor = crate::models::infer_vendor(profile, model);
        let dropped_parameters = RequestPlan::from_profile(profile, Some(model)).dropped_parameters;
        let resolved = crate::ModelCatalog::resolve(profile, Some(model));
        Ok(Self {
            client: crate::stream_guard::build_client()?,
            api_key,
            base_url,
            model: model.to_string(),
            stream_options: profile.effective_openai_stream_options_for_model(model),
            supports_vision: profile.vision,
            vendor,
            wire_protocol: WireProtocol::OpenAiChatCompletions,
            prompt_cache_mode: resolved.capabilities.prompt_cache.value,
            dropped_parameters,
        })
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            vendor: self.vendor.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            wire_protocol: self.wire_protocol.clone(),
        }
    }
}

// ── 请求/响应类型 ────────────────────────────────────────────────────────────

/// 请求体不再用结构体序列化——OpenAI provider 现在直接构造 `serde_json::Map<String, Value>`
/// 并由 thinking adapter 在序列化前 mutate body（写 thinking.type / enable_thinking
/// / reasoning_effort 等 vendor-specific 字段）。这样新增 vendor 字段不需要改
/// 这里的结构体定义。

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize, Default)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// DeepSeek / Qwen / GLM 等在多轮工具调用场景要求把上一轮的
    /// `reasoning_content` 拼回 assistant 消息，否则推理链断裂、续轮被拒。
    /// 仅在 `ThinkingResponseFields::echo_back_in_messages` 为 true 时序列化
    /// ——ollama / vllm 等虽然响应里带 reasoning_content，回传却会被拒。
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    /// MiniMax 特有的结构化 reasoning 块数组，同样只在 vendor 声明支持时回传。
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_details: Option<Vec<Value>>,
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
    /// OpenAI 官方 Responses 风格的 `reasoning` 字段（**不带** `_content` 后缀）。
    ///
    /// OpenRouter 这类聚合网关、以及 GPT-OSS 之后的 vLLM / SGLang，在
    /// `chat/completions` 上都改用这个字段名——实测同一道题 OpenRouter 返回
    /// `reasoning` 4873 字符 / `reasoning_tokens` 1327，而 `reasoning_content`
    /// 字段**根本不存在**。旧实现只认 `reasoning_content`，于是这些端点的思考
    /// 内容被 serde 静默丢弃：既不显示给用户，也不进历史，跨轮推理链断锚——
    /// **同一份 profile 换到 OpenAI 线，thinking「看起来消失了」有很大一部分
    /// 是这个字段名没被认**，而不是模型真的不思考。
    ///
    /// 与 `reasoning_content` 的关系：厂商只写其中一个（迁移期的 vLLM 会把
    /// `reasoning_content` 置成 null），同 chunk 两者都非空时由
    /// `stream_events_from_chunk` 择一，**不拼接**——拼接会让重影。
    #[serde(default)]
    reasoning: Option<String>,
    /// MiniMax `reasoning_details` 字段——结构化 reasoning 块数组（每项含
    /// `{type, text, id}` 等），用于多轮工具调用场景的 reasoning 完整回传。
    /// 国产其它 vendor 一般不写；serde 容忍缺失。
    #[serde(default)]
    reasoning_details: Option<Vec<Value>>,
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
    /// 输出侧的细分：其中有多少是**思考** token。
    ///
    /// `reasoning_tokens` 是 `completion_tokens` 的**子集**——可见正文 =
    /// `completion_tokens - reasoning_tokens`。
    ///
    /// 为什么值得单独解析：思考 token 按输出价计费，但它对用户不可见。
    /// 「这次贵在思考还是贵在正文」是两种完全不同的优化方向（前者调低
    /// effort，后者调输出长度），不拆开就只能给出一个「输出很贵」的
    /// 无从下手的结论。
    ///
    /// 字段名各家略有出入：OpenAI 官方是 `completion_tokens_details`，
    /// 部分兼容网关把它平铺到 `reasoning_tokens`。两种都收。
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    /// 部分网关（DeepSeek / 部分中转）把思考数**平铺**在 usage 顶层。
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

impl UsageData {
    /// 本次输出里被思考占用的 token 数。
    ///
    /// 优先取 `completion_tokens_details.reasoning_tokens`（OpenAI 官方形态），
    /// 退回顶层的 `reasoning_tokens`（部分兼容网关形态）。
    ///
    /// **不会超过 `completion_tokens`**：这是子集关系，若端点报出更大的值
    /// 说明口径有别的含义（个别网关把它当成独立的思考总量）。按子集口径
    /// 截断，面板上的「可见正文」才不会变成负数。
    fn reasoning_tokens(&self) -> u32 {
        let raw = self
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .or(self.reasoning_tokens)
            .unwrap_or(0);
        raw.min(self.completion_tokens.unwrap_or(u32::MAX))
    }
}

#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    started: bool,
}

// ── 内部模型 → API 请求转换 ───────────────────────────────────────────────────

/// 内部模型 → API 请求转换。`echo_reasoning` 来自
/// [`ThinkingSpec::response_fields`]：仅 vendor 声明需要时，才把 assistant 的
/// 推理内容拼回 `reasoning_content` / `reasoning_details`。
fn to_api_messages(
    messages: &[Message],
    supports_vision: bool,
    echo_reasoning: bool,
) -> Vec<ApiMessage> {
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
                                tool_call_id: Some(tool_use_id.clone()),
                                ..Default::default()
                            });
                        }
                        _ => {}
                    }
                }
                if !tool_results.is_empty() {
                    out.extend(tool_results);
                    // 同一条 user 消息里既有 tool_result 又有 Text，是**常态**
                    // 而不是边角：`Session::push_tool_result` 会把验收失败报告、
                    // 回合中追加的用户指令合并进最后一条 user 消息。旧实现
                    // 在这里直接丢掉 `text_parts`，模型带着缺失的上下文继续跑
                    // （验收证据、用户澄清全部静默消失）。
                    //
                    // OpenAI 协议要求 `tool` 消息紧邻 assistant 的 `tool_calls`，
                    // 所以文本只能**后置**成一条独立的 user 消息。
                    if !text_parts.is_empty() || (supports_vision && !image_parts.is_empty()) {
                        if supports_vision && !image_parts.is_empty() {
                            let mut content_parts: Vec<Value> = text_parts
                                .into_iter()
                                .map(|t| serde_json::json!({"type": "text", "text": t}))
                                .collect();
                            content_parts.extend(image_parts);
                            out.push(ApiMessage {
                                role: "user".to_string(),
                                content: Some(Value::Array(content_parts)),
                                ..Default::default()
                            });
                        } else {
                            out.push(ApiMessage {
                                role: "user".to_string(),
                                content: Some(Value::String(text_parts.join(""))),
                                ..Default::default()
                            });
                        }
                    }
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
                        ..Default::default()
                    });
                } else {
                    // 普通文本 user 消息：content 是纯字符串
                    out.push(ApiMessage {
                        role: "user".to_string(),
                        content: Some(Value::String(text_parts.join(""))),
                        ..Default::default()
                    });
                }
            }
            Role::Assistant => {
                let mut text_parts = vec![];
                let mut tool_calls = vec![];
                let mut reasoning_text = String::new();
                let mut reasoning_details: Vec<Value> = vec![];
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
                        // 国产 reasoning 模型（DeepSeek / Qwen / GLM / Doubao /
                        // Moonshot / MiniMax）在多轮工具调用场景要求把推理内容
                        // 拼回 assistant 消息，缺失会断链或被拒。Anthropic 协议
                        // 走 thinking 块（本文件不适用），此处按 vendor 声明的
                        // `echo_back_in_messages` 门控——ollama / vllm 等同样会在
                        // 响应里带 reasoning_content，但回传反而 400。
                        ContentBlock::Thinking {
                            thinking,
                            reasoning_details: details,
                            ..
                        } => {
                            if echo_reasoning {
                                reasoning_text.push_str(thinking);
                                if let Some(items) = details {
                                    reasoning_details.extend(items.iter().cloned());
                                }
                            }
                        }
                        // RedactedThinking 是 Anthropic 专属形态，OpenAI 协议无对应字段。
                        ContentBlock::RedactedThinking { .. } => {}
                        ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
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
                    reasoning_content: if echo_reasoning && !reasoning_text.is_empty() {
                        Some(reasoning_text)
                    } else {
                        None
                    },
                    reasoning_details: if echo_reasoning && !reasoning_details.is_empty() {
                        Some(reasoning_details)
                    } else {
                        None
                    },
                    ..Default::default()
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
        reasoning_tokens: usage.reasoning_tokens(),
    }
}

/// 一个 SSE chunk 可能同时携带 finish_reason 与 usage。旧实现只在 choices 为空
/// 时读取 usage，导致部分 OpenAI 兼容服务的精确 token 被静默丢弃。
///
/// reasoning_content（DeepSeek `deepseek-reasoner` 等）独立于 finish_reason /
/// content / tool_calls，因为它常与 content 在同一 chunk 出现，且 reasoning
/// 应在 content 之前上屏以模拟 Claude 原生 thinking 体验。
///
/// content / tool_calls / finish_reason 三者也相互独立：某些端点会把正文或
/// 工具参数与 finish_reason 合并进同一个 chunk，此时三者都必须被处理。
fn stream_events_from_chunk(
    tool_map: &mut std::collections::HashMap<usize, PendingToolCall>,
    chunk: SseChunk,
) -> Vec<StreamEvent> {
    let mut events = vec![];

    if let Some(choice) = chunk.choices.into_iter().next() {
        // 1. 推理文本独立分支。若与 content 同 chunk，优先推理后 content，
        //    模拟 thinking-then-text。
        //
        //    两个字段名**互不替代**，厂商只写其中一个：
        //      * `reasoning_content` —— DeepSeek / GLM / Qwen 等的既有约定；
        //      * `reasoning` —— vLLM / SGLang / GPT-OSS / OpenRouter 的新约定
        //        （vLLM RFC #27755，依据 GPT-OSS 发布时 OpenAI 对 Chat
        //        Completions 的指导）。迁移期 vLLM 会把 `reasoning_content`
        //        显式置为 **null**，内容全在 `reasoning` 里。
        //
        //    同一 chunk 两者都非空时以 `reasoning_content` 优先，否则同一段思考
        //    会被 emit 两次——TUI 上表现为思考内容重影。
        //
        //    判据用「非空」而不是 `is_none()`：部分网关发的是空串而非 null，
        //    那种形态同样必须让位给 `reasoning`。
        let reasoning_text = choice
            .delta
            .reasoning_content
            .as_deref()
            .filter(|text| !text.is_empty())
            .or_else(|| {
                choice
                    .delta
                    .reasoning
                    .as_deref()
                    .filter(|text| !text.is_empty())
            })
            .map(str::to_string);
        if let Some(text) = reasoning_text {
            events.push(StreamEvent::ThinkingDelta(text));
        }

        // 1b. reasoning_details 数组项（MiniMax `reasoning_details`）——
        //     结构化 reasoning 块，每项作为 ThinkingDetailsDelta 透传给上层，
        //     由 agent 累积到 ContentBlock::Thinking 的 metadata 字段。
        if let Some(details) = choice.delta.reasoning_details {
            for item in details {
                events.push(StreamEvent::ThinkingDetailsDelta(item));
            }
        }

        // 2. content / tool_calls / finish_reason 三者**相互独立**，不再互斥。
        //
        //    旧实现是一条 else-if 链，只处理第一个命中者：某端点把 content 与
        //    finish_reason（或 tool_calls）放进同一 chunk 时，后者被静默丢弃——
        //    表现为「模型明明说了话，界面却少一段正文」。这类端点形态确实存在
        //    （部分网关把最后一段文本与 finish_reason 合并成一个 chunk）。
        //
        //    发射顺序固定为 **content → tool_calls → finish_reason**：同 chunk 的
        //    正文与工具参数必须先于 MessageStop，否则回合收尾会盖掉最后一段输出。
        if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                events.push(StreamEvent::TextDelta(text));
            }
        }
        if let Some(tcs) = choice.delta.tool_calls {
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
        if let Some(fr) = choice.finish_reason {
            events.push(StreamEvent::MessageStop {
                stop_reason: parse_stop_reason(&fr),
            });
        }
    }

    // 3. usage 最后发：精确账本永远跟随 message stop 之后。
    if let Some(usage) = chunk.usage {
        events.push(usage_event(usage));
    }

    events
}

// ── Body 构造（独立函数以便单测） ──────────────────────────────────────────────

/// 该端点是否只认 `max_completion_tokens`（而非 `max_tokens`）。
///
/// 判据刻意**只认官方 OpenAI**：即便某个第三方也支持新名，保留 `max_tokens`
/// 对它们是无害的（老字段在兼容端点上仍是标准），而反过来一刀切会把
/// DeepSeek / MiniMax / GLM / Qwen / 火山的全部请求打断。这是一个
/// 「错了代价不对称」的判断，必须站在保守侧。
fn uses_max_completion_tokens(identity: &ModelIdentity) -> bool {
    identity.wire_protocol == WireProtocol::OpenAiChatCompletions
        && crate::thinking::normalize_vendor(&identity.vendor) == "openai"
}

/// 构造 OpenAI Chat Completions 请求 body Map。vendor-specific thinking
/// 字段由 `apply_thinking_to_openai_body` 在此调用后 mutate。
fn build_openai_request_body(
    identity: &ModelIdentity,
    system: &crate::provider::SystemPrompt<'_>,
    messages: &[Message],
    tools: &[ToolDefinition],
    opts: &crate::provider::RequestOptions,
    stream_options: bool,
    supports_vision: bool,
) -> Map<String, Value> {
    let max_tokens = opts.max_tokens;
    let echo_reasoning = crate::thinking::ThinkingSpec::for_vendor(
        &identity.vendor,
        &identity.wire_protocol,
        &identity.model,
    )
    .response_fields
    .echo_back_in_messages;
    // Chat Completions 没有 system 块 / cache_control 概念，只有一条 system
    // 消息，因此把 stable + volatile 拼成一条即可（`combined` 已处理空段）。
    let mut api_messages = vec![ApiMessage {
        role: "system".to_string(),
        content: Some(Value::String(system.combined())),
        ..Default::default()
    }];
    api_messages.extend(to_api_messages(messages, supports_vision, echo_reasoning));

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

    let mut body_map: Map<String, Value> = Map::new();
    body_map.insert("model".to_string(), Value::String(identity.model.clone()));
    body_map.insert(
        "messages".to_string(),
        Value::Array(
            api_messages
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect(),
        ),
    );
    if !api_tools.is_empty() {
        body_map.insert(
            "tools".to_string(),
            Value::Array(
                api_tools
                    .iter()
                    .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                    .collect(),
            ),
        );
    }
    // 输出上限字段名因端点而异：
    //   * 官方 OpenAI（o1/o3/o4-mini/gpt-5 这些 reasoning 模型）**只认**
    //     `max_completion_tokens`，传 `max_tokens` 会作为「不支持的参数」直接
    //     400；而且这个上限**同时覆盖 reasoning tokens**，语义与本项目的
    //     `max_tokens`（总输出预算）一致。
    //   * 第三方（DeepSeek / MiniMax / GLM / Qwen / 火山）文档里仍是
    //     `max_tokens`，一刀切会打断全部国产模板。
    // **两者不能同时发**：官方会把 `max_tokens` 判成非法参数。
    let max_tokens_key = if uses_max_completion_tokens(identity) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body_map.insert(max_tokens_key.to_string(), Value::Number(max_tokens.into()));
    body_map.insert("stream".to_string(), Value::Bool(true));
    if stream_options {
        body_map.insert(
            "stream_options".to_string(),
            serde_json::to_value(StreamOptions {
                include_usage: true,
            })
            .unwrap_or(Value::Null),
        );
    }

    // vendor-specific thinking 适配：按 vendor 写 thinking.type / enable_thinking
    // / reasoning_effort / thinking_budget 等字段；spec.control == Disabled
    // 时内部直接返回不 mutate body。
    apply_thinking_to_openai_body(identity, &mut body_map, opts);

    body_map
}

// ── Provider 实现 ─────────────────────────────────────────────────────────────

#[async_trait]
impl Provider for OpenAIProvider {
    /// Chat Completions 的缓存由**端点自己**按前缀做（OpenAI 自动 prompt
    /// caching、DeepSeek `Context Caching on Disk`），客户端不发任何字段，
    /// 因此这里只能回报 catalog 的判定，而不是像 Anthropic 那样跟一个运行时
    /// 降级位。
    ///
    /// 不覆盖的后果不只是「显示不对」：`agent.rs` 每轮把
    /// `session.prompt_cache_state` 同步成这里的返回值，默认实现恒 0 →
    /// `/cost` 在 DeepSeek/OpenAI 命中缓存时仍显示「未启用」，与 catalog 判定的
    /// `Automatic`、以及 `usage_event` 已经读到的 `cached_tokens` 三处互相矛盾。
    ///
    /// **永远不返回 `DOWNGRADED`**：那一档的语义是「客户端发的断点被端点拒绝」，
    /// 这条线根本不发断点。
    fn prompt_cache_state(&self) -> u8 {
        use crate::anthropic::{PROMPT_CACHE_STATE_OFF, PROMPT_CACHE_STATE_ON};
        match self.prompt_cache_mode {
            crate::PromptCacheMode::Automatic | crate::PromptCacheMode::ExplicitBreakpoints => {
                PROMPT_CACHE_STATE_ON
            }
            crate::PromptCacheMode::Unsupported => PROMPT_CACHE_STATE_OFF,
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
        let identity = self.identity();
        let body_map = build_openai_request_body(
            &identity,
            system,
            messages,
            tools,
            opts,
            self.stream_options,
            self.supports_vision,
        );
        let body_value = Value::Object(body_map);

        let url = format!("{}/chat/completions", self.base_url);
        // 连接前阶段带指数退避重试（429/5xx/连接错误），流未开始消费，重试透明
        let resp =
            crate::retry::send_with_retry(&crate::retry::RetryPolicy::default(), "OpenAI", || {
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("content-type", "application/json")
                    .json(&body_value)
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

        // idle 看门狗：连接被中间设备静默吞掉时 reqwest 永远不会返回错误，
        // 裸流会无限期挂住（详见 `stream_guard` 模块文档）。
        Ok(crate::stream_guard::with_idle_timeout(
            Box::pin(stream),
            crate::stream_guard::stream_idle_timeout(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 思考 token 必须被解析出来 —— 它是「贵在思考还是贵在正文」的答案。
    ///
    /// 官方形态是 `completion_tokens_details.reasoning_tokens`（子集于
    /// `completion_tokens`）；部分兼容网关把它平铺到 usage 顶层。两种都收。
    #[test]
    fn reasoning_tokens_are_parsed_from_the_official_shape() {
        let chunk = SseChunk {
            choices: vec![],
            usage: Some(UsageData {
                prompt_tokens: Some(100),
                completion_tokens: Some(500),
                prompt_tokens_details: None,
                completion_tokens_details: Some(CompletionTokensDetails {
                    reasoning_tokens: Some(300),
                }),
                reasoning_tokens: None,
            }),
        };
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Usage {
                output_tokens,
                reasoning_tokens,
                ..
            } => Some((*output_tokens, *reasoning_tokens)),
            _ => None,
        });
        assert_eq!(usage, Some((500, 300)));
    }

    /// 平铺形态（部分网关）：顶层 `reasoning_tokens` 也要认。
    #[test]
    fn reasoning_tokens_are_parsed_from_the_flat_shape() {
        let chunk = SseChunk {
            choices: vec![],
            usage: Some(UsageData {
                prompt_tokens: Some(100),
                completion_tokens: Some(200),
                prompt_tokens_details: None,
                completion_tokens_details: None,
                reasoning_tokens: Some(120),
            }),
        };
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        let reasoning = events.iter().find_map(|e| match e {
            StreamEvent::Usage {
                reasoning_tokens, ..
            } => Some(*reasoning_tokens),
            _ => None,
        });
        assert_eq!(reasoning, Some(120));
    }

    /// 端点报出**超过** `completion_tokens` 的思考数时必须截断：
    /// 那是端点口径不同（把它当独立思考总量），不截断会让「可见正文」
    /// 变成负数。
    #[test]
    fn reasoning_tokens_never_exceed_the_completion_tokens() {
        let chunk = SseChunk {
            choices: vec![],
            usage: Some(UsageData {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                prompt_tokens_details: None,
                completion_tokens_details: Some(CompletionTokensDetails {
                    reasoning_tokens: Some(999),
                }),
                reasoning_tokens: None,
            }),
        };
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        let reasoning = events.iter().find_map(|e| match e {
            StreamEvent::Usage {
                reasoning_tokens, ..
            } => Some(*reasoning_tokens),
            _ => None,
        });
        assert_eq!(reasoning, Some(50));
    }

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
                completion_tokens_details: None,
                reasoning_tokens: None,
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
                reasoning_tokens: 0,
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
                    reasoning: None,
                    reasoning_details: None,
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
                    reasoning: None,
                    tool_calls: None,
                    ..Delta::default()
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
                    reasoning: None,
                    tool_calls: None,
                    ..Delta::default()
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

    /// MiniMax `delta.reasoning_details` 是结构化 reasoning 块数组（每项
    /// `{type, text, id, ...}`），用于多轮工具调用场景 reasoning 完整保留。
    /// 每项发射一个 ThinkingDetailsDelta 事件，由 agent.rs 累积到
    /// `ContentBlock::Thinking.reasoning_details`。
    #[test]
    fn reasoning_details_emits_one_event_per_item() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: None,
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: Some(vec![
                        serde_json::json!({"type": "reasoning.text", "text": "step 1", "id": "r1"}),
                        serde_json::json!({"type": "reasoning.text", "text": "step 2", "id": "r2"}),
                    ]),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 2);
        let mut iter = events.into_iter();
        match iter.next().unwrap() {
            StreamEvent::ThinkingDetailsDelta(v) => {
                assert_eq!(v["type"], "reasoning.text");
                assert_eq!(v["text"], "step 1");
                assert_eq!(v["id"], "r1");
            }
            other => panic!("expected ThinkingDetailsDelta, got {other:?}"),
        }
        match iter.next().unwrap() {
            StreamEvent::ThinkingDetailsDelta(v) => {
                assert_eq!(v["text"], "step 2");
            }
            other => panic!("expected ThinkingDetailsDelta, got {other:?}"),
        }
    }

    /// reasoning_content + reasoning_details + content 共存时按
    /// reasoning_content → reasoning_details → content 顺序发射。
    #[test]
    fn reasoning_details_with_content_orders_thinking_first() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("answer".to_string()),
                    reasoning_content: Some("plan".to_string()),
                    reasoning: None,
                    reasoning_details: Some(vec![
                        serde_json::json!({"type": "reasoning.text", "text": "details"}),
                    ]),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::ThinkingDelta(t) if t == "plan"));
        match &events[1] {
            StreamEvent::ThinkingDetailsDelta(v) => assert_eq!(v["text"], "details"),
            other => panic!("expected ThinkingDetailsDelta, got {other:?}"),
        }
        assert!(matches!(&events[2], StreamEvent::TextDelta(t) if t == "answer"));
    }

    /// reasoning_details 缺失时仅按 content/reasoning_content 走原逻辑。
    #[test]
    fn reasoning_details_absent_is_no_op() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("ok".to_string()),
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "ok"));
    }

    /// reasoning_content 为空字符串时不 emit（避免上屏空 thinking）。
    #[test]
    fn empty_reasoning_content_is_skipped() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("hi".to_string()),
                    reasoning_content: Some(String::new()),
                    reasoning: None,
                    tool_calls: None,
                    ..Delta::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "hi"));
    }

    /// 端点把正文与 finish_reason 放进同一 chunk 时，正文**不能**被丢弃。
    ///
    /// 旧实现是一条 else-if 链：`finish_reason` 先命中，正文与工具参数被整段
    /// 吞掉——表现为「模型明明说了话，界面少一段正文」。
    #[test]
    fn content_and_finish_reason_in_same_chunk_are_both_emitted() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("final answer".to_string()),
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "final answer"));
        assert!(matches!(
            &events[1],
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn
            }
        ));
    }

    /// 正文 + 工具参数 + finish_reason 同 chunk：三者都发射，且顺序固定为
    /// content → tool_calls → MessageStop（收尾信号必须最后）。
    #[test]
    fn content_tool_calls_and_finish_reason_in_same_chunk_are_all_emitted() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("calling tool".to_string()),
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                    tool_calls: Some(vec![ToolCallDelta {
                        index: 0,
                        id: Some("call_x".to_string()),
                        function: Some(FunctionDelta {
                            name: Some("Read".to_string()),
                            arguments: Some(r#"{"path":"a.rs"}"#.to_string()),
                        }),
                    }]),
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        let mut tool_map = std::collections::HashMap::new();
        let events = stream_events_from_chunk(&mut tool_map, chunk);
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "calling tool"));
        assert!(matches!(
            &events[1],
            StreamEvent::ToolUseStart { ref id, ref name } if id == "call_x" && name == "Read"
        ));
        assert!(matches!(
            &events[2],
            StreamEvent::ToolUseDelta { ref id, ref json_delta }
                if id == "call_x" && json_delta == r#"{"path":"a.rs"}"#
        ));
        assert!(matches!(
            &events[3],
            StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse
            }
        ));
    }

    /// 思考 + 正文 + finish_reason 同 chunk：reasoning 仍排最前，正文不再被
    /// finish_reason 吞掉。
    #[test]
    fn reasoning_content_and_finish_reason_in_same_chunk_are_both_emitted() {
        let chunk = SseChunk {
            choices: vec![Choice {
                delta: Delta {
                    content: Some("answer".to_string()),
                    reasoning_content: Some("thinking".to_string()),
                    reasoning: None,
                    reasoning_details: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };

        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::ThinkingDelta(t) if t == "thinking"));
        assert!(matches!(&events[1], StreamEvent::TextDelta(t) if t == "answer"));
        assert!(matches!(
            &events[2],
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn
            }
        ));
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

        let api = to_api_messages(&msgs, true, false);
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

        let api = to_api_messages(&msgs, false, false);
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

        let api = to_api_messages(&msgs, true, false);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].content.as_ref().unwrap().as_str(), Some("hello"));
    }

    // ── reasoning 回传（echo_back_in_messages 接线） ──

    fn assistant_with_thinking() -> Vec<Message> {
        vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "先看 grep.rs".to_string(),
                    signature: String::new(),
                    reasoning_details: None,
                },
                ContentBlock::Text {
                    text: "done".to_string(),
                },
            ],
        }]
    }

    /// vendor 声明需要回传时，assistant 消息必须带出 `reasoning_content`，
    /// 否则国产 reasoning 模型在工具调用续轮里推理链断裂。
    #[test]
    fn assistant_echoes_reasoning_content_when_vendor_requires_it() {
        let api = to_api_messages(&assistant_with_thinking(), true, true);
        assert_eq!(api.len(), 1);
        assert_eq!(
            api[0].reasoning_content.as_deref(),
            Some("先看 grep.rs"),
            "echo_reasoning=true 时不得丢弃推理内容"
        );
        // 文本与 reasoning 各自独立，不能互相挤掉。
        assert_eq!(api[0].content.as_ref().unwrap().as_str(), Some("done"));
    }

    /// 反向不变式：vendor 不声明回传时字段必须整个不出现——多写一个未知字段
    /// 就可能被第三方端点判 400（这正是 ollama / vllm 的情况）。
    #[test]
    fn assistant_omits_reasoning_fields_when_vendor_does_not_require_echo() {
        let api = to_api_messages(&assistant_with_thinking(), true, false);
        assert_eq!(api.len(), 1);
        let json = serde_json::to_value(&api[0]).unwrap();
        assert!(
            json.get("reasoning_content").is_none(),
            "echo_reasoning=false 时 reasoning_content 必须缺省"
        );
        assert!(json.get("reasoning_details").is_none());
    }

    /// `reasoning` 字段（OpenAI 官方 Responses 风格 / OpenRouter 等网关）
    /// 必须被解析成 thinking——此前只认 `reasoning_content`，于是这些端点的
    /// 思考内容被 serde 静默丢弃：不显示给用户，也不进历史，跨轮推理链断锚。
    ///
    /// 实测（2026-10）：OpenRouter 同一道题返回 `reasoning` 4873 字符 /
    /// `reasoning_tokens` 1327，而 `reasoning_content` 字段**根本不存在**。
    #[test]
    fn reasoning_field_is_parsed_into_thinking_delta() {
        let chunk: SseChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"2","reasoning":"先算 1+1"}}]}"#,
        )
        .unwrap();
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::ThinkingDelta(text) if text == "先算 1+1"
            )),
            "reasoning 字段必须被当成 thinking 上屏：{events:?}"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(t) if t == "2")));
    }

    /// 两个字段名都出现时**不得重复计入**同一段思考——`reasoning_content`
    /// 优先，`reasoning` 只在它缺位时补位。
    #[test]
    fn reasoning_and_reasoning_content_are_not_double_counted() {
        let chunk: SseChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning":"来自 reasoning 字段","reasoning_content":"来自 reasoning_content 字段"}}]}"#,
        )
        .unwrap();
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        let thinking: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking,
            vec!["来自 reasoning_content 字段"],
            "两个字段同时出现时只应计入一段：{thinking:?}"
        );
    }

    /// vLLM / SGLang 迁移期的真实形态：`reasoning_content` 显式置 **null**、
    /// 内容全在 `reasoning` 里。旧实现只认前者，这条线等于完全没有思考。
    ///
    /// 与 `reasoning_and_reasoning_content_are_not_double_counted` 互补：
    /// 那条钉「都非空只取一个」，这条钉「前者为 null 不能让后者被吞」。
    /// 两种形态在真实网关上都出现过，缺任一条都会让修复只对一半流量生效。
    #[test]
    fn null_reasoning_content_does_not_swallow_reasoning_field() {
        let chunk: SseChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning":"vLLM 的思考","reasoning_content":null}}]}"#,
        )
        .unwrap();
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::ThinkingDelta(text) if text == "vLLM 的思考"
            )),
            "reasoning_content 为 null 时 reasoning 必须补位：{events:?}"
        );
    }

    /// 空串形态同样不能让位失败：有些网关发 `reasoning_content: ""` 而非 null。
    #[test]
    fn empty_string_reasoning_content_does_not_swallow_reasoning_field() {
        let chunk: SseChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning":"空串兜底","reasoning_content":""}}]}"#,
        )
        .unwrap();
        let events = stream_events_from_chunk(&mut std::collections::HashMap::new(), chunk);
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::ThinkingDelta(t) if t == "空串兜底")));
    }

    /// **解析出来 ≠ 要回传**：vLLM / ollama 的 `echo_back_in_messages` 仍为
    /// false，`reasoning` 只用于上屏与落盘，不能顺手打开回传——它们收这个
    /// 字段会 400。这条钉住「可见性」与「回传」两个维度不被合并。
    #[test]
    fn vllm_reasoning_is_visible_but_never_echoed() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "vLLM 的思考".to_string(),
                signature: String::new(),
                reasoning_details: None,
            }],
        }];
        // echo_reasoning=false 由 ThinkingSpec::vllm 的 response_fields 决定。
        let api = to_api_messages(&msgs, false, false);
        let json = serde_json::to_value(&api[0]).unwrap();
        assert!(json.get("reasoning_content").is_none(), "{json}");
        assert!(json.get("reasoning").is_none(), "{json}");
    }

    /// MiniMax 的 `reasoning_details` 与 `reasoning_content` 同时回传。
    #[test]
    fn assistant_echoes_minimax_reasoning_details() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "推理".to_string(),
                signature: String::new(),
                reasoning_details: Some(vec![serde_json::json!({"type": "text", "text": "推理"})]),
            }],
        }];
        let api = to_api_messages(&msgs, true, true);
        assert_eq!(api[0].reasoning_content.as_deref(), Some("推理"));
        let details = api[0]
            .reasoning_details
            .as_ref()
            .expect("details must echo");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["type"], "text");
    }

    /// 空 reasoning 不产生空字符串字段。
    #[test]
    fn empty_reasoning_serializes_no_field() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: String::new(),
                signature: String::new(),
                reasoning_details: None,
            }],
        }];
        let api = to_api_messages(&msgs, true, true);
        let json = serde_json::to_value(&api[0]).unwrap();
        assert!(json.get("reasoning_content").is_none());
        assert!(json.get("reasoning_details").is_none());
    }

    /// body 层集成：`echo_back_in_messages` 由 vendor 声明自动推导，
    /// 调用方不传开关——这里验证 deepseek 会带、ollama 不会。
    #[test]
    fn body_echoes_reasoning_for_deepseek_but_not_ollama() {
        let system = crate::provider::SystemPrompt {
            stable: "s",
            volatile: "",
        };
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            ..Default::default()
        };
        for (vendor, expect_echo) in [("deepseek", true), ("ollama", false)] {
            let body = build_openai_request_body(
                &identity_for(vendor, "some-model"),
                &system,
                &assistant_with_thinking(),
                &[],
                &opts,
                false,
                true,
            );
            let msgs = body["messages"].as_array().unwrap();
            let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
            assert_eq!(
                assistant.get("reasoning_content").is_some(),
                expect_echo,
                "vendor={vendor} 的 reasoning 回传判定不符预期"
            );
        }
    }

    // ── 官方 OpenAI 线（o 系列 / gpt-5 系）──

    /// 官方端点只认 `max_completion_tokens`。传 `max_tokens` 会被判成
    /// 「不支持的参数」直接 400——这是把 `provider = "openai"` 配进来的用户
    /// 必然撞上的第一个错误，而不是什么边缘情况。
    #[test]
    fn official_openai_body_uses_max_completion_tokens() {
        let id = identity_for("openai", "gpt-5");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            true,
            true,
        );
        assert_eq!(body["max_completion_tokens"], 4096);
        assert!(
            body.get("max_tokens").is_none(),
            "两条字段不能同时发：官方会把 max_tokens 判成非法参数"
        );
    }

    /// 第三方模板必须保持 `max_tokens`——一刀切会打断全部国产端点。
    #[test]
    fn third_party_body_keeps_max_tokens() {
        for (vendor, model) in [
            ("deepseek", "deepseek-chat"),
            ("minimax", "MiniMax-M3"),
            ("alibaba", "qwen3-coder-plus"),
            ("zhipu", "glm-4.6"),
            ("volcengine", "doubao-seed-1-6"),
            ("moonshot", "kimi-k3"),
        ] {
            let id = identity_for(vendor, model);
            let opts = crate::provider::RequestOptions {
                max_tokens: 4096,
                ..Default::default()
            };
            let body = build_openai_request_body(
                &id,
                &crate::provider::SystemPrompt::stable_only("sys"),
                &[],
                &[],
                &opts,
                false,
                true,
            );
            assert_eq!(body["max_tokens"], 4096, "{vendor}/{model}");
            assert!(
                body.get("max_completion_tokens").is_none(),
                "{vendor}/{model} 不该收到官方专属字段"
            );
        }
    }

    /// 官方线配 `reasoning_effort` 必须真的写进 body——此前 `for_vendor` 没有
    /// `"openai"` 臂，落到 generic 的 `Disabled`，effort 在**到达 provider 之前**
    /// 就被 catalog 层滤成 None，用户改配置毫无效果。
    #[test]
    fn official_openai_writes_reasoning_effort() {
        let id = identity_for("openai", "o3");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            true,
            true,
        );
        assert_eq!(body["reasoning_effort"], "high");
        // 官方线上除 reasoning_effort 之外**不得**出现任何 thinking 家族字段。
        for forbidden in ["thinking", "enable_thinking", "thinking_budget"] {
            assert!(
                body.get(forbidden).is_none(),
                "官方端点不认识 `{forbidden}`，发了就是 400：{body:?}"
            );
        }
    }

    /// 辅助调用（`text_only`）在官方线上必须降级成低档位而不是「什么都不发」：
    /// 不发字段＝端点按默认档位跑满 reasoning，正是历史故障「预算被思考吃光、
    /// 可见文本为空」的形态。
    #[test]
    fn official_openai_text_only_downgrades_to_low_effort() {
        let id = identity_for("openai", "o3");
        let opts = crate::provider::RequestOptions::text_only(512);
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            true,
            true,
        );
        assert_eq!(
            body["reasoning_effort"], "low",
            "text_only 必须收敛到低档位，否则 reasoning 会把预算吃满"
        );
    }

    /// 早期 `o1-mini` / `o3-mini` 不认 `reasoning_effort`：档位集合留空，
    /// 因此 body 里不能出现该字段（否则 400）。
    ///
    /// 这里必须连 `text_only()` 一起钉——辅助调用（compact / 分类器 / 标题）
    /// 走的正是那条路径，`thinking_switch = "disabled"` 若被降级成
    /// `reasoning_effort: "low"` 就是 400。
    #[test]
    fn official_openai_early_minis_omit_reasoning_effort() {
        for model in ["o1-mini", "o3-mini"] {
            let id = identity_for("openai", model);
            for opts in [
                crate::provider::RequestOptions {
                    max_tokens: 4096,
                    reasoning_effort: Some("high".to_string()),
                    ..Default::default()
                },
                crate::provider::RequestOptions::text_only(512),
            ] {
                let body = build_openai_request_body(
                    &id,
                    &crate::provider::SystemPrompt::stable_only("sys"),
                    &[],
                    &[],
                    &opts,
                    true,
                    true,
                );
                assert!(
                    body.get("reasoning_effort").is_none(),
                    "{model} 不支持 reasoning_effort（opts={opts:?}）：{body:?}"
                );
            }
        }
    }

    /// 官方 spec **不得**声明回传 reasoning：照抄 DeepSeek 臂会打开
    /// `echo_back_in_messages`，而官方 Chat Completions 收这个字段就 400。
    #[test]
    fn official_openai_never_echoes_reasoning() {
        let spec = crate::ThinkingSpec::for_vendor(
            "openai",
            &WireProtocol::OpenAiChatCompletions,
            "gpt-5",
        );
        assert!(!spec.response_fields.echo_back_in_messages);
        assert!(!spec.response_fields.reasoning_content);

        // 端到端：assistant 的 thinking 块不能变成请求里的 reasoning_content。
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            ..Default::default()
        };
        let body = build_openai_request_body(
            &identity_for("openai", "gpt-5"),
            &crate::provider::SystemPrompt::stable_only("sys"),
            &assistant_with_thinking(),
            &[],
            &opts,
            true,
            true,
        );
        let msgs = body["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert!(assistant.get("reasoning_content").is_none(), "{assistant}");
        assert!(assistant.get("reasoning_details").is_none(), "{assistant}");
    }

    // ── 缓存状态回报（P0-2）──

    /// 构造一个指定 vendor / 模型的 provider，只用于读 `prompt_cache_state`。
    ///
    /// vendor 不再是配置项，改为由 `base_url` + 模型名**推断**，所以这里
    /// 必须给出能推导出目标 vendor 的 base_url。
    fn provider_for(vendor: &str, model: &str) -> OpenAIProvider {
        let base_url = match vendor {
            "deepseek" => "https://api.deepseek.com",
            "minimax" => "https://api.minimaxi.com/v1",
            "alibaba" => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "zhipu" => "https://open.bigmodel.cn/api/paas/v4",
            "vllm" => "http://127.0.0.1:8000/v1",
            other => panic!("测试辅助函数未覆盖 vendor {other}"),
        };
        let profile = wyj_config::Profile {
            provider: wyj_config::Provider::OpenAI,
            model: model.to_string(),
            base_url: base_url.to_string(),
            ..wyj_config::Profile::default()
        };
        let cfg = wyj_config::Config {
            active_profile: profile.name.clone(),
            profiles: vec![profile],
            runtime_api_key: Some("sk-test".to_string()),
            ..wyj_config::Config::default()
        };
        OpenAIProvider::with_model(&cfg, model).expect("provider 构造不应依赖网络")
    }

    /// DeepSeek 的缓存由端点自动完成（`Context Caching on Disk`），
    /// `usage_event` 也确实读到了 `cached_tokens`。此前 `OpenAIProvider`
    /// 没覆盖 `prompt_cache_state()`，用的是默认 0，于是 `agent.rs` 每轮把
    /// 会话状态刷成「未启用」——`/cost` 在明明命中缓存时显示未启用，
    /// 与 catalog 判定的 `Automatic` 自相矛盾。
    #[test]
    fn deepseek_endpoint_reports_cache_on() {
        use crate::anthropic::{PROMPT_CACHE_STATE_OFF, PROMPT_CACHE_STATE_ON};
        let provider = provider_for("deepseek", "deepseek-chat");
        assert_eq!(provider.prompt_cache_state(), PROMPT_CACHE_STATE_ON);
        assert_ne!(provider.prompt_cache_state(), PROMPT_CACHE_STATE_OFF);
    }

    /// 用 profile 显式关掉时必须回报未启用——用户 override 优先于静态判定。
    #[test]
    fn explicit_cache_opt_out_reports_cache_off() {
        use crate::anthropic::PROMPT_CACHE_STATE_OFF;
        let profile = wyj_config::Profile {
            provider: wyj_config::Provider::OpenAI,
            model: "deepseek-chat".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            prompt_cache: Some(false),
            ..wyj_config::Profile::default()
        };
        let cfg = wyj_config::Config {
            active_profile: profile.name.clone(),
            profiles: vec![profile],
            runtime_api_key: Some("sk-test".to_string()),
            ..wyj_config::Config::default()
        };
        let provider = OpenAIProvider::with_model(&cfg, "deepseek-chat").unwrap();
        assert_eq!(provider.prompt_cache_state(), PROMPT_CACHE_STATE_OFF);
    }

    /// 不承诺前缀语义的第三方端点仍报未启用——不能为了「有缓存了」而
    /// 对所有 OpenAI 兼容端点一律报 ON。
    #[test]
    fn third_party_endpoint_reports_cache_off() {
        use crate::anthropic::PROMPT_CACHE_STATE_OFF;
        for (vendor, model) in [
            ("minimax", "MiniMax-M3"),
            ("alibaba", "qwen3-coder-plus"),
            ("zhipu", "glm-4.6"),
            ("vllm", "qwen3"),
        ] {
            let provider = provider_for(vendor, model);
            assert_eq!(
                provider.prompt_cache_state(),
                PROMPT_CACHE_STATE_OFF,
                "{vendor}/{model} 不该被报成已缓存"
            );
        }
    }

    // ── build_openai_request_body 与 thinking adapter 的集成单测 ──

    fn identity_for(vendor: &str, model: &str) -> ModelIdentity {
        ModelIdentity {
            vendor: vendor.to_string(),
            model: model.to_string(),
            base_url: "https://example.com".to_string(),
            wire_protocol: WireProtocol::OpenAiChatCompletions,
        }
    }

    #[test]
    fn body_includes_deepseek_thinking_with_effort() {
        let id = identity_for("deepseek", "deepseek-v4-pro");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            false,
            true,
        );
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn body_includes_qwen_budget_only() {
        let id = identity_for("alibaba", "qwen3-coder-plus");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            thinking_budget: Some(8192),
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            false,
            true,
        );
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["thinking_budget"], 8192);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn body_includes_doubao_thinking_object_with_clamped_budget() {
        let id = identity_for("volcengine", "doubao-seed-code");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            thinking_budget: Some(20_000),
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            false,
            true,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn body_includes_minimax_reasoning_split() {
        let id = identity_for("minimax", "MiniMax-M3");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            false,
            true,
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], true);
    }

    #[test]
    fn body_skips_thinking_when_disabled_and_user_opted_in_to_default() {
        let id = identity_for("zhipu", "glm-4.6");
        let opts = crate::provider::RequestOptions::default();
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("sys"),
            &[],
            &[],
            &opts,
            false,
            true,
        );
        // 没开 thinking 时 body 不带 thinking 字段
        assert!(body.get("thinking").is_none());
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn body_emits_official_openai_chat_completions_shape() {
        let id = identity_for("openai", "gpt-5");
        let opts = crate::provider::RequestOptions {
            max_tokens: 4096,
            ..Default::default()
        };
        let body = build_openai_request_body(
            &id,
            &crate::provider::SystemPrompt::stable_only("You are helpful."),
            &[],
            &[],
            &opts,
            false,
            true,
        );
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["stream"], true);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful.");
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn openai_system_message_concatenates_stable_and_volatile() {
        // Chat Completions 没有 system 块 / cache_control 概念，两段拼成一条
        // system 消息即可；拼接结果必须与 SystemPrompt::combined() 一致。
        let id = identity_for("openai", "gpt-test");
        let opts = crate::provider::RequestOptions::text_only(16);
        let system = crate::provider::SystemPrompt {
            stable: "STABLE",
            volatile: "VOLATILE",
        };
        let body = build_openai_request_body(&id, &system, &[], &[], &opts, false, true);
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], system.combined().as_str());
        assert!(messages[0]["content"].as_str().unwrap().contains("STABLE"));
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("VOLATILE"));
    }
}
