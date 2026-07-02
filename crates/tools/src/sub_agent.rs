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
use wyj_api::types::ToolDefinition;
use wyj_core::{
    tool::{Tool, ToolContext, ToolResult},
    Agent, AgentDefinition, Session, ToolEvent,
};
use wyj_i18n::{tr, tr_fmt};

use crate::agent_hub::{SubAgentEvent, SubAgentHub};

/// 按 agent 定义创建子 Agent（持有 provider 和按定义过滤后的工具集）
pub type AgentFactory = Arc<dyn Fn(&AgentDefinition) -> Result<Agent> + Send + Sync>;

pub struct SubAgentTool {
    defs: Arc<Vec<AgentDefinition>>,
    hub: Arc<SubAgentHub>,
    factory: AgentFactory,
}

impl SubAgentTool {
    pub fn new(
        defs: Arc<Vec<AgentDefinition>>,
        hub: Arc<SubAgentHub>,
        factory: impl Fn(&AgentDefinition) -> Result<Agent> + Send + Sync + 'static,
    ) -> Self {
        Self {
            defs,
            hub,
            factory: Arc::new(factory),
        }
    }

    fn find_def(&self, type_name: &str) -> Option<&AgentDefinition> {
        self.defs.iter().find(|d| d.name == type_name)
    }

    fn available_types(&self) -> String {
        self.defs
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Deserialize)]
struct Input {
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
        let types = self
            .defs
            .iter()
            .map(|d| {
                tr_fmt(
                    "subagent.type_line",
                    &[("name", d.name.as_str()), ("description", &d.description)],
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let type_names: Vec<&str> = self.defs.iter().map(|d| d.name.as_str()).collect();
        ToolDefinition {
            name: self.name().to_string(),
            description: tr_fmt("subagent.tool_desc", &[("types", &types)]),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "subagent_type": {
                        "type": "string",
                        "enum": type_names,
                        "description": tr("subagent.schema_type")
                    },
                    "description": {
                        "type": "string",
                        "description": tr("subagent.schema_description")
                    },
                    "prompt": {
                        "type": "string",
                        "description": tr("subagent.schema_prompt")
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": tr("subagent.schema_background")
                    },
                    "system": {
                        "type": "string",
                        "description": tr("subagent.schema_system")
                    }
                },
                "required": ["description", "prompt"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;

        let type_name = inp.subagent_type.as_deref().unwrap_or("general-purpose");
        let Some(def) = self.find_def(type_name) else {
            return Ok(ToolResult::err(tr_fmt(
                "subagent.unknown_type",
                &[("name", type_name), ("available", &self.available_types())],
            )));
        };

        let agent = match (self.factory)(def) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolResult::err(tr_fmt(
                    "subagent.create_failed",
                    &[("err", &e.to_string())],
                )))
            }
        };
        let agent = match inp.system {
            Some(sys) => agent.with_system(sys),
            None => agent,
        };

        let background = inp.run_in_background.unwrap_or(false);
        let id = self.hub.alloc_id();
        let agent_type = def.name.clone();
        let description = inp
            .description
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| truncate_chars(&inp.prompt, 40));

        // 在第一次 await 之前同步发出 Started，保证前端收到的 Started 顺序
        // 与父 Agent 的 ToolStart 顺序一致（FIFO 配对的前提）。
        self.hub.emit(SubAgentEvent::Started {
            id,
            agent_type: agent_type.clone(),
            description: description.clone(),
            background,
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
                }),
                ToolEvent::End {
                    name,
                    is_error,
                    elapsed_secs,
                    ..
                } => hub_tool.emit(SubAgentEvent::ToolEnd {
                    id,
                    tool_name: name,
                    is_error,
                    elapsed_secs,
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
        let allowed = ctx.allowed_tools().cloned();
        let prompt = inp.prompt;
        let semaphore = self.hub.semaphore();
        let hub_task = self.hub.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<ToolResult>();
        let task_type = agent_type.clone();
        let task_desc = description.clone();

        let handle = tokio::spawn(async move {
            let start = Instant::now();
            // 并发上限：超限时在此排队（UI 期间显示为等待中）
            let _permit = semaphore.acquire_owned().await;

            let mut session = Session::new();
            session.push_user(prompt);

            let mut sub_ctx = crate::ctx::ToolCtx::new(&cwd);
            // 继承父级的工具白名单限制（如 Plan 模式），避免子 Agent 成为绕过限制
            // 的后门；类型定义自身的工具限制已在 factory 注册工具时收窄，交集生效。
            if let Some(allowed) = allowed {
                sub_ctx.permission_mode = crate::ctx::PermissionMode::Allowlist(allowed);
            }

            let mut output_buf = String::new();
            let run_res = agent
                .run_turn(&mut session, &sub_ctx, &mut |d| output_buf.push_str(d))
                .await;
            let (content, is_error) = match run_res {
                Ok(()) => {
                    if output_buf.is_empty() {
                        (tr("subagent.no_output"), false)
                    } else {
                        (output_buf, false)
                    }
                }
                Err(e) => (
                    tr_fmt("subagent.run_failed", &[("err", &e.to_string())]),
                    true,
                ),
            };

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
        self.hub.register(id, background, handle);

        if background {
            Ok(ToolResult::ok(tr_fmt(
                "subagent.bg_started",
                &[
                    ("id", &format!("a{id}")),
                    ("type", &agent_type),
                    ("desc", &description),
                ],
            )))
        } else {
            match result_rx.await {
                Ok(r) => Ok(r),
                // 任务被 abort（如用户 ESC 中断）：sender 被丢弃
                Err(_) => Ok(ToolResult::err(tr("subagent.interrupted"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
