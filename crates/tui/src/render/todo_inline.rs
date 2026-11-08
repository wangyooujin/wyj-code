//! 消息流内嵌 tasklist：任务 → 步骤摘要 → 执行内容。
//!
//! - `Ctrl+T`（聚焦任务后）展开/收起该任务下的步骤摘要
//! - `Ctrl+O`（聚焦任务后）展开/收起步骤下的具体执行（bash/read/write 等）

use super::*;
use std::collections::HashSet;

/// 进行中任务步骤未展开时，默认露出的跟尾行数。
pub(super) const TODO_LIVE_VIEWPORT: usize = 4;

/// 收集已归因到任一 todo 的消息 id（主聊天归并用）。
pub(super) fn attributed_todo_message_ids(
    todo_logs: &HashMap<String, Vec<TodoExecutionEntry>>,
) -> HashSet<u64> {
    let mut ids = HashSet::new();
    for entries in todo_logs.values() {
        for entry in entries {
            if let TodoExecutionEntry::Message(id) = entry {
                ids.insert(*id);
            }
        }
    }
    ids
}

/// 主聊天是否应跳过该消息：有 tasklist 时，已归因的 ToolCall/ToolResult 只画在任务下。
pub(super) fn should_skip_attributed_tool_message(
    msg: &ChatMessage,
    attributed: Option<&HashSet<u64>>,
) -> bool {
    let Some(ids) = attributed else {
        return false;
    };
    if !matches!(msg.role, MessageRole::ToolCall | MessageRole::ToolResult) {
        return false;
    }
    ids.contains(&msg.id)
}

/// 有 tasklist 时，面板插在本轮「工作消息」之后、「最终助手正文」之前。
///
/// 根因：旧实现把 todos 挂在 `messages` + `streaming_buf` **之后**，最终回复
/// 永远画在列表上方，跟随时列表又贴在输入框附近——看起来像停靠底部。
/// 正确顺序是：本轮工具/思考 → 任务列表 → 最终输出 / 流式正文。
pub(super) fn todo_panel_split_index(
    messages: &[ChatMessage],
    attributed: Option<&HashSet<u64>>,
) -> usize {
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m.role, MessageRole::User));
    let turn_start = last_user.unwrap_or(0);

    let mut last_work: Option<usize> = None;
    for (i, m) in messages.iter().enumerate().skip(turn_start) {
        let attributed_hit = attributed.is_some_and(|ids| ids.contains(&m.id));
        let is_work = attributed_hit
            || matches!(
                m.role,
                MessageRole::Thinking
                    | MessageRole::ToolCall
                    | MessageRole::ToolResult
                    | MessageRole::TurnSummary
            );
        if is_work {
            last_work = Some(i);
        }
    }

    match last_work {
        Some(i) => i + 1,
        // 本轮还没有工具/思考：跟在最近一条用户消息后，流式最终回复仍能落在面板下。
        None => last_user.map(|i| i + 1).unwrap_or(messages.len()),
    }
}

#[derive(Clone, Copy)]
enum StepKind {
    Done,
    Live,
    Note,
}

struct CompactStep {
    kind: StepKind,
    text: String,
    /// 成对的 ToolCall；Note 为 None。
    call: Option<ChatMessage>,
    /// 对应 ToolResult（若已结束）。
    result: Option<ChatMessage>,
}

/// 从 `Read(path)` / `Bash(cmd)` 标题里抽出括号内参数，作步骤摘要的宾语。
fn tool_arg_from_title(content: &str) -> &str {
    if let Some(start) = content.find('(') {
        if let Some(end) = content.rfind(')') {
            if end > start + 1 {
                return content[start + 1..end].trim();
            }
        }
    }
    content.trim()
}

/// 步骤层文案：动词短语 + 短参数（「读取 · app.rs」），不是裸工具注册名。
fn step_semantic_text(msg: &ChatMessage) -> String {
    let name = msg.tool_name.as_deref().unwrap_or("Tool");
    let activity = tool_activity_label(name);
    let arg = tool_arg_from_title(&msg.content);
    if arg.is_empty() {
        activity
    } else {
        truncate_line(&format!("{activity} · {arg}"), 72)
    }
}

