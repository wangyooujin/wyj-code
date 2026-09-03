//! `wyj-code schedule ...`：定时任务 CRUD + 真正被系统 crontab 调用的 `run <id>`。
//!
//! `run` 不重用 `main()` 里已经和 TUI 模式深度交织的 Agent/Provider 装配逻辑，
//! 而是以子进程方式调用 wyj-code 自身的 `-p "<prompt>" --cwd <dir>` 入口——
//! 与用户自己手动 crontab 一条 `wyj-code -p "..."` 完全等价，不改动任何现有
//! headless 执行路径。

use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use wyj_store::cron_sync;
use wyj_store::schedule::{self, RunStatus, SchedulePermissions};

/// 读取当前用户 storage 配置(失败回退 default,避免阻塞 schedule 执行)。
/// schedule run 由系统 crontab 触发,headless 环境下不应因 config 解析失败
/// 而拒绝跑任务。
fn storage_cfg() -> wyj_config::StorageRetentionCfg {
    wyj_config::Config::load()
        .map(|cfg| cfg.storage)
        .unwrap_or_default()
}

#[derive(Subcommand, Debug)]
pub enum ScheduleCommand {
    /// List all schedule tasks.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a new schedule task and sync it into the system crontab.
    Add {
        /// Human readable task name.
        name: String,
        /// Natural-language prompt executed on trigger.
        #[arg(long)]
        prompt: String,
        /// Standard 5-field cron expression ("min hour dom month dow").
        #[arg(long)]
        cron: String,
        /// Working directory the task runs in; defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Send a macOS notification when this task fails.
        #[arg(long)]
        notify_on_failure: bool,
        /// Explicit tool allowlist. Defaults to read-only tools.
        #[arg(long, value_delimiter = ',')]
        allowed_tools: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove a schedule task and drop it from the system crontab.
    Remove {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Enable a schedule task (re-adds it to the system crontab).
    Enable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Disable a schedule task (removes it from the system crontab, keeps the record).
    Disable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Review and replace a task's explicit permissions; optionally enable it.
    Review {
        id: String,
        #[arg(long, value_delimiter = ',')]
        allowed_tools: Vec<String>,
        #[arg(long)]
        enable: bool,
        #[arg(long)]
        json: bool,
    },
    /// Re-generate the managed crontab block from the current task list.
    Sync {
        #[arg(long)]
        json: bool,
    },
    /// Execute one task immediately. This is the command the system crontab
    /// actually invokes; not meant for interactive use.
    Run {
        id: String,
        /// Retained for CLI compatibility with v1.4 prereleases. Foreground
        /// computer-use is now fail-closed in every headless/scheduled run,
        /// while background app_computer may safely run alongside the user.
        #[arg(long)]
        manual: bool,
    },
}

pub async fn run(command: ScheduleCommand, cwd: &Path) -> Result<()> {
    match command {
        ScheduleCommand::List { json } => {
            let manifest = schedule::load()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&manifest.tasks)?);
            } else if manifest.tasks.is_empty() {
                println!("No schedule tasks.");
            } else {
                for task in &manifest.tasks {
                    let status = task
                        .last_run
                        .as_ref()
                        .map(|r| format!("{:?}", r.status))
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "{:<36} {:<20} {:<14} {:<8} {}",
                        task.id,
                        task.name,
                        task.cron,
                        if task.needs_permission_review {
                            "review"
                        } else if task.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        status
                    );
                }
            }
        }
        ScheduleCommand::Add {
            name,
            prompt,
            cron,
            cwd: task_cwd,
            notify_on_failure,
            allowed_tools,
            json,
        } => {
            cron_sync::validate_cron(&cron)?;
            let permissions = schedule_permissions(allowed_tools);
            let task = schedule::create_task(schedule::NewTask {
                name,
                prompt,
                cron,
                cwd: task_cwd.unwrap_or_else(|| cwd.to_path_buf()),
                notify_on_failure,
                permissions,
            })?;
            sync_and_warn()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("created {}", task.id);
            }
        }
        ScheduleCommand::Remove { id, json } => {
            schedule::delete_task(&id)?;
            sync_and_warn()?;
            emit_mutation("removed", &id, json)?;
        }
        ScheduleCommand::Enable { id, json } => {
            schedule::set_enabled(&id, true)?;
            sync_and_warn()?;
            emit_mutation("enabled", &id, json)?;
        }
        ScheduleCommand::Disable { id, json } => {
            schedule::set_enabled(&id, false)?;
            sync_and_warn()?;
            emit_mutation("disabled", &id, json)?;
        }
        ScheduleCommand::Review {
            id,
            allowed_tools,
            enable,
            json,
        } => {
            let permissions = schedule_permissions(allowed_tools);
            let task = schedule::review_permissions(&id, permissions, enable)?;
            sync_and_warn()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!(
                    "reviewed {} ({})",
                    id,
                    if task.enabled { "enabled" } else { "disabled" }
                );
            }
        }
        ScheduleCommand::Sync { json } => {
            let manifest = schedule::load()?;
            cron_sync::sync_crontab(&manifest.tasks, &storage_cfg())?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"action":"sync","tasks":manifest.tasks.len()})
                );
            } else {
                println!(
                    "synced {} task(s) into system crontab",
                    manifest.tasks.len()
                );
            }
        }
        ScheduleCommand::Run { id, manual } => run_task(&id, manual).await?,
    }
    Ok(())
}

