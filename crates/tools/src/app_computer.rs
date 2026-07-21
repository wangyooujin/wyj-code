//! macOS 后台优先 computer-use 工具。
//!
//! 与旧版全局 `computer` 不同，本工具的每次动作都绑定稳定窗口目标；点击和
//! 文本走 Accessibility，键盘/滚动最多向目标 PID 定向投递，不移动系统光标、
//! 不主动切前台，也不会在失败时悄悄回退到全局输入。

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use wyj_api::types::{ToolDefinition, ToolResultPart};
use wyj_computer::target::WindowTarget;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct AppComputerTool {
    max_dim: u32,
    focused_quiet_period: Duration,
    observations: Mutex<HashMap<u32, WindowTarget>>,
    /// 某 App/动作若曾在没有人类输入的情况下抢走前台，本会话立即熔断该
    /// 组合，避免同一种不兼容后台动作反复打扰用户。
    incompatible_actions: Mutex<HashSet<(String, String)>>,
}

impl AppComputerTool {
    pub fn new(max_dim: u32, focused_quiet_period: Duration) -> Self {
        Self {
            max_dim,
            focused_quiet_period,
            observations: Mutex::new(HashMap::new()),
            incompatible_actions: Mutex::new(HashSet::new()),
        }
    }

    fn screenshot(&self, input: &Value) -> ToolResult {
        let window_id = match required_u32(input, "window_id") {
            Ok(value) => value,
            Err(error) => return error_result(error),
        };
        if let Some(generation) = input.get("generation").and_then(Value::as_u64) {
            if let Err(error) = wyj_computer::target::validate_window_target(window_id, generation)
            {
                return error_result(error);
            }
        }
        let (target, capture) =
            match wyj_computer::target::capture_window_by_id(window_id, self.max_dim) {
                Ok(result) => result,
                Err(error) => return error_result(error),
            };
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, target.clone());
        let display = serde_json::to_string_pretty(&serde_json::json!({
            "window": target,
            "capture": {
                "pixel_width": capture.physical_width,
                "pixel_height": capture.physical_height,
                "target_width": capture.target_width,
                "target_height": capture.target_height,
                "png_kb": capture.png.len() / 1024,
            }
        }))
        .unwrap_or_else(|_| format!("[app_computer screenshot window_id={window_id}]"));
        ToolResult::with_parts(
            display,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_string(),
                data: capture.png_base64(),
            }],
        )
    }

    fn resolve_observation(&self, input: &Value) -> Result<WindowTarget> {
        let window_id = required_u32(input, "window_id")?;
        let generation = required_u64(input, "generation")?;
        let mut current = wyj_computer::target::validate_window_target(window_id, generation)?;

        if let Some(observed) = self
            .observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&window_id)
            .filter(|observed| observed.generation == generation)
            .cloned()
        {
            current.target_width = observed.target_width;
            current.target_height = observed.target_height;
            return Ok(current);
        }

        current.target_width = required_u32(input, "target_width")?;
        current.target_height = required_u32(input, "target_height")?;
        anyhow::ensure!(
            current.target_width > 0 && current.target_height > 0,
            "target_changed: invalid captured coordinate space; take a new window screenshot"
        );
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(window_id, current.clone());
        Ok(current)
    }

    fn target_region(target: &WindowTarget) -> wyj_computer::activity::InputRegion {
        wyj_computer::activity::InputRegion {
            x: f64::from(target.x),
            y: f64::from(target.y),
            width: f64::from(target.width),
            height: f64::from(target.height),
        }
    }

    fn begin_mutation(
        &self,
        target: &WindowTarget,
        action: &str,
    ) -> Result<(wyj_computer::activity::InputLease, u32)> {
        if self
            .incompatible_actions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&(target.app_name.clone(), action.to_string()))
        {
            return Err(anyhow!(
                "requires_foreground_takeover: background {action} was disabled for `{}` after it changed the user's foreground application",
                target.app_name
            ));
        }

        let frontmost_pid = wyj_computer::accessibility::frontmost_pid()?;
        let quiet = if frontmost_pid == target.pid {
            self.focused_quiet_period
        } else {
            Duration::ZERO
        };
        let lease = wyj_computer::activity::acquire_lease(quiet)?;
        self.ensure_mutation_safe(target, &lease, frontmost_pid == target.pid)?;
        Ok((lease, frontmost_pid))
    }

    fn ensure_mutation_safe(
        &self,
        target: &WindowTarget,
        lease: &wyj_computer::activity::InputLease,
        target_frontmost: bool,
    ) -> Result<()> {
        let conflict = if target_frontmost {
            !wyj_computer::activity::lease_is_valid(lease)
        } else {
            wyj_computer::activity::conflicts_with_background_target(
                lease,
                Self::target_region(target),
                false,
            )
        };
        if conflict {
            return Err(anyhow!(
                "preempted_by_user: external input may affect the target window; do not retry automatically"
            ));
        }
        Ok(())
    }

    fn finish_mutation(
        &self,
        target: &WindowTarget,
        action: &str,
        lease: &wyj_computer::activity::InputLease,
        frontmost_before: u32,
    ) -> Result<()> {
        let frontmost_after = wyj_computer::accessibility::frontmost_pid()?;
        if frontmost_after != frontmost_before {
            if !wyj_computer::activity::lease_is_valid(lease) {
                return Err(anyhow!(
                    "preempted_by_user: the user changed the foreground application during the background action"
                ));
            }
            self.incompatible_actions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert((target.app_name.clone(), action.to_string()));
            wyj_computer::telemetry::record_background_focus_fuse();
            return Err(anyhow!(
                "target_changed: background {action} on `{}` changed the user's foreground application ({frontmost_before} -> {frontmost_after}); this App/action is disabled for the rest of the session",
                target.app_name
            ));
        }

        self.ensure_mutation_safe(target, lease, frontmost_after == target.pid)
    }

    fn inspect(&self, input: &Value) -> ToolResult {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let (x, y) = match required_coordinate(input) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (global_x, global_y) = match target.screenshot_to_global(x, y) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        match wyj_computer::accessibility::inspect(target.pid, global_x, global_y) {
            Ok(info) => ToolResult::ok(
                serde_json::to_string_pretty(&info).unwrap_or_else(|_| format!("{info:?}")),
            ),
            Err(error) => error_result(error),
        }
    }

    fn click(&self, input: &Value) -> ToolResult {
        self.coordinate_mutation(input, "click", |target, x, y| {
            wyj_computer::accessibility::press(target.pid, x, y)?;
            Ok("clicked with AXPress".to_string())
        })
    }

    fn set_text(&self, input: &Value) -> ToolResult {
        let text = match input.get("text").and_then(Value::as_str) {
            Some(text) => text,
            None => return ToolResult::err("missing required field `text`"),
        };
        self.coordinate_mutation(input, "set_text", |target, x, y| {
            let attribute = wyj_computer::accessibility::set_text(target.pid, x, y, text)?;
            Ok(format!("set text using {attribute}"))
        })
    }

    fn scroll(&self, input: &Value) -> ToolResult {
        let direction = match input.get("scroll_direction").and_then(Value::as_str) {
            Some(direction) => direction,
            None => return ToolResult::err("missing required field `scroll_direction`"),
        };
        let amount = input
            .get("scroll_amount")
            .and_then(Value::as_u64)
            .and_then(|amount| u32::try_from(amount).ok())
            .unwrap_or(1)
            .clamp(1, 20);
        self.coordinate_mutation(input, "scroll", |target, x, y| {
            if wyj_computer::accessibility::scroll(target.pid, x, y, direction, amount)? {
                Ok(format!(
                    "scrolled {direction} x{amount} using Accessibility"
                ))
            } else {
                wyj_computer::targeted_event::scroll(target.pid, direction, amount)?;
                Ok(format!(
                    "scrolled {direction} x{amount} using target PID event"
                ))
            }
        })
    }

    fn key(&self, input: &Value) -> ToolResult {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let combo = match input.get("key").and_then(Value::as_str) {
            Some(combo) if !combo.trim().is_empty() => combo,
            _ => return ToolResult::err("missing required field `key`"),
        };
        let (lease, frontmost_before) = match self.begin_mutation(&target, "key") {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        let action_result = wyj_computer::targeted_event::key(target.pid, combo);
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, "key", &lease, frontmost_before) {
            return error_result(error);
        }
        if let Err(error) = action_result {
            return error_result(error);
        }
        ToolResult::ok(format!("sent key `{combo}` to pid {}", target.pid))
    }

    fn coordinate_mutation<F>(&self, input: &Value, action_name: &str, action: F) -> ToolResult
    where
        F: FnOnce(&WindowTarget, f64, f64) -> Result<String>,
    {
        let target = match self.resolve_observation(input) {
            Ok(target) => target,
            Err(error) => return error_result(error),
        };
        let (x, y) = match required_coordinate(input) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (global_x, global_y) = match target.screenshot_to_global(x, y) {
            Ok(coordinate) => coordinate,
            Err(error) => return error_result(error),
        };
        let (lease, frontmost_before) = match self.begin_mutation(&target, action_name) {
            Ok(guard) => guard,
            Err(error) => return error_result(error),
        };
        let action_result = action(&target, global_x, global_y);
        wyj_computer::telemetry::record_background_action();
        if let Err(error) = self.finish_mutation(&target, action_name, &lease, frontmost_before) {
            return error_result(error);
        }
        match action_result {
            Ok(content) => ToolResult::ok(content),
            Err(error) => error_result(error),
        }
    }
}

