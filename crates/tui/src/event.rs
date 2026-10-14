//! 事件类型：终端输入事件 + Agent 输出事件

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 来自 Agent 的输出事件（发到 UI 线程）
// 注意：含 oneshot::Sender 的变体不能 Clone，故整体不 derive Clone
#[derive(Debug)]
pub enum AgentEvent {
    /// 流式文本片段
    TextDelta(String),
    /// extended thinking 文本增量（独立于正文流式展示）
    ThinkingDelta(String),
    /// 工具调用开始（携带输入 JSON，用于提取展示参数）
    ToolStart {
        id: String,
        name: String,
        input_json: serde_json::Value,
    },
    /// 工具调用完成（包含实际执行耗时）
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
        elapsed_secs: f64,
    },
    /// 逐调用工具权限确认请求（含 oneshot 决策通道）
    ToolPermissionRequest {
        tool_name: String,
        action_summary: String,
        response_tx: tokio::sync::oneshot::Sender<wyj_tools::PermissionDecision>,
    },
    /// Agent 一轮完成
    TurnDone,
    /// Agent 出错
    Error(String),
    /// Token 用量（覆盖式更新）。`compact_count` 是本会话自动压缩次数，供状态栏
    /// 显示语义指示；`input` 是跨轮次累加的历史用量总和，供 /cost 与单轮增量
    /// 展示。上下文占用的精确分解走 `/context`，不在事件流里重复传输。
    Usage {
        input: u32,
        output: u32,
        compact_count: u32,
        tool_schema_tokens: u32,
        tool_schema_tokens_saved: u32,
        /// 本回合的时延画像（TTFT / 总耗时 / 重试开销）。跟着 `Usage` 走而不是
        /// 新增一个事件，是因为两者的采集点完全相同（Agent 的 stream 循环收尾
        /// 处）、生命周期也相同（每回合一个快照），拆成两个事件只会让发送侧多
        /// 一半发送次数。
        latency: wyj_core::session::TurnLatency,
    },
    /// 单次 LLM 流式返回的增量 token 用量（非累计），用于把 token 消耗实时归因到
    /// 当前 in_progress 的任务（与 `Usage` 的覆盖式总量不同）。
    UsageDelta {
        input_tokens: u32,
        output_tokens: u32,
    },
    /// TodoWrite 工具完成后推送任务列表快照
    TodoUpdate(Vec<wyj_tools::todo::TodoItem>),
    /// AskQuestion 工具请求用户完成多题访谈（含 oneshot 响应通道）
    AskQuestions {
        questions: Vec<wyj_core::tool::AskQuestionSpec>,
        response_tx: tokio::sync::oneshot::Sender<Option<Vec<wyj_core::tool::QuestionAnswer>>>,
    },
    /// ! Bash 命令执行完成
    BashResult {
        output: String,
        exit_code: i32,
        elapsed_secs: f64,
    },
    /// ExitPlanMode 工具触发的计划批准请求，plan 为完整计划文本（Markdown）
    PlanApprovalRequest {
        plan: String,
        response_tx: tokio::sync::oneshot::Sender<bool>,
    },
    /// Agent 忙碌期间排队的补充消息已被消费并合并进 session
    Injected,
    /// 分组管理面板"拉取模型列表"结果（entry 下标, field 下标, 拉取结果）
    ModelsFetched {
        entry_idx: usize,
        field_idx: usize,
        result: Result<Vec<String>, String>,
    },
    /// 子 Agent 生命周期事件（SubAgentHub 汇聚转发）
    SubAgent(wyj_tools::SubAgentEvent),
    /// 用户在后台任务面板按 `k` 终止某个任务后，kill 流程完成的通知
    ShellKilled(String),
    /// 后台标题生成完成（首轮后 LLM 生成短标题，用于更新终端窗口标题）
    TitleGenerated(String),
    /// /mcp 面板 Browse tab 发起的 registry 搜索结果
    McpRegistryFetched {
        result: Result<Vec<wyj_store::registry::RegistryServerSummary>, String>,
    },
    /// /skills 面板对某个 marketplace 的 clone/pull + 清单解析结果
    SkillMarketplaceSynced {
        marketplace_id: String,
        git_url: String,
        result: Result<Vec<wyj_store::marketplace::MarketplaceSkillEntry>, String>,
    },
    /// /skills 面板 Browse tab 发起的 HTTP 技能注册表（SkillHub）搜索结果
    SkillRegistrySearched {
        result: Result<Vec<wyj_store::skillhub::SkillHubSkill>, String>,
    },
    /// /skills 面板：注册表技能包已下载并扫描出内容预览，等用户确认是否安装。
    /// 携带原始 zip 字节是为了让确认后的安装不必再下载一次。
    SkillRegistryArchiveFetched {
        result: Result<(wyj_store::skillhub::SkillArchivePreview, Vec<u8>), String>,
    },
    /// /mcp 面板"升级"结果（row_idx 对应 McpDialog.installed 下标）
    McpUpgraded {
        row_idx: usize,
        result: Result<wyj_store::UpgradeOutcome, String>,
    },
    /// /skills 面板"升级"结果（row_idx 对应 SkillsDialog.installed 下标）
    SkillUpgraded {
        row_idx: usize,
        result: Result<wyj_store::UpgradeOutcome, String>,
    },
    /// 启动时后台连接某个 MCP server 成功——TUI 不再在界面打开前同步等待
    /// MCP 连接（会拖慢启动），改为界面先打开、MCP 在后台连接，成功后把
    /// 新工具注册进当前 agent 快照并原子替换回 shared_agent。
    McpBackgroundConnected { name: String, tool_count: usize },
    /// 启动时后台连接某个 MCP server 失败或超时——此前只写 `tracing::warn!`
    /// 日志，用户在 TUI 里完全无感知；现在同时推一条可见提示 + 更新
    /// `AppState.mcp_connection_status`，供 `/mcp` 面板逐行展示连接状态。
    McpBackgroundFailed {
        name: String,
        reason: McpConnFailReason,
    },
    /// /plugins 面板对某个插件 marketplace 的 clone/pull + marketplace.json 解析结果
    PluginMarketplaceSynced {
        marketplace_id: String,
        result: Result<wyj_store::plugin_manifest::PluginMarketplaceManifest, String>,
    },
    /// /plugins 面板安装结果
    PluginInstalled {
        result: Result<wyj_store::plugin_install::PluginInstallReport, String>,
    },
    /// /plugins 面板"升级"结果（row_idx 对应 PluginsDialog.installed 下标）
    PluginUpgraded {
        row_idx: usize,
        result: Result<wyj_store::UpgradeOutcome, String>,
    },
    /// `/compact` 的后台压缩结果。
    ///
    /// 为什么必须走事件而不是在主循环里 `await`（`crates/tui/CLAUDE.md` 第 19 条）：
    /// 压缩是一次完整的 LLM 往返，摘要输出预算给到 `min(cw/10, 16000)`，
    /// 再叠 `RetryPolicy::default()` 的 5 次指数退避（单次尝试上限 90s），
    /// 端点不稳时可以拖到数分钟。它挂在主循环上 = 不重绘、不 `event::poll`，
    /// ESC / Ctrl+C 全部失效。
    CompactFinished {
        result: Result<wyj_core::compact::CompactResult, String>,
        /// 压缩后的自动压缩计数（锁内快照，锁必须先放再发事件）。
        compact_count: u32,
    },
    /// 声明了 [`wyj_commands::Command::needs_background`] 的 slash 命令跑完。
    /// 这类命令含子进程 / 网络往返（`gh`、`open`），inline 执行同样冻住主循环。
    BackgroundCommandDone {
        name: String,
        result: Result<wyj_commands::CommandResult, String>,
    },
    /// `/checkpoint` 创建完成。`Ok` 是新 checkpoint 的 id。
    CheckpointCreated(Result<String, String>),
    /// `/rewind` 的准备工作（确认预览 / 回退前保护 + 文件还原）在后台跑完。
    RewindPrepared(Result<RewindPrepared, String>),
    /// `/branch` 在后台跑完（含切换 session / 重定向 agent），UI 只需刷新展示。
    BranchPrepared(Result<BranchSwitch, String>),
    /// `/evolve` 面板里某个确认动作（激活 / 拒绝 / 回滚）执行完成。
    EvolutionActionDone(Result<String, String>),
}

