//! remove 子命令。

use crate::store;
use anyhow::Result;

pub fn remove(name: String, force: bool) -> Result<()> {
    let mut config = store::load()?;
    if config.get_profile(&name).is_none() {
        return Err(crate::errors::WyjError::ProfileNotFound(name, store::profile_names(&config)).into());
    }

    if !force {
        if !crate::tty::is_tty() {
            return Err(anyhow::anyhow!("非交互环境删除需 -f 确认"));
        }
        use dialoguer::Confirm;
        let yes = Confirm::new()
            .with_prompt(format!("删除 profile `{}`?", name))
            .default(false)
            .interact()?;
        if !yes {
            println!("已取消");
            return Ok(());
        }
    }

    config.profiles.retain(|p| p.name != name);
    // 删的是默认 → 清空默认并警告
    if config.default_profile.as_deref() == Some(name.as_str()) {
        config.default_profile = None;
        eprintln!("⚠️  删除的是默认 profile,已清空默认。用 `wyj-code default <name>` 重设。");
    }
    store::save(&config)?;
    println!("已删除 profile: {}", name);
    Ok(())
}
