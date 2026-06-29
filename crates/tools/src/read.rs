//! Read 工具 — 读取文件内容（含行号、范围读取）

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 200_000;

pub struct ReadTool;

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
            description: "读取文件内容，附带行号前缀。支持偏移量和行数限制，适合读取大文件的特定部分。\
                对图片文件返回 base64 编码。".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "要读取的文件绝对路径"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "从第几行开始读取（0-indexed）"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最多读取行数"
                    }
                },
                "required": ["file_path"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let path = resolve_path(ctx.cwd(), &inp.file_path);

        if !path.exists() {
            return Ok(ToolResult::err(format!("文件不存在: {}", path.display())));
        }

        // 图片文件：返回 base64
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp") {
            let bytes = tokio::fs::read(&path).await?;
            let b64 = base64_encode(&bytes);
            let mime = match ext.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                _ => "image/png",
            };
            return Ok(ToolResult::ok(format!("data:{mime};base64,{b64}")));
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
        let end = (start + count).min(total);

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
    if pb.is_absolute() { pb.to_path_buf() } else { cwd.join(pb) }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
        out.push(TABLE[(b0 >> 2)] as char);
        out.push(TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 { TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[b2 & 0x3f] as char } else { '=' });
    }
    out
}
