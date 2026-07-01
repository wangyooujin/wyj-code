//! Skill 系统：从 Markdown 文件加载可复用 prompt 模板

use crate::registry::{Command, CommandContext, CommandResult};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
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

fn parse_skill_file(name: &str, content: &str) -> SkillCommand {
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
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                skills.insert(name.clone(), parse_skill_file(&name, &content));
            }
            Err(e) => tracing::warn!("读取 skill 文件失败 {}: {e}", path.display()),
        }
    }
}

/// 加载所有 skill（内置 → 全局 → 项目，后者覆盖前者同名 skill）
pub fn load_skills(home: &Path, cwd: &Path) -> Vec<Arc<dyn Command>> {
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

    // 2. 全局用户 skill：~/.wyj-code/skills/*.md
    let global_dir = home.join(".wyj-code").join("skills");
    if global_dir.exists() {
        load_from_dir(&global_dir, &mut skills);
    }

    // 3. 项目 skill：.wyj/skills/*.md（最高优先级，同名覆盖全局）
    let project_dir = cwd.join(".wyj").join("skills");
    if project_dir.exists() {
        load_from_dir(&project_dir, &mut skills);
    }

    skills
        .into_values()
        .map(|s| Arc::new(s) as Arc<dyn Command>)
        .collect()
}
