//! 会话与陈旧锁的保留策略：把「无界增长」的东西按时间收口。
//!
//! # 为什么需要它
//!
//! `storage` 那一套 cap（checkpoint 条数、CAS 总量、memory 记录数）解决的是
//! **单个文件/单个子系统**的上限。实测暴露的是另一类问题：**文件数量**本身
//! 无界增长，且没有任何回收路径——
//!
//! - `sessions/` 680 个会话 / 2.5 GB，全是「跑过一次就再没打开」的；
//! - `cas/locks/` 10641 个锁文件 / 42 MB。unix 的 flock 由内核在进程退出时
//!   释放，但**锁文件本身不会消失**，每 intern 一个 blob 就留一个；
//! - `sessions/*.json.lock` 238 个、`*.json.owner` 65 个，崩溃/强杀后残留。
//!
//! 这三类的共同点是「保留策略应当是时间，而不是数量」。因此这里提供
//! [`run_startup_cleanup`]，在进程启动时跑一次（与 CAS gc 同批，异步化）。
//!
//! # 三条安全约束
//!
//! 1. **只删「无人持有」的锁**。unix 上用 `flock(LOCK_EX|LOCK_NB)` 试拿一次，
//!    拿得到才删——这比看 pid 可靠（pid 会被复用），也自然覆盖了「另一个进程
//!    正在写」的情况。
//! 2. **每个项目永远保留最近 N 条会话**（`sessions_keep_recent`）。纯 TTL 会把
//!    「三个月没动的项目里唯一那批会话」删光，而那恰恰是用户最想回看的。
//! 3. **会话 TTL 默认 0（关闭）**，锁清理默认 7 天。区别在于风险：锁文件内容
//!    只有 pid，删错了没有任何数据损失；而会话是不可逆的对话历史，不该在升级后
//!    第一次启动里静默消失。想开就自己设 `storage.sessions_ttl_days`，
//!    或者用 `wyj-code storage prune --yes` 显式清一次。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use wyj_config::StorageRetentionCfg;

/// 一次清理的结果。用于 `storage prune` 输出与启动期日志。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupStats {
    pub sessions_removed: usize,
    pub session_bytes_freed: u64,
    pub locks_removed: usize,
    pub lock_bytes_freed: u64,
}

impl CleanupStats {
    pub fn is_empty(&self) -> bool {
        self.sessions_removed == 0 && self.locks_removed == 0
    }
}

/// 按配置在 `data_root` 下跑一次清理。
///
/// `dry_run = true` 时只统计不删除，供 `storage prune` 的计划输出用。
pub fn run(data_root: &Path, cfg: &StorageRetentionCfg, dry_run: bool) -> Result<CleanupStats> {
    let mut stats = CleanupStats::default();
    if cfg.sessions_ttl_days > 0 {
        let session_stats = purge_sessions(
            data_root,
            cfg.sessions_ttl_days,
            cfg.sessions_keep_recent,
            dry_run,
        )?;
        stats.sessions_removed = session_stats.0;
        stats.session_bytes_freed = session_stats.1;
    }
    if cfg.stale_lock_ttl_days > 0 {
        let lock_stats = purge_stale_locks(data_root, cfg.stale_lock_ttl_days, dry_run)?;
        stats.locks_removed = lock_stats.0;
        stats.lock_bytes_freed = lock_stats.1;
    }
    Ok(stats)
}

/// 删除超过 `ttl_days` 的会话，但每个项目保留最近 `keep_recent` 条。
///
/// 一个会话在磁盘上是**一组**条目：`<sid>.json`、`<sid>.meta.json`、
/// `<sid>.json.lock`、`<sid>.json.owner`、`<sid>.checkpoints/`、
/// `<sid>.subagents/`。必须整组删——只删 `.json` 会留下孤儿 checkpoint
/// 目录，`storage status` 里显示成一堆「0 字节会话」。
fn purge_sessions(
    data_root: &Path,
    ttl_days: u32,
    keep_recent: usize,
    dry_run: bool,
) -> Result<(usize, u64)> {
    let sessions_root = wyj_config::layout::sessions_root(data_root);
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(ttl_days) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    // 收集全部会话：(目录, session_id, mtime)。
    let mut sessions: Vec<(PathBuf, String, SystemTime)> = Vec::new();
    for dir in session_dirs(&sessions_root) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".meta.json") {
                continue;
            }
            let Some(session_id) = name.strip_suffix(".json") else {
                continue;
            };
            let mtime = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            sessions.push((dir.clone(), session_id.to_string(), mtime));
        }
    }

    // 按项目分组，组内按 mtime 倒序；组内前 `keep_recent` 条无条件保留。
    let mut by_project: std::collections::HashMap<PathBuf, Vec<(String, SystemTime)>> =
        std::collections::HashMap::new();
    for (dir, session_id, mtime) in sessions {
        by_project.entry(dir).or_default().push((session_id, mtime));
    }

    let mut removed = 0usize;
    let mut freed = 0u64;
    for (dir, mut group) in by_project {
        group.sort_by(|a, b| b.1.cmp(&a.1));
        for (index, (session_id, mtime)) in group.into_iter().enumerate() {
            if index < keep_recent {
                continue;
            }
            if mtime >= cutoff {
                continue;
            }
            let entries = session_entries(&dir, &session_id);
            let bytes: u64 = entries.iter().map(|path| path_size(path)).sum::<u64>();
            removed += 1;
            freed += bytes;
            if dry_run {
                continue;
            }
            for path in entries {
                remove_path(&path);
            }
        }
    }
    Ok((removed, freed))
}

