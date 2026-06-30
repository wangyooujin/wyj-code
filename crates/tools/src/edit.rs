//! Edit 工具 — 精确字符串替换

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct EditTool;

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
            description: "对文件进行精确的字符串替换。\
                old_string 必须在文件中唯一出现（除非 replace_all=true）。\
                替换前必须先用 Read 读取文件确认精确的缩进和内容。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "要编辑的文件绝对路径"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "要替换的原始字符串（必须与文件内容精确匹配，含缩进）"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "替换后的新字符串"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "是否替换所有出现（默认 false：仅允许出现一次）",
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

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let path = resolve_path(ctx.cwd(), &inp.file_path);

        if !path.exists() {
            return Ok(ToolResult::err(format!("文件不存在: {}", path.display())));
        }

        let content = tokio::fs::read_to_string(&path).await?;

        let count = content.matches(&inp.old_string as &str).count();
        if count == 0 {
            return Ok(ToolResult::err(format!(
                "未找到目标字符串。请确认内容与文件精确匹配（含缩进、换行）。"
            )));
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
        Ok(ToolResult::ok(format!(
            "已替换 {replaced} 处: {}",
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
