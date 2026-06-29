//! completions 子命令:生成 shell 补全脚本。

use anyhow::{anyhow, Result};
use clap::CommandFactory;
use clap_complete::{generate, Shell};

pub fn completions(shell: &str) -> Result<()> {
    let sh = parse_shell(shell)?;
    let mut cmd = crate::cli::Cli::command();
    let bin = "wyj-code";
    generate(sh, &mut cmd, bin, &mut std::io::stdout());
    Ok(())
}

fn parse_shell(s: &str) -> Result<Shell> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "bash" => Shell::Bash,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        "elvish" => Shell::Elvish,
        "powershell" | "pwsh" => Shell::PowerShell,
        other => return Err(anyhow!("不支持的 shell: {}(可选 bash/zsh/fish/elvish/powershell)", other)),
    })
}
