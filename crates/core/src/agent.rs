//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::session::Session;
use crate::tool::{Tool, ToolContext};
use anyhow::Result;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, StopReason, StreamEvent, ToolDefinition},
};

pub struct Agent {
    provider: Box<dyn Provider>,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    tool_impls: HashMap<String, Arc<dyn Tool>>,
    max_tokens: u32,
    max_turns: usize,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>) -> Self {
        Self {
            provider,
            system_prompt: default_system_prompt(),
            tools: vec![],
            tool_impls: HashMap::new(),
            max_tokens: 8192,
            max_turns: 20,
        }
    }

    pub fn with_system(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// 注册工具（同时更新定义列表和实现映射）
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        let def = tool.definition();
        self.tools.retain(|d| d.name != def.name);
        self.tools.push(def);
        self.tool_impls.insert(tool.name().to_string(), tool);
    }

    /// 批量注册工具
    pub fn with_tool_impls(
        mut self,
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> Self {
        for t in tools {
            self.register_tool(t);
        }
        self
    }

    /// 执行一轮用户消息，流式回调文本，处理工具调用循环。
    pub async fn run_turn(
        &self,
        session: &mut Session,
        ctx: &dyn ToolContext,
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
                .stream(&self.system_prompt, &session.messages, &self.tools, self.max_tokens)
                .await?;

            let mut text_buf = String::new();
            let mut pending_tools: Vec<(String, String, String)> = vec![]; // (id, name, json)
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
                        let idx = if id.is_empty() {
                            current_tool_idx
                        } else {
                            pending_tools.iter().position(|(tid, _, _)| *tid == id)
                        };
                        if let Some(i) = idx {
                            pending_tools[i].2.push_str(&json_delta);
                        }
                    }
                    StreamEvent::ToolUseEnd { .. } => {}
                    StreamEvent::MessageStop { stop_reason: sr } => stop_reason = sr,
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
                let input = serde_json::from_str(json)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                });
            }
            session.push_assistant(assistant_blocks);

            if stop_reason != StopReason::ToolUse || pending_tools.is_empty() {
                break;
            }

            // 并发执行所有工具
            let mut handles = vec![];
            for (id, name, json) in pending_tools {
                let input = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                let tool = self.tool_impls.get(&name).cloned();
                // ctx 不是 Send，所以只能顺序执行
                let result: (String, bool) = if let Some(t) = tool {
                    match t.run(input, ctx).await {
                        Ok(r) => (r.content, r.is_error),
                        Err(e) => (format!("工具执行错误: {e}"), true),
                    }
                } else {
                    (format!("工具 `{name}` 未注册"), true)
                };
                handles.push((id, result));
            }

            for (id, (output, is_error)) in handles {
                session.push_tool_result(id, output, is_error);
            }
        }
        Ok(())
    }
}

fn default_system_prompt() -> String {
    "你是一个专业的 AI 编程助手，擅长分析代码、解决技术问题、编写程序。\n\
    当需要操作文件系统、执行命令或搜索代码时，你会主动使用工具完成任务。\n\
    工具使用原则：Read 后才能 Write；Edit 前必须精确匹配。\n\
    使用简洁、准确的中文与用户沟通，优先给出可运行的代码和具体的操作步骤。"
        .to_string()
}
