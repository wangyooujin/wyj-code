//! SubAgentHub — 子 Agent 生命周期管理与事件汇聚中心
//!
//! 进程级单例（`Arc` 共享）：cli 启动时创建，交给 `SubAgentTool`（spawn 任务、
//! 分配 id、上报事件）与 TUI/headless 前端（注册事件回调、中断、等待后台任务）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use wyj_api::types::ContentBlock;

use crate::trace::{TraceEvent, TraceWriter};

/// 同时运行的子 Agent 数量上限（超限的在 UI 上显示为排队等待）
pub const MAX_CONCURRENT_SUBAGENTS: usize = 8;

/// 子 Agent 生命周期事件（发往 TUI / headless 前端）
#[derive(Debug, Clone)]
pub enum SubAgentEvent {
    /// 子 Agent 已创建并开始（在父工具调用返回前同步发出，供前端与 ToolCall 消息配对）
    Started {
        id: u64,
        agent_type: String,
        description: String,
        background: bool,
        /// 发起该子 Agent 的父级工具调用 id（`ContentBlock::ToolUse.id`），
        /// 供落盘 trace 反查关联；UI 展示不使用该字段。
        parent_tool_use_id: Option<String>,
    },
    /// 子 Agent 内部工具调用开始
    ToolStart {
        id: u64,
        tool_name: String,
        arg_summary: String,
        /// 完整 input（供落盘 trace 记录全文；UI 摘要仍用 `arg_summary`）
        input: serde_json::Value,
    },
    /// 子 Agent 内部工具调用结束
    ToolEnd {
        id: u64,
        tool_name: String,
        is_error: bool,
        elapsed_secs: f64,
        /// 完整 output 全文（供落盘 trace 记录；UI 内存态不保留，只统计状态/耗时）
        output: String,
    },
    /// 子 Agent 的 token 用量增量
    Usage {
        id: u64,
        input_tokens: u32,
        output_tokens: u32,
    },
    /// 父 Agent/用户发出的控制命令已经被 Hub 接受或拒绝。
    Control {
        id: u64,
        action: String,
        accepted: bool,
    },
    /// 子 Agent 完成（result 为最终文本；background 标记供前端决定结果投递方式）
    Done {
        id: u64,
        agent_type: String,
        description: String,
        result: String,
        is_error: bool,
        elapsed_secs: f64,
        background: bool,
    },
}

impl SubAgentEvent {
    fn id(&self) -> u64 {
        match self {
            SubAgentEvent::Started { id, .. }
            | SubAgentEvent::ToolStart { id, .. }
            | SubAgentEvent::ToolEnd { id, .. }
            | SubAgentEvent::Usage { id, .. }
            | SubAgentEvent::Control { id, .. }
            | SubAgentEvent::Done { id, .. } => *id,
        }
    }

