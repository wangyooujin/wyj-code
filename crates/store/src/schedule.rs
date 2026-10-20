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
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const SCHEDULE_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulePermissions {
    pub allowed_tools: Vec<String>,
}

impl Default for SchedulePermissions {
    fn default() -> Self {
        Self {
            allowed_tools: ["Read", "Glob", "Grep", "WebFetch", "WebSearch", "TodoWrite"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

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
    /// v1 任务升级后必须人工确认权限；未确认的任务会自动禁用且拒绝运行。
    #[serde(default)]
    pub needs_permission_review: bool,
    #[serde(default)]
    pub permissions: SchedulePermissions,
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
    /// 本进程加载时读到的**原始文件文本**，供 `save_to` 做三方 diff 合并。
    ///
    /// 与 `lockfile::InstalledManifest::baseline_json` 同理：删除是硬删
    /// （`retain`），无脑 union 会把别人刚删的任务复活——而 crontab 里那行还在，
    /// 于是定时任务变成「文件里没有、crontab 里照跑」的幽灵。
    #[serde(skip)]
    pub baseline_json: Option<String>,
}

impl ScheduleManifest {
    fn new() -> Self {
        Self {
            version: SCHEDULE_VERSION,
            tasks: Vec::new(),
            baseline_json: None,
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
        let mut manifest = ScheduleManifest::new();
        // 空基线**必须**记下来：文件不存在是最常见的情形（首次建任务），
        // 此时若返回 `baseline_json: None`，`save_to` 会退化成整份覆盖，
        // 于是两个实例各自「首次新建」时后一个照样会抹掉前一个——恰好把最需要
        // 保护的场景排除在保护之外。
        manifest.baseline_json = serde_json::to_string(&ScheduleManifest::new()).ok();
        return Ok(manifest);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取定时任务文件失败: {}", path.display()))?;
    let mut manifest: ScheduleManifest = serde_json::from_str(&content)
        .with_context(|| format!("解析定时任务文件失败: {}", path.display()))?;
    let migrated = manifest.version < SCHEDULE_VERSION;
    if migrated {
        for task in &mut manifest.tasks {
            task.enabled = false;
            task.needs_permission_review = true;
            task.permissions = SchedulePermissions::default();
        }
    }
    manifest.version = SCHEDULE_VERSION;
    manifest.baseline_json = Some(content);
    if migrated {
        save_to(path, &manifest)?;
    }
    Ok(manifest)
}

fn save_to(path: &Path, manifest: &ScheduleManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建定时任务目录失败: {}", parent.display()))?;
    }
    // 跨进程排他锁：新建/删除定时任务是 load → 改 → save 的 read-modify-write。
    // 无锁时两个实例各自从同一份旧快照出发，后保存者会把对方刚建的任务整个抹掉——
    // 表现是「刚加的定时任务重启后自己没了」，而 crontab 已经被写进去了。
    let _guard = wyj_config::filelock::acquire(&path.with_extension("json.lock"), "tasks.json")?;
    let merged = merge_with_disk(path, manifest)?;
    let content = serde_json::to_string_pretty(&merged).context("序列化定时任务失败")?;
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

/// 把本进程的改动三方合并到锁内重读的磁盘值上，规则同 `lockfile::merge_entries`：
///
/// - base 有、mine 无 → 本进程删掉了它 → 结果里不保留（**不复活**别人刚删的任务）
/// - base 无、mine 有 → 本进程新加的 → 追加
/// - base 有、mine 有 → 以 mine 为准
/// - 三者都没有  → 保留磁盘值（本进程没碰，别人的改动自动并入）
///
/// 无基线（内存构造）时退化为整份覆盖。
fn merge_with_disk(path: &Path, mine: &ScheduleManifest) -> Result<ScheduleManifest> {
    let base = mine
        .baseline_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ScheduleManifest>(raw).ok());
    // 刻意**不**复用 `load_from`：它在检测到旧版本时会回调 `save_to`，而 `save_to`
    // 又会回到这里——迁移路径上直接变成无限递归。这里只关心磁盘上的条目内容。
    let disk = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<ScheduleManifest>(&raw).ok());
    let (Some(base), Some(disk)) = (base, disk) else {
        return Ok(mine.clone());
    };
    let mut merged = disk.clone();
    merged.version = mine.version;
    merged.baseline_json = None;

    let base_ids: HashSet<&str> = base.tasks.iter().map(|t| t.id.as_str()).collect();
    let mine_ids: HashSet<&str> = mine.tasks.iter().map(|t| t.id.as_str()).collect();
    merged.tasks = disk
        .tasks
        .iter()
        .filter(|task| {
            // 保留条件 = 「基线里没有它」或「本进程仍然要它」
            !base_ids.contains(task.id.as_str()) || mine_ids.contains(task.id.as_str())
        })
        .cloned()
        .collect();
    for task in &mine.tasks {
        match merged
            .tasks
            .iter_mut()
            .find(|existing| existing.id == task.id)
        {
            Some(existing) => *existing = task.clone(),
            // 磁盘上没有且本进程基线里有 → 别人刚删了它，尊重删除。
            None if base_ids.contains(task.id.as_str()) => {}
            None => merged.tasks.push(task.clone()),
        }
    }
    merged.tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(merged)
}

pub struct NewTask {
    pub name: String,
    pub prompt: String,
    pub cron: String,
    pub cwd: PathBuf,
    pub notify_on_failure: bool,
    pub permissions: SchedulePermissions,
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
        needs_permission_review: false,
        permissions: req.permissions,
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
    if enabled {
        let manifest = load()?;
        let task = manifest
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or_else(|| anyhow::anyhow!("未找到定时任务: {id}"))?;
        if task.needs_permission_review {
            anyhow::bail!("定时任务 {id} 尚未完成权限审查，不能启用");
        }
    }
    update_task(id, |t| t.enabled = enabled)
}

/// 复核任务权限。
///
/// `enable` 为 `None` 表示**保持任务当前的启用状态不变**。此前签名是
/// `enable: bool` 且无条件 `task.enabled = enable`——而 CLI 的 `--enable`
/// 是 `SetTrue` 布尔（缺省 false），于是「只改权限、不碰启用状态」这个
/// 最自然的操作反而把任务**禁用**了。现在用 `Option<bool>` 区分三态：
/// `Some(true)` 启用、`Some(false)` 禁用、`None` 不动。
pub fn review_permissions(
    id: &str,
    permissions: SchedulePermissions,
    enable: Option<bool>,
) -> Result<ScheduleTask> {
    update_task(id, |task| {
        task.permissions = permissions;
        task.needs_permission_review = false;
        if let Some(enabled) = enable {
            task.enabled = enabled;
        }
    })
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
            permissions: SchedulePermissions::default(),
        }
    }

    #[test]
    fn missing_file_returns_default_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = load_from(&dir.path().join("tasks.json")).unwrap();
        assert_eq!(manifest.version, SCHEDULE_VERSION);
        assert!(manifest.tasks.is_empty());
    }

