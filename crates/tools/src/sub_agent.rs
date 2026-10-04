//! 子 Agent 工具 — 按类型定义派生独立的嵌套 Agent 完成复杂子任务
//!
//! 每个子 Agent 整体 `tokio::spawn` 为独立任务并登记进 [`SubAgentHub`]：
//! 前台调用等待任务结果返回；`run_in_background: true` 时立即返回、任务跨轮次
//! 继续运行，完成结果由前端（TUI/headless）通过 Hub 的 Done 事件投递。

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
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
        }
    }

    /// Bind sub-agent Episodes to the root conversation. All descendants reuse
    /// this id so several sub-agents from one user session do not masquerade as
    /// independent sessions when Evolution evaluates promotion thresholds.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.root_session_id = Some(session_id.into());
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
            return truncate_chars(s, 60);
        }
    }
    truncate_chars(&input.to_string(), 60)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
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
    match &def.tools {
        None => true,
        Some(list) => list
            .iter()
            .any(|name| SIDE_EFFECT_TOOLS.contains(&name.as_str())),
    }
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
        // Normal(Prompt)：子 Agent 没有、也不应该有再弹窗的审批通道，而
        // 「委派这个子任务」本身已经在父级做过一次审批 —— `SubAgentTool::
        // needs_permission` 按被委派类型是否有副作用决定要不要问用户
        // （见 `def_can_touch_side_effects`）。这次委派即覆盖子 Agent 内部
        // 的写/执行类工具。
        //
        // 若这里退回 Prompt，`Prompt` + 无 UI 通道会在 `evaluate`（副作用
        // 工具 + 非交互 surface → `interactive_approval_unavailable`）和
        // `confirm_tool`（无 `ui_ask_tx` → false）两处 fail-closed，把
        // general-purpose 子 Agent 的 Bash/Edit/Write **全部废掉**，比父级
        // 更严 —— 任何"继承"语义都不可能要求这样。
        Some(PermissionMode::Prompt) => PermissionMode::AutoApprove,
        _ => match allowed {
            None => PermissionMode::Prompt,
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
            Some(sys) => agent.with_system(sys),
            None => agent,
        };

        let background = inp.run_in_background.unwrap_or(false);
        let id = self.hub.alloc_id();
        if let Some(session_id) = &self.root_session_id {
            agent.set_session_id(session_id.clone());
        }
        if def
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
            }));
        }
        let agent_type = def.name.clone();
        let description = inp
            .description
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| truncate_chars(&inp.prompt, 40));

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
        let agent = agent
            .with_tool_callback(move |ev| match ev {
                ToolEvent::Start { name, input, .. } => hub_tool.emit(SubAgentEvent::ToolStart {
                    id,
                    tool_name: name,
                    arg_summary: summarize_input(&input),
                    input,
                }),
                ToolEvent::End {
                    name,
                    is_error,
                    elapsed_secs,
                    output,
                    ..
                } => hub_tool.emit(SubAgentEvent::ToolEnd {
                    id,
                    tool_name: name,
                    is_error,
                    elapsed_secs,
                    output,
                }),
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
            let mut next_input = Some(vec![ContentBlock::Text { text: prompt }]);

            let sub_ctx = crate::ctx::ToolCtx::new(&cwd);
            sub_ctx.set_execution_surface(wyj_core::ExecutionSurface::SubAgent);
            // 继承父级的工具白名单限制（如 Plan 模式），避免子 Agent 成为绕过限制
            // 的后门；类型定义自身的工具限制已在 factory 注册工具时收窄，交集生效。
            // 子 Agent 没有审批 UI，不存在运行中被外部改权限的场景，因此构造一个
            // 独立的共享句柄（而非复用父 ctx 的 Arc）即可，避免父子间意外共享可变状态。
            sub_ctx.set_permission_mode(derive_sub_agent_permission_mode(
                parent_mode,
                allowed,
                parent_is_plan,
            ));

            let mut outputs = Vec::new();
            let mut is_error = false;
            let mut interrupted = false;
            while let Some(input) = next_input.take() {
                let retry_input = input.clone();
                session.push_user_with_blocks(input);
                let mut output_buf = String::new();
                let run_res = agent
                    .run_turn(&mut session, &sub_ctx, &mut |delta| {
                        output_buf.push_str(delta)
                    })
                    .await;
                match run_res {
                    Ok(()) if output_buf.is_empty() => outputs.push(tr("subagent.no_output")),
                    Ok(()) => outputs.push(crate::textutil::truncate_head_tail(
                        &output_buf,
                        20_000,
                        10_000,
                    )),
                    Err(error) => {
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
                            next_input.get_or_insert_with(Vec::new).extend(content);
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
            let content = if interrupted {
                tr("subagent.interrupted")
            } else {
                outputs.join("\n\n")
            };
            is_error |= interrupted;

            hub_task.emit(SubAgentEvent::Done {
                id,
                agent_type: task_type,
                description: task_desc,
                result: content.clone(),
                is_error,
                elapsed_secs: start.elapsed().as_secs_f64(),
                background,
            });
            hub_task.finish(id);
            let result = if is_error {
                ToolResult::err(content)
            } else {
                ToolResult::ok(content)
            };
            let _ = result_tx.send(result);
        });
        self.hub
            .register(id, background, self.caller_id, control_tx, handle);

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
    fn definition_schema_accepts_missing_description() {
        // 回归测试:v1.5.15 之前 schema 把 description 标成必填,模型在多 agent
        // 并行场景漏 description 时直接 tool_arguments_invalid,体感"分派失败"。
        // 现在 description 是 optional,prompt 是唯一必填。
        let defs = Arc::new(vec![AgentDefinition {
            name: "general-purpose".to_string(),
            description: "test".to_string(),
            tools: None,
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
                model: None,
                system_prompt: "outer".to_string(),
                builtin: true,
                source: None,
            },
            AgentDefinition {
                name: "child".to_string(),
                description: "leaf".to_string(),
                tools: Some(Vec::new()),
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
        // 既没有模式也没有白名单 → Prompt（维持现状）
        let mode = derive_sub_agent_permission_mode(None, None, false);
        assert!(matches!(mode, PermissionMode::Prompt));
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

    #[test]
    fn sub_agent_in_normal_prompt_mode_inherits_allow() {
        use crate::ctx::PermissionMode;
        // Normal 模式：委派已在父级审批过，子 Agent 不应再被 fail-closed 全量拒绝
        let mode = derive_sub_agent_permission_mode(Some(PermissionMode::Prompt), None, false);
        assert!(
            matches!(mode, PermissionMode::AutoApprove),
            "Normal 模式下子 Agent 应继承放行，否则 Bash/Edit/Write 全废"
        );
    }
}
