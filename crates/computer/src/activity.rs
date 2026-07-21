//! 精确输入仲裁器。
//!
//! wyj-code 合成的输入统一携带 [`INPUT_EVENT_MARKER`]；macOS 上的被动
//! session Event Tap 只把其它事件计为外部输入。这样不再用“动作后 0.8 秒
//! 内的系统 idle 变化大概是自己造成的”这类时间启发式猜测，人类输入可以
//! 立即使现有租约失效，前台兼容动作在监视器不可用时则失败关闭。

use anyhow::Result;
use std::time::Duration;

/// ASCII "WYJCODE"，避免使用 enigo 公共默认 marker `100` 与其它进程碰撞。
pub const INPUT_EVENT_MARKER: i64 = 0x0057_594A_434F_4445;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMonitorStatus {
    NotStarted,
    Starting,
    Running,
    Unavailable,
}

impl InputMonitorStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Unavailable => "unavailable",
        }
    }
}

/// 一次活跃度快照。
#[derive(Debug, Clone)]
pub struct ActivitySnapshot {
    /// 系统 HID 层距离最后一次输入（包含合成事件）的时间，仅用于诊断和
    /// monitor 初始化；安全判断使用 `external_idle_secs`。
    pub idle_secs: f64,
    /// 精确排除了 wyj-code marker 的外部输入空闲时间。
    pub external_idle_secs: Option<f64>,
    /// 每次外部输入单调递增，租约用它做无锁抢占校验。
    pub external_event_seq: Option<u64>,
    pub monitor_status: InputMonitorStatus,
    pub monitor_error: Option<String>,
    /// 当前图形会话是否处于锁屏状态。
    pub screen_locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLease {
    event_seq: u64,
    epoch: u64,
}

/// 后台窗口动作的目标区域，使用全局逻辑坐标（macOS points）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputRegion {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 启动进程级输入监视器（幂等）。
pub fn ensure_monitor() -> InputMonitorStatus {
    imp::ensure_monitor()
}

/// 探测当前活跃度快照；首次调用会异步启动输入监视器。
pub fn snapshot() -> Result<ActivitySnapshot> {
    imp::snapshot()
}

/// 只有 monitor 正常运行、屏幕未锁且外部输入已安静足够久时才签发租约。
pub fn acquire_lease(min_quiet: Duration) -> Result<InputLease> {
    imp::acquire_lease(min_quiet)
}

/// 人类产生任何新输入后，之前签发的租约立即失效。
pub fn lease_is_valid(lease: &InputLease) -> bool {
    imp::lease_is_valid(lease)
}

/// 判断租约签发后的人类输入是否可能影响指定后台窗口。
///
/// 与前台接管的“任何外部输入都撤销”不同，后台动作允许用户继续在其它 App
/// 键入或移动鼠标；只有目标窗口处于前台、指针变更事件落在目标窗口内、监控
/// 丢事件或屏幕锁定时才判为冲突。这样才能真正支持人和 Agent 并行工作。
pub fn conflicts_with_background_target(
    lease: &InputLease,
    region: InputRegion,
    target_frontmost: bool,
) -> bool {
    imp::conflicts_with_background_target(lease, region, target_frontmost)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{
        ActivitySnapshot, InputLease, InputMonitorStatus, InputRegion, INPUT_EVENT_MARKER,
    };
    use anyhow::{anyhow, bail, Result};
    use objc2_core_foundation::{
        kCFRunLoopDefaultMode, CFBoolean, CFMachPort, CFRetained, CFRunLoop, CFString, CFType,
    };
    use objc2_core_graphics::{
        CGEvent, CGEventField, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
        CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
        CGPreflightListenEventAccess, CGSessionCopyCurrentDictionary,
    };
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    const STATUS_NOT_STARTED: u8 = 0;
    const STATUS_STARTING: u8 = 1;
    const STATUS_RUNNING: u8 = 2;
    const STATUS_UNAVAILABLE: u8 = 3;

    const ANY_INPUT_EVENT_TYPE: CGEventType = CGEventType(u32::MAX);
    const SCREEN_LOCKED_KEY: &str = "CGSSessionScreenIsLocked";
    const RECENT_EVENT_CAPACITY: usize = 512;

    #[derive(Debug, Clone, Copy)]
    enum ExternalEventKind {
        PointerMove,
        PointerMutation { x: f64, y: f64 },
        Keyboard,
        MonitorGap,
    }

    #[derive(Debug, Clone, Copy)]
    struct ExternalEvent {
        seq: u64,
        kind: ExternalEventKind,
    }

    struct ExternalState {
        last_external_at: Instant,
        recent_events: VecDeque<ExternalEvent>,
    }

    struct MonitorState {
        status: AtomicU8,
        external: Mutex<ExternalState>,
        /// 完成记录的外部事件数量，供诊断和后台目标冲突分类使用。
        external_event_seq: AtomicU64,
        /// seqlock epoch：事件记录开始时变奇数，记录完成时变回偶数。
        /// 租约直接绑定这个值，因此 callback 一开始就能立即撤销旧租约，且
        /// acquire 不会读到“序号已更新、时间尚未更新”的半完成状态。
        epoch: AtomicU64,
        error: Mutex<Option<String>>,
    }

    impl MonitorState {
        fn new() -> Self {
            Self {
                status: AtomicU8::new(STATUS_NOT_STARTED),
                external: Mutex::new(ExternalState {
                    last_external_at: Instant::now(),
                    recent_events: VecDeque::with_capacity(RECENT_EVENT_CAPACITY),
                }),
                external_event_seq: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                error: Mutex::new(None),
            }
        }

        fn note_external_input(&self, kind: ExternalEventKind) {
            // 先把 epoch 变成奇数，旧租约会在 callback 进入的第一时间失效。
            self.epoch.fetch_add(1, Ordering::AcqRel);
            let seq = self.external_event_seq.fetch_add(1, Ordering::AcqRel) + 1;
            {
                let mut external = lock_unpoisoned(&self.external);
                external.last_external_at = Instant::now();
                if external.recent_events.len() == RECENT_EVENT_CAPACITY {
                    external.recent_events.pop_front();
                }
                external
                    .recent_events
                    .push_back(ExternalEvent { seq, kind });
            }
            // 发布完整记录。偶数 epoch 表示读者可以取得一致快照。
            self.epoch.fetch_add(1, Ordering::Release);
        }

        fn fail(&self, error: impl Into<String>) {
            *lock_unpoisoned(&self.error) = Some(error.into());
            self.status.store(STATUS_UNAVAILABLE, Ordering::Release);
        }
    }

    fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn state() -> &'static Arc<MonitorState> {
        static STATE: OnceLock<Arc<MonitorState>> = OnceLock::new();
        STATE.get_or_init(|| Arc::new(MonitorState::new()))
    }

