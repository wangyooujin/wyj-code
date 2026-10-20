//! 经能力解析后的供应商请求计划。

use serde::{Deserialize, Serialize};

use crate::capabilities::ModelIdentity;
use crate::thinking::{normalize_effort, ThinkingSpec};
use crate::{ModelCapabilities, ModelCatalog, ThinkingMode};
use wyj_config::Profile;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningRequest {
    Disabled,
    BudgetTokens(u32),
    Effort(String),
    /// budget 与 effort **同时**下发。
    ///
    /// 只有「两个旋钮控制不同维度」的端点才用这个变体——目前是官方 Anthropic：
    /// `thinking.budget_tokens` 限**思考**的 token 数，`output_config.effort`
    /// 限**整个响应**的投入档位，官方文档明确说两者可以一起用。
    ///
    /// 其余端点（Qwen / DeepSeek / Moonshot …）的 budget 与 effort 是互斥的，
    /// 仍走 `BudgetTokens` / `Effort` 单值变体。
    ///
    /// 旧会话文件里不会有这个变体（它是新增的），反序列化失败时按
    /// `Disabled` 兜底即可——reasoning 缺失只是少一次深度思考，不影响正确性。
    BudgetAndEffort {
        budget: u32,
        effort: String,
    },
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
        //
        // 档位先单独裁决（`normalize_effort` 查的是 spec 的 `effort_levels`）：
        // 档位越界必须变成**可见的 dropped**，而不是发出去换 400。
        let effort_level = effort.as_deref().and_then(|raw| {
            if !effort_usable {
                return None;
            }
            match normalize_effort(&model, raw) {
                Some(level) => Some(level.to_string()),
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
                    None
                }
            }
        });
        if effort.is_some() && !effort_usable {
            dropped_parameters.push(DroppedParameter {
                name: "reasoning_effort".to_string(),
                reason: "model capability does not support effort-based reasoning".to_string(),
            });
        }
        let budget_usable = budget.filter(|_| spec.budget_tokens_supported && !blocked);
        if budget.is_some() && budget_usable.is_none() {
            dropped_parameters.push(DroppedParameter {
                name: "thinking_budget".to_string(),
                reason: "model capability does not support token-budget reasoning".to_string(),
            });
        }
        // 两个旋钮控制不同维度时（官方 Anthropic）**同时**下发；互斥端点维持
        // 「budget 优先」的历史行为不变（Qwen 协议明确禁止同传）。
        let reasoning = match (budget_usable, effort_level) {
            (Some(budget), Some(effort)) if spec.budget_and_effort_coexist => {
                ReasoningRequest::BudgetAndEffort { budget, effort }
            }
            (Some(budget), _) => ReasoningRequest::BudgetTokens(budget),
            (None, Some(effort)) => ReasoningRequest::Effort(effort),
            (None, None) => ReasoningRequest::Disabled,
        };
        Self {
            model,
            max_tokens: profile.max_tokens.min(capabilities.max_output_tokens),
            reasoning,
            // 缓存策略**不**在此重复计算：唯一真相源是
            // `capabilities.prompt_cache.value`（`reuses_prompt_prefix()` 与两个
            // provider 都读它）。此前这里算了 `CachePolicy` 却零消费者，等于
            // 同一判断存在三份而只有两份生效——改一处不影响另一处。
            tool_policy: ToolRequestPolicy {
                enabled: capabilities.tool_calling.value,
                allow_parallel: capabilities.parallel_tool_calls.value,
                max_tools_per_turn: capabilities.max_tools_per_turn.max(1),
                force_single_tool: !capabilities.parallel_tool_calls.value,
                simplify_schema: !capabilities.strict_tool_schema.value,
            },
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

    /// **官方 OpenAI 线的端到端钉子。**
    ///
    /// 此前 `ThinkingSpec::for_vendor` 没有 `"openai"` 臂，落到 generic 的
    /// `ThinkingControl::Disabled`，于是 `base_capabilities` 把 thinking 标成
    /// `Unsupported`，`agent.rs` 的 `blocked` 判据在**到达 provider 之前**就把
    /// `reasoning_effort` 滤成 None——用户改配置毫无效果，只在日志里留一条
    /// dropped。这条从 profile 出发走完整链路，钉住「catalog 层不能把它吃掉」。
    #[test]
    fn official_openai_effort_survives_the_catalog_layer() {
        let plan = RequestPlan::from_profile(&profile_for("openai", "o3", "high"), None);
        assert_eq!(plan.reasoning, ReasoningRequest::Effort("high".to_string()));
        assert!(
            plan.dropped_parameters.is_empty(),
            "官方 OpenAI 支持 effort，不该有任何 dropped：{:?}",
            plan.dropped_parameters
        );
        // 能力视图也必须一致：thinking 不能仍是 Unsupported，否则 agent 层会
        // 依据它把 effort 再过一遍滤掉。
        let resolution = ModelCatalog::resolve(&profile_for("openai", "o3", "high"), None);
        assert_ne!(
            resolution.capabilities.thinking.value,
            ThinkingMode::Unsupported,
            "catalog 仍把官方 OpenAI 记成不支持 thinking"
        );
    }

    /// 官方线不认 `thinking_budget`：必须记成可见的 dropped，而不是发出去。
    #[test]
    fn official_openai_drops_thinking_budget_visibly() {
        let mut profile = profile_for("openai", "o3", "high");
        profile.thinking_budget = Some(4096);
        let plan = RequestPlan::from_profile(&profile, None);
        // budget 被丢，但 effort 必须保住（与 MiniMax `/anthropic` 同一类坑）。
        assert_eq!(plan.reasoning, ReasoningRequest::Effort("high".to_string()));
        assert!(
            plan.dropped_parameters
                .iter()
                .any(|d| d.name == "thinking_budget"),
            "{:?}",
            plan.dropped_parameters
        );
    }

    /// 档位越界（官方只收 low/medium/high）必须是可见的 dropped，不是 400。
    #[test]
    fn official_openai_drops_unsupported_effort_level() {
        let plan = RequestPlan::from_profile(&profile_for("openai", "o3", "xhigh"), None);
        assert_eq!(plan.reasoning, ReasoningRequest::Disabled);
        let dropped = plan
            .dropped_parameters
            .iter()
            .find(|d| d.name == "reasoning_effort")
            .expect("应记录 reasoning_effort 被丢弃");
        assert!(dropped.reason.contains("xhigh"), "{}", dropped.reason);
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

    /// 官方 Anthropic：budget 与 effort 控制**不同维度**，必须**同时**下发。
    ///
    /// 回归背景：旧实现里 budget 优先短路会顺手把 effort 一起废掉，于是
    /// `reasoning_effort` 在 Anthropic 线上成了死配置——用户配了 effort，
    /// 只看到一条 `thinking_budget` 的 dropped，effort 连同被丢。
    #[test]
    fn official_anthropic_sends_budget_and_effort_together() {
        let mut profile = Profile {
            provider: wyj_config::Provider::Anthropic,
            vendor: Some("anthropic".to_string()),
            model: "claude-opus-4-8".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning_effort: Some("high".to_string()),
            ..Profile::default()
        };
        profile.thinking_budget = Some(4096);
        let plan = RequestPlan::from_profile(&profile, None);
        assert_eq!(
            plan.reasoning,
            ReasoningRequest::BudgetAndEffort {
                budget: 4096,
                effort: "high".to_string()
            },
            "两个旋钮都在，effort 不能被 budget 吃掉"
        );
        assert!(
            plan.dropped_parameters.is_empty(),
            "{:?}",
            plan.dropped_parameters
        );
    }

    /// 老型号（不支持 effort）配了 effort 必须是可见的 dropped，且不牵连 budget。
    #[test]
    fn effort_on_an_unsupported_anthropic_model_is_dropped_visibly() {
        let mut profile = Profile {
            provider: wyj_config::Provider::Anthropic,
            vendor: Some("anthropic".to_string()),
            model: "claude-3-5-sonnet-20241022".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning_effort: Some("high".to_string()),
            ..Profile::default()
        };
        profile.thinking_budget = Some(4096);
        let plan = RequestPlan::from_profile(&profile, None);
        assert_eq!(plan.reasoning, ReasoningRequest::BudgetTokens(4096));
        assert!(plan
            .dropped_parameters
            .iter()
            .any(|parameter| parameter.name == "reasoning_effort"));
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
