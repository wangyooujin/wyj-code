//! 剪贴板文本通道：给「AX 写不进去、又不是纯 ASCII」的输入场景兜底。
//!
//! # 为什么需要它
//!
//! `app_computer` 的 `set_text` 有两条降级路径，覆盖不到中文：
//!
//! 1. **AXValue / AXSelectedText**——自绘应用（微信）在目标点根本不做
//!    hit-test，`kAXErrorNotImplemented`，必然失败；
//! 2. **`CGEventPostToPid` 逐字符按键**——`CGEventKeyboardSetUnicodeString`
//!    对非 ASCII 无效，而 keycode 表只有 ANSI 那 60 多个位置。于是
//!    `targeted_event::type_char` 对任何非 ASCII 字符**显式报错**。
//!
//! 结果是：**中文用户最常用的应用（微信）连一句话都发不出去**。这不是
//! 边角场景，是主要场景。
//!
//! 剪贴板是绕过 IME 的标准做法：写入系统剪贴板 → 向目标 PID 投递 `cmd+V`
//! → 应用自己走正常的粘贴路径。它绕开了「逐字符合成」这个根本走不通的方向，
//! 因为粘贴是应用层的语义动作，不关心内容是什么文字。
//!
//! # 两条硬约束
//!
//! 1. **必须恢复用户原来的剪贴板**。剪贴板是用户的全局状态，写进去不还原
//!    等于偷走了他复制的东西。用 RAII 守卫（[`ClipboardGuard`]），正常路径
//!    与错误路径都复原。
//! 2. **恢复失败不改变工具结果**。与 `tools::spill` 落盘失败同一条原则：
//!    粘贴成功这件事已经发生了，回执必须是「成功」；剪贴板没还原是另一个
//!    问题，只在文本里说明。
//!
//! # 平台范围
//!
//! 仅 macOS。Windows 侧的后台目标化动作本就未实现（见 `crates/tools/CLAUDE.md`），
//! 这里不提前造一套没有调用方的实现。

use anyhow::Result;

/// 剪贴板里一段可恢复的内容。
///
/// 只保存**纯文本**：模型要粘贴的永远是文本，而完整保真（图片、文件 URL、
/// 富文本）需要逐类型快照 `NSPasteboardItem`，代价远大于收益——用户的剪贴板
/// 在 Agent 干活这几秒内被换成纯文本，恢复后富文本丢失的概率极低，且不会
/// 造成错误操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    /// 原来的纯文本内容。`None` 表示剪贴板里没有文本（可能是图片/文件）。
    pub text: Option<String>,
}

impl ClipboardSnapshot {
    /// 恢复是否可能完整：剪贴板原本没有文本时我们无法还原它的原始类型。
    pub fn is_restorable(&self) -> bool {
        self.text.is_some()
    }
}

/// 读取当前剪贴板的纯文本内容。
#[cfg(target_os = "macos")]
pub fn snapshot() -> Result<ClipboardSnapshot> {
    imp::snapshot()
}

#[cfg(not(target_os = "macos"))]
pub fn snapshot() -> Result<ClipboardSnapshot> {
    anyhow::bail!("clipboard text injection is only implemented on macOS")
}

/// 把 `text` 写进系统剪贴板，替换原有内容。
#[cfg(target_os = "macos")]
pub fn write_text(text: &str) -> Result<()> {
    imp::write_text(text)
}

#[cfg(not(target_os = "macos"))]
pub fn write_text(_text: &str) -> Result<()> {
    anyhow::bail!("clipboard text injection is only implemented on macOS")
}

