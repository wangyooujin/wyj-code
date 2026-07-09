//! MCP → Tool 桥接：将 MCP server 的工具暴露为 wyj_core::Tool

use crate::config::{McpServerConfig, McpTransport};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::service::RunningService;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    transport::TokioChildProcess,
    RoleClient, ServiceExt,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

type McpHandle = RunningService<RoleClient, ClientInfo>;

/// 单个 MCP server 连接尝试（子进程启动+握手+发现工具）的超时上限。子进程
/// 启动（尤其 npx/uvx 首次拉包）或网络慢时可能耗时较久，调用方应始终用
/// `tokio::time::timeout` 包一层，避免某个 server 卡住/无响应无限拖慢调用方。
pub const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `-p`（单次问答）模式下等待 MCP server 连接的宽限期，远小于
/// `MCP_CONNECT_TIMEOUT`。`-p` 是真正的单轮、进程跑完即退出，没有 TUI/
/// `--headless` REPL 那种"后台连完之后还有很多轮对话可以补挂工具"的空间，
/// 但也不能照抄 15s 全量等待——本地 stdio/已缓存的 npx/uvx 包通常能在几秒内
/// 连完，只在"首次 npx/uvx 需要联网下载包"这种慢场景下才会触发宽限期截断
/// （该场景本来就是 `-p` 单轮模式无法根治的已知局限）。
pub const MCP_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

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
    command.args(&cfg.args).envs(&cfg.env);

    // MCP server 子进程默认继承父进程的 stderr——不管是 TUI 的 alternate screen
    // 还是普通终端，子进程自己的日志/警告输出都会直接写穿到用户看到的画面上。
    // 关键点：`rmcp::TokioChildProcess::new()` 内部固定用
    // `TokioChildProcessBuilder`，其 stderr 默认值是 `Stdio::inherit()`，
    // 且 `spawn()` 时会用这个默认值重新对 command 调一次 `.stderr(...)`——
    // 这会覆盖任何在 `command.configure(...)` 闭包里直接设置的 stderr，
    // 在这一层设置完全不生效。必须改用 `TokioChildProcess::builder(...)`
    // 拿到 `TokioChildProcessBuilder`，在它上面显式调用 `.stderr(...)`
    // 才是真正被 `spawn()` 采用的值。
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::null())
        .spawn()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;

    fn stdio_cfg(command: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: "test-server".to_string(),
            transport: McpTransport::Stdio,
            command: command.map(str::to_string),
            args: vec![],
            env: Default::default(),
        }
    }

    // `McpBridgeTool`（Ok 分支类型）不实现 Debug（内部持有 rmcp 的连接句柄），
    // 用不到 Debug 的错误信息提取代替 `.unwrap_err()`。
    async fn expect_err_containing(cfg: &McpServerConfig, needle: &str) {
        match connect_mcp_server(cfg).await {
            Ok(_) => panic!("expected connect_mcp_server to fail"),
            Err(e) => assert!(
                e.to_string().contains(needle),
                "error message {:?} does not contain {needle:?}",
                e.to_string()
            ),
        }
    }

    #[tokio::test]
    async fn connect_mcp_server_rejects_non_stdio_transport() {
        let cfg = McpServerConfig {
            name: "http-server".to_string(),
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: Default::default(),
        };
        expect_err_containing(&cfg, "stdio").await;
    }

    #[tokio::test]
    async fn connect_mcp_server_requires_command_for_stdio() {
        expect_err_containing(&stdio_cfg(None), "command").await;
    }

    #[tokio::test]
    async fn connect_mcp_server_fails_fast_on_unspawnable_command() {
        // 命令存在但不是可执行文件/不存在于 PATH，验证子进程启动失败会被
        // 正确包装成 Err 而不是 panic，不依赖任何真实 MCP server。
        let cfg = stdio_cfg(Some("wyj-code-definitely-not-a-real-binary-xyz"));
        expect_err_containing(&cfg, "启动 MCP 子进程失败").await;
    }
}
