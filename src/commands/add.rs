//! add 子命令。

use crate::cli::AddArgs;
use crate::config::{Config, Profile};
use crate::store;
use crate::tty::is_tty;
use anyhow::{anyhow, Result};

pub fn add(args: AddArgs) -> Result<()> {
    let mut config = store::load()?;

    // 非 TTY 且关键字段缺失 → 报错(不 hang)
    let need_interactive = args.name.is_none()
        || args.base_url.is_none()
        || args.auth_token.is_none()
        || args.model.is_none();

    if need_interactive && !is_tty() {
        return Err(anyhow!(
            "非交互环境需提供完整 flag:--base-url --auth-token --model(及可选 NAME)"
        ));
    }

    let (name, mut profile) = if need_interactive {
        add_interactive_with(&args)?
    } else {
        let name = args.name.clone().unwrap();
        let mut p = Profile {
            name: name.clone(),
            base_url: args.base_url.clone(),
            auth_token: args.auth_token.clone(),
            api_key: args.api_key.clone(),
            model: args.model.clone(),
            small_fast_model: args.small_fast_model.clone(),
            ..Default::default()
        };
        apply_set(&mut p, &args.set)?;
        (name, p)
    };

    // 覆盖检查
    if config.get_profile(&name).is_some() && !args.force {
        return Err(crate::errors::WyjError::ProfileExists(name).into());
    }
    profile.name = name.clone();

    // 替换或新增
    if let Some(existing) = config.get_profile_mut(&name) {
        *existing = profile;
    } else {
        config.profiles.push(profile);
    }

    // 首个 profile 自动设默认(除非 --no-default)
    let is_first = config.default_profile.is_none();
    if is_first && !args.no_default {
        config.default_profile = Some(name.clone());
        println!("(自动设为默认 profile)");
    }

    store::save(&config)?;
    println!("已保存 profile: {}", name);
    Ok(())
}

/// 交互式新增(供 config 菜单调用)。
pub fn add_interactive() -> Result<()> {
    add(AddArgs {
        name: None,
        base_url: None,
        auth_token: None,
        api_key: None,
        model: None,
        small_fast_model: None,
        set: vec![],
        no_default: false,
        force: false,
    })
}

fn add_interactive_with(args: &AddArgs) -> Result<(String, Profile)> {
    use dialoguer::{Input, Password};
    let name: String = Input::new()
        .with_prompt("profile 名")
        .default(args.name.clone().unwrap_or_default())
        .interact_text()?;
    if name.is_empty() {
        return Err(anyhow!("profile 名不能为空"));
    }
    let base_url: String = Input::new()
        .with_prompt("ANTHROPIC_BASE_URL")
        .default(args.base_url.clone().unwrap_or_default())
        .interact_text()?;
    let auth_token = Password::new()
        .with_prompt("ANTHROPIC_AUTH_TOKEN(不回显)")
        .allow_empty_password(true)
        .interact()?;
    let model: String = Input::new()
        .with_prompt("ANTHROPIC_MODEL")
        .default(args.model.clone().unwrap_or_default())
        .interact_text()?;
    let small_fast_model: String = Input::new()
        .with_prompt("ANTHROPIC_SMALL_FAST_MODEL(可留空)")
        .allow_empty(true)
        .default(args.small_fast_model.clone().unwrap_or_default())
        .interact_text()?;

    let mut p = Profile {
        name: name.clone(),
        base_url: some_if_nonempty(base_url),
        auth_token: some_if_nonempty(auth_token),
        model: some_if_nonempty(model),
        small_fast_model: some_if_nonempty(small_fast_model),
        ..Default::default()
    };
    if let Some(k) = &args.api_key {
        p.api_key = Some(k.clone());
    }
    apply_set(&mut p, &args.set)?;
    Ok((name, p))
}

fn apply_set(profile: &mut Profile, set: &[String]) -> Result<()> {
    for s in set {
        let (k, v) = s
            .split_once('=')
            .ok_or_else(|| anyhow!("--set 需 KEY=VALUE 格式,得到: {}", s))?;
        if !profile.set_named_field(k, v.to_string()) {
            profile.env.insert(k.to_string(), v.to_string());
        }
    }
    Ok(())
}

fn some_if_nonempty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

#[allow(dead_code)]
fn _unused(_c: &Config) {}
