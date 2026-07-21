//! 对话渲染与布局

use crate::app::{
    fmt_tokens, format_hms, ActionMenu, AgentsDialog, AppState, AskQuestionDialog,
    AskQuestionStage, Attachment, ChatMessage, ChatSelectionAnchor, ExecModeConfirmDialog,
    ExtensionsDialog, FlatRow, ImportDialog, ImportStage, InProgressAnswer, InputOwner,
    McpConnStatus, McpDialog, McpDialogTab, McpOverlay, MemoryDialog, MemoryRow, MessageRole,
    PermissionDialog, PlanApprovalDialog, PluginOverlay, PluginsDialog, PluginsDialogTab,
    ProfileDialog, ProfileInputField, ProfileOverlay, ProfileRow, ScheduleDialog,
    ScheduleInputField, ScheduleOverlay, ScheduleRow, SessionPickerState, SettingsDialog,
    SkillsDialog, SkillsDialogTab, SkillsOverlay, SubAgentStatus, SubAgentUiState, SubToolLine,
    TodoExecutionEntry, TodoRuntimeStats, UiFocus, PROFILE_API_KEY_FIELD_IDX,
    PROFILE_FIELD_LABEL_KEYS, SCHEDULE_FIELD_LABEL_KEYS, SCHEDULE_FIELD_NOTIFY,
    SETTINGS_FIELD_COUNT, SETTINGS_FIELD_LABEL_KEYS,
};
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
use std::collections::HashMap;
use std::time::Instant;
use wyj_config::AgentMode;
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer};
use wyj_core::ClaudeMdSource;
use wyj_tools::todo::{is_todo_collapsible, TodoStatus};
use wyj_tools::trace::TraceEvent;

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
        || (0x2E80..=0x9FFF).contains(&cp)   // CJK 部首/假名/统一表意文字等各区块
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

/// 按显示宽度换行（不截断，不加省略号），CJK 宽字符不跨行拆分；
/// 用于需要完整展示内容的场景（区别于 [`truncate_line`] 的省略号截断）。
pub(crate) fn wrap_line(s: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;
    for c in s.chars() {
        let cw = char_display_width(c);
        if col + cw > max_cols && col > 0 {
            out.push(std::mem::take(&mut cur));
            col = 0;
        }
        cur.push(c);
        col += cw;
    }
    out.push(cur);
    out
}

/// 去掉 Read 工具结果每行开头的 "行号\t" 前缀（`crates/tools/src/read.rs` 为了让模型
/// 能按行号精确编辑而加的），只影响人类可读的展示层——`msg.content` 本身不变，
/// 发给模型的历史记录仍保留原始行号。不匹配 "数字\t" 格式的行（如末尾的
/// "（共 N 行...）" 提示行）原样返回。
fn strip_read_line_number(line: &str) -> &str {
    match line.find('\t') {
        Some(tab_idx) if line[..tab_idx].bytes().all(|b| b.is_ascii_digit()) && tab_idx > 0 => {
            &line[tab_idx + 1..]
        }
        _ => line,
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

/// Spinner 动画帧（braille，复刻 Claude Code 风格）
pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// ToolResult 折叠阈值：内容行数超过此值才折叠，否则始终全量展示。
const THINKING_FOLD_LINES: usize = 5;
const MESSAGE_DETAIL_MIN_ROWS: usize = 8;
const MESSAGE_DETAIL_MAX_ROWS: usize = 18;
const MESSAGE_DETAIL_DEFAULT_ROWS: usize = 12;

/// 从正文行中去掉与 `⎿` 摘要行重复的第一条非空行——仅当 `summary_is_first_line`
/// 为真（即摘要直接复用了正文首行原文，如 Bash 输出首行）时才需要去重；
/// `read`/`grep`/`glob`/`webfetch` 等摘要是合成统计文案，不会与正文重复，原样保留。
fn strip_summary_duplicate_line<'a>(
    lines: &[&'a str],
    summary_is_first_line: bool,
) -> Vec<&'a str> {
    if !summary_is_first_line {
        return lines.to_vec();
    }
    match lines.iter().position(|l| !l.trim().is_empty()) {
        Some(idx) => {
            let mut v = lines.to_vec();
            v.remove(idx);
            v
        }
        None => lines.to_vec(),
    }
}

/// 渲染 ToolResult 正文行，`take` 为 `Some(n)` 时只取开头 n 行（折叠态预览），
/// `None` 时全量渲染（展开态 / 短内容始终全量展示）。
fn render_tool_result_body_lines(
    lines: &mut Vec<Line<'static>>,
    content_lines: &[&str],
    take: Option<usize>,
    line_style: Style,
    max_content_width: usize,
) {
    let iter: Box<dyn Iterator<Item = &&str>> = match take {
        Some(n) => Box::new(content_lines.iter().take(n)),
        None => Box::new(content_lines.iter()),
    };
    for l in iter {
        for wrapped in wrap_line(l, max_content_width.saturating_sub(8)) {
            lines.push(Line::from(Span::styled(
                format!("       {wrapped}"),
                line_style,
            )));
        }
    }
}

pub fn draw(f: &mut Frame, state: &mut AppState, input: &InputBox) {
    let area = f.area();
    let inner_width = area.width.saturating_sub(2) as usize; // -2 for borders
    let input_height = (input.visual_height(inner_width) as u16 + 2).clamp(3, 10);

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

    // 底部面板高度：只保留真正需要固定拦截输入的控制面板。
    // TaskList / AskQuestion 作为实时信息流附加到聊天区尾部，避免互相遮挡。
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
    // 防御性清零：只有 BottomPanel::SubAgents 分支里真正画出详情区时才会重新写入
    // 准确值，避免面板本帧未展示详情区（终端太矮/未展开/无选中）时残留上一帧的旧值。
    state.sub_agent_detail_max_scroll = 0;
    match panel_kind {
        BottomPanel::None => {}
        BottomPanel::Permission => {
            if let Some(dlg) = &state.permission_dialog {
                draw_permission_dialog(f, dlg, chunks[1]);
            }
        }
        BottomPanel::ProjectTrust => {
            if let Some(servers) = &state.pending_mcp_trust {
                draw_project_trust_panel(f, servers, chunks[1]);
            }
        }
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
        BottomPanel::SubAgents => {
            draw_sub_agents_panel(f, state, chunks[1]);
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
        draw_profile_dialog(f, dialog, state.input_owner, area);
    }

    // CLAUDE.md 记忆面板叠加在最顶层
    if let Some(dialog) = &state.memory_dialog {
        draw_memory_dialog(f, dialog, area);
    }

    // MCP server 管理面板叠加在最顶层
    if let Some(dialog) = &state.mcp_dialog {
        draw_mcp_dialog(f, dialog, &state.mcp_connection_status, area);
    }

    // Skill 管理面板叠加在最顶层
    if let Some(dialog) = &state.skills_dialog {
        draw_skills_dialog(f, dialog, area);
    }

    // 插件管理面板叠加在最顶层
    if let Some(dialog) = &state.plugins_dialog {
        draw_plugins_dialog(f, dialog, area);
    }

    // 可用 Agent 类型面板叠加在最顶层
    if let Some(dialog) = &mut state.agents_dialog {
        draw_agents_dialog(f, dialog, area);
    }

    if let Some(dialog) = &mut state.extensions_dialog {
        draw_extensions_dialog(f, dialog, area);
    }

    // 一键导入面板叠加在最顶层
    if let Some(dialog) = &state.import_dialog {
        draw_import_dialog(f, dialog, area);
    }

    // 定时任务面板叠加在最顶层
    if let Some(dialog) = &state.schedule_dialog {
        draw_schedule_dialog(f, dialog, state.input_owner, area);
    }
}

/// 底部面板类型与高度
enum BottomPanel {
    None,
    Permission,
    ProjectTrust,
    ExecModeConfirm,
    PlanApproval,
    SubAgents,
}

/// agents 面板列表区最多同时展示的行数（超出用滚动窗口，不再随数量线性增长）
const SUB_AGENT_LIST_MAX: usize = 6;
/// agents 面板详情区（工具流水 + 结果/状态提示）最多占用的行数（超出内部滚动）
const SUB_AGENT_DETAIL_MAX: u16 = 12;

fn bottom_panel_size(state: &AppState, area_height: u16) -> (u16, BottomPanel) {
    // 权限确认最优先：几乎每次 Edit/Write/Bash 调用都可能弹出，必须始终可立即
    // 响应，不能被其他面板挡住；固定 11 行，完全放得进底部常驻区（对齐
    // Claude Code Inline 模式——不再走全屏浮层，避免每次都触发全屏切换闪烁）。
    if state.permission_dialog.is_some() {
        return (11u16.min(area_height), BottomPanel::Permission);
    }
    // 项目级 MCP server 信任确认：只在启动时出现一次，优先级次于逐调用权限
    // 确认（后者可能在一轮工具调用中途弹出，必须绝对优先响应），但高于其余
    // 面板——这是"要不要允许仓库自带的任意命令执行"的安全门槛，不应被
    // ExecModeConfirm/PlanApproval 这类流程性面板挡住。
    if state.pending_mcp_trust.is_some() {
        return (11u16.min(area_height), BottomPanel::ProjectTrust);
    }
    if state.exec_mode_confirm.is_some() {
        return (4u16.min(area_height), BottomPanel::ExecModeConfirm);
    }
    if state.plan_dialog.is_some() {
        // 计划正文已作为普通消息并入应用内聊天流，
        // 这里只剩固定 3 行的三选一选择器，贴在输入框上方，宽度对齐 Permission。
        return (5u16.min(area_height), BottomPanel::PlanApproval);
    }
    // 子 Agent 聚合面板：运行期间自动显示，全部结束后自动收起；用户通过
    // `/subagents` 或方向键主动进入面板焦点时仍可查看本会话历史详情。
    // 列表区固定行数上限 + 滚动窗口（本会话内全部保留，数量可能持续增长）；
    // 详情展开时追加详情区所需行数，整体按可用高度 70% 封顶，避免聊天区被挤没。
    let visible = state.visible_sub_agents();
    let show_sub_agents = state.has_running_sub_agents() || state.ui_focus == UiFocus::SubAgents;
    if show_sub_agents && !visible.is_empty() {
        let list_rows = visible.len().min(SUB_AGENT_LIST_MAX) as u16;
        let detail_rows = if state.sub_agent_detail_open {
            state
                .selected_sub_agent
                .and_then(|id| state.sub_agents.get(&id))
                .map(|s| {
                    // sizing 阶段没有宽度信息，只能用原始行数粗略估算；
                    // 精确的可视行数在 draw_sub_agents_panel 渲染时用 Paragraph::line_count 重新计算并 clamp scroll。
                    let tool_rows = if s.tool_log.is_empty() {
                        0
                    } else {
                        s.tool_log.len() as u16 + 1 // +1 分隔线
                    };
                    let status_rows = match s.status {
                        SubAgentStatus::Running | SubAgentStatus::Interrupted => 1,
                        SubAgentStatus::Done | SubAgentStatus::Failed => s
                            .final_result
                            .as_deref()
                            .map(|r| r.lines().count().max(1) as u16)
                            .unwrap_or(1),
                    };
                    (tool_rows + status_rows).min(SUB_AGENT_DETAIL_MAX)
                })
                .unwrap_or(0)
        } else {
            0
        };
        let content_rows = list_rows + detail_rows;
        let max_h = (area_height * 7 / 10).max(list_rows + 2);
        let h = (content_rows + 2).clamp(list_rows + 2, max_h);
        return (h.min(area_height), BottomPanel::SubAgents);
    }
    (0, BottomPanel::None)
}

// ─── 对话区 ──────────────────────────────────────────────────────────────────

/// 渲染子 Agent 的内部工具调用明细行（⏺ 工具名(参数) ✓/✗ 耗时），
/// 供 ToolResult 展开区（Edit/Write diff 与普通展开两处）和 agents 面板详情区共用。
fn push_sub_agent_tool_log(
    lines: &mut Vec<Line<'static>>,
    tool_log: &[SubToolLine],
    max_content_width: usize,
) {
    for tl in tool_log {
        let (mark, mark_style) = match (tl.elapsed_secs, tl.is_error) {
            (None, _) => ("…".to_string(), Theme::dim()),
            (Some(e), true) => (format!("✗ {}", format_hms(e)), Theme::error()),
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
    if !tool_log.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
            Theme::dim(),
        )));
    }
}

/// `render_chat_message` 渲染单条消息时需要的跨消息共享只读上下文。
struct ChatRenderCtx<'a> {
    max_content_width: usize,
    selected_message_id: Option<u64>,
    sub_agents: &'a std::collections::BTreeMap<u64, SubAgentUiState>,
    spinner_frame: usize,
    message_detail_scroll: &'a HashMap<u64, u16>,
    detail_viewport_rows: usize,
}

fn detail_viewport_rows(chat_view_height: usize) -> usize {
    if chat_view_height == 0 {
        return MESSAGE_DETAIL_DEFAULT_ROWS;
    }
    ((chat_view_height * 4).div_ceil(10)).clamp(MESSAGE_DETAIL_MIN_ROWS, MESSAGE_DETAIL_MAX_ROWS)
}

fn selected_line_style(
    line: Line<'static>,
    selected_style: &dyn Fn(Style) -> Style,
) -> Line<'static> {
    if line.spans.is_empty() {
        return line;
    }
    let spans = line
        .spans
        .into_iter()
        .map(|span| Span::styled(span.content.into_owned(), selected_style(span.style)))
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn push_detail_viewport_lines(
    lines: &mut Vec<Line<'static>>,
    detail_lines: Vec<Line<'static>>,
    detail_scroll: u16,
    viewport_rows: usize,
    hint_prefix: &str,
) {
    if detail_lines.is_empty() {
        return;
    }
    let viewport = detail_lines.len().min(viewport_rows.max(1));
    let max_scroll = detail_lines.len().saturating_sub(viewport);
    let scroll = (detail_scroll as usize).min(max_scroll);
    for line in detail_lines.into_iter().skip(scroll).take(viewport) {
        lines.push(line);
    }
    if max_scroll > 0 {
        let below = max_scroll.saturating_sub(scroll);
        lines.push(Line::from(Span::styled(
            format!(
                "{hint_prefix}… detail {}/{} · {} below · pgup/pgdn",
                scroll + 1,
                max_scroll + 1,
                below
            ),
            Theme::dim(),
        )));
    }
}

fn clean_thinking_lines(content: &str) -> Vec<&str> {
    content
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render_thinking_block(
    lines: &mut Vec<Line<'static>>,
    msg_id: u64,
    content: &str,
    expanded: bool,
    selected: bool,
    max_content_width: usize,
    detail_scroll: u16,
    detail_viewport_rows: usize,
) {
    let selected_style = |st: Style| -> Style {
        if selected {
            st.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            st
        }
    };
    let clean_lines = clean_thinking_lines(content);
    let total = clean_lines.len();
    let action = if selected && total > THINKING_FOLD_LINES {
        if expanded {
            "  [ctrl+o collapse]"
        } else {
            "  [ctrl+o expand]"
        }
    } else {
        ""
    };
    let folded = if total > THINKING_FOLD_LINES {
        format!(" · {total} lines")
    } else {
        String::new()
    };
    let marker = if selected {
        "  ▶ ✻ thinking"
    } else {
        "  ✻ thinking"
    };
    lines.push(Line::from(vec![
        Span::styled(marker, selected_style(Style::default().fg(Color::Cyan))),
        Span::styled(folded, Theme::dim()),
        Span::styled(action, Theme::dim()),
    ]));
    if expanded {
        let detail_lines = clean_lines
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    format!(
                        "    {}",
                        truncate_line(line, max_content_width.saturating_sub(4))
                    ),
                    Theme::dim(),
                ))
            })
            .collect::<Vec<_>>();
        let _ = msg_id;
        push_detail_viewport_lines(
            lines,
            detail_lines,
            detail_scroll,
            detail_viewport_rows,
            "    ",
        );
    } else {
        for line in clean_lines.into_iter().take(THINKING_FOLD_LINES) {
            lines.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    truncate_line(line, max_content_width.saturating_sub(4))
                ),
                Theme::dim(),
            )));
        }
    }
}