    fn status_from_raw(raw: u8) -> InputMonitorStatus {
        match raw {
            STATUS_NOT_STARTED => InputMonitorStatus::NotStarted,
            STATUS_STARTING => InputMonitorStatus::Starting,
            STATUS_RUNNING => InputMonitorStatus::Running,
            _ => InputMonitorStatus::Unavailable,
        }
    }

    fn stable_external_snapshot(state: &MonitorState) -> (Instant, u64, u64) {
        loop {
            let epoch_before = state.epoch.load(Ordering::Acquire);
            if epoch_before % 2 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let external = lock_unpoisoned(&state.external);
            let last_external_at = external.last_external_at;
            let event_seq = state.external_event_seq.load(Ordering::Acquire);
            let epoch_after = state.epoch.load(Ordering::Acquire);
            drop(external);
            if epoch_before == epoch_after && epoch_after % 2 == 0 {
                return (last_external_at, event_seq, epoch_after);
            }
            std::hint::spin_loop();
        }
    }

    pub fn ensure_monitor() -> InputMonitorStatus {
        let state = state();
        if state
            .status
            .compare_exchange(
                STATUS_NOT_STARTED,
                STATUS_STARTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // 让首次租约不会把进程启动前的用户输入误判成“刚刚发生”。一旦
            // Event Tap 启动，后续时间完全由 callback 精确维护。
            let hid_idle = system_idle_secs();
            let seeded = Instant::now()
                .checked_sub(Duration::from_secs_f64(hid_idle.max(0.0)))
                .unwrap_or_else(Instant::now);
            lock_unpoisoned(&state.external).last_external_at = seeded;

            let thread_state = Arc::clone(state);
            if let Err(error) = thread::Builder::new()
                .name("wyj-input-arbiter".to_string())
                .spawn(move || {
                    if let Err(error) = run_event_tap(&thread_state) {
                        thread_state.fail(error.to_string());
                    }
                })
            {
                state.fail(format!("无法启动输入监视线程: {error}"));
            }
        }
        status_from_raw(state.status.load(Ordering::Acquire))
    }

