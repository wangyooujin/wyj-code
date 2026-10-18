//! 子 Agent 消息增量日志 — 让「进程被 kill」也能恢复出对话现场。
//!
//! # 为什么还需要这一层
//!
//! 交接物（[`crate::handoff`]）覆盖两条路径：正常结束由任务自己写，ESC 中断由
//! `SubAgentHub` 在 abort 前从共享快照补写。两条都要求**进程还活着**。
//!
//! 剩下唯一没覆盖的是硬杀：`SIGKILL`、终端被关、笔记本断电、OOM killer。那时
//! 没有任何收尾代码会跑，交接物写不出来。可用的只有**消息产生时就已经落盘**
//! 的那份记录。
//!
//! # 为什么不复用 trace
//!
//! `a<id>.jsonl` 是**事件流**，记的是「工具 A 调了、返回了什么」。它拼不出对话
//! 历史：没有 assistant 的 tool_use 结构、没有消息边界、`input` 被截到 16KB。
//! 要恢复「做到哪了」，需要的是消息序列本身。
//!
//! # 格式
//!
//! 每行一个 JSON 对象，`type` 区分三种：
//!
//! ```jsonl
//! {"type":"append","pid":123,"message":{...}}
//! {"type":"update_last","pid":123,"message":{...}}
//! {"type":"snapshot","pid":123,"messages":[...]}
//! ```
//!
//! `snapshot` 由压缩与 context editing 触发——它们**原地替换整段历史**，没有
//! 这个锚点，重放会把压缩前的旧消息和压缩后的新消息拼成一个从未存在过的历史。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wyj_api::types::Message;
use wyj_core::session::{MessageEvent, MessageSink};

use crate::trace::trace_dir;

/// 单个子 Agent 消息日志的字节上限（默认 1MB）。
///
/// 比 trace 的 256KB 宽：这里存的是消息正文，而正文正是恢复现场所需的东西。
/// 超限后停写并留一条 `truncated` 标记，读侧据此知道日志不完整。
pub const DEFAULT_MAX_BYTES_PER_AGENT: u64 = 1024 * 1024;

/// 一行日志记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageRecord {
    Append {
        pid: u32,
        message: Message,
    },
    UpdateLast {
        pid: u32,
        message: Message,
    },
    /// 整段替换（压缩 / context editing）。读侧从这个点开始以 `messages` 为基准。
    Snapshot {
        pid: u32,
        messages: Vec<Message>,
    },
    /// 超限或异常：读侧见到它就应当把重建结果标成不完整。
    Truncated {
        pid: u32,
        reason: String,
    },
}

impl MessageRecord {
    pub fn pid(&self) -> u32 {
        match self {
            Self::Append { pid, .. }
            | Self::UpdateLast { pid, .. }
            | Self::Snapshot { pid, .. }
            | Self::Truncated { pid, .. } => *pid,
        }
    }
}

/// 消息日志文件路径：`<sessions_dir>/<session_id>.subagents/a<id>.messages.jsonl`。
pub fn message_log_file(sessions_dir: &Path, session_id: &str, id: u64) -> PathBuf {
    trace_dir(sessions_dir, session_id).join(format!("a{id}.messages.jsonl"))
}

/// 后台消息日志写手：与 [`crate::trace::TraceWriter`] 同构（单线程串行消费，
/// 调用方零阻塞），只是写入的是消息而不是 trace 事件。
pub struct MessageLogWriter {
    tx: tokio::sync::mpsc::UnboundedSender<(u64, MessageEvent)>,
}

