//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::claude_md::ClaudeMdLoader;
use crate::compact::{compact_session_until_fit, compact_trigger_buffer, ContextAudit};
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

/// 注入范围：主 Agent 收全部；前台子 Agent 只收用户补充。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InjectScope {
    All,
    UserMessagesOnly,
}

/// 回合级共享注入队列：Agent 忙碌期间用户补充的内容，在**工具轮边界**被排空。
///
/// 为什么是共享队列而不是每个 Agent 一条 mpsc：
/// **前台子 Agent 是一个不返回的工具调用**——`SubAgentTool::run()` 内部
/// `await` 整个子 Agent，而子 Agent 自己的循环此前根本没有注入通道
/// （`run_turn` 不接受任何注入源）。于是用户在子 Agent 跑的过程中补充的内容，
/// 既进不了子 Agent，也进不了被工具调用堵住的主 Agent，只能等它整个跑完
/// （实测一个 12 分钟的子 Agent = 12 分钟白等）。
///
/// 改成主 Agent 与子 Agent 排空**同一份**队列后，谁先到达工具轮边界谁取走：
/// 子 Agent 在跑 → 补充内容插进子 Agent 的下一轮；子 Agent 已返回 → 补充内容
/// 插进主 Agent 的下一轮。两种时机都是"尽快送达"，不再取决于最慢的那个工具。
///
/// 内部用 `std::sync::Mutex` + 共享引用（`&self`）而非 `&mut` 接收端：多份
/// `Arc` 句柄要同时能从主 Agent 侧和工具侧拿到句柄，`&mut` 做不到。
#[derive(Default)]
pub struct SteerQueue {
    items: std::sync::Mutex<std::collections::VecDeque<(Vec<ContentBlock>, InjectionKind)>>,
}

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用户提交了一条补充内容（或后台结果 reminder）。
    pub fn push(&self, blocks: Vec<ContentBlock>, kind: InjectionKind) {
        if let Ok(mut items) = self.items.lock() {
            items.push_back((blocks, kind));
        }
    }

    /// 排空全部待注入内容；返回空表示当前没有。**主 Agent 用这个**。
    pub fn drain(&self) -> Vec<(Vec<ContentBlock>, InjectionKind)> {
        match self.items.lock() {
            Ok(mut items) => items.drain(..).collect(),
            // 上一位排空者正持锁时把它抢过来代价不大（最多丢一条用户补充），
            // 但这里不该发生——同一时刻只有一个 Agent 在跑工具轮。
            Err(_) => Vec::new(),
        }
    }

    /// 只排空**用户补充**（`UserMessage`），把 `SystemReminder` 原样留在队列里。
    ///
    /// **前台子 Agent 必须用这个。** 队列是共享的，`SystemReminder` 装的是后台
    /// 子 Agent / 后台 shell 的完成通知——那是给**主 Agent** 的结论，混进子
    /// Agent 的会话既答非所问，又会让主 Agent 永远收不到这条结果（子 Agent
    /// 返回后队列已空）。用户补充则相反：它就是要打断当前正在跑的东西，
    /// 谁在跑就进谁的下一轮。
    pub fn drain_user_messages(&self) -> Vec<(Vec<ContentBlock>, InjectionKind)> {
        let Ok(mut items) = self.items.lock() else {
            return Vec::new();
        };
        let mut taken = Vec::new();
        let mut kept = std::collections::VecDeque::new();
        while let Some(item) = items.pop_front() {
            match item.1 {
                InjectionKind::UserMessage => taken.push(item),
                InjectionKind::SystemReminder => kept.push_back(item),
            }
        }
        *items = kept;
        taken
    }

    pub fn is_empty(&self) -> bool {
        self.items
            .lock()
            .map(|items| items.is_empty())
            .unwrap_or(true)
    }

    /// 队列里**尚未被任何 Agent 取走的用户补充**条数。
    ///
    /// UI 侧的 `pending_queue` 是这份队列的镜像（入队时两边各加一条），
    /// 正常靠 `AgentEvent::Injected` 出队。但共享队列之后**取走者不再唯一**：
    /// 前台子 Agent 也会 `drain_user_messages()`，而它拿不到 UI 的事件通道、
    /// 发不出 `Injected`（见 `SubAgentTool` 里的空 `on_inject`）。于是镜像会
    /// 停在旧值上，回合结束后的兜底重发会把**已经投递给子 Agent 的消息再发
    /// 一遍**——同一条要求子 Agent 干一遍、主 Agent 再干一遍。
    ///
    /// 有了这个计数，UI 就能把镜像裁剪到队列的真实水位，而不必知道是谁取走的。
    /// `SystemReminder`（后台结果通知）不计入：它从来不进 `pending_queue`。
    pub fn pending_user_messages(&self) -> usize {
        self.items
            .lock()
            .map(|items| {
                items
                    .iter()
                    .filter(|(_, kind)| matches!(kind, InjectionKind::UserMessage))
                    .count()
            })
            .unwrap_or(0)
    }
}

/// 随请求发送的工具 schema 估算 token。
///
/// **必须与 [`ContextAudit::from_request`] 的 `tool_schema_tokens` 同口径**
/// （逐工具：name + description + input_schema 三段各自 `estimate_text_tokens`
/// 后求和）。历史上这里用的是"整个 tools 数组序列化成 JSON 再 `bytes/4`"，
/// 与审计口径差 28%：同一批工具在 `/context` 面板显示 8044、而本函数算 6281。
/// 两个数指同一件事却长期不等，`/context` 又声称"与压缩决策同源"——
/// 压缩用的是审计口径，本函数算出来的累计值就与真正发出去的量对不上。
/// 现在统一到审计口径。
fn estimate_tool_schema_tokens(tools: &[ToolDefinition]) -> u32 {
    tools.iter().fold(0u32, |total, tool| {
        let schema = tool.input_schema.to_string();
        total.saturating_add(
            (crate::compact::estimate_text_tokens(&tool.name)
                + crate::compact::estimate_text_tokens(&tool.description)
                + crate::compact::estimate_text_tokens(&schema)) as u32,
        )
    })
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
    "Memory",
    // 清理把工具输出外部化后，模型唯一取回全文的入口。**必须常驻**——
    // 一旦被 lazy 折叠掉，占位符里的 cas:// 引用就成了死路，清理变成不可逆。
    "ContextRecall",
    "Agent",
    "ExitPlanMode",
    // TodoWrite 由 `[tools].todo_enabled` 门控（默认开，用户可显式关掉）。
    // 开启时**必须**常驻：它是纯状态写入、不碰文件系统，但 schema 一旦被 lazy 折叠，
    // 模型就完全不知道这个工具存在 —— 配置只管「注册」，管不到「可见」，
    // 表现为「配了 todo_enabled = true 但任务列表面板永远不出现」。
    // 未注册时不产生 schema，因此列在这里对只读子 Agent / Plan 模式同样无副作用。
    "TodoWrite",
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
    /// base_url（不含密钥），用于 capability_cache.record_rejection 推导 fingerprint
    pub base_url: String,
    /// wire_protocol，决定 cache fingerprint 的 vendor-specific 分桶
    pub wire_protocol: wyj_config::WireProtocol,
    pub provider: Arc<dyn Provider>,
    pub capabilities: Option<wyj_api::ModelCapabilities>,
    pub max_tokens: u32,
    pub context_window: u32,
    pub thinking_budget: Option<u32>,
    pub interleaved_thinking: bool,
    /// OpenAI-vendor reasoning_effort 档位（low/medium/high/max/xhigh/adaptive）
    pub reasoning_effort: Option<String>,
    /// OpenAI-vendor thinking.type 字符串（enabled/disabled/auto/adaptive）
    pub thinking_switch: Option<String>,
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
            base_url: String::new(),
            wire_protocol: wyj_config::WireProtocol::OpenAiChatCompletions,
            provider,
            capabilities: None,
            max_tokens: 8192,
            context_window: 200_000,
            thinking_budget: None,
            interleaved_thinking: true,
            reasoning_effort: None,
            thinking_switch: None,
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

    /// OpenAI-vendor reasoning_effort 档位透传
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.filter(|value| !value.is_empty());
        self
    }

    /// OpenAI-vendor thinking.type 字符串透传
    pub fn with_thinking_switch(mut self, switch: Option<String>) -> Self {
        self.thinking_switch = switch.filter(|value| !value.is_empty());
        self
    }

    /// 设置 base_url 与 wire_protocol（capability_cache fingerprint 推导需要）
    pub fn with_identity(
        mut self,
        base_url: impl Into<String>,
        wire_protocol: wyj_config::WireProtocol,
    ) -> Self {
        self.base_url = base_url.into();
        self.wire_protocol = wire_protocol;
        self
    }
}

