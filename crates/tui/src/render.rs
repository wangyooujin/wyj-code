//! 对话渲染与布局

use crate::app::{
    fmt_tokens, format_hms, AppState, AskQuestionDialog, AskQuestionStage, Attachment,
    ExecModeConfirmDialog, InProgressAnswer, MemoryDialog, MemoryRow, MessageRole,
    PermissionDialog, PlanApprovalDialog, ProfileDialog, ProfileOverlay, SessionPickerState,
    SettingsDialog, SubAgentStatus, TodoRuntimeStats, PROFILE_API_KEY_FIELD_IDX,
    PROFILE_FIELD_LABEL_KEYS, SETTINGS_FIELD_COUNT, SETTINGS_FIELD_LABEL_KEYS,
};
use crate::input::InputBox;
use crate::markdown::render_markdown;
use crate::theme::Theme;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::collections::HashMap;
use std::time::Instant;
use wyj_config::AgentMode;
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer};
use wyj_core::ClaudeMdSource;
use wyj_tools::todo::{is_todo_collapsible, TodoStatus};

/// 将 @token 渲染为青色高亮 Span，其余部分为普通文本
fn highlight_at_refs(line: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;
    while let Some(at_pos) = rest.find('@') {
        if at_pos > 0 {
            spans.push(Span::raw(rest[..at_pos].to_string()));
        }
        let after = &rest[at_pos + 1..];
        let end = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        let token = rest[at_pos..at_pos + 1 + end].to_string();
        spans.push(Span::styled(
            token,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        rest = &rest[at_pos + 1 + end..];
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    Line::from(spans)
}

/// 字符显示宽度：CJK 全角 = 2，其余 = 1
pub(crate) fn char_display_width(c: char) -> usize {
    if c == '\t' {
        return 4;
    }
    let cp = c as u32;
    if (0x1100..=0x115F).contains(&cp)       // Hangul Jamo
        || (0x2E80..=0x303E).contains(&cp)   // CJK Radicals
        || (0x3041..=0x33FF).contains(&cp)   // Japanese
        || (0xAC00..=0xD7A3).contains(&cp)   // Hangul Syllables
        || (0xF900..=0xFAFF).contains(&cp)   // CJK Compatibility
        || (0xFE10..=0xFE6F).contains(&cp)   // CJK Compatibility Forms
        || (0xFF00..=0xFF60).contains(&cp)   // Fullwidth
        || (0xFFE0..=0xFFE6).contains(&cp)   // Fullwidth Signs
        || (0x1F300..=0x1F9FF).contains(&cp) // Emoji
        || (0x20000..=0x2FA1F).contains(&cp)
    // CJK Extension
    {
        2
    } else {
        1
    }
}

/// 截断超长字符串（按终端显示宽度，CJK 字符占 2 列）
pub(crate) fn truncate_line(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut width = 0usize;
    let mut result = String::new();
    for c in s.chars() {
        let cw = char_display_width(c);
        if width + cw > max_cols.saturating_sub(1) {
            result.push('…');
            return result;
        }
        width += cw;
        result.push(c);
    }
    s.to_string()
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

/// Spinner 动画帧（braille，复刻 Claude Code 风格）
pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn draw(f: &mut Frame, state: &mut AppState, input: &InputBox) {
    let area = f.area();
    let inner_width = area.width.saturating_sub(2) as usize; // -2 for borders
    let input_height = (input.visual_height(inner_width) as u16 + 2).max(3).min(10);

    // 补全列表高度（@ 文件选取器优先于 slash 补全）
    let completion_height = if !state.file_completions.is_empty() {
        (state.file_completions.len() as u16 + 2).min(10)
    } else if !state.slash_completions.is_empty() {
        (state.slash_completions.len() as u16 + 2).min(8)
    } else {
        0u16
    };

    // 附件预览条高度（有附件时显示）
    let attach_height: u16 = if state.pending_attachments.is_empty() {
        0
    } else {
        3
    };

    // 底部面板高度：AskQuestion 优先，否则 TaskList，否则 0
    let (panel_height, panel_kind) = bottom_panel_size(state, area.height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(panel_height),
            Constraint::Length(completion_height),
            Constraint::Length(attach_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_chat(f, state, chunks[0]);
    match panel_kind {
        BottomPanel::None => {}
        BottomPanel::ExecModeConfirm => {
            if let Some(dlg) = &state.exec_mode_confirm {
                draw_exec_mode_confirm_panel(f, dlg, chunks[1]);
            }
        }
        BottomPanel::PlanApproval => {
            if let Some(dlg) = &state.plan_dialog {
                draw_plan_approval_panel(f, dlg, chunks[1]);
            }
        }
        BottomPanel::AskQuestion => {
            if let Some(dlg) = &state.ask_question_dialog {
                draw_ask_question_panel(f, dlg, chunks[1]);
            }
        }
        BottomPanel::SubAgents => {
            draw_sub_agents_panel(f, state, chunks[1]);
        }
        BottomPanel::TodoList => {
            if let Some(items) = &state.current_todos {
                draw_todo_panel(
                    f,
                    items,
                    state.spinner_frame,
                    state.todo_panel_expanded,
                    &state.todo_stats,
                    chunks[1],
                );
            }
        }
    }
    if !state.file_completions.is_empty() {
        draw_file_completions(f, state, chunks[2]);
    } else if !state.slash_completions.is_empty() {
        draw_slash_completions(f, state, chunks[2]);
    }
    if !state.pending_attachments.is_empty() {
        draw_attachments(f, state, chunks[3]);
    }
    draw_input(f, state, input, chunks[4]);
    draw_status(f, state, chunks[5]);

    // 权限对话框仍以浮层叠加（后渲染的在前）
    if let Some(dlg) = &state.permission_dialog {
        draw_permission_dialog(f, dlg, area);
    }

    // 会话选择器叠加在最顶层
    if let Some(picker) = &state.session_picker {
        draw_session_picker(f, picker, area);
    }

    // 设置面板叠加在最顶层
    if let Some(dialog) = &state.settings_dialog {
        draw_settings_dialog(f, dialog, area);
    }

    // 分组管理面板叠加在最顶层
    if let Some(dialog) = &state.profile_dialog {
        draw_profile_dialog(f, dialog, area);
    }

    // CLAUDE.md 记忆面板叠加在最顶层
    if let Some(dialog) = &state.memory_dialog {
        draw_memory_dialog(f, dialog, area);
    }
}

/// 底部面板类型与高度
enum BottomPanel {
    None,
    ExecModeConfirm,
    PlanApproval,
    AskQuestion,
    SubAgents,
    TodoList,
}

fn bottom_panel_size(state: &AppState, area_height: u16) -> (u16, BottomPanel) {
    if state.exec_mode_confirm.is_some() {
        return (4u16.min(area_height), BottomPanel::ExecModeConfirm);
    }
    if let Some(dlg) = &state.plan_dialog {
        // 计划正文可能很长：面板最多占用可用高度的 70%（保留聊天区/输入框可见），
        // 内部通过滚动查看超出部分，见 draw_plan_approval_panel。
        let content_lines = dlg.plan.lines().count().max(1) as u16;
        let max_h = (area_height * 7 / 10).max(6);
        let h = (content_lines + 3).clamp(6, max_h);
        return (h, BottomPanel::PlanApproval);
    }
    if let Some(dlg) = &state.ask_question_dialog {
        let h = match dlg.stage {
            AskQuestionStage::Answering { index } => {
                let spec = &dlg.questions[index];
                let opts = spec.options.len();
                let desc_lines = spec
                    .options
                    .iter()
                    .filter(|o| o.description.is_some())
                    .count();
                // 题目行 + 分隔线 + (选项数+1个"其他"，每个描述再加一行) + 空行 + hint行 + 边框上下
                (opts as u16 + desc_lines as u16 + 1 + 6).min(area_height)
            }
            AskQuestionStage::Overview { .. } => {
                // 每题两行（题干+答案）+ 分隔线 + 确认提交行 + hint行 + 边框上下
                (dlg.questions.len() as u16 * 2 + 6).min(area_height)
            }
        };
        return (h, BottomPanel::AskQuestion);
    }
    // 子 Agent 聚合面板：有可见子 Agent 即显示（优先于任务列表）；
    // >3 个时自动折叠为仅标题行
    let visible = state.visible_sub_agents();
    if !visible.is_empty() {
        let h = if visible.len() > 3 {
            2u16.min(area_height)
        } else {
            (visible.len() as u16 + 2).min(area_height)
        };
        return (h, BottomPanel::SubAgents);
    }
    if let Some(items) = &state.current_todos {
        if !items.is_empty() {
            let collapsed = is_todo_collapsible(items) && !state.todo_panel_expanded;
            let h = if collapsed {
                2u16.min(area_height)
            } else {
                (items.len() as u16 + 2).min(area_height)
            };
            return (h, BottomPanel::TodoList);
        }
    }
    (0, BottomPanel::None)
}

// ─── 对话区 ──────────────────────────────────────────────────────────────────

fn draw_chat(f: &mut Frame, state: &mut AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::border())
        .title(Span::styled(
            format!(" wyj-code v{} ", env!("CARGO_PKG_VERSION")),
            Theme::dim(),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // 右侧 1 列留给滚动条
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let content_area = cols[0];
    let scrollbar_area = cols[1];

    // 空白聊天区：渲染欢迎页（5 行 shadow logo 渐变 + Profile/Model + cwd 两行看板）
    if state.messages.is_empty() && state.streaming_buf.is_empty() {
        let ctx = crate::welcome::WelcomeContext {
            model: state.model_name.clone(),
            cwd: shorten_home_path(&state.cwd.display().to_string()),
            profile: {
                let p = &state.config.active_profile;
                if p == "default" {
                    None
                } else {
                    Some(p.clone())
                }
            },
            tip_index: state.welcome_tip_idx,
        };
        let lines = crate::welcome::render_welcome(&ctx, content_area.width);
        let para = Paragraph::new(Text::from(lines))
            .style(Theme::input_box())
            .alignment(Alignment::Left);
        f.render_widget(para, content_area);
        state.scrollbar_area = scrollbar_area;
        state.chat_height = content_area.height;
        return;
    }

    let max_content_width = content_area.width.saturating_sub(2) as usize;
    let sep_width = content_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = vec![];
    let mut is_first_user = true;

    // 预扫描：找出最后一条「可折叠」的 ToolResult 索引。
    // 可折叠 = ToolResult && 非 Edit/Write（Edit/Write 永不折叠）&& 内容超 3 行 && 未展开。
    // 只有这一条会显示 "ctrl+o to expand/collapse" 文字提示，
    // 其余可折叠的历史结果改用静默 ⋯N 标记，避免提示与快捷键行为错位。
    let last_collapsible_idx: Option<usize> = state
        .messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            if !matches!(m.role, MessageRole::ToolResult) {
                return false;
            }
            if matches!(m.tool_name.as_deref(), Some("Edit") | Some("Write")) {
                return false;
            }
            if m.expanded {
                return false;
            }
            let summary = if m.display_summary.is_empty() {
                m.content
                    .lines()
                    .next()
                    .unwrap_or("done")
                    .trim()
                    .to_string()
            } else {
                m.display_summary.clone()
            };
            !m.content.is_empty() && m.content != summary && m.content.lines().count() > 3
        })
        .map(|(i, _)| i);

    for (msg_idx, msg) in state.messages.iter().enumerate() {
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

                // 子 Agent 调用：运行期间紧跟一条实时刷新的动态 ⎿ 状态行
                if let Some(s) = msg.sub_agent_id.and_then(|id| state.sub_agents.get(&id)) {
                    match s.status {
                        SubAgentStatus::Running => {
                            let frame = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
                            let stats = wyj_i18n::tr_fmt(
                                "subagent.inline_running",
                                &[
                                    ("elapsed", format_hms(s.elapsed_secs()).as_str()),
                                    ("tokens", &fmt_tokens(s.output_tokens)),
                                    ("count", &s.tool_calls.to_string()),
                                ],
                            );
                            let mut spans = vec![
                                Span::styled("    ⎿  ", Theme::dim()),
                                Span::styled(
                                    format!("{frame} {stats}"),
                                    Style::default().fg(Color::Cyan),
                                ),
                            ];
                            if let Some(cur) = &s.current_tool {
                                spans.push(Span::styled(
                                    format!(" · {}", truncate_line(cur, 40)),
                                    Theme::dim(),
                                ));
                            }
                            lines.push(Line::from(spans));
                        }
                        SubAgentStatus::Interrupted if !s.has_result => {
                            lines.push(Line::from(vec![
                                Span::styled("    ⎿  ", Theme::dim()),
                                Span::styled(
                                    format!("✗ {}", wyj_i18n::tr("subagent.interrupted")),
                                    Theme::error(),
                                ),
                            ]));
                        }
                        _ => {}
                    }
                }
            }

            // ─── ⎿  summary · elapsed  ────────────────────────────────────
            MessageRole::ToolResult => {
                let elapsed_str = msg
                    .elapsed_secs
                    .filter(|&s| s > 0.0)
                    .map(|s| format!("  {}", format_hms(s)))
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
                    let is_diff = matches!(msg.tool_name.as_deref(), Some("Edit") | Some("Write"));

                    // Edit/Write：永不折叠，直接展开全部 diff，带 +/- 配色
                    if is_diff {
                        lines.push(Line::from(Span::styled(
                            format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
                            Theme::dim(),
                        )));
                        // 子 Agent 结果：先列出其内部工具调用明细，再展示最终文本
                        if let Some(s) = msg.sub_agent_id.and_then(|id| state.sub_agents.get(&id)) {
                            for tl in &s.tool_log {
                                let (mark, mark_style) = match (tl.elapsed_secs, tl.is_error) {
                                    (None, _) => ("…".to_string(), Theme::dim()),
                                    (Some(e), true) => {
                                        (format!("✗ {}", format_hms(e)), Theme::error())
                                    }
                                    (Some(e), false) => (
                                        format!("✓ {}", format_hms(e)),
                                        Style::default().fg(Color::Green),
                                    ),
                                };
                                let call = if tl.arg_summary.is_empty() {
                                    tl.tool_name.clone()
                                } else {
                                    format!("{}({})", tl.tool_name, tl.arg_summary)
                                };
                                lines.push(Line::from(vec![
                                    Span::styled("       ⏺ ", Theme::tool_call()),
                                    Span::styled(
                                        truncate_line(&call, max_content_width.saturating_sub(20)),
                                        Theme::dim(),
                                    ),
                                    Span::styled(format!("  {mark}"), mark_style),
                                ]));
                            }
                            if !s.tool_log.is_empty() {
                                lines.push(Line::from(Span::styled(
                                    format!(
                                        "       {}",
                                        "─".repeat(max_content_width.saturating_sub(8))
                                    ),
                                    Theme::dim(),
                                )));
                            }
                        }
                        // diff 行带配色：+ 绿、- 红、上下文 dim
                        let max_lines = 60;
                        for (i, l) in content_lines.iter().enumerate() {
                            if i >= max_lines {
                                lines.push(Line::from(Span::styled(
                                    format!(
                                        "       …({} more lines)",
                                        content_lines.len() - max_lines
                                    ),
                                    Theme::dim(),
                                )));
                                break;
                            }
                            let style = if l.starts_with("+ ") {
                                Style::default().fg(Color::Green)
                            } else if l.starts_with("- ") {
                                Theme::error()
                            } else {
                                Theme::dim()
                            };
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "       {}",
                                    truncate_line(l, max_content_width.saturating_sub(8))
                                ),
                                style,
                            )));
                        }
                    } else if msg.expanded {
                        // 已展开（非 Edit/Write）
                        lines.push(Line::from(Span::styled(
                            format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
                            Theme::dim(),
                        )));
                        // 子 Agent 结果：先列出其内部工具调用明细，再展示最终文本
                        if let Some(s) = msg.sub_agent_id.and_then(|id| state.sub_agents.get(&id)) {
                            for tl in &s.tool_log {
                                let (mark, mark_style) = match (tl.elapsed_secs, tl.is_error) {
                                    (None, _) => ("…".to_string(), Theme::dim()),
                                    (Some(e), true) => {
                                        (format!("✗ {}", format_hms(e)), Theme::error())
                                    }
                                    (Some(e), false) => (
                                        format!("✓ {}", format_hms(e)),
                                        Style::default().fg(Color::Green),
                                    ),
                                };
                                let call = if tl.arg_summary.is_empty() {
                                    tl.tool_name.clone()
                                } else {
                                    format!("{}({})", tl.tool_name, tl.arg_summary)
                                };
                                lines.push(Line::from(vec![
                                    Span::styled("       ⏺ ", Theme::tool_call()),
                                    Span::styled(
                                        truncate_line(&call, max_content_width.saturating_sub(20)),
                                        Theme::dim(),
                                    ),
                                    Span::styled(format!("  {mark}"), mark_style),
                                ]));
                            }
                            if !s.tool_log.is_empty() {
                                lines.push(Line::from(Span::styled(
                                    format!(
                                        "       {}",
                                        "─".repeat(max_content_width.saturating_sub(8))
                                    ),
                                    Theme::dim(),
                                )));
                            }
                        }
                        let line_style = if msg.is_error {
                            Theme::error()
                        } else {
                            Theme::tool_result()
                        };
                        for l in &content_lines {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "       {}",
                                    truncate_line(l, max_content_width.saturating_sub(8))
                                ),
                                line_style,
                            )));
                        }
                        // 只有「最后一条可折叠」且已展开才显示 collapse 提示
                        if last_collapsible_idx == Some(msg_idx) {
                            lines.push(Line::from(Span::styled(
                                "       [ctrl+o to collapse]".to_string(),
                                Theme::dim(),
                            )));
                        }
                    } else if content_lines.len() > 3 {
                        // 折叠态：只有最后一条可折叠的才显示快捷键提示
                        if last_collapsible_idx == Some(msg_idx) {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "       …({} lines, ctrl+o to expand)",
                                    content_lines.len()
                                ),
                                Theme::dim(),
                            )));
                        } else {
                            // 其余可折叠的历史结果：静默标记，不显示快捷键提示
                            lines.push(Line::from(Span::styled(
                                format!("       ⋯{}", content_lines.len()),
                                Theme::dim(),
                            )));
                        }
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
                    .map(|s| format!(" · {}", format_hms(s)))
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
            MessageRole::TurnSummary => {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {}", msg.content),
                    Theme::dim(),
                )]));
                lines.push(Line::from(""));
            }
        }
    }

    // 流式文本（实时输出中）
    if !state.streaming_buf.is_empty() {
        render_markdown(&mut lines, &state.streaming_buf, max_content_width);
    }

    let text = Text::from(lines);

    // 用 ratatui 的渲染折行计数拿精确视觉行数：手工 ceil(width/cw) 估算与实际
    // word-wrap 结果有偏差，长对话下会导致底部若干行永远滚不进视口。
    let cw = content_area.width.max(1);
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    let total_visual_lines = para.line_count(cw).min(u16::MAX as usize) as u16;

    let visible_height = content_area.height;
    state.chat_height = visible_height;

    let max_scroll = total_visual_lines.saturating_sub(visible_height);
    let clamped_offset = state.scroll_offset.min(max_scroll);
    // 把 clamp 后的值写回状态，防止滚轮/按键累加时 raw offset 超过 max_scroll，
    // 导致到达顶部后再往下滚时视觉上卡住（必须按 PageDown 才能恢复）。
    state.scroll_offset = clamped_offset;
    let scroll = max_scroll.saturating_sub(clamped_offset);

    let para = para.scroll((scroll, 0));
    f.render_widget(para, content_area);

    // 记录滚动条区域供鼠标点击命中检测
    state.scrollbar_area = scrollbar_area;

    // 滚动条（内容超出可视区时显示 ▲/▼ 箭头 + 拇指指示器）
    let can_scroll_up = clamped_offset < max_scroll; // 还有更早内容
    let can_scroll_down = clamped_offset > 0; // 还有更新内容
    if total_visual_lines > visible_height && visible_height > 0 {
        // 拇指在中间轨道（排除头尾各 1 行的箭头位置）
        let track_height = visible_height.saturating_sub(2);
        let thumb_row = if max_scroll > 0 && track_height > 0 {
            let pct = clamped_offset as f32 / max_scroll as f32; // 0=底部 1=顶部
            1 + (track_height.saturating_sub(1) as f32 * (1.0 - pct)) as u16
        } else {
            1
        };
        let bar_lines: Vec<Line<'static>> = (0..visible_height)
            .map(|row| {
                if row == 0 {
                    if can_scroll_up {
                        Line::from(Span::styled("▲", Style::default().fg(Color::DarkGray)))
                    } else {
                        Line::from(Span::styled("╷", Theme::dim()))
                    }
                } else if row == visible_height - 1 {
                    if can_scroll_down {
                        Line::from(Span::styled("▼", Style::default().fg(Color::DarkGray)))
                    } else {
                        Line::from(Span::styled("╵", Theme::dim()))
                    }
                } else if row == thumb_row {
                    Line::from(Span::styled("█", Style::default().fg(Color::DarkGray)))
                } else {
                    Line::from(Span::styled("│", Theme::dim()))
                }
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(bar_lines)), scrollbar_area);
    }
}

