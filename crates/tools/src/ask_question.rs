//! AskQuestion 工具 — 向用户发起多题访谈并等待其逐题作答

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use wyj_api::types::ToolDefinition;
use wyj_core::tool::{
    AskQuestionOption, AskQuestionSpec, QuestionAnswer, Tool, ToolContext, ToolResult,
};

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
            for key in &[
                "label",
                "text",
                "value",
                "option",
                "name",
                "content",
                "title",
                "description",
            ] {
                if let Some(Value::String(s)) = m.get(*key) {
                    return s.clone();
                }
            }
            v.to_string()
        }
        _ => v.to_string(),
    }
}

/// 解析单题的 options 数组：兼容元素是纯字符串（无 description）或 {label, description} 对象
fn parse_option_objects(raw: &Value) -> Option<Vec<AskQuestionOption>> {
    let arr = raw.as_array()?;
    let opts: Vec<AskQuestionOption> = arr
        .iter()
        .map(|v| {
            let label = coerce_string(v);
            let description = v
                .as_object()
                .and_then(|m| m.get("description"))
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            AskQuestionOption { label, description }
        })
        .filter(|o| !o.label.is_empty())
        .collect();
    if opts.len() >= 2 {
        Some(opts)
    } else {
        None
    }
}

/// 解析 questions 数组为 AskQuestionSpec 列表
fn parse_questions(raw: &Value) -> Option<Vec<AskQuestionSpec>> {
    let arr = raw.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut specs = Vec::with_capacity(arr.len());
    for item in arr {
        let question = item.get("question").map(coerce_string).unwrap_or_default();
        if question.is_empty() {
            return None;
        }
        let options = item.get("options").and_then(parse_option_objects)?;
        let header = item
            .get("header")
            .and_then(|h| h.as_str())
            .map(|s| s.to_string());
        let multi_select = item
            .get("multiSelect")
            .and_then(|m| m.as_bool())
            .unwrap_or(false);
        specs.push(AskQuestionSpec {
            question,
            header,
            multi_select,
            options,
        });
    }
    Some(specs)
}

