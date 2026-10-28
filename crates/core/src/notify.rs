//! 统一通知通道（v0.2.23+）
//!
//! 覆盖 3 类事件源（TUI/CLI 回合完成 / 错误、后台子 Agent 完成），
//! 通过 [`BellSink`]（stderr `\x07`）和 [`DesktopSink`]（终端原生 OSC 通知，
//! 回落到 macOS `osascript` / Linux `notify-send` / Windows PowerShell BurntToast）
//! 两类 sink 通知用户。
//!
//! 设计要点：
//! - **零第三方依赖**——只用 std + 现成系统命令（`osascript` / `notify-send` / `powershell`）
//! - **fail-closed**——sink 失败 swallow，绝不返回错误给 emit 调用方
//! - **fail-safe init**——`emit` 在未 `init` 时静默 no-op，调用方永远不需要做空指针保护
//! - **rate-limit**——同类事件按 `cfg.rate_limit_seconds` 最小间隔去重，防滥用
//! - **i18n**——`title` / `body` 模板在 dispatcher 层渲染，sink 只看最终字符串
//!
//! ## 桌面通知为什么优先走终端 OSC（v0.3.24+）
//!
//! 「点通知回到 wyj-code 所在终端」只有一个实现方式：**让终端 App 自己当投递者**。
//! 终端原生 OSC 通知（`OSC 9` / `OSC 777;notify`）满足这一点，点击横幅由终端激活
//! 自己的窗口并切到发起通知的标签页。
//!
//! 系统命令路径做不到：`osascript -e 'display notification'` 的投递者是
//! `/usr/bin/osascript`，它的内嵌 Info.plist 声明 `CFBundleIdentifier =
//! com.apple.osascript`，但磁盘上**没有任何 .app 声明这个 id**（`mdfind` 空、
//! `path to application id "com.apple.osascript"` 报 `-1728`）。点击时通知中心要
//! activate 投递者，LaunchServices 解析不到，于是退化成「选择应用/文件」的 open
//! 面板——用户点了通知却被迫挑文件。改命令行参数救不了：即便解析得到，被激活的
//! 也是 `osascript`，不是用户所在的终端。
//!
//! 因此 `desktop.prefer_terminal`（默认 true）先试终端 OSC，终端不支持 / 在
//! tmux·Zellij 内 / 无 tty 时**静默回落**系统命令——此时通知仍会弹出，只是点击
//! 不再跳转。
//!
//! ### ⚠️ 终端通知权限被拒时，通知会**整条消失**
//!
//! OSC 通道走的是终端 App 自己的通知权限。macOS 上若该权限被拒，终端会在内部
//! 把它丢掉，**wyj-code 侧完全无法探测**——`/dev/tty` 写入成功，`terminal_notify`
//! 返回 `Ok`，于是不回落、也不报错。系统日志里的特征是：
//!
//! ```text
//! ghostty: Error while requesting notification authorization:
//!   Error Domain=UNErrorDomain Code=1 "Notifications are not allowed for this application"
//! ```
//!
//! 排查命令：
//! `log show --last 60s --predicate 'subsystem CONTAINS[c] "ghostty"'`
//!
//! 修复在用户侧：系统设置 → 通知 → 该终端 → 允许通知。确实不想开、又需要通知
//! 时把 `[notify.desktop] prefer_terminal` 设为 `false` 退回系统命令——此时通知
//! 能弹出，但点击又会回到「选择应用」面板（见上）。两者不可兼得，OSC 是唯一
//! 同时满足「能弹出」与「点击回到终端」的路径。
//!
//! ## 触发点清单（6 sites）
//! 1. TUI `apply_agent_event(TurnDone)` — `crates/tui/src/app.rs:7394` 之后
//! 2. TUI `apply_agent_event(Error)` — `crates/tui/src/app.rs:7416` 之后
//! 3. TUI 后台子 Agent `SubAgentDone` — `crates/tui/src/app.rs:8052` 之后
//! 4. CLI `-p` 单回合完成 — `crates/cli/src/main.rs` 内 `RuntimeCommand::Prompt` 路径
//! 5. CLI headless REPL 回合完成 — `crates/cli/src/main.rs` 内 `--headless` 循环
//! 6. Daemon ACP 回合完成 — **故意不接**（daemon 无人值守，参见 CLAUDE.md [notify]）
//!
//! ## env override
//! - `WYJ_CODE_NOTIFY_OFF=1` — master 全关（最高优先级）
//! - `WYJ_CODE_NOTIFY_BELL=0/1` — 覆盖 `bell.enabled`
//! - `WYJ_CODE_NOTIFY_DESKTOP=0/1` — 覆盖 `desktop.enabled`
//!
//! env 在 `init` 阶段读取，**绝不写回 cfg**（仿 `Config::runtime_api_key` 的 env 不回写语义）。

