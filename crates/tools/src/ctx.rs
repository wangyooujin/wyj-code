//! ToolCtx — 工具执行上下文

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
pub use wyj_core::permission::PermissionMode;
use wyj_core::permission::{
    safe_resolve_write_target, ExecutionSurface, PermissionPolicy, PermissionRequest,
    PermissionVerdict,
};
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer, ToolContext};

/// 逐调用权限确认的用户决策
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// 仅本次允许（computer-use 工具会把首次批准记为项目级授权）
    AllowOnce,
    /// 始终允许此类工具（写入项目级持久化，跨会话生效）
    AllowAlways,
    /// 拒绝执行
    Deny,
}

/// 首次批准后即按项目记住授权的 computer-use 工具。
///
/// 这两个名字分别对应旧前台兼容路径和 v1.4 后台目标化路径。它们仍各自
/// 独立授权，避免批准后台语义操作时隐式扩大到风险更高的前台全局输入。
pub const PROJECT_APPROVE_ONCE_TOOLS: &[&str] = &["computer", "app_computer"];

/// `name` 是否应在首次批准（AllowOnce 或 AllowAlways）后写入项目级授权。
pub fn is_project_approve_once_tool(name: &str) -> bool {
    PROJECT_APPROVE_ONCE_TOOLS.contains(&name)
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
    /// 运行表面决定 Prompt 是否真的具备人类审批通道。
    pub execution_surface: Arc<RwLock<ExecutionSurface>>,
    /// 路径受保护 deny 等范围授权。与 permission_mode 分离，确保 bypass
    /// 也不能关闭受保护路径。
    pub permission_policy: Arc<RwLock<PermissionPolicy>>,
    /// TUI 模式下注入此 sender，工具通过它向 TUI 发起交互
    pub ui_ask_tx: Option<mpsc::Sender<UiAskRequest>>,
    /// 已获项目级授权的工具名集合，启动时从项目级文件载入；AllowAlways，或
    /// computer-use 首次 AllowOnce 时就地插入并写盘。用 RwLock 支持 &self 修改。
    pub always_allowed: RwLock<HashSet<String>>,
    /// 「始终允许」持久化文件路径（`~/.wyj-code/projects/<project_key>/allowed_tools.json`）；
    /// None 时不持久化（如 headless 未启用）。
    pub allowed_tools_path: Option<PathBuf>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Prompt)),
            execution_surface: Arc::new(RwLock::new(ExecutionSurface::HeadlessRepl)),
            permission_policy: Arc::new(RwLock::new(PermissionPolicy::default())),
            ui_ask_tx: None,
            always_allowed: RwLock::new(HashSet::new()),
            allowed_tools_path: None,
        }
    }

    /// Create an independent session context with the same effective policy. Runtime permission
    /// mutations and ACP request channels do not leak between daemon-managed sessions.
    pub fn fork_for_surface(&self, surface: ExecutionSurface) -> Self {
        Self {
            cwd: self.cwd.clone(),
            permission_mode: Arc::new(RwLock::new(self.permission_mode.read().unwrap().clone())),
            execution_surface: Arc::new(RwLock::new(surface)),
            permission_policy: Arc::new(RwLock::new(
                self.permission_policy.read().unwrap().clone(),
            )),
            ui_ask_tx: None,
            always_allowed: RwLock::new(self.always_allowed.read().unwrap().clone()),
            allowed_tools_path: self.allowed_tools_path.clone(),
        }
    }

    /// 就地替换权限模式的值（不改变共享句柄本身）。
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        *self.permission_mode.write().unwrap() = mode;
    }

    pub fn set_execution_surface(&self, surface: ExecutionSurface) {
        *self.execution_surface.write().unwrap() = surface;
    }

    pub fn allow_plan_document(&self, path: &Path) -> Result<PathBuf, String> {
        self.permission_policy
            .write()
            .unwrap()
            .add_plan_document_grant(&self.cwd, path)
            .map_err(|reason| reason.message)
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

    /// 记住当前项目对某工具的授权，并立即 best-effort 落盘。
    fn allow_for_project(&self, name: &str) {
        self.always_allowed
            .write()
            .unwrap()
            .insert(name.to_string());
        self.persist_allowed_tools();
    }
}

