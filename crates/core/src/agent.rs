//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::claude_md::ClaudeMdLoader;
use crate::compact::{compact_session, compact_trigger_buffer, estimate_tokens};
use crate::hooks::{HookOutcome, HookRunner};
use crate::memory::MemoryStore;
use crate::session::Session;
use crate::tool::{Tool, ToolCallMeta, ToolContext};
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
    /// 会话启动时采集的 git 状态快照（`<system-reminder>` 全文），仅在会话
    /// 首轮注入首条 user 消息。不进 system prompt：git 状态每轮都可能变，
    /// 进 system 会击穿 prompt 缓存；进首轮消息则位于缓存前缀内、轮间稳定。
    git_snapshot: Option<String>,
    /// Extended thinking 预算（None/0 = 关闭）与交错思考开关
    thinking_budget: Option<u32>,
    interleaved_thinking: bool,
    /// thinking 文本增量回调（TUI 展示 / headless stderr 输出）
    thinking_cb: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    /// Hooks 生命周期自动化执行器（可选，子 Agent 不设置，避免嵌套 shell 副作用）
    hook_runner: Option<Arc<HookRunner>>,
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
            git_snapshot: None,
            thinking_budget: None,
            interleaved_thinking: true,
            thinking_cb: None,
            hook_runner: None,
        }
    }

    /// 设置会话启动时的 git 状态快照（见 `git_snapshot` 字段说明）
    pub fn with_git_snapshot(mut self, snapshot: Option<String>) -> Self {
        self.git_snapshot = snapshot;
        self
    }

    /// 配置 extended thinking（budget None/0 = 关闭）
    pub fn with_thinking(mut self, budget: Option<u32>, interleaved: bool) -> Self {
        self.thinking_budget = budget.filter(|b| *b > 0);
        self.interleaved_thinking = interleaved;
        self
    }

    /// 注册 thinking 文本增量回调
    pub fn with_thinking_callback(mut self, cb: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.thinking_cb = Some(Arc::new(cb));
        self
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

    /// 装配 Hooks 执行器（子 Agent 不调用此方法，`hook_runner` 保持 `None`）
    pub fn with_hooks(mut self, runner: Arc<HookRunner>) -> Self {
        self.hook_runner = Some(runner);
        self
    }

    /// 获取当前 Hooks 执行器引用（TUI 侧据此给 `/hooks` 命令提供启用状态）。
    pub fn hook_runner_ref(&self) -> Option<&Arc<HookRunner>> {
        self.hook_runner.as_ref()
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

        // 会话首轮：把 git 状态快照前插进首条 user 消息（仅一次，之后随
        // 历史持久化；resume 的会话历史里已带有当时的快照，不重复注入）
        if let Some(snap) = &self.git_snapshot {
            if session.messages.len() == 1 {
                session.prepend_to_last_user(vec![ContentBlock::Text { text: snap.clone() }]);
            }
        }

        // UserPromptSubmit：调用方已把本次用户提交 push 进 session（3 处调用点
        // 各自 push_user/push_user_with_blocks），这里统一触发一次（每次调用
        // run_turn_with_injection 即代表一次新提交，不随内部 turn 循环重复）。
        if let Some(hr) = &self.hook_runner {
            match hr
                .run(
                    "UserPromptSubmit",
                    None,
                    self.session_id.as_deref(),
                    ctx.cwd(),
                    None,
                    None,
                )
                .await
            {
                HookOutcome::Block(reason) => {
                    // Provider 要求角色严格交替（见 session.rs push_user_blocks_merged
                    // 文档注释）：撤销刚 push 的这条 user 消息，回退到提交前状态，
                    // 避免下次提交时连续两条 user 消息违反该不变量。
                    session.messages.pop();
                    on_text(&reason);
                    return Ok(());
                }
                HookOutcome::Continue {
                    context: Some(ctx_text),
                } => {
                    session.prepend_to_last_user(vec![ContentBlock::Text { text: ctx_text }]);
                }
                _ => {}
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
            let compact_threshold = self
                .context_window
                .saturating_sub(compact_trigger_buffer(self.context_window));
            if estimated > compact_threshold {
                match compact_session(session, self.provider.as_ref(), self.context_window).await {
                    Ok(r) => on_text(&format!(
                        "\n[已压缩对话历史：移除 {} 条消息，节省约 {} tokens]\n",
                        r.messages_removed, r.tokens_saved_estimate
                    )),
                    Err(e) => tracing::warn!("上下文压缩失败: {e}"),
                }
            }

            // 流式消费，带中断重试：流已消费一半时断开（网络重置、供应商
            // overloaded 流内错误等），丢弃本次全部半成品缓冲、整轮重新生成。
            // 不变量：半成品 assistant 消息绝不 push 进 session（流完整结束
            // 才组装），故重试即重新生成，UI 可能出现重复文本片段但正确性无损。
            // usage 事件同样缓冲到流成功后才入账，避免失败尝试的重复计数。
            const MAX_STREAM_RETRIES: u32 = 2;
            let mut stream_retries: u32 = 0;
            let opts = wyj_api::provider::RequestOptions {
                max_tokens: self.max_tokens,
                thinking_budget: self.thinking_budget,
                interleaved: self.interleaved_thinking,
            };
            // 按到达顺序累积的内容块（thinking 可与 tool_use 交错，顺序必须保留）
            enum StreamedBlock {
                Text(String),
                Thinking {
                    text: String,
                    signature: String,
                },
                Redacted(String),
                ToolUse {
                    id: String,
                    name: String,
                    json: String,
                },
            }
            let (blocks, stop_reason) = loop {
                session.api_calls += 1;
                let mut stream = self
                    .provider
                    .stream(&system, &session.messages, &self.tools, &opts)
                    .await?;

                let mut blocks: Vec<StreamedBlock> = vec![];
                let mut current_tool_idx: Option<usize> = None;
                let mut stop_reason = StopReason::EndTurn;
                let mut pending_usage: Vec<(u32, u32, u32, u32)> = vec![];
                let mut stream_err: Option<anyhow::Error> = None;

                while let Some(event) = stream.next().await {
                    let event = match event {
                        Ok(ev) => ev,
                        Err(e) => {
                            stream_err = Some(e);
                            break;
                        }
                    };
                    match event {
                        StreamEvent::TextDelta(delta) => {
                            on_text(&delta);
                            match blocks.last_mut() {
                                Some(StreamedBlock::Text(t)) => t.push_str(&delta),
                                _ => blocks.push(StreamedBlock::Text(delta)),
                            }
                        }
                        StreamEvent::ThinkingStart => {
                            blocks.push(StreamedBlock::Thinking {
                                text: String::new(),
                                signature: String::new(),
                            });
                        }
                        StreamEvent::ThinkingDelta(delta) => {
                            if let Some(cb) = &self.thinking_cb {
                                cb(&delta);
                            }
                            match blocks.last_mut() {
                                Some(StreamedBlock::Thinking { text, .. }) => text.push_str(&delta),
                                _ => blocks.push(StreamedBlock::Thinking {
                                    text: delta,
                                    signature: String::new(),
                                }),
                            }
                        }
                        StreamEvent::ThinkingSignatureDelta(sig) => {
                            if let Some(StreamedBlock::Thinking { signature, .. }) =
                                blocks.last_mut()
                            {
                                signature.push_str(&sig);
                            }
                        }
                        StreamEvent::RedactedThinking(data) => {
                            blocks.push(StreamedBlock::Redacted(data));
                        }
                        StreamEvent::ToolUseStart { id, name } => {
                            blocks.push(StreamedBlock::ToolUse {
                                id,
                                name,
                                json: String::new(),
                            });
                            current_tool_idx = Some(blocks.len() - 1);
                        }
                        StreamEvent::ToolUseDelta { id, json_delta } => {
                            let idx = if id.is_empty() {
                                current_tool_idx
                            } else {
                                blocks.iter().position(|b| {
                                    matches!(b, StreamedBlock::ToolUse { id: tid, .. } if *tid == id)
                                })
                            };
                            if let Some(StreamedBlock::ToolUse { json, .. }) =
                                idx.and_then(|i| blocks.get_mut(i))
                            {
                                json.push_str(&json_delta);
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
                            pending_usage.push((
                                input_tokens,
                                output_tokens,
                                cache_read_input_tokens,
                                cache_creation_input_tokens,
                            ));
                        }
                    }
                }

                match stream_err {
                    Some(e) if stream_retries < MAX_STREAM_RETRIES => {
                        stream_retries += 1;
                        tracing::warn!("流中断（第 {stream_retries} 次重试）: {e}");
                        on_text(&format!(
                            "\n[连接中断，正在重试 {stream_retries}/{MAX_STREAM_RETRIES}...]\n"
                        ));
                        tokio::time::sleep(std::time::Duration::from_secs(
                            1 << stream_retries.min(5),
                        ))
                        .await;
                        continue;
                    }
                    Some(e) => return Err(e),
                    None => {
                        // 流完整结束：usage 一次性入账。供应商返回的 input_tokens
                        // 仅含未命中缓存的（全价）部分，缓存命中（0.1x）与缓存
                        // 写入（1.25x）单独累计用于 /cost 展示。
                        for (input, output, cache_read, cache_write) in pending_usage {
                            session.add_usage(input, output);
                            session.add_cache_usage(cache_read, cache_write);
                            if let Some(cb) = &self.usage_cb {
                                cb(input, output);
                            }
                        }
                        break (blocks, stop_reason);
                    }
                }
            };

            // 组装助手内容块（保持到达顺序；thinking 块含 signature 原样入历史，
            // 工具调用续轮时回传给 API —— 缺失会被 Anthropic 拒绝）
            let mut assistant_blocks = vec![];
            let mut pending_tools: Vec<(String, String, String)> = vec![]; // (id, name, json)
            for b in &blocks {
                match b {
                    StreamedBlock::Text(t) => {
                        if !t.is_empty() {
                            assistant_blocks.push(ContentBlock::Text { text: t.clone() });
                        }
                    }
                    StreamedBlock::Thinking { text, signature } => {
                        assistant_blocks.push(ContentBlock::Thinking {
                            thinking: text.clone(),
                            signature: signature.clone(),
                        });
                    }
                    StreamedBlock::Redacted(data) => {
                        assistant_blocks
                            .push(ContentBlock::RedactedThinking { data: data.clone() });
                    }
                    StreamedBlock::ToolUse { id, name, json } => {
                        let input = serde_json::from_str(json)
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        assistant_blocks.push(ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input,
                        });
                        pending_tools.push((id.clone(), name.clone(), json.clone()));
                    }
                }
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
                // Stop：本轮即将结束，给 hook 一次机会决定是否继续（追加一条
                // user 消息并让循环再跑一轮，而非依赖注入 channel 的时序）。
                if let Some(hr) = &self.hook_runner {
                    let outcome = hr
                        .run(
                            "Stop",
                            None,
                            self.session_id.as_deref(),
                            ctx.cwd(),
                            None,
                            None,
                        )
                        .await;
                    match outcome {
                        HookOutcome::Continue { context } => {
                            session.push_user_blocks_merged(vec![ContentBlock::Text {
                                text: context.unwrap_or_default(),
                            }]);
                            got_injection = true;
                        }
                        HookOutcome::Block(reason) => {
                            session
                                .push_user_blocks_merged(vec![ContentBlock::Text { text: reason }]);
                            got_injection = true;
                        }
                        _ => {}
                    }
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
    /// 返回 (tool_use_id, 发给模型的内容, 是否错误)；End 回调携带展示用文本。
    async fn exec_tool_call(
        &self,
        ctx: &dyn ToolContext,
        id: String,
        name: String,
        input: serde_json::Value,
    ) -> (String, wyj_api::types::ToolResultContent, bool) {
        use wyj_api::types::ToolResultContent;
        let tool = self.tool_impls.get(&name).cloned();

        if let Some(cb) = &self.tool_cb {
            cb(ToolEvent::Start {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }
        let start = Instant::now();

        let (display, content, is_error): (String, ToolResultContent, bool) = if let Some(t) = tool
        {
            let pre_outcome = if let Some(hr) = &self.hook_runner {
                hr.run(
                    "PreToolUse",
                    Some(&name),
                    self.session_id.as_deref(),
                    ctx.cwd(),
                    Some(&input),
                    None,
                )
                .await
            } else {
                HookOutcome::Passthrough
            };

            let meta = ToolCallMeta {
                tool_use_id: id.clone(),
            };
            match pre_outcome {
                HookOutcome::Block(reason) => {
                    let msg = format!("PreToolUse hook 拦截了工具 `{name}`：{reason}");
                    (msg.clone(), ToolResultContent::Text(msg), true)
                }
                // approve：跳过 is_allowed / needs_permission+confirm_tool 两道闸门，直接执行
                HookOutcome::Approve => run_tool(&t, input, ctx, &meta).await,
                HookOutcome::Passthrough | HookOutcome::Continue { .. } => {
                    if !ctx.is_allowed(&name, &input) {
                        let msg = format!("工具 `{name}` 在当前模式下不被允许");
                        (msg.clone(), ToolResultContent::Text(msg), true)
                    } else if t.needs_permission(&input)
                        && !ctx.confirm_tool(&name, &t.action_summary(&input)).await
                    {
                        // 逐调用权限确认：用户拒绝，将拒绝信息回灌给模型（模型据此改道）
                        let msg = format!(
                            "用户拒绝执行工具 `{name}`。请不要重试该操作；改用其他方式，或先向用户询问原因。"
                        );
                        (msg.clone(), ToolResultContent::Text(msg), true)
                    } else {
                        run_tool(&t, input, ctx, &meta).await
                    }
                }
            }
        } else {
            let msg = format!("工具 `{name}` 未注册");
            (msg.clone(), ToolResultContent::Text(msg), true)
        };

        let elapsed_secs = start.elapsed().as_secs_f64();

        // PostToolUse：把 hook 返回的 Block/Continue 附加反馈追加进模型可见的
        // content 与展示用 display，不改变 is_error（补充信息，不是把结果打成失败）。
        let hook_feedback: Option<String> = if let Some(hr) = &self.hook_runner {
            let tool_response = serde_json::json!({ "content": display, "is_error": is_error });
            match hr
                .run(
                    "PostToolUse",
                    Some(&name),
                    self.session_id.as_deref(),
                    ctx.cwd(),
                    None,
                    Some(&tool_response),
                )
                .await
            {
                HookOutcome::Block(reason) => Some(reason),
                HookOutcome::Continue { context: Some(c) } => Some(c),
                _ => None,
            }
        } else {
            None
        };
        let (display, content) = match hook_feedback {
            Some(fb) => {
                let suffix = format!("\n\n[PostToolUse hook] {fb}");
                let content = append_hook_feedback(content, &suffix);
                (format!("{display}{suffix}"), content)
            }
            None => (display, content),
        };

        if let Some(cb) = &self.tool_cb {
            cb(ToolEvent::End {
                id: id.clone(),
                name: name.clone(),
                is_error,
                elapsed_secs,
                output: display,
            });
        }

        (id, content, is_error)
    }

    /// 手动触发上下文压缩（供 /compact 命令使用）
    pub async fn compact_context(
        &self,
        session: &mut Session,
    ) -> Result<crate::compact::CompactResult> {
        compact_session(session, self.provider.as_ref(), self.context_window).await
    }
}

/// 执行工具并组装 (display, content, is_error) 三元组，抽出复用于
/// PreToolUse 的 `Approve`（跳过权限闸门）与常规放行两条路径。
async fn run_tool(
    t: &Arc<dyn Tool>,
    input: serde_json::Value,
    ctx: &dyn ToolContext,
    meta: &ToolCallMeta,
) -> (String, wyj_api::types::ToolResultContent, bool) {
    use wyj_api::types::ToolResultContent;
    match t.run_with_meta(input, ctx, meta).await {
        Ok(r) => match r.parts {
            // 结构化结果（如图片块）：display 用降级文本，模型收 Parts
            Some(parts) => (r.content, ToolResultContent::Parts(parts), r.is_error),
            None => (
                r.content.clone(),
                ToolResultContent::Text(r.content),
                r.is_error,
            ),
        },
        Err(e) => {
            let msg = format!("工具执行错误: {e}");
            (msg.clone(), ToolResultContent::Text(msg), true)
        }
    }
}

/// PostToolUse 反馈追加进工具结果内容（Text 追加后缀，Parts 追加一个 Text part）。
fn append_hook_feedback(
    content: wyj_api::types::ToolResultContent,
    suffix: &str,
) -> wyj_api::types::ToolResultContent {
    use wyj_api::types::{ToolResultContent, ToolResultPart};
    match content {
        ToolResultContent::Text(mut s) => {
            s.push_str(suffix);
            ToolResultContent::Text(s)
        }
        ToolResultContent::Parts(mut parts) => {
            parts.push(ToolResultPart::Text {
                text: suffix.to_string(),
            });
            ToolResultContent::Parts(parts)
        }
        ToolResultContent::Blocks(mut blocks) => {
            blocks.push(serde_json::json!({ "type": "text", "text": suffix }));
            ToolResultContent::Blocks(blocks)
        }
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
            _opts: &wyj_api::provider::RequestOptions,
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
            _opts: &wyj_api::provider::RequestOptions,
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

    /// 记录被调用次数的 mock 工具，用于验证 PreToolUse Block 阻止了工具实际执行
    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "Echo"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "Echo".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::ok("echoed".to_string()))
        }
    }

    /// `is_allowed` 恒返回 false，用于验证 PreToolUse `Approve` 能绕过它
    struct DenyAllCtx;
    #[async_trait::async_trait]
    impl ToolContext for DenyAllCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/tmp")
        }
        fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
            false
        }
    }

    fn hooks_settings_with(
        event: &str,
        matcher: Option<&str>,
        command: &str,
    ) -> crate::hooks::HooksSettings {
        use crate::hooks::{HookCommand, HookMatcherEntry, HooksSettings};
        let mut hooks = HashMap::new();
        hooks.insert(
            event.to_string(),
            vec![HookMatcherEntry {
                matcher: matcher.map(|s| s.to_string()),
                hooks: vec![HookCommand {
                    hook_type: "command".into(),
                    command: command.into(),
                    timeout: Some(5),
                }],
            }],
        );
        HooksSettings { hooks }
    }

    #[tokio::test]
    async fn pre_tool_use_block_prevents_tool_execution() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(EndTurnProvider));
        agent.register_tool(Arc::new(CountingTool {
            calls: calls.clone(),
        }));
        let settings = hooks_settings_with(
            "PreToolUse",
            None,
            r#"echo '{"decision":"block","reason":"nope"}'"#,
        );
        let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
            settings, true,
        )));

        let (id, content, is_error) = agent
            .exec_tool_call(&FakeCtx, "1".into(), "Echo".into(), serde_json::json!({}))
            .await;

        assert_eq!(id, "1");
        assert!(is_error);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "被 PreToolUse Block 的工具不应实际执行"
        );
        match content {
            ToolResultContent::Text(t) => assert!(t.contains("nope")),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn pre_tool_use_approve_bypasses_is_allowed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(EndTurnProvider));
        agent.register_tool(Arc::new(CountingTool {
            calls: calls.clone(),
        }));
        let settings = hooks_settings_with("PreToolUse", None, r#"echo '{"decision":"approve"}'"#);
        let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
            settings, true,
        )));

        let (_id, _content, is_error) = agent
            .exec_tool_call(
                &DenyAllCtx,
                "1".into(),
                "Echo".into(),
                serde_json::json!({}),
            )
            .await;

        assert!(!is_error);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "approve 应绕过 is_allowed 直接执行"
        );
    }

    #[tokio::test]
    async fn post_tool_use_block_appends_feedback_without_marking_error() {
        let mut agent = Agent::new(Arc::new(EndTurnProvider));
        agent.register_tool(Arc::new(CountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let settings = hooks_settings_with(
            "PostToolUse",
            None,
            r#"echo '{"decision":"block","reason":"lint failed"}'"#,
        );
        let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
            settings, true,
        )));

        let (_id, content, is_error) = agent
            .exec_tool_call(&FakeCtx, "1".into(), "Echo".into(), serde_json::json!({}))
            .await;

        assert!(!is_error, "PostToolUse 反馈不应把成功结果打成失败");
        match content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("echoed"));
                assert!(t.contains("lint failed"));
            }
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn no_hook_runner_behaves_exactly_as_before() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(EndTurnProvider));
        agent.register_tool(Arc::new(CountingTool {
            calls: calls.clone(),
        }));

        let (_id, _content, is_error) = agent
            .exec_tool_call(&FakeCtx, "1".into(), "Echo".into(), serde_json::json!({}))
            .await;

        assert!(!is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn user_prompt_submit_block_pops_message_and_skips_model_call() {
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let settings = hooks_settings_with(
            "UserPromptSubmit",
            None,
            r#"echo '{"decision":"block","reason":"denied"}'"#,
        );
        let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
            settings, true,
        )));
        let mut session = Session::new();
        session.push_user("hello");

        let mut out = String::new();
        agent
            .run_turn(&mut session, &FakeCtx, &mut |t| out.push_str(t))
            .await
            .unwrap();

        assert!(out.contains("denied"));
        assert!(
            session.messages.is_empty(),
            "被 block 的 user 消息应回退，不留在 session 里"
        );
    }

    #[tokio::test]
    async fn user_prompt_submit_continue_prepends_context() {
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let settings = hooks_settings_with(
            "UserPromptSubmit",
            None,
            r#"echo '{"additionalContext":"extra ctx"}'"#,
        );
        let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
            settings, true,
        )));
        let mut session = Session::new();
        session.push_user("hello");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        let has_ctx = session.messages[0]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("extra ctx")));
        assert!(has_ctx, "additionalContext 应被前插进首条 user 消息");
    }

    /// 每次调用都计数、恒 EndTurn 的 provider（Stop hook 续跑测试用）
    struct CountingEndTurnProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Provider for CountingEndTurnProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn stop_hook_continue_makes_loop_run_again() {
        // 用一个临时文件做进程外状态：第一次调用返回 continue:false（要求继续），
        // 第二次（文件已存在）放行，让循环真正结束，避免死循环。
        let flag_path =
            std::env::temp_dir().join(format!("wyj-stop-hook-test-{}.flag", uuid::Uuid::new_v4()));
        let flag = flag_path.display();
        let command = format!(
            "if [ -f {flag} ]; then echo '{{}}'; else touch {flag}; echo '{{\"continue\":false,\"reason\":\"once more\"}}'; fi"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(Arc::new(CountingEndTurnProvider {
            calls: calls.clone(),
        }));
        let settings = hooks_settings_with("Stop", None, &command);
        let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
            settings, true,
        )));
        let mut session = Session::new();
        session.push_user("hello");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "Stop 要求 continue 应让循环再跑一轮，模型应被调用两次"
        );
        std::fs::remove_file(&flag_path).ok();
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

    /// 首次流中途断开、第二次成功的 provider（流中断重试测试用）
    struct FlakyProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::TextDelta("半成品".into())),
                    Err(anyhow::anyhow!("connection reset")),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("完整回复".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn stream_interruption_retries_and_discards_partial() {
        let agent = Agent::new(Arc::new(FlakyProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut session = Session::new();
        session.push_user("hi");
        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        // 半成品文本绝不进 session；最终 assistant 消息只含完整回复
        let assistant_texts: Vec<String> = session
            .messages
            .iter()
            .filter(|m| matches!(m.role, wyj_api::types::Role::Assistant))
            .map(|m| m.text())
            .collect();
        assert_eq!(assistant_texts.len(), 1);
        assert_eq!(assistant_texts[0], "完整回复");
        assert!(!assistant_texts[0].contains("半成品"));
        // 两次 API 调用（首次失败 + 重试成功）
        assert_eq!(session.api_calls, 2);
    }

    /// 永远流中断的 provider：耗尽重试后必须报错，且不留半成品消息
    struct AlwaysBrokenProvider;
    #[async_trait::async_trait]
    impl Provider for AlwaysBrokenProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let events: Vec<Result<StreamEvent>> = vec![Err(anyhow::anyhow!("connection reset"))];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn stream_interruption_exhausts_retries_then_fails_clean() {
        let agent = Agent::new(Arc::new(AlwaysBrokenProvider));
        let mut session = Session::new();
        session.push_user("hi");
        let res = agent.run_turn(&mut session, &FakeCtx, &mut |_| {}).await;
        assert!(res.is_err());
        // 无半成品 assistant 消息残留
        assert!(!session
            .messages
            .iter()
            .any(|m| matches!(m.role, wyj_api::types::Role::Assistant)));
        // 首次 + 2 次重试 = 3 次调用
        assert_eq!(session.api_calls, 3);
    }

    /// 先吐 thinking 块（含 signature），再工具调用，再收尾的 provider
    struct ThinkingProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for ThinkingProvider {
        async fn stream(
            &self,
            _system: &str,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ThinkingStart),
                    Ok(StreamEvent::ThinkingDelta("let me ".into())),
                    Ok(StreamEvent::ThinkingDelta("think".into())),
                    Ok(StreamEvent::ThinkingSignatureDelta("sig123".into())),
                    Ok(StreamEvent::ToolUseStart {
                        id: "t1".into(),
                        name: "Sleep".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "t1".into(),
                        json_delta: r#"{"ms":1,"tag":"x"}"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ]
            } else {
                // 续轮请求：历史里必须带有完整 thinking 块（含 signature），
                // 顺序在 tool_use 之前 —— 真实 API 缺失会直接 4xx
                let assistant = messages
                    .iter()
                    .find(|m| matches!(m.role, wyj_api::types::Role::Assistant))
                    .expect("history must contain the assistant message");
                match (&assistant.content[0], &assistant.content[1]) {
                    (
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        },
                        ContentBlock::ToolUse { .. },
                    ) => {
                        assert_eq!(thinking, "let me think");
                        assert_eq!(signature, "sig123");
                    }
                    other => panic!("unexpected block order: {other:?}"),
                }
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

    #[tokio::test]
    async fn thinking_blocks_are_preserved_and_replayed_with_signature() {
        let mut agent = Agent::new(Arc::new(ThinkingProvider {
            calls: AtomicUsize::new(0),
        }))
        .with_thinking(Some(1024), true);
        agent.register_tool(Arc::new(SleepTool));
        let mut session = Session::new();
        session.push_user("go");

        let mut thinking_seen = String::new();
        let agent = agent.with_thinking_callback(move |_| {});
        agent
            .run_turn(&mut session, &FakeCtx, &mut |d| thinking_seen.push_str(d))
            .await
            .unwrap();
        // 第二轮的断言在 ThinkingProvider 内部完成；此处确认最终回复正常
        assert!(session
            .messages
            .last()
            .map(|m| m.text().contains("done"))
            .unwrap_or(false));
    }

    /// override `run_with_meta` 记录收到的 `tool_use_id`，验证 `exec_tool_call`
    /// 正确把 id 透传进 `ToolCallMeta`（供 SubAgent 落盘 trace 关联使用）。
    struct MetaCapturingTool {
        seen_tool_use_id: Arc<std::sync::Mutex<Option<String>>>,
    }
    #[async_trait::async_trait]
    impl Tool for MetaCapturingTool {
        fn name(&self) -> &str {
            "Echo"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "Echo".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<ToolResult> {
            panic!("run() 不应被直接调用，应走 run_with_meta");
        }
        async fn run_with_meta(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn ToolContext,
            meta: &crate::tool::ToolCallMeta,
        ) -> Result<ToolResult> {
            *self.seen_tool_use_id.lock().unwrap() = Some(meta.tool_use_id.clone());
            Ok(ToolResult::ok("echoed"))
        }
    }

    #[tokio::test]
    async fn exec_tool_call_passes_tool_use_id_via_meta() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut agent = Agent::new(Arc::new(EndTurnProvider));
        agent.register_tool(Arc::new(MetaCapturingTool {
            seen_tool_use_id: seen.clone(),
        }));

        let (id, _content, is_error) = agent
            .exec_tool_call(
                &FakeCtx,
                "toolu_42".into(),
                "Echo".into(),
                serde_json::json!({}),
            )
            .await;

        assert_eq!(id, "toolu_42");
        assert!(!is_error);
        assert_eq!(seen.lock().unwrap().as_deref(), Some("toolu_42"));
    }
}

fn default_system_prompt() -> String {
    // 模型侧提示词统一英文原创措辞（见 prompts.rs 模块注释），不走 i18n
    crate::prompts::MAIN.to_string()
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