    /// 两个实例各自从同一份空快照出发、各建一个任务：两个都必须留下。
    /// 旧行为是整份覆盖，后保存者把对方刚建的任务抹掉，而 crontab 里那行还在。
    #[test]
    fn concurrent_creates_keep_both_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");

        // 两个实例各自 load（拿到**同一份**空基线），各建一个任务。
        let mut a = load_from(&path).unwrap();
        let mut b = load_from(&path).unwrap();
        let make = |id: &str| ScheduleTask {
            id: id.to_string(),
            name: id.to_string(),
            prompt: "p".to_string(),
            cron: "0 8 * * *".to_string(),
            cwd: dir.path().to_path_buf(),
            enabled: true,
            needs_permission_review: false,
            permissions: SchedulePermissions::default(),
            notify_on_failure: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_run: None,
        };
        a.tasks.push(make("task-a"));
        b.tasks.push(make("task-b"));

        save_to(&path, &b).unwrap();
        save_to(&path, &a).unwrap();

        let final_manifest = load_from(&path).unwrap();
        let ids: Vec<&str> = final_manifest.tasks.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids.contains(&"task-a") && ids.contains(&"task-b"),
            "并发新建的任务必须都保留，实际: {ids:?}"
        );
    }

    /// 关键性质：一个实例删除的任务**不能**被另一个实例的陈旧快照复活。
    /// 如果这里退化成 union，被删的任务会重新出现在 tasks.json 里，而 crontab 里
    /// 对应那行已经被摘掉——于是它变成既不可见又不可管的幽灵。
    #[test]
    fn a_stale_snapshot_never_resurrects_a_deleted_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut seed = load_from(&path).unwrap();
        seed.tasks.push(ScheduleTask {
            id: "doomed".to_string(),
            name: "d".to_string(),
            prompt: "p".to_string(),
            cron: "0 8 * * *".to_string(),
            cwd: dir.path().to_path_buf(),
            enabled: true,
            needs_permission_review: false,
            permissions: SchedulePermissions::default(),
            notify_on_failure: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_run: None,
        });
        save_to(&path, &seed).unwrap();

        // A、B 都看到含 doomed 的状态；A 删掉它，B 什么都不知道。
        let mut a = load_from(&path).unwrap();
        let b = load_from(&path).unwrap();
        a.tasks.retain(|t| t.id != "doomed");

        save_to(&path, &a).unwrap();
        save_to(&path, &b).unwrap();

        let final_manifest = load_from(&path).unwrap();
        assert!(
            !final_manifest.tasks.iter().any(|t| t.id == "doomed"),
            "被删除的任务不能因为另一个进程的陈旧快照而复活"
        );
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut manifest = ScheduleManifest::new();
        let permissions = SchedulePermissions {
            allowed_tools: vec!["Read".to_string(), "Bash".to_string()],
        };
        manifest.tasks.push(ScheduleTask {
            id: "abc".to_string(),
            name: "复盘".to_string(),
            prompt: "做A股复盘".to_string(),
            cron: "0 20 * * *".to_string(),
            cwd: PathBuf::from("/tmp/trading"),
            enabled: true,
            needs_permission_review: false,
            permissions: permissions.clone(),
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
        assert_eq!(loaded.tasks[0].permissions, permissions);
    }

    #[test]
    fn old_file_without_new_fields_parses_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"tasks":[{{"id":"old","name":"old","prompt":"x","cron":"0 1 * * *","cwd":"{}","enabled":true,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}]}}"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let manifest = load_from(&path).unwrap();
        assert_eq!(manifest.tasks.len(), 1);
        assert!(!manifest.tasks[0].enabled);
        assert!(manifest.tasks[0].needs_permission_review);
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
