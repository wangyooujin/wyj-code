mod cli;
mod commands;
mod config;
mod env_model;
mod errors;
mod fs_util;
mod import_parser;
mod keychain;
mod launcher;
mod merge;
mod store;
mod tty;

use clap::Parser;
use cli::Cli;

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        None => commands::run::run(vec![]),
        Some(cmd) => commands::dispatch(cmd),
    };

    if let Err(e) = result {
        eprintln!("错误: {}", e);
        let mut source = e.source();
        while let Some(s) = source {
            eprintln!("  原因: {}", s);
            source = s.source();
        }
        std::process::exit(1);
    }
}
