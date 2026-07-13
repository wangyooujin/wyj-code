//! Unified Skill/MCP/Plugin resource inventory, diagnostics and migration helpers.
//!
//! The existing install modules remain the compatibility implementation for writing
//! individual resource formats. This module is the stable cross-resource read model
//! used by the CLI, TUI and future runtime hot-apply coordinator.

use crate::lockfile::{self, ExtensionKind, InstallScope};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use wyj_config::{Config, McpServerConfig, McpTransport};

pub const EXTENSIONS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionRecord {
    pub id: String,
    pub kind: ExtensionKind,
    pub scope: InstallScope,
    pub source: Option<String>,
    pub version: Option<String>,
    pub commit: Option<String>,
    pub digest: Option<String>,
    pub enabled: bool,
    pub effective: bool,
    pub health: String,
    pub dependencies: Vec<String>,
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorIssue {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub cwd: PathBuf,
    pub lockfile_versions: BTreeMap<String, u32>,
    pub records: Vec<ExtensionRecord>,
    pub issues: Vec<DoctorIssue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    pub schema_version: u32,
    pub imported_files: Vec<String>,
    pub imported_servers: Vec<String>,
    pub skipped_servers: Vec<String>,
    pub errors: Vec<String>,
}

/// Enumerate managed resources from both global and project lockfiles.
pub fn list(cwd: &Path) -> Result<Vec<ExtensionRecord>> {
    let mut records = Vec::new();
    let global = lockfile::load_global().context("读取全局 extension lockfile 失败")?;
    let project = lockfile::load_project(cwd).context("读取项目 extension lockfile 失败")?;

    append_manifest(&mut records, &global, InstallScope::Global);
    append_manifest(&mut records, &project, InstallScope::Project);

    // Add unmanaged MCP entries so `extensions list` explains why a server is active
    // even when it was configured by hand rather than installed from a registry.
    let configured = configured_mcp_servers(cwd)?;
    let known: HashSet<String> = records
        .iter()
        .filter(|r| r.kind == ExtensionKind::Mcp)
        .map(|r| r.id.clone())
        .collect();
    for (scope, server) in configured {
        let id = format!("mcp:{}", server.name);
        if known.contains(&id) {
            continue;
        }
        let mut details = BTreeMap::new();
        details.insert("transport".to_string(), format!("{:?}", server.transport));
        if let Some(command) = server.command {
            details.insert("command".to_string(), command);
        }
        if let Some(url) = server.url {
            details.insert("url".to_string(), url);
        }
        records.push(ExtensionRecord {
            id,
            kind: ExtensionKind::Mcp,
            scope,
            source: None,
            version: None,
            commit: None,
            digest: None,
            enabled: true,
            effective: true,
            health: "unmanaged".to_string(),
            dependencies: Vec::new(),
            details,
        });
    }

    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records)
}

