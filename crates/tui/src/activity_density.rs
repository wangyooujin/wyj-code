//! 聊天区活动密度：探索合并 / Compact 回合摘要的呈现层辅助。
//!
//! 只影响 TUI 渲染，不改 `ChatMessage` 入库顺序或模型上下文。

use crate::app::{ChatMessage, MessageRole};

/// 工具在主视图里的呈现类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolDisplayKind {
    /// Read / Grep / Glob 等：Balanced 下可合并为 Explored N。
    Explore,
    /// Edit / Write。
    Edit,
    /// Bash / KillShell / BashOutput 工具。
    Command,
    /// 其余动作（MCP、Agent、Computer…）：Compact 摘要里计入「其它」，不合并进 Explored。
    Action,
}

/// 探索类合并白名单（大小写不敏感）。不在名单内的（含 MCP）永不进 Explored。
pub(crate) fn tool_display_kind(tool_name: &str) -> ToolDisplayKind {
    match tool_name.trim().to_ascii_lowercase().as_str() {
        "read" | "grep" | "glob" | "codesearch" | "webfetch" | "websearch" | "toolsearch"
        | "memory" | "contextrecall" => ToolDisplayKind::Explore,
        "edit" | "write" => ToolDisplayKind::Edit,
        "bash" | "bashoutput" | "killshell" => ToolDisplayKind::Command,
        _ => ToolDisplayKind::Action,
    }
}

pub(crate) fn is_explore_tool(tool_name: &str) -> bool {
    matches!(tool_display_kind(tool_name), ToolDisplayKind::Explore)
}

/// 从 ToolCall 或配对的 ToolResult 上取工具名。
pub(crate) fn message_tool_name<'a>(
    call: &'a ChatMessage,
    result: Option<&'a ChatMessage>,
) -> Option<&'a str> {
    call.tool_name
        .as_deref()
        .or_else(|| result.and_then(|r| r.tool_name.as_deref()))
}

/// 已完成且成功的探索类 ToolCall+ToolResult 对。
pub(crate) fn is_successful_explore_pair(call: &ChatMessage, result: &ChatMessage) -> bool {
    if !matches!(call.role, MessageRole::ToolCall) {
        return false;
    }
    if !matches!(result.role, MessageRole::ToolResult) {
        return false;
    }
    if call.sequence_no.is_none() || call.sequence_no != result.sequence_no {
        return false;
    }
    if result.is_error {
        return false;
    }
    message_tool_name(call, Some(result)).is_some_and(is_explore_tool)
}

/// 一轮（User…下一 User）内成功活动的计数，供 Compact 摘要行。
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnActivityStats {
    pub explored: usize,
    pub edits: usize,
    pub commands: usize,
    pub other_actions: usize,
    /// 回合内 thinking 累计秒数（多段相加）。
    pub thinking_secs: f64,
    /// 回合末 TurnSummary 原文（若有），Compact 时可并入摘要。
    pub turn_summary: Option<String>,
    /// 是否存在可折叠的成功活动（不含 thinking / TurnSummary）。
    pub has_foldable_activity: bool,
}

/// 统计 `messages[start..end)` 内可折叠进 Compact 摘要的成功活动。
///
/// 失败、进行中（只有 ToolCall 无配对结果）、BashOutput、Plan、System 不计入
/// `has_foldable_activity` 的「必须露出」集合——调用方仍会单独渲染它们。
pub(crate) fn collect_turn_activity_stats(messages: &[ChatMessage]) -> TurnActivityStats {
    let mut stats = TurnActivityStats::default();
    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        match msg.role {
            MessageRole::Thinking => {
                if let Some(secs) = msg.thinking_elapsed_secs {
                    stats.thinking_secs += secs;
                }
                i += 1;
            }
            MessageRole::TurnSummary => {
                stats.turn_summary = Some(msg.content.clone());
                i += 1;
            }
            MessageRole::ToolCall => {
                if let Some(result) = messages.get(i + 1).filter(|r| {
                    matches!(r.role, MessageRole::ToolResult) && r.sequence_no == msg.sequence_no
                }) {
                    if !result.is_error {
                        let name = message_tool_name(msg, Some(result)).unwrap_or("");
                        match tool_display_kind(name) {
                            ToolDisplayKind::Explore => {
                                stats.explored += 1;
                                stats.has_foldable_activity = true;
                            }
                            ToolDisplayKind::Edit => {
                                stats.edits += 1;
                                stats.has_foldable_activity = true;
                            }
                            ToolDisplayKind::Command => {
                                stats.commands += 1;
                                stats.has_foldable_activity = true;
                            }
                            ToolDisplayKind::Action => {
                                stats.other_actions += 1;
                                stats.has_foldable_activity = true;
                            }
                        }
                    }
                    i += 2;
                } else {
                    // 进行中或未配对：不进折叠桶。
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    stats
}

/// Compact 摘要行文案（不含 hint）。
pub(crate) fn format_turn_activity_summary(stats: &TurnActivityStats) -> String {
    let elapsed = if let Some(summary) = &stats.turn_summary {
        // TurnSummary 形如 `⏱ 0:42 · ↑1.2k ↓300`——Compact 优先用它的耗时段。
        summary
            .split('·')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_start_matches('⏱').trim().to_string())
            .unwrap_or_else(|| {
                if stats.thinking_secs > 0.0 {
                    format!("{:.0}s", stats.thinking_secs)
                } else {
                    "—".to_string()
                }
            })
    } else if stats.thinking_secs > 0.0 {
        format!("{:.0}s", stats.thinking_secs)
    } else {
        "—".to_string()
    };

    wyj_i18n::tr_fmt(
        "density.turn_summary",
        &[
            ("elapsed", elapsed.as_str()),
            ("explored", &stats.explored.to_string()),
            ("edits", &stats.edits.to_string()),
            ("commands", &stats.commands.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_config::ActivityDensity;

    #[test]
    fn explore_whitelist_is_case_insensitive() {
        assert!(is_explore_tool("Read"));
        assert!(is_explore_tool("grep"));
        assert!(is_explore_tool("CodeSearch"));
        assert!(!is_explore_tool("Bash"));
        assert!(!is_explore_tool("Edit"));
        assert!(!is_explore_tool("mcp__foo__bar"));
    }

    #[test]
    fn density_cycle_order() {
        assert_eq!(ActivityDensity::Compact.cycle(), ActivityDensity::Balanced);
        assert_eq!(ActivityDensity::Balanced.cycle(), ActivityDensity::Detailed);
        assert_eq!(ActivityDensity::Detailed.cycle(), ActivityDensity::Compact);
    }
}
