//! `wyj-code skillhub ...`：HTTP 技能注册表（SkillHub）的 headless 入口。
//!
//! `/skills` 面板 Browse tab 只有 TUI 能用，而 `-p`/`--headless`/cron 这类无 UI
//! 通道的场景需要等价能力。这里只做「搜索 / 预览 / 安装」三件事，语义与面板
//! 完全一致：**安装前必须看到来源、作者、版本与包内容摘要并显式确认**，
//! 只有显式传 `--yes` 才跳过（供脚本化场景使用）。
//!
//! 刻意不走 shell out 调 `skillhub` CLI：那样会引入一个 Python 运行时依赖，
//! 且解析它的人类可读输出不如直接用 `wyj_store::skillhub` 稳定。

use anyhow::{bail, Result};
use clap::Subcommand;
use std::io::{IsTerminal, Write};
use wyj_store::skillhub::{self, SkillHubClient, SkillHubInstallRequest};

#[derive(Subcommand, Debug)]
pub enum SkillHubCommand {
    /// 按关键字搜索技能注册表。
    Search {
        /// 搜索关键字。
        query: String,
        /// 最多返回多少条（注册表不支持翻页，一次取回）。
        #[arg(long, default_value_t = 30)]
        limit: u32,
        /// 以 JSON 输出，便于脚本消费。
        #[arg(long)]
        json: bool,
    },
    /// 下载技能包并打印内容摘要（不安装）。
    Info {
        /// 技能 slug。
        slug: String,
        /// 发布者 namespace handle，例如 `clawhub_pskoett`。
        #[arg(long)]
        namespace: String,
    },
    /// 安装技能到 skills 目录。
    Install {
        /// 技能 slug。
        slug: String,
        /// 发布者 namespace handle，例如 `clawhub_pskoett`。
        #[arg(long)]
        namespace: String,
        /// 安装到项目级 `.wyj-code/skills/`（默认安装到全局 `~/.wyj-code/skills/`）。
        #[arg(long)]
        project: bool,
        /// 跳过交互确认（非交互环境必须显式传入，否则无法确认来源）。
        #[arg(long)]
        yes: bool,
    },
}

pub async fn run(command: SkillHubCommand) -> Result<()> {
    let client = SkillHubClient::official();
    match command {
        SkillHubCommand::Search { query, limit, json } => {
            search(&client, &query, limit, json).await
        }
        SkillHubCommand::Info { slug, namespace } => info(&client, &slug, &namespace).await,
        SkillHubCommand::Install {
            slug,
            namespace,
            project,
            yes,
        } => install(&client, &slug, &namespace, project, yes).await,
    }
}

async fn search(client: &SkillHubClient, query: &str, limit: u32, json: bool) -> Result<()> {
    let results = client.search(query, limit).await?;
    if json {
        let payload: Vec<serde_json::Value> = results
            .iter()
            .map(|s| {
                serde_json::json!({
                    "slug": s.slug,
                    "display_name": s.display_name,
                    "description": s.description,
                    "version": s.version,
                    "source": s.source,
                    "namespace": s.namespace,
                    "canonical_name": s.canonical_name,
                    "owner_name": s.owner_name,
                    "stars": s.stars,
                    "installs": s.installs,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if results.is_empty() {
        println!("未找到匹配的技能");
        return Ok(());
    }
    for s in &results {
        println!(
            "{}  v{}  ★{}  {}",
            s.canonical_name, s.version, s.stars, s.display_name
        );
        if !s.description.is_empty() {
            println!("    {}", s.description);
        }
    }
    println!(
        "\n安装: wyj-code skillhub install {} --namespace {}",
        results[0].slug, results[0].namespace
    );
    Ok(())
}

async fn info(client: &SkillHubClient, slug: &str, namespace: &str) -> Result<()> {
    let archive = client.download(slug, namespace).await?;
    let preview = skillhub::preview_archive(&archive)?;
    println!(
        "{slug} · {} 个文件 · {}",
        preview.file_count,
        human_bytes(preview.total_bytes)
    );
    if preview.has_executables() {
        println!("⚠ 包含 {} 个可执行脚本：", preview.executable_count);
        for path in &preview.executable_paths {
            println!("    {path}");
        }
    }
    Ok(())
}

async fn install(
    client: &SkillHubClient,
    slug: &str,
    namespace: &str,
    project: bool,
    yes: bool,
) -> Result<()> {
    let scope = if project {
        wyj_store::InstallScope::Project
    } else {
        wyj_store::InstallScope::Global
    };
    let cwd = std::env::current_dir()?;

    // 先下载并扫描，把"到底会往磁盘里写什么"摆到用户面前再问要不要装。
    let archive = client.download(slug, namespace).await?;
    let preview = skillhub::preview_archive(&archive)?;

    println!("即将安装技能: {slug}");
    println!("  来源:    {namespace}");
    println!(
        "  包内容:  {} 个文件 · {}",
        preview.file_count,
        human_bytes(preview.total_bytes)
    );
    if preview.has_executables() {
        println!(
            "  ⚠ 含 {} 个可执行脚本，Agent 可能通过 Bash 调用：",
            preview.executable_count
        );
        for path in &preview.executable_paths {
            println!("      {path}");
        }
    }
    println!(
        "  安装到:  {}",
        if project {
            "项目 .wyj-code/skills"
        } else {
            "全局 ~/.wyj-code/skills"
        }
    );

    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("非交互环境必须显式传 --yes 以确认安装第三方技能");
        }
        print!("确认安装? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("已取消");
            return Ok(());
        }
    }

    let req = SkillHubInstallRequest {
        base_url: skillhub::SKILLHUB_BASE_URL.to_string(),
        slug: slug.to_string(),
        namespace: namespace.to_string(),
        install_name: String::new(),
        // 注册表搜索结果里的版本是权威展示值；命令行直接装时以 slug 为准，
        // 版本留空让安装流程按需从包内 _meta.json 补齐。
        version: String::new(),
        scope,
        source_label: format!("{namespace}/{slug}"),
    };
    skillhub::install_skillhub_archive(&archive, &req, &cwd)?;
    println!("已安装，将在下一轮 Agent 回合边界生效");
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}
