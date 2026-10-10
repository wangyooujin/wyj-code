//! 国内模型优先的静态能力目录。
//!
//! 静态目录只表达保守兼容默认值，不等价于在线验证。真正的验证状态由
//! `/model doctor --probe ...` 写入 capability cache 后提升。

use serde::{Deserialize, Serialize};
use wyj_config::{Profile, Provider, WireProtocol};

use crate::capabilities::{
    sanitized_base_url, Capability, CapabilitySource, Confidence, ModelCapabilities, ModelIdentity,
    ModelQuirk, PromptCacheMode, PromptDialect, ThinkingMode,
};
use crate::capability_cache::CapabilityCache;
use crate::thinking::{ThinkingControl, ThinkingSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Reference,
    StaticOnly,
    LiveVerified,
    Experimental,
    CustomUnverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogResolution {
    pub identity: ModelIdentity,
    pub capabilities: ModelCapabilities,
    pub endpoint_type: String,
    pub verification_status: VerificationStatus,
    pub known_degradations: Vec<String>,
    pub documentation_url: Option<String>,
    pub catalog_updated_at: String,
}

pub struct ModelCatalog;

impl ModelCatalog {
    pub fn resolve(profile: &Profile, model_override: Option<&str>) -> CatalogResolution {
        Self::resolve_with_cache(profile, model_override, None)
    }

    /// 带 capability cache 的 resolve 版本。`cache` 为 Some 且命中时：
    ///
    /// 1. 把 `rejected_parameters` 里出现过的 thinking 系列参数强制标 Unsupported，
    ///    避免下次 stream() 又发端点已确认不认的字段；
    /// 2. 把 live probe 实测出的并发工具能力反映到 capabilities
    ///    （见 `apply_probed_capabilities`），并把验证状态提升为 `LiveVerified`。
    ///
    /// 用户在 profile 里显式写的 `max_tools_per_turn` 优先级最高，最后覆盖。
    pub fn resolve_with_cache(
        profile: &Profile,
        model_override: Option<&str>,
        cache: Option<&CapabilityCache>,
    ) -> CatalogResolution {
        let model = model_override.unwrap_or(&profile.model);
        let vendor = profile
            .vendor
            .as_deref()
            .map(normalize_vendor)
            .unwrap_or_else(|| infer_vendor(profile, model));
        let wire_protocol = profile.effective_wire_protocol();
        let identity = ModelIdentity {
            vendor: vendor.clone(),
            model: model.to_string(),
            base_url: sanitized_base_url(&resolved_base_url(profile)),
            wire_protocol: wire_protocol.clone(),
        };

        let mut capabilities = base_capabilities(profile, model, &vendor, &wire_protocol);
        let mut probed = false;
        if let Some(cache) = cache {
            // 顺序要紧：探针快照先落，`rejected_parameters` 后落。
            // 端点真的返回过 400（`record_rejection`）是比几天前的探针更强、更新
            // 的信号；反过来先落拒绝记录、再整体替换探针快照，会把它刚设好的
            // `thinking=Unsupported` 冲掉。另注意 `record_rejection` 自己也会写
            // 一条 cache 记录，所以「有记录」不等于「做过探针」。
            probed = apply_probed_capabilities(cache, &identity, &mut capabilities);
            apply_rejected_parameters(cache, &identity, &mut capabilities);
        }
        apply_user_parallel_override(profile, &mut capabilities);
        let (endpoint_type, mut verification_status, known_degradations, documentation_url) =
            catalog_metadata(profile, &vendor);
        if probed && verification_status == VerificationStatus::StaticOnly {
            verification_status = VerificationStatus::LiveVerified;
        }

        if matches!(verification_status, VerificationStatus::Experimental) {
            capabilities.quirks.push(ModelQuirk::Custom(
                "OpenAI-compatible behavior depends on the local server build and launch flags"
                    .to_string(),
            ));
        }

        CatalogResolution {
            identity,
            capabilities,
            endpoint_type,
            verification_status,
            known_degradations,
            documentation_url,
            catalog_updated_at: "2026-08-02".to_string(),
        }
    }
}

/// 把 cache 里 `rejected_parameters` 中的 thinking 系列参数反映到 capabilities：
/// 强制 `capabilities.thinking` 为 `Unsupported` 并把 source 标为 `UserOverride`
/// （端点运行时拒绝 = 用户实际不能开）。幂等：多次调用无副作用。
fn apply_rejected_parameters(
    cache: &CapabilityCache,
    identity: &ModelIdentity,
    capabilities: &mut ModelCapabilities,
) {
    let Ok(Some(record)) = cache.load(identity) else {
        return;
    };
    let blocked = record.rejected_parameters.iter().any(|r| {
        matches!(
            r.parameter.as_str(),
            "thinking"
                | "thinking_budget"
                | "interleaved_thinking"
                | "reasoning_effort"
                | "thinking_switch"
        )
    });
    if blocked {
        capabilities.thinking = Capability::new(
            ThinkingMode::Unsupported,
            CapabilitySource::UserOverride,
            Confidence::High,
        );
        capabilities.interleaved_thinking =
            Capability::new(false, CapabilitySource::UserOverride, Confidence::High);
    }
}

fn base_capabilities(
    profile: &Profile,
    model: &str,
    vendor: &str,
    wire_protocol: &WireProtocol,
) -> ModelCapabilities {
    let is_reference = matches!(vendor, "anthropic" | "openai");
    let is_local = matches!(vendor, "ollama" | "vllm");
    let tool_calling = !is_local || model.to_ascii_lowercase().contains("tool");
    let parallel_tools = is_reference;
    let thinking = match ThinkingSpec::for_vendor(vendor, wire_protocol, model).control {
        ThinkingControl::Disabled => protocol_cap(ThinkingMode::Unsupported),
        ThinkingControl::BudgetOnly => Capability::new(
            ThinkingMode::BudgetTokens,
            CapabilitySource::VerifiedCatalog,
            Confidence::Medium,
        ),
        ThinkingControl::SwitchPlusBudget => {
            if ThinkingSpec::for_vendor(vendor, wire_protocol, model).budget_tokens_supported {
                Capability::new(
                    ThinkingMode::BudgetTokens,
                    CapabilitySource::VerifiedCatalog,
                    Confidence::Medium,
                )
            } else {
                Capability::new(
                    ThinkingMode::Effort,
                    CapabilitySource::VerifiedCatalog,
                    Confidence::Medium,
                )
            }
        }
        ThinkingControl::SwitchPlusEffort
        | ThinkingControl::EffortOnly
        | ThinkingControl::SwitchOnly => Capability::new(
            ThinkingMode::Effort,
            CapabilitySource::VerifiedCatalog,
            Confidence::Medium,
        ),
    };
    // `prompt_cache` 能力必须与**实际发请求时的行为**一致，否则 `model doctor`
    // 会一边显示 "unsupported"、一边用户明明开了 `prompt_cache = true` 且请求
    // 真的带了 cache_control——自相矛盾还会让人以为缓存没生效。
    //
    // 因此：用户在 profile 里显式写了 `prompt_cache` 就按 UserOverride 采信
    // （他比静态目录更清楚自己的端点）；没写才回落到"官方端点支持、第三方
    // 保守视为不支持"的协议默认值。
    //
    // ⚠️ 注意置信度：第三方端点的"支持"是**用户断言**，不是实测。真伪由
    // `AnthropicProvider` 的 400 自动降级裁决，结果回写到 `/cost` 与
    // `wyj-code usage`（`prompt_cache_state`）。在拿到真实
    // `cache_read_input_tokens` 之前，不要把这里当成已验证结论。
    let prompt_cache = match profile.prompt_cache {
        Some(true) => Capability::new(
            PromptCacheMode::ExplicitBreakpoints,
            CapabilitySource::UserOverride,
            Confidence::Medium,
        ),
        Some(false) => Capability::new(
            PromptCacheMode::Unsupported,
            CapabilitySource::UserOverride,
            Confidence::High,
        ),
        None if profile.effective_prompt_cache() => {
            static_cap(PromptCacheMode::ExplicitBreakpoints)
        }
        None => protocol_cap(PromptCacheMode::Unsupported),
    };
    let stream_usage = profile.effective_openai_stream_options_for_model(model)
        || matches!(wire_protocol, WireProtocol::AnthropicMessages);

    ModelCapabilities {
        context_window: profile.context_window,
        max_output_tokens: profile.max_tokens,
        vision: static_cap(profile.vision),
        thinking,
        interleaved_thinking: static_cap(
            profile.interleaved_thinking && profile.thinking_budget.unwrap_or(0) > 0,
        ),
        prompt_cache,
        stream_usage: static_cap(stream_usage),
        tool_calling: static_cap(tool_calling),
        // 三个字段由 `apply_parallel_policy` 统一决定，见该函数注释
        parallel_tool_calls: static_cap(parallel_tools),
        tool_choice: static_cap(is_reference),
        strict_tool_schema: static_cap(is_reference),
        tool_result_images: static_cap(profile.is_official_anthropic_endpoint() && profile.vision),
        structured_output: static_cap(false),
        max_tools_per_turn: if parallel_tools {
            DEFAULT_PARALLEL_MAX_TOOLS
        } else {
            1
        },
        preferred_prompt_dialect: if matches!(
            vendor,
            "zhipu" | "minimax" | "moonshot" | "deepseek" | "alibaba" | "volcengine"
        ) {
            PromptDialect::Bilingual
        } else {
            PromptDialect::ConciseEnglish
        },
        quirks: if parallel_tools {
            Vec::new()
        } else {
            vec![ModelQuirk::RequiresSingleTool]
        },
    }
}

/// reference 端点允许的并行工具数，与 v1.5 之前的硬编码值一致。
pub const DEFAULT_PARALLEL_MAX_TOOLS: usize = 8;

/// `parallel_tool_calls` / `max_tools_per_turn` / `RequiresSingleTool` quirk
/// 三者的**唯一写入点**。
///
/// 这三个字段表达的是同一个事实——「这个端点能不能一次发多个 tool_use」——但
/// 历史上分散在 `base_capabilities`（按 vendor 静态猜测）与 CLI live probe
/// （只写 `parallel_tool_calls`）两处各算一半，导致两种不一致的失效：
///
/// 1. 只写 `parallel_tool_calls` 不写 `max_tools_per_turn`：`prompt_policy`
///    仍会往 system prompt 注入「每次回复最多调用一个工具」，模型照样不批量。
/// 2. 只写 `max_tools_per_turn` 不写 `parallel_tool_calls`：`agent.rs` 的
///    `model_allows_parallel` 仍为 false，即使模型发了多个也退化成串行执行。
///
/// 任何改动这三个字段的代码路径都必须经过这里（含 CLI live probe）。
pub fn apply_parallel_policy(
    caps: &mut ModelCapabilities,
    parallel: bool,
    max_per_turn: usize,
    source: CapabilitySource,
    confidence: Confidence,
) {
    caps.parallel_tool_calls = Capability::new(parallel, source, confidence);
    caps.max_tools_per_turn = max_per_turn.max(1);
    caps.quirks
        .retain(|q| !matches!(q, ModelQuirk::RequiresSingleTool));
    if !parallel {
        caps.quirks.push(ModelQuirk::RequiresSingleTool);
    }
}

/// 用户在 profile 里显式写了 `max_tools_per_turn` 时覆盖静态目录与探测结果。
/// `Some(1)` 同样被视为显式意图（显式退回串行），而不是「没设置」。
fn apply_user_parallel_override(profile: &Profile, caps: &mut ModelCapabilities) {
    let Some(requested) = profile.effective_max_tools_per_turn() else {
        return;
    };
    let parallel = requested > 1;
    apply_parallel_policy(
        caps,
        parallel,
        if parallel { requested } else { 1 },
        CapabilitySource::UserOverride,
        Confidence::High,
    );
}

/// 把 live probe 的结果反映到 capabilities。命中并应用时返回 true。
///
/// 整体替换 `record.capabilities` 是安全的：`run_model_probe` 以静态目录能力为基底
/// 逐字段覆写实测项，落盘记录本身就是「静态基底 + 探测结论」，不存在把未探测项
/// 降级的问题——只搬 `parallel_tool_calls` 反而会丢掉 `strict_tool_schema` /
/// `tool_calling` / `thinking` 等其它实测结论。
///
/// 替换后立刻用 `apply_parallel_policy` 归一化三字段：旧版本写出的记录可能只更新了
/// `parallel_tool_calls` 而漏了 `max_tools_per_turn` / quirk，直接采用会让三者不一致。
fn apply_probed_capabilities(
    cache: &CapabilityCache,
    identity: &ModelIdentity,
    caps: &mut ModelCapabilities,
) -> bool {
    let Ok(Some(record)) = cache.load(identity) else {
        return false;
    };
    let probed = record.capabilities;
    let parallel = probed.parallel_tool_calls.value;
    *caps = probed;
    apply_parallel_policy(
        caps,
        parallel,
        if parallel {
            DEFAULT_PARALLEL_MAX_TOOLS
        } else {
            1
        },
        CapabilitySource::LiveProbe,
        Confidence::Verified,
    );
    true
}

fn static_cap<T>(value: T) -> Capability<T> {
    Capability::new(value, CapabilitySource::StaticCatalog, Confidence::Medium)
}

fn protocol_cap<T>(value: T) -> Capability<T> {
    Capability::new(value, CapabilitySource::ProtocolDefault, Confidence::Medium)
}

fn catalog_metadata(
    profile: &Profile,
    vendor: &str,
) -> (String, VerificationStatus, Vec<String>, Option<String>) {
    let official = match vendor {
        "anthropic" => Some("https://docs.anthropic.com/"),
        "openai" => Some("https://platform.openai.com/docs/"),
        "zhipu" => Some("https://docs.bigmodel.cn/"),
        "minimax" => Some("https://platform.minimaxi.com/document/"),
        "moonshot" => Some("https://platform.moonshot.cn/docs/"),
        "deepseek" => Some("https://api-docs.deepseek.com/"),
        "alibaba" => Some("https://help.aliyun.com/zh/model-studio/"),
        "volcengine" => Some("https://www.volcengine.com/docs/82379"),
        "ollama" => Some("https://docs.ollama.com/api/openai-compatibility"),
        "vllm" => Some("https://docs.vllm.ai/en/latest/serving/openai_compatible_server.html"),
        _ => None,
    }
    .map(str::to_string);
    let endpoint_type = if matches!(vendor, "ollama" | "vllm") {
        "local_compatible"
    } else if matches!(vendor, "anthropic" | "openai")
        && (profile.base_url.trim().is_empty()
            || profile.base_url.contains("api.anthropic.com")
            || profile.base_url.contains("api.openai.com"))
    {
        "official"
    } else if official.is_some() {
        "vendor_compatible"
    } else {
        "custom"
    };
    let verification = match vendor {
        "anthropic" | "openai" => VerificationStatus::Reference,
        "ollama" | "vllm" => VerificationStatus::Experimental,
        "custom" => VerificationStatus::CustomUnverified,
        _ => VerificationStatus::StaticOnly,
    };
    let degradations = match vendor {
        "anthropic" | "openai" => Vec::new(),
        "ollama" | "vllm" => vec![
            "Capabilities vary by served model and server launch flags; run a live probe"
                .to_string(),
        ],
        "custom" => vec![
            "No catalog match; unsupported parameters and tool behavior are conservative"
                .to_string(),
        ],
        _ => vec![
            "Static catalog only; parallel tools and strict schema remain disabled until probed \
             (`wyj-code model doctor --probe full`), or set `max_tools_per_turn` in the profile \
             to override the static guess"
                .to_string(),
        ],
    };
    (
        endpoint_type.to_string(),
        verification,
        degradations,
        official,
    )
}

fn resolved_base_url(profile: &Profile) -> String {
    if !profile.base_url.trim().is_empty() {
        return profile.base_url.trim_end_matches('/').to_string();
    }
    match profile.provider {
        Provider::Anthropic => "https://api.anthropic.com".to_string(),
        Provider::OpenAI => "https://api.openai.com/v1".to_string(),
    }
}

fn infer_vendor(profile: &Profile, model: &str) -> String {
    let base_url = profile.base_url.to_ascii_lowercase();
    if base_url.contains("127.0.0.1:11434") || base_url.contains("localhost:11434") {
        return "ollama".to_string();
    }
    if base_url.contains("vllm") {
        return "vllm".to_string();
    }
    let haystack = format!("{} {}", base_url, model.to_ascii_lowercase());
    for (needle, vendor) in [
        ("minimax", "minimax"),
        ("bigmodel", "zhipu"),
        ("z.ai", "zhipu"),
        ("glm", "zhipu"),
        ("moonshot", "moonshot"),
        ("kimi", "moonshot"),
        ("deepseek", "deepseek"),
        ("dashscope", "alibaba"),
        ("aliyun", "alibaba"),
        ("qwen", "alibaba"),
        ("volces", "volcengine"),
        ("doubao", "volcengine"),
    ] {
        if haystack.contains(needle) {
            return vendor.to_string();
        }
    }
    if profile.is_official_anthropic_endpoint() {
        "anthropic".to_string()
    } else if profile.provider == Provider::OpenAI
        && (profile.base_url.trim().is_empty() || profile.base_url.contains("api.openai.com"))
    {
        "openai".to_string()
    } else {
        "custom".to_string()
    }
}

fn normalize_vendor(vendor: &str) -> String {
    match vendor.trim().to_ascii_lowercase().as_str() {
        "glm" | "zhipu" | "zai" | "z.ai" => "zhipu".to_string(),
        "kimi" | "moonshot" => "moonshot".to_string(),
        "qwen" | "bailian" | "alibaba" => "alibaba".to_string(),
        "doubao" | "volcengine" | "ark" => "volcengine".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_all_required_model_families_without_claiming_live_verification() {
        for (base_url, model, vendor, status, expected_thinking) in [
            (
                "https://open.bigmodel.cn/api/anthropic",
                "glm-5.2",
                "zhipu",
                VerificationStatus::StaticOnly,
                ThinkingMode::Effort,
            ),
            (
                "https://api.minimaxi.com/v1",
                "MiniMax-M2",
                "minimax",
                VerificationStatus::StaticOnly,
                ThinkingMode::Effort,
            ),
            (
                "https://api.moonshot.cn/anthropic",
                "kimi-k2",
                "moonshot",
                VerificationStatus::StaticOnly,
                ThinkingMode::Effort,
            ),
            (
                "https://api.deepseek.com",
                "deepseek-chat",
                "deepseek",
                VerificationStatus::StaticOnly,
                ThinkingMode::Effort,
            ),
            (
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "qwen3-coder-plus",
                "alibaba",
                VerificationStatus::StaticOnly,
                ThinkingMode::BudgetTokens,
            ),
            (
                "https://ark.cn-beijing.volces.com/api/v3",
                "doubao-seed-code",
                "volcengine",
                VerificationStatus::StaticOnly,
                ThinkingMode::BudgetTokens,
            ),
            (
                "http://127.0.0.1:11434/v1",
                "qwen3-coder",
                "ollama",
                VerificationStatus::Experimental,
                ThinkingMode::Unsupported,
            ),
        ] {
            let profile = Profile {
                provider: Provider::OpenAI,
                base_url: base_url.to_string(),
                model: model.to_string(),
                ..Profile::default()
            };
            let resolution = ModelCatalog::resolve(&profile, None);
            assert_eq!(resolution.identity.vendor, vendor);
            assert_eq!(resolution.verification_status, status);
            assert_eq!(
                resolution.capabilities.thinking.value, expected_thinking,
                "vendor={vendor} model={model} expected {expected_thinking:?}, got {:?}",
                resolution.capabilities.thinking.value
            );
        }
    }

    /// 阶段 3：能力目录按 vendor 准确标注 thinking 形态。
    /// 核心契约：catalog 阶段 ThinkingSpec 是唯一真值，不再由 profile.thinking_budget
    /// 一刀切决定。profile 没有 budget 时 capabilities.thinking 仍按 vendor 表达，
    /// 这样 RequestPlan.from_capabilities 能正确翻译 ReasoningRequest。
    #[test]
    fn thinking_capability_follows_vendor_spec_not_profile_budget() {
        // Anthropic 官方端点 → BudgetTokens
        let profile = Profile {
            provider: Provider::Anthropic,
            vendor: Some("anthropic".to_string()),
            model: "claude-opus-4-8".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            ..Profile::default()
        };
        let res = ModelCatalog::resolve(&profile, None);
        assert_eq!(res.capabilities.thinking.value, ThinkingMode::BudgetTokens);

        // Qwen SwitchPlusBudget + budget_tokens_supported=true → BudgetTokens
        let profile = Profile {
            provider: Provider::OpenAI,
            vendor: Some("alibaba".to_string()),
            model: "qwen3-coder-plus".to_string(),
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
            ..Profile::default()
        };
        let res = ModelCatalog::resolve(&profile, None);
        assert_eq!(res.capabilities.thinking.value, ThinkingMode::BudgetTokens);

        // DeepSeek SwitchPlusEffort → Effort
        let profile = Profile {
            provider: Provider::OpenAI,
            vendor: Some("deepseek".to_string()),
            model: "deepseek-chat".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            ..Profile::default()
        };
        let res = ModelCatalog::resolve(&profile, None);
        assert_eq!(res.capabilities.thinking.value, ThinkingMode::Effort);

        // GLM SwitchOnly → Effort
        let profile = Profile {
            provider: Provider::OpenAI,
            vendor: Some("zhipu".to_string()),
            model: "glm-4.6".to_string(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            ..Profile::default()
        };
        let res = ModelCatalog::resolve(&profile, None);
        assert_eq!(res.capabilities.thinking.value, ThinkingMode::Effort);

        // Ollama 非 think 模型 → Unsupported
        let profile = Profile {
            provider: Provider::OpenAI,
            vendor: Some("ollama".to_string()),
            model: "llama3".to_string(),
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            ..Profile::default()
        };
        let res = ModelCatalog::resolve(&profile, None);
        assert_eq!(res.capabilities.thinking.value, ThinkingMode::Unsupported);

        // MiniMax M3 SwitchOnly → Effort
        let profile = Profile {
            provider: Provider::OpenAI,
            vendor: Some("minimax".to_string()),
            model: "MiniMax-M3".to_string(),
            base_url: "https://api.minimaxi.com/v1".to_string(),
            ..Profile::default()
        };
        let res = ModelCatalog::resolve(&profile, None);
        assert_eq!(res.capabilities.thinking.value, ThinkingMode::Effort);
    }

    #[test]
    fn explicit_vendor_and_wire_protocol_win_over_inference() {
        let profile = Profile {
            provider: Provider::OpenAI,
            vendor: Some("minimax".to_string()),
            wire_protocol: Some(WireProtocol::AnthropicMessages),
            base_url: "https://proxy.invalid".to_string(),
            ..Profile::default()
        };
        let resolution = ModelCatalog::resolve(&profile, None);
        assert_eq!(resolution.identity.vendor, "minimax");
        assert_eq!(
            resolution.identity.wire_protocol,
            WireProtocol::AnthropicMessages
        );
    }
}

/// 并发工具能力的三字段同步不变式（`apply_parallel_policy` 的回归钉子）。
///
/// 这三个字段表达同一个事实，任何只改其中之一的路径都会让提速静默失效：
/// 只改 `parallel_tool_calls` → prompt 里仍禁批量；只改 `max_tools_per_turn`
/// → 模型发了多个也退化成串行执行。
#[cfg(test)]
mod parallel_policy_tests {
    use super::*;
    use crate::capability_cache::CapabilityCache;
    use std::path::PathBuf;

    fn minimax_profile() -> Profile {
        Profile {
            provider: Provider::Anthropic,
            vendor: Some("minimax".to_string()),
            wire_protocol: Some(WireProtocol::AnthropicMessages),
            model: "MiniMax-M3.1-Flash-Preview".to_string(),
            base_url: "https://api.minimaxi.com/anthropic".to_string(),
            ..Profile::default()
        }
    }

    fn tmp_cache(tag: &str) -> (PathBuf, CapabilityCache) {
        let dir = std::env::temp_dir().join(format!("wyj-mtpt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.clone(), CapabilityCache::new(&dir))
    }

    fn assert_single_tool(caps: &ModelCapabilities) {
        assert!(!caps.parallel_tool_calls.value);
        assert_eq!(caps.max_tools_per_turn, 1);
        assert!(caps
            .quirks
            .iter()
            .any(|q| matches!(q, ModelQuirk::RequiresSingleTool)));
    }

    fn assert_parallel(caps: &ModelCapabilities, max: usize) {
        assert!(caps.parallel_tool_calls.value);
        assert_eq!(caps.max_tools_per_turn, max);
        assert!(
            !caps
                .quirks
                .iter()
                .any(|q| matches!(q, ModelQuirk::RequiresSingleTool)),
            "放开并发后不得残留 RequiresSingleTool，否则 prompt_policy 仍会禁批量"
        );
    }

    /// 能力视图必须与实际发请求的行为一致：用户在 profile 里显式开了
    /// `prompt_cache = true`，请求就真的带 cache_control，此时 `model doctor`
    /// 再显示 "unsupported" 是自相矛盾，且会让人误判"缓存没生效"。
    #[test]
    fn explicit_prompt_cache_opt_in_is_reflected_in_capabilities() {
        let profile = Profile {
            prompt_cache: Some(true),
            ..minimax_profile()
        };
        let caps = ModelCatalog::resolve(&profile, None).capabilities;
        assert_eq!(
            caps.prompt_cache.value,
            PromptCacheMode::ExplicitBreakpoints,
            "显式开启后能力视图不得仍显示 Unsupported"
        );
        assert_eq!(caps.prompt_cache.source, CapabilitySource::UserOverride);
    }

    #[test]
    fn explicit_prompt_cache_opt_out_stays_unsupported() {
        let profile = Profile {
            prompt_cache: Some(false),
            ..minimax_profile()
        };
        let caps = ModelCatalog::resolve(&profile, None).capabilities;
        assert_eq!(caps.prompt_cache.value, PromptCacheMode::Unsupported);
        assert_eq!(caps.prompt_cache.source, CapabilitySource::UserOverride);
    }

    /// 未显式配置时默认行为**完全不变**：第三方端点仍保守视为不支持缓存。
    #[test]
    fn third_party_without_explicit_opt_in_remains_unsupported() {
        let caps = ModelCatalog::resolve(&minimax_profile(), None).capabilities;
        assert_eq!(caps.prompt_cache.value, PromptCacheMode::Unsupported);
        assert_eq!(caps.prompt_cache.source, CapabilitySource::ProtocolDefault);
    }

    /// 静态目录对第三方端点的判定保持不变——本次改动不得偷偷改变任何人的默认行为。
    #[test]
    fn static_catalog_still_locks_third_party_endpoints_to_single_tool() {
        let caps = ModelCatalog::resolve(&minimax_profile(), None).capabilities;
        assert_single_tool(&caps);
        assert_eq!(
            caps.parallel_tool_calls.source,
            CapabilitySource::StaticCatalog
        );
    }

    /// 用户显式写 `max_tools_per_turn = 4` 时三个字段必须同步放开。
    #[test]
    fn profile_override_flips_all_three_fields_together() {
        let profile = Profile {
            max_tools_per_turn: Some(4),
            ..minimax_profile()
        };
        let caps = ModelCatalog::resolve(&profile, None).capabilities;
        assert_parallel(&caps, 4);
        assert_eq!(
            caps.parallel_tool_calls.source,
            CapabilitySource::UserOverride
        );
    }

    /// `Some(1)` 是显式退回串行，不是「没设置」——reference 端点也要被拉回单工具。
    #[test]
    fn explicit_one_overrides_even_a_reference_endpoint() {
        let profile = Profile {
            provider: Provider::Anthropic,
            vendor: Some("anthropic".to_string()),
            model: "claude-opus-4-8".to_string(),
            base_url: String::new(),
            max_tools_per_turn: Some(1),
            ..Profile::default()
        };
        let caps = ModelCatalog::resolve(&profile, None).capabilities;
        assert_single_tool(&caps);
        assert_eq!(
            caps.parallel_tool_calls.source,
            CapabilitySource::UserOverride
        );
    }

    /// probe 结论必须真正回到运行时：写进 cache 的 `parallel_tool_calls`
    /// 曾经只写不读，`resolve` 传 cache=None 导致它形同虚设。
    #[test]
    fn live_probe_result_actually_reaches_runtime() {
        let (dir, cache) = tmp_cache("probe-parallel");
        let profile = minimax_profile();
        let identity = ModelCatalog::resolve(&profile, None).identity;

        let mut probed = ModelCatalog::resolve(&profile, None).capabilities;
        apply_parallel_policy(
            &mut probed,
            true,
            DEFAULT_PARALLEL_MAX_TOOLS,
            CapabilitySource::LiveProbe,
            Confidence::Verified,
        );
        cache.store(identity.clone(), probed).unwrap();

        let res = ModelCatalog::resolve_with_cache(&profile, None, Some(&cache));
        assert_parallel(&res.capabilities, DEFAULT_PARALLEL_MAX_TOOLS);
        assert_eq!(
            res.capabilities.parallel_tool_calls.source,
            CapabilitySource::LiveProbe
        );
        assert_eq!(res.verification_status, VerificationStatus::LiveVerified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// probe 判定不支持并发时，也不能把静态目录里更强的判断抬上去。
    #[test]
    fn live_probe_reporting_no_parallel_stays_single_tool() {
        let (dir, cache) = tmp_cache("probe-serial");
        let profile = minimax_profile();
        let identity = ModelCatalog::resolve(&profile, None).identity;
        let mut probed = ModelCatalog::resolve(&profile, None).capabilities;
        apply_parallel_policy(
            &mut probed,
            false,
            1,
            CapabilitySource::LiveProbe,
            Confidence::Verified,
        );
        cache.store(identity.clone(), probed).unwrap();

        let res = ModelCatalog::resolve_with_cache(&profile, None, Some(&cache));
        assert_single_tool(&res.capabilities);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 优先级：用户配置 > live probe > 静态目录。
    #[test]
    fn explicit_profile_config_outranks_live_probe() {
        let (dir, cache) = tmp_cache("precedence");
        let identity = ModelCatalog::resolve(&minimax_profile(), None).identity;
        let mut probed = ModelCatalog::resolve(&minimax_profile(), None).capabilities;
        apply_parallel_policy(
            &mut probed,
            true,
            DEFAULT_PARALLEL_MAX_TOOLS,
            CapabilitySource::LiveProbe,
            Confidence::Verified,
        );
        cache.store(identity.clone(), probed).unwrap();

        let profile = Profile {
            max_tools_per_turn: Some(1),
            ..minimax_profile()
        };
        let res = ModelCatalog::resolve_with_cache(&profile, None, Some(&cache));
        assert_single_tool(&res.capabilities);
        assert_eq!(
            res.capabilities.parallel_tool_calls.source,
            CapabilitySource::UserOverride
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端点真的拒绝过参数（`record_rejection`，会写一条 cache 记录）时，
    /// thinking 必须保持 `Unsupported/UserOverride`——即使同一条记录还带着探针快照。
    /// 这条钉的是 `apply_probed_capabilities` 与 `apply_rejected_parameters` 的顺序：
    /// 探针快照是整体替换 capabilities 的，若排在拒绝记录之后，会把刚设好的降级冲掉。
    /// 特别注意 `record_rejection` 自己也写 cache 记录——「有记录」不等于「做过探针」。
    #[test]
    fn runtime_rejection_outranks_the_stored_probe_snapshot() {
        let (dir, cache) = tmp_cache("reject-order");
        let profile = minimax_profile();
        let identity = ModelCatalog::resolve(&profile, None).identity;

        // 先存一份"探针通过并发"的快照
        let mut probed = ModelCatalog::resolve(&profile, None).capabilities;
        apply_parallel_policy(
            &mut probed,
            true,
            DEFAULT_PARALLEL_MAX_TOOLS,
            CapabilitySource::LiveProbe,
            Confidence::Verified,
        );
        cache.store(identity.clone(), probed).unwrap();
        // 再记录端点拒绝了 thinking 参数
        cache.record_rejection(&identity, "thinking_budget", "400");

        let res = ModelCatalog::resolve_with_cache(&profile, None, Some(&cache));
        assert_eq!(
            res.capabilities.thinking.value,
            ThinkingMode::Unsupported,
            "端点拒绝记录必须压过探针快照"
        );
        assert_eq!(
            res.capabilities.thinking.source,
            CapabilitySource::UserOverride
        );
        // 并发结论仍应保留（拒绝记录只针对 thinking 字段）
        assert_parallel(&res.capabilities, DEFAULT_PARALLEL_MAX_TOOLS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等：反复 resolve 不得让 quirks 里堆出多个 RequiresSingleTool。
    #[test]
    fn repeated_resolution_does_not_duplicate_the_single_tool_quirk() {
        let profile = minimax_profile();
        let first = ModelCatalog::resolve(&profile, None).capabilities;
        let second = ModelCatalog::resolve(&profile, None).capabilities;
        let count = |c: &ModelCapabilities| {
            c.quirks
                .iter()
                .filter(|q| matches!(q, ModelQuirk::RequiresSingleTool))
                .count()
        };
        assert_eq!(count(&first), count(&second));

        // 放开并发后再放开一次，quirk 不得残留
        let mut caps = first;
        for _ in 0..3 {
            apply_parallel_policy(
                &mut caps,
                true,
                4,
                CapabilitySource::UserOverride,
                Confidence::High,
            );
        }
        assert_eq!(count(&caps), 0);
        assert_eq!(caps.max_tools_per_turn, 4);
    }
}