/// 渲染单条消息（追加到 `lines`）。从 `draw_chat` 提炼出来，保证完整消息流、
/// 流式尾部和高度测量共用同一份渲染逻辑。
fn render_chat_message(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    _msg_idx: usize,
    is_first_user: &mut bool,
    ctx: &ChatRenderCtx,
) {
    let max_content_width = ctx.max_content_width;
    let selected = ctx.selected_message_id == Some(msg.id);
    let selected_style = |st: Style| -> Style {
        if selected {
            st.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            st
        }
    };
    match msg.role {
        MessageRole::User => {
            if !*is_first_user {
                lines.push(Line::from(Span::styled(
                    "─".repeat(max_content_width.min(60)),
                    Theme::dim(),
                )));
            }
            *is_first_user = false;

            let mut content_lines = msg.content.lines();
            let first_line = content_lines.next().unwrap_or("");
            let marker = if selected { "▶ ❯ " } else { "❯ " };
            lines.push(Line::from(vec![
                Span::styled(marker, selected_style(Theme::user_prefix())),
                Span::styled(
                    truncate_line(first_line, max_content_width),
                    selected_style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ),
            ]));
            for l in content_lines {
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncate_line(l, max_content_width)),
                    selected_style(Style::default()),
                )));
            }
        }

        MessageRole::Assistant => {
            if msg.is_error {
                for (idx, l) in msg.content.lines().enumerate() {
                    let prefix = if selected && idx == 0 {
                        "  ▶ ✗ "
                    } else {
                        "  ✗ "
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{}", truncate_line(l, max_content_width)),
                        selected_style(Theme::error()),
                    )));
                }
            } else {
                // 已定稿消息的 markdown 渲染结果按宽度缓存：避免每帧对
                // 全部历史重跑 markdown 解析（长对话下的主要渲染开销）
                let mut cache = msg.md_cache.borrow_mut();
                let mut rendered = match cache.as_ref() {
                    Some((w, cached)) if *w == max_content_width => cached.clone(),
                    _ => {
                        let mut fresh: Vec<Line<'static>> = vec![];
                        render_markdown(&mut fresh, &msg.content, max_content_width);
                        *cache = Some((max_content_width, fresh));
                        cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
                    }
                };
                if selected {
                    if let Some(first) = rendered.first_mut() {
                        first
                            .spans
                            .insert(0, Span::styled("  ▶ ", selected_style(Theme::dim())));
                    } else {
                        rendered.push(Line::from(Span::styled(
                            "  ▶ assistant",
                            selected_style(Theme::dim()),
                        )));
                    }
                    lines.extend(
                        rendered
                            .into_iter()
                            .map(|line| selected_line_style(line, &selected_style)),
                    );
                } else {
                    lines.extend(rendered);
                }
            }
        }

        MessageRole::Thinking => {
            render_thinking_block(
                lines,
                msg.id,
                &msg.content,
                msg.expanded,
                selected,
                max_content_width,
                ctx.message_detail_scroll.get(&msg.id).copied().unwrap_or(0),
                ctx.detail_viewport_rows,
            );
        }

        // ─── ⏺ ToolName(arg)  ────────────────────────────────────────
        MessageRole::ToolCall => {
            let marker = if selected { "  ▶ ⏺ " } else { "  ⏺ " };
            lines.push(Line::from(vec![
                Span::styled(marker, selected_style(Theme::tool_call())),
                Span::styled(
                    truncate_line(&msg.content, max_content_width.saturating_sub(4)),
                    selected_style(Theme::tool_call()),
                ),
            ]));

            // 子 Agent 调用：运行期间紧跟一条实时刷新的动态 ⎿ 状态行
            if let Some(s) = msg.sub_agent_id.and_then(|id| ctx.sub_agents.get(&id)) {
                match s.status {
                    SubAgentStatus::Running => {
                        let frame = SPINNER_FRAMES[ctx.spinner_frame % SPINNER_FRAMES.len()];
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
            render_tool_result_block(lines, None, msg, selected, &selected_style, ctx);
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
            let total = msg.content.lines().count();
            let first = msg
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("(no output)");
            let marker = if selected { "  ▶ " } else { "  " };
            let action = if msg.expanded {
                "  [ctrl+o collapse]"
            } else {
                "  [ctrl+o expand]"
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker}{icon} bash"), selected_style(style)),
                Span::styled(elapsed_str, Theme::dim()),
                Span::styled(
                    if total > 0 {
                        format!(" · {total} lines")
                    } else {
                        String::new()
                    },
                    Theme::dim(),
                ),
                Span::styled(
                    format!(
                        " · {}",
                        truncate_line(first, max_content_width.saturating_sub(18))
                    ),
                    selected_style(Theme::dim()),
                ),
                Span::styled(if selected { action } else { "" }, Theme::dim()),
            ]));
            if msg.expanded {
                let detail_lines = msg
                    .content
                    .lines()
                    .map(|l| {
                        Line::from(Span::styled(
                            format!(
                                "    {}",
                                truncate_line(l, max_content_width.saturating_sub(2))
                            ),
                            Style::default().fg(Color::DarkGray),
                        ))
                    })
                    .collect::<Vec<_>>();
                let detail_scroll = ctx.message_detail_scroll.get(&msg.id).copied().unwrap_or(0);
                push_detail_viewport_lines(
                    lines,
                    detail_lines,
                    detail_scroll,
                    ctx.detail_viewport_rows,
                    "    ",
                );
            }
        }

        MessageRole::System => {
            let (marker, style) = if msg.is_error {
                ("  ⚠ ", Theme::warning())
            } else {
                ("  ⚙ ", Style::default().fg(Color::Cyan))
            };
            let marker = if selected {
                format!("  ▶{}", marker.trim_start())
            } else {
                marker.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(marker, selected_style(style)),
                Span::styled(msg.content.clone(), selected_style(style)),
            ]));
        }
        MessageRole::TurnSummary => {
            lines.push(Line::from(vec![Span::styled(
                if selected {
                    format!("  ▶ {}", msg.content)
                } else {
                    format!("  {}", msg.content)
                },
                selected_style(Theme::dim()),
            )]));
        }

        // ─── 📋 计划正文  ────────────────────────────────────────────
        // 并入正常消息流；批准/继续规划/手动输入
        // 的交互留在贴底的 draw_plan_approval_selector。
        MessageRole::PlanProposal => {
            let divider = "─".repeat(max_content_width.saturating_sub(2));
            lines.push(Line::from(Span::styled(
                if selected {
                    "  ▶ 📋 计划"
                } else {
                    "  📋 计划"
                },
                selected_style(
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {divider}"),
                selected_style(Style::default().fg(Color::Blue)),
            )));
            let mut body: Vec<Line<'static>> = vec![];
            render_markdown(&mut body, &msg.content, max_content_width.saturating_sub(2));
            for l in body {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(l.spans);
                let line = Line::from(spans);
                if selected {
                    lines.push(selected_line_style(line, &selected_style));
                } else {
                    lines.push(line);
                }
            }
            lines.push(Line::from(Span::styled(
                format!("  {divider}"),
                selected_style(Style::default().fg(Color::Blue)),
            )));
        }
    }
}

fn trim_trailing_blank_lines(lines: &mut Vec<Line<'static>>) {
    while lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
}

fn message_summary(msg: &ChatMessage) -> String {
    if msg.display_summary.is_empty() {
        msg.content
            .lines()
            .next()
            .unwrap_or("done")
            .trim()
            .to_string()
    } else {
        msg.display_summary.clone()
    }
}

fn tool_result_content_lines(msg: &ChatMessage) -> Vec<&str> {
    let raw_content_lines: Vec<&str> = msg.content.lines().collect();
    let content_lines_deduped =
        strip_summary_duplicate_line(&raw_content_lines, msg.summary_is_first_line);
    let is_read = msg.tool_name.as_deref() == Some("Read");
    content_lines_deduped
        .into_iter()
        .map(|l| {
            if is_read {
                strip_read_line_number(l)
            } else {
                l
            }
        })
        .collect()
}

fn render_tool_result_details(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    content_lines: &[&str],
    ctx: &ChatRenderCtx,
) {
    if content_lines.is_empty() || !msg.expanded {
        return;
    }
    let max_content_width = ctx.max_content_width;
    let is_diff = matches!(msg.tool_name.as_deref(), Some("Edit") | Some("Write"));
    lines.push(Line::from(Span::styled(
        format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
        Theme::dim(),
    )));
    let mut detail_lines: Vec<Line<'static>> = Vec::new();
    if let Some(s) = msg.sub_agent_id.and_then(|id| ctx.sub_agents.get(&id)) {
        push_sub_agent_tool_log(&mut detail_lines, &s.tool_log, max_content_width);
    }
    if is_diff {
        for l in content_lines.iter() {
            let style = if l.starts_with("+ ") {
                Style::default().fg(Color::Green)
            } else if l.starts_with("- ") {
                Theme::error()
            } else {
                Theme::dim()
            };
            detail_lines.push(Line::from(Span::styled(
                format!(
                    "       {}",
                    truncate_line(l, max_content_width.saturating_sub(8))
                ),
                style,
            )));
        }
    } else {
        let line_style = if msg.is_error {
            Theme::error()
        } else {
            Theme::tool_result()
        };
        render_tool_result_body_lines(
            &mut detail_lines,
            content_lines,
            None,
            line_style,
            max_content_width,
        );
    }
    let detail_scroll = ctx.message_detail_scroll.get(&msg.id).copied().unwrap_or(0);
    push_detail_viewport_lines(
        lines,
        detail_lines,
        detail_scroll,
        ctx.detail_viewport_rows,
        "       ",
    );
}

fn render_tool_result_block(
    lines: &mut Vec<Line<'static>>,
    call: Option<&ChatMessage>,
    result: &ChatMessage,
    selected: bool,
    selected_style: &dyn Fn(Style) -> Style,
    ctx: &ChatRenderCtx,
) {
    let max_content_width = ctx.max_content_width;
    let elapsed_str = result
        .elapsed_secs
        .filter(|&s| s > 0.0)
        .map(|s| format!(" · {}", format_hms(s)))
        .unwrap_or_default();
    let (summary_style, prefix) = if result.is_error {
        (Theme::error(), "✗ ")
    } else {
        (Theme::dim(), "")
    };
    let summary = message_summary(result);
    let action = if result.expanded {
        "  [ctrl+o collapse]"
    } else {
        "  [ctrl+o expand]"
    };

    if let Some(call) = call {
        let marker = if selected { "  ▶ ⏺ " } else { "  ⏺ " };
        let call_max = (max_content_width / 3).clamp(12, 48);
        let fixed = marker.chars().map(char_display_width).sum::<usize>()
            + call_max.min(call.content.chars().map(char_display_width).sum::<usize>())
            + 5
            + elapsed_str.chars().map(char_display_width).sum::<usize>()
            + if selected { action.len() } else { 0 };
        let summary_max = max_content_width.saturating_sub(fixed).max(12);
        lines.push(Line::from(vec![
            Span::styled(marker, selected_style(Theme::tool_call())),
            Span::styled(
                truncate_line(&call.content, call_max),
                selected_style(Theme::tool_call()),
            ),
            Span::styled("  ⎿ ", selected_style(Theme::dim())),
            Span::styled(
                format!("{prefix}{}", truncate_line(&summary, summary_max)),
                selected_style(summary_style),
            ),
            Span::styled(elapsed_str, Theme::dim()),
            Span::styled(if selected { action } else { "" }, Theme::dim()),
        ]));
    } else {
        let marker = if selected { "  ▶ ⎿  " } else { "    ⎿  " };
        let fixed = marker.chars().map(char_display_width).sum::<usize>()
            + elapsed_str.chars().map(char_display_width).sum::<usize>()
            + if selected { action.len() } else { 0 };
        lines.push(Line::from(vec![
            Span::styled(marker, selected_style(Theme::dim())),
            Span::styled(
                format!(
                    "{prefix}{}",
                    truncate_line(&summary, max_content_width.saturating_sub(fixed).max(12))
                ),
                selected_style(summary_style),
            ),
            Span::styled(elapsed_str, Theme::dim()),
            Span::styled(if selected { action } else { "" }, Theme::dim()),
        ]));
    }

    let content_lines = tool_result_content_lines(result);
    render_tool_result_details(lines, result, &content_lines, ctx);
}

/// 渲染欢迎页所有行。供 [`build_pending_chat_lines`] 与高度测量复用，避免两处
/// `WelcomeContext` 构造 drift。
pub(crate) fn welcome_lines(state: &AppState, max_content_width: usize) -> Vec<Line<'static>> {
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
    crate::welcome::render_welcome(&ctx, max_content_width as u16)
}

fn should_show_welcome(state: &AppState) -> bool {
    !state.welcome_frozen && state.frozen_up_to == 0
}

/// 构建"待渲染"聊天内容：欢迎页（若适用）+ 完整消息流 + thinking/answer 流式文本。
/// 主循环永久运行在 `Viewport::Fullscreen`、不再有冻结机制（`state.frozen_up_to`
/// 永远是 0），因此这里覆盖的是**整个会话历史**，供 [`draw_chat`] 每帧渲染，
/// 超出可视高度的部分由 `AppState.chat_scroll` 驱动的应用内滚动展示。
pub(crate) fn build_pending_chat_lines(
    state: &mut AppState,
    max_content_width: usize,
) -> Vec<Line<'static>> {
    state.ensure_message_ids();
    state.selected_message_line = None;
    // 新空会话的欢迎页是消息流顶部的固定头部，真实消息和流式输出都紧接其后。
    // 会话恢复、切换和 /clear 会显式抑制欢迎页，防止它混进已有历史。
    let mut lines: Vec<Line<'static>> = vec![];
    if should_show_welcome(state) {
        lines.extend(welcome_lines(state, max_content_width));
    }

    let start = state.frozen_up_to.min(state.messages.len());
    let mut is_first_user = !state
        .messages
        .iter()
        .take(start)
        .any(|m| matches!(m.role, MessageRole::User));
    let mut selected_line = None;
    lines.extend(render_message_range(
        MessageRangeRenderArgs {
            messages: &state.messages,
            range: start..state.messages.len(),
            max_content_width,
            sub_agents: &state.sub_agents,
            spinner_frame: state.spinner_frame,
            selected_message_id: state.selected_message_id,
            message_detail_scroll: &state.message_detail_scroll,
            detail_viewport_rows: detail_viewport_rows(state.chat_view_height),
        },
        &mut selected_line,
        &mut is_first_user,
    ));
    state.selected_message_line = selected_line;

    if !state.thinking_buf.is_empty() {
        render_thinking_block(
            &mut lines,
            0,
            &state.thinking_buf,
            false,
            false,
            max_content_width,
            0,
            detail_viewport_rows(state.chat_view_height),
        );
    }

    // 流式文本（实时输出中）
    if !state.streaming_buf.is_empty() {
        render_markdown(&mut lines, &state.streaming_buf, max_content_width);
    }

    if let Some(items) = state.current_todos.clone() {
        let (next_scroll, next_max_scroll) = push_inline_todo_lines(
            &mut lines,
            &items,
            &state.messages,
            &state.sub_agents,
            &state.todo_execution_logs,
            state.spinner_frame,
            state.todo_panel_expanded,
            state.ui_focus == UiFocus::Todos,
            state.selected_todo_id.as_deref(),
            state.todo_detail_open,
            state.todo_detail_scroll,
            &state.todo_stats,
            max_content_width,
        );
        state.todo_detail_scroll = next_scroll;
        state.todo_detail_max_scroll = next_max_scroll;
    } else {
        state.todo_detail_max_scroll = 0;
    }

    if let Some(dlg) = &state.ask_question_dialog {
        push_inline_ask_question_lines(&mut lines, dlg, max_content_width);
    }

    lines
}

