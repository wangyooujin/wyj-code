//! 主会话消息增量日志 —— 让「进程被硬杀」也不丢最后一轮。
//!
//! # 为什么需要它
//!
//! 权威历史是 `<sid>.json`，但它只在 `save()` 时整份重写。TUI 的 autosave 是
//! fire-and-forget，headless / `-p` / ACP 更只在收尾时写一次。`SIGKILL`、
//! 终端被关、笔记本断电、OOM killer 落在两次 save 之间时，那一整轮对话
//! （可能包含几十次工具调用）就没了。
//!
//! 子 Agent 早就解决了这个问题（`wyj_tools::message_log`），但它挂在
//! `Session.message_sink` 上，而主会话从来没接过这个口子。本模块补上这一半：
//! 同一个 `MessageEvent` 抽象、同一份记录格式，只是写到会话自己的目录。
//!
//! # 为什么不用 WAL / 事件溯源取代 `.json`
//!
//! `.json` 是**快照**（一次全文重写，读取简单、原子替换）；本日志是**旁路**
//! （尽力而为，可能丢事件、可能半行）。两者职责不同：日志只回答「比快照多
//! 出来的那几轮是什么」，`.json` 仍然是权威。这样也避免了把整套加载路径
//! 改造成「必须先重放日志」的强耦合。
//!
//! # 与 `.json` 的合并规则
//!
//! [`merge_into_snapshot`] 只在**快照是日志的严格前缀**时才采用日志的尾巴。
//! 这条判据防的是反过来的情形：日志因队列满丢了事件、或快照比日志新
//! （正常 save 之后又写了几轮但日志丢包），此时日志反而更旧，绝不能拿它覆盖。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wyj_api::types::Message;

use crate::session::{MessageEvent, MessageSink};

/// 主会话日志的字节上限（默认 8 MB）。
///
/// 比子 Agent 的 1 MB 宽得多：主会话动辄跑几十上百轮，且它是**最后一轮**
/// 的唯一保底。超过上限后停写并留 `truncated` 标记——`.json` 仍然每轮
/// autosave，日志停写不等于历史丢失。
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 一行日志记录。与 `wyj_tools::message_log::MessageRecord` 同构，
/// 刻意不做成共享类型：core 不能依赖 tools（依赖方向是反的），
/// 而两者格式的一致性由各自的测试与 `pid` 字段保证。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    Append {
        pid: u32,
        message: Message,
    },
    UpdateLast {
        pid: u32,
        message: Message,
    },
    /// 整段替换（压缩 / context editing / rewind）。读侧从这里开始重建。
    Snapshot {
        pid: u32,
        messages: Vec<Message>,
    },
    /// 超限或异常：读侧据此把重建结果标成不完整。
    Truncated {
        pid: u32,
        reason: String,
    },
}

/// 日志文件路径：`<session_dir>/<session_id>.messages.jsonl`。
///
/// 与会话正文同目录，这样 `storage prune` 的「整组删除」能一起清掉它
/// （见 `crate::retention`），不会留下无主的旁路文件。
pub fn journal_file(session_dir: &Path, session_id: &str) -> PathBuf {
    session_dir.join(format!("{session_id}.messages.jsonl"))
}

/// 后台写手：单线程串行消费，调用方零阻塞。
pub struct SessionJournal {
    tx: tokio::sync::mpsc::Sender<(String, MessageEvent)>,
}

/// 写手队列容量。
///
/// 有界是刻意的（与子 Agent 同一理由）：每个 `Appended` 携带一份完整
/// `Message`，工具结果可达几百 KB。队列打满时丢事件比撑爆内存正确——
/// 日志是旁路，`.json` 仍是权威。
const QUEUE_CAPACITY: usize = 256;