/// `/rewind` 在后台 task 里跑完之后带回的结果。
///
/// 拆成两个分支是因为"需要二次确认"不是错误也不是成功：文件还原会覆盖工作区，
/// 必须先把预览文案交给用户，等他重新带上 `--confirm` 再跑一次。
///
/// 后台 task 已经把 `messages` 写回了 live session（那里持锁等锁是对的），
/// 这里带回的 `messages` 只用于重建**展示**用的消息列表。
#[derive(Debug)]
pub enum RewindPrepared {
    /// 需要用户确认，UI 只需把提示文案贴进聊天区。
    NeedsConfirm(String),
    /// 已就绪：UI 据此 `reconstruct_display` 并刷新状态栏。
    Ready {
        /// 被回退到的 checkpoint id（仅用于结果文案）。
        checkpoint_id: String,
        /// 本次是否真的换了会话历史（`/rewind --files` 为 false）。
        /// UI 只在 true 时重建展示列表，避免把仅存在于 UI 层的提示消息冲掉。
        restored_conversation: bool,
        protection_id: String,
        /// 文件还原预览文案（空串表示本次不还原文件）。
        file_text: String,
        messages: Vec<wyj_api::types::Message>,
        compact_count: u32,
        turns: usize,
    },
}

/// `/branch` 后台任务完成后的会话切换信息。
///
/// 与 `RewindPrepared` 同理：session 已被后台 task 换掉，这里只带展示层
/// 重新构建所需的数据 + 新的会话 id（主循环据此同步本地镜像）。
#[derive(Debug)]
pub struct BranchSwitch {
    pub session_id: String,
    pub checkpoint_id: String,
    pub messages: Vec<wyj_api::types::Message>,
    pub turns: usize,
}

/// MCP server 后台连接失败的具体原因，供渲染层区分文案
#[derive(Debug, Clone)]
pub enum McpConnFailReason {
    Error(String),
    Timeout,
}

/// 来自 UI 的用户事件（保留定义，目前未使用）
#[derive(Debug, Clone)]
pub enum UiEvent {
    Submit(String),
    ScrollUp,
    ScrollDown,
    Quit,
}

/// 检测是否是立即退出快捷键 (Ctrl+D)
pub fn is_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL))
}
