//! 消息流内嵌 agentlist：贴在各自 Agent ToolCall 下方。
//!
//! - 运行中：每个 agent 固定 [`AGENT_LIVE_VIEWPORT`] 物理行，默认 follow_tail
//! - 全部完成后：默认一行摘要；Ctrl+A 展开整表（与 tasklist / Ctrl+T 同构）
//! - 完成后执行轨迹不灌进主历史；主聊天只留 Agent ToolResult / handoff（方案 A）

use super::*;

/// 每个运行中子 Agent 在主聊天下的固定视口高度（物理行）。
pub(super) const AGENT_LIVE_VIEWPORT: usize = 8;

/// 本轮（最近一条 User 之后）出现过的子 Agent id，按首次出现顺序。
pub(super) fn turn_agent_ids(messages: &[ChatMessage]) -> Vec<u64> {
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m.role, MessageRole::User))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut ids = Vec::new();
    for msg in messages.iter().skip(last_user) {
        let Some(id) = msg.sub_agent_id else {
            continue;
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// 本轮 agent 是否全部结束（至少有一个，且没有 Running）。
pub(super) fn turn_agents_all_done(
    messages: &[ChatMessage],
    sub_agents: &std::collections::BTreeMap<u64, SubAgentUiState>,
) -> bool {
    let ids = turn_agent_ids(messages);
    if ids.is_empty() {
        return false;
    }
    ids.iter().all(|id| {
        sub_agents
            .get(id)
            .is_some_and(|s| s.status != SubAgentStatus::Running)
    })
}

/// 摘要行应插在本轮最后一个带 `sub_agent_id` 的消息之后。
pub(super) fn agents_summary_split_index(messages: &[ChatMessage]) -> Option<usize> {
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m.role, MessageRole::User))
        .map(|i| i + 1)
        .unwrap_or(0);
    messages
        .iter()
        .enumerate()
        .skip(last_user)
        .rev()
        .find(|(_, m)| m.sub_agent_id.is_some())
        .map(|(i, _)| i + 1)
}

/// 把子 Agent 详情消息压成固定高度视口行（indent 挂在 Agent ToolCall 下）。
pub(super) fn push_agent_live_viewport(
    lines: &mut Vec<Line<'static>>,
    body: &[Line<'static>],
    selected: bool,
    scroll: u16,
    follow_tail: bool,
    max_content_width: usize,
) -> (u16, u16) {
    let viewport = AGENT_LIVE_VIEWPORT;
    let max_scroll = body.len().saturating_sub(viewport) as u16;
    let scroll_y = if follow_tail {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };
    let start = scroll_y as usize;
    let end = (start + viewport).min(body.len());

    let indent = if selected { "    ▶ " } else { "      " };
    let width = max_content_width.saturating_sub(indent.len()).max(8);

    if body.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("{indent}{}", wyj_i18n::tr("subagent.detail_running_idle")),
            Theme::dim(),
        )));
        // 空内容也占满视口高度，避免列表跳动。
        for _ in 1..viewport {
            lines.push(Line::from(Span::raw(String::new())));
        }
        return (0, 0);
    }

    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("{indent}⋮ +{start}"),
            Theme::dim(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{indent}{}", "─".repeat(width.min(24))),
            Theme::dim(),
        )));
    }

    let mut painted = 1usize; // 顶部分隔 / omitted 行
    for line in &body[start..end] {
        let mut spans = vec![Span::raw(indent.to_string())];
        for span in &line.spans {
            spans.push(Span::styled(span.content.clone(), span.style));
        }
        lines.push(Line::from(spans));
        painted += 1;
    }
    while painted < viewport + 1 {
        lines.push(Line::from(Span::raw(String::new())));
        painted += 1;
    }
    if end < body.len() {
        let below = body.len() - end;
        lines.push(Line::from(Span::styled(
            format!("{indent}⋮ +{below}"),
            Theme::dim(),
        )));
    }
    (scroll_y, max_scroll)
}

/// 全部完成后的摘要 / 展开列表。
pub(super) fn push_agents_done_block(
    lines: &mut Vec<Line<'static>>,
    ids: &[u64],
    sub_agents: &std::collections::BTreeMap<u64, SubAgentUiState>,
    expanded: bool,
    focused: bool,
    selected_id: Option<u64>,
    max_content_width: usize,
) {
    if ids.is_empty() {
        return;
    }
    let total = ids.len();
    let done = ids
        .iter()
        .filter(|id| {
            sub_agents
                .get(id)
                .is_some_and(|s| s.status != SubAgentStatus::Running)
        })
        .count();

    let mut total_elapsed = 0.0f64;
    let mut total_in = 0u32;
    let mut total_out = 0u32;
    for id in ids {
        if let Some(s) = sub_agents.get(id) {
            total_elapsed += s.elapsed_secs();
            total_in += s.input_tokens;
            total_out += s.output_tokens;
        }
    }
    let stats_suffix = format!(
        " ⏱ {} ↑{} ↓{}",
        format_hms(total_elapsed),
        fmt_tokens(total_in),
        fmt_tokens(total_out)
    );
    let title = format!(
        "{} [{done}/{total}]{stats_suffix}",
        wyj_i18n::tr("spinner.agents_done")
    );
    let hint = if expanded {
        wyj_i18n::tr("spinner.agents_collapse")
    } else {
        wyj_i18n::tr("spinner.agents_expand")
    };

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", Theme::dim()),
        Span::styled(
            title,
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(hint, Theme::dim()),
    ]));

    if !expanded {
        return;
    }

    let max_item_width = max_content_width.saturating_sub(10);
    for id in ids {
        let Some(s) = sub_agents.get(id) else {
            continue;
        };
        let is_selected = focused && selected_id == Some(*id);
        let marker = if is_selected { "▶ " } else { "  " };
        let (icon, style) = match s.status {
            SubAgentStatus::Done => ("✓".to_string(), Theme::success()),
            SubAgentStatus::Failed => ("✗".to_string(), Theme::error()),
            SubAgentStatus::Interrupted => ("⊘".to_string(), Theme::error()),
            SubAgentStatus::Running => {
                // 调用方只在 all_done 时进来；兜底。
                ("·".to_string(), Theme::dim())
            }
        };
        let bg_tag = if s.background { " ◇bg" } else { "" };
        let head = truncate_line(
            &format!("a{id} {}({})", s.agent_type, s.description),
            max_item_width.saturating_sub(28),
        );
        let spans = vec![
            Span::styled(marker, Theme::dim()),
            Span::styled(format!("{icon} "), style),
            Span::styled(
                head,
                if is_selected {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                },
            ),
            Span::styled(
                format!(
                    " ⏱ {} ↑{} ↓{}{bg_tag}",
                    format_hms(s.elapsed_secs()),
                    fmt_tokens(s.input_tokens),
                    fmt_tokens(s.output_tokens),
                ),
                Theme::dim(),
            ),
        ];
        lines.push(Line::from(spans));
        if let Some(handoff) = s.handoff.as_ref() {
            push_sub_agent_handoff_line(lines, handoff, max_content_width.saturating_sub(4));
        } else if let Some(result) = s.final_result.as_deref() {
            let summary = truncate_line(result.trim(), max_content_width.saturating_sub(8));
            if !summary.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("      ⎿  ", Theme::dim()),
                    Span::styled(summary, Theme::dim()),
                ]));
            }
        }
    }
}
