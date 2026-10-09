//! `wyj-code trust-mcp` / `trust-skills` / `trust`：项目级资源的信任确认 CLI 入口。
//!
//! 两类资源需要用户显式批准，无 UI 通道的场景（`-p`/`--headless`/`wyj-code schedule
//! run`）一律跳过未信任的项目级内容、不静默放行：
//!
//! - **MCP server**（`.wyj-code/mcp.toml`）：`command`/`args` 会被当作子进程直接执行。
//! - **skill**（`.wyj-code/skills/`）：正文是纯文本指令，敲 `/xxx` 后整段作为
//!   模型指令进入对话。
//!
//! 这两个命令供用户在配置定时任务前，先手动交互批准一次，之后无人值守的调用
//! 就能正常工作。
//!
//! **信任决策必须逐类可见**：把 skill 并入 `trust-mcp` 会产生"以为在批准 MCP
//! 却连带批准了仓库作者写的指令正文"的真实误操作。因此 `trust-mcp` 保持只管
//! MCP（已上线、可能被用户脚本化），另设 `trust-skills`，`trust` 是两者的总入口。

use anyhow::Result;
use std::io::{self, Write};
use std::path::Path;
use wyj_store::project_trust::{self, TrustStatus};

/// `trust-mcp` / `trust-skills` / `trust` 的作用范围
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustScope {
    /// 只管 MCP server（`trust-mcp` 的历史行为，逐字保留）
    Mcp,
    /// 只管项目级 skill
    Skills,
    /// 两者都管（`trust`）
    All,
}

pub async fn run(cwd: &Path) -> Result<()> {
    run_with_scope(cwd, TrustScope::Mcp).await
}

pub async fn run_with_scope(cwd: &Path, scope: TrustScope) -> Result<()> {
    if matches!(scope, TrustScope::Mcp | TrustScope::All) {
        run_mcp(cwd)?;
    }
    if matches!(scope, TrustScope::Skills | TrustScope::All) {
        run_skills(cwd)?;
    }
    Ok(())
}

fn run_mcp(cwd: &Path) -> Result<()> {
    match project_trust::trust_status(cwd) {
        TrustStatus::NoProjectServers => {
            println!("当前项目没有定义项目级 MCP server（.wyj-code/mcp.toml / .mcp.json 为空），无需批准。");
            Ok(())
        }
        TrustStatus::Trusted => {
            println!("当前项目级 MCP server 已批准过，且内容未变化。");
            Ok(())
        }
        TrustStatus::Pending(servers) => {
            println!("以下项目级 MCP server 尚未批准，首次连接前需要确认信任：");
            for server in &servers {
                let target = server
                    .command
                    .as_deref()
                    .map(|c| {
                        if server.args.is_empty() {
                            c.to_string()
                        } else {
                            format!("{c} {}", server.args.join(" "))
                        }
                    })
                    .or_else(|| server.url.clone())
                    .unwrap_or_default();
                println!("  - {}: {}", server.name, target);
            }
            print!("批准以上 server 并允许连接？[y/N] ");
            io::stdout().flush().ok();
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let answer = input.trim().to_lowercase();
            if answer == "y" || answer == "yes" {
                project_trust::approve(cwd)?;
                println!("已批准，之后（含无人值守场景）会正常连接这些 server。");
            } else {
                println!("已取消，这些 server 仍不会被连接。");
            }
            Ok(())
        }
    }
}

fn run_skills(cwd: &Path) -> Result<()> {
    let disabled = wyj_store::disabled_skill_names(cwd);
    let infos = wyj_commands::skill::project_skill_infos(cwd, &disabled);

    if infos.is_empty() {
        println!("当前项目没有需要批准的项目级 skill（.wyj-code/skills/ 为空或全部被禁用）。");
        return Ok(());
    }
    if wyj_store::skill_trust::project_skills_trusted(cwd) {
        println!("当前项目级 skill 已批准过，且内容未变化。");
        return Ok(());
    }

    println!("以下项目级 skill 尚未批准（内容来自本仓库，作者为仓库提交者）：");
    for info in &infos {
        println!("  - /{}: {}", info.name, info.description);
    }
    println!("批准后，执行任一个都会把它的整段正文作为指令交给模型。");
    print!("批准以上 skill？[y/N] ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    if answer == "y" || answer == "yes" {
        wyj_store::skill_trust::approve_skills(cwd)?;
        println!("已批准，之后（含无人值守场景）这些 skill 会被加载。");
    } else {
        println!("已取消，这些 skill 仍不会被加载。");
    }
    Ok(())
}
