//! Thinking / reasoning 适配层。
//!
//! 把 vendor × protocol × mode × effort 维度的所有差异集中在一个模块，让 provider
//! 实现（`anthropic.rs` / `openai.rs`）只需要调用一个统一的 dispatch 入口。
//!
//! ## 设计目标
//!
//! 1. **OpenAI 协议路径真的把 thinking 发出去**：之前 `OpenAIProvider::stream`
//!    显式忽略 `opts.thinking_budget`。这里按 vendor 适配各家字段名（DeepSeek
//!    `thinking.type` / Qwen `enable_thinking` / Doubao `thinking` 对象 / GLM
//!    `thinking.type` / Moonshot `thinking.type` 或 `reasoning_effort` / MiniMax
//!    `thinking.type`），所有 adapter 一次性集中维护。
//! 2. **Anthropic 兼容端点细粒度降级**：第三方兼容端点（GLM / MiniMax / Moonshot
//!    的 `/anthropic` 路径）默认不发 `interleaved-thinking` beta header，避免 400。
//!    官方 Anthropic 端点照发。
//! 3. **能力目录按 vendor 准确标注**：`ThinkingSpec::for_vendor` 是 catalog 阶段
//!    唯一的真值来源，`model_catalog.rs` 不再一刀切 `BudgetTokens`。
//!
//! ## 模块结构
//!
//! - `ReasoningEffort`：内部档位枚举（`low / medium / high / max / xhigh` 等），
//!   与各家 enum 值有简单映射，**不**对外暴露在 `ThinkingMode` / `ReasoningRequest` 里。
//! - `ThinkingSpec`：vendor 静态声明的 thinking 形态（含 user_can_disable、
//!   lock_sampling_params、response_fields 等运行期提示）。
//! - `ThinkingAdapter`：trait，每个 vendor 一个 `&'static` 实例。`adapter_for()`
//!   按 `(wire_protocol, vendor)` 静态分派，避免 vtable 成本。
//! - `apply_*_thinking` 自由函数：provider 在序列化请求体后调用，把 thinking 参数
//!   写入 body / header。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::capabilities::ModelIdentity;
use crate::provider::RequestOptions;
use wyj_config::WireProtocol;

/// thinking 字段名版本变体。国产 vendor 历史上多次调整字段名
/// (`thinking.type` vs `enable_thinking` vs `reasoning_effort` 等),不同子版本
/// 走不同发送策略。默认 `StandardV1` 与现有 OpenAI Chat Completions 主流
/// 协议一致；`--probe full` 主动探测时可升级到 ExperimentalV2。
///
/// `detect_vendor_variant` 是占位函数：当前所有已知 vendor 都标 `StandardV1`，
/// 阶段 7 结构预留为主——后续真实探测接入时只需扩 match 即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VendorVariant {
    /// OpenAI Chat Completions 主流标准（默认）
    #[default]
    StandardV1,
    /// 探测发现需要兼容老版本字段（如 vendor 自定义变体）
    ExperimentalV2,
}

/// 按 vendor × model 名推导字段名版本。占位实现——目前所有已知 vendor 都
/// 走 StandardV1；阶段 7 预留接口，--probe full 实际接入探测后扩展。
pub fn detect_vendor_variant(vendor: &str, model: &str) -> VendorVariant {
    let _ = (vendor, model);
    VendorVariant::StandardV1
}

/// OpenAI-vendor `reasoning_effort` 字符串档位的统一表示。
/// 不暴露在 `ThinkingMode` 或 `ReasoningRequest` 里——`ReasoningRequest::Effort(String)`
/// 仍是 wire 协议，传字符串原值让各 vendor adapter 自己解读。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
    XHigh,
    /// GLM / MiniMax M2.x 的 adaptive 模式（服务端按查询自动决定）
    Adaptive,
}

impl ReasoningEffort {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            "xhigh" => Some(Self::XHigh),
            "adaptive" => Some(Self::Adaptive),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::XHigh => "xhigh",
            Self::Adaptive => "adaptive",
        }
    }
}

// ── Vendor 静态声明 ─────────────────────────────────────────────────────────

/// thinking 控制形态。决定 adapter 在 body 里写哪些字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingControl {
    /// 完全不支持（如 Ollama 普通模型、custom vendor）
    Disabled,
    /// 仅 thinking.type 开关（GLM、Moonshot k2.5/2.6、MiniMax 等）
    SwitchOnly,
    /// thinking.type + budget_tokens（DeepSeek 弱支持；Doubao 标准形态）
    SwitchPlusBudget,
    /// thinking.type + reasoning_effort 顶层（DeepSeek 标准形态）
    SwitchPlusEffort,
    /// 仅 budget_tokens（Anthropic 原生 thinking）
    BudgetOnly,
    /// 仅 reasoning_effort 顶层（Moonshot k3、Qwen 互斥时的 effort 分支）
    EffortOnly,
}