/// 把 todo 执行日志压成步骤（工具一对一行；Note 一行）。
fn collect_compact_steps(
    log: &[TodoExecutionEntry],
    messages: &[ChatMessage],
    live_tool_out: &HashMap<usize, String>,
) -> Vec<CompactStep> {
    let mut steps = Vec::new();
    let mut i = 0;
    while i < log.len() {
        match &log[i] {
            TodoExecutionEntry::Note(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    steps.push(CompactStep {
                        kind: StepKind::Note,
                        text: trimmed.to_string(),
                        call: None,
                        result: None,
                    });
                }
                i += 1;
            }
            TodoExecutionEntry::Message(id) => {
                let Some(msg) = messages.iter().find(|m| m.id == *id) else {
                    i += 1;
                    continue;
                };
                match msg.role {
                    MessageRole::ToolCall => {
                        let result = messages.iter().find(|m| {
                            matches!(m.role, MessageRole::ToolResult)
                                && m.sequence_no.is_some()
                                && m.sequence_no == msg.sequence_no
                        });
                        // 失败 Bash：单独成步无阅读价值（模型常自愈重试），跳过。
                        if let Some(res) = result {
                            if is_failed_command_pair(msg, res) {
                                i += 1;
                                continue;
                            }
                        }
                        let mut text = step_semantic_text(msg);
                        if let Some(res) = result {
                            if res.is_error {
                                text.push_str(" ✗");
                            }
                            steps.push(CompactStep {
                                kind: StepKind::Done,
                                text,
                                call: Some(msg.clone()),
                                result: Some(res.clone()),
                            });
                        } else {
                            if let Some(seq) = msg.sequence_no {
                                if let Some(tail) = live_tool_out.get(&seq) {
                                    let last = tail
                                        .lines()
                                        .rev()
                                        .find(|l| !l.trim().is_empty())
                                        .unwrap_or("")
                                        .trim();
                                    if !last.is_empty() {
                                        text = truncate_line(&format!("{text} · {last}"), 72);
                                    }
                                }
                            }
                            steps.push(CompactStep {
                                kind: StepKind::Live,
                                text,
                                call: Some(msg.clone()),
                                result: None,
                            });
                        }
                        i += 1;
                    }
                    MessageRole::ToolResult => {
                        // 成对的 ToolCall 已画过；仅当日志里没有对应 Call 时画孤儿结果。
                        let has_call = msg.sequence_no.is_some_and(|seq| {
                            log.iter().any(|e| {
                                matches!(e, TodoExecutionEntry::Message(mid)
                                if messages.iter().any(|m| {
                                    m.id == *mid
                                        && matches!(m.role, MessageRole::ToolCall)
                                        && m.sequence_no == Some(seq)
                                }))
                            })
                        });
                        if !has_call {
                            let name = msg.tool_name.as_deref().unwrap_or("Tool");
                            let summary = if msg.display_summary.is_empty() {
                                tool_activity_label(name)
                            } else {
                                format!("{} · {}", tool_activity_label(name), msg.display_summary)
                            };
                            steps.push(CompactStep {
                                kind: StepKind::Done,
                                text: truncate_line(&summary, 72),
                                call: None,
                                result: Some(msg.clone()),
                            });
                        }
                        i += 1;
                    }
                    // Assistant / Thinking 等留在主聊天，不进任务下摘要。
                    _ => {
                        i += 1;
                    }
                }
            }
        }
    }
    steps
}

fn push_step_line(
    lines: &mut Vec<Line<'static>>,
    step: &CompactStep,
    spinner_frame: usize,
    max_width: usize,
) {
    let (glyph, style) = match step.kind {
        StepKind::Done => (
            "· ".to_string(),
            Style::default()
                .fg(Theme::muted_color())
                .add_modifier(Modifier::DIM),
        ),
        StepKind::Live => {
            let frame = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
            (
                format!("{frame} "),
                Style::default()
                    .fg(Theme::info_color())
                    .add_modifier(Modifier::BOLD),
            )
        }
        StepKind::Note => ("· ".to_string(), Theme::dim()),
    };
    let text = truncate_line(&step.text, max_width.saturating_sub(6));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(glyph, style),
        Span::styled(text, style),
    ]));
}

/// 在步骤下画出执行层：工具标题 + 完整命令 + 结果/实时输出。
fn push_step_execution(
    lines: &mut Vec<Line<'static>>,
    step: &CompactStep,
    live_tool_out: &HashMap<usize, String>,
    max_content_width: usize,
    spinner_frame: usize,
) {
    let indent = "      ";
    let width = max_content_width.saturating_sub(indent.len()).max(8);

    if let Some(call) = &step.call {
        let title = truncate_line(&call.content, width.saturating_sub(4));
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled("⏺ ".to_string(), Theme::tool_call()),
            Span::styled(title.clone(), Theme::tool_call()),
        ];
        if let Some(res) = &step.result {
            if let Some(secs) = res.elapsed_secs.filter(|&s| s > 0.0) {
                spans.push(Span::styled(
                    format!(" · {}", format_elapsed(secs)),
                    Theme::dim(),
                ));
            }
            if res.is_error {
                spans.push(Span::styled(" · failed", Theme::error()));
            }
        }
        lines.push(Line::from(spans));

        // 完整命令（标题被截断时）
        if let Some(cmd) = call.full_command.as_deref() {
            if !title.contains(cmd) {
                for raw in cmd.lines() {
                    for part in wrap_line(raw, width) {
                        lines.push(Line::from(Span::styled(
                            format!("{indent}  {part}"),
                            Theme::dim(),
                        )));
                    }
                }
            }
        }

        if let Some(res) = &step.result {
            push_exec_result_body(lines, res, indent, width);
        } else if let Some(seq) = call.sequence_no {
            if let Some(tail) = live_tool_out.get(&seq) {
                let frame = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
                lines.push(Line::from(Span::styled(
                    format!("{indent}{frame} …"),
                    Theme::info(),
                )));
                let take = LIVE_TAIL_LINES.min(8);
                let body_lines: Vec<&str> = tail.lines().rev().take(take).collect();
                for raw in body_lines.into_iter().rev() {
                    for part in wrap_line(raw, width.saturating_sub(2)) {
                        lines.push(Line::from(Span::styled(
                            format!("{indent}  {part}"),
                            Theme::tool_result(),
                        )));
                    }
                }
            }
        }
    } else if let Some(res) = &step.result {
        let name = res.tool_name.as_deref().unwrap_or("Tool");
        lines.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(format!("⏺ {name}"), Theme::tool_call()),
        ]));
        push_exec_result_body(lines, res, indent, width);
    }
}

