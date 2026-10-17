//! 瞬移式坐标点击：「跳过去 → 点 → 跳回来」，用于 AX 树空洞的应用。
//!
//! # 为什么需要它
//!
//! 目标是让微信这类应用能被完全自主操控。实测（本机 macOS，微信 v4.1.15，
//! 原生 AppKit 自绘）确认了两件事：
//!
//! 1. **微信不暴露可用的 AX 树**：窗口矩形内 5×5 网格 hit-test **25/25 全部
//!    返回 `kAXErrorNotImplemented(-25208)`**，同一工具在访达上 36/36 全部命中
//!    （阳性对照成立）。AX 树里只有菜单栏，内容区完全空洞。
//! 2. **`CGEventPostToPid` 的鼠标事件无法用于后台点击**：事件确实送达了目标
//!    进程（键盘阳性对照同时成立，证明投递机制与权限都没问题），但
//!    leftMouseDown/Up 到达 `NSApplication.sendEvent` 时 `window == nil`，
//!    AppKit 拿不到窗口就无法做命中测试，于是**不路由给任何 NSWindow/NSView**
//!    ——事件在进程边缘静静死掉，调用方却收到成功返回。补写窗口号字段、预发
//!    mouseMoved primer、调整 down/up 间隔，全部无效。`mouseMoved` 是唯一例外。
//!
//! 结论：「不抢前台 + 不动真实光标 + 命中自绘 AppKit 应用」三者无法同时满足。
//! 要点微信必须接受短暂移动真实光标——本模块就是在这个取舍下实现的那个选项。
//!
//! # 为什么是「瞬移 + 复原」而不是「激活到前台」
//!
//! 激活会**永久**打断用户手头的工作，且抢焦点在高频点击下非常烦人。瞬移方案
//! 只在几毫秒内借用光标，且**不改变前台应用**（见 [`execute`] 的说明），
//! 用户的输入焦点始终留在原处。
//!
//! # 安全边界
//!
//! 本模块**不绕过**既有的任何一道闸门，只是补上最后一块缺失的输入合成能力：
//!
//! - 调用方（`app_computer`）仍必须先拿到 [`crate::activity::InputLease`]，
//!   它证明用户在最近 `min_quiet` 内没有输入、活动监视器在运行、屏幕未锁。
//! - 所有合成事件都带 [`INPUT_EVENT_MARKER`]，因此**不会被本进程自己的
//!   Event Tap 误判成「用户抢占」**（见 `activity::event_tap_callback`）。
//! - 执行前后校验前台 PID：光标瞬移不该改变前台，若变了说明有第三方在抢，
//!   立刻报 `preempted_by_user` 而不是继续。
//! - 光标位置**无条件复原**，包括中途出错时——用 RAII 保证，绝不把用户的光标
//!   留在别人的窗口里。
//!
//! # 已知的用户可见影响
//!
//! 光标会短暂「跳」一下。这是本方案的固有代价，无法消除——见上面的实测结论。
//! 整个往返通常在几十毫秒内完成，且不抢焦点。

use crate::activity::INPUT_EVENT_MARKER;
use anyhow::{anyhow, bail, Result};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton,
};

fn source() -> Result<objc2_core_foundation::CFRetained<CGEventSource>> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .ok_or_else(|| anyhow!("failed to create private CGEventSource"))
}

fn mark(event: &CGEvent) {
    CGEvent::set_integer_value_field(
        Some(event),
        CGEventField::EventSourceUserData,
        INPUT_EVENT_MARKER,
    );
}

/// 读取当前光标位置（全局屏幕逻辑坐标，与 `screenshot_to_global` 同坐标系）。
///
/// **不能用 `CGEvent::location(None)`**：传 `None` 时它读的是「无事件」，返回
/// (0, 0) 而不是光标实际位置——`execute` 会据此把光标复原到屏幕左上角，比
/// 不复原还糟。`CGEvent::new(None)` 同样不行（空事件的 location 无意义）。
///
/// 走 [`crate::cursor_location`]（enigo，与旧 `computer` 工具读光标同一条路），
/// 它把 y 轴从 AppKit 的左下原点翻成屏幕的左上原点，与本模块 warp 用的
/// `CGWarpMouseCursorPosition` 坐标系一致。
fn cursor_position() -> Result<CGPoint> {
    let (x, y) = crate::cursor_location()?;
    Ok(CGPoint {
        x: f64::from(x),
        y: f64::from(y),
    })
}

