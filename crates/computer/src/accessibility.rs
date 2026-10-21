//! macOS Accessibility 后台语义操作。
//!
//! 所有 hit-test 都限制在指定应用 PID；不会读取全局前台元素、移动光标或
//! 激活应用。坐标由 [`crate::target::WindowTarget`] 从最近窗口截图映射而来。

use anyhow::{anyhow, bail, Result};
use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType,
};
use objc2_core_foundation::{CFArray, CFBoolean, CFMutableArray, CFRetained, CFString, CFType};
use serde::Serialize;
use std::ptr::NonNull;

const AX_PRESS: &str = "AXPress";
const AX_INCREMENT: &str = "AXIncrement";
const AX_DECREMENT: &str = "AXDecrement";
/// 弹出元素的上下文菜单（右键菜单）。
///
/// 这是**唯一**能不合成右键事件就打开上下文菜单的途径。`menu.rs` 只覆盖
/// 菜单栏，而菜单栏里根本没有「复制图片」「添加到播放列表」这类动作——
/// 它们只存在于右键菜单。此前这条路径完全缺失，于是「右键」成了 GUI
/// 任务里唯一必须退回全局事件的操作。
const AX_SHOW_MENU: &str = "AXShowMenu";
/// 把元素滚动到可见区域内。
///
/// 元素在滚动区外时 `AXPress` 会返回 `ActionUnsupported`——不是「这个控件
/// 不能按」，而是「它现在不在可见范围里」。没有这一步，模型只能靠滚轮
/// 盲滚再重新列举，而滚多少完全靠猜。
const AX_SCROLL_TO_VISIBLE: &str = "AXScrollToVisible";
const AX_VALUE: &str = "AXValue";
const AX_SELECTED_TEXT: &str = "AXSelectedText";
const AX_ROLE: &str = "AXRole";
const AX_TITLE: &str = "AXTitle";
const AX_PARENT: &str = "AXParent";
const AX_FOCUSED_APPLICATION: &str = "AXFocusedApplication";
const AX_FRONTMOST: &str = "AXFrontmost";
const AX_WINDOWS: &str = "AXWindows";
const AX_CHILDREN: &str = "AXChildren";
const AX_POSITION: &str = "AXPosition";

/// 持有所有权的 AX 元素。AX 的 create/copy 家族全部返回 +1，交给
/// `CFRetained` 管理，避免遍历几百个节点时泄漏或提前释放。
pub type AXUIElementRef = CFRetained<AXUIElement>;

#[derive(Debug, Clone, Serialize)]
pub struct ElementInfo {
    pub pid: u32,
    pub role: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    pub value_settable: bool,
    pub selected_text_settable: bool,
}

/// 设置 AX 全局 messaging 超时（秒）。
///
/// macOS 默认 6 秒。AX 元素的 `CFRelease` 是一次跨进程消息，目标 App 的 AX
/// 线程若已挂起（窗口刚关闭、App 卡顿、元素已失效），释放就会一直等，最终
/// 以 SIGTRAP 崩掉我们的进程——真机实测 Chrome 上遍历 22 个元素后集中释放
/// 必然触发（exit 133），而 AXPress 本身是成功的。
///
/// 调短它让「等不到」变成「及时失败」，把崩溃换成可处理的错误。0 表示恢复
/// 系统默认。
pub fn set_messaging_timeout(seconds: f32) {
    // SAFETY: 参数为正数时设置全局超时；0 恢复默认。API 明确支持这两种用法。
    unsafe {
        let system = AXUIElement::new_system_wide();
        let _ = system.set_messaging_timeout(seconds);
    }
}

pub fn is_process_trusted() -> bool {
    // SAFETY: parameterless system query.
    unsafe { AXIsProcessTrusted() }
}

