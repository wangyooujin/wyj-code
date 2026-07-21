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
    /// `argument-hint` frontmatter 字段，仅影响 `usage_str` 展示，不改变执行逻辑。
    #[allow(dead_code)]
    argument_hint: Option<String>,
    /// `allowed-tools` frontmatter 字段：该命令执行期间临时收紧的工具白名单。
    allowed_tools: Option<Vec<String>>,
    /// `model` frontmatter 字段：引用的 Profile 名；TUI scoped execution 会在本轮使用它。
    #[allow(dead_code)]
    model: Option<String>,
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
    fn is_dynamic(&self) -> bool {
        true
    }

    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let expanded = if self.prompt_template.contains("$ARGUMENTS") {
            self.prompt_template.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.prompt_template.clone()
        } else {
            format!("{}\n\n{}", self.prompt_template, args)
        };
        let text = expanded.trim().to_string();
        if self.allowed_tools.is_some() {
            Ok(CommandResult::RunPromptScoped {
                text,
                allowed_tools: self.allowed_tools.clone(),
                profile: self.model.clone(),
            })
        } else {
            Ok(CommandResult::RunPrompt(text))
        }
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

/// 解析 skill / 自定义命令文件的 frontmatter（复用 `wyj_core::frontmatter::parse`，
/// 与 `agent_def.rs` 共用同一套轻量 `key: value` 解析器，不新增依赖），识别
/// `description`/`argument-hint`/`allowed-tools`/`model` 四个结构化字段，未识别字段
/// 静默忽略。无 frontmatter、或 frontmatter 未提供 `description` 时，回退到既有的
/// "扫描正文首个 `# ` 标题行" 逻辑，保证无 frontmatter 的现有内置/用户 skill 文件行为
/// 零变化。
fn parse_skill_file(name: &str, content: &str) -> SkillCommand {
    let (fields, body) = wyj_core::frontmatter::parse(content);

    let mut description_override: Option<String> = None;
    let mut argument_hint: Option<String> = None;
    let mut allowed_tools: Option<Vec<String>> = None;
    let mut model: Option<String> = None;

    for (key, value) in fields {
        match key.as_str() {
            "description" => {
                if !value.is_empty() {
                    description_override = Some(value);
                }
            }
            "argument-hint" => {
                if !value.is_empty() {
                    argument_hint = Some(value);
                }
            }
            "allowed-tools" => {
                let list: Vec<String> = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !list.is_empty() {
                    allowed_tools = Some(list);
                }
            }
            "model" => {
                if !value.is_empty() {
                    model = Some(value);
                }
            }
            // 未识别字段（如真实 Claude Code 的其他 frontmatter key）静默忽略
            _ => {}
        }
    }

    let (skill_description, prompt_template) = match description_override {
        // frontmatter 显式提供 description：正文原样使用，不再剥离 "# 标题" 行
        Some(desc) => (desc, body.trim().to_string()),
        // 未提供：回退到既有逻辑（向后兼容，保证无 frontmatter 的文件行为不变）
        None => {
            let mut template_lines: Vec<&str> = Vec::new();
            let mut h1_found = false;
            let mut title = String::new();
            for line in body.lines() {
                if !h1_found {
                    if let Some(t) = line.strip_prefix("# ") {
                        title = t.trim().to_string();
                        h1_found = true;
                        continue;
                    }
                }
                template_lines.push(line);
            }
            if h1_found {
                (title, template_lines.join("\n").trim().to_string())
            } else {
                (name.to_string(), body.trim().to_string())
            }
        }
    };

    let usage_str = match &argument_hint {
        Some(hint) => format!("/{name} {hint}"),
        None => format!("/{name} [$ARGUMENTS]"),
    };

    SkillCommand {
        skill_name: name.to_string(),
        skill_description,
        prompt_template,
        usage_str,
        argument_hint,
        allowed_tools,
        model,
    }
}

/// 从单个文件读取并插入（覆盖式，同名直接替换）。
fn load_file_overwrite_as(path: &Path, name: String, skills: &mut HashMap<String, SkillCommand>) {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            skills.insert(name.clone(), parse_skill_file(&name, &content));
        }
        Err(e) => tracing::warn!("读取 skill 文件失败 {}: {e}", path.display()),
    }
}

