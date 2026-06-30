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

/// 尝试将 LLM 错误地打包进单字符串的多选项拆分出来。
/// 支持换行分隔 ("A. 是\nB. 否") 和字母前缀逗号分隔 ("A. 是, B. 否")。
fn try_split_packed_options(s: &str) -> Option<Vec<String>> {
    // 1. 优先按换行拆
    let by_newline: Vec<String> = s
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if by_newline.len() >= 2 {
        return Some(by_newline);
    }

    // 2. 检测 "X. " 形式的字母前缀分割点（B/C/D 出现在空格或逗号/分号后面）
    let mut split_points: Vec<usize> = vec![0];
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for i in 1..len {
        let ch = chars[i];
        if matches!(ch, 'B' | 'C' | 'D')
            && i + 2 < len
            && chars[i + 1] == '.'
            && chars[i + 2] == ' '
        {
            // 前一个非空白字符应是分隔符（逗号、分号、空格）
            let prev = chars[..i]
                .iter()
                .rev()
                .find(|&&c| c != ' ')
                .copied()
                .unwrap_or(',');
            if matches!(prev, ',' | ';' | '，' | '；') {
                split_points.push(i);
            }
        }
    }

    if split_points.len() < 2 {
        return None;
    }

    let s_chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    for (j, &start) in split_points.iter().enumerate() {
        let end = if j + 1 < split_points.len() {
            split_points[j + 1]
        } else {
            s_chars.len()
        };
        let part: String = s_chars[start..end]
            .iter()
            .collect::<String>()
            .trim_start_matches(|c: char| c == ',' || c == '，' || c == ';' || c == '；' || c == ' ')
            .trim()
            .to_string();
        if !part.is_empty() {
            parts.push(part);
        }
    }

    if parts.len() >= 2 {
        Some(parts)
    } else {
        None
    }
}

/// 从 JSON Value 中解析选项列表，兼容 LLM 将多选项打包成单字符串的情况。
fn parse_options(raw: &Value) -> Option<Vec<String>> {
    // 标准路径：数组
    if let Some(arr) = raw.as_array() {
        let strings: Vec<String> = arr.iter().map(coerce_string).collect();
        if strings.len() >= 2 {
            return Some(strings);
        }
        // 只有 1 个元素时尝试拆包
        if let Some(single) = strings.first() {
            if let Some(split) = try_split_packed_options(single) {
                return Some(split);
            }
        }
        return if strings.is_empty() { None } else { Some(strings) };
    }
    // 容错路径：options 直接是字符串
    if let Some(s) = raw.as_str() {
        if let Some(split) = try_split_packed_options(s) {
            return Some(split);
        }
        return Some(vec![s.to_string()]);
    }
    None
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

        let options: Vec<String> = match input.get("options").and_then(|v| parse_options(v)) {
            Some(opts) if !opts.is_empty() => opts,
            _ => return Ok(ToolResult::err("缺少 options 字段或无法解析选项")),
        };

        match ctx.ask_user(&question, &options).await {
            Some(idx) if idx < options.len() => Ok(ToolResult::ok(options[idx].clone())),
            _ => Ok(ToolResult::ok("[已取消]")),
        }
    }
}
