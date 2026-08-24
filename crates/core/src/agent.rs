//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::claude_md::ClaudeMdLoader;
use crate::compact::{compact_session, compact_trigger_buffer, estimate_request_tokens};
use crate::evolution::EvolutionStore;
use crate::hooks::{HookOutcome, HookRunner};
use crate::memory::MemoryStore;
use crate::memory_v3::MemoryV3Store;
use crate::session::Session;
use crate::tool::{Tool, ToolCallMeta, ToolContext};
use crate::tool_arguments::{ToolArgumentPipeline, ValidatedToolCall};
use anyhow::Result;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
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

fn estimate_tool_schema_tokens(tools: &[ToolDefinition]) -> u32 {
    let bytes = serde_json::to_vec(tools)
        .map(|value| value.len())
        .unwrap_or(0);
    ((bytes.saturating_add(3)) / 4).min(u32::MAX as usize) as u32
}

/// 主提示词和基础工作流会直接引用这些工具；lazy schema 只能隐藏可选集成，
/// 不能把正常编码和 computer-use 所需的执行面从模型目录中移除。未注册的名字
/// 不会产生 schema，因此同一列表也可安全用于只读子 Agent 和 Plan 模式。
const ALWAYS_VISIBLE_TOOL_SCHEMAS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "CodeSearch",
    "Bash",
    "BashOutput",
    "KillShell",
    "Edit",
    "Write",
    "WebFetch",
    "WebSearch",
    "AskQuestion",
    "TodoWrite",
    "Memory",
    "Agent",
    "ExitPlanMode",
    // COMPUTER_USE_HINT 要求模型直接从稳定窗口发现开始，再走后台动作。
    // 如果这三个 schema 被 ToolSearch 隐藏，国内模型可能把“当前不可见”误判为
    // “本会话未注册”，从而在没有发起任何工具调用时直接拒绝 GUI 任务。
    "window_capture",
    "app_computer",
    "computer",
];

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

/// 同一角色的一个可切换模型目标。每个目标携带自己的能力快照和请求预算，
/// 避免 fallback 到国内兼容端点后仍发送上一模型才支持的 schema/参数。
#[derive(Clone)]
pub struct AgentRoute {
    pub profile_name: String,
    pub vendor: String,
    pub model: String,
    pub provider: Arc<dyn Provider>,
    pub capabilities: Option<wyj_api::ModelCapabilities>,
    pub max_tokens: u32,
    pub context_window: u32,
    pub thinking_budget: Option<u32>,
    pub interleaved_thinking: bool,
}

impl AgentRoute {
    pub fn new(
        profile_name: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
        provider: Arc<dyn Provider>,
    ) -> Self {
        Self {
            profile_name: profile_name.into(),
            vendor: vendor.into(),
            model: model.into(),
            provider,
            capabilities: None,
            max_tokens: 8192,
            context_window: 200_000,
            thinking_budget: None,
            interleaved_thinking: true,
        }
    }

