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
mod probe {
    use anyhow::{Context, Result};
    use objc2_application_services::{AXError, AXUIElement};
    use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};
    use std::collections::HashMap;
    use std::ptr::NonNull;
    use std::time::Instant;

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

    fn retained_type(raw: *const CFType) -> Option<CFRetained<CFType>> {
        let raw = NonNull::new(raw.cast_mut())?;
        // SAFETY: same ownership rule as above.
        Some(unsafe { CFRetained::from_raw(raw) })
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