/// 响应里实际会出现哪些 thinking 字段，决定 OpenAI provider 是否解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingResponseFields {
    /// `delta.reasoning_content` / `message.reasoning_content` 是否可能存在
    pub reasoning_content: bool,
    /// `delta.reasoning_details` / `message.reasoning_details`（MiniMax 特有）
    pub reasoning_details: bool,
    /// 是否需要把 `reasoning_content` 拼回多轮 messages（GLM/Qwen/DeepSeek 工具调用场景必须）
    pub echo_back_in_messages: bool,
}

/// Vendor × model 的静态声明。catalog 阶段一次性查表，进程内常量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingSpec {
    pub control: ThinkingControl,
    /// 用户 profile 显式关闭时，是否允许（false = vendor 强制开，profile 设 None 也开）
    pub user_can_disable: bool,
    /// 思考启用时是否锁住 sampling 参数（DeepSeek/GLM 启用 thinking 后忽略 temperature/top_p）
    pub lock_sampling_params: bool,
    /// 该 vendor 是否原生支持 token 预算（false = 只支持 type 开关或 effort 档）
    pub budget_tokens_supported: bool,
    /// 该 vendor 支持哪些 effort 档位（空切片 = 不支持 effort）
    pub effort_levels: &'static [ReasoningEffort],
    pub response_fields: ThinkingResponseFields,
    /// 用户传 `disabled` 时是否被静默忽略（true = MiniMax M2.x / DeepSeek reasoner）
    pub ignore_disabled_when_forced: bool,
}

impl ThinkingSpec {
    /// Vendor × wire × model 的唯一查表入口。
    pub fn for_vendor(vendor: &str, wire: &WireProtocol, model: &str) -> Self {
        match (
            wire,
            normalize_vendor(vendor),
            model.to_ascii_lowercase().as_str(),
        ) {
            // ── Anthropic 协议 ──
            (WireProtocol::AnthropicMessages, "anthropic", _) => Self::anthropic_native(),
            (WireProtocol::AnthropicMessages, _, _) => Self::anthropic_compatible(),

            // ── OpenAI 协议 + 各国产 vendor ──
            (WireProtocol::OpenAiChatCompletions, "deepseek", m) => Self::deepseek(m),
            (WireProtocol::OpenAiChatCompletions, "alibaba", m) => Self::qwen(m),
            (WireProtocol::OpenAiChatCompletions, "zhipu", _) => Self::glm(),
            (WireProtocol::OpenAiChatCompletions, "volcengine", m) => Self::doubao(m),
            (WireProtocol::OpenAiChatCompletions, "moonshot", m) => Self::moonshot(m),
            (WireProtocol::OpenAiChatCompletions, "minimax", m) => Self::minimax(m),
            (WireProtocol::OpenAiChatCompletions, "ollama", m) => Self::ollama(m),
            (WireProtocol::OpenAiChatCompletions, "vllm", m) => Self::vllm(m),

            // ── OpenAI Responses / QwenNative / Gemini / 其它 vendor ──
            // 默认走 generic，不主动发 thinking；用户 profile 显式开启时由 adapter
            // 透传为 thinking.type=enabled（部分 Anthropic Responses 端点认得）。
            _ => Self::generic_openai(),
        }
    }

    // ── 各种 spec factory ──