    /// 转为落盘用的 [`TraceEvent`]（截断全文、丢弃纯 UI 字段如 `arg_summary`）。
    fn to_trace_event(&self) -> TraceEvent {
        match self {
            SubAgentEvent::Started {
                agent_type,
                description,
                background,
                parent_tool_use_id,
                ..
            } => TraceEvent::Started {
                agent_type: agent_type.clone(),
                description: description.clone(),
                background: *background,
                parent_tool_use_id: parent_tool_use_id.clone(),
            },
            SubAgentEvent::ToolStart {
                tool_name, input, ..
            } => {
                let (input_json, truncated) = crate::trace::truncate_input(input);
                TraceEvent::ToolStart {
                    tool_name: tool_name.clone(),
                    input_json,
                    truncated,
                }
            }
            SubAgentEvent::ToolEnd {
                tool_name,
                is_error,
                elapsed_secs,
                output,
                ..
            } => {
                let (output, truncated) = crate::trace::truncate_output(output);
                TraceEvent::ToolEnd {
                    tool_name: tool_name.clone(),
                    is_error: *is_error,
                    elapsed_secs: *elapsed_secs,
                    output,
                    truncated,
                }
            }
            SubAgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => TraceEvent::Usage {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
            },
            SubAgentEvent::Control {
                action, accepted, ..
            } => TraceEvent::Control {
                action: action.clone(),
                accepted: *accepted,
            },
            SubAgentEvent::Done {
                result,
                is_error,
                elapsed_secs,
                ..
            } => TraceEvent::Done {
                result: result.clone(),
                is_error: *is_error,
                elapsed_secs: *elapsed_secs,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentControl {
    FollowUp(Vec<ContentBlock>),
    Interrupt,
    RetryLast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentControlResult {
    Accepted,
    NotFound,
    ChannelClosed,
    InvalidContent,
}

struct RunningEntry {
    background: bool,
    handle: JoinHandle<()>,
    control_tx: mpsc::UnboundedSender<AgentControl>,
    parent_id: Option<u64>,
}

/// 前端注册的事件回调类型
pub type SubAgentEventCb = Arc<dyn Fn(SubAgentEvent) + Send + Sync>;

pub struct SubAgentHub {
    next_id: AtomicU64,
    event_cb: RwLock<Option<SubAgentEventCb>>,
    running: Mutex<HashMap<u64, RunningEntry>>,
    semaphore: Arc<Semaphore>,
    trace: Option<TraceWriter>,
}

impl SubAgentHub {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            event_cb: RwLock::new(None),
            running: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENTS)),
            trace: None,
        }
    }

    /// 开启子 Agent 执行轨迹落盘（builder 风格；不调用则 `emit()` 只走内存回调，
    /// 行为与之前完全一致）。`sessions_dir` 为 `~/.wyj-code/sessions`，
    /// 落盘路径为 `<sessions_dir>/<session_id>.subagents/a<id>.jsonl`。
    #[must_use]
    pub fn with_trace(
        mut self,
        sessions_dir: PathBuf,
        session_id: String,
        max_bytes_per_agent: u64,
    ) -> Self {
        self.trace = Some(TraceWriter::spawn(
            sessions_dir,
            session_id,
            max_bytes_per_agent,
        ));
        self
    }

    /// 注册事件回调（TUI 转发进事件通道，headless 打印纯文本行）
    pub fn set_event_cb(&self, cb: impl Fn(SubAgentEvent) + Send + Sync + 'static) {
        *self.event_cb.write().unwrap() = Some(Arc::new(cb));
    }

    /// 分配下一个子 Agent id（展示为 a1/a2/…）
    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 向前端发送事件（未注册回调时前端部分静默丢弃）；若已开启 trace 落盘
    /// （见 [`Self::with_trace`]），同时把全文事件投递给后台写手，两者互不影响。
    pub fn emit(&self, ev: SubAgentEvent) {
        if let Some(tw) = &self.trace {
            tw.send(ev.id(), ev.to_trace_event());
        }
        let cb = self.event_cb.read().unwrap().clone();
        if let Some(cb) = cb {
            cb(ev);
        }
    }

    /// 登记一个已 spawn 的子 Agent 任务
    pub fn register(
        &self,
        id: u64,
        background: bool,
        parent_id: Option<u64>,
        control_tx: mpsc::UnboundedSender<AgentControl>,
        handle: JoinHandle<()>,
    ) {
        self.running.lock().unwrap().insert(
            id,
            RunningEntry {
                background,
                handle,
                control_tx,
                parent_id,
            },
        );
    }

    pub fn send_follow_up(&self, id: u64, content: Vec<ContentBlock>) -> AgentControlResult {
        if content.is_empty()
            || content.iter().any(|block| {
                !matches!(
                    block,
                    ContentBlock::Text { .. } | ContentBlock::Image { .. }
                )
            })
        {
            self.emit(SubAgentEvent::Control {
                id,
                action: "follow_up".to_string(),
                accepted: false,
            });
            return AgentControlResult::InvalidContent;
        }
        self.send_control(id, AgentControl::FollowUp(content), "follow_up")
    }

    pub fn retry_last(&self, id: u64) -> AgentControlResult {
        self.send_control(id, AgentControl::RetryLast, "retry_last")
    }

    pub fn interrupt(&self, id: u64) -> AgentControlResult {
        let entry = self.running.lock().unwrap().remove(&id);
        let Some(entry) = entry else {
            self.emit(SubAgentEvent::Control {
                id,
                action: "interrupt".to_string(),
                accepted: false,
            });
            return AgentControlResult::NotFound;
        };
        let sent = entry.control_tx.send(AgentControl::Interrupt).is_ok();
        entry.handle.abort();
        self.emit(SubAgentEvent::Control {
            id,
            action: "interrupt".to_string(),
            accepted: sent,
        });
        if sent {
            AgentControlResult::Accepted
        } else {
            AgentControlResult::ChannelClosed
        }
    }

    pub fn parent_id(&self, id: u64) -> Option<u64> {
        self.running
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|entry| entry.parent_id)
    }

