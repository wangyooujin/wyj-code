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

pub struct BashTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
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

        // 后台执行：立即返回任务 id，输出经 BashOutput 增量读取
        if inp.run_in_background {
            let manager = crate::bash_session::BashSessionManager::global();
            return match manager.spawn(&inp.command, ctx.cwd()) {
                Ok(id) => Ok(ToolResult::ok(format!(
                    "Started background shell {id}. Use BashOutput to read its output and KillShell to stop it."
                ))),
                Err(e) => Ok(ToolResult::err(format!("后台启动失败: {e}"))),
            };
        }

        let timeout = inp.timeout.unwrap_or(TIMEOUT_SECS);
        let mut command = tokio::process::Command::new("/bin/bash");
        command
            .arg("-c")
            .arg(&inp.command)
            .current_dir(ctx.cwd())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct TestContext {
        cwd: PathBuf,
    }

    #[async_trait]
    impl ToolContext for TestContext {
        fn cwd(&self) -> &Path {
            &self.cwd
        }

        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }
    }

    #[test]
    fn schema_exposes_background_flag() {
        let definition = BashTool.definition();
        let property = &definition.input_schema["properties"]["run_in_background"];
        assert_eq!(property["type"], "boolean");
        assert!(property["description"]
            .as_str()
            .unwrap()
            .contains("background"));
    }

    #[tokio::test]
    async fn bash_runs_command_and_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext {
            cwd: dir.path().to_path_buf(),
        };
        let result = BashTool
            .run(
                serde_json::json!({"command": "printf hello-from-bash"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello-from-bash"));
    }

    #[tokio::test]
    async fn bash_reports_exit_code_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext {
            cwd: dir.path().to_path_buf(),
        };
        let result = BashTool
            .run(serde_json::json!({"command": "exit 7"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("退出码 7"));
    }

    #[tokio::test]
    async fn background_spawn_returns_shell_id() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TestContext {
            cwd: dir.path().to_path_buf(),
        };
        let result = BashTool
            .run(
                serde_json::json!({
                    "command": "sleep 1",
                    "run_in_background": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert!(result.content.contains("Started background shell"));
        crate::bash_session::BashSessionManager::global().kill_all();
    }
}