/// 瞬移光标到 `(x, y)`（全局屏幕逻辑坐标）。
///
/// 用 `CGWarpMouseCursorPosition` 而不是发 mouseMoved：warp 改的是窗口服务器
/// 里的指针位置，**不产生事件**，因此既不会被本进程的 Event Tap 记成外部
/// 输入，也不会在用户屏幕上留下运动痕迹。
fn warp(x: f64, y: f64) -> Result<()> {
    let error = objc2_core_graphics::CGWarpMouseCursorPosition(CGPoint { x, y });
    // 光标瞬移失败对模型没有可操作性，直接把原始码透出即可定位。
    if error != objc2_core_graphics::CGError::Success {
        bail!(
            "failed to warp the mouse cursor to ({x}, {y}): CGError({})",
            error.0
        );
    }
    Ok(())
}

/// 一次点击的语义。左右键之外的修饰键组合暂不支持——它们需要在按下期间保持
/// flag 状态，而瞬移方案要求光标尽快复位，两者冲突；模型可以用 `key` 组合键
/// 或 `press_menu` 替代。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickKind {
    Left,
    Right,
    Middle,
    DoubleLeft,
}

impl ClickKind {
    fn button(self) -> CGMouseButton {
        match self {
            Self::Left | Self::DoubleLeft => CGMouseButton::Left,
            Self::Right => CGMouseButton::Right,
            Self::Middle => CGMouseButton::Center,
        }
    }

    fn count(self) -> i64 {
        match self {
            Self::DoubleLeft => 2,
            _ => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
            Self::DoubleLeft => "double left",
        }
    }

    /// 按下/抬起对应的事件类型。`CGMouseButton` 与 `CGEventType` 是两套枚举，
    /// 这里做一次性映射，避免在投递循环里重复 match。
    fn down_type(self) -> CGEventType {
        match self {
            Self::Right => CGEventType::RightMouseDown,
            Self::Middle => CGEventType::OtherMouseDown,
            _ => CGEventType::LeftMouseDown,
        }
    }

    fn up_type(self) -> CGEventType {
        match self {
            Self::Right => CGEventType::RightMouseUp,
            Self::Middle => CGEventType::OtherMouseUp,
            _ => CGEventType::LeftMouseUp,
        }
    }
}

pub fn parse_click_kind(name: &str) -> Result<ClickKind> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "left" | "left_click" => ClickKind::Left,
        "right" | "right_click" => ClickKind::Right,
        "middle" | "middle_click" => ClickKind::Middle,
        "double_left" | "double_click" => ClickKind::DoubleLeft,
        other => bail!("unsupported click kind `{other}`"),
    })
}

/// 光标位置的 RAII 复原守卫。
///
/// **必须在离开函数前 drop**，否则中途 `?` 早退会把用户的光标留在别人的窗口里。
/// 这正是拖拽逻辑里 `PressedMouse` 用 Drop 兜底的同一个道理。
struct CursorRestore {
    origin: CGPoint,
    armed: bool,
}

impl CursorRestore {
    fn arm(origin: CGPoint) -> Self {
        Self {
            origin,
            armed: true,
        }
    }
}

impl Drop for CursorRestore {
    fn drop(&mut self) {
        if self.armed {
            // 复原失败无处上报（可能在 unwind 中），但让光标停在别人的窗口里
            // 比报错更糟，所以这里只能尽力而为。
            let _ = warp(self.origin.x, self.origin.y);
        }
    }
}

