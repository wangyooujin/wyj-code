//! macOS/Windows 真实实现。`xcap`（截图）与 `enigo`（输入合成）自身已按平台
//! 内部分派实现，这里只需一份共享调用代码——无需再手写 target_os 分支。

use crate::{scale, Capture, DisplaySize, MouseButton};
use anyhow::{anyhow, bail, Context, Result};
use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use xcap::{Monitor, Window};

fn primary_monitor() -> Result<Monitor> {
    let monitors = Monitor::all().context("枚举显示器失败")?;
    monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| Monitor::all().ok().and_then(|v| v.into_iter().next()))
        .ok_or_else(|| anyhow!("未检测到任何显示器"))
}

pub fn primary_display_size() -> Result<DisplaySize> {
    let m = primary_monitor()?;
    Ok(DisplaySize {
        physical_width: m.width().context("读取显示器宽度失败")?,
        physical_height: m.height().context("读取显示器高度失败")?,
    })
}

/// 把已解码的图像按 `fit_within(max_dim)` 下采样（必要时）并编码为 PNG，
/// 组装成 [`Capture`]。`capture_primary`/`capture_region` 共用。
pub(crate) fn encode_capture(
    img: DynamicImage,
    physical_width: u32,
    physical_height: u32,
    max_dim: u32,
) -> Result<Capture> {
    let (target_width, target_height) = scale::fit_within(physical_width, physical_height, max_dim);
    let img = if (target_width, target_height) != (physical_width, physical_height) {
        img.resize_exact(
            target_width.max(1),
            target_height.max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };

    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .context("编码 PNG 失败")?;

    Ok(Capture {
        physical_width,
        physical_height,
        target_width,
        target_height,
        png,
    })
}

pub fn capture_primary(max_dim: u32) -> Result<Capture> {
    let m = primary_monitor()?;
    let rgba = m
        .capture_image()
        .context("截图失败（macOS 需授予屏幕录制权限）")?;
    let img = DynamicImage::ImageRgba8(rgba);
    // 直接从已捕获的图像本身读取真实像素尺寸，不依赖 `Monitor::width()/
    // height()`——后者在 Retina/HiDPI 显示器上返回的是"点"（逻辑坐标系，
    // 通常是原生像素的一半），用它当"物理像素尺寸"会导致后续下采样比例算
    // 错、编码结果与实际截图内容的分辨率对不上。`img.width()/height()`
    // 永远和它自己的像素数据保持一致，不存在这类偏差。
    let (physical_width, physical_height) = (img.width(), img.height());
    encode_capture(img, physical_width, physical_height, max_dim)
}

/// 截取主显示器后裁剪到"点"坐标系（`Monitor::width()/height()`/点击坐标系）
/// 下的矩形 `[x0, y0, x1, y1)`，自动钳制到屏幕边界，只在裁剪结果仍超过
/// `max_dim` 时才下采样——多数"放大看细节"的场景裁剪区域本就远小于
/// `max_dim`，因此能拿到比全屏缩略图高得多的有效分辨率，用于看清小字/
/// 密集数字表格（zoom 动作，见 `tools::computer::ComputerTool`）。
///
/// **Retina/HiDPI 换算**：入参 `x0..y1` 是"点"坐标系（与点击坐标系一致，
/// 由 `CoordScaler::to_physical` 产出），但截图 API 返回的是原生像素图
/// （通常是点数的 2 倍）。之前的实现直接拿"点"坐标去裁剪像素图，在 2x
/// Retina 屏上只会裁到真实请求区域的 1/4 面积，静默丢失大半内容——这里
/// 先按"点"坐标系钳制，再用 [`scale::scale_region_to_pixels`] 换算成像素
/// 坐标系后才裁剪，从根上避免这个问题。
pub fn capture_region(x0: i32, y0: i32, x1: i32, y1: i32, max_dim: u32) -> Result<Capture> {
    let m = primary_monitor()?;
    let logical_width = m.width().context("读取显示器宽度失败")?;
    let logical_height = m.height().context("读取显示器高度失败")?;
    let rgba = m
        .capture_image()
        .context("截图失败（macOS 需授予屏幕录制权限）")?;
    let img = DynamicImage::ImageRgba8(rgba);
    let (pixel_width, pixel_height) = (img.width(), img.height());

    let (cx0, cy0, crop_w, crop_h) =
        scale::clamp_region(x0, y0, x1, y1, logical_width, logical_height);
    let (px_x0, px_y0, px_w, px_h) = scale::scale_region_to_pixels(
        (cx0, cy0, crop_w, crop_h),
        (logical_width, logical_height),
        (pixel_width, pixel_height),
    );

    let cropped = img.crop_imm(px_x0, px_y0, px_w, px_h);
    let (crop_pixel_w, crop_pixel_h) = (cropped.width(), cropped.height());
    encode_capture(cropped, crop_pixel_w, crop_pixel_h, max_dim)
}

/// 按窗口标题/所属 App 名称（不区分大小写，包含匹配）查找一个未最小化的
/// 窗口并截图，不要求该窗口在前台——用于"只读观察"场景（如看一眼某个已
/// 在运行的 App 的消息内容）而不需要把它切到前台、不打断用户当前正在看
/// 的画面。仅解决截图侧的免打扰问题，不解决点击/输入——那仍需 `computer`
/// 工具，且该工具作用于当前前台窗口，需要先把目标 App 带到前台。
///
/// 底层 `xcap::Window::capture_image` 复用的是 `capture_primary`/
/// `capture_region` 已经在承受的同一套系统截图机制，不是新增风险面；
/// 已知不确定性——窗口完全遮挡但同 Space 大概率没问题，跨 Space/虚拟桌面
/// 能否拿到真实像素未经权威验证，需实测（见调研记录，`doc/plan/` 相关）。
pub fn capture_window_by_name(query: &str, max_dim: u32) -> Result<Capture> {
    let q = query.to_lowercase();
    let windows = Window::all().context("枚举窗口失败")?;
    let win = windows
        .into_iter()
        .filter(|w| !w.is_minimized().unwrap_or(true))
        .find(|w| {
            w.title().unwrap_or_default().to_lowercase().contains(&q)
                || w.app_name().unwrap_or_default().to_lowercase().contains(&q)
        })
        .ok_or_else(|| anyhow!("未找到标题/应用名包含 \"{query}\" 的可见窗口"))?;
    let rgba = win
        .capture_image()
        .context("窗口截图失败（该窗口可能已最小化或被系统限制截图）")?;
    let img = DynamicImage::ImageRgba8(rgba);
    // 同 capture_primary：直接读图像自身像素尺寸，不用 Window::width()/
    // height()（Retina 屏上同样是"点"而非像素，见该函数文档）。
    let (physical_width, physical_height) = (img.width(), img.height());
    encode_capture(img, physical_width, physical_height, max_dim)
}

/// macOS 上合成鼠标/键盘事件需要"辅助功能"权限；未授权时 `Enigo::new()`
/// 本身通常仍会成功——系统不报错，而是静默丢弃后续的合成事件（点击/按键
/// 表面"执行成功"但屏幕上什么都没发生），所以这里的提示只能覆盖
/// `Enigo::new()` 真正失败的情况，覆盖不了"静默无效"这种更常见的失败模式；
/// 后者只能靠 `/computer` 诊断命令里的固定提醒文案兜底。
#[cfg(target_os = "macos")]
const INPUT_PERMISSION_HINT: &str =
    "（macOS 需在 系统设置 → 隐私与安全性 → 辅助功能 中为本程序授权）";
#[cfg(not(target_os = "macos"))]
const INPUT_PERMISSION_HINT: &str = "";

fn enigo() -> Result<Enigo> {
    let settings = Settings {
        #[cfg(target_os = "macos")]
        event_source_user_data: Some(crate::activity::INPUT_EVENT_MARKER),
        #[cfg(target_os = "windows")]
        windows_dw_extra_info: Some(crate::activity::INPUT_EVENT_MARKER as usize),
        ..Settings::default()
    };
    Enigo::new(&settings).map_err(|e| anyhow!("初始化输入合成失败: {e}{INPUT_PERMISSION_HINT}"))
}

pub fn cursor_location() -> Result<(i32, i32)> {
    let e = enigo()?;
    e.location().map_err(|e| anyhow!("读取光标位置失败: {e}"))
}

pub fn move_mouse(x: i32, y: i32) -> Result<()> {
    let mut e = enigo()?;
    e.move_mouse(x, y, enigo::Coordinate::Abs)
        .map_err(|e| anyhow!("移动鼠标失败: {e}"))
}

fn to_enigo_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

pub fn click(button: MouseButton, x: i32, y: i32) -> Result<()> {
    move_mouse(x, y)?;
    let mut e = enigo()?;
    e.button(to_enigo_button(button), Direction::Click)
        .map_err(|e| anyhow!("点击失败: {e}"))
}

pub fn double_click(button: MouseButton, x: i32, y: i32) -> Result<()> {
    move_mouse(x, y)?;
    let mut e = enigo()?;
    let b = to_enigo_button(button);
    e.button(b, Direction::Click)
        .map_err(|e| anyhow!("双击失败: {e}"))?;
    e.button(b, Direction::Click)
        .map_err(|e| anyhow!("双击失败: {e}"))
}

pub fn drag(button: MouseButton, from: (i32, i32), to: (i32, i32)) -> Result<()> {
    let mut e = enigo()?;
    e.move_mouse(from.0, from.1, enigo::Coordinate::Abs)
        .map_err(|e| anyhow!("拖拽起点定位失败: {e}"))?;
    let b = to_enigo_button(button);
    e.button(b, Direction::Press)
        .map_err(|e| anyhow!("拖拽按下失败: {e}"))?;
    let mut pressed = PressedMouse {
        enigo: &mut e,
        button,
        armed: true,
    };
    pressed
        .enigo
        .move_mouse(to.0, to.1, enigo::Coordinate::Abs)
        .map_err(|e| anyhow!("拖拽移动失败: {e}"))?;
    pressed.release()
}

/// 拖拽中途报错或 unwind 时也尽力释放鼠标键，避免把用户桌面留在持续按下
/// 状态。显式 release 失败时 Drop 会再尝试一次。
struct PressedMouse<'a> {
    enigo: &'a mut Enigo,
    button: MouseButton,
    armed: bool,
}

