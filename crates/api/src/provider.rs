//! Provider trait — 双格式供应商抽象。

use crate::types::{CompletionResult, Message, StreamEvent, ToolDefinition};
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send + 'static>>;

/// 单次推理请求的选项（收敛为结构体，后续扩展不再破坏 trait 签名）
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub max_tokens: u32,
    /// Extended thinking 预算 token 数；None/0 = 关闭
    pub thinking_budget: Option<u32>,
    /// 工具调用轮之间允许交错思考（仅 thinking 开启时生效）
    pub interleaved: bool,
}

impl RequestOptions {
    /// 纯文本请求（无 thinking），供 compact/memory/summary 等辅助调用使用
    pub fn text_only(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            thinking_budget: None,
            interleaved: false,
        }
    }

    pub fn from_request_plan(plan: &crate::request_plan::RequestPlan) -> Self {
        let thinking_budget = match plan.reasoning {
            crate::request_plan::ReasoningRequest::BudgetTokens(budget) => Some(budget),
            _ => None,
        };
        Self {
            max_tokens: plan.max_tokens,
            thinking_budget,
            interleaved: thinking_budget.is_some(),
        }
    }
}

/// LLM 供应商抽象 — 所有供应商实现此 trait
#[async_trait]
pub trait Provider: Send + Sync {
    /// 发起流式推理，返回 SSE 事件流
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        opts: &RequestOptions,
    ) -> Result<EventStream>;

    /// 发起非流式推理，等待完整结果（默认由 stream 实现，可覆盖以提升性能）
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        opts: &RequestOptions,
    ) -> Result<CompletionResult> {
        use crate::types::{ContentBlock, StopReason};
        use futures::StreamExt;

        let mut stream = self.stream(system, messages, tools, opts).await?;

        let mut text_buf = String::new();
        let mut tool_bufs: Vec<(String, String, String)> = vec![]; // (id, name, json)
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;
        let mut cache_read_input_tokens = 0u32;
        let mut cache_creation_input_tokens = 0u32;

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(delta) => text_buf.push_str(&delta),
                StreamEvent::ToolUseStart { id, name } => {
                    tool_bufs.push((id, name, String::new()));
                }
                StreamEvent::ToolUseDelta { id, json_delta } => {
                    if let Some(buf) = tool_bufs.iter_mut().find(|(bid, _, _)| *bid == id) {
                        buf.2.push_str(&json_delta);
                    }
                }
                StreamEvent::ToolUseEnd { .. } => {}
                // 非流式辅助调用（compact/memory/summary）不开 thinking，忽略
                StreamEvent::ThinkingStart
                | StreamEvent::ThinkingDelta(_)
                | StreamEvent::ThinkingSignatureDelta(_)
                | StreamEvent::RedactedThinking(_) => {}
                StreamEvent::MessageStop { stop_reason: sr } => stop_reason = sr,
                StreamEvent::Usage {
                    input_tokens: i,
                    output_tokens: o,
                    cache_read_input_tokens: c,
                    cache_creation_input_tokens: cw,
                } => {
                    input_tokens = i;
                    output_tokens = o;
                    cache_read_input_tokens = c;
                    cache_creation_input_tokens = cw;
                }
            }
        }

        let mut content = vec![];
        if !text_buf.is_empty() {
            content.push(ContentBlock::Text { text: text_buf });
        }
        for (id, name, json) in tool_bufs {
            let input: serde_json::Value = serde_json::from_str(&json).map_err(|error| {
                anyhow::anyhow!(
                    "provider returned malformed arguments for tool `{}`: {}",
                    name,
                    error
                )
            })?;
            if !input.is_object() {
                anyhow::bail!("provider returned non-object arguments for tool `{}`", name);
            }
            content.push(ContentBlock::ToolUse { id, name, input });
        }

        Ok(CompletionResult {
            content,
            stop_reason,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        })
    }
}