const DESCRIPTION: &str = "Controls a specific macOS application window in the background without moving the user's pointer or activating the app. Prefer this over the legacy `computer` tool. Workflow: list/capture with `window_capture` or this tool, then pass the exact `window_id`, `generation`, and captured `target_width`/`target_height` for actions. `click` uses AXPress; `set_text` writes AXValue/AXSelectedText; `key` and unsupported semantic scrolling are posted only to the target PID. This tool NEVER silently falls back to global mouse/keyboard input. On `target_changed`, capture again. On `requires_foreground_takeover`, `preempted_by_user`, `user_active`, or permission errors, stop and report instead of retrying.";

#[async_trait]
impl Tool for AppComputerTool {
    fn name(&self) -> &str {
        "app_computer"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list_windows", "screenshot", "inspect_element", "click", "set_text", "key", "scroll"]
                    },
                    "window_id": {"type": "integer", "minimum": 0},
                    "generation": {"type": "integer", "minimum": 0},
                    "target_width": {"type": "integer", "minimum": 1},
                    "target_height": {"type": "integer", "minimum": 1},
                    "coordinate": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "scroll_direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"]
                    },
                    "scroll_amount": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["action"]
            }),
            native: None,
        }
    }

    fn needs_permission(&self, input: &Value) -> bool {
        !matches!(
            input.get("action").and_then(Value::as_str),
            Some("list_windows" | "screenshot" | "inspect_element")
        )
    }

    fn action_summary(&self, input: &Value) -> String {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let window = input
            .get("window_id")
            .and_then(Value::as_u64)
            .map(|id| format!("window {id}"))
            .unwrap_or_else(|| "unknown window".to_string());
        format!("{action} in background on {window}")
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing `action` field"))?;
        Ok(match action {
            "list_windows" => match wyj_computer::target::list_windows() {
                Ok(windows) => ToolResult::ok(
                    serde_json::to_string_pretty(&serde_json::json!({
                        "count": windows.len(),
                        "windows": windows,
                    }))
                    .unwrap_or_else(|_| "{\"windows\":[]}".to_string()),
                ),
                Err(error) => error_result(error),
            },
            "screenshot" => self.screenshot(&input),
            "inspect_element" => self.inspect(&input),
            "click" => self.click(&input),
            "set_text" => self.set_text(&input),
            "key" => self.key(&input),
            "scroll" => self.scroll(&input),
            other => ToolResult::err(format!("unsupported app_computer action `{other}`")),
        })
    }
}

