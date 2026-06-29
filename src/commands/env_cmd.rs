//! env 子命令:输出 export 语句供 eval。

use crate::merge;
use crate::store;
use anyhow::Result;

pub fn env(profile: Option<String>) -> Result<()> {
    let config = store::load()?;
    let name = store::resolve_profile_name(&config, profile.as_deref())?;
    let profile = config
        .get_profile(&name)
        .ok_or_else(|| anyhow::anyhow!("内部错误:profile `{}` 未找到", name))?;
    let mut env = merge::merge_env(&config, profile);
    crate::keychain::maybe_overlay_token(&profile, &mut env)?;
    println!("{}", merge::format_exports(&env));
    Ok(())
}
