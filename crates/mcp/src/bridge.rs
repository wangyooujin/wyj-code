//! MCP → Tool 桥接：将 MCP server 的工具暴露为 wyj_core::Tool

use crate::config::{McpServerConfig, McpTransport};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::service::RunningService;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
        TokioChildProcess,
    },
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

/// 单次 MCP 工具调用的超时上限。
///
/// `MCP_CONNECT_TIMEOUT` 只管「连上 + 发现工具」，**不管 `call_tool`**：
/// server 接受请求后不返回结果（自己内部卡死、或它转发给的某个 HTTP 服务
/// 挂住）时，`call_tool` 会无限期 await。
///
/// 危害有两层，一层比一层隐蔽：
/// 1. turn task 永不返回 → `is_thinking` 永久为 true → 用户后续输入全部
///    滞留 `pending_queue` 无人排空（详见 `api::stream_guard` 模块文档描述的
///    同一条死锁链，只是触发点从「LLM 流 hang」换成了「工具 hang」）。
/// 2. 本函数持有 `self.client` 这把 `tokio::sync::Mutex`；hang 期间锁不放，
///    **同一 server 的所有后续调用全部排队等一把永远不会开的锁**。
///
/// 默认 120s：远大于本地 stdio server 的正常调用耗时，又不至于让 UI
/// 无限期无反馈。可用 `WYJ_CODE_MCP_CALL_SECS` 覆盖（0 = 关闭）。
pub const MCP_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// 实际生效的 MCP 调用超时（首次调用时解析环境变量，之后固定）。
pub fn mcp_call_timeout() -> std::time::Duration {
    static CALL: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *CALL.get_or_init(|| {
        let Some(raw) = std::env::var("WYJ_CODE_MCP_CALL_SECS").ok() else {
            return MCP_CALL_TIMEOUT;
        };
        match raw.trim().parse::<u64>() {
            Ok(0) => std::time::Duration::MAX,
            Ok(secs) => std::time::Duration::from_secs(secs),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "WYJ_CODE_MCP_CALL_SECS 不是合法秒数，回退到默认值"
                );
                MCP_CALL_TIMEOUT
            }
        }
    })
}

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
    remote_tool_name: String,
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
        let params = CallToolRequestParams::new(self.remote_tool_name.clone()).with_arguments(args);

        // 取锁与调用包在同一个 timeout 里：锁的等待同样需要上界。只给
        // `call_tool` 单独加超时是不够的——前一次调用若已 hang 并持锁，
        // 后面的调用会永远卡在 `lock()` 上，根本走不到自己的超时。
        let timeout = mcp_call_timeout();
        let outcome = tokio::time::timeout(timeout, async {
            let guard = self.client.lock().await;
            guard.call_tool(params).await
        })
        .await;

        let result = match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => return Err(anyhow::anyhow!("MCP 工具调用失败: {e}")),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "MCP 工具调用超时（{}s 无响应）: {}",
                    timeout.as_secs(),
                    self.remote_tool_name
                ));
            }
        };

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
    let client_info = ClientInfo::default();
    let client: McpHandle = match cfg.transport {
        McpTransport::Stdio => {
            let cmd = cfg
                .command
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("stdio 传输需要 command 字段"))?;
            let mut command = Command::new(cmd);
            command.args(&cfg.args).envs(&cfg.env);

            // 子进程 stderr 必须隔离，避免污染 TUI。
            let (transport, _stderr) = TokioChildProcess::builder(command)
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| anyhow::anyhow!("启动 MCP 子进程失败: {e}"))?;
            client_info
                .serve(transport)
                .await
                .map_err(|e| anyhow::anyhow!("MCP 初始化失败: {e}"))?
        }
        McpTransport::StreamableHttp => {
            let url = cfg
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("streamable_http 传输需要 url 字段"))?;
            let mut custom_headers = std::collections::HashMap::new();
            for (name, value) in &cfg.headers {
                let header_name = http::HeaderName::try_from(name)
                    .map_err(|e| anyhow::anyhow!("MCP HTTP header 名非法 {name}: {e}"))?;
                let header_value = http::HeaderValue::try_from(resolve_env_reference(value))
                    .map_err(|e| anyhow::anyhow!("MCP HTTP header {name} 值非法: {e}"))?;
                custom_headers.insert(header_name, header_value);
            }
            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(url.to_string())
                    .custom_headers(custom_headers),
            );
            client_info
                .serve(transport)
                .await
                .map_err(|e| anyhow::anyhow!("MCP HTTP 初始化失败: {e}"))?
        }
    };

    build_bridges(client, &cfg.name).await
}

fn resolve_env_reference(value: &str) -> String {
    if let Some(name) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        return std::env::var(name).unwrap_or_default();
    }
    value.to_string()
}

async fn build_bridges(client: McpHandle, server_name: &str) -> Result<Vec<McpBridgeTool>> {
    let tools_result = client
        .list_all_tools()
        .await
        .map_err(|e| anyhow::anyhow!("获取工具列表失败: {e}"))?;

    let client = Arc::new(Mutex::new(client));
    let mut bridges = vec![];

    for mcp_tool in tools_result {
        let remote_tool_name = mcp_tool.name.to_string();
        let tool_name = format!(
            "mcp__{}__{}",
            sanitize_server_name(server_name),
            remote_tool_name
        );
        let schema = serde_json::to_value(&mcp_tool.input_schema)
            .unwrap_or(Value::Object(Default::default()));
        let def = ToolDefinition {
            name: tool_name.clone(),
            description: mcp_tool.description.as_deref().unwrap_or("").to_string(),
            input_schema: schema,
            native: None,
        };
        bridges.push(McpBridgeTool {
            tool_name,
            remote_tool_name,
            definition: def,
            client: client.clone(),
        });
    }

    tracing::info!(
        "MCP server {} 连接成功，发现 {} 个工具",
        server_name,
        bridges.len()
    );
    Ok(bridges)
}

fn sanitize_server_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
            url: None,
            headers: Default::default(),
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
            transport: McpTransport::StreamableHttp,
            command: None,
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
        };
        expect_err_containing(&cfg, "url").await;
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