#[allow(clippy::too_many_arguments)]
fn push_inline_todo_lines(
    lines: &mut Vec<Line<'static>>,
    items: &[wyj_tools::todo::TodoItem],
    messages: &[ChatMessage],
    sub_agents: &std::collections::BTreeMap<u64, SubAgentUiState>,
    todo_logs: &HashMap<String, Vec<TodoExecutionEntry>>,
    spinner_frame: usize,
    expanded: bool,
    focused: bool,
    selected_todo_id: Option<&str>,
    detail_open: bool,
    detail_scroll: u16,
    todo_stats: &HashMap<String, TodoRuntimeStats>,
    max_content_width: usize,
) -> (u16, u16) {
    if items.is_empty() {
        return (detail_scroll, 0);
    }

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

    let has_in_progress = items.iter().any(|t| t.status == TodoStatus::InProgress);
    let spinner_prefix = if has_in_progress {
        format!("{} ", SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()])
    } else {
        String::new()
    };
    let title = if all_done {
        format!("任务已完成 [{done}/{total}]{stats_suffix}")
    } else {
        format!("{spinner_prefix}任务列表 [{done}/{total}]{stats_suffix}")
    };

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  TodoWrite", Theme::tool_call()),
        Span::styled("  ", Theme::dim()),
        Span::styled(
            title,
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ),
        if collapsible {
            Span::styled(
                if collapsed {
                    "  (ctrl+t to expand)"
                } else {
                    "  (ctrl+t to collapse)"
                },
                Theme::dim(),
            )
        } else {
            Span::raw("")
        },
    ]));
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(max_content_width.saturating_sub(2))),
        Theme::border(),
    )));

    if collapsed {
        return (detail_scroll, 0);
    }

    let max_item_width = max_content_width.saturating_sub(10);
    let mut selected_item = None;
    for (i, item) in items.iter().enumerate() {
        let is_selected = focused && selected_todo_id == Some(item.id.as_str());
        if is_selected {
            selected_item = Some(item);
        }
        let selected_style = |st: Style| -> Style {
            if is_selected {
                st.bg(Theme::SELECTED_BG).add_modifier(Modifier::BOLD)
            } else {
                st
            }
        };
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
        let display_text = if item.status == TodoStatus::InProgress {
            item.active_form.as_deref().unwrap_or(&item.content)
        } else {
            &item.content
        };
        let content = truncate_line(
            &format!("{prio_str}{display_text}"),
            max_item_width.saturating_sub(24),
        );
        let marker = if is_selected { "▶ " } else { "  " };

        let mut spans = vec![
            Span::styled(marker, selected_style(Theme::dim())),
            Span::styled(
                format!("[{}/{}] ", i + 1, total),
                selected_style(Theme::dim()),
            ),
            Span::styled(format!("{icon} "), selected_style(item_style)),
            Span::styled(content, selected_style(item_style)),
        ];
        if let Some(s) = todo_stats.get(&item.id) {
            spans.push(Span::styled(
                format!(
                    " ⏱ {} ↑{} ↓{}",
                    format_hms(s.elapsed_secs()),
                    fmt_tokens(s.input_tokens),
                    fmt_tokens(s.output_tokens)
                ),
                selected_style(Theme::dim()),
            ));
        }
        lines.push(Line::from(spans));
    }

    let mut next_scroll = detail_scroll;
    let mut next_max_scroll: u16 = 0;
    if focused && detail_open {
        if let Some(item) = selected_item
            .or_else(|| selected_todo_id.and_then(|id| items.iter().find(|item| item.id == id)))
        {
            (next_scroll, next_max_scroll) = push_todo_detail_lines(
                lines,
                item,
                messages,
                sub_agents,
                todo_logs.get(&item.id).map(Vec::as_slice).unwrap_or(&[]),
                todo_stats.get(&item.id),
                detail_scroll,
                max_content_width,
            );
        }
    }

    (next_scroll, next_max_scroll)
}

#[allow(clippy::too_many_arguments)]
fn push_todo_detail_lines(
    lines: &mut Vec<Line<'static>>,
    item: &wyj_tools::todo::TodoItem,
    messages: &[ChatMessage],
    sub_agents: &std::collections::BTreeMap<u64, SubAgentUiState>,
    log: &[TodoExecutionEntry],
    stats: Option<&TodoRuntimeStats>,
    detail_scroll: u16,
    max_content_width: usize,
) -> (u16, u16) {
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(max_content_width.saturating_sub(2))),
        Theme::border(),
    )));

    let status = match item.status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
    };
    let priority = item.priority.as_deref().unwrap_or("-");
    let detail_width = max_content_width.saturating_sub(4).max(1);
    let mut detail_lines: Vec<Line<'static>> = Vec::new();

    for l in wrap_line(
        &format!("id: {} · status: {status} · priority: {priority}", item.id),
        detail_width,
    ) {
        detail_lines.push(Line::from(Span::styled(format!("  {l}"), Theme::dim())));
    }
    if let Some(active) = item.active_form.as_deref() {
        for l in wrap_line(&format!("active: {active}"), detail_width) {
            detail_lines.push(Line::from(Span::styled(
                format!("  {l}"),
                Theme::tool_result(),
            )));
        }
    }
    for l in wrap_line(&format!("task: {}", item.content), detail_width) {
        detail_lines.push(Line::from(Span::styled(
            format!("  {l}"),
            Theme::tool_result(),
        )));
    }
    if let Some(s) = stats {
        detail_lines.push(Line::from(Span::styled(
            format!(
                "  ⏱ {} · ↑{} ↓{}",
                format_hms(s.elapsed_secs()),
                fmt_tokens(s.input_tokens),
                fmt_tokens(s.output_tokens)
            ),
            Theme::dim(),
        )));
    }
    detail_lines.push(Line::from(Span::styled(
        "  execution",
        Style::default()
            .fg(Theme::CLAUDE)
            .add_modifier(Modifier::BOLD),
    )));

    if log.is_empty() {
        detail_lines.push(Line::from(Span::styled(
            "  no captured execution events yet",
            Theme::dim(),
        )));
    } else {
        let empty_detail_scroll = HashMap::new();
        let ctx = ChatRenderCtx {
            max_content_width: detail_width,
            selected_message_id: None,
            sub_agents,
            spinner_frame: 0,
            message_detail_scroll: &empty_detail_scroll,
            detail_viewport_rows: MESSAGE_DETAIL_DEFAULT_ROWS,
        };
        for entry in log {
            match entry {
                TodoExecutionEntry::Message(id) => {
                    if let Some(msg) = messages.iter().find(|m| m.id == *id) {
                        let mut msg = msg.clone();
                        if matches!(
                            msg.role,
                            MessageRole::Thinking
                                | MessageRole::ToolResult
                                | MessageRole::BashOutput
                        ) {
                            msg.expanded = true;
                        }
                        let mut msg_lines = Vec::new();
                        let mut is_first_user = true;
                        render_chat_message(&mut msg_lines, &msg, 0, &mut is_first_user, &ctx);
                        push_prefixed_lines(&mut detail_lines, msg_lines, "  ");
                    }
                }
                TodoExecutionEntry::Note(text) => {
                    for l in wrap_line(text, detail_width) {
                        detail_lines.push(Line::from(Span::styled(format!("  {l}"), Theme::dim())));
                    }
                }
            }
        }
    }

    let viewport = detail_lines.len().min(12);
    let max_scroll = detail_lines.len().saturating_sub(viewport);
    let scroll = (detail_scroll as usize).min(max_scroll);
    for line in detail_lines.into_iter().skip(scroll).take(viewport) {
        lines.push(line);
    }
    (scroll as u16, max_scroll.min(u16::MAX as usize) as u16)
}

fn push_prefixed_lines(
    lines: &mut Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    prefix: &'static str,
) {
    for line in body {
        let mut spans = vec![Span::raw(prefix)];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
}

fn push_inline_ask_question_lines(
    lines: &mut Vec<Line<'static>>,
    dlg: &AskQuestionDialog,
    max_content_width: usize,
) {
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

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  AskQuestion", Style::default().fg(Theme::CLAUDE)),
        Span::styled("  ", Theme::dim()),
        Span::styled(
            title,
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(max_content_width.saturating_sub(2))),
        Theme::border(),
    )));

    let body_width = max_content_width.saturating_sub(4).max(1);
    let body = match dlg.stage {
        AskQuestionStage::Answering { index } => build_answering_lines(dlg, index, body_width),
        AskQuestionStage::Overview { index } => build_overview_lines(dlg, index, body_width),
    };
    push_prefixed_lines(lines, body, "  ");
}

struct MessageRangeRenderArgs<'a> {
    messages: &'a [ChatMessage],
    range: std::ops::Range<usize>,
    max_content_width: usize,
    sub_agents: &'a std::collections::BTreeMap<u64, SubAgentUiState>,
    spinner_frame: usize,
    selected_message_id: Option<u64>,
    message_detail_scroll: &'a HashMap<u64, u16>,
    detail_viewport_rows: usize,
}

/// 渲染 `messages[range]` 为 `Vec<Line>`。`is_first_user` 携带"区间开始前是否已
/// 出现过 User 消息"的状态，供完整消息流与流式尾部共用同一份逻辑。
fn render_message_range(
    args: MessageRangeRenderArgs<'_>,
    selected_line: &mut Option<usize>,
    is_first_user: &mut bool,
) -> Vec<Line<'static>> {
    let MessageRangeRenderArgs {
        messages,
        range,
        max_content_width,
        sub_agents,
        spinner_frame,
        selected_message_id,
        message_detail_scroll,
        detail_viewport_rows,
    } = args;
    let ctx = ChatRenderCtx {
        max_content_width,
        selected_message_id,
        sub_agents,
        spinner_frame,
        message_detail_scroll,
        detail_viewport_rows,
    };
    let mut lines = vec![];
    let mut i = range.start;
    while i < range.end {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let before = lines.len();
        let msg = &messages[i];
        if matches!(msg.role, MessageRole::ToolCall) {
            if let Some(next) = messages.get(i + 1) {
                if i + 1 < range.end
                    && matches!(next.role, MessageRole::ToolResult)
                    && next.sequence_no == msg.sequence_no
                {
                    let selected = selected_message_id == Some(next.id);
                    let selected_style = |st: Style| -> Style {
                        if selected {
                            st.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                        } else {
                            st
                        }
                    };
                    render_tool_result_block(
                        &mut lines,
                        Some(msg),
                        next,
                        selected,
                        &selected_style,
                        &ctx,
                    );
                    trim_trailing_blank_lines(&mut lines);
                    if selected {
                        *selected_line = Some(before);
                    }
                    i += 2;
                    continue;
                }
            }
        }
        render_chat_message(&mut lines, msg, i, is_first_user, &ctx);
        trim_trailing_blank_lines(&mut lines);
        if selected_message_id == Some(msg.id) {
            *selected_line = Some(before);
        }
        i += 1;
    }
    lines
}

