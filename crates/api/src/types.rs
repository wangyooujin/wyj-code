//! 中立内部模型 — 与具体供应商 API 格式无关的会话类型。

use serde::{Deserialize, Serialize};

/// 消息角色
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 单条消息（含一个或多个内容块）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 将消息里所有文本块拼接为纯文本
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// 消息内容块
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 普通文本
    Text { text: String },
    /// 工具调用请求（模型 → 客户端）
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// 工具执行结果（客户端 → 模型）
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        is_error: bool,
    },
    /// base64 图片
    Image { media_type: String, data: String },
    /// Extended thinking 块（assistant 消息内）。工具调用续轮时必须携带
    /// signature 原样回传给 API，否则请求会被拒绝。
    Thinking { thinking: String, signature: String },
    /// 被安全系统加密的思考块，同样必须原样回传
    RedactedThinking { data: String },
}

/// 工具结果内容。`Parts` 为结构化多块内容（文本+图片，Anthropic 原生支持
/// tool_result 内嵌 image block）；`Blocks` 为旧版原始 JSON 透传，仅保留
/// 以兼容历史会话文件的反序列化（untagged 匹配顺序：Parts 有 type tag
/// 结构可与 Blocks 区分，必须放在 Blocks 之前）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Parts(Vec<ToolResultPart>),
    Blocks(Vec<serde_json::Value>),
}

/// 工具结果的单个内容块
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultPart {
    Text {
        text: String,
    },
    /// base64 图片（media_type 如 "image/png"）
    Image {
        media_type: String,
        data: String,
    },
}

impl ToolResultContent {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    /// 展示用文本：Parts 中的图片以占位符表示
    pub fn display_text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    ToolResultPart::Text { text } => text.clone(),
                    ToolResultPart::Image { media_type, .. } => format!("[image: {media_type}]"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Blocks(b) => serde_json::to_string(b).unwrap_or_default(),
        }
    }
}

/// 工具定义（提交给模型的 schema）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 停止原因
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    #[serde(other)]
    Other,
}

/// 流式事件（从供应商 SSE 解析后转换为此格式）
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 文本增量
    TextDelta(String),
    /// 工具调用开始（id + name）
    ToolUseStart { id: String, name: String },
    /// 工具调用参数增量（JSON 片段）
    ToolUseDelta { id: String, json_delta: String },
    /// 工具调用结束
    ToolUseEnd { id: String },
    /// thinking 块开始（extended thinking 开启时）
    ThinkingStart,
    /// thinking 文本增量
    ThinkingDelta(String),
    /// thinking 块签名增量（块结束前到达，需累积并随块回传历史）
    ThinkingSignatureDelta(String),
    /// 加密思考块（整块到达，原样保存回传）
    RedactedThinking(String),
    /// 消息结束
    MessageStop { stop_reason: StopReason },
    /// 用量统计（可选）。`cache_read_input_tokens` 为命中 prompt 缓存
    /// 的输入 token 数（按约 0.1x 价格计费）；`cache_creation_input_tokens`
    /// 为写入缓存的输入 token 数（按约 1.25x 价格计费）。未启用缓存时均为 0。
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_read_input_tokens: u32,
        cache_creation_input_tokens: u32,
    },
}

/// 一次完整推理的汇总结果（流结束后组装）
#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_parts_roundtrip() {
        let content = ToolResultContent::Parts(vec![
            ToolResultPart::Text {
                text: "看图".into(),
            },
            ToolResultPart::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
        ]);
        let json = serde_json::to_string(&content).unwrap();
        // 结构必须带 type tag，与 Anthropic tool_result content 数组一致
        assert!(json.contains(r#""type":"text""#));
        assert!(json.contains(r#""type":"image""#));
        let back: ToolResultContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolResultContent::Parts(ref p) if p.len() == 2));
    }

    #[test]
    fn tool_result_legacy_formats_still_deserialize() {
        // 旧版纯文本
        let back: ToolResultContent = serde_json::from_str(r#""plain text""#).unwrap();
        assert!(matches!(back, ToolResultContent::Text(_)));
        // 旧版 Blocks（无 type tag 的任意 JSON 数组），不能被 Parts 误吞
        let back: ToolResultContent =
            serde_json::from_str(r#"[{"foo": 1}, {"bar": "x"}]"#).unwrap();
        assert!(matches!(back, ToolResultContent::Blocks(_)));
    }

    #[test]
    fn parts_display_text_uses_placeholder_for_images() {
        let content = ToolResultContent::Parts(vec![ToolResultPart::Image {
            media_type: "image/png".into(),
            data: "x".repeat(100_000),
        }]);
        let d = content.display_text();
        assert!(d.contains("image/png"));
        assert!(d.len() < 100);
    }
}
