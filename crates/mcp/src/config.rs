//! MCP Server 配置结构

use serde::{Deserialize, Serialize};

/// 单个 MCP server 配置（在 ~/.wyj-code/config.toml 的 [[mcp_servers]] 段声明）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 服务名称（用于区分多个 server）
    pub name: String,
    /// 传输类型
    pub transport: McpTransport,
    /// stdio: 执行命令
    #[serde(default)]
    pub command: Option<String>,
    /// stdio: 参数列表
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: 环境变量
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}