fn draw_chat(f: &mut Frame, state: &mut AppState, area: Rect) {
    let max_content_width = area.width.saturating_sub(2) as usize;
    let lines = build_pending_chat_lines(state, max_content_width);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });

    let content_line_count = para.line_count(area.width.max(1)).min(u16::MAX as usize) as u16;
    let content_height = content_line_count.min(area.height).max(1);
    let content_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
    };

    let max_scroll = content_line_count.saturating_sub(content_height) as usize;
    state.chat_view_height = content_height as usize;
    state.chat_max_scroll = max_scroll;
    if state.chat_follow_tail {
        state.chat_scroll = max_scroll;
    } else if let Some(line) = state.selected_message_line {
        let height = content_height as usize;
        match state.selected_message_anchor {
            Some(ChatSelectionAnchor::Top) => {
                state.chat_scroll = line.min(max_scroll);
            }
            Some(ChatSelectionAnchor::Bottom) => {
                state.chat_scroll = line
                    .saturating_sub(height.saturating_sub(1))
                    .min(max_scroll);
            }
            None => {
                if line < state.chat_scroll {
                    state.chat_scroll = line;
                } else if line >= state.chat_scroll + height {
                    state.chat_scroll = line.saturating_sub(height.saturating_sub(1));
                }
            }
        }
        state.chat_scroll = state.chat_scroll.min(max_scroll);
    } else {
        state.chat_scroll = state.chat_scroll.min(max_scroll);
    }
    let scroll_y = state.chat_scroll.min(u16::MAX as usize) as u16;
    let para = para.scroll((scroll_y, 0));
    f.render_widget(para, content_area);

    if state.unseen_messages {
        let hint = Paragraph::new(Line::from(Span::styled(
            " new messages below · cmd+down ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        let w = 30.min(area.width);
        let hint_area = Rect {
            x: area.x + area.width.saturating_sub(w),
            y: area.y + area.height.saturating_sub(1),
            width: w,
            height: 1,
        };
        f.render_widget(hint, hint_area);
    }
}

fn push_wrapped_detail_line(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    text: &str,
    style: Style,
    width: usize,
) {
    let prefix = if label.is_empty() {
        "  ".to_string()
    } else {
        format!("  {label}: ")
    };
    let available = width.saturating_sub(prefix.len()).max(1);
    for (idx, wrapped) in wrap_line(text, available).into_iter().enumerate() {
        let line_prefix = if idx == 0 {
            prefix.clone()
        } else {
            "  ".to_string() + &" ".repeat(label.len() + 2)
        };
        lines.push(Line::from(Span::styled(
            format!("{line_prefix}{wrapped}"),
            style,
        )));
    }
}

fn push_sub_agent_trace_lines(
    lines: &mut Vec<Line<'static>>,
    events: &[TraceEvent],
    detail_width: usize,
) {
    for ev in events {
        match ev {
            TraceEvent::Started {
                agent_type,
                description,
                background,
                ..
            } => {
                let bg = if *background { " background" } else { "" };
                push_wrapped_detail_line(
                    lines,
                    "start",
                    &format!("{agent_type}({description}){bg}"),
                    Theme::dim(),
                    detail_width,
                );
            }
            TraceEvent::ToolStart {
                tool_name,
                input_json,
                truncated,
            } => {
                push_wrapped_detail_line(
                    lines,
                    "tool",
                    &format!(
                        "{tool_name} input{}",
                        if *truncated { " [truncated]" } else { "" }
                    ),
                    Theme::tool_call(),
                    detail_width,
                );
                for l in input_json.lines() {
                    push_wrapped_detail_line(lines, "in", l, Theme::dim(), detail_width);
                }
            }
            TraceEvent::ToolEnd {
                tool_name,
                is_error,
                elapsed_secs,
                output,
                truncated,
            } => {
                let style = if *is_error {
                    Theme::error()
                } else {
                    Theme::tool_result()
                };
                push_wrapped_detail_line(
                    lines,
                    "done",
                    &format!(
                        "{tool_name} · {}{}",
                        format_hms(*elapsed_secs),
                        if *truncated { " [truncated]" } else { "" }
                    ),
                    style,
                    detail_width,
                );
                if output.is_empty() {
                    push_wrapped_detail_line(
                        lines,
                        "out",
                        "(no output)",
                        Theme::dim(),
                        detail_width,
                    );
                } else {
                    for l in output.lines() {
                        push_wrapped_detail_line(lines, "out", l, style, detail_width);
                    }
                }
            }
            TraceEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                push_wrapped_detail_line(
                    lines,
                    "usage",
                    &format!(
                        "↑{} ↓{}",
                        fmt_tokens(*input_tokens),
                        fmt_tokens(*output_tokens)
                    ),
                    Theme::dim(),
                    detail_width,
                );
            }
            TraceEvent::Done {
                result,
                is_error,
                elapsed_secs,
            } => {
                let style = if *is_error {
                    Theme::error()
                } else {
                    Theme::tool_result()
                };
                push_wrapped_detail_line(
                    lines,
                    "result",
                    &format!(
                        "{} · {}",
                        if *is_error { "failed" } else { "done" },
                        format_hms(*elapsed_secs)
                    ),
                    style,
                    detail_width,
                );
                for l in result.lines() {
                    push_wrapped_detail_line(lines, "", l, style, detail_width);
                }
            }
        }
    }
}

/// 底部固定面板：子 Agent 总览，支持上下选中 + 展开详情（工具流水 + 最终结果/状态）。
/// 标题 `agents [N]`；Running=spinner / Done=✓ / Failed=✗ / Interrupted=⊘；
/// 列表区固定行数上限（SUB_AGENT_LIST_MAX）+ 滚动窗口；运行期间自动显示，
/// 全部完成后自动收起，历史详情仍可通过 `/subagents` 主动打开。
fn draw_sub_agents_panel(f: &mut Frame, state: &mut AppState, area: Rect) {
    // 按 id 升序（BTreeMap 天然启动顺序），用 owned Vec 避免和后面对 state 的可变借用冲突
    let ids: Vec<u64> = state.sub_agents.keys().copied().collect();

    let title = wyj_i18n::tr_fmt(
        "subagent.panel_title",
        &[("count", ids.len().to_string().as_str())],
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

    let selected_idx = state
        .selected_sub_agent
        .and_then(|id| ids.iter().position(|&i| i == id));
    let detail_open = state.sub_agent_detail_open && selected_idx.is_some();

    let list_rows = (ids.len().min(SUB_AGENT_LIST_MAX) as u16).min(inner.height);
    let (list_area, detail_area) = if detail_open && inner.height > list_rows {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(list_rows), Constraint::Min(1)])
            .split(inner);
        (chunks[0], Some(chunks[1]))
    } else {
        (inner, None)
    };

    let max_content_width = list_area.width.saturating_sub(2) as usize;
    let max_show = (list_area.height as usize).max(1);
    let start = match selected_idx {
        Some(idx) if idx >= max_show => idx - max_show + 1,
        _ => 0,
    };

    let mut lines: Vec<Line<'static>> = vec![];
    for (row_i, id) in ids.iter().skip(start).take(max_show).enumerate() {
        let Some(s) = state.sub_agents.get(id) else {
            continue;
        };
        let is_selected = selected_idx == Some(start + row_i);
        let sel_bg = |mut st: Style| -> Style {
            if is_selected {
                st = st.bg(Theme::SELECTED_BG);
            }
            st
        };

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
            Span::styled(format!(" a{id} "), sel_bg(Theme::dim())),
            Span::styled(format!("{icon} "), sel_bg(item_style)),
            Span::styled(
                truncate_line(&head, max_content_width.saturating_sub(30)),
                sel_bg(item_style),
            ),
            Span::styled(stats, sel_bg(Theme::dim())),
        ];
        if let Some(cur) = &s.current_tool {
            spans.push(Span::styled(
                format!(" {}", truncate_line(cur, 30)),
                sel_bg(Theme::dim()),
            ));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), list_area);

    // 详情区：先列工具调用流水，再按状态展示"运行中/已中断/最终结果"
    if let (Some(detail_area), Some(idx)) = (detail_area, selected_idx) {
        let id = ids[idx];
        let trace_events = state.sub_agent_trace_events(id);
        if let Some(s) = state.sub_agents.get(&id) {
            let detail_width = detail_area.width.saturating_sub(2) as usize;
            let mut detail_lines: Vec<Line<'static>> = vec![];
            if let Some(events) = trace_events.as_deref().filter(|events| !events.is_empty()) {
                push_sub_agent_trace_lines(&mut detail_lines, events, detail_width.max(20));
            } else {
                push_sub_agent_tool_log(&mut detail_lines, &s.tool_log, detail_width.max(20));
                match s.status {
                    SubAgentStatus::Running => {
                        let cur = s.current_tool.as_deref().unwrap_or("…");
                        detail_lines.push(Line::from(Span::styled(
                            format!("  {}{}", wyj_i18n::tr("subagent.detail_running"), cur),
                            Style::default().fg(Color::Cyan),
                        )));
                    }
                    SubAgentStatus::Interrupted => {
                        detail_lines.push(Line::from(Span::styled(
                            format!("  ✗ {}", wyj_i18n::tr("subagent.interrupted")),
                            Theme::error(),
                        )));
                    }
                    SubAgentStatus::Done | SubAgentStatus::Failed => {
                        let style = if s.status == SubAgentStatus::Failed {
                            Theme::error()
                        } else {
                            Theme::tool_result()
                        };
                        if let Some(result) = &s.final_result {
                            for l in result.lines() {
                                detail_lines.push(Line::from(Span::styled(
                                    format!(
                                        "  {}",
                                        truncate_line(l, detail_width.saturating_sub(2))
                                    ),
                                    style,
                                )));
                            }
                        }
                    }
                }
            }

            let text = Text::from(detail_lines);
            let dw = detail_area.width.max(1);
            let para = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
            let total = para.line_count(dw).min(u16::MAX as usize) as u16;
            let visible_height = detail_area.height;
            let max_scroll = total.saturating_sub(visible_height);
            state.sub_agent_detail_max_scroll = max_scroll;
            // clamp 后写回，防止按键累加超过 max_scroll 导致"到顶/底后要多按几次才生效"
            let clamped = state.sub_agent_detail_scroll.min(max_scroll);
            state.sub_agent_detail_scroll = clamped;
            let scroll = max_scroll.saturating_sub(clamped);
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0)),
                detail_area,
            );
        }
    }
}

// ─── 输入框 ──────────────────────────────────────────────────────────────────

fn thinking_elapsed_secs(state: &AppState) -> f64 {
    state
        .turn_start_time
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0)
}

fn thinking_status_label(state: &AppState) -> String {
    if let Some(task) = state
        .current_todos
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|t| t.status == TodoStatus::InProgress)
    {
        return task
            .active_form
            .as_deref()
            .unwrap_or(&task.content)
            .to_string();
    }

    if let Some(op) = state.current_op.as_deref() {
        let name = op.split(['(', ' ']).next().unwrap_or(op);
        return match name {
            "Read" => "Reading file".to_string(),
            "Grep" | "Glob" => "Searching code".to_string(),
            "Bash" => "Running command".to_string(),
            "Edit" | "MultiEdit" | "Write" => "Editing file".to_string(),
            "TodoWrite" => "Updating todos".to_string(),
            "Agent" => "Delegating task".to_string(),
            "WebFetch" | "WebSearch" => "Browsing".to_string(),
            "ExitPlanMode" => "Preparing plan".to_string(),
            other => format!("Running {other}"),
        };
    }

    if state.permission_dialog.is_some() {
        return "Waiting for approval".to_string();
    }
    if state.plan_dialog.is_some() {
        return "Reviewing plan".to_string();
    }
    if !state.pending_queue.is_empty() {
        return "Queuing message".to_string();
    }

    const PHRASES: &[&str] = &[
        "大象装进冰箱",
        "先打开冰箱门",
        "把大象装进去",
        "关上冰箱门",
        "大象装进冰箱了",
    ];
    let phase = (thinking_elapsed_secs(state) / 4.0) as usize;
    PHRASES[phase % PHRASES.len()].to_string()
}

fn thinking_status_suffix(state: &AppState) -> &'static str {
    match ((thinking_elapsed_secs(state) / 0.55) as usize) % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
}

