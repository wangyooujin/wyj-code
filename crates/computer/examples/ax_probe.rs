//! 临时真机验证脚本（非交付物）：验证 AX 元素索引方案的三条前提假设。
//!
//! 1. `AXWindows` 能否从后台应用枚举到窗口元素（根定位前提）；
//! 2. 遍历能否走通，以及每类 App 的 AX 覆盖率（Electron vs AppKit vs 自绘）；
//! 3. `AXPosition`/`AXSize` 能否读出并映射回截图坐标。
//!
//! 用法：
//!   cargo run -p wyj-computer --example ax_probe                    # 列出所有可见窗口 + AX 根状态
//!   cargo run -p wyj-computer --example ax_probe -- <标题子串>       # 深入遍历某一个窗口
//!
//! 只读：不截图、不合成输入、不激活应用。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ax_probe 仅支持 macOS");
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod probe {
    use anyhow::{Context, Result};
    use objc2_application_services::{
        AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue, AXValueType,
    };
    use objc2_core_foundation::{CFArray, CFBoolean, CFMutableArray, CFRetained, CFString, CFType};
    use std::collections::HashMap;
    use std::ptr::NonNull;
    use std::time::Instant;

    /// 探路阶段放宽预算，先看真实规模
    const MAX_DEPTH: u16 = 12;
    const MAX_NODES: usize = 3000;

    pub fn run(filter: Option<String>) -> Result<()> {
        println!("AXIsProcessTrusted = {}", trusted());
        if !trusted() {
            eprintln!("!! 辅助功能未授权，所有 AX 调用都会失败。请先在「系统设置 > 隐私与安全性 > 辅助功能」勾选。");
        }

        let windows = wyj_computer::target::list_windows()?;

        if let Some(filter) = filter {
            let target = windows
                .iter()
                .find(|w| w.title.contains(&filter) || w.app_name.contains(&filter))
                .with_context(|| format!("没有标题/应用名包含 `{filter}` 的可见窗口"))?;
            println!(
                "\n=== 深入遍历: {} / {:?} (pid={}, window_id={}) ===",
                target.app_name, target.title, target.pid, target.window_id
            );
            return dump_window(target);
        }

        println!("\n=== 所有可见窗口的 AXWindows 可枚举性 ===");
        for w in &windows {
            let n = application(w.pid)
                .map(|a| count_children(&a))
                .unwrap_or("?".into());
            println!(
                "{:<30} pid={:<7} AXWindows={:<6} title={:?}",
                w.app_name, w.pid, n, w.title
            );
        }
        Ok(())
    }

    fn trusted() -> bool {
        // SAFETY: parameterless system query.
        unsafe { objc2_application_services::AXIsProcessTrusted() }
    }

    fn application(pid: u32) -> Option<CFRetained<AXUIElement>> {
        let pid = i32::try_from(pid).ok()?;
        // SAFETY: pid comes from xcap's own window enumeration.
        Some(unsafe { AXUIElement::new_application(pid) })
    }

    fn retained(raw: *const AXUIElement) -> Option<CFRetained<AXUIElement>> {
        let raw = NonNull::new(raw.cast_mut())?;
        // SAFETY: AX copy APIs follow CoreFoundation's create/copy ownership rule.
        Some(unsafe { CFRetained::from_raw(raw) })
    }

    fn retained_type(raw: *const CFType) -> Option<CFRetained<CFType>> {
        let raw = NonNull::new(raw.cast_mut())?;
        // SAFETY: same ownership rule as above.
        Some(unsafe { CFRetained::from_raw(raw) })
    }

    fn as_elements(value: &CFRetained<CFType>) -> Vec<CFRetained<AXUIElement>> {
        let Some(array) = value.downcast_ref::<CFArray>() else {
            return Vec::new();
        };
        // SAFETY: AX array attributes always yield CFType elements.
        let typed = unsafe { array.cast_unchecked::<CFType>() };
        typed
            .to_vec()
            .into_iter()
            .filter_map(|item| item.downcast::<AXUIElement>().ok())
            .collect()
    }

    fn copy_attr(element: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
        let attr = CFString::from_str(name);
        let mut raw: *const CFType = std::ptr::null();
        // SAFETY: `raw` is a valid out-pointer per the copy API contract.
        let error = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut raw)) };
        if error != AXError::Success {
            return None;
        }
        retained_type(raw)
    }

    fn children(element: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
        copy_attr(element, "AXChildren")
            .as_ref()
            .map(as_elements)
            .unwrap_or_default()
    }

    fn count_children(app: &AXUIElement) -> String {
        let attr = CFString::from_str("AXWindows");
        let mut raw: *const CFType = std::ptr::null();
        // SAFETY: `raw` is a valid out-pointer per the copy API contract.
        let error = unsafe { app.copy_attribute_value(&attr, NonNull::from(&mut raw)) };
        if error != AXError::Success {
            return format!("AXErr({})", error.0);
        }
        match retained_type(raw) {
            None => "null".into(),
            Some(value) => match value.downcast::<CFArray>() {
                Ok(array) => array.len().to_string(),
                Err(_) => "not-array".to_string(),
            },
        }
    }

    fn text_attr(element: &AXUIElement, name: &str) -> Option<String> {
        copy_attr(element, name)
            .and_then(|value| value.downcast::<CFString>().ok().map(|s| s.to_string()))
    }

    /// 一次 IPC 读完一个节点的全部属性。探路实测这是遍历性能的全部来源：
    /// 逐属性读 3000 节点 = 15000 次 IPC（访达 18.2s），批量读降到 3000 次。
    struct NodeAttrs {
        role: Option<String>,
        title: Option<String>,
        description: Option<String>,
        value: Option<String>,
        enabled: Option<bool>,
        focused: Option<bool>,
        selected: Option<bool>,
        position: Option<(f64, f64)>,
        size: Option<(f64, f64)>,
        children: Vec<CFRetained<AXUIElement>>,
    }

    const NODE_ATTRS: [&str; 9] = [
        "AXRole",
        "AXTitle",
        "AXDescription",
        "AXValue",
        "AXEnabled",
        "AXFocused",
        "AXSelected",
        "AXPosition",
        "AXSize",
    ];

    fn batch_attrs(element: &AXUIElement) -> NodeAttrs {
        let names: CFRetained<CFMutableArray<CFString>> =
            CFMutableArray::with_capacity(NODE_ATTRS.len());
        for name in NODE_ATTRS {
            names.append(&CFString::from_str(name));
        }
        // CFMutableArray<T>::as_opaque() → 未泛型化的 &CFArray（回调参数用的是它）。
        let names: &CFArray = names.as_opaque();
        let mut raw: *const CFArray = std::ptr::null();
        // options = 0：单个属性出错不影响整批（错误以 kAXValueAXErrorType 占位）
        // SAFETY: `names`/`raw` 均符合 API 契约；raw 是有效 out-pointer。
        let error = unsafe {
            element.copy_multiple_attribute_values(
                names,
                AXCopyMultipleAttributeOptions::empty(),
                NonNull::from(&mut raw),
            )
        };
        let mut out = NodeAttrs {
            role: None,
            title: None,
            description: None,
            value: None,
            enabled: None,
            focused: None,
            selected: None,
            position: None,
            size: None,
            children: Vec::new(),
        };
        if error != AXError::Success || raw.is_null() {
            return out;
        }
        let Some(raw) = NonNull::new(raw.cast_mut()) else {
            return out;
        };
        // SAFETY: 成功时 API 返回 +1 retained array。
        let values = unsafe { CFRetained::<CFArray>::from_raw(raw) };
        // SAFETY: 返回数组元素都是 CFType。
        let values = unsafe { values.cast_unchecked::<CFType>() }.to_vec();

        // CFArray::get 返回借用（downcast 消耗 self），故先 clone 出 owned 再转。
        let str_at = |i: usize| -> Option<String> {
            values
                .get(i)?
                .clone()
                .downcast::<CFString>()
                .ok()
                .map(|s| s.to_string())
        };
        let bool_at = |i: usize| -> Option<bool> {
            values
                .get(i)?
                .clone()
                .downcast::<CFBoolean>()
                .ok()
                .map(|b| b.as_bool())
        };
        let ax2_at = |i: usize, ty: AXValueType| -> Option<(f64, f64)> {
            let ax: CFRetained<AXValue> = values.get(i)?.clone().downcast::<AXValue>().ok()?;
            // SAFETY: 栈上 out-param，且类型已先核对。
            unsafe {
                if ax.r#type().0 != ty.0 {
                    return None;
                }
                let mut pair = [0.0f64; 2];
                if !ax.value(ty, NonNull::from(&mut pair).cast()) {
                    return None;
                }
                Some((pair[0], pair[1]))
            }
        };

        out.role = str_at(0);
        out.title = str_at(1);
        out.description = str_at(2);
        out.value = str_at(3);
        out.enabled = bool_at(4);
        out.focused = bool_at(5);
        out.selected = bool_at(6);
        out.position = ax2_at(7, AXValueType::CGPoint);
        out.size = ax2_at(8, AXValueType::CGSize);
        // AXChildren 不在批量列表里（数组类型需单独读，且常为空）
        out.children = children(element);
        out
    }

    /// AXPosition + AXSize → (x, y, w, h)，全局逻辑点。
    fn frame(element: &AXUIElement) -> Option<(f64, f64, f64, f64)> {
        // AXPosition 是 CGPoint，AXSize 是 **CGSize**——两者 AXValue 类型不同，
        // 必须分别按目标类型读，否则会被类型检查静默拒掉（表现为「无几何」）。
        fn read2(value: CFRetained<CFType>, ty: AXValueType) -> Option<(f64, f64)> {
            let ax: CFRetained<AXValue> = value.downcast::<AXValue>().ok()?;
            // SAFETY: out-params are stack-allocated and the type is checked first.
            unsafe {
                if ax.r#type().0 != ty.0 {
                    return None;
                }
                let mut pair = [0.0f64; 2];
                if !ax.value(ty, NonNull::from(&mut pair).cast()) {
                    return None;
                }
                Some((pair[0], pair[1]))
            }
        }
        let (x, y) = read2(copy_attr(element, "AXPosition")?, AXValueType::CGPoint)?;
        let (w, h) = read2(copy_attr(element, "AXSize")?, AXValueType::CGSize)?;
        Some((x, y, w, h))
    }

    fn actions(element: &AXUIElement) -> Vec<String> {
        let mut raw: *const CFArray = std::ptr::null();
        // SAFETY: `raw` is a valid out-pointer per the copy API contract.
        let error = unsafe { element.copy_action_names(NonNull::from(&mut raw)) };
        if error != AXError::Success || raw.is_null() {
            return Vec::new();
        }
        let Some(raw) = NonNull::new(raw.cast_mut()) else {
            return Vec::new();
        };
        // SAFETY: successful copy API returns a +1 retained array.
        let array = unsafe { CFRetained::<CFArray>::from_raw(raw) };
        // SAFETY: action-name arrays always contain CFString elements.
        let typed = unsafe { array.cast_unchecked::<CFType>() };
        typed
            .to_vec()
            .into_iter()
            .filter_map(|v: CFRetained<CFType>| {
                v.downcast::<CFString>().ok().map(|s| s.to_string())
            })
            .collect()
    }

    fn settable(element: &AXUIElement) -> bool {
        let attr = CFString::from_str("AXValue");
        let mut settable: u8 = 0;
        // SAFETY: `settable` is a valid out-pointer per the is-settable contract.
        unsafe {
            element.is_attribute_settable(&attr, NonNull::from(&mut settable)) == AXError::Success
                && settable != 0
        }
    }

    fn dump_window(target: &wyj_computer::target::WindowTarget) -> Result<()> {
        let started = Instant::now();

        // 必须先截图：元素坐标映射到「模型看到的截图空间」需要 target_width/height。
        let (captured, capture) = wyj_computer::target::capture_window_by_id(
            target.window_id,
            wyj_computer::DEFAULT_MAX_DIM,
        )?;
        let capture_ms = started.elapsed().as_millis() as u64;
        println!(
            "已截图 {}x{} → 模型空间 {}x{}（{}ms, {}KB）",
            captured.width,
            captured.height,
            captured.target_width,
            captured.target_height,
            capture_ms,
            capture.png.len() / 1024
        );

        let budget = wyj_computer::element::TraversalBudget::default();
        let snapshot = wyj_computer::element::snapshot_elements(&captured, &budget)?;

        println!("\nroot_available = {}", snapshot.root_available);
        println!("tree_id = {}", snapshot.tree_id);
        println!(
            "输出元素 {} / 访问 {} / 过滤掉 {} / 截断={}",
            snapshot.elements.len(),
            snapshot.visited,
            snapshot.filtered_out,
            snapshot.truncated
        );
        println!(
            "遍历耗时 {}ms（预算 {}ms）\n",
            snapshot.elapsed_ms,
            budget.max_duration.as_millis()
        );

        for e in snapshot.elements.iter().take(20) {
            let pos = e
                .frame_in_screenshot
                .map(|f| format!("({:.0},{:.0},{:.0}x{:.0})", f.x, f.y, f.width, f.height))
                .unwrap_or_else(|| "-".into());
            println!(
                "  {:<6} {:<18} title={:<24.24} acts={:<22.22} settable={:<5} pos={}",
                e.reference.to_string(),
                e.role.clone().unwrap_or_default(),
                e.title.clone().unwrap_or_default(),
                e.actions.join(","),
                e.value_settable || e.selected_text_settable,
                pos
            );
        }
        if snapshot.elements.len() > 20 {
            println!("  … 其余 {} 个省略", snapshot.elements.len() - 20);
        }

        let mut roles: HashMap<String, usize> = HashMap::new();
        for e in &snapshot.elements {
            *roles.entry(e.role.clone().unwrap_or_default()).or_default() += 1;
        }
        let mut top: Vec<_> = roles.into_iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("\n输出 role 分布:");
        for (role, n) in top.iter().take(10) {
            println!("  {role:<20} {n}");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    probe::run(std::env::args().nth(1))
}
