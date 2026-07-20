//! 定时任务：`~/.wyj-code/schedule/tasks.json`
//!
//! 与 `lockfile.rs` 的 `InstalledManifest` 体系分开建模——定时任务是用户内容
//! （一段 prompt + 一条 cron 规则 + 绑定的工作目录），不是"已安装扩展"，没有
//! Global/Project scope 之分：每个任务各自携带 `cwd` 字段区分归属，全局单文件
//! 存储（类比 `core::session_store::SessionFile.cwd` + 按字段过滤，而非物理分
//! 项目目录）。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SCHEDULE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRunRecord {
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// 标准 5 段 POSIX crontab 表达式（`分 时 日 月 周`），与系统 crontab 直接对应。
    pub cron: String,
    pub cwd: PathBuf,
    pub enabled: bool,
    #[serde(default)]
    pub notify_on_failure: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_run: Option<ScheduleRunRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleManifest {
    pub version: u32,
    pub tasks: Vec<ScheduleTask>,
}

impl ScheduleManifest {
    fn new() -> Self {
        Self {
            version: SCHEDULE_VERSION,
            tasks: Vec::new(),
        }
    }
}

pub fn schedule_dir() -> Result<PathBuf> {
    Ok(wyj_config::config_dir()?.join("schedule"))
}

pub fn schedule_file_path() -> Result<PathBuf> {
    Ok(schedule_dir()?.join("tasks.json"))
}

