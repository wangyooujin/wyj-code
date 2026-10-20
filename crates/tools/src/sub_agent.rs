//! 子 Agent 工具 — 按类型定义派生独立的嵌套 Agent 完成复杂子任务
//!
//! 每个子 Agent 整体 `tokio::spawn` 为独立任务并登记进 [`SubAgentHub`]：
//! 前台调用等待任务结果返回；`run_in_background: true` 时立即返回、任务跨轮次
//! 继续运行，完成结果由前端（TUI/headless）通过 Hub 的 Done 事件投递。

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use wyj_api::types::{ContentBlock, ToolDefinition};
use wyj_core::{
    tool::{Tool, ToolCallMeta, ToolContext, ToolResult},
    Agent, AgentDefinition, Session, ToolEvent,
};
use wyj_i18n::{tr, tr_fmt};

use crate::agent_hub::{AgentControl, SubAgentEvent, SubAgentHub};

/// 按 agent 定义创建子 Agent（持有 provider 和按定义过滤后的工具集）
pub type AgentFactory = Arc<dyn Fn(&AgentDefinition) -> Result<Agent> + Send + Sync>;
pub type SharedAgentDefinitions = Arc<std::sync::RwLock<Vec<AgentDefinition>>>;

pub struct SubAgentTool {
    defs: SharedAgentDefinitions,
    hub: Arc<SubAgentHub>,
    factory: AgentFactory,
    root_session_id: Option<String>,
    caller_id: Option<u64>,
    depth: usize,
    max_depth: usize,
    /// 用户配置的回复语言。`inp.system` 覆盖走的是 `with_system`（整体替换），
    /// 会把工厂追加的语言段一并抹掉，因此覆盖之后要按这里存的 locale 补回来。
    response_language: Option<String>,
    /// `~/.wyj-code/sessions`：交接物落盘根目录。`None`（测试/未接文件系统）
    /// 时跳过落盘，行为与本字段引入前一致。
    sessions_dir: Option<PathBuf>,
    /// 消息增量日志写手。与 `sessions_dir` 独立开关：它只解决「进程被硬杀」
    /// 这一种情况，不挂就退回「活着的收尾路径都能写交接物」的原有保证。
    message_log: Option<Arc<crate::message_log::MessageLogWriter>>,
}

impl SubAgentTool {
    pub fn new(
        defs: Arc<Vec<AgentDefinition>>,
        hub: Arc<SubAgentHub>,
        factory: impl Fn(&AgentDefinition) -> Result<Agent> + Send + Sync + 'static,
    ) -> Self {
        Self {
            defs: Arc::new(std::sync::RwLock::new((*defs).clone())),
            hub,
            factory: Arc::new(factory),
            root_session_id: None,
            caller_id: None,
            depth: 0,
            max_depth: 3,
            response_language: None,
            sessions_dir: None,
            message_log: None,
        }
    }

    pub fn new_shared(
        defs: SharedAgentDefinitions,
        hub: Arc<SubAgentHub>,
        factory: impl Fn(&AgentDefinition) -> Result<Agent> + Send + Sync + 'static,
    ) -> Self {
        Self {
            defs,
            hub,
            factory: Arc::new(factory),
            root_session_id: None,
            caller_id: None,
            depth: 0,
            max_depth: 3,
            response_language: None,
            sessions_dir: None,
            message_log: None,
        }
    }

    /// 记录用户配置的回复语言，供 `inp.system` 覆盖后补回语言段。
    pub fn with_response_language(mut self, language: impl Into<String>) -> Self {
        self.response_language = Some(language.into());
        self
    }

    /// Bind sub-agent Episodes to the root conversation. All descendants reuse
    /// this id so several sub-agents from one user session do not masquerade as
    /// independent sessions when Evolution evaluates promotion thresholds.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.root_session_id = Some(session_id.into());
        self
    }

    /// 覆盖最大嵌套层数（来自 `[subagent].max_depth`，默认 3）。
    ///
    /// 0 或 1 都表示「子 Agent 不允许再派子 Agent」；上层调用点负责钳到
    /// 至少 1，避免一个手滑的配置把 `Agent` 工具从主 Agent 那里也拿掉。
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth.max(1);
        self
    }

    /// 指定交接物落盘根目录（`~/.wyj-code/sessions`）。
    ///
    /// 不调用时完全不落盘——测试与「没接文件系统」的调用方保持原有行为。
    pub fn with_sessions_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.sessions_dir = Some(dir.into());
        self
    }

    /// 挂上消息增量日志写手（硬杀场景的唯一恢复依据）。
    pub fn with_message_log(mut self, log: Arc<crate::message_log::MessageLogWriter>) -> Self {
        self.message_log = Some(log);
        self
    }

    fn find_def(&self, type_name: &str) -> Option<AgentDefinition> {
        self.defs
            .read()
            .ok()?
            .iter()
            .find(|d| d.name == type_name)
            .cloned()
    }

    fn available_types(&self) -> String {
        self.defs
            .read()
            .map(|defs| {
                defs.iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default = "default_action")]
    action: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    subagent_type: Option<String>,
    /// 3-5 词的任务简述（UI 展示用）
    #[serde(default)]
    description: Option<String>,
    /// 后台运行：立即返回，完成结果以 system-reminder 注入主对话
    #[serde(default)]
    run_in_background: Option<bool>,
    /// 覆盖类型定义的系统提示（可选，向后兼容旧调用格式）
    #[serde(default)]
    system: Option<String>,
    /// message/interrupt/retry target.
    #[serde(default)]
    target_id: Option<u64>,
}

fn default_action() -> String {
    "spawn".to_string()
}

/// 提取工具输入的主参数做一行摘要（UI 的"当前工具"展示用）
fn summarize_input(input: &Value) -> String {
    for key in [
        "file_path",
        "path",
        "pattern",
        "command",
        "url",
        "prompt",
        "query",
    ] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return wyj_core::textutil::truncate_chars(s, 60, true);
        }
    }
    wyj_core::textutil::truncate_chars(&input.to_string(), 60, true)
}

/// 把工具摘要渲染成交接外壳里的一小段事实清单。
///
/// 刻意只列**结论性**的字段（改了哪些文件、跑过什么、卡在什么错误上），
/// 不列读过的原文——原文可再生（重读一次），塞进主对话就等于把子 Agent 省下
/// 的上下文一次性还回去。返回 `None` 表示一个工具都没调用，此时外壳里连这一
/// 段都不出现，避免给模型一段空标题。
fn format_digest(digest: &crate::handoff::ToolDigest) -> Option<String> {
    if digest.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if !digest.files_modified.is_empty() {
        lines.push(format!(
            "Files modified: {}",
            digest.files_modified.join(", ")
        ));
    }
    if !digest.files_read.is_empty() {
        lines.push(format!("Files read: {}", digest.files_read.join(", ")));
    }
    if !digest.commands.is_empty() {
        lines.push(format!("Commands run: {}", digest.commands.join("; ")));
    }
    if !digest.searches.is_empty() {
        lines.push(format!("Searches: {}", digest.searches.join("; ")));
    }
    if !digest.errors.is_empty() {
        lines.push(format!("Errors seen: {}", digest.errors.join("; ")));
    }
    lines.push(format!("Tool calls: {}", digest.total_calls));
    Some(lines.join("\n"))
}

/// 从 `Agent::run_turn` 的错误里识别「推理轮数触顶」。
///
/// 触顶是一种**有产出**的失败：模型跑了几十轮有效工具调用才撞上上限，工作树
/// 里已经改了东西，只是没来得及交付。把它与 provider 失败、工具 panic 这类
/// 「可能什么都没做出来」的错误分开，交接物的措辞与后续建议都不同。
fn is_turn_limit_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("超过最大推理轮数")
}

/// 组装交接物。
///
/// `last_turn_text` 优先于「从 messages 里捞最后一轮 assistant」：前者是模型
/// 这一轮真正交出的文本，后者还要分辨哪些是中间工具轮的空 assistant 消息。
/// 只有在它为空（例如连一轮都没跑完就被中断）时才回退到扫描 messages。
#[allow(clippy::too_many_arguments)]
fn build_handoff<'a>(
    id: u64,
    agent_type: &str,
    description: &str,
    background: bool,
    status: crate::handoff::HandoffStatus,
    reason: Option<String>,
    last_turn_text: &str,
    messages: impl Iterator<Item = &'a wyj_api::types::Message>,
    elapsed_secs: f64,
) -> crate::handoff::SubAgentHandoff {
    let conclusion = if last_turn_text.trim().is_empty() {
        last_assistant_text(messages)
    } else {
        last_turn_text.to_string()
    };
    crate::handoff::SubAgentHandoff {
        version: crate::handoff::HANDOFF_VERSION,
        id,
        agent_type: agent_type.to_string(),
        description: description.to_string(),
        background,
        status,
        reason,
        conclusion: crate::textutil::truncate_str(
            &conclusion,
            crate::handoff::CONCLUSION_MAX_BYTES,
        )
        .to_string(),
        digest: crate::handoff::ToolDigest::default(),
        elapsed_secs,
    }
}

