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
        /// Parent agent id for nested spawns. Root sub-agents use `None`.
        parent_id: Option<u64>,
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
    /// 子 Agent 每轮结束交出的 assistant 文本（只落盘，不驱动 UI）。
    ///
    /// 与 `ToolStart`/`ToolEnd` 互补：那两个记的是「做了什么」，这个记的是
    /// 「由此得出了什么」。中断后交接任务时，结论不可再生，工具 I/O 可再生。
    Assistant { id: u64, text: String },
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
        /// 任务未完成时，**已经渲染好的模型侧交接外壳**（含三选一措辞与工具
        /// 摘要）；完成时为 `None`。
        ///
        /// 为什么由工具侧渲染而不是前端各写一份：前台走 `ToolResult`、后台走
        /// `system-reminder`，两条路径必须给出**逐字相同**的交接语义，否则同一
        /// 个中断在 TUI 与 headless 下会给模型两种不同的暗示。
        handoff_body: Option<String>,
    },
}

impl SubAgentEvent {
    fn id(&self) -> u64 {
        match self {
            SubAgentEvent::Started { id, .. }
            | SubAgentEvent::ToolStart { id, .. }
            | SubAgentEvent::ToolEnd { id, .. }
            | SubAgentEvent::Usage { id, .. }
            | SubAgentEvent::Assistant { id, .. }
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
            SubAgentEvent::Assistant { text, .. } => TraceEvent::Assistant {
                // 单轮文本按 assistant 文本上限截断（与持久化口径一致），
                // 避免一段超长输出把整份 trace 挤爆——trace 面向交接与审计，
                // 不需要逐字全文。
                text: crate::textutil::truncate_head_tail(
                    text,
                    crate::trace::ASSISTANT_HEAD_BYTES,
                    0,
                ),
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
    PeerMessage {
        from_id: u64,
        content: Vec<ContentBlock>,
    },
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
    /// 运行期进度快照。`interrupt()` 会 abort 任务，收尾代码不再执行，所以
    /// 中断时的交接物只能从这里取——见 [`crate::handoff::HandoffProgress`]。
    progress: Arc<Mutex<crate::handoff::HandoffProgress>>,
    /// 交接物落盘所需的元信息（中断路径要自己拼一份）。
    meta: HandoffMeta,
}

/// 中断路径拼交接物所需的、任务外就能拿到的字段。
#[derive(Debug, Clone, Default)]
pub struct HandoffMeta {
    pub agent_type: String,
    pub description: String,
    pub background: bool,
    pub sessions_dir: Option<PathBuf>,
    pub session_id: Option<String>,
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

    /// 覆盖并发上限（来自 `[subagent].max_concurrent`，默认 `MAX_CONCURRENT_SUBAGENTS`）。
    /// 超限的 spawn 在信号量上排队，不会失败。
    #[must_use]
    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.semaphore = Arc::new(Semaphore::new(max_concurrent.max(1)));
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
        self.register_with_handoff(
            id,
            background,
            parent_id,
            control_tx,
            handle,
            Arc::new(Mutex::new(crate::handoff::HandoffProgress::default())),
            HandoffMeta {
                agent_type: String::new(),
                description: String::new(),
                background,
                sessions_dir: None,
                session_id: None,
            },
        );
    }

    /// [`Self::register`] 的完整版本：额外登记进度快照与交接元信息。
    ///
    /// 中断路径（`interrupt` / `abort_foreground` / `abort_all`）靠这两样在
    /// **任务被 abort 之后**仍然能落一份交接物——abort 会取消 future，任务自己
    /// 的收尾代码一行都不会执行，而 ESC 恰恰是最需要「做到哪了」的场景。
    #[allow(clippy::too_many_arguments)]
    pub fn register_with_handoff(
        &self,
        id: u64,
        background: bool,
        parent_id: Option<u64>,
        control_tx: mpsc::UnboundedSender<AgentControl>,
        handle: JoinHandle<()>,
        progress: Arc<Mutex<crate::handoff::HandoffProgress>>,
        meta: HandoffMeta,
    ) {
        self.running.lock().unwrap().insert(
            id,
            RunningEntry {
                background,
                handle,
                control_tx,
                parent_id,
                progress,
                meta,
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

    /// Send a message between running agents in the same parent/child tree.  The message is
    /// consumed at the receiver's next safe model boundary, exactly like a human follow-up.
    pub fn send_peer_message(
        &self,
        from_id: u64,
        to_id: u64,
        content: Vec<ContentBlock>,
    ) -> AgentControlResult {
        if from_id == to_id || !valid_message_content(&content) {
            return AgentControlResult::InvalidContent;
        }
        let running = self.running.lock().unwrap();
        if !running.contains_key(&from_id) || !running.contains_key(&to_id) {
            drop(running);
            return AgentControlResult::NotFound;
        }
        if root_id(&running, from_id) != root_id(&running, to_id) {
            drop(running);
            self.emit(SubAgentEvent::Control {
                id: to_id,
                action: format!("message_from_a{from_id}"),
                accepted: false,
            });
            return AgentControlResult::InvalidContent;
        }
        let sent = running[&to_id]
            .control_tx
            .send(AgentControl::PeerMessage { from_id, content })
            .is_ok();
        drop(running);
        self.emit(SubAgentEvent::Control {
            id: to_id,
            action: format!("message_from_a{from_id}"),
            accepted: sent,
        });
        if sent {
            AgentControlResult::Accepted
        } else {
            AgentControlResult::ChannelClosed
        }
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
        // abort 已生效，任务自己的收尾不会再跑——交接物只能在这里补。
        write_interrupted_handoff(id, &entry);
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
            write_interrupted_handoff(id, &entry);
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

/// 任务被 abort 之后补写一份中断交接物。
///
/// 为什么必须在 Hub 侧做：`JoinHandle::abort()` 会取消 future，子 Agent 任务
/// 自己那份「收尾 → 写交接物」的代码一行都不会执行。而 ESC 是最常触发中断、
/// 也最需要「做到哪了」的场景——不在这里补，交接物恰好只缺最该有的那一种。
///
/// 数据来自运行期持续更新的 [`HandoffProgress`]（不同于正常结束时的
/// `Session` 全量），所以即使中途被杀也有内容。`sessions_dir` 未配置时静默
/// 跳过（测试与未接文件系统的调用方）。
fn write_interrupted_handoff(id: u64, entry: &RunningEntry) {
    let (Some(dir), Some(session_id)) = (&entry.meta.sessions_dir, &entry.meta.session_id) else {
        return;
    };
    let progress = entry
        .progress
        .lock()
        .map(|p| crate::handoff::HandoffProgress {
            conclusion: p.conclusion.clone(),
            digest: p.digest.clone(),
        })
        .unwrap_or_default();
    let handoff = crate::handoff::SubAgentHandoff {
        version: crate::handoff::HANDOFF_VERSION,
        id,
        agent_type: entry.meta.agent_type.clone(),
        description: entry.meta.description.clone(),
        background: entry.meta.background,
        status: crate::handoff::HandoffStatus::Interrupted,
        reason: Some("interrupted by user".to_string()),
        conclusion: crate::textutil::truncate_str(
            &progress.conclusion,
            crate::handoff::CONCLUSION_MAX_BYTES,
        )
        .to_string(),
        digest: progress.digest,
        elapsed_secs: 0.0,
    };
    if let Err(err) = crate::handoff::write_handoff(dir, session_id, &handoff) {
        tracing::warn!(id, error = %err, "中断交接物落盘失败（不影响中断本身）");
    }
}

fn valid_message_content(content: &[ContentBlock]) -> bool {
    !content.is_empty()
        && content.iter().all(|block| {
            matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::Image { .. }
            )
        })
}

fn root_id(running: &HashMap<u64, RunningEntry>, mut id: u64) -> u64 {
    let mut seen = std::collections::HashSet::new();
    while seen.insert(id) {
        match running.get(&id).and_then(|entry| entry.parent_id) {
            Some(parent) => id = parent,
            None => break,
        }
    }
    id
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

    /// 中断交接物：证明 abort 之后 Hub 仍能写出「做到哪了」。
    ///
    /// 这是整个交接方案的最后一环——子 Agent 任务的收尾代码在 abort 后不会
    /// 执行，所以进度必须由 Hub 侧从运行期快照里补写。没有这条，ESC 中断
    /// （最需要交接的场景）恰好是唯一没有交接记录的场景。
    #[tokio::test]
    async fn interrupt_writes_a_handoff_from_the_live_progress_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SubAgentHub::new();
        let progress = Arc::new(Mutex::new(crate::handoff::HandoffProgress::default()));
        {
            let mut p = progress.lock().unwrap();
            p.record_turn_text("已定位到 middleware/auth.rs:88");
            p.record_tool_start("Read", &serde_json::json!({"file_path": "auth.rs"}));
        }
        let handle = tokio::spawn(std::future::pending::<()>());
        let (tx, _rx) = mpsc::unbounded_channel();
        hub.register_with_handoff(
            4,
            false,
            None,
            tx,
            handle,
            progress,
            HandoffMeta {
                agent_type: "general-purpose".to_string(),
                description: "调查鉴权".to_string(),
                background: false,
                sessions_dir: Some(dir.path().to_path_buf()),
                session_id: Some("s1".to_string()),
            },
        );

        assert_eq!(hub.interrupt(4), AgentControlResult::Accepted);

        let handoff = crate::handoff::read_handoff(dir.path(), "s1", 4)
            .expect("abort 之后必须仍有中断交接物");
        assert_eq!(handoff.status, crate::handoff::HandoffStatus::Interrupted);
        assert_eq!(handoff.conclusion, "已定位到 middleware/auth.rs:88");
        assert_eq!(handoff.digest.files_read, vec!["auth.rs".to_string()]);
    }

    /// 未配置 `sessions_dir` 时中断不落盘、也不 panic（测试与 headless 兼容）。
    #[tokio::test]
    async fn interrupt_without_sessions_dir_is_a_noop_for_handoff() {
        let hub = SubAgentHub::new();
        let handle = tokio::spawn(std::future::pending::<()>());
        let (tx, _rx) = mpsc::unbounded_channel();
        hub.register(5, false, None, tx, handle);
        assert_eq!(hub.interrupt(5), AgentControlResult::Accepted);
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
    async fn peer_messages_are_delivered_within_one_agent_tree() {
        let hub = SubAgentHub::new();
        let (root_tx, _root_rx) = mpsc::unbounded_channel();
        let (left_tx, _left_rx) = mpsc::unbounded_channel();
        let (right_tx, mut right_rx) = mpsc::unbounded_channel();
        hub.register(1, true, None, root_tx, tokio::spawn(std::future::pending()));
        hub.register(
            2,
            true,
            Some(1),
            left_tx,
            tokio::spawn(std::future::pending()),
        );
        hub.register(
            3,
            true,
            Some(1),
            right_tx,
            tokio::spawn(std::future::pending()),
        );
        assert_eq!(
            hub.send_peer_message(
                2,
                3,
                vec![ContentBlock::Text {
                    text: "review this".to_string(),
                }],
            ),
            AgentControlResult::Accepted
        );
        assert!(matches!(
            right_rx.recv().await,
            Some(AgentControl::PeerMessage { from_id: 2, .. })
        ));
        hub.abort_all();
    }

    #[tokio::test]
    async fn peer_messages_are_rejected_across_agent_trees() {
        let hub = SubAgentHub::new();
        let (first_tx, _first_rx) = mpsc::unbounded_channel();
        let (second_tx, _second_rx) = mpsc::unbounded_channel();
        hub.register(
            1,
            true,
            None,
            first_tx,
            tokio::spawn(std::future::pending()),
        );
        hub.register(
            2,
            true,
            None,
            second_tx,
            tokio::spawn(std::future::pending()),
        );
        assert_eq!(
            hub.send_peer_message(
                1,
                2,
                vec![ContentBlock::Text {
                    text: "cross-tree".to_string(),
                }],
            ),
            AgentControlResult::InvalidContent
        );
        hub.abort_all();
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
