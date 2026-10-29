//! 会话状态：消息历史 + 累计用量

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use wyj_api::types::{ContentBlock, Message, Role, ToolResultContent};

/// 子 Agent 的用量快照（不含缓存写入时为 0）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubUsage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

/// 子 Agent 用量的**共享累加器**。
///
/// 子 Agent 跑在独立的 `Session` 上（`sub_agent.rs` 的 `Session::new()`），
/// 它自己的 token 既不会进主会话的 `total_*`，也不会进
/// `AgentEvent::Usage`——`UsageDelta` 只在 TUI 主 Agent 路径发射。因此在
/// `/cost` 里看到的主会话用量**系统性漏掉整棵子 Agent 扇出子树**。
///
/// 机制：主会话在自己的 `ToolContext` 上挂一个本累加器，子 Agent 结束时把
/// 自己那一轮 `Session` 的计数加进来（`add`），主 Agent 在每批工具执行后
/// 调 [`SubUsageTotals::take`] 抽干计入 `Session` 的 `sub_*` 字段
/// （`Session::drain_sub_usage`）。
///
/// 用原子量而非 `Mutex` 是因为子 Agent 可能 `parallel_safe` 并发跑，多个
/// 子 Agent 会在同一批里同时累加；`take` 用 `swap(0)` 保证每个 token 只被
/// 主会话计入一次，天然幂等。
#[derive(Debug, Default)]
pub struct SubUsageTotals {
    input: AtomicU32,
    output: AtomicU32,
    cache_read: AtomicU32,
    cache_write: AtomicU32,
}

impl SubUsageTotals {
    pub fn add(&self, usage: SubUsage) {
        self.input.fetch_add(usage.input, Ordering::Relaxed);
        self.output.fetch_add(usage.output, Ordering::Relaxed);
        self.cache_read
            .fetch_add(usage.cache_read, Ordering::Relaxed);
        self.cache_write
            .fetch_add(usage.cache_write, Ordering::Relaxed);
    }

    /// 抽走并清空累计值。多次并发调用不会重复计账。
    pub fn take(&self) -> SubUsage {
        SubUsage {
            input: self.input.swap(0, Ordering::Relaxed),
            output: self.output.swap(0, Ordering::Relaxed),
            cache_read: self.cache_read.swap(0, Ordering::Relaxed),
            cache_write: self.cache_write.swap(0, Ordering::Relaxed),
        }
    }

    pub fn snapshot(&self) -> SubUsage {
        SubUsage {
            input: self.input.load(Ordering::Relaxed),
            output: self.output.load(Ordering::Relaxed),
            cache_read: self.cache_read.load(Ordering::Relaxed),
            cache_write: self.cache_write.load(Ordering::Relaxed),
        }
    }
}

/// 共享句柄别名，供 `ToolContext::sub_usage_sink` 使用。
pub type SharedSubUsage = Arc<SubUsageTotals>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RoutingEvent {
    pub timestamp: String,
    pub from_profile: String,
    pub to_profile: String,
    pub error_kind: wyj_api::ProviderErrorKind,
    /// v0.2.8 只允许在尚未提交 assistant 消息或工具执行结果的完整边界切换。
    pub boundary: String,
}

