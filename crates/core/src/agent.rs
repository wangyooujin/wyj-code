//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::claude_md::ClaudeMdLoader;
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

/// 注入内容的类别：用户在 Agent 忙碌期间排队的补充消息，或系统产生的
/// 提醒（如后台子 Agent 完成结果）。调用方的 `on_inject` 回调据此区分
/// 是否需要同步 UI 的用户消息队列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionKind {
    UserMessage,
    SystemReminder,
}

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
    /// CLAUDE.md 系记忆文件加载器（可选）
    claude_md: Option<Arc<ClaudeMdLoader>>,
    /// 可选的工具事件回调（Send + Sync，可跨线程）
    tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    /// 可选的 token 用量回调（子 Agent 向 Hub 汇报用量用）
    usage_cb: Option<Arc<dyn Fn(u32, u32) + Send + Sync>>,
    /// 会话标题生成器（可选，仅主 Agent 设置，子 Agent 不设置）
    summary: Option<Arc<crate::summary::SummaryGenerator>>,
    /// 当前会话 ID（用于标题生成，子 Agent 不设置）
    session_id: Option<String>,
    /// 标题生成完成回调（TUI 据此更新终端窗口标题）
    title_cb: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            system_prompt: default_system_prompt(),
            tools: vec![],
            tool_impls: HashMap::new(),
            max_tokens: 8192,
            // 真正的成本/时长上限由每轮 token 预算触发的自动压缩承担；这里仅防止模型死循环
            max_turns: 200,
            context_window: 200_000,
            memory: None,
            claude_md: None,
            tool_cb: None,
            usage_cb: None,
            summary: None,
            session_id: None,
            title_cb: None,
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

    /// 设置单回合最大推理轮数（默认 200）。真正的成本/时长上限由自动压缩承担，
    /// 此值仅防止模型死循环。可适当调低以限制极端情况下的 API 调用次数。
    pub fn with_max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// 在默认系统提示末尾追加额外内容（如 Plan 模式限制说明）
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

    pub fn memory_ref(&self) -> Option<&Arc<MemoryStore>> {
        self.memory.as_ref()
    }

    pub fn with_claude_md(mut self, loader: Arc<ClaudeMdLoader>) -> Self {
        self.claude_md = Some(loader);
        self
    }

    pub fn claude_md_ref(&self) -> Option<&Arc<ClaudeMdLoader>> {
        self.claude_md.as_ref()
    }

    /// 注册工具事件回调（用于 headless 格式化输出或 TUI 事件推送）
    pub fn with_tool_callback(mut self, cb: impl Fn(ToolEvent) + Send + Sync + 'static) -> Self {
        self.tool_cb = Some(Arc::new(cb));
        self
    }

    /// 注册 token 用量回调（每收到一次流式 Usage 事件调用一次，参数为增量值）
    pub fn with_usage_callback(mut self, cb: impl Fn(u32, u32) + Send + Sync + 'static) -> Self {
        self.usage_cb = Some(Arc::new(cb));
        self
    }

    /// 设置会话标题生成器（仅主 Agent 设置，子 Agent 不设置）
    pub fn with_summary(mut self, gen: Arc<crate::summary::SummaryGenerator>) -> Self {
        self.summary = Some(gen);
        self
    }

    /// 设置当前会话 ID（用于标题生成写盘定位）
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// 更新当前会话 ID（用于 TUI 切换会话后更新，无需 rebuild Agent）
    pub fn set_session_id(&mut self, id: impl Into<String>) {
        self.session_id = Some(id.into());
    }

    /// 注册标题生成完成回调（TUI 据此更新终端窗口标题）
    pub fn with_title_callback(mut self, cb: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.title_cb = Some(Arc::new(cb));
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
        self.run_turn_with_injection(session, ctx, on_text, None, |_| {})
            .await
    }

    /// 与 [`run_turn`] 相同，但额外支持在工具调用往返之间／回合结束前的自然边界
    /// 排空一个可选的注入通道，把 Agent 忙碌期间用户提交的补充内容块合并进当前
    /// 对话，而不打断正在进行的流式生成或工具执行。`on_inject` 在每次实际发生
    /// 注入时被调用一次，供调用方（如 TUI）同步 UI 状态。
    pub async fn run_turn_with_injection(
        &self,
        session: &mut Session,
        ctx: &dyn ToolContext,
        on_text: &mut impl FnMut(&str),
        mut inject_rx: Option<
            &mut tokio::sync::mpsc::UnboundedReceiver<(Vec<ContentBlock>, InjectionKind)>,
        >,
        mut on_inject: impl FnMut(InjectionKind),
    ) -> Result<()> {
        // 构建 system prompt 基础部分：默认提示 + 跨会话记忆 + CLAUDE.md 祖先链。
        // CLAUDE.md 内容拼进 system prompt（而非注入 user 消息），配合 prompt caching
        // 使其首轮全价、后续轮次命中缓存按 0.1x 计费，避免跨轮线性累积。
        // 子目录动态 reminder 在循环内追加到 system 末尾（只增不减，前缀仍可缓存）。
        let mut system = self.system_prompt.clone();
        if let Some(mem) = &self.memory {
            // 会话级快照：本会话内容固定，防止后台提取的新记忆改变 system
            // 前缀而击穿 prompt 缓存；新记忆自然在下个会话生效。
            let ctx_str = mem.load_context_cached();
            if !ctx_str.is_empty() {
                system.push_str("\n\n");
                system.push_str(ctx_str);
            }
        }
        if let Some(loader) = &self.claude_md {
            if let Some(reminder) = loader.turn_reminder() {
                system.push_str("\n\n");
                system.push_str(&reminder);
            }
        }

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

            session.api_calls += 1;
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
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                    } => {
                        // 供应商返回的 input_tokens 仅含未命中缓存的（全价）部分，
                        // 缓存命中（0.1x）与缓存写入（1.25x）单独累计用于 /cost 展示。
                        session.add_usage(input_tokens, output_tokens);
                        session
                            .add_cache_usage(cache_read_input_tokens, cache_creation_input_tokens);
                        if let Some(cb) = &self.usage_cb {
                            cb(input_tokens, output_tokens);
                        }
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

            let has_tool_calls = stop_reason == StopReason::ToolUse && !pending_tools.is_empty();

            if has_tool_calls {
                // 解析输入并收集 CLAUDE.md 触达目录（按原始调用顺序）
                let calls: Vec<(String, String, serde_json::Value)> = pending_tools
                    .into_iter()
                    .map(|(id, name, json)| {
                        let input = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                        (id, name, input)
                    })
                    .collect();
                let mut touched_dirs: Vec<std::path::PathBuf> = vec![];
                if self.claude_md.is_some() {
                    for (_, name, input) in &calls {
                        if let Some(dir) = touched_dir(name, input, ctx.cwd()) {
                            touched_dirs.push(dir);
                        }
                    }
                }

                // 分区执行：parallel_safe 的调用（如 SubAgent）各自并发，其余调用
                // 保持相互顺序、但与并发组同时进行；结果按原始下标排序回填保序。
                // 均为单任务内并发（join!），不要求 ctx 满足 Send/'static。
                let total = calls.len();
                let mut par_futs = vec![];
                let mut seq_calls = vec![];
                for (idx, (id, name, input)) in calls.into_iter().enumerate() {
                    let is_par = self
                        .tool_impls
                        .get(&name)
                        .map(|t| t.parallel_safe())
                        .unwrap_or(false);
                    if is_par && total > 1 {
                        par_futs.push(async move {
                            (idx, self.exec_tool_call(ctx, id, name, input).await)
                        });
                    } else {
                        seq_calls.push((idx, id, name, input));
                    }
                }
                let seq_fut = async {
                    let mut out = vec![];
                    for (idx, id, name, input) in seq_calls {
                        out.push((idx, self.exec_tool_call(ctx, id, name, input).await));
                    }
                    out
                };
                let (par_results, seq_results) =
                    tokio::join!(futures::future::join_all(par_futs), seq_fut);

                let mut tool_results: Vec<_> = par_results.into_iter().chain(seq_results).collect();
                tool_results.sort_by_key(|(idx, _)| *idx);
                for (_, (id, output, is_error)) in tool_results {
                    session.push_tool_result(id, output, is_error);
                }

                // 子目录动态加载：本轮工具触达的目录若有未展示过的 CLAUDE.md 系文件，
                // 追加到 system prompt 末尾（而非注入 user 消息），使历史消息保持
                // 干净、避免跨轮重复发送；seen_dirs 去重保证每条只追加一次。
                if let Some(loader) = &self.claude_md {
                    let mut new_reminders = String::new();
                    for dir in &touched_dirs {
                        if let Some(text) = loader.maybe_dir_reminder(dir) {
                            new_reminders.push_str("\n\n");
                            new_reminders.push_str(&text);
                        }
                    }
                    if !new_reminders.is_empty() {
                        system.push_str(&new_reminders);
                    }
                }
            }

            // 排空可选的注入通道：把 Agent 忙碌期间用户提交的补充内容块，
            // 合并进当前工具结果所在的 user 消息（若刚执行过工具），
            // 或作为一条新的 user 消息续接对话（若本轮本要结束）。
            let mut got_injection = false;
            if let Some(rx) = inject_rx.as_deref_mut() {
                while let Ok((blocks, kind)) = rx.try_recv() {
                    session.push_user_blocks_merged(blocks);
                    got_injection = true;
                    on_inject(kind);
                }
            }

            if !has_tool_calls && !got_injection {
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
                // 首轮后触发后台标题生成（若已配置 SummaryGenerator 且有 session_id）
                if let Some(gen) = self.summary.as_ref().cloned() {
                    if let Some(sid) = self.session_id.clone() {
                        let msgs = session.messages.clone();
                        let title_cb = self.title_cb.clone();
                        tokio::spawn(async move {
                            if let Some(title) = gen.generate_title(&sid, &msgs).await {
                                if let Some(cb) = title_cb {
                                    cb(title);
                                }
                            }
                        });
                    }
                }
                break;
            }
        }
        Ok(())
    }

    /// 执行单个工具调用：触发 Start/End 回调、权限检查、执行并计时。
    /// 返回 (tool_use_id, 输出文本, 是否错误)。
    async fn exec_tool_call(
        &self,
        ctx: &dyn ToolContext,
        id: String,
        name: String,
        input: serde_json::Value,
    ) -> (String, String, bool) {
        let tool = self.tool_impls.get(&name).cloned();

        if let Some(cb) = &self.tool_cb {
            cb(ToolEvent::Start {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }
        let start = Instant::now();

        let (output, is_error): (String, bool) = if let Some(t) = tool {
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

        if let Some(cb) = &self.tool_cb {
            cb(ToolEvent::End {
                id: id.clone(),
                name: name.clone(),
                is_error,
                elapsed_secs,
                output: output.clone(),
            });
        }

        (id, output, is_error)
    }

    /// 手动触发上下文压缩（供 /compact 命令使用）
    pub async fn compact_context(
        &self,
        session: &mut Session,
    ) -> Result<crate::compact::CompactResult> {
        compact_session(session, self.provider.as_ref(), self.context_window).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolContext, ToolResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wyj_api::provider::EventStream;
    use wyj_api::types::{Message, StopReason, ToolResultContent};

    struct FakeCtx;
    #[async_trait::async_trait]
    impl ToolContext for FakeCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/tmp")
        }
        fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
            true
        }
    }

    /// 第一轮返回两个 Sleep 工具调用，第二轮返回 EndTurn 文本
    struct TwoToolProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for TwoToolProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _max_tokens: u32,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "t1".into(),
                        name: "Sleep".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "t1".into(),
                        json_delta: r#"{"ms":150,"tag":"first"}"#.into(),
                    }),
                    Ok(StreamEvent::ToolUseStart {
                        id: "t2".into(),
                        name: "Sleep".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "t2".into(),
                        json_delta: r#"{"ms":150,"tag":"second"}"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("done".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 每轮都直接 EndTurn 的 provider（注入路由测试用）
    struct EndTurnProvider;
    #[async_trait::async_trait]
    impl Provider for EndTurnProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _max_tokens: u32,
        ) -> Result<EventStream> {
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 可并发的 mock 工具：睡 ms 毫秒后返回 tag
    struct SleepTool;
    #[async_trait::async_trait]
    impl Tool for SleepTool {
        fn name(&self) -> &str {
            "Sleep"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "Sleep".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        fn parallel_safe(&self) -> bool {
            true
        }
        async fn run(
            &self,
            input: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<ToolResult> {
            let ms = input.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            let tag = input.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            Ok(ToolResult::ok(tag.to_string()))
        }
    }

    #[tokio::test]
    async fn parallel_safe_tools_run_concurrently_and_keep_result_order() {
        let mut agent = Agent::new(Arc::new(TwoToolProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(SleepTool));
        let mut session = Session::new();
        session.push_user("go");

        let start = Instant::now();
        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();
        // 两个 150ms 的 parallel_safe 工具应并发执行：总耗时远小于串行的 300ms
        assert!(
            start.elapsed() < std::time::Duration::from_millis(280),
            "工具未并发执行，耗时 {:?}",
            start.elapsed()
        );

        // 结果必须按原始调用顺序回填
        let mut results = vec![];
        for m in &session.messages {
            for b in &m.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content: ToolResultContent::Text(text),
                    ..
                } = b
                {
                    results.push((tool_use_id.clone(), text.clone()));
                }
            }
        }
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], ("t1".to_string(), "first".to_string()));
        assert_eq!(results[1], ("t2".to_string(), "second".to_string()));
    }

    #[tokio::test]
    async fn injection_kind_routes_to_callback() {
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send((
            vec![ContentBlock::Text {
                text: "bg result".into(),
            }],
            InjectionKind::SystemReminder,
        ))
        .unwrap();

        let mut kinds = vec![];
        let mut session = Session::new();
        session.push_user("hi");
        agent
            .run_turn_with_injection(&mut session, &FakeCtx, &mut |_| {}, Some(&mut rx), |k| {
                kinds.push(k)
            })
            .await
            .unwrap();
        // SystemReminder 注入应回调对应 kind（TUI 据此不弹用户消息队列）
        assert_eq!(kinds, vec![InjectionKind::SystemReminder]);
        // 注入内容已合并进对话
        let merged = session.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("bg result")))
        });
        assert!(merged);
    }
}

fn default_system_prompt() -> String {
    wyj_i18n::tr("system_prompt.default")
}

/// 从工具调用输入里推导其触达的目录，供 CLAUDE.md 子目录动态加载判断。
/// Read/Edit/Write 用 file_path（取父目录）；Glob/Grep 用 path（文件取父目录，目录取自身）。
fn touched_dir(
    tool_name: &str,
    input: &serde_json::Value,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let field = match tool_name {
        "Read" | "Edit" | "Write" => "file_path",
        "Glob" | "Grep" => "path",
        _ => return None,
    };
    let raw = input.get(field)?.as_str()?;
    let p = std::path::Path::new(raw);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    if resolved.is_dir() {
        Some(resolved)
    } else {
        resolved.parent().map(|p| p.to_path_buf())
    }
}
