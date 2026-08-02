//! 工具参数的 fail-closed 解析、有限修复与 JSON Schema 校验。

use std::{collections::HashMap, sync::Arc};

use jsonschema::{error::ValidationErrorKind, Draft, JSONSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wyj_api::types::{RawToolCall, ToolDefinition};

/// 给能力较弱模型暴露的低噪声 schema。执行校验始终使用注册时的原始 schema。
pub fn simplified_tool_definition(definition: &ToolDefinition) -> ToolDefinition {
    let mut simplified = definition.clone();
    simplify_schema_value(&mut simplified.input_schema);
    simplified
}

fn simplify_schema_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for annotation in ["$schema", "title", "description", "default", "examples"] {
                object.remove(annotation);
            }
            for child in object.values_mut() {
                simplify_schema_value(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                simplify_schema_value(child);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolArgumentErrorKind {
    UnknownTool,
    InvalidSchema,
    InvalidJson,
    NotAnObject,
    SchemaViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArgumentIssue {
    pub path: String,
    pub expected: String,
    pub actual: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArgumentError {
    #[serde(rename = "error")]
    pub code: String,
    pub kind: ToolArgumentErrorKind,
    pub tool: String,
    pub issues: Vec<ToolArgumentIssue>,
    pub instruction: String,
}

impl ToolArgumentError {
    fn new(
        tool: impl Into<String>,
        kind: ToolArgumentErrorKind,
        issues: Vec<ToolArgumentIssue>,
    ) -> Self {
        Self {
            code: "tool_arguments_invalid".to_string(),
            kind,
            tool: tool.into(),
            issues,
            instruction: "Regenerate only this tool call. Do not execute another tool.".to_string(),
        }
    }

    pub fn feedback_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"error":"tool_arguments_invalid","instruction":"Regenerate only this tool call."}"#
                .to_string()
        })
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// 是否应用了纯语法修复；只用于可见 telemetry，不改变执行语义。
    pub syntax_repaired: bool,
}

#[derive(Clone)]
enum RegisteredSchema {
    Compiled(Arc<JSONSchema>),
    Invalid(String),
}

/// 工具定义注册时预编译 schema；运行时只校验实例。
#[derive(Clone, Default)]
pub struct ToolArgumentPipeline {
    schemas: HashMap<String, RegisteredSchema>,
}

impl ToolArgumentPipeline {
    pub fn register(&mut self, definition: &ToolDefinition) {
        let compiled = JSONSchema::options()
            .with_draft(Draft::Draft7)
            .compile(&definition.input_schema)
            .map(Arc::new)
            .map(RegisteredSchema::Compiled)
            .unwrap_or_else(|error| RegisteredSchema::Invalid(bounded(error.to_string(), 500)));
        self.schemas.insert(definition.name.clone(), compiled);
    }

    pub fn remove_where(&mut self, mut predicate: impl FnMut(&str) -> bool) {
        self.schemas.retain(|name, _| !predicate(name));
    }

    pub fn process(&self, call: RawToolCall) -> Result<ValidatedToolCall, ToolArgumentError> {
        let schema = match self.schemas.get(&call.name) {
            Some(RegisteredSchema::Compiled(schema)) => schema,
            Some(RegisteredSchema::Invalid(reason)) => {
                return Err(ToolArgumentError::new(
                    call.name,
                    ToolArgumentErrorKind::InvalidSchema,
                    vec![ToolArgumentIssue {
                        path: "$".to_string(),
                        expected: "valid_json_schema".to_string(),
                        actual: reason.clone(),
                    }],
                ));
            }
            None => {
                return Err(ToolArgumentError::new(
                    call.name,
                    ToolArgumentErrorKind::UnknownTool,
                    vec![ToolArgumentIssue {
                        path: "$".to_string(),
                        expected: "registered_tool".to_string(),
                        actual: "unknown_tool".to_string(),
                    }],
                ));
            }
        };

        let (input, syntax_repaired) = parse_with_conservative_repair(&call.raw_arguments)
            .map_err(|actual| {
                ToolArgumentError::new(
                    call.name.clone(),
                    ToolArgumentErrorKind::InvalidJson,
                    vec![ToolArgumentIssue {
                        path: "$".to_string(),
                        expected: "json_object".to_string(),
                        actual,
                    }],
                )
            })?;

        if !input.is_object() {
            return Err(ToolArgumentError::new(
                call.name,
                ToolArgumentErrorKind::NotAnObject,
                vec![ToolArgumentIssue {
                    path: "$".to_string(),
                    expected: "object".to_string(),
                    actual: json_type(&input).to_string(),
                }],
            ));
        }

        if let Err(errors) = schema.validate(&input) {
            let issues = errors
                .take(16)
                .map(|error| validation_issue(&error))
                .collect();
            return Err(ToolArgumentError::new(
                call.name,
                ToolArgumentErrorKind::SchemaViolation,
                issues,
            ));
        }

        Ok(ValidatedToolCall {
            id: call.id,
            name: call.name,
            input,
            syntax_repaired,
        })
    }
}

fn validation_issue(error: &jsonschema::ValidationError<'_>) -> ToolArgumentIssue {
    let path = if error.instance_path.to_string().is_empty() {
        "$".to_string()
    } else {
        format!("${}", error.instance_path)
    };
    let (path, expected, actual) = match &error.kind {
        ValidationErrorKind::Required { property } => {
            let property = property.as_str().unwrap_or("<unknown>");
            (
                format!("{path}.{property}"),
                "required".to_string(),
                "missing".to_string(),
            )
        }
        ValidationErrorKind::Type { kind } => (
            path,
            format!("type:{kind:?}"),
            json_type(error.instance.as_ref()).to_string(),
        ),
        ValidationErrorKind::Enum { options } => (
            path,
            format!("enum:{options}"),
            bounded(error.instance.to_string(), 120),
        ),
        ValidationErrorKind::AdditionalProperties { unexpected } => (
            path,
            "no_additional_properties".to_string(),
            format!("unexpected:{}", unexpected.join(",")),
        ),
        ValidationErrorKind::Minimum { limit }
        | ValidationErrorKind::ExclusiveMinimum { limit } => (
            path,
            format!("minimum:{limit}"),
            bounded(error.instance.to_string(), 120),
        ),
        ValidationErrorKind::Maximum { limit }
        | ValidationErrorKind::ExclusiveMaximum { limit } => (
            path,
            format!("maximum:{limit}"),
            bounded(error.instance.to_string(), 120),
        ),
        _ => (
            path,
            bounded(error.to_string(), 240),
            json_type(error.instance.as_ref()).to_string(),
        ),
    };
    ToolArgumentIssue {
        path,
        expected,
        actual,
    }
}

fn parse_with_conservative_repair(raw: &str) -> Result<(Value, bool), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty_arguments".to_string());
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok((value, false));
    }

    let mut candidate = strip_single_code_fence(trimmed)
        .unwrap_or(trimmed)
        .to_string();
    if let Some(extracted) = extract_unique_json_object(&candidate) {
        candidate = extracted.to_string();
    }
    candidate = remove_trailing_commas(&candidate);
    let before_closing = candidate.clone();
    candidate = close_unclosed_containers(&candidate)
        .ok_or_else(|| "invalid_or_truncated_json".to_string())?;

    serde_json::from_str::<Value>(&candidate)
        .and_then(|value| {
            if candidate != before_closing
                && value.as_object().is_some_and(serde_json::Map::is_empty)
            {
                Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "container-only repair would create an empty object",
                )))
            } else {
                Ok(value)
            }
        })
        .map(|value| (value, true))
        .map_err(|error| bounded(format!("json_parse_error:{error}"), 240))
}

