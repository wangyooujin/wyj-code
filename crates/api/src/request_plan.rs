//! 经能力解析后的供应商请求计划。

use serde::{Deserialize, Serialize};

use crate::capabilities::ModelIdentity;
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
        let reasoning = match (
            profile.thinking_budget.filter(|budget| *budget > 0),
            capabilities.thinking.value,
        ) {
            (Some(budget), ThinkingMode::BudgetTokens) => ReasoningRequest::BudgetTokens(budget),
            (Some(_), _) => {
                dropped_parameters.push(DroppedParameter {
                    name: "thinking_budget".to_string(),
                    reason: "model capability does not support token-budget reasoning".to_string(),
                });
                ReasoningRequest::Disabled
            }
            _ => ReasoningRequest::Disabled,
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
