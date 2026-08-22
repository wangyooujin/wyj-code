//! Bash 工具 — 在 shell 中执行命令

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

use crate::textutil::truncate_head_tail;

const MAX_OUTPUT: usize = 30_000; // 输出截断阈值（字符）
const TIMEOUT_SECS: u64 = 120;
const HOST_EXECUTION_REASON: &str =
    "该命令请求 GUI/LaunchServices 等宿主 OS 集成；通用 Bash sandbox 不授予这类外部副作用";

pub struct BashTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
    #[serde(default)]
    run_outside_sandbox: bool,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::BASH.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_BASH_COMMAND
                    },
                    "timeout": {
                        "type": "integer",
                        "description": crate::descriptions::FIELD_BASH_TIMEOUT,
                        "minimum": 1,
                        "maximum": 600
                    },
                    "description": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_BASH_DESCRIPTION
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Run in the background and return a shell id immediately; read output later with BashOutput and stop it with KillShell. Use for long-running processes like dev servers or watchers (default false)"
                    },
                    "run_outside_sandbox": {
                        "type": "boolean",
                        "description": "Request a one-shot host execution outside the OS sandbox for operations the sandbox intentionally cannot express, such as launching a desktop application. This requires separate interactive approval, is never persistent, and fails closed in plan mode, headless, scheduled, hook, and sub-agent execution. Do not use it merely to bypass a command failure (default false)."
                    }
                },
                "required": ["command"]
            }),
            native: None,
        }
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    fn action_summary(&self, input: &Value) -> String {
        input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let sandbox_policy = ctx.sandbox_policy();
        let explicit_host_execution =
            inp.run_outside_sandbox && sandbox_policy.mode != wyj_sandbox::SandboxMode::Disabled;
        if explicit_host_execution
            && !ctx
                .confirm_unsandboxed_fallback(&inp.command, HOST_EXECUTION_REASON)
                .await
        {
            return Ok(ToolResult::err(
                "宿主执行未获一次性批准，命令没有运行。不要自动重试；请改用 sandbox 内方案或向用户说明该边界。",
            ));
        }

        // 后台执行：立即返回任务 id，输出经 BashOutput 增量读取
        if inp.run_in_background {
            let manager = crate::bash_session::BashSessionManager::global();
            let result = if explicit_host_execution {
                manager.spawn_unsandboxed(&inp.command, ctx.cwd())
            } else {
                match manager.spawn(&inp.command, ctx.cwd(), &sandbox_policy) {
                    Ok(id) => Ok(id),
                    Err(error)
                        if ctx
                            .confirm_unsandboxed_fallback(&inp.command, &error.to_string())
                            .await =>
                    {
                        manager.spawn_unsandboxed(&inp.command, ctx.cwd())
                    }
                    Err(error) => Err(error),
                }
            };
            return match result {
                Ok(id) => Ok(ToolResult::ok(format!(
                    "Started background shell {id}. Use BashOutput to read its output and KillShell to stop it."
                ))),
                Err(e) => Ok(ToolResult::err(format!("后台启动失败: {e}"))),
            };
        }

        let timeout = inp.timeout.unwrap_or(TIMEOUT_SECS);
        let runner = wyj_sandbox::SandboxRunner::detect();
        let (command, ran_outside_sandbox) = if explicit_host_execution {
            (
                runner.unsandboxed_shell_command(&inp.command, ctx.cwd()),
                true,
            )
        } else {
            match runner.shell_command(&inp.command, ctx.cwd(), &sandbox_policy) {
                Ok(command) => (command, false),
                Err(error)
                    if ctx
                        .confirm_unsandboxed_fallback(&inp.command, &error.to_string())
                        .await =>
                {
                    (
                        runner.unsandboxed_shell_command(&inp.command, ctx.cwd()),
                        true,
                    )
                }
                Err(error) => {
                    return Ok(ToolResult::err(format!("Sandbox 拒绝启动命令：{error}")));
                }
            }
        };
        let mut command = tokio::process::Command::from(command);

        let output =
            tokio::time::timeout(std::time::Duration::from_secs(timeout), command.output()).await;

        match output {
            Err(_) => Ok(ToolResult::err(format!(
                "命令超时（{}s）: {}",
                timeout, inp.command
            ))),
            Ok(Err(e)) => Ok(ToolResult::err(format!("执行失败: {e}"))),
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let mut result = String::new();

                // stdout/stderr 共享同一截断预算：先到先得，避免一路很长时
                // 另一路为空而浪费一半配额。保头 60% + 尾 40%（报错几乎总在尾部）。
                let stderr_budget = if stdout.is_empty() {
                    MAX_OUTPUT
                } else {
                    MAX_OUTPUT.saturating_sub(stdout.len()).max(MAX_OUTPUT / 2)
                };
                let stdout_budget = if stderr.is_empty() {
                    MAX_OUTPUT
                } else {
                    MAX_OUTPUT.saturating_sub(stderr.len()).max(MAX_OUTPUT / 2)
                };
                if !stdout.is_empty() {
                    result.push_str(&truncate_head_tail(
                        &stdout,
                        stdout_budget * 6 / 10,
                        stdout_budget * 4 / 10,
                    ));
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push_str("\n--- stderr ---\n");
                    }
                    result.push_str(&truncate_head_tail(
                        &stderr,
                        stderr_budget * 6 / 10,
                        stderr_budget * 4 / 10,
                    ));
                }
                if result.is_empty() {
                    result.push_str("（无输出）");
                }

                let is_error = !out.status.success();
                if is_error && !ran_outside_sandbox && launchservices_sandbox_denial(&stderr) {
                    result.push_str(
                        "\n\nSandbox 边界：macOS Seatbelt 拒绝了 LaunchServices `lsopen`。\
                         启动桌面 App 属于宿主外部副作用；请仅在用户可交互审批时使用 \
                         `run_outside_sandbox=true` 重试，不要把 `(allow lsopen)` 永久加入通用 profile。",
                    );
                }
                if is_error {
                    result = format!("退出码 {}\n{result}", out.status.code().unwrap_or(-1));
                }
                Ok(ToolResult {
                    content: result,
                    is_error,
                    parts: None,
                })
            }
        }
    }
}

