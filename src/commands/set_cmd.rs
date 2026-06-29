//! set / unset / toggle 子命令。

use crate::env_model;
use crate::store;
use anyhow::Result;

pub fn set(profile: String, key: String, value: String) -> Result<()> {
    let mut config = store::load()?;
    if config.get_profile(&profile).is_none() {
        return Err(crate::errors::WyjError::ProfileNotFound(profile, store::profile_names(&config)).into());
    }
    let p = config.get_profile_mut(&profile).unwrap();
    if !p.set_named_field(&key, value.clone()) {
        p.env.insert(key.clone(), value.clone());
    }
    store::save(&config)?;
    println!("已设置 {}::{} = {}", profile, key, value);
    Ok(())
}

pub fn unset(profile: String, key: String) -> Result<()> {
    let mut config = store::load()?;
    if config.get_profile(&profile).is_none() {
        return Err(crate::errors::WyjError::ProfileNotFound(profile, store::profile_names(&config)).into());
    }
    let p = config.get_profile_mut(&profile).unwrap();
    let removed = p.unset_named_field(&key) || p.env.remove(&key).is_some();
    if !removed {
        eprintln!("(profile `{}` 中未找到 `{}`)", profile, key);
        return Ok(());
    }
    store::save(&config)?;
    println!("已删除 {}::{}", profile, key);
    Ok(())
}

pub fn toggle(profile: String, switch: String) -> Result<()> {
    let mut config = store::load()?;
    if config.get_profile(&profile).is_none() {
        return Err(crate::errors::WyjError::ProfileNotFound(profile, store::profile_names(&config)).into());
    }
    if !env_model::is_switch_key(&switch) {
        eprintln!("⚠️  `{}` 不在已知开关列表中(仍将作为通用 env 翻转)。", switch);
    }
    let p = config.get_profile_mut(&profile).unwrap();
    let current = p.env.get(&switch).cloned();
    let next = match current.as_deref() {
        Some("1") | Some("true") => "0".to_string(),
        _ => "1".to_string(),
    };
    p.env.insert(switch.clone(), next.clone());
    store::save(&config)?;
    println!("已翻转 {}::{} → {}", profile, switch, next);
    Ok(())
}