fn emit_mutation(action: &str, id: &str, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({"action": action, "id": id}));
    } else {
        println!("{action} {id}");
    }
    Ok(())
}

fn schedule_permissions(allowed_tools: Vec<String>) -> SchedulePermissions {
    let mut permissions = SchedulePermissions::default();
    if !allowed_tools.is_empty() {
        permissions.allowed_tools = allowed_tools;
    }
    permissions
}

/// crontab 同步失败不影响任务数据本身已经落盘成功；只打印警告，不让整条 CLI
/// 命令因为系统 crontab 不可用（如 Windows）而失败退出。
fn sync_and_warn() -> Result<()> {
    let manifest = schedule::load()?;
    let cfg = storage_cfg();
    if let Err(e) = cron_sync::sync_crontab(&manifest.tasks, &cfg) {
        eprintln!("警告：任务已保存，但同步系统 crontab 失败：{e}");
    }
    Ok(())
}

async fn run_task(id: &str, _manual: bool) -> Result<()> {
    let manifest = schedule::load()?;
    let task = manifest
        .tasks
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| anyhow::anyhow!("未找到定时任务: {id}"))?;
    if !task.enabled {
        eprintln!("定时任务 {id} 已禁用，跳过执行");
        return Ok(());
    }
    if task.needs_permission_review {
        anyhow::bail!("定时任务 {id} 尚未完成权限审查，拒绝运行");
    }
    let task_name = task.name.clone();
    let prompt = task.prompt.clone();
    let task_cwd = task.cwd.clone();
    let notify_on_failure = task.notify_on_failure;
    let permissions = task.permissions.clone();

    schedule::record_run_start(id)?;

    let cfg = storage_cfg();
    let log_path = match prepare_log_file(id, &cfg) {
        Ok(path) => path,
        Err(e) => {
            let msg = format!("无法创建日志文件: {e}");
            schedule::record_run_result(id, RunStatus::Failed, None, Some(msg.clone()))?;
            if notify_on_failure {
                notify_failure(&task_name, &msg);
            }
            return Err(e);
        }
    };

    let exe = std::env::current_exe().context("无法定位 wyj-code 可执行文件路径")?;
    let spawn_result = spawn_headless(&exe, &prompt, &task_cwd, &log_path, &permissions).await;

    let session_id = latest_session_id_for(&task_cwd);

    match spawn_result {
        Ok(status) if status.success() => {
            schedule::record_run_result(id, RunStatus::Success, session_id, None)?;
        }
        Ok(status) => {
            let tail = read_log_tail(&log_path, 4000);
            let err_msg = format!(
                "退出码 {:?}；日志: {}\n{}",
                status.code(),
                log_path.display(),
                tail
            );
            schedule::record_run_result(id, RunStatus::Failed, session_id, Some(err_msg.clone()))?;
            if notify_on_failure {
                notify_failure(&task_name, &err_msg);
            }
        }
        Err(e) => {
            let err_msg = format!("启动子进程失败: {e}");
            schedule::record_run_result(id, RunStatus::Failed, session_id, Some(err_msg.clone()))?;
            if notify_on_failure {
                notify_failure(&task_name, &err_msg);
            }
            return Err(e);
        }
    }
    Ok(())
}

