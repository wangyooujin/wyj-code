//! 对话渲染与布局

use crate::app::{AppState, AskQuestionDialog, MessageRole, PermissionDialog, SessionPickerState};
use crate::input::InputBox;
use crate::markdown::render_markdown;
use crate::theme::Theme;
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use wyj_config::AgentMode;
use wyj_tools::todo::TodoStatus;

/// 截断超长字符串（按字符数）
fn truncate_line(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = chars[..max_chars].iter().collect();
        format!("{truncated}…")
    }
}

/// Spinner 动画帧（来回扫动效果）
pub const SPINNER_FRAMES: &[char] = &['·', '✢', '✳', '✶', '✻', '✽', '✽', '✻', '✶', '✳', '✢', '·'];

pub fn draw(f: &mut Frame, state: &AppState, input: &InputBox) {
    let area = f.area();
    let input_height = (input.display_lines().len() as u16 + 2).max(3).min(10);

    // 补全列表高度
    let completion_height = if state.slash_completions.is_empty() {
        0u16
    } else {
        (state.slash_completions.len() as u16 + 2).min(8)
    };

    // 底部面板高度：AskQuestion 优先，否则 TaskList，否则 0
    let (panel_height, panel_kind) = bottom_panel_size(state, area.height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(panel_height),
            Constraint::Length(completion_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_chat(f, state, chunks[0]);
    match panel_kind {
        BottomPanel::None => {}
        BottomPanel::AskQuestion => {
            if let Some(dlg) = &state.ask_question_dialog {
                draw_ask_question_panel(f, dlg, chunks[1]);
            }
        }
        BottomPanel::TodoList => {
            if let Some(items) = &state.current_todos {
                draw_todo_panel(f, items, state.spinner_frame, chunks[1]);
            }
        }
    }
    if !state.slash_completions.is_empty() {
        draw_slash_completions(f, state, chunks[2]);
    }
    draw_input(f, state, input, chunks[3]);
    draw_status(f, state, chunks[4]);

    // 权限对话框仍以浮层叠加（后渲染的在前）
    if let Some(dlg) = &state.permission_dialog {
        draw_permission_dialog(f, dlg, area);
    }

    // 会话选择器叠加在最顶层
    if let Some(picker) = &state.session_picker {
        draw_session_picker(f, picker, area);
    }
}

/// 底部面板类型与高度
enum BottomPanel {
    None,
    AskQuestion,
    TodoList,
}

fn bottom_panel_size(state: &AppState, area_height: u16) -> (u16, BottomPanel) {
    if let Some(dlg) = &state.ask_question_dialog {
        let h = (dlg.options.len() as u16 + 6).min(area_height);
        return (h, BottomPanel::AskQuestion);
    }
    if let Some(items) = &state.current_todos {
        if !items.is_empty() {
            let h = (items.len() as u16 + 2).min(area_height);
            return (h, BottomPanel::TodoList);
        }
    }
    (0, BottomPanel::None)
}

// ─── 对话区 ──────────────────────────────────────────────────────────────────

fn draw_chat(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::border())
        .title(Span::styled(" wyj-code ", Theme::dim()));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_content_width = inner.width.saturating_sub(4) as usize;
    let sep_width = inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = vec![];
    let mut is_first_user = true;

    for msg in &state.messages {
        match msg.role {
            MessageRole::User => {
                if !is_first_user {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "─".repeat(sep_width.min(60)),
                        Theme::dim(),
                    )));
                }
                is_first_user = false;

                let mut content_lines = msg.content.lines();
                let first_line = content_lines.next().unwrap_or("");
                lines.push(Line::from(vec![
                    Span::styled("❯ ", Theme::user_prefix()),
                    Span::styled(
                        truncate_line(first_line, max_content_width),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                for l in content_lines {
                    lines.push(Line::from(Span::raw(format!(
                        "  {}",
                        truncate_line(l, max_content_width)
                    ))));
                }
                lines.push(Line::from(""));
            }

            MessageRole::Assistant => {
                if msg.is_error {
                    for l in msg.content.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  ✗ {}", truncate_line(l, max_content_width)),
                            Theme::error(),
                        )));
                    }
                } else {
                    render_markdown(&mut lines, &msg.content, max_content_width);
                }
                lines.push(Line::from(""));
            }

            // ─── ⏺ ToolName(arg)  ────────────────────────────────────────
            MessageRole::ToolCall => {
                lines.push(Line::from(vec![
                    Span::styled("  ⏺ ", Theme::tool_call()),
                    Span::styled(
                        truncate_line(&msg.content, max_content_width.saturating_sub(4)),
                        Theme::tool_call(),
                    ),
                ]));
            }

            // ─── ⎿  summary · elapsed  ────────────────────────────────────
            MessageRole::ToolResult => {
                let elapsed_str = msg
                    .elapsed_secs
                    .filter(|&s| s > 0.0)
                    .map(|s| format!("  {s:.1}s"))
                    .unwrap_or_default();

                let (summary_style, prefix) = if msg.is_error {
                    (Theme::error(), "✗ ")
                } else {
                    (Theme::dim(), "")
                };

                let summary = if msg.display_summary.is_empty() {
                    msg.content
                        .lines()
                        .next()
                        .unwrap_or("done")
                        .trim()
                        .to_string()
                } else {
                    msg.display_summary.clone()
                };

                lines.push(Line::from(vec![
                    Span::styled("    ⎿  ", Theme::dim()),
                    Span::styled(
                        format!(
                            "{prefix}{}",
                            truncate_line(&summary, max_content_width.saturating_sub(12))
                        ),
                        summary_style,
                    ),
                    Span::styled(elapsed_str, Theme::dim()),
                ]));

                // 展开/折叠详细内容（ctrl+o）
                if !msg.content.is_empty() && msg.content != summary {
                    let content_lines: Vec<&str> = msg.content.lines().collect();
                    let line_style = if msg.is_error {
                        Theme::error()
                    } else {
                        Theme::tool_result()
                    };

                    if msg.expanded {
                        lines.push(Line::from(Span::styled(
                            format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
                            Theme::dim(),
                        )));
                        for l in &content_lines {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "       {}",
                                    truncate_line(l, max_content_width.saturating_sub(8))
                                ),
                                line_style,
                            )));
                        }
                        lines.push(Line::from(Span::styled(
                            "       [ctrl+o to collapse]".to_string(),
                            Theme::dim(),
                        )));
                    } else if content_lines.len() > 3 {
                        lines.push(Line::from(Span::styled(
                            format!("       …({} lines, ctrl+o to expand)", content_lines.len()),
                            Theme::dim(),
                        )));
                    }
                }
            }

            MessageRole::BashOutput => {
                let (icon, style) = if msg.is_error {
                    ("✗", Theme::error())
                } else {
                    ("$", Style::default().fg(Color::Green))
                };
                let elapsed_str = msg
                    .elapsed_secs
                    .filter(|&s| s > 0.0)
                    .map(|s| format!(" · {s:.1}s"))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} bash", icon), style),
                    Span::styled(elapsed_str, Theme::dim()),
                ]));
                for l in msg.content.lines().take(20) {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    {}",
                            truncate_line(l, max_content_width.saturating_sub(2))
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                let total = msg.content.lines().count();
                if total > 20 {
                    lines.push(Line::from(Span::styled(
                        format!("    …（共 {} 行）", total),
                        Theme::dim(),
                    )));
                }
            }

            MessageRole::System => {
                lines.push(Line::from(vec![
                    Span::styled("  ⚙ ", Style::default().fg(Color::Cyan)),
                    Span::styled(msg.content.clone(), Style::default().fg(Color::Cyan)),
                ]));
                lines.push(Line::from(""));
            }
        }
    }

    // 流式文本（实时输出中）
    if !state.streaming_buf.is_empty() {
        render_markdown(&mut lines, &state.streaming_buf, max_content_width);
    }

    // is_thinking 时在底部显示 spinner 行
    if state.is_thinking {
        let frame = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("{frame} "), Style::default().fg(Theme::CLAUDE)),
            Span::styled("思考中…", Theme::dim()),
        ]));
    }

    let text = Text::from(lines);
    let total_lines = text.lines.len() as u16;
    let visible_height = inner.height;

    let scroll = if total_lines > visible_height {
        let max_scroll = total_lines - visible_height;
        max_scroll.saturating_sub(state.scroll_offset)
    } else {
        0
    };

    let para = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, inner);
}

