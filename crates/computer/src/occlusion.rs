//! 窗口遮挡检测：判断「目标窗口此刻是否被别的窗口盖住」。
//!
//! # 为什么必须有它
//!
//! 观察与动作走的是两条完全不同的路径：
//!
//! - **观察**：`xcap` 的 `Window::capture_image` 走 `CGWindowListCreateImage`
//!   的窗口快照，**截到的是窗口自己的内容**，哪怕它被别的窗口完全盖住；
//! - **动作**：`cursor_click` 把光标瞬移到屏幕坐标再发 HID 事件，事件由
//!   窗口服务器按 **z-order 命中测试**分发——落在最上面那个窗口上。
//!
//! 于是模型看到 A、点到 B。这个错配最典型的触发方式是「另存为」对话框：
//! 目标窗口还在、内容没变、截图正常，但它被一个 sheet 盖住了，点击穿透到
//! sheet 上。模型随后看到「点了没反应」，开始重试同一个坐标。
//!
//! # 只用于「坐标降级」这一条路径
//!
//! AX 语义动作（`AXPress` / `AXValue`）按元素寻址，不经过窗口服务器命中
//! 测试，被遮挡也照样生效——所以本模块**不能**被用作「这个窗口能不能操作」
//! 的通用前置判据，那会把明明可用的 AX 路径也一并拒掉。它只在「AX 不可用、
//! 即将借用真实光标点击」这个降级点上生效。

use anyhow::Result;

/// 窗口在 z-order 栈里的一条记录。只保留遮挡判定需要的字段。
#[derive(Debug, Clone, PartialEq)]
pub struct WindowStackEntry {
    /// `kCGWindowNumber`，与 [`crate::target::WindowTarget::window_id`] 同源。
    pub window_id: u32,
    /// `kCGWindowOwnerPID`。
    pub pid: u32,
    /// `kCGWindowLayer`。0 是普通窗口层，>0 是浮动层（菜单栏 24、Dock 20 等）。
    pub layer: i32,
    /// `kCGWindowAlpha`，0.0~1.0。
    pub alpha: f64,
    /// `kCGWindowBounds`，全局逻辑坐标（点）。
    pub bounds: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    /// 两个矩形的相交面积。不相交（含仅贴边）返回 0。
    pub fn intersection_area(&self, other: &Rect) -> f64 {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        (x1 - x0) * (y1 - y0)
    }

    fn area(&self) -> f64 {
        (self.width.max(0.0)) * (self.height.max(0.0))
    }
}

/// 透明度低于此值的窗口视为不可见，不构成遮挡。
///
/// 不用 `> 0.0`：macOS 上淡出动画会把 alpha 降到 0.001 量级并停留若干帧，
/// 那个状态下用户已经看不到它了，但事件仍然会落上去——反过来，只要 alpha
/// 真有值就得算遮挡，因为事件确实会命中。取一个极小但非零的阈值是这两者
/// 的折中：动画中间帧算遮挡（保守），完全透明的辅助层不算。
const MIN_VISIBLE_ALPHA: f64 = 0.01;

/// 遮挡面积占目标窗口面积的最低比例。
///
/// 不相交就够判了，为什么还要比例阈值：窗口阴影、1px 的高亮边框这类
/// 「技术上相交但用户完全看不到」的情形会让坐标点击被无谓地拒绝。取 2%
/// 是「能盖住一个小控件」的量级，同时不会被边框触发。
const MIN_OCCLUSION_RATIO: f64 = 0.02;

/// 在 z-order 栈里找出盖住 `target` 的那个窗口。
///
/// `stack` 必须是**从前到后**的顺序（`CGWindowListCopyWindowInfo` 的原生
/// 顺序就是如此，不要排序——`list_windows` 那个按 app 名排序的版本不能
/// 用在这里）。`target_index` 是 `target` 在栈中的下标；只有它**之前**的
/// 条目才可能在它上面。
///
/// 纯函数：真机取栈的部分在 [`visible_window_stack`]，判定逻辑可单测。
pub fn occluding_window<'a>(
    stack: &'a [WindowStackEntry],
    target_index: usize,
    target: &WindowStackEntry,
) -> Option<&'a WindowStackEntry> {
    let target_area = target.bounds.area();
    if target_area <= 0.0 {
        return None;
    }
    stack[..target_index.min(stack.len())]
        .iter()
        .find(|candidate| {
            candidate.window_id != target.window_id
                && candidate.alpha >= MIN_VISIBLE_ALPHA
                // 层号更低的是桌面/壁纸类窗口，盖不住普通窗口。
                && candidate.layer >= target.layer
                && candidate.bounds.intersection_area(&target.bounds) / target_area
                    >= MIN_OCCLUSION_RATIO
        })
}

