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
    format!("sess-{ts}")
}

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
