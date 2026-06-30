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
}

/// 工具结果内容（文本或结构化 JSON）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

impl ToolResultContent {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
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
    /// 消息结束
    MessageStop { stop_reason: StopReason },
    /// 用量统计（可选）
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
}

/// 一次完整推理的汇总结果（流结束后组装）
#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub input_tokens: u32,
    pub output_tokens: u32,
}
