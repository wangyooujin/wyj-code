//! Grep 工具 — 正则内容搜索（三种输出模式 + 上下文行）

use anyhow::Result;
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

const MAX_MATCHES: usize = 500;
/// content 模式默认输出行数上限
const DEFAULT_CONTENT_LIMIT: usize = 200;
/// files_with_matches 模式默认文件数上限
const DEFAULT_FILES_LIMIT: usize = 100;

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
    /// "content" | "files_with_matches"（默认）| "count"
    #[serde(default)]
    output_mode: Option<String>,
    /// 匹配行后附带的行数（仅 content 模式）
    #[serde(default, rename = "-A")]
    after: Option<usize>,
    /// 匹配行前附带的行数（仅 content 模式）
    #[serde(default, rename = "-B")]
    before: Option<usize>,
    /// 匹配行前后各附带的行数（仅 content 模式；被 -A/-B 覆盖）
    #[serde(default, rename = "-C")]
    context: Option<usize>,
    /// 输出条数上限（content=行数，files=文件数，count=文件数）
    #[serde(default)]
    head_limit: Option<usize>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: crate::descriptions::GREP.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_GREP_PATTERN
                    },
                    "path": {
                        "type": "string",
                        "description": crate::descriptions::FIELD_GREP_PATH
                    },
                    "include": {
                        "type": "string",
                        "description": "Only search files matching this glob, e.g. *.rs or *.{ts,tsx}"
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Case-sensitive matching (default true)"
                    },
                    "output_mode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "content: matching lines with line numbers (supports -A/-B/-C); files_with_matches: only file paths (default); count: match count per file"
                    },
                    "-A": {
                        "type": "integer",
                        "description": "Lines to show after each match (content mode only)"
                    },
                    "-B": {
                        "type": "integer",
                        "description": "Lines to show before each match (content mode only)"
                    },
                    "-C": {
                        "type": "integer",
                        "description": "Lines to show before and after each match (content mode only)"
                    },
                    "head_limit": {
                        "type": "integer",
                        "description": "Cap the output at the first N lines/files (default: 200 lines for content, 100 files otherwise)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    /// Grep 是纯只读内容搜索，无副作用，可安全并发执行。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let inp: Input = serde_json::from_value(input)?;
        let case_sensitive = inp.case_sensitive.unwrap_or(true);
        let re = regex_build(&inp.pattern, case_sensitive)?;

        let mode = match inp.output_mode.as_deref() {
            Some("content") => OutputMode::Content,
            Some("count") => OutputMode::Count,
            // 默认对齐 Claude Code：只列文件路径，需要行内容时显式选 content
            Some("files_with_matches") | None => OutputMode::Files,
            Some(other) => {
                return Ok(ToolResult::err(format!(
                    "未知 output_mode `{other}`（可选 content / files_with_matches / count）"
                )))
            }
        };
        let after = inp.after.or(inp.context).unwrap_or(0);
        let before = inp.before.or(inp.context).unwrap_or(0);
        let head_limit = inp.head_limit.unwrap_or(match mode {
            OutputMode::Content => DEFAULT_CONTENT_LIMIT,
            _ => DEFAULT_FILES_LIMIT,
        });

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

        let include_glob = inp
            .include
            .as_deref()
            .and_then(|g| globset::Glob::new(g).ok().map(|g| g.compile_matcher()));

        // 将同步文件遍历 + 正则搜索放到 spawn_blocking 里执行，避免阻塞 tokio 异步运行时。
        let (lines, truncated) = tokio::task::spawn_blocking(move || -> (Vec<String>, bool) {
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
                            e.path().file_name().map(|n| g.is_match(n)).unwrap_or(false)
                        })
                    })
                    .map(|e| e.path().to_path_buf())
                    .collect()
            };

            let mut out: Vec<String> = vec![];
            let mut truncated = false;
            let mut total_matches = 0usize;

            'outer: for file in targets {
                let content = match std::fs::read(&file) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if content.contains(&0u8) {
                    continue; // 跳过二进制文件
                }
                let text = String::from_utf8_lossy(&content);

                match mode {
                    OutputMode::Files => {
                        if text.lines().any(|l| re.is_match(l)) {
                            out.push(file.display().to_string());
                            if out.len() >= head_limit {
                                truncated = true;
                                break 'outer;
                            }
                        }
                    }
                    OutputMode::Count => {
                        let n = text.lines().filter(|l| re.is_match(l)).count();
                        if n > 0 {
                            out.push(format!("{}:{n}", file.display()));
                            if out.len() >= head_limit {
                                truncated = true;
                                break 'outer;
                            }
                        }
                    }
                    OutputMode::Content => {
                        // before 环形缓冲 + after 倒计时；上下文行用 `path-line-text`
                        // 分隔（rg 惯例），匹配行用 `path:line:text`
                        let mut before_buf: VecDeque<(usize, &str)> =
                            VecDeque::with_capacity(before + 1);
                        let mut after_remaining = 0usize;
                        let mut last_printed: Option<usize> = None;
                        for (idx, line) in text.lines().enumerate() {
                            let lineno = idx + 1;
                            if re.is_match(line) {
                                total_matches += 1;
                                // 补 before 上下文（跳过已输出过的行）
                                for (bn, bl) in before_buf.iter() {
                                    if last_printed.map_or(true, |lp| *bn > lp) {
                                        out.push(format!("{}-{}-{}", file.display(), bn, bl));
                                    }
                                }
                                before_buf.clear();
                                out.push(format!("{}:{}:{}", file.display(), lineno, line));
                                last_printed = Some(lineno);
                                after_remaining = after;
                                if out.len() >= head_limit || total_matches >= MAX_MATCHES {
                                    truncated = true;
                                    break 'outer;
                                }
                            } else if after_remaining > 0 {
                                out.push(format!("{}-{}-{}", file.display(), lineno, line));
                                last_printed = Some(lineno);
                                after_remaining -= 1;
                                if out.len() >= head_limit {
                                    truncated = true;
                                    break 'outer;
                                }
                            } else if before > 0 {
                                if before_buf.len() == before {
                                    before_buf.pop_front();
                                }
                                before_buf.push_back((lineno, line));
                            }
                        }
                    }
                }
            }
            (out, truncated)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Grep 任务执行失败: {e}"))?;

        if lines.is_empty() {
            return Ok(ToolResult::ok("未找到匹配内容"));
        }
        let mut out = lines.join("\n");
        if truncated {
            out.push_str(&format!("\n（已截断，仅显示前 {head_limit} 条）"));
        }
        Ok(ToolResult::ok(out))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum OutputMode {
    Content,
    Files,
    Count,
}