/// 回退路径：从消息历史里取最后一条非空 assistant 文本。
fn last_assistant_text<'a>(messages: impl Iterator<Item = &'a wyj_api::types::Message>) -> String {
    let mut out = String::new();
    for msg in messages {
        if !matches!(msg.role, wyj_api::types::Role::Assistant) {
            continue;
        }
        let text: String = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if !text.trim().is_empty() {
            out = text;
        }
    }
    out
}

/// 该 agent 类型定义是否可能被授予有外部副作用的工具。
///
/// 供 `SubAgentTool::needs_permission` 决定「要不要先问用户」：
///   * `tools: None` —— 标准工具集全集，含 Write/Edit/Bash → 有副作用；
///   * `tools: Some(list)` —— 只在 list 里出现副作用工具时才算有。
///
/// 内置 Explore / Plan 的 `tools` 是 `READONLY_TOOLS`，因此**不触发**审批；
/// general-purpose 触发。这样审批弹窗只出现在真正可能改盘/执行命令的
/// 委派上，读多写少的探索类委派不被打断。
///
/// `disallowed_tools` 参与判定：`tools: None` + `disallowed_tools: Write, Edit,
/// Bash` 的定义实际拿不到任何副作用工具，不该再弹一次窗——否则用户会被问
/// 「要不要放行一个只读 Agent」这种没有意义的问题。
pub(crate) fn def_can_touch_side_effects(def: &AgentDefinition) -> bool {
    /// 会被 `is_side_effect_tool` 视为有副作用、且在标准工具集里的工具名。
    const SIDE_EFFECT_TOOLS: &[&str] = &[
        "Write",
        "Edit",
        "Bash",
        "BashOutput",
        "KillShell",
        "computer",
        "app_computer",
    ];
    let allowed_and_side_effecting = |name: &str| {
        SIDE_EFFECT_TOOLS.contains(&name) && !def.disallowed_tools.iter().any(|d| d == name)
    };
    match &def.tools {
        None => SIDE_EFFECT_TOOLS
            .iter()
            .any(|name| allowed_and_side_effecting(name)),
        Some(list) => list.iter().any(|name| allowed_and_side_effecting(name)),
    }
}

/// 装配子 Agent 的 `ToolCtx`。
///
/// 三个必须往下传的东西：补充消息队列（让嵌套子 Agent 也能被中途插话）、用量
/// 累加器（嵌套开销能回到根会话）、以及 **Auto 判定闸门**。
///
/// 闸门这条是 Auto 模式的命门：[`derive_sub_agent_permission_mode`] 让 Auto 原样
/// 继承给子 Agent，判定主体因此从「人」换成「闸门」。子 Agent 恒无 `ui_ask_tx`，
/// 一旦这里漏传，`Agent::ensure_tool_permission` 就会退到「逐次人工确认」，再在
/// `ToolCtx::confirm_tool` 上 fail-closed —— 结果是子 Agent 的 Bash/Write/Edit
/// 100% 被拒，而 Manual 下这些走 `AutoApprove` 反而全放行，比父级更严，
/// 与 `ToolCtx::fork_for_surface` 里「闸门要继承」的既有语义也自相矛盾。
fn sub_tool_ctx(
    cwd: &std::path::Path,
    auto_gate: Option<Arc<dyn wyj_core::auto_mode::AutoGate>>,
    steer: Option<Arc<wyj_core::SteerQueue>>,
    sub_usage_sink: Option<wyj_core::session::SharedSubUsage>,
    parent_mode: Option<wyj_core::permission::PermissionMode>,
    allowed: Option<std::collections::HashSet<String>>,
    parent_is_plan: bool,
) -> crate::ctx::ToolCtx {
    let mut sub_ctx = crate::ctx::ToolCtx::new(cwd);
    sub_ctx.set_execution_surface(wyj_core::ExecutionSurface::SubAgent);
    sub_ctx.steer_queue = steer;
    sub_ctx.sub_usage = sub_usage_sink;
    sub_ctx.auto_gate = auto_gate;
    // 继承父级的工具白名单限制（如 Plan 模式），避免子 Agent 成为绕过限制的后门；
    // 类型定义自身的工具限制已在 factory 注册工具时收窄，交集生效。
    // 子 Agent 没有审批 UI，不存在运行中被外部改权限的场景，因此 `ToolCtx::new`
    // 内部就构造了独立的共享句柄（而非复用父 ctx 的 Arc），避免父子间意外共享可变状态。
    sub_ctx.set_permission_mode(derive_sub_agent_permission_mode(
        parent_mode,
        allowed,
        parent_is_plan,
    ));
    sub_ctx
}

/// 派生子 Agent 时，忠实继承父级的权限模式。
///
/// 旧实现只看 `ctx.allowed_tools()`，而它对 `Prompt` / `AutoApprove` 一律返回
/// `None`，于是子 Agent 落回 `ToolCtx::new` 的默认 `Prompt`。子 Agent 没有
/// 审批 UI（`ui_ask_tx` 恒为 None），`Prompt` + 无 UI 通道在两道关卡上都是
/// fail-closed：
///   * `PermissionPolicy::evaluate`：`Prompt` + 副作用工具 + 非交互 surface
///     → `Deny("interactive_approval_unavailable")`；
///   * `ToolCtx::confirm_tool`：`Prompt` + 无 `ui_ask_tx` → `return false`。
///
/// 结果就是子 Agent 的 Bash/Edit/Write 被全量拒掉，而父级同一个工具正常。
/// 这在 **Bypass(`AutoApprove`)** 下尤其荒谬：用户显式选择了「全部放行」，
/// 子 Agent 却比主 Agent 更严。Bypass 现在原样传给子 Agent。
///
/// 白名单语义（`Plan`/`Allowlist`）维持「只能更严不能更松」的既有行为：
/// Plan 父级再收窄一层只读白名单，其余按原白名单继承。
pub(crate) fn derive_sub_agent_permission_mode(
    parent_mode: Option<wyj_core::permission::PermissionMode>,
    allowed: Option<std::collections::HashSet<String>>,
    parent_is_plan: bool,
) -> crate::ctx::PermissionMode {
    use crate::ctx::PermissionMode;
    match parent_mode {
        // Bypass 显式全放行 → 子 Agent 同样全放行
        Some(PermissionMode::AutoApprove) => PermissionMode::AutoApprove,
        // Auto：**原样继承**。理由是 Auto 的判定者不是人而是闸门——它不需要
        // 审批 UI（headless 也能判），而闸门随 ctx 派生自动传给子 Agent。
        // 降成 AutoApprove 等于把子 Agent 里最容易出事的那部分写/执行动作
        // 整个移出分类器视野。
        //
        // 子 Agent 内部判到 `AskUser` 时会在 `confirm_tool` 处 fail-closed
        // （子 ctx 不接 `ui_ask_tx`），这是有意的：委派动作本身已经在父级
        // 按 `def_can_touch_side_effects` 问过一次，子任务内部再判不该悄悄
        // 放行，也不该把 general-purpose 的 Bash/Edit/Write 全部废掉。
        Some(PermissionMode::Auto) => PermissionMode::Auto,
        _ => match allowed {
            None => PermissionMode::Auto,
            Some(allowed) => {
                if parent_is_plan {
                    let read_only = allowed
                        .into_iter()
                        .filter(|name| {
                            !matches!(
                                name.as_str(),
                                "Write"
                                    | "Edit"
                                    | "Agent"
                                    | "computer"
                                    | "app_computer"
                                    | "ExitPlanMode"
                            )
                        })
                        .collect();
                    PermissionMode::Plan(read_only)
                } else {
                    PermissionMode::Allowlist(allowed)
                }
            }
        },
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    /// 只有当被委派的 agent 类型**可能**拿到 Write/Edit/Bash 这类有副作用的
    /// 工具时才要求审批。
    ///
    /// 背景：子 Agent 没有审批 UI，`Normal` 模式下若父级不做任何拦截，
    /// 子 Agent 会在 `evaluate` 与 `confirm_tool` 两处 fail-closed 被全量
    /// 拒绝。放宽子 Agent 之前必须在这里补上审批点，否则整个权限系统在默认
    /// 模式下可被「委派」一步绕过：主 Agent 零弹窗拉起子 Agent，子 Agent
    /// 零弹窗执行任意命令。
    ///
    /// 读多写少的 Explore / Plan（`tools` = `READONLY_TOOLS`）不触发审批，
    /// 避免探索类委派被弹窗打断。解析不到定义时返回 false —— `run()` 本身
    /// 会返回「未知类型」错误，无需再拦一道。
    fn needs_permission(&self, input: &Value) -> bool {
        let type_name = input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general-purpose");
        self.find_def(type_name)
            .map(|def| def_can_touch_side_effects(&def))
            .unwrap_or(false)
    }

    fn action_summary(&self, input: &Value) -> String {
        let type_name = input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general-purpose");
        let desc = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if desc.is_empty() {
            tr_fmt("subagent.spawn_summary", &[("type", type_name)])
        } else {
            tr_fmt(
                "subagent.spawn_summary_named",
                &[("type", type_name), ("desc", desc)],
            )
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        let defs = self
            .defs
            .read()
            .map(|defs| defs.clone())
            .unwrap_or_default();
        let types = defs
            .iter()
            .map(|d| format!("- {}: {}", d.name, d.description))
            .collect::<Vec<_>>()
            .join("\n");
        let type_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::SUB_AGENT_TEMPLATE.replace("{types}", &types),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "message", "interrupt", "retry"],
                        "default": "spawn"
                    },
                    "subagent_type": {
                        "type": "string",
                        "enum": type_names,
                        "description": "Which agent type to spawn (default general-purpose)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional: a short (3-5 word) summary of the task shown to the user in the agents panel. If omitted, the UI auto-derives one from the first words of `prompt`."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The complete, self-contained task for the agent: what to investigate or do, all necessary context, and what the final report must contain"
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Run in the background and return immediately; the result is injected into the conversation when ready (default false)"
                    },
                    "system": {
                        "type": "string",
                        "description": "Optional system-prompt override for the sub-agent"
                    },
                    "target_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Target running agent id for message/interrupt/retry"
                    }
                },
                "anyOf": [
                    {"required": ["prompt"]},
                    {"properties": {"action": {"const": "message"}}, "required": ["action", "target_id", "prompt"]},
                    {"properties": {"action": {"enum": ["interrupt", "retry"]}}, "required": ["action", "target_id"]}
                ],
                "additionalProperties": false
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        self.run_impl(input, ctx, None).await
    }

    /// 携带 `tool_use_id`：填入落盘 trace 的 `Started.parent_tool_use_id`，
    /// 供跨会话时把落盘的子 Agent trace 反查回具体是哪一次 Agent 工具调用。
    async fn run_with_meta(
        &self,
        input: Value,
        ctx: &dyn ToolContext,
        meta: &ToolCallMeta,
    ) -> Result<ToolResult> {
        self.run_impl(input, ctx, Some(meta.tool_use_id.clone()))
            .await
    }
}

