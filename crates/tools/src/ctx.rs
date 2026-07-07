//! ToolCtx — 工具执行上下文

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer, ToolContext};

/// 逐调用权限确认的用户决策
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// 仅本次允许
    AllowOnce,
    /// 始终允许此类工具（写入项目级持久化，跨会话生效）
    AllowAlways,
    /// 拒绝执行
    Deny,
}

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
    /// 逐调用工具权限确认请求：tool_name 为工具名，action_summary 为操作摘要
    ToolPermission {
        tool_name: String,
        action_summary: String,
        response_tx: tokio::sync::oneshot::Sender<PermissionDecision>,
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
    /// 「始终允许」过的工具名集合（逐调用权限确认），启动时从项目级文件载入，
    /// AllowAlways 时就地插入并写盘。用 RwLock 提供 &self 下的内部可变性。
    pub always_allowed: RwLock<HashSet<String>>,
    /// 「始终允许」持久化文件路径（`~/.wyj-code/projects/<project_key>/allowed_tools.json`）；
    /// None 时不持久化（如 headless 未启用）。
    pub allowed_tools_path: Option<PathBuf>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::AutoApprove)),
            ui_ask_tx: None,
            always_allowed: RwLock::new(HashSet::new()),
            allowed_tools_path: None,
        }
    }

    /// 就地替换权限模式的值（不改变共享句柄本身）。
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        *self.permission_mode.write().unwrap() = mode;
    }

    /// 按当前 cwd 所属项目（git 仓库根）载入「始终允许」列表并设定持久化路径。
    /// `config_base` 为 `~/.wyj-code`。载入失败静默忽略（视为空列表）。
    pub fn load_allowed_tools(&mut self, config_base: &Path) {
        let key = wyj_core::project::project_key(&self.cwd);
        let dir = config_base.join("projects").join(key);
        let path = dir.join("allowed_tools.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(list) = serde_json::from_str::<Vec<String>>(&content) {
                *self.always_allowed.write().unwrap() = list.into_iter().collect();
            }
        }
        self.allowed_tools_path = Some(path);
    }

    /// 把当前「始终允许」集合写盘（best-effort，失败仅告警）。
    fn persist_allowed_tools(&self) {
        let Some(path) = &self.allowed_tools_path else {
            return;
        };
        let list: Vec<String> = {
            let set = self.always_allowed.read().unwrap();
            let mut v: Vec<String> = set.iter().cloned().collect();
            v.sort();
            v
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("写入 allowed_tools 失败: {e}");
                }
            }
            Err(e) => tracing::warn!("序列化 allowed_tools 失败: {e}"),
        }
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

    async fn confirm_tool(&self, name: &str, summary: &str) -> bool {
        // 仅 Prompt 模式拦截；AutoApprove(Bypass)/Allowlist(Plan) 不弹窗
        // （Allowlist 的准入已由 is_allowed 前置把关）。
        {
            let mode = self.permission_mode.read().unwrap();
            if !matches!(&*mode, PermissionMode::Prompt) {
                return true;
            }
        }
        // 已「始终允许」的工具直接放行
        if self.always_allowed.read().unwrap().contains(name) {
            return true;
        }
        // 无 UI 通道（headless / 子 Agent）：不阻塞，放行
        let tx = match &self.ui_ask_tx {
            Some(t) => t,
            None => return true,
        };
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let req = UiAskRequest::ToolPermission {
            tool_name: name.to_string(),
            action_summary: summary.to_string(),
            response_tx,
        };
        if tx.send(req).await.is_err() {
            return false;
        }
        match response_rx.await {
            Ok(PermissionDecision::AllowOnce) => true,
            Ok(PermissionDecision::AllowAlways) => {
                self.always_allowed
                    .write()
                    .unwrap()
                    .insert(name.to_string());
                self.persist_allowed_tools();
                true
            }
            Ok(PermissionDecision::Deny) | Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_core::tool::ToolContext;

    fn tmp_base() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wyj-ctx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn allowed_tools_persist_and_reload() {
        let base = tmp_base();
        let cwd = base.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut ctx = ToolCtx::new(&cwd);
        ctx.load_allowed_tools(&base);
        assert!(ctx.always_allowed.read().unwrap().is_empty());
        ctx.always_allowed.write().unwrap().insert("Bash".into());
        ctx.persist_allowed_tools();

        // 新 ctx 从盘上重新载入应看到 Bash
        let mut ctx2 = ToolCtx::new(&cwd);
        ctx2.load_allowed_tools(&base);
        assert!(ctx2.always_allowed.read().unwrap().contains("Bash"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn confirm_tool_auto_allows_outside_prompt_mode() {
        // AutoApprove（Bypass）不应弹窗，直接放行
        let ctx = ToolCtx::new("/tmp");
        ctx.set_permission_mode(PermissionMode::AutoApprove);
        assert!(ctx.confirm_tool("Bash", "ls").await);
    }

    #[tokio::test]
    async fn confirm_tool_allow_always_persists_in_memory() {
        let mut ctx = ToolCtx::new("/tmp");
        ctx.set_permission_mode(PermissionMode::Prompt);
        let (tx, mut rx) = mpsc::channel(8);
        ctx.ui_ask_tx = Some(tx);
        let responder = tokio::spawn(async move {
            if let Some(UiAskRequest::ToolPermission { response_tx, .. }) = rx.recv().await {
                let _ = response_tx.send(PermissionDecision::AllowAlways);
            }
        });
        assert!(ctx.confirm_tool("Bash", "ls").await);
        responder.await.unwrap();
        assert!(ctx.always_allowed.read().unwrap().contains("Bash"));
        // 第二次同名工具无需再问，直接放行（不再触发 responder）
        assert!(ctx.confirm_tool("Bash", "ls -la").await);
    }

    #[tokio::test]
    async fn confirm_tool_deny_returns_false() {
        let mut ctx = ToolCtx::new("/tmp");
        ctx.set_permission_mode(PermissionMode::Prompt);
        let (tx, mut rx) = mpsc::channel(8);
        ctx.ui_ask_tx = Some(tx);
        let responder = tokio::spawn(async move {
            if let Some(UiAskRequest::ToolPermission { response_tx, .. }) = rx.recv().await {
                let _ = response_tx.send(PermissionDecision::Deny);
            }
        });
        assert!(!ctx.confirm_tool("Bash", "rm -rf /").await);
        responder.await.unwrap();
        assert!(!ctx.always_allowed.read().unwrap().contains("Bash"));
    }
}
