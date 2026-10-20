//! 对话框与浮层渲染。
//!
//! 从 `render.rs` 拆出：这一段是纯「给定状态画出一块矩形」的绘制函数，彼此
//! 之间几乎不共享状态，唯一的依赖是父模块的 `use`（`use super::*` 原样继承）
//! 与少数共用助手（`draw_confirm_box` / `draw_action_menu` / `draw_scrollbar`）。
//!
//! 拆分的意义不在「文件小一点」，而在于**对话框不再和聊天区渲染共享同一个
//! 编译单元**：`render.rs` 里聊天区（跟随对话长度增长）与对话框（随功能增加
//! 增长）两条独立的演化线混在一起，改对话框要重新编译 10k 行的文件，改聊天区
//! 同样如此。
//!
//! 约定：这里所有绘制函数都是 `pub(super)`——只被父模块的 `draw()` 分发调用，
//! 不对 crate 外暴露。真正的对外入口仍然只有 `render::draw`。

use super::*;

// ─── 权限对话框（分级授权） ────────────────────────────────────────────────────

/// 权限确认框：Category A 底部常驻面板（对齐 Claude Code Inline 模式），
/// 直接吃 `bottom_panel_size` 已经算好的 `area`（`chunks[1]`），不再自己居中
/// 计算浮层 Rect、不再用 `Clear`——它高频出现在几乎每次 Edit/Write/Bash 调用，
/// 走全屏浮层切换会非常闪烁。
pub(super) fn draw_permission_dialog(f: &mut Frame, dlg: &PermissionDialog, area: Rect) {
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

// ─── 会话占用裁决框 ───────────────────────────────────────────────────────────

/// 「恢复的会话正被另一个实例使用」裁决框。与权限框同为底部常驻面板（不居中浮层）：
/// 它挡在聊天区输入框之上、不抢整屏，用户仍能往上翻看被恢复出来的历史。
///
/// 文案全部走 `tr()`——用户可见 UI 走 i18n 是仓库硬约定（历史上 plan / exec-mode
/// 面板硬编码中文是遗留，不要照抄）。
pub(super) fn draw_session_conflict_panel(f: &mut Frame, dlg: &SessionConflictDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::permission_dialog())
        .title(Span::styled(
            wyj_i18n::tr("dialog.session_conflict_title"),
            Theme::permission_dialog(),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_w = (inner.width as usize).saturating_sub(2).max(20);
    let body = wyj_i18n::tr_fmt(
        "dialog.session_conflict_body",
        &[
            ("id", &dlg.info.session_id),
            ("pid", &dlg.info.owner_pid.to_string()),
        ],
    );
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(truncate_chars(&body, max_w), Theme::dim())),
        Line::from(""),
    ];
    let labels = [
        wyj_i18n::tr("dialog.session_conflict_take_over"),
        wyj_i18n::tr("dialog.session_conflict_read_only"),
        wyj_i18n::tr("dialog.session_conflict_abort"),
    ];
    for (index, label) in labels.iter().enumerate() {
        lines.push(choice_row(
            index == dlg.selected,
            &format!("[{}] {label}", index + 1),
            max_w,
        ));
    }
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("dialog.session_conflict_hint"),
        Theme::highlight(),
    )));

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

/// 一行可选项：`▶` 标记当前选中项（与 `plan_option_row` 同一套视觉语言）。
pub(super) fn choice_row(selected: bool, label: &str, max_w: usize) -> Line<'static> {
    let marker = if selected { "▶ " } else { "  " };
    let style = if selected {
        Theme::highlight()
    } else {
        Theme::dim()
    };
    Line::from(Span::styled(
        truncate_chars(&format!("{marker}{label}"), max_w),
        style,
    ))
}

/// 项目级 MCP server 信任确认面板：只在 TUI 启动后台连接阶段检测到未信任的
/// `.wyj-code/mcp.toml`/`.mcp.json` server 时出现一次，样式对齐
/// `draw_permission_dialog`（同为安全相关确认）。
pub(super) fn draw_project_trust_panel(
    f: &mut Frame,
    servers: &[wyj_config::McpServerConfig],
    area: Rect,
) {
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

/// 项目级 skill 信任面板。展示内容取自
/// `wyj_commands::skill::project_skill_infos`（走与真正加载完全相同的 walk），
/// 因此这里列的命令名与批准后实际生效的完全一致。
pub(super) fn draw_skill_trust_panel(
    f: &mut Frame,
    infos: &[wyj_commands::skill::ProjectSkillInfo],
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::permission_dialog())
        .title(Span::styled(
            wyj_i18n::tr("dialog.skill_trust_title"),
            Theme::permission_dialog(),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::raw(wyj_i18n::tr(
        "dialog.skill_trust_intro",
    )))];
    for info in infos {
        lines.push(Line::from(Span::raw(truncate_chars(
            &format!("  · /{}: {}", info.name, info.description),
            inner.width as usize,
        ))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("dialog.skill_trust_hint"),
        Theme::highlight(),
    )));

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });
    f.render_widget(para, inner);
}

// ─── AskQuestion 底部面板 ─────────────────────────────────────────────────────

