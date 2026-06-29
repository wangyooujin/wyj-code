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
            description: "按 glob 模式搜索文件路径。\
                自动忽略 .gitignore 中的文件。\
                支持 **、? 等通配符。返回匹配文件的绝对路径列表。".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "glob 匹配模式，如 **/*.rs、src/**/*.ts"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索根目录（默认为当前工作目录）"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let root = match &inp.path {
            Some(p) => {
                let pb = std::path::Path::new(p);
                if pb.is_absolute() { pb.to_path_buf() } else { ctx.cwd().join(pb) }
            }
            None => ctx.cwd().to_path_buf(),
        };

        let glob = Glob::new(&inp.pattern)?.compile_matcher();

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

        if matches.is_empty() {
            return Ok(ToolResult::ok("未找到匹配文件"));
        }

        matches.sort();
        let mut out = matches.join("\n");
        if matches.len() == MAX_RESULTS {
            out.push_str(&format!("\n（结果已截断，仅显示前 {MAX_RESULTS} 个）"));
        }
        Ok(ToolResult::ok(out))
    }
}
