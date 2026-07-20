//! 一键导入 Codex / Claude Code 配置：扫描外部工具的 MCP server、自定义命令
//! （skill）、子 Agent 定义，物化成 wyj-code 自管配置。
//!
//! 设计要点：
//! - **只读来源**：绝不改写 `~/.codex/`、`~/.claude.json`、`.mcp.json`、
//!   `~/.claude/`、`.claude/` 里的任何文件，导入 = 复制快照。
//! - **裸读 config.toml**：MCP 全局冲突检测与写回必须走
//!   `Config::load_file_only_at`，不能用 `Config::load()`——后者会把
//!   `~/.claude.json` 的原生 server 合并进内存，既让冲突检测全量误报，
//!   又会在写回时把只读的原生条目误物化进 config.toml。
//! - **幂等**：与目标内容完全相同的候选（MCP 结构相等 / 文件字节相等）不产出，
//!   重复运行 `/import` 列表自然收敛为空。
//! - **遮蔽提示**：Claude 的 commands/agents 目录在合并链里优先级高于 wyj
//!   自有目录（"真实 CC 路径覆盖 wyj 路径"），导入副本在原文件删除前不生效，
//!   这类候选标记 `shadowed` 并在结果报告中提示。
//! - **不写 lockfile**：导入产物没有 marketplace 来源、无法升级，保持
//!   "unmanaged" 状态（`extensions list` 已能正确展示这类条目）。
//! - agent 重名按文件名 stem 判定（本 crate 不解析 frontmatter；frontmatter
//!   `name` 与文件名不一致时可能漏报冲突，运行时 `load_agent_defs` 的覆写链
//!   自会兜底）。

