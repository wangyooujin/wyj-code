//! MCP server 安装编排：包选择策略 + install/upgrade/uninstall/enable + 启动时生效列表
//!
//! 严格遵守"只管配置，不碰依赖安装"：这里只负责把 command/args/env/version 写入
//! config(全局 config.toml 或项目 .wyj-code/mcp.toml) + lockfile，绝不 shell out 执行
//! `npm install`/`pip install`。MCP server 走 npx/uvx 这类运行时自动拉取的启动
//! 方式规避依赖安装问题。

use crate::lockfile::{self, InstallScope, InstalledMcpEntry, McpSource};
use crate::plugin_install;
use crate::registry::{RegistryClient, RegistryPackage, RegistryServerSummary};
use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use wyj_config::{Config, McpServerConfig, McpTransport};

/// 从 registry 的 `packages[]` 选出的可执行方案。
#[derive(Debug, Clone)]
pub enum PackageChoice {
    Npx {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        package_identifier: String,
        package_version: String,
    },
    Uvx {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        package_identifier: String,
        package_version: String,
    },
    /// 纯 remote 型或 oci/nuget/cargo/mcpb 等 wyj-code 暂不支持自动安装的包类型。
    Unsupported { reason_key: &'static str },
}

impl PackageChoice {
    pub fn package_version(&self) -> Option<&str> {
        match self {
            PackageChoice::Npx {
                package_version, ..
            } => Some(package_version),
            PackageChoice::Uvx {
                package_version, ..
            } => Some(package_version),
            PackageChoice::Unsupported { .. } => None,
        }
    }
}

/// 包选择策略：npm 优先 → pypi → 其余类型/纯 remote 一律 Unsupported。
pub fn choose_package(pkgs: &[RegistryPackage]) -> PackageChoice {
    if let Some(p) = pkgs.iter().find(|p| p.registry_type == "npm") {
        return PackageChoice::Npx {
            command: "npx".to_string(),
            args: std::iter::once("-y".to_string())
                .chain(std::iter::once(format!("{}@{}", p.identifier, p.version)))
                .chain(flatten_package_arguments(&p.package_arguments))
                .collect(),
            env: flatten_env_vars(&p.environment_variables),
            package_identifier: p.identifier.clone(),
            package_version: p.version.clone(),
        };
    }
    if let Some(p) = pkgs.iter().find(|p| p.registry_type == "pypi") {
        return PackageChoice::Uvx {
            command: "uvx".to_string(),
            args: std::iter::once(format!("{}=={}", p.identifier, p.version))
                .chain(flatten_package_arguments(&p.package_arguments))
                .collect(),
            env: flatten_env_vars(&p.environment_variables),
            package_identifier: p.identifier.clone(),
            package_version: p.version.clone(),
        };
    }
    PackageChoice::Unsupported {
        reason_key: "mcp.install.unsupported_package",
    }
}

/// 从 registry 响应的 raw JSON 值里尽量抽取常见字段（`value`/`default`），
/// 未知结构直接忽略——`packageArguments` 精确子字段结构待实现联调时对照真实响应收紧。
fn flatten_package_arguments(values: &[serde_json::Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|v| {
            v.get("value")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("default").and_then(|x| x.as_str()))
                .map(|s| s.to_string())
        })
        .collect()
}

fn flatten_env_vars(values: &[serde_json::Value]) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for v in values {
        if let Some(name) = v.get("name").and_then(|x| x.as_str()) {
            let value = v
                .get("value")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("default").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string();
            env.insert(name.to_string(), value);
        }
    }
    env
}

/// 反向 DNS server name（如 `io.modelcontextprotocol/postgres`）取最后一段作为默认落盘名。
pub fn default_server_short_name(server_name: &str) -> String {
    server_name
        .rsplit('/')
        .next()
        .unwrap_or(server_name)
        .to_string()
}

pub struct McpInstallRequest {
    pub server: RegistryServerSummary,
    pub package: PackageChoice,
    pub scope: InstallScope,
    pub name_override: Option<String>,
    /// 该 server 是从哪个 registry 源搜到的（base_url）。持久化进 lockfile，
    /// 后续升级时用它重新连接，而不受用户当前在面板里选中的浏览源影响。
    pub registry_url: String,
}

struct ResolvedPackage {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    package_registry_type: String,
    package_identifier: String,
    package_version: String,
}