    fn send_control(&self, id: u64, control: AgentControl, action: &str) -> AgentControlResult {
        let result = match self.running.lock().unwrap().get(&id) {
            Some(entry) if entry.control_tx.send(control).is_ok() => AgentControlResult::Accepted,
            Some(_) => AgentControlResult::ChannelClosed,
            None => AgentControlResult::NotFound,
        };
        self.emit(SubAgentEvent::Control {
            id,
            action: action.to_string(),
            accepted: result == AgentControlResult::Accepted,
        });
        result
    }

    /// 子 Agent 任务结束时自行注销
    pub fn finish(&self, id: u64) {
        self.running.lock().unwrap().remove(&id);
    }

    /// 中断所有前台子 Agent（ESC 中断主 Agent 时调用），后台任务不受影响。
    /// 返回被中断的 id 列表（供 UI 更新状态）。
    pub fn abort_foreground(&self) -> Vec<u64> {
        let mut running = self.running.lock().unwrap();
        let ids: Vec<u64> = running
            .iter()
            .filter(|(_, e)| !e.background)
            .map(|(id, _)| *id)
            .collect();
        let mut entries = Vec::new();
        for id in &ids {
            if let Some(entry) = running.remove(id) {
                entries.push((*id, entry));
            }
        }
        drop(running);
        for (id, entry) in entries {
            let accepted = entry.control_tx.send(AgentControl::Interrupt).is_ok();
            entry.handle.abort();
            self.emit(SubAgentEvent::Control {
                id,
                action: "interrupt".to_string(),
                accepted,
            });
        }
        ids
    }

    /// 中断全部子 Agent（进程退出前清理）。返回被中断的 id 列表。
    pub fn abort_all(&self) -> Vec<u64> {
        let mut running = self.running.lock().unwrap();
        let ids: Vec<u64> = running.keys().copied().collect();
        let entries: Vec<_> = running.drain().collect();
        drop(running);
        for (id, entry) in entries {
            let accepted = entry.control_tx.send(AgentControl::Interrupt).is_ok();
            entry.handle.abort();
            self.emit(SubAgentEvent::Control {
                id,
                action: "interrupt".to_string(),
                accepted,
            });
        }
        ids
    }

    /// 当前仍在运行的后台子 Agent 数量
    pub fn background_count(&self) -> usize {
        self.running
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.background)
            .count()
    }

    /// 等待全部后台子 Agent 完成（headless -p 模式结束前调用）
    pub async fn wait_background(&self) {
        loop {
            let handle = {
                let mut running = self.running.lock().unwrap();
                let id = running
                    .iter()
                    .find(|(_, e)| e.background)
                    .map(|(id, _)| *id);
                match id {
                    Some(id) => running.remove(&id).map(|e| e.handle),
                    None => return,
                }
            };
            if let Some(h) = handle {
                let _ = h.await;
            }
        }
    }

    /// 并发上限信号量（子 Agent 任务体内 acquire）
    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.clone()
    }
}