/// 计划正文已经作为 `MessageRole::PlanProposal` 消息并入聊天流展示（见
/// `render_chat_message`），这里只剩固定 3 行的三选一选择器：批准 / 继续规划 /
/// 手动输入反馈，↑/↓ 选中 + Enter 确认，对齐 `draw_permission_dialog` 的贴底样式。
pub(super) fn draw_plan_approval_panel(f: &mut Frame, dlg: &PlanApprovalDialog, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::plan())
        .title(Span::styled(
            " 📋 计划已就绪 · ↑/↓ 选择 · Enter 确认 ",
            Style::default()
                .fg(Theme::plan_color())
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
        // 多行逐行渲染（`PlanApprovalDialog` 接了 `apply_edit_key`，Shift+Enter /
        // Ctrl+J / 行尾 `\`+Enter 都能换行，提交时 `input.text()` 会带上全部
        // 内容）。此前只画 `lines.first()` 并把一个假的 `_` 永远贴在行尾：
        // 多行内容直接不可见，用户写三行只看见一行，回车却把三行一起发出去。
        // 直接复用 AskQuestion 自由输入那条已验过的 `freetext_line`（真实块状
        // 光标 + 超宽横向滚动），两个弹窗从此走同一条渲染线。
        let total = input.lines.len();
        for (row, line) in input.lines.iter().enumerate() {
            let mut rendered = freetext_line(input, row, line, max_w);
            // 首行加树形分叉标记，多行读起来是一整块输入而不是几条散项
            if row + 1 < total {
                let marker = Span::styled("├ ", Style::default().fg(Theme::claude_color()));
                let mut spans = vec![marker];
                spans.append(&mut rendered.spans);
                rendered = Line::from(spans);
            }
            lines.push(rendered);
        }
    } else {
        lines.push(plan_option_row(2, cursor, "手动输入反馈…", max_w));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// 三选一面板里的单行选项：高亮项用 "▶ label"（Claude 主题色加粗），
/// 与 `build_answering_lines` 的 AskQuestion 单选样式保持一致。
pub(super) fn plan_option_row(
    idx: usize,
    cursor: usize,
    label: &str,
    max_w: usize,
) -> Line<'static> {
    if idx == cursor {
        Line::from(Span::styled(
            format!("▶ {}", truncate_line(label, max_w.saturating_sub(2))),
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            format!("  {}", truncate_line(label, max_w.saturating_sub(2))),
            Theme::body(),
        ))
    }
}

pub(super) fn draw_exec_mode_confirm_panel(f: &mut Frame, dlg: &ExecModeConfirmDialog, area: Rect) {
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
            Span::styled("待发送：", Theme::muted()),
            Span::styled(preview, Theme::body()),
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
pub(super) fn labels_for_ui(spec: &AskQuestionSpec, indices: &[usize]) -> String {
    indices
        .iter()
        .filter_map(|&i| spec.options.get(i))
        .map(|o| o.label.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 总览页里一题答案的展示文本
pub(super) fn format_confirmed_answer_ui(
    spec: &AskQuestionSpec,
    answer: &QuestionAnswer,
) -> String {
    match answer {
        QuestionAnswer::Selected(indices) => labels_for_ui(spec, indices),
        QuestionAnswer::FreeText(text) => format!("其他: {text}"),
        QuestionAnswer::SelectedWithFreeText(indices, text) => {
            format!("{}、其他: {text}", labels_for_ui(spec, indices))
        }
    }
}

/// 自由文本的一行：`❯ ` 前缀 + 真实块状光标 + 超宽时按光标横向滚动。
///
/// 光标用**反色块**而不是移动终端硬件光标：弹窗是浮层，硬件光标同时被底部
/// 主输入框占用，两者会打架；反色块在任何焦点状态下都稳定可见。
pub(super) fn freetext_line(
    input: &InputBox,
    row: usize,
    line: &str,
    max_w: usize,
) -> Line<'static> {
    const PREFIX: &str = "❯ ";
    let inner_w = max_w.saturating_sub(PREFIX.len()).max(1);
    let chars: Vec<char> = line.chars().collect();
    let cursor_col = if row == input.cursor_row {
        input.cursor_col.min(chars.len())
    } else {
        usize::MAX
    };
    let (start, end) = visible_window(&chars, cursor_col, inner_w);

    let mut spans = vec![Span::styled(
        PREFIX.to_string(),
        Style::default().fg(Theme::claude_color()),
    )];
    if start > 0 {
        spans.push(Span::styled("‹", Theme::dim()));
    }
    for (i, ch) in chars[start..end].iter().enumerate() {
        if cursor_col != usize::MAX && start + i == cursor_col {
            spans.push(Span::styled(
                ch.to_string(),
                Theme::selection_on(Theme::claude_color()).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(ch.to_string(), Theme::body()));
        }
    }
    // 光标停在行尾（插入态）：补一个反色空格，否则看不出还能继续打字
    if cursor_col != usize::MAX && cursor_col == chars.len() {
        spans.push(Span::styled(
            " ".to_string(),
            Theme::selection_on(Theme::claude_color()),
        ));
    }
    if end < chars.len() {
        spans.push(Span::styled("›", Theme::dim()));
    }
    Line::from(spans)
}

/// 按显示宽度取一个「把光标放进窗口」的字符区间（start, end）。
///
/// 不变式：**光标之前的文本必须完整落在窗口内**，否则光标会被挤出可视区、
/// 整个反色块消失——用户就失去了所有光标位置提示（比旧的假 `_` 更糟：旧的
/// 位置虽然永远错，但永远看得见）。
///
/// 向后走的时候要减的是**即将被丢到左边窗外**的那个字符，且循环条件比较的是
/// 「留在窗口内的前缀宽度」。反过来写（拿光标前的总宽度去减、拿丢弃量去比）
/// 会让循环在「左侧丢弃的文本塞得下」时就停——恰好与意图相反，光标偏移量
/// 随之无界增长，行宽超过约 `2 × inner_w` 时必然消失。
pub(super) fn visible_window(chars: &[char], cursor_col: usize, inner_w: usize) -> (usize, usize) {
    let upto = if cursor_col == usize::MAX {
        0
    } else {
        cursor_col.min(chars.len())
    };
    let cursor_w: usize = chars[..upto]
        .iter()
        .map(|c| crate::input::char_width(*c))
        .sum();
    // 光标自身最多占 2 列（CJK），按它的实际宽度留位，否则它会被挤出窗口
    let cursor_cell = crate::input::char_width(*chars.get(upto).unwrap_or(&' '));
    let target = inner_w.saturating_sub(cursor_cell);
    let mut start = 0usize;
    if cursor_w > target {
        // 从左侧逐个丢弃字符，直到「留下的前缀 + 光标」塞得下
        let mut i = 0usize;
        let mut w = cursor_w;
        while i < upto && w > target {
            w -= crate::input::char_width(chars[i]);
            i += 1;
        }
        start = i;
    }
    let mut end = start;
    let mut w = 0usize;
    while end < chars.len() {
        let cw = crate::input::char_width(chars[end]);
        if w + cw > inner_w {
            break;
        }
        w += cw;
        end += 1;
    }
    (start, end)
}

pub(super) fn build_answering_lines(
    dlg: &AskQuestionDialog,
    index: usize,
    max_w: usize,
) -> Vec<Line<'static>> {
    let spec = &dlg.questions[index];
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            truncate_line(&spec.question, max_w),
            Style::default()
                .fg(Theme::text_color())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("─".repeat(max_w), Theme::border())),
    ];

    if let InProgressAnswer::FreeText { input, .. } = &dlg.current {
        // 自由文本此前**只画第一行**、并把一个假的 `_` 永远贴在行尾：多行
        // （Shift+Enter）内容直接看不见，光标位置也永远是错的。这里改成
        // 逐行渲染 + 真实块状光标 + 横向滚动窗口 + 字数。
        for (row, line) in input.lines.iter().enumerate() {
            lines.push(freetext_line(input, row, line, max_w));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr_fmt(
                "dialog.hint_freetext",
                &[("chars", &input.text().chars().count().to_string())],
            ),
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
                    .fg(Theme::claude_color())
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("    {}", truncate_line(&label, max_w.saturating_sub(4))),
                Theme::body(),
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
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "    {}",
                truncate_line(&other_label, max_w.saturating_sub(4))
            ),
            Theme::body(),
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

pub(super) fn build_overview_lines(
    dlg: &AskQuestionDialog,
    index: usize,
    max_w: usize,
) -> Vec<Line<'static>> {
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
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD)
        } else {
            Theme::body()
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
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Theme::body())
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

pub(super) fn draw_session_picker(f: &mut Frame, picker: &SessionPickerState, area: Rect) {
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            wyj_i18n::tr_fmt(
                "dialog.session_picker_title",
                &[("count", &n_sessions.to_string())],
            ),
            Style::default()
                .fg(Theme::claude_color())
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
                .fg(Theme::success_color())
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("    {:<w$}", new_session_label, w = w.saturating_sub(4)),
            Theme::success(),
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
                        .bg(Theme::selected_bg_color())
                        .fg(Theme::text_color())
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
pub(super) fn shorten_home_path(full: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && full.starts_with(&home) {
        format!("~{}", &full[home.len()..])
    } else {
        full.to_string()
    }
}

/// api_key 打码展示：只保留前 8 位 + "..."
pub(super) fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let prefix: String = s.chars().take(8).collect();
    format!("{prefix}...")
}

pub(super) fn draw_settings_dialog(f: &mut Frame, dialog: &SettingsDialog, area: Rect) {
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("settings.title")),
            Style::default()
                .fg(Theme::claude_color())
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
            Theme::body()
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

/// AGENTS.md 记忆面板渲染（/memory 命令触发）
pub(super) fn draw_memory_dialog(f: &mut Frame, dialog: &MemoryDialog, area: Rect) {
    let content_lines = dialog.rows.len() as u16 + 3; // 行列表 + 分隔线 + 错误行 + 提示行
    let height = (content_lines + 2).min(area.height.saturating_sub(2));
    let width = (area.width * 8 / 10).clamp(60, 110).min(area.width);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            wyj_i18n::tr("memory.dialog.title"),
            Style::default()
                .fg(Theme::claude_color())
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
            MemoryRow::ClearAll {
                active_count,
                superseded_count,
            } => wyj_i18n::tr_fmt(
                "memory.dialog.clear_all_label",
                &[
                    ("active", &active_count.to_string()),
                    ("superseded", &superseded_count.to_string()),
                ],
            ),
        };
        let text = truncate_line(&text, w);

        let style = if selected {
            Theme::selected_row()
        } else {
            Theme::body()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    lines.push(Line::from(Span::styled("─".repeat(w), Theme::border())));
    if dialog.pending_clear_all {
        lines.push(Line::from(Span::styled(
            truncate_line(&wyj_i18n::tr("memory.dialog.clear_all_hint"), w),
            Theme::warning(),
        )));
        lines.push(Line::from(Span::styled(
            truncate_line(&wyj_i18n::tr("memory.dialog.clear_all_confirm"), w),
            Theme::warning(),
        )));
    } else if let Some(err) = &dialog.error {
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

pub(super) fn draw_evolution_dialog(f: &mut Frame, dialog: &EvolutionDialog, area: Rect) {
    let width = (area.width * 9 / 10).clamp(72, 140).min(area.width);
    let height = (area.height * 8 / 10).clamp(18, 46).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let dialog_area = Rect::new(x, y, width, height);
    f.render_widget(Clear, dialog_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            wyj_i18n::tr("evolve.title"),
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(inner);
    let tabs = crate::app::EvolutionView::ALL
        .iter()
        .map(|view| {
            let selected = *view == dialog.view;
            Span::styled(
                format!(" {} ", view.label()),
                if selected {
                    Theme::selected_row().add_modifier(Modifier::BOLD)
                } else {
                    Theme::dim()
                },
            )
        })
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(Line::from(tabs)), vertical[0]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(vertical[1]);
    let mut list_lines = Vec::new();
    if dialog.rows.is_empty() {
        list_lines.push(Line::from(Span::styled("  No items", Theme::dim())));
    }
    for (index, row) in dialog.rows.iter().enumerate() {
        let marker = if index == dialog.selected {
            "▶ "
        } else {
            "  "
        };
        let label = match row {
            EvolutionRow::Memory(memory) => format!(
                "{marker}[M/{:?}/{:?}] {}",
                memory.kind, memory.status, memory.summary
            ),
            EvolutionRow::Candidate(candidate) => format!(
                "{marker}[C/{:?}/{:?}] {}",
                candidate.kind, candidate.status, candidate.title
            ),
            EvolutionRow::Episode(episode) => {
                format!("{marker}[E/{:?}] {}", episode.outcome, episode.goal_summary)
            }
            EvolutionRow::Health { label, value } => format!("{marker}{label}: {value}"),
        };
        list_lines.push(Line::from(Span::styled(
            truncate_line(&label, columns[0].width.saturating_sub(2) as usize),
            if index == dialog.selected {
                Theme::selected_row()
            } else {
                Theme::body()
            },
        )));
    }
    f.render_widget(
        Paragraph::new(Text::from(list_lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Theme::border())
                .title(" Items "),
        ),
        columns[0],
    );

    let detail = dialog
        .rows
        .get(dialog.selected)
        .map(|row| match row {
            EvolutionRow::Memory(memory) => format!(
                "{}\n\nid: {}\nkind: {:?}\nstatus: {:?}\nscope: {}:{}\nconfidence: {}\npinned: {}\nepisodes: {}\nsessions: {}\ncitations: {}\n\n{}",
                memory.summary,
                memory.id,
                memory.kind,
                memory.status,
                memory.scope.level,
                memory.scope.value,
                memory.confidence,
                memory.pinned,
                memory.evidence_episode_ids.len(),
                memory.evidence_session_ids.len(),
                memory.citations.len(),
                memory.body
            ),
            EvolutionRow::Candidate(candidate) => {
                let payload = match &candidate.payload {
                    wyj_core::CandidatePayload::Rule {
                        rule_text,
                        suggested_target,
                    } => format!("target: {suggested_target}\n\n{rule_text}"),
                    wyj_core::CandidatePayload::Skill {
                        skill_name,
                        eval,
                        skill_md,
                        ..
                    } => format!(
                        "skill: {skill_name}\neval: pass={} cases={} historical_successes={} sessions={}\n\n{}",
                        eval.structural_pass,
                        eval.cases.len(),
                        eval.historical_successes,
                        eval.distinct_sessions,
                        skill_md
                    ),
                };
                format!(
                    "{}\n\nid: {}\nkind: {:?}\nstatus: {:?}\nrisk: {}\nepisodes: {}\nsessions: {}\nactivated: {}\n\n{}",
                    candidate.summary,
                    candidate.id,
                    candidate.kind,
                    candidate.status,
                    candidate.risk,
                    candidate.evidence_episode_ids.len(),
                    candidate.evidence_session_ids.len(),
                    candidate
                        .activated_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "-".into()),
                    payload
                )
            }
            EvolutionRow::Episode(episode) => format!(
                "{}\n\nid: {}\nsession: {}\noutcome: {:?}\nconfidence: {}\nmodel: {}/{}\ntokens: ↑{} ↓{}\nduration: {} ms\nexternal context: {}\nmanual include: {}\nchanged paths: {}\n\nEvidence:\n{}",
                episode.goal_summary,
                episode.id,
                episode.session_id,
                episode.outcome,
                episode.confidence,
                episode.vendor,
                episode.model,
                episode.input_tokens,
                episode.output_tokens,
                episode.duration_ms,
                episode.external_context,
                episode.included_by_user,
                episode.changed_paths.len(),
                episode
                    .evidence
                    .iter()
                    .map(|evidence| format!("- {:?} {} success={:?}: {}", evidence.kind, evidence.label, evidence.success, evidence.detail))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            EvolutionRow::Health { label, value } => format!("{label}\n\n{value}"),
        })
        .unwrap_or_else(|| "No selected item".to_string());
    f.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .scroll((dialog.detail_scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Theme::border())
                    .title(" Evidence / Detail "),
            ),
        columns[1],
    );

    let footer = if let Some(action) = &dialog.confirm {
        format!("Confirm {:?}?  Enter confirm · Esc cancel", action)
    } else if let Some(error) = &dialog.error {
        error.clone()
    } else {
        wyj_i18n::tr("evolve.hint")
    };
    f.render_widget(
        Paragraph::new(truncate_line(&footer, vertical[2].width as usize)).style(
            if dialog.confirm.is_some() {
                Theme::warning()
            } else {
                Theme::dim()
            },
        ),
        vertical[2],
    );
}

// ── MCP server 管理面板渲染：/mcp 命令触发 ─────────────────────────────────────

pub(super) fn mcp_scope_label(scope: wyj_store::InstallScope) -> String {
    wyj_i18n::tr(match scope {
        wyj_store::InstallScope::Global => "mcp.dialog.scope_global",
        wyj_store::InstallScope::Project => "mcp.dialog.scope_project",
    })
}

pub(super) fn mcp_package_command_preview(
    package: &wyj_store::mcp_install::PackageChoice,
) -> String {
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
pub(super) const MAX_LIST_VIEWPORT: usize = 12;

/// 给定列表总行数、当前光标位置与可视窗口行数，计算滚动窗口起始下标
/// （光标始终落在窗口内；无需在 dialog 状态里额外维护 scroll_offset，
/// 每帧按光标位置重新计算即可）。
pub(super) fn scroll_window_start(total: usize, cursor: usize, visible: usize) -> usize {
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

/// 带固定尾部的正文窗口：返回 `(起始行, 可见行数)`，保证 `cursor` 落在窗口内。
///
/// 面板高度一律被屏幕高度封顶，所以"有多少行？"和"屏幕能放几行？"必须分开算。
/// `footer_rows` 是尾部固定区（分隔线 / 错误行 / 操作提示），它不参与滚动；
/// 正文窗口吃掉剩下的高度。**每个 `rows()` + Paragraph 的面板都必须走这里**——
/// 少走一处的表现就是「光标能移到屏幕外、尾部提示被裁掉」，而渲染代码看上去
/// 完全正常（Paragraph 从不 scroll，超出部分静默丢弃）。菜单类浮层 `footer_rows`
/// 传 0。
pub(super) fn body_window(
    total: usize,
    cursor: usize,
    inner_rows: u16,
    footer_rows: usize,
) -> (usize, usize) {
    let visible = (inner_rows as usize).saturating_sub(footer_rows).max(1);
    (scroll_window_start(total, cursor, visible), visible)
}

pub(super) fn draw_mcp_dialog(
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} — {} ", wyj_i18n::tr("mcp.dialog.title"), tab_label),
            Style::default()
                .fg(Theme::claude_color())
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
                Theme::body()
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
        draw_action_menu(
            f,
            area,
            dialog_area,
            &wyj_i18n::tr("mcp.dialog.title"),
            menu,
        );
    } else {
        draw_mcp_overlay(f, dialog, area);
    }
}

pub(super) fn draw_mcp_overlay(f: &mut Frame, dialog: &McpDialog, area: Rect) {
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
pub(super) fn draw_confirm_box(f: &mut Frame, title: &str, lines: Vec<Line<'static>>, area: Rect) {
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

pub(super) fn draw_skills_dialog(f: &mut Frame, dialog: &SkillsDialog, area: Rect) {
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} — {} ", wyj_i18n::tr("skills.dialog.title"), tab_label),
            Style::default()
                .fg(Theme::claude_color())
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
                Theme::body()
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
                    // 同一张表可能装的是 git marketplace 条目，也可能是注册表搜索
                    // 结果，两者字段完全不同，必须按来源取数。
                    match dialog.browse_source {
                        SkillsBrowseSource::Registry => match dialog.registry_results.get(*idx) {
                            Some(s) => {
                                let mut line = format!(
                                    "{marker}{} v{} — {}",
                                    s.display_name, s.version, s.description
                                );
                                if s.stars > 0 {
                                    line.push_str(&format!("  ★{}", s.stars));
                                }
                                line
                            }
                            None => String::new(),
                        },
                        _ => match dialog.browse_results.get(*idx) {
                            Some(e) => {
                                format!("{marker}{} v{} — {}", e.name, e.version, e.description)
                            }
                            None => String::new(),
                        },
                    }
                }
                // Browse tab 的固定第 0 行是搜索框（沿用 MCP 面板同一约定）
                (SkillsDialogTab::Browse, FlatRow::AddNew) => {
                    let query = dialog.live_input.display_lines().join("");
                    if query.is_empty() {
                        format!(
                            "{marker}{}",
                            wyj_i18n::tr("skills.dialog.search_row_placeholder")
                        )
                    } else {
                        format!(
                            "{marker}{}",
                            wyj_i18n::tr_fmt("skills.dialog.search_row", &[("query", &query)])
                        )
                    }
                }
                (SkillsDialogTab::Installed, FlatRow::AddNew) => String::new(),
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
        draw_action_menu(
            f,
            area,
            dialog_area,
            &wyj_i18n::tr("skills.dialog.title"),
            menu,
        );
    } else {
        draw_skills_overlay(f, &dialog.overlay, area);
    }
}

/// 人类可读的字节数（确认框里展示技能包体积用）。
pub(super) fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

pub(super) fn draw_skills_overlay(f: &mut Frame, overlay: &SkillsOverlay, area: Rect) {
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
        SkillsOverlay::Searching => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("skills.dialog.searching"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("skills.dialog.title"), lines, area);
        }
        SkillsOverlay::FetchingArchive => {
            let lines = vec![Line::from(Span::styled(
                wyj_i18n::tr("skills.dialog.fetching"),
                Theme::dim(),
            ))];
            draw_confirm_box(f, &wyj_i18n::tr("skills.dialog.title"), lines, area);
        }
        // 注册表技能的安装确认：来源 / 作者 / 版本 + 包内容摘要必须一次看全。
        // 技能正文会整段作为模型指令进入对话，而包里可能夹带 scripts/ 可执行内容，
        // 这是不可逆的信任动作，值得占一整屏。
        SkillsOverlay::RegistryInstallConfirm {
            skill,
            preview,
            scope,
            ..
        } => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.install.name_label"), Theme::dim()),
                    Span::raw(skill.display_name.clone()),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.registry.source_label"), Theme::dim()),
                    Span::raw(format!(
                        "{} ({})",
                        if skill.canonical_name.is_empty() {
                            skill.slug.clone()
                        } else {
                            skill.canonical_name.clone()
                        },
                        if skill.source.is_empty() {
                            "community".to_string()
                        } else {
                            skill.source.clone()
                        }
                    )),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.registry.owner_label"), Theme::dim()),
                    Span::raw(skill.owner_name.clone()),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.install.version_label"), Theme::dim()),
                    Span::raw(skill.version.clone()),
                ]),
                Line::from(vec![
                    Span::styled(wyj_i18n::tr("skills.registry.content_label"), Theme::dim()),
                    Span::raw(wyj_i18n::tr_fmt(
                        "skills.registry.content_summary",
                        &[
                            ("files", &preview.file_count.to_string()),
                            ("size", &format_bytes(preview.total_bytes)),
                        ],
                    )),
                ]),
            ];
            if preview.has_executables() {
                lines.push(Line::from(Span::styled(
                    wyj_i18n::tr_fmt(
                        "skills.registry.executables_warning",
                        &[("count", &preview.executable_count.to_string())],
                    ),
                    Theme::warning(),
                )));
                for path in &preview.executable_paths {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", path),
                        Theme::warning(),
                    )));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(wyj_i18n::tr("skills.install.scope_label"), Theme::dim()),
                Span::raw(mcp_scope_label(*scope)),
            ]));
            lines.push(Line::from(Span::styled(
                wyj_i18n::tr("skills.install.confirm_hint"),
                Theme::highlight(),
            )));
            draw_confirm_box(
                f,
                &wyj_i18n::tr("skills.install.registry_confirm_title"),
                lines,
                area,
            );
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