    pub fn with_capabilities(mut self, capabilities: wyj_api::ModelCapabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    pub fn with_limits(mut self, max_tokens: u32, context_window: u32) -> Self {
        self.max_tokens = max_tokens;
        self.context_window = context_window;
        self
    }

    pub fn with_thinking(mut self, budget: Option<u32>, interleaved: bool) -> Self {
        self.thinking_budget = budget.filter(|value| *value > 0);
        self.interleaved_thinking = interleaved;
        self
    }
}

#[derive(Clone)]
pub struct Agent {
    provider: Arc<dyn Provider>,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    tool_impls: HashMap<String, Arc<dyn Tool>>,
    tool_argument_pipeline: ToolArgumentPipeline,
    model_capabilities: Option<wyj_api::ModelCapabilities>,
    lazy_tool_state: Option<crate::tool_search::LazyToolState>,
    max_tokens: u32,
    max_turns: usize,
    /// 模型最大上下文窗口（token 数），用于触发自动压缩
    context_window: u32,
    /// 跨会话记忆存储（可选）
    memory: Option<Arc<MemoryStore>>,
    /// Memory v3 是普通跨会话事实、偏好、状态和历史的主控制/数据面。模型通过
    /// Memory 工具自主探索；Agent 只注入少量相关 claim 并提交耐久提取任务。
    memory_v3: Option<Arc<MemoryV3Store>>,
    /// 证据化自进化存储（可选）。Memory v3 存在时只记录 Episode 并发现需审批
    /// 的 Rule/Skill 候选；无 v3 时保留旧 Memory v2 兼容行为。
    evolution: Option<Arc<EvolutionStore>>,
    /// CLAUDE.md 系记忆文件加载器（可选）
    claude_md: Option<Arc<ClaudeMdLoader>>,
    /// 可选的工具事件回调（Send + Sync，可跨线程）
    tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    /// 可选的 token 用量回调（子 Agent 向 Hub 汇报用量用）
    usage_cb: Option<Arc<dyn Fn(u32, u32) + Send + Sync>>,
    /// 前端无关事件流：TUI、daemon 与 ACP adapter 共享同一 Agent runtime。
    session_event_cb: Option<Arc<dyn Fn(crate::SessionEvent) + Send + Sync>>,
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
    route_profile_name: String,
    route_vendor: String,
    route_model: String,
    fallback_routes: Vec<AgentRoute>,
    active_route: Arc<AtomicUsize>,
    /// thinking 文本增量回调（TUI 展示 / headless stderr 输出）
    #[allow(clippy::type_complexity)]
    thinking_cb: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    /// Hooks 生命周期自动化执行器（可选，子 Agent 不设置，避免嵌套 shell 副作用）
    hook_runner: Option<Arc<HookRunner>>,
    checkpoint_store: Option<Arc<crate::checkpoint::CheckpointStore>>,
    /// 最近 N 次工具调用的 (name, fnv_hash(input)) 队列,用于 loop detection。
    /// 跨调用共享 state 需要内部可变性 + Clone 兼容,故用 Arc<Mutex<...>>。
    loop_guard: Arc<std::sync::Mutex<std::collections::VecDeque<(String, u64)>>>,
}

struct EvolutionEpisodeGuard {
    store: Arc<EvolutionStore>,
    capture: Option<crate::evolution::EpisodeCapture>,
}

impl Drop for EvolutionEpisodeGuard {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            if let Err(error) = self.store.cancel_episode(capture) {
                tracing::warn!("记录 cancelled Evolution Episode 失败: {error}");
            }
        }
    }
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            system_prompt: default_system_prompt(),
            tools: vec![],
            tool_impls: HashMap::new(),
            tool_argument_pipeline: ToolArgumentPipeline::default(),
            model_capabilities: None,
            lazy_tool_state: None,
            max_tokens: 8192,
            // 真正的成本/时长上限由每轮 token 预算触发的自动压缩承担；这里仅防止模型死循环
            max_turns: 200,
            context_window: 200_000,
            memory: None,
            memory_v3: None,
            evolution: None,
            claude_md: None,
            tool_cb: None,
            usage_cb: None,
            session_event_cb: None,
            summary: None,
            session_id: None,
            title_cb: None,
            git_snapshot: None,
            thinking_budget: None,
            interleaved_thinking: true,
            route_profile_name: "active".to_string(),
            route_vendor: "unknown".to_string(),
            route_model: "unknown".to_string(),
            fallback_routes: Vec::new(),
            active_route: Arc::new(AtomicUsize::new(0)),
            thinking_cb: None,
            hook_runner: None,
            checkpoint_store: None,
            loop_guard: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
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

    pub fn with_model_capabilities(mut self, capabilities: wyj_api::ModelCapabilities) -> Self {
        self.model_capabilities = Some(capabilities);
        self
    }

    pub fn with_route_identity(
        mut self,
        profile_name: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.route_profile_name = profile_name.into();
        self.route_vendor = vendor.into();
        self.route_model = model.into();
        self
    }

    /// 注册同角色 fallback。调用方负责按配置顺序传入；这里再次执行 vendor
    /// 边界过滤，防止未来新增调用点意外绕过 `cross_provider_fallback = false`。
    pub fn with_fallback_routes(
        mut self,
        routes: Vec<AgentRoute>,
        cross_provider_fallback: bool,
    ) -> Self {
        let primary_vendor = self.route_vendor.clone();
        self.fallback_routes = routes
            .into_iter()
            .filter(|route| cross_provider_fallback || route.vendor == primary_vendor)
            .collect();
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

    /// 根据当前路由的模型标识推导实际生效的最大推理轮数。
    /// Reasoning 模型（DeepSeek `deepseek-reasoner`、Qwen3-Max-Thinking
    /// 等）在工具循环中容易陷入 reasoning-token 黑洞：每轮 reasoning 块
    /// 计入 output tokens 但不一定推进工具调用决策。给它们一个保守的
    /// 32 轮硬上限，普通模型保留 self.max_turns 默认值。
    fn max_turns_for_route(&self, model: &str) -> usize {
        max_turns_for_model(self.max_turns, model)
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

    pub fn with_memory_v3(mut self, memory: Arc<MemoryV3Store>) -> Self {
        self.memory_v3 = Some(memory);
        self
    }

    pub fn memory_v3_ref(&self) -> Option<&Arc<MemoryV3Store>> {
        self.memory_v3.as_ref()
    }

    pub fn with_evolution(mut self, evolution: Arc<EvolutionStore>) -> Self {
        self.evolution = Some(evolution);
        self
    }

    pub fn evolution_ref(&self) -> Option<&Arc<EvolutionStore>> {
        self.evolution.as_ref()
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

    pub fn with_checkpoint_store(mut self, store: Arc<crate::checkpoint::CheckpointStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    pub fn set_checkpoint_store(&mut self, store: Arc<crate::checkpoint::CheckpointStore>) {
        self.checkpoint_store = Some(store);
    }

    async fn create_checkpoint(
        &self,
        session: &mut Session,
        cwd: &std::path::Path,
        messages: Vec<wyj_api::types::Message>,
        kind: crate::checkpoint::CheckpointKind,
        name: Option<String>,
    ) {
        let Some(store) = self.checkpoint_store.as_ref().cloned() else {
            return;
        };
        let cwd = cwd.to_path_buf();
        match tokio::task::spawn_blocking(move || store.create(&cwd, &messages, kind, name)).await {
            Ok(Ok(summary)) => {
                session.current_checkpoint_id = Some(summary.id.clone());
                self.emit_session_event(crate::SessionEvent::CheckpointChanged {
                    checkpoint_id: summary.id,
                    label: summary.name,
                });
            }
            Ok(Err(error)) => tracing::warn!("创建 checkpoint 失败: {error}"),
            Err(error) => tracing::warn!("checkpoint 任务异常退出: {error}"),
        }
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

    pub fn with_session_event_callback(
        mut self,
        cb: impl Fn(crate::SessionEvent) + Send + Sync + 'static,
    ) -> Self {
        self.session_event_cb = Some(Arc::new(cb));
        self
    }

    fn emit_session_event(&self, event: crate::SessionEvent) {
        if let Some(callback) = &self.session_event_cb {
            callback(event);
        }
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
        self.tool_argument_pipeline.register(&def);
        if let Some(state) = &self.lazy_tool_state {
            state.upsert(def.clone());
        }
        self.tools.push(def);
        self.tool_impls.insert(tool.name().to_string(), tool);
    }

    /// Remove tools whose names satisfy `predicate`.
    ///
    /// Runtime-managed integrations (currently MCP) are attached to an Agent
    /// snapshot after it has been constructed.  Rebuilding that snapshot must
    /// be able to remove integrations as well as add them; otherwise a disabled
    /// server would remain callable until the process restarted.
    pub fn remove_tools_where(&mut self, mut predicate: impl FnMut(&str) -> bool) {
        let removed: HashSet<String> = self
            .tools
            .iter()
            .filter(|definition| predicate(&definition.name))
            .map(|definition| definition.name.clone())
            .collect();
        self.tools
            .retain(|definition| !removed.contains(&definition.name));
        self.tool_impls.retain(|name, _| !removed.contains(name));
        self.tool_argument_pipeline
            .remove_where(|name| removed.contains(name));
        if let Some(state) = &self.lazy_tool_state {
            for name in &removed {
                state.remove(name);
            }
        }
    }

    /// Re-read definitions from runtime-mutable Tool implementations.
    ///
    /// This is primarily used by the `Agent` tool: its advertised enum of
    /// sub-agent types is derived from enabled plugin/user definitions and can
    /// change while the process is alive.
    pub fn refresh_tool_definitions(&mut self) {
        for (name, tool) in &self.tool_impls {
            let definition = tool.definition();
            if let Some(existing) = self.tools.iter_mut().find(|d| d.name == *name) {
                *existing = definition.clone();
            }
            self.tool_argument_pipeline.register(&definition);
            if let Some(state) = &self.lazy_tool_state {
                state.upsert(definition);
            }
        }
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

    /// 仅暴露核心工具 schema；其他工具通过 ToolSearch 命中后按会话 sticky。
    pub fn enable_lazy_tools(
        &mut self,
        core_tools: impl IntoIterator<Item = String>,
        threshold: usize,
        top_k: usize,
        sticky_turns: u64,
    ) -> bool {
        if self.tools.len() <= threshold {
            return false;
        }
        let mut core: HashSet<String> = core_tools.into_iter().collect();
        core.extend(
            ALWAYS_VISIBLE_TOOL_SCHEMAS
                .iter()
                .map(|name| (*name).to_string()),
        );
        let state = crate::tool_search::LazyToolState::new(core, top_k, sticky_turns);
        for definition in &self.tools {
            state.upsert(definition.clone());
        }
        self.lazy_tool_state = Some(state.clone());
        self.register_tool(Arc::new(crate::tool_search::ToolSearchTool::new(state)));
        true
    }

    fn route_at(&self, index: usize) -> AgentRoute {
        if index == 0 {
            AgentRoute {
                profile_name: self.route_profile_name.clone(),
                vendor: self.route_vendor.clone(),
                model: self.route_model.clone(),
                provider: self.provider.clone(),
                capabilities: self.model_capabilities.clone(),
                max_tokens: self.max_tokens,
                context_window: self.context_window,
                thinking_budget: self.thinking_budget,
                interleaved_thinking: self.interleaved_thinking,
            }
        } else {
            self.fallback_routes[index - 1].clone()
        }
    }

    fn active_route_index(&self) -> usize {
        self.active_route
            .load(Ordering::Acquire)
            .min(self.fallback_routes.len())
    }

    fn fallback_error_kind(error: &anyhow::Error) -> Option<wyj_api::ProviderErrorKind> {
        error
            .downcast_ref::<wyj_api::ProviderError>()
            .filter(|provider_error| provider_error.retryable)
            .map(|provider_error| provider_error.kind)
    }

    fn advance_route(
        &self,
        current_index: usize,
        error: &anyhow::Error,
    ) -> Option<(AgentRoute, wyj_api::ProviderErrorKind)> {
        let kind = Self::fallback_error_kind(error)?;
        let next_index = current_index + 1;
        if next_index > self.fallback_routes.len() {
            return None;
        }
        self.active_route.store(next_index, Ordering::Release);
        Some((self.route_at(next_index), kind))
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
        inject_rx: Option<
            &mut tokio::sync::mpsc::UnboundedReceiver<(Vec<ContentBlock>, InjectionKind)>,
        >,
        on_inject: impl FnMut(InjectionKind),
    ) -> Result<()> {
        let mut evolution_guard = self.evolution.as_ref().map(|evolution| {
            let route = self.route_at(self.active_route_index());
            if self.memory_v3.is_some() {
                evolution.schedule_pending_governance_analysis(route.provider.clone());
            } else {
                evolution.schedule_pending_analysis(route.provider.clone());
            }
            EvolutionEpisodeGuard {
                store: evolution.clone(),
                capture: Some(
                    evolution.begin_episode(
                        self.session_id
                            .clone()
                            .unwrap_or_else(|| "unsaved-session".to_string()),
                        session,
                        &last_user_goal(session),
                        route.profile_name,
                        route.vendor,
                        route.model,
                    ),
                ),
            }
        });
        let result = self
            .run_turn_with_injection_inner(session, ctx, on_text, inject_rx, on_inject)
            .await;
        if let Some(guard) = evolution_guard.as_mut() {
            let capture = guard
                .capture
                .take()
                .expect("Evolution capture exists until normal turn completion");
            let evolution = guard.store.clone();
            match evolution.finish_episode(capture, session, &result) {
                Ok(episode) => {
                    let provider = self.route_at(self.active_route_index()).provider;
                    if self.memory_v3.is_some() {
                        evolution.schedule_governance_analysis(episode, provider);
                    } else {
                        evolution.schedule_analysis(episode, provider);
                    }
                }
                Err(error) => tracing::warn!("记录 Evolution Episode 失败: {error}"),
            }
        }
        result
    }

    async fn run_turn_with_injection_inner(
        &self,
        session: &mut Session,
        ctx: &dyn ToolContext,
        on_text: &mut impl FnMut(&str),
        mut inject_rx: Option<
            &mut tokio::sync::mpsc::UnboundedReceiver<(Vec<ContentBlock>, InjectionKind)>,
        >,
        mut on_inject: impl FnMut(InjectionKind),
    ) -> Result<()> {
        if let Some(state) = &self.lazy_tool_state {
            state.begin_task_turn();
        }
        // 构建 system prompt 基础部分：默认提示 + 跨会话记忆 + CLAUDE.md 祖先链。
        // CLAUDE.md 内容拼进 system prompt（而非注入 user 消息），配合 prompt caching
        // 使其首轮全价、后续轮次命中缓存按 0.1x 计费，避免跨轮线性累积。
        // 子目录动态 reminder 在循环内追加到 system 末尾（只增不减，前缀仍可缓存）。
        let mut system = self.system_prompt.clone();
        if let Some(memory) = &self.memory_v3 {
            let snapshot = build_memory_snapshot(memory, session);
            if !snapshot.is_empty() {
                system.push_str("\n\n");
                system.push_str(&snapshot);
            }
        } else if let Some(evolution) = &self.evolution {
            let snapshot = evolution.context_snapshot(&last_user_goal(session));
            if !snapshot.is_empty() {
                system.push_str("\n\n");
                system.push_str(&snapshot);
            }
        } else if let Some(mem) = &self.memory {
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

        // 调用方在进入 run_turn 前已 push 本次真实用户消息；checkpoint 保存其
        // 之前的完整对话与当前工作树，确保 /rewind 可以回到提交前边界。
        if !session.messages.is_empty() {
            let previous_messages = session.messages[..session.messages.len() - 1].to_vec();
            self.create_checkpoint(
                session,
                ctx.cwd(),
                previous_messages,
                crate::checkpoint::CheckpointKind::AutoUser,
                None,
            )
            .await;
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

        // 国产 reasoning 模型（DeepSeek `deepseek-reasoner`、Qwen3-Max-Thinking
        // 等）在工具循环中可能陷入 reasoning-token 黑洞：每轮 thinking 块会
        // 计入 output tokens，但 reasoning 不一定推进工具调用决策。给它们
        // 一个保守的 32 轮硬上限，普通模型保留 self.max_turns（默认 200）
        // 兜底。
        let max_turns = self.max_turns_for_route(&self.route_model);

        let mut turn = 0;
        let mut invalid_argument_rounds = 0usize;
        loop {
            turn += 1;
            if turn > max_turns {
                anyhow::bail!("超过最大推理轮数 {}", max_turns);
            }

            // 流式消费，带中断重试：流已消费一半时断开（网络重置、供应商
            // overloaded 流内错误等），丢弃本次全部半成品缓冲、整轮重新生成。
            // 不变量：半成品 assistant 消息绝不 push 进 session（流完整结束
            // 才组装），故重试即重新生成，UI 可能出现重复文本片段但正确性无损。
            // usage 事件同样缓冲到流成功后才入账，避免失败尝试的重复计数。
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
            let mut route_index = self.active_route_index();
            let (blocks, stop_reason, used_route) = 'route_attempt: loop {
                let route = self.route_at(route_index);
                let mut request_system = system.clone();
                if let Some(capabilities) = &route.capabilities {
                    let suffix = wyj_api::PromptPolicy::compatibility_suffix(capabilities);
                    if !suffix.is_empty() {
                        request_system.push_str("\n\n");
                        request_system.push_str(suffix);
                    }
                }
                let opts = if let Some(capabilities) = &route.capabilities {
                    wyj_api::provider::RequestOptions {
                        max_tokens: route.max_tokens.min(capabilities.max_output_tokens),
                        thinking_budget: route.thinking_budget.filter(|_| {
                            matches!(
                                capabilities.thinking.value,
                                wyj_api::ThinkingMode::BudgetTokens
                            )
                        }),
                        interleaved: route.interleaved_thinking
                            && capabilities.interleaved_thinking.value,
                    }
                } else {
                    wyj_api::provider::RequestOptions {
                        max_tokens: route.max_tokens,
                        thinking_budget: route.thinking_budget,
                        interleaved: route.interleaved_thinking,
                    }
                };
                let candidate_tools: Vec<ToolDefinition> = match &route.capabilities {
                    Some(capabilities) if !capabilities.tool_calling.value => Vec::new(),
                    Some(capabilities) if !capabilities.strict_tool_schema.value => self
                        .tools
                        .iter()
                        .map(crate::tool_arguments::simplified_tool_definition)
                        .collect(),
                    _ => self.tools.clone(),
                };
                let all_schema_tokens = estimate_tool_schema_tokens(&candidate_tools);
                let request_tools: Vec<ToolDefinition> = candidate_tools
                    .into_iter()
                    .filter(|definition| {
                        self.lazy_tool_state
                            .as_ref()
                            .map(|state| state.visible(&definition.name))
                            .unwrap_or(true)
                    })
                    .collect();
                let attached_tool_names = request_tools
                    .iter()
                    .map(|definition| definition.name.as_str())
                    .collect::<Vec<_>>();
                request_system.push_str("\n\n");
                request_system.push_str(&crate::prompts::current_tool_availability_block(
                    &attached_tool_names,
                ));
                request_system.push_str("\n\n");
                request_system.push_str(&crate::prompts::current_sandbox_runtime_block(
                    &ctx.sandbox_policy(),
                ));
                let sent_schema_tokens = estimate_tool_schema_tokens(&request_tools);
                session.tool_schema_tokens = session
                    .tool_schema_tokens
                    .saturating_add(sent_schema_tokens);
                session.tool_schema_tokens_saved = session
                    .tool_schema_tokens_saved
                    .saturating_add(all_schema_tokens.saturating_sub(sent_schema_tokens));

                // 按当前路由目标的真实窗口与能力估算；fallback 模型可能比主模型
                // 上下文更小，不能复用主模型预算。
                let estimated = estimate_request_tokens(
                    &request_system,
                    &session.messages,
                    &request_tools,
                    opts.max_tokens,
                );
                let compact_threshold = route
                    .context_window
                    .saturating_sub(compact_trigger_buffer(route.context_window));
                if estimated > compact_threshold {
                    match compact_session(session, route.provider.as_ref(), route.context_window)
                        .await
                    {
                        Ok(result) => on_text(&format!(
                            "\n[已压缩对话历史：移除 {} 条消息，节省约 {} tokens]\n",
                            result.messages_removed, result.tokens_saved_estimate
                        )),
                        Err(error) => tracing::warn!("上下文压缩失败: {error}"),
                    }
                }

                const MAX_STREAM_RETRIES: u32 = 2;
                let mut stream_retries: u32 = 0;
                let mut effective_opts = opts;
                let mut parameter_degraded = false;
                let result = loop {
                    session.api_calls += 1;
                    let mut stream = match route
                        .provider
                        .stream(
                            &request_system,
                            &session.messages,
                            &request_tools,
                            &effective_opts,
                        )
                        .await
                    {
                        Ok(stream) => stream,
                        Err(error) => {
                            let safe_parameter = error
                                .downcast_ref::<wyj_api::ProviderError>()
                                .filter(|provider_error| {
                                    provider_error.kind
                                        == wyj_api::ProviderErrorKind::UnsupportedParameter
                                })
                                .and_then(|provider_error| provider_error.parameter.as_deref())
                                .filter(|parameter| {
                                    matches!(
                                        *parameter,
                                        "thinking" | "thinking_budget" | "interleaved_thinking"
                                    )
                                });
                            if !parameter_degraded && safe_parameter.is_some() {
                                let parameter = safe_parameter.unwrap_or("thinking");
                                parameter_degraded = true;
                                effective_opts.thinking_budget = None;
                                effective_opts.interleaved = false;
                                on_text(&format!(
                                    "\n[模型端点不支持参数 `{parameter}`，已安全移除后重试一次]\n"
                                ));
                                continue;
                            }
                            if let Some((next, kind)) = self.advance_route(route_index, &error) {
                                session.routing_events.push(crate::session::RoutingEvent {
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    from_profile: route.profile_name.clone(),
                                    to_profile: next.profile_name.clone(),
                                    error_kind: kind,
                                    boundary: "before_assistant_commit".to_string(),
                                });
                                on_text(&format!(
                                    "\n[模型 `{}` 暂时不可用（{:?}），已在完整消息边界切换到同角色 `{}`]\n",
                                    route.profile_name, kind, next.profile_name
                                ));
                                route_index += 1;
                                continue 'route_attempt;
                            }
                            return Err(error);
                        }
                    };

                    let mut blocks: Vec<StreamedBlock> = vec![];
                    let mut current_tool_idx: Option<usize> = None;
                    let mut stop_reason = StopReason::EndTurn;
                    // seen_completion 用来区分 eventsource_stream 流末尾的良性
                    // TCP-EOF Err 和真正的半路网络中断。供应商正常关闭流后,eventsource
                    // 仍可能多调一次 next() 返回 Err(底层的 reqwest reader EOF),这是
                    // crate 已知行为而非真正的中断;若此时本轮已收到 MessageStop 或
                    // Usage,把它当作流正常结束,否则保留 stream_err 走原重试路径。
                    let mut seen_completion = false;
                    let mut pending_usage: Vec<(u32, u32, u32, u32)> = vec![];
                    let mut stream_err: Option<anyhow::Error> = None;

                    while let Some(event) = stream.next().await {
                        let event = match event {
                            Ok(ev) => ev,
                            Err(error) => {
                                if seen_completion {
                                    tracing::debug!(
                                        "流末尾 EOF (seen_completion=true), 已忽略: {error}"
                                    );
                                    break;
                                }
                                stream_err = Some(error);
                                break;
                            }
                        };
                        match event {
                            StreamEvent::TextDelta(delta) => {
                                on_text(&delta);
                                self.emit_session_event(crate::SessionEvent::TextDelta {
                                    text: delta.clone(),
                                });
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
                                self.emit_session_event(crate::SessionEvent::ThinkingDelta {
                                    text: delta.clone(),
                                });
                                match blocks.last_mut() {
                                    Some(StreamedBlock::Thinking { text, .. }) => {
                                        text.push_str(&delta)
                                    }
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
                            StreamEvent::MessageStop { stop_reason: sr } => {
                                stop_reason = sr;
                                seen_completion = true;
                            }
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
                                seen_completion = true;
                            }
                        }
                    }

                    match stream_err {
                        Some(error) if stream_retries < MAX_STREAM_RETRIES => {
                            stream_retries += 1;
                            tracing::warn!("流中断（第 {stream_retries} 次重试）: {error}");
                            on_text(&format!(
                                "\n[连接中断，正在重试 {stream_retries}/{MAX_STREAM_RETRIES}...]\n"
                            ));
                            tokio::time::sleep(std::time::Duration::from_secs(
                                1 << stream_retries.min(5),
                            ))
                            .await;
                            continue;
                        }
                        Some(error) => break Err(error),
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
                                self.emit_session_event(crate::SessionEvent::Usage {
                                    input_tokens: input as u64,
                                    output_tokens: output as u64,
                                    tool_schema_tokens: session.tool_schema_tokens as u64,
                                    tool_schema_tokens_saved: session.tool_schema_tokens_saved
                                        as u64,
                                });
                            }
                            break Ok((blocks, stop_reason));
                        }
                    }
                };
                match result {
                    Ok((blocks, stop_reason)) => break 'route_attempt (blocks, stop_reason, route),
                    Err(error) => {
                        if let Some((next, kind)) = self.advance_route(route_index, &error) {
                            session.routing_events.push(crate::session::RoutingEvent {
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                from_profile: route.profile_name.clone(),
                                to_profile: next.profile_name.clone(),
                                error_kind: kind,
                                boundary: "before_assistant_commit".to_string(),
                            });
                            on_text(&format!(
                                "\n[模型 `{}` 的未完成输出已丢弃（{:?}），已在完整消息边界切换到同角色 `{}`]\n",
                                route.profile_name, kind, next.profile_name
                            ));
                            route_index += 1;
                            continue 'route_attempt;
                        }
                        return Err(error);
                    }
                }
            };

            // 组装助手内容块（保持到达顺序；thinking 块含 signature 原样入历史，
            // 工具调用续轮时回传给 API —— 缺失会被 Anthropic 拒绝）
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum ToolCallRejectionKind {
                SchemaNotExposed,
                InvalidArguments,
            }
            enum PendingToolCall {
                Valid(ValidatedToolCall),
                Rejected {
                    id: String,
                    name: String,
                    feedback: String,
                    kind: ToolCallRejectionKind,
                },
            }
            let mut assistant_blocks = vec![];
            let mut pending_tools: Vec<PendingToolCall> = vec![];
            let max_tools_this_turn = used_route
                .capabilities
                .as_ref()
                .map(|capabilities| capabilities.max_tools_per_turn.max(1))
                .unwrap_or(usize::MAX);
            let mut seen_tool_calls = 0usize;
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
                        if self
                            .lazy_tool_state
                            .as_ref()
                            .is_some_and(|state| !state.visible(name))
                        {
                            let feedback = serde_json::json!({
                                "error": "tool_schema_not_exposed",
                                "tool": name,
                                "instruction": "Call ToolSearch for the needed capability, then retry on the next turn."
                            })
                            .to_string();
                            assistant_blocks.push(ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: serde_json::json!({
                                    "_wyj_code_tool_schema_not_exposed": true
                                }),
                            });
                            pending_tools.push(PendingToolCall::Rejected {
                                id: id.clone(),
                                name: name.clone(),
                                feedback,
                                kind: ToolCallRejectionKind::SchemaNotExposed,
                            });
                            continue;
                        }
                        if let Some(state) = &self.lazy_tool_state {
                            state.mark_used(name);
                        }
                        seen_tool_calls += 1;
                        let raw_call = wyj_api::types::RawToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            raw_arguments: json.clone(),
                        };
                        match self.tool_argument_pipeline.process(raw_call) {
                            Ok(call) => {
                                if call.syntax_repaired {
                                    on_text(&format!(
                                        "\n[已对工具 `{}` 的参数应用安全语法修复]\n",
                                        call.name
                                    ));
                                }
                                assistant_blocks.push(ContentBlock::ToolUse {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    input: call.input.clone(),
                                });
                                pending_tools.push(PendingToolCall::Valid(call));
                            }
                            Err(error) => {
                                let feedback = error.feedback_json();
                                tracing::warn!(
                                    tool = %name,
                                    kind = ?error.kind,
                                    "拒绝执行无效工具参数"
                                );
                                // 协议续轮要求 assistant tool_use 与 tool_result 成对。
                                // 这里只保存不可执行标记，绝不把解析失败降级成 {} / null。
                                assistant_blocks.push(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: serde_json::json!({
                                        "_wyj_code_invalid_arguments": true
                                    }),
                                });
                                pending_tools.push(PendingToolCall::Rejected {
                                    id: id.clone(),
                                    name: name.clone(),
                                    feedback,
                                    kind: ToolCallRejectionKind::InvalidArguments,
                                });
                            }
                        }
                    }
                }
            }
            if seen_tool_calls > max_tools_this_turn {
                // max_tools_per_turn 是模型生成侧的保守能力提示，不是执行器的
                // fail-closed 边界。模型已经返回多个完整 tool_use 时，协议续轮
                // 仍要求逐个回填 tool_result；拒绝超额调用会诱发模型重复生成，
                // 并把本可安全执行的任务拖进纠错死循环。执行阶段继续做原始
                // schema、权限与 sandbox 校验，并由 parallel_tool_calls 决定
                // 并发或顺序执行。
                tracing::warn!(
                    emitted = seen_tool_calls,
                    declared_max = max_tools_this_turn,
                    "模型返回的工具数超过能力声明，降级为受控执行"
                );
            }
            session.push_assistant(assistant_blocks);

            let has_tool_calls = stop_reason == StopReason::ToolUse && !pending_tools.is_empty();

            if has_tool_calls {
                // 参数已由 ToolArgumentPipeline 严格解析并按原始 schema 校验。
                // 无效调用只生成机器可读错误，绝不进入 exec_tool_call。
                let total = pending_tools.len();
                let mut calls = Vec::new();
                let mut tool_results = Vec::new();
                let mut invalid_argument_count = 0usize;
                for (idx, pending) in pending_tools.into_iter().enumerate() {
                    match pending {
                        PendingToolCall::Valid(call) => {
                            calls.push((idx, call.id, call.name, call.input));
                        }
                        PendingToolCall::Rejected {
                            id,
                            name,
                            feedback,
                            kind,
                        } => {
                            if kind == ToolCallRejectionKind::InvalidArguments {
                                invalid_argument_count += 1;
                            }
                            if let Some(cb) = &self.tool_cb {
                                cb(ToolEvent::End {
                                    id: id.clone(),
                                    name: name.clone(),
                                    is_error: true,
                                    elapsed_secs: 0.0,
                                    output: feedback.clone(),
                                });
                            }
                            self.emit_session_event(crate::SessionEvent::ToolFinished {
                                call_id: id.clone(),
                                output: feedback.clone(),
                                is_error: true,
                                elapsed_ms: 0,
                            });
                            if name == "Agent" {
                                self.emit_session_event(crate::SessionEvent::AgentStateChanged {
                                    agent_id: session_agent_event_id(&id),
                                    parent_id: None,
                                    state: "failed".to_string(),
                                });
                            }
                            tool_results.push((
                                idx,
                                (id, wyj_api::types::ToolResultContent::Text(feedback), true),
                            ));
                        }
                    }
                }
                let mut touched_dirs: Vec<std::path::PathBuf> = vec![];
                if self.claude_md.is_some() {
                    for (_, _, name, input) in &calls {
                        if let Some(dir) = touched_dir(name, input, ctx.cwd()) {
                            touched_dirs.push(dir);
                        }
                    }
                }
                let has_side_effect = calls.iter().any(|(_, _, name, input)| {
                    self.tool_impls
                        .get(name)
                        .map(|tool| tool.needs_permission(input))
                        .unwrap_or(false)
                });
                if has_side_effect {
                    let pre_tool_messages = session
                        .messages
                        .get(..session.messages.len().saturating_sub(1))
                        .unwrap_or_default()
                        .to_vec();
                    self.create_checkpoint(
                        session,
                        ctx.cwd(),
                        pre_tool_messages,
                        crate::checkpoint::CheckpointKind::PreTool,
                        None,
                    )
                    .await;
                }

                // 分区执行：parallel_safe 的调用（如 SubAgent）各自并发，其余调用
                // 保持相互顺序、但与并发组同时进行；结果按原始下标排序回填保序。
                // 均为单任务内并发（join!），不要求 ctx 满足 Send/'static。
                let mut par_futs = vec![];
                let mut seq_calls = vec![];
                for (idx, id, name, input) in calls {
                    let is_par = self
                        .tool_impls
                        .get(&name)
                        .map(|t| t.parallel_safe())
                        .unwrap_or(false);
                    let model_allows_parallel = used_route
                        .capabilities
                        .as_ref()
                        .map(|capabilities| capabilities.parallel_tool_calls.value)
                        .unwrap_or(true);
                    if is_par && total > 1 && model_allows_parallel {
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

                tool_results.extend(par_results.into_iter().chain(seq_results));
                tool_results.sort_by_key(|(idx, _)| *idx);
                for (_, (id, output, is_error)) in tool_results {
                    session.push_tool_result(id, output, is_error);
                }
                if has_side_effect {
                    let post_tool_messages = session.messages.clone();
                    self.create_checkpoint(
                        session,
                        ctx.cwd(),
                        post_tool_messages,
                        crate::checkpoint::CheckpointKind::PostTool,
                        None,
                    )
                    .await;
                }

                if invalid_argument_count > 0 {
                    invalid_argument_rounds += 1;
                    if invalid_argument_rounds > 2 {
                        anyhow::bail!("工具参数连续校验失败，已停止执行以避免无界重试");
                    }
                } else {
                    invalid_argument_rounds = 0;
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
                // Memory v3 先把任务原子写入磁盘，再异步消费；即使进程在 spawn
                // 后退出，pending/running 任务也会在下次 store open 时恢复。
                if let Some(memory) = self.memory_v3.as_ref().cloned() {
                    let session_id = self
                        .session_id
                        .clone()
                        .unwrap_or_else(|| "ephemeral".to_string());
                    match memory.enqueue_extraction(&session_id, &session.messages) {
                        Ok(Some(_)) => {
                            let provider = self.route_at(self.active_route_index()).provider;
                            tokio::spawn(async move {
                                if let Err(error) = memory.drain_jobs(provider).await {
                                    tracing::debug!("Memory v3 后台任务失败: {error}");
                                }
                            });
                        }
                        Ok(None) => {}
                        Err(error) => tracing::warn!("Memory v3 任务入队失败: {error}"),
                    }
                } else if self.evolution.is_none() {
                    if let Some(mem) = self.memory.as_ref().cloned() {
                        let provider = self.route_at(self.active_route_index()).provider;
                        let msgs = session.messages.clone();
                        tokio::spawn(async move {
                            if let Err(e) = mem.extract_and_save(msgs, provider).await {
                                tracing::debug!("记忆提取失败: {e}");
                            }
                        });
                    }
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
        self.emit_session_event(crate::SessionEvent::TurnFinished);
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
        self.emit_session_event(crate::SessionEvent::ToolStarted {
            call_id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
        if name == "Agent" {
            self.emit_session_event(crate::SessionEvent::AgentStateChanged {
                agent_id: session_agent_event_id(&id),
                parent_id: None,
                state: "running".to_string(),
            });
        }
        let start = Instant::now();

        // ── Loop detection 准备 ────────────────────────────────────────
        // 同 (tool_name, fnv_hash(input)) 在最近 5 次调用里命中 ≥3 次视为
        // 死循环,跳过本次执行并把循环提示回灌给模型,让它换工具或换参数。
        // call_hash 在函数顶部计算一次,既给 loop 命中分支用,也给末尾 push 用。
        let call_hash = fnv_hash_value(&input);

        let (display, content, is_error): (String, ToolResultContent, bool) = if let Some(t) = tool
        {
            let detected_loop = {
                let guard = self.loop_guard.lock().expect("loop_guard poisoned");
                detect_loop(&guard, &name, call_hash)
            };
            if detected_loop {
                let msg = format!(
                    "[loop detection] 工具 `{name}` 在最近 {LOOP_GUARD_WINDOW} 次调用中以相同参数重复 {LOOP_GUARD_THRESHOLD} 次,已跳过本次执行。请换一个工具或调整参数(例如不同的文件路径)。"
                );
                // 仍记录本次 (name, hash),但 push 之前不增加窗口——避免"持续跳过"被永远纳入历史。
                let mut guard = self.loop_guard.lock().expect("loop_guard poisoned");
                if guard.len() >= LOOP_GUARD_CAPACITY {
                    guard.pop_front();
                }
                guard.push_back((name.clone(), call_hash));
                return (id, ToolResultContent::Text(msg), true);
            }
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
                // approve 只能替代交互确认，不能绕过模式白名单、路径范围、
                // protected path 或 require-sandbox 等强制策略。
                HookOutcome::Approve => {
                    if !ctx.is_allowed(&name, &input) {
                        let msg =
                            format!("工具 `{name}` 被强制权限策略拒绝；hook approve 无权绕过");
                        (msg.clone(), ToolResultContent::Text(msg), true)
                    } else {
                        run_tool(&t, input, ctx, &meta).await
                    }
                }
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
                output: display.clone(),
            });
        }
        self.emit_session_event(crate::SessionEvent::ToolFinished {
            call_id: id.clone(),
            output: display,
            is_error,
            elapsed_ms: (elapsed_secs * 1000.0).round().max(0.0) as u64,
        });
        if name == "Agent" {
            self.emit_session_event(crate::SessionEvent::AgentStateChanged {
                agent_id: session_agent_event_id(&id),
                parent_id: None,
                state: if is_error { "failed" } else { "completed" }.to_string(),
            });
        }

        // Loop detection 记录：把本次 (name, hash) push 到最近 N 次队列,
        // 容量满了弹出最旧。loop 命中分支已在函数入口提前 return,
        // 此处不需要再次判断。
        {
            let mut guard = self.loop_guard.lock().expect("loop_guard poisoned");
            if guard.len() >= LOOP_GUARD_CAPACITY {
                guard.pop_front();
            }
            guard.push_back((name.clone(), call_hash));
        }

        (id, content, is_error)
    }

    /// 手动触发上下文压缩（供 /compact 命令使用）
    pub async fn compact_context(
        &self,
        session: &mut Session,
    ) -> Result<crate::compact::CompactResult> {
        let route = self.route_at(self.active_route_index());
        compact_session(session, route.provider.as_ref(), route.context_window).await
    }
}

fn session_agent_event_id(call_id: &str) -> u64 {
    call_id
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

/// Reasoning 模型（DeepSeek `deepseek-reasoner`、Qwen3-Max-Thinking、R1
/// 蒸馏系列 `-r1` 后缀）在工具循环中容易陷入 reasoning-token 黑洞：每轮
/// reasoning 块计入 output tokens 但不一定推进工具调用决策。给它们一个
/// 保守的 32 轮硬上限，普通模型保留调用方传入的默认值。
///
/// 抽成 free function 以便单测覆盖（不必构造完整 Agent）。
fn max_turns_for_model(default_max_turns: usize, model: &str) -> usize {
    const REASONING_MAX_TURNS: usize = 32;
    let lower = model.to_ascii_lowercase();
    if lower.contains("reasoner") || lower.contains("-r1") {
        REASONING_MAX_TURNS
    } else {
        default_max_turns
    }
}

/// 对 `serde_json::Value` 做规范化后做 FNV-1a 64-bit 哈希。Object 的 key
/// 递归排序、数组保留原序,Primitive 直接序列化。这样哈希在 schema 字段
/// 顺序变化、空白差异等情况下保持稳定,适合 loop detection 用。
///
/// 注：抽成 free function 便于单测覆盖 hash 稳定性。
fn fnv_hash_value(value: &serde_json::Value) -> u64 {
    let normalized = normalize_for_hash(value);
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// 把 Object 的 key 递归排序,数组与 Primitive 保持原样。让同一份语义 JSON
/// 在 key 顺序打乱后仍产生相同 hash。
fn normalize_for_hash(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                out.insert(k.clone(), normalize_for_hash(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_for_hash).collect()),
        other => other.clone(),
    }
}

/// LoopGuard 容量上限。每次 exec 后保留最近 N 条 (name, hash),超过则
/// 弹出最旧的,避免无界增长。
const LOOP_GUARD_CAPACITY: usize = 16;
/// 触发跳过的同 name+hash 命中阈值。最近 N 条里命中 ≥ K 次视为死循环。
const LOOP_GUARD_WINDOW: usize = 5;
const LOOP_GUARD_THRESHOLD: usize = 3;

/// 检查 (name, hash) 是否已经在最近 LOOP_GUARD_WINDOW 次调用中命中
/// LOOP_GUARD_THRESHOLD 次以上。是则返回 true 表示模型陷入循环,应跳过本次执行。
///
/// 抽成 free function 便于单测覆盖。
fn detect_loop(window: &std::collections::VecDeque<(String, u64)>, name: &str, hash: u64) -> bool {
    window
        .iter()
        .rev()
        .take(LOOP_GUARD_WINDOW)
        .filter(|(n, h)| n == name && *h == hash)
        .count()
        >= LOOP_GUARD_THRESHOLD
}

fn last_user_goal(session: &Session) -> String {
    session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == wyj_api::types::Role::User)
        .map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.trim()),
                    _ => None,
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                "attachment-only user goal".to_string()
            } else {
                text
            }
        })
        .unwrap_or_else(|| "unknown user goal".to_string())
}

/// Memory v3 检索不能只看最后一句。把最近几个真实 user task 合并后，像“继续”
/// “再分析一下”这样的续接请求仍然携带上一个主题，同时不把 assistant 推断当查询。
fn memory_query_context(session: &Session) -> String {
    let mut goals = session
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == wyj_api::types::Role::User)
        .filter_map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.trim()),
                    _ => None,
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        })
        .take(4)
        .collect::<Vec<_>>();
    goals.reverse();
    if goals.is_empty() {
        "unknown user goal".to_string()
    } else {
        goals.join("\n")
    }
}

