//! ExitPlanMode 工具：LLM 在 plan 模式下完成规划后调用，触发用户批准对话框

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
            description: "计划分析完毕后调用此工具，向用户提交计划并请求批准。\
                用户批准后将自动切换至执行模式。\
                请在调用前确保已将完整计划写入计划文件。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan_path": {
                        "type": "string",
                        "description": "已保存计划文档的文件路径（可选）"
                    }
                },
                "required": []
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let plan_path = input["plan_path"].as_str();
        let approved = ctx.exit_plan_mode(plan_path).await;
        if approved {
            Ok(ToolResult::ok(
                "用户已批准计划，正在切换至执行模式。接下来可以开始实施。",
            ))
        } else {
            Ok(ToolResult::ok("用户选择继续规划，保持 plan 模式。"))
        }
    }
}
