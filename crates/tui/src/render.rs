//! 对话渲染与布局

use crate::app::{
    fmt_tokens, format_hms, ActionMenu, AppState, AskQuestionDialog, AskQuestionStage, Attachment,
    ChatMessage, ExecModeConfirmDialog, FlatRow, InProgressAnswer, InputOwner, McpConnStatus,
    McpDialog, McpDialogTab, McpOverlay, MemoryDialog, MemoryRow, MessageRole, PermissionDialog,
    PlanApprovalDialog, PluginOverlay, PluginsDialog, PluginsDialogTab, ProfileDialog,
    ProfileInputField, ProfileOverlay, ProfileRow, SessionPickerState, SettingsDialog,
    SkillsDialog, SkillsDialogTab, SkillsOverlay, SubAgentStatus, SubAgentUiState, SubToolLine,
    TodoRuntimeStats, PROFILE_API_KEY_FIELD_IDX, PROFILE_FIELD_LABEL_KEYS, SETTINGS_FIELD_COUNT,
    SETTINGS_FIELD_LABEL_KEYS,
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
pub const TOOL_RESULT_FOLD_LINES: usize = 5;

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

/// 判断一条 ToolResult 的内容是否「可折叠」：非 Edit/Write（永不折叠，走独立 diff 渲染）
/// 且去重后（见 [`strip_summary_duplicate_line`]）正文行数超过 [`TOOL_RESULT_FOLD_LINES`]。
/// 纯函数不依赖 `ChatMessage`，调用方（render.rs 的 last_collapsible_idx 预扫描 /
/// app.rs 的 Ctrl+O 处理）各自按需叠加 `!expanded` 等额外条件。
pub fn is_collapsible_tool_result_content(
    content: &str,
    tool_name: Option<&str>,
    summary_is_first_line: bool,
) -> bool {
    if matches!(tool_name, Some("Edit") | Some("Write")) {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    let lines: Vec<&str> = content.lines().collect();
    strip_summary_duplicate_line(&lines, summary_is_first_line).len() > TOOL_RESULT_FOLD_LINES
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
        BottomPanel::Permission => {
            if let Some(dlg) = &state.permission_dialog {
                draw_permission_dialog(f, dlg, chunks[1]);
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
}

/// 底部面板类型与高度
enum BottomPanel {
    None,
    Permission,
    ExecModeConfirm,
    PlanApproval,
    AskQuestion,
    SubAgents,
    TodoList,
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
    if state.exec_mode_confirm.is_some() {
        return (4u16.min(area_height), BottomPanel::ExecModeConfirm);
    }
    if state.plan_dialog.is_some() {
        // 计划正文已作为普通消息并入聊天流（获得终端原生 scrollback 滚动），
        // 这里只剩固定 3 行的三选一选择器，贴在输入框上方，宽度对齐 Permission。
        return (5u16.min(area_height), BottomPanel::PlanApproval);
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
    // 列表区固定行数上限 + 滚动窗口（本会话内全部保留，数量可能持续增长）；
    // 详情展开时追加详情区所需行数，整体按可用高度 70% 封顶，避免聊天区被挤没。
    let visible = state.visible_sub_agents();
    if !visible.is_empty() {
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

/// 主循环在决定 `Viewport::Inline` 高度前调用：计算除聊天区外、底部所有固定
/// UI（权限确认/AskQuestion 等底部面板、补全列表、附件条、输入框、状态行）
/// 加起来需要的总行数。必须和 [`draw`] 里的布局算法保持一致，否则 Inline
/// 高度会和实际渲染需要的不一致，要么裁掉内容要么留出多余空白。
pub(crate) fn fixed_footer_height(
    state: &AppState,
    input: &InputBox,
    term_width: u16,
    term_height: u16,
) -> u16 {
    let inner_width = term_width.saturating_sub(2) as usize;
    let input_height = (input.visual_height(inner_width) as u16 + 2).clamp(3, 10);
    let completion_height = if !state.file_completions.is_empty() {
        (state.file_completions.len() as u16 + 2).min(10)
    } else if !state.slash_completions.is_empty() {
        (state.slash_completions.len() as u16 + 2).min(8)
    } else {
        0u16
    };
    let attach_height: u16 = if state.pending_attachments.is_empty() {
        0
    } else {
        3
    };
    let (panel_height, _) = bottom_panel_size(state, term_height);
    panel_height + completion_height + attach_height + input_height + 1
}

/// 主循环在决定 `Viewport::Inline` 高度前调用：计算"待渲染聊天区"（欢迎页/
/// 尚未冻结的消息尾部/流式文本）在给定终端宽度下实际需要的可视行数，用于
/// 动态撑开/收缩 Inline viewport 的聊天区部分。
///
/// 按内容实际需要动态定高（而不是直接撑到终端整高）：实测 ratatui 在 tmux/
/// 部分终端下构造一个接近整个屏幕高的 Inline viewport 时，内部依赖的终端
/// 光标位置查询有相当概率出现结果与实际渲染不一致（构造时内部状态完全一致
/// 但视觉结果时对时不对，且概率不低），表现不只是"贴不到底部"，更严重时
/// 输入框里刚打的字会完全不可见（逻辑上已收到，只是没画出来）。这比"没有
/// 贴底"这个外观问题严重得多，所以放弃"始终撑满终端高度"，改回按内容实际
/// 需要动态定高——足够高的终端下输入框仍贴不到最底部，是已知的、暂时接受
/// 的限制，而不是一个可以简单修掉的 bug。
pub(crate) fn pending_chat_visual_height(state: &AppState, term_width: u16) -> u16 {
    let max_content_width = term_width.saturating_sub(2) as usize;
    let lines = build_pending_chat_lines(state, max_content_width);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    para.line_count(term_width.max(1)).min(u16::MAX as usize) as u16
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
    /// 最后一条「可折叠」ToolResult 的下标（Ctrl+O 实际会切换的那一条），
    /// 见 [`last_collapsible_tool_result_idx`]。
    last_collapsible_idx: Option<usize>,
    sub_agents: &'a std::collections::BTreeMap<u64, SubAgentUiState>,
    spinner_frame: usize,
}

/// 找出最后一条「可折叠」的 ToolResult 索引 —— 即 Ctrl+O 实际会切换的那一条
/// （与 app.rs 的 Ctrl+O 处理用同一判定 is_collapsible_tool_result_content，不区分当前是否
/// 已展开，这样无论该条目当前折叠还是展开，都能正确显示对应的 "ctrl+o to expand/collapse"
/// 提示；若在此额外排除 m.expanded，会导致展开后立刻脱离该索引，"[ctrl+o to collapse]"
/// 提示永远无法显示）。其余可折叠的历史结果改用静默 ⋯N 标记，避免提示与快捷键行为错位。
pub(crate) fn last_collapsible_tool_result_idx(messages: &[ChatMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            if !matches!(m.role, MessageRole::ToolResult) {
                return false;
            }
            is_collapsible_tool_result_content(
                &m.content,
                m.tool_name.as_deref(),
                m.summary_is_first_line,
            )
        })
        .map(|(i, _)| i)
}

/// 渲染单条消息（追加到 `lines`）。从 `draw_chat` 提炼出来，供"实时重绘的待定
/// 消息尾部"与后续"一次性冻结进真实 scrollback 的历史消息前缀"两条路径共用，
/// 保证两边渲染逻辑不会 drift。
fn render_chat_message(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    msg_idx: usize,
    is_first_user: &mut bool,
    ctx: &ChatRenderCtx,
) {
    let max_content_width = ctx.max_content_width;
    match msg.role {
        MessageRole::User => {
            if !*is_first_user {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "─".repeat(max_content_width.min(60)),
                    Theme::dim(),
                )));
            }
            *is_first_user = false;

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
                // 已定稿消息的 markdown 渲染结果按宽度缓存：避免每帧对
                // 全部历史重跑 markdown 解析（长对话下的主要渲染开销）
                let mut cache = msg.md_cache.borrow_mut();
                match cache.as_ref() {
                    Some((w, cached)) if *w == max_content_width => {
                        lines.extend(cached.iter().cloned());
                    }
                    _ => {
                        let mut fresh: Vec<Line<'static>> = vec![];
                        render_markdown(&mut fresh, &msg.content, max_content_width);
                        lines.extend(fresh.iter().cloned());
                        *cache = Some((max_content_width, fresh));
                    }
                }
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

            // 展开/折叠详细内容（ctrl+o）。去重：若 `⎿` 摘要行直接复用了正文首行原文
            // （`summary_is_first_line`），正文渲染时跳过该行，避免重复展示同一行。
            let raw_content_lines: Vec<&str> = msg.content.lines().collect();
            let content_lines_deduped =
                strip_summary_duplicate_line(&raw_content_lines, msg.summary_is_first_line);
            if !content_lines_deduped.is_empty() {
                // Read 结果每行带 "行号\t" 前缀（供模型精确编辑用），人类展示层去掉，
                // 不改 msg.content 本身，模型侧历史记录不受影响。
                let is_read = msg.tool_name.as_deref() == Some("Read");
                let content_lines: Vec<&str> = content_lines_deduped
                    .into_iter()
                    .map(|l| {
                        if is_read {
                            strip_read_line_number(l)
                        } else {
                            l
                        }
                    })
                    .collect();
                let is_diff = matches!(msg.tool_name.as_deref(), Some("Edit") | Some("Write"));

                // Edit/Write：永不折叠，直接展开全部 diff，带 +/- 配色
                if is_diff {
                    lines.push(Line::from(Span::styled(
                        format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
                        Theme::dim(),
                    )));
                    // 子 Agent 结果：先列出其内部工具调用明细，再展示最终文本
                    if let Some(s) = msg.sub_agent_id.and_then(|id| ctx.sub_agents.get(&id)) {
                        push_sub_agent_tool_log(lines, &s.tool_log, max_content_width);
                    }
                    // diff 行带配色：+ 绿、- 红、上下文 dim
                    let max_lines = 60;
                    for (i, l) in content_lines.iter().enumerate() {
                        if i >= max_lines {
                            lines.push(Line::from(Span::styled(
                                format!("       …({} more lines)", content_lines.len() - max_lines),
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
                } else if msg.expanded || content_lines.len() <= TOOL_RESULT_FOLD_LINES {
                    // 已展开，或内容行数 <= TOOL_RESULT_FOLD_LINES（始终全量展示，非 Edit/Write）
                    lines.push(Line::from(Span::styled(
                        format!("       {}", "─".repeat(max_content_width.saturating_sub(8))),
                        Theme::dim(),
                    )));
                    // 子 Agent 结果：先列出其内部工具调用明细，再展示最终文本
                    if let Some(s) = msg.sub_agent_id.and_then(|id| ctx.sub_agents.get(&id)) {
                        push_sub_agent_tool_log(lines, &s.tool_log, max_content_width);
                    }
                    let line_style = if msg.is_error {
                        Theme::error()
                    } else {
                        Theme::tool_result()
                    };
                    render_tool_result_body_lines(
                        lines,
                        &content_lines,
                        None,
                        line_style,
                        max_content_width,
                    );
                    // 只有「最后一条可折叠」且已展开才显示 collapse 提示；短内容
                    // （<= TOOL_RESULT_FOLD_LINES 行）永远不会被 last_collapsible_idx 选中
                    // （其判定要求行数 > TOOL_RESULT_FOLD_LINES），这里天然不会误显示。
                    if ctx.last_collapsible_idx == Some(msg_idx) {
                        lines.push(Line::from(Span::styled(
                            "       [ctrl+o to collapse]".to_string(),
                            Theme::dim(),
                        )));
                    }
                } else if content_lines.len() > TOOL_RESULT_FOLD_LINES {
                    // 折叠态：展示开头 TOOL_RESULT_FOLD_LINES 行 + 剩余行数提示
                    let line_style = if msg.is_error {
                        Theme::error()
                    } else {
                        Theme::tool_result()
                    };
                    render_tool_result_body_lines(
                        lines,
                        &content_lines,
                        Some(TOOL_RESULT_FOLD_LINES),
                        line_style,
                        max_content_width,
                    );
                    let remaining = content_lines.len() - TOOL_RESULT_FOLD_LINES;
                    // 折叠态：只有最后一条可折叠的才显示快捷键提示
                    if ctx.last_collapsible_idx == Some(msg_idx) {
                        lines.push(Line::from(Span::styled(
                            format!("       …({remaining} more lines, ctrl+o to expand)"),
                            Theme::dim(),
                        )));
                    } else {
                        // 其余可折叠的历史结果：静默标记，不显示快捷键提示
                        lines.push(Line::from(Span::styled(
                            format!("       ⋯{remaining}"),
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
            let (marker, style) = if msg.is_error {
                ("  ⚠ ", Theme::warning())
            } else {
                ("  ⚙ ", Style::default().fg(Color::Cyan))
            };
            lines.push(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(msg.content.clone(), style),
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

        // ─── 📋 计划正文  ────────────────────────────────────────────
        // 并入正常消息流以获得终端原生 scrollback 滚动；批准/继续规划/手动输入
        // 的交互留在贴底的 draw_plan_approval_selector。
        MessageRole::PlanProposal => {
            let divider = "─".repeat(max_content_width.saturating_sub(2));
            lines.push(Line::from(Span::styled(
                "  📋 计划",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {divider}"),
                Style::default().fg(Color::Blue),
            )));
            let mut body: Vec<Line<'static>> = vec![];
            render_markdown(&mut body, &msg.content, max_content_width.saturating_sub(2));
            for l in body {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(l.spans);
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(Span::styled(
                format!("  {divider}"),
                Style::default().fg(Color::Blue),
            )));
            lines.push(Line::from(""));
        }
    }
}

/// 渲染欢迎页所有行。供 [`build_pending_chat_lines`]（尚未冻结时的每帧重绘）
/// 与 app.rs 主循环的冻结逻辑（欢迎页随第一批冻结内容一起 `insert_before`
/// 写入真实 scrollback 时）共用，避免两处 `WelcomeContext` 构造 drift。
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

/// 构建"待渲染"聊天内容：欢迎页（若适用）+ 尚未冻结进终端真实 scrollback 的
/// 消息尾部（`state.messages[frozen_up_to..]`）+ 流式文本。已冻结的前缀已经
/// 通过 `Terminal::insert_before` 永久写入真实 scrollback，不再参与这里的重建。
///
/// 供 [`draw_chat`] 渲染使用，也供主循环在决定 Inline viewport 高度前测量
/// 所需可见行数（二者必须用同一份构建逻辑，否则高度估算和实际渲染会不一致）。
pub(crate) fn build_pending_chat_lines(
    state: &AppState,
    max_content_width: usize,
) -> Vec<Line<'static>> {
    // 会话仍处于"只有系统消息（如 MCP 连接提示），还没有真实对话"的阶段：欢迎页
    // （5 行 shadow logo 渐变 + Profile/Model + cwd 两行看板）作为消息列表顶部的
    // 固定内容一起渲染，而不是与消息列表互斥——这样 MCP 连接提示会紧跟在欢迎页
    // 后面显示，而不是把欢迎页顶替掉。一旦有任何消息被冻结进真实 scrollback，
    // 说明欢迎页早已滚走，不能再重新显示（此时它已经随第一批冻结内容一起被
    // `insert_before` 写进了终端真实 scrollback，见 app.rs 主循环的冻结逻辑）。
    let show_welcome =
        state.frozen_up_to == 0 && !state.welcome_frozen && state.streaming_buf.is_empty();

    let mut lines: Vec<Line<'static>> = vec![];
    if show_welcome {
        lines.extend(welcome_lines(state, max_content_width));
    }

    // 只有当尚未冻结任何内容时，尾部第一条 User 消息才是"整场对话的第一条"，
    // 不需要在它前面画分隔线；否则视觉上是接着已经滚入 scrollback 的历史继续，
    // 必须照常画分隔线才不会显得突兀。
    let mut is_first_user = state.frozen_up_to == 0;
    lines.extend(render_message_range(
        &state.messages,
        state.frozen_up_to..state.messages.len(),
        max_content_width,
        &state.sub_agents,
        state.spinner_frame,
        &mut is_first_user,
    ));

    // 流式文本（实时输出中）
    if !state.streaming_buf.is_empty() {
        render_markdown(&mut lines, &state.streaming_buf, max_content_width);
    }

    lines
}

/// 渲染 `messages[range]` 为 `Vec<Line>`。`is_first_user` 携带"区间开始前是否已
/// 出现过 User 消息"的状态（调用方按需初始化/复用），供 [`build_pending_chat_lines`]
/// 渲染待定尾部、以及主循环冻结历史前缀写入真实 scrollback 时共用同一份逻辑。
pub(crate) fn render_message_range(
    messages: &[ChatMessage],
    range: std::ops::Range<usize>,
    max_content_width: usize,
    sub_agents: &std::collections::BTreeMap<u64, SubAgentUiState>,
    spinner_frame: usize,
    is_first_user: &mut bool,
) -> Vec<Line<'static>> {
    // 找出最后一条「可折叠」的 ToolResult 索引 —— 即 Ctrl+O 实际会切换的那一条。
    // 注意：始终基于完整 `messages` 扫描（而非仅 `range`），确保冻结历史前缀时
    // 用的判定和渲染待定尾部/Ctrl+O 处理时完全一致。
    let last_collapsible_idx = last_collapsible_tool_result_idx(messages);
    let ctx = ChatRenderCtx {
        max_content_width,
        last_collapsible_idx,
        sub_agents,
        spinner_frame,
    };
    let mut lines = vec![];
    for i in range {
        render_chat_message(&mut lines, &messages[i], i, is_first_user, &ctx);
    }
    lines
}

fn draw_chat(f: &mut Frame, state: &mut AppState, area: Rect) {
    // 不画外框——对齐 Claude Code 的朴素观感。之前每帧都在"当前存活区"外面
    // 包一层 Block 边框，但存活区会随冻结/终端高度变化而移动、伸缩，边框
    // 看起来像一个悬浮在屏幕中间、时断时续的盒子，很违和。
    let max_content_width = area.width.saturating_sub(2) as usize;
    let lines = build_pending_chat_lines(state, max_content_width);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });

    // `area` 通常比实际内容需要的行数高得多——主循环把 Inline viewport 撑到
    // 贴近终端底部，就是为了让输入框永远固定在屏幕最下方（对齐 Claude
    // Code）。这里按内容实际需要的行数在 area 底部截出一块区域渲染，上面
    // 留白，让内容显示为"贴着输入框往上长"而不是贴在屏幕顶部、下面一截
    // 空白追不上屏幕底部。
    let content_line_count = para.line_count(area.width.max(1)).min(u16::MAX as usize) as u16;
    let content_height = content_line_count.min(area.height).max(1);
    let content_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(content_height),
        width: area.width,
        height: content_height,
    };

    // 待定区（尚未冻结的消息尾部 + 流式输出）理论上应该放得下；但极长的单条
    // 流式回复仍可能超出可用高度，此时不滚动会一直停留在内容开头、看不到
    // 正在生成的最新内容——按尾部对齐，始终展示最新部分。
    let scroll_y = content_line_count.saturating_sub(content_height);
    let para = para.scroll((scroll_y, 0));
    f.render_widget(para, content_area);
}

/// 底部固定面板：子 Agent 总览，支持上下选中 + 展开详情（工具流水 + 最终结果/状态）。
/// 标题 `agents [N]`；Running=spinner / Done=✓ / Failed=✗ / Interrupted=⊘；
/// 列表区固定行数上限（SUB_AGENT_LIST_MAX）+ 滚动窗口，本会话内全部保留不再自动清除。
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
                st = st.bg(Color::Blue);
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
        if let Some(s) = state.sub_agents.get(&id) {
            let detail_width = detail_area.width.saturating_sub(2) as usize;
            let mut detail_lines: Vec<Line<'static>> = vec![];
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
                                format!("  {}", truncate_line(l, detail_width.saturating_sub(2))),
                                style,
                            )));
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
        // 进行中的任务优先展示 activeForm 进行时文案（如 "Running tests"）
        let display_text = if item.status == TodoStatus::InProgress {
            item.active_form.as_deref().unwrap_or(&item.content)
        } else {
            &item.content
        };
        let content = truncate_line(
            &format!("{prio_str}{display_text}"),
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
            wyj_i18n::tr("dialog.permission_hint"),
            Theme::highlight(),
        )),
    ];

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
                Style::default()
                    .fg(Theme::CLAUDE)
                    .add_modifier(Modifier::BOLD)
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
                Style::default()
                    .fg(Theme::CLAUDE)
                    .add_modifier(Modifier::BOLD)
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
                Style::default()
                    .fg(Theme::CLAUDE)
                    .add_modifier(Modifier::BOLD)
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

    fn lines_str(n: usize) -> String {
        (0..n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn empty_content_is_not_collapsible() {
        assert!(!is_collapsible_tool_result_content("", None, false));
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
    fn single_line_matching_summary_is_not_collapsible() {
        // 单行输出且摘要复用了该行（summary_is_first_line=true）：去重后正文为空，
        // 不应再判定为可折叠（也不应在展开态渲染出一条和摘要重复的正文）。
        let content = "only line";
        assert!(!is_collapsible_tool_result_content(content, None, true));
    }

    #[test]
    fn edit_write_is_never_collapsible_even_if_long() {
        let content = lines_str(TOOL_RESULT_FOLD_LINES + 5);
        assert!(!is_collapsible_tool_result_content(
            &content,
            Some("Edit"),
            true
        ));
        assert!(!is_collapsible_tool_result_content(
            &content,
            Some("Write"),
            true
        ));
    }

    #[test]
    fn content_at_threshold_is_not_collapsible() {
        let content = lines_str(TOOL_RESULT_FOLD_LINES);
        assert!(!is_collapsible_tool_result_content(
            &content,
            Some("Read"),
            false
        ));
    }

    #[test]
    fn content_over_threshold_is_collapsible() {
        let content = lines_str(TOOL_RESULT_FOLD_LINES + 1);
        assert!(is_collapsible_tool_result_content(
            &content,
            Some("Read"),
            false
        ));
    }

    #[test]
    fn duplicate_first_line_is_stripped_before_fold_threshold_check() {
        // Bash 摘要复用了首行原文：总行数 FOLD_LINES+1，去重后恰好 FOLD_LINES 行，
        // 不应判定为可折叠（修复前会把这条已在摘要展示过的首行也计入正文渲染）。
        let content = lines_str(TOOL_RESULT_FOLD_LINES + 1);
        assert!(!is_collapsible_tool_result_content(
            &content,
            Some("Bash"),
            true
        ));
        // 再多一行，去重后超过阈值，才应判定为可折叠。
        let content = lines_str(TOOL_RESULT_FOLD_LINES + 2);
        assert!(is_collapsible_tool_result_content(
            &content,
            Some("Bash"),
            true
        ));
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
}