fn prepare_log_file(id: &str, cfg: &wyj_config::StorageRetentionCfg) -> Result<PathBuf> {
    let log_dir = schedule::schedule_dir()?.join("logs").join(id);
    std::fs::create_dir_all(&log_dir).context("创建定时任务日志目录失败")?;
    prune_old_logs(&log_dir, cfg.schedule_logs_per_task)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(log_dir.join(format!("{ts}.log")))
}

/// 按 mtime 升序,保留最近 `keep` 个文件,删除其余。
/// `keep == 0` 时跳过(保留全部)。失败仅 `tracing::warn`,不污染主流程。
fn prune_old_logs(dir: &std::path::Path, keep: usize) -> Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("读取日志目录失败 {}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    if entries.len() <= keep {
        return Ok(());
    }
    entries.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    let excess = entries.len() - keep;
    for old in &entries[..excess] {
        if let Err(error) = std::fs::remove_file(old.path()) {
            tracing::warn!("删除定时任务旧日志失败 {}: {error}", old.path().display());
        }
    }
    Ok(())
}

async fn spawn_headless(
    exe: &Path,
    prompt: &str,
    task_cwd: &Path,
    log_path: &Path,
    permissions: &SchedulePermissions,
) -> Result<std::process::ExitStatus> {
    let stdout_file = std::fs::File::create(log_path).context("创建日志文件失败")?;
    let stderr_file = stdout_file.try_clone().context("克隆日志文件句柄失败")?;
    let mut command = Command::new(exe);
    command
        .arg("-p")
        .arg(prompt)
        .arg("--cwd")
        .arg(task_cwd)
        .arg("--allowed-tools")
        .arg(permissions.allowed_tools.join(","));
    let status = command
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .status()
        .await
        .context("等待 wyj-code -p 子进程失败")?;
    Ok(status)
}

fn latest_session_id_for(task_cwd: &Path) -> Option<String> {
    let dir = wyj_config::config_dir().ok()?.join("sessions");
    let store = wyj_core::SessionStore::new(dir).ok()?;
    store
        .last_for_project(task_cwd)
        .ok()
        .flatten()
        .map(|m| m.session_id)
}

fn read_log_tail(path: &Path, max_bytes: usize) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(max_bytes);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(_) => String::new(),
    }
}

#[cfg(target_os = "macos")]
fn notify_failure(task_name: &str, message: &str) {
    let short: String = message
        .chars()
        .take(200)
        .collect::<String>()
        .replace('\n', " ");
    let script = format!(
        "display notification {} with title {}",
        applescript_quote(&short),
        applescript_quote(&format!("wyj-code 定时任务失败: {task_name}"))
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status();
}

#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(not(target_os = "macos"))]
fn notify_failure(_task_name: &str, _message: &str) {
    tracing::warn!("当前平台不支持系统通知，定时任务失败信息已记录到 last_run");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_log_tail_truncates_from_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        std::fs::write(&path, "a".repeat(100)).unwrap();
        let tail = read_log_tail(&path, 10);
        assert_eq!(tail, "a".repeat(10));
    }

    #[test]
    fn read_log_tail_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tail = read_log_tail(&dir.path().join("missing.log"), 10);
        assert_eq!(tail, "");
    }
}
