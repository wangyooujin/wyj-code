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
