//! 会话历史持久化（JSONL 格式，按项目落在 `<data_root>/history/<project_key>/`）
//!
//! 每次会话结束追加一行 token / 轮数统计，供 `/history` 与 `usage` 回溯。
//!
//! 早期是全局单文件 `history/history.jsonl`（靠记录里的 `cwd` 做逻辑隔离）。
//! 改成按项目分目录后，删项目、备份项目、看清「这个仓库花了多少」都变成
//! 一次目录操作；旧文件仍会被 [`HistoryStore::recent`] 读到，不丢历史。

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub session_id: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub turns: usize,
    pub cwd: String,
}

const HISTORY_FILE: &str = "history.jsonl";

pub struct HistoryStore {
    /// 历史根目录（`<data_root>/history`）。单目录模式下与 `dir` 相同。
    root: PathBuf,
    /// 当前项目的历史目录（`<root>/<project_key>`，单目录模式下等于 `root`）。
    dir: PathBuf,
}

impl HistoryStore {
    /// 单目录模式：所有历史写进 `dir/history.jsonl`。
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建历史目录失败: {}", dir.display()))?;
        Ok(Self {
            root: dir.clone(),
            dir,
        })
    }

    /// 按项目分目录模式：写进 `<root>/<project_key>/history.jsonl`。
    pub fn for_project(root: PathBuf, cwd: &Path) -> Result<Self> {
        let dir = root.join(crate::project::project_key(cwd));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建历史目录失败: {}", dir.display()))?;
        Ok(Self { root, dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 追加一条历史记录
    pub fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let path = self.dir.join(HISTORY_FILE);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开历史文件失败: {}", path.display()))?;
        let line = serde_json::to_string(entry)? + "\n";
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// 读取最近 N 条历史。
    ///
    /// 按项目分目录模式下还会并上根目录那份旧 `history.jsonl` 中属于当前项目
    /// 的条目——升级后没跑迁移的用户不该看到 `/history` 突然变空。
    pub fn recent(&self, n: usize) -> Result<Vec<HistoryEntry>> {
        let mut entries = read_jsonl(&self.dir.join(HISTORY_FILE));
        if self.root != self.dir {
            let key = self
                .dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            let mut legacy: Vec<HistoryEntry> = read_jsonl(&self.root.join(HISTORY_FILE))
                .into_iter()
                .filter(|entry| crate::project::project_key(Path::new(&entry.cwd)) == key)
                .collect();
            legacy.extend(entries);
            entries = legacy;
        }
        entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        let len = entries.len();
        if len > n {
            entries.drain(..len - n);
        }
        Ok(entries)
    }
}

fn read_jsonl(path: &Path) -> Vec<HistoryEntry> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn new_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let unique = uuid::Uuid::new_v4().simple().to_string();
    format!("sess-{ts}-{}", &unique[..12])
}

/// 秒级精度的 UTC 时间戳（`2026-10-20T08:45:00Z`）。
///
/// 与 `crate::textutil` 无关；与 `now_rfc3339()`（`core/memory_v3` /
/// `core/evolution` 用）**格式不同**，两者不能混用：
/// - 本函数：秒级、无时区偏移，字符串**字典序 == 时间序**，可直接用于排序。
/// - `now_rfc3339()`：纳秒级、带 `+00:00` 偏移，是合法 RFC3339，可被
///   `DateTime::parse_from_rfc3339` 解析。
///
/// 名字叫 `now_iso` 但格式其实不是完整 ISO8601（缺毫秒），历史包袱。
pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session_id: &str, ts: &str, cwd: &Path) -> HistoryEntry {
        HistoryEntry {
            timestamp: ts.to_string(),
            session_id: session_id.to_string(),
            input_tokens: 1,
            output_tokens: 2,
            turns: 3,
            cwd: cwd.display().to_string(),
        }
    }

    /// 分目录模式：每个项目一个子目录，`recent` 只回自己的。
    #[test]
    fn project_scoped_history_is_isolated_per_project() {
        let base = std::env::temp_dir().join(format!("wyj-hist-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo_a = base.join("repoA");
        let repo_b = base.join("repoB");
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();
        let root = base.join("history");

        let store_a = HistoryStore::for_project(root.clone(), &repo_a).unwrap();
        let store_b = HistoryStore::for_project(root.clone(), &repo_b).unwrap();
        assert_ne!(store_a.dir(), store_b.dir());

        store_a
            .append(&entry("a1", "2026-07-06T10:00:00Z", &repo_a))
            .unwrap();
        store_b
            .append(&entry("b1", "2026-07-06T11:00:00Z", &repo_b))
            .unwrap();

        let a = store_a.recent(10).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].session_id, "a1");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 升级后没跑迁移时，根目录那份旧 `history.jsonl` 里属于当前项目的行
    /// 必须仍然读得到，且按时间序并入。
    #[test]
    fn project_scoped_recent_merges_legacy_flat_file() {
        let base = std::env::temp_dir().join(format!("wyj-hist-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo_a = base.join("repoA");
        let repo_b = base.join("repoB");
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();
        let root = base.join("history");

        // 旧布局：根目录一个文件，混着两个项目的行
        let flat = HistoryStore::new(root.clone()).unwrap();
        flat.append(&entry("old-a", "2026-07-06T09:00:00Z", &repo_a))
            .unwrap();
        flat.append(&entry("old-b", "2026-07-06T08:00:00Z", &repo_b))
            .unwrap();

        let store_a = HistoryStore::for_project(root.clone(), &repo_a).unwrap();
        store_a
            .append(&entry("new-a", "2026-07-07T09:00:00Z", &repo_a))
            .unwrap();

        let recent = store_a.recent(10).unwrap();
        let ids: Vec<&str> = recent.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, vec!["old-a", "new-a"], "按时间升序，且只含 repoA");
        // `recent(n)` 取的是最近 n 条
        let tail = store_a.recent(1).unwrap();
        assert_eq!(tail[0].session_id, "new-a");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_ids_are_unique_under_concurrent_creation() {
        let ids: std::collections::HashSet<String> = (0..256)
            .map(|_| std::thread::spawn(new_session_id))
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 256);
        assert!(ids.iter().all(|id| id.starts_with("sess-")));
    }

    /// `now_iso()` 的格式契约：`YYYY-MM-DDTHH:MM:SSZ`。
    ///
    /// 三处落盘记录的 timestamp 字段依赖它：
    /// - `session_store` 的 meta / file timestamp（且按**字符串**排序，
    ///   依赖「字典序 == 时间序」）。
    /// - `usage_cmd` 的 `--since` 过滤（按 `YYYY-MM-DD` 前缀比较）。
    /// - `acp` 事件时间戳。
    ///
    /// 因此格式不能随意改：改成带偏移的 RFC3339 会让 session 列表的排序
    /// 在跨时区表示下失真。
    #[test]
    fn now_iso_has_a_stable_sortable_format() {
        let ts = now_iso();
        assert_eq!(ts.len(), 20, "期望 YYYY-MM-DDTHH:MM:SSZ，实际 {ts}");
        assert!(ts.ends_with('Z'), "应以 Z 结尾（UTC），实际 {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        // 字典序 == 时间序：后生成的字符串不小于先生成的。
        let earlier = now_iso();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let later = now_iso();
        assert!(earlier <= later, "{earlier} 应 <= {later}");
    }
}