impl PressedMouse<'_> {
    fn release(&mut self) -> Result<()> {
        self.enigo
            .button(to_enigo_button(self.button), Direction::Release)
            .map_err(|e| anyhow!("拖拽释放失败: {e}"))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PressedMouse<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .enigo
                .button(to_enigo_button(self.button), Direction::Release);
        }
    }
}

pub fn scroll(x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
    move_mouse(x, y)?;
    let mut e = enigo()?;
    if dy != 0 {
        e.scroll(dy, Axis::Vertical)
            .map_err(|e| anyhow!("垂直滚动失败: {e}"))?;
    }
    if dx != 0 {
        e.scroll(dx, Axis::Horizontal)
            .map_err(|e| anyhow!("水平滚动失败: {e}"))?;
    }
    Ok(())
}

pub fn type_text(text: &str) -> Result<()> {
    let mut e = enigo()?;
    e.text(text).map_err(|e| anyhow!("输入文本失败: {e}"))
}

/// 解析 "ctrl+shift+s"、"Return"、"cmd+c" 这类按键组合（不区分大小写）。
/// 支持的修饰键：ctrl/control、alt/option、shift、cmd/super/meta/win。
fn parse_key(name: &str) -> Option<Key> {
    let lower = name.to_lowercase();
    Some(match lower.as_str() {
        "ctrl" | "control" => Key::Control,
        "alt" | "option" => Key::Alt,
        "shift" => Key::Shift,
        "cmd" | "super" | "meta" | "win" | "windows" | "command" => Key::Meta,
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // 未识别的多字符键名
            }
            Key::Unicode(c)
        }
    })
}

