//! 终端控制：备用屏幕、鼠标上报、滚轮路由、退出时的按键释放等待。
//!
//! 从 `app.rs` 拆出。这些函数直接写 ANSI 序列，是 TUI 与终端之间的全部契约
//! 面——**改这里等于改对终端能力的假设**，因此单独成文件，免得混在事件循环里
//! 被顺手改掉。
//!
//! 关键背景：Ghostty 在 mode 1007 下把滚轮转成方向键序列，与用户真的按方向键
//! 无法区分。这里用 Kitty 键盘协议的 release 事件做分流（滚轮伪按键没有
//! release），对其它终端直接关 1007 保证不污染输入历史。

use super::*;

pub(super) const SAVE_AND_DISABLE_ALTERNATE_SCROLL: &str = "\x1b[?1007s\x1b[?1007l";
pub(super) const RESTORE_ALTERNATE_SCROLL: &str = "\x1b[?1007r";

/// Fullscreen + `DisableMouseCapture` 下的滚轮路由。
///
/// Ghostty 的 mode 1007 会把 alternate screen 中的滚轮转为普通方向键序列；
/// 开启 Kitty 键盘协议的 release/repeat 事件后，可以用“真实按键有 release，
/// 滚轮伪方向键只有 press”来分流，同时保留终端原生鼠标选择。
/// 其它或经过多路复用器的终端无法证明会透传 release 事件，因此直接
/// 关闭 mode 1007：可以保证滚轮不再污染输入历史，但不伪造“已可应用内滚动”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalWheelRouting {
    GhosttyKeyReleases,
    SuppressSyntheticArrows,
}

impl TerminalWheelRouting {
    pub(super) fn from_environment(term_program: Option<&str>, in_multiplexer: bool) -> Self {
        if term_program.is_some_and(|value| value.eq_ignore_ascii_case("ghostty"))
            && !in_multiplexer
        {
            Self::GhosttyKeyReleases
        } else {
            Self::SuppressSyntheticArrows
        }
    }

    pub(super) fn detect() -> Self {
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let in_multiplexer =
            std::env::var_os("TMUX").is_some() || std::env::var_os("ZELLIJ").is_some();
        Self::from_environment(term_program.as_deref(), in_multiplexer)
    }

    pub(super) fn distinguishes_synthetic_arrows(self) -> bool {
        self == Self::GhosttyKeyReleases
    }
}

/// 等回触发退出的那个按键的 release 事件的总上限。
pub(super) const QUIT_KEY_RELEASE_TIMEOUT: Duration = Duration::from_millis(300);
/// `event::poll` 的切片：分片轮询而不是一次睡满，便于超时精确收口。
pub(super) const QUIT_KEY_POLL_SLICE: Duration = Duration::from_millis(20);

/// 退出前等回触发退出的按键的 release 事件。
///
/// Ghostty 路径压了 `DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES`，一次 Ctrl+C /
/// Ctrl+D 因此会产生 **press + release 两个 `CSI u` 事件**（c → `CSI 99;5;1u` 与
/// `CSI 99;5;3u`；`CSI < u` 只能改写*之后*的编码，杀不掉已经发出的字节）。收到 press
/// 就直接退出的话，松手时才生成的 release 会落进 shell，被 zsh 的 ZLE 当普通字符插进
/// 命令行，表现为提示符后凭空多出 `9;5;3u`。这里把配对的 release 等回来（带上限），
/// 让一次按键的 press/release 在 TUI 内部闭环。
///
/// 其它终端路径的按键没有 release 事件，直接跳过、不浪费退出时间。
pub(super) fn await_quit_key_release(wheel_routing: TerminalWheelRouting) {
    if !wheel_routing.distinguishes_synthetic_arrows() {
        return;
    }
    let deadline = Instant::now() + QUIT_KEY_RELEASE_TIMEOUT;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        match event::poll(remaining.min(QUIT_KEY_POLL_SLICE)) {
            // 拿到任何一个按键事件即认为配对完成（正常情况下就是那个 release）。
            Ok(true) => return,
            Ok(false) => continue,
            Err(_) => return,
        }
    }
}

