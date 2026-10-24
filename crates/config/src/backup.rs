//! 配置备份用的 profile 片段：`wyj-code export` / `import` 的配置载体。
//!
//! # 为什么放在 config crate
//!
//! 备份归档里的配置文件**只包含** `active_profile` 与 `profiles` 两个字段，
//! 既不是完整的 `Config`，也不能直接拿 `Config` 序列化——那样会把 `mcp_servers`
//! 与全部全局开关一起打进包里，导入时再整块盖回去，等于用一次「恢复备份」悄悄
//! 改掉用户的权限档位与 MCP 配置。
//!
//! 而"哪些字段算 profile"这件事只有 `Profile` 的定义方知道。放在这里，`Profile`
//! 新增字段时编译器会直接指出本文件需要跟着改；放在 `store` 侧则要复制一份结构，
//! 新字段会被静默漏掉——正是备份功能最不该出的错。
//!
//! toml 的序列化也就近落在本 crate（`store` 没有 toml 依赖，也不该为一个片段加）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{Config, Profile};

/// 归档内的配置片段。字段刻意只有两个。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesFragment {
    pub active_profile: String,
    pub profiles: Vec<Profile>,
}

impl ProfilesFragment {
    /// 从当前配置里抠出片段。
    pub fn from_config(config: &Config) -> Self {
        Self {
            active_profile: config.active_profile.clone(),
            profiles: config.profiles.clone(),
        }
    }

    /// 序列化成 toml 文本（写进归档）。
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("序列化 profile 片段失败")
    }

    /// 从归档里的 toml 文本还原。
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("profile 片段无法解析，备份可能已损坏")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 片段必须**只**带两个字段：多写一个字段就意味着备份包里会多出用户的
    /// 全局策略（比如 `mcp_servers`），导入时整块盖回去。
    #[test]
    fn fragment_contains_only_profiles_and_active_profile() {
        let config = Config {
            active_profile: "work".into(),
            log_level: "debug".into(),
            ..Config::default()
        };

        let text = ProfilesFragment::from_config(&config).to_toml().unwrap();
        assert!(text.contains("active_profile"));
        assert!(text.contains("[[profiles]]"));
        assert!(!text.contains("log_level"), "全局开关不该进备份: {text}");
        assert!(!text.contains("mcp_servers"), "备份不装 MCP 配置: {text}");
    }

    /// 往返后 profile 的每个字段都要还原，尤其是 `api_key` / `api_key_env`——
    /// 备份的价值就在这些凭据上。
    #[test]
    fn round_trip_preserves_every_profile_field() {
        let profile = Profile {
            name: "mini".into(),
            api_key: Some("sk-test".into()),
            api_key_env: Some("MINI_KEY".into()),
            base_url: "https://example.invalid/anthropic".into(),
            aux_model: Some("aux".into()),
            reasoning_effort: Some("high".into()),
            max_tools_per_turn: Some(4),
            ..Profile::default()
        };

        let config = Config {
            active_profile: "mini".into(),
            profiles: vec![profile.clone()],
            ..Config::default()
        };
        let text = ProfilesFragment::from_config(&config).to_toml().unwrap();
        let back = ProfilesFragment::from_toml(&text).unwrap();

        assert_eq!(back.active_profile, "mini");
        assert_eq!(back.profiles.len(), 1);
        let restored = &back.profiles[0];
        assert_eq!(restored.name, "mini");
        assert_eq!(restored.api_key.as_deref(), Some("sk-test"));
        assert_eq!(restored.api_key_env.as_deref(), Some("MINI_KEY"));
        assert_eq!(restored.base_url, "https://example.invalid/anthropic");
        assert_eq!(restored.aux_model.as_deref(), Some("aux"));
        assert_eq!(restored.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(restored.max_tools_per_turn, Some(4));
    }

    #[test]
    fn broken_toml_is_reported_not_panicked() {
        assert!(ProfilesFragment::from_toml("this is not toml = [").is_err());
    }
}