/// 查询辅助功能授权，并在未授权时**弹出系统授权框**。
///
/// # 为什么只在显式入口调用
///
/// `kAXTrustedCheckOptionPrompt` 一旦为真，系统会在该进程下一次被检查时弹出
/// 「"wyj-code"想要控制这台电脑」的对话框。这个弹窗是模态级打扰：
///
/// - 它不受 TUI 控制，用户可能正在终端里看别的东西；
/// - headless / cron / 子 Agent 场景下弹一个没人看的框毫无意义；
/// - 反复调用会让系统把它当成骚扰，macOS 会静默拒绝后续请求。
///
/// 所以本函数**只由用户显式触发的路径**调用：`/computer` 诊断命令。
/// 工具内部一律走只读的 [`is_process_trusted`]，失败时返回
/// `accessibility_permission_required` 让模型停下并告知用户。
///
/// 返回值表示**当前**是否已授权（提示是异步的，不改变本次返回值）。
pub fn request_trust_prompt() -> bool {
    use objc2_application_services::{kAXTrustedCheckOptionPrompt, AXIsProcessTrustedWithOptions};
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};

    // SAFETY: `kAXTrustedCheckOptionPrompt` 是系统提供的静态字符串常量。
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let value: CFRetained<CFType> = CFBoolean::new(true).into();
    let values: [&CFType; 1] = [&value];
    let options: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[key], &values);
    // 非泛型 `CFDictionary` 是这个 API 的入参类型，做一次类型擦除。
    let options: &CFDictionary = options.as_opaque();
    // SAFETY: options 的键是 CFString、值是 CFBoolean，符合文档要求的类型。
    unsafe { AXIsProcessTrustedWithOptions(Some(options)) }
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