fn push_exec_result_body(
    lines: &mut Vec<Line<'static>>,
    result: &ChatMessage,
    indent: &str,
    width: usize,
) {
    let summary = if result.display_summary.is_empty() {
        if result.is_error {
            "failed".to_string()
        } else {
            "done".to_string()
        }
    } else {
        truncate_line(&result.display_summary, width.saturating_sub(4))
    };
    lines.push(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled("⎿ ".to_string(), Theme::dim()),
        Span::styled(
            summary,
            if result.is_error {
                Theme::error()
            } else {
                Theme::dim()
            },
        ),
    ]));

    // 执行层展开 = 要看具体输出。Edit/Write 出 diff 预览；其余出正文尾部。
    let content = result.content.as_str();
    if content.is_empty() {
        return;
    }
    let max_lines = if is_diff_result(result) {
        COLLAPSED_DIFF_MAX_LINES
    } else {
        40
    };
    let all: Vec<&str> = content.lines().collect();
    let omitted = all.len().saturating_sub(max_lines);
    let shown = if omitted > 0 {
        &all[all.len() - max_lines..]
    } else {
        &all[..]
    };
    if omitted > 0 {
        lines.push(Line::from(Span::styled(
            format!("{indent}  … +{omitted}"),
            Theme::dim(),
        )));
    }
    let body_style = if result.is_error {
        Theme::error()
    } else {
        Theme::tool_result()
    };
    for raw in shown {
        for part in wrap_line(raw, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(
                format!("{indent}  {part}"),
                body_style,
            )));
        }
    }
}

/// 在任务名下画出步骤 / 执行两层；对需要滚动的视口返回 (scroll, max_scroll)。
///
/// - `steps_open`：画出步骤摘要行
/// - `exec_open`：在每步下画出具体执行内容
/// - 步骤未开且进行中：跟尾预览若干步；完成态默认不预览（需 Ctrl+T）
#[allow(clippy::too_many_arguments)]
pub(super) fn push_todo_execution_summary(
    lines: &mut Vec<Line<'static>>,
    item: &wyj_tools::todo::TodoItem,
    messages: &[ChatMessage],
    log: &[TodoExecutionEntry],
    live_tool_out: &HashMap<usize, String>,
    spinner_frame: usize,
    selected: bool,
    steps_open: bool,
    exec_open: bool,
    detail_scroll: u16,
    max_content_width: usize,
) -> (u16, u16) {
    if matches!(item.status, TodoStatus::Pending) {
        return (detail_scroll, 0);
    }
    let steps = collect_compact_steps(log, messages, live_tool_out);
    if steps.is_empty() {
        if matches!(item.status, TodoStatus::InProgress) {
            lines.push(Line::from(Span::styled(
                format!("    {}", wyj_i18n::tr("spinner.todo_waiting_steps")),
                Theme::dim(),
            )));
        }
        return (detail_scroll, 0);
    }

    // 步骤未展开：进行中跟尾预览；完成态不画步骤（用户 Ctrl+T 再看）。
    if !steps_open {
        if matches!(item.status, TodoStatus::InProgress) {
            let viewport = TODO_LIVE_VIEWPORT.min(steps.len());
            let skip = steps.len().saturating_sub(viewport);
            let step_width = max_content_width.saturating_sub(2).max(8);
            if skip > 0 {
                lines.push(Line::from(Span::styled(
                    format!("    · +{skip}"),
                    Theme::dim(),
                )));
            }
            for step in steps.iter().skip(skip) {
                push_step_line(lines, step, spinner_frame, step_width);
            }
        }
        return (0, 0);
    }

    let step_width = max_content_width.saturating_sub(2).max(8);
    let mut body: Vec<Line<'static>> = Vec::new();
    for step in &steps {
        push_step_line(&mut body, step, spinner_frame, step_width);
        if exec_open {
            push_step_execution(
                &mut body,
                step,
                live_tool_out,
                max_content_width,
                spinner_frame,
            );
        }
    }

    // 步骤/执行展开后整段画进聊天流，由 chat_scroll 负责；此处不再做内部视口裁剪。
    let _ = (selected, detail_scroll);
    lines.extend(body);
    (0, 0)
}
