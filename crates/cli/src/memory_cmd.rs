use anyhow::Result;
use clap::Subcommand;
use std::path::Path;
use wyj_core::{MemoryV3Store, MemoryWriteRequest};

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    /// 查看当前项目实际生效的 Memory v3 状态、作用域和耐久队列。
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 使用与 Agent 相同的中文 n-gram、实体和 BM25 风格索引检索。
    Search {
        query: String,
        #[arg(long)]
        recent_context: Option<String>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 在历史会话的原始 transcript 里检索：claim 层提取漏掉时的回溯入口。
    ///
    /// 与 `search` 互补——`search` 查模型提取出的 claim，本命令查原始对话原文
    /// （含当时的工具调用参数与报错）。默认只看当前项目（git 仓库根）的会话。
    SearchSessions {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// 放开「只看当前项目」的隔离，检索全部本地会话。
        #[arg(long)]
        all_projects: bool,
        #[arg(long)]
        json: bool,
    },
    /// 写入一条完整 MemoryWriteRequest JSON；用于迁移、审计和自动化。
    Write {
        #[arg(long)]
        record: String,
    },
    /// 撤销错误记忆；保留历史，并恢复它 supersede 的上一条状态。
    Forget {
        id: String,
        #[arg(long, default_value = "CLI undo")]
        reason: String,
    },
    /// 清空整个 Memory v3 库（Global + Project claims、jobs、audit），旧数据
    /// 移到 backups/<ts>/，`rejected_history.json` 保留。破坏性批操作，
    /// 必须显式 `--yes` 才执行；不带 `--yes` 直接 hard-bail。
    ClearAll {
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(command: MemoryCommand, cwd: &Path, enabled: bool) -> Result<()> {
    let base = wyj_config::config_dir()?;
    let store = MemoryV3Store::new(&base, cwd)?;
    store.set_enabled(enabled);
    match command {
        MemoryCommand::Status { json } => {
            let status = store.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Memory v3: {}", if status.enabled { "on" } else { "off" });
                println!("project: {}", status.project_root);
                println!("project_key: {}", status.project_key);
                println!(
                    "records: {} active, {} superseded, {} expired",
                    status.active_records, status.superseded_records, status.expired_records
                );
                println!(
                    "jobs: {} pending, {} failed",
                    status.pending_jobs, status.failed_jobs
                );
            }
        }
        MemoryCommand::Search {
            query,
            recent_context,
            limit,
            json,
        } => {
            let hits = store.search(&query, recent_context.as_deref(), Some(limit))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("No matching memory claims.");
            } else {
                for hit in hits {
                    println!(
                        "{:.2}  {}  {:?}/{:?}  {}\n  {}\n  source={:?}: {} observed={}",
                        hit.score,
                        hit.record.id,
                        hit.record.scope,
                        hit.record.kind,
                        hit.record.title,
                        hit.record.content.replace('\n', " "),
                        hit.record.source.kind,
                        hit.record.source.locator,
                        hit.record
                            .source
                            .observed_at
                            .as_deref()
                            .unwrap_or("unknown")
                    );
                }
            }
        }
        MemoryCommand::SearchSessions {
            query,
            limit,
            all_projects,
            json,
        } => {
            // `search`（不带项目过滤）走 `list_all`，会扫遍所有项目目录 +
            // 根目录下的旧平铺文件；`search_for_project` 则只留在当前项目。
            // 两者都用 `for_project` 构造的 store，避免单目录模式看不到项目子目录。
            let sessions =
                wyj_core::SessionStore::for_project(wyj_config::layout::sessions_root(&base), cwd)?;
            let hits = if all_projects {
                sessions.search(&query, limit)?
            } else {
                sessions.search_for_project(cwd, &query, limit)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("No matching messages in past sessions.");
            } else {
                for hit in hits {
                    println!(
                        "{:.2}  [{}] {}  #{} ({})\n  {}",
                        hit.score,
                        hit.session_id,
                        hit.title,
                        hit.message_index,
                        hit.role,
                        hit.snippet.replace('\n', " ")
                    );
                }
            }
        }
        MemoryCommand::Write { record } => {
            let request: MemoryWriteRequest = serde_json::from_str(&record)?;
            println!("{}", serde_json::to_string_pretty(&store.upsert(request)?)?);
        }
        MemoryCommand::Forget { id, reason } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&store.forget(&id, &reason)?)?
            );
        }
        MemoryCommand::ClearAll { yes } => {
            if !yes {
                anyhow::bail!("clear-all 是破坏性批操作；请明确加 `--yes` 二次确认。");
            }
            let report = store.clear_all()?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            println!("\n已备份到 {}", report.backup_dir.display());
            if report.rejected_history_preserved {
                println!("rejected_history.json 已保留，background 提议仍尊重用户曾拒绝的指纹。");
            }
        }
    }
    Ok(())
}
