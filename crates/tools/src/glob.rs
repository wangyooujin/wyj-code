//! Glob 工具 — 文件路径匹配

use anyhow::Result;
use async_trait::async_trait;
use globset::Glob;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_RESULTS: usize = 1000;

pub struct GlobTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::GLOB.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_GLOB_PATTERN
                    },
                    "path": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_GLOB_PATH
                    }
                },
                "required": ["pattern"]
            }),
            native: None,
        }
    }

    /// Glob 是纯只读文件搜索，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let root = match &inp.path {
            Some(p) => {
                let pb = std::path::Path::new(p);
                if pb.is_absolute() {
                    pb.to_path_buf()
                } else {
                    ctx.cwd().join(pb)
                }
            }
            None => ctx.cwd().to_path_buf(),
        };

        let glob = Glob::new(&inp.pattern)?.compile_matcher();

        // 将同步目录遍历放到 spawn_blocking 里执行，避免阻塞 tokio 异步运行时。
        let matches = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut matches = vec![];
            let walker = WalkBuilder::new(&root)
                .hidden(false)
                .ignore(true)
                .git_ignore(true)
                .build();

            for entry in walker {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let rel = path.strip_prefix(&root).unwrap_or(path);
                    if glob.is_match(rel) {
                        matches.push(path.display().to_string());
                        if matches.len() >= MAX_RESULTS {
                            break;
                        }
                    }
                }
            }
            Ok(matches)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Glob 任务执行失败: {e}"))??;

        if matches.is_empty() {
            return Ok(ToolResult::ok("未找到匹配文件"));
        }

        let mut matches = matches;
        matches.sort();
        let mut out = matches.join("\n");
        if matches.len() == MAX_RESULTS {
            out.push_str(&format!("\n（结果已截断，仅显示前 {MAX_RESULTS} 个）"));
        }
        Ok(ToolResult::ok(out))
    }
}