#[async_trait]
impl ToolContext for ToolCtx {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn is_allowed(&self, name: &str, input: &Value) -> bool {
        let request = PermissionRequest {
            mode: self.permission_mode.read().unwrap().clone(),
            surface: *self.execution_surface.read().unwrap(),
            tool_name: name.to_string(),
            input: input.clone(),
            cwd: self.cwd.clone(),
        };
        match self.permission_policy.read().unwrap().evaluate(&request) {
            PermissionVerdict::Allow | PermissionVerdict::Ask(_) => true,
            PermissionVerdict::Deny(reason) => {
                tracing::warn!(tool = %name, code = reason.code, "{}", reason.message);
                false
            }
        }
    }

    fn allowed_tools(&self) -> Option<HashSet<String>> {
        match &*self.permission_mode.read().unwrap() {
            PermissionMode::Allowlist(set) | PermissionMode::Plan(set) => Some(set.clone()),
            _ => None,
        }
    }

    fn supports_interactive_confirmation(&self) -> bool {
        self.ui_ask_tx.is_some()
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
            None => return false,
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
        // 已获当前项目授权的工具直接放行。
        if self.always_allowed.read().unwrap().contains(name) {
            return true;
        }
        // 无 UI 通道（headless / 子 Agent）：fail-closed。
        let tx = match &self.ui_ask_tx {
            Some(t) => t,
            None => return false,
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
            Ok(PermissionDecision::AllowOnce | PermissionDecision::AllowAlways)
                if matches!(
                    &*self.permission_mode.read().unwrap(),
                    PermissionMode::Plan(_)
                ) && matches!(name, "Write" | "Edit") =>
            {
                self.allow_plan_document(Path::new(summary)).is_ok()
            }
            Ok(PermissionDecision::AllowOnce) if is_project_approve_once_tool(name) => {
                self.allow_for_project(name);
                true
            }
            Ok(PermissionDecision::AllowOnce) => true,
            Ok(PermissionDecision::AllowAlways) => {
                self.allow_for_project(name);
                true
            }
            Ok(PermissionDecision::Deny) | Err(_) => false,
        }
    }

    fn is_plan_mode(&self) -> bool {
        matches!(
            &*self.permission_mode.read().unwrap(),
            PermissionMode::Plan(_)
        )
    }

