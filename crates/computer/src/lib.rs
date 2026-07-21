//! wyj-computer — 屏幕截图 + 鼠标/键盘输入合成（computer-use 工具的系统层）。
//!
//! 仅 macOS/Windows 提供真实实现（`xcap` 截图 + `enigo` 输入合成，两者内部
//! 已各自按平台分派，本 crate 无需再手写一套 target_os 分支）；其余平台
//! （含 Linux，v1.3.0 不支持）编译进 [`unsupported`] 桩实现，保证 workspace
//! 整体可跨平台编译。详见 doc/plan/v1.3.0-plan.md。

#[cfg(target_os = "macos")]
pub mod accessibility;
pub mod activity;
pub mod scale;
pub mod target;
#[cfg(target_os = "macos")]
pub mod targeted_event;
pub mod telemetry;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod backend;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use backend as imp;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use unsupported as imp;

use anyhow::Result;

/// 当前编译目标是否提供真实实现（决定 ComputerTool 是否注册）。
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// 截图下采样目标长边像素默认值。与 Anthropic 官方 computer-use 博客推荐的
/// "1280x720 是大多数场景的安全默认分辨率"一致（见 doc/plan/v1.3.0-plan.md
/// 增强三）。归属这里而非 `wyj-tools::computer`：这是"按什么分辨率捕获"的
/// 系统层决策，不是工具层的调用约定。
pub const DEFAULT_MAX_DIM: u32 = 1280;

/// 主显示器的物理像素尺寸（未经任何下采样）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplaySize {
    pub physical_width: u32,
    pub physical_height: u32,
}

/// 一次截图结果：`png` 已按 `target_width`/`target_height` 下采样编码，
/// `physical_width`/`physical_height` 保留原始物理像素尺寸供坐标换算使用。
#[derive(Debug, Clone)]
pub struct Capture {
    pub physical_width: u32,
    pub physical_height: u32,
    pub target_width: u32,
    pub target_height: u32,
    pub png: Vec<u8>,
}

impl Capture {
    /// 供 [`wyj_api::types::ToolResultPart::Image`] 使用的 base64 编码。
    pub fn png_base64(&self) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        STANDARD.encode(&self.png)
    }

    /// 本次截图对应的坐标换算器（目标坐标 → 物理像素坐标）。
    pub fn coord_scaler(&self) -> scale::CoordScaler {
        scale::CoordScaler::new(
            self.physical_width,
            self.physical_height,
            self.target_width,
            self.target_height,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 主显示器物理像素尺寸。
pub fn primary_display_size() -> Result<DisplaySize> {
    imp::primary_display_size()
}

/// 截取主显示器，按 `max_dim` 等比下采样后编码为 PNG。
pub fn capture_primary(max_dim: u32) -> Result<Capture> {
    imp::capture_primary(max_dim)
}

/// 截取主显示器后裁剪到物理像素矩形 `[x0, y0, x1, y1)`（自动钳制到屏幕边界）；
/// 只在裁剪结果仍超过 `max_dim` 时才下采样。用于"放大看细节"（zoom 动作）——
/// 裁剪区域通常远小于整屏，因此能拿到比全屏缩略图高得多的有效分辨率。
pub fn capture_region(x0: i32, y0: i32, x1: i32, y1: i32, max_dim: u32) -> Result<Capture> {
    imp::capture_region(x0, y0, x1, y1, max_dim)
}

/// 按窗口标题/所属 App 名称（不区分大小写，包含匹配）查找一个未最小化的
/// 窗口并截图，不要求该窗口在前台——只解决截图侧的免打扰问题（"只读观察"
/// 场景），不解决点击/输入，见 `tools::window_capture::WindowCaptureTool`。
pub fn capture_window_by_name(query: &str, max_dim: u32) -> Result<Capture> {
    imp::capture_window_by_name(query, max_dim)
}

/// 当前鼠标位置（物理像素坐标系；用于失控角检测）。
pub fn cursor_location() -> Result<(i32, i32)> {
    imp::cursor_location()
}

/// 移动鼠标到物理像素坐标（不点击）。
pub fn move_mouse(x: i32, y: i32) -> Result<()> {
    imp::move_mouse(x, y)
}

/// 在物理像素坐标处单击。
pub fn click(button: MouseButton, x: i32, y: i32) -> Result<()> {
    imp::click(button, x, y)
}

/// 在物理像素坐标处双击。
pub fn double_click(button: MouseButton, x: i32, y: i32) -> Result<()> {
    imp::double_click(button, x, y)
}

/// 从 `from` 拖拽到 `to`（物理像素坐标）。
pub fn drag(button: MouseButton, from: (i32, i32), to: (i32, i32)) -> Result<()> {
    imp::drag(button, from, to)
}

/// 移动鼠标到 `(x, y)` 后按 `(dx, dy)` 滚动（物理像素坐标）。
pub fn scroll(x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
    imp::scroll(x, y, dx, dy)
}

/// 输入一段文本（逐字符合成）。
pub fn type_text(text: &str) -> Result<()> {
    imp::type_text(text)
}

/// 按键组合，如 `"ctrl+c"`、`"cmd+shift+s"`、`"Return"`、`"Escape"`。
pub fn key(combo: &str) -> Result<()> {
    imp::key(combo)
}

/// 按下（不释放）一个按键/修饰键组合，与 [`key_up`] 配对使用——用于
/// "点击时按住修饰键"（如 shift-click 多选）。中间可安全插入其他操作
/// （如一次鼠标点击），按键状态在操作系统层面维持，与哪个调用发出它无关。
pub fn key_down(combo: &str) -> Result<()> {
    imp::key_down(combo)
}

/// [`key_down`] 的配对释放。
pub fn key_up(combo: &str) -> Result<()> {
    imp::key_up(combo)
}