impl SessionJournal {
    /// 在 tokio 运行时内启动写手。
    ///
    /// **必须在 runtime 里调用**：本函数用 `tokio::spawn`。CLI 的装配点都在
    /// async 上下文里；`SessionStore` 自身是同步的，所以入口做成独立函数
    /// 而不是 `SessionStore` 的方法。
    pub fn spawn(session_dir: PathBuf, max_bytes: u64) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, MessageEvent)>(QUEUE_CAPACITY);
        tokio::spawn(async move {
            if std::fs::create_dir_all(&session_dir).is_err() {
                return;
            }
            // 每个会话独立计数：TUI 会在同一进程里切换会话，新文件必须从 0
            // 开始，不能被上一个会话已经写满的字节数连坐。
            let mut written: std::collections::HashMap<String, u64> = Default::default();
            let mut stopped: std::collections::HashSet<String> = Default::default();
            while let Some((session_id, event)) = rx.recv().await {
                if session_id.is_empty() || stopped.contains(&session_id) {
                    continue;
                }
                let record = match event {
                    MessageEvent::Appended(message) => JournalRecord::Append {
                        pid: std::process::id(),
                        message,
                    },
                    MessageEvent::UpdatedLast(message) => JournalRecord::UpdateLast {
                        pid: std::process::id(),
                        message,
                    },
                    MessageEvent::Replaced(messages) => JournalRecord::Snapshot {
                        pid: std::process::id(),
                        messages,
                    },
                };
                let Ok(mut value) = serde_json::to_value(&record) else {
                    continue;
                };
                // 与 `.json` 落盘同一套脱敏：工具输出里可能带 token / 私钥。
                //
                // **必须作用在值上，不能作用在序列化后的文本上**：
                // `redact_cow` 的正则前缀含 `\\?["']?`，会拆开 JSON 的转义
                // 引号 `\"`，留下未转义的裸引号。`session_store.rs` 为此
                // 记录过一次事故（120 个 checkpoint / 133MB 永久损坏），
                // 这条路径必须共用「值级脱敏」而不是「文本级脱敏」。
                crate::secret::redact_json_value(&mut value);
                let Ok(line) = serde_json::to_string(&value) else {
                    continue;
                };
                let line_len = line.len() as u64 + 1;
                let used = written.entry(session_id.clone()).or_insert(0);
                let path = journal_file(&session_dir, &session_id);
                if *used + line_len > max_bytes {
                    let marker = JournalRecord::Truncated {
                        pid: std::process::id(),
                        reason: format!("exceeded {max_bytes} bytes"),
                    };
                    if let Ok(mut marker_line) = serde_json::to_string(&marker) {
                        marker_line.push('\n');
                        append(&path, marker_line.as_bytes());
                    }
                    stopped.insert(session_id);
                    continue;
                }
                *used += line_len;
                append(&path, line.as_bytes());
                append(&path, b"\n");
            }
        });
        Self { tx }
    }

    /// 生成**绑定到某个会话**的 `Session.message_sink`。
    ///
    /// 会话 id 随事件一起入队，而不是让写手读一个共享的「当前会话」——
    /// TUI 切换会话时队列里可能还压着旧会话的事件，按写盘时刻取值会把它们
    /// 记到新会话头上（下次 `load` 旧会话就少了几轮、新会话多了几轮。
    /// 这比丢日志糟得多）。子 Agent 的 `sink_for(id)` 用同一套做法。
    pub fn sink_for(&self, session_id: &str) -> MessageSink {
        let tx = self.tx.clone();
        let session_id = session_id.to_string();
        std::sync::Arc::new(move |event: MessageEvent| {
            if tx.try_send((session_id.clone(), event)).is_err() {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        "会话消息日志写盘跟不上，队列已满，本次及后续事件将被丢弃（权威历史仍在 session.json）"
                    );
                }
            }
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
pub struct ReplayedJournal {
    pub messages: Vec<Message>,
    /// 见到 `truncated` 标记，或存在无法解析的行（被 kill 时最后一行可能
    /// 只写了一半）。为 true 时仍可合并，但调用方不应把它当成完整历史。
    pub incomplete: bool,
}

impl SessionJournal {
    /// 装配入口：启动写手。
    ///
    /// 返回 `None`（= 不启用）的两种情形：
    ///   - `enabled == false`：用户显式关掉；
    ///   - 当前没有 tokio 运行时：`tokio::spawn` 会 panic，而 `Session` 在
    ///     同步上下文里也会被构造（测试、离线治理工具）。
    ///
    /// 调用方拿到句柄后用 `session.with_message_sink(j.sink_for(&session_id))`
    /// 接上；切换会话时换一份 `sink_for(新 id)` 即可，写手不需要重建。
    pub fn attach(enabled: bool, sessions_dir: &Path, max_bytes: u64) -> Option<Self> {
        if !enabled || tokio::runtime::Handle::try_current().is_err() {
            return None;
        }
        Some(Self::spawn(sessions_dir.to_path_buf(), max_bytes))
    }

    /// 进程内共享的写手：同一 `sessions_dir` 复用同一个实例。
    ///
    /// 存在的理由：TUI 在**四处**独立地重建 `Session`（`/clear`、`/branch`、
    /// `/new`、`/resume`），每一处都要重挂 sink。若每处各自 spawn 一个写手，
    /// 同一进程里会出现 N 个并发 append 同一个 jsonl 的 writer——行会交错、
    /// 解析失败率上升。注册表让「一个目录一个写手」成为结构性的约束，
    /// 而不是靠调用方自觉。
    ///
    /// 跨项目 resume 时 `sessions_dir` 会变，此时按目录各建一个（写手数量
    /// 与「这次运行碰过几个项目」同阶，不是与回合数同阶）。
    pub fn shared(sessions_dir: &Path, max_bytes: u64) -> Option<Arc<Self>> {
        if tokio::runtime::Handle::try_current().is_err() {
            return None;
        }
        static JOURNALS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<SessionJournal>>>,
        > = std::sync::OnceLock::new();
        let registry = JOURNALS.get_or_init(|| std::sync::Mutex::new(Default::default()));
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = guard.get(sessions_dir) {
            return Some(existing.clone());
        }
        let created = Arc::new(Self::spawn(sessions_dir.to_path_buf(), max_bytes));
        guard.insert(sessions_dir.to_path_buf(), created.clone());
        Some(created)
    }

    /// 把共享写手挂到 `session` 上（按 `session_id` 绑定）。
    ///
    /// 返回 false 表示未挂上（没有运行时）。调用方通常直接忽略返回值——
    /// 日志是旁路，挂不上不影响任何功能。
    pub fn attach_session(
        session: &mut crate::session::Session,
        sessions_dir: &Path,
        session_id: &str,
    ) -> bool {
        let Some(journal) = Self::shared(sessions_dir, DEFAULT_MAX_BYTES) else {
            return false;
        };
        session.message_sink = Some(journal.sink_for(session_id));
        true
    }
}

/// 从日志重放出对话历史。
///
/// 规则：从最后一个 `snapshot` 起算（没有则从空开始），之后 `append` 追加、
/// `update_last` 覆盖最后一条。无法解析的行跳过并置 `incomplete`。
pub fn replay(path: &Path) -> ReplayedJournal {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ReplayedJournal::default();
    };
    let mut out = ReplayedJournal::default();
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<JournalRecord>(line) else {
            if !line.trim().is_empty() {
                out.incomplete = true;
            }
            continue;
        };
        match record {
            JournalRecord::Append { message, .. } => out.messages.push(message),
            JournalRecord::UpdateLast { message, .. } => {
                if let Some(last) = out.messages.last_mut() {
                    *last = message;
                } else {
                    out.incomplete = true;
                    out.messages.push(message);
                }
            }
            JournalRecord::Snapshot { messages, .. } => out.messages = messages,
            JournalRecord::Truncated { .. } => out.incomplete = true,
        }
    }
    out
}

