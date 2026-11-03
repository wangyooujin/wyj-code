//! Progress 工具 — 读写项目级 `.wyj-code/progress.md`。

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use wyj_api::types::ToolDefinition;
use wyj_core::progress::{self, MAX_PROGRESS_BYTES, PROGRESS_REL_PATH};
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct ProgressTool {
    project_root: PathBuf,
}

impl ProgressTool {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl Tool for ProgressTool {
    fn name(&self) -> &str {
        "Progress"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::PROGRESS.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["get", "set"],
                        "description": "get reads the file; set replaces the whole file with `content`"
                    },
                    "content": {
                        "type": "string",
                        "description": format!(
                            "Full markdown body for action=set (max {MAX_PROGRESS_BYTES} bytes). Omit for get."
                        )
                    }
                },
                "required": ["action"]
            }),
            native: None,
        }
    }

    fn parallel_safe(&self) -> bool {
        // get 只读；set 与真实工作工具无文件冲突（专属路径）。允许与 Edit/Bash 同轮。
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| anyhow::anyhow!("invalid Progress input: {e}"))?;
        match parsed.action.as_str() {
            "get" => {
                let path = progress::progress_path(&self.project_root);
                match progress::read_progress(&self.project_root) {
                    Some(body) => Ok(ToolResult::ok(format!(
                        "{PROGRESS_REL_PATH} ({} bytes):\n{body}",
                        body.len()
                    ))),
                    None => Ok(ToolResult::ok(format!(
                        "{PROGRESS_REL_PATH} is empty or missing (looked at {}).",
                        path.display()
                    ))),
                }
            }
            "set" => {
                let content = parsed
                    .content
                    .ok_or_else(|| anyhow::anyhow!("action=set requires `content`"))?;
                progress::write_progress(&self.project_root, &content)
                    .map_err(anyhow::Error::msg)?;
                Ok(ToolResult::ok(format!(
                    "Updated {PROGRESS_REL_PATH} ({} bytes).",
                    content.len()
                )))
            }
            other => Err(anyhow::anyhow!("unknown action `{other}`; use get or set")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ToolCtx;
    use tempfile::tempdir;

    #[tokio::test]
    async fn set_then_get() {
        let dir = tempdir().unwrap();
        let tool = ProgressTool::new(dir.path());
        let ctx = ToolCtx::new(dir.path());
        tool.run(
            serde_json::json!({"action": "set", "content": "## Status\n- done\n"}),
            &ctx,
        )
        .await
        .unwrap();
        let got = tool
            .run(serde_json::json!({"action": "get"}), &ctx)
            .await
            .unwrap();
        assert!(got.content.contains("## Status"));
        assert!(!got.is_error);
    }
}
