//! Agent 装配的唯一入口。
//!
//! ## 为什么要有这个模块
//!
//! `/model` 热切换、Profile 面板保存都会**重建 Agent**，而首建与重建此前是两份
//! 各自独立的 `Agent::new(...)....with_xxx()` 链——各自几十处调用。它们的内容
//! 本该逐字相同：漏一处不是编译错误、也不是测试失败，而是「按 `/model` 之后
//! verify 静默失效」这类只在切换之后才显形的行为差异。
//!
//! 代码里已经留下过两处这样的现场注记（`with_verify` 的注释写着「漏掉这里会让
//! 项目声明了 verify 但切个模型就没了」，`with_skills` 的注释写着「重建漏注入
//! skill 名单会让模型重新变回我不知道有哪些 skill」）。`with_max_depth` 只是
//! **第三处**，而且至今仍不一致——首建侧挂了、重建侧漏了，`/model` 之后子 Agent
//! 嵌套深度静默退回默认值。把共同部分抽成函数，让「漏掉」在语法上不再可能。
//!
//! ## 不变量
//!
//! **首建与重建必须经过同一个 [`build_tool_registry`] 与同一个
//! [`SharedAgentConfig::apply`]**。任何一侧想特殊化某个工具，先想清楚重建路径
//! 是否需要同样处理——否则 `/model` 之后模型看到的世界就变了。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use wyj_core::{Agent, AgentRoute};
use wyj_tools::{
    AskQuestionTool, MemoryTool, SubAgentTool, TodoWriteTool, ToolRegistry, WebSearchTool,
};

/// 装配工具注册表所需的、跨首建/重建保持不变的依赖。
pub struct RegistryDeps<'a> {
    /// 配置只借用：调用点持有的 `cfg` 生命周期覆盖整个装配过程。
    pub cfg: &'a wyj_config::Config,
    pub code_index: Arc<dyn wyj_core::CodeIndex>,
    pub memory_v3: Option<Arc<wyj_core::MemoryV3Store>>,
    /// 跨会话原始 transcript 检索入口。缺失时 Memory 工具的 `search_sessions`
    /// action 明确报不可用（而不是静默空结果）。
    pub sessions: Option<Arc<wyj_core::SessionStore>>,
    pub workspace_cas: Option<Arc<wyj_core::workspace_cas::WorkspaceCas>>,
    pub todo_store: Arc<Mutex<wyj_tools::todo::TodoStore>>,
    pub agent_defs: wyj_tools::SharedAgentDefinitions,
    pub sub_agent_hub: Arc<wyj_tools::SubAgentHub>,
    pub sub_agent_factory: wyj_tools::AgentFactory,
    pub session_id: String,
    pub sessions_dir: PathBuf,
    pub message_log: Option<Arc<wyj_tools::message_log::MessageLogWriter>>,
    pub mcp_tools: wyj_tools::SharedMcpTools,
}

/// computer-use 三个工具的门控函数。
///
/// 它们由 `main.rs` 提供（那里才有 `cfg.tools` 的子项读取与提示文案），但**注册
/// 时机必须落在同一个函数里**——三者是同一组门槛（同样要把截图作为 image block
/// 塞进 tool_result），任何一侧少注册一个都是「开关关了却仍有工具可见」。
pub struct ComputerRegistrars {
    pub computer: fn(&mut ToolRegistry, &wyj_config::Config) -> bool,
    pub window_capture: fn(&mut ToolRegistry, &wyj_config::Config),
    pub app_computer: fn(&mut ToolRegistry, &wyj_config::Config),
}

