//! Computer 工具 — Anthropic 原生 computer-use（桌面 GUI 控制）。
//! 仅 macOS/Windows 编译（见 wyj-computer crate 与 doc/plan/v1.3.0-plan.md）。

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use wyj_api::types::{NativeToolSpec, ToolDefinition, ToolResultPart};
use wyj_computer::scale::CoordScaler;
use wyj_computer::MouseButton;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

/// 截图下采样目标长边像素默认值，定义于 `wyj-computer`（系统层关注点，
/// `/computer` 诊断命令也直接引用它，不必依赖整个 `wyj-tools`）。
pub use wyj_computer::DEFAULT_MAX_DIM;

/// 鼠标停留在屏幕四角附近视为"失控角"，变更类动作执行前检测到就立即中止
/// （用户把鼠标甩到屏角是通用的"停下"信号，兜底防止模型误操作失控）。
const CORNER_THRESHOLD_PX: i32 = 5;

/// 自上一次 screenshot 起允许的连续变更动作数上限。Tool trait 无跨调用的
/// "回合边界"钩子，改用"距上次截图核实进度"作为更贴切的节流锚点：模型若
/// 一直不截图核实就连续操作，往往就是跑偏的信号。
const MAX_ACTIONS_WITHOUT_SCREENSHOT: usize = 20;

/// `wait` 动作允许的最长秒数，避免模型意外传入超大值卡住整轮对话。
const MAX_WAIT_SECS: f64 = 5.0;

/// 每个 scroll_amount 单位对应的物理像素滚动量（无官方文档给出精确值，
/// 需结合实际系统手测微调，见 doc/plan/v1.3.0-plan.md 验证节）。
const SCROLL_STEP_PX: i32 = 40;

/// zoom 动作请求区域四周留出的余量比例（各方向按区域自身宽/高的这个比例
/// 外扩），防止模型给出的框贴得太紧、把目标内容边缘切掉。
const ZOOM_PADDING_RATIO: f64 = 0.12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundFallbackPolicy {
    Disabled,
    Ask,
    IdleOnly,
}

#[derive(Debug, Clone, Copy)]
pub struct ForegroundPolicy {
    pub fallback: ForegroundFallbackPolicy,
    pub quiet_period: Duration,
    pub max_defer: Duration,
    pub restore_context: bool,
}

impl Default for ForegroundPolicy {
    fn default() -> Self {
        Self {
            fallback: ForegroundFallbackPolicy::Disabled,
            quiet_period: Duration::from_secs(2),
            max_defer: Duration::from_secs(30),
            restore_context: true,
        }
    }
}

pub struct ComputerTool {
    max_dim: u32,
    /// true：向模型声明为 Anthropic 原生 `computer_20251124` 工具（无 description/
    /// input_schema，依赖 Claude 训练时习得的调用约定，仅官方端点认得）。
    /// false：声明为普通 custom 工具（带完整 description + input_schema），
    /// 任何具备基本工具调用能力的模型都能按标准协议使用——用于第三方
    /// Anthropic 协议兼容端点（MiniMax/GLM/Kimi 等），它们说的是 Anthropic
    /// 协议但没有 Claude 那层专属训练，收到无 schema 的原生工具会因为不知道
    /// 怎么调用而报错或瞎猜参数。两种模式下 `run()` 的动作分派逻辑完全一致，
    /// 只是工具定义的对外声明方式不同。见 `crates/config::Profile::
    /// is_official_anthropic_endpoint` 与 `cli::register_computer_tool_if_enabled`。
    native: bool,
    physical_width: u32,
    physical_height: u32,
    target_width: u32,
    target_height: u32,
    action_count: AtomicUsize,
    foreground_policy: ForegroundPolicy,
    /// 最近一次全屏 screenshot 对应的精确前台窗口。任何变更动作后清空；
    /// 下一步动作必须重新截图，避免在窗口/焦点变化后沿用旧坐标。
    foreground_observation: std::sync::Mutex<Option<wyj_computer::target::WindowTarget>>,
}

impl ComputerTool {
    /// 构造时探测主显示器物理尺寸并计算下采样目标分辨率；探测失败时（如
    /// macOS 尚未授权屏幕录制）退回一个保守兜底尺寸，工具仍可注册，实际
    /// 调用截图/输入时会把探测失败的原因作为工具错误返回给模型。
    pub fn new(max_dim: u32, native: bool) -> Self {
        Self::new_with_policy(max_dim, native, ForegroundPolicy::default())
    }