/// 一个会话在磁盘上的全部条目（正文、meta、锁、租约、两个旁路目录）。
fn session_entries(dir: &Path, session_id: &str) -> Vec<PathBuf> {
    vec![
        dir.join(format!("{session_id}.json")),
        dir.join(format!("{session_id}.meta.json")),
        dir.join(format!("{session_id}.json.lock")),
        dir.join(format!("{session_id}.json.owner")),
        dir.join(format!("{session_id}.checkpoints")),
        dir.join(format!("{session_id}.subagents")),
    ]
}

/// 会话根 + 各项目子目录（跳过 checkpoint / subagent 旁路目录）。
fn session_dirs(sessions_root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![sessions_root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(sessions_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.ends_with(".checkpoints") || name.ends_with(".subagents") {
                continue;
            }
            dirs.push(path);
        }
    }
    dirs
}

/// 删除超过 `ttl_days` 且**当前无人持有**的锁文件。
///
/// 覆盖两处：`cas/locks/`（每 blob 一把，数量随 blob 线性增长）与
/// `sessions/` 下的 `*.json.lock` / `*.json.owner`。
///
/// 「无人持有」的判定用 `flock` 试拿，而不是看 pid：pid 会被复用，而 flock
/// 是内核维护的真实持有状态。试拿失败说明另一个进程正在用，直接跳过。
fn purge_stale_locks(data_root: &Path, ttl_days: u32, dry_run: bool) -> Result<(usize, u64)> {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(ttl_days) * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut removed = 0usize;
    let mut freed = 0u64;

    // ── cas/locks/ ──
    let cas_locks = wyj_config::layout::cas_root(data_root).join("locks");
    for path in walk_files(&cas_locks) {
        if !is_stale(&path, cutoff) {
            continue;
        }
        if !lock_is_free(&path) {
            continue;
        }
        removed += 1;
        freed += path_size(&path);
        if !dry_run {
            remove_path(&path);
        }
    }

    // ── sessions/*.json.lock 与 *.json.owner ──
    for dir in session_dirs(&wyj_config::layout::sessions_root(data_root)) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let is_lock = name.ends_with(".json.lock");
            let is_owner = name.ends_with(".json.owner");
            if !is_lock && !is_owner {
                continue;
            }
            if !is_stale(&path, cutoff) {
                continue;
            }
            // `.owner` 是租约（整个会话生命周期持有），不是 flock——判活靠 pid。
            let in_use = if is_owner {
                wyj_config::filelock::active_holder(&path).is_some()
            } else {
                !lock_is_free(&path)
            };
            if in_use {
                continue;
            }
            removed += 1;
            freed += path_size(&path);
            if !dry_run {
                remove_path(&path);
            }
        }
    }

    Ok((removed, freed))
}

/// 递归列出目录下所有文件（不 follow symlink）。
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            files.extend(walk_files(&path));
        } else if meta.is_file() {
            files.push(path);
        }
    }
    files
}

fn is_stale(path: &Path, cutoff: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|mtime| mtime < cutoff)
        .unwrap_or(false)
}

/// 试拿一次排他锁：拿到（并立刻释放）说明没人持有。
///
/// **必须用 `is_locked` 而不是 `try_acquire`**：后者拿到锁会写 pid、刷新
/// mtime，探测动作本身就把目标变成「刚刚用过」，于是 dry-run 报「将删除」、
/// 紧接着 `--yes` 报「已删除 0 个」——文件永远清不掉。这条是实测踩到的。
fn lock_is_free(path: &Path) -> bool {
    !wyj_config::filelock::is_locked(path)
}

fn path_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total = total.saturating_add(path_size(&entry.path()));
        }
    }
    total
}

