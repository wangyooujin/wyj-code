//! Skill 系统：从 Markdown 文件加载可复用 prompt 模板

use crate::registry::{Command, CommandContext, CommandResult};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct SkillCommand {
    skill_name: String,
    skill_description: String,
    prompt_template: String,
    usage_str: String,
}

#[async_trait]
impl Command for SkillCommand {
    fn name(&self) -> &str {
        &self.skill_name
    }
    fn description(&self) -> String {
        self.skill_description.clone()
    }
    fn usage(&self) -> String {
        self.usage_str.clone()
    }

    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let expanded = if self.prompt_template.contains("$ARGUMENTS") {
            self.prompt_template.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.prompt_template.clone()
        } else {
            format!("{}\n\n{}", self.prompt_template, args)
        };
        Ok(CommandResult::RunPrompt(expanded.trim().to_string()))
    }
}

// ─── 内置 skill（嵌入二进制，优先级最低，可被用户文件覆盖）──────────────────

static BUILTIN_SKILLS: &[(&str, &str)] = &[
    (
        "run",
        "Build and run the current project using the appropriate build tool \
(cargo, npm, python, make, etc.). Show all output and errors. \
If it fails, diagnose and fix the issue.\n\n$ARGUMENTS",
    ),
    (
        "review",
        "Review the following code or recent changes for:\n\
- Correctness bugs and edge cases\n\
- Reuse and simplification opportunities\n\
- Performance and efficiency\n\
- Style and conventions\n\n\
Use `git diff` to see what changed. Report findings with file:line references. \
Be concise; focus on the highest-impact findings.\n\n$ARGUMENTS",
    ),
    (
        "fix",
        "Fix the following issue in the codebase:\n\n$ARGUMENTS\n\n\
Identify the root cause first, apply a minimal targeted fix, then verify it works.",
    ),
    (
        "explain",
        "Explain the following code or concept in detail:\n\n$ARGUMENTS\n\n\
Cover how it works, why it is designed this way, \
the data flow, and any important caveats or edge cases.",
    ),
    (
        "commit",
        "Review the current changes with `git diff` and `git status`, then:\n\
1. Write a clear, conventional commit message (type: subject)\n\
2. Run: git add -A && git commit -m \"<message>\"\n\n\
Follow the project's existing commit style and keep the message concise.\n\n\
$ARGUMENTS",
    ),
];

// ─── Markdown 解析 ────────────────────────────────────────────────────────────

/// 跳过开头的 YAML frontmatter 块（`---\n...\n---\n`）。社区里不少 Claude Code
/// 命令文件（如 marketplace 里拉取的第三方仓库）会在正文前带一段 `model:` 等
/// 元数据头，不跳过的话会被当成正文混进 prompt 模板。没有 frontmatter 时原样返回。
fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return content;
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.trim() == "---" {
            return &rest[offset + line.len()..];
        }
        offset += line.len();
    }
    content
}

fn parse_skill_file(name: &str, content: &str) -> SkillCommand {
    let content = strip_frontmatter(content);
    let mut description = String::new();
    let mut template_lines: Vec<&str> = Vec::new();
    let mut h1_found = false;

    for line in content.lines() {
        if !h1_found {
            if let Some(title) = line.strip_prefix("# ") {
                description = title.trim().to_string();
                h1_found = true;
                continue;
            }
        }
        template_lines.push(line);
    }

    if !h1_found {
        description = name.to_string();
        template_lines = content.lines().collect();
    }

    SkillCommand {
        skill_name: name.to_string(),
        skill_description: description,
        prompt_template: template_lines.join("\n").trim().to_string(),
        usage_str: format!("/{name} [$ARGUMENTS]"),
    }
}

/// 从单个文件读取并插入（覆盖式，同名直接替换）。
fn load_file_overwrite(path: &Path, skills: &mut HashMap<String, SkillCommand>) {
    let Some(name) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
    else {
        return;
    };
    match std::fs::read_to_string(path) {
        Ok(content) => {
            skills.insert(name.clone(), parse_skill_file(&name, &content));
        }
        Err(e) => tracing::warn!("读取 skill 文件失败 {}: {e}", path.display()),
    }
}

fn load_from_dir(dir: &Path, skills: &mut HashMap<String, SkillCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::warn!("读取 skill 目录失败: {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        load_file_overwrite(&path, skills);
    }
}

/// 同时支持文件路径与目录路径（插件的 `skills`/`commands` 字段可能指向单个
/// `.md` 文件而不是整个目录）。覆盖式写入（同名直接替换），供全局/项目目录使用。
fn load_from_path_overwrite(path: &Path, skills: &mut HashMap<String, SkillCommand>) {
    if path.is_dir() {
        load_from_dir(path, skills);
    } else if path.is_file() {
        load_file_overwrite(path, skills);
    }
}

/// 同上但为"仅当名字未被占用时才插入"语义（先到先得），供插件贡献路径使用，
/// 冲突时记录警告而不是覆盖。
fn load_from_path_if_absent(
    path: &Path,
    plugin_label: &str,
    skills: &mut HashMap<String, SkillCommand>,
) {
    let mut staged: HashMap<String, SkillCommand> = HashMap::new();
    load_from_path_overwrite(path, &mut staged);
    for (name, cmd) in staged {
        if skills.contains_key(&name) {
            tracing::warn!("插件 '{plugin_label}' 的 skill '{name}' 与已有资源同名，已跳过");
            continue;
        }
        skills.insert(name, cmd);
    }
}

