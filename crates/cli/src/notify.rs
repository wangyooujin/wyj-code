//! 统一通知通道（v1.5.14+）
//!
//! 覆盖 4 类事件源（TUI/CLI 回合完成 / 错误、后台子 Agent 完成、定时任务失败），
//! 通过 [`BellSink`]（stderr `\x07`）和 [`DesktopSink`]（macOS `osascript` / Linux
//! `notify-send` / Windows PowerShell BurntToast）两类 sink 通知用户。
//!
//! 设计要点：
//! - **零第三方依赖**——只用 std + 现成系统命令（`osascript` / `notify-send` / `powershell`）
//! - **fail-closed**——sink 失败 swallow，绝不返回错误给 emit 调用方
//! - **fail-safe init**——`emit` 在未 `init` 时静默 no-op，调用方永远不需要做空指针保护
//! - **rate-limit**——同类事件按 `cfg.rate_limit_seconds` 最小间隔去重，防滥用
//! - **i18n**——`title` / `body` 模板在 dispatcher 层渲染，sink 只看最终字符串
//!
//! ## 触发点清单（7 sites）
//! 1. TUI `apply_agent_event(TurnDone)` — `crates/tui/src/app.rs:7394` 之后
//! 2. TUI `apply_agent_event(Error)` — `crates/tui/src/app.rs:7416` 之后
//! 3. TUI 后台子 Agent `SubAgentDone` — `crates/tui/src/app.rs:8052` 之后
//! 4. CLI `-p` 单回合完成 — `crates/cli/src/main.rs:1905` 之前
//! 5. CLI headless REPL 回合完成 — `crates/cli/src/main.rs:3209` 之后
//! 6. Daemon ACP 回合完成 — **故意不接**（daemon 无人值守，参见 CLAUDE.md [notify]）
//! 7. cron schedule 任务失败 — `crates/cli/src/schedule_cmd.rs:267/292/299`
//!
//! ## env override
//! - `WYJ_CODE_NOTIFY_OFF=1` — master 全关（最高优先级）
//! - `WYJ_CODE_NOTIFY_BELL=0/1` — 覆盖 `bell.enabled`
//! - `WYJ_CODE_NOTIFY_DESKTOP=0/1` — 覆盖 `desktop.enabled`
//!
//! env 在 `init` 阶段读取，**绝不写回 cfg**（仿 `Config::resolve_jev_api_key` 模式）。

use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// `DISPATCHER` 选 `Mutex<Option<...>>` 而非 `OnceLock<...>` 的理由：
// 1. `OnceLock::set` 在已 set 时返 Err，测试间无法重置 cfg / sink 列表。
// 2. `Mutex<Option<...>>` 让 `init_with_sinks` 可以直接覆盖（生产路径仍 first-wins，
//    `init` 内部 `if g.is_some() { return; }` 保证）。
// 3. emit 走 `lock().unwrap().as_ref()`，与 OnceLock 性能差异可忽略（emit 频次低）。
static DISPATCHER: Mutex<Option<NotifyDispatcher>> = Mutex::new(None);

// ── 配置类型 ───────────────────────────────────────────────────────────────
//
// Step 1：定义在本文件以便 trait 形状独立验证。
// Step 2：整体迁移到 `wyj_config::NotifyCfg`，本文件改用 `wyj_config::NotifyCfg`。
//
// 字段语义见 CLAUDE.md "Storage caps (defaults)" 表格的 `[notify]` 行，
// 以及 CHANGELOG v1.5.14 条目。

