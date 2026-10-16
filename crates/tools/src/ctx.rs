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
/// 这两个名字分别对应旧前台兼容路径和 v0.2.5 后台目标化路径。它们仍各自
/// 独立授权，避免批准后台语义操作时隐式扩大到风险更高的前台全局输入。
pub const PROJECT_APPROVE_ONCE_TOOLS: &[&str] = &["computer", "app_computer"];

/// `name` 是否应在首次批准（AllowOnce 或 AllowAlways）后写入项目级授权。
pub fn is_project_approve_once_tool(name: &str) -> bool {
    PROJECT_APPROVE_ONCE_TOOLS.contains(&name)
}

/// 全局实时输出 sink：TUI 进程启动时安装一次，之后所有 [`ToolCtx::new`] 构造的
/// ctx 自动带上它。
///
/// 做成进程级而不是逐个 ctx 传参，是因为一次 TUI 会话里 ToolCtx 会在八个
/// 回合构造点被重建（正常回合、追问、计划批准、压缩续跑……），逐处透传只会把
/// 同一个 sender 抄八遍。与 `bash_session::BashSessionManager::global()` 同理：
/// 一个进程同时只有一个交互式前台。headless / 子 Agent 走 `fork_for_surface`
/// 会显式清掉它。
static GLOBAL_TOOL_OUTPUT_TX: std::sync::OnceLock<mpsc::Sender<wyj_core::tool::ToolOutputChunk>> =
    std::sync::OnceLock::new();

/// 安装全局实时输出 sink（仅 TUI 前台调用，且一个进程只应调一次）。
///
/// 重复安装返回 false 且不覆盖：主循环若意外重入，新旧两个 receiver 会同时
/// 抢同一批快照，表现为输出随机停在半途。
pub fn install_global_tool_output_sink(tx: mpsc::Sender<wyj_core::tool::ToolOutputChunk>) -> bool {
    GLOBAL_TOOL_OUTPUT_TX.set(tx).is_ok()
}