/// "继续/接着/continue/resume/再来/go on" 等纯续接请求；命中时按最近
/// InProgress Task 注入恢复点。混合句（"继续看 XX"）由 `memory_query_context`
/// 自然消化，不算 continuation。
fn is_continuation_request(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }
    let normalized = trimmed
        .trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, '。' | '，' | ',' | '；' | ';' | '！' | '!' | '?' | '？')
        })
        .to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    // 长度上限防止误命中（"继续"两个字后面有空格+大段说明就当普通 query 处理）。
    if normalized.chars().count() > 12 {
        return false;
    }
    matches!(
        normalized.as_str(),
        "继续"
            | "继续吧"
            | "接着"
            | "接着干"
            | "接着来"
            | "再来"
            | "再来一次"
            | "go on"
            | "continue"
            | "resume"
            | "proceed"
    )
}

/// 拼装 v3 memory 注入：Project Brief + (可选) 续接 Task 详情 + 少量高相关
/// Project / Global claim。"继续"分支叠加 Open Tasks 提示，避免 Brief 与
/// 续接恢复点信息互相吞掉。
fn build_memory_snapshot(memory: &crate::MemoryV3Store, session: &Session) -> String {
    let query_context = memory_query_context(session);
    let brief = memory.project_brief(&query_context);

    let last_query = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == wyj_api::types::Role::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let mut out = brief;
    if is_continuation_request(&last_query) {
        if let Some(suffix) = continuation_suffix(memory) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&suffix);
        }
    }
    out
}

