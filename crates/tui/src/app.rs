//! TUI 应用主循环

use crate::event::{is_quit, AgentEvent};
use crate::input::InputBox;
use crate::render;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::AgentMode;
use wyj_core::{now_iso, Agent, HistoryEntry, HistoryStore, Session, ToolEvent};
use wyj_tools::todo::TodoItem;
use wyj_tools::{ctx::UiAskRequest, PermissionMode};
use wyj_tools::{TodoStore, ToolCtx};

/// 用于 /model 热切换的 Agent 重建函数类型
pub type RebuildFn = Arc<dyn Fn(&str) -> anyhow::Result<Agent> + Send + Sync>;

/// 消息角色
#[derive(Debug, Clone)]
pub enum MessageRole {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    /// ! Bash 命令输出
    BashOutput,
    /// 系统通知（模式切换、会话事件等）
    System,
}

/// 渲染用消息
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    /// ToolCall = "ToolName(arg)"；ToolResult/BashOutput = 原始输出；User/Assistant = 正文
    pub content: String,
    pub is_error: bool,
    /// 工具执行耗时（ToolResult/BashOutput 专用）
    pub elapsed_secs: Option<f64>,
    /// 本次工具调用的序号（ToolCall 和 ToolResult 共用，从 1 开始）
    pub sequence_no: Option<usize>,
    /// 工具名（ToolResult 专用）
    pub tool_name: Option<String>,
    /// ToolResult 的一行摘要（Claude Code ⎿ 行）
    pub display_summary: String,
    /// 工具结果是否已展开（ToolResult 专用）
    pub expanded: bool,
}

impl ChatMessage {
    fn base(role: MessageRole, content: String) -> Self {
        Self {
            role,
            content,
            is_error: false,
            elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            expanded: false,
        }
    }

    fn user(content: String) -> Self {
        Self::base(MessageRole::User, content)
    }

    fn assistant(content: String) -> Self {
        Self::base(MessageRole::Assistant, content)
    }

    fn assistant_err(content: String) -> Self {
        let mut m = Self::base(MessageRole::Assistant, content);
        m.is_error = true;
        m
    }

    fn tool_call(display: String, seq: usize) -> Self {
        let mut m = Self::base(MessageRole::ToolCall, display);
        m.sequence_no = Some(seq);
        m
    }

    fn tool_result(
        output: String,
        is_error: bool,
        elapsed_secs: f64,
        seq: usize,
        name: String,
        summary: String,
    ) -> Self {
        Self {
            role: MessageRole::ToolResult,
            content: output,
            is_error,
            elapsed_secs: Some(elapsed_secs),
            sequence_no: Some(seq),
            tool_name: Some(name),
            display_summary: summary,
            expanded: false,
        }
    }

    fn bash_output(output: String, exit_code: i32, elapsed_secs: f64) -> Self {
        Self {
            role: MessageRole::BashOutput,
            content: output,
            is_error: exit_code != 0,
            elapsed_secs: Some(elapsed_secs),
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            expanded: false,
        }
    }

    pub fn system(content: String) -> Self {
        Self::base(MessageRole::System, content)
    }
}

/// 权限确认对话框状态
#[derive(Debug)]
pub struct PermissionDialog {
    pub tool_name: String,
    pub input_preview: String,
    pub tx_id: String,
}

/// AskQuestion 对话框状态
pub struct AskQuestionDialog {
    pub question: String,
    pub options: Vec<String>,
    pub selected: usize,
    pub response_tx: tokio::sync::oneshot::Sender<Option<usize>>,
}

// ── 工具展示帮助函数 ─────────────────────────────────────────────────────────

/// 从工具输入 JSON 中提取用于展示的参数字符串（如文件路径或命令）
fn tool_display_arg(name: &str, input: &serde_json::Value) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let shorten = |s: &str| -> String {
        if !home.is_empty() && s.starts_with(&home) {
            format!("~{}", &s[home.len()..])
        } else {
            s.to_string()
        }
    };
    let trunc = |s: String, max: usize| -> String {
        if s.chars().count() > max {
            format!(
                "{}…",
                s.chars().take(max.saturating_sub(1)).collect::<String>()
            )
        } else {
            s
        }
    };

    match name {
        "read" | "write" | "edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            trunc(shorten(path), 55)
        }
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            trunc(cmd.to_string(), 50)
        }
        "glob" => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
            trunc(p.to_string(), 40)
        }
        "grep" => {
            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            trunc(format!("{pat} in {}", shorten(path)), 50)
        }
        "web_fetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            trunc(url.to_string(), 55)
        }
        _ => String::new(),
    }
}

