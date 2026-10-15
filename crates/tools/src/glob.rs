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
                .git_global(true)
                .git_exclude(true)
                // 按仓库位置动态决定，而不是写死 false：非 git 仓库里也让
                // .gitignore 生效（工具描述里承诺的是「Respects .gitignore」，
                // 之前这句话在非 git 目录不成立：被项目显式排除的 `.venv/`、
                // 数据目录照样被遍历）。
                //
                // 而在 git 仓库/worktree 里**必须**为 true，否则 `.git/info/exclude`
                // 失效：`ignore` crate 解析 worktree 的 common git dir（`.git` 是
                // `gitdir:` 指针文件）走的是 `require_git == true` 的分支，写死 false
                // 会让它去读 `<worktree>/.git/info/exclude`——那是个文件，一条规则都
                // 加载不到。managed worktree 里的表现是：主仓库 exclude 掉的目录被遍历。
                .require_git(wyj_config::gitignore::is_inside_git_repo(&root))
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