    pub fn new_with_policy(
        max_dim: u32,
        native: bool,
        foreground_policy: ForegroundPolicy,
    ) -> Self {
        let (physical_width, physical_height) = match wyj_computer::primary_display_size() {
            Ok(d) => (d.physical_width, d.physical_height),
            Err(e) => {
                tracing::warn!("computer-use: 获取主显示器尺寸失败，使用兜底 1280x800: {e}");
                (1280, 800)
            }
        };
        let (target_width, target_height) =
            wyj_computer::scale::fit_within(physical_width, physical_height, max_dim);
        Self {
            max_dim,
            native,
            physical_width,
            physical_height,
            target_width,
            target_height,
            action_count: AtomicUsize::new(0),
            foreground_policy,
            foreground_observation: std::sync::Mutex::new(None),
        }
    }

    fn scaler(&self) -> CoordScaler {
        CoordScaler::new(
            self.physical_width,
            self.physical_height,
            self.target_width,
            self.target_height,
        )
    }

    async fn acquire_foreground_lease(
        &self,
        ctx: &dyn ToolContext,
    ) -> Result<wyj_computer::activity::InputLease> {
        if self.foreground_policy.fallback == ForegroundFallbackPolicy::Disabled {
            bail!(
                "requires_foreground_takeover: legacy computer mutations are disabled; use app_computer"
            );
        }
        if !ctx.supports_interactive_confirmation() {
            bail!(
                "requires_foreground_takeover: headless/scheduled execution may not take over the foreground desktop"
            );
        }

        let started_at = Instant::now();
        let deadline = started_at
            .checked_add(self.foreground_policy.max_defer)
            .unwrap_or(started_at);
        loop {
            match wyj_computer::activity::acquire_lease(self.foreground_policy.quiet_period) {
                Ok(lease) => return Ok(lease),
                Err(error) => {
                    let message = error.to_string();
                    if !message.contains("user_active")
                        && !message.contains("input_monitor_unavailable: exact external-input attribution is required (starting)")
                    {
                        return Err(error);
                    }
                    if Instant::now() >= deadline {
                        bail!(
                            "user_active: foreground takeover deferred for {}s and timed out; do not retry automatically",
                            self.foreground_policy.max_defer.as_secs()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn current_focused_window() -> Option<wyj_computer::target::WindowTarget> {
        wyj_computer::target::list_windows()
            .ok()?
            .into_iter()
            .find(|window| window.focused)
    }

    fn validate_foreground_observation(&self) -> Result<wyj_computer::target::WindowTarget> {
        let observed = self
            .foreground_observation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| {
                anyhow!("target_changed: take a new screenshot before a foreground action")
            })?;
        let current = Self::current_focused_window()
            .ok_or_else(|| anyhow!("target_changed: no focused window is available"))?;
        anyhow::ensure!(
            current.window_id == observed.window_id
                && current.pid == observed.pid
                && current.generation == observed.generation,
            "target_changed: the focused window changed since the last screenshot; take a new screenshot"
        );
        Ok(current)
    }

    fn do_screenshot(&self) -> Result<ToolResult> {
        self.action_count.store(0, Ordering::SeqCst);
        let before = Self::current_focused_window();
        let capture = wyj_computer::capture_primary(self.max_dim)?;
        let after = Self::current_focused_window();
        let stable = before
            .as_ref()
            .zip(after.as_ref())
            .filter(|(before, after)| {
                before.window_id == after.window_id
                    && before.pid == after.pid
                    && before.generation == after.generation
            })
            .map(|(_, after)| after.clone());
        *self
            .foreground_observation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = stable.clone();
        let display = format!(
            "[foreground compatibility screenshot: {}x{} -> {}x{}, {} KB; observed_window={}]",
            capture.physical_width,
            capture.physical_height,
            capture.target_width,
            capture.target_height,
            capture.png.len() / 1024,
            stable
                .as_ref()
                .map(|window| format!("{}:{}:{}", window.window_id, window.pid, window.generation))
                .unwrap_or_else(|| "unstable".to_string())
        );
        let data = capture.png_base64();
        Ok(ToolResult::with_parts(
            display,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data,
            }],
        ))
    }

    /// 放大看细节：裁剪 + 尽量不下采样地重新编码一块区域，比全屏缩略图里的
    /// 同一块内容有效分辨率高得多——用于看清密集数字表格/小字这类全屏截图
    /// 会因下采样而糊掉的内容。测试时验证性质：动态缩放能大幅提升 GUI
    /// grounding/OCR 准确率（RegionFocus/UI-Zoomer 等 2025-2026 论文），也是
    /// Anthropic 官方博客里"截图不要超限、按需请求细节"建议的落地方式。
    fn do_zoom(&self, input: &Value) -> Result<ToolResult> {
        self.action_count.store(0, Ordering::SeqCst);
        let (x0, y0, x1, y1) = required_region(input, "region")?;
        if x1 <= x0 || y1 <= y0 {
            bail!("`region` must satisfy x1 > x0 and y1 > y0");
        }
        // 四周留一点余量：模型给的框贴得太紧容易把目标内容边缘切掉，GUI
        // 定位天然有误差，留白比强制精确更稳。
        let pad_x = ((x1 - x0) as f64 * ZOOM_PADDING_RATIO).round() as i32;
        let pad_y = ((y1 - y0) as f64 * ZOOM_PADDING_RATIO).round() as i32;
        let scaler = self.scaler();
        let (px0, py0) = scaler.to_physical(x0 - pad_x, y0 - pad_y);
        let (px1, py1) = scaler.to_physical(x1 + pad_x, y1 + pad_y);
        let capture = wyj_computer::capture_region(px0, py0, px1, py1, self.max_dim)?;
        let display = format!(
            "[zoom: region ({x0},{y0})-({x1},{y1}) -> {}x{}, {} KB]",
            capture.target_width,
            capture.target_height,
            capture.png.len() / 1024
        );
        let data = capture.png_base64();
        Ok(ToolResult::with_parts(
            display,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data,
            }],
        ))
    }

    fn do_cursor_position(&self) -> Result<ToolResult> {
        let (px, py) = wyj_computer::cursor_location()?;
        let (tx, ty) = self.scaler().to_target(px, py);
        Ok(ToolResult::ok(format!("({tx}, {ty})")))
    }

    async fn do_wait(&self, input: &Value) -> Result<ToolResult> {
        let secs = input
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .clamp(0.0, MAX_WAIT_SECS);
        tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
        Ok(ToolResult::ok(format!("waited {secs}s")))
    }

    fn do_mouse_move(&self, input: &Value) -> Result<ToolResult> {
        let (x, y) = required_coord(input, "coordinate")?;
        let (px, py) = self.scaler().to_physical(x, y);
        wyj_computer::move_mouse(px, py)?;
        Ok(ToolResult::ok(format!("moved to ({x}, {y})")))
    }

    fn resolve_point(&self, input: &Value, field: &str) -> Result<(i32, i32)> {
        match get_coord(input, field)? {
            Some((x, y)) => Ok(self.scaler().to_physical(x, y)),
            None => wyj_computer::cursor_location(),
        }
    }

    /// 点击类动作复用通用 `text` 字段传入要按住的修饰键组合（如 `"shift"`、
    /// `"cmd"`），用于 shift-click 多选/cmd-click 之类的场景——与 `key`/`type`
    /// 动作用同一个 `text` 字段但语义不同（那两个是"发送文本/按键"，这里是
    /// "点击期间按住"），对齐 Anthropic 官方 computer-use 工具的调用约定。
    fn do_click(&self, action: &str, input: &Value) -> Result<ToolResult> {
        let (px, py) = self.resolve_point(input, "coordinate")?;
        let modifiers = input
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());

        if let Some(combo) = modifiers {
            wyj_computer::key_down(combo)?;
        }
        let click_result = match action {
            "left_click" => wyj_computer::click(MouseButton::Left, px, py),
            "right_click" => wyj_computer::click(MouseButton::Right, px, py),
            "middle_click" => wyj_computer::click(MouseButton::Middle, px, py),
            "double_click" => wyj_computer::double_click(MouseButton::Left, px, py),
            _ => unreachable!("dispatched only for click-family actions"),
        };
        // 无论点击成败都要释放修饰键，避免残留状态污染后续操作；点击失败时
        // 优先报告点击本身的错误，释放失败则作为次要错误兜底报告。
        let release_result = modifiers.map(wyj_computer::key_up);
        click_result?;
        if let Some(r) = release_result {
            r?;
        }

        let suffix = modifiers
            .map(|combo| format!(" holding {combo}"))
            .unwrap_or_default();
        Ok(ToolResult::ok(format!("{action} at ({px}, {py}){suffix}")))
    }

