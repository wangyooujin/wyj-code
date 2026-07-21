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

const DESCRIPTION: &str = "Discovers and captures application windows without activating them, moving the pointer, or changing keyboard focus. Start with action `list` (optionally filter with `query`), then call `capture` with the exact `window_id`. The capture result includes a `generation`; pass both `window_id` and `generation` to `app_computer` background actions. If generation changes, discard old coordinates and capture again. A legacy call containing only `query` is treated as capture-by-query for compatibility, but stable IDs are preferred.";

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
        "window": target,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_exposes_stable_window_workflow() {
        let definition = WindowCaptureTool::new(1280).definition();
        assert!(definition.native.is_none());
        assert!(definition.input_schema["properties"]["window_id"].is_object());
        assert_eq!(
            definition.input_schema["properties"]["action"]["enum"],
            serde_json::json!(["list", "capture"])
        );
    }
}
