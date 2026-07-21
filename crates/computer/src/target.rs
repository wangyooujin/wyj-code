//! 稳定窗口目标。
//!
//! computer-use v1.4 的后台动作不再依赖“当前前台窗口”，而是绑定到
//! `window_id + pid + generation`。generation 覆盖窗口身份、标题和逻辑边界；
//! 窗口被关闭重建、移动、缩放或标题发生变化后，旧观察立即失效，调用方必须
//! 重新枚举/截图，避免把动作送到已经变化的目标。

use crate::Capture;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowTarget {
    pub window_id: u32,
    pub pid: u32,
    pub app_name: String,
    pub title: String,
    /// 窗口在全局屏幕中的逻辑坐标（macOS points / Windows desktop coords）。
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// 最近一次按窗口捕获返回给模型的图片坐标空间；仅捕获结果中非零。
    pub target_width: u32,
    pub target_height: u32,
    /// 对窗口身份和逻辑边界计算出的稳定代数。
    pub generation: u64,
    pub focused: bool,
}

impl WindowTarget {
    /// 把窗口截图坐标换算成 AX/桌面使用的全局逻辑坐标。
    pub fn screenshot_to_global(&self, sx: i32, sy: i32) -> Result<(f64, f64)> {
        anyhow::ensure!(
            self.target_width > 0 && self.target_height > 0,
            "target_changed: no captured coordinate space; take a new window screenshot"
        );
        anyhow::ensure!(
            sx >= 0
                && sy >= 0
                && (sx as u32) < self.target_width
                && (sy as u32) < self.target_height,
            "coordinate ({sx}, {sy}) is outside the captured {}x{} window",
            self.target_width,
            self.target_height
        );
        Ok((
            self.x as f64 + sx as f64 * self.width as f64 / self.target_width as f64,
            self.y as f64 + sy as f64 * self.height as f64 / self.target_height as f64,
        ))
    }
}