fn parse_key_combo(combo: &str) -> Result<Vec<Key>> {
    let parts: Vec<&str> = combo.split(['+', '-']).map(str::trim).collect();
    if parts.is_empty() || parts.iter().all(|p| p.is_empty()) {
        bail!("空的按键组合");
    }
    parts
        .iter()
        .map(|p| parse_key(p).ok_or_else(|| anyhow!("无法识别的按键: {p}（组合: {combo}）")))
        .collect()
}

/// 按下（不释放）一个按键组合——用于 shift-click 这类"点击时按住修饰键"
/// 的场景，与 [`key_up`] 配对使用。独立于 [`key`]：按键的按下/释放状态存在
/// 操作系统层面，不依赖发送它的是哪个 `Enigo` 实例，因此中间可以安全地插入
/// 其他操作（如一次鼠标点击）。
pub fn key_down(combo: &str) -> Result<()> {
    let keys = parse_key_combo(combo)?;
    let mut e = enigo()?;
    let mut pressed = Vec::with_capacity(keys.len());
    for k in &keys {
        if let Err(error) = e.key(*k, Direction::Press) {
            for pressed_key in pressed.iter().rev() {
                let _ = e.key(*pressed_key, Direction::Release);
            }
            return Err(anyhow!("按下按键失败: {error}"));
        }
        pressed.push(*k);
    }
    Ok(())
}

/// [`key_down`] 的配对释放：按相反顺序释放同一组合的按键。
pub fn key_up(combo: &str) -> Result<()> {
    let keys = parse_key_combo(combo)?;
    let mut e = enigo()?;
    let mut first_error = None;
    for k in keys.iter().rev() {
        if let Err(error) = e.key(*k, Direction::Release) {
            first_error.get_or_insert(error);
        }
    }
    if let Some(error) = first_error {
        return Err(anyhow!("释放按键失败: {error}"));
    }
    Ok(())
}

pub fn key(combo: &str) -> Result<()> {
    key_down(combo)?;
    key_up(combo)
}