fn required_u32(input: &Value, field: &str) -> Result<u32> {
    input
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| anyhow!("missing or invalid required field `{field}`"))
}

fn required_u64(input: &Value, field: &str) -> Result<u64> {
    input
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing or invalid required field `{field}`"))
}

fn required_coordinate(input: &Value) -> Result<(i32, i32)> {
    let coordinate = input
        .get("coordinate")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing required field `coordinate`"))?;
    anyhow::ensure!(
        coordinate.len() == 2,
        "`coordinate` must contain exactly 2 integers"
    );
    let x = coordinate[0]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow!("invalid coordinate x"))?;
    let y = coordinate[1]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| anyhow!("invalid coordinate y"))?;
    Ok((x, y))
}

fn error_result(error: impl std::fmt::Display) -> ToolResult {
    let message = error.to_string();
    wyj_computer::telemetry::record_error_message(&message);
    let code = [
        "target_changed",
        "requires_foreground_takeover",
        "preempted_by_user",
        "user_active",
        "screen_locked",
        "input_monitor_unavailable",
        "accessibility_permission_required",
    ]
    .into_iter()
    .find(|code| message.contains(code))
    .unwrap_or("app_computer_failed");
    ToolResult::err(
        serde_json::to_string_pretty(&serde_json::json!({
            "error": {
                "code": code,
                "message": message,
                "automatic_retry": false,
                "foreground_fallback_used": false,
            }
        }))
        .unwrap_or(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_is_custom_and_background_explicit() {
        let definition = AppComputerTool::new(1280, Duration::from_secs(2)).definition();
        assert!(definition.native.is_none());
        assert!(definition.description.contains("NEVER silently falls back"));
        assert_eq!(
            definition.input_schema["properties"]["action"]["enum"][0],
            "list_windows"
        );
    }

    #[test]
    fn read_only_actions_do_not_request_permission() {
        let tool = AppComputerTool::new(1280, Duration::from_secs(2));
        assert!(!tool.needs_permission(&serde_json::json!({"action": "screenshot"})));
        assert!(tool.needs_permission(&serde_json::json!({"action": "click"})));
    }

    #[test]
    fn errors_are_structured_and_never_claim_foreground_fallback() {
        let result = error_result(anyhow!("target_changed: moved"));
        assert!(result.is_error);
        assert!(result.content.contains("target_changed"));
        assert!(result
            .content
            .contains("\"foreground_fallback_used\": false"));
    }
}