    fn do_drag(&self, input: &Value) -> Result<ToolResult> {
        let (ex, ey) = required_coord(input, "coordinate")?;
        let end = self.scaler().to_physical(ex, ey);
        let start = self.resolve_point(input, "start_coordinate")?;
        wyj_computer::drag(MouseButton::Left, start, end)?;
        Ok(ToolResult::ok("drag done".to_string()))
    }

    fn do_key(&self, input: &Value) -> Result<ToolResult> {
        let text = required_str(input, "text")?;
        wyj_computer::key(text)?;
        Ok(ToolResult::ok(format!("pressed {text}")))
    }

    fn do_type_with_lease(
        &self,
        input: &Value,
        lease: &wyj_computer::activity::InputLease,
    ) -> Result<ToolResult> {
        let text = required_str(input, "text")?;
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(8) {
            if !wyj_computer::activity::lease_is_valid(lease) {
                bail!(
                    "preempted_by_user: external input arrived while typing; remaining text was not sent"
                );
            }
            let chunk: String = chunk.iter().collect();
            wyj_computer::type_text(&chunk)?;
        }
        Ok(ToolResult::ok(format!("typed {} chars", chars.len())))
    }

    fn do_scroll(&self, input: &Value) -> Result<ToolResult> {
        let (px, py) = self.resolve_point(input, "coordinate")?;
        let direction = required_str(input, "scroll_direction")?;
        let amount = input
            .get("scroll_amount")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            .max(1) as i32;
        let step = amount * SCROLL_STEP_PX;
        let (dx, dy) = match direction {
            "up" => (0, -step),
            "down" => (0, step),
            "left" => (-step, 0),
            "right" => (step, 0),
            other => bail!("unknown scroll_direction: {other}"),
        };
        wyj_computer::scroll(px, py, dx, dy)?;
        Ok(ToolResult::ok(format!("scrolled {direction} x{amount}")))
    }
}