impl MessageLogWriter {
    pub fn spawn(sessions_dir: PathBuf, session_id: String, max_bytes_per_agent: u64) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, MessageEvent)>();
        tokio::spawn(async move {
            let dir = trace_dir(&sessions_dir, &session_id);
            if std::fs::create_dir_all(&dir).is_err() {
                return;
            }
            let mut written: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
            let mut stopped: std::collections::HashSet<u64> = std::collections::HashSet::new();
            while let Some((id, event)) = rx.recv().await {
                if stopped.contains(&id) {
                    continue;
                }
                let record = match event {
                    MessageEvent::Appended(message) => MessageRecord::Append {
                        pid: std::process::id(),
                        message,
                    },
                    MessageEvent::UpdatedLast(message) => MessageRecord::UpdateLast {
                        pid: std::process::id(),
                        message,
                    },
                    MessageEvent::Replaced(messages) => MessageRecord::Snapshot {
                        pid: std::process::id(),
                        messages,
                    },
                };
                let mut line = match serde_json::to_string(&record) {
                    Ok(line) => line,
                    Err(_) => continue,
                };
                // 与 trace 同一套脱敏：工具输出里可能带 token / 私钥。
                line = wyj_core::redact_sensitive_text(&line);
                line.push('\n');
                let used = written.entry(id).or_insert(0);
                if *used + line.len() as u64 > max_bytes_per_agent {
                    // 停写前留一条标记：读侧据此知道「这份日志不完整」，
                    // 而不是把截断后的历史当成真实历史。
                    let marker = MessageRecord::Truncated {
                        pid: std::process::id(),
                        reason: format!("exceeded {max_bytes_per_agent} bytes"),
                    };
                    if let Ok(mut marker_line) = serde_json::to_string(&marker) {
                        marker_line.push('\n');
                        append(
                            &dir.join(format!("a{id}.messages.jsonl")),
                            marker_line.as_bytes(),
                        );
                    }
                    stopped.insert(id);
                    continue;
                }
                *used += line.len() as u64;
                append(&dir.join(format!("a{id}.messages.jsonl")), line.as_bytes());
            }
        });
        Self { tx }
    }

    /// 生成某个子 Agent 的 sink。`try_send` 满则丢弃——落盘是尽力而为，
    /// 绝不能因为写日志把子 Agent 拖住。
    pub fn sink_for(&self, id: u64) -> MessageSink {
        let tx = self.tx.clone();
        std::sync::Arc::new(move |event: MessageEvent| {
            let _ = tx.send((id, event));
        })
    }
}

fn append(path: &Path, bytes: &[u8]) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(bytes);
    }
}

/// 重放结果。
#[derive(Debug, Clone, Default)]
pub struct ReplayedMessages {
    pub messages: Vec<Message>,
    /// 日志被截断（见到 `truncated` 标记）或存在无法解析的行。
    /// 为 true 时调用方**不应**把结果当作完整历史。
    pub incomplete: bool,
}

/// 从消息日志重放出对话历史。
///
/// 重放规则：从最后一个 `snapshot` 起算（没有则从空开始），之后 `append` 追加、
/// `update_last` 覆盖最后一条。无法解析的行跳过并置 `incomplete`——日志在
/// 进程被 kill 时最后一行可能只写了一半。
pub fn replay(path: &Path) -> ReplayedMessages {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ReplayedMessages::default();
    };
    let mut out = ReplayedMessages::default();
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<MessageRecord>(line) else {
            if !line.trim().is_empty() {
                out.incomplete = true;
            }
            continue;
        };
        match record {
            MessageRecord::Append { message, .. } => out.messages.push(message),
            MessageRecord::UpdateLast { message, .. } => {
                if let Some(last) = out.messages.last_mut() {
                    *last = message;
                } else {
                    // 没有前置消息却出现 update：日志本身不完整
                    out.incomplete = true;
                    out.messages.push(message);
                }
            }
            MessageRecord::Snapshot { messages, .. } => out.messages = messages,
            MessageRecord::Truncated { .. } => out.incomplete = true,
        }
    }
    out
}

/// 从重放出的消息里提取「最后一轮 assistant 说了什么」。
///
/// 与 `SubAgentHandoff::conclusion` 同一语义：这是硬杀场景下唯一能拿到的
/// 结论来源。取最后一条**非空** assistant 文本。
pub fn last_assistant_text(messages: &[Message]) -> String {
    use wyj_api::types::{ContentBlock, Role};
    let mut out = String::new();
    for msg in messages {
        if !matches!(msg.role, Role::Assistant) {
            continue;
        }
        let text: String = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if !text.trim().is_empty() {
            out = text;
        }
    }
    out
}

/// 从消息日志里能救回来的进度。
#[derive(Debug, Clone)]
pub struct RecoveredProgress {
    /// 最后一轮 assistant 文本（可能为空：硬杀可能落在两轮之间）
    pub conclusion: String,
    /// 日志本身被截断/损坏，结论可能不是最终状态
    pub incomplete: bool,
    /// 重放出的消息条数，用于判断「确实跑过一轮又一轮」还是只剩个开头
    pub message_count: usize,
}

