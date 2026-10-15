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
    /// 本进程加载时读到的**原始 lockfile 文本**，供 `save_to` 做三方 diff 合并。
    ///
    /// 为什么必须存基线而不能「锁内 union 一下」：**卸载是硬删**（`remove_extension`
    /// 走 `retain`，卸载 skill/plugin 直接从数组里 `retain` 掉）。union 会把另一个
    /// 进程刚卸载的条目复活，用户看到「我明明卸载了它又回来了」。
    ///
    /// 存原始文本而不是「本进程改过的 id 集合」：manifest 有 **7 个并行数组**
    /// （mcp_servers / skills / marketplaces / mcp_registries / plugins /
    /// plugin_marketplaces / extensions），卸载改动分散在多个数组里，逐数组维护
    /// 「我删了哪些 id」极容易漏一项——而漏掉的那一项会以「别人的卸载被撤销」
    /// 的形式暴露出来，比丢更新更难解释。
    ///
    /// 存基线而不是改 `save_global`/`save_project` 的签名：调用点有二十几处，
    /// 让每个调用方自己传 base 只会诱使后来者漏传，退化成静默丢更新。
    #[serde(skip)]
    pub baseline_json: Option<String>,
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
            baseline_json: None,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillSource {
    pub marketplace_id: String,
    pub marketplace_url: String,
    /// marketplace.json 里该条目的 path 字段（相对仓库根）
    pub entry_path: String,
    /// HTTP 技能注册表（SkillHub）来源。与上面三个 git marketplace 字段互斥：
    /// 注册表来源只填 `registry`，marketplace 字段留空。
    pub registry: Option<SkillRegistrySource>,
}

impl SkillSource {
    pub fn marketplace(marketplace_id: &str, marketplace_url: &str, entry_path: &str) -> Self {
        Self {
            marketplace_id: marketplace_id.to_string(),
            marketplace_url: marketplace_url.to_string(),
            entry_path: entry_path.to_string(),
            registry: None,
        }
    }

    /// 是否是 HTTP 注册表来源（升级路径与 marketplace 的 git sync 完全不同）。
    pub fn is_registry(&self) -> bool {
        self.registry.is_some()
    }
}

/// 一个 HTTP 技能注册表条目的来源标识。注册表按 `slug` + `namespace` 定位单个
/// 技能，`base_url` 保留是为了支持自建/私有部署实例（换源后旧条目仍知道自己是
/// 从哪装的）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistrySource {
    /// 注册表 API 根，例如 `https://api.skillhub.cn/api/v1`
    pub base_url: String,
    pub slug: String,
    pub namespace: String,
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
    /// Executable/runtime plugin capabilities. Raw manifest values are snapshotted at install
    /// time and resolved relative to `InstalledPluginEntry::plugin_root` when activated.
    #[serde(default)]
    pub runtime: PluginRuntimeContributions,
    /// 因传输仍不支持或名字冲突而被跳过的能力/资源名，
    /// 如 `["ws-mcp:some-server", "skill-conflict:review"]`。
    pub skipped_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginRuntimeContributions {
    #[serde(default)]
    pub hooks: Option<serde_json::Value>,
    #[serde(default)]
    pub output_styles: Option<serde_json::Value>,
    #[serde(default)]
    pub themes: Option<serde_json::Value>,
    #[serde(default)]
    pub channels: Option<serde_json::Value>,
    #[serde(default)]
    pub lsp_servers: Option<serde_json::Value>,
    #[serde(default)]
    pub monitors: Option<serde_json::Value>,
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    #[serde(default)]
    pub user_config: Option<serde_json::Value>,
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
        let mut manifest = InstalledManifest::new();
        // 空基线**必须**记下来：文件不存在是最常见的情形（首次装扩展），
        // 此时若返回 `baseline_json: None`，`save_to` 会退化成整份覆盖，于是两个
        // 实例各自「首次安装」时后一个照样抹掉前一个——恰好把最需要保护的场景
        // 排除在保护之外。
        manifest.baseline_json = serde_json::to_string(&InstalledManifest::new()).ok();
        return Ok(manifest);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 lockfile 失败: {}", path.display()))?;
    let mut manifest: InstalledManifest = serde_json::from_str(&content)
        .with_context(|| format!("解析 lockfile 失败: {}", path.display()))?;
    // v1 文件缺少统一 extensions 数组；保留所有旧字段并在下次写入时升级。
    manifest.version = LOCKFILE_VERSION;
    manifest.baseline_json = Some(content);
    Ok(manifest)
}

