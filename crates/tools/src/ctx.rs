//! ToolCtx — 工具执行上下文

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use wyj_core::tool::ToolContext;

/// 权限模式
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMode {
    /// 需要用户逐个确认
    Prompt,
    /// 自动允许全部
    AutoApprove,
    /// 白名单模式（仅允许列出的工具）
    Allowlist(HashSet<String>),
}

/// AskQuestion 工具向 TUI 发送的交互请求
pub struct UiAskRequest {
    pub question: String,
    pub options: Vec<String>,
    pub response_tx: tokio::sync::oneshot::Sender<Option<usize>>,
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub permission_mode: PermissionMode,
    /// TUI 模式下注入此 sender，工具通过它向 TUI 发起交互
    pub ui_ask_tx: Option<mpsc::Sender<UiAskRequest>>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            permission_mode: PermissionMode::AutoApprove,
            ui_ask_tx: None,
        }
    }
}

#[async_trait]
impl ToolContext for ToolCtx {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn is_allowed(&self, name: &str, _input: &Value) -> bool {
        match &self.permission_mode {
            PermissionMode::AutoApprove => true,
            PermissionMode::Prompt => true, // UI 层会弹确认，此处放行
            PermissionMode::Allowlist(set) => set.contains(name),
        }
    }

    async fn ask_user(&self, question: &str, options: &[String]) -> Option<usize> {
        let tx = self.ui_ask_tx.as_ref()?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let req = UiAskRequest {
            question: question.to_string(),
            options: options.to_vec(),
            response_tx,
        };
        tx.send(req).await.ok()?;
        response_rx.await.ok().flatten()
    }
}
