//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::compact::{compact_session, estimate_tokens, COMPACT_TRIGGER_BUFFER};
use crate::memory::MemoryStore;
use crate::session::Session;
use crate::tool::{Tool, ToolContext};
use anyhow::Result;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use wyj_api::{
    provider::Provider,
    types::{ContentBlock, StopReason, StreamEvent, ToolDefinition},
};

/// 工具执行事件（供回调使用，例如 headless 格式化输出或 TUI 事件推送）
pub enum ToolEvent {
    Start {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    End {
        id: String,
        name: String,
        is_error: bool,
        elapsed_secs: f64,
        output: String,
    },
}

#[derive(Clone)]
pub struct Agent {
    provider: Arc<dyn Provider>,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    tool_impls: HashMap<String, Arc<dyn Tool>>,
    max_tokens: u32,
    max_turns: usize,
    /// 模型最大上下文窗口（token 数），用于触发自动压缩
    context_window: u32,
    /// 跨会话记忆存储（可选）
    memory: Option<Arc<MemoryStore>>,
    /// 可选的工具事件回调（Send + Sync，可跨线程）
    tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            system_prompt: default_system_prompt(),
            tools: vec![],
            tool_impls: HashMap::new(),
            max_tokens: 8192,
            max_turns: 20,
            context_window: 200_000,
            memory: None,
            tool_cb: None,
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

    pub fn with_context_window(mut self, n: u32) -> Self {
        self.context_window = n;
        self
    }

    /// 在默认系统提示末尾追加额外内容（如项目 WYJ.md 说明）
    pub fn append_system(mut self, extra: impl Into<String>) -> Self {
        let e = extra.into();
        if !e.is_empty() {
            self.system_prompt.push_str("\n\n");
            self.system_prompt.push_str(&e);
        }
        self
    }

    pub fn with_memory(mut self, mem: Arc<MemoryStore>) -> Self {
        self.memory = Some(mem);
        self
    }

    /// 注册工具事件回调（用于 headless 格式化输出或 TUI 事件推送）
    pub fn with_tool_callback(mut self, cb: impl Fn(ToolEvent) + Send + Sync + 'static) -> Self {
        self.tool_cb = Some(Arc::new(cb));
        self
    }

    /// 注册工具（同时更新定义列表和实现映射）
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        let def = tool.definition();
        self.tools.retain(|d| d.name != def.name);
        self.tools.push(def);
        self.tool_impls.insert(tool.name().to_string(), tool);
    }

    /// 追加单个工具（用于 per-turn 动态注册，如 ExitPlanMode）
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.register_tool(tool);
        self
    }

    /// 批量注册工具
    pub fn with_tool_impls(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
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
        // 将记忆上下文追加到系统提示末尾
        let system = if let Some(mem) = &self.memory {
            let ctx_str = mem.load_context();
            if ctx_str.is_empty() {
                self.system_prompt.clone()
            } else {
                format!("{}\n\n{}", self.system_prompt, ctx_str)
            }
        } else {
            self.system_prompt.clone()
        };

        let mut turn = 0;
        loop {
            turn += 1;
            if turn > self.max_turns {
                anyhow::bail!("超过最大推理轮数 {}", self.max_turns);
            }

            // 检查 token 预算，超限时触发自动压缩
            let estimated = estimate_tokens(&session.messages);
            let compact_threshold = self.context_window.saturating_sub(COMPACT_TRIGGER_BUFFER);
            if estimated > compact_threshold {
                match compact_session(session, self.provider.as_ref(), self.context_window).await {
                    Ok(r) => on_text(&format!(
                        "\n[已压缩对话历史：移除 {} 条消息，节省约 {} tokens]\n",
                        r.messages_removed, r.tokens_saved_estimate
                    )),
                    Err(e) => tracing::warn!("上下文压缩失败: {e}"),
                }
            }

            let mut stream = self
                .provider
                .stream(&system, &session.messages, &self.tools, self.max_tokens)
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
                    StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
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
                // 对话轮次结束，触发后台记忆提取
                if let Some(mem) = self.memory.as_ref().cloned() {
                    let provider = self.provider.clone();
                    let msgs = session.messages.clone();
                    tokio::spawn(async move {
                        if let Err(e) = mem.extract_and_save(msgs, provider).await {
                            tracing::debug!("记忆提取失败: {e}");
                        }
                    });
                }
                break;
            }

            // 顺序执行所有工具（ctx 不是 Send，不能并发）
            let mut tool_results = vec![];
            for (id, name, json) in pending_tools {
                let input = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                let tool = self.tool_impls.get(&name).cloned();

                // 触发工具开始回调
                if let Some(cb) = &self.tool_cb {
                    cb(ToolEvent::Start {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
                let start = Instant::now();

                let result: (String, bool) = if let Some(t) = tool {
                    if ctx.is_allowed(&name, &input) {
                        match t.run(input, ctx).await {
                            Ok(r) => (r.content, r.is_error),
                            Err(e) => (format!("工具执行错误: {e}"), true),
                        }
                    } else {
                        (format!("工具 `{name}` 在当前模式下不被允许"), true)
                    }
                } else {
                    (format!("工具 `{name}` 未注册"), true)
                };

                let elapsed_secs = start.elapsed().as_secs_f64();

                // 触发工具完成回调
                if let Some(cb) = &self.tool_cb {
                    cb(ToolEvent::End {
                        id: id.clone(),
                        name: name.clone(),
                        is_error: result.1,
                        elapsed_secs,
                        output: result.0.clone(),
                    });
                }

                tool_results.push((id, name, result, elapsed_secs));
            }

            for (id, _name, (output, is_error), _elapsed) in tool_results {
                session.push_tool_result(id, output, is_error);
            }
        }
        Ok(())
    }

    /// 手动触发上下文压缩（供 /compact 命令使用）
    pub async fn compact_context(
        &self,
        session: &mut Session,
    ) -> Result<crate::compact::CompactResult> {
        compact_session(session, self.provider.as_ref(), self.context_window).await
    }
}

fn default_system_prompt() -> String {
    "你是一个专业的 AI 编程助手，擅长分析代码、解决技术问题、编写程序。\n\
    当需要操作文件系统、执行命令或搜索代码时，你会主动使用工具完成任务。\n\
    工具使用原则：Read 后才能 Write；Edit 前必须精确匹配。\n\
    使用简洁、准确的中文与用户沟通，优先给出可运行的代码和具体的操作步骤。"
        .to_string()
}
