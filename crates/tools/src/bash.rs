//! Bash 工具 — 在 shell 中执行命令

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_OUTPUT: usize = 30_000; // 输出截断阈值（字符）
const TIMEOUT_SECS: u64 = 120;

pub struct BashTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "在系统 shell 中执行命令，并返回 stdout/stderr 输出。\
                适合运行构建命令、测试、文件操作、查看日志等。\
                长时间运行的命令会在超时后终止。".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "超时秒数（默认 120）",
                        "minimum": 1,
                        "maximum": 600
                    },
                    "description": {
                        "type": "string",
                        "description": "命令的简短描述，用于在 UI 中展示"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let timeout = inp.timeout.unwrap_or(TIMEOUT_SECS);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            Command::new("bash")
                .arg("-c")
                .arg(&inp.command)
                .current_dir(ctx.cwd())
                .output(),
        )
        .await;

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

                if !stdout.is_empty() {
                    result.push_str(&truncate(&stdout, MAX_OUTPUT / 2));
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push_str("\n--- stderr ---\n");
                    }
                    result.push_str(&truncate(&stderr, MAX_OUTPUT / 2));
                }
                if result.is_empty() {
                    result.push_str("（无输出）");
                }

                let is_error = !out.status.success();
                if is_error {
                    result = format!("退出码 {}\n{result}", out.status.code().unwrap_or(-1));
                }
                Ok(ToolResult { content: result, is_error })
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!(
        "{}…（已截断，原长 {} 字节）",
        &s[..max],
        s.len()
    )
}
