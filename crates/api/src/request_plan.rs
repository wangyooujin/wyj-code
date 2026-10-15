//! 经能力解析后的供应商请求计划。

use serde::{Deserialize, Serialize};

use crate::capabilities::ModelIdentity;
use crate::thinking::{normalize_effort, ThinkingSpec};
use crate::{ModelCapabilities, ModelCatalog, PromptCacheMode, ThinkingMode};
use wyj_config::Profile;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningRequest {
    Disabled,
    BudgetTokens(u32),
    Effort(String),
    ProviderNative,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequestPolicy {
    pub enabled: bool,
    pub allow_parallel: bool,
    pub max_tools_per_turn: usize,
    pub force_single_tool: bool,
    pub simplify_schema: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    Disabled,
    Automatic,
    ExplicitBreakpoints,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedParameter {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPlan {
    pub model: ModelIdentity,
    pub max_tokens: u32,
    pub reasoning: ReasoningRequest,
    pub tool_policy: ToolRequestPolicy,
    pub cache_policy: CachePolicy,
    pub stream_usage: bool,
    pub vision: bool,
    pub dropped_parameters: Vec<DroppedParameter>,
}

impl RequestPlan {
    pub fn from_profile(profile: &Profile, model_override: Option<&str>) -> Self {
        let resolution = ModelCatalog::resolve(profile, model_override);
        Self::from_capabilities(resolution.identity, &resolution.capabilities, profile)
    }

    pub fn from_capabilities(
        model: ModelIdentity,
        capabilities: &ModelCapabilities,
        profile: &Profile,
    ) -> Self {
        let mut dropped_parameters = Vec::new();
        // 能力快照可能已经把整个 thinking 关掉（用户 override，或上一次端点 4xx 被
        // `CapabilityCache` 记成 rejected）。这时的优先级高于静态 spec——端点已经
        // 亲口说过不认，不能再发一次试错。
        let blocked = matches!(
            capabilities.thinking.value,
            ThinkingMode::Unsupported | ThinkingMode::ProviderNative
        );
        let spec = ThinkingSpec::for_vendor(&model.vendor, &model.wire_protocol, &model.model);
        // 优先级：profile.thinking_budget > profile.effective_reasoning_effort > Disabled
        // （OpenAI-vendor 的 effort 与 Anthropic 协议的 budget 是两种 reasoning）
        let budget = profile.thinking_budget.filter(|budget| *budget > 0);
        let effort = profile.effective_reasoning_effort();
        let effort_usable = !blocked && !spec.effort_levels.is_empty();
        // budget 不被支持时**不能顺手把 effort 一起废掉**：旧 config 里两个都配很常见
        // （DeepSeek、MiniMax `/anthropic` 都是「不支持 budget、认 effort」的组合）。
        // 早先的 match 臂会在 budget 上短路成 `Disabled`，effort 连同被丢——用户只看到
        // 一条 thinking_budget 的 dropped，reasoning 实际已整条关闭。
        let reasoning = match budget.filter(|_| spec.budget_tokens_supported && !blocked) {
            Some(budget) => ReasoningRequest::BudgetTokens(budget),
            None => {
                if budget.is_some() {
                    dropped_parameters.push(DroppedParameter {
                        name: "thinking_budget".to_string(),
                        reason: "model capability does not support token-budget reasoning"
                            .to_string(),
                    });
                }
                // `ThinkingMode` 是单值标签，记不下 Qwen 那种 budget 与 effort 都认的
                // vendor；这里直接问 spec 的 `effort_levels`，顺带把"档位写错"
                // （如给 DeepSeek 填 xhigh）也变成可见的 dropped 而不是 400。
                match (effort.as_deref(), effort_usable) {
                    (None, _) => ReasoningRequest::Disabled,
                    (Some(_), false) => {
                        dropped_parameters.push(DroppedParameter {
                            name: "reasoning_effort".to_string(),
                            reason: "model capability does not support effort-based reasoning"
                                .to_string(),
                        });
                        ReasoningRequest::Disabled
                    }
                    (Some(raw), true) => match normalize_effort(&model, raw) {
                        Some(level) => ReasoningRequest::Effort(level.to_string()),
                        None => {
                            dropped_parameters.push(DroppedParameter {
                                name: "reasoning_effort".to_string(),
                                reason: format!(
                                    "`{raw}` is not a supported level for this model (expected one of: {})",
                                    spec.effort_levels
                                        .iter()
                                        .map(|level| level.as_str())
                                        .collect::<Vec<_>>()
                                        .join("/")
                                ),
                            });
                            ReasoningRequest::Disabled
                        }
                    },
                }
            }
        };
        let cache_policy = match capabilities.prompt_cache.value {
            PromptCacheMode::Unsupported => CachePolicy::Disabled,
            PromptCacheMode::Automatic => CachePolicy::Automatic,
            PromptCacheMode::ExplicitBreakpoints => CachePolicy::ExplicitBreakpoints,
        };
        Self {
            model,
            max_tokens: profile.max_tokens.min(capabilities.max_output_tokens),
            reasoning,
            tool_policy: ToolRequestPolicy {
                enabled: capabilities.tool_calling.value,
                allow_parallel: capabilities.parallel_tool_calls.value,
                max_tools_per_turn: capabilities.max_tools_per_turn.max(1),
                force_single_tool: !capabilities.parallel_tool_calls.value,
                simplify_schema: !capabilities.strict_tool_schema.value,
            },
            cache_policy,
            stream_usage: capabilities.stream_usage.value,
            vision: capabilities.vision.value && profile.vision,
            dropped_parameters,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenAI 协议 + 指定 vendor —— `Profile::default()` 是 Anthropic 协议，
    /// 那样所有 vendor 都会落到 `anthropic_compatible`，测不到 effort 分支。
    fn profile_for(vendor: &str, model: &str, effort: &str) -> Profile {
        Profile {
            provider: wyj_config::Provider::OpenAI,
            vendor: Some(vendor.to_string()),
            model: model.to_string(),
            reasoning_effort: Some(effort.to_string()),
            ..Profile::default()
        }
    }

    /// Qwen 的 `ThinkingMode` 标签是 `BudgetTokens`（单值标签记不下双认），
    /// 曾经因此把 effort 当成"不支持"丢掉——adapter 里那段 effort 分支是死代码。
    #[test]
    fn qwen_effort_is_accepted_even_though_capability_says_budget() {
        let plan =
            RequestPlan::from_profile(&profile_for("alibaba", "qwen3-coder-plus", "high"), None);
        assert_eq!(plan.reasoning, ReasoningRequest::Effort("high".to_string()));
        assert!(
            plan.dropped_parameters.is_empty(),
            "{:?}",
            plan.dropped_parameters
        );
    }

    /// 两者都配时 budget 优先（Qwen 协议互斥），且不产生 dropped。
    #[test]
    fn budget_wins_over_effort_when_both_configured() {
        let mut profile = profile_for("alibaba", "qwen3-coder-plus", "high");
        profile.thinking_budget = Some(4096);
        let plan = RequestPlan::from_profile(&profile, None);
        assert_eq!(plan.reasoning, ReasoningRequest::BudgetTokens(4096));
        assert!(plan.dropped_parameters.is_empty());
    }

    /// 档位写错（DeepSeek 没有 xhigh）必须是可见的 dropped，而不是发出去换 400。
    #[test]
    fn effort_level_outside_vendor_list_is_dropped_visibly() {
        let plan =
            RequestPlan::from_profile(&profile_for("deepseek", "deepseek-v4-pro", "xhigh"), None);
        assert_eq!(plan.reasoning, ReasoningRequest::Disabled);
        let dropped = plan
            .dropped_parameters
            .iter()
            .find(|parameter| parameter.name == "reasoning_effort")
            .expect("应记录 reasoning_effort 被丢弃");
        assert!(dropped.reason.contains("xhigh"), "{}", dropped.reason);
        assert!(dropped.reason.contains("high"), "{}", dropped.reason);
    }

    /// GLM 只认 `thinking.type` 开关；catalog 标成 `Effort` 只是单值标签的副作用，
    /// 不能据此把档位放行。
    #[test]
    fn switch_only_vendor_drops_effort() {
        let plan = RequestPlan::from_profile(&profile_for("zhipu", "glm-4.6", "high"), None);
        assert_eq!(plan.reasoning, ReasoningRequest::Disabled);
        assert!(plan
            .dropped_parameters
            .iter()
            .any(|parameter| parameter.name == "reasoning_effort"));
    }

    /// 能力缓存（用户 override / 上次 4xx 记录）把 thinking 禁掉时，静态 spec 不作数。
    #[test]
    fn capability_block_overrides_static_spec() {
        let profile = profile_for("deepseek", "deepseek-v4-pro", "high");
        let resolution = ModelCatalog::resolve(&profile, None);
        let mut capabilities = resolution.capabilities.clone();
        capabilities.thinking = crate::Capability::new(
            ThinkingMode::Unsupported,
            crate::CapabilitySource::UserOverride,
            crate::Confidence::High,
        );
        let plan = RequestPlan::from_capabilities(resolution.identity, &capabilities, &profile);
        assert_eq!(plan.reasoning, ReasoningRequest::Disabled);
        assert!(plan
            .dropped_parameters
            .iter()
            .any(|parameter| parameter.name == "reasoning_effort"));
    }

    #[test]
    fn unsupported_reasoning_is_dropped_visibly() {
        let profile = Profile {
            thinking_budget: Some(1024),
            vendor: Some("custom".to_string()),
            ..Profile::default()
        };
        let mut caps = ModelCapabilities::conservative(64_000, 8_192);
        caps.tool_calling.value = true;
        let identity = ModelCatalog::resolve(&profile, None).identity;
        let plan = RequestPlan::from_capabilities(identity, &caps, &profile);
        assert_eq!(plan.reasoning, ReasoningRequest::Disabled);
        assert!(plan
            .dropped_parameters
            .iter()
            .any(|parameter| parameter.name == "thinking_budget"));
    }
}