/// 从工具输出生成一行摘要（用于 ⎿ 行）
fn tool_result_summary(name: &str, output: &str, is_error: bool) -> String {
    let trunc1 = |s: &str| -> String {
        let s = s.trim();
        if s.chars().count() > 65 {
            format!("{}…", s.chars().take(64).collect::<String>())
        } else {
            s.to_string()
        }
    };

    if is_error {
        return trunc1(output.lines().next().unwrap_or(output));
    }

    match name {
        "read" => {
            let n = output.lines().count();
            format!("read {n} lines")
        }
        "edit" => trunc1(output.lines().next().unwrap_or("updated")),
        "write" => trunc1(output.lines().next().unwrap_or("written")),
        "bash" => {
            let nonempty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            let n = nonempty.len();
            let first = nonempty.first().copied().unwrap_or("(no output)");
            if n > 1 {
                format!("{} (+{} lines)", trunc1(first), n - 1)
            } else {
                trunc1(first)
            }
        }
        "grep" => {
            let n = output.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} matches")
        }
        "glob" => {
            let n = output.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} files")
        }
        "web_fetch" => format!("fetched {} bytes", output.len()),
        _ => trunc1(output.lines().next().unwrap_or(output)),
    }
}

/// 全局 UI 状态
pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub streaming_buf: String,
    pub is_thinking: bool,
    pub permission_dialog: Option<PermissionDialog>,
    pub ask_question_dialog: Option<AskQuestionDialog>,
    pub scroll_offset: u16,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub cwd: PathBuf,
    pub should_quit: bool,
    pub turns: usize,
    /// 当前 spinner 动画帧索引
    pub spinner_frame: usize,
    /// 累计工具调用次数
    pub tool_call_count: usize,
    /// 工具 id → (名称, 序号)，用于将 ToolEnd 与 ToolStart 关联
    pub tool_info: HashMap<String, (String, usize)>,
    pub model_name: String,
    pub context_window: u32,
    pub mode: AgentMode,
    pub current_task: Option<AbortHandle>,
    pub ctrl_c_pressed: bool,
    pub last_ctrl_c: Option<Instant>,
    /// Slash 命令补全候选列表（命令名, 描述）
    pub slash_completions: Vec<(String, String)>,
    /// 当前选中的补全项索引
    pub slash_selected: usize,
    /// 输入历史（每次 submit 追加）
    pub input_history: Vec<String>,
    /// 当前历史导航索引（None = 未在导航模式）
    pub history_idx: Option<usize>,
    /// 本 Session 已授权的工具（按 s 键授权）
    pub session_allowed: HashSet<String>,
    /// 当前任务列表快照（TodoWrite 更新），用于底部固定面板渲染
    pub current_todos: Option<Vec<TodoItem>>,
}

impl AppState {
    fn new(cwd: PathBuf, model_name: String, context_window: u32, mode: AgentMode) -> Self {
        Self {
            messages: vec![],
            streaming_buf: String::new(),
            is_thinking: false,
            permission_dialog: None,
            ask_question_dialog: None,
            scroll_offset: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            cwd,
            should_quit: false,
            turns: 0,
            spinner_frame: 0,
            tool_call_count: 0,
            tool_info: HashMap::new(),
            model_name,
            context_window,
            mode,
            current_task: None,
            ctrl_c_pressed: false,
            last_ctrl_c: None,
            slash_completions: vec![],
            slash_selected: 0,
            input_history: vec![],
            history_idx: None,
            session_allowed: HashSet::new(),
            current_todos: None,
        }
    }

    /// 中断当前正在运行的 Agent，保留已输出内容并标记 [已中断]
    fn interrupt(&mut self) {
        if let Some(h) = self.current_task.take() {
            h.abort();
        }
        if self.is_thinking {
            if self.streaming_buf.is_empty() {
                self.streaming_buf = "[已中断]".to_string();
            } else {
                self.streaming_buf.push_str("\n\n[已中断]");
            }
            self.flush_streaming();
            self.is_thinking = false;
        }
        self.permission_dialog = None;
        if let Some(dlg) = self.ask_question_dialog.take() {
            let _ = dlg.response_tx.send(None);
        }
    }

