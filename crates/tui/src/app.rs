//! TUI 应用主循环

use crate::event::{AgentEvent, is_quit};
use crate::input::InputBox;
use crate::render;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        KeyEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use wyj_commands::{builtin::standard_registry, CommandContext, CommandResult};
use wyj_core::{Agent, Session};
use wyj_tools::ToolCtx;

/// 消息角色
#[derive(Debug, Clone)]
pub enum MessageRole {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

/// 渲染用消息
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub is_error: bool,
}

/// 权限确认对话框状态
#[derive(Debug, Clone)]
pub struct PermissionDialog {
    pub tool_name: String,
    pub input_preview: String,
    pub tx_id: String,
}

/// 全局 UI 状态
pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub streaming_buf: String,
    pub is_thinking: bool,
    pub permission_dialog: Option<PermissionDialog>,
    pub scroll_offset: u16,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub cwd: PathBuf,
    pub should_quit: bool,
}

impl AppState {
    fn new(cwd: PathBuf) -> Self {
        Self {
            messages: vec![],
            streaming_buf: String::new(),
            is_thinking: false,
            permission_dialog: None,
            scroll_offset: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            cwd,
            should_quit: false,
        }
    }

    fn push_user(&mut self, text: String) {
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content: text,
            is_error: false,
        });
    }

    fn flush_streaming(&mut self) {
        if !self.streaming_buf.is_empty() {
            let text = std::mem::take(&mut self.streaming_buf);
            self.messages.push(ChatMessage {
                role: MessageRole::Assistant,
                content: text,
                is_error: false,
            });
        }
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(d) => self.streaming_buf.push_str(&d),
            AgentEvent::ToolStart { id: _, name } => {
                self.flush_streaming();
                self.messages.push(ChatMessage {
                    role: MessageRole::ToolCall,
                    content: format!("调用 {name}…"),
                    is_error: false,
                });
            }
            AgentEvent::ToolEnd { id: _, output, is_error } => {
                self.messages.push(ChatMessage {
                    role: MessageRole::ToolResult,
                    content: output,
                    is_error,
                });
            }
            AgentEvent::PermissionRequest { tool_name, input_preview, tx_id } => {
                self.permission_dialog = Some(PermissionDialog { tool_name, input_preview, tx_id });
            }
            AgentEvent::TurnDone => {
                self.flush_streaming();
                self.is_thinking = false;
            }
            AgentEvent::Error(e) => {
                self.flush_streaming();
                self.is_thinking = false;
                self.messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: format!("[错误] {e}"),
                    is_error: true,
                });
            }
            AgentEvent::Usage { input, output } => {
                self.total_input_tokens += input;
                self.total_output_tokens += output;
            }
        }
    }
}

pub async fn run_tui(agent: Agent, tool_ctx: ToolCtx, cwd: PathBuf) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = tui_main(&mut terminal, agent, tool_ctx, cwd).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

async fn tui_main<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
    agent: Agent,
    tool_ctx: ToolCtx,
    cwd: PathBuf,
) -> Result<()> {
    let mut state = AppState::new(cwd.clone());
    let mut input = InputBox::new();

    let agent = Arc::new(agent);
    let session = Arc::new(Mutex::new(Session::new()));
    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let cmd_registry = standard_registry();

    loop {
        terminal.draw(|f| render::draw(f, &state, &input))?;

        // 清空 agent 事件队列
        loop {
            match agent_rx.try_recv() {
                Ok(ev) => state.apply_agent_event(ev),
                Err(_) => break,
            }
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            let ev = event::read()?;
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // 权限对话框模式
                    if let Some(dlg) = state.permission_dialog.take() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                state.messages.push(ChatMessage {
                                    role: MessageRole::ToolResult,
                                    content: format!("[已允许 {}]", dlg.tool_name),
                                    is_error: false,
                                });
                            }
                            _ => {
                                state.messages.push(ChatMessage {
                                    role: MessageRole::ToolResult,
                                    content: format!("[已拒绝 {}]", dlg.tool_name),
                                    is_error: true,
                                });
                            }
                        }
                        continue;
                    }

                    if is_quit(&key) {
                        state.should_quit = true;
                    } else if key.code == KeyCode::Enter
                        && key.modifiers.contains(KeyModifiers::SHIFT)
                    {
                        input.insert_newline();
                    } else if key.code == KeyCode::Enter && !state.is_thinking {
                        if !input.is_empty() {
                            let text = input.take();
                            state.scroll_offset = 0;

                            // 先检测是否是 slash 命令
                            let cmd_ctx = CommandContext {
                                cwd: cwd.clone(),
                                model: "".to_string(),
                            };
                            if let Some(result) = cmd_registry.dispatch(&text, &cmd_ctx).await {
                                match result {
                                    Ok(CommandResult::Output(out)) => {
                                        state.messages.push(ChatMessage {
                                            role: MessageRole::Assistant,
                                            content: out,
                                            is_error: false,
                                        });
                                    }
                                    Ok(CommandResult::ClearHistory) => {
                                        state.messages.clear();
                                        let mut sess = session.lock().await;
                                        *sess = Session::new();
                                    }
                                    Ok(CommandResult::SetModel(m)) => {
                                        state.messages.push(ChatMessage {
                                            role: MessageRole::Assistant,
                                            content: format!("模型已切换: {m}（重启生效）"),
                                            is_error: false,
                                        });
                                    }
                                    Ok(CommandResult::Quit) | Ok(CommandResult::None) => {
                                        state.should_quit = true;
                                    }
                                    Err(e) => {
                                        state.messages.push(ChatMessage {
                                            role: MessageRole::Assistant,
                                            content: format!("[命令错误] {e}"),
                                            is_error: true,
                                        });
                                    }
                                }
                            } else {
                                // 普通消息 → 发给 agent
                                state.push_user(text.clone());
                                state.is_thinking = true;

                                let agent_c = agent.clone();
                                let session_c = session.clone();
                                let tx = agent_tx.clone();
                                let ctx_cwd = cwd.clone();

                                tokio::spawn(async move {
                                    let mut sess = session_c.lock().await;
                                    sess.push_user(text);
                                    let ctx = ToolCtx::new(&ctx_cwd);
                                    let tx2 = tx.clone();
                                    let mut on_text = move |d: &str| {
                                        let _ = tx2.try_send(AgentEvent::TextDelta(d.to_string()));
                                    };
                                    match agent_c.run_turn(&mut sess, &ctx, &mut on_text).await {
                                        Ok(_) => { let _ = tx.send(AgentEvent::TurnDone).await; }
                                        Err(e) => { let _ = tx.send(AgentEvent::Error(e.to_string())).await; }
                                    }
                                });
                            }
                        }
                    } else if key.code == KeyCode::PageUp {
                        state.scroll_offset = state.scroll_offset.saturating_add(5);
                    } else if key.code == KeyCode::PageDown {
                        state.scroll_offset = state.scroll_offset.saturating_sub(5);
                    } else if key.code == KeyCode::Backspace {
                        input.backspace();
                    } else if key.code == KeyCode::Left {
                        input.move_left();
                    } else if key.code == KeyCode::Right {
                        input.move_right();
                    } else if let KeyCode::Char(c) = key.code {
                        input.insert_char(c);
                    }
                }
                _ => {}
            }
        }

        if state.should_quit {
            break;
        }
    }
    Ok(())
}
