//! `wyj-code trust-mcp`：项目级 MCP server 信任确认的 CLI 入口。
//!
//! `.wyj-code/mcp.toml`/`<cwd>/.mcp.json` 里的 server 会被当作子进程直接
//! 执行，无 UI 通道的场景（`-p`/`--headless`/`wyj-code schedule run`）一律
//! 跳过未信任的项目级 server、不静默放行（见 `main.rs` 的 `-p`/headless 调用
//! 点）。这个命令供用户在配置定时任务前，先手动交互批准一次，之后无人值守
//! 的调用就能正常连接。

use anyhow::Result;
use std::io::{self, Write};
use std::path::Path;
use wyj_store::project_trust::{self, TrustStatus};

pub async fn run(cwd: &Path) -> Result<()> {
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