/// 目标窗口若被遮挡，返回那个遮挡者。
///
/// 窗口不在当前 on-screen 栈里（已最小化、在别的 Space、或刚被关闭）也返回
/// `Ok(None)`——那种情况下 `cursor_click` 的坐标点击本来就打不中，由调用方
/// 的 `generation` 校验负责拒绝，不该在这里报遮挡（错误码会误导模型）。
pub fn occlusion_of(window_id: u32) -> Result<Option<WindowStackEntry>> {
    let stack = visible_window_stack()?;
    let Some(index) = stack.iter().position(|entry| entry.window_id == window_id) else {
        return Ok(None);
    };
    let target = stack[index].clone();
    Ok(occluding_window(&stack, index, &target).cloned())
}

#[cfg(target_os = "macos")]
pub fn visible_window_stack() -> Result<Vec<WindowStackEntry>> {
    imp::visible_window_stack()
}

#[cfg(not(target_os = "macos"))]
pub fn visible_window_stack() -> Result<Vec<WindowStackEntry>> {
    // Windows 侧的后台目标化动作本就未实现（见 tools/CLAUDE.md），
    // 返回空栈让遮挡检测退化为「不判遮挡」，不阻塞既有能力。
    Ok(Vec::new())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Rect, WindowStackEntry};
    use anyhow::Result;
    use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
    use objc2_core_graphics::{
        kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
        CGWindowListCopyWindowInfo, CGWindowListOption,
    };

    fn number(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i64> {
        dict.get(key)
            .and_then(|value| value.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()))
    }

    fn real(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<f64> {
        dict.get(key)
            .and_then(|value| value.downcast_ref::<CFNumber>().and_then(|n| n.as_f64()))
    }

    fn bounds(dict: &CFDictionary<CFString, CFType>) -> Option<Rect> {
        // SAFETY: `kCGWindowBounds` 对应的值在窗口信息字典里恒为 CFDictionary。
        let raw = dict.get(unsafe { kCGWindowBounds })?;
        let raw = raw.downcast::<CFDictionary>().ok()?;
        // SAFETY: 该字典的键为 CFString、值为 CFType（窗口信息字典的固定形状）。
        let inner: &CFDictionary<CFString, CFType> = unsafe { raw.cast_unchecked() };
        Some(Rect {
            x: real(inner, &CFString::from_str("X"))?,
            y: real(inner, &CFString::from_str("Y"))?,
            width: real(inner, &CFString::from_str("Width"))?,
            height: real(inner, &CFString::from_str("Height"))?,
        })
    }

    pub fn visible_window_stack() -> Result<Vec<WindowStackEntry>> {
        let Some(list) = CGWindowListCopyWindowInfo(
            // 只要屏幕上真实可见的窗口；桌面元素（壁纸、Dock 图标层）排除掉。
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
            0,
        ) else {
            return Ok(Vec::new());
        };
        // SAFETY: 该 API 的返回值恒为 CFDictionary 数组。
        let array: &CFArray<CFType> = unsafe { list.cast_unchecked::<CFType>() };

        // 顺序即 z-order（从前到后），**不要排序**——遮挡判定完全依赖它。
        let mut stack = Vec::with_capacity(array.len());
        for item in array.to_vec() {
            let Ok(dict) = item.downcast::<CFDictionary>() else {
                continue;
            };
            // SAFETY: 该 API 的数组元素恒为 CFString -> CFType 的字典。
            let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
            let Some(window_id) = number(dict, unsafe { kCGWindowNumber })
                .and_then(|value| u32::try_from(value).ok())
            else {
                continue;
            };
            let Some(bounds) = bounds(dict) else {
                continue;
            };
            stack.push(WindowStackEntry {
                window_id,
                pid: number(dict, unsafe { kCGWindowOwnerPID })
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(0),
                layer: number(dict, unsafe { kCGWindowLayer }).unwrap_or(0) as i32,
                alpha: real(dict, unsafe { kCGWindowAlpha }).unwrap_or(1.0),
                bounds,
            });
        }
        Ok(stack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        window_id: u32,
        layer: i32,
        alpha: f64,
        rect: (f64, f64, f64, f64),
    ) -> WindowStackEntry {
        WindowStackEntry {
            window_id,
            pid: 1,
            layer,
            alpha,
            bounds: Rect {
                x: rect.0,
                y: rect.1,
                width: rect.2,
                height: rect.3,
            },
        }
    }

    #[test]
    fn intersection_area_ignores_touching_edges() {
        let a = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let touching = Rect {
            x: 100.0,
            y: 0.0,
            width: 50.0,
            height: 50.0,
        };
        assert_eq!(a.intersection_area(&touching), 0.0);
        let overlapping = Rect {
            x: 50.0,
            y: 50.0,
            width: 100.0,
            height: 100.0,
        };
        assert_eq!(a.intersection_area(&overlapping), 2500.0);
    }

    #[test]
    fn a_window_above_the_target_occludes_it() {
        // 前 -> 后：对话框、目标窗口。
        let stack = vec![
            entry(1, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
            entry(2, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        let occluder = occluding_window(&stack, 1, &stack[1]).expect("必须检测到遮挡");
        assert_eq!(occluder.window_id, 1);
    }

    #[test]
    fn a_window_below_the_target_does_not_occlude() {
        // 前 -> 后：目标窗口、另一个窗口。目标在上面，点击落点就是它自己。
        let stack = vec![
            entry(1, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
            entry(2, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        assert!(occluding_window(&stack, 0, &stack[0]).is_none());
    }

    #[test]
    fn a_fully_transparent_overlay_is_not_an_occluder() {
        let stack = vec![
            entry(1, 0, 0.0, (0.0, 0.0, 800.0, 600.0)),
            entry(2, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        assert!(occluding_window(&stack, 1, &stack[1]).is_none());
    }

    #[test]
    fn a_lower_layer_window_cannot_occlude_a_higher_layer_target() {
        // 目标在浮动层（layer 3），下面那个普通窗口（layer 0）盖不住它。
        let stack = vec![
            entry(1, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
            entry(2, 3, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        assert!(occluding_window(&stack, 1, &stack[1]).is_none());
    }

    #[test]
    fn a_higher_layer_overlay_occludes_a_normal_window() {
        // 菜单栏/浮动面板（layer 24）盖住普通窗口。
        let stack = vec![
            entry(1, 24, 1.0, (0.0, 0.0, 800.0, 30.0)),
            entry(2, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        assert_eq!(occluding_window(&stack, 1, &stack[1]).unwrap().window_id, 1);
    }

    #[test]
    fn a_sliver_of_overlap_is_not_occlusion() {
        // 只压住 1px 边缘（阴影/高亮边框）不该让点击被拒。
        let stack = vec![
            entry(1, 0, 1.0, (0.0, 0.0, 800.0, 1.0)),
            entry(2, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        assert!(occluding_window(&stack, 1, &stack[1]).is_none());
    }

    #[test]
    fn the_same_window_id_is_never_its_own_occluder() {
        // CGWindowList 里同一窗口可能因为阴影/多 layer 出现多条记录。
        let stack = vec![
            entry(7, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
            entry(7, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
        ];
        assert!(occluding_window(&stack, 1, &stack[1]).is_none());
    }

    #[test]
    fn an_empty_target_rect_never_reports_occlusion() {
        // 零面积窗口（刚创建/刚关闭的中间态）不参与判定，避免除零。
        let stack = vec![
            entry(1, 0, 1.0, (0.0, 0.0, 800.0, 600.0)),
            entry(2, 0, 1.0, (0.0, 0.0, 0.0, 0.0)),
        ];
        assert!(occluding_window(&stack, 1, &stack[1]).is_none());
    }
}