/// 构造首建与重建**共用**的工具注册表。
pub fn build_tool_registry(
    deps: &RegistryDeps<'_>,
    computers: &ComputerRegistrars,
) -> ToolRegistry {
    let cfg = deps.cfg;
    let mut registry = ToolRegistry::standard();

    registry.register_arc(Arc::new(wyj_core::CodeSearchTool::new(
        deps.code_index.clone(),
    )));

    if let Some(memory) = &deps.memory_v3 {
        // 会话检索与 claim 检索挂在同一个工具上：模型判断「查记忆」时不该
        // 还要先分清「查提取过的事实」还是「翻原始对话」——一次调用先试
        // search，找不到再 search_sessions 是更自然的用法。
        let mut tool = MemoryTool::new(memory.clone());
        if let Some(sessions) = &deps.sessions {
            tool = tool.with_sessions(sessions.clone());
        }
        registry.register_arc(Arc::new(tool));
        // CAS 可用时必须注册 ContextRecall：否则被外部化的工具输出对模型就是
        // 不可逆的损失（尤其 Bash / WebFetch 重跑也拿不回）。
        if let Some(t) = wyj_tools::context_recall::tool_for_cas(deps.workspace_cas.clone()) {
            registry.register_arc(Arc::new(t));
        }
    }

    // 任务列表由 `[tools].todo_enabled` 门控：未注册的工具不产生 schema，
    // 模型目录里根本看不到，也就不会浪费 LLM 往返去更新它。
    if cfg.tools.todo_enabled {
        registry.register_arc(Arc::new(TodoWriteTool::new(deps.todo_store.clone())));
    }
    registry.register_arc(Arc::new(AskQuestionTool::new()));

    // WebSearch：仅当 `[search]` 配置可用（后端已声明且 key 存在）时注册。未配置时
    // 模型看不到该工具，避免它调用一个必然失败的工具白跑一趟。
    if let Some(backend) = wyj_tools::build_backend(cfg) {
        registry.register_arc(Arc::new(
            WebSearchTool::new(backend, cfg.search.max_results)
                .with_default_domains(&cfg.search.allowed_domains, &cfg.search.blocked_domains),
        ));
    }

    (computers.computer)(&mut registry, cfg);
    (computers.window_capture)(&mut registry, cfg);
    (computers.app_computer)(&mut registry, cfg);

    registry.register_arc(Arc::new(build_sub_agent_tool(deps)));

    // MCP 工具：读取**当前快照**。重建刻意复用现有连接，让 scoped skill 的
    // Agent 看到与下一个正常轮次完全相同的工具集。
    for tool in deps.mcp_tools.read().unwrap().iter() {
        registry.register_arc(tool.clone());
    }
    registry
}

fn build_sub_agent_tool(deps: &RegistryDeps<'_>) -> SubAgentTool {
    let cfg = deps.cfg;
    let factory = deps.sub_agent_factory.clone();
    let mut tool = SubAgentTool::new_shared(
        deps.agent_defs.clone(),
        deps.sub_agent_hub.clone(),
        move |def| factory(def),
    )
    .with_response_language(wyj_i18n::resolve_locale(cfg.language.as_deref()))
    .with_session_id(deps.session_id.clone())
    // 首建与重建都必须挂 max_depth：漏掉时 `/model` 之后子 Agent 嵌套深度静默
    // 退回默认值，没有任何报错。
    .with_max_depth(cfg.subagent.max_depth)
    .with_sessions_dir(deps.sessions_dir.clone());
    if let Some(log) = deps.message_log.clone() {
        tool = tool.with_message_log(log);
    }
    tool
}

