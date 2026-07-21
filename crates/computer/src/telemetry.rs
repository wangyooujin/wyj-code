//! computer-use 本地进程级可观测计数。
//!
//! 这些指标只存在当前 wyj-code 进程内，不写磁盘、不联网，用于 `/computer`
//! 诊断后台/前台路径是否按预期工作以及安全边界是否频繁触发。

use std::sync::atomic::{AtomicU64, Ordering};

static BACKGROUND_ACTIONS: AtomicU64 = AtomicU64::new(0);
static TARGETED_PID_EVENTS: AtomicU64 = AtomicU64::new(0);
static FOREGROUND_ACTIONS: AtomicU64 = AtomicU64::new(0);
static PREEMPTED_BY_USER: AtomicU64 = AtomicU64::new(0);
static TARGET_CHANGED: AtomicU64 = AtomicU64::new(0);
static REQUIRES_FOREGROUND: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_FOCUS_FUSES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetrySnapshot {
    pub background_actions: u64,
    pub targeted_pid_events: u64,
    pub foreground_actions: u64,
    pub preempted_by_user: u64,
    pub target_changed: u64,
    pub requires_foreground: u64,
    pub background_focus_fuses: u64,
    /// v1.4 把“后台失败后自动切全局输入”定义为禁止行为；保留在诊断快照中
    /// 作为可直接核验的恒零不变量。
    pub automatic_foreground_fallbacks: u64,
}

pub fn record_background_action() {
    BACKGROUND_ACTIONS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_targeted_pid_event() {
    TARGETED_PID_EVENTS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_foreground_action() {
    FOREGROUND_ACTIONS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_background_focus_fuse() {
    BACKGROUND_FOCUS_FUSES.fetch_add(1, Ordering::Relaxed);
}

pub fn record_error_message(message: &str) {
    if message.contains("preempted_by_user") {
        PREEMPTED_BY_USER.fetch_add(1, Ordering::Relaxed);
    }
    if message.contains("target_changed") {
        TARGET_CHANGED.fetch_add(1, Ordering::Relaxed);
    }
    if message.contains("requires_foreground_takeover") {
        REQUIRES_FOREGROUND.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn snapshot() -> TelemetrySnapshot {
    TelemetrySnapshot {
        background_actions: BACKGROUND_ACTIONS.load(Ordering::Relaxed),
        targeted_pid_events: TARGETED_PID_EVENTS.load(Ordering::Relaxed),
        foreground_actions: FOREGROUND_ACTIONS.load(Ordering::Relaxed),
        preempted_by_user: PREEMPTED_BY_USER.load(Ordering::Relaxed),
        target_changed: TARGET_CHANGED.load(Ordering::Relaxed),
        requires_foreground: REQUIRES_FOREGROUND.load(Ordering::Relaxed),
        background_focus_fuses: BACKGROUND_FOCUS_FUSES.load(Ordering::Relaxed),
        automatic_foreground_fallbacks: 0,
    }
}
