//! Agent 推理循环：多轮工具调用直到 stop_reason 不再是 tool_use。

use crate::claude_md::ClaudeMdLoader;
use crate::compact::{compact_session_until_fit, compact_trigger_buffer, ContextAudit};
use crate::evolution::EvolutionStore;
use crate::hooks::{HookOutcome, HookRunner};
use crate::memory::MemoryStore;

mod helpers;
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests;

use crate::memory_v3::MemoryV3Store;
use crate::session::Session;
use crate::tool::{Tool, ToolCallMeta, ToolContext};
use crate::tool_arguments::{ToolArgumentPipeline, ValidatedToolCall};
use anyhow::Result;
use futures::StreamExt;
use helpers::*;
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
///
/// 直接转发到 `compact::estimate_tools_tokens`：两处各自维护一份「按工具
/// 序列化 schema 再估 token」的逻辑是缓存失效的温床（改了审计口径忘了改这里，
/// `/context` 与 `/cost` 又劈叉）。缓存也共用一份。
fn estimate_tool_schema_tokens(tools: &[ToolDefinition]) -> u32 {
    crate::compact::estimate_tools_tokens(tools)
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
    /// `base_url` 与 `wire_protocol` 是**必传**参数，不是可选的 builder 后缀。
    ///
    /// 两者都参与 `ModelIdentity` 的构造，而 `ModelIdentity` 又是
    /// capability_cache 的 fingerprint 输入与 thinking spec 的查表键：
    /// 少了 base_url → 同 vendor+model 的不同端点共用同一份 cache 文件，一个
    /// 端点的 4xx 拒绝记录会连带禁掉另一个端点的参数；少了 wire_protocol →
    /// `ThinkingSpec::for_vendor` 按 OpenAI 线查表，Anthropic 线的端点会拿到
    /// 错档的 effort 规格。
    ///
    /// 做成必传而不是 `with_identity()` 后缀，是为了让「漏接线」在编译期暴露：
    /// 之前 `with_identity` 零调用，两个字段恒为空串 / OpenAI，症状要等端点
    /// 返回 400 才看得见。
    pub fn new(
        profile_name: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        wire_protocol: wyj_config::WireProtocol,
        provider: Arc<dyn Provider>,
    ) -> Self {
        Self {
            profile_name: profile_name.into(),
            vendor: vendor.into(),
            model: model.into(),
            base_url: base_url.into(),
            wire_protocol,
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
    system_prompt: Arc<str>,
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
    /// 辅助模型 provider（可选）。
    ///
    /// 只用于**短提示词、低风险**的辅助调用：跨会话记忆提取。这类请求用
    /// 自己的短 system prompt（`prompts::MEMORY_SYSTEM`），**不复用主对话的
    /// system + tools 前缀**，因此换模型不会让主会话的 prompt cache 失效。
    ///
    /// **上下文压缩（compact）绝不使用它**：压缩请求逐字节复用主对话前缀来
    /// 命中缓存，换模型等于前缀不等 → 缓存归零 → 整段历史全价重发。
    ///
    /// `None` = 辅助调用也走主 provider。
    aux_provider: Option<Arc<dyn Provider>>,
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
    /// 本进程是否已尝试过生成会话标题。
    ///
    /// 标题只在首轮生成一次，`SummaryGenerator::generate_title` 内部靠
    /// `SessionFile.title_generated` 挡住第二次 LLM 调用——但**磁盘 IO 挡不住**：
    /// 每次调用都要 `store.load()` 重读并解析整个 session JSON，而调用点在
    /// 每个回合收尾都会走一遍。加上这个标志后，每进程只尝试一次，
    /// 省掉每轮一次全量 session 文件的读 + 解析。
    title_attempted: Arc<std::sync::atomic::AtomicBool>,
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
    /// **最近一次真实请求**的 system volatile 段全文（见
    /// `SystemPrompt` 的 stable/volatile 分段约定）。
    ///
    /// 存在的唯一理由是手动 `/compact`：压缩请求要复用主对话前缀才能命中
    /// provider 的缓存，而 volatile 段里装着「本轮触达目录的规约 reminder」
    /// 「工具可用性名单」「语言提醒」——它们在回合之外**无法重建**（子目录
    /// reminder 由 `maybe_dir_reminder` 按首次触达注入，`seen_dirs` 是全局去重的，
    /// 第二个回合起就再也拿不到了）。旧实现干脆传 `volatile: ""`，缓存必 miss。
    ///
    /// 缓存「上一次真正发出去的字节」是唯一能做到逐字节一致的做法；没有历史
    /// 请求（新会话第一条就 `/compact`）时退回按当前 route 现拼。
    last_request_volatile: Arc<std::sync::Mutex<String>>,
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
            system_prompt: Arc::from(default_system_prompt()),
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
            aux_provider: None,
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
            last_request_volatile: Arc::new(std::sync::Mutex::new(String::new())),
            hook_runner: None,
            checkpoint_store: None,
            loop_guard: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            turn_permission_grants: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            turn_user_goal: Arc::new(std::sync::Mutex::new(String::new())),
            capability_cache: None,
            verify_cfg: None,
            title_attempted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    /// 设置主路由的身份信息。
    ///
    /// `base_url` 与 `wire_protocol` 必传：它们会被 `route_at()` 拷进
    /// `AgentRoute`，再由 `ModelIdentity` 喂给 capability_cache 的 fingerprint
    /// 与 `ThinkingSpec::for_vendor` 的查表。漏传的症状是「同 vendor+model 的
    /// 不同端点互相污染能力缓存」和「Anthropic 线端点拿到 OpenAI 线的 effort
    /// 规格」——两者都要等端点真的回 400 才暴露。
    pub fn with_route_identity(
        mut self,
        profile_name: impl Into<String>,
        vendor: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        wire_protocol: wyj_config::WireProtocol,
    ) -> Self {
        self.route_profile_name = profile_name.into();
        self.route_vendor = vendor.into();
        self.route_model = model.into();
        self.route_base_url = base_url.into();
        self.route_wire_protocol = wire_protocol;
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

    /// 为**另一个会话**派生一份 Agent：共享配置与不可变资源，重置全部回合级
    /// 可变状态。
    ///
    /// `Agent: Clone` 是浅拷贝——`Arc<Mutex<..>>` 字段被**共享**而非复制。这对
    /// 「同一会话内重建」（TUI `/model`、模式切换）正是想要的：重建出的 Agent
    /// 应当继承同一份记忆快照缓存与授权集。但对 **daemon/ACP 的多会话**是错的：
    /// `create_session` 直接 `agent_template.clone()`，于是
    ///
    /// - `turn_permission_grants` 共享 → 会话 A 批准过的 Bash 让会话 B 的 Bash
    ///   免检，且每个回合入口的 `clear()` 会**互相清空对方**的授权；
    /// - `loop_guard` 共享 → A 的重复调用把 B 的同名同参调用判成死循环并跳过；
    /// - `memory_snapshot` / `last_request_volatile` 共享 → A 的规约 reminder
    ///   全文被 B 的 `/compact` 当作前缀复用，跨会话内容串味；
    /// - `title_attempted` 共享 → 整个 daemon 只有一个会话能生成标题；
    /// - `active_route` 共享 → A 的主模型 429 切到 fallback，B 跟着换端点。
    ///
    /// 这些字段的共同点是**语义上属于一次会话**，而不是属于进程。需要跨会话
    /// 共享的（provider、工具表、记忆库、hooks、checkpoint store、能力缓存）
    /// 仍是 `Arc` 克隆，不复制。
    ///
    /// 新增回合级字段时**必须**在这里一并重置，否则它会悄悄漏进新会话——
    /// 钉子：`agent::tests::fork_for_new_session_isolates_turn_scoped_state`。
    pub fn fork_for_new_session(&self) -> Self {
        let mut forked = self.clone();
        forked.loop_guard = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        forked.turn_permission_grants =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        forked.turn_user_goal = Arc::new(std::sync::Mutex::new(String::new()));
        forked.memory_snapshot = Arc::new(std::sync::Mutex::new(MemorySnapshotCache::default()));
        forked.last_request_volatile = Arc::new(std::sync::Mutex::new(String::new()));
        forked.title_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        forked.active_route = Arc::new(AtomicUsize::new(0));
        // 会话标识与标题回调属于「本会话」：派生方负责重新设置。
        forked.session_id = None;
        forked.title_cb = None;
        // 事件回调同理——`AgentSessionRuntime::new` 会按新会话重新接线，
        // 沿用父会话的回调会把 B 的事件投进 A 的广播通道。
        forked.session_event_cb = None;
        forked
    }

    /// 注册 thinking 文本增量回调
    pub fn with_thinking_callback(mut self, cb: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.thinking_cb = Some(Arc::new(cb));
        self
    }

    pub fn with_system(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Arc::from(prompt.into());
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
            // 装配期改写：此时通常独占（refcount==1），`Arc::from` 会直接
            // 复用原缓冲，不额外拷贝。
            let mut buffer = String::with_capacity(self.system_prompt.len() + e.len() + 2);
            buffer.push_str(&self.system_prompt);
            buffer.push_str("\n\n");
            buffer.push_str(&e);
            self.system_prompt = Arc::from(buffer);
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
        let replaced = self
            .system_prompt
            .replace(crate::prompts::SKILL_CATALOG_PLACEHOLDER, &catalog);
        self.system_prompt = Arc::from(replaced);
        self
    }

    pub fn with_memory(mut self, mem: Arc<MemoryStore>) -> Self {
        self.memory = Some(mem);
        self
    }

    /// 设置辅助模型的 provider。
    ///
    /// 仅作用于跨会话记忆提取（短提示词、独立 system prompt，不复用主对话
    /// 前缀）。**不影响 compact**——压缩必须用主 provider 才能命中前缀缓存。
    /// `None` 或不调用 = 辅助调用走主 provider。
    pub fn with_aux_provider(mut self, provider: Option<Arc<dyn Provider>>) -> Self {
        self.aux_provider = provider;
        self
    }

    /// 辅助调用实际使用的 provider：优先辅助模型，其次当前 active route。
    ///
    /// 次选取 active route 而非 `self.provider`，是为了保留既有的 fallback
    /// 语义——主模型切换到备用路由时，没配辅助模型的会话也跟着切过去。
    fn aux_provider_or_route(&self) -> Arc<dyn Provider> {
        self.aux_provider
            .clone()
            .unwrap_or_else(|| self.route_at(self.active_route_index()).provider)
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

    /// 落一份 checkpoint。
    ///
    /// `messages` 是**owned** 的历史快照：`spawn_blocking` 要求 `'static`，
    /// 且 `CheckpointStore::create` 内部还会再拷一份（落盘前的 persist cap
    /// 截断需要可变副本）。因此调用方只在**真的要存**时才 clone——
    /// 没有 store 时整段历史一次都不该复制。
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
        match tokio::task::spawn_blocking(move || store.create(&cwd, messages, kind, name)).await {
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

    /// 本次请求**实际发送**的工具集：先按 route 能力裁剪（端点不支持工具
    /// 调用 / 不认严格 schema），再按 lazy 折叠状态过滤。
    ///
    /// 返回 `(实际发送, 折叠前的全量)` 两个列表——后者只用于算「因折叠省下
    /// 多少 schema token」，与请求内容无关。
    ///
    /// 必须由 `run_turn` 与 `compact_context` **共用**：压缩请求的 tools 段
    /// 排在缓存前缀里，少一个工具、或者换一种 schema 简化写法，整段前缀就
    /// 全价重发。旧实现在 `compact_context` 里只按 lazy 状态过滤，漏掉了能力
    /// 裁剪，正是这种「两处各写一半」的典型。
    fn tools_for_request(&self, route: &AgentRoute) -> (Vec<ToolDefinition>, Vec<ToolDefinition>) {
        let candidate: Vec<ToolDefinition> = match &route.capabilities {
            Some(capabilities) if !capabilities.tool_calling.value => Vec::new(),
            Some(capabilities) if !capabilities.strict_tool_schema.value => self
                .tools
                .iter()
                .map(crate::tool_arguments::simplified_tool_definition)
                .collect(),
            _ => self.tools.clone(),
        };
        let sent = candidate
            .iter()
            .filter(|definition| {
                self.lazy_tool_state
                    .as_ref()
                    .map(|state| state.visible(&definition.name))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        (sent, candidate)
    }

    /// 把「与内容无关、每轮可变」的 volatile 三段按真实请求的顺序追加到
    /// `out`：模型兼容 suffix → 工具可用性名单 → 语言提醒。
    ///
    /// 语言提醒必须在最后（它的全部价值就是位置，见
    /// `response_language_reminder_is_the_last_system_section`）。抽出成
    /// 公共函数是为了让 `compact_context` 能复用同一份拼装——压缩请求的
    /// system 必须与主对话逐字节相同才能命中缓存。
    fn push_request_volatile_tail(
        &self,
        route: &AgentRoute,
        request_tools: &[ToolDefinition],
        out: &mut String,
    ) {
        if let Some(capabilities) = &route.capabilities {
            let suffix = wyj_api::PromptPolicy::compatibility_suffix(capabilities);
            if !suffix.is_empty() {
                out.push_str("\n\n");
                out.push_str(suffix);
            }
        }
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
        out.push_str("\n\n");
        out.push_str(&crate::prompts::current_tool_availability_block(
            &attached_tool_names,
            &hidden_sources,
        ));
        if let Some(language) = &self.response_language {
            out.push_str("\n\n");
            out.push_str(&crate::prompts::response_language_reminder(language));
        }
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
        ratio: f32,
        window: u32,
    ) -> u32 {
        use crate::context_edit::elide_tool_results;
        use std::sync::atomic::Ordering;

        let measure = |messages: &[wyj_api::types::Message]| -> u32 {
            ContextAudit::from_request(request_system, messages, request_tools, max_output_tokens)
                .calibrated_total(ratio)
        };
        let mut estimate = measure(&session.messages);

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
            estimate = measure(&session.messages);
        }

        // 清理改变了历史构成，审计快照必须同步刷新，否则 `/context` 面板和
        // 下一次阈值判定会拿着清理前的数字。窗口一并带上：`/context` 显示的
        // 阈值必须与本次判定同源（见 `ContextAudit::window`）。
        let audit = ContextAudit::from_request(
            request_system,
            &session.messages,
            request_tools,
            max_output_tokens,
        )
        .with_window(window);
        session.context_audit = audit;
        audit.calibrated_total(ratio)
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
        // system prompt 含完整 CLAUDE.md 链（本仓库 30KB+）。它被 `Arc<str>`
        // 共享，装配完成后不再改写——本轮**只在真的需要追加时**才复制一份，
        // 否则直接借用（`Cow::Borrowed`），省掉每轮一次 30KB+ 的深拷贝。
        let memory_snapshot = self.memory_snapshot_text(session);
        // 读盘走 `spawn_blocking`（见 `turn_reminder_async` 的说明）：规约文件
        // 链动辄几十 KB，每轮同步读盘会占住 async 运行时的 worker 线程。
        let reminder = match self.claude_md.as_ref() {
            Some(loader) => loader.turn_reminder_async().await,
            None => None,
        };
        let system_stable = if memory_snapshot.is_empty() && reminder.is_none() {
            std::borrow::Cow::Borrowed(self.system_prompt.as_ref())
        } else {
            let mut buffer =
                String::with_capacity(self.system_prompt.len() + memory_snapshot.len() + 64);
            buffer.push_str(&self.system_prompt);
            if !memory_snapshot.is_empty() {
                buffer.push_str("\n\n");
                buffer.push_str(&memory_snapshot);
            }
            if let Some(reminder) = reminder.as_deref() {
                buffer.push_str("\n\n");
                buffer.push_str(reminder);
            }
            std::borrow::Cow::Owned(buffer)
        };
        let mut system_volatile = String::new();

        // 调用方在进入 run_turn 前已 push 本次真实用户消息；checkpoint 保存其
        // 之前的完整对话与当前工作树，确保 /rewind 可以回到提交前边界。
        //
        // `checkpoint_store` 为 None 时**整段历史不复制**：`previous_messages`
        // 是整段会话历史的深拷贝（每条 message + 每个 ContentBlock + 工具
        // 输出），在长会话里这是每轮最贵的一次分配，而没有 store 时它
        // 算完就丢。判据放在这里（而不是 `create_checkpoint` 内部）才能
        // 真的省掉这次 clone。
        if !session.messages.is_empty() && self.checkpoint_store.is_some() {
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
        // 输出预算被 thinking 吃干净时（`stop_reason == MaxTokens` 且整条
        // 回复只有 thinking 块）自动重试**一次**：临时放大输出预算并把
        // reasoning effort 降一档，把预算从思考让回正文。只做一次是因为
        // 端点真的只肯给这么多时，第二次一样会撞线，反复重试只会烧钱。
        // 声明在循环外：重试要 `continue` 整个回合循环，放里面会被重置成
        // false 从而变成无限重试。
        let mut truncation_retry_used = false;
        let mut truncation_retry = false;
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
                // 部分（本轮触达目录的规约 reminder + suffix + 工具可用性
                // + 语言提醒）归入 volatile，尾部三段由 `push_request_volatile_tail`
                // 统一拼装——手动 `/compact` 要复现同一份字节，顺序与内容
                // 分两处写就是下一次失配的起点。
                let mut request_system_volatile = system_volatile.clone();
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
                let mut opts = if let Some(capabilities) = &route.capabilities {
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
                // 截断重试：预算翻倍（不超过端点声明的上限）并降一档 reasoning
                // effort。两个动作都做——只放大预算救不回「模型把新预算又全花在
                // 思考上」，只降 effort 则在某些端点上不改变实际输出上限。
                // `opts` 每轮 attempt 重建，标志在这里消费完即清，不会溢到下一轮。
                if truncation_retry {
                    truncation_retry = false;
                    let doubled = opts.max_tokens.saturating_mul(2);
                    opts.max_tokens = match &route.capabilities {
                        Some(cap) => doubled.min(cap.max_output_tokens),
                        None => doubled,
                    };
                    if let Some(current) = opts.reasoning_effort.as_deref() {
                        let spec = wyj_api::ThinkingSpec::for_vendor(
                            &route_identity.vendor,
                            &route_identity.wire_protocol,
                            &route_identity.model,
                        );
                        if let Some(lower) = downgrade_reasoning_effort(spec.effort_levels, current)
                        {
                            opts.reasoning_effort = Some(lower.to_string());
                        }
                    }
                    tracing::info!(
                        max_tokens = opts.max_tokens,
                        effort = opts.reasoning_effort.as_deref().unwrap_or("-"),
                        "上一条回复被 max_tokens 截断且没有可见内容，放大输出预算后重试"
                    );
                }
                let (request_tools, candidate_tools) = self.tools_for_request(&route);
                let all_schema_tokens = estimate_tool_schema_tokens(&candidate_tools);
                self.push_request_volatile_tail(
                    &route,
                    &request_tools,
                    &mut request_system_volatile,
                );
                let request_system = wyj_api::SystemPrompt {
                    stable: &system_stable,
                    volatile: &request_system_volatile,
                };
                // 存下「本次真正发出去的 volatile 字节」：手动 `/compact` 在回合之外
                // 重建不了子目录 reminder（`seen_dirs` 全局去重，第二个回合起就没
                // 有了），只能靠这份快照才能与主对话前缀逐字节一致、命中缓存。
                if let Ok(mut slot) = self.last_request_volatile.lock() {
                    *slot = request_system_volatile.clone();
                }
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
                // 阈值判定用**校准后**的估算：字符启发式对代码 / JSON / 多语言
                // 混排的偏差可达数十个百分点，而阈值是按比例切的硬线。系数来自
                // 每轮请求成功后 provider 上报的真实 prompt token（见
                // `Session::observe_prompt_tokens`），未校准时就是 1.0，行为与
                // 旧版逐字节相同。
                let ratio = session.estimate_ratio();
                let estimated_before = context_audit.calibrated_total(ratio);
                // 有效窗口：配置值，但被端点拒绝过就按实测值收紧。`context_window`
                // 是裸配置，填大了两道防线会一起落在够不着的位置。
                let effective_window = session.effective_context_window(route.context_window);
                session.context_audit = context_audit.with_window(effective_window);
                let compact_threshold = effective_window
                    .saturating_sub(compact_trigger_buffer(effective_window, opts.max_tokens));
                // 软阈值：context editing 的**动手线**，与上面的压缩硬阈值分开。
                // 硬阈值决定"何时停手"，软阈值决定"何时开始"——1M 窗口下硬阈值
                // 在 900K，等它触发意味着整段历史被原样重发了几百趟。
                // `soft_limit_ratio = 0` 时取 u32::MAX，条件退化成
                // `estimated_before > compact_threshold`，与改动前完全一致。
                let soft_threshold =
                    soft_threshold_tokens(effective_window, self.context_edit.as_ref());

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
                            ratio,
                            effective_window,
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
                    //
                    // 收敛判据必须与进入条件**同口径**：进入用的是请求级
                    // `ContextAudit::total()`（含 system / 工具 schema / 输出
                    // 预留），若循环内退回 messages-only 的 `estimate_tokens`，
                    // 两者相差的 4~5 万 token 会让它误判「还没压够」，白跑第
                    // 2、3 轮——而每多跑一轮就会把上一轮刚保留的尾部原文再
                    // 摘要一次，这正是长会话越压越失忆的放大器。
                    let measure_request = |messages: &[wyj_api::types::Message]| -> u32 {
                        ContextAudit::from_request(
                            &request_system,
                            messages,
                            &request_tools,
                            opts.max_tokens,
                        )
                        .calibrated_total(ratio)
                    };
                    let outcome = compact_session_until_fit(
                        session,
                        route.provider.as_ref(),
                        effective_window,
                        compact_threshold,
                        compact_prefix,
                        &measure_request,
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
                        // 压缩改写了整段历史，审计快照必须立刻重算：否则
                        // ① `/context` 面板显示的仍是压缩前的占用；
                        // ② 下一次 usage 入账时的校准采样会拿压缩前的旧估算
                        // 当分母，把系数系统性压低、阈值越推越晚。
                        session.context_audit = ContextAudit::from_request(
                            &request_system,
                            &session.messages,
                            &request_tools,
                            opts.max_tokens,
                        )
                        .with_window(effective_window);
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
                // 输出预留不随 thinking 降级改变，但 `opts` 在这里已被移进
                // `effective_opts`，留一份标量给下面的超限判定用。
                let request_max_tokens = opts.max_tokens;
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
                                // 记下这次被拒的真实 prompt 规模：配置里的
                                // `context_window` 是裸值，端点说「太长」就是它
                                // 唯一一次给的实测证据。后续有效窗口按
                                // `min(configured, hint)` 收紧，否则阈值会一直
                                // 落在够不着的位置，每回合都要撞一次 400 才能压。
                                let rejected = ContextAudit::from_request(
                                    &request_system,
                                    outbound_messages,
                                    &request_tools,
                                    request_max_tokens,
                                )
                                .calibrated_total(ratio);
                                session.note_context_overflow(rejected);
                                on_text(&wyj_i18n::tr("agent.context_overflow"));
                                match crate::compact::compact_session(
                                    session,
                                    route.provider.as_ref(),
                                    effective_window,
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
                                // 压缩改写了历史，审计快照必须跟着刷新，否则
                                // ① 重试请求后的校准采样会拿压缩前的旧估算当分母，
                                // 把系数系统性压低、阈值越推越晚；
                                // ② `/context` 面板显示的仍是压缩前的占用。
                                session.context_audit = ContextAudit::from_request(
                                    &request_system,
                                    &session.messages,
                                    &request_tools,
                                    request_max_tokens,
                                )
                                .with_window(effective_window);
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
                                    timestamp: crate::history::now_iso(),
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
                            //
                            // 校准采样只取**最后一条**：重试路径可能在同一轮回填多次
                            // usage，累加会把真实 prompt 抬高一倍，系数随之被系统性
                            // 高估——而系数直接进压缩阈值判定。
                            let real_prompt = pending_usage
                                .last()
                                .map(|(input, _, cache_read, cache_write)| {
                                    input
                                        .saturating_add(*cache_read)
                                        .saturating_add(*cache_write)
                                })
                                .unwrap_or(0);
                            session.observe_prompt_tokens(real_prompt);
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
                                timestamp: crate::history::now_iso(),
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
            let visible_output = has_visible_output(&assistant_blocks);
            // 输出预算被 thinking 吃干净：`MaxTokens` 截断 + 整条回复只有
            // thinking 块。这时把它 push 进历史只会让下一次请求揣着一条空
            // signature 的 thinking 继续在同样预算上重演，所以**不 push**，
            // 放大预算后重新生成一次。只重试一次：第二次仍撞线同样丢弃，
            // 并在收尾处明确告诉用户，而不是静默结束回合。
            let truncated_without_output = stop_reason == StopReason::MaxTokens && !visible_output;
            if truncated_without_output && !truncation_retry_used {
                truncation_retry_used = true;
                truncation_retry = true;
                tracing::warn!(
                    model = %used_route.model,
                    max_tokens = used_route.max_tokens,
                    "模型输出被 max_tokens 截断且只有 thinking，丢弃本条回复并重试一次"
                );
                on_text(&wyj_i18n::tr("agent.output_truncated_retrying"));
                continue;
            }
            // 第二次仍撞线：这条同样只有 thinking，落库只会让下一轮揣着
            // 它继续在同样的预算上撞线，直接丢弃（收尾处已明确告知用户）。
            if !truncated_without_output {
                session.push_assistant(assistant_blocks);
            }

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
                    // PreTool 快照排除最后一条（本次工具调用的 assistant 消息）。
                    //
                    // PostTool 拿的是**完整**历史（多一条 assistant 消息与本轮
                    // tool_result），无法与本快照共享前缀——`push_user_blocks_merged`
                    // 在多个 tool_result 时会把后一个**合并**进前一个（长度不变、
                    // 内容变），拼增量会丢内容。因此这里仍各付一次全量深拷贝。
                    //
                    // 真正省掉的是 store 内部那次多余的 `to_vec()`：此前
                    // `CheckpointStore::create` 收 `&[Message]` 再自己拷一份，
                    // 调用方的 clone 纯属白费。现在 create 收 owned 副本，
                    // 调用方那次 clone 就是唯一一次。
                    let pre_tool_messages =
                        session.messages[..session.messages.len().saturating_sub(1)].to_vec();
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
                    // PostTool 需要**完整**历史（含 assistant 消息与本轮
                    // tool_result），不能拿 PreTool 的前缀拼增量：多个
                    // tool_result 时 `push_user_blocks_merged` 会把后一个
                    // **合并**进前一个创建的 User 消息（长度不变、内容变），
                    // 那种情况下"尾部增量"是空的，拼出来的快照会丢内容。
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
                } else if !visible_output {
                    // 「只有 thinking」同样是空响应。旧判据只看整个块列表是否
                    // 为空，thinking 非空就算「模型说了话」，于是输出预算被
                    // reasoning 吃干净时回合静默收尾——这正是「长会话跑几轮
                    // 就停住、没有任何提示」的成因。自动重试也救不回来时，
                    // 必须把原因和解法明确写出来。
                    tracing::warn!(
                        model = %used_route.model,
                        max_tokens = used_route.max_tokens,
                        stop_reason = ?stop_reason,
                        "回复只有 thinking / redacted_thinking，没有可见内容，回合按成功收尾"
                    );
                    on_text(&wyj_i18n::tr("agent.output_without_visible_content"));
                } else if stop_reason == StopReason::MaxTokens {
                    // 有正文但同样撞了输出上限：正文是被硬切断的。旧实现
                    // 把它当正常结束——用户看到半句话，没有任何解释。
                    tracing::warn!(
                        model = %used_route.model,
                        max_tokens = used_route.max_tokens,
                        "回复达到 max_tokens 上限，正文可能被截断，回合按成功收尾"
                    );
                    on_text(&wyj_i18n::tr("agent.output_truncated"));
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
                                // 记忆提取用辅助模型（若有）：短提示词 + 独立 system
                                // prompt，不复用主对话前缀，换模型不打穿主会话缓存。
                                let provider = self.aux_provider_or_route();
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
                            let provider = self.aux_provider_or_route();
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
                        // 每进程只尝试一次：再往后 `generate_title` 只会
                        // `store.load()` 重读整个 session JSON 后被
                        // `title_generated` 挡掉——纯浪费的磁盘 IO。
                        if self
                            .title_attempted
                            .swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
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
                        // hook 只能替代「询问」，不能替代「禁止」——理由同下。
                        let reason = ctx
                            .denial_reason(&name, &input)
                            .unwrap_or_else(|| "被强制权限策略拒绝".to_string());
                        let msg = format!(
                            "工具 `{name}` 被强制权限策略拒绝；hook approve 无权绕过：{reason}"
                        );
                        (msg.clone(), ToolResultContent::Text(msg), true)
                    } else {
                        run_tool(&t, input, ctx, &meta).await
                    }
                }
                HookOutcome::Passthrough | HookOutcome::Continue { .. } => {
                    if !ctx.is_allowed(&name, &input) {
                        // 带具体原因：hard_deny 与 Plan 白名单是两种完全不同的
                        // 失败，「换个模式就好」和「这条命令永远不行」必须能区分，
                        // 否则模型会对着一条硬禁止规则反复换写法重试。
                        let reason = ctx
                            .denial_reason(&name, &input)
                            .unwrap_or_else(|| format!("工具 `{name}` 在当前模式下不被允许"));
                        let msg = format!("工具 `{name}` 被强制权限策略拒绝：{reason}");
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
    /// 走前缀复用路径：system 与 tools 必须与主对话**逐字节相同**才能命中
    /// provider 缓存，而缓存命中率就是这条路径的全部理由（DeepSeek 命中价是
    /// miss 的 1/50）。三条曾经各写一半、现在共用同一处的地方：
    ///
    /// 1. `tools` 走 [`Self::tools_for_request`]——含 route 能力裁剪与 lazy
    ///    折叠，与 `run_turn` 完全一致；
    /// 2. `volatile` 优先取**最近一次真实请求发出去的字节**
    ///    （[`Self::last_request_volatile`]）。回合之外重建不出子目录规约
    ///    reminder——`ClaudeMdLoader::seen_dirs` 是全局去重的，第二个回合起
    ///    就再也拿不到它，而它在真实请求里是 volatile 的第一段。没有历史请求
    ///    时（新会话第一条就 `/compact`）才现拼尾部三段；
    /// 3. 窗口走 [`Session::effective_context_window`]，与自动压缩同一份判定
    ///    依据。旧实现直接用配置值，于是「手动压过之后自动压缩的阈值又变了」。
    pub async fn compact_context(
        &self,
        session: &mut Session,
    ) -> Result<crate::compact::CompactResult> {
        use crate::compact::{compact_session, CompactPrefix};
        let route = self.route_at(self.active_route_index());

        // `Arc` 共享：system prompt 含完整 CLAUDE.md 链（本仓库 30KB+），
        // 每轮克隆一份是纯浪费。装配完成后它不再被改写，共享安全。
        // 同 run_turn：无追加时零拷贝借用（见那里的说明）。
        let memory_snapshot = self.memory_snapshot_text(session);
        // 读盘走 `spawn_blocking`（见 `turn_reminder_async` 的说明）：规约文件
        // 链动辄几十 KB，每轮同步读盘会占住 async 运行时的 worker 线程。
        let reminder = match self.claude_md.as_ref() {
            Some(loader) => loader.turn_reminder_async().await,
            None => None,
        };
        let system_stable = if memory_snapshot.is_empty() && reminder.is_none() {
            std::borrow::Cow::Borrowed(self.system_prompt.as_ref())
        } else {
            let mut buffer =
                String::with_capacity(self.system_prompt.len() + memory_snapshot.len() + 64);
            buffer.push_str(&self.system_prompt);
            if !memory_snapshot.is_empty() {
                buffer.push_str("\n\n");
                buffer.push_str(&memory_snapshot);
            }
            if let Some(reminder) = reminder.as_deref() {
                buffer.push_str("\n\n");
                buffer.push_str(reminder);
            }
            std::borrow::Cow::Owned(buffer)
        };
        let (request_tools, _) = self.tools_for_request(&route);
        let snapshot = self
            .last_request_volatile
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let volatile = if snapshot.is_empty() {
            // 从未发过请求：按当前 route 现拼尾部三段。此时不存在子目录
            // reminder（本轮还没触达过任何目录），因此与真实请求一致。
            let mut fresh = String::new();
            self.push_request_volatile_tail(&route, &request_tools, &mut fresh);
            fresh
        } else {
            snapshot
        };

        compact_session(
            session,
            route.provider.as_ref(),
            session.effective_context_window(route.context_window),
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
                        volatile: &volatile,
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
