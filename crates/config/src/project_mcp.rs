//! 项目级 MCP 配置：`<cwd>/.wyj/mcp.toml`
//!
//! 格式与全局 `config.toml` 的 `[[mcp_servers]]` 段一致，同名 server 覆盖全局配置，
//! 不同名则追加。不做祖先目录 walk，与现有 Skill 项目目录（`.wyj/skills/`）约定一致。

use crate::{Config, McpServerConfig};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectMcpConfig {
    pub mcp_servers: Vec<McpServerConfig>,
}

/// 返回项目级 MCP 配置文件路径（`<cwd>/.wyj/mcp.toml`）。
pub fn project_mcp_path(cwd: &Path) -> PathBuf {
    cwd.join(".wyj").join("mcp.toml")
}

/// 加载项目级 MCP server 列表；文件不存在则返回空列表。
pub fn load_project_mcp(cwd: &Path) -> Result<Vec<McpServerConfig>> {
    let path = project_mcp_path(cwd);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取项目级 MCP 配置失败: {}", path.display()))?;
    let parsed: ProjectMcpConfig = toml::from_str(&content)
        .with_context(|| format!("解析项目级 MCP 配置失败: {}", path.display()))?;
    Ok(parsed.mcp_servers)
}

/// Read Claude Code's native `{ "mcpServers": { ... } }` shape without mutating
/// the source file. This is used as a compatibility layer; wyj-code writes its
/// canonical TOML representation separately.
pub fn load_native_mcp(path: &Path) -> Result<Vec<McpServerConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取原生 MCP 配置失败: {}", path.display()))?;
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析原生 MCP 配置失败: {}", path.display()))?;
    let servers = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("原生 MCP 配置缺少 mcpServers 对象"))?;
    let mut result = Vec::new();
    for (name, raw) in servers {
        let url = raw.get("url").and_then(Value::as_str).map(str::to_string);
        let transport = if url.is_some() {
            crate::McpTransport::StreamableHttp
        } else {
            crate::McpTransport::Stdio
        };
        let args = raw
            .get("args")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        result.push(McpServerConfig {
            name: name.clone(),
            transport,
            command: raw
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string),
            args,
            env: json_string_map(raw.get("env")),
            url,
            headers: json_string_map(raw.get("headers")),
        });
    }
    Ok(result)
}

fn json_string_map(value: Option<&Value>) -> std::collections::HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 保存项目级 MCP server 列表（会自动创建 `.wyj/` 目录）。
pub fn save_project_mcp(cwd: &Path, servers: &[McpServerConfig]) -> Result<()> {
    let path = project_mcp_path(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建项目配置目录失败: {}", parent.display()))?;
    }
    let cfg = ProjectMcpConfig {
        mcp_servers: servers.to_vec(),
    };
    let content = toml::to_string_pretty(&cfg).context("序列化项目级 MCP 配置失败")?;
    super::write_atomic(&path, &content)
        .with_context(|| format!("写入项目级 MCP 配置失败: {}", path.display()))
}

/// 合并全局 + 项目级 MCP server 列表（项目同名覆盖全局，新增追加末尾）。
pub fn merged_mcp_servers(cfg: &Config, cwd: &Path) -> Vec<McpServerConfig> {
    let project_servers = load_project_mcp(cwd).unwrap_or_else(|e| {
        tracing::warn!("加载项目级 MCP 配置失败，忽略: {e}");
        Vec::new()
    });

    let mut merged: Vec<McpServerConfig> = cfg.mcp_servers.clone();
    for project_server in project_servers {
        if let Some(existing) = merged.iter_mut().find(|s| s.name == project_server.name) {
            *existing = project_server;
        } else {
            merged.push(project_server);
        }
    }
    let native_path = cwd.join(".mcp.json");
    for native_server in load_native_mcp(&native_path).unwrap_or_else(|e| {
        tracing::warn!("加载原生项目 MCP 配置失败，忽略: {e}");
        Vec::new()
    }) {
        if let Some(existing) = merged.iter_mut().find(|s| s.name == native_server.name) {
            *existing = native_server;
        } else {
            merged.push(native_server);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpTransport;

    fn mcp(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(command.to_string()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
        }
    }

    fn base_config(mcp_servers: Vec<McpServerConfig>) -> Config {
        Config {
            mcp_servers,
            ..Default::default()
        }
    }

    #[test]
    fn load_project_mcp_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let servers = load_project_mcp(dir.path()).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let servers = vec![mcp("postgres", "npx")];
        save_project_mcp(dir.path(), &servers).unwrap();
        let loaded = load_project_mcp(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "postgres");
    }

    #[test]
    fn merged_project_overrides_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = base_config(vec![mcp("postgres", "global-cmd")]);
        save_project_mcp(dir.path(), &[mcp("postgres", "project-cmd")]).unwrap();

        let merged = merged_mcp_servers(&cfg, dir.path());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].command.as_deref(), Some("project-cmd"));
    }

    #[test]
    fn native_project_mcp_overrides_legacy_toml() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = base_config(vec![mcp("postgres", "global-cmd")]);
        save_project_mcp(dir.path(), &[mcp("postgres", "legacy-project-cmd")]).unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{"postgres":{"command":"native-project-cmd"}}}"#,
        )
        .unwrap();

        let merged = merged_mcp_servers(&cfg, dir.path());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].command.as_deref(), Some("native-project-cmd"));
    }

    #[test]
    fn merged_different_name_appends() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = base_config(vec![mcp("postgres", "global-cmd")]);
        save_project_mcp(dir.path(), &[mcp("fetch", "project-cmd")]).unwrap();

        let merged = merged_mcp_servers(&cfg, dir.path());
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|s| s.name == "postgres"));
        assert!(merged.iter().any(|s| s.name == "fetch"));
    }

    #[test]
    fn merged_no_project_file_equals_global() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = base_config(vec![mcp("postgres", "global-cmd")]);
        let merged = merged_mcp_servers(&cfg, dir.path());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].command.as_deref(), Some("global-cmd"));
    }
}
