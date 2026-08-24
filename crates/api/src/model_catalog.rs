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

    /// 带 capability cache 的 resolve 版本。`cache` 为 Some 且命中时，把
    /// `rejected_parameters` 里出现过的 thinking 系列参数强制标 Unsupported，
    /// 避免下次 stream() 又发端点已确认不认的字段。
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
        if let Some(cache) = cache {
            apply_rejected_parameters(cache, &identity, &mut capabilities);
        }
        let (endpoint_type, verification_status, known_degradations, documentation_url) =
            catalog_metadata(profile, &vendor);

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
    let prompt_cache = if profile.effective_prompt_cache() {
        static_cap(PromptCacheMode::ExplicitBreakpoints)
    } else {
        protocol_cap(PromptCacheMode::Unsupported)
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
        parallel_tool_calls: static_cap(parallel_tools),
        tool_choice: static_cap(is_reference),
        strict_tool_schema: static_cap(is_reference),
        tool_result_images: static_cap(profile.is_official_anthropic_endpoint() && profile.vision),
        structured_output: static_cap(false),
        max_tools_per_turn: if is_reference { 8 } else { 1 },
        preferred_prompt_dialect: if matches!(
            vendor,
            "zhipu" | "minimax" | "moonshot" | "deepseek" | "alibaba" | "volcengine"
        ) {
            PromptDialect::Bilingual
        } else {
            PromptDialect::ConciseEnglish
        },
        quirks: if !parallel_tools {
            vec![ModelQuirk::RequiresSingleTool]
        } else {
            Vec::new()
        },
    }
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
            "Static catalog only; parallel tools and strict schema remain disabled until probed"
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