/// 首建与重建共用的 Agent 配置尾部。
///
/// 覆盖 `with_claude_md` 之后到 lazy tool 折叠之前的**全部**步骤。每一项都是
/// 「重建路径一旦漏掉就会静默降级」的类别：persist cap（切换 Profile 后仍守住
/// 单次 stream 不撞穿窗口）、context editing、verify（项目声明了校验但切个模型
/// 就没了）、记忆 / 演化 / checkpoint 三个 store、capability cache。
pub struct SharedAgentConfig<'a> {
    pub cfg: &'a wyj_config::Config,
    pub cwd: &'a Path,
    pub claude_md: Arc<wyj_core::ClaudeMdLoader>,
    pub hooks: Arc<wyj_core::HookRunner>,
    pub checkpoint_store: Option<Arc<wyj_core::checkpoint::CheckpointStore>>,
    pub memory_store: Option<Arc<wyj_core::MemoryStore>>,
    pub memory_v3: Option<Arc<wyj_core::MemoryV3Store>>,
    pub evolution: Option<Arc<wyj_core::EvolutionStore>>,
    pub capability_cache: Arc<wyj_api::CapabilityCache>,
    /// 已经装配好的 context editing 配置。调用方负责构造（它需要 `cfg` 与
    /// 进程级累计器），这里只负责挂上，避免两条路径各算一次而参数不同。
    pub context_edit: Option<wyj_core::context_edit::ContextEditCfg>,
    pub plugin_output_style: Option<String>,
    /// 辅助 LLM 往返（标题 / Auto 分类器 / 记忆提取 / 演化）的统一用量累加器。
    ///
    /// 必须走这里而不是首建路径：`/model` 重建出的 Agent 若拿不到同一个
    /// `Arc`，换模型之后那些辅助往返就记不进任何地方了。`None` = 未装配
    /// （测试路径），辅助往返不记账但不影响功能。
    pub aux_usage: Option<wyj_core::aux_usage::SharedAuxUsage>,
    /// 会话滚动工作文档。`Some` 时压缩优先 splice 它（省一次全量摘要往返），
    /// 并每 4 轮增量更新一次。
    ///
    /// 走 `SharedAgentConfig` 而不是只在首建时装：`/model` 重建出的 Agent
    /// 若拿不到同一个文档，压缩就会在换模型后突然退回全量摘要——用户看到
    /// 的是「换了模型成本就变了」，而真实原因是装配漏了一项。
    pub session_memory: Option<Arc<wyj_core::session_memory::SessionMemory>>,
}

impl SharedAgentConfig<'_> {
    pub fn apply(self, mut agent: Agent) -> Agent {
        let cfg = self.cfg;
        if let Some(style) = &self.plugin_output_style {
            agent = agent.append_system(style.clone());
        }
        agent = agent.with_claude_md(self.claude_md).with_hooks(self.hooks);
        if let Some(store) = &self.checkpoint_store {
            agent = agent.with_checkpoint_store(store.clone());
        }
        // runtime ContentBlock 截断兜底：`/model` 重建 agent 时同样注入，
        // 否则切换 Profile 后单次 stream 可能撞穿上下文窗口。
        agent = agent
            .with_persist_cap(Some(cfg.persist_cap.clone()))
            .with_context_edit(self.context_edit)
            // 项目在 `.wyj-code/settings.json` 声明了 `verify.command` 才生效，
            // 没声明则挂 None —— 零配置项目零成本。
            .with_verify(wyj_core::verify::load_verify_cfg(self.cwd))
            // 验收契约（完成门禁）：同样零假设，没声明 `[acceptance]` 就完全不参与。
            // 解析失败时 `AcceptanceLoad.error` 会带着原因进 Agent，收尾时明确
            // 报给用户——门禁「看起来没生效」必须比「没门禁」更显眼。
            .with_acceptance(wyj_core::acceptance::load_acceptance_cfg(self.cwd));
        if let Some(mem) = &self.memory_store {
            agent = agent.with_memory(mem.clone());
        }
        if let Some(memory) = &self.memory_v3 {
            agent = agent.with_memory_v3(memory.clone());
        }
        if let Some(evolution) = &self.evolution {
            agent = agent.with_evolution(evolution.clone());
        }
        agent = agent.with_aux_usage(self.aux_usage);
        agent = agent.with_session_memory(self.session_memory.clone());
        agent.with_capability_cache(Some(self.capability_cache))
    }
}