use crate::lockfile::InstallScope;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wyj_config::{Config, McpServerConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportSourceApp {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    Mcp,
    Skill,
    Agent,
}

impl ImportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImportKind::Mcp => "mcp",
            ImportKind::Skill => "skill",
            ImportKind::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum ImportPayload {
    Mcp(McpServerConfig),
    /// skill/agent 的物化复制：`dest_rel` 是目标 `skills/`/`agents/` 根下的
    /// 相对路径（skill 保留子目录以维持 namespace，agent 平铺）。
    File {
        dest_rel: PathBuf,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportCandidate {
    pub kind: ImportKind,
    /// 展示名：mcp = server 名；skill = 含 namespace 的命令名（`ns:cmd`）；
    /// agent = 文件名 stem。
    pub name: String,
    pub source_app: ImportSourceApp,
    /// 原始来源文件（报告展示用）。
    pub source_path: PathBuf,
    pub scope: InstallScope,
    /// Some(冲突描述) = 目标位置已有同名且内容不同的条目，导入即覆盖。
    pub conflict: Option<String>,
    /// true = 导入后仍被在线原文件遮蔽（合并链里真实 CC 路径优先）。
    pub shadowed: bool,
    pub payload: ImportPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFilter {
    All,
    Codex,
    Claude,
}

#[derive(Debug, Default, Serialize)]
pub struct ScanResult {
    pub candidates: Vec<ImportCandidate>,
    /// 单个来源解析失败不中断整个扫描，错误收集在这里。
    pub errors: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportOutcome {
    /// 新写入的条目（`kind:name (scope)`）。
    pub applied: Vec<String>,
    /// 覆盖既有配置的条目。
    pub overwritten: Vec<String>,
    pub errors: Vec<String>,
    /// 被在线原文件遮蔽的已导入条目（`kind:name ← 原文件路径`），UI 侧配合
    /// 固定说明文案展示。
    pub shadow_warnings: Vec<String>,
}

/// 扫描来源与写入目标的路径集合，全部可注入以便测试指向临时目录。
pub struct ImportTargets {
    /// 用于定位 `~/.codex`、`~/.claude.json`、`~/.claude/`。
    pub home: PathBuf,
    /// 全局 config.toml 路径（MCP Global 的冲突检测与写回目标）。
    pub global_config_path: PathBuf,
    pub global_skills_dir: PathBuf,
    pub global_agents_dir: PathBuf,
    /// 项目根：`.mcp.json`、`.claude/` 来源与 `.wyj-code/` 写入目标由此推出。
    pub cwd: PathBuf,
}

impl ImportTargets {
    pub fn from_real_home(cwd: &Path) -> Result<Self> {
        let config_dir = wyj_config::config_dir()?;
        Ok(Self {
            home: wyj_config::home_dir()?,
            global_config_path: config_dir.join("config.toml"),
            global_skills_dir: config_dir.join("skills"),
            global_agents_dir: config_dir.join("agents"),
            cwd: cwd.to_path_buf(),
        })
    }

    fn dest_dir(&self, kind: ImportKind, scope: InstallScope) -> PathBuf {
        let project_dir = wyj_config::project_config_dir(&self.cwd);
        match (kind, scope) {
            (ImportKind::Skill, InstallScope::Global) => self.global_skills_dir.clone(),
            (ImportKind::Skill, InstallScope::Project) => project_dir.join("skills"),
            (ImportKind::Agent, InstallScope::Global) => self.global_agents_dir.clone(),
            (ImportKind::Agent, InstallScope::Project) => project_dir.join("agents"),
            (ImportKind::Mcp, _) => unreachable!("MCP 候选不走文件目标目录"),
        }
    }
}

fn scope_label(scope: InstallScope) -> &'static str {
    match scope {
        InstallScope::Global => "global",
        InstallScope::Project => "project",
    }
}

/// 扫描全部可导入项。来源枚举见模块文档；`filter` 限定来源应用。
pub fn scan_importable(targets: &ImportTargets, filter: ImportFilter) -> Result<ScanResult> {
    let mut result = ScanResult::default();

    if matches!(filter, ImportFilter::All | ImportFilter::Codex) {
        scan_codex(targets, &mut result);
    }
    if matches!(filter, ImportFilter::All | ImportFilter::Claude) {
        scan_claude(targets, &mut result)?;
    }
    Ok(result)
}

fn scan_codex(targets: &ImportTargets, result: &mut ScanResult) {
    let codex_dir = targets.home.join(".codex");

    // MCP：~/.codex/config.toml 的 [mcp_servers.*] → Global
    let codex_config = codex_dir.join("config.toml");
    match wyj_config::load_codex_mcp(&codex_config) {
        Ok(servers) => push_mcp_candidates(
            targets,
            result,
            servers,
            ImportSourceApp::Codex,
            &codex_config,
            InstallScope::Global,
        ),
        Err(e) => result
            .errors
            .push(format!("{}: {e}", codex_config.display())),
    }

    // Skill：~/.codex/prompts/*.md（平铺）→ 全局 skills
    let prompts_dir = codex_dir.join("prompts");
    for (rel, path) in list_md_files(&prompts_dir, false) {
        push_file_candidate(
            targets,
            result,
            ImportKind::Skill,
            ImportSourceApp::Codex,
            InstallScope::Global,
            &path,
            rel,
            false,
        );
    }
}

fn scan_claude(targets: &ImportTargets, result: &mut ScanResult) -> Result<()> {
    // MCP：~/.claude.json → Global；<cwd>/.mcp.json → Project
    let global_json = targets.home.join(".claude.json");
    if global_json.exists() {
        match wyj_config::load_native_mcp(&global_json) {
            Ok(servers) => push_mcp_candidates(
                targets,
                result,
                servers,
                ImportSourceApp::Claude,
                &global_json,
                InstallScope::Global,
            ),
            Err(e) => result
                .errors
                .push(format!("{}: {e}", global_json.display())),
        }
    }
    let project_json = targets.cwd.join(".mcp.json");
    if project_json.exists() {
        match wyj_config::load_native_mcp(&project_json) {
            Ok(servers) => push_mcp_candidates(
                targets,
                result,
                servers,
                ImportSourceApp::Claude,
                &project_json,
                InstallScope::Project,
            ),
            Err(e) => result
                .errors
                .push(format!("{}: {e}", project_json.display())),
        }
    }

    // Skill：commands 目录递归（子目录 = namespace），导入副本被在线原文件遮蔽
    let claude_home = targets.home.join(".claude");
    for (rel, path) in list_md_files(&claude_home.join("commands"), true) {
        push_file_candidate(
            targets,
            result,
            ImportKind::Skill,
            ImportSourceApp::Claude,
            InstallScope::Global,
            &path,
            rel,
            true,
        );
    }
    for (rel, path) in list_md_files(&targets.cwd.join(".claude").join("commands"), true) {
        push_file_candidate(
            targets,
            result,
            ImportKind::Skill,
            ImportSourceApp::Claude,
            InstallScope::Project,
            &path,
            rel,
            true,
        );
    }

    // Agent：agents 目录平铺（load_agent_defs 不递归），同样被在线原文件遮蔽
    for (rel, path) in list_md_files(&claude_home.join("agents"), false) {
        push_file_candidate(
            targets,
            result,
            ImportKind::Agent,
            ImportSourceApp::Claude,
            InstallScope::Global,
            &path,
            rel,
            true,
        );
    }
    for (rel, path) in list_md_files(&targets.cwd.join(".claude").join("agents"), false) {
        push_file_candidate(
            targets,
            result,
            ImportKind::Agent,
            ImportSourceApp::Claude,
            InstallScope::Project,
            &path,
            rel,
            true,
        );
    }
    Ok(())
}

fn push_mcp_candidates(
    targets: &ImportTargets,
    result: &mut ScanResult,
    servers: Vec<McpServerConfig>,
    source_app: ImportSourceApp,
    source_path: &Path,
    scope: InstallScope,
) {
    if servers.is_empty() {
        return;
    }
    let existing: Vec<McpServerConfig> = match scope {
        InstallScope::Global => match Config::load_file_only_at(&targets.global_config_path) {
            Ok(cfg) => cfg.mcp_servers,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: {e}", targets.global_config_path.display()));
                return;
            }
        },
        InstallScope::Project => match wyj_config::load_project_mcp(&targets.cwd) {
            Ok(servers) => servers,
            Err(e) => {
                result.errors.push(format!("project mcp.toml: {e}"));
                return;
            }
        },
    };
    let existing: HashMap<&str, &McpServerConfig> =
        existing.iter().map(|s| (s.name.as_str(), s)).collect();
    for server in servers {
        let conflict = match existing.get(server.name.as_str()) {
            // 目标已有完全相同的配置：无事可做，不产出候选（幂等）
            Some(current) if **current == server => continue,
            Some(_) => Some(format!(
                "already configured in {} scope with different settings",
                scope_label(scope)
            )),
            None => None,
        };
        result.candidates.push(ImportCandidate {
            kind: ImportKind::Mcp,
            name: server.name.clone(),
            source_app,
            source_path: source_path.to_path_buf(),
            scope,
            conflict,
            shadowed: false,
            payload: ImportPayload::Mcp(server),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn push_file_candidate(
    targets: &ImportTargets,
    result: &mut ScanResult,
    kind: ImportKind,
    source_app: ImportSourceApp,
    scope: InstallScope,
    source_path: &Path,
    dest_rel: PathBuf,
    shadowed: bool,
) {
    let content = match std::fs::read_to_string(source_path) {
        Ok(content) => content,
        Err(e) => {
            result
                .errors
                .push(format!("{}: {e}", source_path.display()));
            return;
        }
    };
    let dest = targets.dest_dir(kind, scope).join(&dest_rel);
    let conflict = match std::fs::read_to_string(&dest) {
        // 目标文件字节级相同：已导入过，不产出候选（幂等）
        Ok(existing) if existing == content => return,
        Ok(_) => Some(format!("file exists: {}", dest.display())),
        Err(_) => None,
    };
    result.candidates.push(ImportCandidate {
        kind,
        name: rel_to_display_name(&dest_rel),
        source_app,
        source_path: source_path.to_path_buf(),
        scope,
        conflict,
        shadowed,
        payload: ImportPayload::File { dest_rel, content },
    });
}

/// `a/b/c.md` → `a:b:c`（与 skill loader 的 namespace 命名一致）。
fn rel_to_display_name(rel: &Path) -> String {
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = parts.last_mut() {
        if let Some(stem) = last.strip_suffix(".md") {
            *last = stem.to_string();
        }
    }
    parts.join(":")
}

/// 列出目录下的 `.md` 文件，返回 `(相对路径, 绝对路径)`；`recursive` 控制是否
/// 深入子目录（skill namespace 需要，agent 平铺不需要）。目录不存在返回空。
fn list_md_files(dir: &Path, recursive: bool) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    collect_md_files(dir, PathBuf::new(), recursive, &mut out);
    out.sort();
    out
}

fn collect_md_files(dir: &Path, rel: PathBuf, recursive: bool, out: &mut Vec<(PathBuf, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                let sub_rel = rel.join(entry.file_name());
                collect_md_files(&path, sub_rel, recursive, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push((rel.join(entry.file_name()), path));
        }
    }
}

/// 把选中的候选写入 wyj 配置。MCP 按 scope 汇总后各写一次（避免同一文件
/// 多次读写），文件类候选逐个复制。
pub fn apply_import(
    targets: &ImportTargets,
    selected: &[ImportCandidate],
) -> Result<ImportOutcome> {
    let mut outcome = ImportOutcome::default();

    // MCP：按 scope 分组批量 upsert
    for scope in [InstallScope::Global, InstallScope::Project] {
        let picked: Vec<&ImportCandidate> = selected
            .iter()
            .filter(|c| c.kind == ImportKind::Mcp && c.scope == scope)
            .collect();
        if picked.is_empty() {
            continue;
        }
        if let Err(e) = apply_mcp_batch(targets, scope, &picked, &mut outcome) {
            outcome
                .errors
                .push(format!("mcp ({}): {e}", scope_label(scope)));
        }
    }

    // Skill / Agent：物化复制
    for candidate in selected
        .iter()
        .filter(|c| !matches!(c.payload, ImportPayload::Mcp(_)))
    {
        let ImportPayload::File { dest_rel, content } = &candidate.payload else {
            continue;
        };
        let dest = targets
            .dest_dir(candidate.kind, candidate.scope)
            .join(dest_rel);
        let write = || -> Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }
            std::fs::write(&dest, content)
                .with_context(|| format!("写入文件失败: {}", dest.display()))?;
            Ok(())
        };
        match write() {
            Ok(()) => record_applied(&mut outcome, candidate),
            Err(e) => outcome.errors.push(format!(
                "{}:{}: {e}",
                candidate.kind.as_str(),
                candidate.name
            )),
        }
    }

    Ok(outcome)
}

fn apply_mcp_batch(
    targets: &ImportTargets,
    scope: InstallScope,
    picked: &[&ImportCandidate],
    outcome: &mut ImportOutcome,
) -> Result<()> {
    match scope {
        InstallScope::Global => {
            let mut cfg = Config::load_file_only_at(&targets.global_config_path)?;
            for candidate in picked {
                let ImportPayload::Mcp(server) = &candidate.payload else {
                    continue;
                };
                upsert_by_name(&mut cfg.mcp_servers, server.clone());
                record_applied(outcome, candidate);
            }
            if let Some(parent) = targets.global_config_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
            }
            cfg.save_to(&targets.global_config_path)
        }
        InstallScope::Project => {
            let mut servers = wyj_config::load_project_mcp(&targets.cwd)?;
            for candidate in picked {
                let ImportPayload::Mcp(server) = &candidate.payload else {
                    continue;
                };
                upsert_by_name(&mut servers, server.clone());
                record_applied(outcome, candidate);
            }
            wyj_config::save_project_mcp(&targets.cwd, &servers)
        }
    }
}

fn upsert_by_name(servers: &mut Vec<McpServerConfig>, server: McpServerConfig) {
    if let Some(existing) = servers.iter_mut().find(|s| s.name == server.name) {
        *existing = server;
    } else {
        servers.push(server);
    }
}

fn record_applied(outcome: &mut ImportOutcome, candidate: &ImportCandidate) {
    let label = format!(
        "{}:{} ({})",
        candidate.kind.as_str(),
        candidate.name,
        scope_label(candidate.scope)
    );
    if candidate.conflict.is_some() {
        outcome.overwritten.push(label);
    } else {
        outcome.applied.push(label);
    }
    if candidate.shadowed {
        outcome.shadow_warnings.push(format!(
            "{}:{} ← {}",
            candidate.kind.as_str(),
            candidate.name,
            candidate.source_path.display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(home: &Path, cwd: &Path) -> ImportTargets {
        let config_dir = home.join(".wyj-code");
        ImportTargets {
            home: home.to_path_buf(),
            global_config_path: config_dir.join("config.toml"),
            global_skills_dir: config_dir.join("skills"),
            global_agents_dir: config_dir.join("agents"),
            cwd: cwd.to_path_buf(),
        }
    }

    fn setup_sources(home: &Path, cwd: &Path) {
        // Codex：2 个 MCP（其一带未知字段）+ 1 个 prompt
        let codex = home.join(".codex");
        std::fs::create_dir_all(codex.join("prompts")).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            r#"
[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
startup_timeout_sec = 20

[mcp_servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
"#,
        )
        .unwrap();
        std::fs::write(codex.join("prompts").join("deploy.md"), "# Deploy\ngo").unwrap();

        // Claude 全局：1 个与 codex 重名的 MCP + namespace 命令 + agent
        std::fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"context7":{"command":"bunx","args":["context7"]}}}"#,
        )
        .unwrap();
        let claude = home.join(".claude");
        std::fs::create_dir_all(claude.join("commands").join("ns")).unwrap();
        std::fs::create_dir_all(claude.join("agents")).unwrap();
        std::fs::write(claude.join("commands").join("ns").join("tool.md"), "# T\nt").unwrap();
        std::fs::write(claude.join("agents").join("reviewer.md"), "# R\nr").unwrap();

        // 项目：.mcp.json + 项目 commands/agents
        std::fs::write(
            cwd.join(".mcp.json"),
            r#"{"mcpServers":{"pg":{"command":"npx","args":["pg-mcp"]}}}"#,
        )
        .unwrap();
        let proj_claude = cwd.join(".claude");
        std::fs::create_dir_all(proj_claude.join("commands")).unwrap();
        std::fs::create_dir_all(proj_claude.join("agents")).unwrap();
        std::fs::write(proj_claude.join("commands").join("x.md"), "# X\nx").unwrap();
        std::fs::write(proj_claude.join("agents").join("y.md"), "# Y\ny").unwrap();
    }

    #[test]
    fn scan_enumerates_all_sources() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        setup_sources(home.path(), cwd.path());

        let t = targets(home.path(), cwd.path());
        let scan = scan_importable(&t, ImportFilter::All).unwrap();
        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        // codex: 2 mcp + 1 prompt；claude: 1 mcp 全局 + 1 mcp 项目 + 2 skill + 2 agent
        assert_eq!(scan.candidates.len(), 9);

        let names: Vec<&str> = scan.candidates.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"context7"));
        assert!(names.contains(&"deploy"));
        assert!(names.contains(&"ns:tool"));
        assert!(names.contains(&"reviewer"));
        assert!(names.contains(&"pg"));

        // codex 来源不遮蔽，claude commands/agents 遮蔽
        let deploy = scan.candidates.iter().find(|c| c.name == "deploy").unwrap();
        assert!(!deploy.shadowed);
        let tool = scan
            .candidates
            .iter()
            .find(|c| c.name == "ns:tool")
            .unwrap();
        assert!(tool.shadowed);
        assert_eq!(tool.scope, InstallScope::Global);
        let y = scan.candidates.iter().find(|c| c.name == "y").unwrap();
        assert_eq!(y.scope, InstallScope::Project);
        assert_eq!(y.kind, ImportKind::Agent);
    }

    #[test]
    fn filter_limits_sources() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        setup_sources(home.path(), cwd.path());
        let t = targets(home.path(), cwd.path());

        let codex_only = scan_importable(&t, ImportFilter::Codex).unwrap();
        assert_eq!(codex_only.candidates.len(), 3);
        assert!(codex_only
            .candidates
            .iter()
            .all(|c| c.source_app == ImportSourceApp::Codex));

        let claude_only = scan_importable(&t, ImportFilter::Claude).unwrap();
        assert_eq!(claude_only.candidates.len(), 6);
    }

    #[test]
    fn mcp_conflict_marked_when_config_has_different_same_name() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        setup_sources(home.path(), cwd.path());
        let t = targets(home.path(), cwd.path());

        // 全局 config.toml 已有一个不同配置的 context7
        std::fs::create_dir_all(t.global_config_path.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.mcp_servers.push(McpServerConfig {
            name: "context7".into(),
            transport: wyj_config::McpTransport::Stdio,
            command: Some("other".into()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
        });
        cfg.save_to(&t.global_config_path).unwrap();

        let scan = scan_importable(&t, ImportFilter::All).unwrap();
        let conflicted: Vec<_> = scan
            .candidates
            .iter()
            .filter(|c| c.name == "context7" && c.conflict.is_some())
            .collect();
        // codex 与 claude 两个来源的 context7 都与既有配置不同 → 都标冲突
        assert_eq!(conflicted.len(), 2);
    }

    #[test]
    fn apply_then_rescan_is_empty_for_non_conflicting() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        setup_sources(home.path(), cwd.path());
        let t = targets(home.path(), cwd.path());

        let scan = scan_importable(&t, ImportFilter::All).unwrap();
        let non_conflicting: Vec<ImportCandidate> = scan
            .candidates
            .into_iter()
            .filter(|c| c.conflict.is_none())
            .collect();
        let outcome = apply_import(&t, &non_conflicting).unwrap();
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!outcome.applied.is_empty());
        // claude 来源的 skill/agent 有遮蔽提示
        assert!(outcome
            .shadow_warnings
            .iter()
            .any(|w| w.starts_with("skill:ns:tool")));

        // 幂等闭环：除"claude 与 codex 的 context7 互异"这类真实差异外，
        // setup 里 codex/claude 的 context7 配置不同——codex 版先写入后，
        // claude 版会对上冲突。重扫时其余候选应消失。
        let rescan = scan_importable(&t, ImportFilter::All).unwrap();
        for c in &rescan.candidates {
            assert!(
                c.conflict.is_some(),
                "重扫出现无冲突候选（幂等破坏）: {}:{}",
                c.kind.as_str(),
                c.name
            );
        }

        // 落盘核对
        assert!(t.global_skills_dir.join("deploy.md").exists());
        assert!(t.global_skills_dir.join("ns").join("tool.md").exists());
        assert!(t.global_agents_dir.join("reviewer.md").exists());
        assert!(wyj_config::project_config_dir(&t.cwd)
            .join("agents")
            .join("y.md")
            .exists());
        let cfg = Config::load_file_only_at(&t.global_config_path).unwrap();
        assert!(cfg.mcp_servers.iter().any(|s| s.name == "fetch"));
        let project = wyj_config::load_project_mcp(&t.cwd).unwrap();
        assert!(project.iter().any(|s| s.name == "pg"));
    }

    #[test]
    fn conflicting_candidate_overwrites_when_selected() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        setup_sources(home.path(), cwd.path());
        let t = targets(home.path(), cwd.path());

        // 先把 codex 版 context7 导入，再选中 claude 版（冲突项）覆盖
        let scan = scan_importable(&t, ImportFilter::Codex).unwrap();
        apply_import(&t, &scan.candidates).unwrap();

        let rescan = scan_importable(&t, ImportFilter::Claude).unwrap();
        let claude_ctx: Vec<ImportCandidate> = rescan
            .candidates
            .into_iter()
            .filter(|c| c.name == "context7")
            .collect();
        assert_eq!(claude_ctx.len(), 1);
        assert!(claude_ctx[0].conflict.is_some());

        let outcome = apply_import(&t, &claude_ctx).unwrap();
        assert_eq!(outcome.overwritten.len(), 1);
        let cfg = Config::load_file_only_at(&t.global_config_path).unwrap();
        let ctx = cfg
            .mcp_servers
            .iter()
            .find(|s| s.name == "context7")
            .unwrap();
        assert_eq!(ctx.command.as_deref(), Some("bunx"));
    }
}
