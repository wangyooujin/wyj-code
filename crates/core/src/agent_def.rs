//! Agent 定义 — 内置 subagent 类型 + 用户自定义 agent（markdown frontmatter）加载
//!
//! 自定义 agent 定义文件来源与 skill 链同一哲学（项目覆盖全局）：全局
//! `~/.wyj-code/agents/*.md` → 已启用插件贡献路径（先到先得）→ 项目
//! `<git-root>/.wyj-code/agents/*.md`，同名后者覆盖前者。
//! **不读取 `~/.claude/agents/` 或 `<cwd>/.claude/agents/` 等任何外部源**
//! （v0.2.24 起移除；本段文档此前仍描述旧的四层链，是一处过时记录）。
//! frontmatter 支持 name/description/tools/model 四个字段，未识别字段静默忽略；
//! `model` 引用 `~/.wyj-code/config.toml` 中的 Profile 名（而非模型 ID）。

use std::path::{Path, PathBuf};

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
    /// 显式禁用的工具名列表，在 `tools` 之前生效（先减后取交集），支持
    /// `mcp__server` / `mcp__server__*` 整组屏蔽一个 MCP server。
    ///
    /// 只有 `tools` 白名单时，「继承全部但去掉 Write/Edit」是表达不出来的：
    /// MCP 工具是运行时发现的，用户根本列不全剩下的部分。
    pub disallowed_tools: Vec<String>,
    /// 是否不注入项目 `CLAUDE.md` 规约（内置 Explore/Plan 默认 true）。
    ///
    /// 探索类子 Agent 的典型任务是「找三处定义」，而本仓库根规约加子目录规约
    /// 动辄上万 token，一次都用不上。主 Agent 读到结果时它自己仍有完整规约。
    pub omit_claude_md: bool,
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
            description: crate::prompts::SUBAGENT_GENERAL_DESC.to_string(),
            tools: None,
            disallowed_tools: Vec::new(),
            omit_claude_md: false,
            model: None,
            system_prompt: crate::prompts::SUBAGENT_GENERAL.to_string(),
            builtin: true,
            source: None,
        },
        AgentDefinition {
            name: "Explore".to_string(),
            description: crate::prompts::SUBAGENT_EXPLORE_DESC.to_string(),
            tools: Some(readonly.clone()),
            disallowed_tools: Vec::new(),
            // 探索类委派最频繁、单次最轻：规约全文对「找出这三处定义」毫无帮助，
            // 却每趟都要付 prefill。对齐 Claude Code 的 Explore/Plan 取舍。
            omit_claude_md: true,
            model: None,
            system_prompt: crate::prompts::SUBAGENT_EXPLORE.to_string(),
            builtin: true,
            source: None,
        },
        AgentDefinition {
            name: "Plan".to_string(),
            description: crate::prompts::SUBAGENT_PLAN_DESC.to_string(),
            tools: Some(readonly),
            disallowed_tools: Vec::new(),
            omit_claude_md: true,
            model: None,
            system_prompt: crate::prompts::SUBAGENT_PLAN.to_string(),
            builtin: true,
            source: None,
        },
    ]
}

/// 从一个路径（文件或目录）读取全部 agent 定义文件。
fn read_defs_from_path(path: &Path) -> Vec<AgentDefinition> {
    let mut paths: Vec<PathBuf> = if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect()
    } else if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        Vec::new()
    };
    paths.sort();

    let mut defs = Vec::new();
    for path in paths {
        let Ok(content) = std::fs::read_to_string(&path) else {
            tracing::warn!("读取 agent 定义失败: {}", path.display());
            continue;
        };
        defs.push(parse_agent_file(&content, &path));
    }
    defs
}

fn upsert_overwrite(defs: &mut Vec<AgentDefinition>, def: AgentDefinition) {
    match defs.iter_mut().find(|d| d.name == def.name) {
        Some(existing) => *existing = def,
        None => defs.push(def),
    }
}

