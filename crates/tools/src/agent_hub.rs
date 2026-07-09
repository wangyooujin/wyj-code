//! SubAgentHub — 子 Agent 生命周期管理与事件汇聚中心
//!
//! 进程级单例（`Arc` 共享）：cli 启动时创建，交给 `SubAgentTool`（spawn 任务、
//! 分配 id、上报事件）与 TUI/headless 前端（注册事件回调、中断、等待后台任务）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

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

struct RunningEntry {
    background: bool,
    handle: JoinHandle<()>,
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
    pub fn register(&self, id: u64, background: bool, handle: JoinHandle<()>) {
        self.running
            .lock()
            .unwrap()
            .insert(id, RunningEntry { background, handle });
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
        for id in &ids {
            if let Some(e) = running.remove(id) {
                e.handle.abort();
            }
        }
        ids
    }

    /// 中断全部子 Agent（进程退出前清理）。返回被中断的 id 列表。
    pub fn abort_all(&self) -> Vec<u64> {
        let mut running = self.running.lock().unwrap();
        let ids: Vec<u64> = running.keys().copied().collect();
        for (_, e) in running.drain() {
            e.handle.abort();
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
        hub.register(1, false, fg);
        hub.register(2, true, bg);
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
        hub.register(3, true, handle);
        hub.wait_background().await;
        assert_eq!(flag.load(Ordering::SeqCst), 1);
    }
}