fn draw_input(f: &mut Frame, state: &AppState, input: &InputBox, area: Rect) {
    // 主输入框被 /mcp /skills /plugins 面板借用做配置输入时（见 `InputOwner`），
    // 整个函数改画 dialog 自己的 live_input 草稿，边框/标题变色 + 嵌入提示文字，
    // 提交/取消后 `state.input_owner` 归 None，下一帧自动恢复聊天输入框外观。
    if let Some(owner) = state.input_owner {
        let borrowed = owner.live_input(state);
        let (prompt, color) = owner.prompt();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(color))
            .title(Span::styled(
                format!(" {prompt} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if let Some(ib) = borrowed {
            let wrap_width = inner.width as usize;
            let lines: Vec<Line> = ib
                .display_lines()
                .iter()
                .flat_map(|l| InputBox::wrap_for_render(l, wrap_width))
                .map(Line::from)
                .collect();
            f.render_widget(
                Paragraph::new(Text::from(lines)).style(Theme::input_box()),
                inner,
            );
            let (vis_row, vis_col) = ib.cursor_visual_pos(wrap_width);
            let cursor_x = (inner.x + vis_col as u16).min(inner.x + inner.width.saturating_sub(1));
            let cursor_y = (inner.y + vis_row as u16).min(inner.y + inner.height.saturating_sub(1));
            f.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
        return;
    }

    // 检测 ! bash 模式：首行以 ! 开头且不在思考中
    let is_bang = !state.is_thinking
        && input
            .display_lines()
            .first()
            .map(|l| l.starts_with('!'))
            .unwrap_or(false);

    let (mut title_content, title_style) = if state.is_thinking {
        let frame = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
        let op = thinking_status_label(state);
        let suffix = thinking_status_suffix(state);
        (
            format!(" {frame} {op}{suffix} · esc to interrupt "),
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
    title_content = truncate_line(&title_content, area.width.saturating_sub(2) as usize);

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
                            .bg(Theme::SELECTED_BG)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        desc_str,
                        Style::default()
                            .bg(Theme::SELECTED_BG)
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

fn interaction_usage_text(state: &AppState) -> String {
    format!(
        "total ↑{} ↓{}",
        fmt_tokens(state.total_input_tokens),
        fmt_tokens(state.total_output_tokens)
    )
}

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
    let usage_text = interaction_usage_text(state);

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
        " ◆ {}{} · [{}] {}% · {} · {}",
        state.model_name, mode_str, bar, pct_int, usage_text, cwd_str
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
        Span::styled(format!("] {}% · ", pct_int), Theme::dim()),
        Span::styled(usage_text, Style::default().fg(Color::Cyan)),
        Span::styled(format!(" · {}", cwd_str), Theme::dim()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right_help, right_style),
        Span::raw(" "),
    ]);

    let line = Line::from(spans);
    let para = Paragraph::new(line).style(Theme::status_bar());
    f.render_widget(para, area);
}

// ─── 权限对话框（分级授权） ────────────────────────────────────────────────────

/// 权限确认框：Category A 底部常驻面板（对齐 Claude Code Inline 模式），
/// 直接吃 `bottom_panel_size` 已经算好的 `area`（`chunks[1]`），不再自己居中
/// 计算浮层 Rect、不再用 `Clear`——它高频出现在几乎每次 Edit/Write/Bash 调用，
/// 走全屏浮层切换会非常闪烁。
fn draw_permission_dialog(f: &mut Frame, dlg: &PermissionDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::permission_dialog())
        .title(Span::styled(
            wyj_i18n::tr("dialog.permission_title"),
            Theme::permission_dialog(),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let preview = truncate_chars(&dlg.action_summary, (inner.width as usize * 3).max(80));

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
            if wyj_tools::ctx::is_project_approve_once_tool(&dlg.tool_name) {
                wyj_i18n::tr("dialog.permission_hint_project_once")
            } else {
                wyj_i18n::tr("dialog.permission_hint")
            },
            Theme::highlight(),
        )),
    ];

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

/// 项目级 MCP server 信任确认面板：只在 TUI 启动后台连接阶段检测到未信任的
/// `.wyj-code/mcp.toml`/`.mcp.json` server 时出现一次，样式对齐
/// `draw_permission_dialog`（同为安全相关确认）。
fn draw_project_trust_panel(f: &mut Frame, servers: &[wyj_config::McpServerConfig], area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::permission_dialog())
        .title(Span::styled(
            wyj_i18n::tr("dialog.project_trust_title"),
            Theme::permission_dialog(),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::raw(wyj_i18n::tr(
        "dialog.project_trust_intro",
    )))];
    for server in servers {
        let target = server
            .command
            .as_deref()
            .map(|c| {
                if server.args.is_empty() {
                    c.to_string()
                } else {
                    format!("{c} {}", server.args.join(" "))
                }
            })
            .or_else(|| server.url.clone())
            .unwrap_or_default();
        lines.push(Line::from(Span::raw(truncate_chars(
            &format!("  · {}: {target}", server.name),
            inner.width as usize,
        ))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("dialog.project_trust_hint"),
        Theme::highlight(),
    )));

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

// ─── AskQuestion 底部面板 ─────────────────────────────────────────────────────

/// 计划正文已经作为 `MessageRole::PlanProposal` 消息并入聊天流展示（见
/// `render_chat_message`），这里只剩固定 3 行的三选一选择器：批准 / 继续规划 /
/// 手动输入反馈，↑/↓ 选中 + Enter 确认，对齐 `draw_permission_dialog` 的贴底样式。
fn draw_plan_approval_panel(f: &mut Frame, dlg: &PlanApprovalDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(
            " 📋 计划已就绪 · ↑/↓ 选择 · Enter 确认 ",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cursor = dlg.cursor();
    let max_w = inner.width as usize;
    let mut lines: Vec<Line<'static>> = vec![
        plan_option_row(0, cursor, "批准并切换至执行模式", max_w),
        plan_option_row(1, cursor, "继续规划", max_w),
    ];
    if let Some(input) = dlg.freetext_input() {
        let text = input.lines.first().map(|s| s.as_str()).unwrap_or("");
        lines.push(Line::from(Span::styled(
            format!(
                "❯ 手动输入： {}_",
                truncate_line(text, max_w.saturating_sub(8))
            ),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(plan_option_row(2, cursor, "手动输入反馈…", max_w));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// 三选一面板里的单行选项：高亮项用 "▶ label"（Claude 主题色加粗），
/// 与 `build_answering_lines` 的 AskQuestion 单选样式保持一致。
fn plan_option_row(idx: usize, cursor: usize, label: &str, max_w: usize) -> Line<'static> {
    if idx == cursor {
        Line::from(Span::styled(
            format!("▶ {}", truncate_line(label, max_w.saturating_sub(2))),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            format!("  {}", truncate_line(label, max_w.saturating_sub(2))),
            Style::default().fg(Color::White),
        ))
    }
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

// ─── 会话选择器 ───────────────────────────────────────────────────────────────

fn draw_session_picker(f: &mut Frame, picker: &SessionPickerState, area: Rect) {
    let n_sessions = picker.sessions.len();
    // 显示项：1条"新建会话" + 1条分割线 + n条历史 + 1条分割线 + 1条提示 = n+4
    let height = ((n_sessions as u16 + 4).max(5)).min(area.height.saturating_sub(4));
    let width = (area.width * 4 / 5).clamp(50, 92);

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
                        .bg(Theme::SELECTED_BG)
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
    for (idx, key) in SETTINGS_FIELD_LABEL_KEYS
        .iter()
        .enumerate()
        .take(SETTINGS_FIELD_COUNT)
    {
        let label = wyj_i18n::tr(key);
        let selected = idx == dialog.selected;
        let value = dialog.draft.display_value(idx);

        let marker = if selected { "▶ " } else { "  " };
        let text = format!("{marker}{label:<label_width$}{value}");
        let text = truncate_line(&text, w);

        let style = if selected {
            Theme::selected_row()
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
            Theme::selected_row()
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

// ── MCP server 管理面板渲染：/mcp 命令触发 ─────────────────────────────────────

fn mcp_scope_label(scope: wyj_store::InstallScope) -> String {
    wyj_i18n::tr(match scope {
        wyj_store::InstallScope::Global => "mcp.dialog.scope_global",
        wyj_store::InstallScope::Project => "mcp.dialog.scope_project",
    })
}

fn mcp_package_command_preview(package: &wyj_store::mcp_install::PackageChoice) -> String {
    match package {
        wyj_store::mcp_install::PackageChoice::Npx { command, args, .. }
        | wyj_store::mcp_install::PackageChoice::Uvx { command, args, .. } => {
            format!("{command} {}", args.join(" "))
        }
        wyj_store::mcp_install::PackageChoice::Unsupported { .. } => String::new(),
    }
}

/// 列表类面板一次最多渲染的行数：超出时按光标位置滚动，保证选中行与底部
/// 分隔线/状态行/提示行始终可见，不会被过长的列表挤出屏幕。
const MAX_LIST_VIEWPORT: usize = 12;

/// 给定列表总行数、当前光标位置与可视窗口行数，计算滚动窗口起始下标
/// （光标始终落在窗口内；无需在 dialog 状态里额外维护 scroll_offset，
/// 每帧按光标位置重新计算即可）。
fn scroll_window_start(total: usize, cursor: usize, visible: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let cursor = cursor.min(total.saturating_sub(1));
    if cursor < visible {
        0
    } else {
        (cursor + 1 - visible).min(total.saturating_sub(visible))
    }
}

fn draw_mcp_dialog(
    f: &mut Frame,
    dialog: &McpDialog,
    conn_status: &HashMap<String, McpConnStatus>,
    area: Rect,
) {
    let rows = dialog.rows();
    let visible_rows = rows.len().clamp(1, MAX_LIST_VIEWPORT);
    let extra_lines: u16 = 3; // 分隔线 + 状态行 + 提示行
    let content_lines = visible_rows as u16 + extra_lines;
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let tab_label = wyj_i18n::tr(match dialog.tab {
        McpDialogTab::Installed => "mcp.dialog.tab_installed",
        McpDialogTab::Registries => "mcp.dialog.tab_registries",
        McpDialogTab::Browse => "mcp.dialog.tab_browse",
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} — {} ", wyj_i18n::tr("mcp.dialog.title"), tab_label),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
        let empty_key = match dialog.tab {
            McpDialogTab::Installed => "mcp.dialog.empty_installed",
            McpDialogTab::Registries => "mcp.dialog.empty_registries",
            McpDialogTab::Browse => "mcp.dialog.empty_browse",
        };
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr(empty_key),
            Theme::dim(),
        )));
    } else {
        let start = scroll_window_start(rows.len(), dialog.cursor, visible_rows);
        for (pos, row) in rows.iter().enumerate().skip(start).take(visible_rows) {
            let selected = pos == dialog.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Theme::selected_row()
            } else {
                Style::default().fg(Color::White)
            };
            let text = match (dialog.tab, row) {
                (McpDialogTab::Installed, FlatRow::Entry(idx)) => {
                    let row = &dialog.installed[*idx];
                    let enabled = row.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                    let enabled_marker = if enabled { "●" } else { "○" };
                    let scope_label = mcp_scope_label(row.scope);
                    let tag = if row.managed.as_ref().is_some_and(|m| m.is_managed()) {
                        format!(
                            "v{}",
                            row.managed
                                .as_ref()
                                .and_then(|m| m.version.clone())
                                .unwrap_or_default()
                        )
                    } else {
                        wyj_i18n::tr("mcp.dialog.unmanaged_tag")
                    };
                    let status_suffix = match conn_status.get(&row.config.name) {
                        Some(McpConnStatus::Connecting) => {
                            format!(" · {}", wyj_i18n::tr("mcp.status.connecting"))
                        }
                        Some(McpConnStatus::Connected { tool_count }) => format!(
                            " · {}",
                            wyj_i18n::tr_fmt(
                                "mcp.status.connected",
                                &[("count", &tool_count.to_string())]
                            )
                        ),
                        Some(McpConnStatus::Failed) => {
                            format!(" · {}", wyj_i18n::tr("mcp.status.failed"))
                        }
                        Some(McpConnStatus::TimedOut) => {
                            format!(" · {}", wyj_i18n::tr("mcp.status.timed_out"))
                        }
                        None => String::new(),
                    };
                    format!(
                        "{marker}{enabled_marker} {:<20} [{scope_label}] {tag}{status_suffix}",
                        row.config.name
                    )
                }
                (McpDialogTab::Registries, FlatRow::Entry(idx)) => {
                    let source = &dialog.registries[*idx];
                    let active_marker = if source.id == dialog.active_registry.id {
                        "★"
                    } else {
                        " "
                    };
                    format!(
                        "{marker}{active_marker} {} — {}",
                        source.name, source.base_url
                    )
                }
                (McpDialogTab::Registries, FlatRow::AddNew) => {
                    format!("{marker}{}", wyj_i18n::tr("mcp.dialog.add_registry_row"))
                }
                (McpDialogTab::Browse, FlatRow::AddNew) => {
                    let query = dialog.live_input.display_lines().join("");
                    if query.is_empty() {
                        format!(
                            "{marker}{} [{}] {}",
                            wyj_i18n::tr("mcp.dialog.search_label"),
                            dialog.active_registry.name,
                            wyj_i18n::tr("mcp.dialog.search_row_placeholder")
                        )
                    } else {
                        format!(
                            "{marker}{} [{}] {}",
                            wyj_i18n::tr("mcp.dialog.search_label"),
                            dialog.active_registry.name,
                            query
                        )
                    }
                }
                (McpDialogTab::Browse, FlatRow::Entry(idx)) => {
                    let server = &dialog.browse_results[*idx];
                    format!("{marker}{} — {}", server.name, server.description)
                }
                // Installed 没有 AddNew 行（新增 MCP server 走 Browse+安装），
                // rows() 保证不会产生这个组合，这里只是穷尽匹配。
                (McpDialogTab::Installed, FlatRow::AddNew) => String::new(),
            };
            let text = truncate_line(&text, w);
            lines.push(Line::from(Span::styled(text, style)));
        }
        if dialog.tab == McpDialogTab::Browse && matches!(dialog.overlay, McpOverlay::Searching) {
            lines.push(Line::from(Span::styled(
                wyj_i18n::tr("mcp.dialog.searching"),
                Theme::dim(),
            )));
        }
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(Span::styled(
            truncate_line(err, w),
            Theme::warning(),
        )));
    } else if let Some(status) = &dialog.status {
        lines.push(Line::from(Span::styled(
            truncate_line(status, w),
            Theme::dim(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    let hint_key = match dialog.tab {
        McpDialogTab::Installed => "mcp.dialog.hint_installed",
        McpDialogTab::Registries => "mcp.dialog.hint_registries",
        McpDialogTab::Browse => "mcp.dialog.hint_browse",
    };
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr(hint_key),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);

    if let Some(menu) = &dialog.menu {
        draw_action_menu(f, area, &wyj_i18n::tr("mcp.dialog.title"), menu);
    } else {
        draw_mcp_overlay(f, dialog, area);
    }
}

fn draw_mcp_overlay(f: &mut Frame, dialog: &McpDialog, area: Rect) {
    let overlay = &dialog.overlay;
    let (title, lines): (String, Vec<Line<'static>>) = match overlay {
        McpOverlay::None => return,
        McpOverlay::Searching => (
            wyj_i18n::tr("mcp.dialog.title"),
            vec![Line::from(Span::styled(
                wyj_i18n::tr("mcp.dialog.searching"),
                Theme::dim(),
            ))],
        ),
        McpOverlay::Upgrading { .. } => (
            wyj_i18n::tr("mcp.dialog.title"),
            vec![Line::from(Span::styled(
                wyj_i18n::tr("mcp.upgrade.in_progress"),
                Theme::dim(),
            ))],
        ),
        McpOverlay::Detail { title, lines } => (
            title.clone(),
            lines
                .iter()
                .map(|l| Line::from(Span::raw(l.clone())))
                .collect(),
        ),
        McpOverlay::InstallConfirm {
            server,
            package,
            scope,
        } => (
            wyj_i18n::tr("mcp.install.confirm_title"),
            vec![
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("mcp.install.name_label"), Theme::dim()),
                    Span::raw(server.name.clone()),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("mcp.install.scope_label"), Theme::dim()),
                    Span::raw(mcp_scope_label(*scope)),
                ]),
                Line::from(Span::styled(
                    wyj_i18n::tr("mcp.install.command_label"),
                    Theme::dim(),
                )),
                Line::from(format!("  {}", mcp_package_command_preview(package))),
                Line::from(""),
                Line::from(Span::styled(
                    wyj_i18n::tr("mcp.install.confirm_warning"),
                    Theme::warning(),
                )),
                Line::from(Span::styled(
                    wyj_i18n::tr("mcp.install.confirm_hint"),
                    Theme::highlight(),
                )),
            ],
        ),
        McpOverlay::AddRegistry => {
            // 文本内容现在借用底部主输入框（`dialog.live_input`），此浮层
            // 只保留提示文案，真正的输入渲染在底部输入框（借用态样式，见 draw_input）。
            (
                wyj_i18n::tr("mcp.registry.add_title"),
                vec![Line::from(Span::styled(
                    wyj_i18n::tr("mcp.registry.add_prompt"),
                    Theme::dim(),
                ))],
            )
        }
    };

    draw_confirm_box(f, &title, lines, area);
}

/// 通用居中确认弹框（安装/卸载/同步等 overlay 复用），布局对齐权限确认对话框风格。
///
/// 宽度/最小高度刻意与 `draw_mcp_dialog`/`draw_skills_dialog` 的基础面板保持一致的
/// 尺寸公式：这是同一次渲染里叠加在基础面板之上的第二层浮层，若尺寸随内容长度
/// 忽大忽小，上一帧遗留的边框/文字会在两者未重叠的区域露出（ratatui 按 Rect 增量
/// 清屏，不会清到 Rect 之外的旧内容）。用与基础面板相同的宽度公式 + 足够大的
/// 固定最小高度，确保浮层稳定覆盖住基础面板，切换 overlay 状态时也不会互相露底。
fn draw_confirm_box(f: &mut Frame, title: &str, lines: Vec<Line<'static>>, area: Rect) {
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);
    let height = (lines.len() as u16 + 2)
        .max(16)
        .min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::permission_dialog())
        .title(Span::styled(title.to_string(), Theme::permission_dialog()));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

// ── Skill 管理面板渲染：/skills 命令触发 ───────────────────────────────────────

fn draw_skills_dialog(f: &mut Frame, dialog: &SkillsDialog, area: Rect) {
    let rows = dialog.rows();
    let visible_rows = rows.len().clamp(1, MAX_LIST_VIEWPORT);
    let content_lines = visible_rows as u16 + 3; // 分隔线 + 状态行 + 提示行
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let tab_label = wyj_i18n::tr(match dialog.tab {
        SkillsDialogTab::Installed => "skills.dialog.tab_installed",
        SkillsDialogTab::Marketplaces => "skills.dialog.tab_marketplaces",
        SkillsDialogTab::Browse => "skills.dialog.tab_browse",
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} — {} ", wyj_i18n::tr("skills.dialog.title"), tab_label),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
        let empty_key = match dialog.tab {
            SkillsDialogTab::Installed => "skills.dialog.empty_installed",
            SkillsDialogTab::Marketplaces => "skills.dialog.empty_marketplaces",
            SkillsDialogTab::Browse => "skills.dialog.empty_marketplace_entries",
        };
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr(empty_key),
            Theme::dim(),
        )));
    } else {
        let start = scroll_window_start(rows.len(), dialog.cursor, visible_rows);
        for (pos, row) in rows.iter().enumerate().skip(start).take(visible_rows) {
            let selected = pos == dialog.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Theme::selected_row()
            } else {
                Style::default().fg(Color::White)
            };
            let text = match (dialog.tab, row) {
                (SkillsDialogTab::Installed, FlatRow::Entry(idx)) => {
                    let row = &dialog.installed[*idx];
                    let enabled = row.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                    let enabled_marker = if enabled { "●" } else { "○" };
                    let tag = if row.builtin {
                        wyj_i18n::tr("agents.builtin_tag")
                    } else if let Some(scope) = row.scope {
                        let scope_label = mcp_scope_label(scope);
                        if row.managed.as_ref().is_some_and(|m| m.is_managed()) {
                            format!(
                                "{scope_label} v{}",
                                row.managed
                                    .as_ref()
                                    .and_then(|m| m.version.clone())
                                    .unwrap_or_default()
                            )
                        } else {
                            format!(
                                "{scope_label} ({})",
                                wyj_i18n::tr("skills.dialog.unmanaged_tag")
                            )
                        }
                    } else {
                        String::new()
                    };
                    format!(
                        "{marker}{enabled_marker} {:<16} [{tag}] {}",
                        row.name, row.description
                    )
                }
                (SkillsDialogTab::Marketplaces, FlatRow::Entry(idx)) => {
                    format!("{marker}{}", dialog.marketplaces[*idx].git_url)
                }
                (SkillsDialogTab::Marketplaces, FlatRow::AddNew) => {
                    format!(
                        "{marker}{}",
                        wyj_i18n::tr("skills.dialog.add_marketplace_row")
                    )
                }
                (SkillsDialogTab::Browse, FlatRow::Entry(idx)) => {
                    let e = &dialog.browse_results[*idx];
                    format!("{marker}{} v{} — {}", e.name, e.version, e.description)
                }
                (SkillsDialogTab::Installed, FlatRow::AddNew)
                | (SkillsDialogTab::Browse, FlatRow::AddNew) => String::new(),
            };
            let text = truncate_line(&text, w);
            lines.push(Line::from(Span::styled(text, style)));
        }
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(Span::styled(
            truncate_line(err, w),
            Theme::warning(),
        )));
    } else if let Some(status) = &dialog.status {
        lines.push(Line::from(Span::styled(
            truncate_line(status, w),
            Theme::dim(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    let hint_key = match dialog.tab {
        SkillsDialogTab::Installed => "skills.dialog.hint_installed",
        SkillsDialogTab::Marketplaces => "skills.dialog.hint_marketplaces",
        SkillsDialogTab::Browse => "skills.dialog.hint_browse",
    };
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr(hint_key),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);

    if let Some(menu) = &dialog.menu {
        draw_action_menu(f, area, &wyj_i18n::tr("skills.dialog.title"), menu);
    } else {
        draw_skills_overlay(f, &dialog.overlay, area);
    }
}

fn draw_skills_overlay(f: &mut Frame, overlay: &SkillsOverlay, area: Rect) {
    match overlay {
        SkillsOverlay::None => {}
        SkillsOverlay::AddMarketplace => {
            // 文本内容现在借用底部主输入框（`dialog.live_input`），此浮层
            // 只保留提示文案，真正的输入渲染在底部输入框（借用态样式，见 draw_input）。
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("skills.marketplace.add_prompt"),
                Theme::dim(),
            ))];
            draw_confirm_box(
                f,
                &wyj_i18n::tr("skills.marketplace.add_title"),
                lines,
                area,
            );
        }
        SkillsOverlay::Syncing { .. } => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("skills.dialog.syncing"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("skills.dialog.title"), lines, area);
        }
        SkillsOverlay::InstallConfirm { entry, scope, .. } => {
            let lines = vec![
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.install.name_label"), Theme::dim()),
                    Span::raw(entry.name.clone()),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.install.scope_label"), Theme::dim()),
                    Span::raw(mcp_scope_label(*scope)),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.install.path_label"), Theme::dim()),
                    Span::raw(entry.path.clone()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    wyj_i18n::tr("skills.install.confirm_hint"),
                    Theme::highlight(),
                )),
            ];
            draw_confirm_box(
                f,
                &wyj_i18n::tr("skills.install.confirm_title"),
                lines,
                area,
            );
        }
        SkillsOverlay::Upgrading { .. } => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("skills.upgrade.in_progress"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("skills.dialog.title"), lines, area);
        }
        SkillsOverlay::Detail { title, lines } => {
            let lines = lines
                .iter()
                .map(|l| Line::from(Span::raw(l.clone())))
                .collect();
            draw_confirm_box(f, title, lines, area);
        }
    }
}