fn save_to(path: &Path, manifest: &InstalledManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 lockfile 目录失败: {}", parent.display()))?;
    }
    // 跨进程排他锁：装/卸扩展是 load → 改 → save 的 read-modify-write，此前
    // 两个 wyj-code 同时装东西时后保存者会把对方刚装的条目整个盖掉，
    // 表现为「刚装好的 skill 重启后不见了」。
    let _guard =
        wyj_config::filelock::acquire(&path.with_extension("json.lock"), "installed.json")?;
    let merged = merge_with_disk(path, manifest)?;
    let content = serde_json::to_string_pretty(&merged).context("序列化 lockfile 失败")?;
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

/// 把本进程的改动三方合并到锁内重读的磁盘值上。
///
/// 规则（`base` = 本进程加载时的 manifest，`mine` = 内存态，`disk` = 此刻磁盘值），
/// 对 manifest 里**每一个数组**按 join key 逐条判定：
///
/// | base 有 | mine 有 | 结论 |
/// |---|---|---|
/// | ✓ | ✓ | 本进程可能改过它 → 以 `mine` 为准 |
/// | ✓ | ✗ | **本进程卸载了它** → 从结果删除（不复活） |
/// | ✗ | ✓ | 本进程新装的 → 追加 |
/// | ✗ | ✗ | 本进程没碰 → 保留 `disk` 的 |
///
/// 「base 有而 mine 无」这一格就是**不能**用 union 的原因：union 会把别人刚卸载的
/// 条目复活，用户看到「我明明卸载了，它又回来了」。
///
/// 7 个数组全部走同一条规则——旧数组（`mcp_servers` / `skills` / `plugins` …）目前
/// 仍是运行时的兼容来源，只合并 `extensions` 会让「卸载」在这些数组上失效。
///
/// 无基线（内存构造的 manifest，见测试与迁移路径）时退化为整份覆盖——此时本进程
/// 掌握全部事实，无从也无须合并。
fn merge_with_disk(path: &Path, mine: &InstalledManifest) -> Result<InstalledManifest> {
    let parse_base = || {
        let raw = mine.baseline_json.as_deref()?;
        serde_json::from_str::<InstalledManifest>(raw).ok()
    };
    let Some(base) = parse_base() else {
        return Ok(mine.clone());
    };
    let Ok(disk) = load_from(path) else {
        return Ok(mine.clone());
    };
    let mut merged = disk.clone();
    merged.version = mine.version;
    merged.baseline_json = None;

    merged.mcp_servers = merge_entries(
        &disk.mcp_servers,
        &base.mcp_servers,
        &mine.mcp_servers,
        |e| e.name.clone(),
    );
    merged.skills = merge_entries(&disk.skills, &base.skills, &mine.skills, |e| e.name.clone());
    merged.marketplaces = merge_entries(
        &disk.marketplaces,
        &base.marketplaces,
        &mine.marketplaces,
        |e| e.id.clone(),
    );
    merged.mcp_registries = merge_entries(
        &disk.mcp_registries,
        &base.mcp_registries,
        &mine.mcp_registries,
        |e| e.id.clone(),
    );
    merged.plugins = merge_entries(&disk.plugins, &base.plugins, &mine.plugins, |e| {
        e.name.clone()
    });
    merged.plugin_marketplaces = merge_entries(
        &disk.plugin_marketplaces,
        &base.plugin_marketplaces,
        &mine.plugin_marketplaces,
        |e| e.id.clone(),
    );
    merged.extensions = merge_entries(&disk.extensions, &base.extensions, &mine.extensions, |e| {
        e.id.clone()
    });
    Ok(merged)
}