/// 沿祖先链上溯执行 AXPress。
///
/// 元素本身常常不是可按下的（如一个 `AXStaticText` 真正的点击目标是它的
/// `AXButton` 祖先），所以要像坐标版本一样逐层上溯，最多 6 层。
fn press_chain(chain: &[AXUIElementRef]) -> Result<()> {
    let action = CFString::from_str(AX_PRESS);
    let mut last_error = AXError::ActionUnsupported;
    for element in chain {
        // SAFETY: `element` is retained; AXPerformAction takes a borrowed ref.
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

/// 对已解析的元素句柄执行 AXPress（含父链上溯）。
///
/// B 期的执行入口：元素由 `element_ref` 定位而来，不经坐标命中，因此
/// 不会误点到同位置的另一个控件。
pub fn press_handle(handle: &AXUIElement) -> Result<()> {
    require_trusted()?;
    let mut chain = Vec::new();
    let mut current = // SAFETY: `handle` 由调用方保证在本次调用期间存活；retain 只做 +1，不转移所有权。
        unsafe { CFRetained::retain(NonNull::from(handle)) }.into();
    for _ in 0..6 {
        let Some(element) = current else { break };
        chain.push(element.clone());
        current = parent(&element)?;
    }
    press_chain(&chain)
}

/// 对已解析的元素句柄弹出上下文菜单（`AXShowMenu`）。
///
/// # 为什么只尝试元素本身，不上溯父链
///
/// `AXPress` 需要上溯是因为「按钮的文字子节点」本身不可按，真正的目标是
/// 它的祖先。`AXShowMenu` 不同：它作用在**容器**上（列表行、图片、文本
/// 选区），而这些容器通常就是元素本身；上溯到父容器反而会弹出「整个列表」
/// 的菜单而不是「这一行」的菜单——那会让模型选到一个语义完全不同的动作。
///
/// 因此这里只试元素本身，失败就如实报错并建议模型改用坐标右键
/// （`click` + `click_kind: right`），后者能精确命中。
pub fn show_menu_handle(handle: &AXUIElement) -> Result<()> {
    require_trusted()?;
    let action = CFString::from_str(AX_SHOW_MENU);
    // SAFETY: `handle` 由调用方保证在本次调用期间存活；perform_action 只借用。
    let error = unsafe { handle.perform_action(&action) };
    if error == AXError::Success {
        return Ok(());
    }
    bail!(
        "requires_foreground_takeover: this element does not expose AXShowMenu (AXError({})); \
         use `click` with `click_kind: right` at the element's centre instead",
        error.0
    )
}

/// 把元素滚动到可见区域内（`AXScrollToVisible`）。
///
/// 返回 `Ok(true)` 表示滚动动作被接受；`Ok(false)` 表示该元素不支持
/// （不一定是错误：很多元素本来就在可见区内，不需要滚动）。
///
/// 调用方应在 `AXPress` 失败**之后**、改用坐标**之前**尝试它：元素在
/// 滚动区外时 `AXPress` 报的是 `ActionUnsupported`，而那不代表控件不可按。
pub fn scroll_to_visible_handle(handle: &AXUIElement) -> Result<bool> {
    require_trusted()?;
    let action = CFString::from_str(AX_SCROLL_TO_VISIBLE);
    // SAFETY: `handle` 由调用方保证在本次调用期间存活；perform_action 只借用。
    let error = unsafe { handle.perform_action(&action) };
    Ok(error == AXError::Success)
}

/// 对已解析的元素句柄写入文本（AXValue / AXSelectedText）。
pub fn set_text_handle(handle: &AXUIElement, text: &str) -> Result<&'static str> {
    require_trusted()?;
    let value = CFString::from_str(text);
    let value_as_type: CFRetained<CFType> = value.into();
    let mut current = // SAFETY: `handle` 由调用方保证在本次调用期间存活；retain 只做 +1，不转移所有权。
        unsafe { CFRetained::retain(NonNull::from(handle)) }.into();
    for _ in 0..6 {
        let Some(element) = current else { break };
        for (attribute_name, result_name) in
            [(AX_VALUE, "AXValue"), (AX_SELECTED_TEXT, "AXSelectedText")]
        {
            if !is_settable(&element, attribute_name) {
                continue;
            }
            let attribute = CFString::from_str(attribute_name);
            // SAFETY: `element` is retained and `attribute`/`value_as_type` live through the call.
            let error = unsafe { element.set_attribute_value(&attribute, &value_as_type) };
            if error == AXError::Success {
                return Ok(result_name);
            }
        }
        current = parent(&element)?;
    }
    bail!("requires_foreground_takeover: target element exposes no settable AXValue/AXSelectedText")
}

/// 对已解析的元素句柄做语义滚动。`Ok(false)` 表示该方向不适用。
pub fn scroll_handle(handle: &AXUIElement, direction: &str, amount: u32) -> Result<bool> {
    let action_name = match direction {
        "up" => AX_DECREMENT,
        "down" => AX_INCREMENT,
        "left" | "right" => return Ok(false),
        other => bail!("invalid scroll direction `{other}`"),
    };
    require_trusted()?;
    let action = CFString::from_str(action_name);
    let mut current = // SAFETY: `handle` 由调用方保证在本次调用期间存活；retain 只做 +1，不转移所有权。
        unsafe { CFRetained::retain(NonNull::from(handle)) }.into();
    for _ in 0..6 {
        let Some(element) = current else { break };
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
        current = parent(&element)?;
    }
    Ok(false)
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

/// 一次 `copy_multiple_attribute_values` 读回的属性集合。
///
/// 遍历性能的全部来源：逐属性读 3000 节点 = 15000 次 IPC（访达实测 18.2s），
/// 批量读把每节点的固定开销压到 **2 次**（1 次批量 + 1 次 `copy_action_names`），
/// 使 400 节点的遍历落在 1.2s 级别。
///
/// 三个属性（`AXChildren` / `AXIdentifier` / `AXMenuItemCmdChar`）曾经是事后
/// 单独读的，那等于把「每节点 2 次」直接加到 5 次——它们是普通属性，批量读
/// 对「该属性不存在」的节点只是多一个数组槽位，代价为零。
pub struct BatchAttributes {
    pub role: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub selected: Option<bool>,
    /// `AXPosition`，全局逻辑点。
    pub position: Option<(f64, f64)>,
    /// `AXSize`，全局逻辑点。
    pub size: Option<(f64, f64)>,
    /// 恒为 `false`。真正的值由 [`settable_flags`] 在保留判定之后补读，
    /// 因为 `AXUIElementIsAttributeSettable` 没有批量接口。
    pub settable: bool,
    pub selected_text_settable: bool,
    pub actions: Vec<String>,
    pub children: Vec<AXUIElementRef>,
    /// `AXIdentifier`：开发者给的稳定选择器（菜单项上是 selector，如 `NewTab`）。
    pub identifier: Option<String>,
    /// `AXMenuItemCmdChar`：菜单项显示的快捷键字符。
    pub shortcut: Option<String>,
}

impl BatchAttributes {
    fn empty() -> Self {
        Self {
            role: None,
            title: None,
            description: None,
            value: None,
            enabled: None,
            focused: None,
            selected: None,
            position: None,
            size: None,
            settable: false,
            selected_text_settable: false,
            actions: Vec::new(),
            children: Vec::new(),
            identifier: None,
            shortcut: None,
        }
    }
}

/// 批量读取的属性名，顺序与 [`batch_attributes`] 的解析下标一一对应。
///
/// # 为什么这里要尽量多装
///
/// 每多一个属性就多一次跨进程往返，而遍历是**逐节点**的：400 个节点的遍历
/// 里，每多 1 次 IPC 就多 400 次往返（实测约 0.4ms/次，即 +160ms）。因此凡是
/// 「大部分节点都可能有、且形状固定」的属性都应该塞进这一批，而不是事后用
/// `copy_attribute_value` 单独读。
///
/// `AXIdentifier` / `AXMenuItemCmdChar` 曾经是单独读的（注释理由是「只有菜单
/// 类元素才有，会拖慢所有节点」）——那个理由站不住：批量读对「该属性不存在」
/// 的节点只多一次数组槽位，代价为零；而单独读对**每个**节点都多一次完整
/// 往返。这两项在控件节点上确实多半为空，但在菜单项上正是关键信息。
const BATCH_ATTRS: [&str; 12] = [
    "AXRole",
    "AXTitle",
    "AXDescription",
    "AXValue",
    "AXEnabled",
    "AXFocused",
    "AXSelected",
    "AXPosition",
    "AXSize",
    // 子节点数组一并批量读：它是遍历的骨架，每个节点都要，单独读等于把
    // 「每节点 2 次 IPC」直接翻倍。
    "AXChildren",
    "AXIdentifier",
    "AXMenuItemCmdChar",
];

/// 把 `CFRetained<CFType>` 降级成字符串属性。
fn as_string(value: &CFRetained<CFType>) -> Option<String> {
    let text: CFRetained<CFString> = value.clone().downcast::<CFString>().ok()?;
    Some(text.to_string())
}

/// 把 `CFRetained<CFType>` 降级成布尔属性。
fn as_bool(value: &CFRetained<CFType>) -> Option<bool> {
    value
        .clone()
        .downcast::<CFBoolean>()
        .ok()
        .map(|flag| flag.as_bool())
}

/// 从 `AXValue` 里读出一个二元数值（CGPoint / CGSize 共用）。
///
/// 必须按目标类型读：`AXPosition` 是 **CGPoint**，`AXSize` 是 **CGSize**，
/// 两者 AXValue 类型不同。用同一种类型去读会被静默拒掉，表现为「元素没有几何」，
/// 而探路阶段正是先踩了这个坑才把 `带几何的元素` 从 0 修到 3000。
fn as_ax_pair(value: &CFRetained<CFType>, kind: AXValueType) -> Option<(f64, f64)> {
    let ax: CFRetained<AXValue> = value.clone().downcast::<AXValue>().ok()?;
    // SAFETY: `pair` 是栈上 out-param，且 `ax` 的类型已在下一行核对过。
    unsafe {
        if ax.r#type().0 != kind.0 {
            return None;
        }
        let mut pair = [0.0f64; 2];
        if !ax.value(kind, NonNull::from(&mut pair).cast()) {
            return None;
        }
        Some((pair[0], pair[1]))
    }
}

/// 读一个节点的全部常用属性 + 动作名 + 子节点。
///
/// `copy_multiple_attribute_values` 的 `options` 传 `empty()`：单个属性失败
/// 不会中断整批（错误以 `kAXValueAXErrorType` 占位在对应位置），这对
/// 「有些节点没有 AXValue / AXPosition」是必需的。
///
/// # 本函数的 IPC 预算：2 次
///
/// 批量读 1 次 + `copy_action_names` 1 次。刻意**不**再读 `settable`
/// （见下）也不重复检查授权——后者本地开销虽小，但遍历 400 个节点就白跑
/// 400 次，由调用方在循环外查一次即可。
///
/// 注意 `settable` 字段在本函数返回时**恒为 false**：它由调用方在保留判定
/// 之后用 [`settable_flags`] 补读。任何直接使用 `BatchAttributes::settable`
/// 却不补读的代码都会拿到错误结论。
pub fn batch_attributes(element: &AXUIElement) -> BatchAttributes {
    let mut out = BatchAttributes::empty();

    let names: CFRetained<CFMutableArray<CFString>> =
        CFMutableArray::with_capacity(BATCH_ATTRS.len());
    for name in BATCH_ATTRS {
        names.append(&CFString::from_str(name));
    }
    let names = names.as_opaque();

    let mut raw: *const CFArray = std::ptr::null();
    // SAFETY: `names`/`raw` 均符合 API 契约，`raw` 是有效 out-pointer。
    let error = unsafe {
        element.copy_multiple_attribute_values(
            names,
            AXCopyMultipleAttributeOptions::empty(),
            NonNull::from(&mut raw),
        )
    };
    if error == AXError::Success && !raw.is_null() {
        let Some(pointer) = NonNull::new(raw.cast_mut()) else {
            return out;
        };
        // SAFETY: 成功时 API 返回 +1 retained array。
        let values = unsafe { CFRetained::<CFArray>::from_raw(pointer) };
        // SAFETY: 返回数组的元素全部是 CFType。
        let values: &CFArray<CFType> = unsafe { values.cast_unchecked::<CFType>() };
        let values = values.to_vec();
        let at = |index: usize| values.get(index).cloned();

        if let Some(value) = at(0) {
            out.role = as_string(&value);
        }
        if let Some(value) = at(1) {
            out.title = as_string(&value);
        }
        if let Some(value) = at(2) {
            out.description = as_string(&value);
        }
        if let Some(value) = at(3) {
            out.value = as_string(&value);
        }
        if let Some(value) = at(4) {
            out.enabled = as_bool(&value);
        }
        if let Some(value) = at(5) {
            out.focused = as_bool(&value);
        }
        if let Some(value) = at(6) {
            out.selected = as_bool(&value);
        }
        if let Some(value) = at(7) {
            out.position = as_ax_pair(&value, AXValueType::CGPoint);
        }
        if let Some(value) = at(8) {
            out.size = as_ax_pair(&value, AXValueType::CGSize);
        }
        // 槽位 9：`AXChildren` 是 CFArray。批量读成功时它已经是 +1 owned
        // 的元素数组，直接逐个 downcast，不需要再发一次 copy。
        if let Some(value) = at(9) {
            if let Some(array) = value.downcast_ref::<CFArray>() {
                // SAFETY: AX 数组属性的元素全部是 CFType。
                let typed: &CFArray<CFType> = unsafe { array.cast_unchecked::<CFType>() };
                out.children = typed
                    .to_vec()
                    .into_iter()
                    .filter_map(|item: CFRetained<CFType>| item.downcast::<AXUIElement>().ok())
                    .collect();
            }
        }
        if let Some(value) = at(10) {
            out.identifier = as_string(&value);
        }
        if let Some(value) = at(11) {
            out.shortcut = as_string(&value);
        }
    }

    // `settable` 刻意**不在这里读**：`AXUIElementIsAttributeSettable` 没有批量
    // 接口，每读一次就是一次往返。而它只对「最终会展示给模型的元素」有意义
    // ——`should_keep` 会淘汰掉绝大多数访问过的节点（同花顺实测可交互率 1%）。
    // 由调用方在保留判定之后调 [`settable_flags`] 补读，IPC 次数从「每节点 2 次」
    // 降到「每保留元素 2 次」。
    //
    // `is_process_trusted()` 同理不在这里调：它虽然是本地调用，但每次遍历
    // 400 个节点就白跑 400 次。由 [`crate::element`] 的遍历入口统一检查。
    out.actions = action_names(element);
    out
}

/// 补读两个 `settable` 标志（各一次 IPC）。
///
/// 只有通过 `should_keep` 保留的元素才需要它——模型据此决定 `set_text` 走
/// AX 写入还是剪贴板粘贴。
pub fn settable_flags(element: &AXUIElement) -> (bool, bool) {
    (
        is_settable(element, AX_VALUE),
        is_settable(element, AX_SELECTED_TEXT),
    )
}

/// 取得一个 application 元素（菜单栏挂在它下面）。
pub fn find_application_element(pid: u32) -> Result<Option<AXUIElementRef>> {
    require_trusted()?;
    let pid_i32 = i32::try_from(pid).map_err(|_| anyhow!("invalid pid {pid}"))?;
    // SAFETY: `pid_i32` 来自 xcap 的窗口枚举，是有效进程 id；create 返回 +1。
    let app = unsafe { AXUIElement::new_application(pid_i32) };
    // SAFETY: `app` 是刚创建的 +1 引用，在此期间存活；retain 再 +1 后即转交所有权。
    Ok(Some(unsafe {
        CFRetained::retain(std::ptr::NonNull::from(app.as_ref()))
    }))
}

/// application 元素的直接子节点（AXWindow / AXMenuBar / AXScrollArea …）。
pub fn application_children(app: &AXUIElement) -> Vec<AXUIElementRef> {
    child_elements(app)
}

/// 规范化菜单项标签。
///
/// AX 的 `AXTitle` 常带尾随空白甚至整串为空（分隔线）。空串与纯空白统一
/// 归一为 `None`，让调用方不必各自处理。
pub fn normalize_label(raw: Option<String>) -> Option<String> {
    let value = raw?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 读 `AXChildren` 数组。
fn child_elements(element: &AXUIElement) -> Vec<AXUIElementRef> {
    let Ok(Some(value)) = copy_attribute(element, AX_CHILDREN) else {
        return Vec::new();
    };
    let Some(array) = value.downcast_ref::<CFArray>() else {
        return Vec::new();
    };
    // SAFETY: AX 数组属性的元素全部是 CFType。
    let typed: &CFArray<CFType> = unsafe { array.cast_unchecked::<CFType>() };
    typed
        .to_vec()
        .into_iter()
        .filter_map(|item: CFRetained<CFType>| {
            // to_vec() 返回的已是 +1 owned 元素，直接 downcast 转移所有权。
            item.downcast::<AXUIElement>().ok()
        })
        .collect()
}

/// 读元素支持的动作名。
fn action_names(element: &AXUIElement) -> Vec<String> {
    let mut raw: *const CFArray = std::ptr::null();
    // SAFETY: `raw` 是有效 out-pointer。
    let error = unsafe { element.copy_action_names(NonNull::from(&mut raw)) };
    if error != AXError::Success || raw.is_null() {
        return Vec::new();
    }
    let Some(pointer) = NonNull::new(raw.cast_mut()) else {
        return Vec::new();
    };
    // SAFETY: 成功时 API 返回 +1 retained array。
    let array = unsafe { CFRetained::<CFArray>::from_raw(pointer) };
    // SAFETY: 动作名数组的元素全部是 CFString。
    let typed: &CFArray<CFType> = unsafe { array.cast_unchecked::<CFType>() };
    typed
        .to_vec()
        .into_iter()
        .filter_map(|item: CFRetained<CFType>| {
            let name: &CFString = item.downcast_ref::<CFString>()?;
            Some(name.to_string())
        })
        .collect()
}

/// 从应用元素读 `AXWindows`。
fn window_elements(pid: u32) -> Result<Vec<AXUIElementRef>> {
    let pid_i32 = i32::try_from(pid).map_err(|_| anyhow!("invalid target pid {pid}"))?;
    // SAFETY: `pid_i32` 是有效进程 id；create API 返回 +1 引用。
    let application = unsafe { AXUIElement::new_application(pid_i32) };
    let Some(value) = copy_attribute(&application, AX_WINDOWS)? else {
        return Ok(Vec::new());
    };
    let Some(array) = value.downcast_ref::<CFArray>() else {
        return Ok(Vec::new());
    };
    // SAFETY: AX 数组属性的元素全部是 CFType。
    let typed: &CFArray<CFType> = unsafe { array.cast_unchecked::<CFType>() };
    Ok(typed
        .to_vec()
        .into_iter()
        .filter_map(|item: CFRetained<CFType>| {
            // to_vec() 返回的已是 +1 owned 元素，直接 downcast 转移所有权。
            item.downcast::<AXUIElement>().ok()
        })
        .collect())
}

/// 读元素的 `AXPosition`（全局逻辑点）。
fn element_position(element: &AXUIElement) -> Option<(f64, f64)> {
    let Ok(Some(value)) = copy_attribute(element, AX_POSITION) else {
        return None;
    };
    as_ax_pair(&value, AXValueType::CGPoint)
}

/// 定位目标窗口对应的 AX 元素。
///
/// 不能用 `AXFocusedWindow`——后台窗口拿不到焦点（真机已验证 `AXWindows`
/// 对 Chrome / 同花顺 / 访达均可枚举）。策略是先用 `AXIdentifier` 精确匹配，
/// 再退到 `AXTitle`，最后退到「左上角与窗口原点最接近」，因为实测同花顺的
/// `AXTitle` 是空串（`accessibility.rs` 的 `AXTitle` 常量对它无效）。
///
/// 返回 `Ok(None)` 表示该 App 不暴露 AX 树，调用方据此走坐标降级路径
/// （而不是把它当成「空元素列表」，否则模型会反复重试）。
pub fn find_window_element(target: &crate::target::WindowTarget) -> Result<Option<AXUIElementRef>> {
    require_trusted()?;
    let windows = window_elements(target.pid)?;
    if windows.is_empty() {
        return Ok(None);
    }

    // 命中判据：左上角与窗口原点最接近。AX 的 AXTitle 在部分 App 上是空串
    // 或与窗口标题不一致，用它做主判据会误选同 App 的其他窗口
    // （`target.rs::frontmost_window` 记录的同花顺多窗口故障同源）。
    let mut best: Option<(f64, AXUIElementRef)> = None;
    for window in windows {
        let position = element_position(&window);
        let distance = position
            .map(|(x, y)| (x - f64::from(target.x)).abs() + (y - f64::from(target.y)).abs())
            .unwrap_or(f64::MAX);
        match &best {
            Some((current, _)) if *current <= distance => {}
            _ => best = Some((distance, window)),
        }
    }

    let Some((distance, window)) = best else {
        return Ok(None);
    };
    // 容忍 8pt：AX 报告的窗口边框与 xcap 的窗口矩形可能差一条边框。
    if distance > 8.0 && distance != f64::MAX {
        return Ok(None);
    }
    Ok(Some(window))
}