/// 底部固定面板：子 Agent 总览（每行一个，对齐任务列表风格）
/// 标题 `agents [N]`；Running=spinner / Done=✓ / Failed=✗ / Interrupted=⊘；
/// >3 个时自动折叠为仅标题行
fn draw_sub_agents_panel(f: &mut Frame, state: &AppState, area: Rect) {
    let visible = state.visible_sub_agents();

    let title = wyj_i18n::tr_fmt(
        "subagent.panel_title",
        &[("count", visible.len().to_string().as_str())],
    );
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

    // >3 个时折叠为仅标题行
    if visible.len() > 3 {
        return;
    }

    let max_content_width = inner.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = vec![];

    for (id, s) in visible {
        // 状态图标 + 内容配色（对齐任务列表的 ○/spinner/✓ 风格）
        let (icon, item_style) = match s.status {
            SubAgentStatus::Running => (
                SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()].to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            SubAgentStatus::Done => (
                "✓".to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
            SubAgentStatus::Failed => ("✗".to_string(), Theme::error()),
            SubAgentStatus::Interrupted => ("⊘".to_string(), Theme::dim()),
        };

        let bg_tag = if s.background { " ◇bg" } else { "" };
        let head = format!("a{} {}({})", id, s.agent_type, s.description);
        let stats = format!(
            " ⏱ {} ↑{} ↓{}{bg_tag}",
            format_hms(s.elapsed_secs()),
            fmt_tokens(s.input_tokens),
            fmt_tokens(s.output_tokens),
        );
        let mut spans = vec![
            Span::styled(format!(" a{id} "), Theme::dim()),
            Span::styled(format!("{icon} "), item_style),
            Span::styled(
                truncate_line(&head, max_content_width.saturating_sub(30)),
                item_style,
            ),
            Span::styled(stats, Theme::dim()),
        ];
        if let Some(cur) = &s.current_tool {
            spans.push(Span::styled(
                format!(" {}", truncate_line(cur, 30)),
                Theme::dim(),
            ));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// 底部固定面板：任务列表
fn draw_todo_panel(
    f: &mut Frame,
    items: &[wyj_tools::todo::TodoItem],
    spinner_frame: usize,
    expanded: bool,
    todo_stats: &HashMap<String, TodoRuntimeStats>,
    area: Rect,
) {
    let total = items.len();
    let done = items
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let collapsible = is_todo_collapsible(items);
    let collapsed = collapsible && !expanded;
    let all_done = total > 0 && done == total;

    let total_elapsed: f64 = todo_stats.values().map(|s| s.elapsed_secs()).sum();
    let total_in: u32 = todo_stats.values().map(|s| s.input_tokens).sum();
    let total_out: u32 = todo_stats.values().map(|s| s.output_tokens).sum();
    let stats_suffix = if todo_stats.is_empty() {
        String::new()
    } else {
        format!(
            " ⏱ {} ↑{} ↓{}",
            format_hms(total_elapsed),
            fmt_tokens(total_in),
            fmt_tokens(total_out)
        )
    };

    // 折叠态下逐条任务的 spinner 不会被渲染（循环整体被跳过），用标题栏的
    // spinner 前缀补上"仍在运行"的动感提示；全部完成/无进行中任务时不需要。
    let has_in_progress = items.iter().any(|t| t.status == TodoStatus::InProgress);
    let spinner_prefix = if collapsed && has_in_progress {
        format!("{} ", SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()])
    } else {
        String::new()
    };

    let title = if collapsed {
        if all_done {
            format!(" ✓ 任务已完成 [{done}/{total}]{stats_suffix} (ctrl+t to expand) ")
        } else {
            format!(" {spinner_prefix}任务列表 [{done}/{total}]{stats_suffix} (ctrl+t to expand) ")
        }
    } else if collapsible {
        format!(" 任务列表 [{done}/{total}]{stats_suffix} (ctrl+t to collapse) ")
    } else {
        format!(" 任务列表 [{done}/{total}]{stats_suffix} ")
    };

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

    if collapsed {
        return;
    }

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
            max_content_width.saturating_sub(24),
        );

        let mut spans = vec![
            Span::styled(format!("[{idx_str}] "), Theme::dim()),
            Span::styled(format!("{icon} "), item_style),
            Span::styled(content, item_style),
        ];
        if let Some(s) = todo_stats.get(&item.id) {
            spans.push(Span::styled(
                format!(
                    " ⏱ {} ↑{} ↓{}",
                    format_hms(s.elapsed_secs()),
                    fmt_tokens(s.input_tokens),
                    fmt_tokens(s.output_tokens)
                ),
                Theme::dim(),
            ));
        }
        lines.push(Line::from(spans));
    }

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

// ─── 输入框 ──────────────────────────────────────────────────────────────────

fn draw_input(f: &mut Frame, state: &AppState, input: &InputBox, area: Rect) {
    // 检测 ! bash 模式：首行以 ! 开头且不在思考中
    let is_bang = !state.is_thinking
        && input
            .display_lines()
            .first()
            .map(|l| l.starts_with('!'))
            .unwrap_or(false);

    let (title_content, title_style) = if state.is_thinking {
        let frame = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
        let op = state.current_op.as_deref().unwrap_or("Thinking");
        (
            format!(" {frame} {op} · esc to interrupt "),
            Style::default().fg(Theme::CLAUDE),
        )
    } else if is_bang {
        (
            " $ bash · Enter to run ".to_string(),
            Style::default()
                .fg(Theme::SUCCESS)
                .add_modifier(Modifier::BOLD),
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

    let border_style = if is_bang {
        Style::default().fg(Theme::SUCCESS)
    } else {
        match &state.mode {
            AgentMode::Plan if !state.is_thinking => Style::default().fg(Color::Blue),
            AgentMode::Bypass if !state.is_thinking => Style::default().fg(Color::Yellow),
            _ => Theme::border(),
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title_content, title_style));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let text_style = if is_bang {
        Style::default().fg(Theme::SUCCESS)
    } else {
        Theme::input_box()
    };
    // 手动按显示宽度折行（而非交给 ratatui 的 Wrap）：必须和 InputBox::cursor_visual_pos
    // 用同一套折行算法，否则光标计算出的坐标会和实际渲染的换行位置对不上，
    // 导致换行后光标错位、看起来像输入内容错乱。
    let wrap_width = inner.width as usize;
    let lines: Vec<Line> = if is_bang {
        input
            .display_lines()
            .iter()
            .flat_map(|l| InputBox::wrap_for_render(l, wrap_width))
            .map(Line::from)
            .collect()
    } else {
        input
            .display_lines()
            .iter()
            .flat_map(|l| InputBox::wrap_for_render(l, wrap_width))
            .map(|seg| highlight_at_refs(&seg))
            .collect()
    };
    let para = Paragraph::new(Text::from(lines)).style(text_style);
    f.render_widget(para, inner);

    if state.is_thinking {
        // is_thinking 时不设置光标位置，ratatui 会自动隐藏终端光标（避免和 spinner 冲突）
        return;
    }

    // 光标位置：考虑长行折行后的视觉坐标
    let (vis_row, vis_col) = input.cursor_visual_pos(inner.width as usize);
    let cursor_x = (inner.x + vis_col as u16).min(inner.x + inner.width.saturating_sub(1));
    let cursor_y = (inner.y + vis_row as u16).min(inner.y + inner.height.saturating_sub(1));

    // 粘贴瞬时提示：在输入框光标/粘贴位置显示 1.5s，便于用户确认已粘贴图片/文件/文字
    if let Some(hint) = &state.paste_hint {
        if hint.expires_at > Instant::now() {
            let (hint_row, hint_col) =
                input.visual_pos_for(hint.cursor_row, hint.cursor_col, inner.width as usize);
            if hint_row < inner.height as usize {
                let max_w = inner.width.saturating_sub(hint_col as u16) as usize;
                let text = if max_w == 0 {
                    String::new()
                } else {
                    truncate_line(&hint.text, max_w)
                };
                if !text.is_empty() {
                    let w = text.chars().map(char_display_width).sum::<usize>() as u16;
                    let area =
                        Rect::new(inner.x + hint_col as u16, inner.y + hint_row as u16, w, 1);
                    let para = Paragraph::new(Line::from(Span::styled(
                        text,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                            .add_modifier(Modifier::REVERSED),
                    )));
                    f.render_widget(para, area);
                }
            }
        }
    }

    f.set_cursor_position(Position::new(cursor_x, cursor_y));
}

// ─── 附件预览条 ───────────────────────────────────────────────────────────────

fn draw_attachments(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " 附件 · Enter 发送 · ESC 中断清空 ",
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut spans: Vec<Span> = vec![];
    for att in &state.pending_attachments {
        match att {
            Attachment::Image { preview_label, .. } => {
                spans.push(Span::styled(
                    format!(" [图片 {preview_label}] "),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Attachment::File { path } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                spans.push(Span::styled(
                    format!(" [文件 {name}] "),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
        spans.push(Span::raw("  "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

// ─── @ 文件选取器下拉 ─────────────────────────────────────────────────────────

fn draw_file_completions(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " @ 文件 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_show = inner.height as usize;
    let items = &state.file_completions;
    let selected = state.file_selected;

    let start = if selected >= max_show {
        selected - max_show + 1
    } else {
        0
    };

    let name_col_w = items
        .iter()
        .map(|e| e.display.chars().count() + 2)
        .max()
        .unwrap_or(8)
        .min(32);
    let desc_budget = (inner.width as usize).saturating_sub(name_col_w + 4);

    let lines: Vec<Line<'static>> = items
        .iter()
        .skip(start)
        .take(max_show)
        .enumerate()
        .map(|(i, entry)| {
            let real_idx = start + i;
            let prefix = if entry.is_dir { "▸ " } else { "  " };
            let display = format!("{prefix}{}", entry.display);
            let name_padded = format!(" {:width$}", display, width = name_col_w.saturating_sub(1));
            let hint = if entry.is_dir {
                format!(
                    "  {}/",
                    truncate_chars(&entry.rel_path, desc_budget.saturating_sub(1))
                )
            } else {
                format!("  {}", truncate_chars(&entry.rel_path, desc_budget))
            };

            if real_idx == selected {
                Line::from(vec![
                    Span::styled(
                        name_padded,
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(hint, Style::default().bg(Color::Cyan).fg(Color::Black)),
                ])
            } else {
                let name_style = if entry.is_dir {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
                Line::from(vec![
                    Span::styled(name_padded, name_style),
                    Span::styled(hint, Theme::dim()),
                ])
            }
        })
        .collect();

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
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
    let (used, total) = (state.context_tokens, state.context_window);
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

    let (right_help, right_style) = if !state.pending_queue.is_empty() {
        (
            format!(
                "● {} 条消息已排队，将在当前操作完成后发送",
                state.pending_queue.len()
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else if state.ctrl_c_pressed {
        (
            "ctrl+c again to exit".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "ctrl+d or ctrl+c twice to exit  /help".to_string(),
            Theme::dim(),
        )
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
        .title(Span::styled(
            wyj_i18n::tr("dialog.permission_title"),
            Theme::permission_dialog(),
        ));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let preview = truncate_chars(&dlg.input_preview, (inner.width as usize * 3).max(80));

    let lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(wyj_i18n::tr("dialog.permission_tool_label"), Theme::dim()),
            Span::styled(dlg.tool_name.clone(), Theme::permission_dialog()),
        ]),
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Theme::border(),
        )),
        Line::from(Span::raw(preview)),
        Line::from(""),
        Line::from(Span::styled(
            wyj_i18n::tr("dialog.permission_hint"),
            Theme::highlight(),
        )),
    ];

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

// ─── AskQuestion 底部面板 ─────────────────────────────────────────────────────

fn draw_plan_approval_panel(f: &mut Frame, dlg: &PlanApprovalDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(
            " 📋 计划已就绪 ",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // 底部固定一行操作提示，其余空间展示计划正文（可滚动）
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let (content_area, hint_area) = (rows[0], rows[1]);

    let mut lines: Vec<Line<'static>> = vec![];
    render_markdown(&mut lines, &dlg.plan, content_area.width as usize);
    let text = Text::from(lines);

    let cw = content_area.width.max(1);
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    let total_visual_lines = para.line_count(cw).min(u16::MAX as usize) as u16;
    let visible_height = content_area.height;
    let max_scroll = total_visual_lines.saturating_sub(visible_height);
    let scroll = dlg.scroll.min(max_scroll);
    f.render_widget(para.scroll((scroll, 0)), content_area);

    let hint = if total_visual_lines > visible_height {
        "  [y/Enter] 批准并切换至执行模式   [n/Esc] 继续规划   [↑/↓] 滚动查看完整计划"
    } else {
        "  [y/Enter] 批准并切换至执行模式   [n/Esc] 继续规划"
    };
    let hint_para = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(hint_para, hint_area);
}

fn draw_exec_mode_confirm_panel(f: &mut Frame, dlg: &ExecModeConfirmDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::warning())
        .title(Span::styled(
            " ⚠ 检测到计划已批准 ",
            Theme::warning().add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_w = inner.width as usize;
    let preview = truncate_line(&dlg.pending_message, max_w.saturating_sub(6));

    let lines = vec![
        Line::from(vec![
            Span::styled("待发送：", Style::default().fg(Color::DarkGray)),
            Span::styled(preview, Style::default().fg(Color::White)),
        ]),
        Line::from(Span::styled(
            "  [y/Enter] 切换执行模式并发送   [n] 保持规划模式发送   [Esc] 取消",
            Theme::warning().add_modifier(Modifier::BOLD),
        )),
    ];
    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

/// 把选项下标列表格式化为顿号分隔的 label 文本
fn labels_for_ui(spec: &AskQuestionSpec, indices: &[usize]) -> String {
    indices
        .iter()
        .filter_map(|&i| spec.options.get(i))
        .map(|o| o.label.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 总览页里一题答案的展示文本
fn format_confirmed_answer_ui(spec: &AskQuestionSpec, answer: &QuestionAnswer) -> String {
    match answer {
        QuestionAnswer::Selected(indices) => labels_for_ui(spec, indices),
        QuestionAnswer::FreeText(text) => format!("其他: {text}"),
        QuestionAnswer::SelectedWithFreeText(indices, text) => {
            format!("{}、其他: {text}", labels_for_ui(spec, indices))
        }
    }
}

fn build_answering_lines(
    dlg: &AskQuestionDialog,
    index: usize,
    max_w: usize,
) -> Vec<Line<'static>> {
    let spec = &dlg.questions[index];
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            truncate_line(&spec.question, max_w),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("─".repeat(max_w), Theme::border())),
    ];

    if let InProgressAnswer::FreeText { input, .. } = &dlg.current {
        let text = input.lines.first().map(|s| s.as_str()).unwrap_or("");
        lines.push(Line::from(Span::styled(
            format!("> {}_", truncate_line(text, max_w.saturating_sub(2))),
            Style::default().fg(Theme::CLAUDE),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr("dialog.hint_freetext_submit_cancel"),
            Theme::dim(),
        )));
        return lines;
    }

    let (cursor, checked): (usize, Option<&std::collections::BTreeSet<usize>>) = match &dlg.current
    {
        InProgressAnswer::Single { cursor } => (*cursor, None),
        InProgressAnswer::Multi { cursor, checked } => (*cursor, Some(checked)),
        InProgressAnswer::FreeText { .. } => unreachable!(),
    };

    for (i, opt) in spec.options.iter().enumerate() {
        let marker = match checked {
            Some(set) if set.contains(&i) => "[x] ",
            Some(_) => "[ ] ",
            None => "",
        };
        let label = format!("{marker}{}", opt.label);
        if i == cursor {
            lines.push(Line::from(Span::styled(
                format!("  ▶ {}", truncate_line(&label, max_w.saturating_sub(4))),
                Style::default()
                    .fg(Theme::CLAUDE)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("    {}", truncate_line(&label, max_w.saturating_sub(4))),
                Style::default().fg(Color::White),
            )));
        }
        if let Some(desc) = &opt.description {
            lines.push(Line::from(Span::styled(
                format!("      {}", truncate_line(desc, max_w.saturating_sub(6))),
                Theme::dim(),
            )));
        }
    }

    // "其他"固定追加在选项末尾
    let other_label = wyj_i18n::tr("dialog.ask_question_other_label");
    if cursor == spec.options.len() {
        lines.push(Line::from(Span::styled(
            format!(
                "  ▶ {}",
                truncate_line(&other_label, max_w.saturating_sub(4))
            ),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "    {}",
                truncate_line(&other_label, max_w.saturating_sub(4))
            ),
            Style::default().fg(Color::White),
        )));
    }

    lines.push(Line::from(""));
    let hint_key = if spec.multi_select {
        "dialog.hint_multi_select"
    } else {
        "dialog.hint_single_select"
    };
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr(hint_key),
        Theme::dim(),
    )));
    lines
}

fn build_overview_lines(dlg: &AskQuestionDialog, index: usize, max_w: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, spec) in dlg.questions.iter().enumerate() {
        let header = spec
            .header
            .as_ref()
            .filter(|h| !h.is_empty())
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        let title = format!("Q{} {header}{}", i + 1, spec.question);
        let style = if i == index {
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if i == index { "▶ " } else { "  " };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", truncate_line(&title, max_w.saturating_sub(2))),
            style,
        )));
        let answer_text = dlg.confirmed[i]
            .as_ref()
            .map(|c| format_confirmed_answer_ui(spec, &c.answer))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!(
                "     → {}",
                truncate_line(&answer_text, max_w.saturating_sub(6))
            ),
            Theme::dim(),
        )));
    }

    lines.push(Line::from(Span::styled("─".repeat(max_w), Theme::border())));
    let submit_label = wyj_i18n::tr("dialog.ask_question_confirm_submit");
    let is_submit_row = index == dlg.questions.len();
    let (prefix, style) = if is_submit_row {
        (
            "▶ ",
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::default().fg(Color::White))
    };
    lines.push(Line::from(Span::styled(
        format!("{prefix}{submit_label}"),
        style,
    )));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("dialog.hint_overview"),
        Theme::dim(),
    )));
    lines
}

fn draw_ask_question_panel(f: &mut Frame, dlg: &AskQuestionDialog, area: Rect) {
    let title = match dlg.stage {
        AskQuestionStage::Answering { index } => {
            let spec = &dlg.questions[index];
            let header_suffix = match &spec.header {
                Some(h) if !h.is_empty() => format!(" [{h}]"),
                _ => String::new(),
            };
            format!(
                "{} ({}/{}){header_suffix}",
                wyj_i18n::tr("dialog.ask_question_title"),
                index + 1,
                dlg.questions.len()
            )
        }
        AskQuestionStage::Overview { .. } => wyj_i18n::tr("dialog.ask_question_overview_title"),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_w = inner.width as usize;
    let lines = match dlg.stage {
        AskQuestionStage::Answering { index } => build_answering_lines(dlg, index, max_w),
        AskQuestionStage::Overview { index } => build_overview_lines(dlg, index, max_w),
    };

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
            wyj_i18n::tr_fmt(
                "dialog.session_picker_title",
                &[("count", &n_sessions.to_string())],
            ),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let w = inner.width as usize;
    let home = std::env::var("HOME").unwrap_or_default();
    let new_session_label = wyj_i18n::tr("dialog.new_session_label");

    let mut lines: Vec<Line<'static>> = Vec::new();

    // "新建会话" 条目（selected == 0 时高亮）
    if picker.selected == 0 {
        lines.push(Line::from(Span::styled(
            format!("  ▶ {:<w$}", new_session_label, w = w.saturating_sub(4)),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("    {:<w$}", new_session_label, w = w.saturating_sub(4)),
            Style::default().fg(Color::Green),
        )));
    }

    if !picker.sessions.is_empty() {
        lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));

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
            let right = format!(
                "  {}  {}  {}",
                time_str,
                cwd_last,
                wyj_i18n::tr_fmt(
                    "dialog.session_turns_suffix",
                    &[("turns", &meta.turns.to_string())]
                )
            );
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
        wyj_i18n::tr("dialog.hint_nav_select_cancel"),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

// ─── 语言选择器 ───────────────────────────────────────────────────────────────

/// 把 `$HOME` 开头的路径缩短为 `~/...`（与状态栏同款）
fn shorten_home_path(full: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && full.starts_with(&home) {
        format!("~{}", &full[home.len()..])
    } else {
        full.to_string()
    }
}

/// api_key 打码展示：只保留前 8 位 + "..."
fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let prefix: String = s.chars().take(8).collect();
    format!("{prefix}...")
}

fn draw_settings_dialog(f: &mut Frame, dialog: &SettingsDialog, area: Rect) {
    // 2 字段（log_level/language）+ 分隔线 + 错误行 + 提示行
    let content_lines = SETTINGS_FIELD_COUNT as u16 + 3;
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 6 / 10).clamp(50, 90).min(area.width);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("settings.title")),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;
    let label_width = 18usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for idx in 0..SETTINGS_FIELD_COUNT {
        let label = wyj_i18n::tr(SETTINGS_FIELD_LABEL_KEYS[idx]);
        let selected = idx == dialog.selected;
        let value = dialog.draft.display_value(idx);

        let marker = if selected { "▶ " } else { "  " };
        let text = format!("{marker}{label:<label_width$}{value}");
        let text = truncate_line(&text, w);

        let style = if selected {
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(Span::styled(
            truncate_line(err, w),
            Theme::warning(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("settings.hint"),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

/// CLAUDE.md 记忆面板渲染（/memory 命令触发）
fn draw_memory_dialog(f: &mut Frame, dialog: &MemoryDialog, area: Rect) {
    let content_lines = dialog.rows.len() as u16 + 3; // 行列表 + 分隔线 + 错误行 + 提示行
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            wyj_i18n::tr("memory.dialog.title"),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;
    let label_width = 10usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (idx, row) in dialog.rows.iter().enumerate() {
        let selected = idx == dialog.selected;
        let marker = if selected { "▶ " } else { "  " };

        let text = match row {
            MemoryRow::File(f) => {
                let source_label = wyj_i18n::tr(match f.source {
                    ClaudeMdSource::Global => "claude_md.source.global",
                    ClaudeMdSource::Project => "claude_md.source.project",
                    ClaudeMdSource::Subdir => "claude_md.source.subdir",
                });
                let suffix = if f.exists {
                    String::new()
                } else {
                    format!("  {}", wyj_i18n::tr("memory.dialog.not_found"))
                };
                format!("{marker}[{source_label:<4}] {}{suffix}", f.path.display())
            }
            MemoryRow::AutoMemoryToggle => {
                let label = wyj_i18n::tr("memory.dialog.auto_memory_label");
                let value = wyj_i18n::tr(if dialog.auto_memory_enabled {
                    "memory.dialog.auto_memory_on"
                } else {
                    "memory.dialog.auto_memory_off"
                });
                format!("{marker}{label:<label_width$}{value}")
            }
            MemoryRow::AutoMemoryIndex { path, exists } => {
                let label = wyj_i18n::tr("memory.dialog.auto_memory_index_label");
                let value = if *exists {
                    path.display().to_string()
                } else {
                    wyj_i18n::tr("memory.dialog.auto_memory_index_empty")
                };
                format!("{marker}{label:<label_width$}{value}")
            }
        };
        let text = truncate_line(&text, w);

        let style = if selected {
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(Span::styled(
            truncate_line(err, w),
            Theme::warning(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("memory.dialog.hint"),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

/// 分组管理面板渲染（/model 无参命令触发）
fn draw_profile_dialog(f: &mut Frame, dialog: &ProfileDialog, area: Rect) {
    let rows = dialog.rows();
    let content_lines = rows.len() as u16 + 4; // 行列表 + 分隔线 + 错误行 + 提示两行
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("profile.title")),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;
    let label_width = 18usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (row_idx, (entry_idx, field_idx)) in rows.iter().enumerate() {
        let entry = &dialog.entries[*entry_idx];
        let selected_row = row_idx == dialog.cursor;
        let editing = selected_row && dialog.editing.is_some();

        let text = match field_idx {
            None => {
                let marker = if *entry_idx == dialog.active_idx {
                    "●"
                } else {
                    " "
                };
                let expand_marker = if dialog.expanded == Some(*entry_idx) {
                    "▾"
                } else {
                    "▸"
                };
                let cursor = if selected_row { "▶" } else { " " };
                let summary = format!(
                    "{} [{}] {}",
                    entry.display_value(0),
                    entry.model,
                    entry.name
                );
                format!("{cursor} {expand_marker} {marker} {summary}")
            }
            Some(f_idx) => {
                let label = wyj_i18n::tr(PROFILE_FIELD_LABEL_KEYS[*f_idx]);
                let value = if editing {
                    dialog.editing.as_ref().unwrap().lines.join("")
                } else if *f_idx == PROFILE_API_KEY_FIELD_IDX {
                    mask_secret(entry.text_value(*f_idx))
                } else {
                    entry.display_value(*f_idx)
                };
                let cursor = if selected_row { "▶" } else { " " };
                format!("{cursor}     {label:<label_width$}{value}")
            }
        };
        let text = truncate_line(&text, w);

        let style = if editing {
            Style::default().fg(Color::Black).bg(Theme::CLAUDE)
        } else if selected_row {
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(Span::styled(
            truncate_line(err, w),
            Theme::warning(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        truncate_line(&wyj_i18n::tr("profile.dialog.hint1"), w),
        Theme::dim(),
    )));
    lines.push(Line::from(Span::styled(
        truncate_line(&wyj_i18n::tr("profile.dialog.hint2"), w),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);

    if let Some(ib) = &dialog.editing {
        let (_, vis_col) = ib.cursor_visual_pos(w.saturating_sub(7 + label_width));
        let cursor_x = (inner.x + (7 + label_width + vis_col) as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        let cursor_y =
            (inner.y + dialog.cursor as u16).min(inner.y + inner.height.saturating_sub(1));
        f.set_cursor_position(Position::new(cursor_x, cursor_y));
    }

    match &dialog.overlay {
        ProfileOverlay::None => {}
        ProfileOverlay::Renaming { input, .. } => {
            draw_profile_text_overlay(
                f,
                area,
                "profile.overlay.rename_title",
                &input.lines.join(""),
            );
        }
        ProfileOverlay::TemplatePicker { selected } => {
            draw_profile_list_overlay(
                f,
                area,
                "profile.overlay.template_title",
                wyj_api::PROFILE_TEMPLATES
                    .iter()
                    .map(|t| {
                        if t.note.is_empty() {
                            t.label.to_string()
                        } else {
                            format!("{}  ({})", t.label, t.note)
                        }
                    })
                    .collect::<Vec<_>>(),
                *selected,
            );
        }
        ProfileOverlay::ConfirmDelete { entry_idx } => {
            let name = &dialog.entries[*entry_idx].name;
            draw_profile_text_overlay(
                f,
                area,
                "profile.overlay.confirm_delete_title",
                &wyj_i18n::tr_fmt("profile.overlay.confirm_delete_body", &[("name", name)]),
            );
        }
        ProfileOverlay::FetchingModels { .. } => {
            draw_profile_text_overlay(
                f,
                area,
                "profile.overlay.fetching_title",
                &wyj_i18n::tr("profile.fetch.in_progress"),
            );
        }
        ProfileOverlay::ModelsPicker {
            models, selected, ..
        } => {
            draw_profile_list_overlay(
                f,
                area,
                "profile.overlay.models_title",
                models.clone(),
                *selected,
            );
        }
    }
}

fn draw_profile_text_overlay(f: &mut Frame, area: Rect, title_key: &str, body: &str) {
    let width = (area.width * 5 / 10).clamp(40, 70).min(area.width);
    let height = 5u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr(title_key)),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;
    let para = Paragraph::new(Text::from(vec![Line::from(truncate_line(body, w))]));
    f.render_widget(para, inner);
}

fn draw_profile_list_overlay(
    f: &mut Frame,
    area: Rect,
    title_key: &str,
    items: Vec<String>,
    selected: usize,
) {
    let width = (area.width * 6 / 10).clamp(40, 90).min(area.width);
    let height = ((items.len() as u16) + 2)
        .min(area.height.saturating_sub(2))
        .max(4);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr(title_key)),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;

    let lines: Vec<Line<'static>> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let marker = if i == selected { "▶ " } else { "  " };
            let text = truncate_line(&format!("{marker}{item}"), w);
            let style = if i == selected {
                Style::default()
                    .fg(Theme::CLAUDE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(text, style))
        })
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
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
        wyj_i18n::tr("dialog.time_just_now")
    } else if diff < 3600 {
        wyj_i18n::tr_fmt(
            "dialog.time_minutes_ago",
            &[("n", &(diff / 60).to_string())],
        )
    } else if diff < 86400 {
        wyj_i18n::tr_fmt(
            "dialog.time_hours_ago",
            &[("n", &(diff / 3600).to_string())],
        )
    } else if diff < 7 * 86400 {
        wyj_i18n::tr_fmt(
            "dialog.time_days_ago",
            &[("n", &(diff / 86400).to_string())],
        )
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
