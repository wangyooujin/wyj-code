//! Read 工具 — 读取文件内容（含行号、范围读取）

use anyhow::Result;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as base64_engine;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::{ToolDefinition, ToolResultPart};
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 200_000;
/// 图片原始字节上限（Anthropic API 单图 base64 后约 5MB 限制）
const MAX_IMAGE_BYTES: usize = 3_750_000;

pub struct ReadTool {
    tracker: crate::write::ReadTracker,
}

impl ReadTool {
    pub fn new(tracker: crate::write::ReadTracker) -> Self {
        Self { tracker }
    }
}

#[derive(Deserialize)]
struct Input {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::READ.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_FILE_PATH
                    },
                    "offset": {
                        "type": "integer",
                        "description": crate::descriptions::FIELD_READ_OFFSET
                    },
                    "limit": {
                        "type": "integer",
                        "description": crate::descriptions::FIELD_READ_LIMIT
                    }
                },
                "required": ["file_path"]
            }),
            native: None,
        }
    }

    /// Read 是纯只读 I/O 操作，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let path = resolve_path(ctx.cwd(), &inp.file_path);

        if !path.exists() {
            return Ok(ToolResult::err(format!("文件不存在: {}", path.display())));
        }

        // 图片文件：返回 base64
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
        ) {
            let bytes = tokio::fs::read(&path).await?;
            // API 图片上限 5MB（base64 后再膨胀 4/3），超限直接拒绝
            if bytes.len() > MAX_IMAGE_BYTES {
                return Ok(ToolResult::err(format!(
                    "图片过大（{} 字节，上限 {} 字节）",
                    bytes.len(),
                    MAX_IMAGE_BYTES
                )));
            }
            let b64 = base64_engine.encode(&bytes);
            let mime = match ext.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                _ => "image/png",
            };
            // 以真正的 image content block 发给模型（旧实现把 base64 当纯文本
            // 内联：模型看不到图，还平白消耗几十万字符的 token）
            let display = format!("[image: {} KB {mime}]", bytes.len() / 1024);
            return Ok(ToolResult::with_parts(
                display,
                vec![ToolResultPart::Image {
                    media_type: mime.to_string(),
                    data: b64,
                }],
            ));
        }

        let content = tokio::fs::read(&path).await?;
        if content.len() > MAX_BYTES && inp.offset.is_none() && inp.limit.is_none() {
            return Ok(ToolResult::err(format!(
                "文件过大（{} 字节），请指定 offset/limit 分段读取",
                content.len()
            )));
        }

        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let start = inp.offset.unwrap_or(0);
        let count = inp.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
        if start > total {
            return Ok(ToolResult::err(format!(
                "读取范围超出文件：offset {start}（从 0 开始），文件共 {total} 行"
            )));
        }
        let end = start.saturating_add(count).min(total);

        // 只有模型实际请求了文件内的有效范围，才允许后续 Edit/Write 通过“已读”检查。
        // 越界请求虽然完成了底层 I/O，但模型没有看到文件内容，不能视为已读。
        self.tracker.mark_read(&path.to_string_lossy());

        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{}\t{}\n", start + i + 1, line));
        }

        if end < total {
            out.push_str(&format!("\n（共 {total} 行，已显示 {start}–{end}）"));
        }

        Ok(ToolResult::ok(out))
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

    #[tokio::test]
    async fn out_of_range_offset_returns_tool_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CLAUDE.md");
        let content = (1..=152)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&path, content).await.unwrap();

        let tracker = crate::write::ReadTracker::default();
        let tool = ReadTool::new(tracker.clone());
        let result = tool
            .run(
                serde_json::json!({
                    "file_path": path,
                    "offset": 160,
                    "limit": 60
                }),
                &TestContext {
                    cwd: dir.path().to_path_buf(),
                },
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("offset 160"));
        assert!(result.content.contains("152 行"));
        assert!(
            !tracker.has_read(&path.to_string_lossy()),
            "越界请求没有向模型返回文件内容，不应解锁后续 Edit/Write"
        );
    }

    #[tokio::test]
    async fn offset_at_end_of_file_returns_an_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        tokio::fs::write(&path, "one\ntwo\n").await.unwrap();

        let tracker = crate::write::ReadTracker::default();
        let tool = ReadTool::new(tracker.clone());
        let result = tool
            .run(
                serde_json::json!({
                    "file_path": path,
                    "offset": 2,
                    "limit": usize::MAX
                }),
                &TestContext {
                    cwd: dir.path().to_path_buf(),
                },
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.is_empty());
        assert!(tracker.has_read(&path.to_string_lossy()));
    }
}