/// 单个数组的三方合并，见 [`merge_with_disk`] 的规则表。
///
/// `key` 取 `fn(&T) -> String`（而不是返回 `&String` 的闭包）：`impl Fn(&T) -> K`
/// 会要求闭包对**任意**生命周期都能返回 `K`，而借用返回做不到；直接返回 owned 的
/// key 换来一个能编译的签名，一行 `name.clone()` 的开销相对 JSON 序列化可以忽略。
fn merge_entries<T: Clone>(disk: &[T], base: &[T], mine: &[T], key: fn(&T) -> String) -> Vec<T> {
    let base_keys: HashSet<String> = base.iter().map(&key).collect();
    let mine_keys: HashSet<String> = mine.iter().map(&key).collect();
    // 先保留 disk 上「本进程没删」的条目：磁盘上多出来的（别人装的）自动并入。
    let mut result: Vec<T> = disk
        .iter()
        .filter(|entry| {
            let key = key(entry);
            // 保留条件 = 「基线里没有它」或「本进程仍然要它」
            !base_keys.contains(&key) || mine_keys.contains(&key)
        })
        .cloned()
        .collect();
    for entry in mine {
        let entry_key = key(entry);
        match result.iter_mut().find(|e| key(e) == entry_key) {
            Some(existing) => {
                // 磁盘上已有同名条目：不论是本进程改的还是本进程新装的，都以本进程为准
                //（并发装同一 id 时本进程拿到的版本可能更新）。
                *existing = entry.clone();
            }
            None if base_keys.contains(&entry_key) => {
                // mine 有、base 有、disk 却没有 → 别人刚卸载了它。
                // 尊重删除，不复活——否则等于撤销别人刚做的卸载。
            }
            None => {
                // 本进程新装的。
                result.push(entry.clone());
            }
        }
    }
    result.sort_by_key(key);
    result
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

/// 汇总全局 + 项目 lockfile 里 `enabled == false` 的 skill 名称，再 union
/// 项目级 `.wyj-code/settings.toml` 的 `disabled_skills`（按名字禁用，无论
/// 条目来源——手动丢进 `.wyj-code/skills/` 的文件没有 lockfile 记录，只能
/// 靠这层开关禁用），供 `crates/commands::skill::load_skills` 过滤禁用项。
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
    if let Ok(settings) = wyj_config::load_project_settings(cwd) {
        disabled.extend(settings.disabled_skills);
    }
    disabled
}

/// 汇总全局 + 项目 lockfile 里 `enabled == false` 的 MCP server 名称，再
/// union 项目级 `.wyj-code/settings.toml` 的 `disabled_mcp_servers`（同上，
/// 覆盖手写进 `mcp.toml` 而未经 `/extensions install` 的条目），供
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
    if let Ok(settings) = wyj_config::load_project_settings(cwd) {
        disabled.extend(settings.disabled_mcp_servers);
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
            source: Some(SkillSource::marketplace(
                "abc",
                "file:///tmp/x",
                "skills/a.md",
            )),
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
        });
        manifest.skills.push(InstalledSkillEntry {
            name: "disabled-skill".to_string(),
            version: Some("1.0.0".to_string()),
            scope: InstallScope::Global,
            source: Some(SkillSource::marketplace(
                "abc",
                "file:///tmp/x",
                "skills/b.md",
            )),
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

    /// `disabled_skill_names`/`disabled_mcp_names` 也读全局 lockfile（真实
    /// `~/.wyj-code/installed.json`），断言用 `contains` 而非 `len`/`==`，
    /// 避免受运行测试的机器上已有全局条目影响而变得不确定。
    #[test]
    fn disabled_names_union_project_settings_toml() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_settings(
            dir.path(),
            &wyj_config::ProjectSettings {
                disabled_skills: vec!["hand-dropped-skill".to_string()],
                disabled_mcp_servers: vec!["hand-written-server".to_string()],
            },
        )
        .unwrap();

        let disabled_skills = disabled_skill_names(dir.path());
        assert!(disabled_skills.contains("hand-dropped-skill"));

        let disabled_mcp = disabled_mcp_names(dir.path());
        assert!(disabled_mcp.contains("hand-written-server"));
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
                runtime: Default::default(),
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