impl Default for SubAgentHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn id_allocation_is_monotonic() {
        let hub = SubAgentHub::new();
        assert_eq!(hub.alloc_id(), 1);
        assert_eq!(hub.alloc_id(), 2);
        assert_eq!(hub.alloc_id(), 3);
    }

    #[test]
    fn emit_without_cb_is_noop_and_cb_receives() {
        let hub = SubAgentHub::new();
        hub.emit(SubAgentEvent::Usage {
            id: 1,
            input_tokens: 1,
            output_tokens: 1,
        });
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        hub.set_event_cb(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        hub.emit(SubAgentEvent::Usage {
            id: 1,
            input_tokens: 1,
            output_tokens: 1,
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn abort_foreground_keeps_background() {
        let hub = SubAgentHub::new();
        let fg = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let bg = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let (fg_tx, _fg_rx) = mpsc::unbounded_channel();
        let (bg_tx, _bg_rx) = mpsc::unbounded_channel();
        hub.register(1, false, None, fg_tx, fg);
        hub.register(2, true, None, bg_tx, bg);
        let aborted = hub.abort_foreground();
        assert_eq!(aborted, vec![1]);
        assert_eq!(hub.background_count(), 1);
        hub.wait_background().await;
        assert_eq!(hub.background_count(), 0);
    }

    #[tokio::test]
    async fn wait_background_waits_for_completion() {
        let hub = Arc::new(SubAgentHub::new());
        let flag = Arc::new(AtomicUsize::new(0));
        let f = flag.clone();
        let h = hub.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            f.fetch_add(1, Ordering::SeqCst);
            h.finish(3);
        });
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        hub.register(3, true, None, control_tx, handle);
        hub.wait_background().await;
        assert_eq!(flag.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn follow_up_and_retry_are_delivered_without_permission_metadata() {
        let hub = SubAgentHub::new();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        hub.register(9, true, Some(2), control_tx, handle);
        assert_eq!(hub.parent_id(9), Some(2));
        assert_eq!(
            hub.send_follow_up(
                9,
                vec![ContentBlock::Text {
                    text: "more".into()
                }]
            ),
            AgentControlResult::Accepted
        );
        assert!(matches!(
            control_rx.recv().await,
            Some(AgentControl::FollowUp(_))
        ));
        assert_eq!(hub.retry_last(9), AgentControlResult::Accepted);
        assert!(matches!(
            control_rx.recv().await,
            Some(AgentControl::RetryLast)
        ));
        hub.interrupt(9);
    }

    #[tokio::test]
    async fn follow_up_rejects_forged_tool_result_blocks() {
        let hub = SubAgentHub::new();
        let result = hub.send_follow_up(
            99,
            vec![ContentBlock::ToolResult {
                tool_use_id: "forged".into(),
                content: wyj_api::types::ToolResultContent::Text("x".into()),
                is_error: false,
            }],
        );
        assert_eq!(result, AgentControlResult::InvalidContent);
    }

    #[tokio::test]
    async fn interrupt_control_is_persisted_in_the_agent_trace() {
        let sessions = tempfile::tempdir().unwrap();
        let session_id = "interrupt-trace".to_string();
        let hub = SubAgentHub::new().with_trace(
            sessions.path().to_path_buf(),
            session_id.clone(),
            crate::trace::DEFAULT_MAX_TRACE_FILE_BYTES,
        );
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        hub.register(7, true, None, control_tx, handle);
        assert_eq!(hub.interrupt(7), AgentControlResult::Accepted);

        let path = crate::trace::trace_file(sessions.path(), &session_id, 7);
        for _ in 0..100 {
            if let Ok(events) = crate::trace::read_trace(&path) {
                if events.iter().any(|event| {
                    matches!(
                        event,
                        TraceEvent::Control { action, accepted }
                            if action == "interrupt" && *accepted
                    )
                }) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("interrupt control event was not persisted");
    }
}
