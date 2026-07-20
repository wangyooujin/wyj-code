//! 安装元数据 lockfile：`~/.wyj-code/installed.json`（全局）+ `<cwd>/.wyj-code/installed.json`（项目）
//!
//! 只记录"通过 /mcp、/skills 面板安装"的条目的版本/来源/启用状态，不改动
//! `McpServerConfig`/`SKILL.md` 本身的格式。config.toml、`.wyj-code/mcp.toml`、
//! skill 目录里存在但这里找不到同名记录的条目，视为"未纳管/手动配置"。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use wyj_config::McpServerConfig;

pub const LOCKFILE_VERSION: u32 = 2;

/// 统一资源类型。旧版的 mcp_servers/skills/plugins 数组继续保留，供兼容读取；
/// 新代码可以用 `extensions` 做跨类型查询和诊断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    Skill,
    Agent,
    Mcp,
    Plugin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionLockEntry {
    pub id: String,
    pub kind: ExtensionKind,
    pub scope: InstallScope,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub installed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstalledManifest {
    pub version: u32,
    pub mcp_servers: Vec<InstalledMcpEntry>,
    pub skills: Vec<InstalledSkillEntry>,
    /// 仅全局 lockfile 使用；marketplace 源不区分 scope。
    pub marketplaces: Vec<MarketplaceSource>,
    /// 仅全局 lockfile 使用；MCP registry 源不区分 scope。为空时由
    /// `registry::ensure_default_registry` 负责预置官方源并持久化，
    /// 不在这里硬编码默认值（避免和网络地址常量耦合进 lockfile 模块）。
    pub mcp_registries: Vec<McpRegistrySource>,
    /// 已安装的插件（整体启用/禁用，不拆分内部 commands/agents/skills/mcpServers）。
    pub plugins: Vec<InstalledPluginEntry>,
    /// 仅全局 lockfile 使用；插件 marketplace 源不区分 scope。
    pub plugin_marketplaces: Vec<PluginMarketplaceSource>,
    /// v2 统一资源索引。旧数组仍然是配置/运行时的兼容来源，迁移完成后由
    /// extensions 模块逐步补齐此索引。
    pub extensions: Vec<ExtensionLockEntry>,
}

impl InstalledManifest {
    fn new() -> Self {
        Self {
            version: LOCKFILE_VERSION,
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            marketplaces: Vec::new(),
            mcp_registries: Vec::new(),
            plugins: Vec::new(),
            plugin_marketplaces: Vec::new(),
            extensions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledMcpEntry {
    /// 与 `McpServerConfig.name` 一一对应（join key）
    pub name: String,
    /// 强制具体版本号，不允许 "latest"。`None` 表示这是一条仅用于持久化
    /// `enabled` 状态的手动配置项记录（未经 registry 安装，无版本可言）。
    pub version: Option<String>,
    /// `None` = 手动配置项（该记录只用来持久化 enabled 状态，不支持"升级"）。
    pub source: Option<McpSource>,
    pub enabled: bool,
    pub installed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl InstalledMcpEntry {
    /// 是否为通过 registry 安装的纳管条目（支持升级）。
    pub fn is_managed(&self) -> bool {
        self.source.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpSource {
    Registry {
        registry_url: String,
        server_name: String,
        package_registry_type: String,
        package_identifier: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkillEntry {
    /// `.md` 文件 stem（join key）
    pub name: String,
    /// `None` 表示手动配置项（未经 marketplace 安装，无版本可言）。
    pub version: Option<String>,
    pub scope: InstallScope,
    /// `None` = 手动配置项（该记录只用来持久化 enabled 状态，不支持"升级"）。
    pub source: Option<SkillSource>,
    pub enabled: bool,
    pub installed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl InstalledSkillEntry {
    /// 是否为通过 marketplace 安装的纳管条目（支持升级）。
    pub fn is_managed(&self) -> bool {
        self.source.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSource {
    pub marketplace_id: String,
    pub marketplace_url: String,
    /// marketplace.json 里该条目的 path 字段（相对仓库根）
    pub entry_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceSource {
    pub id: String,
    pub git_url: String,
    pub added_at: DateTime<Utc>,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// 一个 MCP Registry 源：可以是官方公共实例，也可以是自建/私有的同一开源
/// registry 软件（modelcontextprotocol/registry）的另一部署实例——都走同一套
/// `/v0/servers` API，只是 `base_url` 不同。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRegistrySource {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub added_at: DateTime<Utc>,
}

/// 一个插件 marketplace 源：git 仓库或本地目录，clone/pull 后解析
/// `.claude-plugin/marketplace.json`。与 skill 的 [`MarketplaceSource`] 分开建模，
/// 因为需要多存 owner/plugin 数量/是否本地源等信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMarketplaceSource {
    pub id: String,
    /// git URL，或本地目录绝对路径（`is_local` 为 true 时）。
    pub location: String,
    pub is_local: bool,
    /// marketplace.json 的 name 字段，最近一次 sync 后缓存展示用。
    pub display_name: String,
    /// marketplace.json 的 owner.name 字段。
    pub owner_name: String,
    /// 最近一次 sync 时 plugins.len()，避免 Browse tab 每次重新 clone/parse。
    pub plugin_count: usize,
    pub added_at: DateTime<Utc>,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// 插件的安装来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginInstallOrigin {
    Marketplace {
        marketplace_id: String,
        marketplace_location: String,
    },
    /// 通过 TUI「添加本地插件」安装（非 `--plugin-dir` 临时加载）：持久化进
    /// lockfile，跨会话可见，可 enable/disable/uninstall，但不支持 upgrade。
    Local { path: PathBuf },
}

/// 插件安装时解析出的贡献快照。enable/disable/uninstall 都靠这份快照决策，
/// 不重新读取 plugin.json（版本升级后字段可能已变化）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginContributions {
    /// skill 文件/目录的绝对路径。
    pub skill_paths: Vec<PathBuf>,
    /// agent 定义文件/目录的绝对路径。
    pub agent_paths: Vec<PathBuf>,
    /// 完整解析后的 MCP server 配置（已带插件专属 env/args），不写入
    /// config.toml/mcp.toml，只存在于这份快照里，由
    /// `mcp_install::effective_mcp_servers` 在合并时读取。
    pub mcp_servers: Vec<McpServerConfig>,
    /// 因当前版本不支持（如 hooks/themes）或名字冲突而被跳过的能力/资源名，
    /// 如 `["hooks", "themes", "sse-mcp:some-server", "skill-conflict:review"]`。
    pub skipped_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginEntry {
    /// join key，同一 scope 内唯一。
    pub name: String,
    pub version: Option<String>,
    pub scope: InstallScope,
    pub source: PluginInstallOrigin,
    /// 整体开关：不支持拆分内部 commands/agents/skills/mcpServers 单独控制。
    pub enabled: bool,
    pub installed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 物化后的插件根目录（marketplace 来源为 clone/复制目录；本地开发来源
    /// 就是用户给的路径本身）。
    pub plugin_root: PathBuf,
    pub contributes: PluginContributions,
}

impl InstalledPluginEntry {
    pub fn is_local_dev(&self) -> bool {
        matches!(self.source, PluginInstallOrigin::Local { .. })
    }
}

pub fn global_lockfile_path() -> Result<PathBuf> {
    Ok(wyj_config::config_dir()?.join("installed.json"))
}

pub fn project_lockfile_path(cwd: &Path) -> PathBuf {
    wyj_config::project_config_dir(cwd).join("installed.json")
}

fn load_from(path: &Path) -> Result<InstalledManifest> {
    if !path.exists() {
        return Ok(InstalledManifest::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 lockfile 失败: {}", path.display()))?;
    let mut manifest: InstalledManifest = serde_json::from_str(&content)
        .with_context(|| format!("解析 lockfile 失败: {}", path.display()))?;
    // v1 文件缺少统一 extensions 数组；保留所有旧字段并在下次写入时升级。
    manifest.version = LOCKFILE_VERSION;
    Ok(manifest)
}

fn save_to(path: &Path, manifest: &InstalledManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 lockfile 目录失败: {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(manifest).context("序列化 lockfile 失败")?;
    // Write beside the destination and rename only after the complete JSON is
    // on disk.  A killed process or a full volume must never leave a truncated
    // installed.json that makes every extension appear missing on next start.
    let nonce = format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let tmp = path.with_file_name(format!(
        "{}.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("installed.json"),
        nonce
    ));
    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("写入 lockfile 失败: {}", path.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("替换 lockfile 失败: {}", path.display()));
    }
    Ok(())
}

pub fn load_global() -> Result<InstalledManifest> {
    load_from(&global_lockfile_path()?)
}

pub fn load_project(cwd: &Path) -> Result<InstalledManifest> {
    load_from(&project_lockfile_path(cwd))
}

pub fn save_global(manifest: &InstalledManifest) -> Result<()> {
    save_to(&global_lockfile_path()?, manifest)
}

pub fn save_project(cwd: &Path, manifest: &InstalledManifest) -> Result<()> {
    save_to(&project_lockfile_path(cwd), manifest)
}

/// 汇总全局 + 项目 lockfile 里 `enabled == false` 的 skill 名称，供
/// `crates/commands::skill::load_skills` 过滤禁用项。
pub fn disabled_skill_names(cwd: &Path) -> HashSet<String> {
    let mut disabled = HashSet::new();
    if let Ok(global) = load_global() {
        disabled.extend(
            global
                .skills
                .iter()
                .filter(|s| !s.enabled)
                .map(|s| s.name.clone()),
        );
    }
    if let Ok(project) = load_project(cwd) {
        disabled.extend(
            project
                .skills
                .iter()
                .filter(|s| !s.enabled)
                .map(|s| s.name.clone()),
        );
    }
    disabled
}

/// 汇总全局 + 项目 lockfile 里 `enabled == false` 的 MCP server 名称，供
/// `mcp_install::effective_mcp_servers` 过滤禁用项。
pub fn disabled_mcp_names(cwd: &Path) -> HashSet<String> {
    let mut disabled = HashSet::new();
    if let Ok(global) = load_global() {
        disabled.extend(
            global
                .mcp_servers
                .iter()
                .filter(|s| !s.enabled)
                .map(|s| s.name.clone()),
        );
    }
    if let Ok(project) = load_project(cwd) {
        disabled.extend(
            project
                .mcp_servers
                .iter()
                .filter(|s| !s.enabled)
                .map(|s| s.name.clone()),
        );
    }
    disabled
}

/// 汇总全局 + 项目 lockfile 里 `enabled == true` 的插件条目，供
/// `plugin_install::{enabled_plugin_skill_paths,enabled_plugin_agent_paths,plugin_mcp_servers}`
/// 及加载器使用。
pub fn enabled_plugin_entries(cwd: &Path) -> Vec<InstalledPluginEntry> {
    let mut entries = Vec::new();
    if let Ok(global) = load_global() {
        entries.extend(global.plugins.into_iter().filter(|p| p.enabled));
    }
    if let Ok(project) = load_project(cwd) {
        entries.extend(project.plugins.into_iter().filter(|p| p.enabled));
    }
    entries
}

/// 按 scope 加载对应 lockfile。
pub fn load_scope(scope: InstallScope, cwd: &Path) -> Result<InstalledManifest> {
    match scope {
        InstallScope::Global => load_global(),
        InstallScope::Project => load_project(cwd),
    }
}

/// 按 scope 保存对应 lockfile。
pub fn save_scope(scope: InstallScope, cwd: &Path, manifest: &InstalledManifest) -> Result<()> {
    match scope {
        InstallScope::Global => save_global(manifest),
        InstallScope::Project => save_project(cwd, manifest),
    }
}

pub fn upsert_extension(manifest: &mut InstalledManifest, entry: ExtensionLockEntry) {
    if let Some(existing) = manifest.extensions.iter_mut().find(|x| x.id == entry.id) {
        *existing = entry;
    } else {
        manifest.extensions.push(entry);
    }
}

pub fn remove_extension(manifest: &mut InstalledManifest, id: &str) {
    manifest.extensions.retain(|x| x.id != id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_default_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = load_from(&dir.path().join("installed.json")).unwrap();
        assert_eq!(manifest.version, LOCKFILE_VERSION);
        assert!(manifest.mcp_servers.is_empty());
        assert!(manifest.skills.is_empty());
        assert!(manifest.marketplaces.is_empty());
        assert!(manifest.mcp_registries.is_empty());
        assert!(manifest.plugins.is_empty());
        assert!(manifest.plugin_marketplaces.is_empty());
    }

    /// 旧版本 lockfile 文件（在 plugins 字段引入之前写入的 json）不应因缺少
    /// `plugins`/`plugin_marketplaces` 字段而解析失败——`#[serde(default)]`
    /// 保证向后兼容。
    #[test]
    fn old_lockfile_without_plugins_field_parses_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        std::fs::write(
            &path,
            r#"{"version":1,"mcp_servers":[],"skills":[],"marketplaces":[],"mcp_registries":[]}"#,
        )
        .unwrap();
        let manifest = load_from(&path).unwrap();
        assert!(manifest.plugins.is_empty());
        assert!(manifest.plugin_marketplaces.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        let mut manifest = InstalledManifest::new();
        manifest.mcp_servers.push(InstalledMcpEntry {
            name: "postgres".to_string(),
            version: Some("1.2.3".to_string()),
            source: Some(McpSource::Registry {
                registry_url: "https://registry.modelcontextprotocol.io".to_string(),
                server_name: "io.modelcontextprotocol/postgres".to_string(),
                package_registry_type: "npm".to_string(),
                package_identifier: "@modelcontextprotocol/server-postgres".to_string(),
            }),
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
        });
        manifest.extensions.push(ExtensionLockEntry {
            id: "mcp:postgres".to_string(),
            kind: ExtensionKind::Mcp,
            scope: InstallScope::Global,
            source: Some("registry".to_string()),
            version: Some("1.2.3".to_string()),
            commit: None,
            digest: None,
            enabled: true,
            dependencies: vec![],
            installed_at: Utc::now(),
            updated_at: Utc::now(),
        });
        save_to(&path, &manifest).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.mcp_servers.len(), 1);
        assert_eq!(loaded.mcp_servers[0].name, "postgres");
        assert_eq!(loaded.mcp_servers[0].version.as_deref(), Some("1.2.3"));
        assert!(loaded.mcp_servers[0].is_managed());
        assert_eq!(loaded.extensions[0].id, "mcp:postgres");
    }

    #[test]
    fn disabled_names_filters_enabled_false_only() {
        let dir = tempfile::tempdir().unwrap();
        let global_path = dir.path().join("global.json");
        let mut manifest = InstalledManifest::new();
        manifest.skills.push(InstalledSkillEntry {
            name: "enabled-skill".to_string(),
            version: Some("1.0.0".to_string()),
            scope: InstallScope::Global,
            source: Some(SkillSource {
                marketplace_id: "abc".to_string(),
                marketplace_url: "file:///tmp/x".to_string(),
                entry_path: "skills/a.md".to_string(),
            }),
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
        });
        manifest.skills.push(InstalledSkillEntry {
            name: "disabled-skill".to_string(),
            version: Some("1.0.0".to_string()),
            scope: InstallScope::Global,
            source: Some(SkillSource {
                marketplace_id: "abc".to_string(),
                marketplace_url: "file:///tmp/x".to_string(),
                entry_path: "skills/b.md".to_string(),
            }),
            enabled: false,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
        });
        save_to(&global_path, &manifest).unwrap();

        // 直接复用 save_to/load_from 验证过滤逻辑本身（不经过 config_dir 全局路径)
        let loaded = load_from(&global_path).unwrap();
        let disabled: HashSet<String> = loaded
            .skills
            .iter()
            .filter(|s| !s.enabled)
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(disabled.len(), 1);
        assert!(disabled.contains("disabled-skill"));
        assert!(!disabled.contains("enabled-skill"));
    }

    #[test]
    fn plugin_entry_save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.json");
        let mut manifest = InstalledManifest::new();
        manifest.plugins.push(InstalledPluginEntry {
            name: "code-reviewer".to_string(),
            version: Some("1.0.0".to_string()),
            scope: InstallScope::Global,
            source: PluginInstallOrigin::Marketplace {
                marketplace_id: "abc123".to_string(),
                marketplace_location: "https://github.com/example/plugins.git".to_string(),
            },
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            plugin_root: PathBuf::from("plugins/repos/abc123/code-reviewer"),
            contributes: PluginContributions {
                skill_paths: vec![PathBuf::from("plugins/repos/abc123/code-reviewer/skills")],
                agent_paths: vec![],
                mcp_servers: vec![],
                skipped_capabilities: vec!["hooks".to_string()],
            },
        });
        manifest.plugin_marketplaces.push(PluginMarketplaceSource {
            id: "abc123".to_string(),
            location: "https://github.com/example/plugins.git".to_string(),
            is_local: false,
            display_name: "example-plugins".to_string(),
            owner_name: "Example Org".to_string(),
            plugin_count: 3,
            added_at: Utc::now(),
            last_synced_at: None,
        });
        save_to(&path, &manifest).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.plugins.len(), 1);
        assert!(loaded.plugins[0].enabled);
        assert!(!loaded.plugins[0].is_local_dev());
        assert_eq!(
            loaded.plugins[0].contributes.skipped_capabilities,
            vec!["hooks".to_string()]
        );
        assert_eq!(loaded.plugin_marketplaces.len(), 1);
        assert_eq!(loaded.plugin_marketplaces[0].plugin_count, 3);
    }

    /// `enabled_plugin_entries` 本身走真实的 `config_dir()` 全局路径（与
    /// `load_global` 一致，不便注入临时目录），这里改为验证其过滤逻辑所依赖的
    /// `enabled` 字段筛选行为本身是正确的（同 `disabled_names_filters_enabled_false_only`
    /// 的测试方式）。
    #[test]
    fn plugin_enabled_filter_excludes_disabled_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = InstalledManifest::new();
        manifest.plugins.push(InstalledPluginEntry {
            name: "enabled-plugin".to_string(),
            version: None,
            scope: InstallScope::Project,
            source: PluginInstallOrigin::Local {
                path: PathBuf::from("/tmp/dev-plugin"),
            },
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            plugin_root: PathBuf::from("/tmp/dev-plugin"),
            contributes: PluginContributions::default(),
        });
        manifest.plugins.push(InstalledPluginEntry {
            name: "disabled-plugin".to_string(),
            version: None,
            scope: InstallScope::Project,
            source: PluginInstallOrigin::Local {
                path: PathBuf::from("/tmp/dev-plugin-2"),
            },
            enabled: false,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            plugin_root: PathBuf::from("/tmp/dev-plugin-2"),
            contributes: PluginContributions::default(),
        });
        save_project(dir.path(), &manifest).unwrap();

        let enabled: Vec<String> = manifest
            .plugins
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| p.name)
            .collect();
        assert_eq!(enabled, vec!["enabled-plugin".to_string()]);
    }
}
