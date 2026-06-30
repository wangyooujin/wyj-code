//! MCP → Tool 桥接：将 MCP server 的工具暴露为 wyj_core::Tool

use crate::config::{McpServerConfig, McpTransport};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::service::RunningService;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    transport::{child_process::ConfigureCommandExt, TokioChildProcess},
    RoleClient, ServiceExt,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

type McpHandle = RunningService<RoleClient, ClientInfo>;

/// 桥接单个 MCP 工具 → wyj_core::Tool
pub struct McpBridgeTool {
    tool_name: String,
    definition: ToolDefinition,
    client: Arc<Mutex<McpHandle>>,
}

#[async_trait]
impl Tool for McpBridgeTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let args = match input.as_object() {
            Some(m) => m.clone(),
            None => serde_json::Map::new(),
        };
        let params = CallToolRequestParams::new(self.tool_name.clone()).with_arguments(args);

        let guard = self.client.lock().await;
        let result = guard
            .call_tool(params)
            .await
            .map_err(|e| anyhow::anyhow!("MCP 工具调用失败: {e}"))?;

        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.to_string()))
            .collect::<Vec<_>>()
            .join("\n");

        let is_error = result.is_error.unwrap_or(false);
        if is_error {
            Ok(ToolResult::err(text))
        } else {
            Ok(ToolResult::ok(text))
        }
    }
}

/// 连接 MCP server 并发现所有工具，返回桥接工具列表
pub async fn connect_mcp_server(cfg: &McpServerConfig) -> Result<Vec<McpBridgeTool>> {
    if cfg.transport != McpTransport::Stdio {
        anyhow::bail!("当前仅支持 stdio 传输类型");
    }
    let cmd = cfg
        .command
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("stdio 传输需要 command 字段"))?;

    let mut command = Command::new(cmd);
    for arg in &cfg.args {
        command.arg(arg);
    }
    for (k, v) in &cfg.env {
        command.env(k, v);
    }

    let transport = TokioChildProcess::new(command.configure(|_cmd| {}))
        .map_err(|e| anyhow::anyhow!("启动 MCP 子进程失败: {e}"))?;

    let client_info = ClientInfo::default();
    let client: McpHandle = client_info
        .serve(transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP 初始化失败: {e}"))?;

    let tools_result = client
        .list_all_tools()
        .await
        .map_err(|e| anyhow::anyhow!("获取工具列表失败: {e}"))?;

    let client = Arc::new(Mutex::new(client));
    let mut bridges = vec![];

    for mcp_tool in tools_result {
        let schema = serde_json::to_value(&mcp_tool.input_schema)
            .unwrap_or(Value::Object(Default::default()));
        let def = ToolDefinition {
            name: mcp_tool.name.to_string(),
            description: mcp_tool.description.as_deref().unwrap_or("").to_string(),
            input_schema: schema,
        };
        bridges.push(McpBridgeTool {
            tool_name: mcp_tool.name.to_string(),
            definition: def,
            client: client.clone(),
        });
    }

    tracing::info!(
        "MCP server {} 连接成功，发现 {} 个工具",
        cfg.name,
        bridges.len()
    );
    Ok(bridges)
}