/// 鼠标报告模式：只开「按下/抬起 + 拖拽 + SGR 坐标」，**刻意不开 1003**。
///
/// - `1000h` 普通按钮跟踪：滚轮与点击都能上报；
/// - `1002h` 拖拽跟踪：按住左键移动（未来做拖选/滚动条拖动的基础）；
/// - `1006h` SGR 坐标：列号 > 223 时仍准确（旧的 X10 编码会在宽终端截断）。
///
/// 为什么不开 `1003h`（全移动上报）：触控板轻微移动就会持续产生事件，主循环
/// 每个事件重绘一帧，等于把 CPU 拖满。当前需求只有滚轮，没有悬停高亮。
pub(super) const ENABLE_WHEEL_MOUSE_REPORTING: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// 按需开关鼠标报告。`false` 走 crossterm 的 `DisableMouseCapture`（幂等，
/// 连 1003 一起关），保证从「开启」退回「关闭」时没有残留模式。
pub(super) fn set_mouse_capture<W: Write>(writer: &mut W, enabled: bool) -> io::Result<()> {
    if enabled {
        // 先全部关一遍再按需打开：`/mouse` 反复切换时不会把 1003 之类的
        // 历史残留模式留在终端里。
        execute!(
            writer,
            DisableMouseCapture,
            Print(ENABLE_WHEEL_MOUSE_REPORTING)
        )
    } else {
        execute!(writer, DisableMouseCapture)
    }
}

/// 进入全屏 TUI。
///
/// `mouse_capture` 为 false 时明确关闭鼠标捕获：鼠标由终端原生处理，用户可
/// 直接拖选文字，不需要按住 Shift/Option。为 true 时开启最小鼠标报告
/// （见 [`ENABLE_WHEEL_MOUSE_REPORTING`]），滚轮事件才会送进应用——这是
/// macOS Terminal.app 下唯一能滚动的途径（它不支持 DEC mode 1007）。
pub(super) fn enter_terminal_screen<W: Write>(
    writer: &mut W,
    wheel_routing: TerminalWheelRouting,
    mouse_capture: bool,
) -> io::Result<()> {
    execute!(writer, EnableBracketedPaste, EnterAlternateScreen)?;
    set_mouse_capture(writer, mouse_capture)?;
    match wheel_routing {
        TerminalWheelRouting::GhosttyKeyReleases => execute!(
            writer,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        ),
        TerminalWheelRouting::SuppressSyntheticArrows => {
            execute!(writer, Print(SAVE_AND_DISABLE_ALTERNATE_SCROLL))
        }
    }
}

