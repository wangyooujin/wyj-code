//! Write 工具 — 写入文件（覆盖）

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

use crate::diff::{make_added, make_diff};

/// diff 输出行数上限（含上下文/增删行），避免超长 diff 撑爆输出
const MAX_DIFF_LINES: usize = 200;

/// 跟踪已读取的文件路径（写前须已 Read）
#[derive(Clone, Default)]
pub struct ReadTracker(Arc<Mutex<HashSet<String>>>);

impl ReadTracker {
    pub fn mark_read(&self, path: &str) {
        self.0.lock().unwrap().insert(path.to_string());
    }
    pub fn has_read(&self, path: &str) -> bool {
        // 新文件（不存在）无需先读
        !std::path::Path::new(path).exists() || self.0.lock().unwrap().contains(path)
    }
}

pub struct WriteTool {
    tracker: ReadTracker,
}

impl Default for WriteTool {
    fn default() -> Self {
        Self {
            tracker: ReadTracker::default(),
        }
    }
}

impl WriteTool {
    #[allow(dead_code)]
    pub fn tracker(&self) -> ReadTracker {
        self.tracker.clone()
    }
}

#[derive(Deserialize)]
struct Input {
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "将内容写入文件（覆盖现有内容）。\
                写入已存在的文件前，必须先用 Read 读取该文件。\
                父目录不存在时会自动创建。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "目标文件绝对路径"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的完整内容"
                    }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let path = resolve_path(ctx.cwd(), &inp.file_path);
        let path_str = path.to_string_lossy().to_string();

        if !self.tracker.has_read(&path_str) {
            return Ok(ToolResult::err(format!(
                "安全检查失败：写入 `{}` 前必须先用 Read 读取该文件",
                path.display()
            )));
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // 写入前读取旧内容（若存在），用于生成 diff
        let old_content = tokio::fs::read_to_string(&path).await.ok();
        tokio::fs::write(&path, inp.content.as_bytes()).await?;
        self.tracker.mark_read(&path_str);

        let detail = match &old_content {
            Some(old) => make_diff(old, &inp.content, MAX_DIFF_LINES),
            None => make_added(&inp.content, MAX_DIFF_LINES),
        };
        Ok(ToolResult::ok(format!(
            "已写入: {}\n{detail}",
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
