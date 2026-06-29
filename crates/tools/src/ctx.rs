//! ToolCtx — 工具执行上下文

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use serde_json::Value;
use wyj_core::tool::ToolContext;

/// 权限模式
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMode {
    /// 需要用户逐个确认
    Prompt,
    /// 自动允许全部（危险：仅测试）
    AutoApprove,
    /// 白名单模式（仅允许列出的工具）
    Allowlist(HashSet<String>),
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub permission_mode: PermissionMode,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            permission_mode: PermissionMode::AutoApprove,
        }
    }
}

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
}