#[derive(Clone, Default)]
pub struct Session {
    pub messages: Vec<Message>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// 累计命中 prompt 缓存的输入 token 数（按约 0.1x 价格计费，不计入
    /// `total_input_tokens`，单独统计以便 /cost 展示缓存命中情况）
    pub total_cache_read_tokens: u32,
    /// 累计写入 prompt 缓存的输入 token 数（按约 1.25x 价格计费，同样
    /// 不计入 `total_input_tokens`；不统计会系统性低估成本）
    pub total_cache_write_tokens: u32,
    /// 本会话累计**成功**的模型往返数。
    ///
    /// 「成功」是硬要求：token 只在流完整结束时入账，而失败重试
    /// （400 降级、参数降级、路由切换）**不产生 token**。若把尝试次数
    /// 当分母，`input_per_api_call` 会被系统性稀释——重试越多，这个数越小，
    /// 而它的用途恰恰是回答「每次调用带多少上下文」。
    ///
    /// 失败的尝试记在 [`Self::api_attempts`] 里，两者分开后
    /// 「重试开销」也成了可观测的量。
    pub api_calls: u32,
    /// 本会话累计**发起过**的模型请求次数（含失败重试）。
    ///
    /// 与 [`Self::api_calls`] 的差值 = 失败/重试的次数。它不是成本口径
    /// （失败不花钱），而是**稳定性口径**——差值大说明端点在拒绝或抖动。
    pub api_attempts: u32,
    /// **累计**随模型请求发送的工具 schema 估算 token（全会话求和，每次
    /// `provider.stream` 前累加一次）。它回答的是"本次会话为工具定义总共
    /// 付了多少"，不是"当前上下文里占了多少"。
    ///
    /// ⚠️ 不要拿它当单请求的稳态值展示：一次 200 次调用的会话会累到百万级
    /// （实测最高 1,457,192），而真实的单请求值只有 6,281。单请求口径看
    /// [`Self::context_audit`] 的 `tool_schema_tokens`——`/context` 面板用的
    /// 就是它。
    pub tool_schema_tokens: u32,
    /// **最近一次请求**实际发送的工具 schema 估算 token（稳态值）。
    ///
    /// 与 [`crate::compact::ContextAudit::tool_schema_tokens`] 同源同值，
    /// 单独存一份是为了让 JSON / 插件事件这类拿不到 `context_audit` 的消费
    /// 方也能拿到稳态口径，不必自己重算。
    pub tool_schema_tokens_last: u32,
    /// lazy schema 相对全量工具集合累计避免发送的估算 token。
    pub tool_schema_tokens_saved: u32,
    /// 子 Agent 扇出累计消耗的输入 token。与 `total_input_tokens` **不重叠**
    /// ——子 Agent 跑在自己的 `Session` 上，其用量由
    /// [`Self::drain_sub_usage`] 从 `ToolContext` 的共享累加器搬进来。
    pub sub_input_tokens: u32,
    /// 子 Agent 累计输出 token。
    pub sub_output_tokens: u32,
    /// 子 Agent 累计缓存命中 token。
    pub sub_cache_read_tokens: u32,
    /// 子 Agent 累计缓存写入 token（当前配置的第三方端点不上报，恒为 0，
    /// 与主会话的 `total_cache_write_tokens` 同因，详见 `/cost` 的说明行）。
    pub sub_cache_write_tokens: u32,
    /// **辅助 LLM 往返**累计消耗：标题生成、Auto 权限分类器、记忆提取、
    /// 演化分析等「不是主推理循环」的调用。
    ///
    /// 为什么要单列而不是并进 `total_input_tokens`：这些调用用的是**另一个
    /// 模型/端点**（`aux_model`、`classifier_profile`），单价与主模型不同，
    /// 混进主口径会让 `/cost` 的金额算错。但也绝不能像旧实现那样直接丢弃——
    /// Auto 模式下分类器每拦一个工具就是一次往返，一次会话几十次很常见，
    /// 丢掉等于系统性低估真实花费。
    ///
    /// 与 `api_calls` 的关系：这些调用**不计入** `api_calls`。`api_calls`
    /// 的语义是「主推理循环跑了几趟」，它是 `input/api_call` 这个派生指标
    /// 的分母；把辅助往返混进去会让「每次调用带多少上下文」被系统性低估。
    pub aux_input_tokens: u32,
    /// 辅助 LLM 往返累计输出 token。
    pub aux_output_tokens: u32,
    /// 辅助 LLM 往返累计缓存命中 token。
    pub aux_cache_read_tokens: u32,
    /// 辅助 LLM 往返累计次数。
    pub aux_calls: u32,
    /// 最近一次模型请求的上下文占用分解。与压缩决策**同源**，供 TUI 的
    /// `/context` 面板使用——旧实现里面板/状态栏自己调 `estimate_tokens`
    /// （只算 messages），与 `estimate_request_tokens`（含 system + tools +
    /// 输出预留）差出一万多 token，百分比和压缩时机对不上号。
    pub context_audit: crate::compact::ContextAudit,
    /// 本会话自动压缩发生次数（含因 provider 报 context 超限触发的强制压缩）。
    /// 压缩对用户不可见，除以数字之外必须让"自动管理发生过"本身可见。
    pub compact_count: u32,
    /// 当前摘要处于第几代（0 = 还没有摘要）。
    ///
    /// 与 [`Self::compact_count`] **不等价**：`reset_entire_session` 的压缩
    /// 会把历史整体换成一份全新摘要，代际归 1 而不是继续累加。真正决定信息
    /// 保真度的是代际——它是「摘要叠了多少层」，而 `compact_count` 只是
    /// 「发生过几次压缩」。
    ///
    /// `/context` 与 `/cost` 都展示它：超过
    /// [`crate::compact::MAX_COMPACT_GENERATIONS`] 时模型看到的是「一张地图
    /// 而非一份记录」，用户应当知道这件事已经发生。
    pub compact_generation: u32,
    /// 自动压缩的**跨回合**停滞冷却：压缩跑完仍超阈值时置位，逐回合递减，
    /// 期间跳过自动压缩。
    ///
    /// 为什么必须存在：单次请求内的 `MAX_COMPACT_PASSES` + 5% 进展判据只
    /// 挡住「一轮里连压三次」，挡不住「每个用户回合都压一次、每次都压不
    /// 下去」——那才是用户看到的「不停压缩」死循环，代价是每回合一次全量
    /// 摘要往返，而历史早已只剩摘要，压无可压。
    ///
    /// 冷却而不是永久关闭：窗口/输出预算可能被用户改掉，端点也可能在
    /// 400 后给出更准的实测上界；3 轮后重试一次的成本远低于永久误关。
    pub compact_cooldown_turns: u32,
    /// 本会话累计的**思考** token（输出的子集，用户不可见）。
    ///
    /// Anthropic 线不上报这个值（它的 output 已含 thinking），所以那里恒为 0
    /// ——面板据此**不显示**思考占比，而不是显示「思考占 0%」这种误导性数字。
    pub total_reasoning_tokens: u32,
    /// 最近一次压缩释放的估算 token。
    pub last_compact_saved_tokens: u32,
    /// 本会话被 context editing 外部化到 CAS 的 blob hash。会话被丢弃时
    /// （`/clear` / 新会话 / 退出 / resume 别的会话）必须逐个 `release`——
    /// `WorkspaceCas::gc` 只回收 `ref_count == 0` 的 blob，不记就永不回收。
    pub elided_blobs: Vec<String>,
    /// 本会话 context editing 累计释放的估算 token。
    pub context_edit_freed_tokens: u32,
    /// 逐请求的缓存观测（最近若干条）。用于归因「缓存命中率为什么掉了」。
    ///
    /// 只保留最近 [`CACHE_SAMPLE_HISTORY`] 条：归因只需**相邻两次**的
    /// 对比，而留着几百条只是让会话文件变大。
    pub cache_samples: Vec<crate::cache_attribution::CacheSample>,
    /// 逐请求的 prompt 指纹（最近若干条），与 `cache_samples` 配套。
    ///
    /// 指纹本身只采一次（进程内第一次发请求时），但缓存掉量可能发生在
    /// 后面任何一趟——所以这里存的是**每趟都记**的快照，而不是只存最新。
    pub cache_fingerprints: Vec<Option<crate::prompt_fingerprint::PromptFingerprint>>,
    /// 同角色模型 fallback 记录。只保存 profile 名与错误分类，不保存请求正文、
    /// endpoint query 或认证信息。
    pub routing_events: Vec<RoutingEvent>,
    /// 本会话实际生效的 prompt cache 模式，取值见
    /// [`crate::session_store::prompt_cache_state`]。2 = 曾开启但端点 400 已降级，
    /// 让 `/cost` 能解释「缓存为什么是 0」。
    pub prompt_cache_state: u8,
    pub current_checkpoint_id: Option<String>,
    pub branch_parent_session_id: Option<String>,
    pub branch_parent_checkpoint_id: Option<String>,
    /// 本会话临时使用的分组名（`/profile` 面板里「仅本会话使用」）。
    ///
    /// 与 `Config.active_profile` 是**两个不同的东西**：后者是写进
    /// `~/.wyj-code/config.toml` 的全局默认值，改了会影响下次启动和其它项目；
    /// 这个字段只活在当前会话里，随 `SessionFile` 落盘，`--resume` 后继续生效，
    /// 会话结束就没了。`None` = 跟随全局 `active_profile`。
    ///
    /// 存放的是**分组名**而不是整份 `Profile` 快照：分组定义（端点/Key/窗口）
    /// 仍以 `config.toml` 为唯一事实来源，避免会话文件里留下一份会漂移的副本。
    /// 分组被改名或删除时，恢复侧按「找不到就回落全局」处理。
    pub session_profile: Option<String>,
    /// 本会话的思考强度档位（`/effort` 设置）。
    ///
    /// **随会话落盘**，`--resume` 后继续生效——思考档位是「这个任务要想
    /// 多深」的工作状态，换个任务就该换档，不该每次都重设一遍。
    ///
    /// 但它**不写 `config.toml`**：档位属于「这次对话怎么跑」，不属于
    /// 「这个分组是什么端点」。
    ///
    /// 取值是档位名（`low` / `medium` / `high` / `xhigh` / `max` /
    /// `adaptive`）或特殊的 `"off"`（彻底关思考）。`None` = 跟随分组里
    /// 配的 `reasoning_effort`。
    pub session_effort: Option<String>,
    /// 最近一次请求实际发出的 system prompt 指纹（见
    /// [`crate::prompt_fingerprint`]）。
    ///
    /// **由 `Agent` 在每次发出请求时写入，随 `SessionFile` 落盘**。它的全部
    /// 价值在跨进程：同一会话隔天 `--resume`，模型行为变了而没有任何地方能
    /// 解释——记忆快照按 10 轮分桶、规约链每轮重读盘、Skill 名单与工具集随
    /// 配置漂移、版本升级整体换掉主提示。`context_audit` 只给 token 总量，
    /// 答不出「哪一段变了」。
    pub prompt_fingerprint: Option<crate::prompt_fingerprint::PromptFingerprint>,
    /// 本次进程内是否已用落盘指纹做过一次漂移检测。
    ///
    /// 用独立布尔而不是「`prompt_drift_since_resume.is_none()`」判断：后者把
    /// 「还没检测」和「检测过、没差异」压成同一个状态，于是每轮都重算一次
    /// （虽然结果恒等，但每轮都在跟上一轮自己比）。
    pub prompt_fingerprint_checked: bool,
    /// resume 后第一次请求时测得的 prompt 漂移。
    ///
    /// `None` = 还没跑过请求，或落盘没有指纹可比（升级前的老会话）。
    /// `Some(空)` = 比过了，两个可比 section 完全一致。
    pub prompt_drift_since_resume: Option<crate::prompt_fingerprint::PromptDrift>,
    /// 最近一次模型往返的时延画像（`/context` 面板展示）。
    pub latency: TurnLatency,
    /// 字符启发式估算相对真实 prompt token 的校准系数（EWMA）。
    ///
    /// `None` = 还没有可用样本，按 1.0 处理。用 `Option` 而不是「默认 1.0 的
    /// f32」是因为 `Session` 用 `#[derive(Default)]`：`f32::default()` 是 0.0，
    /// 悄悄把系数变成 0 会让所有压缩阈值判定失效，而这类默认值错误在测试里
    /// 极难发现。`None` 让「未校准」成为一个显式状态。
    ///
    /// `estimate_text_tokens` 是启发式：CJK 按 1.5/字、其余按 1/3 字符，对
    /// 代码 / JSON / 多语言混排的实际偏差可达数十个百分点，而压缩阈值是按
    /// **比例**切的硬线。偏低时压缩触发过晚（真实上下文已接近窗口上限才动手，
    /// 也可能先撞 400）；偏高时压缩触发过早（还没到该压的时候就把现场摘要了）。
    ///
    /// 每轮请求成功后用 provider 上报的真实 prompt token 与估算值求比值，
    /// 再做指数滑动平均（旧值权重 0.9）：单轮抖动不该改变阈值判定，而偏差
    /// 一旦是系统性的，几轮之内就会收敛到真值附近。
    ///
    /// 只在 `[0.5, 2.0]` 区间内采信样本——落在区间外的比值几乎一定意味着
    /// 端点上报口径不同（某些兼容端点把 cache_read 算进 prompt_tokens、
    /// 或者干脆不上报），拿它做校准会让阈值漂到不可预期的位置。
    pub input_estimate_ratio: Option<f32>,
    /// provider 亲口报告过上下文超限时记下的真实输入量。
    ///
    /// 配置里的 `context_window` 是**裸 u32、无探测兜底**，填错（比如端点
    /// 实际只有 256K 却写了 1M）会让压缩阈值落在根本摸不到的位置，两道防线
    /// 同时失效，只剩 400 兜底。这个字段让「被端点拒绝过一次」这件事变成
    /// 后续判定有效窗口的依据：`min(configured, hint * 1.05)`。
    pub overflow_window_hint: Option<u32>,
    /// 消息增量落盘回调（可选）。每条消息进入 `messages` 时同步调用一次。
    ///
    /// **为什么需要在 `Session` 上开口子**：子 Agent 跑在一个纯内存的
    /// `Session` 上，正常结束/中断两条路径都能在收尾时写交接物，但**进程被
    /// kill**（`SIGKILL`、终端被关、笔记本断电）时收尾代码一行都不执行。那时
    /// 唯一还能救回「做到哪了」的依据，就是这条消息在产生时就已落盘。
    ///
    /// 回调必须是**同步非阻塞**的：实现侧应当只做 `try_send` 到后台写手，
    /// 与 `trace::TraceWriter` 同一套做法。`Arc` + `dyn` 而非泛型，是因为
    /// `Session` 到处都是按值克隆的，泛型会污染几乎每个签名。
    pub message_sink: Option<MessageSink>,
    /// 本轮是否因**推理轮数预算用尽**而被迫收尾（`wrap_up_sent` 已置位）。
    ///
    /// 存在的理由：回合正常结束与「轮数被关闸」在 Agent 眼里都是
    /// `Ok(())`，上层只看到「模型不再调工具了」，无法区分
    /// 「活干完了」和「预算没了，工具被框架关掉」。这个区别对 UI 至关重要：
    /// 前者应当把遗留的 `InProgress` 补成 `Completed`，后者必须原样保留
    /// ——否则任务明明做到一半，面板却打了勾（实测现场）。
    ///
    /// 每回合入口重置，由 `run_turn_with_injection_inner` 在注入收尾提醒的
    /// 同时置位。**不落盘**：它是单回合的瞬时事实，`--resume` 时没有意义。
    pub turn_truncated: bool,
}

