//! ExitPlanMode 工具：LLM 在 plan 模式下完成规划后调用，携带完整计划文本向用户请求批准

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ExitPlanMode".to_string(),
            description: crate::descriptions::EXIT_PLAN_MODE.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The complete implementation plan (Markdown), shown to the user verbatim for approval. Write it in the user's language."
                    }
                },
                "required": ["plan"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let plan = input["plan"].as_str().unwrap_or_default();
        let approved = ctx.exit_plan_mode(plan).await;
        if approved {
            Ok(ToolResult::ok(
                "User approved the plan. Execution mode is now enabled — proceed with the implementation.",
            ))
        } else {
            Ok(ToolResult::ok(
                "User chose to keep planning. Stay in plan mode and refine the plan.",
            ))
        }
    }
}