    fn resolve_write_target(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        safe_resolve_write_target(&self.cwd, raw).map_err(|reason| reason.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_core::tool::ToolContext;

    async fn confirm_with_decision(
        ctx: &ToolCtx,
        rx: &mut mpsc::Receiver<UiAskRequest>,
        name: &str,
        summary: &str,
        decision: PermissionDecision,
    ) -> bool {
        let respond = async {
            match rx.recv().await {
                Some(UiAskRequest::ToolPermission { response_tx, .. }) => {
                    let _ = response_tx.send(decision);
                }
                _ => panic!("expected tool permission request"),
            }
        };
        let (allowed, ()) = tokio::join!(ctx.confirm_tool(name, summary), respond);
        allowed
    }

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
    async fn computer_tools_allow_once_persists_for_same_project() {
        for name in PROJECT_APPROVE_ONCE_TOOLS {
            assert!(is_project_approve_once_tool(name));
            let base = tmp_base();
            let cwd = base.join("project");
            std::fs::create_dir_all(&cwd).unwrap();

            let mut ctx = ToolCtx::new(&cwd);
            ctx.load_allowed_tools(&base);
            ctx.set_permission_mode(PermissionMode::Prompt);
            let (tx, mut rx) = mpsc::channel(8);
            ctx.ui_ask_tx = Some(tx);

            assert!(
                confirm_with_decision(
                    &ctx,
                    &mut rx,
                    name,
                    "mutating action",
                    PermissionDecision::AllowOnce,
                )
                .await
            );
            assert!(ctx.always_allowed.read().unwrap().contains(*name));
            let persisted: Vec<String> = serde_json::from_str(
                &std::fs::read_to_string(ctx.allowed_tools_path.as_ref().unwrap()).unwrap(),
            )
            .unwrap();
            assert!(persisted.iter().any(|tool| tool == name));

            // 当前会话后续动作不再请求 UI。接收端已关闭，若仍发送会返回 false。
            drop(rx);
            assert!(ctx.confirm_tool(name, "another mutating action").await);

            // 重建同一项目的 ToolCtx 后仍直接放行。
            let mut reloaded = ToolCtx::new(&cwd);
            reloaded.load_allowed_tools(&base);
            reloaded.set_permission_mode(PermissionMode::Prompt);
            let (tx, rx) = mpsc::channel(1);
            reloaded.ui_ask_tx = Some(tx);
            drop(rx);
            assert!(reloaded.confirm_tool(name, "after restart").await);

            std::fs::remove_dir_all(&base).ok();
        }
    }

    #[tokio::test]
    async fn computer_project_approval_does_not_leak_to_another_project() {
        let base = tmp_base();
        let project_a = base.join("project-a");
        let project_b = base.join("project-b");
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();

        let mut approved = ToolCtx::new(&project_a);
        approved.load_allowed_tools(&base);
        approved.set_permission_mode(PermissionMode::Prompt);
        let (tx, mut rx) = mpsc::channel(1);
        approved.ui_ask_tx = Some(tx);
        assert!(
            confirm_with_decision(
                &approved,
                &mut rx,
                "app_computer",
                "click",
                PermissionDecision::AllowOnce,
            )
            .await
        );

        let mut isolated = ToolCtx::new(&project_b);
        isolated.load_allowed_tools(&base);
        isolated.set_permission_mode(PermissionMode::Prompt);
        let (tx, mut rx) = mpsc::channel(1);
        isolated.ui_ask_tx = Some(tx);
        assert!(
            !confirm_with_decision(
                &isolated,
                &mut rx,
                "app_computer",
                "click",
                PermissionDecision::Deny,
            )
            .await
        );
        assert!(!isolated
            .always_allowed
            .read()
            .unwrap()
            .contains("app_computer"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn computer_deny_does_not_persist() {
        let base = tmp_base();
        let cwd = base.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut ctx = ToolCtx::new(&cwd);
        ctx.load_allowed_tools(&base);
        ctx.set_permission_mode(PermissionMode::Prompt);
        let (tx, mut rx) = mpsc::channel(1);
        ctx.ui_ask_tx = Some(tx);
        assert!(
            !confirm_with_decision(
                &ctx,
                &mut rx,
                "computer",
                "left click",
                PermissionDecision::Deny,
            )
            .await
        );
        assert!(!ctx.always_allowed.read().unwrap().contains("computer"));
        assert!(!ctx.allowed_tools_path.as_ref().unwrap().exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn ordinary_tool_allow_once_remains_one_shot() {
        let mut ctx = ToolCtx::new("/tmp");
        ctx.set_permission_mode(PermissionMode::Prompt);
        let (tx, mut rx) = mpsc::channel(2);
        ctx.ui_ask_tx = Some(tx);

        assert!(
            confirm_with_decision(&ctx, &mut rx, "Bash", "ls", PermissionDecision::AllowOnce,)
                .await
        );
        assert!(!ctx.always_allowed.read().unwrap().contains("Bash"));
        assert!(
            !confirm_with_decision(&ctx, &mut rx, "Bash", "ls -la", PermissionDecision::Deny,)
                .await
        );
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