fn resolve_package(package: &PackageChoice) -> Result<ResolvedPackage> {
    match package {
        PackageChoice::Npx {
            command,
            args,
            env,
            package_identifier,
            package_version,
        } => Ok(ResolvedPackage {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            package_registry_type: "npm".to_string(),
            package_identifier: package_identifier.clone(),
            package_version: package_version.clone(),
        }),
        PackageChoice::Uvx {
            command,
            args,
            env,
            package_identifier,
            package_version,
        } => Ok(ResolvedPackage {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            package_registry_type: "pypi".to_string(),
            package_identifier: package_identifier.clone(),
            package_version: package_version.clone(),
        }),
        PackageChoice::Unsupported { .. } => {
            anyhow::bail!("该 server 暂不支持自动安装，仅支持 stdio 型 npm/pypi 包；请使用手动新增")
        }
    }
}

fn package_digest(package: &ResolvedPackage) -> String {
    let canonical = format!(
        "{}:{}@{}:{}",
        package.package_registry_type,
        package.package_identifier,
        package.package_version,
        package.args.join("\u{1f}")
    );
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn upsert_by_name(list: &mut Vec<McpServerConfig>, item: McpServerConfig) {
    if let Some(existing) = list.iter_mut().find(|s| s.name == item.name) {
        *existing = item;
    } else {
        list.push(item);
    }
}

fn upsert_mcp_config(mcp_config: McpServerConfig, scope: InstallScope, cwd: &Path) -> Result<()> {
    match scope {
        InstallScope::Global => {
            let mut cfg = Config::load()?;
            upsert_by_name(&mut cfg.mcp_servers, mcp_config);
            cfg.save()
        }
        InstallScope::Project => {
            let mut servers = wyj_config::load_project_mcp(cwd)?;
            upsert_by_name(&mut servers, mcp_config);
            wyj_config::save_project_mcp(cwd, &servers)
        }
    }
}

fn restore_mcp_config(
    name: &str,
    previous: Option<McpServerConfig>,
    scope: InstallScope,
    cwd: &Path,
) -> Result<()> {
    match scope {
        InstallScope::Global => {
            let mut cfg = Config::load()?;
            cfg.mcp_servers.retain(|server| server.name != name);
            if let Some(previous) = previous {
                cfg.mcp_servers.push(previous);
            }
            cfg.save()
        }
        InstallScope::Project => {
            let mut servers = wyj_config::load_project_mcp(cwd)?;
            servers.retain(|server| server.name != name);
            if let Some(previous) = previous {
                servers.push(previous);
            }
            wyj_config::save_project_mcp(cwd, &servers)
        }
    }
}

fn upsert_lockfile_entry(manifest: &mut lockfile::InstalledManifest, entry: InstalledMcpEntry) {
    if let Some(existing) = manifest
        .mcp_servers
        .iter_mut()
        .find(|e| e.name == entry.name)
    {
        *existing = entry;
    } else {
        manifest.mcp_servers.push(entry);
    }
}

/// 安装（首次写入）或"覆盖式升级"（目标 scope 已存在同名条目时直接覆盖）。
pub fn install_mcp_server(req: &McpInstallRequest, cwd: &Path) -> Result<()> {
    let resolved = resolve_package(&req.package)?;
    let name = req
        .name_override
        .clone()
        .unwrap_or_else(|| default_server_short_name(&req.server.name));
    let extension_id = format!("mcp:{name}");
    let package_version = resolved.package_version.clone();
    let package_digest = package_digest(&resolved);

    let previous_config = match req.scope {
        InstallScope::Global => Config::load()?
            .mcp_servers
            .into_iter()
            .find(|server| server.name == name),
        InstallScope::Project => wyj_config::load_project_mcp(cwd)?
            .into_iter()
            .find(|server| server.name == name),
    };

    let mcp_config = McpServerConfig {
        name: name.clone(),
        transport: McpTransport::Stdio,
        command: Some(resolved.command),
        args: resolved.args,
        env: resolved.env,
        url: None,
        headers: Default::default(),
    };
    upsert_mcp_config(mcp_config, req.scope, cwd).context("写入 MCP server 配置失败")?;

    let mut manifest = lockfile::load_scope(req.scope, cwd)?;
    let now = Utc::now();
    let existing_installed_at = manifest
        .mcp_servers
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.installed_at);
    upsert_lockfile_entry(
        &mut manifest,
        InstalledMcpEntry {
            name: name.clone(),
            version: Some(resolved.package_version),
            source: Some(McpSource::Registry {
                registry_url: req.registry_url.clone(),
                server_name: req.server.name.clone(),
                package_registry_type: resolved.package_registry_type,
                package_identifier: resolved.package_identifier,
            }),
            enabled: true,
            installed_at: existing_installed_at.unwrap_or(now),
            updated_at: now,
        },
    );
    lockfile::upsert_extension(
        &mut manifest,
        lockfile::ExtensionLockEntry {
            id: extension_id,
            kind: lockfile::ExtensionKind::Mcp,
            scope: req.scope,
            source: Some(req.registry_url.clone()),
            version: Some(package_version),
            commit: None,
            digest: Some(package_digest),
            enabled: true,
            dependencies: Vec::new(),
            installed_at: existing_installed_at.unwrap_or(now),
            updated_at: now,
        },
    );
    if let Err(error) = lockfile::save_scope(req.scope, cwd, &manifest) {
        if let Err(rollback_error) = restore_mcp_config(&name, previous_config, req.scope, cwd) {
            return Err(error).context(format!(
                "写入 lockfile 失败，配置回滚也失败: {rollback_error}"
            ));
        }
        return Err(error).context("写入 lockfile 失败，已回滚 MCP 配置");
    }
    Ok(())
}

