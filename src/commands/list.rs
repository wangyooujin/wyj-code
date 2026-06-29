//! list 子命令 + 交互式 config 菜单。

use crate::store;
use anyhow::Result;

pub fn list() -> Result<()> {
    let config = store::load()?;
    if config.profiles.is_empty() {
        println!("(暂无 profile,运行 `wyj-code import` 或 `wyj-code add` 新增)");
        return Ok(());
    }
    let default = config.default_profile.as_deref().unwrap_or("");
    let max_name = config.profiles.iter().map(|p| p.name.len()).max().unwrap_or(4);
    for p in &config.profiles {
        let marker = if p.name == default { "*" } else { " " };
        let model = p.model.as_deref().unwrap_or("-");
        let base = p.base_url.as_deref().unwrap_or("-");
        println!(
            "{} {:width$}  model={:<16}  url={}",
            marker,
            p.name,
            model,
            base,
            width = max_name
        );
    }
    Ok(())
}

/// 轻量交互式菜单。
pub fn config_menu() -> Result<()> {
    if !crate::tty::is_tty() {
        eprintln!("`config` 需要交互式终端。请用子命令:list/add/edit/remove/default/import/run/env。");
        std::process::exit(1);
    }
    use dialoguer::Select;
    loop {
        let items = vec![
            "列出 profile",
            "新增 profile",
            "编辑 profile",
            "设置默认 profile",
            "从 zshrc 导入",
            "退出",
        ];
        let idx = Select::new()
            .with_prompt("wyj-code")
            .items(&items)
            .default(0)
            .interact_opt()?
            .unwrap_or(items.len() - 1);
        match idx {
            0 => { list()?; }
            1 => { super::add::add_interactive()?; }
            2 => { edit_interactive()?; }
            3 => { set_default_interactive()?; }
            4 => { super::import::import_default()?; }
            _ => return Ok(()),
        }
        println!();
    }
}

fn edit_interactive() -> Result<()> {
    use dialoguer::Select;
    let config = store::load()?;
    if config.profiles.is_empty() {
        println!("(暂无 profile)");
        return Ok(());
    }
    let names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let idx = Select::new()
        .with_prompt("选择要编辑的 profile")
        .items(&names)
        .default(0)
        .interact_opt()?
        .unwrap_or(0);
    let name = names[idx].to_string();
    super::edit::edit(name, false)?;
    Ok(())
}

fn set_default_interactive() -> Result<()> {
    use dialoguer::Select;
    let config = store::load()?;
    if config.profiles.is_empty() {
        println!("(暂无 profile)");
        return Ok(());
    }
    let names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let idx = Select::new()
        .with_prompt("选择默认 profile")
        .items(&names)
        .default(0)
        .interact_opt()?
        .unwrap_or(0);
    let name = names[idx].to_string();
    super::default_cmd::default_cmd(Some(name))?;
    Ok(())
}
