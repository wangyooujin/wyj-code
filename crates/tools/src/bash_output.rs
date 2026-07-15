//! BashOutput / KillShell 工具 — 后台 Bash 任务的增量读取与终止

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

use crate::bash_session::{BashSessionManager, JobStatus};

pub struct BashOutputTool;

#[derive(Deserialize)]
struct OutputInput {
    bash_id: String,
    /// 可选正则：只返回匹配的行
    #[serde(default)]
    filter: Option<String>,
}

#[async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "BashOutput"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Reads new output from a background shell started with Bash run_in_background=true. Returns only output produced since the last BashOutput call, plus the shell's status and exit code once it finishes. Poll it while waiting for a long-running task.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "bash_id": {
                        "type": "string",
                        "description": "The background shell id returned by Bash (e.g. \"bash_1\")"
                    },
                    "filter": {
                        "type": "string",
                        "description": "Optional regex; only lines matching it are returned"
                    }
                },
                "required": ["bash_id"]
            }),
            native: None,
        }
    }

    /// 纯读取操作，可与其他只读调用并发
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: OutputInput = serde_json::from_value(input)?;
        let Some(job) = BashSessionManager::global().get(&inp.bash_id) else {
            return Ok(ToolResult::err(format!(
                "后台任务 `{}` 不存在",
                inp.bash_id
            )));
        };

        let (new_output, dropped) = job.read_new();
        let filtered = match &inp.filter {
            Some(pat) => match regex::Regex::new(pat) {
                Ok(re) => new_output
                    .lines()
                    .filter(|l| re.is_match(l))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => return Ok(ToolResult::err(format!("filter 正则无效: {e}"))),
            },
            None => new_output,
        };

        let status_line = match job.status() {
            JobStatus::Running => format!("(running, {:.0}s elapsed)", job.elapsed_secs()),
            JobStatus::Exited(code) => format!("(exited with code {code})"),
        };
        let mut out = String::new();
        if dropped > 0 {
            out.push_str(&format!("[oldest {dropped} bytes dropped by buffer cap]\n"));
        }
        if filtered.is_empty() {
            out.push_str(&format!("(no new output) {status_line}"));
        } else {
            out.push_str(&filtered);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&status_line);
        }
        Ok(ToolResult::ok(out))
    }
}

pub struct KillShellTool;

#[derive(Deserialize)]
struct KillInput {
    shell_id: String,
}

#[async_trait]
impl Tool for KillShellTool {
    fn name(&self) -> &str {
        "KillShell"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Terminates a background shell started with Bash run_in_background=true (SIGTERM to the process group, then SIGKILL after 2s). Idempotent: succeeds if the shell already exited.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "shell_id": {
                        "type": "string",
                        "description": "The background shell id to terminate (e.g. \"bash_1\")"
                    }
                },
                "required": ["shell_id"]
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: KillInput = serde_json::from_value(input)?;
        match BashSessionManager::global().kill(&inp.shell_id).await {
            Ok(true) => Ok(ToolResult::ok(format!("已终止后台任务 {}", inp.shell_id))),
            Ok(false) => Ok(ToolResult::err(format!(
                "后台任务 `{}` 不存在",
                inp.shell_id
            ))),
            Err(e) => Ok(ToolResult::err(format!("终止失败: {e}"))),
        }
    }
}