impl SubAgentTool {
    async fn run_impl(
        &self,
        input: Value,
        ctx: &dyn ToolContext,
        parent_tool_use_id: Option<String>,
    ) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;

        if inp.action != "spawn" {
            return Ok(self.run_control(&inp));
        }
        // `description` 是 UI 增强(agents 面板展示用),`run_impl` 后续会从
        // `prompt` 前缀派生 fallback(line 312-315)。这里只校验 prompt 真正必填,
        // 不要因 description 缺失而拒绝 schema-合法的调用 —— 多 agent 并行场景下
        // 模型经常只填 `prompt`,漏 `description`,把 description 也标记成必填会
        // 直接触发 tool_arguments_invalid,用户体感是"分派失败"。
        if inp.prompt.trim().is_empty() {
            return Ok(ToolResult::err(
                "Agent spawn requires non-empty prompt".to_string(),
            ));
        }
        if self.depth >= self.max_depth {
            return Ok(ToolResult::err(format!(
                "Nested Agent depth limit ({}) reached",
                self.max_depth
            )));
        }

        let type_name = inp.subagent_type.as_deref().unwrap_or("general-purpose");
        let Some(def) = self.find_def(type_name) else {
            return Ok(ToolResult::err(tr_fmt(
                "subagent.unknown_type",
                &[("name", type_name), ("available", &self.available_types())],
            )));
        };