fn strip_single_code_fence(input: &str) -> Option<&str> {
    let rest = input.strip_prefix("```")?;
    let first_newline = rest.find('\n')?;
    let body = &rest[first_newline + 1..];
    let body = body.strip_suffix("```")?.trim_end();
    if body.contains("```") {
        None
    } else {
        Some(body.trim())
    }
}

/// 只在文本中恰好存在一个完整顶层 JSON object 时提取它。
fn extract_unique_json_object(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    let mut spans = Vec::new();
    let mut start = None;
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (idx, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte == b'"' {
            in_string = true;
            continue;
        }
        match byte {
            b'{' => {
                if stack.is_empty() {
                    start = Some(idx);
                }
                stack.push(byte);
            }
            b'[' if !stack.is_empty() => stack.push(byte),
            b'}' => {
                if stack.pop() != Some(b'{') {
                    stack.clear();
                    start = None;
                    continue;
                }
                if stack.is_empty() {
                    if let Some(begin) = start.take() {
                        spans.push((begin, idx + 1));
                    }
                }
            }
            b']' if !stack.is_empty() && stack.pop() != Some(b'[') => {
                stack.clear();
                start = None;
            }
            _ => {}
        }
    }

    if spans.len() == 1 {
        Some(&input[spans[0].0..spans[0].1])
    } else {
        None
    }
}

fn remove_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_string {
            out.push(byte as char);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            idx += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            out.push('"');
            idx += 1;
            continue;
        }
        if byte == b',' {
            let mut next = idx + 1;
            while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                next += 1;
            }
            if next < bytes.len() && matches!(bytes[next], b'}' | b']') {
                idx += 1;
                continue;
            }
        }
        out.push(byte as char);
        idx += 1;
    }
    out
}

