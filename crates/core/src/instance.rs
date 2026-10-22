//! 同一项目下的多实例注册表。
//!
//! 会话锁（`session_store::SessionStore::active_owner`）只回答「**这个会话**是否被
//! 别人用着」，而真正会互相踩坏的是工作区本身：两个实例开在两个不同的新会话里，
//! 各自 id 不同、互不冲突，却对着**同一份 checkout** 跑 `Edit` / `cargo test` /
//! `/rewind`。`SessionStore` 对此一无所知，于是这类冲突此前完全静默。
//!
//! 本模块补的就是这一层：每个进程启动时在自己的项目目录下留一份
//! `<config_base>/projects/<project_key>/instances/<pid>-<uuid>.json`，退出时删除。
//! 残留文件（被 `kill -9`）由 [`live_peers`] 的存活判定自动忽略，因此不需要后台清理任务。
//!
//! **只做提示，不做阻断**——同一 checkout 里跑两个实例是合法用法（一个改代码、
//! 一个只读问问题），注册表的价值是让用户在动手前就知道对面还有人。真正的阻断放在
//! [`crate::checkpoint::CheckpointStore::preview_files`] 这类会造成**不可逆损失**的
//! 动作上。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::history::now_iso;
use crate::project::{project_key, project_root};

/// 一个正在运行（或已崩溃未清理）的实例。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceRecord {
    pub pid: u32,
    pub session_id: String,
    /// 实例实际的工作目录（`--cwd` 解析后的绝对路径）。冲突判定只认这个。
    pub cwd: PathBuf,
    /// 项目身份（`~/.wyj-code` 下各类状态的归属键）。
    pub project_root: PathBuf,
    pub started_at: String,
}

/// 本进程在项目实例表里的登记项；`drop` 时删除，正常退出的路径不需要额外收尾。
pub struct InstanceGuard {
    path: PathBuf,
}