/// 升级：重新从该条目当初安装时记录的 registry 源（而非用户当前在面板里选中
/// 浏览的源）拉取最新版本详情，重新 choose_package，若版本号变化则覆盖
/// config + lockfile 并返回 `Upgraded`；否则返回 `AlreadyLatest` 且不做任何
/// 改动。仅对纳管条目（`source.is_some()`）开放，手动条目应在调用前由上层拒绝。
pub async fn upgrade_mcp_server(
    name: &str,
    scope: InstallScope,
    cwd: &Path,
) -> Result<crate::UpgradeOutcome> {
    let manifest = lockfile::load_scope(scope, cwd)?;
    let entry = manifest
        .mcp_servers
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("未找到已安装的 MCP server: {name}"))?;
    let McpSource::Registry {
        registry_url,
        server_name,
        ..
    } = entry
        .source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("'{name}' 是手动配置项，没有版本信息，无法升级"))?;

    let client = RegistryClient::new(registry_url.clone());
    let candidates = client.search_servers(server_name).await?;
    let latest = candidates
        .into_iter()
        .find(|s| &s.name == server_name)
        .ok_or_else(|| anyhow::anyhow!("registry 中未找到 server: {server_name}"))?;

    let package = choose_package(&latest.packages);
    let new_version = package
        .package_version()
        .ok_or_else(|| anyhow::anyhow!("registry 最新版本不支持自动安装"))?
        .to_string();

    if Some(new_version.as_str()) == entry.version.as_deref() {
        return Ok(crate::UpgradeOutcome::AlreadyLatest {
            version: new_version,
        });
    }

    let req = McpInstallRequest {
        server: latest,
        package,
        scope,
        name_override: Some(name.to_string()),
        registry_url: registry_url.clone(),
    };
    install_mcp_server(&req, cwd)?;
    Ok(crate::UpgradeOutcome::Upgraded {
        version: new_version,
    })
}

