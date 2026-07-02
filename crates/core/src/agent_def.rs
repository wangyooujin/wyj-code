//! Agent 定义 — 内置 subagent 类型 + 用户自定义 agent（markdown frontmatter）加载
//!
//! 自定义 agent 定义文件复用真实 Claude Code 的目录约定：
//! 全局 `~/.claude/agents/*.md` + 项目 `{cwd}/.claude/agents/*.md`，同名后者覆盖前者。
//! frontmatter 支持 name/description/tools/model 四个字段，未识别字段静默忽略；
//! `model` 引用 `~/.wyj-code/config.toml` 中的 Profile 名（而非模型 ID）。

use std::path::{Path, PathBuf};
use wyj_i18n::tr;

/// 内置 Explore/Plan 类型的只读工具集
pub const READONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "WebFetch"];

/// 一个可供 SubAgent 工具派生的 agent 类型定义
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// 类型名（frontmatter name 优先，缺省用文件名 stem）
    pub name: String,
    /// 一行简介（会拼进 Agent 工具的 description 供模型选型）
    pub description: String,
    /// 允许的工具名列表；None 表示不限制（使用标准工具集全集）
    pub tools: Option<Vec<String>>,
    /// 引用 config.toml 中的 Profile 名；None 表示使用全局 subagent 配置或主模型
    pub model: Option<String>,
    /// 系统提示词（frontmatter 之后的 markdown 正文）
    pub system_prompt: String,
    /// 是否内置类型
    pub builtin: bool,
    /// 自定义定义的来源文件路径（内置为 None）
    pub source: Option<PathBuf>,
}

/// 内置的三种 agent 类型：general-purpose / Explore / Plan
pub fn builtin_defs() -> Vec<AgentDefinition> {
    let readonly: Vec<String> = READONLY_TOOLS.iter().map(|s| s.to_string()).collect();
    vec![
        AgentDefinition {
            name: "general-purpose".to_string(),
            description: tr("subagent.builtin_general_desc"),
            tools: None,
            model: None,
            system_prompt: tr("system_prompt.subagent_general"),
            builtin: true,
            source: None,
        },
        AgentDefinition {
            name: "Explore".to_string(),
            description: tr("subagent.builtin_explore_desc"),
            tools: Some(readonly.clone()),
            model: None,
            system_prompt: tr("system_prompt.subagent_explore"),
            builtin: true,
            source: None,
        },
        AgentDefinition {
            name: "Plan".to_string(),
            description: tr("subagent.builtin_plan_desc"),
            tools: Some(readonly),
            model: None,
            system_prompt: tr("system_prompt.subagent_plan"),
            builtin: true,
            source: None,
        },
    ]
}

/// 加载全部 agent 定义：内置 → 全局 ~/.claude/agents → 项目 .claude/agents，同名后者覆盖。
pub fn load_agent_defs(cwd: &Path) -> Vec<AgentDefinition> {
    let mut defs = builtin_defs();
    let mut dirs: Vec<PathBuf> = vec![];
    if let Ok(home) = wyj_config::claude_home_dir() {
        dirs.push(home.join("agents"));
    }
    dirs.push(cwd.join(".claude").join("agents"));

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(content) = std::fs::read_to_string(&path) else {
                tracing::warn!("读取 agent 定义失败: {}", path.display());
                continue;
            };
            let def = parse_agent_file(&content, &path);
            match defs.iter_mut().find(|d| d.name == def.name) {
                Some(existing) => *existing = def,
                None => defs.push(def),
            }
        }
    }
    defs
}

/// 解析单个 agent 定义文件（frontmatter + markdown 正文）
fn parse_agent_file(content: &str, path: &Path) -> AgentDefinition {
    let (fields, body) = parse_frontmatter(content);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();
    let mut def = AgentDefinition {
        name: stem,
        description: String::new(),
        tools: None,
        model: None,
        system_prompt: body.trim().to_string(),
        builtin: false,
        source: Some(path.to_path_buf()),
    };
    for (key, value) in fields {
        match key.as_str() {
            "name" => {
                if !value.is_empty() {
                    def.name = value;
                }
            }
            "description" => def.description = value,
            "tools" => {
                let list: Vec<String> = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !list.is_empty() {
                    def.tools = Some(list);
                }
            }
            "model" => {
                if !value.is_empty() {
                    def.model = Some(value);
                }
            }
            // 未识别字段（如真实 Claude Code 的其他 frontmatter key）静默忽略
            _ => {}
        }
    }
    def
}