/// 底部固定面板：任务列表
fn draw_todo_panel(
    f: &mut Frame,
    items: &[wyj_tools::todo::TodoItem],
    spinner_frame: usize,
    area: Rect,
) {
    let total = items.len();
    let done = items
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let title = format!(" 任务列表 [{done}/{total}] ");

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::border())
        .title(Span::styled(
            title,
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_content_width = inner.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = vec![];

    for (i, item) in items.iter().enumerate() {
        let (icon, item_style) = match item.status {
            TodoStatus::Pending => ("○".to_string(), Style::default().fg(Color::DarkGray)),
            TodoStatus::InProgress => {
                let frame = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
                (
                    frame.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            }
            TodoStatus::Completed => (
                "✓".to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        };

        let prio_str = item
            .priority
            .as_deref()
            .map(|p| format!("[{p}] "))
            .unwrap_or_default();
        let idx_str = format!("{}/{}", i + 1, total);
        let content = truncate_line(
            &format!("{prio_str}{}", item.content),
            max_content_width.saturating_sub(10),
        );

        lines.push(Line::from(vec![
            Span::styled(format!("[{idx_str}] "), Theme::dim()),
            Span::styled(format!("{icon} "), item_style),
            Span::styled(content, item_style),
        ]));
    }

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

// ─── 输入框 ──────────────────────────────────────────────────────────────────

fn draw_input(f: &mut Frame, state: &AppState, input: &InputBox, area: Rect) {
    let (title_content, title_style) = if state.is_thinking {
        let frame = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
        (
            format!(" {frame} esc to interrupt · ctrl+c to cancel "),
            Style::default().fg(Theme::CLAUDE),
        )
    } else {
        match &state.mode {
            AgentMode::Plan => (
                " [plan] Enter to send · Shift+Tab to switch mode ".to_string(),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            AgentMode::Bypass => (
                " [bypass] Enter to send · Shift+Tab to switch mode ".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            AgentMode::Normal => (
                " Enter to send · Shift+Enter newline · / commands · ! bash · Shift+Tab mode "
                    .to_string(),
                Theme::dim(),
            ),
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(match &state.mode {
            AgentMode::Plan if !state.is_thinking => Style::default().fg(Color::Blue),
            AgentMode::Bypass if !state.is_thinking => Style::default().fg(Color::Yellow),
            _ => Theme::border(),
        })
        .title(Span::styled(title_content, title_style));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.is_thinking {
        // is_thinking 时不设置光标位置，ratatui 会自动隐藏终端光标
        return;
    }

    let lines: Vec<Line> = input
        .display_lines()
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();

    let para = Paragraph::new(Text::from(lines)).style(Theme::input_box());
    f.render_widget(para, inner);

    // 设置真正的终端光标位置（同时解决：光标不可见 + IME 候选框定位）
    let cursor_x =
        (inner.x + input.cursor_display_col() as u16).min(inner.x + inner.width.saturating_sub(1));
    let cursor_y =
        (inner.y + input.cursor_row as u16).min(inner.y + inner.height.saturating_sub(1));
    f.set_cursor_position(Position::new(cursor_x, cursor_y));
}

// ─── Slash 命令补全下拉 ───────────────────────────────────────────────────────

fn draw_slash_completions(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(
            " / 命令 & Skill ",
            Style::default().fg(Color::Blue),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_show = inner.height as usize;
    let items = &state.slash_completions;
    let selected = state.slash_selected;

    // 滚动窗口：保持 selected 可见
    let start = if selected >= max_show {
        selected - max_show + 1
    } else {
        0
    };

    // 固定名称列宽（所有候选中最长的 name，上限 28 字符）
    let name_col_w = items
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(6)
        .min(28);
    let desc_budget = (inner.width as usize).saturating_sub(name_col_w + 4);

    let lines: Vec<Line<'static>> = items
        .iter()
        .skip(start)
        .take(max_show)
        .enumerate()
        .map(|(i, (name, desc))| {
            let real_idx = start + i;
            let name_pad = format!(" {:width$}", name, width = name_col_w);
            let desc_str = format!("  {}", truncate_chars(desc, desc_budget));
            if real_idx == selected {
                Line::from(vec![
                    Span::styled(
                        name_pad,
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        desc_str,
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::Rgb(180, 200, 255)),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(name_pad, Style::default().fg(Color::White)),
                    Span::styled(desc_str, Theme::dim()),
                ])
            }
        })
        .collect();

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

// ─── 状态栏 ──────────────────────────────────────────────────────────────────

fn draw_status(f: &mut Frame, state: &AppState, area: Rect) {
    let (used, total) = (state.total_input_tokens, state.context_window);
    let pct = if total > 0 {
        (used as f64 / total as f64).min(1.0)
    } else {
        0.0
    };
    let bar_width = 8usize;
    let filled = ((pct * bar_width as f64).round() as usize).min(bar_width);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
    let pct_int = (pct * 100.0).round() as u32;

    let progress_style = if pct >= 0.90 {
        Theme::progress_danger()
    } else if pct >= 0.70 {
        Theme::progress_warn()
    } else {
        Theme::progress_normal()
    };

    let cwd_str = {
        let full = state.cwd.display().to_string();
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() && full.starts_with(&home) {
            format!("~{}", &full[home.len()..])
        } else {
            full
        }
    };

    let (right_help, right_style) = if state.ctrl_c_pressed {
        (
            "ctrl+c again to exit",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("ctrl+d or ctrl+c twice to exit  /help", Theme::dim())
    };

    let mode_span = match &state.mode {
        AgentMode::Plan => Some(Span::styled(
            " [plan] ",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )),
        AgentMode::Bypass => Some(Span::styled(
            " [bypass] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        AgentMode::Normal => None,
    };

    let mode_str = match &state.mode {
        AgentMode::Plan => " [plan]",
        AgentMode::Bypass => " [bypass]",
        AgentMode::Normal => "",
    };
    let left_text = format!(
        " ◆ {}{} · [{}] {}% · {}",
        state.model_name, mode_str, bar, pct_int, cwd_str
    );
    let right_len = right_help.chars().count();
    let pad = (area.width as usize).saturating_sub(left_text.chars().count() + right_len + 1);

    let mut spans = vec![
        Span::styled(
            " ◆ ",
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(state.model_name.clone(), Theme::dim()),
    ];
    if let Some(ms) = mode_span {
        spans.push(ms);
    }
    spans.extend([
        Span::styled(" · [".to_string(), Theme::dim()),
        Span::styled(bar, progress_style),
        Span::styled(format!("] {}% · {}", pct_int, cwd_str), Theme::dim()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right_help, right_style),
        Span::raw(" "),
    ]);

    let line = Line::from(spans);
    let para = Paragraph::new(line).style(Theme::status_bar());
    f.render_widget(para, area);
}

// ─── 权限对话框（分级授权） ────────────────────────────────────────────────────

fn draw_permission_dialog(f: &mut Frame, dlg: &PermissionDialog, area: Rect) {
    let width = (area.width * 3 / 4).max(40).min(area.width);
    let height = 11u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::permission_dialog())
        .title(Span::styled(" ⚑ 权限确认 ", Theme::permission_dialog()));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let preview = truncate_chars(&dlg.input_preview, (inner.width as usize * 3).max(80));

    let lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("工具: ", Theme::dim()),
            Span::styled(dlg.tool_name.clone(), Theme::permission_dialog()),
        ]),
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Theme::border(),
        )),
        Line::from(Span::raw(preview)),
        Line::from(""),
        Line::from(Span::styled(
            "  [y] 本次允许  [s] Session 允许  [p] 永久允许  [n] 拒绝",
            Theme::highlight(),
        )),
    ];

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

// ─── AskQuestion 底部面板 ─────────────────────────────────────────────────────

fn draw_ask_question_panel(f: &mut Frame, dlg: &AskQuestionDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            " ◆ Agent 提问 ",
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_w = inner.width as usize;

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            truncate_line(&dlg.question, max_w),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("─".repeat(max_w), Theme::border())),
    ];

    for (i, opt) in dlg.options.iter().enumerate() {
        if i == dlg.selected {
            lines.push(Line::from(Span::styled(
                format!("  ▶ {}", truncate_line(opt, max_w.saturating_sub(4))),
                Style::default()
                    .fg(Theme::CLAUDE)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("    {}", truncate_line(opt, max_w.saturating_sub(4))),
                Style::default().fg(Color::White),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ 选择  Enter 确认  Esc 取消",
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

// ─── 会话选择器 ───────────────────────────────────────────────────────────────

fn draw_session_picker(f: &mut Frame, picker: &SessionPickerState, area: Rect) {
    let n_sessions = picker.sessions.len();
    // 显示项：1条"新建会话" + 1条分割线 + n条历史 + 1条分割线 + 1条提示 = n+4
    let height = ((n_sessions as u16 + 4).max(5)).min(area.height.saturating_sub(4));
    let width = (area.width * 4 / 5).min(92).max(50);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" 会话列表 ({n_sessions}) "),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let w = inner.width as usize;
    let home = std::env::var("HOME").unwrap_or_default();

    let mut lines: Vec<Line<'static>> = Vec::new();

    // "新建会话" 条目（selected == 0 时高亮）
    if picker.selected == 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▶ {:<w$}", "+ 新建会话", w = w.saturating_sub(4)),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("    {:<w$}", "+ 新建会话", w = w.saturating_sub(4)),
            Style::default().fg(Color::Green),
        )));
    }

    if !picker.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "─".repeat(w),
            Theme::border(),
        )));

        for (i, meta) in picker.sessions.iter().enumerate() {
            let selected = picker.selected == i + 1;

            // cwd 缩短显示（取最后一级目录名）
            let cwd_short = if !home.is_empty() && meta.cwd.starts_with(&home) {
                format!("~{}", &meta.cwd[home.len()..])
            } else {
                meta.cwd.clone()
            };
            let cwd_last = cwd_short
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(&cwd_short)
                .to_string();

            let time_str = format_relative_time(&meta.timestamp);
            let right = format!("  {}  {}  {}轮", time_str, cwd_last, meta.turns);
            let right_w = right.chars().count();
            let title_w = w.saturating_sub(right_w + 4);
            let title = truncate_chars(&meta.title, title_w);

            let line_str = format!("    {:<tw$}{}", title, right, tw = title_w);
            let line_str: String = line_str.chars().take(w).collect();

            if selected {
                lines.push(Line::from(Span::styled(
                    line_str,
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )));
            } else {
                lines.push(Line::from(Span::raw(line_str)));
            }
        }
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    lines.push(Line::from(Span::styled(
        "  ↑↓ 导航  Enter 选择  Esc 取消",
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

/// 将 ISO 8601 时间戳格式化为相对时间字符串
fn format_relative_time(timestamp: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let ts = parse_iso_to_secs(timestamp).unwrap_or(now);
    let diff = now.saturating_sub(ts);

    if diff < 60 {
        "刚才".to_string()
    } else if diff < 3600 {
        format!("{}分钟前", diff / 60)
    } else if diff < 86400 {
        format!("{}小时前", diff / 3600)
    } else if diff < 7 * 86400 {
        format!("{}天前", diff / 86400)
    } else {
        timestamp.get(..10).unwrap_or(timestamp).to_string()
    }
}

/// 粗略地将 ISO 8601 字符串（"2024-06-29T15:30:45Z"）解析为 Unix 秒
fn parse_iso_to_secs(s: &str) -> Option<u64> {
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let year: u64 = dp.next()?.parse().ok()?;
    let month: u64 = dp.next()?.parse().ok()?;
    let day: u64 = dp.next()?.parse().ok()?;
    let mut tp = time.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let m: u64 = tp.next()?.parse().ok()?;
    let sec: u64 = tp.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    // 粗略计算（不考虑闰年/闰秒，仅用于相对时间展示）
    let y_days = (year.saturating_sub(1970)) * 365;
    let mo_days: u64 = [0u64, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334]
        .get((month as usize).saturating_sub(1))
        .copied()
        .unwrap_or(0);
    Some((y_days + mo_days + day.saturating_sub(1)) * 86400 + h * 3600 + m * 60 + sec)
}