use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// cfg 类型从 config crate 导入；`NotifyCfg` 是 crate 内主要使用类型所以 re-export。
pub use wyj_config::NotifyCfg;

// `DISPATCHER` 选 `Mutex<Option<...>>` 而非 `OnceLock<...>` 的理由：
// 1. `OnceLock::set` 在已 set 时返 Err，测试间无法重置 cfg / sink 列表。
// 2. `Mutex<Option<...>>` 让 `init_with_sinks` 可以直接覆盖（生产路径仍 first-wins，
//    `init` 内部 `if g.is_some() { return; }` 保证）。
// 3. emit 走 `lock().unwrap().as_ref()`，与 OnceLock 性能差异可忽略（emit 频次低）。
static DISPATCHER: Mutex<Option<NotifyDispatcher>> = Mutex::new(None);

// ── 配置类型 ───────────────────────────────────────────────────────────────
//
// cfg 类型（`NotifyCfg` / `NotifyBellCfg` / `NotifyDesktopCfg` / `NotifyEventsCfg`）
// 定义在 `wyj_config` crate；本文件通过 `pub use` re-export 保持向后兼容。
// 字段语义见 CLAUDE.md `[notify]` 段 + CHANGELOG v0.2.23 条目。

// ── 事件类型 ───────────────────────────────────────────────────────────────

/// 3 类事件的强类型枚举。事件源（6 sites）发出，dispatcher 在 `emit` 内根据
/// `kind()` 路由到对应 gate / rate-limit / sink。
#[derive(Debug, Clone)]
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
}

impl NotificationEvent {
    pub fn kind(&self) -> NotificationKind {
        match self {
            Self::TurnFinished { .. } => NotificationKind::TurnFinished,
            Self::TurnError { .. } => NotificationKind::TurnError,
            Self::SubAgentDone { .. } => NotificationKind::SubAgentDone,
        }
    }

    /// 事件源标识，sink 用作上下文后缀或日志前缀。
    pub fn source(&self) -> &'static str {
        match self {
            Self::TurnFinished { .. } | Self::TurnError { .. } => "cli",
            Self::SubAgentDone { .. } => "subagent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    TurnFinished,
    TurnError,
    SubAgentDone,
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
            sinks.push(Box::new(DesktopSink {
                prefer_terminal: cfg.desktop.prefer_terminal,
            }));
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
            } => {
                let duration_str = duration_ms.to_string();
                let body = wyj_i18n::tr_fmt(
                    "notify.body.turn_finished",
                    &[("duration_ms", &duration_str), ("summary", &summary)],
                );
                NotificationPayload {
                    kind,
                    title: title_for(kind),
                    body: append_session_id(body, session_id, self.cfg.include_session_id),
                    source,
                }
            }
            NotificationEvent::TurnError { error, session_id } => {
                let body = wyj_i18n::tr_fmt("notify.body.turn_error", &[("error", &error)]);
                NotificationPayload {
                    kind,
                    title: title_for(kind),
                    body: append_session_id(body, session_id, self.cfg.include_session_id),
                    source,
                }
            }
            NotificationEvent::SubAgentDone { agent_id, summary } => {
                let body = wyj_i18n::tr_fmt(
                    "notify.body.subagent_done",
                    &[("agent_id", &agent_id), ("summary", &summary)],
                );
                NotificationPayload {
                    kind,
                    title: title_for(kind),
                    body,
                    source,
                }
            }
        }
    }
}