/// 顶层 `[notify]` 配置块。所有字段都有合理默认；master / desktop / events.* 是
/// opt-out（默认开），`bell` 与 `include_session_id` 是 opt-in（默认关）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyCfg {
    /// 总开关。默认 `true`（opt-out），仿 `evolution.enabled=true`。
    pub enabled: bool,
    /// 终端响铃（stderr `\x07`）。默认 `false`（opt-in）—— 共享场景（图书馆/会议）易扰民。
    #[serde(default)]
    pub bell: NotifyBellCfg,
    /// 桌面通知。默认 `true`（opt-out）。
    #[serde(default)]
    pub desktop: NotifyDesktopCfg,
    /// 4 类事件的细粒度开关。
    #[serde(default)]
    pub events: NotifyEventsCfg,
    /// 同类事件最小发送间隔（秒）。`0` 关闭限流。默认 30。
    pub rate_limit_seconds: u64,
    /// 是否在通知 body 末尾追加 `[session:xxx]` 上下文。默认 `false`。
    pub include_session_id: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyBellCfg {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyDesktopCfg {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyEventsCfg {
    pub turn_finished: bool,
    pub turn_error: bool,
    pub subagent_done: bool,
    pub schedule_failure: bool,
}

impl Default for NotifyCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            bell: NotifyBellCfg::default(),  // enabled: false
            desktop: NotifyDesktopCfg::default(),  // enabled: true
            events: NotifyEventsCfg::default(),  // 全 true
            rate_limit_seconds: 30,
            include_session_id: false,
        }
    }
}

impl Default for NotifyDesktopCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for NotifyEventsCfg {
    fn default() -> Self {
        Self {
            turn_finished: true,
            turn_error: true,
            subagent_done: true,
            schedule_failure: true,
        }
    }
}

// ── 事件类型 ───────────────────────────────────────────────────────────────

/// 4 类事件的强类型枚举。事件源（7 sites）发出，dispatcher 在 `emit` 内根据
/// `kind()` 路由到对应 gate / rate-limit / sink。
#[derive(Debug, Clone)]
#[allow(dead_code)] // Step 1 仅走 ScheduleFailed；Step 3/4 接 TUI/CLI 触发点后自动消除
pub enum NotificationEvent {
    /// 主回合正常完成（CLI `-p` / REPL / TUI 回合）。
    TurnFinished {
        duration_ms: u64,
        summary: String,
        session_id: Option<String>,
    },
    /// 主回合出错。
    TurnError {
        error: String,
        session_id: Option<String>,
    },
    /// 后台子 Agent 完成（`run_in_background: true` 的 Agent 工具调用）。
    SubAgentDone { agent_id: String, summary: String },
    /// cron schedule 任务失败。
    ScheduleFailed { task_name: String, error: String },
}

impl NotificationEvent {
    pub fn kind(&self) -> NotificationKind {
        match self {
            Self::TurnFinished { .. } => NotificationKind::TurnFinished,
            Self::TurnError { .. } => NotificationKind::TurnError,
            Self::SubAgentDone { .. } => NotificationKind::SubAgentDone,
            Self::ScheduleFailed { .. } => NotificationKind::ScheduleFailed,
        }
    }

    /// 事件源标识，sink 用作上下文后缀或日志前缀。
    pub fn source(&self) -> &'static str {
        match self {
            Self::TurnFinished { .. } | Self::TurnError { .. } => "cli",
            Self::SubAgentDone { .. } => "subagent",
            Self::ScheduleFailed { .. } => "schedule",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    TurnFinished,
    TurnError,
    SubAgentDone,
    ScheduleFailed,
}

// ── Dispatcher payload ─────────────────────────────────────────────────────

/// 渲染后的事件载荷。sink 只看这个结构，不需要知道 `NotificationEvent` 的具体变体。
pub struct NotificationPayload<'a> {
    #[allow(dead_code)] // Step 3 sink logging 才用；当前仅 `title`/`body` 真读
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    #[allow(dead_code)] // 同上
    pub source: &'a str,
}

// ── Sink trait ─────────────────────────────────────────────────────────────

pub trait NotificationSink: Send + Sync {
    fn name(&self) -> &'static str;
    fn send(&self, payload: &NotificationPayload) -> Result<(), String>;
}

// ── Dispatcher ─────────────────────────────────────────────────────────────

pub struct NotifyDispatcher {
    cfg: NotifyCfg,
    sinks: Vec<Box<dyn NotificationSink>>,
    last_emit: HashMap<NotificationKind, Instant>,
}

