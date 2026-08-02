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
            caller_id: None,
            depth: 0,
            max_depth: 3,
        }
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

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "Agent"
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
                        "description": "A short (3-5 word) description of the task, shown to the user"
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
                    {"required": ["description", "prompt"]},
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
        if inp.prompt.trim().is_empty()
            || inp
                .description
                .as_deref()
                .map_or(true, |description| description.is_empty())
        {
            return Ok(ToolResult::err(
                "Agent spawn requires non-empty description and prompt".to_string(),
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
        if def
            .tools
            .as_ref()
            .map_or(true, |tools| tools.iter().any(|tool| tool == "Agent"))
        {
            agent.register_tool(Arc::new(Self {
                defs: self.defs.clone(),
                hub: self.hub.clone(),
                factory: self.factory.clone(),
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
        let parent_sandbox = ctx.sandbox_policy();
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
            sub_ctx.replace_sandbox_policy(parent_sandbox);
            sub_ctx.allow_unsandboxed_fallback(false);
            // 继承父级的工具白名单限制（如 Plan 模式），避免子 Agent 成为绕过限制
            // 的后门；类型定义自身的工具限制已在 factory 注册工具时收窄，交集生效。
            // 子 Agent 没有审批 UI，不存在运行中被外部改权限的场景，因此构造一个
            // 独立的共享句柄（而非复用父 ctx 的 Arc）即可，避免父子间意外共享可变状态。
            if let Some(allowed) = allowed {
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
                    sub_ctx.set_permission_mode(crate::ctx::PermissionMode::Plan(read_only));
                } else {
                    sub_ctx.set_permission_mode(crate::ctx::PermissionMode::Allowlist(allowed));
                }
            }

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
                // 原 sub_ctx，不能增加工具白名单、写根、网络或 sandbox 权限。
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
            _system: &str,
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
            _system: &str,
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
            _system: &str,
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
}
