//! AskQuestion 工具 — 向用户提问并等待其选择一个选项

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{Tool, ToolContext, ToolResult};

pub struct AskQuestionTool;

impl AskQuestionTool {
    pub fn new() -> Self {
        Self
    }
}

/// 从 JSON Value 中提取字符串：直接是 string、或对象中的 label/text/value 字段
fn coerce_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(m) => {
            for key in &["label", "text", "value", "option", "name"] {
                if let Some(Value::String(s)) = m.get(*key) {
                    return s.clone();
                }
            }
            v.to_string()
        }
        _ => v.to_string(),
    }
}

#[async_trait]
impl Tool for AskQuestionTool {
    fn name(&self) -> &str {
        "AskQuestion"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "向用户提问并等待其选择一个选项。仅当需要用户明确做出选择时使用，\
                          不要用于可以自行决策的情况。\
                          question 是问题文本，options 是 2-4 个纯字符串选项。\
                          返回用户选中的选项文字，若用户取消则返回 [已取消]。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "向用户展示的问题"
                    },
                    "options": {
                        "type": "array",
                        "description": "选项列表（2-4 个纯字符串，不要用对象）",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "maxItems": 4
                    }
                },
                "required": ["question", "options"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let question = match input.get("question") {
            Some(v) => coerce_string(v),
            None => return Ok(ToolResult::err("缺少 question 字段")),
        };

        let options: Vec<String> = match input.get("options").and_then(|v| v.as_array()) {
            Some(arr) => arr.iter().map(coerce_string).collect(),
            None => return Ok(ToolResult::err("缺少 options 字段或不是数组")),
        };

        if options.is_empty() {
            return Ok(ToolResult::err("选项不能为空"));
        }

        match ctx.ask_user(&question, &options).await {
            Some(idx) if idx < options.len() => Ok(ToolResult::ok(options[idx].clone())),
            _ => Ok(ToolResult::ok("[已取消]")),
        }
    }
}