    pub const fn anthropic_native() -> Self {
        Self {
            control: ThinkingControl::BudgetOnly,
            user_can_disable: true,
            lock_sampling_params: false,
            budget_tokens_supported: true,
            effort_levels: &[],
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }

    pub const fn anthropic_compatible() -> Self {
        // 第三方 Anthropic 兼容端点：默认不发 interleaved-thinking beta，
        // 但仍发 thinking 字段。budget_tokens 是 Anthropic 协议原生字段。
        Self {
            control: ThinkingControl::BudgetOnly,
            user_can_disable: true,
            lock_sampling_params: false,
            budget_tokens_supported: true,
            effort_levels: &[],
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }

    fn deepseek(model: &str) -> Self {
        let force_on =
            model.contains("reasoner") || model.contains("-r1") || model == "deepseek-r1";
        Self {
            control: ThinkingControl::SwitchPlusEffort,
            user_can_disable: !force_on,
            lock_sampling_params: true,
            budget_tokens_supported: false,
            effort_levels: &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: force_on,
        }
    }

    fn qwen(model: &str) -> Self {
        // -thinking 后缀强制开；qwen3-coder-plus 这类支持 thinking_budget；
        // reasoning_effort 与 thinking_budget 互斥（Qwen 协议约束）
        let force_on = model.contains("-thinking") || model.contains("qwq");
        Self {
            control: ThinkingControl::SwitchPlusBudget,
            user_can_disable: !force_on,
            lock_sampling_params: false,
            budget_tokens_supported: true,
            effort_levels: &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ],
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: force_on,
        }
    }

    fn glm() -> Self {
        Self {
            control: ThinkingControl::SwitchOnly,
            user_can_disable: true,
            lock_sampling_params: true, // GLM 启用 thinking 后 temperature 被忽略
            budget_tokens_supported: false,
            effort_levels: &[],
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: false,
        }
    }

    fn doubao(model: &str) -> Self {
        let force_on = model.contains("-thinking");
        Self {
            control: ThinkingControl::SwitchPlusBudget,
            user_can_disable: !force_on,
            lock_sampling_params: false,
            budget_tokens_supported: true,
            effort_levels: &[],
            response_fields: ThinkingResponseFields {
                reasoning_content: true,
                reasoning_details: false,
                echo_back_in_messages: true,
            },
            ignore_disabled_when_forced: force_on,
        }
    }

    fn moonshot(model: &str) -> Self {
        if model.contains("k3") {
            // kimi-k3 走顶层 reasoning_effort
            Self {
                control: ThinkingControl::EffortOnly,
                user_can_disable: true,
                lock_sampling_params: false,
                budget_tokens_supported: false,
                effort_levels: &[
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: false,
            }
        } else {
            // k2.5/2.6 走 thinking.type；强制思考的 -thinking/-code 后缀不认 disabled
            let force_on = model.contains("-thinking") || model.contains("-code");
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: !force_on,
                lock_sampling_params: false,
                budget_tokens_supported: false,
                effort_levels: &[],
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: force_on,
            }
        }
    }

    fn minimax(model: &str) -> Self {
        // M2.x 强制思考（`thinking.type=adaptive`，忽略 user disabled）；
        // M3 起允许显式关闭、并支持 effort 档位（low / high / max，
        // 对齐 Moonshot k3 的档位集合，避开 server 端可能不认的 xhigh/adaptive）。
        let is_m2 = model.contains("m2") && !model.contains("m3");
        if is_m2 {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: false,
                lock_sampling_params: false,
                budget_tokens_supported: false,
                effort_levels: &[],
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: true,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: true,
            }
        } else {
            Self {
                control: ThinkingControl::SwitchPlusEffort,
                user_can_disable: true,
                lock_sampling_params: false,
                budget_tokens_supported: false,
                effort_levels: &[
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: true,
                    echo_back_in_messages: true,
                },
                ignore_disabled_when_forced: false,
            }
        }
    }

    fn ollama(model: &str) -> Self {
        let supports_think = model.contains("think");
        if supports_think {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: true,
                lock_sampling_params: false,
                budget_tokens_supported: false,
                effort_levels: &[],
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: false,
                },
                ignore_disabled_when_forced: false,
            }
        } else {
            Self::generic_openai()
        }
    }

    fn vllm(model: &str) -> Self {
        if model.contains("think") || model.contains("reasoning") {
            Self {
                control: ThinkingControl::SwitchOnly,
                user_can_disable: true,
                lock_sampling_params: false,
                budget_tokens_supported: false,
                effort_levels: &[],
                response_fields: ThinkingResponseFields {
                    reasoning_content: true,
                    reasoning_details: false,
                    echo_back_in_messages: false,
                },
                ignore_disabled_when_forced: false,
            }
        } else {
            Self::generic_openai()
        }
    }

    const fn generic_openai() -> Self {
        Self {
            control: ThinkingControl::Disabled,
            user_can_disable: true,
            lock_sampling_params: false,
            budget_tokens_supported: false,
            effort_levels: &[],
            response_fields: ThinkingResponseFields {
                reasoning_content: false,
                reasoning_details: false,
                echo_back_in_messages: false,
            },
            ignore_disabled_when_forced: false,
        }
    }
}

fn normalize_vendor(vendor: &str) -> &'static str {
    match vendor.trim().to_ascii_lowercase().as_str() {
        "glm" | "zhipu" | "zai" | "z.ai" => "zhipu",
        "kimi" | "moonshot" => "moonshot",
        "qwen" | "bailian" | "alibaba" => "alibaba",
        "doubao" | "volcengine" | "ark" => "volcengine",
        "minimax" => "minimax",
        "deepseek" => "deepseek",
        "anthropic" => "anthropic",
        "openai" => "openai",
        "ollama" => "ollama",
        "vllm" => "vllm",
        _ => "custom",
    }
}

// ── ThinkingAdapter trait + dispatch ─────────────────────────────────────────

/// 各 vendor 的 thinking 应用策略。`&'static` 实例，dispatch 时零分配。
pub trait ThinkingAdapter: Send + Sync {
    /// 把 thinking 参数应用到 OpenAI 协议的请求 body。
    /// 仅在 control != Disabled 时调用；否则 body 不变。
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions);

    /// 是否在 Anthropic 兼容端点发 `interleaved-thinking` beta header。
    /// 官方 Anthropic 端点 → true；第三方兼容 → false。
    fn emit_interleaved_beta(&self, is_official_anthropic: bool) -> bool;
}