    pub fn snapshot() -> Result<ActivitySnapshot> {
        ensure_monitor();
        let state = state();
        let monitor_status = status_from_raw(state.status.load(Ordering::Acquire));
        let running = monitor_status == InputMonitorStatus::Running;
        let stable = running.then(|| stable_external_snapshot(state));
        Ok(ActivitySnapshot {
            idle_secs: system_idle_secs(),
            external_idle_secs: stable
                .as_ref()
                .map(|(last_external_at, _, _)| last_external_at.elapsed().as_secs_f64()),
            external_event_seq: stable.map(|(_, event_seq, _)| event_seq),
            monitor_status,
            monitor_error: lock_unpoisoned(&state.error).clone(),
            screen_locked: is_screen_locked(),
        })
    }

    pub fn acquire_lease(min_quiet: Duration) -> Result<InputLease> {
        let snapshot = snapshot()?;
        if snapshot.screen_locked {
            bail!("screen_locked: computer-use mutation is unavailable while the screen is locked");
        }
        if snapshot.monitor_status != InputMonitorStatus::Running {
            bail!(
                "input_monitor_unavailable: exact external-input attribution is required ({})",
                snapshot
                    .monitor_error
                    .as_deref()
                    .unwrap_or(snapshot.monitor_status.label())
            );
        }
        let state = state();
        let (last_external_at, event_seq, epoch) = stable_external_snapshot(state);
        if is_screen_locked() {
            bail!("screen_locked: computer-use mutation is unavailable while the screen is locked");
        }
        let idle = last_external_at.elapsed().as_secs_f64();
        if idle < min_quiet.as_secs_f64() {
            bail!(
                "user_active: external input was detected {:.3}s ago; need {:.3}s quiet",
                idle,
                min_quiet.as_secs_f64()
            );
        }
        Ok(InputLease { event_seq, epoch })
    }

    pub fn lease_is_valid(lease: &InputLease) -> bool {
        let state = state();
        status_from_raw(state.status.load(Ordering::Acquire)) == InputMonitorStatus::Running
            && state.epoch.load(Ordering::Acquire) == lease.epoch
            && lease.epoch % 2 == 0
            && state.external_event_seq.load(Ordering::Acquire) == lease.event_seq
            && !is_screen_locked()
    }

    pub fn conflicts_with_background_target(
        lease: &InputLease,
        region: InputRegion,
        target_frontmost: bool,
    ) -> bool {
        let state = state();
        if status_from_raw(state.status.load(Ordering::Acquire)) != InputMonitorStatus::Running
            || is_screen_locked()
        {
            return true;
        }

        let epoch_before = state.epoch.load(Ordering::Acquire);
        if epoch_before % 2 != 0 {
            return true;
        }
        let external = lock_unpoisoned(&state.external);
        let current_seq = state.external_event_seq.load(Ordering::Acquire);
        let epoch_after = state.epoch.load(Ordering::Acquire);
        if epoch_before != epoch_after || epoch_after % 2 != 0 {
            return true;
        }
        if current_seq == lease.event_seq {
            return false;
        }
        if current_seq < lease.event_seq {
            return true;
        }
        recent_events_conflict(
            &external.recent_events,
            lease.event_seq,
            region,
            target_frontmost,
        )
    }

