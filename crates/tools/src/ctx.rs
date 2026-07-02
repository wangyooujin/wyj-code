//! ToolCtx — 工具执行上下文

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer, ToolContext};

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

/// 工具向 TUI 发送的交互请求
pub enum UiAskRequest {
    /// AskQuestion 工具的多题访谈请求
    Questions {
        questions: Vec<AskQuestionSpec>,
        response_tx: tokio::sync::oneshot::Sender<Option<Vec<QuestionAnswer>>>,
    },
    /// ExitPlanMode 工具的计划批准请求，plan 为完整计划文本（Markdown）
    ExitPlanMode {
        plan: String,
        response_tx: tokio::sync::oneshot::Sender<bool>,
    },
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    /// 运行期可实时更新的共享句柄：审批/模式切换发生在某轮工具调用循环
    /// 进行中时（如 Plan 审批、Shift+Tab），需要立即影响同一轮剩余的
    /// 权限判定，因此不能是每轮快照一次的普通值。
    pub permission_mode: Arc<RwLock<PermissionMode>>,
    /// TUI 模式下注入此 sender，工具通过它向 TUI 发起交互
    pub ui_ask_tx: Option<mpsc::Sender<UiAskRequest>>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::AutoApprove)),
            ui_ask_tx: None,
        }
    }

    /// 就地替换权限模式的值（不改变共享句柄本身）。
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        *self.permission_mode.write().unwrap() = mode;
    }
}

#[async_trait]
impl ToolContext for ToolCtx {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn is_allowed(&self, name: &str, _input: &Value) -> bool {
        match &*self.permission_mode.read().unwrap() {
            PermissionMode::AutoApprove => true,
            PermissionMode::Prompt => true, // UI 层会弹确认，此处放行
            PermissionMode::Allowlist(set) => set.contains(name),
        }
    }

    fn allowed_tools(&self) -> Option<HashSet<String>> {
        match &*self.permission_mode.read().unwrap() {
            PermissionMode::Allowlist(set) => Some(set.clone()),
            _ => None,
        }
    }

    async fn ask_questions(&self, questions: &[AskQuestionSpec]) -> Option<Vec<QuestionAnswer>> {
        let tx = self.ui_ask_tx.as_ref()?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let req = UiAskRequest::Questions {
            questions: questions.to_vec(),
            response_tx,
        };
        tx.send(req).await.ok()?;
        response_rx.await.ok().flatten()
    }

    async fn exit_plan_mode(&self, plan: &str) -> bool {
        let tx = match &self.ui_ask_tx {
            Some(t) => t,
            None => return true, // headless：自动批准
        };
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let req = UiAskRequest::ExitPlanMode {
            plan: plan.to_string(),
            response_tx,
        };
        if tx.send(req).await.is_err() {
            return false;
        }
        response_rx.await.unwrap_or(false)
    }
}