/// 加载所有 skill：内置 → 全局 `~/.wyj-code/skills/*.md`（覆盖内置）→ 已启用插件
/// 贡献路径（按安装顺序，先到先得，跳过并警告同名冲突）→ 项目
/// `.wyj/skills/*.md`（最高优先级，覆盖一切，包括插件）。
/// `disabled` 由上层调用方传入(汇总全局+项目 lockfile 里 `enabled == false` 的
/// skill 名)，用于过滤掉被 /skills 面板禁用的条目。
pub fn load_skills(
    home: &Path,
    cwd: &Path,
    disabled: &HashSet<String>,
    plugin_skill_sources: &[PathBuf],
) -> Vec<Arc<dyn Command>> {
    let mut skills: HashMap<String, SkillCommand> = HashMap::new();

    // 1. 内置 skill（优先级最低）
    for &(name, template) in BUILTIN_SKILLS {
        skills.insert(
            name.to_string(),
            SkillCommand {
                skill_name: name.to_string(),
                skill_description: wyj_i18n::tr(&format!("skill.{name}.desc")),
                prompt_template: template.to_string(),
                usage_str: format!("/{name} [$ARGUMENTS]"),
            },
        );
    }

    // 2. 全局用户 skill：~/.wyj-code/skills/*.md（覆盖内置，这是既有的"用户手动
    // 覆盖内置"能力，不属于插件冲突场景）
    let global_dir = home.join(".wyj-code").join("skills");
    if global_dir.exists() {
        load_from_dir(&global_dir, &mut skills);
    }

    // 3. 已启用插件贡献路径（按安装顺序，先到先得，跳过并警告同名冲突）
    for path in plugin_skill_sources {
        load_from_path_if_absent(path, "plugin", &mut skills);
    }

    // 4. 项目 skill：.wyj/skills/*.md（最高优先级，覆盖一切，包括插件）
    let project_dir = cwd.join(".wyj").join("skills");
    if project_dir.exists() {
        load_from_dir(&project_dir, &mut skills);
    }

    skills
        .into_values()
        .filter(|s| !disabled.contains(&s.skill_name))
        .map(|s| Arc::new(s) as Arc<dyn Command>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_frontmatter_before_parsing_title() {
        let content = "---\nmodel: claude-sonnet-4-0\n---\n\n# Automated Documentation Generation\n\n生成文档。\n\n$ARGUMENTS";
        let skill = parse_skill_file("doc-generate", content);
        assert_eq!(
            skill.skill_description,
            "Automated Documentation Generation"
        );
        assert!(!skill.prompt_template.contains("model: claude-sonnet-4-0"));
        assert!(!skill.prompt_template.contains("---"));
        assert!(skill.prompt_template.contains("生成文档"));
    }

    #[test]
    fn no_frontmatter_parses_unchanged() {
        let content = "# 测试 skill\n打印 Hello。";
        let skill = parse_skill_file("hello", content);
        assert_eq!(skill.skill_description, "测试 skill");
        assert_eq!(skill.prompt_template, "打印 Hello。");
    }

    #[test]
    fn no_title_falls_back_to_name_and_keeps_full_body() {
        let content = "只是一段说明文字，没有标题。";
        let skill = parse_skill_file("plain", content);
        assert_eq!(skill.skill_description, "plain");
        assert_eq!(skill.prompt_template, "只是一段说明文字，没有标题。");
    }

    fn names(cmds: &[Arc<dyn Command>]) -> HashSet<String> {
        cmds.iter().map(|c| c.name().to_string()).collect()
    }

    #[test]
    fn load_skills_supports_plugin_single_file_and_directory_sources() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let plugin_root = tempfile::tempdir().unwrap();

        std::fs::write(plugin_root.path().join("single.md"), "# Single\nhi").unwrap();
        let plugin_skills_dir = plugin_root.path().join("skills");
        std::fs::create_dir_all(&plugin_skills_dir).unwrap();
        std::fs::write(plugin_skills_dir.join("dir-one.md"), "# Dir One\nhi").unwrap();

        let sources = vec![plugin_root.path().join("single.md"), plugin_skills_dir];
        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &sources);
        let found = names(&cmds);
        assert!(found.contains("single"));
        assert!(found.contains("dir-one"));
    }

    #[test]
    fn load_skills_plugin_conflict_with_global_is_skipped_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let global_dir = home.path().join(".wyj-code").join("skills");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("review.md"), "# User Review\nuser version").unwrap();

        let plugin_root = tempfile::tempdir().unwrap();
        std::fs::write(
            plugin_root.path().join("review.md"),
            "# Plugin Review\nplugin version",
        )
        .unwrap();

        let sources = vec![plugin_root.path().join("review.md")];
        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &sources);
        let review = cmds.iter().find(|c| c.name() == "review").unwrap();
        assert_eq!(review.description(), "User Review"); // 先到先得，插件被跳过
    }

    #[test]
    fn load_skills_project_dir_still_overrides_plugin_contribution() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let project_dir = cwd.path().join(".wyj").join("skills");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("custom.md"),
            "# Project Override\nproject version",
        )
        .unwrap();

        let plugin_root = tempfile::tempdir().unwrap();
        std::fs::write(
            plugin_root.path().join("custom.md"),
            "# Plugin Custom\nplugin version",
        )
        .unwrap();

        let sources = vec![plugin_root.path().join("custom.md")];
        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &sources);
        let custom = cmds.iter().find(|c| c.name() == "custom").unwrap();
        assert_eq!(custom.description(), "Project Override"); // 项目级仍能覆盖插件
    }
}
