//! wyj-store — MCP / Skill 配置管理中心的数据层
//!
//! 承载：安装元数据 lockfile、MCP Registry HTTP client、Skill git marketplace
//! client、MCP/Skill 的 install/upgrade/uninstall/enable 编排逻辑。
//!
//! 严格遵守"只管配置，不碰依赖安装"：这里只负责把 command/args/env/version 等
//! 配置信息写入本地文件（config.toml、`.wyj/mcp.toml`、`.md`、lockfile），绝不
//! shell out 执行 `npm install`/`pip install` 之类的依赖安装命令。

pub mod extensions;
pub mod lockfile;
pub mod marketplace;
pub mod mcp_install;
pub mod plugin_install;
pub mod plugin_manifest;
pub mod registry;
pub mod self_update;
pub mod skill_install;

pub use lockfile::{disabled_mcp_names, disabled_skill_names, InstallScope};

/// `upgrade_mcp_server`/`upgrade_skill` 的结果：区分"确实拉到了新版本并覆盖安装"
/// 和"registry/marketplace 上已经是当前版本，未做任何改动"，供 UI 展示不同文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    Upgraded { version: String },
    AlreadyLatest { version: String },
}

impl UpgradeOutcome {
    pub fn version(&self) -> &str {
        match self {
            UpgradeOutcome::Upgraded { version } => version,
            UpgradeOutcome::AlreadyLatest { version } => version,
        }
    }
}