/// 只读、无副作用的 action：截图/放大看细节/查坐标/等待，无需权限确认，
/// 且都会重置"距上次截图的连续动作数"计数（zoom 和 screenshot 一样是模型
/// 在核实现实，不是在盲跑）。
fn is_read_only_action(action: &str) -> bool {
    matches!(action, "screenshot" | "zoom" | "cursor_position" | "wait")
}

/// 鼠标是否位于屏幕四角附近的失控角判定（物理像素坐标）。
fn in_dead_corner(x: i32, y: i32, width: u32, height: u32) -> bool {
    let near_edge_x = x <= CORNER_THRESHOLD_PX || x >= width as i32 - 1 - CORNER_THRESHOLD_PX;
    let near_edge_y = y <= CORNER_THRESHOLD_PX || y >= height as i32 - 1 - CORNER_THRESHOLD_PX;
    near_edge_x && near_edge_y
}

fn get_coord(input: &Value, field: &str) -> Result<Option<(i32, i32)>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or_else(|| anyhow!("`{field}` must be a [x, y] array"))?;
            if arr.len() != 2 {
                bail!("`{field}` must have exactly 2 elements");
            }
            let x = arr[0]
                .as_i64()
                .ok_or_else(|| anyhow!("`{field}[0]` must be an integer"))?
                as i32;
            let y = arr[1]
                .as_i64()
                .ok_or_else(|| anyhow!("`{field}[1]` must be an integer"))?
                as i32;
            Ok(Some((x, y)))
        }
    }
}

fn required_coord(input: &Value, field: &str) -> Result<(i32, i32)> {
    get_coord(input, field)?.ok_or_else(|| anyhow!("missing required field `{field}`"))
}

/// 解析 `[x0, y0, x1, y1]` 形式的矩形区域（zoom 动作用）。
fn required_region(input: &Value, field: &str) -> Result<(i32, i32, i32, i32)> {
    let arr = input
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing required field `{field}` ([x0, y0, x1, y1] array)"))?;
    if arr.len() != 4 {
        bail!("`{field}` must have exactly 4 elements: [x0, y0, x1, y1]");
    }
    let mut n = [0i32; 4];
    for (i, v) in arr.iter().enumerate() {
        n[i] = v
            .as_i64()
            .ok_or_else(|| anyhow!("`{field}[{i}]` must be an integer"))? as i32;
    }
    Ok((n[0], n[1], n[2], n[3]))
}