impl InstanceGuard {
    /// 登记本进程，并返回**登记时已存在**的其他存活实例。
    ///
    /// 返回值是快照而非实时视图：它只用于启动时的一句告警。运行期间新起的实例
    /// 不会出现在这里，那由对方启动时的告警负责——两边各提醒一次，正是想要的。
    pub fn register(
        config_base: &Path,
        cwd: &Path,
        session_id: &str,
    ) -> Result<(Self, Vec<InstanceRecord>)> {
        let root = project_root(cwd);
        let dir = instances_dir(config_base, &root);
        fs::create_dir_all(&dir)
            .with_context(|| format!("create instance registry {}", dir.display()))?;
        let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let peers = collect(&dir, &canonical_cwd);
        let path = dir.join(format!(
            "{}-{}.json",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        let record = InstanceRecord {
            pid: std::process::id(),
            session_id: session_id.to_string(),
            cwd: canonical_cwd,
            project_root: root,
            started_at: now_iso(),
        };
        crate::serialize::write_json_atomic(&path, &record)?;
        Ok((Self { path }, peers))
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // best-effort：删不掉只留一条 json，`live_peers` 靠 pid 存活判定忽略它。
        let _ = fs::remove_file(&self.path);
    }
}

/// 当前是否有**其他存活实例**正对着同一个工作目录。
///
/// 与 [`InstanceGuard::register`] 的区别是实时查询：`/rewind` 这类不可逆操作在
/// 执行前要知道「此刻」还有谁，而不是启动那一刻有谁。
pub fn live_peers(cwd: &Path) -> Vec<InstanceRecord> {
    let Ok(config_base) = wyj_config::config_dir() else {
        return Vec::new();
    };
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    collect(
        &instances_dir(&config_base, &project_root(&canonical_cwd)),
        &canonical_cwd,
    )
}

fn instances_dir(config_base: &Path, root: &Path) -> PathBuf {
    config_base
        .join("projects")
        .join(project_key(root))
        .join("instances")
}

/// 冒名写一条「别的存活进程正用着这个目录」的登记，返回登记文件路径。
///
/// 只给跨模块测试用：真实注册必然落成**本进程** pid，而 [`collect`] 会把自己过滤
/// 掉，于是「有 peer」这条路径在单进程测试里永远走不到。
///
/// 返回的路径在 `~/.wyj-code/projects/<key>/instances/` 下——`project_key` 由调用方
/// 的临时目录派生，因此删掉 `instances/` 的父目录即可完全收尾（见 [`remove_for_tests`]）。
#[cfg(test)]
pub(crate) fn register_foreign_for_tests(config_base: &Path, cwd: &Path, pid: u32) -> PathBuf {
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let dir = instances_dir(config_base, &project_root(&canonical_cwd));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{pid}-foreign.json"));
    let record = InstanceRecord {
        pid,
        session_id: "foreign".into(),
        project_root: project_root(&canonical_cwd),
        cwd: canonical_cwd,
        started_at: now_iso(),
    };
    crate::serialize::write_json_atomic(&path, &record).unwrap();
    path
}

/// 清掉 [`register_foreign_for_tests`] 在整个 `projects/<key>/` 下留下的痕迹。
///
/// 测试必须自己收尾：这些目录落在**真实**的 `~/.wyj-code/projects/` 里，一个用例
/// 每次跑都留一个空壳，几百次之后 `projects/` 会被临时项目名塞满。
#[cfg(test)]
pub(crate) fn remove_for_tests(record_path: &Path) {
    let Some(key_dir) = record_path.parent().and_then(Path::parent) else {
        return;
    };
    let _ = fs::remove_dir_all(key_dir);
}

/// 读整个登记表，顺手删掉死进程留下的条目，只返回「同 cwd + 存活 + 不是自己」的。
///
/// **同 cwd 才是冲突**：同一项目下开了 worktree 的两个实例，文件物理隔离，互不影响，
/// 把它们算成冲突只会训练用户忽略告警。
fn collect(dir: &Path, cwd: &Path) -> Vec<InstanceRecord> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let self_pid = std::process::id();
    let mut peers = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<InstanceRecord>(&content) else {
            // 半截 JSON 或旧 schema：当作无效登记清掉，否则每次启动都要读一遍。
            let _ = fs::remove_file(&path);
            continue;
        };
        if !wyj_config::filelock::is_process_alive(record.pid) {
            let _ = fs::remove_file(&path);
            continue;
        }
        // pid 会被系统复用：一个死进程的登记若恰好撞上复用的 pid，会把无关的活进程
        // 当成「另一个 wyj-code」。记录里的 cwd 一并校验可以把误报压到几乎没有。
        if record.pid == self_pid || record.cwd != cwd {
            continue;
        }
        peers.push(record);
    }
    peers.sort_by(|left, right| left.started_at.cmp(&right.started_at));
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "wyj-instance-{tag}-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn fake_project(base: &Path) -> PathBuf {
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        repo
    }

    /// 手工写一条「另一个存活进程」的登记。用真实存活 pid（本进程）会撞上自我过滤，
    /// 所以换成 pid 1（init/launchd，永远存在）。
    fn write_foreign(dir: &Path, cwd: &Path, pid: u32) {
        fs::create_dir_all(dir).unwrap();
        let record = InstanceRecord {
            pid,
            session_id: "other".into(),
            cwd: cwd.canonicalize().unwrap(),
            project_root: project_root(cwd),
            started_at: now_iso(),
        };
        fs::write(
            dir.join(format!("{pid}-test.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn registering_reports_a_live_peer_on_the_same_checkout() {
        let base = tmp_base("peer");
        let repo = fake_project(&base);
        let dir = instances_dir(&base, &project_root(&repo));
        write_foreign(&dir, &repo, 1);

        let (_guard, peers) = InstanceGuard::register(&base, &repo, "mine").unwrap();
        assert_eq!(peers.len(), 1, "同一 checkout 上的存活实例必须被报告");
        assert_eq!(peers[0].pid, 1);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_peer_in_a_different_worktree_is_not_a_conflict() {
        let base = tmp_base("worktree");
        let repo = fake_project(&base);
        let worktree = base.join("wt");
        fs::create_dir_all(&worktree).unwrap();
        write_foreign(&instances_dir(&base, &project_root(&repo)), &worktree, 1);

        let (_guard, peers) = InstanceGuard::register(&base, &repo, "mine").unwrap();
        assert!(
            peers.is_empty(),
            "worktree 里文件物理隔离，不该被算成同目录冲突"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn dead_pids_are_pruned_instead_of_reported_forever() {
        let base = tmp_base("dead");
        let repo = fake_project(&base);
        let dir = instances_dir(&base, &project_root(&repo));
        // 必须拿一个**真的死掉**的 pid：`u32::MAX` 转成 `pid_t` 是 -1，而
        // `kill(-1, 0)` 命中所有进程返回 0，会被判成「存活」——这个坑本身也说明
        // 存活判定不能靠猜。
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();
        write_foreign(&dir, &repo, dead_pid);

        let (_guard, peers) = InstanceGuard::register(&base, &repo, "mine").unwrap();
        assert!(peers.is_empty(), "死进程不算占用");
        let remaining: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&dead_pid.to_string())
            })
            .collect();
        assert!(remaining.is_empty(), "死进程的登记必须在读表时被清掉");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn guard_removes_its_own_record_on_drop() {
        let base = tmp_base("drop");
        let repo = fake_project(&base);

        let guard = InstanceGuard::register(&base, &repo, "mine").unwrap().0;
        let path = guard.path.clone();
        assert!(path.exists());
        drop(guard);
        assert!(!path.exists(), "正常退出不该留下登记");
        fs::remove_dir_all(&base).ok();
    }
}