        let mut agent = match (self.factory)(&def) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult::err(tr_fmt(
                    "subagent.create_failed",
                    &[("err", &e.to_string())],
                )))
            }
        };
        agent = match inp.system {
            Some(sys) => {
                let mut a = agent.with_system(sys);
                // `with_system` 是整体替换，工厂追加的语言段被一并抹掉；
                // 覆盖之后补回来，否则这次 spawn 的子 Agent 会退回英文思考。
                if let Some(lang) = self.response_language.as_deref() {
                    a = a.append_system(wyj_core::prompts::sub_agent_language_section(lang));
                }
                a
            }
            None => agent,
        };

        let background = inp.run_in_background.unwrap_or(false);
        let id = self.hub.alloc_id();
        if let Some(session_id) = &self.root_session_id {
            agent.set_session_id(session_id.clone());
        }
        // 嵌套注册：深度已触顶时**不注册** Agent 工具，而不是留一个每次调用
        // 都报 "depth limit reached" 的工具。模型看到工具没了会自己把活干完；
        // 看到工具还在则会换个措辞反复重试——这是 `CLAUDE_CODE_MAX_SUBAGENT_
        // SPAWN_DEPTH` 同款的取舍。
        if self.depth + 1 < self.max_depth.max(1)
            && !def.disallowed_tools.iter().any(|tool| tool == "Agent")
            && def
                .tools
                .as_ref()
                .map_or(true, |tools| tools.iter().any(|tool| tool == "Agent"))
        {
            agent.register_tool(Arc::new(Self {
                defs: self.defs.clone(),
                hub: self.hub.clone(),
                factory: self.factory.clone(),
                root_session_id: self.root_session_id.clone(),
                caller_id: Some(id),
                depth: self.depth + 1,
                max_depth: self.max_depth,
                response_language: self.response_language.clone(),
                sessions_dir: self.sessions_dir.clone(),
                message_log: self.message_log.clone(),
            }));
        }
        let agent_type = def.name.clone();
        let description = inp
            .description
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| wyj_core::textutil::truncate_chars(&inp.prompt, 40, true));

        // 在第一次 await 之前同步发出 Started，保证前端收到的 Started 顺序
        // 与父 Agent 的 ToolStart 顺序一致（FIFO 配对的前提）。
        self.hub.emit(SubAgentEvent::Started {
            id,
            parent_id: self.caller_id,
            agent_type: agent_type.clone(),
            description: description.clone(),
            background,
            parent_tool_use_id,
        });

        // 挂内部事件回调：工具事件与 token 用量汇入 Hub
        let hub_tool = self.hub.clone();
        let hub_usage = self.hub.clone();
        // 工具摘要实时累积：与 trace 的 `from_trace` 共用同一套提取规则
        // （`ToolDigest::record_start` / `record_error`），两条路径不会漂移。
        // 用 Mutex 而非 channel：回调本身是同步的，且这里只有零星几次字段追加。
        // 用 `HandoffProgress` 而不是裸 `ToolDigest`：同一份快照既供正常收尾组装
        // 交接物，也供 `Hub::interrupt` 在 abort 之后补写（那时任务自己的收尾
        // 代码已经不会执行）。
        let progress = Arc::new(std::sync::Mutex::new(
            crate::handoff::HandoffProgress::default(),
        ));
        let digest_for_cb = progress.clone();
        let progress_in_task = progress.clone();
        let agent = agent
            .with_tool_callback(move |ev| match ev {
                ToolEvent::Start { name, input, .. } => {
                    if let Ok(mut d) = digest_for_cb.lock() {
                        d.record_tool_start(&name, &input);
                    }
                    hub_tool.emit(SubAgentEvent::ToolStart {
                        id,
                        tool_name: name,
                        arg_summary: summarize_input(&input),
                        input,
                    })
                }
                ToolEvent::End {
                    name,
                    is_error,
                    elapsed_secs,
                    output,
                    ..
                } => {
                    if is_error {
                        if let Ok(mut d) = digest_for_cb.lock() {
                            d.record_tool_error(&name, &output);
                        }
                    }
                    hub_tool.emit(SubAgentEvent::ToolEnd {
                        id,
                        tool_name: name,
                        is_error,
                        elapsed_secs,
                        output,
                    })
                }
            })
            .with_usage_callback(move |input_tokens, output_tokens| {
                hub_usage.emit(SubAgentEvent::Usage {
                    id,
                    input_tokens,
                    output_tokens,
                })
            });

        // 组装 owned 执行环境后整体 spawn（子 Agent 的一切依赖均 'static）
        let cwd = ctx.cwd().to_path_buf();
        let allowed = ctx.allowed_tools();
        let parent_is_plan = ctx.is_plan_mode();
        // 必须在 spawn 之前取：下面的 `tokio::spawn` 要求捕获的依赖是 'static，
        // 而 `ctx: &dyn ToolContext` 是方法借用。与 allowed / parent_is_plan
        // 同样先取成 owned 值再 move 进闭包。
        let parent_mode = ctx.permission_mode();
        let prompt = inp.prompt;
        let semaphore = self.hub.semaphore();
        let parent_id = self.caller_id;
        let hub_task = self.hub.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<ToolResult>();
        let task_type = agent_type.clone();
        let task_desc = description.clone();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel::<AgentControl>();
        // 用户中途补充的内容：与主 Agent 排空**同一份**队列。没有它，前台子
        // Agent 就是一段完全不透光的等待——用户在它跑的过程中补充的要求既进不了
        // 子 Agent，也进不了被本次工具调用堵住的主 Agent（实测一个 12 分钟的子
        // Agent = 12 分钟白等）。`Option<&SteerQueue>` 是 `'static` 引用，
        // 配合上面 `Arc` 句柄可以直接 move 进 task。
        let steer: Option<Arc<wyj_core::SteerQueue>> = ctx.steer_queue().cloned();
        // 子 Agent 用量回灌通道，与 steer 同理必须先取成 'static 的 Arc。
        // 逐层透传：每层只把自己那层 Session 的 total_* 加进去，各层不相交，
        // 因此嵌套子 Agent 的开销同样能汇总到根（不会双计，见 ToolCtx::sub_usage）。
        let sub_usage_sink: Option<wyj_core::session::SharedSubUsage> =
            ctx.sub_usage_sink().cloned();
        // Auto 判定闸门同样必须先取成 'static 的 Arc 才能 move 进 spawn。
        // 漏掉它的后果见 `sub_tool_ctx`：Auto 模式下子 Agent 会退化成「无闸门 + 无审批
        // UI」，副作用工具全被拒。
        let auto_gate = ctx.auto_gate().cloned();
        // assistant 文本通道：与 tool/usage 回调同源，落 trace 用。
        let hub_assistant = self.hub.clone();
        // 交接物落盘所需的 owned 值（`tokio::spawn` 要求 'static）。
        let sessions_dir = self.sessions_dir.clone();
        let session_id_for_handoff = self.root_session_id.clone();
        let message_log = self.message_log.clone();

        let handle = tokio::spawn(async move {
            let start = Instant::now();
            // 并发上限：超限时在此排队（UI 期间显示为等待中）
            // Root agents count against the global limit. Nested agents are bounded by depth and
            // do not take another permit, preventing a parent-waits-for-child semaphore deadlock.
            let _permit = if parent_id.is_none() {
                semaphore.acquire_owned().await.ok()
            } else {
                None
            };

            let mut session = Session::new();
            // 消息增量落盘：正常结束/ESC 中断的交接物由本任务与 Hub 补写，
            // 但**进程被硬杀**时两者都不会执行。挂上 sink 后，每条消息在产生
            // 时就已进日志，事后能从日志重放出「做到哪了」。
            if let Some(log) = message_log.as_ref() {
                session = session.with_message_sink(log.sink_for(id));
            }
            let mut next_input = Some(vec![ContentBlock::Text { text: prompt }]);

            let sub_ctx = sub_tool_ctx(
                &cwd,
                auto_gate,
                steer.clone(),
                sub_usage_sink.clone(),
                parent_mode,
                allowed,
                parent_is_plan,
            );

            let mut outputs = Vec::new();
            let mut is_error = false;
            let mut interrupted = false;
            // 每轮收尾时的 assistant 文本（未截断）。交接物要用最后一轮，
            // trace 的 `Assistant` 事件要用每一轮。
            let mut last_turn_text = String::new();
            let mut failure_reason: Option<String> = None;
            let mut turn_limited = false;
            while let Some(input) = next_input.take() {
                let retry_input = input.clone();
                session.push_user_with_blocks(input);
                let mut output_buf = String::new();
                let run_res = agent
                    .run_turn_with_user_steerage(
                        &mut session,
                        &sub_ctx,
                        &mut |delta| output_buf.push_str(delta),
                        steer.as_deref(),
                        |_kind| {},
                    )
                    .await;
                // 每轮交出的文本既是 trace 里的「得出什么」，也是交接物的候选结论。
                // 放在 match 之前：失败轮也可能已经产出有价值的中间结论。
                if !output_buf.trim().is_empty() {
                    hub_assistant.emit(SubAgentEvent::Assistant {
                        id,
                        text: output_buf.clone(),
                    });
                    last_turn_text = output_buf.clone();
                    // 同步进共享快照：ESC 中断会 abort 任务，收尾代码不再执行，
                    // 中断交接物只能从这份快照里取结论。
                    if let Ok(mut p) = progress_in_task.lock() {
                        p.record_turn_text(&output_buf);
                    }
                }
                match run_res {
                    Ok(()) if output_buf.is_empty() => outputs.push(tr("subagent.no_output")),
                    Ok(()) => outputs.push(crate::textutil::truncate_head_tail(
                        &output_buf,
                        20_000,
                        10_000,
                    )),
                    Err(error) => {
                        // 轮数触顶与「真出错」是两种终态：前者任务没做完但产出
                        // 有价值，后者可能什么都没做出来。交接物要把它们分开。
                        if is_turn_limit_error(&error) {
                            turn_limited = true;
                        }
                        failure_reason.get_or_insert_with(|| error.to_string());
                        outputs.push(tr_fmt(
                            "subagent.run_failed",
                            &[("err", &error.to_string())],
                        ));
                        is_error = true;
                    }
                }

                // FollowUp/Retry 只在完整模型消息与工具往返结束后消费。它们复用
                // 原 sub_ctx，不能增加工具白名单或写权限。
                while let Ok(control) = control_rx.try_recv() {
                    match control {
                        AgentControl::FollowUp(content) => {
                            next_input.get_or_insert_with(Vec::new).extend(content);
                        }
                        AgentControl::PeerMessage { from_id, content } => {
                            next_input
                                .get_or_insert_with(Vec::new)
                                .push(ContentBlock::Text {
                                    text: format!("<agent-message from=\"a{from_id}\">"),
                                });
                            // 来自另一个子 Agent 的话，同样是模型输出：转义后
                            // 才能保证它伪造不出闭合标签、也冒充不了用户指令。
                            next_input.get_or_insert_with(Vec::new).extend(
                                content.into_iter().map(|block| match block {
                                    ContentBlock::Text { text } => ContentBlock::Text {
                                        text: wyj_core::textutil::escape_harness_markers(&text),
                                    },
                                    other => other,
                                }),
                            );
                            next_input
                                .get_or_insert_with(Vec::new)
                                .push(ContentBlock::Text {
                                    text: "</agent-message>".to_string(),
                                });
                        }
                        AgentControl::RetryLast => {
                            next_input
                                .get_or_insert_with(Vec::new)
                                .extend(retry_input.clone());
                        }
                        AgentControl::Interrupt => {
                            interrupted = true;
                            break;
                        }
                    }
                }
                if interrupted {
                    break;
                }
            }
            // 中断不是「什么都没发生」。旧实现在这里把 outputs 整个丢掉，主 Agent
            // 只收到一句「子 Agent 被中断」——于是「上次做到哪里」这个唯一有交接
            // 价值的信息，恰恰在中断时丢失。现在保留已完成轮的产出，并显式标明
            // 中断发生在哪里。
            let content = if interrupted {
                if outputs.is_empty() {
                    tr("subagent.interrupted")
                } else {
                    format!("{}\n\n{}", outputs.join("\n\n"), tr("subagent.interrupted"))
                }
            } else {
                outputs.join("\n\n")
            };
            is_error |= interrupted;

            // 用量回灌。子 Agent 跑在自己的 `Session` 上，其 token 既不进主会话的
            // `total_*`，也不发 `AgentEvent::Usage`（那只由主 Agent 路径发射）——
            // 不记这一笔，`/cost` 看到的就是一棵被整棵截掉的子树账（实测最高
            // 消耗会话 6 个用户轮次跑 225 次 API 调用，其中大量来自子 Agent）。
            // 只加**本层** Session 的 `total_*`：各层 Session 的计数只覆盖自己
            // 发出的调用，天然不相交，嵌套子 Agent 同样能汇总到根。
            if let Some(sink) = &sub_usage_sink {
                sink.add(wyj_core::session::SubUsage {
                    input: session.total_input_tokens,
                    output: session.total_output_tokens,
                    cache_read: session.total_cache_read_tokens,
                    cache_write: session.total_cache_write_tokens,
                });
            }

            let elapsed_secs = start.elapsed().as_secs_f64();
            let handoff_status = if interrupted {
                crate::handoff::HandoffStatus::Interrupted
            } else if turn_limited {
                crate::handoff::HandoffStatus::TurnLimit
            } else if is_error {
                crate::handoff::HandoffStatus::Failed
            } else {
                crate::handoff::HandoffStatus::Completed
            };
            let handoff = build_handoff(
                id,
                &task_type,
                &task_desc,
                background,
                handoff_status,
                if interrupted {
                    Some(tr("subagent.interrupted"))
                } else {
                    failure_reason.clone()
                },
                &last_turn_text,
                session.messages.iter(),
                elapsed_secs,
            );
            // 工具摘要来自运行期累积的那一份（不是事后从 trace 反推：trace 有
            // 256KB 上限，超限停写后反推会静默丢尾部调用）。
            let mut handoff = handoff;
            if let Ok(p) = progress_in_task.lock() {
                handoff.digest = p.digest.clone();
            }
            // 交接物落盘：尽力而为。写失败只 warn，绝不把一次正常运行判成失败。
            if let (Some(dir), Some(sid)) = (&sessions_dir, &session_id_for_handoff) {
                if let Err(err) = crate::handoff::write_handoff(dir, sid, &handoff) {
                    tracing::warn!(
                        id,
                        error = %err,
                        "子 Agent 交接物落盘失败（不影响本次运行）"
                    );
                }
            }

            // 回填给模型的内容分三档：
            //   * 完成 → `subagent-report` 定界外壳（正文可能含读到的第三方原文）
            //   * 未完成 → `subagent_handoff_body` 外壳，显式给三选一，否则模型
            //     会默认「我接手」，把本该隔离的上下文灌回主对话
            //   * 出错且没有任何产出 → 裸错误文本（本进程自己写的，不含第三方内容）
            let handoff_body = handoff.status.is_incomplete().then(|| {
                wyj_core::prompts::subagent_handoff_body(
                    handoff.status.as_str(),
                    handoff.reason.as_deref(),
                    &content,
                    format_digest(&handoff.digest).as_deref(),
                )
            });

            hub_task.emit(SubAgentEvent::Done {
                id,
                agent_type: task_type,
                description: task_desc,
                result: content.clone(),
                is_error,
                elapsed_secs,
                background,
                handoff_body: handoff_body.clone(),
            });
            hub_task.finish(id);
            let model_content = match (handoff_body, is_error) {
                (Some(body), _) => body,
                (None, true) => content.clone(),
                (None, false) => wyj_core::prompts::subagent_report_body(&content),
            };
            let result = if is_error {
                ToolResult::err(model_content)
            } else {
                ToolResult::ok(model_content)
            };
            let _ = result_tx.send(result);
        });
        self.hub.register_with_handoff(
            id,
            background,
            self.caller_id,
            control_tx,
            handle,
            progress,
            crate::agent_hub::HandoffMeta {
                agent_type: agent_type.clone(),
                description: description.clone(),
                background,
                sessions_dir: self.sessions_dir.clone(),
                session_id: self.root_session_id.clone(),
            },
        );

        if background {
            // 模型侧文本，英文（结果注入通知见 prompts::bg_agent_done_reminder）
            Ok(ToolResult::ok(format!(
                "Background agent a{id} ({agent_type}: {description}) started. Its result will arrive as a system-reminder when done — continue with your current work, do not wait."
            )))
        } else {
            match result_rx.await {
                Ok(r) => Ok(r),
                // 任务被 abort（如用户 ESC 中断）：sender 被丢弃
                Err(_) => Ok(ToolResult::err(tr("subagent.interrupted"))),
            }
        }
    }

    fn run_control(&self, input: &Input) -> ToolResult {
        let Some(target_id) = input.target_id else {
            return ToolResult::err("Agent control action requires target_id".to_string());
        };
        let result = match input.action.as_str() {
            "message" => {
                let Some(from_id) = self.caller_id else {
                    return ToolResult::err(
                        "The root agent should use /agent-control follow-up; peer messaging is for running sub-agents"
                            .to_string(),
                    );
                };
                if input.prompt.trim().is_empty() {
                    return ToolResult::err("Agent message requires prompt".to_string());
                }
                self.hub.send_peer_message(
                    from_id,
                    target_id,
                    vec![ContentBlock::Text {
                        text: input.prompt.clone(),
                    }],
                )
            }
            "interrupt" => self.hub.interrupt(target_id),
            "retry" => self.hub.retry_last(target_id),
            other => return ToolResult::err(format!("Unknown Agent action: {other}")),
        };
        match result {
            crate::agent_hub::AgentControlResult::Accepted => ToolResult::ok(format!(
                "Agent action {} accepted for a{}",
                input.action, target_id
            )),
            other => ToolResult::err(format!(
                "Agent action {} for a{} failed: {:?}",
                input.action, target_id, other
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use wyj_api::provider::{EventStream, Provider};
    use wyj_api::types::{Message, StopReason, StreamEvent};

    #[test]
    fn sub_agent_ctx_inherits_auto_gate() {
        // 回归：子 Agent 的 ctx 曾漏挂 auto_gate，而 Auto 模式又原样继承给子 Agent
        // （见 derive_sub_agent_permission_mode）。于是 ensure_tool_permission 退到
        // 「逐次人工确认」，子 Agent 又恒无 ui_ask_tx → fail-closed → 它的
        // Bash/Write/Edit 100% 被拒，而 Manual 下这些走 AutoApprove 反而全放行。
        use wyj_core::auto_mode::{AutoGate, AutoGateImpl};

        let gate: Arc<dyn AutoGate> = Arc::new(AutoGateImpl::new(Default::default(), None));
        let cwd = std::path::Path::new("/tmp");
        let mut parent = crate::ctx::ToolCtx::new(cwd);
        parent.auto_gate = Some(gate);

        // 调用点做的事：spawn 之前从父 ctx 取成 owned，再传给子 ctx 装配。
        let sub = sub_tool_ctx(
            cwd,
            parent.auto_gate().cloned(),
            None,
            None,
            Some(wyj_core::permission::PermissionMode::Auto),
            None,
            false,
        );
        assert!(
            sub.auto_gate.is_some(),
            "Auto 模式下的子 Agent 必须带上判定闸门，否则副作用工具全被拒"
        );
        assert!(matches!(
            sub.permission_mode(),
            Some(crate::ctx::PermissionMode::Auto)
        ));
    }

    #[test]
    fn definition_schema_accepts_missing_description() {
        // 回归测试:v0.2.24 之前 schema 把 description 标成必填,模型在多 agent
        // 并行场景漏 description 时直接 tool_arguments_invalid,体感"分派失败"。
        // 现在 description 是 optional,prompt 是唯一必填。
        let defs = Arc::new(vec![AgentDefinition {
            name: "general-purpose".to_string(),
            description: "test".to_string(),
            tools: None,
            disallowed_tools: Vec::new(),
            omit_claude_md: false,
            model: None,
            system_prompt: "test".to_string(),
            builtin: true,
            source: None,
        }]);
        let hub = Arc::new(SubAgentHub::new());
        let tool = SubAgentTool::new(defs, hub, |_| -> Result<Agent> {
            panic!("factory not called in this test")
        });
        let td = tool.definition();
        assert_eq!(td.name, "Agent");

        let mut pipeline = wyj_core::tool_arguments::ToolArgumentPipeline::default();
        pipeline.register(&td);

        // (1) 无 description 也能通过 schema
        let call = wyj_api::types::RawToolCall {
            id: "t1".into(),
            name: "Agent".into(),
            raw_arguments: r#"{"prompt": "do the thing"}"#.into(),
        };
        let result = pipeline
            .process(call)
            .expect("schema should accept call with no description");
        assert_eq!(result.input["prompt"], "do the thing");

        // (2) 有 description 仍然 OK
        let call = wyj_api::types::RawToolCall {
            id: "t2".into(),
            name: "Agent".into(),
            raw_arguments: r#"{"description": "do thing", "prompt": "do the thing"}"#.into(),
        };
        let result = pipeline.process(call).expect("schema accepts description");
        assert_eq!(result.input["description"], "do thing");

        // (3) 缺 prompt 仍然失败(必填性保留)
        let call = wyj_api::types::RawToolCall {
            id: "t3".into(),
            name: "Agent".into(),
            raw_arguments: r#"{"description": "do thing"}"#.into(),
        };
        let err = pipeline
            .process(call)
            .expect_err("schema must still require prompt");
        assert_eq!(
            err.kind,
            wyj_core::tool_arguments::ToolArgumentErrorKind::SchemaViolation
        );
    }

    #[test]
    fn summarize_prefers_primary_arg() {
        let v = serde_json::json!({"file_path": "/a/b.rs", "other": 1});
        assert_eq!(summarize_input(&v), "/a/b.rs");
    }

    #[test]
    fn summarize_truncates_long_values() {
        let long = "x".repeat(100);
        let v = serde_json::json!({ "command": long });
        let s = summarize_input(&v);
        assert_eq!(s.chars().count(), 61); // 60 + 省略号
        assert!(s.ends_with('…'));
    }

    struct DelayedFollowUpProvider {
        calls: Arc<AtomicUsize>,
        observed_follow_up: Arc<AtomicBool>,
    }

    struct SpawnChildProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for SpawnChildProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "nested-1".to_string(),
                        name: "Agent".to_string(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "nested-1".to_string(),
                        json_delta: serde_json::json!({
                            "subagent_type": "child",
                            "description": "nested child",
                            "prompt": "finish child"
                        })
                        .to_string(),
                    }),
                    Ok(StreamEvent::ToolUseEnd {
                        id: "nested-1".to_string(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ])))
            } else {
                Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamEvent::TextDelta("outer done".to_string())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ])))
            }
        }
    }

    /// 把子 Agent 实际发出的 system prompt 抓下来的 provider（一次性槽）。
    struct SystemSpyProvider {
        seen: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for SystemSpyProvider {
        async fn stream(
            &self,
            system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            if let Ok(mut slot) = self.seen.lock() {
                if slot.is_none() {
                    *slot = Some(system.combined());
                }
            }
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                },
            )])))
        }
    }

    /// `inp.system` 走的是 `with_system`（整体替换），会把工厂追加的语言段抹掉。
    /// 覆盖之后必须按 `with_response_language` 记下的 locale 补回来——否则模型
    /// 只要在 Agent 调用里带一个 `system` 字段，这次 spawn 就退回英文思考，
    /// 且现象随模型是否恰好传该字段而随机复现。
    #[tokio::test]
    async fn system_override_still_carries_the_configured_response_language() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let provider_seen = seen.clone();
        let hub = Arc::new(SubAgentHub::new());
        let tool = SubAgentTool::new(
            Arc::new(vec![AgentDefinition {
                name: "general-purpose".to_string(),
                description: "test".to_string(),
                tools: None,
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "factory prompt".to_string(),
                builtin: true,
                source: None,
            }]),
            hub,
            move |_| {
                Ok(Agent::new(Arc::new(SystemSpyProvider {
                    seen: provider_seen.clone(),
                })))
            },
        )
        .with_response_language("zh");
        let cwd = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::ToolCtx::new(cwd.path());

        let _ = tool
            .run(
                serde_json::json!({
                    "subagent_type": "general-purpose",
                    "description": "override test",
                    "prompt": "do the thing",
                    "system": "custom system prompt from the model"
                }),
                &ctx,
            )
            .await;

        let system = seen.lock().unwrap().clone().expect("provider 未被调用");
        assert!(
            system.contains("custom system prompt from the model"),
            "system 覆盖应当生效: {system}"
        );
        assert!(
            system.contains("# Response language") && system.contains("Chinese (zh)"),
            "system 覆盖后语言段被抹掉了，子 Agent 会退回英文思考: {system}"
        );
    }

    struct EndProvider {
        saw_agent_tool: Option<Arc<AtomicBool>>,
    }

    #[async_trait::async_trait]
    impl Provider for EndProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            if let Some(observed) = &self.saw_agent_tool {
                observed.store(
                    tools.iter().any(|tool| tool.name == "Agent"),
                    Ordering::SeqCst,
                );
            }
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("child done".to_string())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    #[async_trait::async_trait]
    impl Provider for DelayedFollowUpProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let turn = self.calls.fetch_add(1, Ordering::SeqCst);
            if turn > 0
                && messages.iter().any(|message| {
                    message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Text { text } if text == "more"))
                })
            {
                self.observed_follow_up.store(true, Ordering::SeqCst);
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta(format!("turn-{turn}"))),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    #[tokio::test]
    async fn background_follow_up_runs_on_the_next_safe_model_boundary() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_follow_up = Arc::new(AtomicBool::new(false));
        let provider_calls = calls.clone();
        let provider_observed = observed_follow_up.clone();
        let hub = Arc::new(SubAgentHub::new());
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        hub.set_event_cb(move |event| {
            let _ = event_tx.send(event);
        });
        let tool = SubAgentTool::new(
            Arc::new(vec![AgentDefinition {
                name: "general-purpose".to_string(),
                description: "test".to_string(),
                tools: None,
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "test".to_string(),
                builtin: true,
                source: None,
            }]),
            hub.clone(),
            move |_| {
                Ok(Agent::new(Arc::new(DelayedFollowUpProvider {
                    calls: provider_calls.clone(),
                    observed_follow_up: provider_observed.clone(),
                })))
            },
        );
        let cwd = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::ToolCtx::new(cwd.path());

        let started = tool
            .run(
                serde_json::json!({
                    "subagent_type": "general-purpose",
                    "description": "follow-up test",
                    "prompt": "first",
                    "run_in_background": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!started.is_error);
        assert_eq!(
            hub.send_follow_up(
                1,
                vec![ContentBlock::Text {
                    text: "more".to_string(),
                }],
            ),
            crate::agent_hub::AgentControlResult::Accepted
        );

        let done = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(SubAgentEvent::Done { result, .. }) = event_rx.recv().await {
                    break result;
                }
            }
        })
        .await
        .expect("sub-agent follow-up did not finish");
        assert!(done.contains("turn-0"));
        assert!(done.contains("turn-1"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(observed_follow_up.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn nested_agents_do_not_deadlock_when_all_root_permits_are_occupied() {
        let defs = Arc::new(vec![
            AgentDefinition {
                name: "outer".to_string(),
                description: "spawns a child".to_string(),
                tools: Some(vec!["Agent".to_string()]),
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "outer".to_string(),
                builtin: true,
                source: None,
            },
            AgentDefinition {
                name: "child".to_string(),
                description: "leaf".to_string(),
                tools: Some(Vec::new()),
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "child".to_string(),
                builtin: true,
                source: None,
            },
        ]);
        let hub = Arc::new(SubAgentHub::new());
        let tool = SubAgentTool::new(defs, hub.clone(), move |definition| {
            if definition.name == "outer" {
                Ok(Agent::new(Arc::new(SpawnChildProvider {
                    calls: AtomicUsize::new(0),
                })))
            } else {
                Ok(Agent::new(Arc::new(EndProvider {
                    saw_agent_tool: None,
                })))
            }
        });
        let cwd = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::ToolCtx::new(cwd.path());
        let runs = (0..crate::agent_hub::MAX_CONCURRENT_SUBAGENTS).map(|index| {
            tool.run(
                serde_json::json!({
                    "subagent_type": "outer",
                    "description": format!("outer {index}"),
                    "prompt": "spawn child"
                }),
                &ctx,
            )
        });
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            futures::future::join_all(runs),
        )
        .await
        .expect("nested sub-agents deadlocked while roots held all permits");
        assert!(results
            .into_iter()
            .all(|result| result.is_ok_and(|result| !result.is_error)));
        assert_eq!(hub.background_count(), 0);
    }

    #[tokio::test]
    async fn agent_tool_is_not_injected_when_definition_whitelist_excludes_it() {
        let observed = Arc::new(AtomicBool::new(false));
        let provider_observed = observed.clone();
        let tool = SubAgentTool::new(
            Arc::new(vec![AgentDefinition {
                name: "leaf".to_string(),
                description: "leaf".to_string(),
                tools: Some(vec!["Read".to_string()]),
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "leaf".to_string(),
                builtin: true,
                source: None,
            }]),
            Arc::new(SubAgentHub::new()),
            move |_| {
                Ok(Agent::new(Arc::new(EndProvider {
                    saw_agent_tool: Some(provider_observed.clone()),
                })))
            },
        );
        let cwd = tempfile::tempdir().unwrap();
        let result = tool
            .run(
                serde_json::json!({
                    "subagent_type": "leaf",
                    "description": "leaf task",
                    "prompt": "finish"
                }),
                &crate::ctx::ToolCtx::new(cwd.path()),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(!observed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn sub_agent_episode_reuses_the_root_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let evolution_cfg = wyj_config::EvolutionCfg {
            use_experiences: false,
            ..wyj_config::EvolutionCfg::default()
        };
        let evolution =
            Arc::new(wyj_core::EvolutionStore::new(dir.path(), &repo, evolution_cfg).unwrap());
        let evolution_for_factory = evolution.clone();
        let tool = SubAgentTool::new(
            Arc::new(vec![AgentDefinition {
                name: "general-purpose".to_string(),
                description: "test".to_string(),
                tools: None,
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "test".to_string(),
                builtin: true,
                source: None,
            }]),
            Arc::new(SubAgentHub::new()),
            move |_| {
                Ok(Agent::new(Arc::new(EndProvider {
                    saw_agent_tool: None,
                }))
                .with_evolution(evolution_for_factory.clone()))
            },
        )
        .with_session_id("root-session");

        let result = tool
            .run(
                serde_json::json!({
                    "subagent_type": "general-purpose",
                    "description": "record an episode",
                    "prompt": "finish the delegated task"
                }),
                &crate::ctx::ToolCtx::new(&repo),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let episodes = evolution.list_episodes(10).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].session_id, "root-session");
        assert_eq!(episodes[0].goal_summary, "finish the delegated task");
    }

    #[tokio::test]
    async fn nested_spawn_stops_at_the_configured_depth_limit() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let calls = factory_calls.clone();
        let tool = SubAgentTool {
            defs: Arc::new(std::sync::RwLock::new(vec![AgentDefinition {
                name: "general-purpose".to_string(),
                description: "test".to_string(),
                tools: None,
                disallowed_tools: Vec::new(),
                omit_claude_md: false,
                model: None,
                system_prompt: "test".to_string(),
                builtin: true,
                source: None,
            }])),
            hub: Arc::new(SubAgentHub::new()),
            factory: Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Agent::new(Arc::new(EndProvider {
                    saw_agent_tool: None,
                })))
            }),
            root_session_id: None,
            caller_id: Some(7),
            depth: 3,
            max_depth: 3,
            response_language: None,
            sessions_dir: None,
            message_log: None,
        };
        let cwd = tempfile::tempdir().unwrap();
        let result = tool
            .run(
                serde_json::json!({
                    "description": "too deep",
                    "prompt": "spawn"
                }),
                &crate::ctx::ToolCtx::new(cwd.path()),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("depth limit"));
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
    }

    /// 深度触顶的那一层不该再拿到 `Agent` 工具。
    ///
    /// 留着一个「调了就报 depth limit」的工具，模型会换个措辞反复重试；
    /// 工具直接消失时它才会自己把活干完。判据是 `depth + 1 < max_depth`：
    /// 子 Agent 的 depth 取 `self.depth + 1`，所以要给它留出下一层。
    #[tokio::test]
    async fn agent_tool_disappears_at_the_depth_limit() {
        async fn spawned_child_sees_agent_tool(depth: usize, max_depth: usize) -> bool {
            let observed = Arc::new(AtomicBool::new(false));
            let provider_observed = observed.clone();
            let tool = SubAgentTool {
                defs: Arc::new(std::sync::RwLock::new(vec![AgentDefinition {
                    name: "general-purpose".to_string(),
                    description: "test".to_string(),
                    tools: None,
                    disallowed_tools: Vec::new(),
                    omit_claude_md: false,
                    model: None,
                    system_prompt: "test".to_string(),
                    builtin: true,
                    source: None,
                }])),
                hub: Arc::new(SubAgentHub::new()),
                factory: Arc::new(move |_| {
                    Ok(Agent::new(Arc::new(EndProvider {
                        saw_agent_tool: Some(provider_observed.clone()),
                    })))
                }),
                root_session_id: None,
                caller_id: Some(1),
                depth,
                max_depth,
                response_language: None,
                sessions_dir: None,
                message_log: None,
            };
            let dir = tempfile::tempdir().unwrap();
            let result = tool
                .run(
                    serde_json::json!({"description": "probe", "prompt": "go"}),
                    &crate::ctx::ToolCtx::new(dir.path()),
                )
                .await
                .unwrap();
            assert!(!result.is_error, "探测本身不该失败: {}", result.content);
            observed.load(Ordering::SeqCst)
        }

        // 还有一层余量：子 Agent 应当仍能看到 Agent
        assert!(
            spawned_child_sees_agent_tool(0, 3).await,
            "depth=0 max=3 时子 Agent 还能再派一层"
        );
        assert!(spawned_child_sees_agent_tool(1, 3).await);
        // 触顶：这一层已到最深处，Agent 工具必须消失
        assert!(
            !spawned_child_sees_agent_tool(1, 2).await,
            "触顶后仍暴露 Agent 会让模型反复重试同一个调用"
        );
        // 配置为 1（禁止嵌套）：第一层子 Agent 立刻拿不到 Agent
        assert!(!spawned_child_sees_agent_tool(0, 1).await);
    }

    // ── 子 Agent 权限继承 ───────────────────────────────────────────────
    //
    // 回归背景：`allowed_tools()` 只在白名单语义下返回 Some，对 Prompt /
    // AutoApprove 一律返回 None。子 Agent 过去只据它决定自身模式，于是父级
    // Bypass(AutoApprove) 时子 Agent 落回默认 Prompt；子 Agent 无审批 UI，
    // Prompt + 无 UI 通道在 evaluate 和 confirm_tool 两道关卡都 fail-closed，
    // 导致 Bash/Edit/Write 被全量拒绝 —— 比父级更严，任何继承语义都不可能
    // 要求这样。

    #[test]
    fn sub_agent_inherits_bypass_as_auto_approve() {
        use crate::ctx::PermissionMode;
        // Bypass 父级：`allowed_tools()` 返回 None（与真实 ToolCtx 一致）
        let mode = derive_sub_agent_permission_mode(Some(PermissionMode::AutoApprove), None, false);
        assert!(
            matches!(mode, PermissionMode::AutoApprove),
            "Bypass 必须原样传给子 Agent，否则子 Agent 比主 Agent 更严"
        );
    }

    #[test]
    fn sub_agent_keeps_plan_read_only_narrowing() {
        use crate::ctx::PermissionMode;
        let base: std::collections::HashSet<String> =
            ["Bash", "Read", "Grep", "Write", "Edit", "Agent"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mode = derive_sub_agent_permission_mode(
            Some(PermissionMode::Plan(base.clone())),
            Some(base),
            true,
        );
        let PermissionMode::Plan(set) = mode else {
            panic!("Plan 父级应产出 Plan 子模式")
        };
        assert!(set.contains("Bash") && set.contains("Read") && set.contains("Grep"));
        assert!(
            !set.contains("Write") && !set.contains("Edit") && !set.contains("Agent"),
            "Plan 父级下子 Agent 必须继续只读"
        );
    }

    #[test]
    fn sub_agent_inherits_allowlist_unchanged() {
        use crate::ctx::PermissionMode;
        let base: std::collections::HashSet<String> =
            ["Bash", "Read"].iter().map(|s| s.to_string()).collect();
        let mode = derive_sub_agent_permission_mode(
            Some(PermissionMode::Allowlist(base.clone())),
            Some(base.clone()),
            false,
        );
        let PermissionMode::Allowlist(set) = mode else {
            panic!("Allowlist 父级应产出 Allowlist 子模式")
        };
        assert_eq!(set, base);
    }

    #[test]
    fn sub_agent_without_exposed_parent_mode_falls_back_to_allowlist_derivation() {
        use crate::ctx::PermissionMode;
        // `permission_mode()` 返回 None 的 ToolContext（如旧 mock）应退回
        // 旧的 allowed_tools() 派生逻辑，保证向后兼容。
        let base: std::collections::HashSet<String> =
            ["Bash", "Read"].iter().map(|s| s.to_string()).collect();
        let mode = derive_sub_agent_permission_mode(None, Some(base.clone()), false);
        assert!(matches!(mode, PermissionMode::Allowlist(_)));
        // 既没有模式也没有白名单 → Auto（走闸门，无白名单兜底）
        let mode = derive_sub_agent_permission_mode(None, None, false);
        assert!(matches!(mode, PermissionMode::Auto));
    }

    /// Auto 父级：子 Agent **原样继承**，不降级。
    ///
    /// 降级成 `AutoApprove` 是这里曾经给手动模式的做法（因为子 Agent 没有弹窗
    /// 通道，逐次确认会把 general-purpose 的 Bash/Edit/Write 全废）。合并成单一
    /// auto 之后这条理由依然成立，而且降级会更糟：闸门本来就随 ctx 派生传给子
    /// Agent，降级等于把最容易出事的写/执行动作整个移出分类器视野。
    #[test]
    fn sub_agent_in_auto_mode_inherits_the_gate_instead_of_auto_approve() {
        use crate::ctx::PermissionMode;
        let mode = derive_sub_agent_permission_mode(Some(PermissionMode::Auto), None, false);
        assert!(
            matches!(mode, PermissionMode::Auto),
            "auto 父级下子 Agent 必须继承闸门判定，而不是全放行"
        );
    }

    // ── Agent 工具的审批点 ─────────────────────────────────────────────
    //
    // 放宽子 Agent 内部权限的前提：主 Agent 拉起子 Agent 这一步本身要过审批，
    // 否则「委派」就成了一条绕过整个权限系统的路径。审批粒度按被委派类型的
    // 副作用能力决定 —— 只读类型不打断。

    fn tool_with_builtin_defs() -> SubAgentTool {
        SubAgentTool::new(
            Arc::new(wyj_core::agent_def::builtin_defs()),
            Arc::new(SubAgentHub::new()),
            |_| -> Result<Agent> { panic!("factory not called in this test") },
        )
    }

    #[test]
    fn agent_tool_requires_permission_only_for_side_effecting_types() {
        let tool = tool_with_builtin_defs();
        // general-purpose: tools = None（标准工具集全集，含 Write/Edit/Bash）
        assert!(
            tool.needs_permission(&serde_json::json!({
                "prompt": "改一下代码",
                "subagent_type": "general-purpose"
            })),
            "general-purpose 能拿到 Bash/Edit，必须先问用户"
        );
        // 缺省 subagent_type 也是 general-purpose
        assert!(tool.needs_permission(&serde_json::json!({ "prompt": "改一下代码" })));

        // Explore / Plan: tools = READONLY_TOOLS，不应打断
        for ty in ["Explore", "Plan"] {
            assert!(
                !tool.needs_permission(&serde_json::json!({
                    "prompt": "帮我调研",
                    "subagent_type": ty
                })),
                "{ty} 是只读类型，不应触发审批弹窗"
            );
        }
    }

    #[test]
    fn agent_tool_skips_permission_for_unknown_type() {
        // 解析不到定义时交给 run() 返回「未知类型」，不重复拦一道
        let tool = tool_with_builtin_defs();
        assert!(!tool.needs_permission(&serde_json::json!({
            "prompt": "x",
            "subagent_type": "does-not-exist"
        })));
    }

    #[test]
    fn agent_tool_action_summary_names_type_and_task() {
        let tool = tool_with_builtin_defs();
        // 摘要必须点名类型与任务，便于用户在审批弹窗里判断
        let named = tool.action_summary(&serde_json::json!({
            "prompt": "x", "subagent_type": "general-purpose", "description": "修 bug"
        }));
        assert!(
            named.contains("general-purpose") && named.contains("修 bug"),
            "{named}"
        );
        let bare = tool.action_summary(&serde_json::json!({ "prompt": "x" }));
        assert!(bare.contains("general-purpose"), "{bare}");
        assert_ne!(named, bare, "有 description 时摘要应更具体");
    }

    #[test]
    fn def_can_touch_side_effects_matches_tools_frontmatter() {
        let mk = |name: &str, tools: Option<Vec<String>>| AgentDefinition {
            name: name.to_string(),
            description: "d".to_string(),
            tools,
            disallowed_tools: Vec::new(),
            omit_claude_md: false,
            model: None,
            system_prompt: "s".to_string(),
            builtin: true,
            source: None,
        };
        assert!(def_can_touch_side_effects(&mk("all", None)));
        assert!(!def_can_touch_side_effects(&mk(
            "ro",
            Some(vec!["Read".into(), "Grep".into()])
        )));
        assert!(def_can_touch_side_effects(&mk(
            "wo",
            Some(vec!["Read".into(), "Write".into()])
        )));
        assert!(def_can_touch_side_effects(&mk(
            "sh",
            Some(vec!["Bash".into()])
        )));
    }

    /// `disallowed_tools` 把副作用工具全减光时，spawn 不该再要审批。
    /// 否则用户会被问「要不要放行一个只读 Agent」，而拒绝与否都不改变实际能力。
    #[test]
    fn def_with_only_disallowed_side_effect_tools_needs_no_permission() {
        let def = AgentDefinition {
            name: "read-only".to_string(),
            description: "d".to_string(),
            tools: None,
            disallowed_tools: vec![
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
                "BashOutput".to_string(),
                "KillShell".to_string(),
                "computer".to_string(),
                "app_computer".to_string(),
            ],
            omit_claude_md: false,
            model: None,
            system_prompt: "s".to_string(),
            builtin: false,
            source: None,
        };
        assert!(
            !def_can_touch_side_effects(&def),
            "副作用工具已被黑名单减光，不应再要求审批"
        );
    }

    #[test]
    fn sub_agent_in_bypass_mode_inherits_auto_approve() {
        use crate::ctx::PermissionMode;
        // Bypass 显式全放行时子 Agent 同样全放行（Bypass 不能比父级更严）。
        let mode = derive_sub_agent_permission_mode(Some(PermissionMode::AutoApprove), None, false);
        assert!(matches!(mode, PermissionMode::AutoApprove));
    }

    #[test]
    fn sub_agent_keeps_auto_mode_for_classifier_review() {
        use crate::ctx::PermissionMode;
        // Auto 父级：子 Agent 没有审批 UI，但闸门不需要 UI，且随 ctx 派生传递。
        // 若降成 AutoApprove，子 Agent 里的写/执行动作就完全脱离分类器视野。
        let mode = derive_sub_agent_permission_mode(Some(PermissionMode::Auto), None, false);
        assert!(
            matches!(mode, PermissionMode::Auto),
            "Auto 模式下子 Agent 必须继续过分类器"
        );
    }

    // ── 交接物（中断/触顶时「做到哪了」） ────────────────────────────────

    #[test]
    fn turn_limit_errors_are_recognised() {
        let limit = anyhow::anyhow!(
            "超过最大推理轮数 200。已自动存档到触顶前的状态，可用 `/rewind` 回到工作现场后换个思路继续。"
        );
        assert!(is_turn_limit_error(&limit));
        assert!(!is_turn_limit_error(&anyhow::anyhow!("connection reset")));
    }

    /// 中断必须保留已完成轮的产出。
    ///
    /// 这是「交接」这件事的地基：旧实现在中断时把 `outputs` 整个丢掉，主 Agent
    /// 只收到一句「子 Agent 被中断」——恰恰在最需要「上次做到哪里」的时候把
    /// 唯一有交接价值的信息扔了。
    #[test]
    fn interrupted_run_keeps_completed_turn_outputs() {
        let outputs = ["已确认鉴权走 middleware/auth.rs:88".to_string()];
        let interrupted_line = "子 Agent 已被中断";
        // 与 run 内联逻辑同构：有产出时拼接，无产出时才退回纯中断语。
        let with_output = if outputs.is_empty() {
            interrupted_line.to_string()
        } else {
            format!("{}\n\n{}", outputs.join("\n\n"), interrupted_line)
        };
        assert!(
            with_output.contains("middleware/auth.rs:88"),
            "{with_output}"
        );
        assert!(with_output.contains(interrupted_line));

        let empty: Vec<String> = Vec::new();
        let without_output = if empty.is_empty() {
            interrupted_line.to_string()
        } else {
            format!("{}\n\n{}", empty.join("\n\n"), interrupted_line)
        };
        assert_eq!(without_output, interrupted_line);
    }

    #[test]
    fn handoff_conclusion_falls_back_to_last_assistant_message() {
        use wyj_api::types::Message;
        let messages = [
            Message::user("任务"),
            Message::assistant_text("中间结论 A"),
            Message::assistant_text("最后一轮结论 B"),
        ];
        let handoff = build_handoff(
            3,
            "general-purpose",
            "调查",
            false,
            crate::handoff::HandoffStatus::Interrupted,
            Some("interrupted by user".to_string()),
            // 运行期没来得及记录（例如第一轮就被打断）
            "",
            messages.iter(),
            1.5,
        );
        assert_eq!(handoff.conclusion, "最后一轮结论 B");
        assert!(handoff.status.is_incomplete());
    }

    #[test]
    fn handoff_prefers_live_turn_text_over_message_scan() {
        use wyj_api::types::Message;
        let messages = [Message::assistant_text("旧的")];
        let handoff = build_handoff(
            3,
            "Explore",
            "找定义",
            false,
            crate::handoff::HandoffStatus::TurnLimit,
            None,
            "这一轮交出的结论",
            messages.iter(),
            9.0,
        );
        assert_eq!(handoff.conclusion, "这一轮交出的结论");
    }

    #[test]
    fn digest_rendering_lists_modified_files_before_reads() {
        let digest = crate::handoff::ToolDigest {
            files_modified: vec!["src/a.rs".to_string()],
            files_read: vec!["src/b.rs".to_string()],
            commands: vec!["cargo test".to_string()],
            searches: vec!["fn main".to_string()],
            errors: vec!["Bash: error[E0308]".to_string()],
            total_calls: 4,
        };
        let text = format_digest(&digest).expect("有调用就应有摘要");
        // 改过的文件排最前：它才是「现场状态」里最需要被看见的一行
        assert!(text.starts_with("Files modified: src/a.rs"), "{text}");
        assert!(text.contains("Files read: src/b.rs"));
        assert!(text.contains("Tool calls: 4"));
    }

    #[test]
    fn digest_rendering_is_absent_when_nothing_ran() {
        assert!(
            format_digest(&crate::handoff::ToolDigest::default()).is_none(),
            "一个工具都没调用时不该给模型一段空标题"
        );
    }
}
