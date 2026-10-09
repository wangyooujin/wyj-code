//! 项目级 MCP server 信任确认。
//!
//! `<git-root>/.wyj-code/mcp.toml` 里的 `command`/`args` 会被当作子进程直接执行，
//! 且随 `git clone` 一起落地——克隆一个陌生仓库、或者给这个仓库配了 `wyj-code
//! schedule` 定时任务，都可能在用户没意识到的情况下静默执行仓库自带的任意命
//! 令。本模块只覆盖"项目级来源"的 server（不含全局 `~/.wyj-code/config.toml`
//! 的 `[[mcp_servers]]`，那是用户自己机器上的配置，天然可信），要求用户在首
//! 次连接前显式批准一次；批准记录必须落在仓库内容控制不到的位置——
//! `~/.wyj-code/projects/<project_key>/`（与既有 `allowed_tools.json` 同级），
//! 否则被信任的仓库自己就能在同一个受版本控制的文件里悄悄把"已批准"标记也
//! 改掉，形同虚设。
//!
//! 自 v1.5.15 起不再读取 `<cwd>/.mcp.json` 等"原生"配置——wyj-code 只读
//! `<git-root>/.wyj-code/mcp.toml` 一个项目级来源。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use wyj_config::McpServerConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectTrustRecord {
    approved_fingerprint: String,
    approved_at: DateTime<Utc>,
}

/// 首次连接前需要用户批准与否的状态。
pub enum TrustStatus {
    /// 项目根本没有定义项目级 MCP server，不需要弹窗。
    NoProjectServers,
    /// 已批准过当前内容对应的指纹。
    Trusted,
    /// 有项目级 server，但从未批准过，或内容已变化（批准记录对应的指纹
    /// 与当前指纹不一致——含"被 git pull 悄悄替换过"的场景）。
    Pending(Vec<McpServerConfig>),
}

/// 只读 `<git-root>/.wyj-code/mcp.toml` 一个项目级来源——不再合并
/// `<cwd>/.mcp.json` 等"原生"配置（v1.5.15 起）。
fn project_scoped_mcp_servers(cwd: &Path) -> Vec<McpServerConfig> {
    wyj_config::load_project_mcp(cwd).unwrap_or_else(|e| {
        tracing::warn!("加载项目级 MCP 配置失败，忽略: {e}");
        Vec::new()
    })
}

/// 对项目级来源的 server 列表计算指纹：按 name 排序后规范序列化再 sha256，
/// 避免注释/空白/字段书写顺序变动导致误判"配置变了"，只在语义内容真的
/// 变化时才让指纹变化。
pub fn compute_project_mcp_fingerprint(cwd: &Path) -> String {
    let mut servers = project_scoped_mcp_servers(cwd);
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    let canonical = serde_json::to_string(&servers).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn trust_path(cwd: &Path) -> Result<PathBuf> {
    project_trust_record_path(cwd, "mcp_trust.json")
}

/// 信任记录的落盘位置：`~/.wyj-code/projects/<project_key>/<file_name>`。
///
/// **必须落在仓库内容控制不到的位置**——若写在 `<git-root>/.wyj-code/` 下，
/// 被信任的仓库自己就能在同一个受版本控制的文件里把「已批准」标记改掉，
/// 形同虚设。`skill_trust` 模块复用本函数以保持这条不变式一致。
pub(crate) fn project_trust_record_path(cwd: &Path, file_name: &str) -> Result<PathBuf> {
    let key = wyj_core::project_key(cwd);
    Ok(wyj_config::config_dir()?
        .join("projects")
        .join(key)
        .join(file_name))
}

fn load_record(cwd: &Path) -> Option<ProjectTrustRecord> {
    let path = trust_path(cwd).ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// 当前信任状态。
pub fn trust_status(cwd: &Path) -> TrustStatus {
    let servers = project_scoped_mcp_servers(cwd);
    if servers.is_empty() {
        return TrustStatus::NoProjectServers;
    }
    let current_fingerprint = compute_project_mcp_fingerprint(cwd);
    match load_record(cwd) {
        Some(record) if record.approved_fingerprint == current_fingerprint => TrustStatus::Trusted,
        _ => TrustStatus::Pending(servers),
    }
}

/// 批准当前项目级 MCP 配置（写入当前指纹 + 时间戳）。
pub fn approve(cwd: &Path) -> Result<()> {
    let path = trust_path(cwd)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建信任记录目录失败: {}", parent.display()))?;
    }
    let record = ProjectTrustRecord {
        approved_fingerprint: compute_project_mcp_fingerprint(cwd),
        approved_at: Utc::now(),
    };
    let content = serde_json::to_string_pretty(&record).context("序列化信任记录失败")?;
    std::fs::write(&path, content).with_context(|| format!("写入信任记录失败: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_config::McpTransport;

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

    #[test]
    fn no_project_servers_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            trust_status(dir.path()),
            TrustStatus::NoProjectServers
        ));
    }

    #[test]
    fn pending_until_approved_then_trusted() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(dir.path(), &[mcp("postgres", "npx")]).unwrap();

        assert!(matches!(trust_status(dir.path()), TrustStatus::Pending(_)));

        approve(dir.path()).unwrap();
        assert!(matches!(trust_status(dir.path()), TrustStatus::Trusted));
    }

    #[test]
    fn changing_content_after_approval_goes_back_to_pending() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(dir.path(), &[mcp("postgres", "npx")]).unwrap();
        approve(dir.path()).unwrap();
        assert!(matches!(trust_status(dir.path()), TrustStatus::Trusted));

        // 内容被改过（如 git pull 带来新提交）：指纹变化，需要重新批准。
        wyj_config::save_project_mcp(dir.path(), &[mcp("postgres", "curl evil.example.com")])
            .unwrap();
        assert!(matches!(trust_status(dir.path()), TrustStatus::Pending(_)));
    }

    #[test]
    fn fingerprint_stable_across_reordering() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(dir.path(), &[mcp("zeta", "npx"), mcp("alpha", "npx")])
            .unwrap();
        let fp1 = compute_project_mcp_fingerprint(dir.path());

        wyj_config::save_project_mcp(dir.path(), &[mcp("alpha", "npx"), mcp("zeta", "npx")])
            .unwrap();
        let fp2 = compute_project_mcp_fingerprint(dir.path());

        assert_eq!(fp1, fp2);
    }
}
