//! macOS Accessibility 后台语义操作。
//!
//! 所有 hit-test 都限制在指定应用 PID；不会读取全局前台元素、移动光标或
//! 激活应用。坐标由 [`crate::target::WindowTarget`] 从最近窗口截图映射而来。

use anyhow::{anyhow, bail, Result};
use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement};
use objc2_core_foundation::{CFBoolean, CFRetained, CFString, CFType};
use serde::Serialize;
use std::ptr::NonNull;

const AX_PRESS: &str = "AXPress";
const AX_INCREMENT: &str = "AXIncrement";
const AX_DECREMENT: &str = "AXDecrement";
const AX_VALUE: &str = "AXValue";
const AX_SELECTED_TEXT: &str = "AXSelectedText";
const AX_ROLE: &str = "AXRole";
const AX_TITLE: &str = "AXTitle";
const AX_PARENT: &str = "AXParent";
const AX_FOCUSED_APPLICATION: &str = "AXFocusedApplication";
const AX_FRONTMOST: &str = "AXFrontmost";

#[derive(Debug, Clone, Serialize)]
pub struct ElementInfo {
    pub pid: u32,
    pub role: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    pub value_settable: bool,
    pub selected_text_settable: bool,
}

pub fn is_process_trusted() -> bool {
    // SAFETY: parameterless system query.
    unsafe { AXIsProcessTrusted() }
}

fn require_trusted() -> Result<()> {
    if is_process_trusted() {
        Ok(())
    } else {
        bail!(
            "accessibility_permission_required: enable wyj-code in System Settings > Privacy & Security > Accessibility"
        )
    }
}

fn ax_result(error: AXError, operation: &str) -> Result<()> {
    if error == AXError::Success {
        Ok(())
    } else {
        bail!("{operation} failed with AXError({})", error.0)
    }
}

fn retained_element(raw: *const AXUIElement, operation: &str) -> Result<CFRetained<AXUIElement>> {
    let raw = NonNull::new(raw.cast_mut())
        .ok_or_else(|| anyhow!("{operation} returned a null AX element"))?;
    // SAFETY: AX copy APIs follow CoreFoundation's create/copy ownership rule.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn retained_type(raw: *const CFType, operation: &str) -> Result<CFRetained<CFType>> {
    let raw = NonNull::new(raw.cast_mut())
        .ok_or_else(|| anyhow!("{operation} returned a null AX attribute"))?;
    // SAFETY: AX copy APIs follow CoreFoundation's create/copy ownership rule.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn element_pid(element: &AXUIElement) -> Result<u32> {
    let mut pid: libc::pid_t = 0;
    let error = unsafe { element.pid(NonNull::from(&mut pid)) };
    ax_result(error, "AXUIElementGetPid")?;
    u32::try_from(pid).map_err(|_| anyhow!("AX element returned invalid pid {pid}"))
}

/// 通过系统级 Accessibility 元素取得真实前台应用 PID。
///
/// 不能用“可见窗口中 is_focused=true”代替：无窗口应用、系统面板或窗口枚举
/// 短暂失败时会得到 `None`，从而把前台变化误判为“前后都没变”。后台动作把
/// 前台不变当作硬安全条件，因此这里在无法精确取得 PID 时失败关闭。
pub fn frontmost_pid() -> Result<u32> {
    require_trusted()?;
    // SAFETY: 创建系统级 AX 根元素，不激活或修改任何应用。
    let system = unsafe { AXUIElement::new_system_wide() };
    let focused = copy_attribute(&system, AX_FOCUSED_APPLICATION)?
        .ok_or_else(|| anyhow!("target_changed: AXFocusedApplication is unavailable"))?
        .downcast::<AXUIElement>()
        .map_err(|_| anyhow!("target_changed: AXFocusedApplication returned an invalid type"))?;
    element_pid(&focused)
}

/// 恢复一个已知应用为前台。只供用户明确开启的 foreground compatibility
/// 路径做现场恢复；后台 `app_computer` 绝不会调用它。
pub fn activate_application(pid: u32) -> Result<()> {
    require_trusted()?;
    let pid_i32 = i32::try_from(pid).map_err(|_| anyhow!("invalid target pid {pid}"))?;
    // SAFETY: `pid_i32` 是从之前精确观察到的应用 PID 转换而来。
    let application = unsafe { AXUIElement::new_application(pid_i32) };
    let attribute = CFString::from_str(AX_FRONTMOST);
    let value: &CFType = CFBoolean::new(true).as_ref();
    ax_result(
        unsafe { application.set_attribute_value(&attribute, value) },
        "restore AXFrontmost",
    )?;
    let restored = frontmost_pid()?;
    anyhow::ensure!(
        restored == pid,
        "target_changed: failed to restore foreground pid {pid} (current {restored})"
    );
    Ok(())
}

fn element_at(pid: u32, x: f64, y: f64) -> Result<CFRetained<AXUIElement>> {
    require_trusted()?;
    let pid_i32 = i32::try_from(pid).map_err(|_| anyhow!("invalid target pid {pid}"))?;
    // SAFETY: `pid_i32` is a validated process id; returned object is retained.
    let application = unsafe { AXUIElement::new_application(pid_i32) };
    let mut raw: *const AXUIElement = std::ptr::null();
    let error = unsafe {
        application.copy_element_at_position(x as f32, y as f32, NonNull::from(&mut raw))
    };
    ax_result(error, "AXUIElementCopyElementAtPosition")?;
    let element = retained_element(raw, "AXUIElementCopyElementAtPosition")?;
    let actual_pid = element_pid(&element)?;
    if actual_pid != pid {
        bail!("target_changed: AX hit-test returned pid {actual_pid}, expected target pid {pid}");
    }
    Ok(element)
}