impl NotifyDispatcher {
    fn new(cfg: NotifyCfg) -> Self {
        let mut sinks: Vec<Box<dyn NotificationSink>> = Vec::new();
        if cfg.bell.enabled {
            sinks.push(Box::new(BellSink));
        }
        if cfg.desktop.enabled {
            sinks.push(Box::new(DesktopSink));
        }
        Self {
            cfg,
            sinks,
            last_emit: HashMap::new(),
        }
    }

    /// 主开关 / 事件 gate / 限流 / 渲染 / fan-out 的完整流水线。
    fn dispatch(&mut self, event: NotificationEvent) {
        // 1. master gate
        if !self.cfg.enabled {
            return;
        }
        // 2. per-event gate
        if !self.event_gate(event.kind()) {
            return;
        }
        // 3. rate limit（同 kind 的最小间隔）
        if !self.rate_limit_ok(event.kind()) {
            return;
        }
        // 4. 渲染 payload
        let payload = self.render(event);
        // 5. fan-out；sink 失败 swallow + 一次性 debug
        for sink in &self.sinks {
            if let Err(err) = sink.send(&payload) {
                debug_sink_failure_once(sink.name(), &err);
            }
        }
    }

    fn event_gate(&self, kind: NotificationKind) -> bool {
        let events = &self.cfg.events;
        match kind {
            NotificationKind::TurnFinished => events.turn_finished,
            NotificationKind::TurnError => events.turn_error,
            NotificationKind::SubAgentDone => events.subagent_done,
            NotificationKind::ScheduleFailed => events.schedule_failure,
        }
    }

    fn rate_limit_ok(&mut self, kind: NotificationKind) -> bool {
        if self.cfg.rate_limit_seconds == 0 {
            return true;
        }
        let now = Instant::now();
        let min_gap = Duration::from_secs(self.cfg.rate_limit_seconds);
        if let Some(last) = self.last_emit.get(&kind) {
            if now.duration_since(*last) < min_gap {
                return false;
            }
        }
        self.last_emit.insert(kind, now);
        true
    }

    fn render(&self, event: NotificationEvent) -> NotificationPayload<'_> {
        let source = event.source();
        let kind = event.kind();
        match event {
            NotificationEvent::TurnFinished {
                duration_ms,
                summary,
                session_id,
            } => NotificationPayload {
                kind,
                title: title_for(kind),
                body: append_session_id(
                    format!("Duration {duration_ms}ms. {summary}"),
                    session_id,
                    self.cfg.include_session_id,
                ),
                source,
            },
            NotificationEvent::TurnError { error, session_id } => NotificationPayload {
                kind,
                title: title_for(kind),
                body: append_session_id(error, session_id, self.cfg.include_session_id),
                source,
            },
            NotificationEvent::SubAgentDone { agent_id, summary } => NotificationPayload {
                kind,
                title: title_for(kind),
                body: format!("Agent {agent_id}. {summary}"),
                source,
            },
            NotificationEvent::ScheduleFailed { task_name, error } => NotificationPayload {
                kind,
                title: title_for(kind),
                body: format!("Task {task_name}. {error}"),
                source,
            },
        }
    }
}

fn title_for(kind: NotificationKind) -> String {
    match kind {
        NotificationKind::TurnFinished => "wyj-code turn finished".to_string(),
        NotificationKind::TurnError => "wyj-code turn error".to_string(),
        NotificationKind::SubAgentDone => "wyj-code subagent finished".to_string(),
        NotificationKind::ScheduleFailed => "wyj-code schedule task failed".to_string(),
    }
}

fn append_session_id(body: String, session_id: Option<String>, enabled: bool) -> String {
    if !enabled {
        return body;
    }
    let Some(id) = session_id else { return body };
    let suffix = format!(" [session:{id}]");
    // 截断预算与正文合并：保留总长 ≤ 200 字符
    const MAX_BODY: usize = 200;
    let total = body.len() + suffix.len();
    if total <= MAX_BODY {
        format!("{body}{suffix}")
    } else {
        let cut = body.chars().take(MAX_BODY.saturating_sub(suffix.len())).collect::<String>();
        format!("{cut}{suffix}")
    }
}