/// 增量消息事件：落盘侧据此决定「追加一行」还是「改写最后一行」。
///
/// 为什么不能只发 `&Message`：`push_user_blocks_merged`（工具结果、运行时
/// 补充输入）与 `prepend_to_last_user`（每轮规约注入）都**原地修改最后一条
/// user 消息**。只追加不明写改，日志里的最后一条会永久停在残缺状态。
#[derive(Clone)]
pub enum MessageEvent {
    /// 新消息追加到末尾
    Appended(Message),
    /// 最后一条消息被原地修改（合并 / 前插）
    UpdatedLast(Message),
    /// 整段历史被替换（压缩）。`messages` 是替换后的完整列表。
    Replaced(Vec<Message>),
}

/// 消息增量回调类型
pub type MessageSink = std::sync::Arc<dyn Fn(MessageEvent) + Send + Sync>;

impl std::fmt::Debug for Session {
    /// `message_sink` 是 `dyn Fn`，无法 derive Debug；手写一份只标「有没有挂
    /// sink」，让日志里能看出增量落盘是否生效，又不去碰闭包本身。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("messages", &self.messages.len())
            .field("total_input_tokens", &self.total_input_tokens)
            .field("total_output_tokens", &self.total_output_tokens)
            .field("api_calls", &self.api_calls)
            .field("message_sink", &self.message_sink.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for MessageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Appended(m) => write!(f, "Appended({:?})", m.role),
            Self::UpdatedLast(m) => write!(f, "UpdatedLast({:?})", m.role),
            Self::Replaced(v) => write!(f, "Replaced({} messages)", v.len()),
        }
    }
}

impl Session {
    /// 挂上消息增量落盘回调（builder 风格）。
    pub fn with_message_sink(mut self, sink: MessageSink) -> Self {
        self.message_sink = Some(sink);
        self
    }

    fn emit(&self, event: MessageEvent) {
        if let Some(sink) = &self.message_sink {
            sink(event);
        }
    }

    /// 外部直接改写 `messages` 后（压缩、上下文编辑）调用，通知 sink 打一次
    /// 全量快照。增量日志只有靠这个锚点才能在压缩后正确重放。
    pub fn emit_replaced(&self) {
        self.emit(MessageEvent::Replaced(self.messages.clone()));
    }
}

/// 一次模型往返的时延分解。
///
/// **为什么必须拆成三段而不是只记一个总耗时**：总耗时高是个没有行动指向的结论，
/// 而「慢在哪」直接对应三种完全不同的处置——
///
/// - `ttft_ms` 高 → 首 token 前的固定开销（DNS/连接/TLS、上传大 prefill、缓存
///   未命中的全量 prefill、服务端排队）。这是 prompt 大小与缓存命中率的问题，
///   不是模型慢。
/// - `ttft_ms` 低但 `total_ms` 大 → 服务端在稳定吐 token，说明是模型真的在长考
///   或者输出量大。该降的是 `max_tokens` 或换模型，不是上下文工程。
/// - `retry_ms > 0` → 其中相当一部分「慢」根本不是推理，是重试打出来的。
///   把它单列出来是为了不把网络抖动误判成模型性能问题。
///
/// `ttft_ms` 从发起 `provider.stream()` 算起（含连接建立与请求上传），到收到
/// 第一个 `TextDelta` / `ThinkingDelta` 为止——这是用户真正感知的「点了回车到
/// 屏幕开始动」的时间，用 stream 建立后的时刻起算会漏掉最该被看见的那一段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnLatency {
    /// 首 token 延迟（毫秒）。`None` = 本次往返没有产生任何输出增量。
    pub ttft_ms: Option<u64>,
    /// 从发起请求到收到 `MessageStop` 的总耗时（毫秒），含重试。
    pub total_ms: u64,
    /// 因可重试错误重发请求额外花掉的时间（毫秒）。0 = 一次就成了。
    pub retry_ms: u64,
    /// 本次往返是否发生过至少一次重试（`retry_ms` 的可见化，
    /// 避免「总共只慢了 300ms」的噪声被当成信号）。
    pub retried: bool,
}

