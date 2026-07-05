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
    /// 权限确认请求（stub，目前仅展示）
    PermissionRequest {
        tool_name: String,
        input_preview: String,
        tx_id: String,
    },
    /// Agent 一轮完成
    TurnDone,
    /// Agent 出错
    Error(String),
    /// Token 用量（覆盖式更新）。`context_tokens` 是本轮结束时 session.messages 的
    /// 实际大小估算（供状态栏占比显示），与 `input`（跨轮次累加的历史用量总和，
    /// 供 /cost 与单轮增量展示）是不同的量。
    Usage {
        input: u32,
        output: u32,
        context_tokens: u32,
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
    PermissionResponse { tx_id: String, approved: bool },
    ScrollUp,
    ScrollDown,
    Quit,
}

/// 检测是否是立即退出快捷键 (Ctrl+D)
pub fn is_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL))
}