/// sink 首次失败时 `tracing::debug!` 一次，用 `OnceLock<bool>` 防重复日志洪水。
fn debug_sink_failure_once(sink_name: &str, err: &str) {
    static FLAGGED: OnceLock<()> = OnceLock::new();
    if FLAGGED.set(()).is_ok() {
        tracing::debug!("notify sink {sink_name} failed (further failures suppressed): {err}");
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// 初始化全局 dispatcher。`cfg` 内含 master / 通道 / 事件 / rate-limit 配置。
/// 多次调用安全（first-wins：`DISPATCHER` 已 set 时 no-op）。
pub fn init(cfg: &NotifyCfg) {
    let mut resolved = cfg.clone();
    // env override：master OFF / BELL / DESKTOP
    if std::env::var("WYJ_CODE_NOTIFY_OFF").as_deref() == Ok("1") {
        resolved.enabled = false;
    }
    if let Ok(v) = std::env::var("WYJ_CODE_NOTIFY_BELL") {
        if v == "1" {
            resolved.bell.enabled = true;
        } else if v == "0" {
            resolved.bell.enabled = false;
        }
    }
    if let Ok(v) = std::env::var("WYJ_CODE_NOTIFY_DESKTOP") {
        if v == "1" {
            resolved.desktop.enabled = true;
        } else if v == "0" {
            resolved.desktop.enabled = false;
        }
    }
    let mut g = DISPATCHER.lock().unwrap();
    if g.is_some() {
        return; // first-wins：保护生产路径，测试走 `init_with_sinks` 强制覆盖
    }
    *g = Some(NotifyDispatcher::new(resolved));
}

/// 发送一条通知事件。`DISPATCHER` 未初始化时静默 no-op；
/// master gate / event gate / rate-limit 任一关闭时也 no-op；
/// sink 失败 swallow，主流程不受影响。
pub fn emit(event: NotificationEvent) {
    let mut g = DISPATCHER.lock().unwrap();
    if let Some(d) = g.as_mut() {
        d.dispatch(event);
    }
}

// ── 测试用入口 ─────────────────────────────────────────────────────────────

/// 仅供 `#[cfg(test)]` 使用：注入自定义 sink 列表（生产路径仍走 `init`）。
/// 直接覆盖 `DISPATCHER`，让每个测试都能换 cfg + sink 组合。
#[cfg(test)]
pub fn init_with_sinks(cfg: &NotifyCfg, sinks: Vec<Box<dyn NotificationSink>>) {
    let mut g = DISPATCHER.lock().unwrap();
    *g = Some(NotifyDispatcher {
        cfg: cfg.clone(),
        sinks,
        last_emit: HashMap::new(),
    });
}

// ── BellSink ───────────────────────────────────────────────────────────────

/// 终端响铃 sink。直接写 stderr `\x07`——raw mode 只影响 stdin line discipline，
/// stderr 不在 alt-screen 范围内，跨平台通用。
struct BellSink;

impl NotificationSink for BellSink {
    fn name(&self) -> &'static str {
        "bell"
    }

    fn send(&self, _payload: &NotificationPayload) -> Result<(), String> {
        let mut s = std::io::stderr().lock();
        s.write_all(b"\x07").map_err(|e| e.to_string())
    }
}

// ── DesktopSink ────────────────────────────────────────────────────────────

/// 桌面通知 sink。`cfg(target_os)` 分发到 macOS / Linux / Windows 实现。
struct DesktopSink;

impl NotificationSink for DesktopSink {
    fn name(&self) -> &'static str {
        "desktop"
    }

    fn send(&self, payload: &NotificationPayload) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            macos_send(payload)
        }
        #[cfg(target_os = "linux")]
        {
            linux_send(payload)
        }
        #[cfg(target_os = "windows")]
        {
            windows_send(payload)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = payload;
            Err("unsupported platform".to_string())
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_send(payload: &NotificationPayload) -> Result<(), String> {
    let script = format!(
        "display notification {} with title {}",
        applescript_quote(&payload.body),
        applescript_quote(&payload.title),
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "linux")]
fn linux_send(payload: &NotificationPayload) -> Result<(), String> {
    std::process::Command::new("notify-send")
        .arg("--app-name=wyj-code")
        .arg(&payload.title)
        .arg(&payload.body)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "windows")]
