//! 项目级设置：`<git-root>/.wyj-code/settings.toml`
//!
//! 只负责"本项目禁用哪些 skill / MCP server"的开关，不涉及 skill/MCP 本身的
//! 内容定义（那些分别在 `.wyj-code/skills/` 目录与 `.wyj-code/mcp.toml`）。
//! 与 lockfile 里 `enabled: false` 的区别：lockfile 的禁用仅覆盖走
//! `/extensions install` 装进来的条目，本文件按名字禁用，无论条目来源
//! （六层合并链任意一层、手写进 mcp.toml），供
//! `wyj_store::lockfile::{disabled_skill_names, disabled_mcp_names}` union。

use crate::write_atomic;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSettings {
    pub disabled_skills: Vec<String>,
    pub disabled_mcp_servers: Vec<String>,
}

/// 返回项目级设置文件路径（`<git-root>/.wyj-code/settings.toml`）。
pub fn project_settings_path(cwd: &Path) -> PathBuf {
    crate::project_config_dir(cwd).join("settings.toml")
}

/// 加载项目级设置；文件不存在则返回默认空值。
pub fn load_project_settings(cwd: &Path) -> Result<ProjectSettings> {
    let path = project_settings_path(cwd);
    if !path.exists() {
        return Ok(ProjectSettings::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取项目级设置失败: {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("解析项目级设置失败: {}", path.display()))
}

/// 保存项目级设置（会自动创建 `.wyj-code/` 目录）。
pub fn save_project_settings(cwd: &Path, settings: &ProjectSettings) -> Result<()> {
    let path = project_settings_path(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建项目配置目录失败: {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(settings).context("序列化项目级设置失败")?;
    write_atomic(&path, &content).with_context(|| format!("写入项目级设置失败: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_project_settings(dir.path()).unwrap();
        assert!(settings.disabled_skills.is_empty());
        assert!(settings.disabled_mcp_servers.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ProjectSettings {
            disabled_skills: vec!["review".to_string()],
            disabled_mcp_servers: vec!["postgres".to_string()],
        };
        save_project_settings(dir.path(), &settings).unwrap();
        let loaded = load_project_settings(dir.path()).unwrap();
        assert_eq!(loaded.disabled_skills, vec!["review".to_string()]);
        assert_eq!(loaded.disabled_mcp_servers, vec!["postgres".to_string()]);
    }

    #[test]
    fn nested_cwd_loads_settings_from_git_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("src").join("feature");
        std::fs::create_dir_all(&nested).unwrap();
        let settings = ProjectSettings {
            disabled_skills: vec!["review".to_string()],
            disabled_mcp_servers: vec!["postgres".to_string()],
        };
        save_project_settings(repo.path(), &settings).unwrap();

        let loaded = load_project_settings(&nested).unwrap();
        assert_eq!(loaded.disabled_skills, vec!["review".to_string()]);
        assert_eq!(loaded.disabled_mcp_servers, vec!["postgres".to_string()]);
    }
}