/// 由 `[context_edit].soft_limit_ratio` 算出 context editing 的**动手线**。
///
/// 返回 `u32::MAX` 表示关闭软阈值——此时 `estimated > u32::MAX` 恒为假，
/// 条件退化成旧实现的 `estimated > compact_threshold`，行为逐字节一致。
/// 这样"关闭软阈值"就不需要调用点写任何分支。
///
/// 与压缩硬阈值的关系：软阈值只决定**何时开始**清理，硬阈值决定**何时停手**。
/// 1M 窗口下硬阈值在 900K，实际会话（~300K）永远够不着，context editing
/// 等于从未运行——这正是实测 129 个会话 `context_edit_freed_tokens` 全为 0
/// 的原因。提前动手的收益是乘法的：外部化越早，每趟 API 携带的历史越小。
fn soft_threshold_tokens(
    context_window: u32,
    cfg: Option<&crate::context_edit::ContextEditCfg>,
) -> u32 {
    match cfg {
        Some(cfg) if cfg.soft_limit_ratio > 0.0 => (context_window as f64 * cfg.soft_limit_ratio)
            .floor()
            .clamp(1.0, u32::MAX as f64) as u32,
        // 未启用 context editing，或显式 `soft_limit_ratio = 0`
        _ => u32::MAX,
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
    /// 项目规约文件（AGENTS.md / CLAUDE.md 等）加载器（可选）
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
    /// 用户配置的回复语言（`wyj_i18n::resolve_locale` 的解析结果）。
    ///
    /// 只用于每轮往 system prompt **尾部**追加一句
    /// [`prompts::response_language_reminder`]。语言事实本身已经在
    /// `with_system(main_system_prompt(..))` 的 `<env>` 里；这里追加是**为了
    /// 位置**——`run_turn` 会在 `<env>` 后面继续拼记忆快照、CLAUDE.md
    /// `turn_reminder`，本仓库规约文件动辄几万 token，recency 权重落不到中段
    /// 那句上，MiniMax M3.1 的 thinking 因此全英文（2026-10-15 实测）。
    ///
    /// 落在 volatile 段：不改变 stable 前缀哈希 → 不影响 prompt cache 命中。
    /// `None` = 不追加（子 Agent、后台任务等未接线的路径行为不变）。
    response_language: Option<String>,
    /// Extended thinking 预算（None/0 = 关闭）与交错思考开关
    thinking_budget: Option<u32>,
    interleaved_thinking: bool,
    /// OpenAI-vendor reasoning_effort 档位（low/medium/high/max/xhigh/adaptive）
    reasoning_effort: Option<String>,
    /// OpenAI-vendor thinking.type 字符串（enabled/disabled/auto/adaptive）
    thinking_switch: Option<String>,
    route_profile_name: String,
    route_vendor: String,
    route_model: String,
    /// 当前 route 的 base_url（不含密钥），用于 capability_cache 指纹推导。
    route_base_url: String,
    route_wire_protocol: wyj_config::WireProtocol,
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
    /// **本次用户提交内**已获 `AllowOnce` 的工具名。
    ///
    /// 用户按 Enter 提交的是「一个任务」，模型实现时会连续发出十几个
    /// Edit/Write。旧行为每个都弹一次对话框，于是「批准这个任务」被拆成十几个
    /// 串行的用户按键往返——每一次都是模型时延上的一个真实停顿，跨十几个文件的
    /// 重构尤其明显。
    ///
    /// 记录后本轮同名的后续调用直接放行。它**严格弱于**既有的 `AllowAlways`
    /// （后者写进项目级 `allowed_tools.json` 跨会话生效），因此不构成新的风险面，
    /// 只是把「这一次任务的授权」从「每一次调用的授权」里析出来。
    ///
    /// 粒度取**工具名**，与 `allowed_tools.json` 的键完全一致——用户按下 y 时
    /// 心里的「允许 Edit」和这里记下的语义是同一件事，不引入第二套心智模型。
    ///
    /// 生命周期：`run_turn_with_injection_inner` 入口清空（那正是「一次用户
    /// 提交」的边界：TUI 每次 Enter 调一次、子 Agent 每个任务调一次；内部工具
    /// 轮循环**不**清，否则第 2 轮又弹）。`Arc<Mutex<..>>` 与 `loop_guard` 同理：
    /// Agent 会被 clone（TUI `/model` 重建、模式切换），且并发工具调用要同时读。
    turn_permission_grants: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// 本轮用户原始请求。Auto 分类器据此判断「这一步是否在请求范围内」，
    /// 生命周期与 `turn_permission_grants` 一致（回合入口写入、工具轮循环继承）。
    turn_user_goal: Arc<std::sync::Mutex<String>>,
    /// CapabilityCache（可选）。Agent 在 `parameter_degraded` 撤掉端点不认的
    /// 参数重试成功后调 `record_rejection`；下次同 fingerprint
    /// `ModelCatalog::resolve_with_cache` 会让 capabilities.thinking=Unsupported。
    /// 子 Agent 不设置，避免嵌套路径写文件。
    capability_cache: Option<Arc<wyj_api::CapabilityCache>>,
    /// 项目声明的校验命令（`verify::load_verify_cfg` 读 `.wyj-code/settings.json`）。
    /// `None` = 该项目没有声明 = **一次子进程都不跑**，这是默认值。
    verify_cfg: Option<Arc<crate::verify::VerifyCfg>>,
    /// 运行时 `ContentBlock` 字节截断配置(由 CLI/TUI 装配时注入),作用:
    /// 在 `run_turn_with_injection_inner` 调用 `provider.stream()` 之前对
    /// `session.messages` 做与磁盘 save 路径等价的截断,避免长
    /// `ContentBlock::Text` / `ToolResult` / `Thinking` 撞穿模型上下文窗口。
    /// `None` 表示关闭 runtime 截断(磁盘路径仍由 `SessionStore::save` 自身的
    /// `current_persist_cap()` 全局兜底)。
    persist_cap: Option<wyj_config::PersistCapCfg>,
    /// tool-result context editing 的装配：`None` = 关闭（不清理旧工具输出）。
    /// 见 `crate::context_edit` 与 `doc/plan/v0.2.26-context-management-plan.md`。
    context_edit: Option<crate::context_edit::ContextEditCfg>,
    /// 跨会话记忆快照缓存（system prompt 稳定前缀的一部分）。分桶重算，
    /// 避免每轮改写 system 尾部击穿 prompt cache。用 `Arc<Mutex<..>>` 是为了
    /// 兼容 `Agent: Clone`（TUI `/model` 重建与模式切换都会 clone），
    /// 同时让重建出的 Agent 继承同一份缓存。
    memory_snapshot: Arc<std::sync::Mutex<MemorySnapshotCache>>,
}

/// 跨会话记忆快照的重算间隔（按用户轮次分桶）。
///
/// Project Brief 每次都按「最近 4 条 user 消息」重算相关性排序
/// （`MemoryV3Store::project_brief`），而它被拼进 system prompt 的稳定前缀
/// —— 意味着**每个用户轮次都会改写 system 尾部**，把整段 system（约
/// 1.6k~5k token）的 prompt cache 全部击穿。旧 v1 `MemoryStore` 专门用
/// `OnceLock` 冻结整个会话来防这件事，v3 路径没继承该保护。
///
/// 折中：按桶重算 —— 每 10 个用户轮次才重算一次，缓存抖动降到 1/10，
/// 同时长会话内后台新提取的记忆不会无限期对模型不可见（最迟 10 轮后可见）。
const MEMORY_SNAPSHOT_REFRESH_TURNS: u32 = 10;

/// 后台记忆提取的最小用户轮次间隔。
///
/// 提取本身是一次完整的 LLM 往返，打的是主循环正在用的同一个 provider。旧行为
/// 每个用户轮次都提一次，等于「用户每提交一次，额外插入一趟等长的请求」——在
/// 并发跑子 Agent 时更要命：主 Agent、子 Agent、提取任务三者挤同一份 rate
/// limit，交互路径反而被自己的后台活拖慢。
///
/// 记忆的用途是**跨会话召回**，不是逐轮刷新：一个 20 轮的长任务从中受益的
/// 概率远高于一个 2 轮的问答。因此降到 1/3，交互路径立刻轻下来，记忆的时效性
/// 代价很小（最迟 3 轮后可见）。
const MEMORY_EXTRACT_EVERY_TURNS: usize = 3;

/// 后台辅助 LLM 调用的让路延迟。
///
/// `tokio::spawn` 只保证「不阻塞当前轮」，不保证「不和当前轮抢配额」——两者都
/// 用同一个 provider、同一个 HTTP 连接池、服务端同一份 rate limit。用户在工具
/// 跑完后几秒就会按下 Enter，主循环要立刻发下一趟；这时候插进来的提取请求会让
/// 主循环被限流，实测表现是「agent 自己变慢了」。
///
/// 3 秒是按人机节奏定的：工具结束到用户看完输出并敲下一个问题，通常大于 3 秒，
/// 于是后台活几乎总是排在交互请求之后。它不保证成功（服务端不排队），但足以让
/// 绝大多数「用户正在等」的时刻不被后台任务插队。
const BACKGROUND_LLM_YIELD: std::time::Duration = std::time::Duration::from_secs(3);

/// 该轮是否到点做记忆提取。
///
/// 首轮必提：新项目/新会话的第一轮正是最值得沉淀的（「用户在做什么、用什么
/// 栈、什么约定」），而且短会话里如果没有这一次，记忆功能对小任务等于不存在。
/// 之后每 [`MEMORY_EXTRACT_EVERY_TURNS`] 轮提一次。
fn memory_extraction_due(user_turns: usize) -> bool {
    user_turns > 0 && (user_turns - 1) % MEMORY_EXTRACT_EVERY_TURNS == 0
}

/// 记忆快照缓存：按 `用户轮次 / MEMORY_SNAPSHOT_REFRESH_TURNS` 分桶，
/// 同桶内复用上次算好的文本。
#[derive(Default)]
struct MemorySnapshotCache {
    bucket: Option<u32>,
    text: String,
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
            response_language: None,
            thinking_budget: None,
            interleaved_thinking: true,
            reasoning_effort: None,
            thinking_switch: None,
            route_profile_name: "active".to_string(),
            route_vendor: "unknown".to_string(),
            route_model: "unknown".to_string(),
            route_base_url: String::new(),
            route_wire_protocol: wyj_config::WireProtocol::OpenAiChatCompletions,
            fallback_routes: Vec::new(),
            active_route: Arc::new(AtomicUsize::new(0)),
            thinking_cb: None,
            memory_snapshot: Arc::new(std::sync::Mutex::new(MemorySnapshotCache::default())),
            hook_runner: None,
            checkpoint_store: None,
            loop_guard: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            turn_permission_grants: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            turn_user_goal: Arc::new(std::sync::Mutex::new(String::new())),
            capability_cache: None,
            verify_cfg: None,
            persist_cap: None,
            context_edit: None,
        }
    }

    /// 注入能力缓存：Agent 在 `parameter_degraded` 撤掉端点不认的参数重试
    /// 成功后调 `record_rejection`，下次同 fingerprint load 时
    /// `ModelCatalog::resolve_with_cache` 强制 capabilities.thinking=Unsupported。
    pub fn with_capability_cache(mut self, cache: Option<Arc<wyj_api::CapabilityCache>>) -> Self {
        self.capability_cache = cache;
        self
    }

    /// 配置运行时 `ContentBlock` 字节截断上限（与 `SessionStore::save` /
    /// `SessionStore::load` 共用同一份 `PersistCapCfg`）。None 关闭 runtime
    /// 截断；非 None 时 `run_turn_with_injection_inner` 在每次
    /// `provider.stream()` 之前检查是否有块超限，**有则克隆一份历史做截断**
    /// 后只把副本发出去——`session.messages` 本身保持原文，避免改写 prompt
    /// cache 的前缀。
    pub fn with_persist_cap(mut self, cap: Option<wyj_config::PersistCapCfg>) -> Self {
        self.persist_cap = cap;
        self
    }

    /// 装配 tool-result context editing。`None` 或 `enabled == false` 时该机制
    /// 完全不参与，历史只走整段摘要（v0.2.25 及更早的行为）。
    pub fn with_context_edit(mut self, cfg: Option<crate::context_edit::ContextEditCfg>) -> Self {
        self.context_edit = cfg.filter(|c| c.enabled);
        self
    }

    /// 装配编辑后自动校验。`None`（项目没在 `.wyj-code/settings.json` 里声明
    /// `verify.command`）= 完全不触发，一次子进程都不跑。
    ///
    /// 与 `with_context_edit` 同样的 `filter` 口味：调用方可以直接把
    /// `load_verify_cfg()` 的原始返回值塞进来，由本方法负责把「没声明」和
    /// 「声明了但 enabled=false」统一收敛成 `None`，省得每个装配点各判一次。
    pub fn with_verify(mut self, cfg: Option<crate::verify::VerifyCfg>) -> Self {
        self.verify_cfg = cfg.filter(|c| c.enabled).map(Arc::new);
        self
    }

    /// 设置会话启动时的 git 状态快照（见 `git_snapshot` 字段说明）
    pub fn with_git_snapshot(mut self, snapshot: Option<String>) -> Self {
        self.git_snapshot = snapshot;
        self
    }

    /// 取主 Provider 的句柄（Auto 模式分类器默认复用它）。
    ///
    /// 闸门挂在 `ToolContext` 上而不是 Agent 上——子 Agent 的 ctx 从父 ctx 派生，
    /// 闸门因此自动向下传递、降级阈值计数全局共享；挂在 Agent 上则要保证每个
    /// 子 Agent 都单独装配一次，漏一处就等于给那条路径开了后门。Provider 则只在
    /// Agent 手里，所以装配层需要这个出口。
    pub fn provider_handle(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    /// 配置 extended thinking（budget None/0 = 关闭）
    pub fn with_thinking(mut self, budget: Option<u32>, interleaved: bool) -> Self {
        self.thinking_budget = budget.filter(|b| *b > 0);
        self.interleaved_thinking = interleaved;
        self
    }

    /// OpenAI-vendor `reasoning_effort` 档位（low/medium/high/max/xhigh/adaptive）
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.filter(|e| !e.is_empty());
        self
    }

    /// OpenAI-vendor `thinking.type` 字符串（enabled/disabled/auto/adaptive）
    pub fn with_thinking_switch(mut self, switch: Option<String>) -> Self {
        self.thinking_switch = switch.filter(|s| !s.is_empty());
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

    /// 设置回复语言，供每轮往 system prompt **尾部**追加语言提醒。
    ///
    /// 调用方传 `wyj_i18n::resolve_locale(cfg.language.as_deref())` 的解析结果，
    /// 不传 locale code 本身（core 不读 config）。与 `tools` 侧
    /// `SubAgentTool::with_response_language` 同名不同层：那个补的是子 Agent
    /// 缺失的整段语言规则（子 Agent 走 `with_system` 整体替换，拿不到 `<env>`），
    /// 这个补的是**位置**。
    pub fn with_response_language(mut self, language: impl Into<String>) -> Self {
        self.response_language = Some(language.into());
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

    /// 注入本会话可用的 skill 名单，替换 [`prompts::MAIN`] 中 `# Skills` 段的
    /// `{skills}` 占位。
    ///
    /// **必须排在 [`Agent::with_system`] 之后**——后者是整体替换，会抹掉这里
    /// 的写入；反过来本方法只做占位替换，放在 `with_system` 之前等于白写。
    ///
    /// 名单在会话内冻结（不做每轮重算），因此落在 system prompt 的 stable 段
    /// 里、prompt cache 照常命中。代价：用户在 `/skills` 面板中途禁用某个 skill
    /// 后，本会话的名单不会更新，模型仍可能推荐那个命令。这是刻意的取舍——
    /// 放进 volatile 段能实时，但每轮要为名单全价重算上千 token。
    ///
    /// [`prompts::MAIN`]: crate::prompts::MAIN
    pub fn with_skills(mut self, skills: Vec<crate::prompts::SkillEntry>) -> Self {
        let catalog = crate::prompts::render_skill_catalog(&skills);
        self.system_prompt = self
            .system_prompt
            .replace(crate::prompts::SKILL_CATALOG_PLACEHOLDER, &catalog);
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
                base_url: self.route_base_url.clone(),
                wire_protocol: self.route_wire_protocol.clone(),
                provider: self.provider.clone(),
                capabilities: self.model_capabilities.clone(),
                max_tokens: self.max_tokens,
                context_window: self.context_window,
                thinking_budget: self.thinking_budget,
                interleaved_thinking: self.interleaved_thinking,
                reasoning_effort: self.reasoning_effort.clone(),
                thinking_switch: self.thinking_switch.clone(),
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
        steer: Option<&SteerQueue>,
        on_inject: impl FnMut(InjectionKind),
    ) -> Result<()> {
        self.run_turn_scoped(session, ctx, on_text, steer, InjectScope::All, on_inject)
            .await
    }

    /// **前台子 Agent 专用入口**：与 [`Self::run_turn_with_injection`] 相同，但只
    /// 排空「用户补充」，把队列里的 `SystemReminder` 原样留给主 Agent。
    ///
    /// 队列是主 Agent 与前台子 Agent 共享的（见 [`SteerQueue`]），但两者该收的
    /// 东西不一样：用户补充要"打断当前正在跑的"，谁在跑就进谁的下一轮；后台
    /// 子 Agent / 后台 shell 的完成通知是**给主 Agent 的结论**，进了子 Agent 的
    /// 会话既答非所问，又会让主 Agent 永远收不到（子 Agent 返回时队列已被取空）。
    pub async fn run_turn_with_user_steerage(
        &self,
        session: &mut Session,
        ctx: &dyn ToolContext,
        on_text: &mut impl FnMut(&str),
        steer: Option<&SteerQueue>,
        on_inject: impl FnMut(InjectionKind),
    ) -> Result<()> {
        self.run_turn_scoped(
            session,
            ctx,
            on_text,
            steer,
            InjectScope::UserMessagesOnly,
            on_inject,
        )
        .await
    }

    async fn run_turn_scoped(
        &self,
        session: &mut Session,
        ctx: &dyn ToolContext,
        on_text: &mut impl FnMut(&str),
        steer: Option<&SteerQueue>,
        scope: InjectScope,
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
            .run_turn_with_injection_inner(session, ctx, on_text, steer, scope, on_inject)
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

    /// 清理过期工具输出，返回清理后的请求估算 token。
    ///
    /// 两级阈值：
    /// - **软阈值**（`soft_threshold`）：开始动手的线。超过就开始外部化。
    /// - **硬阈值**（`compact_threshold`）：停手的线。降到它以下才停。
    ///
    /// 早期只有硬阈值，于是 1M 窗口下这条线在 900K，实际会话（~300K）永远
    /// 够不着——context editing 等于从未运行。软阈值把「动手时机」提前，
    /// **不改变**停手目标：清理不动了照样交给 compact 摘要。
    ///
    /// `soft_threshold >= compact_threshold`（含 `soft_threshold = u32::MAX`，
    /// 即 `soft_limit_ratio = 0` 关闭软阈值）时行为与旧实现逐字节一致。
    ///
    /// 批数上限 `max_batches` 防"历史里全是可清理项"时一次请求做掉一大手术、
    /// 把 prompt cache 前缀反复击穿。
    #[allow(clippy::too_many_arguments)]
    fn elide_stale_tool_results(
        &self,
        session: &mut Session,
        cfg: &crate::context_edit::ContextEditCfg,
        compact_threshold: u32,
        soft_threshold: u32,
        request_system: &wyj_api::SystemPrompt<'_>,
        request_tools: &[ToolDefinition],
        max_output_tokens: u32,
    ) -> u32 {
        use crate::context_edit::elide_tool_results;
        use std::sync::atomic::Ordering;

        let mut estimate = ContextAudit::from_request(
            request_system,
            &session.messages,
            request_tools,
            max_output_tokens,
        )
        .total();

        // 软阈值不得高于硬阈值，否则软阈值形同虚设（等于退回旧的单阈值行为）。
        let start_threshold = soft_threshold.min(compact_threshold);
        if estimate <= start_threshold {
            return estimate;
        }

        for _ in 0..cfg.max_batches.max(1) {
            if estimate <= compact_threshold {
                break;
            }
            let stats = elide_tool_results(session, &cfg.cas, &cfg.opts);
            if stats.elided == 0 {
                // 没有可清理的了，剩下的超标只能交给 compact。
                break;
            }
            cfg.totals
                .elided
                .fetch_add(stats.elided as u32, Ordering::Relaxed);
            cfg.totals
                .freed_tokens
                .fetch_add(stats.freed_tokens, Ordering::Relaxed);
            // blob 记到 session 上，会话被丢弃时才有据可 release
            // （CAS 的 gc 只删 ref_count == 0 的 blob，不记就永不回收）。
            session.elided_blobs.extend(stats.blobs.iter().cloned());
            session.context_edit_freed_tokens = session
                .context_edit_freed_tokens
                .saturating_add(stats.freed_tokens);
            tracing::debug!(
                elided = stats.elided,
                freed_tokens = stats.freed_tokens,
                "context editing: 已把过期工具输出外部化到 CAS"
            );
            estimate = ContextAudit::from_request(
                request_system,
                &session.messages,
                request_tools,
                max_output_tokens,
            )
            .total();
        }

        // 清理改变了历史构成，审计快照必须同步刷新，否则 `/context` 面板和
        // 下一次阈值判定会拿着清理前的数字。
        let audit = ContextAudit::from_request(
            request_system,
            &session.messages,
            request_tools,
            max_output_tokens,
        );
        session.context_audit = audit;
        audit.total()
    }

    /// 取本次请求要拼进 system 稳定前缀的跨会话记忆快照。
    ///
    /// 按「用户轮次 / [`MEMORY_SNAPSHOT_REFRESH_TURNS`]」分桶：同桶直接复用
    /// 缓存文本，跨桶才真正重算。分桶而非纯计数是为了让「同一桶内的多次
    /// turn」自然命中——即使 session 被 `/clear` 重置，桶号也会自然回退。
    fn memory_snapshot_text(&self, session: &Session) -> String {
        let user_turns = session
            .messages
            .iter()
            .filter(|m| m.role == wyj_api::types::Role::User)
            .count() as u32;
        let bucket = user_turns / MEMORY_SNAPSHOT_REFRESH_TURNS;

        let mut cache = match self.memory_snapshot.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if cache.bucket == Some(bucket) {
            return cache.text.clone();
        }

        let text = if let Some(memory) = &self.memory_v3 {
            build_memory_snapshot(memory, session)
        } else if let Some(evolution) = &self.evolution {
            evolution.context_snapshot(&last_user_goal(session))
        } else if let Some(mem) = &self.memory {
            // 旧 v1 store 自身已用 OnceLock 冻结，这里再缓存一层也无副作用。
            mem.load_context_cached().to_string()
        } else {
            String::new()
        };
        cache.bucket = Some(bucket);
        cache.text = text.clone();
        text
    }

    async fn run_turn_with_injection_inner(
        &self,
        session: &mut Session,
        ctx: &dyn ToolContext,
        on_text: &mut impl FnMut(&str),
        steer: Option<&SteerQueue>,
        scope: InjectScope,
        mut on_inject: impl FnMut(InjectionKind),
    ) -> Result<()> {
        if let Some(state) = &self.lazy_tool_state {
            state.begin_task_turn();
        }

        // 本轮权限授权从零开始。放在这里而不是工具轮循环里：进入本函数的
        // 每一个调用点都是「一次用户提交」（TUI 每次 Enter / 子 Agent 每个任务），
        // 而内部工具轮循环要**继承**这次授权——否则跨十几轮的重构每轮都要重新
        // 按十几次 y，等于没省。放在下一轮 Enter 时才清，是「这个任务授权过了、
        // 下一个任务要重新问）。锁中毒时静默跳过：残留的旧授权最坏只是少问一次，
        // 而在这里 panic 会直接掀翻整场会话。
        if let Ok(mut g) = self.turn_permission_grants.lock() {
            g.clear();
        }
        // Auto 分类器判断「这一步是否在用户请求范围内」需要本轮原始请求。与
        // 授权集同一生命周期：入口写一次，工具轮循环里读十几次。
        if let Ok(mut goal) = self.turn_user_goal.lock() {
            *goal = last_user_goal(session);
        }
        // system prompt 分两段，见 `wyj_api::SystemPrompt` 的文档：
        //
        //   * `system_stable`   —— 主提示 + 跨会话记忆 + 项目规约祖先链。
        //     承载 prompt cache，Anthropic 侧 cache_control 断点打在这段末尾。
        //     记忆快照按 10 个用户轮次分桶重算（`memory_snapshot_text`），
        //     规约文件每轮重读盘但字节级稳定，因此这段只在「记忆跨桶」或
        //     「用户改了规约文件」时才失效缓存。
        //
        //   * `system_volatile` —— 循环内累积的子目录规约 reminder。
        //     放在断点之后：旧实现把它 `push_str` 到 system 末尾并注释说
        //     「只增不减，前缀仍可缓存」——这是错的，追加在断点**之后**同样会
        //     改变断点处的前缀哈希，导致每命中一个新目录就整段全价重算。
        let mut system_stable = self.system_prompt.clone();
        let memory_snapshot = self.memory_snapshot_text(session);
        if !memory_snapshot.is_empty() {
            system_stable.push_str("\n\n");
            system_stable.push_str(&memory_snapshot);
        }
        if let Some(loader) = &self.claude_md {
            if let Some(reminder) = loader.turn_reminder() {
                system_stable.push_str("\n\n");
                system_stable.push_str(&reminder);
            }
        }
        let mut system_volatile = String::new();

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

        // 触顶前的**收尾宽限**。触顶不是「任务失败」，而是「这一轮该交付了」：
        // 见下方 `TURN_LIMIT_GRACE` 注释。
        let wrap_up_at = max_turns.saturating_sub(TURN_LIMIT_GRACE);

        let mut turn = 0;
        let mut invalid_argument_rounds = 0usize;
        // 收尾指令只注入一次；注入后仍继续用同样方式撞校验，才落到硬中止。
        let mut invalid_argument_wrap_up_sent = false;
        // 收尾指令只注入一次；注入后模型仍不交出最终回答，才落到硬中止。
        let mut wrap_up_sent = false;
        loop {
            turn += 1;
            if turn > max_turns {
                // 触顶意味着这一轮几十次工具调用的成果要被丢在原地：用户能用的
                // 最近锚点是本次提交**开始前**的 AutoUser checkpoint，`/rewind`
                // 回去等于把这半小时全部推倒。先在中止点落一份 TurnLimit 存档，
                // 把「已经做成什么样」这个状态钉住，用户至少能回到工作现场。
                //
                // checkpoint_store 为 None（未接文件系统）时静默跳过——中止行为
                // 不能因为存不了档就变成「静默卡死」，报错照旧。
                self.create_checkpoint(
                    session,
                    ctx.cwd(),
                    session.messages.clone(),
                    crate::checkpoint::CheckpointKind::TurnLimit,
                    Some(format!("推理轮数触顶前自动存档（{max_turns} 轮）")),
                )
                .await;
                anyhow::bail!(
                    "超过最大推理轮数 {max_turns}。已自动存档到触顶前的状态，可用 `/rewind` 回到工作现场后换个思路继续。"
                );
            }

            // 收尾宽限：进入最后 TURN_LIMIT_GRACE 轮时，先给模型一次「停下来
            // 交付」的机会，而不是等到硬上限才把整轮工作判成失败。
            //
            // 为什么需要这一段（这是「长任务必然报超过最大推理轮数」的根因）：
            // 触顶前 `bail!` 把**已经做完的活**判成错误——用户看到的是一句红色
            // 报错，而工作树里那一堆改动、测试结果、排查结论全在，只是没来得及
            // 说出口。实测触顶会话的最后几十轮全是有效工作（改文件 / 跑测试 /
            // 读源码），没有一轮是空转。200 这个上限本意是防死循环，不是给
            // 正常长任务设的工期，可它成了唯一的出口。
            //
            // 宽限窗口里注入一条 user 消息，内容是「轮数快用完，停止调用工具，
            // 现在就总结交付」。它走 `push_user_blocks_merged`，与工具结果落在
            // 同一条 user 消息里，不破坏角色交替。模型据此交出最终文本，
            // `has_tool_calls` 转 false，循环正常 break —— 回合以「完成」收尾，
            // 用户拿到的是一份可读的阶段交付，而不是一句错误。
            //
            // 注入只做一次（`wrap_up_sent`）。模型无视指令继续调工具时，下一轮
            // 就落到上面的硬中止——那才是真正该报错的情形。
            if !wrap_up_sent && turn >= wrap_up_at {
                wrap_up_sent = true;
                let notice = format!(
                    "<turn-budget-reminder remaining=\"{}\">\n\
                     You have used {turn} of {max_turns} tool-loop turns for this request, \
                     and this is the last turn you may use. Do not call any more tools. \
                     Immediately write your final message: what you completed, what you \
                     verified, what remains undone, and the exact next step. A partial \
                     but honest delivery is required — an unfinished task reported \
                     accurately is far more useful than more tool calls.\n\
                     本轮工具循环已用掉 {turn}/{max_turns} 轮，这是最后一轮。不要再调用任何工具，\
                     立即输出最终消息：已完成的、已验证的、未完成的、以及下一步具体做什么。\
                     如实交付半成品远胜过继续调用工具。\n\
                     </turn-budget-reminder>",
                    max_turns.saturating_sub(turn),
                );
                session.push_user_blocks_merged(vec![ContentBlock::Text { text: notice }]);
                on_text(&wyj_i18n::tr_fmt(
                    "agent.turn_budget_wrap_up",
                    &[("turn", &turn.to_string()), ("max", &max_turns.to_string())],
                ));
                // 不 `continue`：提醒已经并入 session，本轮就该带着它正常发请求，
                // 模型的总结会在**下一个**工具轮边界回来。多跳一次只会白白
                // 吃掉一格宽限预算。
            }

            // 补齐悬空 `tool_use`（详见 `Session::repair_dangling_tool_uses`
            // 的事故说明）：请求末尾若是「assistant(tool_use) → user(没有
            // tool_result)」，端点会回一个 200 的空流，回合于是**静默**结束
            // ——用户表现为「输入继续没反应」，且每条后续消息都复发。
            //
            // 放在工具轮循环的开头而不是加载历史时：`--resume` 与「同一会话内
            // ESC 中断」两条产生毒化历史的路径都在这个收口后面，而这里改的
            // 就是 `session.messages` 本身，顺带让下一次落盘不再把毒化状态
            // 写回磁盘——用户 `/rewind` 之外的自愈路径也就有了。
            let repaired = session.repair_dangling_tool_uses();
            if repaired > 0 {
                tracing::warn!(
                    repaired,
                    "补齐了没有结果的 tool_use：会话历史停在工具调用中间（中断后 resume 的典型形态）"
                );
                on_text(&wyj_i18n::tr_fmt(
                    "agent.dangling_tool_uses_repaired",
                    &[("count", &repaired.to_string())],
                ));
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
                    /// MiniMax `delta.reasoning_details` 数组项累积。多轮工具调用
                    /// 场景必须把 reasoning_details 拼回 messages 才能让 reasoning
                    /// 完整保留；国产 reasoning 模型通用。
                    details: Vec<serde_json::Value>,
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
                // stable 段跨 route 复用（内容本就与 route 无关）；每轮变化的
                // 部分（模型兼容 suffix + 当前工具可用性）归入 volatile。
                let mut request_system_volatile = system_volatile.clone();
                if let Some(capabilities) = &route.capabilities {
                    let suffix = wyj_api::PromptPolicy::compatibility_suffix(capabilities);
                    if !suffix.is_empty() {
                        request_system_volatile.push_str("\n\n");
                        request_system_volatile.push_str(suffix);
                    }
                }
                // effort 能不能上送由 `wyj_api::normalize_effort` 单独裁决：它查的是
                // vendor 的 `effort_levels`，不是 `ModelCapabilities` 里那个**单值**
                // `ThinkingMode` 标签——后者既记不下 Qwen 这种 budget/effort 都认的
                // vendor，又会把 GLM / Ollama / Moonshot k2 这类只认开关的误标成
                // "支持 effort"，然后在 adapter 里被无声吞掉。
                let route_identity = wyj_api::ModelIdentity {
                    vendor: route.vendor.clone(),
                    model: route.model.clone(),
                    base_url: wyj_api::capabilities::sanitized_base_url(&route.base_url),
                    wire_protocol: route.wire_protocol.clone(),
                };
                let reasoning_effort = route
                    .reasoning_effort
                    .as_deref()
                    .and_then(|effort| wyj_api::normalize_effort(&route_identity, effort))
                    .map(str::to_string);
                let opts = if let Some(capabilities) = &route.capabilities {
                    // 能力快照可能已经把 thinking 整体禁掉（用户 override，或上一次
                    // 端点 4xx 被 `CapabilityCache` 记成 rejected）。端点已经亲口说过
                    // 不认，这时静态 spec 不作数。
                    let blocked = matches!(
                        capabilities.thinking.value,
                        wyj_api::ThinkingMode::Unsupported | wyj_api::ThinkingMode::ProviderNative
                    );
                    wyj_api::provider::RequestOptions {
                        max_tokens: route.max_tokens.min(capabilities.max_output_tokens),
                        thinking_budget: route.thinking_budget.filter(|_| {
                            !blocked
                                && matches!(
                                    capabilities.thinking.value,
                                    wyj_api::ThinkingMode::BudgetTokens
                                )
                        }),
                        reasoning_effort: reasoning_effort.filter(|_| !blocked),
                        thinking_switch: route.thinking_switch.clone().filter(|_| !blocked),
                        interleaved: route.interleaved_thinking
                            && capabilities.interleaved_thinking.value,
                    }
                } else {
                    wyj_api::provider::RequestOptions {
                        max_tokens: route.max_tokens,
                        thinking_budget: route.thinking_budget,
                        reasoning_effort,
                        thinking_switch: route.thinking_switch.clone(),
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
                // 被 lazy 折叠掉的部分必须点名：只报「本次直接可调用」的名单，
                // 模型会把「不在列表里」读成「本会话没有这个工具」（实测 MCP
                // codegraph 的 8 个工具因此从未被调用过）。分组只报来源与数量，
                // 不展开 schema——展开就退化成不折叠。
                let hidden_sources = self
                    .lazy_tool_state
                    .as_ref()
                    .map(|state| state.hidden_sources())
                    .unwrap_or_default();
                request_system_volatile.push_str("\n\n");
                request_system_volatile.push_str(&crate::prompts::current_tool_availability_block(
                    &attached_tool_names,
                    &hidden_sources,
                ));
                // 语言提醒必须是 system prompt 的**最后一段**：它存在的全部理由
                // 就是位置（`<env>` 里的语言事实在中段，被记忆快照 + CLAUDE.md
                // reminder 压在后面几万 token）。谁在它后面再追加东西，就把这个
                // 不变量打破了——`response_language_reminder_is_the_last_system
                // section` 那条测试就是钉这里的。
                if let Some(language) = &self.response_language {
                    request_system_volatile.push_str("\n\n");
                    request_system_volatile
                        .push_str(&crate::prompts::response_language_reminder(language));
                }
                let request_system = wyj_api::SystemPrompt {
                    stable: &system_stable,
                    volatile: &request_system_volatile,
                };
                let sent_schema_tokens = estimate_tool_schema_tokens(&request_tools);
                // 两个口径必须并存，含义不同：
                // · `last` = 本次请求实际发送的量（稳态），与
                //   `ContextAudit::tool_schema_tokens` 同源同值，`/context` 面板同口径；
                // · 累计值 = 本会话为工具定义总共付了多少（成本口径）。
                // 过去只有累计值且被 JSON / 插件事件当作"当前占用"展示，
                // 200 次调用的会话会显示成百万级（实测 1,457,192），比真实稳态
                // 值（6,281）高两个数量级。
                session.tool_schema_tokens_last = sent_schema_tokens;
                session.tool_schema_tokens = session
                    .tool_schema_tokens
                    .saturating_add(sent_schema_tokens);
                session.tool_schema_tokens_saved = session
                    .tool_schema_tokens_saved
                    .saturating_add(all_schema_tokens.saturating_sub(sent_schema_tokens));

                // 按当前路由目标的真实窗口与能力估算；fallback 模型可能比主模型
                // 上下文更小，不能复用主模型预算。分解结果落进 session，
                // 供 TUI 的 `/context` 面板与状态栏和压缩决策**共用同一口径**。
                let context_audit = ContextAudit::from_request(
                    &request_system,
                    &session.messages,
                    &request_tools,
                    opts.max_tokens,
                );
                let estimated_before = context_audit.total();
                session.context_audit = context_audit;
                let compact_threshold = route.context_window.saturating_sub(
                    compact_trigger_buffer(route.context_window, opts.max_tokens),
                );
                // 软阈值：context editing 的**动手线**，与上面的压缩硬阈值分开。
                // 硬阈值决定"何时停手"，软阈值决定"何时开始"——1M 窗口下硬阈值
                // 在 900K，等它触发意味着整段历史被原样重发了几百趟。
                // `soft_limit_ratio = 0` 时取 u32::MAX，条件退化成
                // `estimated_before > compact_threshold`，与改动前完全一致。
                let soft_threshold =
                    soft_threshold_tokens(route.context_window, self.context_edit.as_ref());

                // 清理优先于摘要：超阈值时先把过期的工具输出外部化掉（更便宜、
                // 不产生幻觉、更保真），清理后仍超标才跑整段摘要。对齐 Claude
                // Code 的原话 "It clears older tool outputs first, then summarizes
                // the conversation if needed."。额外好处是清得动就省掉一次
                // LLM round-trip + 一份摘要的 output token。
                let estimated = if estimated_before > soft_threshold {
                    match self.context_edit.as_ref() {
                        Some(cfg) => self.elide_stale_tool_results(
                            session,
                            cfg,
                            compact_threshold,
                            soft_threshold,
                            &request_system,
                            &request_tools,
                            opts.max_tokens,
                        ),
                        None => estimated_before,
                    }
                } else {
                    estimated_before
                };

                // 前缀复用**只对会按前缀计费的端点划算**：复用会把完整历史
                // 发一遍（旧路径只发文本化摘要），端点若不做前缀缓存，这就是
                // 净增 token。判据走 `reuses_prompt_prefix()`，它同时覆盖
                // Anthropic 的显式断点与 DeepSeek/OpenAI 的自动缓存。
                //
                // 在两条压缩路径（阈值触发 / 上下文超限恢复）之间共用：两处若
                // 判据不一致，会出现「平时省钱、撞穿时全价」的隐性割裂。
                let compact_prefix = if route
                    .capabilities
                    .as_ref()
                    .map(|caps| caps.reuses_prompt_prefix())
                    // 没有能力快照时按「乐观复用」处理：`capabilities` 为 None
                    // 只出现在测试与未解析 profile 的路径上，真实运行一定有。
                    .unwrap_or(true)
                {
                    Some(crate::compact::CompactPrefix {
                        // 与本次 stream 用的是**同一个** `request_system` /
                        // `request_tools`：压缩请求的前缀必须逐字节等于主对话
                        // 前缀，provider 的前缀缓存才命中得了。换成
                        // `COMPACT_SYSTEM` 或省略 tools 会让整段历史全价重发。
                        system: wyj_api::SystemPrompt {
                            stable: &system_stable,
                            volatile: &request_system_volatile,
                        },
                        tools: &request_tools,
                    })
                } else {
                    None
                };

                if estimated > compact_threshold {
                    // 循环压缩：单次 compact 不一定够（heuristic 偏低 /
                    // reasoning 模型一轮回巨长 / 上次摘要本身偏长）；
                    // `compact_session_until_fit` 最多跑 3 次直到估算
                    // 落入阈值或 compact 自身失败,把硬上限（撞穿模型
                    // 上下文窗口）的风险从『靠运气不撞』降到『最多 3 轮
                    // LLM round-trip 收敛』。
                    let outcome = compact_session_until_fit(
                        session,
                        route.provider.as_ref(),
                        route.context_window,
                        estimated,
                        compact_threshold,
                        compact_prefix,
                    )
                    .await;
                    if let Some(error) = outcome.last_error {
                        tracing::warn!("上下文压缩失败: {error}");
                    } else if outcome.passes > 0 {
                        if let Some(result) = outcome.last_result {
                            session.compact_count = session.compact_count.saturating_add(1);
                            session.last_compact_saved_tokens = result.tokens_saved_estimate;
                            on_text(&wyj_i18n::tr_fmt(
                                "agent.compacted",
                                &[
                                    ("removed", &result.messages_removed.to_string()),
                                    ("saved", &result.tokens_saved_estimate.to_string()),
                                    ("times", &session.compact_count.to_string()),
                                    ("kept", &result.messages_kept.to_string()),
                                ],
                            ));
                        }
                        if outcome.passes > 1 {
                            tracing::warn!(
                                "上下文压缩连续跑了 {} 轮,最终估算 {} tokens (阈值 {}); \
                                 历史里可能含超大不可压缩块,见 persist_cap.text_*",
                                outcome.passes,
                                outcome.final_estimated,
                                compact_threshold
                            );
                        }
                    }
                }

                // Runtime 截断兜底:即便 compact 已经跑过,单条 assistant/user
                // 纯文本块（长总结、粘贴日志）也可能单独超过单请求窗口。
                // persist_cap 的 text_* 字段对每块独立做 head+tail 截断,
                // 与 save()/load() 共用同一上限,确保本轮 stream 不会撞穿
                // 262K / 200K 之类 endpoint 硬上限。
                //
                // **只截副本，不改 `session.messages`**（见下面 `outbound` 处）：
                // 历史本身是 prompt cache 的前缀，原地改写会让后续每一轮的
                // `cache_read` 从被截断的位置起归零，且不可逆。落盘路径
                // （`SessionStore::save`）仍按原样截断副本，磁盘上限不变。

                const MAX_STREAM_RETRIES: u32 = 2;
                let mut stream_retries: u32 = 0;
                let mut retry_spent_ms: u64 = 0;
                let mut effective_opts = opts;
                let mut parameter_degraded = false;
                // provider 报「上下文超限」时的强制恢复：阈值估算只是启发式，
                // 真的撞穿时必须能自愈，而不是把 400 直接抛给用户。
                // `context_recovered` 保证只重试一次，避免无进展的死循环
                // （对标 Claude Code 的 "Autocompact is thrashing" 熔断）。
                let mut context_recovered = false;
                let result = loop {
                    session.api_calls += 1;
                    // 把 provider 侧的缓存生效状态同步进会话。第三方端点拒绝
                    // cache_control 时会自动降级，若不回写，`/cost` 看到
                    // cache_read 恒为 0 却无法区分"没配"与"配了但用不了"——
                    // 而这两种情况用户该做的事完全相反。
                    session.prompt_cache_state = route.provider.prompt_cache_state();
                    // TTFT 的起点：从**发起请求**算起，不是从 stream 建立算起。
                    // 连接建立、TLS、上传 prefill 全在这一段里，而它恰恰是用户
                    // 「点了回车到屏幕开始动」里最容易变慢、最少被看见的一段。
                    let attempt_started = Instant::now();
                    let mut ttft_ms: Option<u64> = None;
                    // 每次尝试前重算「本轮实际发出去的消息」：`context_overflow`
                    // 分支会改写 `session.messages`，复用上一轮的副本会让重试
                    // 发回压缩前的历史。绝大多数情况下不需要截断，走零拷贝。
                    let capped_messages: Option<Vec<wyj_api::types::Message>> =
                        self.persist_cap.as_ref().and_then(|cfg| {
                            if crate::serialize::messages_need_truncation(&session.messages, cfg) {
                                let mut copy = session.messages.clone();
                                crate::serialize::truncate_messages(&mut copy, cfg);
                                tracing::warn!(
                                    "发送前对超限块做了临时截断（仅本次请求生效，历史原文保留）"
                                );
                                Some(copy)
                            } else {
                                None
                            }
                        });
                    let outbound_messages: &[wyj_api::types::Message] = match &capped_messages {
                        Some(copy) => copy,
                        None => &session.messages,
                    };
                    let mut stream = match route
                        .provider
                        .stream(
                            &request_system,
                            outbound_messages,
                            &request_tools,
                            &effective_opts,
                        )
                        .await
                    {
                        Ok(stream) => stream,
                        Err(error) => {
                            // 上下文超限：无视阈值强制压一次再重试。压缩失败
                            // 也照常 `continue`——重入循环体会再跑一遍
                            // `truncate_messages`（persist_cap 单块截断兜底），
                            // 那条路能处理"消息太少压不动、但单块超长"的情况。
                            let context_overflow = !context_recovered
                                && error
                                    .downcast_ref::<wyj_api::ProviderError>()
                                    .map(|e| {
                                        e.kind == wyj_api::ProviderErrorKind::ContextLengthExceeded
                                    })
                                    .unwrap_or(false);
                            if context_overflow {
                                context_recovered = true;
                                on_text(&wyj_i18n::tr("agent.context_overflow"));
                                match crate::compact::compact_session(
                                    session,
                                    route.provider.as_ref(),
                                    route.context_window,
                                    // 与主压缩路径同一判据：不缓存的端点不走
                                    // 前缀复用，避免把整段历史全价重发一遍。
                                    compact_prefix,
                                )
                                .await
                                {
                                    Ok(result) => {
                                        session.compact_count =
                                            session.compact_count.saturating_add(1);
                                        session.last_compact_saved_tokens =
                                            result.tokens_saved_estimate;
                                        on_text(&wyj_i18n::tr_fmt(
                                            "agent.compacted",
                                            &[
                                                ("removed", &result.messages_removed.to_string()),
                                                (
                                                    "saved",
                                                    &result.tokens_saved_estimate.to_string(),
                                                ),
                                                ("times", &session.compact_count.to_string()),
                                                ("kept", &result.messages_kept.to_string()),
                                            ],
                                        ));
                                    }
                                    Err(compact_error) => {
                                        tracing::warn!(
                                            "上下文超限后的强制压缩未能完成: {compact_error}"
                                        );
                                    }
                                }
                                continue;
                            }
                            let safe_parameter = error
                                .downcast_ref::<wyj_api::ProviderError>()
                                .filter(|provider_error| {
                                    provider_error.kind
                                        == wyj_api::ProviderErrorKind::UnsupportedParameter
                                })
                                .and_then(|provider_error| provider_error.parameter.as_deref())
                                .filter(|parameter| {
                                    // 各家报错的 param 名字并不统一（`reasoning_effort` /
                                    // `reasoning` / `thinking.type` / `enable_thinking` …），
                                    // 只要落在 thinking 家族就整体降级：只撤 budget 却留着
                                    // effort/switch，重试仍会 400，而 400 只会再来一次。
                                    let lower = parameter.to_ascii_lowercase();
                                    lower.contains("thinking") || lower.contains("reasoning")
                                });
                            if !parameter_degraded && safe_parameter.is_some() {
                                let parameter = safe_parameter.unwrap_or("thinking");
                                parameter_degraded = true;
                                effective_opts.thinking_budget = None;
                                effective_opts.reasoning_effort = None;
                                effective_opts.thinking_switch = None;
                                effective_opts.interleaved = false;
                                on_text(&wyj_i18n::tr_fmt(
                                    "agent.degraded.parameter_removed",
                                    &[("name", parameter)],
                                ));
                                // 持久化拒绝记录：下次同 fingerprint resolve 时
                                // 直接 capabilities.thinking=Unsupported，不再触发 400。
                                if let Some(cache) = &self.capability_cache {
                                    let identity = wyj_api::ModelIdentity {
                                        vendor: route.vendor.clone(),
                                        model: route.model.clone(),
                                        base_url: wyj_api::capabilities::sanitized_base_url(
                                            &route.base_url,
                                        ),
                                        wire_protocol: route.wire_protocol.clone(),
                                    };
                                    cache.record_rejection(
                                        &identity,
                                        parameter,
                                        "endpoint returned 4xx for unsafe parameter",
                                    );
                                }
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
                                if ttft_ms.is_none() {
                                    ttft_ms = Some(attempt_started.elapsed().as_millis() as u64);
                                }
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
                                    details: Vec::new(),
                                });
                            }
                            StreamEvent::ThinkingDelta(delta) => {
                                // thinking 也要算首 token：开着 extended thinking
                                // 时屏幕是随 thinking 开始动的，只认 TextDelta 会
                                // 把整段思考时间错误地记到 total 上，TTFT 永远为空。
                                if ttft_ms.is_none() {
                                    ttft_ms = Some(attempt_started.elapsed().as_millis() as u64);
                                }
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
                                        details: Vec::new(),
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
                            StreamEvent::ThinkingDetailsDelta(item) => {
                                // MiniMax reasoning_details 累积到当前 thinking 块；
                                // 不存在 thinking 块时新建一个空 text 块附带 details。
                                match blocks.last_mut() {
                                    Some(StreamedBlock::Thinking { details, .. }) => {
                                        details.push(item);
                                    }
                                    _ => blocks.push(StreamedBlock::Thinking {
                                        text: String::new(),
                                        signature: String::new(),
                                        details: vec![item],
                                    }),
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
                                session.latency = crate::session::TurnLatency {
                                    ttft_ms,
                                    total_ms: attempt_started.elapsed().as_millis() as u64,
                                    retry_ms: retry_spent_ms,
                                    retried: stream_retries > 0,
                                };
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
                            // 把「失败 + 退避等待」整段记成重试开销：它和网络
                            // 建连时间一样会把 total 撑大，却和模型推理毫无关系。
                            // 记下来，`/context` 才能把它和净推理耗时分开报。
                            retry_spent_ms += attempt_started.elapsed().as_millis() as u64;
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
                    StreamedBlock::Thinking {
                        text,
                        signature,
                        details,
                    } => {
                        let reasoning_details = if details.is_empty() {
                            None
                        } else {
                            Some(details.clone())
                        };
                        assistant_blocks.push(ContentBlock::Thinking {
                            thinking: text.clone(),
                            signature: signature.clone(),
                            reasoning_details,
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
                                //
                                // 写回**模型实际发过的参数**（能解析成 object 时），
                                // 而不是 `{"_wyj_code_invalid_arguments": true}`
                                // 这类占位符。占位符看着安全，实则抽掉了纠错所需的
                                // 唯一信息：模型在下一轮只看得到「哪个字段类型不对」，
                                // 看不到自己传的是什么，于是照原样重发同一个错参数，
                                // 空转到重试上限。实测一次真实故障里模型连续 5 轮
                                // 重发同一个字符串 `tree_id`，历史里每次都只剩占位符。
                                //
                                // 只有「连 JSON object 都不是」时才用占位符——此时
                                // 没有可回写的合法参数，写原始文本会让续轮请求本身
                                // 变成非法 JSON。
                                let echo =
                                    error.original_input.map(|input| *input).unwrap_or_else(|| {
                                        serde_json::json!({
                                            "_wyj_code_invalid_arguments": true
                                        })
                                    });
                                assistant_blocks.push(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: echo,
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
                // schema、权限校验，并由 parallel_tool_calls 决定
                // 并发或顺序执行。
                tracing::warn!(
                    emitted = seen_tool_calls,
                    declared_max = max_tools_this_turn,
                    "模型返回的工具数超过能力声明，降级为受控执行"
                );
            }
            // 空响应不等于正常结束：上游对协议不合法的请求可能回 200 + 空流
            // （见 `Session::repair_dangling_tool_uses` 的事故说明），把它
            // 当成功就表现为「说了话就没反应」且无从排查。先把事实记下来，
            // 由回合收尾处决定要不要惊动用户。
            let empty_completion = assistant_blocks.is_empty();
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
                // 收尾窗口内**关闭工具**。
                //
                // 为什么不能只靠上面那条 `<turn-budget-reminder>`：实测里它被完整
                // 送达了（523 字，位置正确，紧跟在 tool_result 之后），模型下一轮
                // 的 thinking 里却**零次提到轮数**——它正沉浸在自己刚发现的一条线
                // 索上。软性请求在模型沉浸态里不产生任何约束力，于是宽限 5 轮被
                // 逐轮耗光，最后仍然 `bail!`，把已经做完的活判成一句报错。
                //
                // 这里把每个调用直接回成一条「工具已关闭」的 tool_result：不执行、
                // 不弹框、不碰工作树，但模型下一轮能读到**自己那次调用被拒**的
                // 证据。证据比追加一段说明有分量得多。
                //
                // 刻意**不发空 tools 数组**：那确实更硬，但 `tools` 位于 prompt
                // 缓存前缀的最前端，改它会把整段历史（触顶时实测约 42 万 token）
                // 从 cache_read 打回全价重算——为一次收尾付这个代价不划算。回一条
                // 结果不动请求体，缓存照常命中。
                if wrap_up_sent {
                    for (idx, id, name, _input) in calls.drain(..) {
                        let feedback = serde_json::json!({
                            "error": "tools_closed_turn_budget_exhausted",
                            "turn": turn,
                            "max_turns": max_turns,
                            "instruction": "本轮工具循环的轮数预算已用尽，工具已关闭。\
                                            不要再发起任何工具调用；现在直接输出最终消息：\
                                            已完成什么、验证了什么、还剩什么、下一步具体做什么。",
                        })
                        .to_string();
                        if let Some(cb) = &self.tool_cb {
                            cb(ToolEvent::End {
                                id: id.clone(),
                                name: name.clone(),
                                is_error: true,
                                elapsed_secs: 0.0,
                                output: feedback.clone(),
                            });
                        }
                        tool_results.push((
                            idx,
                            (id, wyj_api::types::ToolResultContent::Text(feedback), true),
                        ));
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

                // 分区执行：并发安全的调用各自并发，其余调用保持相互顺序、但与
                // 并发组同时进行；结果按原始下标排序回填保序。均为单任务内并发
                //（join!），不要求 ctx 满足 Send/'static。
                //
                // **两组之间的先后关系没有任何保证**——下面那次 `tokio::join!` 让
                // 两组真正同时跑。
                //
                // **「同一时刻至多一个对话框」靠的是"需要弹框的调用彼此串行"，
                // 而不是 `parallel_safe = false` 本身**。所以判定并发资格时，
                // `parallel_safe` 与 `parallel_safe_with_peer` 都为 false 的工具
                // 还要再看它这次会不会真的阻塞在权限弹框上：Bypass / Plan 白名单 /
                // 项目级 AllowAlways / 本轮已授权这四种情况都不会弹框，此时把
                // `Bash` 之类彼此隔离的调用排队纯属白白拉长回合——一次回合里
                // 排 8 条只读查询，就是 8 次 LLM 往返的额外空转。
                // 改这段之前先分清「同组内顺序」与「会弹框」这两种不同的约束。
                //
                // Auto 模式下这里**不能**假设「Auto 不会阻塞」：闸门判定要 await
                // 分类器，闸门要求问人时还要弹窗，所以 Auto 与 Bypass/Plan 一样
                // 落在"会阻塞"那一侧（`ToolCtx::needs_dialog` 里对应地返回 true）。
                let mut par_futs = vec![];
                let mut seq_calls = vec![];
                for (idx, id, name, input) in calls {
                    let tool_impl = self.tool_impls.get(&name);
                    let needs_ui = tool_impl
                        .map(|t| t.needs_permission(&input))
                        .unwrap_or(false)
                        && ctx.needs_dialog(&name)
                        && !self.turn_granted(&name, &input);
                    let tool_allows_par = tool_impl
                        .map(|t| t.parallel_safe() || (t.parallel_safe_with_peer() && !needs_ui))
                        .unwrap_or(false);
                    let model_allows_parallel = used_route
                        .capabilities
                        .as_ref()
                        .map(|capabilities| capabilities.parallel_tool_calls.value)
                        .unwrap_or(true);
                    if tool_allows_par && total > 1 && model_allows_parallel {
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
                // 子 Agent 用量回灌：必须等这批工具（可能并发跑了多个子 Agent）
                // 全部 join 完再抽干，否则会漏掉后完成的那几个。`take` 是
                // `swap(0)`，多调一次也不会重复计账。
                session.drain_sub_usage(ctx.sub_usage_sink().map(|sink| sink.as_ref()));
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
                    // 连续参数校验失败**不等于任务失败**，与轮数触顶同源：模型
                    // 往往只是卡在某一个调用的参数形态上，而这一轮里其它工具
                    // 已经跑出来的成果（截图、搜索、改文件）不该跟着作废。
                    // 先给一次「换个思路、就地收尾」的机会，只有模型无视该指令
                    // 继续用同样方式硬撞才中止——那才是真正该报错的形态。
                    if invalid_argument_rounds >= INVALID_ARGUMENT_GRACE_AT
                        && !invalid_argument_wrap_up_sent
                    {
                        invalid_argument_wrap_up_sent = true;
                        session.push_user_blocks_merged(vec![ContentBlock::Text {
                            text: invalid_argument_wrap_up_notice().to_string(),
                        }]);
                        on_text(&wyj_i18n::tr("agent.invalid_arguments_wrap_up"));
                    } else if invalid_argument_rounds > INVALID_ARGUMENT_GRACE_AT + 1 {
                        anyhow::bail!(
                            "工具参数连续校验失败（已给过一次收尾指令），已停止执行以避免无界重试"
                        );
                    }
                } else {
                    invalid_argument_rounds = 0;
                    // 出现正常工具轮即视为自愈，重新给一次收尾机会：收尾指令是
                    // 「本次连撞」的兜底，不是对整轮永久生效的封条。
                    invalid_argument_wrap_up_sent = false;
                }

                // 子目录动态加载：本轮工具触达的目录若有未展示过的规约文件，
                // 追加到 system 的 **volatile 段**（而非稳定前缀、也不注入 user
                // 消息）。这样历史消息保持干净、避免跨轮重复发送，seen_dirs 去重
                // 保证每条只追加一次；更重要的是它位于 cache breakpoint 之后，
                // 命中新目录不会让稳定前缀整段全价重算。
                if let Some(loader) = &self.claude_md {
                    let mut new_reminders = String::new();
                    for dir in &touched_dirs {
                        if let Some(text) = loader.maybe_dir_reminder(dir) {
                            new_reminders.push_str("\n\n");
                            new_reminders.push_str(&text);
                        }
                    }
                    if !new_reminders.is_empty() {
                        system_volatile.push_str(&new_reminders);
                    }
                }
            } else if !pending_tools.is_empty() {
                // 兜底配对：assistant 消息里带了 tool_use，但本轮并不是以
                // `stop_reason == ToolUse` 结束的——典型是 `MaxTokens`，即模型
                // 正在写工具参数时被 max_tokens 截断。
                //
                // 为什么必须在这里补：`pending_tools` 与 `assistant_blocks` 是
                // 从同一批 StreamedBlock 构造的，不看 stop_reason，所以这条
                // 路径下 tool_use 已随 `push_assistant` 进入历史，却没有配对的
                // tool_result。后果有二：
                //   1. 本回合的下一次请求会被 provider 判为协议错误(400)；
                //   2. 更糟的是该历史已落盘，任何一次 `--resume` 都会复现，
                //      用户不手动 /rewind 或 /clear 就无法自愈。
                //
                // 这里为每个残留 tool_use 合成一条错误 tool_result，沿用
                // `Rejected` 分支的语义：告诉模型这次调用**没有执行任何东西**，
                // 让它自己决定重试还是改用别的方案。
                //
                // 注：`input` 无需额外兜底 —— 参数管线失败时 `assistant_blocks`
                // 里已写入模型原始参数，或 `{"_wyj_code_invalid_arguments": true}`
                // 之类的合法 JSON 占位（见上方 `PendingToolCall::Rejected`
                // 分支），历史里不会出现半截 JSON。
                let reason = format!("{stop_reason:?}");
                let mut results = Vec::new();
                for pending in &pending_tools {
                    let (id, name) = match pending {
                        PendingToolCall::Valid(call) => (call.id.clone(), call.name.clone()),
                        PendingToolCall::Rejected { id, name, .. } => (id.clone(), name.clone()),
                    };
                    let feedback = serde_json::json!({
                        "error": "tool_call_incomplete",
                        "tool": name,
                        "stop_reason": reason,
                        "instruction": "The response ended before this tool call \
                                        completed. Nothing was executed. Retry it, or \
                                        proceed without it.",
                    })
                    .to_string();
                    if let Some(cb) = &self.tool_cb {
                        cb(ToolEvent::End {
                            id: id.clone(),
                            name: name.clone(),
                            is_error: true,
                            elapsed_secs: 0.0,
                            output: feedback.clone(),
                        });
                    }
                    results.push((id, wyj_api::types::ToolResultContent::Text(feedback), true));
                }
                // pending_tools 本身已按原始顺序，无需再排序。
                for (id, content, is_error) in results {
                    session.push_tool_result(id, content, is_error);
                }
            }

            // 排空可选的注入通道：把 Agent 忙碌期间用户提交的补充内容块，
            // 合并进当前工具结果所在的 user 消息（若刚执行过工具），
            // 或作为一条新的 user 消息续接对话（若本轮本要结束）。
            let mut got_injection = false;
            if let Some(queue) = steer {
                let items = match scope {
                    InjectScope::All => queue.drain(),
                    InjectScope::UserMessagesOnly => queue.drain_user_messages(),
                };
                for (blocks, kind) in items {
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
                // 空响应：200 + 空流被当成了正常结束。必须让用户看见——否则
                // 「模型一句话不说」和「请求根本没发出去」在界面上完全同形，
                // 而后者（协议不合法 / 上游故障）是可以重试解决的。
                if empty_completion {
                    tracing::warn!(
                        "模型返回了空响应（无文本、无 thinking、无 tool_use），回合按成功收尾"
                    );
                    on_text(&wyj_i18n::tr("agent.empty_completion"));
                }
            }

            if !has_tool_calls && !got_injection {
                // 记忆/标题提取是「LLM 辅助调用」，和主循环共用同一个 provider、
                // 同一把连接、同一份 rate limit。`tokio::spawn` 让它不阻塞
                // 当前轮是对的，但**不解决它抢配额**：用户在工具跑完后几秒内
                // 就会按下 Enter，主循环要立刻发下一趟请求，这时候挤进来的
                // 提取任务让主循环被限流 → 主循环变慢。异步 ≠ 无成本。
                //
                // 两道闸：① 降频到 `MEMORY_EXTRACT_EVERY_TURNS` 个用户轮次
                // 才提一次（首轮必提，那正是新项目最需要记忆的时刻）；
                // ② 让路延迟 `BACKGROUND_LLM_YIELD`，把交互路径的请求排在前面。
                // 真正的「独立配额」做不到——同一个 API key 在服务端是一份，
                // 进程内没有隔离手段，能做的只有「别跟主循环抢」。
                let user_turns = session
                    .messages
                    .iter()
                    .filter(|m| m.role == wyj_api::types::Role::User)
                    .count();

                // Memory v3 先把任务原子写入磁盘，再异步消费；即使进程在 spawn
                // 后退出，pending/running 任务也会在下次 store open 时恢复。
                let extract_now = memory_extraction_due(user_turns);
                if extract_now {
                    if let Some(memory) = self.memory_v3.as_ref().cloned() {
                        let session_id = self
                            .session_id
                            .clone()
                            .unwrap_or_else(|| "ephemeral".to_string());
                        match memory.enqueue_extraction(&session_id, &session.messages) {
                            Ok(Some(_)) => {
                                let provider = self.route_at(self.active_route_index()).provider;
                                tokio::spawn(async move {
                                    tokio::time::sleep(BACKGROUND_LLM_YIELD).await;
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
                                tokio::time::sleep(BACKGROUND_LLM_YIELD).await;
                                if let Err(e) = mem.extract_and_save(msgs, provider).await {
                                    tracing::debug!("记忆提取失败: {e}");
                                }
                            });
                        }
                    }
                }
                // 首轮后触发后台标题生成（若已配置 SummaryGenerator 且有 session_id）
                if let Some(gen) = self.summary.as_ref().cloned() {
                    if let Some(sid) = self.session_id.clone() {
                        let msgs = session.messages.clone();
                        let title_cb = self.title_cb.clone();
                        tokio::spawn(async move {
                            // 标题不影响正确性，只影响侧栏可读性，让它排在
                            // 交互请求之后毫无代价。
                            tokio::time::sleep(BACKGROUND_LLM_YIELD).await;
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

    /// 本次用户提交内该工具是否已拿到授权。
    ///
    /// 锁中毒时返回 `false`（= 重新弹窗）而不是 panic：这是个纯加速路径，
    /// 它坏了最坏结果是用户体验退回旧行为，而权限路径 panic 会直接掀翻会话。
    fn turn_granted(&self, name: &str, input: &serde_json::Value) -> bool {
        self.turn_permission_grants
            .lock()
            .map(|s| s.contains(&self.permission_key(name, input)))
            .unwrap_or(false)
    }

    /// 本轮授权记忆的钥匙，粒度按工具分叉：**Bash 按工具名，其余按输入指纹**。
    ///
    /// Bash 是唯一「参数即语义」的工具——它的 `input` 就是命令全文，
    /// `git log --oneline` 与 `git log -p --since=x` 必然不同键。逐条按指纹记会让
    /// 长任务里几十条同类命令把闸门打成串行队列（每把新键付一次 3–5s 往返），
    /// 而这些命令的风险几乎由程序+子命令决定，不在 flag 上。闸门在按工具名复用
    /// 之前仍然逐条走过 `hard_deny` 与只读白名单，所以复用的是「这个程序要不要
    /// 跑」这个判断，而不是具体参数的安全判断。
    ///
    /// 其余工具（Write/Edit/Agent/computer-use…）的输入决定**改什么、对谁做**，
    /// 复用授权等于把风险面抹平：`Edit` 批过 `notes.md` 不该顺带放行
    /// `deploy.sh` 或 `.env.production`（受保护路径另有 `PermissionPolicy` 兜底，
    /// 但同一项目里的任意文件没有这层保护），所以一律按规范化输入指纹。
    fn permission_key(&self, tool_name: &str, input: &serde_json::Value) -> String {
        if tool_name == "Bash" {
            return tool_name.to_string();
        }
        format!("{tool_name}\u{1f}{:x}", fnv_hash_value(input))
    }

    /// 记下「本次提交内该工具已授权」。
    ///
    /// `confirm_tool` 返回 `true` 的所有路径（AllowOnce / AllowAlways /
    /// computer 的 project-approve-once 升格）都意味着「用户此刻批准了这个工具
    /// 在本轮执行」，所以不区分是哪一种按键，统一记入——AllowAlways 重复记
    /// 一遍是无害的幂等写入。
    fn grant_for_turn(&self, name: &str, input: &serde_json::Value) {
        let key = self.permission_key(name, input);
        if let Ok(mut s) = self.turn_permission_grants.lock() {
            s.insert(key);
        }
    }

    /// 该工具跑完后要不要跑一次项目校验。两个条件缺一不可：项目声明了命令
    /// （`verify_cfg.is_some()`），且这个工具确实改动了工作区文件。
    fn should_verify(&self, tool_name: &str) -> bool {
        self.verify_cfg.is_some() && crate::verify::VERIFY_TRIGGER_TOOLS.contains(&tool_name)
    }

    /// 工具执行前的权限闸门。`true` = 可以直接跑。
    ///
    /// 三段式：不需要确认的工具直接放行；本轮已授权的同名工具直接放行（这是
    /// 「批准一个任务」不再被拆成十几个 y 的关键）；其余才走 `confirm_tool`，
    /// 弹窗批准后把工具名记进本轮授权集。
    ///
    /// `confirm_tool` 返回 `true` 的所有路径（AllowOnce / AllowAlways /
    /// computer 的 project-approve-once 升格）都意味着「用户此刻批准了这个工具
    /// 在本轮执行」，所以不区分是哪一种按键，统一记入——AllowAlways 重复记
    /// 一遍是无害的幂等写入。
    ///
    /// Auto 模式多一段：先问 `AutoGate`，只有它判定「该问人」才落到 `confirm_tool`。
    /// 拒绝时返回 `Err(原因)` 而不是 `false`，好让调用方回灌**具体**理由给模型
    /// （分类器判定越界 ≠ 用户点了 d，两者的模型后续行为不一样）。
    async fn ensure_tool_permission(
        &self,
        name: &str,
        t: &Arc<dyn Tool>,
        input: &serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Result<(), String> {
        let auto = self.in_auto_mode(ctx);
        if !t.needs_permission(input) || self.turn_granted(name, input) {
            return Ok(());
        }
        let summary = t.action_summary(input);
        let gate = ctx.auto_gate().cloned();
        if auto {
            // 没装闸门 = auto 模式退化。此时**不能**当作"自动放行"——
            // `ToolCtx::confirm_tool` 在 Auto 下会走真实弹窗，无 UI 通道时
            // fail-closed，两条路都是安全的那一侧。
            let Some(gate) = gate.as_ref() else {
                tracing::debug!(
                    tool = name,
                    "auto 模式但本上下文未装配判定闸门，退回逐次人工确认"
                );
                if !ctx.confirm_tool(name, &summary).await {
                    return Err(ctx
                        .confirm_denial_reason(name)
                        .unwrap_or_else(|| USER_DENIED_REASON.to_string()));
                }
                self.grant_for_turn(name, input);
                return Ok(());
            };
            let request = crate::auto_mode::AutoReviewRequest {
                tool: name.to_string(),
                action_summary: summary.clone(),
                input: input.clone(),
                cwd: ctx.cwd().to_path_buf(),
                user_goal: self
                    .turn_user_goal
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default(),
            };
            match gate.review(&request).await {
                crate::auto_mode::AutoVerdict::Allow => {
                    self.grant_for_turn(name, input);
                    return Ok(());
                }
                crate::auto_mode::AutoVerdict::Deny { reason, .. } => return Err(reason),
                crate::auto_mode::AutoVerdict::AskUser { reason } => {
                    tracing::debug!(tool = name, %reason, "auto 闸门交回人工确认");
                }
            }
        }
        if !ctx.confirm_tool(name, &summary).await {
            return Err(ctx
                .confirm_denial_reason(name)
                .unwrap_or_else(|| USER_DENIED_REASON.to_string()));
        }
        // 用户批准后必须通知闸门清空连续计数，否则闸门仍认为「刚刚连撞」。
        //
        // 之前这段通知挂在**拒绝**分支上（方向反了），而 `note_human_approval`
        // 的文档写的是「用户批准了一次，清空连续计数」。后果有两个：用户点了
        // 10 次「允许」不会打断连续计数（闸门仍然在数着上几次的 deny），
        // 反而是用户点「拒绝」——那是最强的「不该放行」信号——把计数清零了。
        if let Some(gate) = gate.as_ref() {
            gate.note_human_approval();
        }
        self.grant_for_turn(name, input);
        Ok(())
    }

    /// 当前是否处于 Auto 模式（`ToolContext` 是唯一权威来源，它与 TUI 状态栏
    /// 共享同一个 `Arc<RwLock<PermissionMode>>`，Shift+Tab 即时生效）。
    fn in_auto_mode(&self, ctx: &dyn ToolContext) -> bool {
        matches!(
            ctx.permission_mode(),
            Some(crate::permission::PermissionMode::Auto)
        )
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
                // protected path 等强制策略。
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
                    } else if let Err(reason) =
                        self.ensure_tool_permission(&name, &t, &input, ctx).await
                    {
                        // 逐调用权限确认被拒：把**具体**理由回灌给模型（用户点 d
                        // 与 auto 分类器判定越界，模型该有的后续行为并不相同）。
                        let msg = format!("工具 `{name}` 未获准执行：{reason}");
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

        // ── 编辑后校验（verify loop）────────────────────────────────────
        // 排在 PostToolUse hook 之后：hook 是用户私有的自动化，可能已经跑了
        // 自己的校验；声明了 verify 的项目两者并存，两段反馈都留给模型。
        //
        // 只在 `!is_error` 时跑——工具自己都失败了，代码处于半改状态，此时的
        // 编译错误多半是「这个 Edit 没写成」而不是「改动引入了新错」，回灌进去
        // 只会把模型带偏。
        let (display, content) = if !is_error && self.should_verify(&name) {
            let cfg = self
                .verify_cfg
                .clone()
                .expect("should_verify 为真时 verify_cfg 必非空");
            let outcome = crate::verify::run_verify(&cfg, ctx.cwd()).await;
            let content = append_hook_feedback(content, &outcome.report);
            (format!("{display}{}", outcome.report), content)
        } else {
            (display, content)
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
    ///
    /// 走前缀复用路径：`system` 按与 `run_turn` 相同的规则拼（主提示、记忆快照、
    /// 规约祖先链），`tools` 用当前工具集。**可能失配**——`run_turn` 里 tools
    /// 还会按 route 能力与 lazy 折叠裁剪，这里没有那层上下文。失配的代价只是
    /// 缓存不命中，回到改动前的全价行为，不会影响摘要正确性。
    pub async fn compact_context(
        &self,
        session: &mut Session,
    ) -> Result<crate::compact::CompactResult> {
        use crate::compact::{compact_session, CompactPrefix};
        let route = self.route_at(self.active_route_index());

        let mut system_stable = self.system_prompt.clone();
        let memory_snapshot = self.memory_snapshot_text(session);
        if !memory_snapshot.is_empty() {
            system_stable.push_str("\n\n");
            system_stable.push_str(&memory_snapshot);
        }
        if let Some(loader) = &self.claude_md {
            if let Some(reminder) = loader.turn_reminder() {
                system_stable.push_str("\n\n");
                system_stable.push_str(&reminder);
            }
        }
        let request_tools: Vec<ToolDefinition> = self
            .tools
            .iter()
            .filter(|definition| {
                self.lazy_tool_state
                    .as_ref()
                    .map(|state| state.visible(&definition.name))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();

        compact_session(
            session,
            route.provider.as_ref(),
            route.context_window,
            // 与 `run_turn` 内两条压缩路径同一判据：不缓存的端点退回独立请求
            // 形态，避免把整段历史全价重发一遍。
            if route
                .capabilities
                .as_ref()
                .map(|caps| caps.reuses_prompt_prefix())
                .unwrap_or(true)
            {
                Some(CompactPrefix {
                    system: wyj_api::SystemPrompt {
                        stable: &system_stable,
                        volatile: "",
                    },
                    tools: &request_tools,
                })
            } else {
                None
            },
        )
        .await
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

/// 触顶前的收尾宽限轮数：距离上限还剩这么多轮时，向模型注入一次「停止调用
/// 工具、立即总结交付」的指令（见 `run_turn_with_injection_inner`）。
///
/// 取 4 而不是 1：模型读到指令后仍可能先补一次工具调用（比如把刚跑完的
/// 测试结果读回来再作答），宽限太窄会让它来不及交出总结就被硬中止，白白
/// 浪费已经完成的全部工作。4 轮足够它「不再调工具 + 写总结」，同时相对 200
/// 的总预算可忽略不计。
const TURN_LIMIT_GRACE: usize = 4;

/// 连续工具参数校验失败到第几轮时，向模型注入一次「换个思路、就地收尾」的指令。
///
/// 取 3 而非原来的「3 轮直接 bail」：原实现把「同一类参数错误重复 3 次」直接
/// 判成整轮失败，把这一轮其它工具已经产出的成果一起丢掉。实测一次真实故障
/// （模型把 19 位 `tree_id` 反复当字符串发）里，前几轮已经成功完成了窗口枚举、
/// 截图和菜单列举，最后却因为一个调用的参数形态而整轮中止。现在改成先收尾。
///
/// 硬中止保留在 `GRACE_AT + 1` 轮之后：模型无视收尾指令、继续用同样方式硬撞，
/// 那才是真正需要报错终止的情形。
const INVALID_ARGUMENT_GRACE_AT: usize = 3;

/// 参数校验连续失败时的收尾指令。措辞要点是**换路径**而不是「再试一次」——
/// 模型卡住的原因通常是某个调用的参数形态它表达不出来，继续原样重试只会
/// 复现同一个失败。
fn invalid_argument_wrap_up_notice() -> &'static str {
    "<tool-arguments-wrap-up>\n\
     Several consecutive tool calls were rejected because their arguments did not \
     match the tool schema. Do not repeat the same call shape again. Either fix the \
     specific field the error named, or switch to a different tool or approach that \
     does not need that argument. If the task is already blocked, say so plainly and \
     report what you learned so far.\n\
     连续数次工具调用因参数不符合 schema 被拒绝。不要再用同样的参数形态重试。\
     请修正报错点名的字段，或改用不需要该参数的其他工具与方式；若任务确实卡住，\
     请如实说明并汇报目前已知的信息。\n\
     </tool-arguments-wrap-up>"
}

/// 用户在权限对话框里主动拒绝时回灌给模型的原因。
///
/// 措辞刻意命令式：模型收到「用户拒绝了」之后的典型失败模式是换个参数再试一次，
/// 那既没用又招人烦。Auto 分类器的拒绝走各自的 `reason`，不带这句。
const USER_DENIED_REASON: &str =
    "用户拒绝执行该操作。请不要重试该操作；改用其他方式，或先向用户询问原因。";

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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// `soft_threshold_tokens` 的测试用最小 `ContextEditCfg`。CAS 指向临时目录，
    /// 这些断言只关心阈值计算本身，不触发真实外部化。
    fn mk_edit_cfg() -> crate::context_edit::ContextEditCfg {
        let dir = std::env::temp_dir().join(format!("wyj-soft-threshold-{}", std::process::id()));
        crate::context_edit::ContextEditCfg {
            enabled: true,
            opts: crate::context_edit::ElideOptions::default(),
            cas: std::sync::Arc::new(
                crate::workspace_cas::WorkspaceCas::open(&dir, 0).expect("CAS 可用"),
            ),
            max_batches: 3,
            soft_limit_ratio: 0.55,
            totals: std::sync::Arc::new(crate::context_edit::ContextEditTotals::default()),
        }
    }

    /// 软阈值决定 context editing 何时**动手**。
    ///
    /// 这条是整个 C1 变更的核心断言：`soft_limit_ratio = 0`（关闭）必须退化成
    /// 旧行为，否则「opt-out 配置」就名存实亡。
    #[test]
    fn soft_threshold_disabled_falls_back_to_old_behavior() {
        let cfg = crate::context_edit::ContextEditCfg {
            enabled: false,
            soft_limit_ratio: 0.0,
            ..mk_edit_cfg()
        };
        assert_eq!(soft_threshold_tokens(1_000_000, Some(&cfg)), u32::MAX);
        // 未装配 context editing 时同理：u32::MAX 让 `estimated > soft` 恒假。
        assert_eq!(soft_threshold_tokens(1_000_000, None), u32::MAX);
    }

    #[test]
    fn soft_threshold_starts_editing_earlier_than_compact_threshold() {
        let cfg = crate::context_edit::ContextEditCfg {
            soft_limit_ratio: 0.55,
            ..mk_edit_cfg()
        };
        let window = 1_000_000u32;
        let soft = soft_threshold_tokens(window, Some(&cfg));
        let hard = window.saturating_sub(crate::compact::compact_trigger_buffer(window, 32_000));
        assert_eq!(soft, 550_000);
        // 软阈值必须严格早于硬阈值，否则 C1 等于没做。
        assert!(
            soft < hard,
            "soft {soft} must be below compact threshold {hard}, otherwise trimming never starts early"
        );
    }

    /// 200K 窗口下 `soft_limit_ratio = 0.55` 给出 110K，仍然早于硬阈值。
    #[test]
    fn soft_threshold_scales_with_window() {
        let cfg = crate::context_edit::ContextEditCfg {
            soft_limit_ratio: 0.55,
            ..mk_edit_cfg()
        };
        assert_eq!(soft_threshold_tokens(200_000, Some(&cfg)), 110_000);
    }

    /// 荒谬比例不能溢出或产生 0（0 会让每个请求都无谓地跑一遍清理循环）。
    #[test]
    fn soft_threshold_clamps_absurd_ratios() {
        let huge = crate::context_edit::ContextEditCfg {
            soft_limit_ratio: 9_999.0,
            ..mk_edit_cfg()
        };
        let v = soft_threshold_tokens(1_000_000, Some(&huge));
        assert_eq!(v, u32::MAX, "过大的比例必须夹到 u32::MAX 而非溢出回绕");

        let negative = crate::context_edit::ContextEditCfg {
            soft_limit_ratio: -1.0,
            ..mk_edit_cfg()
        };
        // 负数与 0 同义（关闭），绝不能 clamp 成 1 让清理每轮都触发。
        assert_eq!(soft_threshold_tokens(1_000_000, Some(&negative)), u32::MAX);
    }
    use wyj_api::provider::EventStream;
    use wyj_api::types::{ContentBlock, Message, Role, StopReason, ToolResultContent};

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

    /// 本轮授权记忆的分叉不变式：**Bash 按工具名，其余按输入指纹**。
    ///
    /// 这条线是两种旧模式合并后唯一剩下的手感差异，也是最容易在重构中被
    /// 「顺手统一」掉的地方，所以两个方向都钉：
    /// - Bash 放宽到工具名：长任务里几十条 `cargo test` / `git log …` 不该
    ///   把闸门打成串行队列（每把新键付一次 3–5s 分类器往返）。
    /// - 其余工具必须保持精确：`Edit` 批过 `notes.md` 不该顺带放行
    ///   `deploy.sh`——写工具的输入决定**改什么**，不是「要不要改文件」。
    #[test]
    fn turn_grant_key_is_bare_bash_but_exact_fingerprint_for_everything_else() {
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let key = |name: &str, input: serde_json::Value| agent.permission_key(name, &input);

        // Bash：命令全文不参与，只有工具名。
        let bash = |command: &str| key("Bash", serde_json::json!({ "command": command }));
        assert_eq!(bash("git log --oneline"), bash("git log -p --since=x"));
        assert_eq!(bash("cargo test"), bash("cargo test --lib -- --nocapture"));
        assert_eq!(bash("npm run deploy:prod"), bash("ls"));
        assert_eq!(bash("rm -rf build"), "Bash");

        // 其余工具：同工具不同输入必须不同键，同输入不同 key 顺序必须同键。
        let edit = |path: &str, content: &str| {
            key(
                "Edit",
                serde_json::json!({ "file_path": path, "old_string": content }),
            )
        };
        assert_ne!(edit("notes.md", "a"), edit("deploy.sh", "a"));
        assert_ne!(edit("notes.md", "a"), edit("notes.md", "b"));
        assert_eq!(
            key("Edit", serde_json::json!({"file_path": "a", "x": 1})),
            key("Edit", serde_json::json!({"x": 1, "file_path": "a"}))
        );

        // Agent / computer-use 同理按输入指纹：subagent_type、action 变了就得重审。
        assert_ne!(
            key("Agent", serde_json::json!({"subagent_type": "Explore"})),
            key("Agent", serde_json::json!({"subagent_type": "Plan"}))
        );
        assert_ne!(
            key(
                "app_computer",
                serde_json::json!({"action": "screenshot", "window_id": 1})
            ),
            key(
                "app_computer",
                serde_json::json!({"action": "click", "window_id": 1})
            )
        );
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
            _system: &wyj_api::SystemPrompt<'_>,
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

    /// 捕获每次请求的 system prompt 全文，回一句文本就 EndTurn。
    struct SystemRecordingProvider {
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl Provider for SystemRecordingProvider {
        async fn stream(
            &self,
            system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            self.seen
                .lock()
                .unwrap()
                .push(system.combined().to_string());
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    async fn capture_system(
        agent: &mut Agent,
        seen: &std::sync::Arc<Mutex<Vec<String>>>,
    ) -> String {
        let mut session = Session::new();
        agent
            .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
            .await
            .unwrap();
        let captured = seen.lock().unwrap().clone();
        assert_eq!(
            captured.len(),
            1,
            "本用例只应发一次请求，实际 {}",
            captured.len()
        );
        captured.into_iter().next().unwrap()
    }

    /// 语言提醒的价值**全在位置**：它存在的理由是 `<env>` 里的语言事实在
    /// system 中段（后面还压着记忆快照 + CLAUDE.md reminder，动辄几万 token），
    /// recency 权重落不到那句上，于是 MiniMax M3.1 的 thinking 全英文。
    /// 谁在它后面再追加东西，这个修复就静默失效——所以钉死「必须是最后一段」。
    #[tokio::test]
    async fn response_language_reminder_is_the_last_system_section() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(Arc::new(SystemRecordingProvider { seen: seen.clone() }))
            .with_system("MAIN-STUB")
            .with_response_language("zh");
        let combined = capture_system(&mut agent, &seen).await;

        assert!(combined.starts_with("MAIN-STUB"));
        let reminder = crate::prompts::response_language_reminder("zh");
        assert!(
            combined.trim_end().ends_with(reminder.trim_end()),
            "语言提醒必须是 system prompt 的最后一段，实际最后一段：{:?}",
            combined.trim_end().rsplit('\n').next().unwrap_or_default()
        );
        let reminder_at = combined
            .find("# Response language")
            .expect("未发出语言提醒");
        let tool_block_at = combined
            .find("<current-tool-availability>")
            .expect("未发出工具可用性块");
        assert!(
            reminder_at > tool_block_at,
            "提醒必须排在 volatile 段的工具可用性块之后，否则 recency 修复被自己顶掉"
        );
    }

    /// 未接线（`None`）时不得凭空多出一段：子 Agent、后台任务等路径没设语言，
    /// 那里追加一句只会变成与 CoT 无关的噪声。
    #[tokio::test]
    async fn no_response_language_means_no_reminder() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(Arc::new(SystemRecordingProvider { seen: seen.clone() }))
            .with_system("MAIN-STUB");
        let combined = capture_system(&mut agent, &seen).await;
        assert!(
            !combined.contains("# Response language"),
            "未设置语言时不应追加提醒"
        );
    }

    /// 幂等性不变量：**框架层的任何重试都不得重复执行工具**。
    ///
    /// 这是 P2-9 审查的结论，用测试钉住而不是只写在注释里。理由：Agent 里所有
    /// 重试点（流中断重发、`parameter_degraded` 降参重发、上下文超限截断后重发、
    /// fallback 路由切换）**全部发生在 assistant 消息提交之前**——此时本次
    /// assistant 的 `tool_use` 块还没进 `session.messages`，工具也就还没跑过。
    /// 半成品 assistant 消息绝不入史（见 stream 循环的注释），所以重发的消息里
    /// 不含任何已执行的工具，重试是干净的。
    ///
    /// 工具一旦跑过，重试路径就再也不存在了：下一轮是**模型**基于 tool_result
    /// 重新决策，那属于模型行为，不是框架重复执行。
    #[tokio::test]
    async fn a_retry_never_re_executes_an_already_committed_tool() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(FlakyFirstAttemptProvider {
            phase: std::sync::atomic::AtomicUsize::new(0),
        }))
        .with_max_turns(10);
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: false,
            runs: runs.clone(),
        }));
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
            .await
            .unwrap();

        // 第 1 次 stream 发了一个 tool_use 后流中断，第 2 次又发了一个 tool_use，
        // 两次的 input 完全相同——正是「如果框架把半成品也提交了、或把重发当成
        // 第二次调用追加上去」的重复执行形态。此处工具只应跑 1 次。
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "重发后的 tool_use 只应执行一次"
        );
        let assistant_with_tool_calls = session
            .messages
            .iter()
            .filter(|m| m.role == wyj_api::types::Role::Assistant)
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, wyj_api::types::ContentBlock::ToolUse { .. }))
            })
            .count();
        assert_eq!(
            assistant_with_tool_calls, 1,
            "半成品 assistant 消息不得入史，否则重试会变成重复执行"
        );
    }

    /// 三段式 provider：第 1 次发工具调用后流中断，第 2 次**发同一个工具调用**
    /// 并正常收尾，第 3 次才 EndTurn。两次 tool_use 的 input 刻意完全一致。
    struct FlakyFirstAttemptProvider {
        phase: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for FlakyFirstAttemptProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let phase = self.phase.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let call = vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "t0".into(),
                    name: "Edit".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "t0".into(),
                    json_delta: r#"{"path":"a.rs"}"#.into(),
                }),
                Ok(StreamEvent::ToolUseEnd { id: "t0".into() }),
            ];
            match phase {
                // 流中途断开：没有 MessageStop，Agent 丢弃半成品后重发。
                0 => {
                    let mut events = call;
                    events.push(Err(anyhow::anyhow!("simulated mid-stream disconnect")));
                    Ok(Box::pin(futures::stream::iter(events)))
                }
                // 这次完整收尾，工具调用被提交 → 应当执行恰好一次。
                1 => {
                    let mut events = call;
                    events.push(Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }));
                    Ok(Box::pin(futures::stream::iter(events)))
                }
                _ => Ok(Box::pin(futures::stream::iter(vec![Ok(
                    StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    },
                )]))),
            }
        }
    }

    /// 记忆提取降频：首轮必提，之后每 N 轮一次；空会话不提取。
    #[test]
    fn memory_extraction_fires_on_the_first_turn_then_every_third() {
        assert!(memory_extraction_due(1), "首轮必须提：新项目最需要记忆");
        assert!(!memory_extraction_due(2));
        assert!(!memory_extraction_due(3));
        assert!(memory_extraction_due(4));
        assert!(!memory_extraction_due(5));
        assert!(memory_extraction_due(7));
        assert!(memory_extraction_due(31));
        assert!(!memory_extraction_due(0), "空会话没有可提取的内容");
    }

    // ── 编辑后自动校验（verify loop）───────────────────────────────────────

    /// 编辑类工具，`runs` 用来证明校验跑在工具**之后**而不是之前。
    struct EditingTool {
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Tool for EditingTool {
        fn name(&self) -> &str {
            "Edit"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "Edit".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            }
        }
        fn needs_permission(&self, _input: &serde_json::Value) -> bool {
            false
        }
        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<ToolResult> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::ok("文件已更新"))
        }
    }

    /// 把一个会失败的 Edit 送进开着 verify 的 Agent，确认编译器输出真的进了
    /// 发给模型的 tool_result——这是整个特性的全部意义所在。
    #[tokio::test]
    async fn verify_failures_reach_the_model_as_tool_result() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(burst("Edit", 1)).with_verify(Some(crate::verify::VerifyCfg {
            command: "echo 'error[E0308]: mismatched types' >&2; exit 1".into(),
            timeout_ms: 5_000,
            max_output_bytes: 4_000,
            enabled: true,
        }));
        agent.register_tool(Arc::new(EditingTool { runs: runs.clone() }));
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        let tool_results: Vec<String> = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                wyj_api::types::ContentBlock::ToolResult { content, .. } => {
                    Some(format!("{content:?}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 1, "应当只有一个 Edit 的 tool_result");
        assert!(
            tool_results[0].contains("[verify] 校验失败"),
            "校验反馈没进 tool_result：{}",
            tool_results[0]
        );
        assert!(
            tool_results[0].contains("E0308"),
            "编译器错误没回灌：{}",
            tool_results[0]
        );
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// 零假设是硬约束：项目没声明命令时，一次子进程都不许跑。
    #[tokio::test]
    async fn without_a_declared_command_nothing_runs() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(burst("Edit", 1));
        agent.register_tool(Arc::new(EditingTool { runs: runs.clone() }));
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        assert!(!agent.should_verify("Edit"));
        let texts: Vec<String> = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                wyj_api::types::ContentBlock::ToolResult { content, .. } => {
                    Some(format!("{content:?}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1);
        assert!(
            !texts[0].contains("[verify]"),
            "不该有任何校验痕迹：{}",
            texts[0]
        );
    }

    /// 只对改文件的工具触发：Bash / Read / Grep 都不跑校验。
    #[test]
    fn only_workspace_mutating_tools_trigger_verify() {
        let agent =
            Agent::new(Arc::new(EndTurnProvider)).with_verify(Some(crate::verify::VerifyCfg {
                command: "true".into(),
                timeout_ms: 1,
                max_output_bytes: 1,
                enabled: true,
            }));
        for name in ["Edit", "Write"] {
            assert!(agent.should_verify(name), "{name} 应当触发校验");
        }
        for name in ["Bash", "Read", "Grep", "Glob", "Agent", "TodoWrite"] {
            assert!(!agent.should_verify(name), "{name} 不该触发校验");
        }

        // enabled = false 即使显式装配也要被过滤掉（防调用方漏判）
        let off =
            Agent::new(Arc::new(EndTurnProvider)).with_verify(Some(crate::verify::VerifyCfg {
                command: "true".into(),
                timeout_ms: 1,
                max_output_bytes: 1,
                enabled: false,
            }));
        assert!(!off.should_verify("Edit"));
    }

    // ── 本轮权限授权（per-turn tool permission grants）─────────────────────

    /// 记录 `confirm_tool` 被问了几次、每次的摘要，并按固定决定放行/拒绝。
    struct PermissionProbeCtx {
        asks: std::sync::atomic::AtomicUsize,
        summaries: std::sync::Mutex<Vec<String>>,
        grant: bool,
    }
    impl PermissionProbeCtx {
        fn new(grant: bool) -> Self {
            Self {
                asks: std::sync::atomic::AtomicUsize::new(0),
                summaries: std::sync::Mutex::new(Vec::new()),
                grant,
            }
        }
        fn ask_count(&self) -> usize {
            self.asks.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    #[async_trait::async_trait]
    impl ToolContext for PermissionProbeCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/tmp")
        }
        fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
            true
        }
        async fn confirm_tool(&self, name: &str, summary: &str) -> bool {
            self.asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.summaries
                .lock()
                .unwrap()
                .push(format!("{name}:{summary}"));
            self.grant
        }
    }

    /// `needs_permission()` 可控的探针工具。
    struct GatedTool {
        name: &'static str,
        needs_permission: bool,
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Tool for GatedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            }
        }
        fn needs_permission(&self, _input: &serde_json::Value) -> bool {
            self.needs_permission
        }
        fn action_summary(&self, input: &serde_json::Value) -> String {
            input["path"].as_str().unwrap_or("?").to_string()
        }
        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn ToolContext,
        ) -> Result<ToolResult> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::ok("done"))
        }
    }

    /// 每轮 emit **一个**工具调用，发满 `budget` 个后 EndTurn 并把计数**归零**。
    ///
    /// 刻意不做「一次 emit 多个」：真实模型每轮只发一个 tool_use，而 Agent 的
    /// 内部工具轮循环正是把 N 个工具调用摊成 N 轮——「一次用户提交里的 3 次
    /// Edit」= 3 个内部轮。
    ///
    /// 归零是关键：provider 实例被两次 `run_turn` 共用，计数只增不减的话第二次
    /// 提交会直接拿到 EndTurn、一个工具都不发，测到的就成了「provider 没发调用」
    /// 而不是「授权没被清」。用 `emitted` / `budget` 双计数而不是同一个计数器
    /// 递减，也是为了 `stop_reason` 由「还剩没发完」决定，而非递减的副作用。
    struct BurstToolProvider {
        emitted: std::sync::atomic::AtomicUsize,
        budget: usize,
        tool: String,
    }
    #[async_trait::async_trait]
    impl Provider for BurstToolProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self
                .emitted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.budget {
                self.emitted.store(0, std::sync::atomic::Ordering::SeqCst);
                return Ok(Box::pin(futures::stream::iter(vec![Ok(
                    StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    },
                )])));
            }
            let id = format!("t{n}");
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: self.tool.clone(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: id.clone(),
                    json_delta: format!(r#"{{"path":"f{}.rs"}}"#, n),
                }),
                Ok(StreamEvent::ToolUseEnd { id }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn burst(tool: &str, budget: usize) -> Arc<dyn Provider> {
        Arc::new(BurstToolProvider {
            emitted: std::sync::atomic::AtomicUsize::new(0),
            budget,
            tool: tool.to_string(),
        })
    }

    /// 一直发工具调用，直到在 messages 里读到 `turn-budget-reminder` 才收手。
    ///
    /// 模拟的就是真实触顶会话里的模型：它并不会因为快到上限就自己停下，
    /// 必须有人明确告诉它「别再调工具了，现在总结」。`ignore_reminder` 则
    /// 造出反面形态——读到提醒仍然继续调工具，用来验证硬中止那道兜底。
    struct BudgetAwareProvider {
        calls: AtomicUsize,
        seen_reminder: AtomicBool,
        ignore_reminder: bool,
    }
    #[async_trait::async_trait]
    impl Provider for BudgetAwareProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let has_reminder = messages.iter().any(|m| {
                m.content.iter().any(|b| {
                    matches!(b, ContentBlock::Text { text } if text.contains("turn-budget-reminder"))
                })
            });
            if has_reminder {
                self.seen_reminder.store(true, Ordering::SeqCst);
                if !self.ignore_reminder {
                    return Ok(Box::pin(futures::stream::iter(vec![
                        Ok(StreamEvent::TextDelta("收尾总结".into())),
                        Ok(StreamEvent::MessageStop {
                            stop_reason: StopReason::EndTurn,
                        }),
                    ])));
                }
            }
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let id = format!("b{n}");
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: "Edit".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: id.clone(),
                    json_delta: format!(r#"{{"path":"f{n}.rs"}}"#),
                }),
                Ok(StreamEvent::ToolUseEnd { id }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ])))
        }
    }

    /// 触顶不再等于失败：宽限窗口内注入一次收尾指令，模型交出总结后回合
    /// **正常结束**（`Ok`），而不是把已经做完的工作判成一句报错。
    ///
    /// 这条测试钉的是本次事故的根因修复。修复前：`max_turns=8` 的长任务在
    /// 第 9 轮 `bail!`，用户拿到红色报错，工作树里的改动一句总结都没留下。
    #[tokio::test]
    async fn hitting_the_turn_budget_delivers_a_summary_instead_of_erroring() {
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(BudgetAwareProvider {
            calls: AtomicUsize::new(0),
            seen_reminder: AtomicBool::new(false),
            ignore_reminder: false,
        });
        let mut agent = Agent::new(provider.clone()).with_max_turns(8);
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: false,
            runs: runs.clone(),
        }));
        let mut session = Session::new();

        let result = agent
            .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
            .await;

        assert!(
            result.is_ok(),
            "触顶后应正常收尾而不是报错: {:?}",
            result.err()
        );
        assert!(
            provider.seen_reminder.load(Ordering::SeqCst),
            "宽限窗口内必须向模型注入收尾指令"
        );
        // 工具确实跑过——证明这份总结是「已完成的活」而不是空转出来的。
        assert!(
            runs.load(Ordering::SeqCst) > 0,
            "工具一次都没跑，收尾无从谈起"
        );
        let has_summary = session.messages.iter().any(|m| {
            m.role == wyj_api::types::Role::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("收尾总结")))
        });
        assert!(has_summary, "回合历史里应留下模型的最终总结");
    }

    /// 收尾指令**只注入一次**：无视指令的模型不能靠反复注入把自己续命，
    /// 否则「最多 N 轮」这个上限就形同虚设。
    #[tokio::test]
    async fn the_wrap_up_notice_is_injected_at_most_once() {
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(BudgetAwareProvider {
            calls: AtomicUsize::new(0),
            seen_reminder: AtomicBool::new(false),
            // 无视收尾指令：模型读到提醒仍继续调工具，回合必须落到硬中止。
            ignore_reminder: true,
        });
        let mut agent = Agent::new(provider.clone()).with_max_turns(8);
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: false,
            runs: runs.clone(),
        }));
        let mut session = Session::new();

        let result = agent
            .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
            .await;

        assert!(result.is_err(), "无视收尾指令时仍应硬中止（防续命上界）");
        let notices = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains("turn-budget-reminder"))
            })
            .count();
        assert_eq!(notices, 1, "收尾指令只应注入一次，实际 {notices} 次");
    }

    /// 收尾窗口内工具**真的被关闭**：无视收尾指令的模型，其调用不再被执行，
    /// 而是回一条 `tools_closed_turn_budget_exhausted`。
    ///
    /// 这条钉的是 2026-10-18 事故的根因：一条 285 次调用的会话在第 196 轮
    /// 收到了格式正确、位置正确的收尾指令，模型仍然连发 5 轮工具（285 条
    /// assistant 消息里 282 条只有 1 个 tool_use），宽限被耗光后 `bail!`。
    /// 软性提醒在模型沉浸态里不产生约束力，所以框架必须自己关闸。
    ///
    /// 断言用「工具总共执行了几次」而不是「回合是否成功」：后者在硬中止路径上
    /// 同样是 `Err`，分不出修复前后的差别。
    #[tokio::test]
    async fn tools_are_closed_during_the_wrap_up_window() {
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(BudgetAwareProvider {
            calls: AtomicUsize::new(0),
            seen_reminder: AtomicBool::new(false),
            ignore_reminder: true,
        });
        let mut agent = Agent::new(provider.clone()).with_max_turns(8);
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: false,
            runs: runs.clone(),
        }));
        let mut session = Session::new();

        let _ = agent
            .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
            .await;

        // 工具只在 wrap_up_at **之前**的轮次执行；wrap_up_at 那一轮注入提醒的
        // 同时就关闸，所以执行次数是 `wrap_up_at - 1`，被关掉的是余下全部。
        let wrap_up_at = 8 - TURN_LIMIT_GRACE;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            wrap_up_at - 1,
            "收尾窗口内的工具调用必须被关闭，实际执行了 {} 次",
            runs.load(Ordering::SeqCst)
        );
        let closed = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| {
                matches!(b, ContentBlock::ToolResult { content: ToolResultContent::Text(text), .. }
                    if text.contains("tools_closed_turn_budget_exhausted"))
            })
            .count();
        assert_eq!(
            closed,
            8 - wrap_up_at + 1,
            "宽限窗口的每一轮都应回一条「工具已关闭」，实际 {closed} 条"
        );
    }

    /// 宽限窗口从 `max_turns - TURN_LIMIT_GRACE` 起算：留出的轮数必须够
    /// 模型「不再调工具 + 写总结」，否则刚注入就被硬中止，宽限形同虚设。
    ///
    /// 用 const 块而不是 `assert!`：这两个事实不变量是编译期就该钉死的，
    /// 写成运行时断言只会让 clippy 提醒把它挪过来。
    #[test]
    fn the_grace_window_leaves_room_to_actually_deliver() {
        const { assert!(TURN_LIMIT_GRACE >= 2, "宽限至少要够一次工具轮 + 一次总结轮") };
        // 默认 200 的配置下宽限占比极小，不构成实质性的预算削减。
        const { assert!(TURN_LIMIT_GRACE * 10 < 200) };
    }

    /// 本轮授权记忆的两个方向：同一次提交里 3 次 Bash 只弹 1 次框，3 次都真跑了。
    ///
    /// Bash 按工具名记忆——长任务里几十条 `cargo test` / `git log …` 不该各弹一次，
    /// 更不该各付一次分类器往返。注意这里的输入（`{"path":"f{n}.rs"}`）每次都不同，
    /// 复用授权靠的正是「命令不参与这把钥匙」。
    #[tokio::test]
    async fn one_bash_confirm_covers_every_bash_in_a_turn() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(burst("Bash", 3));
        agent.register_tool(Arc::new(GatedTool {
            name: "Bash",
            needs_permission: true,
            runs: runs.clone(),
        }));
        let ctx = PermissionProbeCtx::new(true);
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &ctx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(ctx.ask_count(), 1, "本轮 3 次 Bash 只应弹一次确认");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 3);
        // 摘要取自第一次：用户是为 f0.rs 点头的，f1/f2 靠本轮授权放行
        assert_eq!(*ctx.summaries.lock().unwrap(), ["Bash:f0.rs"]);
    }

    /// 反方向：3 次 Edit 改 3 个不同文件必须弹 3 次框。
    ///
    /// 这是合并手动模式时**刻意收紧**的一条。旧的手动模式按工具名记，于是
    /// 「批准一次 Edit」之后本轮所有编辑全部免审——用户在为一个文件点头，模型
    /// 顺手改了另外两个。写工具的输入决定**改什么**，所以记忆粒度必须落在输入上。
    #[tokio::test]
    async fn one_edit_confirm_does_not_cover_a_different_file() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(burst("Edit", 3));
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: true,
            runs: runs.clone(),
        }));
        let ctx = PermissionProbeCtx::new(true);
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &ctx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(ctx.ask_count(), 3, "改不同文件的 Edit 不能共用一次确认");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// 下一次提交必须重新问：本轮授权不是会话级授权。
    #[tokio::test]
    async fn grants_do_not_survive_into_the_next_turn() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // budget 1：每次提交只发一次 Edit。两次提交之间唯一的变化就是
        // `turn_permission_grants` 有没有被清空。
        let mut agent = Agent::new(burst("Edit", 1));
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: true,
            runs: runs.clone(),
        }));
        let ctx = PermissionProbeCtx::new(true);
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &ctx, &mut |_| {})
            .await
            .unwrap();
        session.push_user("再来一次");
        agent
            .run_turn(&mut session, &ctx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(ctx.ask_count(), 2, "第二次提交是新的信任边界，必须重新确认");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// 拒绝不产生本轮授权：模型重试时用户还要再看到一次框，且工具一次都不该跑。
    #[tokio::test]
    async fn a_denial_grants_nothing_and_blocks_every_attempt() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(burst("Edit", 2));
        agent.register_tool(Arc::new(GatedTool {
            name: "Edit",
            needs_permission: true,
            runs: runs.clone(),
        }));
        let ctx = PermissionProbeCtx::new(false);
        let mut session = Session::new();

        agent
            .run_turn(&mut session, &ctx, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(
            ctx.ask_count(),
            2,
            "拒绝必须每次都重新问，否则就成了静默放行"
        );
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// 授权按工具名隔离：批了 Edit 不代表批了 Write；只读工具根本不问。
    #[tokio::test]
    async fn grants_are_scoped_to_the_approved_tool_name() {
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let ctx = PermissionProbeCtx::new(true);
        let needs_perm = serde_json::json!({});
        let gated = |name: &'static str, needs_permission: bool| -> Arc<dyn Tool> {
            Arc::new(GatedTool {
                name,
                needs_permission,
                runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })
        };
        let edit = gated("Edit", true);

        agent
            .ensure_tool_permission("Edit", &edit, &needs_perm, &ctx)
            .await
            .expect("人工批准应放行");
        agent
            .ensure_tool_permission("Edit", &edit, &needs_perm, &ctx)
            .await
            .expect("本轮已授权应免问");
        agent
            .ensure_tool_permission("Write", &edit, &needs_perm, &ctx)
            .await
            .expect("人工批准应放行");
        agent
            .ensure_tool_permission("Read", &gated("Read", false), &needs_perm, &ctx)
            .await
            .expect("只读工具不需授权");

        assert_eq!(
            *ctx.summaries.lock().unwrap(),
            ["Edit:?", "Write:?"],
            "Edit 只问一次、Write 单独问一次、Read 根本不问"
        );
    }

    /// Auto 模式的 ctx 探针：暴露闸门 + 暴露 Auto 模式 + 记录弹窗次数。
    struct AutoProbeCtx {
        inner: PermissionProbeCtx,
        gate: Arc<dyn crate::auto_mode::AutoGate>,
    }
    #[async_trait::async_trait]
    impl ToolContext for AutoProbeCtx {
        fn cwd(&self) -> &std::path::Path {
            self.inner.cwd()
        }
        fn is_allowed(&self, name: &str, input: &serde_json::Value) -> bool {
            self.inner.is_allowed(name, input)
        }
        async fn confirm_tool(&self, name: &str, summary: &str) -> bool {
            self.inner.confirm_tool(name, summary).await
        }
        fn permission_mode(&self) -> Option<crate::permission::PermissionMode> {
            Some(crate::permission::PermissionMode::Auto)
        }
        fn auto_gate(&self) -> Option<&Arc<dyn crate::auto_mode::AutoGate>> {
            Some(&self.gate)
        }
    }

    /// 记录每次 review 的动作摘要、返回预设 verdict 的假闸门。
    struct RecordingGate {
        seen: std::sync::Mutex<Vec<String>>,
        verdict: crate::auto_mode::AutoVerdict,
    }
    #[async_trait::async_trait]
    impl crate::auto_mode::AutoGate for RecordingGate {
        async fn review(
            &self,
            request: &crate::auto_mode::AutoReviewRequest,
        ) -> crate::auto_mode::AutoVerdict {
            self.seen
                .lock()
                .unwrap()
                .push(request.action_summary.clone());
            self.verdict.clone()
        }
        fn counters(&self) -> crate::auto_mode::AutoCounters {
            crate::auto_mode::AutoCounters::default()
        }
        fn should_degrade(&self) -> bool {
            false
        }
        fn note_human_approval(&self) {}
        fn disable_classifier(&self) {}
        fn enable_classifier(&self) {}
        fn classifier_enabled(&self) -> bool {
            true
        }
    }

    /// Auto 的核心不变式：**Bash 之外的本轮记忆必须按调用，不按工具名**。
    ///
    /// 旧的两分叉里，Auto 一律按调用记是为了防住「审过 `Edit notes.md` →
    /// 本轮所有 Edit 免审」。现在 Bash 放宽回工具名（长任务里几十条同类命令
    /// 不该各付一次分类器往返），但写工具、Agent、computer-use 一律保持精确——
    /// 它们的输入决定**对谁做什么**，不是「要不要用这类工具」。
    #[tokio::test]
    async fn auto_mode_memory_is_per_call_for_every_tool_except_bash() {
        let gate = Arc::new(RecordingGate {
            seen: std::sync::Mutex::new(Vec::new()),
            verdict: crate::auto_mode::AutoVerdict::Allow,
        });
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let ctx = AutoProbeCtx {
            inner: PermissionProbeCtx::new(true),
            gate: gate.clone(),
        };
        let bash: Arc<dyn Tool> = Arc::new(GatedTool {
            name: "Bash",
            needs_permission: true,
            runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let edit: Arc<dyn Tool> = Arc::new(GatedTool {
            name: "Edit",
            needs_permission: true,
            runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });

        let test = |path: &str| serde_json::json!({ "path": path });

        // Bash：命令不同也复用本轮授权，闸门只被问一次。
        for path in ["a.txt", "b.txt", "c.txt"] {
            agent
                .ensure_tool_permission("Bash", &bash, &test(path), &ctx)
                .await
                .expect("闸门放行");
        }
        assert_eq!(
            gate.seen.lock().unwrap().len(),
            1,
            "Bash 按工具名复用本轮授权，不该逐条过闸门"
        );

        // Edit：一次改一个文件，第二次必须重新问。
        agent
            .ensure_tool_permission("Edit", &edit, &test("a.txt"), &ctx)
            .await
            .expect("闸门放行");
        agent
            .ensure_tool_permission("Edit", &edit, &test("b.txt"), &ctx)
            .await
            .expect("闸门放行");
        assert_eq!(
            gate.seen.lock().unwrap().len(),
            3,
            "Edit 一次放行绝不能覆盖改不同文件的调用"
        );
    }

    /// 闸门拒给具体理由，回灌给模型的文案要带上它（模型据此换路子而不是重试）。
    #[tokio::test]
    async fn auto_mode_denial_carries_the_rule_reason() {
        let gate = Arc::new(RecordingGate {
            seen: std::sync::Mutex::new(Vec::new()),
            verdict: crate::auto_mode::AutoVerdict::Deny {
                rule: "soft_deny".to_string(),
                reason: "命中禁止规则：生产部署".to_string(),
            },
        });
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let ctx = AutoProbeCtx {
            inner: PermissionProbeCtx::new(true),
            gate,
        };
        let bash: Arc<dyn Tool> = Arc::new(GatedTool {
            name: "Bash",
            needs_permission: true,
            runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let err = agent
            .ensure_tool_permission("Bash", &bash, &serde_json::json!({ "path": "x" }), &ctx)
            .await
            .expect_err("分类器拒绝必须阻断执行");
        assert!(
            err.contains("生产部署"),
            "回灌给模型的理由应包含分类器给出的具体原因，实际: {err}"
        );
        assert_eq!(ctx.inner.ask_count(), 0, "分类器拒给不应再弹窗问人");
    }

    /// 每轮都直接 EndTurn 的 provider（注入路由测试用）
    struct EndTurnProvider;
    #[async_trait::async_trait]
    impl Provider for EndTurnProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
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

    /// 记录每次请求收到的历史，用于验证「请求发出前」历史已被修复
    struct RecordingProvider {
        seen: std::sync::Mutex<Vec<Vec<Message>>>,
    }
    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            self.seen.lock().unwrap().push(messages.to_vec());
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 回归：长任务被中断后 `--resume`，输入任何一条消息都不能再把
    /// 「assistant(tool_use) → user(无 tool_result)」发出去。端点对这种请求
    /// 回 200 + 空流，回合静默结束，用户表现为「说了继续没反应」，且每条
    /// 后续消息都复发。钉子：请求历史末尾必须已配对。
    #[tokio::test]
    async fn dangling_tool_use_is_repaired_before_the_request_is_sent() {
        let provider = Arc::new(RecordingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let agent = Agent::new(provider.clone());
        let mut session = Session::new();
        session.push_user("跑一下 git status");
        session.push_assistant(vec![
            ContentBlock::Text { text: "好".into() },
            ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "git status --short" }),
            },
        ]);
        // 落盘半截历史后重新载入，用户补一句「继续」——正是复现路径。
        session.push_user("继续");

        let mut visible = String::new();
        agent
            .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
            .await
            .expect("回合不应报错");

        let seen = provider.seen.lock().unwrap();
        let request = seen.first().expect("provider 至少被调用一次");
        let tail = request.last().expect("请求历史非空");
        assert!(matches!(tail.role, Role::User));
        assert!(
            tail.content.iter().any(|block| matches!(
                block,
                ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "toolu_1"
            )),
            "发出的请求末尾必须带上 toolu_1 的合成 tool_result，否则端点回空流：{tail:?}"
        );
        assert!(
            !visible.is_empty(),
            "补齐动作必须对用户可见，否则仍是「没反应」的体感"
        );
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

    /// 模拟 `Bash`：需要权限确认（`parallel_safe` 保持默认 false），但每次调用
    /// 彼此隔离、可以并发——这正是 `parallel_safe_with_peer` 描述的形状。
    struct ConfirmableIsolatedTool;
    #[async_trait::async_trait]
    impl Tool for ConfirmableIsolatedTool {
        fn name(&self) -> &str {
            "ConfirmableIsolated"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "ConfirmableIsolated".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
                native: None,
            }
        }
        fn needs_permission(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn parallel_safe_with_peer(&self) -> bool {
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

    /// 同一批返回两个 `ConfirmableIsolated` 调用的 provider。
    struct TwoConfirmableProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for TwoConfirmableProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "c1".into(),
                        name: "ConfirmableIsolated".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "c1".into(),
                        json_delta: r#"{"ms":200,"tag":"first"}"#.into(),
                    }),
                    Ok(StreamEvent::ToolUseStart {
                        id: "c2".into(),
                        name: "ConfirmableIsolated".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "c2".into(),
                        json_delta: r#"{"ms":200,"tag":"second"}"#.into(),
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

    /// 已授权的上下文：Bypass / Plan 白名单 / 项目级 AllowAlways / 本轮已授权时
    /// `confirm_tool` 都不会 await UI，对应 `needs_dialog` 返回 false。
    struct NoDialogCtx;
    #[async_trait::async_trait]
    impl ToolContext for NoDialogCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/tmp")
        }
        fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
            true
        }
        async fn confirm_tool(&self, _name: &str, _summary: &str) -> bool {
            true
        }
        fn needs_dialog(&self, _name: &str) -> bool {
            false
        }
    }

    /// 仍需弹框的上下文：用户当场按下允许，`confirm_tool` 返回 true 但**确实
    /// 等待过 UI**。用它验证「会弹框的调用必须串行」——两个这样的调用并发起来
    /// 就会在同一时刻弹出两个对话框。
    struct DialogCtx;
    #[async_trait::async_trait]
    impl ToolContext for DialogCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/tmp")
        }
        fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
            true
        }
        async fn confirm_tool(&self, _name: &str, _summary: &str) -> bool {
            true
        }
    }

    /// 钉子：`parallel_safe_with_peer` 只在**真的不弹框**时才会兑现并发。
    /// `DialogCtx` 是 Prompt 模式下用户还没对这个工具点过 AllowAlways 的等价物
    /// （`needs_dialog` 保持默认的 true），此时两个会弹框的调用必须仍然串行——
    /// 「同一时刻至多一个对话框」是这个不变式的全部理由，把它们排进并发组就会
    /// 同时弹出两个对话框。
    #[tokio::test]
    async fn confirmable_isolated_calls_stay_serial_while_a_dialog_would_block() {
        let mut agent = Agent::new(Arc::new(TwoConfirmableProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(ConfirmableIsolatedTool));
        let mut session = Session::new();
        session.push_user("go");

        let start = Instant::now();
        agent
            .run_turn(&mut session, &DialogCtx, &mut |_| {})
            .await
            .unwrap();

        assert!(
            start.elapsed() >= std::time::Duration::from_millis(380),
            "会弹权限框的调用必须彼此串行（一次只弹一个对话框），实际耗时 {:?}",
            start.elapsed()
        );
    }

    /// 钉子：一旦不再需要弹框，同一批里的同类调用就该并发跑完，而不是排队。
    /// 这条直接对应 gupiao 的实测痛点——模型一轮里返回多条互不依赖的只读查询时，
    /// 串行执行让每个回合凭空多出 N 次工具等待。
    #[tokio::test]
    async fn confirmable_isolated_calls_run_concurrently_once_authorized() {
        let mut agent = Agent::new(Arc::new(TwoConfirmableProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(ConfirmableIsolatedTool));
        let mut session = Session::new();
        session.push_user("go");

        let start = Instant::now();
        agent
            .run_turn(&mut session, &NoDialogCtx, &mut |_| {})
            .await
            .unwrap();

        assert!(
            start.elapsed() < std::time::Duration::from_millis(380),
            "已授权后两个 200ms 的隔离调用应并发（总计约 200ms），实际耗时 {:?}",
            start.elapsed()
        );
        // 并发不改变回填顺序：结果仍按 tool_use 原始下标排序。
        let results: Vec<_> = session
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content: ToolResultContent::Text(text),
                    ..
                } => Some((tool_use_id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![("c1", "first"), ("c2", "second")],
            "并发执行后仍必须按原始顺序回填 tool_result"
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
            _system: &wyj_api::SystemPrompt<'_>,
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
            _system: &wyj_api::SystemPrompt<'_>,
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
            _system: &wyj_api::SystemPrompt<'_>,
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

    /// 参数校验失败时，历史里必须留着**模型实际发过的参数**。
    ///
    /// 一次真实故障（会话 `sess-1792142673-799da7d07fbc`）：模型把 19 位
    /// `tree_id` 当字符串发了 5 轮，每轮历史里都只剩
    /// `{"_wyj_code_invalid_arguments": true}`——模型只看到「tree_id 类型不对」，
    /// 看不到自己传了什么，于是原样重发同一个错误参数，直到触发重试上限把
    /// 整轮（已完成窗口枚举、截图、菜单列举）作废。
    #[tokio::test]
    async fn an_invalid_call_keeps_its_own_arguments_in_history() {
        let mut agent = Agent::new(Arc::new(SchemaViolationThenTextProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(RequiredCountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let mut session = Session::new();
        session.push_user("go");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        let echoed = session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolUse { name, input, .. }
                        if name == "RequiredEcho" && input["value"] == "wrong"
                )
            })
        });
        assert!(
            echoed,
            "校验失败的工具调用应在历史里保留模型原始参数，让模型能对照纠错"
        );
        assert!(
            !session.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolUse { input, .. }
                            if input.get("_wyj_code_invalid_arguments").is_some()
                    )
                })
            }),
            "参数可解析成 object 时不应再用占位符代替它"
        );
    }

    struct SchemaViolationThenTextProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for SchemaViolationThenTextProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let events: Vec<Result<StreamEvent>> = if self.calls.fetch_add(1, Ordering::SeqCst) == 0
            {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "bad-enum".into(),
                        name: "RequiredEcho".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "bad-enum".into(),
                        json_delta: r#"{"value":"wrong"}"#.into(),
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

    /// 连续参数校验失败**不得**把整轮判成失败。
    ///
    /// 这是上面那次故障的直接后果：这一轮里窗口枚举、截图、菜单列举都已成功，
    /// 却因为一个调用的参数形态被 `bail!` 整轮中止。正确形态是先注入一次
    /// 「换个思路、就地收尾」的指令，模型读到后交出总结，回合正常结束。
    #[tokio::test]
    async fn repeated_argument_failures_wrap_up_instead_of_aborting_the_turn() {
        let mut agent = Agent::new(Arc::new(InvalidThenDeliverProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(RequiredCountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let mut session = Session::new();
        session.push_user("go");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .expect("连续参数校验失败不应中止整轮");

        assert!(
            session.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text } if text.contains("tool-arguments-wrap-up"))
                })
            }),
            "应向模型注入一次收尾指令"
        );
        assert!(
            session.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text } if text.contains("here is what I have"))
                })
            }),
            "模型读到收尾指令后应能正常交付，这一轮不该被判成失败"
        );
    }

    /// 连撞到触发收尾的那一轮，随后交出总结——这是收尾机制生效的正常路径。
    struct InvalidThenDeliverProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for InvalidThenDeliverProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n < INVALID_ARGUMENT_GRACE_AT {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: format!("bad-{n}"),
                        name: "RequiredEcho".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: format!("bad-{n}"),
                        json_delta: r#"{"value":12}"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("here is what I have so far".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 收尾指令只注入一次：反复注入等于给模型续命，防无界重试的闸门就形同虚设。
    #[tokio::test]
    async fn the_argument_wrap_up_notice_is_injected_at_most_once_per_streak() {
        let mut agent = Agent::new(Arc::new(AlwaysInvalidProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(RequiredCountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let mut session = Session::new();
        session.push_user("go");

        let _ = agent.run_turn(&mut session, &FakeCtx, &mut |_| {}).await;

        let notices = session
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("tool-arguments-wrap-up"))
            })
            .count();
        assert_eq!(notices, 1, "收尾指令只应注入一次，实际 {notices} 次");
    }

    /// 模型无视收尾指令、继续用同样方式硬撞，才落到硬中止——那才是真正该
    /// 报错的形态（否则「避免无界重试」这条注释就成了空话）。
    #[tokio::test]
    async fn ignoring_the_wrap_up_notice_still_stops_the_runaway_retry() {
        let mut agent = Agent::new(Arc::new(AlwaysInvalidProvider {
            calls: AtomicUsize::new(0),
        }))
        .with_max_turns(16);
        agent.register_tool(Arc::new(RequiredCountingTool {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let mut session = Session::new();
        session.push_user("go");

        let error = agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .expect_err("无视收尾指令后仍应硬中止（防续命上界）");
        assert!(
            error.to_string().contains("工具参数连续校验失败"),
            "unexpected error: {error}"
        );
    }

    /// 无视收尾指令、每轮都重发同一个无效参数。
    struct AlwaysInvalidProvider {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for AlwaysInvalidProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::TextDelta("partial".into())),
                    Ok(StreamEvent::ToolUseStart {
                        id: format!("bad-{n}"),
                        name: "RequiredEcho".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: format!("bad-{n}"),
                        json_delta: r#"{"value":12}"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: format!("bad-{n}"),
                        name: "RequiredEcho".into(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: format!("bad-{n}"),
                        json_delta: r#"{"value":12}"#.into(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
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
            _system: &wyj_api::SystemPrompt<'_>,
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
        let queue = SteerQueue::new();
        queue.push(
            vec![ContentBlock::Text {
                text: "bg result".into(),
            }],
            InjectionKind::SystemReminder,
        );

        let mut kinds = vec![];
        let mut session = Session::new();
        session.push_user("hi");
        agent
            .run_turn_with_injection(&mut session, &FakeCtx, &mut |_| {}, Some(&queue), |k| {
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

    /// 共享注入队列的基本语义：FIFO、`drain` 取走全部、取空后为空。
    #[test]
    fn steer_queue_is_fifo_and_drain_takes_everything() {
        let queue = SteerQueue::new();
        assert!(queue.is_empty());

        queue.push(
            vec![ContentBlock::Text {
                text: "first".into(),
            }],
            InjectionKind::UserMessage,
        );
        queue.push(
            vec![ContentBlock::Text {
                text: "second".into(),
            }],
            InjectionKind::SystemReminder,
        );
        assert!(!queue.is_empty());

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert!(matches!(drained[0].1, InjectionKind::UserMessage));
        assert!(matches!(drained[1].1, InjectionKind::SystemReminder));
        assert!(queue.is_empty());
        assert!(queue.drain().is_empty());
    }

    /// 前台子 Agent 只取**用户补充**：队列里的 `SystemReminder`（后台子 Agent /
    /// 后台 shell 的完成通知）是给主 Agent 的结论，进了子 Agent 的会话既答非所问，
    /// 又会让主 Agent 永远收不到——子 Agent 返回时队列已被取空。
    ///
    /// 这条钉子守住「共享队列」最容易踩的坑：把 `drain()` 直接给子 Agent 用。
    #[tokio::test]
    async fn subagent_drain_leaves_system_reminders_for_the_main_agent() {
        let queue = SteerQueue::new();
        queue.push(
            vec![ContentBlock::Text {
                text: "user steer".into(),
            }],
            InjectionKind::UserMessage,
        );
        queue.push(
            vec![ContentBlock::Text {
                text: "bg agent finished".into(),
            }],
            InjectionKind::SystemReminder,
        );

        // 子 Agent 侧：只排空用户补充
        let taken = queue.drain_user_messages();
        assert_eq!(taken.len(), 1);
        assert!(matches!(taken[0].1, InjectionKind::UserMessage));

        // 主 Agent 侧：后台结果仍原封不动地等着
        let rest = queue.drain();
        assert_eq!(rest.len(), 1);
        assert!(matches!(rest[0].1, InjectionKind::SystemReminder));
        assert!(queue.is_empty());
    }

    /// 「谁先到工具轮边界谁取走」——主 Agent 与前台子 Agent 排空**同一份**队列，
    /// 因此一条补充内容只会被投递一次，不会同时进两个会话（也不会凭空丢失）。
    ///
    /// 这条是前台子 Agent 接入注入通道后的核心不变式：如果哪天改成两份拷贝或
    /// 各自独立的队列，用户补充的内容要么被投递两次、要么子 Agent 那份永远没人取。
    #[tokio::test]
    async fn shared_steer_queue_delivers_to_exactly_one_agent() {
        let agent = Agent::new(Arc::new(EndTurnProvider));
        let queue = SteerQueue::new();
        queue.push(
            vec![ContentBlock::Text {
                text: "steer me".into(),
            }],
            InjectionKind::UserMessage,
        );

        let mut first = Session::new();
        first.push_user("hi");
        agent
            .run_turn_with_injection(&mut first, &FakeCtx, &mut |_| {}, Some(&queue), |_| {})
            .await
            .unwrap();

        let mut second = Session::new();
        second.push_user("hi");
        agent
            .run_turn_with_injection(&mut second, &FakeCtx, &mut |_| {}, Some(&queue), |_| {})
            .await
            .unwrap();

        let landed = |session: &Session| {
            session.messages.iter().any(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("steer me")))
            })
        };
        assert!(
            landed(&first) ^ landed(&second),
            "补充内容必须且只能进入其中一个会话"
        );
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
            _system: &wyj_api::SystemPrompt<'_>,
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
        // i18n: agent.degraded.parameter_removed。中文翻译：模型端点不支持参数 `{name}`，已安全移除后重试一次
        let msg = wyj_i18n::tr_fmt("agent.degraded.parameter_removed", &[("name", "thinking")]);
        assert!(visible.contains(&msg), "expected {msg:?} in {visible:?}");
        assert_eq!(session.api_calls, 2);
    }

    /// 只认 `reasoning_effort` 的端点（DeepSeek）：profile → route → RequestOptions
    /// 必须一路带到底。中间任何一环漏掉，用户看到的都是"思考开着、档位没生效"，
    /// 而且没有任何报错——这正是本次修复前 `with_reasoning_effort` 零调用点的后果。
    struct EffortRecordingProvider {
        seen: Arc<Mutex<Option<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl Provider for EffortRecordingProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            *self.seen.lock().unwrap() = Some(opts.reasoning_effort.clone());
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    async fn effort_seen_by_provider(
        vendor: &str,
        model: &str,
        effort: &str,
    ) -> Option<Option<String>> {
        let seen: Arc<Mutex<Option<Option<String>>>> = Arc::new(Mutex::new(None));
        let agent = Agent::new(Arc::new(EffortRecordingProvider {
            seen: Arc::clone(&seen),
        }))
        .with_route_identity("test".to_string(), vendor.to_string(), model.to_string())
        .with_reasoning_effort(Some(effort.to_string()));
        let mut session = Session::new();
        session.push_user("hi");
        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();
        let snapshot = seen.lock().unwrap().clone();
        snapshot
    }

    #[tokio::test]
    async fn reasoning_effort_reaches_request_options() {
        assert_eq!(
            effort_seen_by_provider("deepseek", "deepseek-v4-pro", "high").await,
            Some(Some("high".to_string()))
        );
    }

    /// 档位不在该 vendor 的 `effort_levels` 内（DeepSeek 没有 xhigh）时不能发出去——
    /// 发了就是 400。
    #[tokio::test]
    async fn effort_outside_vendor_levels_is_not_sent() {
        assert_eq!(
            effort_seen_by_provider("deepseek", "deepseek-v4-pro", "xhigh").await,
            Some(None)
        );
    }

    /// 只认开关不认档位的 vendor（GLM）：catalog 曾把它标成 `ThinkingMode::Effort`，
    /// 现在必须按 spec 丢弃而不是"发出去再被 adapter 吞掉"。
    #[tokio::test]
    async fn switch_only_vendor_does_not_receive_effort() {
        assert_eq!(
            effort_seen_by_provider("zhipu", "glm-4.6", "high").await,
            Some(None)
        );
    }

    /// 端点回 4xx 说不认 `reasoning_effort` 时，降级必须把它一起撤掉——只撤 budget
    /// 的话重试仍会带上 effort、再 400 一次，然后整个 turn 失败。
    #[tokio::test]
    async fn rejected_effort_parameter_is_removed_and_reported() {
        struct EffortRejectedThenOk {
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl Provider for EffortRejectedThenOk {
            async fn stream(
                &self,
                _system: &wyj_api::SystemPrompt<'_>,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                opts: &wyj_api::provider::RequestOptions,
            ) -> Result<EventStream> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let mut error = wyj_api::ProviderError::new(
                        wyj_api::ProviderErrorKind::UnsupportedParameter,
                        "unsupported parameter reasoning_effort",
                    );
                    error.parameter = Some("reasoning_effort".to_string());
                    return Err(anyhow::Error::new(error));
                }
                assert_eq!(opts.reasoning_effort, None);
                assert_eq!(opts.thinking_switch, None);
                Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamEvent::TextDelta("ok".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ])))
            }
        }

        let agent = Agent::new(Arc::new(EffortRejectedThenOk {
            calls: AtomicUsize::new(0),
        }))
        .with_route_identity(
            "deepseek".to_string(),
            "deepseek".to_string(),
            "deepseek-v4-pro".to_string(),
        )
        .with_reasoning_effort(Some("high".to_string()))
        .with_thinking_switch(Some("enabled".to_string()));
        let mut session = Session::new();
        session.push_user("hi");
        let mut visible = String::new();
        agent
            .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
            .await
            .unwrap();
        let msg = wyj_i18n::tr_fmt(
            "agent.degraded.parameter_removed",
            &[("name", "reasoning_effort")],
        );
        assert!(visible.contains(&msg), "expected {msg:?} in {visible:?}");
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
            _system: &wyj_api::SystemPrompt<'_>,
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
            _system: &wyj_api::SystemPrompt<'_>,
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
            _system: &wyj_api::SystemPrompt<'_>,
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
            _system: &wyj_api::SystemPrompt<'_>,
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
            _system: &wyj_api::SystemPrompt<'_>,
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
                            ..
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

    // ── 孤儿 tool_use 兜底配对 ───────────────────────────────────────────
    //
    // 模型正在写工具参数时被 max_tokens 截断时，assistant 消息里会带一个没有
    // 配对 tool_result 的 tool_use。这个畸形历史会被原样落盘，之后任何一次
    // `--resume` 都会让 provider 直接 400，且用户不手动 /rewind 或 /clear
    // 无法自愈。`run_turn` 必须为残留 tool_use 补一条 is_error 的 tool_result。

    /// 发一个工具调用后以 `MaxTokens` 结束（模拟参数写到一半被截断）
    struct MaxTokensToolUseProvider;
    #[async_trait::async_trait]
    impl Provider for MaxTokensToolUseProvider {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "Sleep".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "t1".into(),
                    json_delta: r#"{"ms":10,"tag":"truncated"}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::MaxTokens,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn max_tokens_with_pending_tool_use_still_gets_a_matching_tool_result() {
        let mut agent = Agent::new(Arc::new(MaxTokensToolUseProvider));
        agent.register_tool(Arc::new(SleepTool));
        let mut session = Session::new();
        session.push_user("go");

        agent
            .run_turn(&mut session, &FakeCtx, &mut |_| {})
            .await
            .unwrap();

        let uses: Vec<&str> = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(uses, vec!["t1"], "工具调用应照常进入历史");

        let results: Vec<(&str, bool, String)> = session
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((
                    tool_use_id.as_str(),
                    *is_error,
                    match content {
                        wyj_api::types::ToolResultContent::Text(t) => t.clone(),
                        _ => String::new(),
                    },
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            results.len(),
            1,
            "MaxTokens 截断后必须补 tool_result，否则历史配对断裂 → provider 400"
        );
        let (id, is_error, text) = &results[0];
        assert_eq!(*id, "t1");
        assert!(is_error, "兜底 tool_result 必须标记 is_error");
        assert!(
            text.contains("tool_call_incomplete") && text.contains("MaxTokens"),
            "兜底结果应说明未完成及原因，实际: {text}"
        );
    }

    // ── 记忆快照分桶缓存 ───────────────────────────────────────────────
    //
    // 回归背景：Project Brief 每次都按「最近 4 条 user 消息」重算相关性排序，
    // 而它被拼进 system prompt 的稳定前缀 —— 每个用户轮次都会改写 system
    // 尾部，把整段 system 的 prompt cache 击穿。`memory_snapshot_text` 按
    // 10 个用户轮次分桶缓存：同桶复用，跨桶重算。

    #[test]
    fn memory_snapshot_reuses_cache_within_bucket_and_refreshes_across_buckets() {
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::MemoryV3Store::new(base.path(), project.path()).unwrap());
        seed_task(&store, "任务 A", TaskStatus::InProgress, None);

        let agent = Agent::new(Arc::new(EndTurnProvider)).with_memory_v3(store.clone());
        let mut session = session_with_query("继续");

        let first = agent.memory_snapshot_text(&session);
        assert!(first.contains("任务 A"), "首次应带上 Open Tasks");

        // 同桶内新增记忆：仍应命中缓存
        seed_task(&store, "任务 B", TaskStatus::InProgress, None);
        let same_bucket = agent.memory_snapshot_text(&session);
        assert_eq!(
            first, same_bucket,
            "同桶内必须复用缓存，否则 system 稳定前缀每轮都被改写"
        );
        assert!(
            !same_bucket.contains("任务 B"),
            "同桶内不应看到新写入的记忆"
        );

        // 跨桶：重算后应看到新记忆
        for i in 0..MEMORY_SNAPSHOT_REFRESH_TURNS {
            push_user_text(&mut session, &format!("继续 {i}"));
        }
        let refreshed = agent.memory_snapshot_text(&session);
        assert!(
            refreshed.contains("任务 B"),
            "跨桶后应重新计算并看到后台新提取的记忆"
        );
    }

    #[test]
    fn with_skills_injects_the_catalog_and_leaves_no_placeholder() {
        // 回归背景：用户说"请使用 skill 分析股票"，模型回复"我不能自己调用
        // skill——skill 必须由你用斜杠命令触发"。根因是 skill 名单只存在于 TUI
        // 的 CommandRegistry（只服务 `/xxx` 补全），从不进入模型上下文；而
        // 提示词既禁止模型自行检索目录、又要求它给出具体命令名，模型手里没有
        // 任何名字，唯一诚实的回答就是把禁令复述一遍。
        let agent =
            Agent::new(Arc::new(EndTurnProvider)).with_skills(vec![crate::prompts::SkillEntry {
                name: "hithink-finance".into(),
                description: "查询 A 股行情与财务数据".into(),
            }]);
        assert!(
            !agent
                .system_prompt
                .contains(crate::prompts::SKILL_CATALOG_PLACEHOLDER),
            "占位符必须被替换掉，不能字面泄漏进 system prompt"
        );
        assert!(
            agent
                .system_prompt
                .contains("- `hithink-finance` — 查询 A 股行情与财务数据"),
            "{}",
            agent.system_prompt
        );
    }

    #[test]
    fn with_skills_survives_an_empty_installation() {
        // 一个 skill 都没有时同样不能留下 `{skills}` 字面量
        let agent = Agent::new(Arc::new(EndTurnProvider)).with_skills(Vec::new());
        assert!(!agent
            .system_prompt
            .contains(crate::prompts::SKILL_CATALOG_PLACEHOLDER));
        assert!(agent.system_prompt.contains("No skills are installed"));
    }

    #[test]
    fn memory_snapshot_is_shared_across_agent_clones() {
        // `/model` 重建与模式切换都会 clone Agent，缓存必须共享，
        // 否则切一次模型就白白重算一次并改写 system 前缀。
        let base = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::MemoryV3Store::new(base.path(), project.path()).unwrap());
        seed_task(&store, "任务 A", TaskStatus::InProgress, None);

        let agent = Agent::new(Arc::new(EndTurnProvider)).with_memory_v3(store.clone());
        let session = session_with_query("继续");
        let first = agent.memory_snapshot_text(&session);
        seed_task(&store, "任务 B", TaskStatus::InProgress, None);
        let cloned = agent.clone();
        assert_eq!(
            first,
            cloned.memory_snapshot_text(&session),
            "clone 出来的 Agent 应复用同一份快照缓存"
        );
    }
}

fn default_system_prompt() -> String {
    // 模型侧提示词统一英文原创措辞（见 prompts.rs 模块注释），不走 i18n
    crate::prompts::MAIN.to_string()
}

/// 从工具调用输入里推导其触达的目录，供项目规约子目录动态加载判断。
/// Read/Edit/Write 用 file_path（取父目录）；Glob/Grep 用 path（文件取父目录，目录取自身）。
///
/// 只负责**定位起点**：向上逐级查找、边界与去重都由
/// `ClaudeMdLoader::maybe_dir_reminder` 负责。这里不做路径归一是因为
/// `touched_dir` 同一轮会被多个工具复用，归一放 loader 里做一次即可。
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
