//! edit 子命令。

use crate::store;
use anyhow::{anyhow, Result};
use std::path::PathBuf;

pub fn edit(name: String, raw: bool) -> Result<()> {
    if raw {
        return edit_raw();
    }
    let mut config = store::load()?;
    if config.get_profile(&name).is_none() {
        return Err(crate::errors::WyjError::ProfileNotFound(name.clone(), store::profile_names(&config)).into());
    }
    let profile = config.get_profile_mut(&name).unwrap();

    profile.base_url = prompt_opt("ANTHROPIC_BASE_URL", profile.base_url.as_deref())?;
    profile.model = prompt_opt("ANTHROPIC_MODEL", profile.model.as_deref())?;
    profile.small_fast_model = prompt_opt("ANTHROPIC_SMALL_FAST_MODEL", profile.small_fast_model.as_deref())?;
    profile.haiku_model = prompt_opt("ANTHROPIC_DEFAULT_HAIKU_MODEL", profile.haiku_model.as_deref())?;
    profile.sonnet_model = prompt_opt("ANTHROPIC_DEFAULT_SONNET_MODEL", profile.sonnet_model.as_deref())?;
    profile.opus_model = prompt_opt("ANTHROPIC_DEFAULT_OPUS_MODEL", profile.opus_model.as_deref())?;
    profile.max_context_tokens = prompt_opt("CLAUDE_CODE_MAX_CONTEXT_TOKENS", profile.max_context_tokens.as_deref())?;
    profile.timeout_ms = prompt_opt("API_TIMEOUT_MS", profile.timeout_ms.as_deref())?;

    store::save(&config)?;
    println!("已更新 profile: {}", name);
    Ok(())
}

fn prompt_opt(label: &str, current: Option<&str>) -> Result<Option<String>> {
    use dialoguer::Input;
    let v: String = Input::new()
        .with_prompt(label)
        .allow_empty(true)
        .default(current.unwrap_or("").to_string())
        .interact_text()?;
    Ok(if v.is_empty() { None } else { Some(v) })
}

fn edit_raw() -> Result<()> {
    let path: PathBuf = store::config_path()?;
    if !path.exists() {
        // 创建空配置骨架再打开
        store::save(&crate::config::Config::default())?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|e| anyhow!("启动编辑器 {} 失败: {}", editor, e))?;
    if !status.success() {
        return Err(anyhow!("编辑器退出码非零"));
    }
    Ok(())
}
