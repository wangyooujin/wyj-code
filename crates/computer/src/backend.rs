//! macOS/Windows 真实实现。`xcap`（截图）与 `enigo`（输入合成）自身已按平台
//! 内部分派实现，这里只需一份共享调用代码——无需再手写 target_os 分支。

use crate::{scale, Capture, DisplaySize, MouseButton};
use anyhow::{anyhow, bail, Context, Result};
use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use xcap::Monitor;

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
fn encode_capture(
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
    let physical_width = m.width().context("读取显示器宽度失败")?;
    let physical_height = m.height().context("读取显示器高度失败")?;
    let rgba = m
        .capture_image()
        .context("截图失败（macOS 需授予屏幕录制权限）")?;
    encode_capture(
        DynamicImage::ImageRgba8(rgba),
        physical_width,
        physical_height,
        max_dim,
    )
}

/// 截取主显示器后裁剪到物理像素矩形 `[x0, y0, x1, y1)`（自动钳制到屏幕边界，
/// 至少 1x1），只在裁剪结果仍超过 `max_dim` 时才下采样——多数"放大看细节"
/// 的场景裁剪区域本就远小于 `max_dim`，因此能拿到比全屏缩略图高得多的
/// 有效分辨率，用于看清小字/密集数字表格（zoom 动作，见
/// `tools::computer::ComputerTool`）。
pub fn capture_region(x0: i32, y0: i32, x1: i32, y1: i32, max_dim: u32) -> Result<Capture> {
    let m = primary_monitor()?;
    let physical_width = m.width().context("读取显示器宽度失败")?;
    let physical_height = m.height().context("读取显示器高度失败")?;
    let rgba = m
        .capture_image()
        .context("截图失败（macOS 需授予屏幕录制权限）")?;

    let (cx0, cy0, crop_w, crop_h) =
        scale::clamp_region(x0, y0, x1, y1, physical_width, physical_height);

    let cropped = DynamicImage::ImageRgba8(rgba).crop_imm(cx0, cy0, crop_w, crop_h);
    encode_capture(cropped, crop_w, crop_h, max_dim)
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
    Enigo::new(&Settings::default())
        .map_err(|e| anyhow!("初始化输入合成失败: {e}{INPUT_PERMISSION_HINT}"))
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
    e.move_mouse(to.0, to.1, enigo::Coordinate::Abs)
        .map_err(|e| anyhow!("拖拽移动失败: {e}"))?;
    e.button(b, Direction::Release)
        .map_err(|e| anyhow!("拖拽释放失败: {e}"))
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
    for k in &keys {
        e.key(*k, Direction::Press)
            .map_err(|e| anyhow!("按下按键失败: {e}"))?;
    }
    Ok(())
}

/// [`key_down`] 的配对释放：按相反顺序释放同一组合的按键。
pub fn key_up(combo: &str) -> Result<()> {
    let keys = parse_key_combo(combo)?;
    let mut e = enigo()?;
    for k in keys.iter().rev() {
        e.key(*k, Direction::Release)
            .map_err(|e| anyhow!("释放按键失败: {e}"))?;
    }
    Ok(())
}

pub fn key(combo: &str) -> Result<()> {
    key_down(combo)?;
    key_up(combo)
}