fn copy_attribute(element: &AXUIElement, name: &str) -> Result<Option<CFRetained<CFType>>> {
    let attribute = CFString::from_str(name);
    let mut raw: *const CFType = std::ptr::null();
    let error = unsafe { element.copy_attribute_value(&attribute, NonNull::from(&mut raw)) };
    if error == AXError::NoValue || error == AXError::AttributeUnsupported {
        return Ok(None);
    }
    ax_result(error, &format!("copy {name}"))?;
    retained_type(raw, &format!("copy {name}")).map(Some)
}

fn copy_string_attribute(element: &AXUIElement, name: &str) -> Result<Option<String>> {
    Ok(copy_attribute(element, name)?
        .and_then(|value| value.downcast::<CFString>().ok())
        .map(|value| value.to_string()))
}

fn is_settable(element: &AXUIElement, name: &str) -> bool {
    let attribute = CFString::from_str(name);
    let mut settable: u8 = 0;
    let error = unsafe { element.is_attribute_settable(&attribute, NonNull::from(&mut settable)) };
    error == AXError::Success && settable != 0
}

fn parent(element: &AXUIElement) -> Result<Option<CFRetained<AXUIElement>>> {
    Ok(copy_attribute(element, AX_PARENT)?.and_then(|value| value.downcast::<AXUIElement>().ok()))
}

fn element_chain(start: CFRetained<AXUIElement>) -> Result<Vec<CFRetained<AXUIElement>>> {
    let mut chain = vec![start];
    for _ in 0..6 {
        let Some(next) = parent(chain.last().expect("chain is non-empty"))? else {
            break;
        };
        chain.push(next);
    }
    Ok(chain)
}

pub fn inspect(pid: u32, x: f64, y: f64) -> Result<ElementInfo> {
    let element = element_at(pid, x, y)?;
    Ok(ElementInfo {
        pid: element_pid(&element)?,
        role: copy_string_attribute(&element, AX_ROLE)?,
        title: copy_string_attribute(&element, AX_TITLE)?,
        value: copy_string_attribute(&element, AX_VALUE)?,
        value_settable: is_settable(&element, AX_VALUE),
        selected_text_settable: is_settable(&element, AX_SELECTED_TEXT),
    })
}

pub fn press(pid: u32, x: f64, y: f64) -> Result<()> {
    let chain = element_chain(element_at(pid, x, y)?)?;
    let action = CFString::from_str(AX_PRESS);
    let mut last_error = AXError::ActionUnsupported;
    for element in chain {
        let error = unsafe { element.perform_action(&action) };
        if error == AXError::Success {
            return Ok(());
        }
        last_error = error;
        if !matches!(
            error,
            AXError::ActionUnsupported | AXError::AttributeUnsupported | AXError::NoValue
        ) {
            break;
        }
    }
    bail!(
        "requires_foreground_takeover: target element does not expose AXPress (AXError({}))",
        last_error.0
    )
}

pub fn set_text(pid: u32, x: f64, y: f64, text: &str) -> Result<&'static str> {
    let chain = element_chain(element_at(pid, x, y)?)?;
    let value = CFString::from_str(text);
    let value_as_type: CFRetained<CFType> = value.into();
    for element in chain {
        for (attribute_name, result_name) in
            [(AX_VALUE, "AXValue"), (AX_SELECTED_TEXT, "AXSelectedText")]
        {
            if !is_settable(&element, attribute_name) {
                continue;
            }
            let attribute = CFString::from_str(attribute_name);
            let error = unsafe { element.set_attribute_value(&attribute, &value_as_type) };
            if error == AXError::Success {
                return Ok(result_name);
            }
        }
    }
    bail!("requires_foreground_takeover: target element exposes no settable AXValue/AXSelectedText")
}

/// 尝试用控件语义滚动。返回 `Ok(false)` 表示目标不支持该 AX action，调用方
/// 可以继续尝试定向 PID 事件，但绝不能回退到全局鼠标滚轮。
pub fn scroll(pid: u32, x: f64, y: f64, direction: &str, amount: u32) -> Result<bool> {
    let action_name = match direction {
        "up" => AX_DECREMENT,
        "down" => AX_INCREMENT,
        "left" | "right" => return Ok(false),
        other => bail!("invalid scroll direction `{other}`"),
    };
    let chain = element_chain(element_at(pid, x, y)?)?;
    let action = CFString::from_str(action_name);
    for element in chain {
        let first = unsafe { element.perform_action(&action) };
        if first == AXError::Success {
            for _ in 1..amount.clamp(1, 20) {
                ax_result(unsafe { element.perform_action(&action) }, action_name)?;
            }
            return Ok(true);
        }
        if !matches!(
            first,
            AXError::ActionUnsupported | AXError::AttributeUnsupported | AXError::NoValue
        ) {
            ax_result(first, action_name)?;
        }
    }
    Ok(false)
}
