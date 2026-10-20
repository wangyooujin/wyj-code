//! 会话历史持久化（JSONL 格式，存于 ~/.wyj-code/history/）

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub session_id: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub turns: usize,
    pub cwd: String,
}

pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建历史目录失败: {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// 追加一条历史记录
    pub fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let path = self.dir.join("history.jsonl");
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

    /// 读取最近 N 条历史
    pub fn recent(&self, n: usize) -> Result<Vec<HistoryEntry>> {
        let path = self.dir.join("history.jsonl");
        if !path.exists() {
            return Ok(vec![]);
        }
        let content = std::fs::read_to_string(&path)?;
        let mut entries: Vec<HistoryEntry> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let len = entries.len();
        if len > n {
            entries.drain(..len - n);
        }
        Ok(entries)
    }
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
