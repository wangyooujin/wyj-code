//! 消息流内嵌 tasklist：轻量标题 + 每任务下 compact 执行摘要。

use super::*;
use std::collections::HashSet;

/// 进行中任务下默认露出的执行步骤行数（跟尾滚动）。
pub(super) const TODO_LIVE_VIEWPORT: usize = 4;
/// 已完成任务下默认露出的末尾步骤数；超出用 +N 提示。
pub(super) const TODO_COMPLETED_PREVIEW: usize = 4;

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

#[derive(Clone, Copy)]
enum StepKind {
    Done,
    Live,
    Note,
}

struct CompactStep {
    kind: StepKind,
    text: String,
}

/// 把 todo 执行日志压成 compact 步骤（工具一对一行；Note 一行）。
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
                        let label = truncate_line(&msg.content, 72);
                        if let Some(res) = result {
                            let mut text = label;
                            if res.is_error {
                                text.push_str(" ✗");
                            }
                            steps.push(CompactStep {
                                kind: StepKind::Done,
                                text,
                            });
                        } else {
                            let mut text = label;
                            if let Some(seq) = msg.sequence_no {
                                if let Some(tail) = live_tool_out.get(&seq) {
                                    let last = tail
                                        .lines()
                                        .rev()
                                        .find(|l| !l.trim().is_empty())
                                        .unwrap_or("")
                                        .trim();
                                    if !last.is_empty() {
                                        text = truncate_line(
                                            &format!("{text} · {last}"),
                                            72,
                                        );
                                    }
                                }
                            }
                            steps.push(CompactStep {
                                kind: StepKind::Live,
                                text,
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
                                name.to_string()
                            } else {
                                format!("{name} · {}", msg.display_summary)
                            };
                            steps.push(CompactStep {
                                kind: StepKind::Done,
                                text: truncate_line(&summary, 72),
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
            "✓ ".to_string(),
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

/// 在任务名下画出 compact 执行摘要；对需要滚动的视口返回 (scroll, max_scroll)。
///
/// - `Completed`：默认末尾 `TODO_COMPLETED_PREVIEW` 行；`detail_open` 时全量。
/// - `InProgress`：默认 `TODO_LIVE_VIEWPORT` 跟尾；选中且 `detail_open` 时全量可滚。
/// - `Pending`：无摘要。
#[allow(clippy::too_many_arguments)]
pub(super) fn push_todo_execution_summary(
    lines: &mut Vec<Line<'static>>,
    item: &wyj_tools::todo::TodoItem,
    messages: &[ChatMessage],
    log: &[TodoExecutionEntry],
    live_tool_out: &HashMap<usize, String>,
    spinner_frame: usize,
    selected: bool,
    detail_open: bool,
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

    let full = detail_open && selected;
    let viewport = if full {
        steps.len().max(1)
    } else if matches!(item.status, TodoStatus::InProgress) {
        TODO_LIVE_VIEWPORT
    } else {
        TODO_COMPLETED_PREVIEW
    };

    let mut body: Vec<Line<'static>> = Vec::new();
    let step_width = max_content_width.saturating_sub(2).max(8);

    if !full && matches!(item.status, TodoStatus::Completed) && steps.len() > viewport {
        let omitted = steps.len() - viewport;
        body.push(Line::from(Span::styled(
            format!("    · +{omitted}"),
            Theme::dim(),
        )));
        for step in steps.iter().skip(omitted) {
            push_step_line(&mut body, step, spinner_frame, step_width);
        }
        for line in body {
            lines.push(line);
        }
        return (0, 0);
    }

    for step in &steps {
        push_step_line(&mut body, step, spinner_frame, step_width);
    }

    let max_scroll = body.len().saturating_sub(viewport.min(body.len()));
    let scroll = if full && selected {
        (detail_scroll as usize).min(max_scroll)
    } else {
        // 进行中默认贴尾；完成态预览已在上面处理。
        max_scroll
    };
    for line in body.into_iter().skip(scroll).take(viewport) {
        lines.push(line);
    }
    (
        scroll as u16,
        max_scroll.min(u16::MAX as usize) as u16,
    )
}
