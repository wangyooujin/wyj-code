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
use wyj_store::schedule::{self, RunStatus};

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
                        if task.enabled { "enabled" } else { "disabled" },
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
            json,
        } => {
            cron_sync::validate_cron(&cron)?;
            let task = schedule::create_task(schedule::NewTask {
                name,
                prompt,
                cron,
                cwd: task_cwd.unwrap_or_else(|| cwd.to_path_buf()),
                notify_on_failure,
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
        ScheduleCommand::Sync { json } => {
            let manifest = schedule::load()?;
            cron_sync::sync_crontab(&manifest.tasks)?;
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

/// crontab 同步失败不影响任务数据本身已经落盘成功；只打印警告，不让整条 CLI
/// 命令因为系统 crontab 不可用（如 Windows）而失败退出。
fn sync_and_warn() -> Result<()> {
    let manifest = schedule::load()?;
    if let Err(e) = cron_sync::sync_crontab(&manifest.tasks) {
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
    let task_name = task.name.clone();
    let prompt = task.prompt.clone();
    let task_cwd = task.cwd.clone();
    let notify_on_failure = task.notify_on_failure;

    schedule::record_run_start(id)?;

    let log_path = match prepare_log_file(id) {
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
    let spawn_result = spawn_headless(&exe, &prompt, &task_cwd, &log_path).await;

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

fn prepare_log_file(id: &str) -> Result<PathBuf> {
    let log_dir = schedule::schedule_dir()?.join("logs").join(id);
    std::fs::create_dir_all(&log_dir).context("创建定时任务日志目录失败")?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(log_dir.join(format!("{ts}.log")))
}

async fn spawn_headless(
    exe: &Path,
    prompt: &str,
    task_cwd: &Path,
    log_path: &Path,
) -> Result<std::process::ExitStatus> {
    let stdout_file = std::fs::File::create(log_path).context("创建日志文件失败")?;
    let stderr_file = stdout_file.try_clone().context("克隆日志文件句柄失败")?;
    let status = Command::new(exe)
        .arg("-p")
        .arg(prompt)
        .arg("--cwd")
        .arg(task_cwd)
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
