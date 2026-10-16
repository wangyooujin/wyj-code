//! 工具参数的 fail-closed 解析、有限修复与 JSON Schema 校验。

use std::{collections::HashMap, sync::Arc};

use jsonschema::{
    error::{TypeKind, ValidationErrorKind},
    paths::{JSONPointer, PathChunk},
    primitive_type::PrimitiveType,
    Draft, JSONSchema,
};
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
    /// 模型**实际发出**的参数（已能解析成 JSON object 时才为 `Some`）。
    ///
    /// 不参与 `feedback_json` 序列化——它是给调用方写回历史用的，不是给模型
    /// 看的内容。存在的理由：模型要自我纠正就必须看到自己传过什么，只回一句
    /// `$/tree_id expected integer` 等于让它闭着眼重试同一个参数。
    ///
    /// 装箱是为了压住 `ToolArgumentError` 的体积：`process` 的 `Err` 变体一旦
    /// 超过 clippy 的 `result_large_err` 阈值，每个成功调用也要按错误路径搬运
    /// 这么大的栈对象。它只在失败路径上被读一次，不值得为此让热路径变慢。
    #[serde(skip)]
    pub original_input: Option<Box<Value>>,
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
            original_input: None,
        }
    }

    /// 附上模型实际发出的参数，供调用方写回历史以便模型对照纠错。
    fn with_original_input(mut self, input: &Value) -> Self {
        if input.is_object() {
            self.original_input = Some(Box::new(input.clone()));
        }
        self
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
            // 先做一次**保守的类型修复**再判定失败：模型把整数当字符串发出来是
            // 可恢复的传输问题，不是语义错误，直接判死会让模型在同一个坑里
            // 空转到触发重试上限。修复只在「schema 明确要求 integer/number、
            // 实际是纯数字字符串、且转换无损」时发生，且修完必须重新通过
            // 完整校验才采纳。
            let errors: Vec<_> = errors.collect();
            let mut repaired_input = input.clone();
            let mut repaired_ok = false;
            let mut issues = None;
            if repair_numeric_string_types(&mut repaired_input, &errors) {
                let outcome = schema.validate(&repaired_input);
                if outcome.is_ok() {
                    repaired_ok = true;
                } else if let Err(rest) = outcome {
                    issues = Some(
                        rest.take(16)
                            .map(|error| validation_issue(&error))
                            .collect(),
                    );
                }
            }
            if repaired_ok {
                return Ok(ValidatedToolCall {
                    id: call.id,
                    name: call.name,
                    input: repaired_input,
                    syntax_repaired: true,
                });
            }
            let issues = issues.unwrap_or_else(|| {
                errors
                    .into_iter()
                    .take(16)
                    .map(|error| validation_issue(&error))
                    .collect()
            });
            return Err(ToolArgumentError::new(
                call.name,
                ToolArgumentErrorKind::SchemaViolation,
                issues,
            )
            .with_original_input(&input));
        }

        Ok(ValidatedToolCall {
            id: call.id,
            name: call.name,
            input,
            syntax_repaired,
        })
    }
}

