//! 子 Agent 完整执行轨迹落盘 — 旁路存储，独立于会话消息（`Message`/`ContentBlock`）。
//!
//! 格式：JSONL，每个子 Agent 一个文件：
//! `<sessions_dir>/<session_id>.subagents/a<id>.jsonl`。
//! 写入收敛到一个专职后台任务（单线程串行 append），`TraceWriter::send` 只是
//! non-blocking channel 发送；8 并发子 Agent 各自文件互不干扰、无需加锁。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// 单次工具调用 input 落盘上限（字节）；超限截断（保头，输入的关键信息通常在开头）。
pub const MAX_INPUT_BYTES: usize = 16_000;
/// 单次工具调用 output 落盘时保留的头部字节数。
pub const OUTPUT_HEAD_BYTES: usize = 20_000;
/// 单次工具调用 output 落盘时保留的尾部字节数（结论/报错通常在尾部）。
pub const OUTPUT_TAIL_BYTES: usize = 10_000;
/// 单个子 Agent trace 文件默认字节上限，超限后续事件停写（`SubAgentCfg::trace_max_bytes_per_agent` 可覆盖）。
pub const DEFAULT_MAX_TRACE_FILE_BYTES: u64 = 256 * 1024;

/// 子 Agent 执行轨迹事件（落盘用，字段为完整数据，非 UI 摘要）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TraceEvent {
    Started {
        agent_type: String,
        description: String,
        background: bool,
        /// 关联到会话消息里 `ContentBlock::ToolUse.id`，供跨会话反查。
        parent_tool_use_id: Option<String>,
    },
    ToolStart {
        tool_name: String,
        /// 完整 input 的 JSON 文本（截断时 `truncated=true`）。
        input_json: String,
        truncated: bool,
    },
    ToolEnd {
        tool_name: String,
        is_error: bool,
        elapsed_secs: f64,
        /// 完整 output 文本（截断时 `truncated=true`）。
        output: String,
        truncated: bool,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
    Control {
        action: String,
        accepted: bool,
    },
    Done {
        result: String,
        is_error: bool,
        elapsed_secs: f64,
    },
}

/// 截断 input JSON：保头，长度以字节计。
pub fn truncate_input(value: &serde_json::Value) -> (String, bool) {
    let s = value.to_string();
    if s.len() <= MAX_INPUT_BYTES {
        (s, false)
    } else {
        (
            crate::textutil::truncate_str(&s, MAX_INPUT_BYTES).to_string(),
            true,
        )
    }
}

/// 截断 output：保头 [`OUTPUT_HEAD_BYTES`] + 保尾 [`OUTPUT_TAIL_BYTES`]。
pub fn truncate_output(s: &str) -> (String, bool) {
    if s.len() <= OUTPUT_HEAD_BYTES + OUTPUT_TAIL_BYTES {
        (s.to_string(), false)
    } else {
        (
            crate::textutil::truncate_head_tail(s, OUTPUT_HEAD_BYTES, OUTPUT_TAIL_BYTES),
            true,
        )
    }
}

/// 某会话的子 Agent trace 根目录：`<sessions_dir>/<session_id>.subagents/`。
pub fn trace_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.subagents"))
}

/// 单个子 Agent 的 trace 文件路径。
pub fn trace_file(sessions_dir: &Path, session_id: &str, id: u64) -> PathBuf {
    trace_dir(sessions_dir, session_id).join(format!("a{id}.jsonl"))
}

/// 列出某会话已落盘的全部子 Agent id（升序）。目录不存在时返回空列表。
pub fn list_trace_ids(sessions_dir: &Path, session_id: &str) -> Vec<u64> {
    let dir = trace_dir(sessions_dir, session_id);
    let mut ids: Vec<u64> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                name.strip_prefix('a')?
                    .strip_suffix(".jsonl")?
                    .parse::<u64>()
                    .ok()
            })
            .collect(),
        Err(_) => vec![],
    };
    ids.sort_unstable();
    ids
}

