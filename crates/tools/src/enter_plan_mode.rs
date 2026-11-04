//! EnterPlanMode：Auto/Bypass 下由模型主动请求进入 Plan，先对齐方案再改代码。

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

use crate::descriptions;

/// Auto 模式下「何时该先规划」的积极性（对齐 Claude Code external / ant 两套提示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanSensitivity {
    /// 仅在架构歧义、需求不清、高风险或多方案时进入（默认）。
    #[default]
    Conservative,
    /// 非平凡实现任务优先进入。
    Proactive,
}

impl PlanSensitivity {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "proactive" | "aggressive" | "external" => Self::Proactive,
            _ => Self::Conservative,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Proactive => "proactive",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Conservative => descriptions::ENTER_PLAN_MODE_CONSERVATIVE,
            Self::Proactive => descriptions::ENTER_PLAN_MODE_PROACTIVE,
        }
    }
}

pub struct EnterPlanModeTool {
    sensitivity: PlanSensitivity,
}

impl EnterPlanModeTool {
    pub fn new(sensitivity: PlanSensitivity) -> Self {
        Self { sensitivity }
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "EnterPlanMode".to_string(),
            description: self.sensitivity.description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "One short sentence explaining why planning first is needed; shown to the user in the approval dialog."
                    }
                },
                "required": []
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        if ctx.is_plan_mode() {
            return Ok(ToolResult::ok(
                "Already in plan mode. Continue exploring with read-only tools, then call ExitPlanMode with the complete Markdown plan when ready.",
            ));
        }
        if !ctx.supports_interactive_confirmation() {
            return Ok(ToolResult::err(
                "EnterPlanMode requires an interactive approval channel. Stay in the current mode: make reasonable assumptions and proceed, or ask the user to switch to plan mode with Shift+Tab / /plan.",
            ));
        }
        let reason = input["reason"].as_str().unwrap_or("").trim();
        let approved = ctx.enter_plan_mode(reason).await;
        if approved {
            Ok(ToolResult::ok(wyj_core::prompts::ENTERED_PLAN_MODE))
        } else {
            Ok(ToolResult::ok(
                "User declined to enter plan mode. Stay in the current execution mode and proceed with the task; do not call EnterPlanMode again for the same request unless the user asks to plan first.",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_sensitivity_parse_aliases() {
        assert_eq!(
            PlanSensitivity::parse("conservative"),
            PlanSensitivity::Conservative
        );
        assert_eq!(
            PlanSensitivity::parse("proactive"),
            PlanSensitivity::Proactive
        );
        assert_eq!(
            PlanSensitivity::parse("aggressive"),
            PlanSensitivity::Proactive
        );
        assert_eq!(PlanSensitivity::parse(""), PlanSensitivity::Conservative);
    }
}