fn required_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required field `{field}`"))
}

fn fmt_coord(v: Option<&Value>) -> String {
    match v.and_then(Value::as_array) {
        Some(arr) if arr.len() == 2 => format!(
            "({}, {})",
            arr[0].as_i64().unwrap_or_default(),
            arr[1].as_i64().unwrap_or_default()
        ),
        _ => "(current position)".to_string(),
    }
}

/// 从 `input` 构造一句人类可读的操作摘要，供权限确认弹窗展示。
fn summarize_action(input: &Value) -> String {
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("computer");
    match action {
        "wait" => format!(
            "wait {}s",
            input.get("duration").and_then(Value::as_f64).unwrap_or(1.0)
        ),
        "mouse_move" => format!("mouse_move to {}", fmt_coord(input.get("coordinate"))),
        "left_click" | "right_click" | "middle_click" | "double_click" => {
            let coord = fmt_coord(input.get("coordinate"));
            match input
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(combo) => format!("{action} at {coord} holding {combo}"),
                None => format!("{action} at {coord}"),
            }
        }
        "left_click_drag" => format!(
            "drag {} -> {}",
            fmt_coord(input.get("start_coordinate")),
            fmt_coord(input.get("coordinate"))
        ),
        "key" => format!(
            "key {}",
            input.get("text").and_then(Value::as_str).unwrap_or("")
        ),
        "type" => format!(
            "type \"{}\"",
            input.get("text").and_then(Value::as_str).unwrap_or("")
        ),
        "scroll" => format!(
            "scroll {} x{} at {}",
            input
                .get("scroll_direction")
                .and_then(Value::as_str)
                .unwrap_or("?"),
            input
                .get("scroll_amount")
                .and_then(Value::as_i64)
                .unwrap_or(1),
            fmt_coord(input.get("coordinate"))
        ),
        other => other.to_string(),
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "computer"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: if self.native {
                // 原生工具描述不会发给模型（provider 层按 native 分支序列化，
                // description/input_schema 都被忽略），仅供本地 UI 展示用。
                crate::descriptions::COMPUTER.to_string()
            } else {
                // custom 工具模式：这段文字是模型理解本工具调用约定的唯一来源
                // （没有 Claude 那层内置训练兜底），必须完整、可独立执行。
                crate::descriptions::computer_custom_description(
                    self.target_width,
                    self.target_height,
                )
            },
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "screenshot", "zoom", "cursor_position", "mouse_move",
                            "left_click", "right_click", "middle_click", "double_click",
                            "left_click_drag", "key", "type", "scroll", "wait"
                        ]
                    },
                    "coordinate": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "start_coordinate": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "region": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 4,
                        "maxItems": 4
                    },
                    "text": {"type": "string"},
                    "scroll_direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"]
                    },
                    "scroll_amount": {"type": "integer"},
                    "duration": {"type": "number"}
                },
                "required": ["action"]
            }),
            native: self.native.then(|| NativeToolSpec {
                tool_type: "computer_20251124".to_string(),
                extra: serde_json::json!({
                    "display_width_px": self.target_width,
                    "display_height_px": self.target_height,
                }),
                beta: "computer-use-2025-11-24".to_string(),
            }),
        }
    }

    fn needs_permission(&self, input: &Value) -> bool {
        matches!(
            self.foreground_policy.fallback,
            ForegroundFallbackPolicy::Ask
        ) && match input.get("action").and_then(Value::as_str) {
            Some(action) => !is_read_only_action(action),
            None => true,
        }
    }

    fn action_summary(&self, input: &Value) -> String {
        summarize_action(input)
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing `action` field"))?
            .to_string();

        if is_read_only_action(&action) {
            return match action.as_str() {
                "screenshot" => self.do_screenshot(),
                "zoom" => self.do_zoom(&input),
                "cursor_position" => self.do_cursor_position(),
                "wait" => self.do_wait(&input).await,
                _ => unreachable!("is_read_only_action 枚举已穷尽"),
            };
        }

        let lease = match self.acquire_foreground_lease(ctx).await {
            Ok(lease) => lease,
            Err(error) => return Ok(foreground_error(error)),
        };
        let foreground_before = match self.validate_foreground_observation() {
            Ok(window) => window,
            Err(error) => return Ok(foreground_error(error)),
        };
        #[cfg(not(target_os = "macos"))]
        let _ = &foreground_before;
        if !wyj_computer::activity::lease_is_valid(&lease) {
            return Ok(foreground_error(anyhow!(
                "preempted_by_user: external input arrived before the foreground action"
            )));
        }
        if let Ok((cx, cy)) = wyj_computer::cursor_location() {
            if in_dead_corner(cx, cy, self.physical_width, self.physical_height) {
                return Ok(ToolResult::err(
                    "Aborted: cursor is in a screen corner (dead-man's corner failsafe). \
                     The user likely moved it there to stop automation.",
                ));
            }
        }
        let count = self.action_count.fetch_add(1, Ordering::SeqCst) + 1;
        if count > MAX_ACTIONS_WITHOUT_SCREENSHOT {
            return Ok(ToolResult::err(format!(
                "Stopped: {count} actions since the last screenshot (limit \
                 {MAX_ACTIONS_WITHOUT_SCREENSHOT}). Take a screenshot to verify progress, \
                 or stop and ask the user."
            )));
        }

        let restore_pointer = (self.foreground_policy.restore_context
            && matches!(
                action.as_str(),
                "left_click"
                    | "right_click"
                    | "middle_click"
                    | "double_click"
                    | "left_click_drag"
                    | "scroll"
            ))
        .then(|| wyj_computer::cursor_location().ok())
        .flatten();
        let result = match action.as_str() {
            "mouse_move" => self.do_mouse_move(&input),
            "left_click" | "right_click" | "middle_click" | "double_click" => {
                self.do_click(&action, &input)
            }
            "left_click_drag" => self.do_drag(&input),
            "key" => self.do_key(&input),
            "type" => self.do_type_with_lease(&input, &lease),
            "scroll" => self.do_scroll(&input),
            other => Ok(ToolResult::err(format!(
                "unsupported computer action: {other}"
            ))),
        };
        wyj_computer::telemetry::record_foreground_action();
        if !wyj_computer::activity::lease_is_valid(&lease) {
            return Ok(foreground_error(anyhow!(
                "preempted_by_user: external input arrived during foreground action; do not retry automatically"
            )));
        }
        #[cfg(target_os = "macos")]
        if self.foreground_policy.restore_context {
            match wyj_computer::accessibility::frontmost_pid() {
                Ok(pid) if pid != foreground_before.pid => {
                    if let Err(error) =
                        wyj_computer::accessibility::activate_application(foreground_before.pid)
                    {
                        return Ok(foreground_error(anyhow!(
                            "foreground_context_restore_failed: {error}"
                        )));
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    return Ok(foreground_error(anyhow!(
                        "foreground_context_restore_failed: {error}"
                    )))
                }
            }
        }
        if let Some((x, y)) = restore_pointer {
            if let Err(error) = wyj_computer::move_mouse(x, y) {
                return Ok(foreground_error(anyhow!(
                    "foreground_context_restore_failed: {error}"
                )));
            }
        }
        if !wyj_computer::activity::lease_is_valid(&lease) {
            return Ok(foreground_error(anyhow!(
                "preempted_by_user: external input arrived while restoring foreground context"
            )));
        }
        result
    }
}

