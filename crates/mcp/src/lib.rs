//! wyj-mcp — MCP 客户端，将外部 MCP server 工具桥接为内部 Tool trait

pub mod bridge;
pub mod config;

pub use bridge::McpBridgeTool;
pub use config::McpServerConfig;