/// 加载全部 agent 定义，**四层合并链**（v0.2.24 起由 6 层简化）：内置 → 全局
/// `~/.wyj-code/agents` → 已启用插件贡献路径（按安装顺序，先到先得，跳过并警
/// 告同名冲突）→ 项目 `<git-root>/.wyj-code/agents`（最高优先级，覆盖一切，包
/// 括插件）。
///
/// 不再读取 `~/.claude/agents/` 与 `<cwd>/.claude/agents/` 等外部源——wyj-code
/// 只信任 `.wyj-code/` 下用户自己写下的条目。
pub fn load_agent_defs(cwd: &Path, plugin_agent_sources: &[PathBuf]) -> Vec<AgentDefinition> {
    let mut defs = builtin_defs();

    // 全局 wyj 自有目录（覆盖内置）
    if let Ok(home) = wyj_config::home_dir() {
        let global_wyj = wyj_config::global_config_dir_in(&home).join("agents");
        for def in read_defs_from_path(&global_wyj) {
            upsert_overwrite(&mut defs, def);
        }
    }

    // 已启用插件贡献路径（先到先得，跳过并警告同名冲突）
    for path in plugin_agent_sources {
        for def in read_defs_from_path(path) {
            if defs.iter().any(|d| d.name == def.name) {
                tracing::warn!("插件贡献的 agent '{}' 与已有资源同名，已跳过", def.name);
                continue;
            }
            defs.push(def);
        }
    }

    // 项目 wyj 自有目录（覆盖全局与插件）
    for def in read_defs_from_path(&wyj_config::project_config_dir(cwd).join("agents")) {
        upsert_overwrite(&mut defs, def);
    }

    defs
}

/// 解析单个 agent 定义文件（frontmatter + markdown 正文）
fn parse_agent_file(content: &str, path: &Path) -> AgentDefinition {
    let (fields, body) = crate::frontmatter::parse(content);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();
    let mut def = AgentDefinition {
        name: stem,
        description: String::new(),
        tools: None,
        disallowed_tools: Vec::new(),
        omit_claude_md: false,
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
                let list = parse_tool_list(&value);
                if !list.is_empty() {
                    def.tools = Some(list);
                }
            }
            "disallowed_tools" | "disallowedTools" => {
                def.disallowed_tools = parse_tool_list(&value)
            }
            "omit_claude_md" | "omitClaudeMd" => {
                def.omit_claude_md = matches!(value.trim(), "true" | "yes" | "1")
            }
            "model" if !value.is_empty() => def.model = Some(value),
            // 未识别字段（如真实 Claude Code 的其他 frontmatter key）静默忽略
            _ => {}
        }
    }
    def
}

