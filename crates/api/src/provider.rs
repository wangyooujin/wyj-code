//! Provider trait — 双格式供应商抽象。

use crate::types::{CompletionResult, Message, StreamEvent, ToolDefinition};
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send + 'static>>;

/// LLM 供应商抽象 — 所有供应商实现此 trait
#[async_trait]
pub trait Provider: Send + Sync {
    /// 发起流式推理，返回 SSE 事件流
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<EventStream>;

    /// 发起非流式推理，等待完整结果（默认由 stream 实现，可覆盖以提升性能）
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<CompletionResult> {
        use crate::types::{ContentBlock, StopReason};
        use futures::StreamExt;

        let mut stream = self.stream(system, messages, tools, max_tokens).await?;

        let mut text_buf = String::new();
        let mut tool_bufs: Vec<(String, String, String)> = vec![]; // (id, name, json)
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens = 0u32;
        let mut output_tokens = 0u32;

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
                StreamEvent::MessageStop { stop_reason: sr } => stop_reason = sr,
                StreamEvent::Usage {
                    input_tokens: i,
                    output_tokens: o,
                } => {
                    input_tokens = i;
                    output_tokens = o;
                }
            }
        }

        let mut content = vec![];
        if !text_buf.is_empty() {
            content.push(ContentBlock::Text { text: text_buf });
        }
        for (id, name, json) in tool_bufs {
            let input: serde_json::Value =
                serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
            content.push(ContentBlock::ToolUse { id, name, input });
        }

        Ok(CompletionResult {
            content,
            stop_reason,
            input_tokens,
            output_tokens,
        })
    }
}
