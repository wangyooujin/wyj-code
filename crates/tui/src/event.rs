//! 事件类型：终端输入事件 + Agent 输出事件

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 来自 Agent 的输出事件（发到 UI 线程）
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 流式文本片段
    TextDelta(String),
    /// 工具调用开始
    ToolStart { id: String, name: String },
    /// 工具调用完成
    ToolEnd { id: String, output: String, is_error: bool },
    /// 权限确认请求
    PermissionRequest { tool_name: String, input_preview: String, tx_id: String },
    /// Agent 一轮完成
    TurnDone,
    /// Agent 出错
    Error(String),
    /// Token 用量
    Usage { input: u32, output: u32 },
}

/// 来自 UI 的用户事件
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 用户提交消息
    Submit(String),
    /// 对权限请求的响应
    PermissionResponse { tx_id: String, approved: bool },
    /// 滚动操作
    ScrollUp,
    ScrollDown,
    /// 退出
    Quit,
}

/// 检测是否是退出快捷键 (Ctrl+C / Ctrl+D / Escape)
pub fn is_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
        || matches!(key.code, KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL))
}