    fn recent_events_conflict(
        recent_events: &VecDeque<ExternalEvent>,
        lease_event_seq: u64,
        region: InputRegion,
        target_frontmost: bool,
    ) -> bool {
        let Some(first) = recent_events.front() else {
            return true;
        };
        if first.seq > lease_event_seq.saturating_add(1) {
            // 动作期间事件量超过环形缓冲容量，无法可靠分类，失败关闭。
            return true;
        }

        recent_events
            .iter()
            .filter(|event| event.seq > lease_event_seq)
            .any(|event| match event.kind {
                ExternalEventKind::PointerMove => false,
                ExternalEventKind::Keyboard => target_frontmost,
                ExternalEventKind::MonitorGap => true,
                ExternalEventKind::PointerMutation { x, y } => {
                    x >= region.x
                        && y >= region.y
                        && x < region.x + region.width.max(0.0)
                        && y < region.y + region.height.max(0.0)
                }
            })
    }

    fn event_mask(types: &[CGEventType]) -> u64 {
        types.iter().fold(0u64, |mask, ty| mask | (1u64 << ty.0))
    }

    unsafe extern "C-unwind" fn event_tap_callback(
        _proxy: CGEventTapProxy,
        event_type: CGEventType,
        event: NonNull<CGEvent>,
        _user_info: *mut c_void,
    ) -> *mut CGEvent {
        if event_type == CGEventType::TapDisabledByTimeout
            || event_type == CGEventType::TapDisabledByUserInput
        {
            let state = state();
            state.note_external_input(ExternalEventKind::MonitorGap);
            state.status.store(STATUS_STARTING, Ordering::Release);
            return event.as_ptr();
        }

        // SAFETY: CoreGraphics guarantees `event` is valid for the callback duration.
        let event_ref = unsafe { event.as_ref() };
        let marker =
            CGEvent::integer_value_field(Some(event_ref), CGEventField::EventSourceUserData);
        if marker != INPUT_EVENT_MARKER {
            let kind = match event_type {
                CGEventType::MouseMoved => ExternalEventKind::PointerMove,
                CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                    ExternalEventKind::Keyboard
                }
                _ => {
                    let location = CGEvent::location(Some(event_ref));
                    ExternalEventKind::PointerMutation {
                        x: location.x,
                        y: location.y,
                    }
                }
            };
            state().note_external_input(kind);
        }
        event.as_ptr()
    }

    fn run_event_tap(state: &MonitorState) -> Result<()> {
        if !CGPreflightListenEventAccess() {
            bail!(
                "macOS Input Monitoring permission is missing; enable it for wyj-code in System Settings > Privacy & Security > Input Monitoring"
            );
        }

        let mask = event_mask(&[
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ]);
        // SAFETY: callback uses only process-global synchronized state; user_info is null.
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                mask,
                Some(event_tap_callback),
                std::ptr::null_mut(),
            )
        }
        .ok_or_else(|| anyhow!("failed to create passive macOS Event Tap"))?;
        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
            .ok_or_else(|| anyhow!("failed to create Event Tap run-loop source"))?;
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| anyhow!("failed to access Event Tap thread run loop"))?;
        // SAFETY: CoreFoundation exports this process-lifetime singleton mode.
        let default_mode = unsafe { kCFRunLoopDefaultMode };
        run_loop.add_source(Some(&source), default_mode);
        CGEvent::tap_enable(&tap, true);
        if !CGEvent::tap_is_enabled(&tap) {
            bail!("macOS Event Tap was created but could not be enabled");
        }
        state.status.store(STATUS_RUNNING, Ordering::Release);

        loop {
            CFRunLoop::run_in_mode(default_mode, 0.25, false);
            if !tap.is_valid() {
                bail!("macOS Event Tap became invalid");
            }
            if !CGEvent::tap_is_enabled(&tap) {
                CGEvent::tap_enable(&tap, true);
                if !CGEvent::tap_is_enabled(&tap) {
                    bail!("macOS Event Tap was disabled and could not be re-enabled");
                }
                state.status.store(STATUS_RUNNING, Ordering::Release);
            }
        }
    }

    fn system_idle_secs() -> f64 {
        CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::HIDSystemState,
            ANY_INPUT_EVENT_TYPE,
        )
    }

    fn is_screen_locked() -> bool {
        let Some(dict) = CGSessionCopyCurrentDictionary() else {
            return false;
        };
        let typed: &objc2_core_foundation::CFDictionary<CFString, CFType> =
            unsafe { dict.cast_unchecked() };
        let key = CFString::from_str(SCREEN_LOCKED_KEY);
        let Some(value): Option<CFRetained<CFType>> = typed.get(&key) else {
            return false;
        };
        value
            .downcast_ref::<CFBoolean>()
            .map(|value| value.as_bool())
            .unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn region() -> InputRegion {
            InputRegion {
                x: 100.0,
                y: 200.0,
                width: 400.0,
                height: 300.0,
            }
        }

        #[test]
        fn event_recording_publishes_a_complete_even_epoch() {
            let state = MonitorState::new();
            state.note_external_input(ExternalEventKind::Keyboard);
            assert_eq!(state.external_event_seq.load(Ordering::Acquire), 1);
            assert_eq!(state.epoch.load(Ordering::Acquire), 2);
            assert_eq!(lock_unpoisoned(&state.external).recent_events.len(), 1);
        }

        #[test]
        fn unrelated_background_input_does_not_conflict() {
            let events = VecDeque::from([
                ExternalEvent {
                    seq: 11,
                    kind: ExternalEventKind::Keyboard,
                },
                ExternalEvent {
                    seq: 12,
                    kind: ExternalEventKind::PointerMutation { x: 20.0, y: 30.0 },
                },
                ExternalEvent {
                    seq: 13,
                    kind: ExternalEventKind::PointerMove,
                },
            ]);
            assert!(!recent_events_conflict(&events, 10, region(), false));
        }

        #[test]
        fn target_pointer_or_foreground_keyboard_input_conflicts() {
            let target_pointer = VecDeque::from([ExternalEvent {
                seq: 11,
                kind: ExternalEventKind::PointerMutation { x: 150.0, y: 250.0 },
            }]);
            assert!(recent_events_conflict(&target_pointer, 10, region(), false));

            let keyboard = VecDeque::from([ExternalEvent {
                seq: 11,
                kind: ExternalEventKind::Keyboard,
            }]);
            assert!(recent_events_conflict(&keyboard, 10, region(), true));
        }

        #[test]
        fn missing_history_or_monitor_gap_fails_closed() {
            let truncated = VecDeque::from([ExternalEvent {
                seq: 20,
                kind: ExternalEventKind::PointerMove,
            }]);
            assert!(recent_events_conflict(&truncated, 10, region(), false));

            let gap = VecDeque::from([ExternalEvent {
                seq: 11,
                kind: ExternalEventKind::MonitorGap,
            }]);
            assert!(recent_events_conflict(&gap, 10, region(), false));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{ActivitySnapshot, InputLease, InputMonitorStatus, InputRegion};
    use anyhow::{bail, Result};
    use std::time::Duration;

    pub fn ensure_monitor() -> InputMonitorStatus {
        InputMonitorStatus::Unavailable
    }

    pub fn snapshot() -> Result<ActivitySnapshot> {
        Ok(ActivitySnapshot {
            idle_secs: 0.0,
            external_idle_secs: None,
            external_event_seq: None,
            monitor_status: InputMonitorStatus::Unavailable,
            monitor_error: Some(
                "exact external-input monitoring is only implemented on macOS".into(),
            ),
            screen_locked: false,
        })
    }

    pub fn acquire_lease(_min_quiet: Duration) -> Result<InputLease> {
        bail!("input_monitor_unavailable: exact external-input monitoring is only implemented on macOS")
    }

    pub fn lease_is_valid(_lease: &InputLease) -> bool {
        false
    }

    pub fn conflicts_with_background_target(
        _lease: &InputLease,
        _region: InputRegion,
        _target_frontmost: bool,
    ) -> bool {
        true
    }
}
