//! TUI 终端状态还原助手。
//!
//! `run_tui` 走 alternate screen + raw mode + mouse-capture-off 路径，正常返回时由它自己配对清理；
//! 但进程 panic 会跳过末尾清理，导致三件事：终端卡在 raw mode（按键不显示 / 输入不响应）；
//! 仍在 alternate screen 内，下一次 `wyj-code` 启动画面叠加残留 frame；
//! Rust 默认 panic handler 写 stderr 到 TTY，字符直接覆盖 ratatui cell，看起来像"画面撕裂"。
//!
//! 本模块提供一个进程级 `panic::set_hook`：进入 alternate screen 时调 `mark_active()`、
//! 离开后调 `mark_inactive()`；panic 发生时如果标志位为 true，hook 会先 best-effort
//! 还原终端（pop 键盘增强、DisableMouseCapture、LeaveAlternateScreen、disable_raw_mode、Show），
//! 再调默认 hook 写 panic 信息。配合 `install()` 在 `main()` 入口最前注册，整条链路兜底。
//!
//! 重复 disable / leave / Show 是 best-effort：crossterm 重复执行一般是幂等的，
//! 任何错误吞掉即可。后台 tokio worker 线程 panic 也会触发 hook 并尝试还原
//! ——worker 持有不了 TTY，crossterm 命令在那个 fd 上写会失败，错误吞掉；
//! 代价是偶尔一次无害的 stdout ANSI 序列尝试，可接受。
//!
//! 真实案例：2026-09-07 用户跑 `stock_fenxi` 项目时，`memory_v3.rs:977` 的
//! `String::truncate(MAX_CONTEXT_BYTES)` 撞到 CJK char boundary panic，
//! panic 输出叠加在 alternate screen 上造成排版撕裂
//! （已被 `wyj-core::textutil::floor_char_boundary` 治本）。

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::ThreadId;

/// alternate screen 进入标志。`mark_active(true)` → `mark_inactive(false)`
/// 与 `run_tui` 内的 `enter_terminal_screen`/`leave_terminal_screen` 配对。
static TUI_SCREEN_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 持有 TUI 控制终端的线程（`mark_active` 的调用方）。
///
/// 只有这个线程 panic 时才排空 tty 输入队列：后台 tokio worker 的 panic 同样会
/// 触发 hook，而从 worker 读 `/dev/tty` 会把用户在 TUI 里刚敲的键一起吞掉。
static TUI_OWNER_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

/// 在 `main()` 入口最前注册一次 panic hook。**重复调用安全**
/// （`take_hook` + `set_hook` 链式保留前一个 hook）。
pub fn install() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // swap 同时读取 + 清零,避免双重还原。
        if TUI_SCREEN_ACTIVE.swap(false, Ordering::SeqCst) {
            restore_terminal_best_effort();
        }
        prev(info);
    }));
}

/// 标记已进入 alternate screen（`run_tui` 内 `enter_terminal_screen` 成功后调）。
pub fn mark_active() {
    TUI_SCREEN_ACTIVE.store(true, Ordering::SeqCst);
    if let Ok(mut owner) = TUI_OWNER_THREAD.lock() {
        *owner = Some(std::thread::current().id());
    }
}

/// 标记已离开 alternate screen（`run_tui` 内 `leave_terminal_screen` 成功后调）。
pub fn mark_inactive() {
    TUI_SCREEN_ACTIVE.store(false, Ordering::SeqCst);
    if let Ok(mut owner) = TUI_OWNER_THREAD.lock() {
        *owner = None;
    }
}

/// 当前线程是否持有 TUI 控制终端（= `mark_active` 的调用线程）。
fn is_tui_owner_thread() -> bool {
    TUI_OWNER_THREAD
        .lock()
        .ok()
        .and_then(|owner| *owner)
        .is_some_and(|id| id == std::thread::current().id())
}

fn restore_terminal_best_effort() {
    use crossterm::{
        cursor::Show,
        event::{DisableMouseCapture, PopKeyboardEnhancementFlags},
        execute,
        terminal::{disable_raw_mode, LeaveAlternateScreen},
    };
    let mut stdout = std::io::stdout();
    // 与 run_tui 末尾清理顺序相反：键盘增强 → DisableMouseCapture → LeaveAlternateScreen,
    // 再排空 tty 队列 → disable_raw_mode → Show 光标。每一步都可能因为 TTY 已损坏或后台
    // 线程无 TTY 而失败,全部 best-effort 吞掉。
    //
    // `PopKeyboardEnhancementFlags` 无条件发：panic 路径拿不到 `TerminalWheelRouting`，
    // 而漏 pop 的后果比多发一次严重得多 —— shell 会永久停在 Kitty 键盘增强模式，
    // Ctrl+C 不再产生 SIGINT，之后每次按键都变成 `CSI u` 序列被 zsh 当字符插入。
    // 没压过标志的终端会直接忽略 `CSI < u`。
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen);
    // 仍处于 raw mode 时排空：canonical 模式下 ISIG 会让队列里残留的 0x03 反向 SIGINT 自己。
    if is_tui_owner_thread() {
        crate::terminal_input::drain_pending_input();
    }
    let _ = disable_raw_mode();
    let _ = execute!(stdout, Show);
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_active_and_inactive_toggle() {
        // 注意：这些测试不能并发运行（操作全局 atomic），靠运行顺序。
        mark_inactive();
        assert!(!TUI_SCREEN_ACTIVE.load(Ordering::SeqCst));
        assert!(!is_tui_owner_thread());
        mark_active();
        assert!(TUI_SCREEN_ACTIVE.load(Ordering::SeqCst));
        // panic 路径只有在 TUI 所属线程上才排空 tty，避免 worker panic 吞掉用户按键。
        assert!(is_tui_owner_thread());
        mark_inactive();
        assert!(!TUI_SCREEN_ACTIVE.load(Ordering::SeqCst));
        assert!(!is_tui_owner_thread());
    }
}
