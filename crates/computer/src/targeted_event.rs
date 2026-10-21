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

/// 向目标进程投递 `cmd+V`，让它把剪贴板内容粘贴进当前焦点控件。
///
/// # 为什么这条路径是必要的
///
/// 逐字符投递（[`type_char`]）对非 ASCII 文字必然失败，而 AX 写入对自绘应用
/// 也必然失败——两条路都不通时，中文输入就成了死胡同。粘贴绕开了「合成每个
/// 字符」这个方向：它是应用层的语义动作，不关心内容是什么文字，因此中文、
/// 日文、emoji 全都走同一条路。
///
/// # 调用方必须先写好剪贴板
///
/// 本函数**只负责发按键**，不碰剪贴板。写入与恢复由
/// [`crate::clipboard::ClipboardGuard`] 负责——那个守卫保证用户的剪贴板
/// 在动作结束后复原，是这条路径能成立的前提。
///
/// # 为什么还要求前台 PID 不变
///
/// 键盘事件虽然直达目标 PID，但 `cmd+V` 的「粘贴」由目标进程自己处理；
/// 如果这几毫秒里前台换了人，说明有第三方在抢，此时目标进程的焦点控件可能
/// 已经不是调用方以为的那个。
pub fn paste(pid: u32) -> Result<()> {
    key(pid, "cmd+v")
}

