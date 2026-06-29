//! Grep 工具 — 正则内容搜索

use anyhow::Result;
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_MATCHES: usize = 500;

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "在文件内容中搜索正则表达式匹配。\
                返回匹配行及其行号。自动忽略二进制文件和 .gitignore 的内容。".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "正则表达式模式"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索目录或文件路径（默认当前目录）"
                    },
                    "include": {
                        "type": "string",
                        "description": "只搜索匹配此 glob 的文件，如 *.rs、*.{ts,tsx}"
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "是否大小写敏感（默认 true）"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let case_sensitive = inp.case_sensitive.unwrap_or(true);

        let re = regex_build(&inp.pattern, case_sensitive)?;

        let root = match &inp.path {
            Some(p) => {
                let pb = std::path::Path::new(p);
                if pb.is_absolute() { pb.to_path_buf() } else { ctx.cwd().join(pb) }
            }
            None => ctx.cwd().to_path_buf(),
        };

        let include_glob = inp.include.as_deref().map(|g| {
            globset::Glob::new(g).ok().map(|g| g.compile_matcher())
        }).flatten();

        let mut results = vec![];
        let mut truncated = false;

        let targets: Vec<_> = if root.is_file() {
            vec![root.clone()]
        } else {
            WalkBuilder::new(&root)
                .hidden(false)
                .ignore(true)
                .git_ignore(true)
                .build()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter(|e| {
                    include_glob.as_ref().map_or(true, |g| {
                        e.path().file_name()
                            .map(|n| g.is_match(n))
                            .unwrap_or(false)
                    })
                })
                .map(|e| e.path().to_path_buf())
                .collect()
        };

        'outer: for file in targets {
            let content = match std::fs::read(&file) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // 跳过二进制文件
            if content.contains(&0u8) {
                continue;
            }
            let text = String::from_utf8_lossy(&content);
            for (lineno, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    results.push(format!("{}:{}:{}", file.display(), lineno + 1, line));
                    if results.len() >= MAX_MATCHES {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }

        if results.is_empty() {
            return Ok(ToolResult::ok("未找到匹配内容"));
        }

        let mut out = results.join("\n");
        if truncated {
            out.push_str(&format!("\n（已截断，仅显示前 {MAX_MATCHES} 个匹配）"));
        }
        Ok(ToolResult::ok(out))
    }
}

fn regex_build(pattern: &str, case_sensitive: bool) -> Result<regex::Regex> {
    let re = if case_sensitive {
        regex::Regex::new(pattern)?
    } else {
        regex::Regex::new(&format!("(?i){pattern}"))?
    };
    Ok(re)
}