    fn push_user(&mut self, text: String) {
        self.messages.push(ChatMessage::user(text));
    }

    fn flush_streaming(&mut self) {
        if !self.streaming_buf.is_empty() {
            let text = std::mem::take(&mut self.streaming_buf);
            self.messages.push(ChatMessage::assistant(text));
        }
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(d) => self.streaming_buf.push_str(&d),

            AgentEvent::ToolStart {
                id,
                name,
                input_json,
            } => {
                self.tool_call_count += 1;
                let seq = self.tool_call_count;
                let arg = tool_display_arg(&name, &input_json);
                let display = if arg.is_empty() {
                    name.clone()
                } else {
                    format!("{name}({arg})")
                };
                self.tool_info.insert(id, (name, seq));
                self.flush_streaming();
                self.messages.push(ChatMessage::tool_call(display, seq));
            }

            AgentEvent::ToolEnd {
                id,
                output,
                is_error,
                elapsed_secs,
            } => {
                let (name, seq) = self.tool_info.remove(&id).unwrap_or_default();
                let summary = tool_result_summary(&name, &output, is_error);
                self.messages.push(ChatMessage::tool_result(
                    output,
                    is_error,
                    elapsed_secs,
                    seq,
                    name,
                    summary,
                ));
            }

            AgentEvent::PermissionRequest {
                tool_name,
                input_preview,
                tx_id,
            } => {
                self.permission_dialog = Some(PermissionDialog {
                    tool_name,
                    input_preview,
                    tx_id,
                });
            }

            AgentEvent::TurnDone => {
                self.flush_streaming();
                self.is_thinking = false;
                self.turns += 1;
            }

            AgentEvent::Error(e) => {
                self.flush_streaming();
                self.is_thinking = false;
                self.messages
                    .push(ChatMessage::assistant_err(format!("[错误] {e}")));
            }

            AgentEvent::Usage { input, output } => {
                self.total_input_tokens = input;
                self.total_output_tokens = output;
            }

            AgentEvent::TodoUpdate(items) => {
                self.current_todos = Some(items);
                self.scroll_offset = 0; // 自动滚动到最新任务列表
            }

            AgentEvent::AskQuestion {
                question,
                options,
                response_tx,
            } => {
                self.ask_question_dialog = Some(AskQuestionDialog {
                    question,
                    options,
                    selected: 0,
                    response_tx,
                });
            }

            AgentEvent::BashResult {
                output,
                exit_code,
                elapsed_secs,
            } => {
                self.messages
                    .push(ChatMessage::bash_output(output, exit_code, elapsed_secs));
                self.scroll_offset = 0;
            }
        }
    }
}