fn draw_plugins_dialog(f: &mut Frame, dialog: &PluginsDialog, area: Rect) {
    let rows = dialog.rows();
    let visible_rows = rows.len().clamp(1, MAX_LIST_VIEWPORT);
    let content_lines = visible_rows as u16 + 3;
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let tab_label = wyj_i18n::tr(match dialog.tab {
        PluginsDialogTab::Installed => "plugins.dialog.tab_installed",
        PluginsDialogTab::Marketplaces => "plugins.dialog.tab_marketplaces",
        PluginsDialogTab::Browse => "plugins.dialog.tab_browse",
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} — {} ", wyj_i18n::tr("plugins.dialog.title"), tab_label),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
        let empty_key = match dialog.tab {
            PluginsDialogTab::Installed => "plugins.dialog.empty_installed",
            PluginsDialogTab::Marketplaces => "plugins.dialog.empty_marketplaces",
            PluginsDialogTab::Browse => "plugins.dialog.empty_marketplace_entries",
        };
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr(empty_key),
            Theme::dim(),
        )));
    } else {
        let start = scroll_window_start(rows.len(), dialog.cursor, visible_rows);
        for (pos, row) in rows.iter().enumerate().skip(start).take(visible_rows) {
            let selected = pos == dialog.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Theme::selected_row()
            } else {
                Style::default().fg(Color::White)
            };
            let text = match (dialog.tab, row) {
                (PluginsDialogTab::Installed, FlatRow::Entry(idx)) => {
                    let row = &dialog.installed[*idx];
                    let enabled_marker = if row.enabled { "●" } else { "○" };
                    let source_label = if row.is_local_dev {
                        wyj_i18n::tr("plugins.dialog.local_dev_tag")
                    } else {
                        format!(
                            "{} v{}",
                            mcp_scope_label(row.scope),
                            row.version.clone().unwrap_or_default()
                        )
                    };
                    format!(
                        "{marker}{enabled_marker} {:<20} [{source_label}] {}",
                        row.name, row.resource_summary
                    )
                }
                (PluginsDialogTab::Installed, FlatRow::AddNew) => {
                    format!("{marker}{}", wyj_i18n::tr("plugins.dialog.add_local_row"))
                }
                (PluginsDialogTab::Marketplaces, FlatRow::Entry(idx)) => {
                    let m = &dialog.marketplaces[*idx];
                    let label = if m.display_name.is_empty() || m.display_name == m.location {
                        m.location.clone()
                    } else {
                        format!("{} ({})", m.display_name, m.location)
                    };
                    format!("{marker}{label}")
                }
                (PluginsDialogTab::Marketplaces, FlatRow::AddNew) => {
                    format!(
                        "{marker}{}",
                        wyj_i18n::tr("plugins.dialog.add_marketplace_row")
                    )
                }
                (PluginsDialogTab::Browse, FlatRow::Entry(idx)) => {
                    let e = &dialog.browse_results[*idx];
                    let name = e.manifest.name.clone().unwrap_or_else(|| "?".to_string());
                    let version = e.manifest.version.clone().unwrap_or_default();
                    let description = e.manifest.description.clone().unwrap_or_default();
                    format!("{marker}{name} v{version} — {description}")
                }
                (PluginsDialogTab::Browse, FlatRow::AddNew) => String::new(),
            };
            let text = truncate_line(&text, w);
            lines.push(Line::from(Span::styled(text, style)));
        }
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if let Some(err) = &dialog.error {
        lines.push(Line::from(Span::styled(
            truncate_line(err, w),
            Theme::warning(),
        )));
    } else if let Some(status) = &dialog.status {
        lines.push(Line::from(Span::styled(
            truncate_line(status, w),
            Theme::dim(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    let hint_key = match dialog.tab {
        PluginsDialogTab::Installed => "plugins.dialog.hint_installed",
        PluginsDialogTab::Marketplaces => "plugins.dialog.hint_marketplaces",
        PluginsDialogTab::Browse => "plugins.dialog.hint_browse",
    };
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr(hint_key),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);

    if let Some(menu) = &dialog.menu {
        draw_action_menu(f, area, &wyj_i18n::tr("plugins.dialog.title"), menu);
    } else {
        draw_plugins_overlay(f, &dialog.overlay, area);
    }
}

fn draw_plugins_overlay(f: &mut Frame, overlay: &PluginOverlay, area: Rect) {
    match overlay {
        PluginOverlay::None => {}
        PluginOverlay::AddMarketplace => {
            // 文本内容现在借用底部主输入框（`dialog.live_input`），此浮层
            // 只保留提示文案，真正的输入渲染在底部输入框（借用态样式，见 draw_input）。
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("plugins.marketplace.add_prompt"),
                Theme::dim(),
            ))];
            draw_confirm_box(
                f,
                &wyj_i18n::tr("plugins.marketplace.add_title"),
                lines,
                area,
            );
        }
        PluginOverlay::Syncing { .. } => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("plugins.dialog.syncing"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("plugins.dialog.title"), lines, area);
        }
        PluginOverlay::InstallConfirm { entry, scope, .. } => {
            let name = entry
                .manifest
                .name
                .clone()
                .unwrap_or_else(|| "?".to_string());
            let lines = vec![
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("plugins.install.name_label"), Theme::dim()),
                    Span::raw(name),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("plugins.install.scope_label"), Theme::dim()),
                    Span::raw(mcp_scope_label(*scope)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    wyj_i18n::tr("plugins.install.confirm_hint"),
                    Theme::highlight(),
                )),
            ];
            draw_confirm_box(
                f,
                &wyj_i18n::tr("plugins.install.confirm_title"),
                lines,
                area,
            );
        }
        PluginOverlay::Installing => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("plugins.install.in_progress"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("plugins.dialog.title"), lines, area);
        }
        PluginOverlay::InstallReport { report } => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("plugins.install.name_label"), Theme::dim()),
                    Span::raw(report.name.clone()),
                ]),
                Line::from(wyj_i18n::tr_fmt(
                    "plugins.install.resource_counts",
                    &[
                        ("cmd", &report.skill_count.to_string()),
                        ("agent", &report.agent_count.to_string()),
                        ("mcp", &report.mcp_count.to_string()),
                    ],
                )),
            ];
            if !report.skipped_capabilities.is_empty() {
                lines.push(Line::from(Span::styled(
                    wyj_i18n::tr_fmt(
                        "plugins.install.skipped_capabilities_label",
                        &[
                            ("count", &report.skipped_capabilities.len().to_string()),
                            ("names", &report.skipped_capabilities.join(", ")),
                        ],
                    ),
                    Theme::warning(),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                wyj_i18n::tr("plugins.install.restart_required_hint"),
                Theme::highlight(),
            )));
            draw_confirm_box(
                f,
                &wyj_i18n::tr("plugins.install.report_title"),
                lines,
                area,
            );
        }
        PluginOverlay::Upgrading { .. } => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("plugins.upgrade.in_progress"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("plugins.dialog.title"), lines, area);
        }
        PluginOverlay::AddLocalPlugin => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("plugins.local.add_prompt"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("plugins.local.add_title"), lines, area);
        }
        PluginOverlay::Detail { title, lines } => {
            let lines = lines
                .iter()
                .map(|l| Line::from(Span::raw(l.clone())))
                .collect();
            draw_confirm_box(f, title, lines, area);
        }
    }
}

fn draw_agents_dialog(f: &mut Frame, dialog: &mut AgentsDialog, area: Rect) {
    let list_rows = dialog.defs.len().clamp(1, MAX_LIST_VIEWPORT);
    let detail_rows = if dialog.detail_open { 12 } else { 0 };
    let content_lines = list_rows as u16 + detail_rows + 3;
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 120).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("agents.dialog.title")),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let chunks = if dialog.detail_open && inner.height > list_rows as u16 + 3 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(list_rows as u16),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(0),
                Constraint::Length(2),
            ])
            .split(inner)
    };

    let list_area = chunks[0];
    let w = list_area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if dialog.defs.is_empty() {
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr("agents.dialog.empty"),
            Theme::dim(),
        )));
    } else {
        let start = scroll_window_start(dialog.defs.len(), dialog.selected, list_rows);
        for (pos, def) in dialog.defs.iter().enumerate().skip(start).take(list_rows) {
            let selected = pos == dialog.selected;
            let marker = if selected { "▶ " } else { "  " };
            let source = if def.builtin {
                wyj_i18n::tr("agents.builtin_tag")
            } else {
                def.source
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            };
            let text = truncate_line(
                &format!("{marker}{} — {}  [{source}]", def.name, def.description),
                w,
            );
            let style = if selected {
                Theme::selected_row()
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)), list_area);

    if dialog.detail_open {
        if let Some(def) = dialog.selected_def() {
            let detail_area = chunks[1];
            let detail_width = detail_area.width.saturating_sub(2) as usize;
            let mut detail_lines: Vec<Line<'static>> = Vec::new();
            let tools = def
                .tools
                .as_ref()
                .map(|t| t.join(", "))
                .unwrap_or_else(|| wyj_i18n::tr("agents.tools_all"));
            let source = if def.builtin {
                wyj_i18n::tr("agents.builtin_tag")
            } else {
                def.source
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            };
            let model = def.model.as_deref().unwrap_or("-");
            for l in [
                format!("name: {}", def.name),
                format!("description: {}", def.description),
                format!("model: {model}"),
                format!("tools: {tools}"),
                format!("source: {source}"),
                "system prompt:".to_string(),
            ] {
                for wrapped in wrap_line(&l, detail_width.max(1)) {
                    detail_lines.push(Line::from(Span::styled(
                        format!("  {wrapped}"),
                        Theme::dim(),
                    )));
                }
            }
            if def.system_prompt.is_empty() {
                detail_lines.push(Line::from(Span::styled("  -", Theme::tool_result())));
            } else {
                for l in def.system_prompt.lines() {
                    for wrapped in wrap_line(l, detail_width.max(1)) {
                        detail_lines.push(Line::from(Span::styled(
                            format!("  {wrapped}"),
                            Theme::tool_result(),
                        )));
                    }
                }
            }
            let para = Paragraph::new(Text::from(detail_lines.clone())).wrap(Wrap { trim: false });
            let total = para.line_count(detail_area.width.max(1));
            let max_scroll = total.saturating_sub(detail_area.height as usize);
            dialog.detail_scroll = (dialog.detail_scroll as usize).min(max_scroll) as u16;
            f.render_widget(
                Paragraph::new(Text::from(detail_lines))
                    .wrap(Wrap { trim: false })
                    .scroll((dialog.detail_scroll, 0)),
                detail_area,
            );
        }
    }

    let footer_area = chunks[2];
    let hint = if dialog.detail_open {
        wyj_i18n::tr("agents.dialog.hint_detail")
    } else {
        wyj_i18n::tr("agents.dialog.hint")
    };
    let footer = vec![
        Line::from(Span::styled(
            "─".repeat(footer_area.width as usize),
            Theme::border(),
        )),
        Line::from(Span::styled(
            truncate_line(&hint, footer_area.width as usize),
            Theme::dim(),
        )),
    ];
    f.render_widget(Paragraph::new(Text::from(footer)), footer_area);
}

fn draw_extensions_dialog(f: &mut Frame, dialog: &mut ExtensionsDialog, area: Rect) {
    let list_rows = dialog.records.len().clamp(1, MAX_LIST_VIEWPORT);
    let detail_rows = if dialog.detail_open { 10 } else { 0 };
    let footer_rows = if dialog.confirm.is_some() { 3 } else { 2 };
    let content_lines = list_rows as u16 + detail_rows + footer_rows + 1;
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 9 / 10).clamp(72, 140).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            " Extensions ",
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(list_rows as u16),
            Constraint::Min(1),
            Constraint::Length(footer_rows),
        ])
        .split(inner);

    let list_area = chunks[0];
    let mut lines = Vec::new();
    if dialog.records.is_empty() {
        lines.push(Line::from(Span::styled(
            "No managed or configured extensions found.",
            Theme::dim(),
        )));
    } else {
        let start = scroll_window_start(dialog.records.len(), dialog.selected, list_rows);
        for (pos, record) in dialog
            .records
            .iter()
            .enumerate()
            .skip(start)
            .take(list_rows)
        {
            let selected = pos == dialog.selected;
            let marker = if selected { "▶ " } else { "  " };
            let status = if !record.enabled {
                "disabled"
            } else if record.effective {
                "active"
            } else {
                "inactive"
            };
            let version = record.version.as_deref().unwrap_or("-");
            let text = truncate_line(
                &format!(
                    "{marker}{:<30} {:<7} {:<8} {:<12} v{}",
                    record.id,
                    format!("{:?}", record.scope).to_lowercase(),
                    status,
                    record.health,
                    version
                ),
                list_area.width as usize,
            );
            let style = if selected {
                Theme::selected_row()
            } else if !record.enabled {
                Theme::dim()
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)), list_area);

    if dialog.detail_open {
        if let Some(record) = dialog.selected_record() {
            let detail_area = chunks[1];
            let width = detail_area.width.saturating_sub(2) as usize;
            let mut detail_lines = Vec::new();
            let mut fields = vec![
                format!("id: {}", record.id),
                format!("kind: {:?}", record.kind),
                format!("scope: {:?}", record.scope),
                format!("health: {}", record.health),
                format!(
                    "enabled: {}  effective: {}",
                    record.enabled, record.effective
                ),
                format!("version: {}", record.version.as_deref().unwrap_or("-")),
                format!("source: {}", record.source.as_deref().unwrap_or("-")),
                format!(
                    "commit: {}  digest: {}",
                    record.commit.as_deref().unwrap_or("-"),
                    record.digest.as_deref().unwrap_or("-")
                ),
            ];
            if !record.dependencies.is_empty() {
                fields.push(format!("dependencies: {}", record.dependencies.join(", ")));
            }
            for (key, value) in &record.details {
                fields.push(format!("{key}: {value}"));
            }
            for field in fields {
                for wrapped in wrap_line(&field, width.max(1)) {
                    detail_lines.push(Line::from(Span::styled(
                        format!("  {wrapped}"),
                        Theme::dim(),
                    )));
                }
            }
            let total = Paragraph::new(Text::from(detail_lines.clone()))
                .line_count(detail_area.width.max(1));
            let max_scroll = total.saturating_sub(detail_area.height as usize);
            dialog.detail_scroll = (dialog.detail_scroll as usize).min(max_scroll) as u16;
            f.render_widget(
                Paragraph::new(Text::from(detail_lines))
                    .wrap(Wrap { trim: false })
                    .scroll((dialog.detail_scroll, 0)),
                detail_area,
            );
        }
    } else if let Some(error) = &dialog.error {
        f.render_widget(
            Paragraph::new(Span::styled(error.clone(), Theme::error())).wrap(Wrap { trim: true }),
            chunks[1],
        );
    }

    let footer_area = chunks[2];
    let mut footer = vec![Line::from(Span::styled(
        "─".repeat(footer_area.width as usize),
        Theme::border(),
    ))];
    if let Some(action) = dialog.confirm {
        let id = dialog
            .selected_record()
            .map(|record| record.id.as_str())
            .unwrap_or("selected resource");
        footer.push(Line::from(Span::styled(
            truncate_line(
                &format!(
                    "Confirm {} {}? [y/Enter] yes · [n/Esc] cancel",
                    ExtensionsDialog::action_label(action),
                    id
                ),
                footer_area.width as usize,
            ),
            Style::default().fg(Color::Yellow),
        )));
    } else {
        footer.push(Line::from(Span::styled(
            truncate_line(
                "↑/↓ select · Enter detail · e enable · d disable · x remove · r refresh · Esc close",
                footer_area.width as usize,
            ),
            Theme::dim(),
        )));
    }
    f.render_widget(Paragraph::new(Text::from(footer)), footer_area);
}

