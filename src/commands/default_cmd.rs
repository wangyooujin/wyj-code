//! default / use 子命令。

use crate::config::Config;
use crate::store;
use anyhow::Result;

pub fn default_cmd(name: Option<String>) -> Result<()> {
    let mut config = store::load()?;
    match name {
        Some(n) => {
            if config.get_profile(&n).is_none() {
                return Err(crate::errors::WyjError::ProfileNotFound(n, store::profile_names(&config)).into());
            }
            config.default_profile = Some(n.clone());
            store::save(&config)?;
            println!("默认 profile 已设为: {}", n);
            Ok(())
        }
        None => {
            match &config.default_profile {
                Some(n) => {
                    println!("{}", n);
                    Ok(())
                }
                None => {
                    println!("(未设置默认 profile)");
                    Ok(())
                }
            }
        }
    }
}

#[allow(dead_code)]
fn _unused(_c: &Config) {}