fn append_manifest(
    records: &mut Vec<ExtensionRecord>,
    manifest: &lockfile::InstalledManifest,
    scope: InstallScope,
) {
    for entry in &manifest.extensions {
        records.push(ExtensionRecord {
            id: entry.id.clone(),
            kind: entry.kind,
            scope,
            source: entry.source.clone(),
            version: entry.version.clone(),
            commit: entry.commit.clone(),
            digest: entry.digest.clone(),
            enabled: entry.enabled,
            effective: entry.enabled,
            health: if entry.enabled { "enabled" } else { "disabled" }.to_string(),
            dependencies: entry.dependencies.clone(),
            details: BTreeMap::new(),
        });
    }

    for entry in &manifest.skills {
        if manifest
            .extensions
            .iter()
            .any(|x| x.id == format!("skill:{}", entry.name))
        {
            continue;
        }
        records.push(ExtensionRecord {
            id: format!("skill:{}", entry.name),
            kind: ExtensionKind::Skill,
            scope,
            source: entry.source.as_ref().map(|s| s.marketplace_url.clone()),
            version: entry.version.clone(),
            commit: None,
            digest: None,
            enabled: entry.enabled,
            effective: entry.enabled,
            health: if entry.enabled { "enabled" } else { "disabled" }.to_string(),
            dependencies: Vec::new(),
            details: BTreeMap::new(),
        });
    }

    for entry in &manifest.mcp_servers {
        if manifest
            .extensions
            .iter()
            .any(|x| x.id == format!("mcp:{}", entry.name))
        {
            continue;
        }
        records.push(ExtensionRecord {
            id: format!("mcp:{}", entry.name),
            kind: ExtensionKind::Mcp,
            scope,
            source: entry.source.as_ref().map(|s| match s {
                lockfile::McpSource::Registry { registry_url, .. } => registry_url.clone(),
            }),
            version: entry.version.clone(),
            commit: None,
            digest: None,
            enabled: entry.enabled,
            effective: entry.enabled,
            health: if entry.enabled { "enabled" } else { "disabled" }.to_string(),
            dependencies: Vec::new(),
            details: BTreeMap::new(),
        });
    }

    for entry in &manifest.plugins {
        if manifest
            .extensions
            .iter()
            .any(|x| x.id == format!("plugin:{}", entry.name))
        {
            continue;
        }
        let mut details = BTreeMap::new();
        details.insert(
            "skills".to_string(),
            entry.contributes.skill_paths.len().to_string(),
        );
        details.insert(
            "agents".to_string(),
            entry.contributes.agent_paths.len().to_string(),
        );
        details.insert(
            "mcp".to_string(),
            entry.contributes.mcp_servers.len().to_string(),
        );
        if !entry.contributes.skipped_capabilities.is_empty() {
            details.insert(
                "skipped".to_string(),
                entry.contributes.skipped_capabilities.join(","),
            );
        }
        records.push(ExtensionRecord {
            id: format!("plugin:{}", entry.name),
            kind: ExtensionKind::Plugin,
            scope,
            source: Some(format!("{:?}", entry.source)),
            version: entry.version.clone(),
            commit: None,
            digest: None,
            enabled: entry.enabled,
            effective: entry.enabled,
            health: if entry.enabled { "enabled" } else { "disabled" }.to_string(),
            dependencies: Vec::new(),
            details,
        });
    }
}