/// 一键导入面板渲染（/import 命令触发）
fn draw_import_dialog(f: &mut Frame, dialog: &ImportDialog, area: Rect) {
    let is_report = matches!(dialog.stage, ImportStage::Report(_));
    let body_rows = if is_report {
        let ImportStage::Report(outcome) = &dialog.stage else {
            unreachable!()
        };
        (outcome.applied.len()
            + outcome.overwritten.len()
            + outcome.shadow_warnings.len()
            + outcome.errors.len()
            + 6)
        .clamp(3, 20)
    } else {
        dialog.candidates.len().clamp(1, MAX_LIST_VIEWPORT)
    };
    let error_rows = (!dialog.scan_errors.is_empty() || dialog.error.is_some()) as usize;
    let height = ((body_rows + error_rows + 4) as u16).min(area.height.saturating_sub(2));
    let width = (area.width * 9 / 10).clamp(72, 132).min(area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::CLAUDE))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("import.dialog.title")),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);
    let body_area = chunks[0];
    let mut lines = Vec::new();

    if let ImportStage::Report(outcome) = &dialog.stage {
        if !outcome.applied.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} ({})",
                    wyj_i18n::tr("import.report.applied"),
                    outcome.applied.len()
                ),
                Style::default().fg(Color::Green),
            )));
            for item in &outcome.applied {
                lines.push(Line::from(Span::raw(format!("  ✓ {item}"))));
            }
        }
        if !outcome.overwritten.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} ({})",
                    wyj_i18n::tr("import.report.overwritten"),
                    outcome.overwritten.len()
                ),
                Style::default().fg(Color::Yellow),
            )));
            for item in &outcome.overwritten {
                lines.push(Line::from(Span::raw(format!("  ↻ {item}"))));
            }
        }
        if !outcome.shadow_warnings.is_empty() {
            lines.push(Line::from(Span::styled(
                wyj_i18n::tr("import.report.shadowed_note"),
                Style::default().fg(Color::Yellow),
            )));
            for item in &outcome.shadow_warnings {
                lines.push(Line::from(Span::styled(
                    truncate_line(&format!("  ≫ {item}"), body_area.width as usize),
                    Theme::dim(),
                )));
            }
        }
        if !outcome.errors.is_empty() {
            lines.push(Line::from(Span::styled(
                wyj_i18n::tr("import.report.errors"),
                Theme::error(),
            )));
            for item in &outcome.errors {
                lines.push(Line::from(Span::styled(
                    truncate_line(&format!("  ✗ {item}"), body_area.width as usize),
                    Theme::error(),
                )));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                wyj_i18n::tr("import.report.nothing"),
                Theme::dim(),
            )));
        }
    } else if dialog.candidates.is_empty() {
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr("import.dialog.empty"),
            Theme::dim(),
        )));
    } else {
        let visible = dialog.candidates.len().clamp(1, MAX_LIST_VIEWPORT);
        let start = scroll_window_start(dialog.candidates.len(), dialog.cursor, visible);
        for (pos, candidate) in dialog
            .candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let selected = pos == dialog.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let checkbox = if dialog.checked.contains(&pos) {
                "[x]"
            } else {
                "[ ]"
            };
            let source = match candidate.source_app {
                wyj_store::import::ImportSourceApp::Codex => "codex",
                wyj_store::import::ImportSourceApp::Claude => "claude",
            };
            let mut flags = Vec::new();
            if candidate.conflict.is_some() {
                flags.push(wyj_i18n::tr("import.label.conflict"));
            }
            if candidate.shadowed {
                flags.push(wyj_i18n::tr("import.label.shadowed"));
            }
            let text = truncate_line(
                &format!(
                    "{marker}{checkbox} {:<6} {:<32} {source:<6} → {:<7} {}",
                    candidate.kind.as_str(),
                    candidate.name,
                    format!("{:?}", candidate.scope).to_lowercase(),
                    flags.join(" ")
                ),
                body_area.width as usize,
            );
            let style = if selected {
                Theme::selected_row()
            } else if candidate.conflict.is_some() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    for err in dialog.scan_errors.iter().chain(dialog.error.iter()) {
        lines.push(Line::from(Span::styled(
            truncate_line(&format!("! {err}"), body_area.width as usize),
            Theme::error(),
        )));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), body_area);

    let footer_area = chunks[1];
    let hint = if is_report {
        wyj_i18n::tr("import.report.hint")
    } else {
        wyj_i18n::tr("import.dialog.hint")
    };
    let footer = vec![
        Line::from(Span::styled(
            "─".repeat(footer_area.width as usize),
            Theme::border(),
        )),
        Line::from(Span::styled(
            truncate_line(&hint, footer_area.width as usize),
            Theme::dim(),
        )),
    ];
    f.render_widget(Paragraph::new(Text::from(footer)), footer_area);
}