/// Dispatch：`match (wire, vendor)` 静态分派到具体 adapter。
pub fn adapter_for(identity: &ModelIdentity) -> &'static dyn ThinkingAdapter {
    let wire = &identity.wire_protocol;
    let vendor = normalize_vendor(&identity.vendor);
    match (wire, vendor) {
        (WireProtocol::AnthropicMessages, "anthropic") => &AnthropicNativeAdapter,
        (WireProtocol::AnthropicMessages, _) => &AnthropicCompatibleAdapter,
        (WireProtocol::OpenAiChatCompletions, "deepseek") => &DeepSeekOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "alibaba") => &QwenOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "zhipu") => &ZhipuOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "volcengine") => &VolcengineOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "moonshot") => &MoonshotOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, "minimax") => {
            // M3 起支持 reasoning_effort；M2.x 仍走 SwitchOnly。spec.effort_levels
            // 是否为空决定了是否能上送 effort，但 &'static adapter 拿不到 model
            // 信息，所以这里按 model 名分派到 MinimaxAdapter::M2 / M3。
            if identity.model.to_ascii_lowercase().contains("m3") {
                static M3_ADAPTER: MinimaxAdapter = MinimaxAdapter::M3;
                &M3_ADAPTER
            } else {
                static M2_ADAPTER: MinimaxAdapter = MinimaxAdapter::M2;
                &M2_ADAPTER
            }
        }
        (WireProtocol::OpenAiChatCompletions, "ollama") => &OllamaOpenAiAdapter,
        (WireProtocol::OpenAiChatCompletions, _) => &GenericOpenAiAdapter,
        _ => &GenericOpenAiAdapter,
    }
}

// ── Adapter 实现 ──────────────────────────────────────────────────────────────

/// 官方 Anthropic：thinking 字段照发 + interleaved beta 照发。
struct AnthropicNativeAdapter;

impl ThinkingAdapter for AnthropicNativeAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // Anthropic 协议不走 OpenAI body；anthropic.rs 自行处理。
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        true
    }
}

/// 第三方 Anthropic 兼容端点：thinking 字段照发，但不附加 interleaved beta。
struct AnthropicCompatibleAdapter;

impl ThinkingAdapter for AnthropicCompatibleAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // 同上，anthropic.rs 处理。
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        // 第三方兼容端点不认 interleaved-thinking beta 时会 400。
        // 调用方传入 is_official_anthropic_endpoint() 结果，但即便官方 Anthropic
        // 路径的兼容模式也按"无 beta"对待。
        false
    }
}

/// DeepSeek：`thinking.type` + 顶层 `reasoning_effort`。
struct DeepSeekOpenAiAdapter;

impl ThinkingAdapter for DeepSeekOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
        }
        if let Some(effort) = opts
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse)
        {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort.as_str().to_string()),
            );
        }
        // DeepSeek 启用 thinking 后 temperature 等被忽略，主动移除避免歧义
        if opts.thinking_budget.is_some() || opts.reasoning_effort.is_some() {
            body.remove("temperature");
            body.remove("top_p");
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Qwen：`enable_thinking` + `thinking_budget` 或 `reasoning_effort`（互斥）。
struct QwenOpenAiAdapter;

impl ThinkingAdapter for QwenOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        // Qwen 用顶层 bool `enable_thinking`，但 profile 也可能传 "false"/"true" 字符串
        let switch_type = effective_switch_type(opts, "enabled");
        if let Some(switch_type) = switch_type {
            let enabled = !matches!(switch_type.as_str(), "disabled" | "false");
            body.insert("enable_thinking".to_string(), Value::Bool(enabled));
        }

        // Qwen 互斥规则：thinking_budget 与 reasoning_effort 不能同传
        if let Some(budget) = opts.thinking_budget {
            body.insert("thinking_budget".to_string(), Value::Number(budget.into()));
            // effort 不发
        } else if let Some(effort_str) = opts.reasoning_effort.as_deref() {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort_str.to_string()),
            );
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// GLM/Z.ai：`thinking.type`；启用时锁 temperature。
struct ZhipuOpenAiAdapter;