fn foreground_error(error: impl std::fmt::Display) -> ToolResult {
    let message = error.to_string();
    wyj_computer::telemetry::record_error_message(&message);
    let code = [
        "requires_foreground_takeover",
        "target_changed",
        "preempted_by_user",
        "user_active",
        "screen_locked",
        "input_monitor_unavailable",
        "foreground_context_restore_failed",
    ]
    .into_iter()
    .find(|code| message.contains(code))
    .unwrap_or("foreground_computer_failed");
    ToolResult::err(
        serde_json::to_string_pretty(&serde_json::json!({
            "error": {
                "code": code,
                "message": message,
                "automatic_retry": false,
                "mode": "foreground_compatibility"
            }
        }))
        .unwrap_or(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestContext {
        interactive: bool,
    }

    #[async_trait]
    impl ToolContext for TestContext {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new(".")
        }

        fn is_allowed(&self, _name: &str, _input: &Value) -> bool {
            true
        }

        fn supports_interactive_confirmation(&self) -> bool {
            self.interactive
        }
    }

    #[test]
    fn read_only_actions_do_not_need_permission() {
        for action in ["screenshot", "zoom", "cursor_position", "wait"] {
            assert!(is_read_only_action(action), "{action} should be read-only");
        }
    }

    #[test]
    fn mutating_actions_need_permission() {
        for action in [
            "mouse_move",
            "left_click",
            "right_click",
            "middle_click",
            "double_click",
            "left_click_drag",
            "key",
            "type",
            "scroll",
        ] {
            assert!(
                !is_read_only_action(action),
                "{action} should need permission"
            );
        }
    }

    #[test]
    fn dead_corner_detects_all_four_corners() {
        let (w, h) = (1920, 1080);
        assert!(in_dead_corner(0, 0, w, h));
        assert!(in_dead_corner(w as i32 - 1, 0, w, h));
        assert!(in_dead_corner(0, h as i32 - 1, w, h));
        assert!(in_dead_corner(w as i32 - 1, h as i32 - 1, w, h));
        assert!(in_dead_corner(2, 2, w, h)); // 阈值内的近似角落
    }

    #[test]
    fn dead_corner_does_not_trigger_at_screen_center() {
        assert!(!in_dead_corner(960, 540, 1920, 1080));
    }

    #[test]
    fn dead_corner_does_not_trigger_on_an_edge_but_not_a_corner() {
        // 屏幕边缘中点（贴左边但纵向居中）不算失控角，只有四角才算
        assert!(!in_dead_corner(0, 540, 1920, 1080));
    }

    #[test]
    fn get_coord_parses_valid_pair() {
        let input = serde_json::json!({"coordinate": [10, 20]});
        assert_eq!(get_coord(&input, "coordinate").unwrap(), Some((10, 20)));
    }

    #[test]
    fn get_coord_returns_none_when_absent() {
        let input = serde_json::json!({});
        assert_eq!(get_coord(&input, "coordinate").unwrap(), None);
    }

    #[test]
    fn get_coord_rejects_wrong_length() {
        let input = serde_json::json!({"coordinate": [1, 2, 3]});
        assert!(get_coord(&input, "coordinate").is_err());
    }

    #[test]
    fn required_coord_errors_when_missing() {
        let input = serde_json::json!({});
        assert!(required_coord(&input, "coordinate").is_err());
    }

    #[test]
    fn required_region_parses_valid_rect() {
        let input = serde_json::json!({"region": [10, 20, 300, 400]});
        assert_eq!(
            required_region(&input, "region").unwrap(),
            (10, 20, 300, 400)
        );
    }

    #[test]
    fn required_region_errors_when_missing() {
        let input = serde_json::json!({});
        assert!(required_region(&input, "region").is_err());
    }

    #[test]
    fn required_region_rejects_wrong_length() {
        let input = serde_json::json!({"region": [10, 20, 300]});
        assert!(required_region(&input, "region").is_err());
    }

    #[test]
    fn do_zoom_rejects_reversed_region_before_touching_the_display() {
        // x1 <= x0：应在触碰真实屏幕（无 GUI 的沙箱里会失败）之前就被拒绝
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, false);
        let input = serde_json::json!({"action": "zoom", "region": [300, 300, 100, 100]});
        let err = tool.do_zoom(&input).unwrap_err();
        assert!(err.to_string().contains("x1 > x0"));
    }

    #[test]
    fn needs_permission_defaults_to_true_for_unknown_action() {
        let tool = ComputerTool::new_with_policy(
            DEFAULT_MAX_DIM,
            true,
            ForegroundPolicy {
                fallback: ForegroundFallbackPolicy::Ask,
                ..ForegroundPolicy::default()
            },
        );
        let input = serde_json::json!({});
        assert!(tool.needs_permission(&input));
    }

    #[test]
    fn needs_permission_is_false_for_screenshot() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, true);
        let input = serde_json::json!({"action": "screenshot"});
        assert!(!tool.needs_permission(&input));
    }

    #[test]
    fn needs_permission_is_false_for_zoom() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, true);
        let input = serde_json::json!({"action": "zoom", "region": [0, 0, 100, 100]});
        assert!(!tool.needs_permission(&input));
    }

    #[test]
    fn needs_permission_is_true_for_left_click() {
        let tool = ComputerTool::new_with_policy(
            DEFAULT_MAX_DIM,
            true,
            ForegroundPolicy {
                fallback: ForegroundFallbackPolicy::Ask,
                ..ForegroundPolicy::default()
            },
        );
        let input = serde_json::json!({"action": "left_click", "coordinate": [1, 2]});
        assert!(tool.needs_permission(&input));
    }

    #[test]
    fn default_policy_disables_foreground_mutation_without_prompt() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, true);
        assert!(!tool.needs_permission(&serde_json::json!({
            "action": "left_click",
            "coordinate": [1, 2]
        })));
        assert_eq!(
            tool.foreground_policy.fallback,
            ForegroundFallbackPolicy::Disabled
        );
    }

    #[tokio::test]
    async fn foreground_takeover_fails_closed_without_interactive_channel() {
        let tool = ComputerTool::new_with_policy(
            DEFAULT_MAX_DIM,
            false,
            ForegroundPolicy {
                fallback: ForegroundFallbackPolicy::Ask,
                ..ForegroundPolicy::default()
            },
        );
        let error = tool
            .acquire_foreground_lease(&TestContext { interactive: false })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("headless/scheduled"));
    }

    #[tokio::test]
    async fn disabled_foreground_takeover_fails_before_monitor_probe() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, false);
        let error = tool
            .acquire_foreground_lease(&TestContext { interactive: true })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mutations are disabled"));
    }

    #[test]
    fn action_summary_formats_click_with_coordinate() {
        let input = serde_json::json!({"action": "left_click", "coordinate": [10, 20]});
        assert_eq!(summarize_action(&input), "left_click at (10, 20)");
    }

    #[test]
    fn action_summary_formats_type() {
        let input = serde_json::json!({"action": "type", "text": "hello"});
        assert_eq!(summarize_action(&input), "type \"hello\"");
    }

    #[test]
    fn action_summary_appends_held_modifier_for_click() {
        let input = serde_json::json!({
            "action": "left_click",
            "coordinate": [10, 20],
            "text": "shift"
        });
        assert_eq!(
            summarize_action(&input),
            "left_click at (10, 20) holding shift"
        );
    }

    #[test]
    fn action_summary_omits_modifier_suffix_when_text_absent() {
        let input = serde_json::json!({"action": "left_click", "coordinate": [10, 20]});
        assert_eq!(summarize_action(&input), "left_click at (10, 20)");
    }

    #[test]
    fn action_summary_omits_modifier_suffix_when_text_empty() {
        let input = serde_json::json!({
            "action": "left_click",
            "coordinate": [10, 20],
            "text": ""
        });
        assert_eq!(summarize_action(&input), "left_click at (10, 20)");
    }

    #[test]
    fn definition_reports_native_computer_tool_with_target_display_size() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, true);
        let def = tool.definition();
        let native = def.native.expect("computer tool must be native");
        assert_eq!(native.tool_type, "computer_20251124");
        assert_eq!(native.beta, "computer-use-2025-11-24");
        let extra = native.extra.as_object().unwrap();
        assert_eq!(
            extra.get("display_width_px").and_then(Value::as_u64),
            Some(tool.target_width as u64)
        );
        assert_eq!(
            extra.get("display_height_px").and_then(Value::as_u64),
            Some(tool.target_height as u64)
        );
    }

    #[test]
    fn custom_mode_definition_has_no_native_spec_but_full_schema() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, false);
        let def = tool.definition();
        // custom 模式：不带 native 分支，input_schema 和 description 是
        // 真正发给模型的内容（不是仅供本地展示）
        assert!(def.native.is_none());
        assert!(def.input_schema.get("properties").is_some());
        assert_eq!(def.input_schema["required"], serde_json::json!(["action"]));
        // zoom 动作与 region 字段必须出现在真正发给模型的 schema 里
        let actions = def.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert!(actions.iter().any(|a| a == "zoom"));
        assert!(def.input_schema["properties"].get("region").is_some());
    }

    #[test]
    fn custom_mode_description_embeds_actual_target_resolution() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, false);
        let def = tool.definition();
        let expected = format!("{}x{}", tool.target_width, tool.target_height);
        assert!(
            def.description.contains(&expected),
            "description should embed the real target resolution {expected}, got: {}",
            def.description
        );
        // 没有原生工具那层隐式训练兜底，必须自己把动作清单讲清楚
        assert!(def.description.contains("left_click"));
        assert!(def.description.contains("screenshot"));
    }

    #[test]
    fn native_mode_description_is_display_only_placeholder() {
        let tool = ComputerTool::new(DEFAULT_MAX_DIM, true);
        let def = tool.definition();
        assert_eq!(def.description, crate::descriptions::COMPUTER);
    }
}
