//! token 子命令:管理 keychain 中的 AUTH_TOKEN,并切换 profile 的 keychain 标志。

use crate::cli::TokenAction;
use crate::store;
use anyhow::Result;

pub fn token(profile: String, action: TokenAction) -> Result<()> {
    match action {
        TokenAction::Set => set(profile),
        TokenAction::Get => get(profile),
        TokenAction::Delete => delete(profile),
    }
}

fn set(profile: String) -> Result<()> {
    let mut config = store::load()?;
    if config.get_profile(&profile).is_none() {
        return Err(crate::errors::WyjError::ProfileNotFound(profile, store::profile_names(&config)).into());
    }
    if !crate::tty::is_tty() {
        return Err(anyhow::anyhow!("token set 需要交互式终端"));
    }
    use dialoguer::Password;
    let token = Password::new()
        .with_prompt("输入 AUTH_TOKEN(存入 Keychain,不回显)")
        .allow_empty_password(false)
        .interact()?;

    crate::keychain::set(&profile, &token)?;

    // 标记 profile 使用 keychain,并清除明文 token(若存在)
    let p = config.get_profile_mut(&profile).unwrap();
    p.keychain = true;
    p.auth_token = None;
    store::save(&config)?;

    println!("已将 {} 的 token 存入 Keychain,并切换为 keychain 模式(明文 token 已清除)。", profile);
    Ok(())
}

fn get(profile: String) -> Result<()> {
    let token = crate::keychain::get(&profile)?;
    println!("{}", token);
    Ok(())
}

fn delete(profile: String) -> Result<()> {
    let mut config = store::load()?;
    crate::keychain::delete(&profile)?;
    // 关闭 keychain 标志
    if let Some(p) = config.get_profile_mut(&profile) {
        if p.keychain {
            p.keychain = false;
            store::save(&config)?;
        }
    }
    println!("已从 Keychain 删除 {} 的 token。", profile);
    Ok(())
}