impl TurnLatency {
    /// 扣掉重试之后的净推理耗时。重试是网络问题，不该记在模型账上。
    pub fn net_total_ms(&self) -> u64 {
        self.total_ms.saturating_sub(self.retry_ms)
    }
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前生效的估算校准系数（未校准时为 1.0）。
    pub fn estimate_ratio(&self) -> f32 {
        self.input_estimate_ratio.unwrap_or(1.0)
    }

    /// 用一次真实请求用量更新校准系数。
    ///
    /// `real_prompt_tokens` 必须是**完整 prompt**（全价 input + 缓存命中 +
    /// 缓存写入）。OpenAI 侧 `input_tokens` 已被 `saturating_sub(cached)`
    /// 处理过，只拿它当真实值会系统性低估，进而把系数越压越低。
    ///
    /// 三道护栏：
    ///   1. 估算值太小（< 1000 token）不采样——分母小的时候整数启发式的
    ///      相对误差本来就大，采进来只会让系数抖；
    ///   2. 比值落在 `[0.5, 2.0]` 之外直接丢弃——那不是估算偏差，是端点
    ///      上报口径不同（或干脆编了个数）；
    ///   3. EWMA 权重 0.1：系统性偏差几轮内收敛，单轮抖动不会改写阈值。
    pub fn observe_prompt_tokens(&mut self, real_prompt_tokens: u32) {
        // 分母必须是**不含输出预留**的 prompt：端点上报的 `prompt_tokens`
        // 也从来不含本次要生成的输出。两边都扣掉才是同一个量，否则系数会被
        // 系统性放大（`max_tokens` 越大偏得越多），阈值跟着被推晚。
        let prompt_estimate = self.context_audit.prompt_tokens();
        if prompt_estimate < 1_000 || real_prompt_tokens == 0 {
            return;
        }
        let ratio = real_prompt_tokens as f32 / prompt_estimate as f32;
        if !(0.5..=2.0).contains(&ratio) {
            tracing::debug!(
                real_prompt_tokens,
                prompt_estimate,
                ratio,
                "prompt token 实测/估算比值落在合理区间外，丢弃该校准样本"
            );
            return;
        }
        let next = match self.input_estimate_ratio {
            Some(previous) => previous * 0.9 + ratio * 0.1,
            None => ratio,
        };
        self.input_estimate_ratio = Some(next.clamp(0.5, 2.0));
    }

    /// 本会话实际生效的上下文窗口。
    ///
    /// **配置值就是配置值**：`configured` 原样生效，唯一的例外是
    /// `overflow_window_hint`——端点亲口以「上下文超限」拒绝时记下的实测上界。
    ///
    /// 两层取值，取最小：
    ///   1. `configured`——用户填的配置值；
    ///   2. `overflow_window_hint`——端点亲口拒绝时记下的实测上界。
    ///
    /// ## 为什么不再按静态容量表收紧（v0.3.24 移除）
    ///
    /// 旧实现还有第三层：按 `known_model_capacity()` 对**模型名子串**猜出的
    /// 窗口收紧配置值。这条路径有三个无法弥合的缺陷：
    ///
    /// 1. **它改写的是用户显式配置**。用户填 `context_window = 1_000_000`
    ///    表达的是「我这个端点支持 1M」——那是他掌握的信息，比仓库里一张
    ///    静态表新。静默收紧到表里的值属于「运行期偷偷改配置」，
    ///    与 `--config-status` 那套「只提示不改值」的既有做法自相矛盾。
    /// 2. **判据是子串匹配，两个协议下结果还会不一样**。同一张表要同时服务
    ///    Anthropic 与 OpenAI 两条线，而模型名写法各异
    ///    （`MiniMax-M3.1-Flash` vs `minimax/minimax-m3.1-flash:free`），
    ///    是否命中随名字变。于是「anthropic 有压缩提示、openai 没有」这种
    ///    表面上的协议差异，实际来自字符串匹配的偶然结果——这正是
    ///    用户实测反馈的现象。
    /// 3. **容量表给的是保守下限**（见 `known_model_capacity` 的文档），
    ///    用它去收紧一个更大的配置值，方向本来就是反的。
    ///
    /// 配置值填得超过端点真实窗口时，自愈路径仍在：端点报
    /// `ContextLengthExceeded` → [`Self::note_context_overflow`] 记下实测上界
    /// → 后续窗口按实测收紧。这是**端点亲口给出的证据**，不是猜的。
    ///
    /// `catalog_hint` 已从签名移除：静态表仍用于 `/context` 面板与
    /// `--config-status` 的**提示**（「你填的窗口比目录值大，确认过吗」），
    /// 但它不再参与任何判定，留一个被忽略的参数只会让人以为它还有作用。
    pub fn effective_context_window(&self, configured: u32) -> u32 {
        match self.overflow_window_hint {
            Some(hint) if hint > 0 => configured.min(hint),
            _ => configured,
        }
    }

    /// 记下「这次请求被端点以上下文超限为由拒绝」。
    ///
    /// 只往小取：连续撞两次时，较严的那次才是真实窗口的上界，而
    /// [`Self::overflow_window_hint`] 是公开字段——在调用点写 `min` 等于把
    /// 「只许收紧」这条不变量交给每个调用方各自维护。
    pub fn note_context_overflow(&mut self, real_prompt_tokens: u32) {
        if real_prompt_tokens == 0 {
            return;
        }
        self.overflow_window_hint = Some(match self.overflow_window_hint {
            Some(previous) => previous.min(real_prompt_tokens),
            None => real_prompt_tokens,
        });
    }