/// 卸载：从 config（对应 scope）删除该条目 + 从 lockfile 删除对应 entry。
/// 手动条目（lockfile 无记录）同样支持删除 config 条目。
///
/// **原生来源（`~/.claude.json`/`.mcp.json`）例外**：`wyj_config::Config::load()`
/// 每次都会把 `~/.claude.json` 的 `mcpServers` 重新合并回 `cfg.mcp_servers`
/// （项目级同理，见 `merged_mcp_servers` 对 `.mcp.json` 的处理），这两个文件是
/// Claude Code 自己的，wyj-code 不应该也没法去改它们。如果照常删 config 条目 +
/// 清空 lockfile 记录，下一次任何代码路径调用 `Config::load()`（本函数的调用方
/// 几乎都会紧接着刷新一次）都会把它原样合并回来，且因为 lockfile 记录被清空、
/// `managed` 变成 `None`，UI 上 enabled 状态默认还是"启用"——卸载在磁盘上其实
/// 从没生效过，服务器会立刻又出现在列表里、继续被连接，用户看到的就是"卸载
/// 了但没反应"。对这类名字，唯一能持久化的动作是"禁用"（写一条
/// `enabled: false` 的 lockfile 记录，`effective_mcp_servers` 据此把它排除在
/// 运行时连接列表外），因此改为委托给 `set_mcp_enabled`；调用方（TUI 面板）
/// 再把"原生来源 + 已禁用"的条目从"已安装"列表里过滤掉，视觉上等价于卸载。
pub fn uninstall_mcp_server(name: &str, scope: InstallScope, cwd: &Path) -> Result<()> {
    let is_native = wyj_config::native_mcp_names(cwd).contains(name);

    match scope {
        InstallScope::Global => {
            let mut cfg = Config::load()?;
            cfg.mcp_servers.retain(|s| s.name != name);
            cfg.save()?;
        }
        InstallScope::Project => {
            let mut servers = wyj_config::load_project_mcp(cwd)?;
            servers.retain(|s| s.name != name);
            wyj_config::save_project_mcp(cwd, &servers)?;
        }
    }

    if is_native {
        return set_mcp_enabled(name, scope, cwd, false);
    }

    let mut manifest = lockfile::load_scope(scope, cwd)?;
    manifest.mcp_servers.retain(|e| e.name != name);
    lockfile::remove_extension(&mut manifest, &format!("mcp:{name}"));
    lockfile::save_scope(scope, cwd, &manifest)
}

/// 启用/禁用（仅改 lockfile.enabled，不删 config 条目）。手动条目若还没有
/// lockfile 记录，会补一条 `source: None` 的记录用来持久化 enabled 状态。
pub fn set_mcp_enabled(name: &str, scope: InstallScope, cwd: &Path, enabled: bool) -> Result<()> {
    let mut manifest = lockfile::load_scope(scope, cwd)?;
    let now = Utc::now();
    if let Some(existing) = manifest.mcp_servers.iter_mut().find(|e| e.name == name) {
        existing.enabled = enabled;
        existing.updated_at = now;
    } else {
        manifest.mcp_servers.push(InstalledMcpEntry {
            name: name.to_string(),
            version: None,
            source: None,
            enabled,
            installed_at: now,
            updated_at: now,
        });
    }
    let id = format!("mcp:{name}");
    if let Some(existing) = manifest.extensions.iter_mut().find(|e| e.id == id) {
        existing.enabled = enabled;
        existing.updated_at = now;
    } else {
        lockfile::upsert_extension(
            &mut manifest,
            lockfile::ExtensionLockEntry {
                id,
                kind: lockfile::ExtensionKind::Mcp,
                scope,
                source: None,
                version: None,
                commit: None,
                digest: None,
                enabled,
                dependencies: Vec::new(),
                installed_at: now,
                updated_at: now,
            },
        );
    }
    lockfile::save_scope(scope, cwd, &manifest)
}

