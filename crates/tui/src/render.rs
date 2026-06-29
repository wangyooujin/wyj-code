//! 对话渲染与布局

use crate::app::{AppState, ChatMessage, MessageRole, PermissionDialog};
use crate::input::InputBox;
use crate::theme::Theme;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, state: &AppState, input: &InputBox) {
    let area = f.area();

    // 状态栏高度 1 行，输入框最小 3 行，剩余给对话区
    let input_height = (input.display_lines().len() as u16 + 2).max(3).min(10);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_chat(f, state, chunks[0]);
    draw_input(f, state, input, chunks[1]);
    draw_status(f, state, chunks[2]);

    // 权限对话框覆盖在中央
    if let Some(dlg) = &state.permission_dialog {
        draw_permission_dialog(f, dlg, area);
    }
}

fn draw_chat(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Theme::border())
        .title(" wyj-code ");

    let inner = block.inner(area);
    f.render_widget(block, area);

    // 构建所有行
    let mut lines: Vec<Line> = vec![];
    for msg in &state.messages {
        match msg.role {
            MessageRole::User => {
                lines.push(Line::from(vec![
                    Span::styled("你  ", Theme::user_prefix()),
                    Span::raw(""),
                ]));
                for l in msg.content.lines() {
                    lines.push(Line::from(Span::raw(format!("  {l}"))));
                }
                lines.push(Line::from(""));
            }
            MessageRole::Assistant => {
                lines.push(Line::from(vec![
                    Span::styled("AI  ", Theme::assistant_prefix()),
                    Span::raw(""),
                ]));
                render_assistant_text(&mut lines, &msg.content);
                lines.push(Line::from(""));
            }
            MessageRole::ToolCall => {
                lines.push(Line::from(vec![
                    Span::styled("⚙ ", Theme::tool_call()),
                    Span::styled(&msg.content, Theme::tool_call()),
                ]));
            }
            MessageRole::ToolResult => {
                let style = if msg.is_error {
                    Theme::error()
                } else {
                    Theme::tool_result()
                };
                // 工具结果限制展示行数
                let content_lines: Vec<&str> = msg.content.lines().collect();
                let show = content_lines.len().min(8);
                for l in &content_lines[..show] {
                    lines.push(Line::from(Span::styled(format!("  {l}"), style)));
                }
                if content_lines.len() > show {
                    lines.push(Line::from(Span::styled(
                        format!("  …（共 {} 行）", content_lines.len()),
                        Theme::dim(),
                    )));
                }
                lines.push(Line::from(""));
            }
        }
    }

    // 流式中当前积累的文本
    if !state.streaming_buf.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("AI  ", Theme::assistant_prefix()),
            Span::raw(""),
        ]));
        render_assistant_text(&mut lines, &state.streaming_buf);
    }

    let text = Text::from(lines);
    let total_lines = text.lines.len() as u16;
    let visible_height = inner.height;

    // 滚动：从底部往上计算偏移
    let scroll = if total_lines > visible_height {
        let max_scroll = total_lines - visible_height;
        let offset = state.scroll_offset;
        max_scroll.saturating_sub(offset)
    } else {
        0
    };

    let para = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, inner);
}

fn render_assistant_text<'a>(lines: &mut Vec<Line<'a>>, text: &str) {
    // 简单代码块检测：``` 开头的行用不同颜色
    let mut in_code = false;
    for raw_line in text.lines() {
        if raw_line.starts_with("```") {
            in_code = !in_code;
            lines.push(Line::from(Span::styled(
                raw_line.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        } else if in_code {
            lines.push(Line::from(Span::styled(
                format!("  {raw_line}"),
                Style::default().fg(Color::Cyan),
            )));
        } else {
            lines.push(Line::from(Span::raw(format!("  {raw_line}"))));
        }
    }
}

fn draw_input(f: &mut Frame, state: &AppState, input: &InputBox, area: Rect) {
    let title = if state.is_thinking {
        " 思考中… (Ctrl+C 中断) "
    } else {
        " 输入 (Enter 发送 / Shift+Enter 换行) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Theme::border())
        .title(title);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.is_thinking {
        let para = Paragraph::new("…").style(Theme::dim());
        f.render_widget(para, inner);
        return;
    }

    let lines: Vec<Line> = input
        .display_lines()
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i == input.cursor_row {
                // 渲染光标位置
                let col = input.cursor_display_col();
                let before = &l[..col.min(l.len())];
                let cursor_char = l.chars().nth(col).map(|c| c.to_string()).unwrap_or_else(|| " ".to_string());
                let after = if col < l.len() { &l[col + cursor_char.len()..] } else { "" };
                Line::from(vec![
                    Span::raw(before.to_string()),
                    Span::styled(cursor_char, Style::default().bg(Color::White).fg(Color::Black)),
                    Span::raw(after.to_string()),
                ])
            } else {
                Line::from(l.as_str())
            }
        })
        .collect();

    let para = Paragraph::new(Text::from(lines)).style(Theme::input_box());
    f.render_widget(para, inner);
}

fn draw_status(f: &mut Frame, state: &AppState, area: Rect) {
    let tokens = format!(
        " tokens: {}↑ {}↓  cwd: {}",
        state.total_input_tokens,
        state.total_output_tokens,
        state.cwd.display()
    );
    let help = "Ctrl+C 退出  /help 命令";
    let padding = area.width.saturating_sub(tokens.len() as u16 + help.len() as u16 + 2);
    let text = format!("{tokens}{}{help}", " ".repeat(padding as usize));
    let para = Paragraph::new(text).style(Theme::status_bar());
    f.render_widget(para, area);
}

fn draw_permission_dialog(f: &mut Frame, dlg: &PermissionDialog, area: Rect) {
    let width = (area.width * 3 / 4).max(40);
    let height = 10u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Theme::permission_dialog())
        .title(" 权限确认 ");

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let lines = vec![
        Line::from(Span::styled(
            format!("工具: {}", dlg.tool_name),
            Theme::permission_dialog(),
        )),
        Line::from(""),
        Line::from(Span::styled("参数预览:", Theme::highlight())),
        Line::from(Span::raw(truncate(&dlg.input_preview, 200))),
        Line::from(""),
        Line::from(Span::styled("[y] 允许    [n] 拒绝", Theme::highlight())),
    ];

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