/// FNV-1a 64-bit：不依赖进程随机种子，便于工具调用间稳定校验。
fn generation_for(
    window_id: u32,
    pid: u32,
    app_name: &str,
    title: &str,
    bounds: (i32, i32, u32, u32),
) -> u64 {
    let (x, y, width, height) = bounds;
    let mut hash = 0xcbf29ce484222325u64;
    let mut push = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    push(&window_id.to_le_bytes());
    push(&pid.to_le_bytes());
    push(app_name.as_bytes());
    push(title.as_bytes());
    push(&x.to_le_bytes());
    push(&y.to_le_bytes());
    push(&width.to_le_bytes());
    push(&height.to_le_bytes());
    hash
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use super::{generation_for, WindowTarget};
    use crate::{backend, Capture};
    use anyhow::{anyhow, Context, Result};
    use image::DynamicImage;
    use xcap::Window;

    fn to_target(window: &Window) -> Result<WindowTarget> {
        let window_id = window.id().context("读取窗口 id 失败")?;
        let pid = window.pid().context("读取窗口 pid 失败")?;
        let app_name = window.app_name().unwrap_or_default();
        let title = window.title().unwrap_or_default();
        let x = window.x().context("读取窗口 x 失败")?;
        let y = window.y().context("读取窗口 y 失败")?;
        let width = window.width().context("读取窗口宽度失败")?;
        let height = window.height().context("读取窗口高度失败")?;
        let focused = window.is_focused().unwrap_or(false);
        let generation = generation_for(window_id, pid, &app_name, &title, (x, y, width, height));
        Ok(WindowTarget {
            window_id,
            pid,
            app_name,
            title,
            x,
            y,
            width,
            height,
            target_width: 0,
            target_height: 0,
            generation,
            focused,
        })
    }

    fn visible_windows() -> Result<Vec<Window>> {
        Ok(Window::all()
            .context("枚举窗口失败")?
            .into_iter()
            .filter(|window| !window.is_minimized().unwrap_or(true))
            .filter(|window| {
                window.width().unwrap_or_default() > 0 && window.height().unwrap_or_default() > 0
            })
            .collect())
    }

    pub fn list_windows() -> Result<Vec<WindowTarget>> {
        let mut targets: Vec<_> = visible_windows()?
            .iter()
            .filter_map(|window| match to_target(window) {
                Ok(target) => Some(target),
                Err(error) => {
                    tracing::debug!(%error, "跳过无法读取元数据的窗口");
                    None
                }
            })
            .collect();
        targets.sort_by_key(|target| (!target.focused, target.app_name.clone(), target.window_id));
        Ok(targets)
    }

    pub fn find_window_by_id(window_id: u32) -> Result<WindowTarget> {
        let windows = visible_windows()?;
        let window = windows
            .iter()
            .find(|window| window.id().ok() == Some(window_id))
            .ok_or_else(|| anyhow!("target_changed: window {window_id} no longer exists"))?;
        to_target(window)
    }

    pub fn validate_window_target(window_id: u32, generation: u64) -> Result<WindowTarget> {
        let target = find_window_by_id(window_id)?;
        anyhow::ensure!(
            target.generation == generation,
            "target_changed: window {window_id} changed (expected generation {generation}, current {}); take a new window screenshot",
            target.generation
        );
        Ok(target)
    }

    pub fn capture_window_by_id(window_id: u32, max_dim: u32) -> Result<(WindowTarget, Capture)> {
        let windows = visible_windows()?;
        let window = windows
            .iter()
            .find(|window| window.id().ok() == Some(window_id))
            .ok_or_else(|| anyhow!("target_changed: window {window_id} no longer exists"))?;
        let mut target = to_target(window)?;
        let rgba = window
            .capture_image()
            .context("窗口截图失败（窗口可能已最小化或被系统限制截图）")?;
        let image = DynamicImage::ImageRgba8(rgba);
        let (pixel_width, pixel_height) = (image.width(), image.height());
        let capture = backend::encode_capture(image, pixel_width, pixel_height, max_dim)?;
        let current = find_window_by_id(window_id)?;
        anyhow::ensure!(
            current.generation == target.generation,
            "target_changed: window {window_id} changed while it was being captured; take a new window screenshot"
        );
        target.focused = current.focused;
        target.target_width = capture.target_width;
        target.target_height = capture.target_height;
        Ok((target, capture))
    }

    pub fn find_window_by_query(query: &str) -> Result<WindowTarget> {
        let query = query.trim().to_lowercase();
        anyhow::ensure!(!query.is_empty(), "window query must not be empty");
        list_windows()?
            .into_iter()
            .find(|target| {
                target.title.to_lowercase().contains(&query)
                    || target.app_name.to_lowercase().contains(&query)
            })
            .ok_or_else(|| anyhow!("未找到标题/应用名包含该查询的可见窗口"))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::WindowTarget;
    use crate::Capture;
    use anyhow::{bail, Result};

    fn unsupported<T>() -> Result<T> {
        bail!("computer-use 仅支持 macOS/Windows，当前平台不可用")
    }

    pub fn list_windows() -> Result<Vec<WindowTarget>> {
        unsupported()
    }
    pub fn find_window_by_id(_window_id: u32) -> Result<WindowTarget> {
        unsupported()
    }
    pub fn validate_window_target(_window_id: u32, _generation: u64) -> Result<WindowTarget> {
        unsupported()
    }
    pub fn capture_window_by_id(_window_id: u32, _max_dim: u32) -> Result<(WindowTarget, Capture)> {
        unsupported()
    }
    pub fn find_window_by_query(_query: &str) -> Result<WindowTarget> {
        unsupported()
    }
}

pub fn list_windows() -> Result<Vec<WindowTarget>> {
    imp::list_windows()
}

pub fn find_window_by_id(window_id: u32) -> Result<WindowTarget> {
    imp::find_window_by_id(window_id)
}

pub fn validate_window_target(window_id: u32, generation: u64) -> Result<WindowTarget> {
    imp::validate_window_target(window_id, generation)
}

pub fn capture_window_by_id(window_id: u32, max_dim: u32) -> Result<(WindowTarget, Capture)> {
    imp::capture_window_by_id(window_id, max_dim)
}

pub fn find_window_by_query(query: &str) -> Result<WindowTarget> {
    imp::find_window_by_query(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_changes_with_identity_or_bounds() {
        let base = generation_for(1, 2, "App", "Title", (0, 0, 800, 600));
        assert_ne!(base, generation_for(2, 2, "App", "Title", (0, 0, 800, 600)));
        assert_ne!(
            base,
            generation_for(1, 2, "App", "Title", (10, 0, 800, 600))
        );
        assert_eq!(base, generation_for(1, 2, "App", "Title", (0, 0, 800, 600)));
    }

    #[test]
    fn screenshot_coordinates_map_to_window_logical_bounds() {
        let target = WindowTarget {
            window_id: 1,
            pid: 2,
            app_name: "App".into(),
            title: "Title".into(),
            x: 100,
            y: 50,
            width: 800,
            height: 600,
            target_width: 400,
            target_height: 300,
            generation: 3,
            focused: false,
        };
        assert_eq!(
            target.screenshot_to_global(200, 150).unwrap(),
            (500.0, 350.0)
        );
        assert!(target.screenshot_to_global(400, 0).is_err());
    }
}