pub(super) fn leave_terminal_screen<W: Write>(
    writer: &mut W,
    wheel_routing: TerminalWheelRouting,
) -> io::Result<()> {
    match wheel_routing {
        TerminalWheelRouting::GhosttyKeyReleases => execute!(writer, PopKeyboardEnhancementFlags)?,
        TerminalWheelRouting::SuppressSyntheticArrows => {
            execute!(writer, Print(RESTORE_ALTERNATE_SCROLL))?
        }
    }
    execute!(
        writer,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArrowRoutingAction {
    Pass,
    Defer,
    QueuePhysical { code: KeyCode, count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ArrowRoutingResult {
    pub(super) scroll_lines: i32,
    pub(super) action: ArrowRoutingAction,
}

/// 区分 Ghostty mode 1007 产生的滚轮伪方向键与真实方向键。
///
/// 真实按键在 `REPORT_EVENT_TYPES` 下会产生 release/repeat；Ghostty 的滚轮
/// 转译路径则直接写入传统 `ESC[A` / `ESC[B`，crossterm 只能看到 press。
/// 第一个无修饰方向键先暂存：收到 release/repeat 就回放为真实键盘事件；
/// 收到第二个连续 press 就将整串路由到聊天内容滚动，绝不触碰输入历史。
#[derive(Debug, Default)]
pub(super) struct GhosttyArrowRouter {
    pending: Option<KeyCode>,
    physical_down: Option<KeyCode>,
    wheel_burst: Option<(KeyCode, Instant)>,
}

impl GhosttyArrowRouter {
    const WHEEL_BURST_GAP: Duration = Duration::from_millis(40);

    pub(super) fn route(&mut self, key: KeyEvent) -> ArrowRoutingResult {
        self.route_at(key, Instant::now())
    }

    pub(super) fn route_at(&mut self, key: KeyEvent, now: Instant) -> ArrowRoutingResult {
        if self
            .wheel_burst
            .is_some_and(|(_, at)| now.saturating_duration_since(at) > Self::WHEEL_BURST_GAP)
        {
            self.wheel_burst = None;
        }

        let mut scroll_lines = 0;
        if let Some(pending) = self.pending {
            let continues_pending = key.code == pending
                && key.modifiers.is_empty()
                && matches!(
                    key.kind,
                    KeyEventKind::Press | KeyEventKind::Repeat | KeyEventKind::Release
                );
            if !continues_pending {
                scroll_lines += arrow_scroll_delta(pending);
                self.pending = None;
                self.wheel_burst = None;
            }
        }

        if !matches!(key.code, KeyCode::Up | KeyCode::Down) || !key.modifiers.is_empty() {
            return ArrowRoutingResult {
                scroll_lines,
                action: ArrowRoutingAction::Pass,
            };
        }

        match key.kind {
            KeyEventKind::Release => {
                if self.pending == Some(key.code) {
                    self.pending = None;
                    self.physical_down = None;
                    self.wheel_burst = None;
                    ArrowRoutingResult {
                        scroll_lines,
                        action: ArrowRoutingAction::QueuePhysical {
                            code: key.code,
                            count: 1,
                        },
                    }
                } else {
                    if self.physical_down == Some(key.code) {
                        self.physical_down = None;
                    }
                    ArrowRoutingResult {
                        scroll_lines,
                        action: ArrowRoutingAction::Defer,
                    }
                }
            }
            KeyEventKind::Repeat => {
                let count = if self.pending == Some(key.code) {
                    self.pending = None;
                    2
                } else {
                    1
                };
                self.physical_down = Some(key.code);
                self.wheel_burst = None;
                ArrowRoutingResult {
                    scroll_lines,
                    action: ArrowRoutingAction::QueuePhysical {
                        code: key.code,
                        count,
                    },
                }
            }
            KeyEventKind::Press => {
                if self.physical_down == Some(key.code) {
                    return ArrowRoutingResult {
                        scroll_lines,
                        action: ArrowRoutingAction::QueuePhysical {
                            code: key.code,
                            count: 1,
                        },
                    };
                }

                if self.pending == Some(key.code) {
                    self.pending = None;
                    self.wheel_burst = Some((key.code, now));
                    scroll_lines += arrow_scroll_delta(key.code) * 2;
                } else if self.wheel_burst.is_some_and(|(code, at)| {
                    code == key.code && now.saturating_duration_since(at) <= Self::WHEEL_BURST_GAP
                }) {
                    self.wheel_burst = Some((key.code, now));
                    scroll_lines += arrow_scroll_delta(key.code);
                } else {
                    self.pending = Some(key.code);
                    self.wheel_burst = None;
                }
                ArrowRoutingResult {
                    scroll_lines,
                    action: ArrowRoutingAction::Defer,
                }
            }
        }
    }
}

pub(super) const fn arrow_scroll_delta(code: KeyCode) -> i32 {
    match code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod terminal_screen_tests {
    use super::*;

    pub(super) fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    pub(super) fn startup_disables_mouse_capture_for_direct_terminal_selection() {
        let mut output = Vec::new();
        enter_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases, false)
            .unwrap();

        for disabled_mode in [
            b"\x1b[?1000l".as_slice(),
            b"\x1b[?1002l".as_slice(),
            b"\x1b[?1003l".as_slice(),
            b"\x1b[?1006l".as_slice(),
        ] {
            assert!(
                contains_bytes(&output, disabled_mode),
                "启动序列必须关闭全部鼠标报告模式: {output:?}"
            );
        }
        for enabled_mode in [
            b"\x1b[?1000h".as_slice(),
            b"\x1b[?1002h".as_slice(),
            b"\x1b[?1003h".as_slice(),
            b"\x1b[?1006h".as_slice(),
        ] {
            assert!(
                !contains_bytes(&output, enabled_mode),
                "启动序列不得重新开启鼠标捕获: {output:?}"
            );
        }
        assert!(
            contains_bytes(&output, b"\x1b[>3u"),
            "Ghostty 路径必须请求 press/repeat/release 键盘事件: {output:?}"
        );
        assert!(
            !contains_bytes(&output, b"\x1b[?1007l"),
            "Ghostty 路径需保留 mode 1007，再由应用区分滚轮伪方向键: {output:?}"
        );

        output.clear();
        leave_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases).unwrap();
        assert!(
            contains_bytes(&output, b"\x1b[<1u"),
            "退出时必须弹出启动时压入的 Kitty 键盘增强标志: {output:?}"
        );
        assert!(contains_bytes(&output, b"\x1b[?1049l"));
    }

    /// 开启鼠标捕获时只发「按下/抬起 + 拖拽 + SGR」三种模式。
    ///
    /// 1003（全移动上报）**必须缺席**：触控板轻微移动就会持续产生事件，
    /// 主循环每个事件重绘一帧。这条钉子防止有人图省事直接抄 crossterm 的
    /// `EnableMouseCapture`（它把 1000/1002/1003/1015/1006 全打开）。
    #[test]
    pub(super) fn mouse_capture_enables_only_wheel_and_drag_reporting() {
        let mut output = Vec::new();
        enter_terminal_screen(
            &mut output,
            TerminalWheelRouting::SuppressSyntheticArrows,
            true,
        )
        .unwrap();

        for enabled_mode in [
            b"\x1b[?1000h".as_slice(),
            b"\x1b[?1002h".as_slice(),
            b"\x1b[?1006h".as_slice(),
        ] {
            assert!(
                contains_bytes(&output, enabled_mode),
                "开启鼠标捕获必须打开该模式: {output:?}"
            );
        }
        assert!(
            !contains_bytes(&output, b"\x1b[?1003h"),
            "不得开启 1003 全移动上报（触控板会打满主循环）: {output:?}"
        );

        // 关闭时把三个模式全部收回，不残留。
        output.clear();
        set_mouse_capture(&mut output, false).unwrap();
        for disabled_mode in [
            b"\x1b[?1000l".as_slice(),
            b"\x1b[?1002l".as_slice(),
            b"\x1b[?1003l".as_slice(),
            b"\x1b[?1006l".as_slice(),
        ] {
            assert!(
                contains_bytes(&output, disabled_mode),
                "关闭鼠标捕获必须收回该模式: {output:?}"
            );
        }
    }

    #[test]
    pub(super) fn external_editor_handoff_pops_and_restores_tui_keyboard_protocols() {
        let mut output = Vec::new();
        leave_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases).unwrap();
        let resume_at = output.len();
        enter_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases, false)
            .unwrap();

        let suspended = &output[..resume_at];
        assert!(contains_bytes(suspended, b"\x1b[<1u"));
        assert!(contains_bytes(suspended, b"\x1b[?2004l"));
        assert!(contains_bytes(suspended, b"\x1b[?1049l"));

        let resumed = &output[resume_at..];
        assert!(contains_bytes(resumed, b"\x1b[?2004h"));
        assert!(contains_bytes(resumed, b"\x1b[?1049h"));
        assert!(contains_bytes(resumed, b"\x1b[>3u"));
    }

    #[test]
    pub(super) fn unsupported_terminal_suppresses_alternate_scroll_arrow_translation() {
        let mut output = Vec::new();
        enter_terminal_screen(
            &mut output,
            TerminalWheelRouting::SuppressSyntheticArrows,
            false,
        )
        .unwrap();
        assert!(contains_bytes(
            &output,
            SAVE_AND_DISABLE_ALTERNATE_SCROLL.as_bytes()
        ));
        assert!(!contains_bytes(&output, b"\x1b[>3u"));

        output.clear();
        leave_terminal_screen(&mut output, TerminalWheelRouting::SuppressSyntheticArrows).unwrap();
        assert!(contains_bytes(&output, RESTORE_ALTERNATE_SCROLL.as_bytes()));
        assert!(contains_bytes(&output, b"\x1b[?1049l"));
    }

    #[test]
    pub(super) fn ghostty_routing_requires_a_direct_terminal_connection() {
        assert_eq!(
            TerminalWheelRouting::from_environment(Some("ghostty"), false),
            TerminalWheelRouting::GhosttyKeyReleases
        );
        assert_eq!(
            TerminalWheelRouting::from_environment(Some("Ghostty"), true),
            TerminalWheelRouting::SuppressSyntheticArrows
        );
        assert_eq!(
            TerminalWheelRouting::from_environment(Some("Apple_Terminal"), false),
            TerminalWheelRouting::SuppressSyntheticArrows
        );
    }
}

#[cfg(test)]
mod ghostty_arrow_router_tests {
    use super::*;

    pub(super) fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    #[test]
    pub(super) fn real_arrow_is_replayed_only_after_release() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        assert_eq!(
            router.route_at(key(KeyCode::Up, KeyEventKind::Press), now),
            ArrowRoutingResult {
                scroll_lines: 0,
                action: ArrowRoutingAction::Defer,
            }
        );
        assert_eq!(
            router.route_at(
                key(KeyCode::Up, KeyEventKind::Release),
                now + Duration::from_millis(80)
            ),
            ArrowRoutingResult {
                scroll_lines: 0,
                action: ArrowRoutingAction::QueuePhysical {
                    code: KeyCode::Up,
                    count: 1,
                },
            }
        );
    }

    #[test]
    pub(super) fn wheel_arrow_burst_scrolls_content_without_replaying_keyboard_input() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        let _ = router.route_at(key(KeyCode::Up, KeyEventKind::Press), now);
        assert_eq!(
            router.route_at(
                key(KeyCode::Up, KeyEventKind::Press),
                now + Duration::from_millis(2)
            ),
            ArrowRoutingResult {
                scroll_lines: -2,
                action: ArrowRoutingAction::Defer,
            }
        );
        assert_eq!(
            router.route_at(
                key(KeyCode::Up, KeyEventKind::Press),
                now + Duration::from_millis(4)
            ),
            ArrowRoutingResult {
                scroll_lines: -1,
                action: ArrowRoutingAction::Defer,
            }
        );
    }

    #[test]
    pub(super) fn physical_repeat_replays_initial_press_and_repeat() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        let _ = router.route_at(key(KeyCode::Down, KeyEventKind::Press), now);
        assert_eq!(
            router.route_at(
                key(KeyCode::Down, KeyEventKind::Repeat),
                now + Duration::from_millis(300)
            ),
            ArrowRoutingResult {
                scroll_lines: 0,
                action: ArrowRoutingAction::QueuePhysical {
                    code: KeyCode::Down,
                    count: 2,
                },
            }
        );
    }

    #[test]
    pub(super) fn lone_wheel_arrow_is_flushed_to_content_before_the_next_key() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        let _ = router.route_at(key(KeyCode::Down, KeyEventKind::Press), now);
        assert_eq!(
            router.route_at(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                now + Duration::from_millis(10)
            ),
            ArrowRoutingResult {
                scroll_lines: 1,
                action: ArrowRoutingAction::Pass,
            }
        );
    }
}