/// 分组管理面板渲染（/model 无参命令触发）
fn draw_profile_dialog(
    f: &mut Frame,
    dialog: &ProfileDialog,
    input_owner: Option<InputOwner>,
    area: Rect,
) {
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

    // 当前哪一行正被借用（重命名/字段编辑），借用中的行不再显示实时输入内容——
    // 真正的输入渲染在底部主输入框（`draw_input` 的借用态分支），这里只放占位提示。
    let editing_row = match input_owner {
        Some(InputOwner::Profile(ProfileInputField::Rename { entry_idx })) => {
            Some(ProfileRow::Header(entry_idx))
        }
        Some(InputOwner::Profile(ProfileInputField::Field {
            entry_idx,
            field_idx,
        })) => Some(ProfileRow::Field(entry_idx, field_idx)),
        _ => None,
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let selected_row = row_idx == dialog.cursor;
        let editing = Some(*row) == editing_row;

        let text = match row {
            ProfileRow::Header(entry_idx) => {
                let entry = &dialog.entries[*entry_idx];
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
            ProfileRow::Field(entry_idx, f_idx) => {
                let entry = &dialog.entries[*entry_idx];
                let label = wyj_i18n::tr(PROFILE_FIELD_LABEL_KEYS[*f_idx]);
                let value = if editing {
                    wyj_i18n::tr("profile.dialog.editing_placeholder")
                } else if *f_idx == PROFILE_API_KEY_FIELD_IDX {
                    mask_secret(entry.text_value(*f_idx))
                } else {
                    entry.display_value(*f_idx)
                };
                let cursor = if selected_row { "▶" } else { " " };
                format!("{cursor}     {label:<label_width$}{value}")
            }
            ProfileRow::AddNew => {
                let cursor = if selected_row { "▶" } else { " " };
                format!("{cursor} + {}", wyj_i18n::tr("profile.dialog.add_new_row"))
            }
        };
        let text = truncate_line(&text, w);

        let style = if editing {
            Style::default().fg(Color::Black).bg(Theme::CLAUDE)
        } else if selected_row {
            Theme::selected_row()
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

    // 光标完全交给 draw_input 的借用态分支（借用中不在这里画光标）。

    if let Some(menu) = &dialog.menu {
        draw_action_menu(f, area, &wyj_i18n::tr("profile.title"), menu);
        return;
    }

    match &dialog.overlay {
        ProfileOverlay::None => {}
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
        ProfileOverlay::FetchingModels { .. } => {
            draw_profile_text_overlay(
                f,
                area,
                "profile.overlay.fetching_title",
                &wyj_i18n::tr("profile.fetch.in_progress"),
            );
        }
        ProfileOverlay::UnsavedChanges { selected } => {
            draw_profile_list_overlay(
                f,
                area,
                "profile.overlay.unsaved_title",
                vec![
                    wyj_i18n::tr("profile.overlay.unsaved_save_close"),
                    wyj_i18n::tr("profile.overlay.unsaved_discard_close"),
                    wyj_i18n::tr("profile.overlay.unsaved_cancel"),
                ],
                *selected,
            );
        }
    }
}

/// 布局与字段渲染逻辑同 `draw_profile_dialog`：Header 行汇总名字/cron/下次触发
/// 时间/最近一次运行状态，展开后逐字段列出，菜单/未保存确认/同步失败提示复用
/// `draw_action_menu`/`draw_profile_list_overlay`/`draw_profile_text_overlay`
/// 这三个已经与 Profile 无关的通用浮层绘制函数。
fn draw_schedule_dialog(
    f: &mut Frame,
    dialog: &ScheduleDialog,
    input_owner: Option<InputOwner>,
    area: Rect,
) {
    let rows = dialog.rows();
    let content_lines = rows.len() as u16 + 4;
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
            format!(" {} ", wyj_i18n::tr("schedule.title")),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let w = inner.width as usize;
    let label_width = 18usize;

    let editing_row = match input_owner {
        Some(InputOwner::Schedule(ScheduleInputField::Field {
            task_idx,
            field_idx,
        })) => Some(ScheduleRow::Field(task_idx, field_idx)),
        Some(InputOwner::Schedule(ScheduleInputField::Frequency { task_idx, .. })) => {
            Some(ScheduleRow::Field(task_idx, 2))
        }
        _ => None,
    };

    let now = chrono::Utc::now();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let selected_row = row_idx == dialog.cursor;
        let editing = Some(*row) == editing_row;

        let text = match row {
            ScheduleRow::Header(task_idx) => {
                let task = &dialog.tasks[*task_idx];
                let marker = if task.enabled { "●" } else { "○" };
                let expand_marker = if dialog.expanded == Some(*task_idx) {
                    "▾"
                } else {
                    "▸"
                };
                let cursor = if selected_row { "▶" } else { " " };
                let next_run = wyj_store::cron_sync::next_run_after(&task.cron, now)
                    .ok()
                    .flatten()
                    .map(|t| t.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| wyj_i18n::tr("schedule.dialog.invalid_cron_short"));
                let status = task
                    .last_run
                    .as_ref()
                    .map(|r| format!("{:?}", r.status))
                    .unwrap_or_else(|| "-".to_string());
                format!(
                    "{cursor} {expand_marker} {marker} {}  [{}]  next:{next_run}  last:{status}",
                    task.name, task.cron
                )
            }
            ScheduleRow::Field(task_idx, f_idx) => {
                let label = wyj_i18n::tr(SCHEDULE_FIELD_LABEL_KEYS[*f_idx]);
                let value = if editing {
                    wyj_i18n::tr("schedule.dialog.editing_placeholder")
                } else if *f_idx == SCHEDULE_FIELD_NOTIFY {
                    wyj_i18n::tr(if dialog.tasks[*task_idx].notify_on_failure {
                        "schedule.dialog.on"
                    } else {
                        "schedule.dialog.off"
                    })
                } else {
                    dialog.field_text(*task_idx, *f_idx)
                };
                let cursor = if selected_row { "▶" } else { " " };
                format!("{cursor}     {label:<label_width$}{value}")
            }
            ScheduleRow::AddNew => {
                let cursor = if selected_row { "▶" } else { " " };
                format!("{cursor} + {}", wyj_i18n::tr("schedule.dialog.add_new_row"))
            }
        };
        let text = truncate_line(&text, w);

        let style = if editing {
            Style::default().fg(Color::Black).bg(Theme::CLAUDE)
        } else if selected_row {
            Theme::selected_row()
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
        truncate_line(&wyj_i18n::tr("schedule.dialog.hint1"), w),
        Theme::dim(),
    )));
    lines.push(Line::from(Span::styled(
        truncate_line(&wyj_i18n::tr("schedule.dialog.hint2"), w),
        Theme::dim(),
    )));

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);

    if let Some(menu) = &dialog.menu {
        draw_action_menu(f, area, &wyj_i18n::tr("schedule.title"), menu);
        return;
    }

    match &dialog.overlay {
        ScheduleOverlay::None => {}
        ScheduleOverlay::UnsavedChanges { selected } => {
            draw_profile_list_overlay(
                f,
                area,
                "profile.overlay.unsaved_title",
                vec![
                    wyj_i18n::tr("profile.overlay.unsaved_save_close"),
                    wyj_i18n::tr("profile.overlay.unsaved_discard_close"),
                    wyj_i18n::tr("profile.overlay.unsaved_cancel"),
                ],
                *selected,
            );
        }
        ScheduleOverlay::SyncError { message } => {
            draw_profile_text_overlay(f, area, "schedule.overlay.sync_error_title", message);
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

/// 三个面板(`/mcp`/`/skills`/`/plugins`)共用的"操作菜单"浮层：选中某一行按 Enter
/// 弹出，Up/Down 选、Enter 确认、Esc 逐级返回。居中定位而非贴边行——列表可能
/// 滚动，行级定位在选中行贴近屏幕边缘时会被截断，故直接复用
/// `draw_profile_list_overlay` 的居中 Block + 高亮手法。
pub fn draw_action_menu<T, A: PartialEq>(
    f: &mut Frame,
    area: Rect,
    title: &str,
    menu: &ActionMenu<T, A>,
) {
    if let Some(confirming) = &menu.confirming {
        let label = menu
            .items
            .iter()
            .find(|it| &it.action == confirming)
            .map(|it| it.label.as_str())
            .unwrap_or("");
        draw_confirm_overlay(f, area, title, label);
        return;
    }

    let width = (area.width * 6 / 10).clamp(40, 90).min(area.width);
    let height = ((menu.items.len() as u16) + 2)
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
            format!(" {title} "),
            Style::default()
                .fg(Theme::CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;

    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let marker = if i == menu.selected { "▶ " } else { "  " };
            let mut text = format!("{marker}{}", item.label);
            if item.disabled {
                if let Some(reason) = &item.disabled_reason {
                    text.push_str(&format!(" ({reason})"));
                }
            }
            let text = truncate_line(&text, w);
            let style = if item.disabled {
                Theme::dim()
            } else if i == menu.selected {
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

/// `draw_action_menu` 里危险操作的二级确认子步骤："确定要 {label} 吗？"
fn draw_confirm_overlay(f: &mut Frame, area: Rect, title: &str, action_label: &str) {
    let width = (area.width * 6 / 10).clamp(40, 70).min(area.width);
    let height = 4u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::WARNING))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Theme::WARNING)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;

    let question = truncate_line(
        &wyj_i18n::tr_fmt(
            "dialog.action_menu.confirm_question",
            &[("action", action_label)],
        ),
        w,
    );
    let hint = truncate_line(&wyj_i18n::tr("dialog.action_menu.confirm_hint"), w);
    let lines = vec![
        Line::from(Span::styled(question, Theme::warning())),
        Line::from(Span::styled(hint, Theme::dim())),
    ];
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

#[cfg(test)]
mod tool_result_fold_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            wyj_config::Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        )
    }

    fn sub_agent(status: SubAgentStatus) -> SubAgentUiState {
        SubAgentUiState {
            agent_type: "general-purpose".to_string(),
            description: "test task".to_string(),
            background: false,
            status,
            started_at: Instant::now(),
            final_elapsed: (status != SubAgentStatus::Running).then_some(1.0),
            input_tokens: 10,
            output_tokens: 5,
            tool_calls: 0,
            current_tool: None,
            tool_log: vec![],
            has_result: status != SubAgentStatus::Running,
            finished_at: (status != SubAgentStatus::Running).then(Instant::now),
            final_result: (status == SubAgentStatus::Done).then(|| "done".to_string()),
        }
    }

    #[test]
    fn completed_sub_agents_do_not_keep_passive_panel_visible() {
        let mut state = make_state();
        state.sub_agents.insert(1, sub_agent(SubAgentStatus::Done));

        let (height, panel) = bottom_panel_size(&state, 40);

        assert_eq!(height, 0);
        assert!(matches!(panel, BottomPanel::None));
    }

    #[test]
    fn running_sub_agents_keep_automatic_panel_visible() {
        let mut state = make_state();
        state
            .sub_agents
            .insert(1, sub_agent(SubAgentStatus::Running));

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(height > 0);
        assert!(matches!(panel, BottomPanel::SubAgents));
    }

    #[test]
    fn pending_mcp_trust_shows_project_trust_panel() {
        let mut state = make_state();
        state.pending_mcp_trust = Some(vec![wyj_config::McpServerConfig {
            name: "postgres".to_string(),
            transport: wyj_config::McpTransport::Stdio,
            command: Some("npx".to_string()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
        }]);

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(height > 0);
        assert!(matches!(panel, BottomPanel::ProjectTrust));
    }

    #[test]
    fn permission_dialog_outranks_pending_mcp_trust() {
        let mut state = make_state();
        state.pending_mcp_trust = Some(vec![]);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.permission_dialog = Some(PermissionDialog {
            tool_name: "Bash".to_string(),
            action_summary: "ls".to_string(),
            response_tx: tx,
        });

        let (_, panel) = bottom_panel_size(&state, 40);

        assert!(matches!(panel, BottomPanel::Permission));
    }

    #[test]
    fn completed_sub_agents_remain_available_when_panel_is_opened_explicitly() {
        let mut state = make_state();
        state.sub_agents.insert(1, sub_agent(SubAgentStatus::Done));
        state.selected_sub_agent = Some(1);
        state.ui_focus = UiFocus::SubAgents;

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(height > 0);
        assert!(matches!(panel, BottomPanel::SubAgents));
    }

    #[test]
    fn rendering_short_todo_detail_writes_back_zero_max_scroll() {
        let mut state = make_state();
        state.current_todos = Some(vec![wyj_tools::todo::TodoItem {
            id: "a".to_string(),
            content: "short task".to_string(),
            status: wyj_tools::todo::TodoStatus::InProgress,
            priority: None,
            active_form: None,
        }]);
        state.selected_todo_id = Some("a".to_string());
        state.ui_focus = UiFocus::Todos;
        state.todo_detail_open = true;
        state.todo_detail_scroll = 0;
        // 未渲染前先塞一个陈旧的非零值，验证渲染层确实会重新计算并回写。
        state.todo_detail_max_scroll = 99;

        let _ = build_pending_chat_lines(&mut state, 100);

        assert_eq!(state.todo_detail_max_scroll, 0);
    }

    #[test]
    fn welcome_still_shows_before_startup_system_messages() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::system("MCP connected".to_string()));

        assert!(should_show_welcome(&state));

        let rendered = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        let welcome_idx = rendered
            .iter()
            .position(|line| line.contains("test-model"))
            .expect("welcome model line should be present");
        let system_idx = rendered
            .iter()
            .position(|line| line.contains("MCP connected"))
            .expect("system startup message should be present");
        assert!(welcome_idx < system_idx);
    }

    #[test]
    fn welcome_stays_before_first_real_conversation_message_until_frozen() {
        let mut state = make_state();
        let mut msg = ChatMessage::system("hello".to_string());
        msg.role = MessageRole::User;
        state.messages.push(msg);

        assert!(should_show_welcome(&state));

        let rendered = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        let welcome_idx = rendered
            .iter()
            .position(|line| line.contains("test-model"))
            .expect("welcome model line should be present");
        let user_idx = rendered
            .iter()
            .position(|line| line.contains("hello"))
            .expect("first user message should be present");
        assert!(welcome_idx < user_idx);
    }

    #[test]
    fn welcome_stays_before_first_streaming_answer_until_frozen() {
        let mut state = make_state();
        let mut msg = ChatMessage::system("hello".to_string());
        msg.role = MessageRole::User;
        state.messages.push(msg);
        state.streaming_buf = "streaming answer".to_string();

        let rendered = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        let welcome_idx = rendered
            .iter()
            .position(|line| line.contains("test-model"))
            .expect("welcome model line should be present");
        let user_idx = rendered
            .iter()
            .position(|line| line.contains("hello"))
            .expect("first user message should be present");
        let answer_idx = rendered
            .iter()
            .position(|line| line.contains("streaming answer"))
            .expect("streaming answer should be present");
        assert!(welcome_idx < user_idx);
        assert!(user_idx < answer_idx);
    }

    #[test]
    fn welcome_hides_when_frozen() {
        let mut state = make_state();
        state.welcome_frozen = true;
        assert!(!should_show_welcome(&state));
    }

    #[test]
    fn pending_chat_lines_start_after_frozen_boundary() {
        let mut state = make_state();
        state.welcome_frozen = true;
        let mut old_user = ChatMessage::system("old request".to_string());
        old_user.role = MessageRole::User;
        state.messages.push(old_user);
        let mut old_assistant = ChatMessage::system("old answer".to_string());
        old_assistant.role = MessageRole::Assistant;
        state.messages.push(old_assistant);
        let mut live_user = ChatMessage::system("live request".to_string());
        live_user.role = MessageRole::User;
        state.messages.push(live_user);
        state.frozen_up_to = 2;

        let rendered = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert!(!rendered.iter().any(|line| line.contains("old request")));
        assert!(!rendered.iter().any(|line| line.contains("old answer")));
        assert!(rendered.iter().any(|line| line.contains("live request")));
    }

    #[test]
    fn thinking_status_label_uses_tool_context_and_rotates_idle_copy() {
        let mut state = make_state();
        assert_eq!(thinking_status_label(&state), "大象装进冰箱");

        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_secs(9));
        assert_eq!(thinking_status_label(&state), "把大象装进去");

        state.current_op = Some("Read(crates/tui/src/render.rs)".to_string());
        assert_eq!(thinking_status_label(&state), "Reading file");

        state.current_op = Some("Bash(cargo test)".to_string());
        assert_eq!(thinking_status_label(&state), "Running command");
    }

    #[test]
    fn thinking_status_label_prefers_active_todo_name() {
        let mut state = make_state();
        state.current_op = Some("Read(crates/tui/src/render.rs)".to_string());
        state.current_todos = Some(vec![wyj_tools::todo::TodoItem {
            id: "a".to_string(),
            content: "检查交互焦点".to_string(),
            status: TodoStatus::InProgress,
            priority: Some("high".to_string()),
            active_form: Some("正在检查交互焦点".to_string()),
        }]);

        assert_eq!(thinking_status_label(&state), "正在检查交互焦点");
    }

    #[test]
    fn thinking_status_label_changes_slowly_not_per_spinner_frame() {
        let mut state = make_state();
        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_millis(900));
        let first = thinking_status_label(&state);

        state.spinner_frame = 8;
        assert_eq!(thinking_status_label(&state), first);

        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_secs(5));
        assert_ne!(thinking_status_label(&state), first);
    }

    #[test]
    fn thinking_status_suffix_animates_independently_from_label() {
        let mut state = make_state();
        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_millis(100));
        assert_eq!(thinking_status_suffix(&state), "");

        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_millis(700));
        assert_eq!(thinking_status_label(&state), "大象装进冰箱");
        assert_eq!(thinking_status_suffix(&state), ".");
    }

    #[test]
    fn interaction_usage_text_shows_session_totals_only() {
        let mut state = make_state();
        state.total_input_tokens = 12_345;
        state.total_output_tokens = 678;
        state.last_turn_elapsed_secs = Some(12.0);
        state.last_turn_input_tokens = 1_000;
        state.last_turn_output_tokens = 200;

        let text = interaction_usage_text(&state);
        assert_eq!(text, "total ↑12,345 ↓678");
    }

    #[test]
    fn interaction_usage_text_ignores_running_turn_delta() {
        let mut state = make_state();
        state.total_input_tokens = 150;
        state.total_output_tokens = 40;
        state.turn_start_time = Some(Instant::now());
        state.turn_start_input_tokens = 100;
        state.turn_start_output_tokens = 10;

        let text = interaction_usage_text(&state);
        assert_eq!(text, "total ↑150 ↓40");
    }

    #[test]
    fn strip_read_line_number_removes_numeric_tab_prefix() {
        assert_eq!(strip_read_line_number("42\tfn main() {}"), "fn main() {}");
        assert_eq!(strip_read_line_number("1\t"), "");
    }

    #[test]
    fn strip_read_line_number_leaves_non_matching_lines_untouched() {
        // read.rs 结尾追加的提示行不带 "数字\t" 前缀，不应被误伤
        assert_eq!(
            strip_read_line_number("（共 100 行，已显示 0–50）"),
            "（共 100 行，已显示 0–50）"
        );
        // 没有 tab 的普通行
        assert_eq!(strip_read_line_number("no tab here"), "no tab here");
        // tab 前不是纯数字
        assert_eq!(strip_read_line_number("a1\tcontent"), "a1\tcontent");
    }

    #[test]
    fn strip_summary_duplicate_line_drops_only_first_nonempty_line() {
        let lines = vec!["", "first", "second", "third"];
        assert_eq!(
            strip_summary_duplicate_line(&lines, true),
            vec!["", "second", "third"]
        );
        assert_eq!(strip_summary_duplicate_line(&lines, false), lines);
    }

    #[test]
    fn wrap_line_splits_without_dropping_content() {
        let s = "0 8 * * * /usr/bin/env PATH=/opt/homebrew/bin:/usr/local/bin/usr/bin:/bin /Users/foo/venv/bin/python /Users/foo/script.py";
        let wrapped = wrap_line(s, 20);
        assert!(wrapped
            .iter()
            .all(|l| l.chars().map(char_display_width).sum::<usize>() <= 20));
        assert_eq!(wrapped.join(""), s, "换行不应丢失或改变任何字符");
    }

    #[test]
    fn wrap_line_short_string_stays_single_line() {
        assert_eq!(wrap_line("short", 20), vec!["short".to_string()]);
    }

    #[test]
    fn wrap_line_never_splits_wide_char_across_lines() {
        let s = "中".repeat(10);
        let wrapped = wrap_line(&s, 5);
        for l in &wrapped {
            assert!(l.chars().map(char_display_width).sum::<usize>() <= 5);
        }
        assert_eq!(wrapped.join(""), s);
    }

    #[test]
    fn completed_tool_call_and_result_render_as_single_compact_block() {
        let call = ChatMessage {
            id: 1,
            role: MessageRole::ToolCall,
            content: "Read(crates/tui/src/render.rs)".to_string(),
            is_error: false,
            elapsed_secs: None,
            sequence_no: Some(1),
            tool_name: Some("Read".to_string()),
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        };
        let result = ChatMessage {
            id: 2,
            role: MessageRole::ToolResult,
            content: "1\tfirst\n2\tsecond\n3\tthird".to_string(),
            is_error: false,
            elapsed_secs: Some(0.2),
            sequence_no: Some(1),
            tool_name: Some("Read".to_string()),
            display_summary: "read 3 lines".to_string(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        };
        let messages = vec![call, result];
        let mut selected_line = None;
        let mut is_first_user = true;
        let empty_detail_scroll = HashMap::new();
        let rendered = render_message_range(
            MessageRangeRenderArgs {
                messages: &messages,
                range: 0..messages.len(),
                max_content_width: 100,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                selected_message_id: Some(2),
                message_detail_scroll: &empty_detail_scroll,
                detail_viewport_rows: MESSAGE_DETAIL_DEFAULT_ROWS,
            },
            &mut selected_line,
            &mut is_first_user,
        );
        let rendered_text = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered_text.len(), 1);
        assert!(rendered_text[0].contains("Read("));
        assert!(rendered_text[0].contains("⎿ read 3 lines"));
        assert!(rendered_text[0].contains("ctrl+o expand"));
        assert_eq!(selected_line, Some(0));
    }

    #[test]
    fn message_blocks_have_one_blank_line_between_them() {
        let messages = vec![
            ChatMessage {
                id: 1,
                role: MessageRole::Assistant,
                content: "first response".to_string(),
                is_error: false,
                elapsed_secs: None,
                sequence_no: None,
                tool_name: None,
                display_summary: String::new(),
                summary_is_first_line: false,
                expanded: false,
                sub_agent_id: None,
                md_cache: std::cell::RefCell::new(None),
            },
            ChatMessage {
                id: 2,
                role: MessageRole::Assistant,
                content: "second response".to_string(),
                is_error: false,
                elapsed_secs: None,
                sequence_no: None,
                tool_name: None,
                display_summary: String::new(),
                summary_is_first_line: false,
                expanded: false,
                sub_agent_id: None,
                md_cache: std::cell::RefCell::new(None),
            },
        ];
        let mut selected_line = None;
        let mut is_first_user = true;
        let empty_detail_scroll = HashMap::new();
        let rendered = render_message_range(
            MessageRangeRenderArgs {
                messages: &messages,
                range: 0..messages.len(),
                max_content_width: 100,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                selected_message_id: Some(2),
                message_detail_scroll: &empty_detail_scroll,
                detail_viewport_rows: MESSAGE_DETAIL_DEFAULT_ROWS,
            },
            &mut selected_line,
            &mut is_first_user,
        );
        let rendered_text = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered_text,
            vec!["  first response", "", "  ▶   second response"]
        );
        assert_eq!(selected_line, Some(2));
    }

    #[test]
    fn completed_tool_call_pair_stays_compact_before_next_block_separator() {
        let call = ChatMessage {
            id: 1,
            role: MessageRole::ToolCall,
            content: "Read(crates/tui/src/render.rs)".to_string(),
            is_error: false,
            elapsed_secs: None,
            sequence_no: Some(1),
            tool_name: Some("Read".to_string()),
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        };
        let result = ChatMessage {
            id: 2,
            role: MessageRole::ToolResult,
            content: "1\tfirst\n2\tsecond".to_string(),
            is_error: false,
            elapsed_secs: Some(0.2),
            sequence_no: Some(1),
            tool_name: Some("Read".to_string()),
            display_summary: "read 2 lines".to_string(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        };
        let next = ChatMessage {
            id: 3,
            role: MessageRole::Assistant,
            content: "after tool".to_string(),
            is_error: false,
            elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        };
        let messages = vec![call, result, next];
        let mut selected_line = None;
        let mut is_first_user = true;
        let empty_detail_scroll = HashMap::new();
        let rendered = render_message_range(
            MessageRangeRenderArgs {
                messages: &messages,
                range: 0..messages.len(),
                max_content_width: 100,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                selected_message_id: None,
                message_detail_scroll: &empty_detail_scroll,
                detail_viewport_rows: MESSAGE_DETAIL_DEFAULT_ROWS,
            },
            &mut selected_line,
            &mut is_first_user,
        );
        let rendered_text = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered_text.len(), 3);
        assert!(rendered_text[0].contains("Read("));
        assert!(rendered_text[0].contains("⎿ read 2 lines"));
        assert!(rendered_text[1].trim().is_empty());
        assert_eq!(rendered_text[2], "  after tool");
    }

    #[test]
    fn thinking_is_cleaned_and_folded_to_five_lines() {
        let thinking = ChatMessage {
            id: 1,
            role: MessageRole::Thinking,
            content: "\n\none\n\ntwo\nthree\n\nfour\nfive\nsix\nseven\n".to_string(),
            is_error: false,
            elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        };
        let messages = vec![thinking];
        let mut selected_line = None;
        let mut is_first_user = true;
        let empty_detail_scroll = HashMap::new();
        let rendered = render_message_range(
            MessageRangeRenderArgs {
                messages: &messages,
                range: 0..messages.len(),
                max_content_width: 100,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                selected_message_id: Some(1),
                message_detail_scroll: &empty_detail_scroll,
                detail_viewport_rows: MESSAGE_DETAIL_DEFAULT_ROWS,
            },
            &mut selected_line,
            &mut is_first_user,
        );
        let rendered_text = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered_text.len(), 6);
        assert!(rendered_text[0].contains("thinking · 7 lines"));
        assert!(rendered_text[0].contains("ctrl+o expand"));
        assert!(rendered_text.iter().any(|line| line.contains("one")));
        assert!(rendered_text.iter().any(|line| line.contains("five")));
        assert!(!rendered_text.iter().any(|line| line.contains("six")));
        assert!(rendered_text.iter().all(|line| !line.trim().is_empty()));
        assert_eq!(selected_line, Some(0));
    }

    #[test]
    fn expanded_bash_output_uses_detail_viewport() {
        let mut state = make_state();
        state.chat_view_height = 30;
        state.messages.push(ChatMessage {
            id: 1,
            role: MessageRole::BashOutput,
            content: (1..=30)
                .map(|i| format!("line-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            is_error: false,
            elapsed_secs: Some(0.2),
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: true,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        });
        state.message_detail_scroll.insert(1, 0);

        let rendered = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert!(rendered.iter().any(|line| line.contains("line-12")));
        assert!(!rendered.iter().any(|line| line.contains("line-20")));
        assert!(rendered.iter().any(|line| line.contains("pgup/pgdn")));
    }
}