/// 合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// 快照已是最新，日志没有更多内容。
    Unchanged,
    /// 日志里有快照尚未包含的尾部消息，已追加。
    Recovered(usize),
    /// 日志与快照无法对齐（快照比日志新、或内容分叉），**保持快照不变**。
    Diverged,
}

/// 把日志里比快照多出来的尾部消息合并进 `snapshot`。
///
/// 两条判据缺一不可：
///   1. 日志更**长**（`journal.len() > snapshot.len()`）；
///   2. 快照是日志的**严格前缀**（逐条 `==`）。
///
/// 只满足第 1 条是不够的：日志可能因为队列满丢了中间事件，长度仍然够，
/// 但内容已经和快照分叉。那类情况下以快照为准，绝不能拿一份缺失中间轮次的
/// 日志去覆盖一份完整快照。
pub fn merge_into_snapshot(snapshot: &mut Vec<Message>, journal: &[Message]) -> MergeOutcome {
    if journal.len() <= snapshot.len() {
        return if journal == snapshot.as_slice() {
            MergeOutcome::Unchanged
        } else {
            MergeOutcome::Diverged
        };
    }
    if snapshot.as_slice() != &journal[..snapshot.len()] {
        return MergeOutcome::Diverged;
    }
    let extra = journal.len() - snapshot.len();
    snapshot.extend_from_slice(&journal[snapshot.len()..]);
    MergeOutcome::Recovered(extra)
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

    fn write_lines(path: &Path, records: &[JournalRecord]) {
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wyj-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn replay_appends_and_updates_in_order() {
        let dir = temp_dir("replay");
        let path = journal_file(&dir, "s1");
        write_lines(
            &path,
            &[
                JournalRecord::Append {
                    pid: 1,
                    message: user("任务"),
                },
                JournalRecord::Append {
                    pid: 1,
                    message: assistant("第一轮"),
                },
                JournalRecord::Append {
                    pid: 1,
                    message: user("工具结果 A"),
                },
                JournalRecord::UpdateLast {
                    pid: 1,
                    message: user("工具结果 A + B"),
                },
            ],
        );
        let replayed = replay(&path);
        assert!(!replayed.incomplete);
        assert_eq!(replayed.messages.len(), 3);
        assert_eq!(replayed.messages[2], user("工具结果 A + B"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 压缩是整段替换：没有 snapshot 锚点，重放会把压缩前后拼成一个
    /// 从未存在过的历史。
    #[test]
    fn snapshot_resets_the_replay_baseline() {
        let dir = temp_dir("snap");
        let path = journal_file(&dir, "s1");
        write_lines(
            &path,
            &[
                JournalRecord::Append {
                    pid: 1,
                    message: user("旧 1"),
                },
                JournalRecord::Append {
                    pid: 1,
                    message: user("旧 2"),
                },
                JournalRecord::Snapshot {
                    pid: 1,
                    messages: vec![user("压缩摘要")],
                },
                JournalRecord::Append {
                    pid: 1,
                    message: assistant("压缩后"),
                },
            ],
        );
        let replayed = replay(&path);
        assert_eq!(replayed.messages.len(), 2);
        assert_eq!(replayed.messages[0], user("压缩摘要"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn half_written_trailing_line_is_skipped_and_flagged() {
        let dir = temp_dir("half");
        let path = journal_file(&dir, "s1");
        std::fs::create_dir_all(&dir).unwrap();
        let valid = serde_json::to_string(&JournalRecord::Append {
            pid: 1,
            message: user("完整的一行"),
        })
        .unwrap();
        std::fs::write(&path, format!("{valid}\n{{\"type\":\"append\",\"mess")).unwrap();
        let replayed = replay(&path);
        assert_eq!(replayed.messages.len(), 1);
        assert!(replayed.incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_of_missing_file_is_empty_not_an_error() {
        let replayed = replay(Path::new("/nonexistent/s1.messages.jsonl"));
        assert!(replayed.messages.is_empty());
        assert!(!replayed.incomplete);
    }

    /// 核心场景：`.json` 停在两轮前，日志里有被 SIGKILL 打断的那一轮。
    #[test]
    fn merge_recovers_the_tail_lost_between_saves() {
        let mut snapshot = vec![user("任务"), assistant("第一轮")];
        let journal = vec![
            user("任务"),
            assistant("第一轮"),
            user("第二轮"),
            assistant("第二轮结论"),
        ];
        assert_eq!(
            merge_into_snapshot(&mut snapshot, &journal),
            MergeOutcome::Recovered(2)
        );
        assert_eq!(snapshot.len(), 4);
        assert_eq!(snapshot[3], assistant("第二轮结论"));
    }

    #[test]
    fn merge_is_a_noop_when_snapshot_is_already_latest() {
        let mut snapshot = vec![user("任务"), assistant("第一轮")];
        let journal = snapshot.clone();
        assert_eq!(
            merge_into_snapshot(&mut snapshot, &journal),
            MergeOutcome::Unchanged
        );
        assert_eq!(snapshot.len(), 2);
    }

    /// 日志丢过事件（队列满）时长度可能仍更大，但内容已分叉。
    /// 这时必须以快照为准——拿一份缺中间轮次的日志覆盖完整快照，
    /// 会让模型看到一段跳跃的历史。
    #[test]
    fn merge_refuses_when_journal_diverged_from_snapshot() {
        let mut snapshot = vec![user("任务"), assistant("第一轮")];
        let journal = vec![user("任务"), assistant("第一轮{被截断}"), user("第二轮")];
        assert_eq!(
            merge_into_snapshot(&mut snapshot, &journal),
            MergeOutcome::Diverged
        );
        assert_eq!(snapshot.len(), 2, "分叉时快照必须原样保留");
    }
}