fn load_file_overwrite(path: &Path, skills: &mut HashMap<String, SkillCommand>) {
    let Some(name) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
    else {
        return;
    };
    load_file_overwrite_as(path, name, skills);
}

fn load_from_dir_with_namespace(
    dir: &Path,
    namespace: Option<&str>,
    skills: &mut HashMap<String, SkillCommand>,
) {
    // 标准目录式 Skill：`<name>/SKILL.md`。目录名（含上层 namespace）就是
    // 命令名；该目录内部的 references/assets/*.md 是 Skill 私有资源，不应
    // 被递归误注册成额外 slash command。
    if let Some(name) = namespace {
        let entrypoint = dir.join("SKILL.md");
        if entrypoint.is_file() {
            load_file_overwrite_as(&entrypoint, name.to_string(), skills);
            return;
        }
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::warn!("读取 skill 目录失败: {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let Some(segment) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let next_namespace = namespace
                .map(|ns| format!("{ns}:{segment}"))
                .unwrap_or_else(|| segment.to_string());
            load_from_dir_with_namespace(&path, Some(&next_namespace), skills);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = namespace
                .map(|ns| format!("{ns}:{stem}"))
                .unwrap_or_else(|| stem.to_string());
            load_file_overwrite_as(&path, name, skills);
        }
    }
}

fn load_from_dir(dir: &Path, skills: &mut HashMap<String, SkillCommand>) {
    load_from_dir_with_namespace(dir, None, skills);
}

/// 同时支持文件路径与目录路径（插件的 `skills`/`commands` 字段可能指向单个
/// `.md` 文件而不是整个目录）。覆盖式写入（同名直接替换），供全局/项目目录使用。
fn load_from_path_overwrite(path: &Path, skills: &mut HashMap<String, SkillCommand>) {
    if path.is_dir() {
        let entrypoint = path.join("SKILL.md");
        if entrypoint.is_file() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                load_file_overwrite_as(&entrypoint, name.to_string(), skills);
            }
        } else {
            load_from_dir(path, skills);
        }
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