/// 读回单个子 Agent 的完整事件序列（按写入顺序）。忽略无法解析的行（如被截断的最后一行）。
pub fn read_trace(path: &Path) -> std::io::Result<Vec<TraceEvent>> {
    let content = std::fs::read_to_string(path)?;
    Ok(content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// 后台 trace 写手：串行处理 `(sub_agent_id, TraceEvent)` 队列，每 id 独立文件 append。
/// 单线程消费保证并发写入互不干扰、无需为文件加锁；调用方 `send` 为 non-blocking。
pub struct TraceWriter {
    tx: mpsc::UnboundedSender<(u64, TraceEvent)>,
}

impl TraceWriter {
    /// 启动后台写手任务。`max_bytes_per_agent` 为单个子 Agent trace 文件的字节上限，
    /// 超限后该 id 的后续事件静默丢弃（不影响其它子 Agent、不影响主流程）。
    pub fn spawn(sessions_dir: PathBuf, session_id: String, max_bytes_per_agent: u64) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, TraceEvent)>();
        tokio::spawn(async move {
            let dir = trace_dir(&sessions_dir, &session_id);
            if std::fs::create_dir_all(&dir).is_err() {
                return;
            }
            let mut written_bytes: HashMap<u64, u64> = HashMap::new();
            let mut over_limit: HashSet<u64> = HashSet::new();
            while let Some((id, ev)) = rx.recv().await {
                if over_limit.contains(&id) {
                    continue;
                }
                let Ok(mut line) = serde_json::to_string(&ev) else {
                    continue;
                };
                line = wyj_core::redact_sensitive_text(&line);
                line.push('\n');
                let used = written_bytes.entry(id).or_insert(0);
                if *used + line.len() as u64 > max_bytes_per_agent {
                    over_limit.insert(id);
                    continue;
                }
                *used += line.len() as u64;
                let path = dir.join(format!("a{id}.jsonl"));
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = f.write_all(line.as_bytes());
                }
            }
        });
        Self { tx }
    }

    /// 投递一个事件，non-blocking，channel 已关闭（写手任务退出）时静默丢弃。
    pub fn send(&self, id: u64, ev: TraceEvent) {
        let _ = self.tx.send((id, ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 测试专用临时目录：进程内自增计数器 + 纳秒时间戳，避免并行测试互相踩踏
    /// （不引入 uuid 依赖只为测试用一次）。
    fn unique_tmp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("wyj-trace-test-{label}-{nanos}-{n}"))
    }

    #[test]
    fn truncate_input_passthrough_when_short() {
        let v = serde_json::json!({"a": 1});
        let (s, truncated) = truncate_input(&v);
        assert!(!truncated);
        assert_eq!(s, v.to_string());
    }

    #[test]
    fn truncate_input_truncates_when_long() {
        let v = serde_json::json!({"a": "x".repeat(MAX_INPUT_BYTES + 100)});
        let (s, truncated) = truncate_input(&v);
        assert!(truncated);
        assert!(s.len() <= MAX_INPUT_BYTES);
    }

    #[test]
    fn truncate_output_keeps_head_and_tail() {
        let s = "H".repeat(OUTPUT_HEAD_BYTES) + &"T".repeat(OUTPUT_TAIL_BYTES + 500);
        let (out, truncated) = truncate_output(&s);
        assert!(truncated);
        assert!(out.starts_with('H'));
        assert!(out.ends_with('T'));
    }

    #[tokio::test]
    async fn writer_persists_events_to_jsonl() {
        let tmp = unique_tmp_dir("basic");
        let session_id = "sess-test".to_string();
        let writer = TraceWriter::spawn(
            tmp.clone(),
            session_id.clone(),
            DEFAULT_MAX_TRACE_FILE_BYTES,
        );

        writer.send(
            1,
            TraceEvent::Started {
                agent_type: "general-purpose".into(),
                description: "test task".into(),
                background: false,
                parent_tool_use_id: Some("toolu_1".into()),
            },
        );
        writer.send(
            1,
            TraceEvent::ToolStart {
                tool_name: "Read".into(),
                input_json: "{\"file_path\":\"/a\"}".into(),
                truncated: false,
            },
        );
        writer.send(
            1,
            TraceEvent::Done {
                result: "done".into(),
                is_error: false,
                elapsed_secs: 1.5,
            },
        );

        // channel 消费是异步的，轮询等待文件出现内容（避免测试里裸 sleep 造成 flaky）。
        let path = trace_file(&tmp, &session_id, 1);
        for _ in 0..100 {
            if let Ok(events) = read_trace(&path) {
                if events.len() == 3 {
                    assert!(matches!(events[0], TraceEvent::Started { .. }));
                    assert!(matches!(events[2], TraceEvent::Done { .. }));
                    let _ = std::fs::remove_dir_all(&tmp);
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&tmp);
        panic!("trace file did not converge to 3 events in time");
    }

    #[tokio::test]
    async fn writer_keeps_concurrent_ids_separate() {
        let tmp = unique_tmp_dir("concurrent");
        let session_id = "sess-concurrent".to_string();
        let writer = TraceWriter::spawn(
            tmp.clone(),
            session_id.clone(),
            DEFAULT_MAX_TRACE_FILE_BYTES,
        );

        for id in 1..=8u64 {
            writer.send(
                id,
                TraceEvent::Started {
                    agent_type: format!("type-{id}"),
                    description: "d".into(),
                    background: false,
                    parent_tool_use_id: None,
                },
            );
        }

        for id in 1..=8u64 {
            let path = trace_file(&tmp, &session_id, id);
            let mut ok = false;
            for _ in 0..100 {
                if let Ok(events) = read_trace(&path) {
                    if let Some(TraceEvent::Started { agent_type, .. }) = events.first() {
                        assert_eq!(agent_type, &format!("type-{id}"));
                        ok = true;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(ok, "id {id} trace missing or wrong content");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn writer_stops_after_max_bytes_per_agent() {
        let tmp = unique_tmp_dir("cap");
        let session_id = "sess-cap".to_string();
        let writer = TraceWriter::spawn(tmp.clone(), session_id.clone(), 200);

        writer.send(
            1,
            TraceEvent::Started {
                agent_type: "t".into(),
                description: "d".into(),
                background: false,
                parent_tool_use_id: None,
            },
        );
        for _ in 0..50 {
            writer.send(
                1,
                TraceEvent::ToolEnd {
                    tool_name: "Bash".into(),
                    is_error: false,
                    elapsed_secs: 0.1,
                    output: "x".repeat(100),
                    truncated: false,
                },
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let path = trace_file(&tmp, &session_id, 1);
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(bytes <= 200, "trace file exceeded cap: {bytes} bytes");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn writer_redacts_secret_like_trace_content() {
        let tmp = unique_tmp_dir("secret");
        let session_id = "sess-secret".to_string();
        let writer = TraceWriter::spawn(
            tmp.clone(),
            session_id.clone(),
            DEFAULT_MAX_TRACE_FILE_BYTES,
        );
        let secret = format!("{}{}", "sk-test-", "F".repeat(24));
        writer.send(
            1,
            TraceEvent::Done {
                result: format!("credential: {secret}"),
                is_error: false,
                elapsed_secs: 0.1,
            },
        );
        let path = trace_file(&tmp, &session_id, 1);
        for _ in 0..100 {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if raw.contains(wyj_core::REDACTED_SECRET) {
                    assert!(!raw.contains(&secret));
                    let _ = std::fs::remove_dir_all(&tmp);
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&tmp);
        panic!("redacted trace did not reach disk");
    }
}