/// 把若干个选项下标格式化为用顿号分隔的 label 列表
fn labels_for(spec: &AskQuestionSpec, indices: &[usize]) -> String {
    indices
        .iter()
        .filter_map(|&i| spec.options.get(i))
        .map(|o| o.label.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 把访谈结果序列化成回传给 LLM 的固定协议文本（不走 i18n）
fn render_answers(questions: &[AskQuestionSpec], answers: &[QuestionAnswer]) -> String {
    let mut out = String::from("访谈已完成，用户作答如下：\n\n");
    for (i, (spec, answer)) in questions.iter().zip(answers.iter()).enumerate() {
        let n = i + 1;
        match &spec.header {
            Some(h) if !h.is_empty() => out.push_str(&format!("Q{n}（{h}）: {}\n", spec.question)),
            _ => out.push_str(&format!("Q{n}: {}\n", spec.question)),
        }
        match answer {
            QuestionAnswer::Selected(indices) => {
                out.push_str(&format!("→ 用户选择: {}\n", labels_for(spec, indices)));
            }
            QuestionAnswer::FreeText(text) => {
                out.push_str(&format!("→ 用户输入（其他）: {text}\n"));
            }
            QuestionAnswer::SelectedWithFreeText(indices, text) => {
                out.push_str(&format!(
                    "→ 用户选择: {}；补充说明（其他）: {text}\n",
                    labels_for(spec, indices)
                ));
            }
        }
        if n != questions.len() {
            out.push('\n');
        }
    }
    out
}

#[async_trait]
impl Tool for AskQuestionTool {
    fn name(&self) -> &str {
        "AskQuestion"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "向用户发起一次结构化访谈，一次可提出 1-4 个问题，每题提供 2-4 个选项供用户选择\
                          （可单选或多选）。适用于需要收集用户明确偏好/决策的场景（如技术选型、范围确认、\
                          优先级排序），不要用于用户能自行推断或你能自主决策的情况。\
                          每题会在界面上自动追加一个「其他」选项，用户可选择后自由输入文本作答，\
                          不需要你自己在 options 里加「其他」。\
                          用户逐题作答后会看到一个总览确认页，确认后才会把所有题目的问答结果一并返回给你；\
                          若用户中途取消，会返回 [已取消]。\
                          返回内容是所有题目的问答对文本，多选题的多个答案以顿号分隔，自由文本答案会标注来源为「其他」。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "1-4 个访谈问题，会依次向用户展示",
                        "minItems": 1,
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "向用户展示的完整问题文本"
                                },
                                "header": {
                                    "type": "string",
                                    "description": "简短分类标签（建议不超过 12 个字符，如“技术栈”“范围”）"
                                },
                                "multiSelect": {
                                    "type": "boolean",
                                    "description": "是否允许多选，默认 false（单选）"
                                },
                                "options": {
                                    "type": "array",
                                    "description": "2-4 个选项（不含系统自动追加的“其他”选项）",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string", "description": "选项文字" },
                                            "description": { "type": "string", "description": "可选，选项的补充说明" }
                                        },
                                        "required": ["label"]
                                    }
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }

    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult> {
        let questions: Vec<AskQuestionSpec> = match input.get("questions").and_then(parse_questions)
        {
            Some(qs) if !qs.is_empty() => qs,
            _ => return Ok(ToolResult::err("缺少 questions 字段或无法解析问题列表")),
        };

        match ctx.ask_questions(&questions).await {
            Some(answers) if answers.len() == questions.len() => {
                Ok(ToolResult::ok(render_answers(&questions, &answers)))
            }
            _ => Ok(ToolResult::ok(
                "[已取消] 用户取消了本次访谈，未提供任何回答。",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::ToolCtx;
    use serde_json::json;

    fn make_ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[test]
    fn parse_questions_happy_path() {
        let raw = json!([
            {
                "question": "选哪种数据库？",
                "header": "技术栈",
                "options": [
                    {"label": "PostgreSQL", "description": "成熟稳定"},
                    {"label": "MySQL"}
                ]
            },
            {
                "question": "需要哪些能力？",
                "multiSelect": true,
                "options": [
                    {"label": "缓存"},
                    {"label": "队列"},
                    {"label": "搜索"}
                ]
            }
        ]);
        let specs = parse_questions(&raw).expect("应能解析");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].header.as_deref(), Some("技术栈"));
        assert!(!specs[0].multi_select);
        assert_eq!(specs[0].options.len(), 2);
        assert_eq!(specs[0].options[0].description.as_deref(), Some("成熟稳定"));
        assert!(specs[1].multi_select);
        assert_eq!(specs[1].options.len(), 3);
    }

    #[test]
    fn parse_option_objects_coerces_plain_strings() {
        let raw = json!(["A", "B", "C"]);
        let opts = parse_option_objects(&raw).expect("应能解析");
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "A");
        assert_eq!(opts[0].description, None);
    }

    #[test]
    fn parse_option_objects_rejects_single_option() {
        let raw = json!([{"label": "只有一个"}]);
        assert!(parse_option_objects(&raw).is_none());

        let questions_raw = json!([
            {"question": "只有一个选项的题", "options": [{"label": "唯一"}]}
        ]);
        assert!(parse_questions(&questions_raw).is_none());
    }

    #[test]
    fn render_answers_formats_multi_select_and_freetext() {
        let questions = vec![
            AskQuestionSpec {
                question: "支持哪些能力？".to_string(),
                header: Some("能力".to_string()),
                multi_select: true,
                options: vec![
                    AskQuestionOption {
                        label: "缓存".to_string(),
                        description: None,
                    },
                    AskQuestionOption {
                        label: "队列".to_string(),
                        description: None,
                    },
                ],
            },
            AskQuestionSpec {
                question: "还有什么补充？".to_string(),
                header: None,
                multi_select: false,
                options: vec![
                    AskQuestionOption {
                        label: "无".to_string(),
                        description: None,
                    },
                    AskQuestionOption {
                        label: "有".to_string(),
                        description: None,
                    },
                ],
            },
        ];
        let answers = vec![
            QuestionAnswer::Selected(vec![0, 1]),
            QuestionAnswer::FreeText("自定义文本".to_string()),
        ];
        let text = render_answers(&questions, &answers);
        assert!(text.contains("Q1（能力）: 支持哪些能力？"));
        assert!(text.contains("→ 用户选择: 缓存、队列"));
        assert!(text.contains("Q2: 还有什么补充？"));
        assert!(text.contains("→ 用户输入（其他）: 自定义文本"));
    }

    #[tokio::test]
    async fn run_returns_cancelled_text_when_ctx_declines() {
        let tool = AskQuestionTool::new();
        let ctx = make_ctx();
        let input = json!({
            "questions": [
                {"question": "测试问题？", "options": [{"label": "是"}, {"label": "否"}]}
            ]
        });
        let result = tool.run(input, &ctx).await.unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("[已取消]"));
    }
}