/// 新建 ctx 时取用全局 sink（未安装则为 None）。
pub(crate) fn global_tool_output_sink() -> Option<mpsc::Sender<wyj_core::tool::ToolOutputChunk>> {
    GLOBAL_TOOL_OUTPUT_TX.get().cloned()
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
    /// 工具执行途中的实时输出通道，仅 TUI 前台挂载（headless / ACP / 子 Agent
    /// 为 None，见 `fork_for_surface`）。前台 Bash 据此把 stdout/stderr 尾部快照
    /// 推进聊天区；收不到就退化成「跑完一次性返回」，不改变结果内容。
    pub tool_output_tx: Option<mpsc::Sender<wyj_core::tool::ToolOutputChunk>>,
    /// 已获项目级授权的工具名集合，启动时从项目级文件载入；AllowAlways，或
    /// computer-use 首次 AllowOnce 时就地插入并写盘。用 RwLock 支持 &self 修改。
    pub always_allowed: RwLock<HashSet<String>>,
    /// 「始终允许」持久化文件路径（`~/.wyj-code/projects/<project_key>/allowed_tools.json`）；
    /// None 时不持久化（如 headless 未启用）。
    pub allowed_tools_path: Option<PathBuf>,
    /// 回合级共享注入队列（用户在 Agent 忙碌期间补充的内容）。
    ///
    /// **存在的理由是前台子 Agent**：`SubAgentTool::run()` 会 `await` 整个子
    /// Agent，而子 Agent 此前没有注入通道，用户在它跑的过程中补充的要求既进不了
    /// 子 Agent、也进不了被工具调用堵住的主 Agent，只能等它整个跑完。挂上这份
    /// 队列后，子 Agent 与主 Agent 排空同一份，谁先到工具轮边界谁取走。
    /// headless / cron / 后台子 Agent 场景为 None。
    pub steer_queue: Option<Arc<wyj_core::SteerQueue>>,
    /// 子 Agent 用量的共享累加器。主会话 ctx 持有它，`SubAgentTool` 把子 Agent
    /// 自己的 `Session` 计数加进来，主 Agent 每批工具执行后抽干入账。
    ///
    /// **逐层透传，不会重复计数**：每一层只把自己那层 `Session` 的
    /// `total_*` 加进去，而各层 `Session` 的计数只覆盖**自己发出的** API
    /// 调用——嵌套子 Agent 的调用记在它自己的 Session 上，与父层的
    /// `total_*` 天然不相交。`fork_for_surface` 仍然清掉它：fork 出来的
    /// 是独立会话（ACP/daemon），不该继承某一个会话的累加器。
    pub sub_usage: Option<wyj_core::session::SharedSubUsage>,
    /// Auto 模式判定闸门（`PermissionMode::Auto` 下由它决定要不要问人）。
    ///
    /// 随 ctx 一起派生（见 `fork_for_surface`）而不是挂在 Agent 上：子 Agent 的
    /// ctx 从父 ctx 派生，闸门因此自动向下传递，降级阈值计数也不会分裂成
    /// 每个子 Agent 一份。
    pub auto_gate: Option<std::sync::Arc<dyn wyj_core::auto_mode::AutoGate>>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            permission_mode: Arc::new(RwLock::new(PermissionMode::Auto)),
            execution_surface: Arc::new(RwLock::new(ExecutionSurface::HeadlessRepl)),
            permission_policy: Arc::new(RwLock::new(PermissionPolicy::default())),
            ui_ask_tx: None,
            tool_output_tx: global_tool_output_sink(),
            always_allowed: RwLock::new(HashSet::new()),
            allowed_tools_path: None,
            steer_queue: None,
            sub_usage: None,
            auto_gate: None,
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
            tool_output_tx: None,
            always_allowed: RwLock::new(self.always_allowed.read().unwrap().clone()),
            allowed_tools_path: self.allowed_tools_path.clone(),
            // ACP/daemon 的会话各自独立：注入队列不跨会话共享（daemon 本身也没有
            // 「回合进行中补话」这个交互面）。实时输出同理——非 TUI 表面没有
            // 接收端，挂上只会让工具白白多跑一条推送路径。
            steer_queue: None,
            // fork 出的会话各自独立（ACP/daemon），不该继承某一个会话的子 Agent
            // 用量累加器——那会把别的会话的开销记到本会话头上。
            sub_usage: None,
            // 闸门**要**继承：它是进程级的策略实例（同一个 Provider、同一份
            // 降级计数），子 Agent 绕过它就等于绕过 auto 模式的全部保护。
            auto_gate: self.auto_gate.clone(),
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
    ///
    /// 跨进程安全的关键是**在锁内重新读盘再取并集**，而不是把内存快照整份覆盖：
    /// 本进程启动时只 `load_allowed_tools` 一次，此后内存里那份集合对别的进程
    /// 发生的审批一无所知。直接整份写回会让「A 批准的 Bash」被「B 批准的 Edit」
    /// 静默抹掉，用户表现为下次启动又要重新按一遍 `a`；而旧的裸 `fs::write` 在并发
    /// 时还会截出半截 JSON，`load_allowed_tools` 的 `if let Ok` 把它当空列表 ——
    /// 该项目的「始终允许」于是**整体**失效一次。锁 + 原子替换 + 取并集三者缺一不可。
    fn persist_allowed_tools(&self) {
        let Some(path) = &self.allowed_tools_path else {
            return;
        };
        let guard = match wyj_config::filelock::acquire(path, "project tool allowlist") {
            Ok(guard) => guard,
            Err(e) => {
                tracing::warn!("获取 allowed_tools 写锁失败: {e}");
                return;
            }
        };
        let mut merged: std::collections::HashSet<String> = std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_json::from_str::<Vec<String>>(&content).ok())
            .unwrap_or_default()
            .into_iter()
            .collect();
        merged.extend(self.always_allowed.read().unwrap().iter().cloned());
        let mut list: Vec<String> = merged.into_iter().collect();
        list.sort();
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = wyj_core::serialize::write_atomic(path, json.as_bytes()) {
                    tracing::warn!("写入 allowed_tools 失败: {e}");
                }
            }
            Err(e) => tracing::warn!("序列化 allowed_tools 失败: {e}"),
        }
        drop(guard);
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

    fn steer_queue(&self) -> Option<&Arc<wyj_core::SteerQueue>> {
        self.steer_queue.as_ref()
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

    fn tool_output(&self) -> Option<&mpsc::Sender<wyj_core::tool::ToolOutputChunk>> {
        self.tool_output_tx.as_ref()
    }

    fn sub_usage_sink(&self) -> Option<&wyj_core::session::SharedSubUsage> {
        self.sub_usage.as_ref()
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
        // 只有 Auto 需要弹窗：闸门判 Allow 时压根不会走到这里，只有判 AskUser
        // （含「压根没装闸门」与「分类器已停用降级」）才落下来——所以 Auto 必须
        // 走真实弹窗路径，否则这两条会变成无声放行。
        // AutoApprove(Bypass)/Allowlist(Plan) 则不弹窗（Allowlist 的准入已由
        // is_allowed 前置把关）。
        {
            let mode = self.permission_mode.read().unwrap();
            if !matches!(&*mode, PermissionMode::Auto) {
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

    fn needs_dialog(&self, name: &str) -> bool {
        // 判据与 confirm_tool 的两个「直接放行」分支严格对齐，差一个字就会
        // 出现「Agent 以为不弹框、把多个调用排进并发组，结果同时弹出 N 个
        // 对话框」——那正好破坏 `needs_dialog` 存在的唯一理由。
        // Auto 也返回 true：闸门判定要 await 分类器、判定为 AskUser 时还要弹窗，
        // 两者都会阻塞。这些调用必须串行，否则会同时弹出 N 个对话框。
        if !matches!(&*self.permission_mode.read().unwrap(), PermissionMode::Auto) {
            return false;
        }
        if self.always_allowed.read().unwrap().contains(name) {
            return false;
        }
        // 无 UI 通道时 confirm_tool 直接 fail-closed 返回 false（并不 await），
        // 但那种情况下调用压根不会被执行，仍按会弹框处理，让这类上下文
        // 退回串行的旧行为。
        true
    }

    fn is_plan_mode(&self) -> bool {
        matches!(
            &*self.permission_mode.read().unwrap(),
            PermissionMode::Plan(_)
        )
    }

    fn permission_mode(&self) -> Option<wyj_core::permission::PermissionMode> {
        Some(self.permission_mode.read().unwrap().clone())
    }

    fn auto_gate(&self) -> Option<&std::sync::Arc<dyn wyj_core::auto_mode::AutoGate>> {
        self.auto_gate.as_ref()
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

    /// 钉子：`needs_dialog` 的两个 false 分支必须与 `confirm_tool` 的两个
    /// 「直接放行、不发 UiAskRequest」分支一字不差地对齐。
    ///
    /// 错位的后果是双向的，且都很难在别处被发现：
    /// `needs_dialog` 判 false 而 `confirm_tool` 仍发请求 → Agent 把多个调用
    /// 排进并发组 → 同一时刻弹出多个对话框；
    /// 反向错位只是白白丢掉并发，属于性能退化而非正确性问题，所以判据只按
    /// 「宁可串行，不可多弹框」的方向偏保守。
    #[test]
    fn needs_dialog_matches_confirm_tool_fast_paths() {
        let base = tmp_base();
        let cwd = base.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut ctx = ToolCtx::new(&cwd);
        ctx.load_allowed_tools(&base);

        // Prompt 模式 + 未授权：会弹框。
        *ctx.permission_mode.write().unwrap() = PermissionMode::Auto;
        assert!(ctx.needs_dialog("Bash"));
        assert!(ctx.needs_dialog("Edit"));

        // 项目级 AllowAlways（用户按过 a）：不再弹框。
        ctx.always_allowed.write().unwrap().insert("Bash".into());
        assert!(!ctx.needs_dialog("Bash"), "已 AllowAlways 的工具不该再弹框");
        assert!(ctx.needs_dialog("Edit"), "只授权了 Bash，Edit 仍需弹框");

        // 其余模式一律不弹框。
        for mode in [
            PermissionMode::AutoApprove,
            PermissionMode::Plan(HashSet::new()),
        ] {
            *ctx.permission_mode.write().unwrap() = mode.clone();
            assert!(!ctx.needs_dialog("Bash"), "{mode:?} 不该弹框");
            assert!(!ctx.needs_dialog("Edit"), "{mode:?} 不该弹框");
        }

        std::fs::remove_dir_all(&base).ok();
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
        ctx.set_permission_mode(PermissionMode::Auto);
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
            ctx.set_permission_mode(PermissionMode::Auto);
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
            reloaded.set_permission_mode(PermissionMode::Auto);
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
        approved.set_permission_mode(PermissionMode::Auto);
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
        isolated.set_permission_mode(PermissionMode::Auto);
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
        ctx.set_permission_mode(PermissionMode::Auto);
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
        ctx.set_permission_mode(PermissionMode::Auto);
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
        ctx.set_permission_mode(PermissionMode::Auto);
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

    #[tokio::test]
    async fn bypass_mode_exposes_auto_approve_and_no_allowlist() {
        // Bypass 下 `allowed_tools()` 返回 None（白名单语义不适用），
        // 子 Agent 必须改从 `permission_mode()` 读到 AutoApprove，
        // 否则会落回 Prompt 而被无 UI 通道 fail-closed。
        let ctx = ToolCtx::new("/tmp");
        ctx.set_permission_mode(PermissionMode::AutoApprove);
        assert!(ctx.allowed_tools().is_none());
        assert!(matches!(
            ctx.permission_mode(),
            Some(PermissionMode::AutoApprove)
        ));
        assert!(ctx.confirm_tool("Bash", "ls").await);
    }

    /// Auto 模式下子 Agent 的 Bash 必须 fail-closed：子 ctx 不接 `ui_ask_tx`，
    /// 闸门判 `AskUser` 时 `confirm_tool` 没有审批通道可走。
    ///
    /// 这是删掉 `PermissionMode::Prompt` 之后**唯一**保证「无交互面不放行」的
    /// 关卡（policy 层在 Auto 下对一切非受保护路径都返回 `Allow`），所以两条
    /// 路径都要钉：`is_allowed` 放行不等于执行，真正拦住的是 `confirm_tool`。
    #[tokio::test]
    async fn sub_agent_bash_fails_closed_under_auto_without_an_approval_channel() {
        use serde_json::json;
        let cwd = std::env::temp_dir();
        let input = json!({ "command": "ls -la" });

        let ctx = ToolCtx::new(&cwd);
        ctx.set_execution_surface(wyj_core::ExecutionSurface::SubAgent);
        ctx.set_permission_mode(PermissionMode::Auto);
        assert!(
            ctx.is_allowed("Bash", &input),
            "Auto 在 policy 层不自行判定副作用，交由闸门 / confirm_tool"
        );
        assert!(
            !ctx.confirm_tool("Bash", "ls -la").await,
            "子 Agent ctx 没有 ui_ask_tx，闸门判 AskUser 时必须 fail-closed"
        );
    }

    /// Bypass 派生的子 Agent 全放行，不受上面那条约束影响。
    #[tokio::test]
    async fn sub_agent_bash_is_allowed_after_inheriting_auto_approve() {
        use serde_json::json;
        let cwd = std::env::temp_dir();
        let input = json!({ "command": "ls -la" });

        let after = ToolCtx::new(&cwd);
        after.set_execution_surface(wyj_core::ExecutionSurface::SubAgent);
        after.set_permission_mode(PermissionMode::AutoApprove);
        assert!(
            after.is_allowed("Bash", &input),
            "继承放行后 evaluate 应放行"
        );
        assert!(after.confirm_tool("Bash", "ls -la").await);
    }

    /// Plan 模式必须**继续**收窄：子 Agent 拿到的是只读白名单 + Plan 语义，
    /// 写工具与非只读 Bash 仍应被拒。
    #[test]
    fn plan_parent_keeps_sub_agent_read_only() {
        use serde_json::json;
        let allowed: HashSet<String> = ["Read", "Glob", "Grep", "Bash"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ctx = ToolCtx::new(std::env::temp_dir());
        ctx.set_execution_surface(wyj_core::ExecutionSurface::SubAgent);
        ctx.set_permission_mode(PermissionMode::Plan(allowed));
        // 只读命令放行
        assert!(ctx.is_allowed("Bash", &json!({ "command": "ls" })));
        // 有副作用的命令仍被拒
        assert!(!ctx.is_allowed("Bash", &json!({ "command": "rm -rf /tmp/x" })));
        // 写工具不在白名单
        assert!(!ctx.is_allowed(
            "Write",
            &json!({ "file_path": "src/lib.rs", "content": "x" })
        ));
    }
}
