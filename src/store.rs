//! profiles.toml 的加载/保存/CRUD。

use crate::config::Config;
use crate::errors::WyjError;
use crate::fs_util::{is_mode_too_open, write_atomic_secure};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 返回配置目录 ~/.wyj-code。
pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("无法定位家目录")?;
    Ok(home.join(".wyj-code"))
}

/// 返回配置文件路径 ~/.wyj-code/profiles.toml。
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("profiles.toml"))
}

/// 加载配置。文件不存在时返回空 Config(不报错)。
/// TOML 损坏时返回 WyjError::TomlParse(绝不覆写)。
pub fn load() -> Result<Config> {
    let path = config_path()?;
    load_from(&path)
}

pub fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    if is_mode_too_open(path) {
        eprintln!("⚠️  配置文件 {} 权限过宽(含 token),将在下次保存时收紧为 0600。", path.display());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置失败: {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| WyjError::TomlParse {
        path: path.display().to_string(),
        err: e.to_string(),
    })?;
    cfg.warn_on_duplicate_profiles();
    Ok(cfg)
}

/// 保存配置(原子写 + 0600)。
pub fn save(config: &Config) -> Result<()> {
    let path = config_path()?;
    save_to(config, &path)
}

pub fn save_to(config: &Config, path: &Path) -> Result<()> {
    let text = toml::to_string_pretty(config).context("序列化配置失败")?;
    write_atomic_secure(path, &text)
}

impl Config {
    /// 检测重名 profile 并向 stderr 警告。
    pub fn warn_on_duplicate_profiles(&self) {
        let mut seen = std::collections::HashSet::new();
        for p in &self.profiles {
            if !seen.insert(&p.name) {
                eprintln!("⚠️  配置中存在重名 profile `{}`,将使用首个匹配。", p.name);
            }
        }
    }
}

/// profile 名列表,逗号拼接,用于错误提示。
pub fn profile_names(config: &Config) -> String {
    if config.profiles.is_empty() {
        "(无)".to_string()
    } else {
        config
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 解析 profile(指定名或默认),失败返回友好错误。
pub fn resolve_profile_name(config: &Config, given: Option<&str>) -> Result<String> {
    if let Some(name) = given {
        if config.get_profile(name).is_none() {
            return Err(WyjError::ProfileNotFound(name.to_string(), profile_names(config)).into());
        }
        return Ok(name.to_string());
    }
    match &config.default_profile {
        Some(name) if config.get_profile(name).is_some() => Ok(name.clone()),
        Some(name) => Err(WyjError::ProfileNotFound(name.clone(), profile_names(config)).into()),
        None => {
            if config.profiles.is_empty() {
                Err(WyjError::NoProfiles.into())
            } else {
                Err(WyjError::NoDefaultProfile(profile_names(config)).into())
            }
        }
    }
}