fn title_for(kind: NotificationKind) -> String {
    wyj_i18n::tr(match kind {
        NotificationKind::TurnFinished => "notify.title.turn_finished",
        NotificationKind::TurnError => "notify.title.turn_error",
        NotificationKind::SubAgentDone => "notify.title.subagent_done",
    })
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
        let cut = body
            .chars()
            .take(MAX_BODY.saturating_sub(suffix.len()))
            .collect::<String>();
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

/// 桌面通知 sink。
///
/// 两条投递路径，**先试终端原生、再回落系统命令**：
///
/// 1. **终端自带的 OSC 通知**（`OSC 9` / `OSC 777;notify`）——发送者是终端 App
///    自己，点击横幅会激活该终端窗口并切到发起通知的标签页/面板。这是「点击
///    通知回到 wyj-code 所在终端」唯一的实现方式。
/// 2. **系统命令**（macOS `osascript` / Linux `notify-send` / Windows BurntToast）
///    ——发送者是一个子进程，与用户所在的终端窗口没有任何身份关联。
///
/// **为什么 macOS 上必须优先走第 1 条**：`osascript -e 'display notification'`
/// 的投递者是 `/usr/bin/osascript`。它的内嵌 Info.plist 声明了
/// `CFBundleIdentifier = com.apple.osascript`，但磁盘上不存在任何声明该 id 的
/// .app（`mdfind` 为空，`path to application id "com.apple.osascript"` 报
/// `-1728`）。点击通知时通知中心要激活投递者，LaunchServices 解析不到目标，
/// 于是退化成一个「选择应用/文件」的 open 面板——现场表现就是用户点了通知却
/// 被要求挑一个文件。改命令行参数救不了：即便解析得到，被激活的也是
/// `osascript` 而不是 wyj-code 所在的终端。
struct DesktopSink {
    prefer_terminal: bool,
}

impl NotificationSink for DesktopSink {
    fn name(&self) -> &'static str {
        "desktop"
    }

    fn send(&self, payload: &NotificationPayload) -> Result<(), String> {
        if self.prefer_terminal {
            match terminal_notify(payload) {
                Ok(()) => return Ok(()),
                // 终端不支持 / 无 tty：静默回落，首次记一次 debug（每次 emit 都
                // 记会让 debug 日志被这条淹没）。
                Err(reason) => note_terminal_fallback_once(&reason),
            }
        }
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

/// 终端原生通知的两种线格式。选哪种取决于终端**文档化**支持哪条，而不是
/// 「哪条更全」——发出去终端不认就是静默丢通知，比缺个标题更糟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalNotify {
    /// `OSC 9 ; <text> BEL`（iTerm2 起头，Ghostty 官方文档化的形态）。只有正文，
    /// 无独立标题，因此调用方把标题拼进正文。
    Osc9,
    /// `OSC 777 ; notify ; <title> ; <body> BEL`（rxvt-unicode 起头，WezTerm 官方
    /// 文档化的形态）。带独立标题。
    Osc777,
    /// 终端没有文档化的通知通道，回落系统命令。
    Unsupported,
}

/// 终端能力判定。`in_multiplexer` 为真时一律回落：OSC 要穿过 tmux / Zellij 得
/// 显式开启 passthrough，未开启时序列会被复用器吞掉（静默丢通知），而在此处
/// 探测「对方有没有配 passthrough」没有可靠手段。
fn detect_terminal_notify(term_program: Option<&str>, in_multiplexer: bool) -> TerminalNotify {
    if in_multiplexer {
        return TerminalNotify::Unsupported;
    }
    match term_program
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        // Ghostty 的 OSC 9 是官方文档化的；OSC 777 虽也实现，但没写进 VT 参考。
        // iTerm2 是 OSC 9 的原始实现方。
        Some("ghostty") | Some("iterm.app") => TerminalNotify::Osc9,
        // WezTerm 的 VT 参考里 OSC 9 与 OSC 777 都有且都进表格；用带标题的那条。
        Some("wezterm") => TerminalNotify::Osc777,
        // Apple Terminal / VS Code 集成终端没有内置通知通道（VS Code 需要第三方
        // 扩展解析 OSC），一律回落。
        _ => TerminalNotify::Unsupported,
    }
}