/// 加载所有 skill / 自定义命令，六层合并链（同作用域内，真实 Claude Code 路径覆盖
/// wyj-code 自造路径——用户从真 CC 迁移过来的命令应该权威，与项目一贯"越贴近真 CC
/// 语义越优先"的哲学一致）：
///
/// 1. 内置（最低）
/// 2. 全局 wyj-code：`~/.wyj-code/skills/*.md`
/// 3. 全局真 CC：`~/.claude/commands/*.md`（覆盖 #2 同名条目）
/// 4. 已启用插件贡献路径（先到先得，跳过并警告同名冲突，不变）
/// 5. 项目 wyj-code：`<git-root>/.wyj-code/skills`（支持 `name.md` 与
///    `name/SKILL.md`）
/// 6. 项目真 CC：`.claude/commands/*.md`（覆盖 #1-#5 同名条目，最高优先级）
///
/// 递归扫描 `*.md`，子目录映射为 Claude Code 风格的 `namespace:name` 命令名。
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
                argument_hint: None,
                allowed_tools: None,
                model: None,
            },
        );
    }

    // 2. 全局用户 skill：~/.wyj-code/skills/*.md（覆盖内置，这是既有的"用户手动
    // 覆盖内置"能力，不属于插件冲突场景）
    let global_wyj_dir = wyj_config::global_config_dir_in(home).join("skills");
    if global_wyj_dir.exists() {
        load_from_dir(&global_wyj_dir, &mut skills);
    }

    // 3. 全局真 CC 自定义命令：~/.claude/commands/*.md（覆盖 #2 同名条目）
    let global_claude_dir = home.join(".claude").join("commands");
    if global_claude_dir.exists() {
        load_from_dir(&global_claude_dir, &mut skills);
    }

    // 4. 已启用插件贡献路径（按安装顺序，先到先得，跳过并警告同名冲突）
    for path in plugin_skill_sources {
        load_from_path_if_absent(path, "plugin", &mut skills);
    }

    // 5. 项目 Skill：<git-root>/.wyj-code/skills（单文件或目录式，覆盖 #1-#4）
    let project_wyj_dir = wyj_config::project_config_dir(cwd).join("skills");
    if project_wyj_dir.exists() {
        load_from_dir(&project_wyj_dir, &mut skills);
    }

    // 6. 项目真 CC 自定义命令：.claude/commands/*.md（覆盖以上全部，最高优先级）
    let project_claude_dir = cwd.join(".claude").join("commands");
    if project_claude_dir.exists() {
        load_from_dir(&project_claude_dir, &mut skills);
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

    #[test]
    fn description_field_overrides_h1_title_and_is_not_stripped_from_body() {
        let content = "---\ndescription: 显式描述\n---\n# 这行不再被剥离\n正文";
        let skill = parse_skill_file("x", content);
        assert_eq!(skill.skill_description, "显式描述");
        assert!(skill.prompt_template.contains("# 这行不再被剥离"));
        assert!(skill.prompt_template.contains("正文"));
    }

    #[test]
    fn allowed_tools_split_by_comma_and_trimmed() {
        let content = "---\nallowed-tools: Read, Grep,Glob\n---\nbody";
        let skill = parse_skill_file("x", content);
        assert_eq!(
            skill.allowed_tools.as_deref(),
            Some(&["Read".to_string(), "Grep".to_string(), "Glob".to_string()][..])
        );
    }

    #[test]
    fn argument_hint_changes_usage_string() {
        let content = "---\nargument-hint: <issue-number>\n---\nbody";
        let skill = parse_skill_file("fix-issue", content);
        assert_eq!(skill.usage_str, "/fix-issue <issue-number>");
    }

    #[test]
    fn no_argument_hint_keeps_default_usage_string() {
        let content = "body without frontmatter";
        let skill = parse_skill_file("plain", content);
        assert_eq!(skill.usage_str, "/plain [$ARGUMENTS]");
    }

    #[test]
    fn unknown_frontmatter_fields_are_silently_ignored() {
        let content = "---\ncolor: red\nfoo: bar\n---\nbody";
        let skill = parse_skill_file("x", content);
        assert_eq!(skill.skill_description, "x"); // 无 description/标题，回退文件名
        assert_eq!(skill.prompt_template, "body");
        assert!(skill.allowed_tools.is_none());
        assert!(skill.model.is_none());
    }

    #[test]
    fn no_frontmatter_leaves_new_fields_none() {
        let content = "# 标题\n正文";
        let skill = parse_skill_file("x", content);
        assert!(skill.argument_hint.is_none());
        assert!(skill.allowed_tools.is_none());
        assert!(skill.model.is_none());
    }

    #[test]
    fn model_field_parsed_and_stored() {
        let content = "---\nmodel: haiku-profile\n---\nbody";
        let skill = parse_skill_file("x", content);
        assert_eq!(skill.model.as_deref(), Some("haiku-profile"));
    }

    #[tokio::test]
    async fn model_field_is_forwarded_to_scoped_execution() {
        let content = "---\nmodel: haiku-profile\nallowed-tools: Read\n---\nbody";
        let skill = parse_skill_file("x", content);
        let result = skill
            .run(
                "",
                &CommandContext {
                    cwd: PathBuf::new(),
                    model: String::new(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    context_window: 0,
                    estimated_tokens: 0,
                    home_dir: PathBuf::new(),
                    sub_input_tokens: 0,
                    sub_output_tokens: 0,
                    effective_mcp_count: 0,
                    plugin_agent_paths: vec![],
                    hooks_enabled: false,
                    dynamic_commands: vec![],
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(result, CommandResult::RunPromptScoped { profile: Some(ref p), .. } if p == "haiku-profile")
        );
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
        let standard_skill_dir = plugin_root.path().join("standard-skill");
        std::fs::create_dir_all(&standard_skill_dir).unwrap();
        std::fs::write(standard_skill_dir.join("SKILL.md"), "# Standard Skill\nhi").unwrap();

        let sources = vec![
            plugin_root.path().join("single.md"),
            plugin_skills_dir,
            standard_skill_dir,
        ];
        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &sources);
        let found = names(&cmds);
        assert!(found.contains("single"));
        assert!(found.contains("dir-one"));
        assert!(found.contains("standard-skill"));
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
    fn nested_command_directory_becomes_namespace() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let nested = cwd.path().join(".claude").join("commands").join("backend");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("review.md"), "# Review\nbody").unwrap();
        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &[]);
        assert!(names(&cmds).contains("backend:review"));
    }

    #[test]
    fn load_skills_project_dir_still_overrides_plugin_contribution() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let project_dir = cwd.path().join(".wyj-code").join("skills");
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

    #[test]
    fn nested_cwd_loads_directory_skill_from_git_root() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let skill_dir = repo.path().join(".wyj-code").join("skills").join("release");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Project Release\nrelease instructions",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("references").join("notes.md"),
            "# Internal Notes\nnot a command",
        )
        .unwrap();
        let nested = repo.path().join("crates").join("cli");
        std::fs::create_dir_all(&nested).unwrap();

        let cmds = load_skills(home.path(), &nested, &HashSet::new(), &[]);
        let release = cmds.iter().find(|c| c.name() == "release").unwrap();
        assert_eq!(release.description(), "Project Release");
        assert!(!names(&cmds).contains("release:references:notes"));
    }

    #[test]
    fn global_real_cc_commands_dir_is_loaded() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let commands_dir = home.path().join(".claude").join("commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(commands_dir.join("hello.md"), "# Hello\nhi from real cc").unwrap();

        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &[]);
        let hello = cmds.iter().find(|c| c.name() == "hello").unwrap();
        assert_eq!(hello.description(), "Hello");
    }

    #[test]
    fn global_real_cc_commands_override_global_wyj_skills_same_scope() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let wyj_dir = home.path().join(".wyj-code").join("skills");
        std::fs::create_dir_all(&wyj_dir).unwrap();
        std::fs::write(wyj_dir.join("review.md"), "# WYJ Review\nwyj version").unwrap();

        let cc_dir = home.path().join(".claude").join("commands");
        std::fs::create_dir_all(&cc_dir).unwrap();
        std::fs::write(cc_dir.join("review.md"), "# Real CC Review\ncc version").unwrap();

        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &[]);
        let review = cmds.iter().find(|c| c.name() == "review").unwrap();
        assert_eq!(review.description(), "Real CC Review"); // 同作用域内真 CC 路径胜出
    }

    #[test]
    fn project_real_cc_commands_override_everything() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let global_cc_dir = home.path().join(".claude").join("commands");
        std::fs::create_dir_all(&global_cc_dir).unwrap();
        std::fs::write(global_cc_dir.join("custom.md"), "# Global CC\nglobal").unwrap();

        let project_wyj_dir = cwd.path().join(".wyj-code").join("skills");
        std::fs::create_dir_all(&project_wyj_dir).unwrap();
        std::fs::write(
            project_wyj_dir.join("custom.md"),
            "# Project WYJ\nproject wyj",
        )
        .unwrap();

        let project_cc_dir = cwd.path().join(".claude").join("commands");
        std::fs::create_dir_all(&project_cc_dir).unwrap();
        std::fs::write(project_cc_dir.join("custom.md"), "# Project CC\nproject cc").unwrap();

        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &[]);
        let custom = cmds.iter().find(|c| c.name() == "custom").unwrap();
        assert_eq!(custom.description(), "Project CC"); // 项目真 CC 路径最高优先级
    }

    #[test]
    fn project_wyj_skills_still_override_global_real_cc() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let global_cc_dir = home.path().join(".claude").join("commands");
        std::fs::create_dir_all(&global_cc_dir).unwrap();
        std::fs::write(global_cc_dir.join("custom.md"), "# Global CC\nglobal").unwrap();

        let project_wyj_dir = cwd.path().join(".wyj-code").join("skills");
        std::fs::create_dir_all(&project_wyj_dir).unwrap();
        std::fs::write(
            project_wyj_dir.join("custom.md"),
            "# Project WYJ\nproject wyj",
        )
        .unwrap();

        let cmds = load_skills(home.path(), cwd.path(), &HashSet::new(), &[]);
        let custom = cmds.iter().find(|c| c.name() == "custom").unwrap();
        // 作用域优先级（全局 < 项目）不受"同作用域内真 CC 胜出"规则影响
        assert_eq!(custom.description(), "Project WYJ");
    }
}