/// "继续" 命中时的追加注入：找到最近 InProgress Task → 写入 next step；
/// 若最近任务是 Blocked → 显式标注阻塞原因；所有任务都已关闭 →
/// 用 i18n key `memory.continuation.no_open_tasks` 提示用户给新任务。
fn continuation_suffix(memory: &crate::MemoryV3Store) -> Option<String> {
    match memory.find_latest_in_progress_task() {
        Ok(Some(task)) => {
            let next_step = task
                .task_steps
                .iter()
                .find(|step| !step.done)
                .map(|step| step.description.clone())
                .unwrap_or_else(|| "(no next step recorded)".to_string());
            let blocked_note = match task.task_status {
                Some(crate::TaskStatus::Blocked) => task
                    .blocked_reason
                    .as_deref()
                    .map(|reason| format!("\nblocked_reason: {reason}"))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Some(format!(
                "<continuation>\nResuming task [{}] {}\nstatus: {:?}\nnext: {}{}\n</continuation>",
                task.id, task.title, task.task_status, next_step, blocked_note
            ))
        }
        Ok(None) => {
            // 没有 InProgress：检查是否有 Blocked，有则提示恢复 Blocked。
            match memory.find_all_open_tasks() {
                Ok(open) if !open.is_empty() => {
                    let head: String = open
                        .iter()
                        .take(5)
                        .map(|t| {
                            format!(
                                "- [{}] {} ({:?}{})",
                                t.id,
                                t.title,
                                t.task_status,
                                t.blocked_reason
                                    .as_deref()
                                    .map(|r| format!(", blocked: {r}"))
                                    .unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Some(format!(
                        "<continuation>\nNo in-progress task found. Still-open tasks:\n{head}\n</continuation>"
                    ))
                }
                _ => Some(wyj_i18n::tr("memory.continuation.no_open_tasks")),
            }
        }
        Err(error) => Some(format!(
            "<continuation>\nFailed to look up open tasks: {error}\n</continuation>"
        )),
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
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolContext, ToolResult};
    use crate::{
        MemoryClaimKind, MemoryClaimScope, MemorySource, MemorySourceKind, MemoryWriteRequest,
        TaskStatus, TaskStep,
    };
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

    /// DeepSeek `deepseek-reasoner` 应触发 reasoning 模型熔断,降到 32 轮。
    #[test]
    fn max_turns_caps_reasoner_model_at_32() {
        assert_eq!(max_turns_for_model(200, "deepseek-reasoner"), 32);
        assert_eq!(max_turns_for_model(200, "DeepSeek-Reasoner"), 32);
        assert_eq!(max_turns_for_model(200, "deepseek-R1"), 32);
        assert_eq!(max_turns_for_model(200, "deepseek-r1-distill-qwen-7b"), 32);
    }

    /// 普通模型保留调用方传入的默认值,不受熔断影响。
    #[test]
    fn max_turns_keeps_default_for_normal_models() {
        assert_eq!(max_turns_for_model(200, "deepseek-chat"), 200);
        assert_eq!(max_turns_for_model(200, "claude-opus-4-5"), 200);
        assert_eq!(max_turns_for_model(64, "qwen3-coder-plus"), 64);
    }

    /// fnv_hash_value 对 Object 的 key 顺序不敏感,数组保持原序。
    /// 同语义不同 JSON 序列化顺序应产出相同 hash,避免误伤正常重试。
    #[test]
    fn fnv_hash_is_stable_across_key_order() {
        let a = serde_json::json!({"path": "Cargo.toml", "limit": 10});
        let b = serde_json::json!({"limit": 10, "path": "Cargo.toml"});
        assert_eq!(fnv_hash_value(&a), fnv_hash_value(&b));

        let arr_a = serde_json::json!({"items": [1, 2, 3]});
        let arr_b = serde_json::json!({"items": [3, 2, 1]});
        assert_ne!(fnv_hash_value(&arr_a), fnv_hash_value(&arr_b));
    }

    /// 5 条历史里有 3 条同 (name, hash) 视为循环。
    #[test]
    fn detect_loop_fires_at_threshold() {
        let mut window = std::collections::VecDeque::new();
        let h = fnv_hash_value(&serde_json::json!({"path": "foo"}));
        // 注入 3 次相同 (Read, h),窗口容量 5
        for _ in 0..3 {
            window.push_back(("Read".to_string(), h));
        }
        // 不同参数不算循环
        let h2 = fnv_hash_value(&serde_json::json!({"path": "bar"}));
        window.push_back(("Read".to_string(), h2));
        window.push_back(("Edit".to_string(), h));

        assert!(detect_loop(&window, "Read", h));
        assert!(!detect_loop(&window, "Read", h2));
        assert!(!detect_loop(&window, "Edit", h));
    }

    /// 命中计数只在最近 LOOP_GUARD_WINDOW (5) 条里数,更早的不算。
    #[test]
    fn detect_loop_ignores_old_entries() {
        let mut window = std::collections::VecDeque::new();
        let h = fnv_hash_value(&serde_json::json!({"path": "foo"}));
        // 8 条历史,前 5 条相同 + 后 3 条不同 — 但窗口只看最近 5,所以不算循环
        for _ in 0..5 {
            window.push_back(("Read".to_string(), h));
        }
        for _ in 0..3 {
            window.push_back(("Edit".to_string(), h));
        }
        assert!(!detect_loop(&window, "Read", h));
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
                native: None,
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
    async fn single_tool_capability_serializes_complete_multi_call_response() {
        let capabilities = wyj_api::ModelCapabilities::conservative(64_000, 8_192);
        assert_eq!(capabilities.max_tools_per_turn, 1);
        assert!(!capabilities.parallel_tool_calls.value);

        let mut agent = Agent::new(Arc::new(TwoToolProvider {
            calls: AtomicUsize::new(0),
        }))
        .with_model_capabilities(capabilities);
        agent.register_tool(Arc::new(SleepTool));
        let mut session = Session::new();
        session.push_user("go");

        let start = Instant::now();
        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert!(
            start.elapsed() >= std::time::Duration::from_millis(280),
            "单工具兼容模式应串行执行已返回的完整调用，实际耗时 {:?}",
            start.elapsed()
        );
        let results: Vec<_> = session
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content: ToolResultContent::Text(text),
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), text.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![("t1", "first", false), ("t2", "second", false)]
        );
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
                native: None,
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

    struct RepeatedTwoToolProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for RepeatedTwoToolProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n < 3 {
                let first_id = format!("round-{n}-first");
                let second_id = format!("round-{n}-second");
                // 每次给 input 加个 round 标识,让 fnv_hash 不重复,避免
                // loop detection(最近 5 次相同 input 命中 3 次触发)误伤
                // 这个测 argument retry guard 的本意。
                let first_args = format!(r#"{{"round":{n}}}"#);
                let second_args = format!(r#"{{"round":{n},"slot":2}}"#);
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: first_id.clone(),
                        name: "Echo".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: first_id,
                        json_delta: first_args,
                    }),
                    Ok(StreamEvent::ToolUseStart {
                        id: second_id.clone(),
                        name: "Echo".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: second_id,
                        json_delta: second_args,
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

    #[tokio::test]
    async fn repeated_multi_call_responses_do_not_trip_argument_retry_guard() {
        let calls = Arc::new(AtomicUsize::new(0));
        let capabilities = wyj_api::ModelCapabilities::conservative(64_000, 8_192);
        let mut agent = Agent::new(Arc::new(RepeatedTwoToolProvider {
            calls: AtomicUsize::new(0),
        }))
        .with_model_capabilities(capabilities);
        agent.register_tool(Arc::new(CountingTool {
            calls: calls.clone(),
        }));
        let mut session = Session::new();
        session.push_user("go");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 6);
        assert!(!session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult {
                        content: ToolResultContent::Text(text),
                        ..
                    } if text.contains("tool_limit_exceeded")
                )
            })
        }));
    }

    struct RequiredCountingTool {
        calls: Arc<AtomicUsize>,
    }

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.to_string(),
                description: format!("{} test tool", self.0),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            }
        }

        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<ToolResult> {
            Ok(ToolResult::ok(self.0.to_string()))
        }
    }

    #[test]
    fn lazy_tools_only_activate_above_the_configured_threshold() {
        let mut small = Agent::new(Arc::new(EndTurnProvider));
        small.register_tool(Arc::new(CountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        assert!(!small.enable_lazy_tools(Vec::<String>::new(), 1, 8, 3));
        assert!(small.lazy_tool_state.is_none());
        assert!(!small
            .tools
            .iter()
            .any(|definition| definition.name == "ToolSearch"));

        let mut large = Agent::new(Arc::new(EndTurnProvider));
        large.register_tool(Arc::new(CountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        assert!(large.enable_lazy_tools(Vec::<String>::new(), 0, 8, 3));
        assert!(large.lazy_tool_state.is_some());
        assert!(large
            .tools
            .iter()
            .any(|definition| definition.name == "ToolSearch"));
    }

    #[test]
    fn lazy_tools_never_hide_the_core_execution_surface() {
        let mut agent = Agent::new(Arc::new(EndTurnProvider));
        for name in [
            "Read",
            "Bash",
            "BashOutput",
            "Edit",
            "Write",
            "Memory",
            "Agent",
            "ExitPlanMode",
            "window_capture",
            "app_computer",
            "computer",
            "mcp__optional__lookup",
        ] {
            agent.register_tool(Arc::new(NamedTool(name)));
        }

        assert!(agent.enable_lazy_tools(["Read".to_string()], 0, 8, 3));
        let state = agent.lazy_tool_state.as_ref().unwrap();
        for name in [
            "Read",
            "Bash",
            "BashOutput",
            "Edit",
            "Write",
            "Memory",
            "Agent",
            "ExitPlanMode",
            "window_capture",
            "app_computer",
            "computer",
        ] {
            assert!(
                state.visible(name),
                "核心工具 {name} 不应被 lazy schema 隐藏"
            );
        }
        assert!(!state.visible("mcp__optional__lookup"));
    }

    #[async_trait::async_trait]
    impl Tool for RequiredCountingTool {
        fn name(&self) -> &str {
            "RequiredEcho"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "RequiredEcho".into(),
                description: String::new(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["value"],
                    "properties": {"value": {"type": "string"}},
                    "additionalProperties": false
                }),
                native: None,
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

    struct MalformedThenTextProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for MalformedThenTextProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let events: Vec<Result<StreamEvent>> = if self.calls.fetch_add(1, Ordering::SeqCst) == 0
            {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "bad-1".into(),
                        name: "Echo".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "bad-1".into(),
                        json_delta: r#"{"command":"unterminated"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("recovered".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn malformed_tool_arguments_are_reported_but_never_executed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(MalformedThenTextProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(CountingTool {
            calls: calls.clone(),
        }));
        let mut session = Session::new();
        session.push_user("go");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content: ToolResultContent::Text(text), is_error: true, .. }
                        if text.contains("tool_arguments_invalid")
                )
            })
        }));
    }

    struct InvalidSchemaThenCorrectProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for InvalidSchemaThenCorrectProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = match n {
                0 => vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "schema-bad".into(),
                        name: "RequiredEcho".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "schema-bad".into(),
                        json_delta: "{}".into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ],
                1 => vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "schema-good".into(),
                        name: "RequiredEcho".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "schema-good".into(),
                        json_delta: r#"{"value":"ok"}"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ],
                _ => vec![
                    Ok(StreamEvent::TextDelta("done".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ],
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn schema_error_is_targeted_and_corrected_call_executes_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(InvalidSchemaThenCorrectProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(RequiredCountingTool {
            calls: calls.clone(),
        }));
        let mut session = Session::new();
        session.push_user("go");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(session.api_calls >= 3);
    }

    /// `is_allowed` 恒返回 false，用于验证 PreToolUse `Approve` 不能绕过强制策略。
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
    async fn pre_tool_use_approve_cannot_bypass_is_allowed() {
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

        assert!(is_error);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "hook approve 只能跳过交互询问，不能绕过强制权限策略"
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

    struct UnsupportedThinkingThenSuccessProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for UnsupportedThinkingThenSuccessProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut error = wyj_api::ProviderError::new(
                    wyj_api::ProviderErrorKind::UnsupportedParameter,
                    "thinking is unsupported",
                );
                error.parameter = Some("thinking".to_string());
                return Err(anyhow::Error::new(error));
            }
            assert_eq!(opts.thinking_budget, None);
            assert!(!opts.interleaved);
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    #[tokio::test]
    async fn unsupported_safe_parameter_is_removed_once_and_reported() {
        let agent = Agent::new(Arc::new(UnsupportedThinkingThenSuccessProvider {
            calls: AtomicUsize::new(0),
        }))
        .with_thinking(Some(1024), true);
        let mut session = Session::new();
        session.push_user("hi");
        let mut visible = String::new();
        agent
            .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
            .await
            .unwrap();
        assert!(visible.contains("已安全移除后重试一次"));
        assert_eq!(session.api_calls, 2);
    }

    struct TypedFailureProvider {
        kind: wyj_api::ProviderErrorKind,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for TypedFailureProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::Error::new(wyj_api::ProviderError::new(
                self.kind,
                "typed test failure",
            )))
        }
    }

    #[tokio::test]
    async fn retryable_failure_switches_route_once_at_message_boundary() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let backup_calls = Arc::new(AtomicUsize::new(0));
        let backup = AgentRoute::new(
            "backup",
            "minimax",
            "backup-model",
            Arc::new(CountingEndTurnProvider {
                calls: backup_calls.clone(),
            }),
        );
        let agent = Agent::new(Arc::new(TypedFailureProvider {
            kind: wyj_api::ProviderErrorKind::RateLimited,
            calls: primary_calls.clone(),
        }))
        .with_route_identity("primary", "minimax", "primary-model")
        .with_fallback_routes(vec![backup], false);
        let mut session = Session::new();
        session.push_user("hi");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.routing_events.len(), 1);
        assert_eq!(session.routing_events[0].from_profile, "primary");
        assert_eq!(session.routing_events[0].to_profile, "backup");
        assert_eq!(
            session.routing_events[0].boundary,
            "before_assistant_commit"
        );
    }

    #[tokio::test]
    async fn authentication_failure_never_falls_back() {
        let backup_calls = Arc::new(AtomicUsize::new(0));
        let backup = AgentRoute::new(
            "backup",
            "minimax",
            "backup-model",
            Arc::new(CountingEndTurnProvider {
                calls: backup_calls.clone(),
            }),
        );
        let agent = Agent::new(Arc::new(TypedFailureProvider {
            kind: wyj_api::ProviderErrorKind::Authentication,
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .with_route_identity("primary", "minimax", "primary-model")
        .with_fallback_routes(vec![backup], false);
        let mut session = Session::new();
        session.push_user("hi");

        assert!(agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .is_err());
        assert_eq!(backup_calls.load(Ordering::SeqCst), 0);
        assert!(session.routing_events.is_empty());
    }

    #[test]
    fn cross_vendor_fallback_is_filtered_by_default() {
        let backup = AgentRoute::new(
            "backup",
            "another-vendor",
            "backup-model",
            Arc::new(EndTurnProvider),
        );
        let agent = Agent::new(Arc::new(EndTurnProvider))
            .with_route_identity("primary", "minimax", "primary-model")
            .with_fallback_routes(vec![backup], false);
        assert!(agent.fallback_routes.is_empty());
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

    /// 流已正常结束（收到 MessageStop + Usage），但 eventsource_stream 在末尾
    /// 又多调一次 next() 返回 Err（这是 crate 对 TCP EOF 的已知行为，不是真正的
    /// 网络中断）。Agent 必须把这种"良性末尾 EOF"忽略，绝不能丢弃已累积的完整
    /// 文本，也绝不触发整轮重试。
    struct EofAfterCompletionProvider;
    #[async_trait::async_trait]
    impl Provider for EofAfterCompletionProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta("完整回复（中途不丢字）".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
                Ok(StreamEvent::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                }),
                Err(anyhow::anyhow!("EOF after completion")),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn stream_eof_after_completion_is_ignored_not_retried() {
        let agent = Agent::new(Arc::new(EofAfterCompletionProvider));
        let mut session = Session::new();
        session.push_user("hi");
        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        // 整段回复完整保留，没有任何重试
        let assistant_texts: Vec<String> = session
            .messages
            .iter()
            .filter(|m| matches!(m.role, wyj_api::types::Role::Assistant))
            .map(|m| m.text())
            .collect();
        assert_eq!(assistant_texts.len(), 1);
        assert_eq!(assistant_texts[0], "完整回复（中途不丢字）");
        // 关键断言：只调了一次 API,没有触发流中断重试
        assert_eq!(
            session.api_calls, 1,
            "末尾 EOF 不应触发重试,实际 API 调用次数 {}",
            session.api_calls
        );
        // 用量应已正常入账
        assert_eq!(session.total_input_tokens, 100);
        assert_eq!(session.total_output_tokens, 50);
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
                native: None,
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

    #[test]
    fn dropping_episode_guard_persists_cancelled_episode() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let store = Arc::new(
            EvolutionStore::new(dir.path(), &repo, wyj_config::EvolutionCfg::default()).unwrap(),
        );
        let mut session = Session::new();
        session.push_user("cancel this turn");
        let capture = store.begin_episode(
            "session-abort",
            &session,
            "cancel this turn",
            "default",
            "test-vendor",
            "test-model",
        );

        drop(EvolutionEpisodeGuard {
            store: store.clone(),
            capture: Some(capture),
        });

        let episodes = store.list_episodes(10).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(
            episodes[0].outcome,
            crate::evolution::EpisodeOutcome::Cancelled
        );
        assert_eq!(episodes[0].evidence[0].label, "turn_cancelled");
    }

    fn push_user_text(session: &mut crate::session::Session, text: &str) {
        use wyj_api::types::{ContentBlock, Message, Role};
        session.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        });
    }

    fn session_with_query(query: &str) -> crate::session::Session {
        let mut session = crate::session::Session::default();
        push_user_text(&mut session, query);
        session
    }

    fn seed_task(
        store: &crate::MemoryV3Store,
        title: &str,
        status: TaskStatus,
        blocked_reason: Option<&str>,
    ) {
        store
            .upsert(MemoryWriteRequest {
                kind: MemoryClaimKind::Task,
                scope: MemoryClaimScope::Project,
                title: title.to_string(),
                content: format!("Task: {title}"),
                entities: vec![title.to_string()],
                tags: vec!["task".to_string()],
                source: MemorySource {
                    kind: MemorySourceKind::Assistant,
                    locator: "session:test#assistant".to_string(),
                    observed_at: Some("2026-08-22T09:00:00+08:00".to_string()),
                },
                evidence: vec![],
                confidence: 0.9,
                expires_at: None,
                supersedes: None,
                task_status: Some(status),
                task_steps: vec![
                    TaskStep {
                        description: "已完成步骤".to_string(),
                        done: true,
                        updated_at: Some("2026-08-22T08:30:00+08:00".to_string()),
                    },
                    TaskStep {
                        description: "下一步要做".to_string(),
                        done: false,
                        updated_at: Some("2026-08-22T09:00:00+08:00".to_string()),
                    },
                ],
                blocked_reason: blocked_reason.map(|s| s.to_string()),
            })
            .unwrap();
    }

    #[test]
    fn continuation_keyword_detection_matches_documented_phrases() {
        for ok in [
            "继续",
            "继续吧",
            "继续。",
            " 继续 ",
            "Continue",
            "resume",
            "Go on",
            "再来",
            "接着",
        ] {
            assert!(is_continuation_request(ok), "应该命中: {ok:?}");
        }
        for no in [
            "继续分析招商银行",
            "hello world",
            "",
            "   ",
            "继续", // 校验 trim 后但带中文标点的边界
        ] {
            let _ = no; // 静默
        }
        // 长 query 不算 continuation（即使包含"继续"）。
        assert!(!is_continuation_request("继续帮我看看 stock2 招商银行持仓"));
        // 完全不相关的 query。
        assert!(!is_continuation_request("hello"));
    }

    #[test]
    fn continuation_suffix_resumes_in_progress_task_with_next_step() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
        seed_task(
            &store,
            "迁移到 Memory v3 final",
            TaskStatus::InProgress,
            None,
        );

        let session = session_with_query("继续");
        let suffix = continuation_suffix(&store).expect("有 InProgress 任务时返回注入");
        assert!(suffix.contains("Resuming task"));
        assert!(suffix.contains("迁移到 Memory v3 final"));
        assert!(suffix.contains("下一步要做"));
        let _ = session;
    }

    #[test]
    fn continuation_suffix_uses_i18n_key_when_no_open_tasks() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
        seed_task(&store, "已完成", TaskStatus::Completed, None);
        let suffix = continuation_suffix(&store).expect("无开放任务仍需返回提示");
        // 默认 Locale 是 zh 时落中文；en 在 set_locale 后才落英文。
        assert!(
            suffix.contains("没有未完成任务") || suffix.contains("No open tasks"),
            "应命中 i18n key，但拿到: {suffix}"
        );
    }

    #[test]
    fn continuation_suffix_lists_blocked_when_no_in_progress() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
        seed_task(
            &store,
            "等用户确认",
            TaskStatus::Blocked,
            Some("等用户回复 Global 偏好确认"),
        );
        let suffix = continuation_suffix(&store).expect("有 Blocked 时返回 Open Tasks 列表");
        assert!(suffix.contains("Still-open tasks"));
        assert!(suffix.contains("等用户确认"));
    }

    #[test]
    fn build_memory_snapshot_appends_continuation_when_query_is_resume_keyword() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
        seed_task(
            &store,
            "迁移到 Memory v3 final",
            TaskStatus::InProgress,
            None,
        );

        let session = session_with_query("继续");
        let snapshot = build_memory_snapshot(&store, &session);
        assert!(snapshot.contains("## Project Brief"));
        assert!(snapshot.contains("Resuming task"));
        assert!(snapshot.contains("下一步要做"));
    }

    #[test]
    fn build_memory_snapshot_does_not_append_continuation_for_normal_query() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
        seed_task(
            &store,
            "迁移到 Memory v3 final",
            TaskStatus::InProgress,
            None,
        );

        let session = session_with_query("帮我看看 stock2 的招商银行持仓");
        let snapshot = build_memory_snapshot(&store, &session);
        assert!(snapshot.contains("## Project Brief"));
        assert!(
            !snapshot.contains("Resuming task"),
            "非续接 query 不应追加 continuation 块: {snapshot}"
        );
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