/// 组装 OSC 序列。控制字符（含 `ESC` / `BEL` / 换行）会让终端提前终止序列并把
/// 残渣当可见字符画到屏幕上，必须剔除。
fn osc_notification_sequence(support: TerminalNotify, title: &str, body: &str) -> Option<String> {
    let body = sanitize_osc_field(body);
    match support {
        // OSC 9 没有标题字段，把标题并进正文，否则用户只能看到半句话。
        TerminalNotify::Osc9 => Some(format!(
            "\x1b]9;{}: {}\x07",
            sanitize_osc_field(title),
            body
        )),
        // OSC 777 用分号分隔三段；标题里的分号会把它劈成两段，换成空格。
        TerminalNotify::Osc777 => Some(format!(
            "\x1b]777;notify;{};{}\x07",
            sanitize_osc_field(title).replace(';', " "),
            body
        )),
        TerminalNotify::Unsupported => None,
    }
}

fn sanitize_osc_field(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

/// 走终端原生通道投递。写 `/dev/tty` 而不是 stdout：TUI 模式下 stdout 由
/// ratatui 接管，把控制序列插进它的 diff 输出中间没有任何好处；直接开 tty 也
/// 让 headless / REPL 模式共用同一条路径。
fn terminal_notify(payload: &NotificationPayload) -> Result<(), String> {
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let in_multiplexer = std::env::var_os("TMUX").is_some() || std::env::var_os("ZELLIJ").is_some();
    let support = detect_terminal_notify(term_program.as_deref(), in_multiplexer);
    let sequence = osc_notification_sequence(support, &payload.title, &payload.body).ok_or_else(
        || match (term_program.as_deref(), in_multiplexer) {
            (Some(program), true) => format!("{program} inside a terminal multiplexer"),
            (Some(program), false) => format!("{program} has no documented OSC notification"),
            (None, _) => "TERM_PROGRAM is not set".to_string(),
        },
    )?;
    write_terminal_sequence(&sequence)
}

#[cfg(unix)]
fn write_terminal_sequence(sequence: &str) -> Result<(), String> {
    use std::fs::OpenOptions;
    let mut tty = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("open /dev/tty: {e}"))?;
    tty.write_all(sequence.as_bytes())
        .map_err(|e| format!("write /dev/tty: {e}"))?;
    tty.flush().map_err(|e| format!("flush /dev/tty: {e}"))
}

#[cfg(not(unix))]
fn write_terminal_sequence(_sequence: &str) -> Result<(), String> {
    Err("terminal OSC notifications are unsupported on this platform".to_string())
}