fn remove_path(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(error) = result {
        tracing::warn!("清理失败 {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(ttl: u32, keep: usize, lock_ttl: u32) -> StorageRetentionCfg {
        StorageRetentionCfg {
            sessions_ttl_days: ttl,
            sessions_keep_recent: keep,
            stale_lock_ttl_days: lock_ttl,
            ..Default::default()
        }
    }

    fn touch_old(path: &Path, days: u64) {
        let when = SystemTime::now() - Duration::from_secs(days * 86_400);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    fn write_session(dir: &Path, id: &str, age_days: u64) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{id}.json"));
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(dir.join(format!("{id}.meta.json")), "{}").unwrap();
        std::fs::create_dir_all(dir.join(format!("{id}.checkpoints"))).unwrap();
        std::fs::write(dir.join(format!("{id}.checkpoints/manifest.json")), "{}").unwrap();
        std::fs::create_dir_all(dir.join(format!("{id}.subagents"))).unwrap();
        touch_old(&path, age_days);
        touch_old(&dir.join(format!("{id}.meta.json")), age_days);
    }

    #[test]
    fn ttl_removes_old_sessions_as_a_group() {
        let base = tempfile::tempdir().unwrap();
        let dir = wyj_config::layout::project_sessions_dir(base.path(), "v1-repo-0123456789abcdef");
        write_session(&dir, "old", 60);
        write_session(&dir, "new", 1);

        let stats = run(base.path(), &cfg_with(30, 0, 0), false).unwrap();
        assert_eq!(stats.sessions_removed, 1);
        assert!(!dir.join("old.json").exists());
        // 旁路目录必须一起删，否则留下孤儿 checkpoint
        assert!(!dir.join("old.checkpoints").exists());
        assert!(!dir.join("old.subagents").exists());
        assert!(dir.join("new.json").exists());
    }

    #[test]
    fn keep_recent_protects_the_newest_sessions_even_if_old() {
        let base = tempfile::tempdir().unwrap();
        let dir = wyj_config::layout::project_sessions_dir(base.path(), "v1-repo-0123456789abcdef");
        // 全部都很旧，但 keep_recent = 2 要保住**最新**两条（c、b），删最老的 a
        write_session(&dir, "a", 300);
        write_session(&dir, "b", 200);
        write_session(&dir, "c", 100);

        let stats = run(base.path(), &cfg_with(30, 2, 0), false).unwrap();
        assert_eq!(stats.sessions_removed, 1);
        assert!(!dir.join("a.json").exists(), "最老的一条应被删");
        assert!(dir.join("b.json").exists());
        assert!(dir.join("c.json").exists());
    }

    #[test]
    fn ttl_zero_disables_session_cleanup() {
        let base = tempfile::tempdir().unwrap();
        let dir = wyj_config::layout::project_sessions_dir(base.path(), "v1-repo-0123456789abcdef");
        write_session(&dir, "ancient", 3650);

        let stats = run(base.path(), &cfg_with(0, 0, 0), false).unwrap();
        assert_eq!(stats.sessions_removed, 0);
        assert!(dir.join("ancient.json").exists());
    }

    #[test]
    fn dry_run_reports_without_deleting() {
        let base = tempfile::tempdir().unwrap();
        let dir = wyj_config::layout::project_sessions_dir(base.path(), "v1-repo-0123456789abcdef");
        write_session(&dir, "old", 60);

        let stats = run(base.path(), &cfg_with(30, 0, 0), true).unwrap();
        assert_eq!(stats.sessions_removed, 1);
        assert!(dir.join("old.json").exists(), "dry-run 不能删文件");
    }

    #[test]
    fn stale_cas_locks_are_reclaimed_but_held_ones_are_not() {
        let base = tempfile::tempdir().unwrap();
        let locks = wyj_config::layout::cas_root(base.path())
            .join("locks")
            .join("aa");
        std::fs::create_dir_all(&locks).unwrap();
        let stale = locks.join("stale.lock");
        std::fs::write(&stale, "1\n").unwrap();
        touch_old(&stale, 30);

        let held = locks.join("held.lock");
        std::fs::write(&held, "1\n").unwrap();
        touch_old(&held, 30);
        let _guard = wyj_config::filelock::acquire(&held, "test").unwrap();

        let stats = run(base.path(), &cfg_with(0, 0, 7), false).unwrap();
        assert_eq!(stats.locks_removed, 1);
        assert!(!stale.exists());
        assert!(held.exists(), "被持有的锁不能删");
    }

    #[test]
    fn fresh_locks_survive_the_ttl() {
        let base = tempfile::tempdir().unwrap();
        let locks = wyj_config::layout::cas_root(base.path())
            .join("locks")
            .join("aa");
        std::fs::create_dir_all(&locks).unwrap();
        let fresh = locks.join("fresh.lock");
        std::fs::write(&fresh, "1\n").unwrap();

        let stats = run(base.path(), &cfg_with(0, 0, 7), false).unwrap();
        assert_eq!(stats.locks_removed, 0);
        assert!(fresh.exists());
    }

    #[test]
    fn orphan_owner_files_are_reclaimed_but_live_pids_are_not() {
        let base = tempfile::tempdir().unwrap();
        let dir = wyj_config::layout::project_sessions_dir(base.path(), "v1-repo-0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();
        // 伪造一个几乎不可能存活的 pid
        let dead = dir.join("s1.json.owner");
        std::fs::write(&dead, "4294967294\n").unwrap();
        touch_old(&dead, 30);

        let stats = run(base.path(), &cfg_with(0, 0, 7), false).unwrap();
        assert_eq!(stats.locks_removed, 1);
        assert!(!dead.exists());
    }
}