/// 逐字符向目标进程投递按键。
///
/// **只支持 ASCII 可打印字符**。中文、日文等非拉丁文字**无法**用这种方式
/// 输入：`CGEventKeyboardSetUnicodeString` 虽能携带任意 Unicode，但除标准
/// ASCII 外的字符绝大多数 App 都不消费该字段，而是走 IME 合成路径——那需要
/// 目标进程处于前台、或至少持有 IME 上下文。因此非 ASCII 字符一律报错，
/// 让调用方明确知道「这条路走不通」，而不是静默丢字。
///
/// 逐字符而不是整串：整串的 `CGEventKeyboardSetUnicodeString` 在部分 App 里
/// 只有最后一个字符生效，逐字符是实测更稳的形态。
pub fn type_char(pid: u32, character: char) -> Result<()> {
    let (code, flags) = char_to_keycode(character).ok_or_else(|| {
        anyhow!(
            "cannot type `{character}` via target PID events: only ASCII printable characters \
             are supported. Non-Latin text cannot be typed into a background app this way — \
             use an accessibility element write, the `key` action, or ask the user to type it."
        )
    })?;
    let before = crate::accessibility::frontmost_pid()?;
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

/// ANSI 布局下 ASCII 可打印字符到 (keycode, flags) 的映射。
///
/// **直接查 [`key_code`]，不另写一张表**。字母区看起来像 a=0、b=1…，实际不是：
/// QWERTY 下 `z` 是 keycode 6、`s` 是 1、`d` 是 2，而 `a` 是 0、`w` 是 13。
/// 任何「小写字母按 ASCII 减 'a'」的算术式都会错得离谱（`z` 会算出 25，真值 6，
/// 落在数字 3 的键位上）。
///
/// 数字行同样反直觉：1-0 对应 18..29，且 5/6 交换（6 在 22、5 在 23）。
///
/// 需要 shift 的字符（大写字母、`!@#$%^&*()`、`_+{}|:"<>?~`）返回
/// `(未 shift 的键位, MaskShift)`：键位与未 shift 版本相同，只是按下时要多带
/// 一个 flag——这正是不能另写一张表的原因，shift 版本与基础版本必须共用键位。
fn char_to_keycode(character: char) -> Option<(CGKeyCode, CGEventFlags)> {
    if !character.is_ascii_graphic() && character != ' ' {
        return None;
    }
    // 大写字母必须带 shift flag，而 `key_code()` 不返回 flags——所以这里
    // 先剥掉大小写，把 flag 单独算出来，而不是为 shift 单独维护第二张表
    // （shift 大写的键位与小写完全相同，重复一遍只会引入漂移）。
    let (base, flags) = if character.is_ascii_uppercase() {
        (character.to_ascii_lowercase(), CGEventFlags::MaskShift)
    } else {
        (character, CGEventFlags::empty())
    };
    // shift 字符：键位与未 shift 版本相同，但必须带 MaskShift，统一在这里
    // 折成「查基础键 + 加 shift」，避免每个分支各写一遍 return 早退。
    let shifted_base = match base {
        '{' => Some("["),
        '}' => Some("]"),
        '|' => Some("\\"),
        ':' => Some(";"),
        '"' => Some("'"),
        '<' => Some(","),
        '>' => Some("."),
        '?' => Some("/"),
        '~' => Some("`"),
        '_' => Some("-"),
        '+' => Some("="),
        // 数字行上的 shift 符号。注意 5/6 在 ANSI 布局里是交换的，映射必须
        // 按**字符**走（`%` 是 shift+5，`^` 是 shift+6），不能按下标算。
        '!' => Some("1"),
        '@' => Some("2"),
        '#' => Some("3"),
        '$' => Some("4"),
        '%' => Some("5"),
        '^' => Some("6"),
        '&' => Some("7"),
        '*' => Some("8"),
        '(' => Some("9"),
        ')' => Some("0"),
        _ => None,
    };
    let name: String = match base {
        'a'..='z' | '0'..='9' => base.to_string(),
        ' ' => "space".to_string(),
        _ => match shifted_base {
            Some(base) => {
                return Some((key_code(base)?, CGEventFlags::MaskShift));
            }
            None => base.to_string(),
        },
    };
    Some((key_code(&name)?, flags))
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

    /// 剪贴板粘贴依赖 `cmd+v` 能被解析成「带 Command 修饰的 V」。
    /// 这条解析一旦漂移，中文输入整条通道会静默失效——`paste()` 内部就是
    /// `key(pid, "cmd+v")`，没有第二处可回退的定义。
    #[test]
    fn paste_combo_resolves_to_command_v() {
        let (code, flags) = parse_key("cmd+v").unwrap();
        // ANSI 布局里 V 是 keycode 9。
        assert_eq!(code, 9);
        assert!(flags.contains(CGEventFlags::MaskCommand));
        assert!(!flags.contains(CGEventFlags::MaskShift));
    }

    /// 字母与数字的键位映射是后台输入的基础：错了就是「点了搜索框但搜的是别的
    /// 东西」这种极难自查的失败。
    #[test]
    fn ascii_characters_map_to_their_ansi_keycodes() {
        let code = |character| char_to_keycode(character).map(|(code, _)| code);
        assert_eq!(code('a'), Some(0));
        assert_eq!(code('w'), Some(13));
        // QWERTY 字母区不是连续的：s=1、d=2、z=6。任何「减 'a'」的算术式
        // 都会把 z 映射到 25（数字 3 的键位），实测踩过。
        assert_eq!(code('s'), Some(1));
        assert_eq!(code('z'), Some(6));
        assert_eq!(code(' '), Some(49));
        // 数字行不连续，且 5/6 交换：1→18, 5→23, 6→22, 9→25, 0→29。
        assert_eq!(code('1'), Some(18));
        assert_eq!(code('5'), Some(23));
        assert_eq!(code('6'), Some(22));
        assert_eq!(code('9'), Some(25));
        assert_eq!(code('0'), Some(29));
        assert_eq!(code('.'), Some(47));
        assert_eq!(code('/'), Some(44));
    }

    /// shift 字符与未 shift 版本**共用键位**，只是多带一个 MaskShift flag。
    /// 这一点错了就是「打出小写字母却以为是大写」这类静默错误。
    #[test]
    fn shifted_characters_share_the_keycode_and_add_shift() {
        for (plain, shifted) in [
            ('a', 'A'),
            ('z', 'Z'),
            ('1', '!'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('0', ')'),
            ('-', '_'),
            ('=', '+'),
            ('[', '{'),
            (';', ':'),
            ('\'', '"'),
            ('\\', '|'),
            (',', '<'),
            ('.', '>'),
            ('/', '?'),
            ('`', '~'),
        ] {
            let (plain_code, plain_flags) = char_to_keycode(plain).unwrap();
            let (shifted_code, shifted_flags) = char_to_keycode(shifted).unwrap();
            assert_eq!(
                plain_code, shifted_code,
                "`{plain}` 与 `{shifted}` 必须落在同一个键位上"
            );
            assert!(
                !plain_flags.contains(CGEventFlags::MaskShift),
                "`{plain}` 不该带 shift"
            );
            assert!(
                shifted_flags.contains(CGEventFlags::MaskShift),
                "`{shifted}` 必须带 shift"
            );
        }
    }

    /// `char_to_keycode` 与 `key_code` 是**同一张 ANSI 键位表的两个入口**
    /// （一个按键名、一个按键字符）。两张表各写一遍时必然会有一处先漂移——
    /// 本仓库真的发生过：数字行写成算术式 `17 + digit` 时，5 与 6 整体错位。
    #[test]
    fn the_two_keycode_tables_never_drift_apart() {
        for (character, name) in [
            ('a', "a"),
            ('z', "z"),
            ('w', "w"),
            ('1', "1"),
            ('5', "5"),
            ('6', "6"),
            ('9', "9"),
            ('0', "0"),
            ('.', "."),
            ('/', "/"),
        ] {
            assert_eq!(
                char_to_keycode(character).map(|(code, _)| code),
                key_code(name),
                "`{character}` 与按键名 `{name}` 必须落在同一个 keycode 上"
            );
        }
    }

    /// 中文与非 Latin 字符必须明确报错而不是静默丢字——后台输入走不通时，
    /// 模型需要知道「这条路不通」才能换方案。
    #[test]
    fn non_ascii_is_refused_with_a_reason() {
        for character in ['望', 'あ', '€', '\n', '\t'] {
            assert_eq!(
                char_to_keycode(character),
                None,
                "must refuse `{character}` rather than type a wrong key"
            );
        }
        assert!(type_char(0, '望').is_err());
    }
}