fn launchservices_sandbox_denial(stderr: &str) -> bool {
    cfg!(target_os = "macos")
        && stderr.contains("_LSOpenURLsWithCompletionHandler()")
        && stderr.contains("error -54")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct TestContext {
        cwd: PathBuf,
        approve_host_execution: bool,
    }

    #[async_trait]
    impl ToolContext for TestContext {
        fn cwd(&self) -> &Path {
            &self.cwd
        }

        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }

        async fn confirm_unsandboxed_fallback(&self, _command: &str, reason: &str) -> bool {
            assert!(reason.contains("宿主 OS 集成"));
            self.approve_host_execution
        }

        fn sandbox_policy(&self) -> wyj_sandbox::SandboxPolicy {
            wyj_sandbox::SandboxPolicy::enforced_workspace(&self.cwd)
        }
    }

    #[test]
    fn schema_exposes_one_shot_host_execution_flag() {
        let definition = BashTool.definition();
        let property = &definition.input_schema["properties"]["run_outside_sandbox"];
        assert_eq!(property["type"], "boolean");
        assert!(property["description"]
            .as_str()
            .unwrap()
            .contains("separate interactive approval"));
    }

    #[tokio::test]
    async fn host_execution_fails_closed_without_separate_approval() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext {
            cwd: dir.path().to_path_buf(),
            approve_host_execution: false,
        };
        let result = BashTool
            .run(
                serde_json::json!({
                    "command": "printf should-not-run",
                    "run_outside_sandbox": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("命令没有运行"));
    }

    #[tokio::test]
    async fn host_execution_runs_only_after_separate_approval() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext {
            cwd: dir.path().to_path_buf(),
            approve_host_execution: true,
        };
        let result = BashTool
            .run(
                serde_json::json!({
                    "command": "printf host-ok",
                    "run_outside_sandbox": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, "host-ok");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recognizes_launchservices_permission_denial() {
        assert!(launchservices_sandbox_denial(
            "_LSOpenURLsWithCompletionHandler() failed for the application /Applications/App.app with error -54."
        ));
        assert!(!launchservices_sandbox_denial("Unable to find application"));
    }
}