/// 解析 markdown 文件头部的 `---` frontmatter 块。
/// 返回 (key-value 对列表, 正文)。无 frontmatter 时字段列表为空、整个内容作为正文。
fn parse_frontmatter(content: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (vec![], content);
    };
    // 找到关闭的 --- 行
    let mut fields = vec![];
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.trim() == "---" {
            let body = &rest[offset + line.len()..];
            return (fields, body);
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if !key.is_empty() {
                fields.push((key, value));
            }
        }
        offset += line.len();
    }
    // 没有关闭的 ---：视作无 frontmatter，整个内容当正文
    (vec![], content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> AgentDefinition {
        parse_agent_file(content, Path::new("/tmp/agents/reviewer.md"))
    }

    #[test]
    fn full_frontmatter() {
        let def = parse(
            "---\nname: code-reviewer\ndescription: 审查代码\ntools: Read, Grep,Glob\nmodel: haiku-profile\n---\n\n你是审查专家。",
        );
        assert_eq!(def.name, "code-reviewer");
        assert_eq!(def.description, "审查代码");
        assert_eq!(
            def.tools.as_deref(),
            Some(&["Read".to_string(), "Grep".to_string(), "Glob".to_string()][..])
        );
        assert_eq!(def.model.as_deref(), Some("haiku-profile"));
        assert_eq!(def.system_prompt, "你是审查专家。");
        assert!(!def.builtin);
    }

    #[test]
    fn missing_fields_fall_back() {
        let def = parse("---\ndescription: 只有描述\n---\nbody");
        assert_eq!(def.name, "reviewer"); // 文件名 stem
        assert!(def.tools.is_none());
        assert!(def.model.is_none());
        assert_eq!(def.system_prompt, "body");
    }

    #[test]
    fn unknown_fields_ignored() {
        let def = parse("---\nname: x\ncolor: red\nfoo: bar\n---\nbody");
        assert_eq!(def.name, "x");
        assert_eq!(def.system_prompt, "body");
    }

    #[test]
    fn no_frontmatter_whole_file_is_prompt() {
        let def = parse("# 直接就是提示词\n\n内容");
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.system_prompt, "# 直接就是提示词\n\n内容");
    }

    #[test]
    fn unclosed_frontmatter_treated_as_body() {
        let def = parse("---\nname: x\n没有关闭");
        assert_eq!(def.name, "reviewer");
        assert!(def.system_prompt.contains("name: x"));
    }

    #[test]
    fn value_with_colon_kept_intact() {
        let def = parse("---\ndescription: 用法: 先读后写\n---\nbody");
        assert_eq!(def.description, "用法: 先读后写");
    }

    #[test]
    fn crlf_frontmatter() {
        let def = parse("---\r\nname: win\r\n---\r\nbody");
        assert_eq!(def.name, "win");
        assert_eq!(def.system_prompt, "body");
    }

    #[test]
    fn builtin_defs_have_three_types() {
        let defs = builtin_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["general-purpose", "Explore", "Plan"]);
        assert!(defs.iter().skip(1).all(|d| d.tools.is_some()));
    }

    #[test]
    fn custom_overrides_builtin_by_name() {
        let tmp = std::env::temp_dir().join(format!("wyj-agentdef-test-{}", std::process::id()));
        let agents_dir = tmp.join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("explore.md"),
            "---\nname: Explore\ndescription: 自定义覆盖\n---\n自定义提示词",
        )
        .unwrap();
        let defs = load_agent_defs(&tmp);
        let explore = defs.iter().find(|d| d.name == "Explore").unwrap();
        assert_eq!(explore.description, "自定义覆盖");
        assert!(!explore.builtin);
        // 覆盖是替换而非追加
        assert_eq!(defs.iter().filter(|d| d.name == "Explore").count(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
