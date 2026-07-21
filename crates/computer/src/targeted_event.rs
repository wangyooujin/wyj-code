//! 向指定 macOS 进程投递键盘/滚动事件。
//!
//! 这是 Accessibility 无法表达操作时的后台兜底：事件使用 private source、
//! 带 wyj-code marker，并通过 `CGEventPostToPid` 发送；绝不使用全局 post。

use crate::activity::INPUT_EVENT_MARKER;
use anyhow::{anyhow, bail, Result};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGKeyCode,
    CGScrollEventUnit,
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

fn key_code(name: &str) -> Option<CGKeyCode> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" => 49,
        "`" => 50,
        "backspace" | "delete" => 51,
        "escape" | "esc" => 53,
        "home" => 115,
        "end" => 119,
        "pageup" | "page_up" => 116,
        "pagedown" | "page_down" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    })
}

fn parse_key(combo: &str) -> Result<(CGKeyCode, CGEventFlags)> {
    let mut flags = CGEventFlags::empty();
    let mut key = None;
    for part in combo
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "meta" | "super" => flags |= CGEventFlags::MaskCommand,
            "ctrl" | "control" => flags |= CGEventFlags::MaskControl,
            "alt" | "option" => flags |= CGEventFlags::MaskAlternate,
            "shift" => flags |= CGEventFlags::MaskShift,
            _ => {
                if key.is_some() {
                    bail!("invalid target key combination `{combo}`");
                }
                key = Some(
                    key_code(part)
                        .ok_or_else(|| anyhow!("unsupported target key combination `{combo}`"))?,
                );
            }
        }
    }
    Ok((
        key.ok_or_else(|| anyhow!("unsupported target key combination `{combo}`"))?,
        flags,
    ))
}

fn validate_focus_unchanged(before: u32) -> Result<()> {
    let after = crate::accessibility::frontmost_pid()?;
    if after != before {
        bail!(
            "target_changed: frontmost application changed during background event ({before} -> {after})"
        );
    }
    Ok(())
}

pub fn key(pid: u32, combo: &str) -> Result<()> {
    let before = crate::accessibility::frontmost_pid()?;
    let (code, flags) = parse_key(combo)?;
    let source = source()?;
    let down = CGEvent::new_keyboard_event(Some(&source), code, true)
        .ok_or_else(|| anyhow!("failed to create target key-down event"))?;
    let up = CGEvent::new_keyboard_event(Some(&source), code, false)
        .ok_or_else(|| anyhow!("failed to create target key-up event"))?;
    for event in [&*down, &*up] {
        mark(event);
        CGEvent::set_flags(Some(event), flags);
        CGEvent::post_to_pid(pid as libc::pid_t, Some(event));
    }
    crate::telemetry::record_targeted_pid_event();
    validate_focus_unchanged(before)
}

pub fn scroll(pid: u32, direction: &str, amount: u32) -> Result<()> {
    let before = crate::accessibility::frontmost_pid()?;
    let amount = i32::try_from(amount.clamp(1, 20)).unwrap_or(20);
    let (vertical, horizontal, wheels) = match direction {
        "up" => (amount, 0, 1),
        "down" => (-amount, 0, 1),
        "left" => (0, amount, 2),
        "right" => (0, -amount, 2),
        other => bail!("invalid scroll direction `{other}`"),
    };
    let source = source()?;
    let event = CGEvent::new_scroll_wheel_event2(
        Some(&source),
        CGScrollEventUnit::Line,
        wheels,
        vertical,
        horizontal,
        0,
    )
    .ok_or_else(|| anyhow!("failed to create target scroll event"))?;
    mark(&event);
    CGEvent::post_to_pid(pid as libc::pid_t, Some(&event));
    crate::telemetry::record_targeted_pid_event();
    validate_focus_unchanged(before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key() {
        let (code, flags) = parse_key("cmd+shift+s").unwrap();
        assert_eq!(code, 1);
        assert!(flags.contains(CGEventFlags::MaskCommand));
        assert!(flags.contains(CGEventFlags::MaskShift));
    }

    #[test]
    fn rejects_unknown_or_multiple_keys() {
        assert!(parse_key("cmd+not-a-key+s").is_err());
        assert!(parse_key("a+b").is_err());
        assert!(parse_key("cmd+shift").is_err());
    }
}