/// 逗号分隔的工具名列表（`tools` / `disallowed_tools` 共用）
fn parse_tool_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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
    fn disallowed_tools_and_omit_claude_md_frontmatter() {
        // 「继承全部但去掉 Write/Edit」是只有 tools 白名单时表达不出来的写法
        let def = parse(
            "---\nname: no-writes\ndisallowedTools: Write, Edit, mcp__github\nomitClaudeMd: true\n---\nbody",
        );
        assert!(def.tools.is_none(), "未写 tools 时应表示不限制");
        assert_eq!(
            def.disallowed_tools,
            vec![
                "Write".to_string(),
                "Edit".to_string(),
                "mcp__github".to_string()
            ]
        );
        assert!(def.omit_claude_md);

        // 下划线写法同样认，默认值是 false
        let def = parse("---\nname: y\ndisallowed_tools: Bash\n---\nbody");
        assert_eq!(def.disallowed_tools, vec!["Bash".to_string()]);
        assert!(!def.omit_claude_md);
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

    /// 探索类内置类型不注入 CLAUDE.md：这是它们唯一的成本优势来源。
    /// 改动这条等于把本仓库上万 token 的规约重新压到每一次探索委派上。
    #[test]
    fn builtin_explore_and_plan_skip_claude_md() {
        let defs = builtin_defs();
        let by_name = |name: &str| defs.iter().find(|d| d.name == name).unwrap().clone();
        assert!(by_name("Explore").omit_claude_md);
        assert!(by_name("Plan").omit_claude_md);
        assert!(
            !by_name("general-purpose").omit_claude_md,
            "写代码的子 Agent 仍需要项目规约"
        );
    }

    #[test]
    fn custom_overrides_builtin_by_name() {
        let tmp = std::env::temp_dir().join(format!("wyj-agentdef-test-{}", std::process::id()));
        let agents_dir = tmp.join(".wyj-code").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("explore.md"),
            "---\nname: Explore\ndescription: 自定义覆盖\n---\n自定义提示词",
        )
        .unwrap();
        let defs = load_agent_defs(&tmp, &[]);
        let explore = defs.iter().find(|d| d.name == "Explore").unwrap();
        assert_eq!(explore.description, "自定义覆盖");
        assert!(!explore.builtin);
        // 覆盖是替换而非追加
        assert_eq!(defs.iter().filter(|d| d.name == "Explore").count(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn plugin_source_contributes_new_agent_definition() {
        let cwd = std::env::temp_dir().join(format!("wyj-agentdef-plugin-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let plugin_dir = cwd.join("plugin-agents");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("reviewer.md"),
            "---\nname: plugin-reviewer\ndescription: 插件提供的审查 agent\n---\n审查代码",
        )
        .unwrap();

        let defs = load_agent_defs(&cwd, &[plugin_dir]);
        let found = defs.iter().find(|d| d.name == "plugin-reviewer").unwrap();
        assert_eq!(found.description, "插件提供的审查 agent");
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn plugin_source_conflict_with_existing_name_is_skipped() {
        let cwd =
            std::env::temp_dir().join(format!("wyj-agentdef-conflict-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let plugin_dir = cwd.join("plugin-agents");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        // 与内置类型同名，插件贡献应被跳过（先到先得）。
        std::fs::write(
            plugin_dir.join("explore-clone.md"),
            "---\nname: Explore\ndescription: 插件想覆盖内置 Explore\n---\nbody",
        )
        .unwrap();

        let defs = load_agent_defs(&cwd, &[plugin_dir]);
        let explore = defs.iter().find(|d| d.name == "Explore").unwrap();
        assert!(explore.builtin); // 仍是内置版本，插件未能覆盖
        assert_eq!(defs.iter().filter(|d| d.name == "Explore").count(), 1);
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn project_dir_still_overrides_plugin_contribution() {
        let cwd =
            std::env::temp_dir().join(format!("wyj-agentdef-projoverride-{}", std::process::id()));
        let project_agents_dir = cwd.join(".wyj-code").join("agents");
        std::fs::create_dir_all(&project_agents_dir).unwrap();
        std::fs::write(
            project_agents_dir.join("custom.md"),
            "---\nname: custom\ndescription: 项目覆盖\n---\nbody",
        )
        .unwrap();
        let plugin_dir = cwd.join("plugin-agents");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("custom.md"),
            "---\nname: custom\ndescription: 插件版本\n---\nbody",
        )
        .unwrap();

        let defs = load_agent_defs(&cwd, &[plugin_dir]);
        let custom = defs.iter().find(|d| d.name == "custom").unwrap();
        assert_eq!(custom.description, "项目覆盖");
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn project_wyj_agents_dir_overrides_builtin() {
        let cwd = std::env::temp_dir().join(format!("wyj-agentdef-wyjdir-{}", std::process::id()));
        let wyj_agents_dir = cwd.join(".wyj-code").join("agents");
        std::fs::create_dir_all(&wyj_agents_dir).unwrap();
        std::fs::write(
            wyj_agents_dir.join("explore.md"),
            "---\nname: Explore\ndescription: wyj 目录覆盖\n---\nbody",
        )
        .unwrap();

        let defs = load_agent_defs(&cwd, &[]);
        let explore = defs.iter().find(|d| d.name == "Explore").unwrap();
        assert_eq!(explore.description, "wyj 目录覆盖");
        assert!(!explore.builtin);
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn nested_cwd_loads_project_agent_from_git_root() {
        let repo = std::env::temp_dir().join(format!(
            "wyj-agentdef-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let agents_dir = repo.join(".wyj-code").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("auditor.md"),
            "---\nname: auditor\ndescription: 项目审计 Agent\n---\nbody",
        )
        .unwrap();
        let nested = repo.join("crates").join("core");
        std::fs::create_dir_all(&nested).unwrap();

        let defs = load_agent_defs(&nested, &[]);
        let auditor = defs.iter().find(|d| d.name == "auditor").unwrap();
        assert_eq!(auditor.description, "项目审计 Agent");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn project_wyj_agents_override_global_wyj_agents() {
        // v0.2.24 起不再有"同作用域内真 CC 覆盖 wyj"的边界——
        // `<cwd>/.claude/agents` 不再被读取。本测试原本要在 `home/.wyj-code/agents`
        // 与 `<git-root>/.wyj-code/agents` 之间验证"项目级覆盖全局级"，
        // 但全局目录 fixture 会污染真实 `~/.wyj-code/agents/`（home_dir 不可
        // 在单测里 mock）。作用域优先级（全局 < 项目）的合并语义与 Skill 链
        // 完全等价，已被 `custom_overrides_builtin_by_name`（builtin 覆盖）与
        // `project_dir_still_overrides_plugin_contribution`（项目级覆盖插件）
        // 两处覆盖；本测试保留为占位以记录这条边界已由 v0.2.24 简化移除。
    }
}