/// 首建与重建共用的**主链路**参数。
///
/// 之所以和 `SharedAgentConfig` 分开：这一段依赖「本次要发往哪个模型」，每次
/// 重建都可能变；而 `SharedAgentConfig` 的输入在两次装配之间完全不变。
pub struct AgentCoreConfig {
    pub env_info: wyj_core::prompts::EnvInfo,
    pub skills: Vec<wyj_core::prompts::SkillEntry>,
    pub locale: String,
    pub git_snapshot: Option<String>,
    pub max_tokens: u32,
    pub context_window: u32,
    pub max_turns: usize,
    /// 强制思考端点的单回合熔断轮数（`[agent].forced_thinking_max_turns`）。
    pub forced_thinking_max_turns: usize,
    /// 花费熔断闸（金额 + 输出 token）。默认全关，见 `wyj_core::budget`。
    pub budget_limits: wyj_core::budget::BudgetLimits,
    pub capabilities: wyj_api::capabilities::ModelCapabilities,
    pub route_profile: String,
    pub route_vendor: String,
    pub model_name: String,
    /// 当前 route 的 base_url（不含密钥）。参与 capability_cache fingerprint，
    /// 漏传会让同 vendor+model 的不同端点共用同一份能力缓存。
    pub route_base_url: String,
    /// 当前 route 的线协议。决定 `ThinkingSpec::for_vendor` 查哪张规格表。
    pub route_wire_protocol: wyj_config::WireProtocol,
    pub fallback_routes: Vec<AgentRoute>,
    pub cross_provider_fallback: bool,
    pub thinking_budget: Option<u32>,
    pub interleaved_thinking: bool,
    pub reasoning_effort: Option<String>,
    pub thinking_switch: Option<String>,
}

impl AgentCoreConfig {
    pub fn apply(self, agent: Agent) -> Agent {
        // 必须紧跟 with_system：它做 `{skills}` 占位替换，而 with_system 是整体替换
        agent
            .with_system(wyj_core::prompts::main_system_prompt(&self.env_info))
            .with_skills(self.skills)
            // `<env>` 里的语言事实在 system 中段（后面还压着记忆快照 + CLAUDE.md
            // reminder），尾部再钉一遍提高 recency。见 prompts::response_language_reminder。
            .with_response_language(self.locale)
            .with_git_snapshot(self.git_snapshot)
            .with_max_tokens(self.max_tokens)
            .with_context_window(self.context_window)
            .with_max_turns(self.max_turns)
            .with_forced_thinking_max_turns(self.forced_thinking_max_turns)
            .with_budget_limits(self.budget_limits)
            .with_model_capabilities(self.capabilities)
            .with_route_identity(
                self.route_profile,
                self.route_vendor,
                self.model_name,
                self.route_base_url,
                self.route_wire_protocol,
            )
            .with_fallback_routes(self.fallback_routes, self.cross_provider_fallback)
            .with_thinking(self.thinking_budget, self.interleaved_thinking)
            .with_reasoning_effort(self.reasoning_effort)
            .with_thinking_switch(self.thinking_switch)
    }
}

/// 把注册表里的工具定义装进 Agent，并按模型能力决定是否折叠 lazy schema。
///
/// lazy 工具名单写在这里而不是两个调用点各写一份：它是「模型目录」的一部分，
/// 两侧不一致意味着 `/model` 之后模型看到的工具集合变了。
pub fn attach_tools(
    agent: &mut Agent,
    registry: &ToolRegistry,
    cfg: &wyj_config::Config,
    enable_lazy: bool,
) {
    for def in registry.definitions() {
        if let Some(t) = registry.get(&def.name) {
            agent.register_tool(t);
        }
    }
    if enable_lazy {
        agent.enable_lazy_tools(
            ["Read", "Glob", "Grep", "AskQuestion", "Memory"]
                .into_iter()
                .map(str::to_string),
            cfg.model_runtime.lazy_tools_threshold,
            cfg.model_runtime.lazy_tools_top_k,
            cfg.model_runtime.lazy_tools_sticky_turns,
        );
    }
}