    /// 清空对话与本次用量，但保留当前 session 的 checkpoint/branch 血缘。
    /// `/clear` 不是创建新 session，不能把分支归属悄悄抹掉。
    ///
    /// ⚠️ 这会**连带清空 `elided_blobs`**。调用方必须先
    /// [`Session::take_elided_blobs`] 取出来并 `context_edit::release_session_blobs`
    /// 释放，否则 CAS 里那些 blob 的 `ref_count` 永远停在 1，`gc` 永远删不掉。
    pub fn clear_conversation(&mut self) {
        let current_checkpoint_id = self.current_checkpoint_id.clone();
        let branch_parent_session_id = self.branch_parent_session_id.clone();
        let branch_parent_checkpoint_id = self.branch_parent_checkpoint_id.clone();
        // prompt_cache_state 描述的是「当前 profile/端点是否真的在用缓存」，
        // 属于运行环境而非本次对话内容——`/clear` 换对话不换端点，抹掉它会让
        // 紧接着的 `/cost` 把"已降级"误报成"从未开启"。
        let prompt_cache_state = self.prompt_cache_state;
        // 同理：会话级分组是「这个会话跑在哪套端点上」，不是对话内容。
        // `/clear` 只清消息，不该顺手把用户刚选的会话级模型丢掉。
        let session_profile = self.session_profile.clone();
        // 思考档位同理：它是「这次对话要想多深」，换话题不代表要换档。
        let session_effort = self.session_effort.clone();
        // 估算校准系数与「端点拒绝过」的实测窗口同样描述运行环境：这轮踩过的
        // 坑（估算偏低 / 配置窗口填大了）不会因为换了个话题就消失，清掉等于
        // 让新对话从头再失准一遍。
        let input_estimate_ratio = self.input_estimate_ratio;
        let overflow_window_hint = self.overflow_window_hint;
        *self = Self {
            current_checkpoint_id,
            branch_parent_session_id,
            branch_parent_checkpoint_id,
            prompt_cache_state,
            session_profile,
            session_effort,
            input_estimate_ratio,
            overflow_window_hint,
            ..Self::default()
        };
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        let message = Message::user(text);
        self.messages.push(message.clone());
        self.emit(MessageEvent::Appended(message));
    }

