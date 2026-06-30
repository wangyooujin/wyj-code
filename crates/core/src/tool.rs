//! Tool trait — 所有工具的统一抽象

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use wyj_api::types::ToolDefinition;

/// 工具执行结果
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// 工具上下文（由运行时注入）
#[async_trait]
pub trait ToolContext: Send + Sync {
    fn cwd(&self) -> &std::path::Path;
    fn is_allowed(&self, name: &str, input: &Value) -> bool;
    /// 向用户提问并等待选择（TUI 模式下弹浮层，headless 返回 None）
    async fn ask_user(&self, _question: &str, _options: &[String]) -> Option<usize> {
        None
    }
}

/// 工具抽象
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }
    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>;
}