impl ThinkingAdapter for ZhipuOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
        }
        if opts.thinking_budget.is_some() || opts.reasoning_effort.is_some() {
            // GLM 启用 thinking 后 temperature/top_p 被忽略，主动移除
            body.remove("temperature");
            body.remove("top_p");
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Doubao：`thinking.type` + `thinking.budget_tokens`。
struct VolcengineOpenAiAdapter;

impl ThinkingAdapter for VolcengineOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            let mut thinking = json!({ "type": switch_type });
            if let Some(budget) = opts.thinking_budget {
                // Doubao budget 经验上限 8192；超过时夹到 8192 而非 400
                let clamped = budget.min(8192);
                thinking["budget_tokens"] = json!(clamped);
            }
            body.insert("thinking".to_string(), thinking);
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Moonshot：k3 走 effort，其余走 thinking.type。
struct MoonshotOpenAiAdapter;

impl ThinkingAdapter for MoonshotOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        // 通过 opts 中携带的 vendor 信息推断 k3；这里简化为：若 reasoning_effort 已
        // 显式设置且 spec 为 EffortOnly（adapter 选型决定），则走 effort 路径。
        // 实际判别在 catalog 阶段，adapter 自身只关心 wire 字段名。
        if let Some(effort_str) = opts.reasoning_effort.as_deref() {
            body.insert(
                "reasoning_effort".to_string(),
                Value::String(effort_str.to_string()),
            );
        } else if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// MiniMax：`thinking.type`（M2.x 强制 adaptive；M3 接受 enabled/disabled）。
/// M3 同时支持顶层 `reasoning_effort`（low / high / max），与 switch 分支独立
/// 写入：user 既可以单独配 effort（adapter 默认 `thinking.type=enabled`），
/// 也可以单独配 switch（effort 不上送）。
///
/// M2.x 与 M3 形态差异显著：M2 SwitchOnly 强制开且**不**支持 effort（M2 服务端
/// 不认顶层 `reasoning_effort`，发出去会 400）；M3 SwitchPlusEffort 支持 effort
/// 但仅限 spec.effort_levels = [Low, High, Max] 列出的档位（其它如 xhigh、
/// adaptive 被 parse 后仍需过滤，避免 server 端不认）。
///
/// 单个 `&'static` adapter 实例无法携带 model 信息，因此拆成 enum 两个分支，
/// `adapter_for` 按 model 名（含 m3）分派到对应 `&'static MinimaxAdapter`。
enum MinimaxAdapter {
    M2,
    M3,
}

impl ThinkingAdapter for MinimaxAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if let Some(switch_type) = effective_switch_type(opts, "enabled") {
            body.insert("thinking".to_string(), json!({ "type": switch_type }));
            // MiniMax 启用 reasoning_split 让 thinking 与 content 分字段返回
            body.insert("reasoning_split".to_string(), Value::Bool(true));
        }
        // 只在 M3 路径上送 reasoning_effort，且必须命中 spec.effort_levels：
        //   MINIMAX_M3_EFFORT_LEVELS = [Low, High, Max]（对齐 Moonshot k3）
        // 其它已识别档位（xhigh / adaptive 等）即使 parse 成功也丢弃，
        // 避免 server 端不认返回 400。
        if matches!(self, Self::M3) {
            if let Some(effort) = opts
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse)
            {
                const MINIMAX_M3_EFFORT_LEVELS: &[ReasoningEffort] = &[
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ];
                if MINIMAX_M3_EFFORT_LEVELS.contains(&effort) {
                    body.insert(
                        "reasoning_effort".to_string(),
                        Value::String(effort.as_str().to_string()),
                    );
                }
            }
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Ollama：仅当 model 名含 "think" 时发 `think: true`。
struct OllamaOpenAiAdapter;

impl ThinkingAdapter for OllamaOpenAiAdapter {
    fn apply_openai(&self, body: &mut Map<String, Value>, opts: &RequestOptions) {
        if opts.thinking_budget.is_some() || opts.reasoning_effort.is_some() {
            body.insert("think".to_string(), Value::Bool(true));
        }
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

/// Generic OpenAI：不主动发 thinking；即使用户 profile 配置了 budget/effort，
/// 也不主动猜测未知 vendor 支持什么字段——保持请求体不变。
/// 阶段 7 字段名版本探测启用后，会先发探测请求再决定是否回填 thinking。
struct GenericOpenAiAdapter;

impl ThinkingAdapter for GenericOpenAiAdapter {
    fn apply_openai(&self, _body: &mut Map<String, Value>, _opts: &RequestOptions) {
        // 故意空：generic 兜底不应假设未知 vendor 支持 thinking。
        // 已有 RequestPlan.from_capabilities 在 capabilities.thinking == Unsupported
        // 时把 reasoning 设成 Disabled 并写 dropped_parameter（用户可见）。
    }

    fn emit_interleaved_beta(&self, _is_official: bool) -> bool {
        false
    }
}

// ── 自由函数：provider 调用入口 ──────────────────────────────────────────────

/// 推导 effective `thinking.type` 字符串。
///
/// 返回 `None` 表示"用户没显式指定，vendor 自己决定"（GLM adaptive 等场景）。
/// 返回 `Some(s)` 表示"应该写到 body 里"。
///
/// 优先级：
/// 1. `opts.thinking_switch`（profile 显式覆盖）→ Some(trim+lowercase)
/// 2. 用户开了 budget/effort → Some(default)
/// 3. 否则 → None（不写 thinking 字段）
fn effective_switch_type(opts: &RequestOptions, default: &str) -> Option<String> {
    if let Some(explicit) = opts.thinking_switch.as_deref() {
        return Some(explicit.trim().to_ascii_lowercase());
    }
    if opts.thinking_budget.is_some() || opts.reasoning_effort.is_some() {
        Some(default.to_string())
    } else {
        None
    }
}

/// OpenAI provider 调用的统一入口。在 `OpenAIProvider::stream` 序列化 body 后调用。
/// 当 `ThinkingSpec::for_vendor(...).control == Disabled` 时直接返回，不改 body
/// （generic 与不支持的 vendor 都不会自作主张塞 thinking 字段）。
pub fn apply_thinking_to_openai_body(
    identity: &ModelIdentity,
    body: &mut Map<String, Value>,
    opts: &RequestOptions,
) {
    let spec = ThinkingSpec::for_vendor(&identity.vendor, &identity.wire_protocol, &identity.model);
    if matches!(spec.control, ThinkingControl::Disabled) {
        return;
    }
    let adapter = adapter_for(identity);
    adapter.apply_openai(body, opts);
}

/// Anthropic provider 调用的统一入口。返回是否发 `interleaved-thinking-2025-05-14` beta。
pub fn should_emit_interleaved_beta(identity: &ModelIdentity, is_official_anthropic: bool) -> bool {
    let adapter = adapter_for(identity);
    adapter.emit_interleaved_beta(is_official_anthropic)
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ModelIdentity;

    fn identity(vendor: &str, wire: WireProtocol, model: &str) -> ModelIdentity {
        ModelIdentity {
            vendor: vendor.to_string(),
            model: model.to_string(),
            base_url: "https://example.com".to_string(),
            wire_protocol: wire,
        }
    }

    #[test]
    fn reasoning_effort_parse_round_trip() {
        for s in [
            "minimal", "low", "medium", "high", "max", "xhigh", "adaptive",
        ] {
            let parsed = ReasoningEffort::parse(s).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!(ReasoningEffort::parse("unknown").is_none());
    }

    #[test]
    fn vendor_normalization_aliases() {
        assert_eq!(normalize_vendor("GLM"), "zhipu");
        assert_eq!(normalize_vendor("z.ai"), "zhipu");
        assert_eq!(normalize_vendor("Kimi"), "moonshot");
        assert_eq!(normalize_vendor("Moonshot"), "moonshot");
        assert_eq!(normalize_vendor("Qwen"), "alibaba");
        assert_eq!(normalize_vendor("Bailian"), "alibaba");
        assert_eq!(normalize_vendor("doubao"), "volcengine");
        assert_eq!(normalize_vendor("ark"), "volcengine");
        assert_eq!(normalize_vendor("MiniMax"), "minimax");
        assert_eq!(normalize_vendor("DeepSeek"), "deepseek");
    }

    #[test]
    fn anthropic_native_spec_is_budget_only() {
        let spec = ThinkingSpec::for_vendor(
            "anthropic",
            &WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        );
        assert_eq!(spec.control, ThinkingControl::BudgetOnly);
        assert!(spec.budget_tokens_supported);
        assert!(spec.user_can_disable);
    }

    #[test]
    fn anthropic_compatible_spec_omits_interleaved_beta() {
        let adapter = adapter_for(&identity(
            "zhipu",
            WireProtocol::AnthropicMessages,
            "glm-4.6",
        ));
        assert!(!adapter.emit_interleaved_beta(false));
        assert!(!adapter.emit_interleaved_beta(true));
    }

    #[test]
    fn anthropic_official_emits_interleaved_beta() {
        let adapter = adapter_for(&identity(
            "anthropic",
            WireProtocol::AnthropicMessages,
            "claude-opus-4-8",
        ));
        assert!(adapter.emit_interleaved_beta(true));
    }

    #[test]
    fn deepseek_hybrid_uses_switch_plus_effort() {
        let spec = ThinkingSpec::for_vendor(
            "deepseek",
            &WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchPlusEffort);
        assert!(spec.user_can_disable);
        assert!(!spec.ignore_disabled_when_forced);
    }

    #[test]
    fn deepseek_reasoner_is_forced_on() {
        let spec = ThinkingSpec::for_vendor(
            "deepseek",
            &WireProtocol::OpenAiChatCompletions,
            "deepseek-reasoner",
        );
        assert!(!spec.user_can_disable);
        assert!(spec.ignore_disabled_when_forced);
    }

    #[test]
    fn qwen_thinking_suffix_is_forced_on() {
        let spec =
            ThinkingSpec::for_vendor("alibaba", &WireProtocol::OpenAiChatCompletions, "qwq-plus");
        assert!(!spec.user_can_disable);
        assert_eq!(spec.control, ThinkingControl::SwitchPlusBudget);
    }

    #[test]
    fn glm_openai_is_switch_only_and_locks_sampling() {
        let spec =
            ThinkingSpec::for_vendor("zhipu", &WireProtocol::OpenAiChatCompletions, "glm-4.6");
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
        assert!(spec.lock_sampling_params);
        assert!(!spec.budget_tokens_supported);
    }

    #[test]
    fn doubao_thinking_suffix_is_forced_on() {
        let spec = ThinkingSpec::for_vendor(
            "volcengine",
            &WireProtocol::OpenAiChatCompletions,
            "doubao-1.5-thinking-pro",
        );
        assert!(!spec.user_can_disable);
    }

    #[test]
    fn moonshot_k3_uses_effort_only() {
        let spec =
            ThinkingSpec::for_vendor("moonshot", &WireProtocol::OpenAiChatCompletions, "kimi-k3");
        assert_eq!(spec.control, ThinkingControl::EffortOnly);
    }

    #[test]
    fn moonshot_k2_5_uses_switch_only() {
        let spec = ThinkingSpec::for_vendor(
            "moonshot",
            &WireProtocol::OpenAiChatCompletions,
            "kimi-k2.5",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
    }

    #[test]
    fn moonshot_thinking_suffix_is_forced_on() {
        let spec = ThinkingSpec::for_vendor(
            "moonshot",
            &WireProtocol::OpenAiChatCompletions,
            "kimi-k2-thinking",
        );
        assert!(!spec.user_can_disable);
    }

    #[test]
    fn minimax_m2_is_forced_on_with_details() {
        let spec = ThinkingSpec::for_vendor(
            "minimax",
            &WireProtocol::OpenAiChatCompletions,
            "MiniMax-M2",
        );
        assert!(!spec.user_can_disable);
        assert!(spec.response_fields.reasoning_details);
    }

    #[test]
    fn minimax_m3_can_be_disabled() {
        let spec = ThinkingSpec::for_vendor(
            "minimax",
            &WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        );
        assert!(spec.user_can_disable);
        // M3 从 v1.5.9 起走 SwitchPlusEffort：保留 thinking.type 开关同时
        // 接受 reasoning_effort 档位（low/high/max，对齐 Moonshot k3）。
        assert_eq!(spec.control, ThinkingControl::SwitchPlusEffort);
        assert_eq!(
            spec.effort_levels,
            &[
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ][..]
        );
        assert!(!spec.budget_tokens_supported);
        assert!(!spec.ignore_disabled_when_forced);
        assert!(spec.response_fields.reasoning_details);
    }

    /// M3 路径：switch + effort 两条字段独立写入 body。user 既可以单独配 switch、
    /// 也可以单独配 effort（adapter 因 effort 非空回落到默认 enabled），也可以两者
    /// 都配。M3 从 v1.5.9 起走 SwitchPlusEffort（与 DeepSeek / Moonshot k3 同形态）。
    #[test]
    fn minimax_m3_emits_switch_and_effort_independently() {
        let id = identity("minimax", WireProtocol::OpenAiChatCompletions, "MiniMax-M3");
        let adapter = adapter_for(&id);

        // switch + effort 同时配：两条字段独立写入。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            thinking_switch: Some("enabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert_eq!(body["reasoning_effort"], "high");

        // 只配 effort：effective_switch_type 因 effort 非空回落到 "enabled"，
        // body 同时含 thinking + reasoning_split + reasoning_effort。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("max".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert_eq!(body["reasoning_effort"], "max");

        // 只配 switch：effort 不上送（保持 switch 与 effort 独立）。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("enabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert!(body.get("reasoning_effort").is_none());
    }

    /// M3 effort 档位必须能被 ReasoningEffort::parse 识别；非法字符串（不在
    /// spec.effort_levels 内的）被 and_then(parse) 静默丢弃，避免 400。
    #[test]
    fn minimax_m3_effort_levels_round_trip() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        for effort in ["low", "high", "max"] {
            let mut body = Map::new();
            let opts = RequestOptions {
                max_tokens: 1024,
                reasoning_effort: Some(effort.to_string()),
                ..Default::default()
            };
            adapter.apply_openai(&mut body, &opts);
            assert_eq!(body["reasoning_effort"], effort, "档位 {effort} 应原样上送");
        }
        // 非法档位（xhigh 不在 M3 档位列表里）：parse 失败，adapter 不上送。
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert!(
            body.get("reasoning_effort").is_none(),
            "xhigh 不在 M3 档位列表，应被 parse 过滤: {body:?}"
        );
    }

    /// M2.x 仍是 SwitchOnly 强制开；adapter 不上送 reasoning_effort——
    /// 冻结当前 M2 行为，避免 M2 误支持 effort（M2 服务端若不认 effort 字段会 400）。
    #[test]
    fn minimax_m2_apply_does_not_emit_effort() {
        let id = identity("minimax", WireProtocol::OpenAiChatCompletions, "MiniMax-M2");
        let adapter = adapter_for(&id);
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            thinking_switch: Some("enabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
        assert!(
            body.get("reasoning_effort").is_none(),
            "M2 SwitchOnly 不应上送 reasoning_effort: {body:?}"
        );
    }

    /// M3 + `thinking_switch = "disabled"`：body 写 thinking.type="disabled"，
    /// 按当前 adapter 实现同时写 reasoning_split=true（关掉也写，
    /// 让服务端回包时仍走分字段路径；这是当前代码事实，不是建议）。
    /// 该测试冻结这个行为，避免后续静默改动。
    #[test]
    fn minimax_m3_disabled_switch_writes_disabled_type_and_split() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("disabled".to_string()),
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["reasoning_split"], Value::Bool(true));
    }

    /// M3 + 完全没设任何开关：effective_switch_type 返回 None，body 不动。
    /// 这是 SwitchOnly vendor 的"用户没说就什么都不发"基线。
    #[test]
    fn minimax_m3_no_switch_leaves_body_untouched() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            ..Default::default()
        };
        adapter.apply_openai(&mut body, &opts);
        assert!(body.is_empty(), "不应写入任何 thinking 字段: {body:?}");
    }

    /// MiniMax 走 OpenAI 协议——interleaved-thinking beta 永远不发，
    /// 即便 base_url 指向官方 Anthropic 兼容端点也不发。
    #[test]
    fn minimax_never_emits_interleaved_beta() {
        let adapter = adapter_for(&identity(
            "minimax",
            WireProtocol::OpenAiChatCompletions,
            "MiniMax-M3",
        ));
        assert!(!adapter.emit_interleaved_beta(false));
        assert!(!adapter.emit_interleaved_beta(true));
    }

    #[test]
    fn ollama_think_model_uses_switch_only() {
        let spec = ThinkingSpec::for_vendor(
            "ollama",
            &WireProtocol::OpenAiChatCompletions,
            "qwen3-think",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
    }

    #[test]
    fn ollama_non_think_model_is_disabled() {
        let spec =
            ThinkingSpec::for_vendor("ollama", &WireProtocol::OpenAiChatCompletions, "qwen3");
        assert_eq!(spec.control, ThinkingControl::Disabled);
    }

    #[test]
    fn vllm_thinking_model_is_switch_only() {
        let spec = ThinkingSpec::for_vendor(
            "vllm",
            &WireProtocol::OpenAiChatCompletions,
            "deepseek-r1-distill-reasoning",
        );
        assert_eq!(spec.control, ThinkingControl::SwitchOnly);
    }

    #[test]
    fn unknown_vendor_openai_falls_back_to_generic_disabled() {
        let spec =
            ThinkingSpec::for_vendor("unknown", &WireProtocol::OpenAiChatCompletions, "mystery");
        assert_eq!(spec.control, ThinkingControl::Disabled);
    }

    #[test]
    fn openai_responses_falls_back_to_generic() {
        let spec = ThinkingSpec::for_vendor("openai", &WireProtocol::OpenAiResponses, "gpt-5");
        assert_eq!(spec.control, ThinkingControl::Disabled);
    }

    // ── apply_openai 行为 ──

    #[test]
    fn deepseek_apply_with_effort_emits_switch_and_effort() {
        let id = identity(
            "deepseek",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        // DeepSeek 启用后 temperature 应被移除（profile 默认值）
    }

    #[test]
    fn qwen_budget_and_effort_are_mutually_exclusive() {
        let id = identity(
            "alibaba",
            WireProtocol::OpenAiChatCompletions,
            "qwen3-coder-plus",
        );
        let mut body = Map::new();

        // 给 budget
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_budget: Some(4096),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["thinking_budget"], 4096);
        assert!(body.get("reasoning_effort").is_none());

        // 给 effort
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("thinking_budget").is_none());
    }

    #[test]
    fn glm_with_budget_locks_sampling_params() {
        let id = identity("zhipu", WireProtocol::OpenAiChatCompletions, "glm-4.6");
        let mut body = Map::new();
        body.insert("temperature".to_string(), json!(0.6));
        body.insert("top_p".to_string(), json!(0.9));
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn doubao_clamps_budget_above_8192() {
        let id = identity(
            "volcengine",
            WireProtocol::OpenAiChatCompletions,
            "doubao-seed-code",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_budget: Some(20_000),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn minimax_m2_emit_reasoning_split() {
        let id = identity("minimax", WireProtocol::OpenAiChatCompletions, "MiniMax-M2");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_split"], true);
    }

    #[test]
    fn explicit_thinking_switch_overrides_default() {
        let id = identity(
            "deepseek",
            WireProtocol::OpenAiChatCompletions,
            "deepseek-v4-pro",
        );
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            thinking_switch: Some("auto".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["thinking"]["type"], "auto");
    }

    #[test]
    fn ollama_think_model_emits_think_true() {
        let id = identity("ollama", WireProtocol::OpenAiChatCompletions, "qwen3-think");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert_eq!(body["think"], true);
    }

    #[test]
    fn ollama_non_think_model_emits_nothing() {
        let id = identity("ollama", WireProtocol::OpenAiChatCompletions, "qwen3");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert!(body.is_empty());
    }

    #[test]
    fn generic_openai_does_not_invent_thinking_field() {
        // Generic 兜底不应自作主张塞 thinking.type=enabled；catalog 阶段
        // 已经把 unknown vendor 的 capabilities.thinking 标成 Unsupported，
        // RequestPlan.from_capabilities 会把 reasoning 设成 Disabled 并写
        // dropped_parameter（用户可见）。这里验证 adapter 真的不动 body。
        let id = identity("custom", WireProtocol::OpenAiChatCompletions, "mystery");
        let mut body = Map::new();
        let opts = RequestOptions {
            max_tokens: 1024,
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        apply_thinking_to_openai_body(&id, &mut body, &opts);
        assert!(body.is_empty());
    }
}
