//! 插件安装编排：marketplace 源管理 + materialize（clone/复制）+ install/upgrade/
//! uninstall/enable + 启动期只读投影（skill/agent 路径、mcp server 配置）。
//!
//! 插件整体落盘保留自己的目录树（`~/.wyj-code/plugins/repos/<marketplace_id>/<name>/`），
//! 不拆散拷贝进 `~/.wyj-code/skills/`、`~/.claude/agents/` 等既有共享目录：卸载=删一个
//! 目录，upgrade=对整个插件目录重新 clone/复制，与真实 Claude Code（整仓库 clone）的
//! 心智模型一致。插件贡献的 MCP server 配置完全不写入 config.toml/mcp.toml，只存在于
//! lockfile 的 contributes 快照里，由 `mcp_install::effective_mcp_servers` 在合并时读取。
//!
//! 同名资源冲突（跨插件、或插件与用户已有配置）统一为"先到先得，跳过并警告"，但这里
//! 只负责产出贡献快照（`resolve_contributions`），实际的冲突判定发生在消费方
//! （`commands::skill::load_skills` / `core::agent_def::load_agent_defs` /
//! `mcp_install::effective_mcp_servers`），本模块不做冲突检测。

use crate::lockfile::{
    self, InstallScope, InstalledManifest, InstalledPluginEntry, PluginContributions,
    PluginInstallOrigin, PluginMarketplaceSource,
};
use crate::marketplace;
use crate::plugin_manifest::{
    PluginCommandEntry, PluginCommandsField, PluginManifest, PluginManifestPartial,
    PluginMarketplaceEntry, PluginMarketplaceManifest, PluginMcpServerDef, PluginMcpServersField,
    PluginPathListField, PluginSource,
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use wyj_config::{McpServerConfig, McpTransport};

pub struct PluginInstallRequest {
    /// 已解析好的最终 manifest（marketplace.json 内联覆盖已合并）。
    pub manifest: PluginManifest,
    pub source: PluginSource,
    pub marketplace_id: Option<String>,
    pub marketplace_location: Option<String>,
    pub scope: InstallScope,
    pub name_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PluginInstallReport {
    pub name: String,
    pub version: Option<String>,
    pub skill_count: usize,
    pub agent_count: usize,
    pub mcp_count: usize,
    pub skipped_capabilities: Vec<String>,
}

// ─── 路径规划 ──────────────────────────────────────────────────────────────────

fn plugins_root() -> Result<PathBuf> {
    Ok(wyj_config::config_dir()?.join("plugins"))
}

fn plugin_repo_dir_under(plugins_root: &Path, marketplace_id: &str, plugin_name: &str) -> PathBuf {
    plugins_root
        .join("repos")
        .join(marketplace_id)
        .join(plugin_name)
}

/// 插件 marketplace 与 skill marketplace 是分开的命名空间（各自缓存目录不同），
/// 但复用同一个哈希函数取短 id 没有问题（不同 root 下不会互相冲突）。
pub fn plugin_marketplace_id(location: &str) -> String {
    marketplace::marketplace_id(location)
}

fn plugin_marketplace_cache_dir_under(plugins_root: &Path, location: &str) -> PathBuf {
    plugins_root
        .join("marketplaces")
        .join(plugin_marketplace_id(location))
}

fn plugin_marketplace_cache_dir(location: &str) -> Result<PathBuf> {
    Ok(plugin_marketplace_cache_dir_under(
        &plugins_root()?,
        location,
    ))
}

// ─── materialize：clone / 复制 source 到插件自己的目录树 ──────────────────────

async fn clone_repo(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<()> {
    let dest_str = dest.to_string_lossy().to_string();
    let mut args: Vec<&str> = vec!["clone", "--depth", "1"];
    if let Some(r) = git_ref {
        args.push("--branch");
        args.push(r);
    }
    args.push(url);
    args.push(&dest_str);
    let output = Command::new("git")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("执行 git clone 失败（是否已安装 git？）")?;
    if !output.status.success() {
        anyhow::bail!(
            "git clone 失败: {url}\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn pull_repo(dir: &Path) -> Result<()> {
    let dir_str = dir.to_string_lossy().to_string();
    let output = Command::new("git")
        .args(["-C", &dir_str, "pull", "--ff-only"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("执行 git pull 失败（是否已安装 git？）")?;
    if !output.status.success() {
        anyhow::bail!(
            "git pull 失败: {}\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    if !src.exists() {
        anyhow::bail!("源目录不存在: {}", src.display());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("创建目录失败: {}", dest.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("读取目录失败: {}", src.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }
        let src_path = entry.path();
        let dest_path = dest.join(&file_name);
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path).with_context(|| {
                format!(
                    "复制文件失败: {} -> {}",
                    src_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// 把 `source` 物化到 `<plugins_root>/repos/<marketplace_id>/<plugin_name>/`，
/// 已存在则先清空重建（覆盖式安装/升级）。`plugins_root` 抽出为参数便于测试注入
/// 临时目录，避免污染真实 `~/.wyj-code`；对外的 [`materialize_plugin_source`]
/// 固定用真实根目录。
async fn materialize_plugin_source_under(
    plugins_root: &Path,
    source: &PluginSource,
    marketplace_id: &str,
    marketplace_cache_dir: Option<&Path>,
    plugin_name: &str,
) -> Result<PathBuf> {
    let dest = plugin_repo_dir_under(plugins_root, marketplace_id, plugin_name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("清理旧插件目录失败: {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建插件目录失败: {}", parent.display()))?;
    }

    match source {
        PluginSource::LocalPath(rel) => {
            let cache_dir = marketplace_cache_dir.ok_or_else(|| {
                anyhow::anyhow!("本地相对路径 source 缺少 marketplace 缓存目录上下文")
            })?;
            let src = cache_dir.join(rel);
            copy_dir_recursive(&src, &dest)?;
        }
        PluginSource::Github { repo, git_ref, .. } => {
            clone_repo(
                &format!("https://github.com/{repo}.git"),
                git_ref.as_deref(),
                &dest,
            )
            .await?;
        }
        PluginSource::GitUrl { url, git_ref, .. } => {
            clone_repo(url, git_ref.as_deref(), &dest).await?;
        }
        PluginSource::GitSubdir {
            url, path, git_ref, ..
        } => {
            let tmp = tempfile::tempdir().context("创建临时目录失败")?;
            clone_repo(url, git_ref.as_deref(), tmp.path()).await?;
            let src = tmp.path().join(path);
            copy_dir_recursive(&src, &dest)?;
        }
        PluginSource::NpmUnsupported { .. } => {
            anyhow::bail!(
                "暂不支持从 npm 包安装插件，请使用 github/git-url/git-subdir/本地路径来源"
            );
        }
    }
    Ok(dest)
}

// ─── manifest 解析：读取插件自己的 plugin.json / strict=false 容错 / 覆盖合并 ──

fn read_own_plugin_manifest(
    plugin_root: &Path,
    strict: bool,
    fallback_name: &str,
) -> Result<PluginManifest> {
    let path = plugin_root.join(".claude-plugin").join("plugin.json");
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 plugin.json 失败: {}", path.display()))?;
        return serde_json::from_str(&content)
            .with_context(|| format!("解析 plugin.json 失败: {}", path.display()));
    }
    if strict {
        anyhow::bail!(
            "插件目录缺少 .claude-plugin/plugin.json（strict 模式要求必须存在）: {}",
            plugin_root.display()
        );
    }
    Ok(synthesize_manifest_from_conventional_dirs(
        plugin_root,
        fallback_name,
    ))
}

/// `strict=false` 且没有 plugin.json 时的容错：只识别约定俗成的 `commands/`
/// 和 `skills/` 两个子目录名，其余一律不识别。
fn synthesize_manifest_from_conventional_dirs(plugin_root: &Path, name: &str) -> PluginManifest {
    let commands = if plugin_root.join("commands").is_dir() {
        Some(PluginCommandsField::Single("commands".to_string()))
    } else {
        None
    };
    let skills = if plugin_root.join("skills").is_dir() {
        Some(PluginPathListField::Single("skills".to_string()))
    } else {
        None
    };
    PluginManifest {
        name: name.to_string(),
        version: None,
        description: None,
        author: None,
        homepage: None,
        repository: None,
        license: None,
        keywords: Vec::new(),
        dependencies: Vec::new(),
        commands,
        agents: None,
        skills,
        mcp_servers: None,
        hooks: None,
        output_styles: None,
        themes: None,
        channels: None,
        lsp_servers: None,
        monitors: None,
        settings: None,
        user_config: None,
    }
}

/// marketplace.json 条目里的内联字段覆盖插件自己 plugin.json 里的同名字段。
fn merge_manifest_override(own: PluginManifest, over: &PluginManifestPartial) -> PluginManifest {
    PluginManifest {
        name: over.name.clone().unwrap_or(own.name),
        version: over.version.clone().or(own.version),
        description: over.description.clone().or(own.description),
        author: over.author.clone().or(own.author),
        homepage: over.homepage.clone().or(own.homepage),
        repository: over.repository.clone().or(own.repository),
        license: over.license.clone().or(own.license),
        keywords: if over.keywords.is_empty() {
            own.keywords
        } else {
            over.keywords.clone()
        },
        dependencies: if over.dependencies.is_empty() {
            own.dependencies
        } else {
            over.dependencies.clone()
        },
        commands: over.commands.clone().or(own.commands),
        agents: over.agents.clone().or(own.agents),
        skills: over.skills.clone().or(own.skills),
        mcp_servers: over.mcp_servers.clone().or(own.mcp_servers),
        hooks: over.hooks.clone().or(own.hooks),
        output_styles: over.output_styles.clone().or(own.output_styles),
        themes: over.themes.clone().or(own.themes),
        channels: over.channels.clone().or(own.channels),
        lsp_servers: over.lsp_servers.clone().or(own.lsp_servers),
        monitors: over.monitors.clone().or(own.monitors),
        settings: over.settings.clone().or(own.settings),
        user_config: over.user_config.clone().or(own.user_config),
    }
}

fn source_name_hint(source: &PluginSource) -> Option<String> {
    fn last_segment(p: &str) -> Option<String> {
        let trimmed = p.trim_end_matches('/').trim_end_matches(".git");
        trimmed.rsplit('/').next().map(|s| s.to_string())
    }
    match source {
        PluginSource::LocalPath(p) => last_segment(p),
        PluginSource::Github { repo, .. } => repo.rsplit('/').next().map(|s| s.to_string()),
        PluginSource::GitUrl { url, .. } => last_segment(url),
        PluginSource::GitSubdir { path, .. } => last_segment(path),
        PluginSource::NpmUnsupported { package, .. } => Some(package.clone()),
    }
}

// ─── 贡献解析：从 manifest + 已物化的 plugin_root 算出 skill/agent/mcp 快照 ──────

fn command_entry_content(
    name: &str,
    entry: &PluginCommandEntry,
    plugin_root: &Path,
) -> Option<String> {
    if let Some(source) = &entry.source {
        return std::fs::read_to_string(plugin_root.join(source)).ok();
    }
    let desc = entry
        .description
        .clone()
        .unwrap_or_else(|| name.to_string());
    Some(format!(
        "# {desc}\n\n{}",
        entry.content.clone().unwrap_or_default()
    ))
}

fn to_mcp_server_config(name: &str, def: &PluginMcpServerDef) -> Option<McpServerConfig> {
    match def {
        PluginMcpServerDef::Stdio { command, args, env } => Some(McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(command.clone()),
            args: args.clone(),
            env: env.clone(),
        }),
        _ => None,
    }
}

fn load_mcp_json_file(path: &Path) -> Result<HashMap<String, PluginMcpServerDef>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 mcpServers 文件失败: {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("解析 mcpServers 文件失败: {}", path.display()))?;
    let servers_value = value.get("mcpServers").cloned().unwrap_or(value);
    serde_json::from_value(servers_value)
        .with_context(|| format!("解析 mcpServers 内容失败: {}", path.display()))
}

/// 从 manifest + 已物化的 plugin_root 解析出 skill_paths/agent_paths/mcp_servers，
/// 并统计 skipped_capabilities（不支持的能力字段 + 非 stdio 传输的 mcp server）。
/// 注意：不做同名冲突检测——冲突判定发生在消费方（load_skills/load_agent_defs/
/// effective_mcp_servers），这里只产出原始贡献快照。
pub fn resolve_contributions(manifest: &PluginManifest, plugin_root: &Path) -> PluginContributions {
    let mut skipped: Vec<String> = manifest
        .unsupported_capability_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    let mut skill_paths: Vec<PathBuf> = manifest
        .skills
        .clone()
        .map(PluginPathListField::into_paths)
        .unwrap_or_default()
        .into_iter()
        .map(|p| plugin_root.join(p))
        .collect();

    let agent_paths: Vec<PathBuf> = manifest
        .agents
        .clone()
        .map(PluginPathListField::into_paths)
        .unwrap_or_default()
        .into_iter()
        .map(|p| plugin_root.join(p))
        .collect();

    // commands 字段（字符串/数组形态）当作额外的 skill 路径处理——wyj-code 用
    // 同一套 markdown 加载器处理两者；Map 形态（内联 content）物化成合成 `.md` 文件。
    if let Some(commands) = &manifest.commands {
        match commands {
            PluginCommandsField::Single(p) => skill_paths.push(plugin_root.join(p)),
            PluginCommandsField::Multiple(v) => {
                skill_paths.extend(v.iter().map(|p| plugin_root.join(p)));
            }
            PluginCommandsField::Map(map) => {
                let generated_dir = plugin_root.join(".wyj-generated").join("commands");
                for (cmd_name, entry) in map {
                    if let Some(source) = &entry.source {
                        skill_paths.push(plugin_root.join(source));
                        continue;
                    }
                    if std::fs::create_dir_all(&generated_dir).is_ok() {
                        if let Some(content) = command_entry_content(cmd_name, entry, plugin_root) {
                            let dest = generated_dir.join(format!("{cmd_name}.md"));
                            if std::fs::write(&dest, content).is_ok() {
                                skill_paths.push(dest);
                            }
                        }
                    }
                }
            }
        }
    }

    let mut mcp_servers = Vec::new();
    if let Some(field) = &manifest.mcp_servers {
        let entries: Vec<(String, PluginMcpServerDef)> = match field {
            PluginMcpServersField::Map(map) => {
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            }
            PluginMcpServersField::Path(rel) => {
                let path = plugin_root.join(rel);
                match load_mcp_json_file(&path) {
                    Ok(map) => map.into_iter().collect(),
                    Err(e) => {
                        tracing::warn!("读取插件 mcpServers 文件失败 {}: {e}", path.display());
                        skipped.push("mcpServers(读取失败)".to_string());
                        Vec::new()
                    }
                }
            }
        };
        for (name, def) in entries {
            match to_mcp_server_config(&name, &def) {
                Some(cfg) => mcp_servers.push(cfg),
                None => skipped.push(format!("{}-mcp:{name}", def.transport_label())),
            }
        }
    }

    PluginContributions {
        skill_paths,
        agent_paths,
        mcp_servers,
        skipped_capabilities: skipped,
    }
}

// ─── lockfile 落盘 ────────────────────────────────────────────────────────────

fn upsert_plugin_entry(manifest: &mut InstalledManifest, entry: InstalledPluginEntry) {
    if let Some(existing) = manifest.plugins.iter_mut().find(|e| e.name == entry.name) {
        *existing = entry;
    } else {
        manifest.plugins.push(entry);
    }
}

fn finalize_plugin_install(
    name: &str,
    manifest: &PluginManifest,
    plugin_root: PathBuf,
    origin: PluginInstallOrigin,
    scope: InstallScope,
    cwd: &Path,
) -> Result<PluginInstallReport> {
    let contributes = resolve_contributions(manifest, &plugin_root);

    let mut manifest_lock = lockfile::load_scope(scope, cwd)?;
    let now = Utc::now();
    let existing_installed_at = manifest_lock
        .plugins
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.installed_at);
    upsert_plugin_entry(
        &mut manifest_lock,
        InstalledPluginEntry {
            name: name.to_string(),
            version: manifest.version.clone(),
            scope,
            source: origin,
            enabled: true,
            installed_at: existing_installed_at.unwrap_or(now),
            updated_at: now,
            plugin_root,
            contributes: contributes.clone(),
        },
    );
    lockfile::save_scope(scope, cwd, &manifest_lock)?;

    Ok(PluginInstallReport {
        name: name.to_string(),
        version: manifest.version.clone(),
        skill_count: contributes.skill_paths.len(),
        agent_count: contributes.agent_paths.len(),
        mcp_count: contributes.mcp_servers.len(),
        skipped_capabilities: contributes.skipped_capabilities,
    })
}

async fn install_plugin_under(
    plugins_root: &Path,
    req: &PluginInstallRequest,
    cwd: &Path,
) -> Result<PluginInstallReport> {
    let name = req
        .name_override
        .clone()
        .unwrap_or_else(|| req.manifest.name.clone());
    let marketplace_id = req
        .marketplace_id
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let marketplace_cache_dir = match &req.marketplace_location {
        Some(loc) if Path::new(loc).exists() => Some(PathBuf::from(loc)),
        Some(loc) => Some(plugin_marketplace_cache_dir_under(plugins_root, loc)),
        None => None,
    };

    let plugin_root = materialize_plugin_source_under(
        plugins_root,
        &req.source,
        &marketplace_id,
        marketplace_cache_dir.as_deref(),
        &name,
    )
    .await?;

    let origin = PluginInstallOrigin::Marketplace {
        marketplace_id,
        marketplace_location: req.marketplace_location.clone().unwrap_or_default(),
    };
    finalize_plugin_install(&name, &req.manifest, plugin_root, origin, req.scope, cwd)
}

/// 安装（首次写入）或"覆盖式重装"（同 scope 同名已存在直接覆盖）。要求
/// `req.manifest` 已经是最终解析好的 manifest（marketplace 内联覆盖已合并）——
/// TUI Browse→Install 流程请用 [`resolve_and_install_from_marketplace`]。
pub async fn install_plugin(req: &PluginInstallRequest, cwd: &Path) -> Result<PluginInstallReport> {
    install_plugin_under(&plugins_root()?, req, cwd).await
}

/// TUI Browse→Install 流程的高层入口：物化 source → 读取插件自己的
/// `.claude-plugin/plugin.json`（`entry.strict=true` 要求必须存在，否则报错；
/// `strict=false` 时退化为只识别 `commands/`/`skills/` 两个约定子目录）→ 与
/// marketplace.json 内联覆盖合并 → 落盘 lockfile。
pub async fn resolve_and_install_from_marketplace(
    marketplace_id: &str,
    marketplace_location: &str,
    entry: &PluginMarketplaceEntry,
    scope: InstallScope,
    name_override: Option<String>,
    cwd: &Path,
) -> Result<PluginInstallReport> {
    let marketplace_cache_dir = if Path::new(marketplace_location).exists() {
        PathBuf::from(marketplace_location)
    } else {
        plugin_marketplace_cache_dir(marketplace_location)?
    };

    let staging_name = name_override
        .clone()
        .or_else(|| entry.manifest.name.clone())
        .or_else(|| source_name_hint(&entry.source))
        .ok_or_else(|| {
            anyhow::anyhow!("无法确定插件名：marketplace 条目未提供 name 也无法从 source 推断")
        })?;

    let root = plugins_root()?;
    let mut plugin_root = materialize_plugin_source_under(
        &root,
        &entry.source,
        marketplace_id,
        Some(marketplace_cache_dir.as_path()),
        &staging_name,
    )
    .await?;

    let own = read_own_plugin_manifest(&plugin_root, entry.strict, &staging_name)?;
    let manifest = merge_manifest_override(own, &entry.manifest);
    let final_name = name_override.unwrap_or_else(|| manifest.name.clone());

    if final_name != staging_name {
        let renamed = plugin_repo_dir_under(&root, marketplace_id, &final_name);
        if renamed.exists() {
            std::fs::remove_dir_all(&renamed).ok();
        }
        std::fs::rename(&plugin_root, &renamed).with_context(|| {
            format!(
                "重命名插件目录失败: {} -> {}",
                plugin_root.display(),
                renamed.display()
            )
        })?;
        plugin_root = renamed;
    }

    let origin = PluginInstallOrigin::Marketplace {
        marketplace_id: marketplace_id.to_string(),
        marketplace_location: marketplace_location.to_string(),
    };
    finalize_plugin_install(&final_name, &manifest, plugin_root, origin, scope, cwd)
}

/// 升级：对 marketplace 来源重新 sync marketplace + 重新物化插件目录（覆盖式，
/// 版本号是否变化在写入前后比较）；本地开发来源没有"升级"概念，直接拒绝。
pub async fn upgrade_plugin(
    name: &str,
    scope: InstallScope,
    cwd: &Path,
) -> Result<crate::UpgradeOutcome> {
    let manifest = lockfile::load_scope(scope, cwd)?;
    let entry = manifest
        .plugins
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("未找到已安装的插件: {name}"))?;
    let PluginInstallOrigin::Marketplace {
        marketplace_id,
        marketplace_location,
    } = entry.source.clone()
    else {
        anyhow::bail!("'{name}' 是本地开发插件，没有版本信息，无法升级");
    };
    let previous_version = entry.version.clone();

    let marketplace_manifest = sync_plugin_marketplace(&marketplace_id).await?;
    let plugin_entry = marketplace_manifest
        .plugins
        .iter()
        .find(|p| p.manifest.name.as_deref() == Some(name))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("marketplace 中未找到插件: {name}"))?;

    let report = resolve_and_install_from_marketplace(
        &marketplace_id,
        &marketplace_location,
        &plugin_entry,
        scope,
        Some(name.to_string()),
        cwd,
    )
    .await?;

    let new_version = report.version.unwrap_or_default();
    if Some(new_version.as_str()) == previous_version.as_deref() {
        Ok(crate::UpgradeOutcome::AlreadyLatest {
            version: new_version,
        })
    } else {
        Ok(crate::UpgradeOutcome::Upgraded {
            version: new_version,
        })
    }
}

/// 卸载：删除物化目录（本地开发来源不删用户目录）+ 从 lockfile 删除记录。
pub fn uninstall_plugin(name: &str, scope: InstallScope, cwd: &Path) -> Result<()> {
    let mut manifest = lockfile::load_scope(scope, cwd)?;
    let Some(pos) = manifest.plugins.iter().position(|e| e.name == name) else {
        return Ok(());
    };
    let entry = manifest.plugins.remove(pos);
    lockfile::save_scope(scope, cwd, &manifest)?;

    if !entry.is_local_dev() && entry.plugin_root.exists() {
        std::fs::remove_dir_all(&entry.plugin_root)
            .with_context(|| format!("删除插件目录失败: {}", entry.plugin_root.display()))?;
    }
    Ok(())
}

/// 启用/禁用（整体开关，仅改 lockfile.enabled，不删物化目录）。
pub fn set_plugin_enabled(
    name: &str,
    scope: InstallScope,
    cwd: &Path,
    enabled: bool,
) -> Result<()> {
    let mut manifest = lockfile::load_scope(scope, cwd)?;
    let entry = manifest
        .plugins
        .iter_mut()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("未找到已安装的插件: {name}"))?;
    entry.enabled = enabled;
    entry.updated_at = Utc::now();
    lockfile::save_scope(scope, cwd, &manifest)
}

// ─── 启动期 / 加载器消费的只读投影 ──────────────────────────────────────────────

pub fn enabled_plugin_skill_paths(cwd: &Path) -> Vec<PathBuf> {
    lockfile::enabled_plugin_entries(cwd)
        .into_iter()
        .flat_map(|e| e.contributes.skill_paths)
        .collect()
}

pub fn enabled_plugin_agent_paths(cwd: &Path) -> Vec<PathBuf> {
    lockfile::enabled_plugin_entries(cwd)
        .into_iter()
        .flat_map(|e| e.contributes.agent_paths)
        .collect()
}

pub fn plugin_mcp_servers(cwd: &Path) -> Vec<(String, McpServerConfig)> {
    lockfile::enabled_plugin_entries(cwd)
        .into_iter()
        .flat_map(|e| {
            let name = e.name.clone();
            e.contributes
                .mcp_servers
                .into_iter()
                .map(move |s| (name.clone(), s))
        })
        .collect()
}

// ─── marketplace 源管理三件套 ──────────────────────────────────────────────────

pub fn add_plugin_marketplace(location: &str) -> Result<PluginMarketplaceSource> {
    let is_local = Path::new(location).exists();
    let id = plugin_marketplace_id(location);
    let mut manifest = lockfile::load_global()?;
    if let Some(existing) = manifest.plugin_marketplaces.iter().find(|m| m.id == id) {
        return Ok(existing.clone());
    }
    let source = PluginMarketplaceSource {
        id,
        location: location.to_string(),
        is_local,
        // 首次添加时还未 sync，用 location 占位；sync 成功后由
        // mark_plugin_marketplace_synced 用 marketplace.json 里的真实值回填。
        display_name: location.to_string(),
        owner_name: String::new(),
        plugin_count: 0,
        added_at: Utc::now(),
        last_synced_at: None,
    };
    manifest.plugin_marketplaces.push(source.clone());
    lockfile::save_global(&manifest)?;
    Ok(source)
}

pub fn remove_plugin_marketplace(id: &str) -> Result<()> {
    let mut manifest = lockfile::load_global()?;
    let Some(pos) = manifest.plugin_marketplaces.iter().position(|m| m.id == id) else {
        return Ok(());
    };
    let source = manifest.plugin_marketplaces.remove(pos);
    lockfile::save_global(&manifest)?;

    if !source.is_local {
        let dir = plugin_marketplace_cache_dir(&source.location)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
    }
    Ok(())
}

pub fn list_plugin_marketplaces() -> Result<Vec<PluginMarketplaceSource>> {
    Ok(lockfile::load_global()?.plugin_marketplaces)
}

fn parse_marketplace_manifest(dir: &Path) -> Result<PluginMarketplaceManifest> {
    let nested = dir.join(".claude-plugin").join("marketplace.json");
    let manifest_path = if nested.exists() {
        nested
    } else {
        // 容错：部分仓库把清单直接放在根目录而不是 .claude-plugin/ 下。
        dir.join("marketplace.json")
    };
    let content = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("读取 marketplace.json 失败: {}", manifest_path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("解析 marketplace.json 失败: {}", manifest_path.display()))
}

async fn sync_plugin_marketplace_at_under(
    marketplaces_root: &Path,
    location: &str,
    is_local: bool,
) -> Result<PluginMarketplaceManifest> {
    let dir = if is_local {
        PathBuf::from(location)
    } else {
        let dir = marketplaces_root.join(plugin_marketplace_id(location));
        if dir.join(".git").exists() {
            pull_repo(&dir).await?;
        } else {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            clone_repo(location, None, &dir).await?;
        }
        dir
    };
    parse_marketplace_manifest(&dir)
}

fn mark_plugin_marketplace_synced(id: &str, manifest: &PluginMarketplaceManifest) -> Result<()> {
    let mut lock = lockfile::load_global()?;
    if let Some(m) = lock.plugin_marketplaces.iter_mut().find(|m| m.id == id) {
        m.last_synced_at = Some(Utc::now());
        m.display_name = manifest.name.clone();
        m.owner_name = manifest.owner.name.clone();
        m.plugin_count = manifest.plugins.len();
        lockfile::save_global(&lock)?;
    }
    Ok(())
}

/// clone/pull 已添加的插件 marketplace 源并重新解析 `marketplace.json`。
pub async fn sync_plugin_marketplace(id: &str) -> Result<PluginMarketplaceManifest> {
    let sources = list_plugin_marketplaces()?;
    let source = sources
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow::anyhow!("未找到插件 marketplace 源: {id}"))?;
    let manifest = sync_plugin_marketplace_at_under(
        &plugins_root()?.join("marketplaces"),
        &source.location,
        source.is_local,
    )
    .await?;
    mark_plugin_marketplace_synced(id, &manifest)?;
    Ok(manifest)
}

// ─── 本地开发插件 ──────────────────────────────────────────────────────────────

/// 纯解析，不落盘：`--plugin-dir` 场景专用。容错模式（`strict=false`）为主，
/// 便于开发中的插件在补全 plugin.json 之前也能被识别 commands/skills 目录。
pub fn load_local_plugin(path: &Path) -> Result<PluginManifest> {
    let fallback_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("local-plugin");
    read_own_plugin_manifest(path, false, fallback_name)
}

/// TUI「添加本地插件」专用：解析 + 落盘 lockfile（source = Local），会持续出现在
/// Installed 列表、可 enable/disable/uninstall，但没有 upgrade。与 `--plugin-dir`
/// 的临时加载（不落盘、仅当次进程生效）是两条独立路径。
pub fn install_local_plugin(
    path: &Path,
    scope: InstallScope,
    cwd: &Path,
) -> Result<PluginInstallReport> {
    let manifest = load_local_plugin(path)?;
    let origin = PluginInstallOrigin::Local {
        path: path.to_path_buf(),
    };
    finalize_plugin_install(
        &manifest.name.clone(),
        &manifest,
        path.to_path_buf(),
        origin,
        scope,
        cwd,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_manifest::PluginAuthor;
    use std::process::Command as StdCommand;

    fn init_git_repo(root: &Path) {
        StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(root)
            .status()
            .unwrap();
    }

    fn commit_all(root: &Path) {
        StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(root)
            .status()
            .unwrap();
    }

    fn sample_manifest(name: &str) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: Some("测试插件".to_string()),
            author: Some(PluginAuthor {
                name: "Tester".to_string(),
                email: None,
                url: None,
            }),
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            dependencies: vec![],
            commands: None,
            agents: Some(PluginPathListField::Single("agents".to_string())),
            skills: Some(PluginPathListField::Single("skills".to_string())),
            mcp_servers: None,
            hooks: None,
            output_styles: None,
            themes: None,
            channels: None,
            lsp_servers: None,
            monitors: None,
            settings: None,
            user_config: None,
        }
    }

    #[test]
    fn resolve_contributions_skips_unsupported_capabilities_and_transports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();

        let mut manifest = sample_manifest("with-caps");
        manifest.hooks = Some(serde_json::json!({"PreToolUse": []}));
        manifest.themes = Some(serde_json::json!(["dark"]));
        let mut mcp_map: HashMap<String, PluginMcpServerDef> = HashMap::new();
        mcp_map.insert(
            "local-tool".to_string(),
            PluginMcpServerDef::Stdio {
                command: "node".to_string(),
                args: vec!["server.js".to_string()],
                env: HashMap::new(),
            },
        );
        mcp_map.insert(
            "remote-tool".to_string(),
            PluginMcpServerDef::Sse {
                url: "https://example.com/mcp".to_string(),
                headers: HashMap::new(),
                oauth: None,
            },
        );
        manifest.mcp_servers = Some(PluginMcpServersField::Map(mcp_map));

        let contributes = resolve_contributions(&manifest, dir.path());
        assert_eq!(contributes.skill_paths.len(), 1);
        assert_eq!(contributes.agent_paths.len(), 1);
        assert_eq!(contributes.mcp_servers.len(), 1);
        assert_eq!(contributes.mcp_servers[0].name, "local-tool");
        assert!(contributes
            .skipped_capabilities
            .contains(&"hooks".to_string()));
        assert!(contributes
            .skipped_capabilities
            .contains(&"themes".to_string()));
        assert!(contributes
            .skipped_capabilities
            .iter()
            .any(|s| s.contains("sse-mcp:remote-tool")));
    }

    #[test]
    fn resolve_contributions_generates_markdown_for_inline_command_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = sample_manifest("inline-cmd");
        manifest.skills = None;
        manifest.agents = None;
        let mut cmd_map: HashMap<String, PluginCommandEntry> = HashMap::new();
        cmd_map.insert(
            "greet".to_string(),
            PluginCommandEntry {
                source: None,
                content: Some("Say hello to $ARGUMENTS".to_string()),
                description: Some("Greet someone".to_string()),
                argument_hint: None,
                model: None,
                allowed_tools: None,
            },
        );
        manifest.commands = Some(PluginCommandsField::Map(cmd_map));

        let contributes = resolve_contributions(&manifest, dir.path());
        assert_eq!(contributes.skill_paths.len(), 1);
        let generated = std::fs::read_to_string(&contributes.skill_paths[0]).unwrap();
        assert!(generated.contains("# Greet someone"));
        assert!(generated.contains("Say hello to $ARGUMENTS"));
    }

    #[tokio::test]
    async fn materialize_git_url_source_clones_into_plugins_root() {
        let repo_dir = tempfile::tempdir().unwrap();
        init_git_repo(repo_dir.path());
        std::fs::create_dir_all(repo_dir.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            repo_dir.path().join(".claude-plugin").join("plugin.json"),
            r#"{"name":"my-plugin","version":"1.0.0"}"#,
        )
        .unwrap();
        commit_all(repo_dir.path());

        let plugins_root = tempfile::tempdir().unwrap();
        let git_url = format!("file://{}", repo_dir.path().display());
        let source = PluginSource::GitUrl {
            url: git_url,
            git_ref: None,
            sha: None,
        };
        let dest = materialize_plugin_source_under(
            plugins_root.path(),
            &source,
            "marketplace-abc",
            None,
            "my-plugin",
        )
        .await
        .unwrap();

        assert!(dest.join(".claude-plugin").join("plugin.json").exists());
        assert_eq!(
            dest,
            plugins_root
                .path()
                .join("repos")
                .join("marketplace-abc")
                .join("my-plugin")
        );
    }

    #[tokio::test]
    async fn materialize_git_subdir_source_copies_only_subpath() {
        let repo_dir = tempfile::tempdir().unwrap();
        init_git_repo(repo_dir.path());
        std::fs::create_dir_all(repo_dir.path().join("plugins").join("foo")).unwrap();
        std::fs::write(
            repo_dir
                .path()
                .join("plugins")
                .join("foo")
                .join("marker.txt"),
            "foo",
        )
        .unwrap();
        std::fs::create_dir_all(repo_dir.path().join("plugins").join("bar")).unwrap();
        std::fs::write(
            repo_dir
                .path()
                .join("plugins")
                .join("bar")
                .join("marker.txt"),
            "bar",
        )
        .unwrap();
        commit_all(repo_dir.path());

        let plugins_root = tempfile::tempdir().unwrap();
        let source = PluginSource::GitSubdir {
            url: format!("file://{}", repo_dir.path().display()),
            path: "plugins/foo".to_string(),
            git_ref: None,
            sha: None,
        };
        let dest = materialize_plugin_source_under(plugins_root.path(), &source, "mp", None, "foo")
            .await
            .unwrap();

        assert!(dest.join("marker.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("marker.txt")).unwrap(),
            "foo"
        );
        assert!(!dest.join("foo").exists());
        assert!(!dest.join("bar").exists());
    }

    #[tokio::test]
    async fn materialize_local_path_source_copies_from_marketplace_cache() {
        let marketplace_cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(marketplace_cache.path().join("plugins").join("my-plugin"))
            .unwrap();
        std::fs::write(
            marketplace_cache
                .path()
                .join("plugins")
                .join("my-plugin")
                .join("marker.txt"),
            "hello",
        )
        .unwrap();

        let plugins_root = tempfile::tempdir().unwrap();
        let source = PluginSource::LocalPath("plugins/my-plugin".to_string());
        let dest = materialize_plugin_source_under(
            plugins_root.path(),
            &source,
            "mp",
            Some(marketplace_cache.path()),
            "my-plugin",
        )
        .await
        .unwrap();

        assert!(dest.join("marker.txt").exists());
    }

    fn make_marketplace_entry(
        name: &str,
        plugin_root: PathBuf,
        enabled: bool,
    ) -> InstalledPluginEntry {
        make_entry_with_origin(
            name,
            plugin_root,
            enabled,
            PluginInstallOrigin::Marketplace {
                marketplace_id: "mp".to_string(),
                marketplace_location: "https://example.com/plugins.git".to_string(),
            },
        )
    }

    fn make_entry(name: &str, plugin_root: PathBuf, enabled: bool) -> InstalledPluginEntry {
        make_entry_with_origin(
            name,
            plugin_root.clone(),
            enabled,
            PluginInstallOrigin::Local { path: plugin_root },
        )
    }

    fn make_entry_with_origin(
        name: &str,
        plugin_root: PathBuf,
        enabled: bool,
        source: PluginInstallOrigin,
    ) -> InstalledPluginEntry {
        InstalledPluginEntry {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            scope: InstallScope::Project,
            source,
            enabled,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            plugin_root,
            contributes: PluginContributions {
                skill_paths: vec![PathBuf::from("skills")],
                agent_paths: vec![PathBuf::from("agents")],
                mcp_servers: vec![McpServerConfig {
                    name: format!("{name}-tool"),
                    transport: McpTransport::Stdio,
                    command: Some("node".to_string()),
                    args: vec![],
                    env: HashMap::new(),
                }],
                skipped_capabilities: vec![],
            },
        }
    }

    #[test]
    fn uninstall_plugin_removes_directory_and_lockfile_entry() {
        let cwd = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::write(plugin_dir.path().join("marker.txt"), "x").unwrap();

        let mut manifest = lockfile::load_project(cwd.path()).unwrap();
        manifest.plugins.push(make_marketplace_entry(
            "demo",
            plugin_dir.path().to_path_buf(),
            true,
        ));
        lockfile::save_project(cwd.path(), &manifest).unwrap();

        uninstall_plugin("demo", InstallScope::Project, cwd.path()).unwrap();

        assert!(!plugin_dir.path().exists());
        let after = lockfile::load_project(cwd.path()).unwrap();
        assert!(after.plugins.is_empty());
    }

    #[test]
    fn uninstall_local_dev_plugin_keeps_user_directory() {
        let cwd = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();

        let mut manifest = lockfile::load_project(cwd.path()).unwrap();
        manifest.plugins.push(make_entry(
            "dev-plugin",
            plugin_dir.path().to_path_buf(),
            true,
        ));
        lockfile::save_project(cwd.path(), &manifest).unwrap();

        uninstall_plugin("dev-plugin", InstallScope::Project, cwd.path()).unwrap();

        assert!(plugin_dir.path().exists()); // 本地开发来源不删用户目录
        let after = lockfile::load_project(cwd.path()).unwrap();
        assert!(after.plugins.is_empty());
    }

    #[test]
    fn set_plugin_enabled_toggles_flag() {
        let cwd = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        let mut manifest = lockfile::load_project(cwd.path()).unwrap();
        manifest
            .plugins
            .push(make_entry("demo", plugin_dir.path().to_path_buf(), true));
        lockfile::save_project(cwd.path(), &manifest).unwrap();

        set_plugin_enabled("demo", InstallScope::Project, cwd.path(), false).unwrap();
        let after = lockfile::load_project(cwd.path()).unwrap();
        assert!(!after.plugins[0].enabled);
    }

    #[test]
    fn enabled_projections_exclude_disabled_plugins() {
        let cwd = tempfile::tempdir().unwrap();
        let enabled_dir = tempfile::tempdir().unwrap();
        let disabled_dir = tempfile::tempdir().unwrap();
        let mut manifest = lockfile::load_project(cwd.path()).unwrap();
        manifest.plugins.push(make_entry(
            "enabled-plugin",
            enabled_dir.path().to_path_buf(),
            true,
        ));
        manifest.plugins.push(make_entry(
            "disabled-plugin",
            disabled_dir.path().to_path_buf(),
            false,
        ));
        lockfile::save_project(cwd.path(), &manifest).unwrap();

        let skills = enabled_plugin_skill_paths(cwd.path());
        let agents = enabled_plugin_agent_paths(cwd.path());
        let servers = plugin_mcp_servers(cwd.path());

        assert_eq!(skills.len(), 1);
        assert_eq!(agents.len(), 1);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "enabled-plugin");
        assert_eq!(servers[0].1.name, "enabled-plugin-tool");
    }

    #[test]
    fn load_local_plugin_reads_own_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.path().join(".claude-plugin").join("plugin.json"),
            r#"{"name":"dev-plugin","version":"0.1.0"}"#,
        )
        .unwrap();

        let manifest = load_local_plugin(dir.path()).unwrap();
        assert_eq!(manifest.name, "dev-plugin");
        assert_eq!(manifest.version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn load_local_plugin_synthesizes_when_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("commands")).unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();

        let manifest = load_local_plugin(dir.path()).unwrap();
        assert!(manifest.commands.is_some());
        assert!(manifest.skills.is_some());
        assert!(manifest.agents.is_none());
    }

    #[test]
    fn install_local_plugin_writes_lockfile_entry() {
        let cwd = tempfile::tempdir().unwrap();
        let plugin_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plugin_dir.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            plugin_dir.path().join(".claude-plugin").join("plugin.json"),
            r#"{"name":"dev-plugin","version":"0.1.0"}"#,
        )
        .unwrap();

        let report =
            install_local_plugin(plugin_dir.path(), InstallScope::Project, cwd.path()).unwrap();
        assert_eq!(report.name, "dev-plugin");

        let manifest = lockfile::load_project(cwd.path()).unwrap();
        assert_eq!(manifest.plugins.len(), 1);
        assert!(manifest.plugins[0].is_local_dev());
        assert_eq!(manifest.plugins[0].plugin_root, plugin_dir.path());
    }

    #[test]
    fn merge_manifest_override_prefers_partial_when_present() {
        let own = sample_manifest("own-name");
        let partial = PluginManifestPartial {
            name: Some("override-name".to_string()),
            version: None,
            description: Some("覆盖描述".to_string()),
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            dependencies: vec![],
            commands: None,
            agents: None,
            skills: None,
            mcp_servers: None,
            hooks: None,
            output_styles: None,
            themes: None,
            channels: None,
            lsp_servers: None,
            monitors: None,
            settings: None,
            user_config: None,
        };
        let merged = merge_manifest_override(own, &partial);
        assert_eq!(merged.name, "override-name");
        assert_eq!(merged.description.as_deref(), Some("覆盖描述"));
        assert_eq!(merged.version.as_deref(), Some("1.0.0")); // 未覆盖，保留原值
    }

    #[test]
    fn source_name_hint_extracts_last_segment() {
        assert_eq!(
            source_name_hint(&PluginSource::Github {
                repo: "owner/my-plugin".to_string(),
                git_ref: None,
                sha: None
            }),
            Some("my-plugin".to_string())
        );
        assert_eq!(
            source_name_hint(&PluginSource::GitUrl {
                url: "https://example.com/repo/my-plugin.git".to_string(),
                git_ref: None,
                sha: None
            }),
            Some("my-plugin".to_string())
        );
    }
}