/// 把剪贴板恢复成 `snapshot` 记录的内容。
///
/// 原本没有文本时**什么也不做**——强行写入空串会把用户复制的图片/文件替换成
/// 一个空字符串，比不恢复更糟。
#[cfg(target_os = "macos")]
pub fn restore(snapshot: &ClipboardSnapshot) -> Result<()> {
    match &snapshot.text {
        Some(text) => imp::write_text(text),
        None => Ok(()),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn restore(_snapshot: &ClipboardSnapshot) -> Result<()> {
    Ok(())
}

/// 作用域内的剪贴板借用：进入时记下原内容，离开时无条件复原。
///
/// 为什么必须是 RAII 而不是「粘贴完记得调 restore」：中间的 `?` 早退、
/// panic、以及调用方忘了收尾，都会把用户的剪贴板永久换成 Agent 写的内容。
/// 与 `cursor_click::CursorRestore`、`backend::PressedMouse` 同一个模式。
pub struct ClipboardGuard {
    snapshot: ClipboardSnapshot,
    armed: bool,
}

impl ClipboardGuard {
    /// 写入 `text` 并返回守卫。离开作用域时剪贴板复原。
    pub fn write(text: &str) -> Result<Self> {
        let snapshot = snapshot()?;
        let mut guard = Self {
            snapshot,
            armed: true,
        };
        if let Err(error) = write_text(text) {
            // 写入失败时什么都没改，不需要复原。
            guard.armed = false;
            return Err(error);
        }
        Ok(guard)
    }

    /// 当前内容是否还能完整复原（原本没有文本时为 false）。
    pub fn is_restorable(&self) -> bool {
        self.snapshot.is_restorable()
    }

    /// 提前复原并解除守卫，返回复原错误供调用方决定是否报告。
    pub fn restore_now(mut self) -> Result<()> {
        self.armed = false;
        restore(&self.snapshot)
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if self.armed {
            // 复原失败无处上报（可能正在 unwind），但把用户复制的东西换成
            // Agent 的文本比报错更糟，所以尽力而为。
            let _ = restore(&self.snapshot);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ClipboardSnapshot;
    use anyhow::Result;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    pub fn snapshot() -> Result<ClipboardSnapshot> {
        let pasteboard = NSPasteboard::generalPasteboard();
        // SAFETY: `NSPasteboardTypeString` 是系统提供的静态字符串常量。
        let text = pasteboard
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|value| value.to_string());
        Ok(ClipboardSnapshot { text })
    }

    pub fn write_text(text: &str) -> Result<()> {
        let pasteboard = NSPasteboard::generalPasteboard();
        // `clearContents` 必须在写之前调用：NSPasteboard 是「一次写入一个
        // 内容」，不清空的话 `setString:forType:` 会在旧内容上叠加，某些
        // 应用读到的仍是上一次的东西。
        pasteboard.clearContents();
        let value = NSString::from_str(text);
        // SAFETY: `NSPasteboardTypeString` 是系统提供的静态字符串常量。
        let ok = pasteboard.setString_forType(&value, unsafe { NSPasteboardTypeString });
        anyhow::ensure!(ok, "failed to write text to the system pasteboard");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_with_text_is_restorable() {
        let snapshot = ClipboardSnapshot {
            text: Some("user copied this".into()),
        };
        assert!(snapshot.is_restorable());
    }

    /// 原本没有文本（剪贴板里是图片/文件）时**不能**声称可恢复：`restore`
    /// 会跳过写入，强行写空串会把用户的图片换成一个空字符串。
    #[test]
    fn a_snapshot_without_text_is_not_restorable() {
        let snapshot = ClipboardSnapshot { text: None };
        assert!(!snapshot.is_restorable());
    }

    /// 空串与「没有文本」是两件事：空串是用户确实复制了一个空文本，
    /// 它可以被恢复。
    #[test]
    fn an_empty_string_is_still_a_restorable_text() {
        let snapshot = ClipboardSnapshot {
            text: Some(String::new()),
        };
        assert!(snapshot.is_restorable());
    }

    /// 真机验证：写入 → 读回 → 守卫离开后原内容复原。
    ///
    /// 默认跳过（会真实改动用户剪贴板）。真机验证时显式跑：
    ///
    /// ```text
    /// cargo test -p wyj-computer clipboard::tests::guard_restores_the_user_clipboard -- --ignored --nocapture
    /// ```
    ///
    /// 这条测试守护的是**中文输入通道的唯一前提**：如果 `write_text` 或
    /// `restore` 有一处不工作，`set_text` 的降级链会在用户毫无察觉的情况下
    /// 把他的剪贴板永久换成 Agent 的文本。
    #[test]
    #[ignore = "会真实改写用户剪贴板；仅真机验证时手动运行"]
    fn guard_restores_the_user_clipboard() {
        let original = "wyj-code clipboard probe: original".to_string();
        write_text(&original).unwrap();

        {
            let guard = ClipboardGuard::write("临时写入的中文内容").unwrap();
            assert!(guard.is_restorable());
            assert_eq!(
                snapshot().unwrap().text.as_deref(),
                Some("临时写入的中文内容"),
                "写入后必须能读回中文——这是 set_text 通道成立的前提"
            );
            // guard 在这里 drop。
        }

        assert_eq!(
            snapshot().unwrap().text.as_deref(),
            Some(original.as_str()),
            "守卫离开后必须恢复用户原来的剪贴板内容"
        );
    }
}
