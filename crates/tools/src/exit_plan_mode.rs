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
            description: "计划分析完毕后调用此工具，将完整计划内容（Markdown 格式）作为 plan \
                参数提交给用户审批。用户批准后将自动切换至执行模式。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "完整的计划内容（Markdown 格式），将原样展示给用户审批"
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
                "用户已批准计划，正在切换至执行模式。接下来可以开始实施。",
            ))
        } else {
            Ok(ToolResult::ok("用户选择继续规划，保持 plan 模式。"))
        }
    }
}
