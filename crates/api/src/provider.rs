//! Provider trait — 双格式供应商抽象。

use crate::types::{CompletionResult, Message, StreamEvent, ToolDefinition};
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send + 'static>>;

/// 一次请求的 system prompt，按「会话内稳定前缀」与「每轮变化尾部」两段传入。
///
/// Anthropic 的 prompt cache 按**前缀**匹配：`cache_control` 断点打在哪个块
/// 末尾，该块及其之前的内容才被缓存。之前整个 system 被压成**单个** text 块、
/// 断点打在块尾，于是任何一处尾部改动都会让整段 system（约 1.6k~5k token）
/// 全价重算：
///   * `<current-tool-availability>` 随工具懒加载命中/过期而变；
///   * 模型兼容 suffix 随 route 能力而变；
///   * Project Brief 每次按「最近 4 条 user 消息」重算相关性排序；
///   * 子目录 CLAUDE.md reminder 被 `push_str` 追加到 system 末尾——旧注释
///     里「只增不减，前缀仍可缓存」的说法是错的，追加在断点**之后**同样会
///     改变断点处的前缀哈希。
///
/// 拆成两段后，`stable` 承载进缓存的内容（主提示 / `<env>` / 模式段 /
/// 记忆快照 / CLAUDE.md 祖先链），断点只打在它末尾；`volatile` 承载每轮
/// 变化的内容，不打断点，也不影响 `stable` 的缓存命中。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemPrompt<'a> {
    /// 会话内稳定前缀，命中 prompt cache。
    pub stable: &'a str,
    /// 每轮变化部分，不参与缓存。
    pub volatile: &'a str,
}

impl<'a> SystemPrompt<'a> {
    /// 单一稳定 system，无 volatile 尾部。摘要生成、记忆提取、标题生成等
    /// 辅助 LLM 调用都属于这一类。
    pub fn stable_only(system: &'a str) -> Self {
        Self {
            stable: system,
            volatile: "",
        }
    }

    /// 两段拼接后的完整文本。供 token 估算与 OpenAI 协议使用
    /// （Chat Completions 没有 system 块概念，只有一条 system 消息）。
    pub fn combined(&self) -> String {
        match (self.stable.is_empty(), self.volatile.is_empty()) {
            (true, true) => String::new(),
            (true, false) => self.volatile.to_string(),
            (false, true) => self.stable.to_string(),
            (false, false) => format!("{}\n\n{}", self.stable, self.volatile),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stable.is_empty() && self.volatile.is_empty()
    }
}

/// 单次推理请求的选项（收敛为结构体，后续扩展不再破坏 trait 签名）
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub max_tokens: u32,
    /// Extended thinking 预算 token 数；None/0 = 关闭
    /// Anthropic 原生 / Qwen / Doubao 等支持 budget_tokens 的 vendor 走这里
    pub thinking_budget: Option<u32>,
    /// OpenAI-vendor `reasoning_effort` 档位（low/medium/high/max/xhigh/auto），
    /// DeepSeek/Qwen 互斥分支/Moonshot k3 等走这里
    pub reasoning_effort: Option<String>,
    /// OpenAI-vendor `thinking.type` 字符串（enabled/disabled/auto/adaptive）。
    /// 由 profile.thinking_switch 直接透传，agent 层负责拼装；adapter 内部决定
    /// 是否真的写到 body
    pub thinking_switch: Option<String>,
    /// 工具调用轮之间允许交错思考（仅 Anthropic 协议 + thinking_budget 开启时生效）
    pub interleaved: bool,
}

impl RequestOptions {
    /// 纯文本请求（无 thinking），供 compact/memory/summary 等辅助调用使用
    pub fn text_only(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            thinking_budget: None,
            reasoning_effort: None,
            thinking_switch: None,
            interleaved: false,
        }
    }

    pub fn from_request_plan(plan: &crate::request_plan::RequestPlan) -> Self {
        let (thinking_budget, reasoning_effort) = match &plan.reasoning {
            crate::request_plan::ReasoningRequest::BudgetTokens(budget) => (Some(*budget), None),
            crate::request_plan::ReasoningRequest::Effort(effort) => (None, Some(effort.clone())),
            crate::request_plan::ReasoningRequest::Disabled
            | crate::request_plan::ReasoningRequest::ProviderNative => (None, None),
        };
        let active = thinking_budget.is_some() || reasoning_effort.is_some();
        Self {
            max_tokens: plan.max_tokens,
            thinking_budget,
            reasoning_effort,
            thinking_switch: None,
            interleaved: active,
        }
    }
}

/// LLM 供应商抽象 — 所有供应商实现此 trait
#[async_trait]
pub trait Provider: Send + Sync {
    /// 发起流式推理，返回 SSE 事件流
    async fn stream(
        &self,
        system: &SystemPrompt<'_>,
        messages: &[Message],
        tools: &[ToolDefinition],
        opts: &RequestOptions,
    ) -> Result<EventStream>;

    /// 发起非流式推理，等待完整结果（默认由 stream 实现，可覆盖以提升性能）
    async fn complete(
        &self,
        system: &SystemPrompt<'_>,
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
                | StreamEvent::ThinkingDetailsDelta(_)
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
