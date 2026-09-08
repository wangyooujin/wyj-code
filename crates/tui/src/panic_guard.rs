//! TUI 终端状态还原助手。
//!
//! `run_tui` 走 alternate screen + raw mode + mouse-capture-off 路径，正常返回时由它自己配对清理；
//! 但进程 panic 会跳过末尾清理，导致三件事：终端卡在 raw mode（按键不显示 / 输入不响应）；
//! 仍在 alternate screen 内，下一次 `wyj-code` 启动画面叠加残留 frame；
//! Rust 默认 panic handler 写 stderr 到 TTY，字符直接覆盖 ratatui cell，看起来像"画面撕裂"。
//!
//! 本模块提供一个进程级 `panic::set_hook`：进入 alternate screen 时调 `mark_active()`、
//! 离开后调 `mark_inactive()`；panic 发生时如果标志位为 true，hook 会先 best-effort
//! 还原终端（DisableMouseCapture、LeaveAlternateScreen、disable_raw_mode、Show），
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

/// alternate screen 进入标志。`mark_active(true)` → `mark_inactive(false)`
/// 与 `run_tui` 内的 `enter_terminal_screen`/`leave_terminal_screen` 配对。
static TUI_SCREEN_ACTIVE: AtomicBool = AtomicBool::new(false);

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
}

/// 标记已离开 alternate screen（`run_tui` 内 `leave_terminal_screen` 成功后调）。
pub fn mark_inactive() {
    TUI_SCREEN_ACTIVE.store(false, Ordering::SeqCst);
}

fn restore_terminal_best_effort() {
    use crossterm::{
        cursor::Show,
        event::DisableMouseCapture,
        execute,
        terminal::{disable_raw_mode, LeaveAlternateScreen},
    };
    let mut stdout = std::io::stdout();
    // 与 run_tui 末尾清理顺序相反：先 DisableMouseCapture,再 LeaveAlternateScreen,
    // 再 disable_raw_mode,最后 Show 光标。每一步都可能因为 TTY 已损坏或后台
    // 线程无 TTY 而失败,全部 best-effort 吞掉。
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen);
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
        mark_active();
        assert!(TUI_SCREEN_ACTIVE.load(Ordering::SeqCst));
        mark_inactive();
        assert!(!TUI_SCREEN_ACTIVE.load(Ordering::SeqCst));
    }
}