/// 只补全已完整结束 token 之后缺失的 `}`/`]`。字符串未闭合或括号错配时拒绝，
/// 避免把截断的路径、命令或编辑内容“修”成可执行值。
fn close_unclosed_containers(input: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.pop() != Some(ch) => return None,
            _ => {}
        }
    }
    if in_string || escaped {
        return None;
    }
    let mut output = input.to_string();
    while let Some(closer) = stack.pop() {
        output.push(closer);
    }
    Some(output)
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn bounded(value: impl Into<String>, max_chars: usize) -> String {
    let value = value.into();
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pipeline(schema: Value) -> ToolArgumentPipeline {
        let mut pipeline = ToolArgumentPipeline::default();
        pipeline.register(&ToolDefinition {
            name: "Edit".to_string(),
            description: String::new(),
            input_schema: schema,
            native: None,
        });
        pipeline
    }

    fn call(raw: &str) -> RawToolCall {
        RawToolCall {
            id: "tool-1".to_string(),
            name: "Edit".to_string(),
            raw_arguments: raw.to_string(),
        }
    }

    #[test]
    fn strict_valid_object_passes_without_repair() {
        let p = pipeline(json!({
            "type": "object",
            "required": ["file_path"],
            "properties": {"file_path": {"type": "string"}},
            "additionalProperties": false
        }));
        let result = p.process(call(r#"{"file_path":"src/lib.rs"}"#)).unwrap();
        assert!(!result.syntax_repaired);
        assert_eq!(result.input["file_path"], "src/lib.rs");
    }

    #[test]
    fn repairs_fence_surrounding_text_trailing_comma_and_complete_brace() {
        let p = pipeline(json!({"type": "object", "required": ["n"]}));
        for raw in [
            "```json\n{\"n\":1}\n```",
            "arguments: {\"n\":1} end",
            "{\"n\":1,}",
            "{\"n\":1",
        ] {
            let result = p.process(call(raw)).unwrap();
            assert!(result.syntax_repaired, "expected repair for {raw}");
            assert_eq!(result.input["n"], 1);
        }
    }

    #[test]
    fn never_guesses_truncated_string_value() {
        let p = pipeline(json!({
            "type": "object",
            "required": ["file_path"],
            "properties": {"file_path": {"type": "string"}}
        }));
        let error = p.process(call(r#"{"file_path":"src/lib"#)).unwrap_err();
        assert_eq!(error.kind, ToolArgumentErrorKind::InvalidJson);
    }

    #[test]
    fn rejects_null_array_and_ambiguous_multiple_objects() {
        let p = pipeline(json!({"type": "object"}));
        for raw in ["null", "[]"] {
            let error = p.process(call(raw)).unwrap_err();
            assert_eq!(error.kind, ToolArgumentErrorKind::NotAnObject);
        }
        let error = p
            .process(call("first {\"a\":1} second {\"b\":2}"))
            .unwrap_err();
        assert_eq!(error.kind, ToolArgumentErrorKind::InvalidJson);
    }

    #[test]
    fn schema_required_enum_range_and_additional_properties_are_enforced() {
        let p = pipeline(json!({
            "type": "object",
            "required": ["mode", "count"],
            "properties": {
                "mode": {"type": "string", "enum": ["safe"]},
                "count": {"type": "integer", "minimum": 1, "maximum": 3}
            },
            "additionalProperties": false
        }));
        for raw in [
            r#"{"count":1}"#,
            r#"{"mode":"unsafe","count":1}"#,
            r#"{"mode":"safe","count":9}"#,
            r#"{"mode":"safe","count":1,"command":"rm"}"#,
        ] {
            let error = p.process(call(raw)).unwrap_err();
            assert_eq!(error.kind, ToolArgumentErrorKind::SchemaViolation);
            assert!(!error.issues.is_empty());
        }
    }

    #[test]
    fn parse_failure_never_becomes_empty_object() {
        let p = pipeline(json!({"type": "object"}));
        for raw in ["", "not-json", "{", r#"{"command":"echo"#] {
            assert!(p.process(call(raw)).is_err(), "must reject {raw:?}");
        }
    }

    #[test]
    fn simplified_schema_removes_annotations_but_keeps_execution_constraints() {
        let definition = ToolDefinition {
            name: "Echo".to_string(),
            description: "tool description".to_string(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "description": "noise",
                "required": ["mode"],
                "properties": {
                    "mode": {"type": "string", "enum": ["safe"], "description": "noise"}
                },
                "additionalProperties": false
            }),
            native: None,
        };
        let simplified = simplified_tool_definition(&definition);
        assert!(simplified.input_schema.get("description").is_none());
        assert_eq!(simplified.input_schema["required"], json!(["mode"]));
        assert_eq!(
            simplified.input_schema["properties"]["mode"]["enum"],
            json!(["safe"])
        );
        assert_eq!(simplified.input_schema["additionalProperties"], false);
    }
}
