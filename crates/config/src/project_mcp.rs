//! 项目级 MCP 配置：`<git-root>/.wyj-code/mcp.toml`
//!
//! 格式与全局 `config.toml` 的 `[[mcp_servers]]` 段一致，同名 server 覆盖全局配置，
//! 不同名则追加。项目根由 `project_config_dir` 统一解析，因此从仓库子目录启动
//! 仍会读取仓库根配置，与 Skill/settings/agent/lockfile 的项目边界一致。
//!
//! **不再读取任何原生 Claude Code / Codex 等外部源**：wyj-code 只读写
//! `~/.wyj-code/`（全局）与 `<git-root>/.wyj-code/`（项目级），其它路径一律不
//! 触碰。`load_native_mcp` / `native_mcp_names` 等历史兼容工具已于 v1.5.15 删除。

use crate::{Config, McpServerConfig};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectMcpConfig {
    pub mcp_servers: Vec<McpServerConfig>,
}

/// 返回项目级 MCP 配置文件路径（`<git-root>/.wyj-code/mcp.toml`）。
pub fn project_mcp_path(cwd: &Path) -> PathBuf {
    crate::project_config_dir(cwd).join("mcp.toml")
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

/// 保存项目级 MCP server 列表（会自动创建 `.wyj-code/` 目录）。
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
///
/// 不再读取 `<cwd>/.mcp.json` 等原生配置——wyj-code 只信任用户自己在
/// `~/.wyj-code/config.toml` 与 `<git-root>/.wyj-code/mcp.toml` 里写下的条目。
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
    fn nested_cwd_loads_mcp_from_git_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("crates").join("demo");
        std::fs::create_dir_all(&nested).unwrap();

        save_project_mcp(repo.path(), &[mcp("postgres", "project-cmd")]).unwrap();
        let loaded = load_project_mcp(&nested).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].command.as_deref(), Some("project-cmd"));
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