/// 启动时使用：合并全局 + 项目配置，并过滤 lockfile 里 `enabled == false` 的条目；
/// 再叠加已启用插件贡献的 MCP server（不写入 config.toml/mcp.toml，只存在于插件
/// lockfile 的 contributes 快照里）。插件贡献的 server 与用户已有配置（含手动、
/// registry 安装、更早安装的插件）同名时"先到先得"，跳过并警告，不覆盖任何已有
/// 配置——对齐决策 5（同名资源冲突统一为"先到先得，跳过并警告"）。
pub fn effective_mcp_servers(cfg: &Config, cwd: &Path) -> Vec<McpServerConfig> {
    let merged = wyj_config::merged_mcp_servers(cfg, cwd);
    let disabled = lockfile::disabled_mcp_names(cwd);
    let mut result: Vec<McpServerConfig> = merged
        .into_iter()
        .filter(|s| !disabled.contains(&s.name))
        .collect();

    let mut existing_names: HashSet<String> = result.iter().map(|s| s.name.clone()).collect();
    for (plugin_name, server) in plugin_install::plugin_mcp_servers(cwd) {
        if existing_names.contains(&server.name) {
            tracing::warn!(
                "插件 '{plugin_name}' 的 MCP server '{}' 与已有配置同名，已跳过",
                server.name
            );
            continue;
        }
        existing_names.insert(server.name.clone());
        result.push(server);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npm_package(identifier: &str, version: &str) -> RegistryPackage {
        RegistryPackage {
            registry_type: "npm".to_string(),
            identifier: identifier.to_string(),
            version: version.to_string(),
            registry_base_url: None,
            runtime_hint: Some("npx".to_string()),
            package_arguments: vec![],
            runtime_arguments: vec![],
            environment_variables: vec![],
        }
    }

    fn pypi_package(identifier: &str, version: &str) -> RegistryPackage {
        RegistryPackage {
            registry_type: "pypi".to_string(),
            identifier: identifier.to_string(),
            version: version.to_string(),
            registry_base_url: None,
            runtime_hint: Some("uvx".to_string()),
            package_arguments: vec![],
            runtime_arguments: vec![],
            environment_variables: vec![],
        }
    }

    fn oci_package() -> RegistryPackage {
        RegistryPackage {
            registry_type: "oci".to_string(),
            identifier: "docker.io/mcp/filesystem".to_string(),
            version: "1.0.0".to_string(),
            registry_base_url: None,
            runtime_hint: None,
            package_arguments: vec![],
            runtime_arguments: vec![],
            environment_variables: vec![],
        }
    }

    #[test]
    fn npm_takes_priority_over_pypi() {
        let pkgs = vec![
            pypi_package("weather-mcp-server", "0.5.0"),
            npm_package("@modelcontextprotocol/server-postgres", "1.2.3"),
        ];
        match choose_package(&pkgs) {
            PackageChoice::Npx {
                command,
                package_identifier,
                package_version,
                args,
                ..
            } => {
                assert_eq!(command, "npx");
                assert_eq!(package_identifier, "@modelcontextprotocol/server-postgres");
                assert_eq!(package_version, "1.2.3");
                assert!(args.contains(&"-y".to_string()));
                assert!(args.contains(&"@modelcontextprotocol/server-postgres@1.2.3".to_string()));
            }
            other => panic!("expected Npx, got {other:?}"),
        }
    }

    #[test]
    fn pypi_used_when_no_npm() {
        let pkgs = vec![pypi_package("weather-mcp-server", "0.5.0")];
        match choose_package(&pkgs) {
            PackageChoice::Uvx {
                command,
                package_version,
                ..
            } => {
                assert_eq!(command, "uvx");
                assert_eq!(package_version, "0.5.0");
            }
            other => panic!("expected Uvx, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_types_fall_back() {
        let pkgs = vec![oci_package()];
        assert!(matches!(
            choose_package(&pkgs),
            PackageChoice::Unsupported { .. }
        ));
    }

    #[test]
    fn remote_only_falls_back_to_unsupported() {
        // 纯 remote 型 server 的 packages 为空数组（remotes 字段与 choose_package 无关，
        // 因为它只接收 packages 切片）。
        let pkgs: Vec<RegistryPackage> = vec![];
        assert!(matches!(
            choose_package(&pkgs),
            PackageChoice::Unsupported { .. }
        ));
    }

    #[test]
    fn install_then_uninstall_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let server = RegistryServerSummary {
            name: "io.modelcontextprotocol/postgres".to_string(),
            description: "PostgreSQL MCP server".to_string(),
            version: "1.2.3".to_string(),
            packages: vec![npm_package(
                "@modelcontextprotocol/server-postgres",
                "1.2.3",
            )],
            remotes: vec![],
        };
        let package = choose_package(&server.packages);
        let req = McpInstallRequest {
            server,
            package,
            scope: InstallScope::Project,
            name_override: None,
            registry_url: "https://registry.modelcontextprotocol.io".to_string(),
        };
        install_mcp_server(&req, dir.path()).unwrap();

        let servers = wyj_config::load_project_mcp(dir.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "postgres");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));

        let manifest = lockfile::load_project(dir.path()).unwrap();
        assert_eq!(manifest.mcp_servers.len(), 1);
        assert!(manifest.mcp_servers[0].is_managed());

        uninstall_mcp_server("postgres", InstallScope::Project, dir.path()).unwrap();
        let servers = wyj_config::load_project_mcp(dir.path()).unwrap();
        assert!(servers.is_empty());
        let manifest = lockfile::load_project(dir.path()).unwrap();
        assert!(manifest.mcp_servers.is_empty());
    }

    /// 回归测试：`.mcp.json`（原生 Claude Code 项目配置）里的 server 不是
    /// wyj-code 自己的文件，"卸载"删不掉磁盘上那一行——`merged_mcp_servers`
    /// 每次都会把它重新合并回来。之前的实现直接清空 lockfile 记录，导致
    /// `managed` 变回 `None`、UI 默认按"启用"处理，卸载形同虚设，用户反复
    /// 点卸载它还是在运行。现在应该改为落一条禁用记录，让
    /// `effective_mcp_servers`（真正决定连接哪些 server 的入口）把它排除在外，
    /// 同时不去碰 `.mcp.json` 本身。
    #[test]
    fn uninstall_native_origin_server_disables_instead_of_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_json = dir.path().join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers": {"native-srv": {"command": "npx", "args": ["-y", "native-srv-mcp"]}}}"#,
        )
        .unwrap();
        assert!(wyj_config::native_mcp_names(dir.path()).contains("native-srv"));

        uninstall_mcp_server("native-srv", InstallScope::Project, dir.path()).unwrap();

        // .mcp.json 本身不是 wyj-code 的文件，不应该被改动。
        let raw = std::fs::read_to_string(&mcp_json).unwrap();
        assert!(raw.contains("native-srv"));

        // 但 lockfile 应该落一条禁用记录……
        let manifest = lockfile::load_project(dir.path()).unwrap();
        let entry = manifest
            .mcp_servers
            .iter()
            .find(|e| e.name == "native-srv")
            .expect("应写入一条 enabled=false 的 shadow 记录");
        assert!(!entry.enabled);

        // ……真正决定运行时连接哪些 server 的 effective_mcp_servers 因此把它排除在外。
        let cfg = wyj_config::Config::default();
        let effective = effective_mcp_servers(&cfg, dir.path());
        assert!(!effective.iter().any(|s| s.name == "native-srv"));
    }

    #[test]
    fn set_enabled_creates_shadow_entry_for_manual_config() {
        let dir = tempfile::tempdir().unwrap();
        set_mcp_enabled("manual-server", InstallScope::Project, dir.path(), false).unwrap();
        let manifest = lockfile::load_project(dir.path()).unwrap();
        assert_eq!(manifest.mcp_servers.len(), 1);
        assert!(!manifest.mcp_servers[0].enabled);
        assert!(!manifest.mcp_servers[0].is_managed());
    }

    fn plugin_entry_with_mcp_servers(
        name: &str,
        servers: Vec<McpServerConfig>,
    ) -> crate::lockfile::InstalledPluginEntry {
        crate::lockfile::InstalledPluginEntry {
            name: name.to_string(),
            version: None,
            scope: InstallScope::Project,
            source: crate::lockfile::PluginInstallOrigin::Local {
                path: std::path::PathBuf::from("/tmp/does-not-matter"),
            },
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            plugin_root: std::path::PathBuf::from("/tmp/does-not-matter"),
            contributes: crate::lockfile::PluginContributions {
                skill_paths: vec![],
                agent_paths: vec![],
                mcp_servers: servers,
                skipped_capabilities: vec![],
            },
        }
    }

    #[test]
    fn effective_mcp_servers_includes_non_conflicting_plugin_contribution() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wyj_config::Config::default();

        let mut manifest = lockfile::load_project(dir.path()).unwrap();
        manifest.plugins.push(plugin_entry_with_mcp_servers(
            "my-plugin",
            vec![McpServerConfig {
                name: "plugin-tool".to_string(),
                transport: McpTransport::Stdio,
                command: Some("npx".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
        ));
        lockfile::save_project(dir.path(), &manifest).unwrap();

        let effective = effective_mcp_servers(&cfg, dir.path());
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].name, "plugin-tool");
    }

    #[test]
    fn effective_mcp_servers_skips_plugin_contribution_conflicting_with_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wyj_config::Config {
            mcp_servers: vec![McpServerConfig {
                name: "shared-name".to_string(),
                transport: McpTransport::Stdio,
                command: Some("user-configured".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
            ..wyj_config::Config::default()
        };

        let mut manifest = lockfile::load_project(dir.path()).unwrap();
        manifest.plugins.push(plugin_entry_with_mcp_servers(
            "my-plugin",
            vec![McpServerConfig {
                name: "shared-name".to_string(),
                transport: McpTransport::Stdio,
                command: Some("plugin-provided".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
        ));
        lockfile::save_project(dir.path(), &manifest).unwrap();

        let effective = effective_mcp_servers(&cfg, dir.path());
        // 用户已有配置优先，插件同名条目被跳过，不覆盖。
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].command.as_deref(), Some("user-configured"));
    }
}