/// 回落只在**首次**记一条 debug：`desktop.enabled=true` 且终端不支持时每次
/// emit 都会走到这里，逐次记会淹没 debug 日志。
fn note_terminal_fallback_once(reason: &str) {
    static NOTED: OnceLock<()> = OnceLock::new();
    if NOTED.set(()).is_ok() {
        tracing::debug!(
            "terminal OSC notification unavailable ({reason}); falling back to the system command"
        );
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
    use wyj_config::NotifyEventsCfg;

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
        let sinks: Vec<Box<dyn NotificationSink>> =
            vec![Box::new(rec1.clone()), Box::new(rec2.clone())];
        init_with_sinks(&NotifyCfg::default(), sinks);
        emit(NotificationEvent::TurnError {
            error: "boom".to_string(),
            session_id: None,
        });
        assert_eq!(rec1.received().len(), 1);
        assert_eq!(rec2.received().len(), 1);
        assert!(rec1.received()[0].1.contains("boom"));
        assert!(rec2.received()[0].1.contains("boom"));
    }

    #[test]
    fn master_off_skips_all_sinks() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rec = RecordingSink::new(0);
        let sinks: Vec<Box<dyn NotificationSink>> = vec![Box::new(rec.clone())];
        let cfg = NotifyCfg {
            enabled: false,
            ..NotifyCfg::default()
        };
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
        let cfg = NotifyCfg {
            events: NotifyEventsCfg {
                subagent_done: false,
                ..NotifyEventsCfg::default()
            },
            ..NotifyCfg::default()
        };
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
        let cfg = NotifyCfg {
            rate_limit_seconds: 60,
            ..NotifyCfg::default()
        };
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
        let cfg = NotifyCfg {
            rate_limit_seconds: 0,
            ..NotifyCfg::default()
        };
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
        let rec_failing = RecordingSink::new(10); // 总是失败
        let sinks: Vec<Box<dyn NotificationSink>> =
            vec![Box::new(rec_failing.clone()), Box::new(rec_ok.clone())];
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
        assert!(
            cfg.desktop.prefer_terminal,
            "终端原生通知默认开——系统命令的通知点不开发起它的终端"
        );
        assert!(cfg.events.turn_finished);
        assert!(cfg.events.turn_error);
        assert!(
            cfg.events.subagent_done,
            "subagent_done 用户选择 opt-out 默认开"
        );
        assert_eq!(cfg.rate_limit_seconds, 30);
        assert!(!cfg.include_session_id);
    }

    #[test]
    fn terminal_support_follows_documented_sequences_only() {
        // Ghostty 的 VT 参考文档化了 OSC 9（OSC 777 也实现但未收录），取文档化那条。
        assert_eq!(
            detect_terminal_notify(Some("ghostty"), false),
            TerminalNotify::Osc9
        );
        // iTerm2 是 OSC 9 的原始实现方；TERM_PROGRAM 实际取值是 "iTerm.app"。
        assert_eq!(
            detect_terminal_notify(Some("iTerm.app"), false),
            TerminalNotify::Osc9
        );
        // WezTerm 的 VT 参考里 OSC 777 是带独立标题的那条。
        assert_eq!(
            detect_terminal_notify(Some("WezTerm"), false),
            TerminalNotify::Osc777
        );
        // Apple Terminal / VS Code 集成终端没有内置通知通道。
        assert_eq!(
            detect_terminal_notify(Some("Apple_Terminal"), false),
            TerminalNotify::Unsupported
        );
        assert_eq!(
            detect_terminal_notify(Some("vscode"), false),
            TerminalNotify::Unsupported
        );
        assert_eq!(
            detect_terminal_notify(None, false),
            TerminalNotify::Unsupported
        );
    }

    #[test]
    fn terminal_multiplexer_forces_the_system_fallback() {
        // OSC 要穿过 tmux / Zellij 得开 passthrough，未开时序列被复用器吞掉——
        // 静默丢通知比走系统命令更糟，因此一律回落。
        assert_eq!(
            detect_terminal_notify(Some("ghostty"), true),
            TerminalNotify::Unsupported
        );
        assert_eq!(
            detect_terminal_notify(Some("WezTerm"), true),
            TerminalNotify::Unsupported
        );
    }

    #[test]
    fn osc9_sequence_merges_title_into_the_body() {
        let seq = osc_notification_sequence(TerminalNotify::Osc9, "标题", "完成了").unwrap();
        assert_eq!(seq, "\x1b]9;标题: 完成了\x07");
        assert_eq!(
            osc_notification_sequence(TerminalNotify::Unsupported, "t", "b"),
            None
        );
    }

    #[test]
    fn osc777_sequence_keeps_title_and_body_separate() {
        let seq = osc_notification_sequence(TerminalNotify::Osc777, "标题", "完成了").unwrap();
        assert_eq!(seq, "\x1b]777;notify;标题;完成了\x07");
    }

    #[test]
    fn osc_fields_cannot_terminate_the_sequence_early() {
        // 正文里的 BEL / ESC 若不剔除，终端会就此终止 OSC，残渣被画到屏幕上。
        let seq = osc_notification_sequence(
            TerminalNotify::Osc9,
            "t\x1b]0;pwned\x07",
            "body\x07\x1b[31mred",
        )
        .unwrap();
        assert_eq!(seq.matches('\x07').count(), 1, "只允许结尾那一个 BEL");
        assert_eq!(seq.matches('\x1b').count(), 1, "只允许开头那一个 ESC");

        // OSC 777 的标题里出现分号会把它劈成两个字段。
        let seq = osc_notification_sequence(TerminalNotify::Osc777, "a;b", "body").unwrap();
        assert_eq!(seq, "\x1b]777;notify;a b;body\x07");
    }
}
