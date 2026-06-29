//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::session::Session;
use anyhow::Result;
use futures::StreamExt;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, StopReason, StreamEvent, ToolDefinition},
};

/// 工具执行回调类型（M1 阶段：无真实工具，仅预留接口）
pub type ToolExecutor = Box<
    dyn Fn(String, String, serde_json::Value) -> futures::future::BoxFuture<'static, Result<String>>
        + Send
        + Sync,
>;

pub struct Agent {
    provider: Box<dyn Provider>,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    max_tokens: u32,
    /// 单次推理最多循环轮数（防止无限循环）
    max_turns: usize,
    tool_executor: Option<ToolExecutor>,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>) -> Self {
        Self {
            provider,
            system_prompt: default_system_prompt(),
            tools: vec![],
            max_tokens: 8192,
            max_turns: 20,
            tool_executor: None,
        }
    }

    pub fn with_system(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn with_tool_executor(mut self, exec: ToolExecutor) -> Self {
        self.tool_executor = Some(exec);
        self
    }

    /// 执行一轮用户消息，流式输出文本到 stdout，处理工具调用循环。
    /// 返回本轮最终会话状态（含助手回复）。
    pub async fn run_turn(
        &self,
        session: &mut Session,
        on_text: &mut impl FnMut(&str),
    ) -> Result<()> {
        let mut turn = 0;
        loop {
            turn += 1;
            if turn > self.max_turns {
                anyhow::bail!("超过最大推理轮数 {}", self.max_turns);
            }

            let mut stream = self
                .provider
                .stream(
                    &self.system_prompt,
                    &session.messages,
                    &self.tools,
                    self.max_tokens,
                )
                .await?;

            // 流式收集本轮输出
            let mut text_buf = String::new();
            // (id, name, json_buf)
            let mut pending_tools: Vec<(String, String, String)> = vec![];
            let mut current_tool_idx: Option<usize> = None;
            let mut stop_reason = StopReason::EndTurn;

            while let Some(event) = stream.next().await {
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        on_text(&delta);
                        text_buf.push_str(&delta);
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        let idx = pending_tools.len();
                        pending_tools.push((id, name, String::new()));
                        current_tool_idx = Some(idx);
                    }
                    StreamEvent::ToolUseDelta { id, json_delta } => {
                        // Anthropic 的 delta 不带 id，用 current_tool_idx 关联
                        // OpenAI 的 delta 带 id，精确匹配
                        let idx = if id.is_empty() {
                            current_tool_idx
                        } else {
                            pending_tools.iter().position(|(tid, _, _)| *tid == id)
                        };
                        if let Some(i) = idx {
                            pending_tools[i].2.push_str(&json_delta);
                        }
                    }
                    StreamEvent::ToolUseEnd { id } => {
                        // 可选：记录结束
                        let _ = id;
                    }
                    StreamEvent::MessageStop { stop_reason: sr } => {
                        stop_reason = sr;
                    }
                    StreamEvent::Usage { input_tokens, output_tokens } => {
                        session.add_usage(input_tokens, output_tokens);
                    }
                }
            }

            // 组装助手内容块
            let mut assistant_blocks = vec![];
            if !text_buf.is_empty() {
                assistant_blocks.push(ContentBlock::Text { text: text_buf });
            }
            for (id, name, json) in &pending_tools {
                let input: serde_json::Value =
                    serde_json::from_str(json).unwrap_or(serde_json::Value::Object(Default::default()));
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                });
            }
            session.push_assistant(assistant_blocks);

            // 若不需要调用工具，结束循环
            if stop_reason != StopReason::ToolUse || pending_tools.is_empty() {
                break;
            }

            // 执行工具并追加结果
            if pending_tools.is_empty() {
                break;
            }
            for (id, name, json) in &pending_tools {
                let input: serde_json::Value =
                    serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
                let result = if let Some(exec) = &self.tool_executor {
                    match exec(id.clone(), name.clone(), input).await {
                        Ok(out) => (out, false),
                        Err(e) => (format!("工具执行失败: {e}"), true),
                    }
                } else {
                    (format!("工具 `{name}` 未注册"), true)
                };
                session.push_tool_result(id.clone(), result.0, result.1);
            }
            // 继续下一轮（携带工具结果给模型）
        }
        Ok(())
    }
}

fn default_system_prompt() -> String {
    "你是一个智能编程助手。你会帮助用户分析代码、解决问题、编写程序。\
    在需要执行操作时，你会使用提供的工具完成任务，并及时向用户汇报结果。\
    请使用简洁、准确的语言与用户沟通，优先输出可操作的内容。"
        .to_string()
}