pub fn doctor(cwd: &Path) -> Result<DoctorReport> {
    let records = list(cwd)?;
    let configured = configured_mcp_servers(cwd)?;
    let mut issues = Vec::new();
    let mut lockfile_versions = BTreeMap::new();
    let global = lockfile::load_global()?;
    let project = lockfile::load_project(cwd)?;
    lockfile_versions.insert("global".to_string(), global.version);
    lockfile_versions.insert("project".to_string(), project.version);

    for record in &records {
        if !record.enabled {
            continue;
        }
        if record.health == "unmanaged" {
            issues.push(DoctorIssue {
                code: "unmanaged_resource".to_string(),
                severity: "info".to_string(),
                message: format!("{} is configured manually", record.id),
                remediation: Some(
                    "install it through `wyj-code extensions` to manage its version".to_string(),
                ),
            });
        }
        if record.kind == ExtensionKind::Plugin
            && record.details.get("skipped").is_some_and(|x| !x.is_empty())
        {
            issues.push(DoctorIssue {
                code: "plugin_capability_skipped".to_string(),
                severity: "warning".to_string(),
                message: format!(
                    "{} has skipped capabilities: {}",
                    record.id, record.details["skipped"]
                ),
                remediation: Some(
                    "inspect the plugin manifest and upgrade to a supported capability set"
                        .to_string(),
                ),
            });
        }
        for dependency in &record.dependencies {
            if !records.iter().any(|candidate| &candidate.id == dependency) {
                issues.push(DoctorIssue {
                    code: "missing_dependency".to_string(),
                    severity: "error".to_string(),
                    message: format!("{} depends on missing {}", record.id, dependency),
                    remediation: Some(
                        "install the dependency or remove the plugin before enabling it"
                            .to_string(),
                    ),
                });
            }
        }
        if record.kind == ExtensionKind::Mcp {
            let name = record.id.strip_prefix("mcp:").unwrap_or_default();
            if let Some((_, server)) = configured.iter().find(|(_, server)| server.name == name) {
                match server.transport {
                    McpTransport::Stdio => {
                        if let Some(command) = &server.command {
                            if !command_available(command) {
                                issues.push(DoctorIssue {
                                    code: "mcp_command_missing".to_string(),
                                    severity: "error".to_string(),
                                    message: format!("{} command is not available: {}", record.id, command),
                                    remediation: Some("install the runner or update the MCP command in the configuration".to_string()),
                                });
                            }
                        }
                    }
                    McpTransport::StreamableHttp => {
                        if server.url.as_deref().unwrap_or_default().is_empty() {
                            issues.push(DoctorIssue {
                                code: "mcp_url_missing".to_string(),
                                severity: "error".to_string(),
                                message: format!("{} has no Streamable HTTP URL", record.id),
                                remediation: Some("set url in the MCP configuration".to_string()),
                            });
                        }
                    }
                }
            }
        }
    }

    for file in native_mcp_candidates(cwd) {
        if file.exists() {
            issues.push(DoctorIssue {
                code: "migration_available".to_string(),
                severity: "info".to_string(),
                message: format!("native MCP configuration found at {}", file.display()),
                remediation: Some(
                    "run `wyj-code extensions migrate` after reviewing the import".to_string(),
                ),
            });
        }
    }

    Ok(DoctorReport {
        schema_version: EXTENSIONS_SCHEMA_VERSION,
        cwd: cwd.to_path_buf(),
        lockfile_versions,
        records,
        issues,
    })
}

fn command_available(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|dir| dir.join(command))
        .any(|candidate| candidate.is_file())
}

pub fn set_enabled(id: &str, scope: InstallScope, cwd: &Path, enabled: bool) -> Result<()> {
    let (kind, name) = parse_id(id)?;
    match kind {
        ExtensionKind::Skill => crate::skill_install::set_skill_enabled(name, scope, cwd, enabled),
        ExtensionKind::Mcp => crate::mcp_install::set_mcp_enabled(name, scope, cwd, enabled),
        ExtensionKind::Plugin => {
            crate::plugin_install::set_plugin_enabled(name, scope, cwd, enabled)
        }
        ExtensionKind::Agent => {
            anyhow::bail!("agent definitions are controlled by their source files")
        }
    }
}

pub fn remove(id: &str, scope: InstallScope, cwd: &Path) -> Result<()> {
    let (kind, name) = parse_id(id)?;
    match kind {
        ExtensionKind::Skill => crate::skill_install::uninstall_skill(name, scope, cwd),
        ExtensionKind::Mcp => crate::mcp_install::uninstall_mcp_server(name, scope, cwd),
        ExtensionKind::Plugin => crate::plugin_install::uninstall_plugin(name, scope, cwd),
        ExtensionKind::Agent => {
            anyhow::bail!("agent definitions are controlled by their source files")
        }
    }
}

fn parse_id(id: &str) -> Result<(ExtensionKind, &str)> {
    let (kind, name) = id
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("资源 ID 必须是 kind:name，例如 mcp:postgres"))?;
    let kind = match kind {
        "skill" => ExtensionKind::Skill,
        "mcp" => ExtensionKind::Mcp,
        "plugin" => ExtensionKind::Plugin,
        "agent" => ExtensionKind::Agent,
        _ => anyhow::bail!("未知资源类型: {kind}"),
    };
    if name.trim().is_empty() {
        anyhow::bail!("资源名称不能为空");
    }
    Ok((kind, name))
}