fn load_from(path: &Path) -> Result<ScheduleManifest> {
    if !path.exists() {
        return Ok(ScheduleManifest::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取定时任务文件失败: {}", path.display()))?;
    let mut manifest: ScheduleManifest = serde_json::from_str(&content)
        .with_context(|| format!("解析定时任务文件失败: {}", path.display()))?;
    manifest.version = SCHEDULE_VERSION;
    Ok(manifest)
}

fn save_to(path: &Path, manifest: &ScheduleManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建定时任务目录失败: {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(manifest).context("序列化定时任务失败")?;
    let nonce = format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let tmp = path.with_file_name(format!(
        "{}.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tasks.json"),
        nonce
    ));
    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("写入定时任务文件失败: {}", path.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("替换定时任务文件失败: {}", path.display()));
    }
    Ok(())
}

pub fn load() -> Result<ScheduleManifest> {
    load_from(&schedule_file_path()?)
}

pub fn save(manifest: &ScheduleManifest) -> Result<()> {
    save_to(&schedule_file_path()?, manifest)
}

pub struct NewTask {
    pub name: String,
    pub prompt: String,
    pub cron: String,
    pub cwd: PathBuf,
    pub notify_on_failure: bool,
}

/// 新建任务并落盘，返回生成的完整 `ScheduleTask`（含分配的 id）。
///
/// 实际读写发生在 `create_task_at`（`path` 可注入，供测试用临时文件），本函数
/// 只是绑定真实全局路径的薄封装；`update_task`/`delete_task` 等其余 CRUD 同理。
pub fn create_task(req: NewTask) -> Result<ScheduleTask> {
    create_task_at(&schedule_file_path()?, req)
}

fn create_task_at(path: &Path, req: NewTask) -> Result<ScheduleTask> {
    let mut manifest = load_from(path)?;
    let now = Utc::now();
    let task = ScheduleTask {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        prompt: req.prompt,
        cron: req.cron,
        cwd: req.cwd,
        enabled: true,
        notify_on_failure: req.notify_on_failure,
        created_at: now,
        updated_at: now,
        last_run: None,
    };
    manifest.tasks.push(task.clone());
    save_to(path, &manifest)?;
    Ok(task)
}

/// 按 id 就地修改任务（`mutator` 里改字段即可，`updated_at` 由本函数统一刷新）。
pub fn update_task(id: &str, mutator: impl FnOnce(&mut ScheduleTask)) -> Result<ScheduleTask> {
    update_task_at(&schedule_file_path()?, id, mutator)
}

fn update_task_at(
    path: &Path,
    id: &str,
    mutator: impl FnOnce(&mut ScheduleTask),
) -> Result<ScheduleTask> {
    let mut manifest = load_from(path)?;
    let task = manifest
        .tasks
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or_else(|| anyhow::anyhow!("未找到定时任务: {id}"))?;
    mutator(task);
    task.updated_at = Utc::now();
    let updated = task.clone();
    save_to(path, &manifest)?;
    Ok(updated)
}

pub fn delete_task(id: &str) -> Result<()> {
    delete_task_at(&schedule_file_path()?, id)
}

fn delete_task_at(path: &Path, id: &str) -> Result<()> {
    let mut manifest = load_from(path)?;
    let before = manifest.tasks.len();
    manifest.tasks.retain(|t| t.id != id);
    if manifest.tasks.len() == before {
        anyhow::bail!("未找到定时任务: {id}");
    }
    save_to(path, &manifest)
}

pub fn set_enabled(id: &str, enabled: bool) -> Result<ScheduleTask> {
    update_task(id, |t| t.enabled = enabled)
}

pub fn record_run_start(id: &str) -> Result<()> {
    update_task(id, |t| {
        t.last_run = Some(ScheduleRunRecord {
            started_at: Utc::now(),
            finished_at: None,
            status: RunStatus::Running,
            session_id: None,
            error: None,
        });
    })
    .map(|_| ())
}

pub fn record_run_result(
    id: &str,
    status: RunStatus,
    session_id: Option<String>,
    error: Option<String>,
) -> Result<()> {
    update_task(id, |t| {
        let started_at = t
            .last_run
            .as_ref()
            .map(|r| r.started_at)
            .unwrap_or_else(Utc::now);
        t.last_run = Some(ScheduleRunRecord {
            started_at,
            finished_at: Some(Utc::now()),
            status,
            session_id,
            error,
        });
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task_req(cwd: &Path) -> NewTask {
        NewTask {
            name: "测试任务".to_string(),
            prompt: "抓取今日 AI 新闻并总结".to_string(),
            cron: "0 8 * * *".to_string(),
            cwd: cwd.to_path_buf(),
            notify_on_failure: false,
        }
    }

    #[test]
    fn missing_file_returns_default_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = load_from(&dir.path().join("tasks.json")).unwrap();
        assert_eq!(manifest.version, SCHEDULE_VERSION);
        assert!(manifest.tasks.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut manifest = ScheduleManifest::new();
        manifest.tasks.push(ScheduleTask {
            id: "abc".to_string(),
            name: "复盘".to_string(),
            prompt: "做A股复盘".to_string(),
            cron: "0 20 * * *".to_string(),
            cwd: PathBuf::from("/tmp/trading"),
            enabled: true,
            notify_on_failure: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_run: None,
        });
        save_to(&path, &manifest).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks[0].id, "abc");
        assert_eq!(loaded.tasks[0].cron, "0 20 * * *");
    }

    #[test]
    fn old_file_without_new_fields_parses_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(&path, r#"{"version":1,"tasks":[]}"#).unwrap();
        let manifest = load_from(&path).unwrap();
        assert!(manifest.tasks.is_empty());
    }

    #[test]
    fn create_generates_unique_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let t1 = create_task_at(&path, new_task_req(dir.path())).unwrap();
        let t2 = create_task_at(&path, new_task_req(dir.path())).unwrap();
        assert_ne!(t1.id, t2.id);
        let manifest = load_from(&path).unwrap();
        assert_eq!(manifest.tasks.len(), 2);
    }

    #[test]
    fn update_delete_and_run_record_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let task = create_task_at(&path, new_task_req(dir.path())).unwrap();

        let updated = update_task_at(&path, &task.id, |t| t.name = "改名后".to_string()).unwrap();
        assert_eq!(updated.name, "改名后");

        update_task_at(&path, &task.id, |t| {
            t.last_run = Some(ScheduleRunRecord {
                started_at: Utc::now(),
                finished_at: None,
                status: RunStatus::Running,
                session_id: None,
                error: None,
            });
        })
        .unwrap();
        let manifest = load_from(&path).unwrap();
        let run = manifest.tasks[0].last_run.as_ref().unwrap();
        assert_eq!(run.status, RunStatus::Running);

        update_task_at(&path, &task.id, |t| {
            t.last_run = Some(ScheduleRunRecord {
                started_at: t.last_run.as_ref().unwrap().started_at,
                finished_at: Some(Utc::now()),
                status: RunStatus::Failed,
                session_id: None,
                error: Some("boom".to_string()),
            });
        })
        .unwrap();
        let manifest = load_from(&path).unwrap();
        let run = manifest.tasks[0].last_run.as_ref().unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_deref(), Some("boom"));
        assert!(run.finished_at.is_some());

        delete_task_at(&path, &task.id).unwrap();
        let manifest = load_from(&path).unwrap();
        assert!(manifest.tasks.is_empty());
    }

    #[test]
    fn delete_missing_task_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        assert!(delete_task_at(&path, "no-such-id").is_err());
    }
}
