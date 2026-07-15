//! 非 macOS/Windows 平台的兜底实现：保证 workspace 整体可编译，运行时报错。
//! v1.3.0 首版不支持 Linux（见 doc/plan/v1.3.0-plan.md “保留边界”）。

use crate::{Capture, DisplaySize, MouseButton};
use anyhow::{bail, Result};

fn unsupported<T>() -> Result<T> {
    bail!("computer-use 仅支持 macOS/Windows，当前平台不可用")
}

pub fn primary_display_size() -> Result<DisplaySize> {
    unsupported()
}

pub fn capture_primary(_max_dim: u32) -> Result<Capture> {
    unsupported()
}

pub fn capture_region(_x0: i32, _y0: i32, _x1: i32, _y1: i32, _max_dim: u32) -> Result<Capture> {
    unsupported()
}

pub fn cursor_location() -> Result<(i32, i32)> {
    unsupported()
}

pub fn move_mouse(_x: i32, _y: i32) -> Result<()> {
    unsupported()
}

pub fn click(_button: MouseButton, _x: i32, _y: i32) -> Result<()> {
    unsupported()
}

pub fn double_click(_button: MouseButton, _x: i32, _y: i32) -> Result<()> {
    unsupported()
}

pub fn drag(_button: MouseButton, _from: (i32, i32), _to: (i32, i32)) -> Result<()> {
    unsupported()
}

pub fn scroll(_x: i32, _y: i32, _dx: i32, _dy: i32) -> Result<()> {
    unsupported()
}

pub fn type_text(_text: &str) -> Result<()> {
    unsupported()
}

pub fn key(_combo: &str) -> Result<()> {
    unsupported()
}

pub fn key_down(_combo: &str) -> Result<()> {
    unsupported()
}

pub fn key_up(_combo: &str) -> Result<()> {
    unsupported()
}