/// Import Claude-style `mcpServers` JSON into the existing project/global TOML
/// configuration without deleting or rewriting the native source file.
pub fn migrate(cwd: &Path) -> Result<MigrationReport> {
    let mut report = MigrationReport {
        schema_version: EXTENSIONS_SCHEMA_VERSION,
        ..Default::default()
    };
    let project_file = cwd.join(".mcp.json");
    if project_file.exists() {
        match read_native_mcp(&project_file) {
            Ok(servers) => {
                let mut existing = wyj_config::load_project_mcp(cwd)?;
                let mut names: HashSet<String> = existing.iter().map(|s| s.name.clone()).collect();
                for server in servers {
                    if names.insert(server.name.clone()) {
                        report.imported_servers.push(server.name.clone());
                        existing.push(server);
                    } else {
                        report.skipped_servers.push(server.name);
                    }
                }
                wyj_config::save_project_mcp(cwd, &existing)?;
                report
                    .imported_files
                    .push(project_file.display().to_string());
            }
            Err(e) => report
                .errors
                .push(format!("{}: {e}", project_file.display())),
        }
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        let global_file = home.join(".claude.json");
        if global_file.exists() {
            match read_native_mcp(&global_file) {
                Ok(servers) => {
                    let mut cfg = Config::load()?;
                    let mut names: HashSet<String> =
                        cfg.mcp_servers.iter().map(|s| s.name.clone()).collect();
                    for server in servers {
                        if names.insert(server.name.clone()) {
                            report.imported_servers.push(server.name.clone());
                            cfg.mcp_servers.push(server);
                        } else {
                            report.skipped_servers.push(server.name);
                        }
                    }
                    cfg.save()?;
                    report
                        .imported_files
                        .push(global_file.display().to_string());
                }
                Err(e) => report
                    .errors
                    .push(format!("{}: {e}", global_file.display())),
            }
        }
    }
    Ok(report)
}

fn configured_mcp_servers(cwd: &Path) -> Result<Vec<(InstallScope, McpServerConfig)>> {
    let mut servers = Vec::new();
    let cfg = Config::load()?;
    servers.extend(
        cfg.mcp_servers
            .into_iter()
            .map(|s| (InstallScope::Global, s)),
    );
    servers.extend(
        wyj_config::load_project_mcp(cwd)?
            .into_iter()
            .map(|s| (InstallScope::Project, s)),
    );
    Ok(servers)
}

fn native_mcp_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![cwd.join(".mcp.json")];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".claude.json"));
    }
    paths
}

fn read_native_mcp(path: &Path) -> Result<Vec<McpServerConfig>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取原生 MCP 配置失败: {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析原生 MCP 配置失败: {}", path.display()))?;
    let servers = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("配置中缺少 mcpServers 对象"))?;
    let mut out = Vec::new();
    for (name, raw) in servers {
        let command = raw
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string);
        let args = raw
            .get("args")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let env = string_map(raw.get("env"));
        let url = raw.get("url").and_then(Value::as_str).map(str::to_string);
        let headers = string_map(raw.get("headers"));
        let transport = if url.is_some() {
            McpTransport::StreamableHttp
        } else {
            McpTransport::Stdio
        };
        out.push(McpServerConfig {
            name: name.clone(),
            transport,
            command,
            args,
            env,
            url,
            headers,
        });
    }
    Ok(out)
}

fn string_map(value: Option<&Value>) -> std::collections::HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

pub fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extension_ids() {
        assert_eq!(parse_id("mcp:postgres").unwrap().0, ExtensionKind::Mcp);
        assert!(parse_id("unknown:x").is_err());
        assert!(parse_id("mcp:").is_err());
    }

    #[test]
    fn reads_native_stdio_and_http_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"local":{"command":"node","args":["server.js"]},"remote":{"url":"https://example.test/mcp","headers":{"Authorization":"${TOKEN}"}}}}"#,
        )
        .unwrap();
        let servers = read_native_mcp(&path).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].transport, McpTransport::Stdio);
        assert_eq!(servers[1].transport, McpTransport::StreamableHttp);
    }
}