fn regex_build(pattern: &str, case_sensitive: bool) -> Result<regex::Regex> {
    let re = if case_sensitive {
        regex::Regex::new(pattern)?
    } else {
        regex::Regex::new(&format!("(?i){pattern}"))?
    };
    Ok(re)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ToolCtx;
    use serde_json::json;

    async fn run_grep(input: Value, dir: &std::path::Path) -> String {
        let ctx = ToolCtx::new(dir);
        GrepTool.run(input, &ctx).await.unwrap().content
    }

    struct FixtureDir(std::path::PathBuf);
    impl FixtureDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_dir() -> FixtureDir {
        let dir = std::env::temp_dir().join(format!(
            "wyj-grep-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.txt"),
            "line one\nMATCH here\nline three\nline four\nMATCH again\nline six\n",
        )
        .unwrap();
        std::fs::write(dir.join("b.txt"), "nothing\nto see\n").unwrap();
        FixtureDir(dir)
    }

    #[tokio::test]
    async fn default_mode_lists_files_only() {
        let dir = fixture_dir();
        let out = run_grep(json!({"pattern": "MATCH"}), dir.path()).await;
        assert!(out.contains("a.txt"));
        assert!(!out.contains("b.txt"));
        assert!(!out.contains("MATCH here"), "默认模式不应输出行内容");
    }

    #[tokio::test]
    async fn content_mode_with_context_lines() {
        let dir = fixture_dir();
        let out = run_grep(
            json!({"pattern": "MATCH here", "output_mode": "content", "-C": 1}),
            dir.path(),
        )
        .await;
        assert!(out.contains(":2:MATCH here"), "匹配行带 path:line: 前缀");
        assert!(out.contains("-1-line one"), "before 上下文用 - 分隔");
        assert!(out.contains("-3-line three"), "after 上下文用 - 分隔");
    }

    #[tokio::test]
    async fn count_mode_reports_per_file_counts() {
        let dir = fixture_dir();
        let out = run_grep(
            json!({"pattern": "MATCH", "output_mode": "count"}),
            dir.path(),
        )
        .await;
        assert!(out.contains("a.txt:2"));
    }

    #[tokio::test]
    async fn head_limit_truncates() {
        let dir = fixture_dir();
        let out = run_grep(
            json!({"pattern": "line", "output_mode": "content", "head_limit": 2}),
            dir.path(),
        )
        .await;
        assert!(out.contains("已截断"));
    }
}