/// 把「schema 要求整数/数字、实际给了纯数字字符串」的字段就地转成数字。
///
/// 为什么需要（一次真实故障）：`app_computer` 的 `tree_id` 是 FNV-1a u64，
/// 量级 ~1.5e19，远超 JSON 安全整数 2^53。模型看到这种 19 位数字时序列化成
/// 字符串是常见行为，于是 `type: integer` 校验每轮都失败；而模型的历史里只
/// 留着校验错误、没有自己传过的值，无从自我纠正，三轮后被当成无界重试中止，
/// 整轮工作作废。这是**传输层的类型抖动**，不是语义错误，框架应当吸收。
///
/// 三条保守边界，缺一不可：
/// 1. 只改 schema 报错指向的那一个路径，不做全局类型推断；
/// 2. 只接受能**无损**解析成 `i64`/`u64` 的字符串——带小数点、指数、空格、
///    前导零的非规范写法一律不动（模型该收到的是明确报错而不是被悄悄改写）；
/// 3. 调用方必须重新跑一遍完整 schema 校验才采纳结果（见 [`process`]），所以
///    即使误转也会被完整校验拦下并如实报错。
///
/// 返回是否真的改动了任何字段。
fn repair_numeric_string_types(
    input: &mut Value,
    errors: &[jsonschema::ValidationError<'_>],
) -> bool {
    let mut repaired = false;
    for error in errors {
        if !matches!(error.kind, ValidationErrorKind::Type { .. }) {
            continue;
        }
        if !type_error_expects_number(&error.kind) {
            continue;
        }
        let Some(slot) = value_at_path_mut(input, &error.instance_path) else {
            continue;
        };
        let Value::String(raw) = slot else {
            continue;
        };
        let Some(number) = parse_canonical_number(raw) else {
            continue;
        };
        *slot = number;
        repaired = true;
    }
    repaired
}

/// 这条 `Type` 错误是否期待 integer / number。
fn type_error_expects_number(kind: &ValidationErrorKind) -> bool {
    let ValidationErrorKind::Type { kind } = kind else {
        return false;
    };
    let expects = |primitive: PrimitiveType| {
        matches!(primitive, PrimitiveType::Integer | PrimitiveType::Number)
    };
    match kind {
        TypeKind::Single(primitive) => expects(*primitive),
        TypeKind::Multiple(types) => types.into_iter().any(expects),
    }
}

/// 按 JSON Pointer 定位到 `input` 中对应的值。
fn value_at_path_mut<'a>(input: &'a mut Value, path: &JSONPointer) -> Option<&'a mut Value> {
    let mut cursor = input;
    for chunk in path.iter() {
        cursor = match (cursor, chunk) {
            (Value::Object(map), PathChunk::Property(key)) => map.get_mut(key.as_ref())?,
            (Value::Array(items), PathChunk::Index(index)) => items.get_mut(*index)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// 只接受规范写法的十进制整数字符串，解析失败一律返回 `None`（保持原样）。
///
/// 排除 `"007"` / `" 7"` / `"7.0"` / `"7e3"`：这些要么有前导零歧义、要么在
/// i64/u64 与 f64 之间会引入精度不确定性，把它们「修」成数字等于替模型猜。
fn parse_canonical_number(raw: &str) -> Option<Value> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    // 先 i64 再 u64：u64 覆盖 tree_id / generation 这类 > i64::MAX 的哈希。
    if let Ok(value) = raw.parse::<i64>() {
        return Some(Value::from(value));
    }
    raw.parse::<u64>().ok().map(Value::from)
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

    /// 一次真实故障的回归：模型把 19 位 `tree_id`（FNV-1a u64，远超 JSON 安全
    /// 整数 2^53）当字符串发出，`type: integer` 每轮都判失败，三轮后整轮中止。
    /// 类型抖动属于传输层问题，框架应当吸收而不是判死。
    #[test]
    fn a_numeric_string_for_an_integer_field_is_repaired() {
        let p = pipeline(json!({
            "type": "object",
            "required": ["tree_id"],
            "properties": {"tree_id": {"type": "integer", "minimum": 0}},
            "additionalProperties": false
        }));
        // 取自故障会话里的真实 tree_id：> i64::MAX，只能走 u64 分支。
        let tree_id = 15073053814190294464u64;
        let result = p
            .process(call(&format!(r#"{{"tree_id":"{tree_id}"}}"#)))
            .unwrap();
        assert!(result.syntax_repaired);
        assert_eq!(result.input["tree_id"].as_u64(), Some(tree_id));
    }

    /// 修复只覆盖「schema 明确要数字 + 实际是规范十进制整数串」。其余形态一律
    /// 保持原样并如实报错——把 `"007"`/`"7.0"` 悄悄改掉等于替模型猜值。
    #[test]
    fn non_canonical_or_out_of_range_numeric_strings_are_still_rejected() {
        let p = pipeline(json!({
            "type": "object",
            "properties": {
                "tree_id": {"type": "integer", "minimum": 0},
                "limit": {"type": "integer", "maximum": 100}
            },
            "additionalProperties": false
        }));
        for raw in [
            r#"{"tree_id":"007"}"#,          // 前导零
            r#"{"tree_id":" 7"}"#,           // 空白
            r#"{"tree_id":"7.0"}"#,          // 小数
            r#"{"tree_id":"7e3"}"#,          // 指数
            r#"{"tree_id":"not-a-number"}"#, // 非数字
            r#"{"tree_id":""}"#,             // 空串
            r#"{"limit":"500"}"#,            // 转成数字后仍越界
        ] {
            let error = p.process(call(raw)).unwrap_err();
            assert_eq!(
                error.kind,
                ToolArgumentErrorKind::SchemaViolation,
                "must still reject {raw}"
            );
        }
    }

    /// 真正的字符串字段绝不能被当成数字：schema 说 string 时保持原值，
    /// 否则 `"tree_id": "abc"` 会被改写成损坏的参数。
    #[test]
    fn string_typed_fields_are_never_coerced_to_numbers() {
        let p = pipeline(json!({
            "type": "object",
            "properties": {"ref": {"type": "string", "pattern": "^[a-z]+$"}},
            "additionalProperties": false
        }));
        let error = p.process(call(r#"{"ref":"123"}"#)).unwrap_err();
        assert_eq!(error.kind, ToolArgumentErrorKind::SchemaViolation);
    }

    /// 校验失败必须把**模型实际发过的参数**带出去，否则调用方只能写占位符，
    /// 模型在下一轮看不到自己传了什么，只能原样重发同一个错误。
    #[test]
    fn a_schema_violation_carries_the_arguments_the_model_actually_sent() {
        let p = pipeline(json!({
            "type": "object",
            "properties": {"mode": {"type": "string", "enum": ["safe"]}},
            "additionalProperties": false
        }));
        let error = p.process(call(r#"{"mode":"unsafe"}"#)).unwrap_err();
        assert_eq!(
            error.original_input.as_ref().unwrap()["mode"],
            json!("unsafe")
        );
        // 原始参数是给调用方写回历史用的，不该作为独立字段进反馈 JSON。
        let feedback: serde_json::Value = serde_json::from_str(&error.feedback_json()).unwrap();
        assert!(feedback.get("original_input").is_none());
    }

    /// 反过来：JSON 本身解析不出来时没有合法参数可回传，必须是 `None`，
    /// 让调用方退回占位符而不是把半截 JSON 写进历史。
    #[test]
    fn an_unparseable_call_has_no_original_input_to_echo() {
        let p = pipeline(json!({"type": "object"}));
        let error = p.process(call(r#"{"file_path":"src/lib"#)).unwrap_err();
        assert_eq!(error.kind, ToolArgumentErrorKind::InvalidJson);
        assert!(error.original_input.is_none());
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
