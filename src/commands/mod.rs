//! 子命令实现。

pub mod add;
pub mod completions;
pub mod default_cmd;
pub mod edit;
pub mod env_cmd;
pub mod import;
pub mod list;
pub mod remove;
pub mod run;
pub mod set_cmd;
pub mod token;

use crate::cli::Command;
use anyhow::Result;

pub fn dispatch(cmd: Command) -> Result<()> {
    match cmd {
        Command::Run { args } => run::run(args),
        Command::Env { profile } => env_cmd::env(profile),
        Command::List => list::list(),
        Command::Add(a) => add::add(a),
        Command::Edit { name, raw } => edit::edit(name, raw),
        Command::Remove { name, force } => remove::remove(name, force),
        Command::Default { name } => default_cmd::default_cmd(name),
        Command::Use { profile } => default_cmd::default_cmd(Some(profile)),
        Command::Import(a) => import::import(a),
        Command::Set { profile, key, value } => set_cmd::set(profile, key, value),
        Command::Unset { profile, key } => set_cmd::unset(profile, key),
        Command::Toggle { profile, switch } => set_cmd::toggle(profile, switch),
        Command::Config => crate::commands::list::config_menu(),
        Command::Completions { shell } => completions::completions(&shell),
        Command::Token { profile, action } => token::token(profile, action),
    }
}