/// 硬杀场景的兜底入口：没有交接物时，从消息日志重建进度。
///
/// 返回 `None` 的条件是日志不存在或一条消息都没有——那说明子 Agent 还没
/// 真正开跑，硬杀造成的损失为零，没必要编一份空交接物出来。
pub fn recover_progress(
    sessions_dir: &Path,
    session_id: &str,
    id: u64,
) -> Option<RecoveredProgress> {
    let path = message_log_file(sessions_dir, session_id, id);
    let replayed = replay(&path);
    if replayed.messages.is_empty() {
        return None;
    }
    Some(RecoveredProgress {
        conclusion: last_assistant_text(&replayed.messages),
        incomplete: replayed.incomplete,
        message_count: replayed.messages.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_api::types::{ContentBlock, Role};

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn write_lines(path: &Path, records: &[MessageRecord]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut body = String::new();
        for r in records {
            body.push_str(&serde_json::to_string(r).unwrap());
            body.push('\n');
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn replay_appends_and_updates_in_order() {
        let dir = std::env::temp_dir().join(format!("wyj-msglog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = message_log_file(&dir, "s1", 2);
        write_lines(
            &path,
            &[
                MessageRecord::Append {
                    pid: 1,
                    message: user("任务"),
                },
                MessageRecord::Append {
                    pid: 1,
                    message: assistant("第一轮结论"),
                },
                // push_user_blocks_merged：工具结果合并进最后一条 user 消息
                MessageRecord::Append {
                    pid: 1,
                    message: user("工具结果 A"),
                },
                MessageRecord::UpdateLast {
                    pid: 1,
                    message: user("工具结果 A + B"),
                },
            ],
        );
        let replayed = replay(&path);
        assert!(!replayed.incomplete);
        assert_eq!(replayed.messages.len(), 3);
        assert_eq!(replayed.messages[2].content.len(), 1);
        let ContentBlock::Text { text } = &replayed.messages[2].content[0] else {
            panic!("应是文本块");
        };
        assert_eq!(text, "工具结果 A + B");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 压缩后的 snapshot 必须成为新基准——否则重放会把压缩前后拼在一起。
    #[test]
    fn snapshot_resets_the_replay_baseline() {
        let dir = std::env::temp_dir().join(format!("wyj-msglog-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = message_log_file(&dir, "s1", 3);
        write_lines(
            &path,
            &[
                MessageRecord::Append {
                    pid: 1,
                    message: user("旧消息 1"),
                },
                MessageRecord::Append {
                    pid: 1,
                    message: user("旧消息 2"),
                },
                MessageRecord::Snapshot {
                    pid: 1,
                    messages: vec![user("压缩摘要")],
                },
                MessageRecord::Append {
                    pid: 1,
                    message: assistant("压缩后第一轮"),
                },
            ],
        );
        let replayed = replay(&path);
        assert_eq!(replayed.messages.len(), 2);
        let ContentBlock::Text { text } = &replayed.messages[0].content[0] else {
            panic!()
        };
        assert_eq!(text, "压缩摘要");
    }

    /// 被 kill 时最后一行往往只写了一半：必须跳过并标记不完整，而不是 panic。
    #[test]
    fn half_written_trailing_line_is_skipped() {
        let dir = std::env::temp_dir().join(format!("wyj-msglog-trunc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = message_log_file(&dir, "s1", 4);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let valid = serde_json::to_string(&MessageRecord::Append {
            pid: 1,
            message: user("完整的一行"),
        })
        .unwrap();
        std::fs::write(&path, format!("{valid}\n{{\"type\":\"append\",\"mess")).unwrap();
        let replayed = replay(&path);
        assert_eq!(replayed.messages.len(), 1);
        assert!(replayed.incomplete, "半行必须被标记为不完整");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_marker_marks_the_log_incomplete() {
        let dir = std::env::temp_dir().join(format!("wyj-msglog-mark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = message_log_file(&dir, "s1", 5);
        write_lines(
            &path,
            &[
                MessageRecord::Append {
                    pid: 1,
                    message: user("一"),
                },
                MessageRecord::Truncated {
                    pid: 1,
                    reason: "exceeded".to_string(),
                },
            ],
        );
        let replayed = replay(&path);
        assert!(replayed.incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_assistant_text_skips_empty_turns() {
        let messages = vec![
            user("任务"),
            assistant("第一轮"),
            // 工具调用轮的 assistant 可能没有文本块
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }],
            },
        ];
        assert_eq!(last_assistant_text(&messages), "第一轮");
    }

    #[test]
    fn replay_of_missing_file_is_empty_not_an_error() {
        let replayed = replay(Path::new("/nonexistent/a1.messages.jsonl"));
        assert!(replayed.messages.is_empty());
        assert!(!replayed.incomplete);
    }
}
