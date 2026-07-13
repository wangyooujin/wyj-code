//! Edit 工具 — 精确字符串替换

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

use crate::diff::make_diff;

/// diff 输出行数上限（含上下文/增删行），避免超长 diff 撑爆输出
const MAX_DIFF_LINES: usize = 200;

pub struct EditTool {
    tracker: crate::write::ReadTracker,
}

impl EditTool {
    pub fn new(tracker: crate::write::ReadTracker) -> Self {
        Self { tracker }
    }
}

#[derive(Deserialize)]
struct Input {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::EDIT.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_FILE_PATH
                    },
                    "old_string": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_EDIT_OLD
                    },
                    "new_string": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_EDIT_NEW
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": crate::descriptions::FIELD_EDIT_REPLACE_ALL,
                        "default": false
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    fn action_summary(&self, input: &Value) -> String {
        input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let path = resolve_path(ctx.cwd(), &inp.file_path);

        if !path.exists() {
            return Ok(ToolResult::err(format!("文件不存在: {}", path.display())));
        }

        // 与 Write 相同的安全约束：编辑前必须先 Read 过该文件
        let path_str = path.to_string_lossy().to_string();
        if !self.tracker.has_read(&path_str) {
            return Ok(ToolResult::err(format!(
                "安全检查失败：编辑 `{}` 前必须先用 Read 读取该文件",
                path.display()
            )));
        }

        let content = tokio::fs::read_to_string(&path).await?;

        let count = content.matches(&inp.old_string as &str).count();
        if count == 0 {
            return Ok(ToolResult::err(
                "未找到目标字符串。请确认内容与文件精确匹配（含缩进、换行）。".to_string(),
            ));
        }
        if count > 1 && !inp.replace_all {
            return Ok(ToolResult::err(format!(
                "目标字符串出现了 {count} 次，需唯一匹配。\
                请提供更多上下文，或设置 replace_all=true。"
            )));
        }

        let new_content = if inp.replace_all {
            content.replace(&inp.old_string as &str, &inp.new_string)
        } else {
            content.replacen(&inp.old_string as &str, &inp.new_string, 1)
        };

        tokio::fs::write(&path, new_content.as_bytes()).await?;

        let replaced = if inp.replace_all { count } else { 1 };
        let diff = make_diff(&content, &new_content, MAX_DIFF_LINES);
        Ok(ToolResult::ok(format!(
            "已替换 {replaced} 处: {}\n{diff}",
            path.display()
        )))
    }
}

fn resolve_path(cwd: &std::path::Path, p: &str) -> std::path::PathBuf {
    let pb = std::path::Path::new(p);
    if pb.is_absolute() {
        pb.to_path_buf()
    } else {
        cwd.join(pb)
    }
}