fn windows_send(payload: &NotificationPayload) -> Result<(), String> {
    let ps = format!(
        "[reflection.assembly]::loadwithpartialname('BurntToast') | Out-Null; \
         New-BurntToastNotification -Text '{}', '{}'",
        payload.title.replace('\'', "''"),
        payload.body.replace('\'', "''"),
    );
    std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(ps)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ── 单测 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// 全局 DISPATCHER 是进程级单例，cargo test 默认并行跑测试会让多个测试
    /// 互相覆盖对方的 cfg + sink，导致测试结果依赖运气。
    /// 用 TEST_LOCK 串行化所有 notify::tests::* 的执行。
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// 录制所有 send 调用的测试 sink。
    #[derive(Default, Clone)]
    struct RecordingSink {
        payloads: Arc<StdMutex<Vec<(String, String)>>>,
        fail_first_n: Arc<StdMutex<u32>>,
    }

    impl RecordingSink {
        fn new(fail_first_n: u32) -> Self {
            Self {
                payloads: Arc::new(StdMutex::new(Vec::new())),
                fail_first_n: Arc::new(StdMutex::new(fail_first_n)),
            }
        }

        fn received(&self) -> Vec<(String, String)> {
            self.payloads.lock().unwrap().clone()
        }
    }

    impl NotificationSink for RecordingSink {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn send(&self, payload: &NotificationPayload) -> Result<(), String> {
            let mut remaining = self.fail_first_n.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                return Err("synthetic failure".to_string());
            }
            self.payloads
                .lock()
                .unwrap()
                .push((payload.title.clone(), payload.body.clone()));
            Ok(())
        }
    }

    #[test]
    fn uninitialized_emit_is_noop() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 故意不调 init_with_sinks：验证 emit 在 DISPATCHER 为 None 时不 panic
        emit(NotificationEvent::TurnFinished {
            duration_ms: 1,
            summary: "x".to_string(),
            session_id: None,
        });
    }

    #[test]
    fn dispatcher_fans_out_to_multiple_sinks() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec1 = RecordingSink::new(0);
        let rec2 = RecordingSink::new(0);
        let sinks: Vec<Box<dyn NotificationSink>> = vec![
            Box::new(rec1.clone()),
            Box::new(rec2.clone()),
        ];
        init_with_sinks(&NotifyCfg::default(), sinks);
        emit(NotificationEvent::ScheduleFailed {
            task_name: "test-task".to_string(),
            error: "boom".to_string(),
        });
        assert_eq!(rec1.received().len(), 1);
        assert_eq!(rec2.received().len(), 1);
        assert!(rec1.received()[0].1.contains("test-task"));
        assert!(rec2.received()[0].1.contains("boom"));
    }

    #[test]
    fn master_off_skips_all_sinks() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec = RecordingSink::new(0);
        let sinks: Vec<Box<dyn NotificationSink>> = vec![Box::new(rec.clone())];
        let mut cfg = NotifyCfg::default();
        cfg.enabled = false;
        init_with_sinks(&cfg, sinks);
        emit(NotificationEvent::TurnFinished {
            duration_ms: 100,
            summary: "x".to_string(),
            session_id: None,
        });
        assert_eq!(rec.received().len(), 0);
    }

    #[test]
    fn event_specific_off_skips_that_event() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec = RecordingSink::new(0);
        let sinks: Vec<Box<dyn NotificationSink>> = vec![Box::new(rec.clone())];
        let mut cfg = NotifyCfg::default();
        cfg.events.subagent_done = false;
        init_with_sinks(&cfg, sinks);
        emit(NotificationEvent::SubAgentDone {
            agent_id: "a1".to_string(),
            summary: "x".to_string(),
        });
        assert_eq!(rec.received().len(), 0);
        // 但其它事件仍能到达
        emit(NotificationEvent::TurnFinished {
            duration_ms: 1,
            summary: "x".to_string(),
            session_id: None,
        });
        assert_eq!(rec.received().len(), 1);
    }

    #[test]
    fn rate_limit_blocks_close_calls() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec = RecordingSink::new(0);
        let sinks: Vec<Box<dyn NotificationSink>> = vec![Box::new(rec.clone())];
        let mut cfg = NotifyCfg::default();
        cfg.rate_limit_seconds = 60;
        init_with_sinks(&cfg, sinks);
        // 两次同 kind 在 60s 内：第二次应被吞掉
        emit(NotificationEvent::TurnFinished {
            duration_ms: 1,
            summary: "first".to_string(),
            session_id: None,
        });
        emit(NotificationEvent::TurnFinished {
            duration_ms: 2,
            summary: "second".to_string(),
            session_id: None,
        });
        assert_eq!(rec.received().len(), 1);
        assert!(rec.received()[0].1.contains("first"));
    }

    #[test]
    fn rate_limit_zero_disables_limiting() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec = RecordingSink::new(0);
        let sinks: Vec<Box<dyn NotificationSink>> = vec![Box::new(rec.clone())];
        let mut cfg = NotifyCfg::default();
        cfg.rate_limit_seconds = 0;
        init_with_sinks(&cfg, sinks);
        emit(NotificationEvent::TurnFinished {
            duration_ms: 1,
            summary: "a".to_string(),
            session_id: None,
        });
        emit(NotificationEvent::TurnFinished {
            duration_ms: 2,
            summary: "b".to_string(),
            session_id: None,
        });
        assert_eq!(rec.received().len(), 2);
    }

    #[test]
    fn sink_failure_does_not_block_other_sinks() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec_ok = RecordingSink::new(0);
        let rec_failing = RecordingSink::new(10);  // 总是失败
        let sinks: Vec<Box<dyn NotificationSink>> = vec![
            Box::new(rec_failing.clone()),
            Box::new(rec_ok.clone()),
        ];
        init_with_sinks(&NotifyCfg::default(), sinks);
        emit(NotificationEvent::TurnFinished {
            duration_ms: 1,
            summary: "x".to_string(),
            session_id: None,
        });
        // 失败的 sink 不应阻断后续 sink
        assert_eq!(rec_ok.received().len(), 1);
    }

    #[test]
    fn append_session_id_respects_flag_and_truncates() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // flag off → 不附加
        let body = "hello".to_string();
        let out = append_session_id(body.clone(), Some("abc".to_string()), false);
        assert_eq!(out, "hello");

        // flag on, 总长未超 → 附加
        let body = "x".repeat(50);
        let out = append_session_id(body.clone(), Some("abc".to_string()), true);
        assert!(out.contains(" [session:abc]"));

        // flag on, 总长超 → 截断正文保留后缀
        let body = "x".repeat(250);
        let out = append_session_id(body, Some("abc".to_string()), true);
        assert!(out.ends_with("[session:abc]"));
        assert!(out.chars().count() <= 200);
    }

    #[test]
    fn defaults_match_recommendation() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = NotifyCfg::default();
        assert!(cfg.enabled);
        assert!(!cfg.bell.enabled, "bell 应 opt-in 默认关");
        assert!(cfg.desktop.enabled, "desktop 应 opt-out 默认开");
        assert!(cfg.events.turn_finished);
        assert!(cfg.events.turn_error);
        assert!(cfg.events.subagent_done, "subagent_done 用户选择 opt-out 默认开");
        assert!(cfg.events.schedule_failure);
        assert_eq!(cfg.rate_limit_seconds, 30);
        assert!(!cfg.include_session_id);
    }
}