    /// 取走并清空本会话外部化过的 CAS blob 引用。
    ///
    /// 必须在会话真正被丢弃的路径（`/clear`、新会话、退出、resume 别的会话）
    /// 调 [`crate::context_edit::release_session_blobs`] 之前调用。
    pub fn take_elided_blobs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.elided_blobs)
    }

    /// 推送含多个内容块的用户消息（支持图片/文件附件）
    pub fn push_user_with_blocks(&mut self, blocks: Vec<ContentBlock>) {
        let message = Message {
            role: Role::User,
            content: blocks,
        };
        self.messages.push(message.clone());
        self.emit(MessageEvent::Appended(message));
    }

    pub fn push_assistant(&mut self, blocks: Vec<ContentBlock>) {
        let message = Message {
            role: Role::Assistant,
            content: blocks,
        };
        self.messages.push(message.clone());
        self.emit(MessageEvent::Appended(message));
    }

    pub fn push_tool_result(
        &mut self,
        tool_use_id: String,
        content: wyj_api::types::ToolResultContent,
        is_error: bool,
    ) {
        let block = ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        };
        self.push_user_blocks_merged(vec![block]);
    }

    /// 追加内容块：若最后一条已是 user 消息则合并进去，否则新建一条 user 消息。
    /// 用于工具结果、以及 Agent 忙碌时用户补充输入的运行时注入，二者都必须
    /// 保持与已有 user 消息同一轮次，以满足 Provider 对角色严格交替的要求。
    pub fn push_user_blocks_merged(&mut self, blocks: Vec<ContentBlock>) {
        match self.messages.last_mut() {
            Some(m) if matches!(m.role, Role::User) => {
                m.content.extend(blocks);
                let updated = m.clone();
                self.emit(MessageEvent::UpdatedLast(updated));
            }
            _ => {
                let message = Message {
                    role: Role::User,
                    content: blocks,
                };
                self.messages.push(message.clone());
                self.emit(MessageEvent::Appended(message));
            }
        }
    }

    /// 把内容块前插到最后一条消息（必须是 user 消息）的内容开头。
    /// 用于每轮对话开始时把 AGENTS.md `<system-reminder>` 插在用户实际输入之前。
    pub fn prepend_to_last_user(&mut self, blocks: Vec<ContentBlock>) {
        if let Some(m) = self.messages.last_mut() {
            if matches!(m.role, Role::User) {
                let mut new_content = blocks;
                new_content.append(&mut m.content);
                m.content = new_content;
                let updated = m.clone();
                self.emit(MessageEvent::UpdatedLast(updated));
            }
        }
    }

    pub fn add_usage(&mut self, input: u32, output: u32) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
    }

    pub fn add_cache_usage(&mut self, cache_read: u32, cache_write: u32) {
        self.total_cache_read_tokens += cache_read;
        self.total_cache_write_tokens += cache_write;
    }

    /// 记一次输出里的**思考** token。
    ///
    /// 它是 `total_output_tokens` 的子集（不可见的推理部分）。单独记一份是
    /// 为了让 `/cost` 能回答「这次贵在思考还是贵在正文」——两者是不同的
    /// 优化方向（调低 effort vs 调短输出）。
    pub fn add_reasoning_tokens(&mut self, reasoning: u32) {
        self.total_reasoning_tokens = self.total_reasoning_tokens.saturating_add(reasoning);
    }

    /// 记一次**辅助 LLM 往返**（标题生成 / Auto 分类器 / 记忆提取 / 演化）。
    ///
    /// 与 [`Self::add_usage`] 分开是因为这些调用走另一个模型或端点，单价不同；
    /// 且它们不计入 `api_calls`——那个字段是「主推理循环跑了几趟」，是
    /// `input/api_call` 派生指标的分母，混进辅助往返会让它系统性偏低。
    ///
    /// 调用点必须**在检查返回值之前**记账：即使解析失败、摘要为空、模型回了
    /// 一句废话，这轮 token 也已经花掉了。这与 `compact.rs` 里压缩往返的
    /// 记账时机是同一条纪律。
    pub fn add_aux_usage(&mut self, input: u32, output: u32, cache_read: u32) {
        self.aux_input_tokens = self.aux_input_tokens.saturating_add(input);
        self.aux_output_tokens = self.aux_output_tokens.saturating_add(output);
        self.aux_cache_read_tokens = self.aux_cache_read_tokens.saturating_add(cache_read);
        self.aux_calls = self.aux_calls.saturating_add(1);
    }

    /// 补齐「停在工具调用中间」的悬空 `tool_use`，返回补了几条。
    ///
    /// 事故背景（`--resume` 后输入「继续」毫无反应，每条消息都复发）：
    /// 长任务被 ESC / 关进程打断时，assistant 消息已经带着 `tool_use` 进入
    /// 历史，工具结果却还没写回；退出时 `SessionStore::save` 把这份半截历史
    /// 原样落盘。resume 载入后，用户发的任何一条消息都会让请求的**末尾**变成
    /// 「assistant(tool_use) → user(没有 tool_result)」。Anthropic 兼容端点对
    /// 这种请求既不报错也不重试，而是回一个 HTTP 200、内容为空的 SSE 流
    /// （实测 MiniMax M3.1：`message_start content=[]` → `message_delta
    /// stop_reason=""` → `message_stop`）。客户端于是把空响应当成正常回复结束
    /// 回合——没有文本、没有报错、spinner 直接停——而那条空 assistant 消息
    /// 还会再次落盘，会话从此永久中毒。
    ///
    /// 只修**最后一条 assistant 消息**里的悬空调用，这是实测划出的边界：
    /// 同一个请求体里，悬空 `tool_use` 出现在历史中段（后面还有完整的
    /// user/assistant 往返）时端点照常返回正常内容。而 context editing 会把
    /// 旧 `tool_result` 外部化成占位文本——若按「全历史扫描未配对」来补，
    /// 会给这些已外部化的旧调用凭空造出结果，污染历史。
    ///
    /// 合成结果插在末尾那条 user 消息的**最前面**：协议要求 `tool_result` 位于
    /// 紧跟其后的 user 消息里，且与工具执行路径（`push_tool_result` →
    /// `push_user_blocks_merged`）的形态一致。末尾不是 user 消息时（压缩、
    /// 子 Agent 收尾等边界）另起一条 user 消息承载，保证角色严格交替。
    ///
    /// 幂等：没有悬空调用时返回 0 且不改历史，因此可以在每次请求前无条件调。
    pub fn repair_dangling_tool_uses(&mut self) -> usize {
        let Some(last_assistant) = self
            .messages
            .iter()
            .rposition(|message| matches!(message.role, Role::Assistant))
        else {
            return 0;
        };
        let answered: HashSet<&str> = self.messages[last_assistant + 1..]
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        let dangling: Vec<(String, String)> = self.messages[last_assistant]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, .. } if !answered.contains(id.as_str()) => {
                    Some((id.clone(), name.clone()))
                }
                _ => None,
            })
            .collect();
        if dangling.is_empty() {
            return 0;
        }
        let repaired = dangling.len();
        let results: Vec<ContentBlock> = dangling
            .into_iter()
            .map(|(id, name)| ContentBlock::ToolResult {
                tool_use_id: id,
                content: ToolResultContent::Text(
                    serde_json::json!({
                        "error": "tool_call_interrupted",
                        "tool": name,
                        "instruction": "This call was interrupted before it produced a \
                                        result. Nothing was executed. Retry it, or proceed \
                                        without it.",
                    })
                    .to_string(),
                ),
                is_error: true,
            })
            .collect();
        match self.messages.last_mut() {
            Some(last) if matches!(last.role, Role::User) => {
                let mut content = results;
                content.append(&mut last.content);
                last.content = content;
            }
            _ => self.push_user_with_blocks(results),
        }
        repaired
    }

    /// 把子 Agent 累加器里已记录的用量搬进本会话的 `sub_*` 字段。
    ///
    /// 由 `Agent::run_turn` 在**每批工具执行后**调用一次（子 Agent 可能
    /// `parallel_safe` 并发，抽干必须等这批 join 完）。`take` 内部是
    /// `swap(0)`，所以重复调用不会重复计账；没有累加器时是 no-op。
    pub fn drain_sub_usage(&mut self, sink: Option<&SubUsageTotals>) {
        let Some(sink) = sink else { return };
        let usage = sink.take();
        self.sub_input_tokens = self.sub_input_tokens.saturating_add(usage.input);
        self.sub_output_tokens = self.sub_output_tokens.saturating_add(usage.output);
        self.sub_cache_read_tokens = self.sub_cache_read_tokens.saturating_add(usage.cache_read);
        self.sub_cache_write_tokens = self
            .sub_cache_write_tokens
            .saturating_add(usage.cache_write);
    }

    /// 抽干辅助往返用量累加器，计入 `aux_*` 字段。
    ///
    /// 与 [`Self::drain_sub_usage`] 同构，但**调用时机不同**：子 Agent 只在
    /// 工具批边界完成，辅助往返（标题 / 分类器 / 记忆 / 演化）随时可能结束，
    /// 所以主循环在每个回合开头与收尾各调一次。
    pub fn drain_aux_usage(&mut self, sink: Option<&crate::aux_usage::AuxUsageTotals>) {
        let Some(sink) = sink else { return };
        let usage = sink.take();
        self.aux_input_tokens = self.aux_input_tokens.saturating_add(usage.input);
        self.aux_output_tokens = self.aux_output_tokens.saturating_add(usage.output);
        self.aux_cache_read_tokens = self.aux_cache_read_tokens.saturating_add(usage.cache_read);
        self.aux_calls = self.aux_calls.saturating_add(usage.calls);
    }

    /// 本会话的子 Agent 用量合计（`/cost` 的「子 Agent」行用）。
    pub fn sub_usage(&self) -> SubUsage {
        SubUsage {
            input: self.sub_input_tokens,
            output: self.sub_output_tokens,
            cache_read: self.sub_cache_read_tokens,
            cache_write: self.sub_cache_write_tokens,
        }
    }

    /// 从落盘文件恢复**全部计量与血缘字段**。
    ///
    /// 与 [`SessionFile::from_session`] 成对：这两个函数是 `Session` ↔
    /// `SessionFile` 之间计量字段的唯一起止点。resume / `/resume` 切换 / 分支
    /// 三条恢复路径以前各自手写 `sess.total_input_tokens = file.input_tokens`
    /// 这一串，新增字段必然漏改（这正是 cache/api_calls 此前只能活在内存里的
    /// 根因）。集中到这里后，漏改在编译期暴露。
    ///
    /// 注意**不**碰 `messages`——各恢复路径对消息体的处理不同（是否
    /// `materialize_elided`、是否替换为 checkpoint 内容），由调用方自己决定。
    pub fn restore_usage_from(&mut self, file: &crate::session_store::SessionFile) {
        self.total_input_tokens = file.input_tokens;
        self.total_output_tokens = file.output_tokens;
        self.total_cache_read_tokens = file.cache_read_tokens;
        self.total_cache_write_tokens = file.cache_write_tokens;
        self.api_calls = file.api_calls;
        self.api_attempts = file.api_attempts;
        self.tool_schema_tokens = file.tool_schema_tokens;
        self.tool_schema_tokens_saved = file.tool_schema_tokens_saved;
        self.sub_input_tokens = file.sub_input_tokens;
        self.sub_output_tokens = file.sub_output_tokens;
        self.sub_cache_read_tokens = file.sub_cache_read_tokens;
        self.sub_cache_write_tokens = file.sub_cache_write_tokens;
        self.aux_input_tokens = file.aux_input_tokens;
        self.aux_output_tokens = file.aux_output_tokens;
        self.aux_cache_read_tokens = file.aux_cache_read_tokens;
        self.aux_calls = file.aux_calls;
        self.prompt_cache_state = file.prompt_cache_state;
        self.input_estimate_ratio = file.input_estimate_ratio;
        self.overflow_window_hint = file.overflow_window_hint;
        self.compact_count = file.compact_count;
        self.compact_generation = file.compact_generation;
        // `elided_blobs` 必须一起恢复：它是「外部化到 CAS 的 blob 仍被本会话
        // 引用」的唯一凭证。漏掉它 = 退出/`/clear` 时一个都不 release，这些
        // blob 的 ref_count 永远 ≥1，`WorkspaceCas::gc` 永远删不掉；而且下次
        // 同内容再被外部化时 `intern()` 又 +1——每次 resume 净增一批。
        self.elided_blobs = file.elided_blobs.clone();
        self.total_reasoning_tokens = file.reasoning_tokens;
        self.cache_samples = file.cache_samples.clone();
        self.cache_fingerprints = file.cache_fingerprints.clone();
        self.context_edit_freed_tokens = file.context_edit_freed_tokens;
        self.session_profile = file.session_profile.clone();
        self.session_effort = file.session_effort.clone();
        self.routing_events = file.routing_events.clone();
        self.current_checkpoint_id = file.current_checkpoint_id.clone();
        self.branch_parent_session_id = file.branch_parent_session_id.clone();
        self.branch_parent_checkpoint_id = file.branch_parent_checkpoint_id.clone();
        self.prompt_fingerprint = file.prompt_fingerprint.clone();
        // **切换会话时必须把「本进程已检测过」与「上一个会话的漂移结论」一起清掉**。
        // `/resume` 是在同一个 `Session` 实例上调用本方法的（`app.rs` 的会话切换
        // 分支），只搬指纹会让 `Agent` 的 `if !prompt_fingerprint_checked` 直接跳过
        // 检测，而 `/context` 拿到的 `prompt_drift_since_resume` 仍是上一个会话的
        // 值——张冠李戴比不显示更糟。启动路径（新 `Session`）这两项本来就是默认值，
        // 在方法里重置对它们无副作用。
        self.prompt_fingerprint_checked = false;
        self.prompt_drift_since_resume = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 悬空 tool_use 补齐 ────────────────────────────────────────────────

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({ "command": "git status --short" }),
        }
    }

    fn dangling_tail() -> Session {
        // resume 一个「长任务被 ESC 打断」后的会话：末尾 assistant 带着
        // tool_use，工具结果从未写回，随后用户发来「继续」。
        let mut session = Session::new();
        session.push_user("跑一下 git status");
        session.push_assistant(vec![
            ContentBlock::Text {
                text: "好".to_string(),
            },
            tool_use("toolu_1"),
        ]);
        session.push_user("继续");
        session
    }

    #[test]
    fn dangling_tool_use_gets_a_synthetic_result() {
        let mut session = dangling_tail();
        assert_eq!(session.repair_dangling_tool_uses(), 1);

        let tail = session.messages.last().unwrap();
        assert!(matches!(tail.role, Role::User));
        assert!(
            matches!(&tail.content[0], ContentBlock::ToolResult { tool_use_id, is_error: true, .. }
                if tool_use_id == "toolu_1"),
            "合成结果必须是 user 消息里的第一个块，且标记为错误"
        );
        assert!(
            matches!(&tail.content[1], ContentBlock::Text { text } if text == "继续"),
            "用户原文必须仍留在合成结果之后"
        );
    }

    #[test]
    fn repair_is_idempotent_and_leaves_clean_history_untouched() {
        let mut session = dangling_tail();
        assert_eq!(session.repair_dangling_tool_uses(), 1);
        let after_first = session.messages.len();
        assert_eq!(
            session.repair_dangling_tool_uses(),
            0,
            "已配对的历史不得被二次修补"
        );
        assert_eq!(session.messages.len(), after_first);
    }

    // ── 估算校准与有效窗口 ──────────────────────────────────────────────

    fn session_with_audit(prompt_estimate: u32) -> Session {
        let mut session = Session::new();
        session.context_audit = crate::compact::ContextAudit {
            system_tokens: prompt_estimate,
            tool_schema_tokens: 0,
            history_tokens: 0,
            output_reserve: 32_000,
            framing_tokens: 0,
            window: 0,
            reclaimable_tool_result_tokens: 0,
            ..Default::default()
        };
        session
    }

    /// 未校准时系数必须是 1.0：阈值判定此时与旧版逐字节相同。
    #[test]
    fn estimate_ratio_defaults_to_one() {
        let session = Session::new();
        assert_eq!(session.input_estimate_ratio, None);
        assert_eq!(session.estimate_ratio(), 1.0);
    }

    /// 估算偏低时系数要往上走：压缩阈值跟着上移，压缩才会在真正接近窗口
    /// 之前触发，而不是先撞 400。
    #[test]
    fn observe_prompt_tokens_tracks_a_systematic_underestimate() {
        let mut session = session_with_audit(100_000);
        // 真实是估算的 1.4 倍 → 系数应逐步向 1.4 靠
        for _ in 0..40 {
            session.observe_prompt_tokens(140_000);
        }
        let ratio = session.estimate_ratio();
        assert!(
            (1.3..=1.5).contains(&ratio),
            "系数应收敛到实测偏差附近，实测 {ratio}"
        );
    }

    /// 区间外的样本（端点把 cache 算进 prompt、或干脆编数）必须丢弃：
    /// 照单全收会让压缩阈值漂到不可预期的位置。
    #[test]
    fn observe_prompt_tokens_ignores_out_of_range_samples() {
        let mut session = session_with_audit(100_000);
        session.input_estimate_ratio = Some(1.0);
        session.observe_prompt_tokens(100_000_000);
        assert_eq!(session.estimate_ratio(), 1.0, "越界样本必须被丢弃");
        session.observe_prompt_tokens(0);
        assert_eq!(session.estimate_ratio(), 1.0, "零上报必须被丢弃");
    }

    /// 分母太小时不采样：整数启发式在小规模上的相对误差天然很大，采进来
    /// 只会让系数抖动。
    #[test]
    fn observe_prompt_tokens_skips_tiny_estimates() {
        let mut session = session_with_audit(500);
        session.input_estimate_ratio = Some(1.0);
        session.observe_prompt_tokens(100_000);
        assert_eq!(session.estimate_ratio(), 1.0);
    }

    /// 端点拒绝过一次之后，有效窗口必须按实测收紧——否则 `context_window`
    /// 写成 1M 而真实只有 256K 时，两道防线会一起落在够不着的位置。
    #[test]
    fn overflow_hint_shrinks_the_effective_window() {
        let mut session = Session::new();
        assert_eq!(session.effective_context_window(1_000_000), 1_000_000);
        session.note_context_overflow(256_000);
        assert_eq!(session.effective_context_window(1_000_000), 256_000);
        // 配置值更小时依然以配置为准（hint 只是上界证据，不是真相覆盖）
        assert_eq!(session.effective_context_window(128_000), 128_000);
    }

    /// 只往小取：连续撞两次时记住更严的那个。
    #[test]
    fn overflow_hint_only_ever_shrinks() {
        let mut session = Session::new();
        session.note_context_overflow(400_000);
        session.note_context_overflow(200_000);
        assert_eq!(session.overflow_window_hint, Some(200_000));
        assert_eq!(session.effective_context_window(1_000_000), 200_000);
        // 0 是「没量到」，不能把已学到的窗口上界抹成 0
        session.note_context_overflow(0);
        assert_eq!(session.overflow_window_hint, Some(200_000));
    }

    /// **配置值就是配置值**：静态容量表不得改写它。
    ///
    /// 这是用户明确要求的语义——`context_window` 填多少就是多少，Anthropic
    /// 与 OpenAI 两条线走同一个判据。旧实现按 `known_model_capacity()` 的
    /// 模型名子串收紧，导致同一个模型换个名字写法（`MiniMax-M3.1-Flash` vs
    /// `minimax/minimax-m3.1-flash:free`）就得到不同的窗口，
    /// 表现为「anthropic 端点频繁压缩、openai 端点不压缩」。
    #[test]
    fn configured_window_is_never_rewritten_by_the_catalog() {
        let session = Session::new();
        // 目录说 204,800、配置写 1M → 仍按 1M 判定（Minimax 现场配置）。
        assert_eq!(session.effective_context_window(1_000_000), 1_000_000);
        // 配置值更小：同样保持原值，绝不放大。
        assert_eq!(session.effective_context_window(100_000), 100_000);
    }

    /// 真实的收紧只剩一条：端点亲口拒绝时记下的实测上界。
    #[test]
    fn only_the_endpoint_measured_hint_shrinks_the_window() {
        let mut session = Session::new();
        assert_eq!(
            session.effective_context_window(1_000_000),
            1_000_000,
            "没有实测证据时必须原样使用配置值"
        );
        session.note_context_overflow(128_000);
        assert_eq!(
            session.effective_context_window(1_000_000),
            128_000,
            "端点实测才是唯一有效的收紧依据"
        );
    }

    /// `/clear` 换对话，但端点不会变——校准系数描述的是「这套端点 + 这类
    /// 内容下估算偏多少」，抹掉它等于让新对话从头再失准一遍。
    #[test]
    fn clear_conversation_keeps_the_estimate_calibration() {
        let mut session = session_with_audit(100_000);
        session.input_estimate_ratio = Some(1.25);
        session.overflow_window_hint = Some(300_000);
        session.push_user("某个问题");
        session.clear_conversation();
        assert_eq!(session.estimate_ratio(), 1.25);
        assert_eq!(session.overflow_window_hint, Some(300_000));
        assert!(session.messages.is_empty(), "对话本身仍然要清空");
    }

    #[test]
    fn answered_tool_use_is_never_touched() {
        let mut session = Session::new();
        session.push_user("跑一下 git status");
        session.push_assistant(vec![tool_use("toolu_1")]);
        session.push_tool_result(
            "toolu_1".to_string(),
            ToolResultContent::Text("done".into()),
            false,
        );
        session.push_user("继续");
        assert_eq!(session.repair_dangling_tool_uses(), 0);
    }

    #[test]
    fn dangling_tool_use_in_the_middle_is_left_alone() {
        // 端点对历史中段的未配对 tool_use 是容忍的（实测），且 context editing
        // 会把旧 tool_result 外部化成占位文本——按「全历史扫描未配对」去补，
        // 会给这些旧调用凭空造结果、污染历史。
        let mut session = Session::new();
        session.push_user("a");
        session.push_assistant(vec![tool_use("toolu_old")]);
        session.push_user("换个命令");
        session.push_assistant(vec![ContentBlock::Text {
            text: "已经改好了".to_string(),
        }]);
        session.push_user("继续");
        assert_eq!(session.repair_dangling_tool_uses(), 0);
        assert_eq!(session.messages.len(), 5);
    }

    #[test]
    fn tail_that_is_not_a_user_message_gets_its_own_result_message() {
        // 压缩 / 子 Agent 收尾等边界上，请求末尾可能仍是 assistant 消息。
        // 另起一条 user 消息承载结果，才能维持角色严格交替。
        let mut session = Session::new();
        session.push_user("跑一下");
        session.push_assistant(vec![tool_use("toolu_1"), tool_use("toolu_2")]);
        assert_eq!(session.repair_dangling_tool_uses(), 2);
        let tail = session.messages.last().unwrap();
        assert!(matches!(tail.role, Role::User));
        assert_eq!(tail.content.len(), 2);
    }

    #[test]
    fn history_without_assistant_message_is_a_noop() {
        let mut session = Session::new();
        session.push_user("只有用户消息");
        assert_eq!(session.repair_dangling_tool_uses(), 0);
        assert_eq!(session.messages.len(), 1);
    }

    // ── 子 Agent 用量回灌 ────────────────────────────────────────────────

    fn usage(input: u32, output: u32, cache_read: u32, cache_write: u32) -> SubUsage {
        SubUsage {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn sub_totals_take_is_draining_not_peeking() {
        let totals = SubUsageTotals::default();
        totals.add(usage(100, 20, 5_000, 7));
        let first = totals.take();
        assert_eq!(first, usage(100, 20, 5_000, 7));
        // 抽干后必须归零：否则主 Agent 每批工具执行后 drain 一次，
        // 同一批子 Agent 的用量会被反复计入（take 的语义是 drain）。
        assert_eq!(totals.take(), SubUsage::default());
    }

    #[test]
    fn concurrent_subagents_accumulate_without_loss() {
        let totals = Arc::new(SubUsageTotals::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let totals = Arc::clone(&totals);
                std::thread::spawn(move || totals.add(usage(10, 2, 100, 0)))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(totals.take(), usage(80, 16, 800, 0));
    }

    #[test]
    fn drain_moves_subagent_usage_into_session_without_touching_main_totals() {
        let totals = SubUsageTotals::default();
        totals.add(usage(3_000, 400, 120_000, 0));
        let mut session = Session::new();
        session.add_usage(500, 50);
        session.add_cache_usage(9_000, 0);

        session.drain_sub_usage(Some(&totals));

        // 主会话口径不变——子 Agent 用量必须**不重叠**，否则 /cost 会双计。
        assert_eq!(session.total_input_tokens, 500);
        assert_eq!(session.total_output_tokens, 50);
        assert_eq!(session.total_cache_read_tokens, 9_000);
        // 子 Agent 口径独立。
        assert_eq!(session.sub_input_tokens, 3_000);
        assert_eq!(session.sub_output_tokens, 400);
        assert_eq!(session.sub_cache_read_tokens, 120_000);
    }

    #[test]
    fn repeated_drain_is_idempotent() {
        let totals = SubUsageTotals::default();
        totals.add(usage(1_000, 100, 50_000, 0));
        let mut session = Session::new();
        session.drain_sub_usage(Some(&totals));
        session.drain_sub_usage(Some(&totals));
        session.drain_sub_usage(Some(&totals));
        assert_eq!(session.sub_input_tokens, 1_000);
        assert_eq!(session.sub_cache_read_tokens, 50_000);
    }

    #[test]
    fn drain_without_sink_is_a_noop() {
        let mut session = Session::new();
        session.drain_sub_usage(None);
        assert_eq!(session.sub_usage(), SubUsage::default());
    }

    #[test]
    fn clear_conversation_also_resets_subagent_usage() {
        let totals = SubUsageTotals::default();
        totals.add(usage(700, 70, 21_000, 0));
        let mut session = Session::new();
        session.drain_sub_usage(Some(&totals));
        assert!(session.sub_input_tokens > 0);
        // /clear 换的是对话内容，但计费用量一并归零——否则新对话的 /cost
        // 会把上一段对话的子 Agent 开销算进来。
        session.clear_conversation();
        assert_eq!(session.sub_usage(), SubUsage::default());
    }

    // ── 工具 schema 双口径 ───────────────────────────────────────────────

    /// 回归：`tool_schema_tokens` 是**全会话累加**，`tool_schema_tokens_last`
    /// 才是最近一次请求的稳态值。历史上只有前者且被当作稳态值展示，
    /// 225 次调用的会话会显示成 1,457,192（真实稳态值只有 6,281）。
    #[test]
    fn tool_schema_accumulates_while_last_stays_steady() {
        let mut session = Session::new();
        for _ in 0..225 {
            session.tool_schema_tokens_last = 6_281;
            session.tool_schema_tokens = session.tool_schema_tokens.saturating_add(6_281);
        }
        assert_eq!(session.tool_schema_tokens, 225 * 6_281);
        assert_eq!(session.tool_schema_tokens_last, 6_281);
        assert!(session.tool_schema_tokens > session.tool_schema_tokens_last * 100);
    }

    #[test]
    fn clear_conversation_preserves_checkpoint_lineage() {
        let mut session = Session::new();
        session.push_user("hello");
        session.total_input_tokens = 10;
        session.current_checkpoint_id = Some("checkpoint-1".to_string());
        session.branch_parent_session_id = Some("parent".to_string());
        session.branch_parent_checkpoint_id = Some("parent-checkpoint".to_string());
        session.clear_conversation();
        assert!(session.messages.is_empty());
        assert_eq!(session.total_input_tokens, 0);
        assert_eq!(
            session.current_checkpoint_id.as_deref(),
            Some("checkpoint-1")
        );
        assert_eq!(session.branch_parent_session_id.as_deref(), Some("parent"));
        assert_eq!(
            session.branch_parent_checkpoint_id.as_deref(),
            Some("parent-checkpoint")
        );
    }
}