/// 启动 TUI 主界面
pub async fn run_tui(
    agent: Agent,
    rebuild_fn: RebuildFn,
    cwd: PathBuf,
    history_store: Option<HistoryStore>,
    session_id: String,
    model_name: String,
    context_window: u32,
    mode: AgentMode,
    todo_store: Arc<std::sync::Mutex<TodoStore>>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = tui_main(
        &mut terminal,
        agent,
        rebuild_fn,
        cwd,
        history_store,
        session_id,
        model_name,
        context_window,
        mode,
        todo_store,
    )
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

/// 循环切换 Agent 模式：Normal → Plan → Bypass → Normal
fn cycle_mode(mode: &AgentMode) -> AgentMode {
    match mode {
        AgentMode::Normal => AgentMode::Plan,
        AgentMode::Plan => AgentMode::Bypass,
        AgentMode::Bypass => AgentMode::Normal,
    }
}

/// 根据 AgentMode 构建对应的 PermissionMode
fn mode_to_permission(mode: &AgentMode) -> PermissionMode {
    match mode {
        AgentMode::Plan => {
            let set: HashSet<String> = ["Read", "Glob", "Grep", "WebFetch", "AskQuestion"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            PermissionMode::Allowlist(set)
        }
        AgentMode::Bypass => PermissionMode::AutoApprove,
        AgentMode::Normal => PermissionMode::Prompt,
    }
}

/// 根据输入框内容更新 slash 补全候选列表
fn update_slash_completions(
    state: &mut AppState,
    input: &InputBox,
    registry: &wyj_commands::CommandRegistry,
) {
    let first_line = input.lines.first().map(|s| s.as_str()).unwrap_or("");
    if input.lines.len() == 1 && first_line.starts_with('/') {
        let mut completions = registry.complete(first_line);
        completions.sort_by(|a, b| a.0.cmp(&b.0));
        if completions != state.slash_completions {
            state.slash_completions = completions;
            state.slash_selected = 0;
        }
    } else if !state.slash_completions.is_empty() {
        state.slash_completions.clear();
        state.slash_selected = 0;
    }
}

async fn tui_main<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
    agent: Agent,
    rebuild_fn: RebuildFn,
    cwd: PathBuf,
    history_store: Option<HistoryStore>,
    session_id: String,
    model_name: String,
    context_window: u32,
    mode: AgentMode,
    todo_store: Arc<std::sync::Mutex<TodoStore>>,
) -> Result<()> {
    let shared_mode = Arc::new(tokio::sync::Mutex::new(mode.clone()));
    let mut state = AppState::new(cwd.clone(), model_name, context_window, mode);
    let mut input = InputBox::new();

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (ui_ask_tx, mut ui_ask_rx) = mpsc::channel::<UiAskRequest>(8);
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let cmd_registry = standard_registry_with_skills(&home_dir, &cwd);

    // 工具回调：ToolStart/ToolEnd → AgentEvent，同时拦截 TodoWrite 读取快照
    let todo_store_cb = todo_store.clone();
    let tool_tx = agent_tx.clone();
    let agent = agent.with_tool_callback(move |event| match event {
        ToolEvent::Start { id, name, input } => {
            let _ = tool_tx.try_send(AgentEvent::ToolStart {
                id,
                name,
                input_json: input,
            });
        }
        ToolEvent::End {
            id,
            name,
            is_error,
            elapsed_secs,
            output,
        } => {
            if name == "TodoWrite" && !is_error {
                if let Ok(store) = todo_store_cb.lock() {
                    let items = store.items.clone();
                    let _ = tool_tx.try_send(AgentEvent::TodoUpdate(items));
                }
            }
            let _ = tool_tx.try_send(AgentEvent::ToolEnd {
                id,
                output,
                is_error,
                elapsed_secs,
            });
        }
    });

    // 用 RwLock 包装 agent，支持 /model 热切换
    let shared_agent = Arc::new(std::sync::RwLock::new(Arc::new(agent)));
    let session = Arc::new(Mutex::new(Session::new()));

    let mut last_spinner_advance = Instant::now();

    loop {
        // Ctrl+C 首次按下超过 3 秒未确认则重置
        if let Some(t) = state.last_ctrl_c {
            if t.elapsed() > Duration::from_secs(3) {
                state.ctrl_c_pressed = false;
                state.last_ctrl_c = None;
            }
        }

        // 推进 spinner 动画帧（每 ~120ms 一帧）
        if state.is_thinking && last_spinner_advance.elapsed().as_millis() >= 120 {
            state.spinner_frame = (state.spinner_frame + 1) % render::SPINNER_FRAMES.len();
            last_spinner_advance = Instant::now();
        }

        terminal.draw(|f| render::draw(f, &state, &input))?;

        // 清空 agent 事件队列
        loop {
            match agent_rx.try_recv() {
                Ok(ev) => state.apply_agent_event(ev),
                Err(_) => break,
            }
        }

        // 消费 ui_ask 请求，转成 AgentEvent::AskQuestion（含 oneshot sender）
        loop {
            match ui_ask_rx.try_recv() {
                Ok(req) => {
                    state.apply_agent_event(AgentEvent::AskQuestion {
                        question: req.question,
                        options: req.options,
                        response_tx: req.response_tx,
                    });
                }
                Err(_) => break,
            }
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            let ev = event::read()?;
            match ev {
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        state.scroll_offset = state.scroll_offset.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        state.scroll_offset = state.scroll_offset.saturating_sub(3);
                    }
                    _ => {}
                },
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // ① AskQuestion 对话框优先拦截全部按键
                    if state.ask_question_dialog.is_some() {
                        match key.code {
                            KeyCode::Up => {
                                if let Some(dlg) = &mut state.ask_question_dialog {
                                    if dlg.selected > 0 {
                                        dlg.selected -= 1;
                                    }
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dlg) = &mut state.ask_question_dialog {
                                    if dlg.selected + 1 < dlg.options.len() {
                                        dlg.selected += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(dlg) = state.ask_question_dialog.take() {
                                    let _ = dlg.response_tx.send(Some(dlg.selected));
                                }
                            }
                            KeyCode::Esc => {
                                if let Some(dlg) = state.ask_question_dialog.take() {
                                    let _ = dlg.response_tx.send(None);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ② 权限对话框拦截（分级授权）
                    if state.permission_dialog.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    false,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("allowed {}", dlg.tool_name),
                                ));
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.session_allowed.insert(dlg.tool_name.clone());
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    false,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("allowed {} (session)", dlg.tool_name),
                                ));
                            }
                            KeyCode::Char('p') | KeyCode::Char('P') => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    false,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("always allowed {}", dlg.tool_name),
                                ));
                            }
                            _ => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    true,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("denied {}", dlg.tool_name),
                                ));
                            }
                        }
                        continue;
                    }

                    // ③ Slash 补全列表拦截 ↑/↓/Tab/Esc
                    if !state.slash_completions.is_empty() {
                        match key.code {
                            KeyCode::Up => {
                                if state.slash_selected > 0 {
                                    state.slash_selected -= 1;
                                }
                                continue;
                            }
                            KeyCode::Down => {
                                if state.slash_selected + 1 < state.slash_completions.len() {
                                    state.slash_selected += 1;
                                }
                                continue;
                            }
                            KeyCode::Tab => {
                                let selected =
                                    state.slash_completions[state.slash_selected].0.clone();
                                input = InputBox::new();
                                for c in selected.chars() {
                                    input.insert_char(c);
                                }
                                input.insert_char(' ');
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                continue;
                            }
                            KeyCode::Esc => {
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                continue;
                            }
                            _ => {} // 其他键落穿到下方正常处理
                        }
                    }

                    // Ctrl+D → 立即退出
                    if is_quit(&key) {
                        state.should_quit = true;
                        continue;
                    }

                    // Ctrl+C → 中断 Agent 或二次确认退出
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if state.is_thinking {
                            state.interrupt();
                        } else if state.ctrl_c_pressed {
                            state.should_quit = true;
                        } else {
                            state.ctrl_c_pressed = true;
                            state.last_ctrl_c = Some(Instant::now());
                        }
                        continue;
                    }

                    // ESC → 中断 Agent
                    if key.code == KeyCode::Esc {
                        if state.is_thinking {
                            state.interrupt();
                        }
                        continue;
                    }

                    // Shift+Tab → 循环切换模式
                    if key.code == KeyCode::BackTab {
                        let new_mode = cycle_mode(&state.mode);
                        let label = new_mode.label();
                        *shared_mode.lock().await = new_mode.clone();
                        state.mode = new_mode;
                        state
                            .messages
                            .push(ChatMessage::system(format!("已切换至 {} 模式", label)));
                        continue;
                    }

                    // 其他按键重置 Ctrl+C 计数
                    state.ctrl_c_pressed = false;
                    state.last_ctrl_c = None;

                    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
                        input.insert_newline();
                        state.slash_completions.clear();
                    } else if key.code == KeyCode::Enter && !state.is_thinking {
                        if !input.is_empty() {
                            let text = input.take();
                            state.scroll_offset = 0;
                            state.slash_completions.clear();
                            state.history_idx = None;

                            let trimmed = text.trim().to_string();

                            // ── ! Bash 内联执行 ──────────────────────────────
                            if let Some(cmd_str) = trimmed.strip_prefix('!') {
                                let cmd_str = cmd_str.trim().to_string();
                                state.push_user(text.clone());
                                state.input_history.push(text);
                                let tx = agent_tx.clone();
                                let start = Instant::now();
                                tokio::spawn(async move {
                                    let elapsed;
                                    let (output, exit_code) =
                                        match tokio::process::Command::new("sh")
                                            .arg("-c")
                                            .arg(&cmd_str)
                                            .output()
                                            .await
                                        {
                                            Ok(out) => {
                                                elapsed = start.elapsed().as_secs_f64();
                                                let mut combined = String::new();
                                                let so = String::from_utf8_lossy(&out.stdout);
                                                let se = String::from_utf8_lossy(&out.stderr);
                                                if !so.is_empty() {
                                                    combined.push_str(&so);
                                                }
                                                if !se.is_empty() {
                                                    if !combined.is_empty() {
                                                        combined.push('\n');
                                                    }
                                                    combined.push_str(&se);
                                                }
                                                (combined, out.status.code().unwrap_or(-1))
                                            }
                                            Err(e) => {
                                                elapsed = start.elapsed().as_secs_f64();
                                                (format!("执行失败: {e}"), -1)
                                            }
                                        };
                                    let _ = tx
                                        .send(AgentEvent::BashResult {
                                            output,
                                            exit_code,
                                            elapsed_secs: elapsed,
                                        })
                                        .await;
                                });
                                continue;
                            }

                            // ── /mode 命令：运行时切换模式 ───────────────────
                            if let Some(args) = trimmed.strip_prefix("/mode") {
                                let args = args.trim();
                                let new_mode = match args {
                                    "plan" => Some(AgentMode::Plan),
                                    "bypass" => Some(AgentMode::Bypass),
                                    "normal" | "" => Some(AgentMode::Normal),
                                    _ => None,
                                };
                                match new_mode {
                                    Some(m) => {
                                        let label = m.label();
                                        *shared_mode.lock().await = m.clone();
                                        state.mode = m;
                                        state.messages.push(ChatMessage::system(format!(
                                            "已切换至 {} 模式",
                                            label
                                        )));
                                    }
                                    None => {
                                        state.messages.push(ChatMessage::system(format!(
                                            "未知模式 '{args}'。可选：normal / plan / bypass"
                                        )));
                                    }
                                }
                                state.input_history.push(trimmed);
                                continue;
                            }

                            // ── 其他 slash 命令 ─────────────────────────────
                            let estimated = {
                                let sess = session.lock().await;
                                wyj_core::estimate_tokens(&sess.messages)
                            };
                            let cmd_ctx = CommandContext {
                                cwd: cwd.clone(),
                                model: state.model_name.clone(),
                                input_tokens: state.total_input_tokens,
                                output_tokens: state.total_output_tokens,
                                context_window,
                                estimated_tokens: estimated,
                                home_dir: std::env::var("HOME")
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_default(),
                            };
                            if let Some(result) = cmd_registry.dispatch(&trimmed, &cmd_ctx).await {
                                match result {
                                    Ok(CommandResult::Output(out)) => {
                                        state.messages.push(ChatMessage::assistant(out));
                                    }
                                    Ok(CommandResult::ClearHistory) => {
                                        state.messages.clear();
                                        state.total_input_tokens = 0;
                                        state.total_output_tokens = 0;
                                        let mut sess = session.lock().await;
                                        *sess = Session::new();
                                        state.messages.push(ChatMessage::assistant(
                                            "对话已清空。".to_string(),
                                        ));
                                    }
                                    Ok(CommandResult::CompactHistory) => {
                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let mut sess = session.lock().await;
                                        match agent_c.compact_context(&mut sess).await {
                                            Ok(r) if r.messages_removed > 0 => {
                                                state.messages.push(ChatMessage::assistant(
                                                    format!(
                                                        "已压缩：移除 {} 条消息，节省约 {} tokens",
                                                        r.messages_removed, r.tokens_saved_estimate
                                                    ),
                                                ));
                                            }
                                            Ok(_) => {
                                                state.messages.push(ChatMessage::assistant(
                                                    "上下文较短，无需压缩。".to_string(),
                                                ));
                                            }
                                            Err(e) => {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    format!("[压缩失败] {e}"),
                                                ));
                                            }
                                        }
                                    }
                                    Ok(CommandResult::SetModel(m)) => match rebuild_fn(&m) {
                                        Ok(new_agent) => {
                                            let tool_tx2 = agent_tx.clone();
                                            let todo_cb = todo_store.clone();
                                            let new_agent = new_agent.with_tool_callback(
                                                move |event: ToolEvent| match event {
                                                    ToolEvent::Start { id, name, input } => {
                                                        let _ = tool_tx2.try_send(
                                                            AgentEvent::ToolStart {
                                                                id,
                                                                name,
                                                                input_json: input,
                                                            },
                                                        );
                                                    }
                                                    ToolEvent::End {
                                                        id,
                                                        name,
                                                        is_error,
                                                        elapsed_secs,
                                                        output,
                                                    } => {
                                                        if name == "TodoWrite" && !is_error {
                                                            if let Ok(store) = todo_cb.lock() {
                                                                let items = store.items.clone();
                                                                let _ = tool_tx2.try_send(
                                                                    AgentEvent::TodoUpdate(items),
                                                                );
                                                            }
                                                        }
                                                        let _ = tool_tx2.try_send(
                                                            AgentEvent::ToolEnd {
                                                                id,
                                                                output,
                                                                is_error,
                                                                elapsed_secs,
                                                            },
                                                        );
                                                    }
                                                },
                                            );
                                            *shared_agent.write().unwrap() = Arc::new(new_agent);
                                            state.model_name = m.clone();
                                            state.messages.push(ChatMessage::assistant(format!(
                                                "模型已切换至 {m}"
                                            )));
                                        }
                                        Err(e) => {
                                            state.messages.push(ChatMessage::assistant_err(
                                                format!("[切换失败] {e}"),
                                            ));
                                        }
                                    },
                                    Ok(CommandResult::RunPrompt(prompt)) => {
                                        // Skill 展开后的 prompt → 当作用户消息发给 agent
                                        state.push_user(prompt.clone());
                                        state.is_thinking = true;
                                        state.spinner_frame = 0;

                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let session_c = session.clone();
                                        let tx = agent_tx.clone();
                                        let ctx_cwd = cwd.clone();
                                        let mode_arc = shared_mode.clone();
                                        let ui_ask_tx_clone = ui_ask_tx.clone();

                                        let handle = tokio::spawn(async move {
                                            let mut sess = session_c.lock().await;
                                            sess.push_user(prompt);
                                            let current_mode = mode_arc.lock().await.clone();
                                            let mut ctx = ToolCtx::new(&ctx_cwd);
                                            ctx.permission_mode = mode_to_permission(&current_mode);
                                            ctx.ui_ask_tx = Some(ui_ask_tx_clone);
                                            let tx2 = tx.clone();
                                            let mut on_text = move |d: &str| {
                                                let _ = tx2
                                                    .try_send(AgentEvent::TextDelta(d.to_string()));
                                            };
                                            match agent_c
                                                .run_turn(&mut sess, &ctx, &mut on_text)
                                                .await
                                            {
                                                Ok(_) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Usage {
                                                            input: sess.total_input_tokens,
                                                            output: sess.total_output_tokens,
                                                        })
                                                        .await;
                                                    let _ = tx.send(AgentEvent::TurnDone).await;
                                                }
                                                Err(e) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Error(e.to_string()))
                                                        .await;
                                                }
                                            }
                                        });
                                        state.current_task = Some(handle.abort_handle());
                                    }
                                    Ok(CommandResult::Quit) | Ok(CommandResult::None) => {
                                        state.should_quit = true;
                                    }
                                    Err(e) => {
                                        state.messages.push(ChatMessage::assistant_err(format!(
                                            "[命令错误] {e}"
                                        )));
                                    }
                                }
                                state.input_history.push(trimmed);
                            } else {
                                // ── 普通消息 → 发给 agent ───────────────────
                                state.push_user(text.clone());
                                state.input_history.push(text.clone());
                                state.is_thinking = true;
                                state.spinner_frame = 0;

                                let agent_c = shared_agent.read().unwrap().clone();
                                let session_c = session.clone();
                                let tx = agent_tx.clone();
                                let ctx_cwd = cwd.clone();
                                let mode_arc = shared_mode.clone();
                                let ui_ask_tx_clone = ui_ask_tx.clone();

                                let handle = tokio::spawn(async move {
                                    let mut sess = session_c.lock().await;
                                    sess.push_user(text);
                                    let current_mode = mode_arc.lock().await.clone();
                                    let mut ctx = ToolCtx::new(&ctx_cwd);
                                    ctx.permission_mode = mode_to_permission(&current_mode);
                                    ctx.ui_ask_tx = Some(ui_ask_tx_clone);
                                    let tx2 = tx.clone();
                                    let mut on_text = move |d: &str| {
                                        let _ = tx2.try_send(AgentEvent::TextDelta(d.to_string()));
                                    };
                                    match agent_c.run_turn(&mut sess, &ctx, &mut on_text).await {
                                        Ok(_) => {
                                            let _ = tx
                                                .send(AgentEvent::Usage {
                                                    input: sess.total_input_tokens,
                                                    output: sess.total_output_tokens,
                                                })
                                                .await;
                                            let _ = tx.send(AgentEvent::TurnDone).await;
                                        }
                                        Err(e) => {
                                            let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                                        }
                                    }
                                });
                                state.current_task = Some(handle.abort_handle());
                            }
                        }
                    } else if key.code == KeyCode::PageUp {
                        state.scroll_offset = state.scroll_offset.saturating_add(5);
                    } else if key.code == KeyCode::PageDown {
                        state.scroll_offset = state.scroll_offset.saturating_sub(5);
                    } else if key.code == KeyCode::Up && !state.is_thinking {
                        // 输入历史导航（仅在无补全列表时）
                        if state.slash_completions.is_empty() {
                            let hist_len = state.input_history.len();
                            if hist_len > 0 {
                                let new_idx = match state.history_idx {
                                    None => hist_len - 1,
                                    Some(i) => i.saturating_sub(1),
                                };
                                state.history_idx = Some(new_idx);
                                let recalled = state.input_history[new_idx].clone();
                                input = InputBox::new();
                                for c in recalled.chars() {
                                    if c == '\n' {
                                        input.insert_newline();
                                    } else {
                                        input.insert_char(c);
                                    }
                                }
                            }
                        }
                    } else if key.code == KeyCode::Down && !state.is_thinking {
                        if state.slash_completions.is_empty() {
                            if let Some(idx) = state.history_idx {
                                if idx + 1 < state.input_history.len() {
                                    let new_idx = idx + 1;
                                    state.history_idx = Some(new_idx);
                                    let recalled = state.input_history[new_idx].clone();
                                    input = InputBox::new();
                                    for c in recalled.chars() {
                                        if c == '\n' {
                                            input.insert_newline();
                                        } else {
                                            input.insert_char(c);
                                        }
                                    }
                                } else {
                                    // 超出历史末尾，清空退出历史模式
                                    state.history_idx = None;
                                    input = InputBox::new();
                                }
                            }
                        }
                    } else if key.code == KeyCode::Backspace {
                        if key.modifiers.contains(KeyModifiers::ALT) {
                            // Alt+Backspace — 删词
                            input.delete_word_backward();
                        } else {
                            input.backspace();
                        }
                        update_slash_completions(&mut state, &input, &cmd_registry);
                    } else if key.code == KeyCode::Delete {
                        input.delete_char_forward();
                        update_slash_completions(&mut state, &input, &cmd_registry);
                    } else if key.code == KeyCode::Home {
                        input.move_to_start_of_line();
                    } else if key.code == KeyCode::End {
                        input.move_to_end_of_line();
                    } else if key.code == KeyCode::Left {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            || key.modifiers.contains(KeyModifiers::ALT)
                        {
                            input.move_word_backward();
                        } else {
                            input.move_left();
                        }
                    } else if key.code == KeyCode::Right {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            || key.modifiers.contains(KeyModifiers::ALT)
                        {
                            input.move_word_forward();
                        } else {
                            input.move_right();
                        }
                    } else if let KeyCode::Char(c) = key.code {
                        // Ctrl 组合键
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                'a' => input.move_to_start_of_line(),
                                'e' => input.move_to_end_of_line(),
                                'k' => {
                                    input.kill_to_end_of_line();
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                }
                                'u' => {
                                    input.kill_to_start_of_line();
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                }
                                'w' => {
                                    input.delete_word_backward();
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                }
                                'l' => {
                                    state.scroll_offset = 0;
                                }
                                'o' => {
                                    // Ctrl+O — 展开/折叠最后一条工具结果（对齐 Claude Code）
                                    if let Some(last_tool) = state
                                        .messages
                                        .iter_mut()
                                        .rev()
                                        .find(|m| matches!(m.role, MessageRole::ToolResult))
                                    {
                                        last_tool.expanded = !last_tool.expanded;
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            state.history_idx = None;
                            input.insert_char(c);
                            update_slash_completions(&mut state, &input, &cmd_registry);
                        }
                    }
                }
                _ => {}
            }
        }

        if state.should_quit {
            break;
        }
    }

    // 退出时保存会话历史
    if let Some(hs) = history_store {
        let _ = hs.append(&HistoryEntry {
            timestamp: now_iso(),
            session_id,
            input_tokens: state.total_input_tokens,
            output_tokens: state.total_output_tokens,
            turns: state.turns,
            cwd: cwd.display().to_string(),
        });
    }

    Ok(())
}