/// 在全局坐标 `(x, y)` 处点击，结束后把光标复原到原位。
///
/// `(x, y)` 是**全局屏幕逻辑坐标**（与 `WindowTarget::screenshot_to_global`
/// 同一坐标系）。
///
/// 刻意**不发 mouseMoved**：既然马上要 warp 过去再 warp 回来，移动事件除了
/// 让目标窗口多一次 hover 响应之外没有意义，还可能触发「鼠标悬停展开浮层」
/// 之类的副作用，反而改变点击落点当时的界面状态。
///
/// 前台应用在整个过程中不变：窗口服务器把「谁在前台」与「事件投给谁」分开
/// 处理，点击非激活窗口通常不会夺走前台（与点击 Dock 图标、菜单栏的行为一致）。
pub fn execute(x: f64, y: f64, kind: ClickKind) -> Result<String> {
    let before = cursor_position()?;
    let mut restore = CursorRestore::arm(before);

    let source = source()?;
    warp(x, y)?;

    let button = kind.button();
    for index in 0..kind.count() {
        // `clickState` 是 AppKit 识别双击的依据：0=down, 1=up, 2=第二个 down,
        // 3=第二个 up。写错会被当成两次独立单击。
        for (state, event_type) in [(0_i64, kind.down_type()), (1_i64, kind.up_type())] {
            let event =
                CGEvent::new_mouse_event(Some(&source), event_type, CGPoint { x, y }, button)
                    .ok_or_else(|| anyhow!("failed to create mouse event"))?;
            mark(&event);
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventClickState,
                if kind.count() > 1 {
                    index * 2 + state
                } else {
                    state
                },
            );
            CGEvent::set_flags(Some(&event), CGEventFlags::empty());
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
    }

    // 立即复位——这是整个方案的关键：用户看到的只是一次极短的位移。
    warp(before.x, before.y)?;
    restore.armed = false;

    crate::telemetry::record_background_action();
    Ok(format!(
        "{} clicked at ({}, {}) via cursor warp; the cursor was restored to ({}, {}) \
         and the foreground app was not changed",
        kind.label(),
        x,
        y,
        before.x,
        before.y
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_click_kind() {
        assert_eq!(parse_click_kind("left").unwrap(), ClickKind::Left);
        assert_eq!(parse_click_kind("right").unwrap(), ClickKind::Right);
        assert_eq!(parse_click_kind("middle").unwrap(), ClickKind::Middle);
        assert_eq!(
            parse_click_kind("double_left").unwrap(),
            ClickKind::DoubleLeft
        );
        assert_eq!(
            parse_click_kind("DOUBLE_CLICK").unwrap(),
            ClickKind::DoubleLeft
        );
        assert!(parse_click_kind("drag").is_err());
    }

    #[test]
    fn double_click_uses_two_down_up_pairs() {
        assert_eq!(ClickKind::DoubleLeft.count(), 2);
        assert_eq!(ClickKind::Left.count(), 1);
        // 双击必须是左键：右键双击在 macOS 上没有可移植语义。
        assert_eq!(ClickKind::DoubleLeft.button(), CGMouseButton::Left);
    }

    /// 真机验证：光标瞬移点击后**必须回到原位**。
    ///
    /// 这是整个方案成立的前提——用户的指针被借用几十毫秒是可以接受的，被留在
    /// 别人的窗口里则不可接受。默认跳过（会真实移动指针），真机验证时显式跑：
    ///
    /// ```text
    /// cargo test -p wyj-computer cursor_click::tests::cursor_returns_to_origin -- --ignored --nocapture
    /// ```
    ///
    /// 落在主显示器内的空白处，不点任何窗口控件。
    #[test]
    #[ignore = "会真实移动用户指针；仅真机验证时手动运行"]
    fn cursor_returns_to_origin() {
        let origin = cursor_position().unwrap();
        // 往左挪 40pt：保证与原点不同，否则「复原」是空断言；同时尽量不点中
        // 任何窗口控件（左边缘通常是桌面或窗口最外侧）。
        let target = CGPoint {
            x: (origin.x - 40.0).max(1.0),
            y: origin.y,
        };
        assert!(
            (target.x - origin.x).abs() + (target.y - origin.y).abs() >= 1.0,
            "测试自身无效：目标点与原点重合"
        );
        execute(target.x, target.y, ClickKind::Left).unwrap();
        let after = cursor_position().unwrap();
        assert!(
            (after.x - origin.x).abs() < 1.0 && (after.y - origin.y).abs() < 1.0,
            "光标必须回到原位：期望 ({}, {})，实际 ({}, {})",
            origin.x,
            origin.y,
            after.x,
            after.y
        );
    }
}
