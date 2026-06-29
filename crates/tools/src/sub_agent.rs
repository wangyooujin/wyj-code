//! 子 Agent 工具 — 派生独立的嵌套 Agent 完成复杂子任务

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use wyj_api::types::ToolDefinition;
use wyj_core::{tool::{Tool, ToolContext, ToolResult}, Agent, Session};

pub struct SubAgentTool {
    /// 工厂函数：创建子 Agent（持有 provider 和工具集）
    agent_factory: Arc<dyn Fn() -> Agent + Send + Sync>,
}

impl SubAgentTool {
    pub fn new(factory: impl Fn() -> Agent + Send + Sync + 'static) -> Self {
        Self { agent_factory: Arc::new(factory) }
    }
}

#[derive(Deserialize)]
struct Input {
    prompt: String,
    #[serde(default)]
    system: Option<String>,
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "启动一个独立的子 Agent 完成指定任务。\
                子 Agent 拥有独立的上下文和工具集，完成后将结果返回。\
                适用于需要多步骤规划、并行执行或隔离上下文的复杂子任务。".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "子 Agent 的任务描述"
                    },
                    "system": {
                        "type": "string",
                        "description": "子 Agent 的系统提示（可选，覆盖默认）"
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;

        let mut agent = (self.agent_factory)();
        if let Some(sys) = inp.system {
            agent = agent.with_system(sys);
        }

        let mut session = Session::new();
        session.push_user(inp.prompt);

        let cwd = ctx.cwd().to_path_buf();
        let sub_ctx = crate::ctx::ToolCtx::new(&cwd);

        let mut output_buf = String::new();
        agent.run_turn(&mut session, &sub_ctx, &mut |d| output_buf.push_str(d)).await?;

        // 把最后一条 assistant 消息提取出来
        let result = if output_buf.is_empty() {
            "子 Agent 未产生输出".to_string()
        } else {
            output_buf
        };

        Ok(ToolResult::ok(result))
    }
}