pub(super) fn draw_plugins_dialog(f: &mut Frame, dialog: &PluginsDialog, area: Rect) {
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} — {} ", wyj_i18n::tr("plugins.dialog.title"), tab_label),
            Style::default()
                .fg(Theme::claude_color())
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
                Theme::body()
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
        draw_action_menu(
            f,
            area,
            dialog_area,
            &wyj_i18n::tr("plugins.dialog.title"),
            menu,
        );
    } else {
        draw_plugins_overlay(f, &dialog.overlay, area);
    }
}

pub(super) fn draw_plugins_overlay(f: &mut Frame, overlay: &PluginOverlay, area: Rect) {
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

pub(super) fn draw_agents_dialog(f: &mut Frame, dialog: &mut AgentsDialog, area: Rect) {
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("agents.dialog.title")),
            Style::default()
                .fg(Theme::claude_color())
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
                Theme::body()
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

pub(super) fn draw_extensions_dialog(f: &mut Frame, dialog: &mut ExtensionsDialog, area: Rect) {
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            " Extensions ",
            Style::default()
                .fg(Theme::claude_color())
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
                Theme::body()
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
            Theme::warning(),
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
/// 分组管理面板渲染（/model 无参命令触发）
pub(super) fn draw_profile_dialog(
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("profile.title")),
            Style::default()
                .fg(Theme::claude_color())
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
    // 正文行必须按光标开窗：面板高度被屏幕封顶后，旧实现把 `rows()` 全量塞进一个
    // 从不 `.scroll()` 的 Paragraph，超出 inner 的部分连同尾部的「+ 新建分组」和
    // 两条提示一起被裁掉——而 `cursor` 仍能一路往下走，于是用户看到的是一个没有
    // ▶ 的静态列表（分组多、或展开字段多时必现）。尾部 4 行（分隔线 + 错误行 +
    // 两条提示）固定常驻，正文窗口只吃剩下的高度；窗口起点每帧按光标重算，与
    // mcp/skills/plugins 列表共用同一个助手。
    const FOOTER_LINES: usize = 4;
    let (window_start, visible_rows) =
        body_window(rows.len(), dialog.cursor, inner.height, FOOTER_LINES);
    for (row_idx, row) in rows
        .iter()
        .enumerate()
        .skip(window_start)
        .take(visible_rows)
    {
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
                let spec = &PROFILE_FIELD_SPECS[*f_idx];
                let label = wyj_i18n::tr(spec.key);
                let value = if editing {
                    wyj_i18n::tr("profile.dialog.editing_placeholder")
                } else if matches!(spec.kind, ProfileFieldKind::Secret) {
                    mask_secret(entry.text_value(*f_idx))
                } else {
                    entry.display_value(*f_idx)
                };
                let cursor = if selected_row { "▶" } else { " " };
                // 标签列必须按**显示宽度**补空格：中文标签（"上下文长度"）每个
                // 汉字占 2 列，`{:<width$}` 数的是 char 数，中英混排时值列会错位。
                let pad = label_width.saturating_sub(display_width(&label));
                let padding = " ".repeat(pad);
                format!("{cursor}     {label}{padding}{value}")
            }
            ProfileRow::AddNew => {
                let cursor = if selected_row { "▶" } else { " " };
                format!("{cursor} + {}", wyj_i18n::tr("profile.dialog.add_new_row"))
            }
        };
        let text = truncate_line(&text, w);

        let style = if editing {
            Theme::selection_on(Theme::claude_color())
        } else if selected_row {
            Theme::selected_row()
        } else {
            Theme::body()
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
        draw_action_menu(f, area, dialog_area, &wyj_i18n::tr("profile.title"), menu);
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
pub(super) fn draw_schedule_dialog(
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
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr("schedule.title")),
            Style::default()
                .fg(Theme::claude_color())
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
    // 与 `draw_profile_dialog` 同一处结构：高度被屏幕封顶后，全量塞进无 scroll 的
    // Paragraph 会让光标走到屏幕外、末尾「+ 新建任务」和两条提示一起被裁掉。
    // 尾部 4 行（分隔线 + 错误行 + 两条提示）固定常驻，正文按光标开窗。
    const FOOTER_LINES: usize = 4;
    let (window_start, visible_rows) =
        body_window(rows.len(), dialog.cursor, inner.height, FOOTER_LINES);
    for (row_idx, row) in rows
        .iter()
        .enumerate()
        .skip(window_start)
        .take(visible_rows)
    {
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
                let review = if task.needs_permission_review {
                    "  [permission review required]"
                } else {
                    ""
                };
                format!(
                    "{cursor} {expand_marker} {marker} {}  [{}]  next:{next_run}  last:{status}{review}",
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
            Theme::selection_on(Theme::claude_color())
        } else if selected_row {
            Theme::selected_row()
        } else {
            Theme::body()
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
        draw_action_menu(f, area, dialog_area, &wyj_i18n::tr("schedule.title"), menu);
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

pub(super) fn draw_profile_text_overlay(f: &mut Frame, area: Rect, title_key: &str, body: &str) {
    let width = (area.width * 5 / 10).clamp(40, 70).min(area.width);
    let height = 5u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {} ", wyj_i18n::tr(title_key)),
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;
    let para = Paragraph::new(Text::from(vec![Line::from(truncate_line(body, w))]));
    f.render_widget(para, inner);
}

pub(super) fn draw_profile_list_overlay(
    f: &mut Frame,
    area: Rect,
    title_key: &str,
    items: Vec<String>,
    selected: usize,
) {
    let total = items.len();
    let width = (area.width * 6 / 10).clamp(40, 90).min(area.width);
    let height = ((total as u16) + 2)
        .min(area.height.saturating_sub(2))
        .max(4);
    // 与操作菜单同一条规则：高度被屏幕封顶后必须按选中项开窗，否则超出 inner
    // 的条目（以及它们的 ▶）会被 Paragraph 静默裁掉。模板/未保存确认这些列表
    // 目前都不长，但"不长"不是不变式——入口在这里，就别留第二套写法。
    let (start, visible) = body_window(total, selected, height.saturating_sub(2), 0);
    let scroll_hint = if total > visible {
        format!(" · {}/{}", (selected + 1).min(total), total)
    } else {
        String::new()
    };
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {}{scroll_hint} ", wyj_i18n::tr(title_key)),
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;

    let lines: Vec<Line<'static>> = items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, item)| {
            let marker = if i == selected { "▶ " } else { "  " };
            let text = truncate_line(&format!("{marker}{item}"), w);
            let style = if i == selected {
                Style::default()
                    .fg(Theme::claude_color())
                    .add_modifier(Modifier::BOLD)
            } else {
                Theme::body()
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
pub(super) fn draw_action_menu<T, A: PartialEq>(
    f: &mut Frame,
    area: Rect,
    anchor: Rect,
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
        draw_confirm_overlay(f, area, anchor, title, label);
        return;
    }

    let width = (anchor.width * 7 / 10)
        .clamp(36, 72)
        .min(area.width.saturating_sub(4));
    let total = menu.items.len();
    let height = ((total as u16) + 2)
        .min(area.height.saturating_sub(4))
        .max(4);
    // 高度被屏幕封顶后，条目必须**按光标位置开窗**：旧实现把全部 items 交给一个
    // 从不 `.scroll()` 的 Paragraph，超出 inner 的部分被直接裁掉，而 `selected`
    // 照样能一路往下走——于是 `/model` 拉回上百个模型时，光标和 ▶ 高亮一起走到
    // 屏幕外，现场表现就是"按 ↓ 列表不动、下面的选项永远看不见"（选择其实已经
    // 生效，用户在盲选）。窗口起点每帧按光标重算，与 mcp/skills/plugins 列表共用
    // `scroll_window_start`，无需在 `ActionMenu` 里新增滚动状态。
    let (start, visible) = body_window(total, menu.selected, height.saturating_sub(2), 0);
    // 有内容在窗口外时在标题里给出位置，否则用户不知道"下面还有"。
    let scroll_hint = if total > visible {
        format!(" · {}/{}", menu.selected + 1, total)
    } else {
        String::new()
    };
    // **贴着父面板摆放，而不是叠在它身上。** 之前子菜单和父面板用的是同一个
    // `area`（整屏）居中，两者坐标完全重合，父面板的文字从弹窗左右两侧露出来
    // （实测第二行 profile 被切掉一半），看起来像渲染错位而不是"打开了一个
    // 子菜单"。右边放不下就落到下方，两边都不行才退回居中。
    let right_edge = area.x + area.width;
    let bottom_edge = area.y + area.height;
    let (mut x, mut y) = (anchor.right() + 1, anchor.y + 1);
    if x + width + 1 > right_edge {
        x = anchor.x + 2;
        y = anchor.bottom() + 1;
    }
    if y + height + 1 > bottom_edge {
        y = bottom_edge.saturating_sub(height + 1);
    }
    x = x.clamp(area.x, right_edge.saturating_sub(width));
    y = y.clamp(area.y, bottom_edge.saturating_sub(height));
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::claude_color()))
        .title(Span::styled(
            format!(" {title}{scroll_hint} "),
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    let w = inner.width as usize;

    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
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
                    .fg(Theme::claude_color())
                    .add_modifier(Modifier::BOLD)
            } else {
                Theme::body()
            };
            Line::from(Span::styled(text, style))
        })
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// `draw_action_menu` 里危险操作的二级确认子步骤："确定要 {label} 吗？"
pub(super) fn draw_confirm_overlay(
    f: &mut Frame,
    area: Rect,
    anchor: Rect,
    title: &str,
    action_label: &str,
) {
    // 二级确认同样贴着触发它的弹窗摆，不要再叠上去
    let width = (anchor.width * 7 / 10)
        .clamp(40, 70)
        .min(area.width.saturating_sub(4));
    let height = 4u16.min(area.height.saturating_sub(2));
    let (mut x, mut y) = (anchor.right() + 1, anchor.y + 1);
    if x + width + 1 > area.x + area.width {
        x = anchor.x + 2;
        y = anchor.bottom() + 1;
    }
    let bottom_edge = area.y + area.height;
    let right_edge = area.x + area.width;
    x = x.clamp(area.x, right_edge.saturating_sub(width));
    y = y.clamp(area.y, bottom_edge.saturating_sub(height));
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::warning_color()))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Theme::warning_color())
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
pub(super) fn format_relative_time(timestamp: &str) -> String {
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
pub(super) fn parse_iso_to_secs(s: &str) -> Option<u64> {
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
mod dialog_window_scroll_tests {
    use super::*;
    use crate::app::{ActionMenu, ActionMenuItem, ProfileDialog};
    use crate::test_support::render_to_text;
    use wyj_config::{Config, Profile};

    fn item(label: String, action: u8) -> ActionMenuItem<u8> {
        ActionMenuItem {
            label,
            action,
            dangerous: false,
            disabled: false,
            disabled_reason: None,
        }
    }

    fn render_menu(menu: &ActionMenu<(), u8>, width: u16, height: u16) -> String {
        render_to_text(
            |f| {
                draw_action_menu(
                    f,
                    Rect::new(0, 0, width, height),
                    Rect::new(10, 2, 60, 18),
                    "models",
                    menu,
                )
            },
            width,
            height,
        )
    }

    /// 用户现场：`/model` 的模型下拉有上百条，光标走到第 N 行后旧实现只画
    /// `items[0..inner]`，选中项与 ▶ 一起消失在屏幕外，按 ↓ 看起来毫无反应。
    #[test]
    fn long_action_menu_scrolls_to_keep_the_selected_item_visible() {
        let items = (0..60u8)
            .map(|i| item(format!("model-{i:02}"), i))
            .collect();
        let mut menu = ActionMenu::new((), items);
        menu.selected = 50;

        let text = render_menu(&menu, 80, 24);
        let selected_line = text
            .lines()
            .find(|l| l.contains("model-50"))
            .unwrap_or_else(|| panic!("selected item model-50 is off screen:\n{text}"));

        // 高亮必须跟着选中项一起进窗口，而不是留在屏幕外的某一行上。
        assert!(
            selected_line.contains('▶'),
            "no cursor on {selected_line:?}"
        );
        // 80x24 的菜单 inner 是 18 行：窗口应落在 33..=50。
        assert!(text.contains("model-33"), "window did not scroll:\n{text}");
        assert!(
            !text.contains("model-32"),
            "window start should be 33, found model-32:\n{text}"
        );
        assert!(
            !text.contains("model-00"),
            "first page should be scrolled away:\n{text}"
        );
        // 标题里的位置指示让用户知道"下面还有"。
        assert!(text.contains("51/60"), "missing scroll indicator:\n{text}");
    }

    /// 短菜单（绝大多数面板的行操作菜单）保持原样：全部可见、不出现位置指示。
    #[test]
    fn short_action_menu_renders_every_item_without_scrolling() {
        let items = (0..3u8).map(|i| item(format!("row-{i}"), i)).collect();
        let menu = ActionMenu::new((), items);

        let text = render_menu(&menu, 80, 24);
        for i in 0..3 {
            assert!(
                text.contains(&format!("row-{i}")),
                "missing row-{i}:\n{text}"
            );
        }
        assert!(!text.contains("1/3"), "no indicator needed:\n{text}");
    }

    fn many_profiles_dialog(count: usize) -> ProfileDialog {
        let mut cfg = Config::default();
        cfg.profiles = (0..count)
            .map(|i| Profile {
                name: format!("prof-{i:02}"),
                ..Profile::default()
            })
            .collect();
        cfg.active_profile = cfg.profiles[0].name.clone();
        let mut dialog = ProfileDialog::new_for_onboarding(&cfg);
        dialog.expanded = None;
        dialog.cursor = dialog.rows().len() - 1;
        dialog
    }

    /// 同一个根因的第二个落点：分组面板的分组一多，旧实现把全部行塞进无 scroll
    /// 的 Paragraph，光标走到屏幕外、末尾「+ 新建分组」和两条提示一起被裁掉。
    #[test]
    fn profile_rows_scroll_to_keep_cursor_and_footer_visible() {
        let dialog = many_profiles_dialog(40);
        assert_eq!(dialog.rows().len(), 41); // 40 头行 + 新建行

        let text = render_to_text(
            |f| draw_profile_dialog(f, &dialog, None, Rect::new(0, 0, 80, 24)),
            80,
            24,
        );

        let add_new = wyj_i18n::tr("profile.dialog.add_new_row");
        let add_line = text
            .lines()
            .find(|l| l.contains(&add_new))
            .unwrap_or_else(|| panic!("add-new row is off screen:\n{text}"));
        assert!(add_line.contains('▶'), "no cursor on {add_line:?}");

        // 窗口起点 = 40 + 1 - 16 = 25，因此 prof-25 在、prof-24 不在。
        assert!(text.contains("prof-25"), "window did not scroll:\n{text}");
        assert!(
            !text.contains("prof-24"),
            "window start should be 25:\n{text}"
        );
        assert!(
            !text.contains("prof-00"),
            "first page should be gone:\n{text}"
        );

        // 尾部固定区（分隔线 + 错误行 + 两条提示）不得被正文挤掉。
        let hint2: String = wyj_i18n::tr("profile.dialog.hint2")
            .chars()
            .take(8)
            .collect();
        assert!(text.contains(&hint2), "footer hint2 is missing:\n{text}");
    }

    /// 模板/未保存确认这类列表浮层是同一个入口：条目超出封顶高度时，
    /// 选中项同样必须留在窗口内（模板列表当前不长，但写法不该有两套）。
    #[test]
    fn list_overlay_scrolls_to_keep_the_selected_item_visible() {
        let items: Vec<String> = (0..40).map(|i| format!("item-{i:02}")).collect();
        let text = render_to_text(
            |f| {
                draw_profile_list_overlay(
                    f,
                    Rect::new(0, 0, 80, 24),
                    "profile.overlay.template_title",
                    items.clone(),
                    30,
                )
            },
            80,
            24,
        );

        let selected_line = text
            .lines()
            .find(|l| l.contains("item-30"))
            .unwrap_or_else(|| panic!("selected item item-30 is off screen:\n{text}"));
        assert!(
            selected_line.contains('▶'),
            "no cursor on {selected_line:?}"
        );
        // 80x24：inner 20 行，窗口应落在 11..=30。
        assert!(text.contains("item-11"), "window did not scroll:\n{text}");
        assert!(
            !text.contains("item-10"),
            "window start should be 11:\n{text}"
        );
        assert!(text.contains("31/40"), "missing scroll indicator:\n{text}");
    }

    /// 分组少时不动窗口：仍从第一行画起，行为与修复前一致。
    #[test]
    fn small_profile_dialog_starts_at_the_first_row() {
        let dialog = many_profiles_dialog(3);
        let text = render_to_text(
            |f| draw_profile_dialog(f, &dialog, None, Rect::new(0, 0, 80, 24)),
            80,
            24,
        );
        assert!(text.contains("prof-00"));
        assert!(text.contains(&wyj_i18n::tr("profile.dialog.add_new_row")));
    }
}
