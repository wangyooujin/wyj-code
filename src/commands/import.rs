//! import 子命令。

use crate::cli::ImportArgs;
use crate::config::Profile;
use crate::import_parser;
use crate::store;
use anyhow::{anyhow, Result};

pub fn import(args: ImportArgs) -> Result<()> {
    let zshrc_path = match &args.zshrc {
        Some(p) => p.clone(),
        None => {
            let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位家目录"))?;
            home.join(".zshrc").to_string_lossy().into_owned()
        }
    };
    let text = std::fs::read_to_string(&zshrc_path)
        .map_err(|e| anyhow!("读取 {} 失败: {}", zshrc_path, e))?;

    let parsed = import_parser::parse_zshrc(&text, args.name.as_deref());
    if parsed.is_empty() {
        eprintln!("未在 {} 找到可导入的 alias model_*。", zshrc_path);
        return Ok(());
    }

    let mut config = store::load()?;

    if args.dry_run {
        println!("(dry-run 预览,不写入)");
        for (alias_name, p) in &parsed {
            println!("── alias {} → profile {} ──", alias_name, p.name);
            print_profile(p);
        }
        return Ok(());
    }

    for (alias_name, mut p) in parsed {
        if config.get_profile(&p.name).is_some() && !args.force {
            eprintln!("⚠️  跳过已存在的 profile `{}`(用 -f 覆盖)。来源 alias: {}", p.name, alias_name);
            continue;
        }
        let name = p.name.clone();
        if let Some(existing) = config.get_profile_mut(&name) {
            *existing = p;
        } else {
            if config.default_profile.is_none() {
                config.default_profile = Some(name.clone());
            }
            // 重新 borrow 安全:push
            p.name = name.clone();
            config.profiles.push(p);
        }
        println!("已导入 profile: {} (from alias {})", name, alias_name);
    }

    store::save(&config)?;
    Ok(())
}

/// 供 config 菜单调用:默认参数导入。
pub fn import_default() -> Result<()> {
    import(ImportArgs {
        zshrc: None,
        name: None,
        dry_run: false,
        force: false,
    })
}

fn print_profile(p: &Profile) {
    if let Some(v) = &p.base_url {
        println!("  ANTHROPIC_BASE_URL = {}", v);
    }
    if let Some(v) = &p.auth_token {
        let masked = mask(v);
        println!("  ANTHROPIC_AUTH_TOKEN = {}", masked);
    }
    if let Some(v) = &p.model {
        println!("  ANTHROPIC_MODEL = {}", v);
    }
    if let Some(v) = &p.small_fast_model {
        println!("  ANTHROPIC_SMALL_FAST_MODEL = {}", v);
    }
    for (k, v) in &p.env {
        let val = if k.contains("TOKEN") || k.contains("KEY") {
            mask(v)
        } else {
            v.clone()
        };
        println!("  {} = {}", k, val);
    }
}

fn mask(s: &str) -> String {
    if s.len() <= 8 {
        "****".to_string()
    } else {
        format!("{}...{}", &s[..4], &s[s.len() - 4..])
    }
}
