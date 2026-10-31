//! 稳定窗口发现与后台截图工具。
//!
//! `list` 返回可见窗口及其稳定 `window_id/generation`；`capture` 必须按 id
//! 精确捕获，并把截图坐标空间写回目标元数据。保留 `query` 作为首次发现的
//! 兼容入口，但后续动作应始终使用返回的 id/generation，不能反复模糊匹配。

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use wyj_api::types::{ToolDefinition, ToolResultPart};
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct WindowCaptureTool {
    max_dim: u32,
}

impl WindowCaptureTool {
    pub fn new(max_dim: u32) -> Self {
        Self { max_dim }
    }
}

const DESCRIPTION: &str = "Discovers and captures application windows without activating them, moving the pointer, or changing keyboard focus. Start with action `list` (optionally filter with `query`), then call `capture` with the exact `window_id`. The capture result includes `generation` as a decimal string; copy that string exactly into `app_computer` and do not send it as a JSON number (it is a u64 past 2^53 and a JSON number is rounded). A list leaves `target_width` and `target_height` at 0; the capture metadata sizes are the screenshot coordinate space, not the window's point size. If generation changes, discard old coordinates and capture again. A legacy call containing only `query` is treated as capture-by-query for compatibility, but stable IDs are preferred.";

fn list_result(query: Option<&str>) -> ToolResult {
    let mut windows = match wyj_computer::target::list_windows() {
        Ok(windows) => windows,
        Err(error) => return ToolResult::err(format!("window_capture list failed: {error}")),
    };
    if let Some(query) = query.filter(|query| !query.trim().is_empty()) {
        let query = query.to_lowercase();
        windows.retain(|window| {
            window.app_name.to_lowercase().contains(&query)
                || window.title.to_lowercase().contains(&query)
        });
    }
    let windows: Vec<Value> = windows.iter().map(window_for_model).collect();
    match serde_json::to_string_pretty(&serde_json::json!({
        "count": windows.len(),
        "windows": windows,
    })) {
        Ok(json) => ToolResult::ok(json),
        Err(error) => ToolResult::err(format!("window_capture list serialization failed: {error}")),
    }
}

fn capture_result(window_id: u32, max_dim: u32) -> ToolResult {
    let (target, capture) = match wyj_computer::target::capture_window_by_id(window_id, max_dim) {
        Ok(result) => result,
        Err(error) => return ToolResult::err(format!("window_capture failed: {error}")),
    };
    let metadata = serde_json::json!({
        "window": window_for_model(&target),
        "capture": {
            "pixel_width": capture.physical_width,
            "pixel_height": capture.physical_height,
            "target_width": capture.target_width,
            "target_height": capture.target_height,
            "png_kb": capture.png.len() / 1024,
        }
    });
    let display = serde_json::to_string_pretty(&metadata)
        .unwrap_or_else(|_| format!("[window_capture id={window_id}]"));
    ToolResult::with_parts(
        display,
        vec![ToolResultPart::Image {
            media_type: "image/png".to_string(),
            data: capture.png_base64(),
        }],
    )
}

#[async_trait]
impl Tool for WindowCaptureTool {
    fn name(&self) -> &str {
        "window_capture"
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
                        "enum": ["list", "capture"],
                        "description": "Use list for discovery and capture for a stable-ID screenshot. If omitted with query, legacy capture-by-query is used."
                    },
                    "window_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Exact window ID returned by list. Required for capture unless using legacy query discovery."
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional case-insensitive app/title filter for list, or legacy initial capture lookup."
                    }
                }
            }),
            native: None,
        }
    }

    /// `list` / `capture` 都是纯只读截图：不激活窗口、不移动指针、不改焦点，
    /// 因此同批次里截多个窗口互不影响，可以并发。
    ///
    /// 截图像素级开销不小（每次 capture 都要编码 PNG），串行排会让
    /// 「先 list 再 capture 多个窗口」这类正常流程变成一串加法。
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let action = input.get("action").and_then(Value::as_str);
        let query = input.get("query").and_then(Value::as_str);
        match action {
            Some("list") => Ok(list_result(query)),
            Some("capture") => {
                let Some(window_id) = input
                    .get("window_id")
                    .and_then(Value::as_u64)
                    .and_then(|id| u32::try_from(id).ok())
                else {
                    return Ok(ToolResult::err(
                        "missing or invalid required field `window_id` for capture",
                    ));
                };
                Ok(capture_result(window_id, self.max_dim))
            }
            Some(other) => Ok(ToolResult::err(format!(
                "unsupported window_capture action `{other}`"
            ))),
            None => {
                let Some(query) = query.filter(|query| !query.trim().is_empty()) else {
                    return Ok(ToolResult::err(
                        "missing `action`; use action=list or action=capture",
                    ));
                };
                let target = match wyj_computer::target::find_window_by_query(query) {
                    Ok(target) => target,
                    Err(error) => {
                        return Ok(ToolResult::err(format!("window_capture failed: {error}")))
                    }
                };
                Ok(capture_result(target.window_id, self.max_dim))
            }
        }
    }
}

/// 给模型的窗口 JSON。`generation` 必须是十进制字符串。
///
/// 它是 FNV-1a u64，超过 JSON 安全整数 2^53。写成 JSON number 后，任何
/// JavaScript 解析器都会把它舍入，模型再抄回来就对不上 `validate_window_target`。
pub(crate) fn window_for_model(target: &wyj_computer::target::WindowTarget) -> Value {
    let mut value = serde_json::to_value(target).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "generation".to_string(),
            Value::String(target.generation.to_string()),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_computer::target::WindowTarget;

    #[test]
    fn generation_is_emitted_as_an_exact_decimal_string() {
        let target = WindowTarget {
            window_id: 4303,
            pid: 1,
            app_name: "Weather".into(),
            title: "天气".into(),
            x: 0,
            y: 0,
            width: 1400,
            height: 860,
            target_width: 0,
            target_height: 0,
            generation: 17_902_492_282_727_121_041,
            focused: false,
        };
        let value = window_for_model(&target);
        assert_eq!(
            value["generation"],
            serde_json::json!("17902492282727121041")
        );
        assert!(value["generation"].is_string());
        assert_eq!(value["target_width"], 0);
        assert_eq!(value["target_height"], 0);
    }

    #[test]
    fn definition_exposes_stable_window_workflow() {
        let definition = WindowCaptureTool::new(1280).definition();
        assert!(definition.native.is_none());
        assert!(definition.input_schema["properties"]["window_id"].is_object());
        assert_eq!(
            definition.input_schema["properties"]["action"]["enum"],
            serde_json::json!(["list", "capture"])
        );
        assert!(definition.description.contains("decimal string"));
    }
}
