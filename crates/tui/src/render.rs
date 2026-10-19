//! 对话渲染与布局

use crate::app::{
    fmt_tokens, format_hms, ActionMenu, AgentsDialog, AppState, AskQuestionDialog,
    AskQuestionStage, Attachment, ChatMessage, EvolutionDialog, EvolutionRow,
    ExecModeConfirmDialog, ExtensionsDialog, FlatRow, InProgressAnswer, InputOwner, McpConnStatus,
    McpDialog, McpDialogTab, McpOverlay, MemoryDialog, MemoryRow, MessageRole, PermissionDialog,
    PlanApprovalDialog, PluginOverlay, PluginsDialog, PluginsDialogTab, ProfileDialog,
    ProfileFieldKind, ProfileInputField, ProfileOverlay, ProfileRow, ScheduleDialog,
    ScheduleInputField, ScheduleOverlay, ScheduleRow, SessionConflictDialog, SessionPickerState,
    SettingsDialog, SkillsBrowseSource, SkillsDialog, SkillsDialogTab, SkillsOverlay,
    SubAgentStatus, SubAgentUiState, SubToolLine, TodoExecutionEntry, TodoRuntimeStats, UiFocus,
    PROFILE_FIELD_SPECS, SCHEDULE_FIELD_LABEL_KEYS, SCHEDULE_FIELD_NOTIFY, SETTINGS_FIELD_COUNT,
    SETTINGS_FIELD_LABEL_KEYS,
};
use crate::input::{is_atomic_marker, InputBox, ATTACHMENT_MARKER, PASTE_MARKER};
use crate::markdown::{
    display_width, render_markdown, render_streaming_markdown, render_streaming_thinking_markdown,
    render_thinking_markdown,
};
use crate::theme::Theme;
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
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
            Theme::hint().add_modifier(Modifier::BOLD),
        ));
        rest = &rest[at_pos + 1 + end..];
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    Line::from(spans)
}

/// 将待发送附件的内部原子标记投影为输入框内的只读占位符。
///
/// InputBox 只保存不可见的单字符标记，发送正文时会过滤；这里按标记在编辑序列
/// 中的真实位置展开图片/文件标签，所以占位符会跟随粘贴位置，而不是堆到首行行首。
///
/// 折叠的长文本粘贴（`PASTE_MARKER`）走**同一个循环**，展开成
/// `[Pasted text #n +N lines]`。两类标记可以任意交错（先贴图、再贴一大段、再贴图），
/// 所以必须是"一次遍历同时识别两种标记"，而不是各投影一遍再把两个 InputBox 合起来
/// ——「两遍合成」正是附件占位符曾经出过 bug 的形态（同一个附件显示两次）。
/// 两张表**编号空间独立**：`[Image #1]` 数图片，`[Pasted text #1]` 数折叠块，
/// 中间插入另一类不会让任一方跳号。
fn input_with_placeholders(
    input: &InputBox,
    attachments: &[Attachment],
    pastes: &[crate::app::PasteBlock],
) -> InputBox {
    let mut attachment_labels = Vec::with_capacity(attachments.len());
    let mut image_number = 0usize;
    for attachment in attachments {
        match attachment {
            Attachment::Image { .. } => {
                image_number += 1;
                attachment_labels.push(format!("[Image #{image_number}]"));
            }
            Attachment::File { path } => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                attachment_labels.push(format!("[File: {name}]"));
            }
        }
    }
    let paste_labels: Vec<String> = pastes
        .iter()
        .enumerate()
        .map(|(idx, block)| format!("[Pasted text #{} +{} lines]", idx + 1, block.line_count))
        .collect();

    let mut display = input.clone();
    let mut attachment_idx = 0usize;
    let mut paste_idx = 0usize;
    for (row, source) in input.lines.iter().enumerate() {
        let chars = source.chars().collect::<Vec<_>>();
        let mut rendered = String::new();
        let mut mapped_cursor = None;

        for col in 0..=chars.len() {
            if row == input.cursor_row && col == input.cursor_col {
                mapped_cursor = Some(rendered.chars().count());
            }
            if col == chars.len() {
                break;
            }

            let ch = chars[col];
            let label = match ch {
                ATTACHMENT_MARKER => attachment_labels.get(attachment_idx),
                PASTE_MARKER => paste_labels.get(paste_idx),
                _ => {
                    rendered.push(ch);
                    continue;
                }
            };
            if ch == ATTACHMENT_MARKER {
                attachment_idx += 1;
            } else {
                paste_idx += 1;
            }
            let Some(label) = label else {
                continue;
            };

            if rendered
                .chars()
                .last()
                .is_some_and(|ch| !ch.is_whitespace())
            {
                rendered.push(' ');
            }
            rendered.push_str(label);
            let next = chars.get(col + 1).copied();
            if next.is_none()
                || next.is_some_and(is_atomic_marker)
                || next.is_some_and(|ch| !ch.is_whitespace())
            {
                rendered.push(' ');
            }
        }

        display.lines[row] = rendered;
        if let Some(cursor_col) = mapped_cursor {
            display.cursor_col = cursor_col;
        }
    }

    // 兼容升级前已存在但没有编辑器标记的瞬时附件状态；新粘贴路径不会进入这里。
    if attachment_idx < attachment_labels.len() {
        let prefix = format!("{} ", attachment_labels[attachment_idx..].join(" "));
        display.lines[0].insert_str(0, &prefix);
        if display.cursor_row == 0 {
            display.cursor_col += prefix.chars().count();
        }
    }
    display
}

/// 聊天区里那条用户消息的展示文本——**与输入框走同一条投影线**。
///
/// 附件占位符的位置信息只存在于输入框内部的原子标记里（`InputBox::text()`
/// 会把它们过滤掉，事后无从还原），所以这里必须接收**发送瞬间**的输入框
/// 快照。输入框里写的是 `[Image #1]`，消息区就必须是 `[Image #1]`：把
/// 同一个附件在两处渲染成不同样子（输入框 `[Image #1]`、消息区末尾一行
/// `[图片 402×288]`）会让人以为发送丢了图或换了一张图。
///
/// `expand_at_refs_to_attachments` 从正文里的 `@path` 追加的文件没有对应
/// 标记（用户就是敲了一行路径，不是粘贴），它们在末尾单列，避免凭空消失。
///
/// `pastes` 是这次提交时被折叠的长文本块：它们的占位符位置同样只存在于
/// 输入框快照里，必须在这里投影成 `[Pasted text #n +N lines]`——**不是原文**。
/// 发给模型的正文由 `take_composer_text` 展开，两者用途不同，不要混。
pub(crate) fn composer_display_text(
    input: &InputBox,
    attachments: &[Attachment],
    pastes: &[crate::app::PasteBlock],
) -> String {
    let marked = input.attachment_marker_count().min(attachments.len());
    // 只投影有标记的那一段：`input_with_placeholders` 末尾还有一段
    // 「旧状态兼容」兜底，会把没有标记的附件补到行首（输入框需要它来对齐
    // `pending_attachments`，聊天区不需要——它们由下面统一在末尾单列，
    // 两边都画一遍就是同一个附件出现两次）。
    let mut display = input_with_placeholders(input, &attachments[..marked], pastes).text();

    // 已被占位符投影的图片数，作为末尾追加时的编号起点。
    let mut image_number = attachments
        .iter()
        .take(marked)
        .filter(|att| matches!(att, Attachment::Image { .. }))
        .count();
    for att in &attachments[marked..] {
        match att {
            Attachment::Image { preview_label, .. } => {
                image_number += 1;
                display.push_str(&format!("\n[Image #{image_number} {preview_label}]"));
            }
            Attachment::File { path } => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                display.push_str(&format!("\n[File: {name}]"));
            }
        }
    }
    display
}

#[cfg(test)]
mod attachment_input_tests {
    use super::*;

    #[test]
    fn image_placeholder_is_rendered_at_the_paste_position() {
        let mut input = InputBox::new();
        input.insert_text("describe this");
        input.insert_attachment_marker();
        let attachments = vec![Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded".to_string(),
            preview_label: "320×180".to_string(),
        }];

        let display = input_with_placeholders(&input, &attachments, &[]);

        assert_eq!(display.display_lines(), &["describe this [Image #1] "]);
        assert_eq!(
            display.cursor_col,
            "describe this [Image #1] ".chars().count()
        );
        assert_eq!(input.text(), "describe this");
    }

    /// 用户报的核心症状：输入框里是 `[Image #1]`（还带粘贴位置），发出去
    /// 消息区却变成末尾一行 `[图片 402×288]`——同一个附件两套长相，会让人
    /// 以为发送丢了图或换了一张。这条钉死"两处必须是同一段文本"。
    #[test]
    fn chat_display_is_byte_identical_to_the_input_box() {
        let mut input = InputBox::new();
        input.insert_attachment_marker();
        input.insert_text("请按图更新自选股");
        let attachments = vec![Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded".to_string(),
            preview_label: "402×288".to_string(),
        }];

        let in_input_box = input_with_placeholders(&input, &attachments, &[])
            .display_lines()
            .join("\n");
        let in_chat = composer_display_text(&input, &attachments, &[]);

        assert_eq!(in_chat, in_input_box);
        assert_eq!(in_chat, "[Image #1] 请按图更新自选股");
    }

    /// 折叠的粘贴块走同一条投影线：输入框里是 `[Pasted text #1 +N lines]`，
    /// 消息区就必须是同一段。发出去时展开成原文的是**模型那份**，展示给用户的
    /// 聊天记录仍然只画占位符——否则一条消息能在聊天区里糊满几百行，折叠就白做了。
    #[test]
    fn pasted_text_placeholder_is_identical_in_the_input_box_and_the_chat() {
        let mut input = InputBox::new();
        input.insert_text("看下这段");
        input.insert_paste_marker();
        input.insert_text("有问题吗");
        let pastes = vec![crate::app::PasteBlock {
            text: (0..120)
                .map(|i| format!("行{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            line_count: 120,
        }];

        let in_input_box = input_with_placeholders(&input, &[], &pastes)
            .display_lines()
            .join("\n");
        let in_chat = composer_display_text(&input, &[], &pastes);

        assert_eq!(in_chat, in_input_box);
        assert_eq!(in_chat, "看下这段 [Pasted text #1 +120 lines] 有问题吗");
    }

    /// 折叠的粘贴块在输入框里必须**只占固定的一行**，与内容长度无关。
    /// 输入框高度被 `draw` 硬 clamp 到 10 行（内区只剩 8 行），`draw_input` 的
    /// `Paragraph` 又没有滚动、永远从第 0 行起画——占位符的宽度一旦随内容增长，
    /// 后续内容就又掉进那个不可见的区域，「粘贴完打不了字」的老症状原样复现。
    ///
    /// 窄终端下 `[Pasted text #1 +N lines]` 自己折成两行是正常的（3~10 行的
    /// 输入框放得下），所以这里断言的是**不变量**：贴 3 行和贴 30000 行，
    /// 占位符占的行数必须一模一样。
    #[test]
    fn a_pasted_text_placeholder_does_not_grow_with_the_content() {
        let measure = |line_count: usize| {
            let mut input = InputBox::new();
            input.insert_text("看下这段");
            input.insert_paste_marker();
            input.insert_text("有问题吗");
            let pastes = vec![crate::app::PasteBlock {
                text: (0..line_count)
                    .map(|i| format!("行{i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                line_count,
            }];
            input_with_placeholders(&input, &[], &pastes).visual_height(40)
        };

        assert_eq!(
            measure(3),
            measure(30_000),
            "占位符的显示行数不能随粘贴内容长度变化"
        );
        assert_eq!(
            measure(3),
            2,
            "40 列宽下 `[Pasted text #1 +3 lines]` 加上下文折成两行"
        );
    }

    /// 正文里的 `@path` 追加出来的附件没有输入框标记（用户敲的是路径文本，
    /// 不是粘贴），不能在末尾被漏掉——单列一行，且用同一套 `[File: …]` 文案。
    #[test]
    fn at_referenced_files_without_a_marker_are_listed_at_the_end() {
        let mut input = InputBox::new();
        input.insert_text("看下 @notes.txt");
        let attachments = vec![Attachment::File {
            path: std::path::PathBuf::from("/tmp/wyj/notes.txt"),
        }];

        assert_eq!(
            composer_display_text(&input, &attachments, &[]),
            "看下 @notes.txt\n[File: notes.txt]"
        );
    }

    #[test]
    fn text_typed_after_an_image_stays_after_its_placeholder() {
        let mut input = InputBox::new();
        input.insert_text("请看");
        input.insert_attachment_marker();
        input.insert_text("这张图");
        let attachments = vec![Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded".to_string(),
            preview_label: "320×180".to_string(),
        }];

        let display = input_with_placeholders(&input, &attachments, &[]);

        assert_eq!(display.display_lines(), &["请看 [Image #1] 这张图"]);
        assert_eq!(display.cursor_col, "请看 [Image #1] 这张图".chars().count());
        assert_eq!(input.text(), "请看这张图");
    }

    #[test]
    fn images_pasted_at_different_cursor_positions_render_in_visual_order() {
        let mut input = InputBox::new();
        input.insert_text("右侧");
        input.insert_attachment_marker();
        input.move_to_start_of_line();
        input.insert_attachment_marker();
        let attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "left".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "right".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        let display = input_with_placeholders(&input, &attachments, &[]);

        assert_eq!(display.display_lines(), &["[Image #1] 右侧 [Image #2] "]);
    }

    #[test]
    fn multiple_images_have_compact_individual_placeholders() {
        let mut input = InputBox::new();
        input.insert_attachment_marker();
        input.insert_attachment_marker();
        let attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "first".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "second".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        let display = input_with_placeholders(&input, &attachments, &[]);

        assert_eq!(display.display_lines(), &["[Image #1] [Image #2] "]);
        assert_eq!(display.cursor_col, "[Image #1] [Image #2] ".chars().count());
    }

    #[test]
    fn image_numbers_ignore_file_attachments() {
        let mut input = InputBox::new();
        input.insert_attachment_marker();
        input.insert_attachment_marker();
        input.insert_attachment_marker();
        let attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "first".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::File {
                path: std::path::PathBuf::from("notes.txt"),
            },
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "second".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        let display = input_with_placeholders(&input, &attachments, &[]);

        assert_eq!(
            display.display_lines(),
            &["[Image #1] [File: notes.txt] [Image #2] "]
        );
    }
}

/// 字符显示宽度：CJK 全角 = 2，其余 = 1
///
/// **零宽字符一律交给 unicode-width 判定**（返回 0）。手写区间表只覆盖"可见字符"，
/// 零宽空格 U+200B、组合记号、变体选择符、ZWJ 全都落到 `else` 分支被算成 1 列，
/// 而终端**根本不占列**——于是含这类字符的行按 1 列折行、实际渲染 0 列，
/// 右边线就比框宽出 N 列。这类字符在粘贴的代码里并不罕见（零宽空格是很多格式化
/// 工具的产物），所以这不是理论问题。
///
/// 可见字符仍走手写表：它对"终端实际显示什么"的判断比 unicode-width 更贴近
/// 经验值（例如某些符号），贸然整体换掉会牵动全应用的折行结果。
pub(crate) fn char_display_width(c: char) -> usize {
    if c == '\t' {
        return 4;
    }
    if unicode_width::UnicodeWidthChar::width(c) == Some(0) {
        return 0;
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

/// 取第 `idx` 帧动画字符的 `String`，供需要把它塞进 i18n 占位符的调用方使用
/// （`tr_fmt` 的实参是 `&str`，`char` 需要先变成有所有权的字符串）。
pub(crate) fn spinner_frame_str(idx: usize) -> String {
    SPINNER_FRAMES[idx % SPINNER_FRAMES.len()].to_string()
}

/// `!` bash 输出的**正文**渲染（不含标题行）：逐行折行到 `width`，首行带
/// `⎿` 前缀、续行对齐空出同宽缩进。
///
/// 空输出回落到 `(no output)`——与旧的三行预览路径一致，让「跑成功了但什么
/// 都没打印」和「正文被清空」在屏幕上可区分。
fn render_bash_output_body(
    content: &str,
    width: usize,
    style: ratatui::style::Style,
) -> Vec<Line<'static>> {
    let mut body = Vec::new();
    // 判据用 `content.trim()` 而不是「`body` 是否为空」：只含空白的输出经
    // `lines()` 会得到若干空行，`body` 非空却毫无信息量。与旧的预览路径同义。
    if content.trim().is_empty() {
        body.push(Line::from(Span::styled("    ⎿ (no output)", style)));
        return body;
    }
    let mut first = true;
    for raw in content.lines() {
        if raw.trim().is_empty() {
            // 与工具结果展开态同一条纪律：空行必须是真正的空 Line，
            // 否则 `trim_trailing_blank_lines` 弹不掉，每条 `!` 输出尾部
            // 都会留一行拖尾空格。
            body.push(Line::from(""));
            continue;
        }
        for wrapped in wrap_line(raw, width) {
            let prefix = if first { "    ⎿ " } else { "       " };
            body.push(Line::from(Span::styled(
                format!("{prefix}{wrapped}"),
                style,
            )));
            first = false;
        }
    }
    body
}

pub fn draw(f: &mut Frame, state: &mut AppState, input: &InputBox) {
    let area = f.area();
    let inner_width = area.width.saturating_sub(2) as usize; // -2 for borders
    let display_attachments = if state.input_owner.is_none() {
        state.pending_attachments.as_slice()
    } else {
        &[]
    };
    let display_input = input_with_placeholders(input, display_attachments, &state.pending_pastes);
    let paste_hint_position = state.paste_hint.as_ref().map(|hint| {
        let mut hint_input = input.clone();
        hint_input.cursor_row = hint
            .cursor_row
            .min(hint_input.lines.len().saturating_sub(1));
        hint_input.cursor_col = hint
            .cursor_col
            .min(hint_input.lines[hint_input.cursor_row].chars().count());
        let projected =
            input_with_placeholders(&hint_input, display_attachments, &state.pending_pastes);
        (projected.cursor_row, projected.cursor_col)
    });
    let input_height = (display_input.visual_height(inner_width) as u16 + 2).clamp(3, 10);

    // 补全列表高度（@ 文件选取器优先于 slash 补全）
    let completion_height = if !state.file_completions.is_empty() {
        (state.file_completions.len() as u16 + 2).min(10)
    } else if !state.slash_completions.is_empty() {
        (state.slash_completions.len() as u16 + 2).min(8)
    } else {
        0u16
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
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    // 防御性清零：只有内容区真正画出子 Agent 执行内容（`draw_sub_agent_content`）
    // 时才会重新写入准确值，避免本帧未展示（面板未聚焦/无选中）时残留上一帧的旧值。
    state.sub_agent_detail_max_scroll = 0;
    state.shell_detail_max_scroll = 0;
    // 选中子 Agent 后内容区切换为它的执行内容；↑↓ 在运行中的子 Agent 之间切换，
    // Esc / ↑ 越界回到输入框时恢复聊天流。
    if state.ui_focus == UiFocus::SubAgents && state.selected_sub_agent.is_some() {
        draw_sub_agent_content(f, state, chunks[0]);
    } else {
        draw_chat(f, state, chunks[0]);
    }
    match panel_kind {
        BottomPanel::None => {}
        BottomPanel::Permission => {
            if let Some(dlg) = &state.permission_dialog {
                draw_permission_dialog(f, dlg, chunks[1]);
            }
        }
        BottomPanel::SessionConflict => {
            if let Some(dlg) = &state.session_conflict {
                draw_session_conflict_panel(f, dlg, chunks[1]);
            }
        }
        BottomPanel::ProjectTrust => {
            if let Some(servers) = &state.pending_mcp_trust {
                draw_project_trust_panel(f, servers, chunks[1]);
            }
        }
        BottomPanel::SkillTrust => {
            if let Some(infos) = &state.pending_skill_trust {
                draw_skill_trust_panel(f, infos, chunks[1]);
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
        BottomPanel::ShellProcesses => {
            draw_shell_panel(f, state, chunks[1]);
        }
    }
    if !state.file_completions.is_empty() {
        draw_file_completions(f, state, chunks[2]);
    } else if !state.slash_completions.is_empty() {
        draw_slash_completions(f, state, chunks[2]);
    }
    draw_input(f, state, &display_input, chunks[3], paste_hint_position);
    draw_status(f, state, chunks[4]);

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

    if let Some(dialog) = &state.evolution_dialog {
        draw_evolution_dialog(f, dialog, area);
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

    // 定时任务面板叠加在最顶层
    if let Some(dialog) = &state.schedule_dialog {
        draw_schedule_dialog(f, dialog, state.input_owner, area);
    }

    // 所有浮层都完成后再扫描最终 Buffer，避免被面板覆盖的聊天链接仍落在坐标表中。
    // backend 会在 Buffer diff 之后注入 OSC 8，不影响这里的宽度与换行计算。
    crate::hyperlink::linkify_buffer(
        f.buffer_mut(),
        chunks[0],
        &state.cwd,
        &state.hyperlink_registry,
    );
}

/// 底部面板类型与高度
enum BottomPanel {
    None,
    Permission,
    SessionConflict,
    ProjectTrust,
    SkillTrust,
    ExecModeConfirm,
    PlanApproval,
    SubAgents,
    ShellProcesses,
}

/// 会话占用裁决框的固定高度：2 行边框 + 2 行说明 + 3 行选项 + 1 行操作提示。
const SESSION_CONFLICT_PANEL_H: u16 = 8;

/// agents 面板列表区最多同时展示的行数（超出用滚动窗口，不再随数量线性增长）
const SUB_AGENT_LIST_MAX: usize = 6;
/// 后台任务面板列表区最多同时展示的行数
const SHELL_LIST_MAX: usize = 6;
/// 后台任务面板详情区（实时输出）最多占用的行数（超出内部滚动）
const SHELL_DETAIL_MAX: u16 = 12;
/// 面板详情区读多少字节的输出尾部（与 `SHELL_WAKE_TAIL_BYTES` 同量级，
/// 保证面板看到的内容与唤醒时喂给模型的内容一致）
const SHELL_TAIL_BYTES: usize = 64 * 1024;

/// 后台任务面板高度（列表行 + 边框 + 可选详情区）。
/// sizing 阶段没有宽度信息，详情区行数只能按 `SHELL_DETAIL_MAX` 粗估；
/// 精确的可视行数在 `draw_shell_panel` 渲染时用 `Paragraph::line_count`
/// 重算并 clamp 写回。
fn shell_panel_height(state: &AppState, area_height: u16) -> u16 {
    let list_rows = state.running_shells().len().min(SHELL_LIST_MAX) as u16;
    let detail_rows = if state.shell_detail_open {
        SHELL_DETAIL_MAX
    } else {
        0
    };
    let content_rows = list_rows + detail_rows;
    let max_h = (area_height * 7 / 10).max(list_rows + 2);
    (content_rows + 2)
        .clamp(list_rows + 2, max_h)
        .min(area_height)
}

fn bottom_panel_size(state: &AppState, area_height: u16) -> (u16, BottomPanel) {
    // 权限确认最优先：几乎每次 Edit/Write/Bash 调用都可能弹出，必须始终可立即
    // 响应，不能被其他面板挡住；固定 11 行，完全放得进底部常驻区（对齐
    // Claude Code Inline 模式——不再走全屏浮层，避免每次都触发全屏切换闪烁）。
    if state.permission_dialog.is_some() {
        return (11u16.min(area_height), BottomPanel::Permission);
    }
    // 会话占用裁决排在权限框之后：权限框只可能在一轮工具调用中途出现，而占用框
    // 只在启动横幅那一瞬间存在，两者实际不会同框。万一真的重叠（`/resume` 撞上
    // 中途弹出的权限框），权限优先是既有行为，不为这个特例去改它。
    if state.session_conflict.is_some() {
        return (
            SESSION_CONFLICT_PANEL_H.min(area_height),
            BottomPanel::SessionConflict,
        );
    }
    // 项目级 MCP server 信任确认：只在启动时出现一次，优先级次于逐调用权限
    // 确认（后者可能在一轮工具调用中途弹出，必须绝对优先响应），但高于其余
    // 面板——这是"要不要允许仓库自带的任意命令执行"的安全门槛，不应被
    // ExecModeConfirm/PlanApproval 这类流程性面板挡住。
    if state.pending_mcp_trust.is_some() {
        return (11u16.min(area_height), BottomPanel::ProjectTrust);
    }
    // 项目级 skill 信任确认：与 MCP 同类的安全门槛，排在 MCP 之后（后者能执行
    // 任意命令、级别更高），同样压过 ExecModeConfirm/PlanApproval 这类流程性面板。
    if let Some(infos) = &state.pending_skill_trust {
        // 高度必须覆盖 intro 的折行（窄屏下 2-3 行）+ 条目 + 空行 + hint。
        // intro 长度随语言不同、还会随宽度折行，这里给足余量而不是精算——
        // 宁可面板略高，也不要把 `[y] 信任并启用` 这行操作提示裁掉
        // （裁掉的话用户根本不知道该怎么应答）。
        let rows = (infos.len() as u16).min(6) + 9;
        return (rows.min(area_height), BottomPanel::SkillTrust);
    }
    if state.exec_mode_confirm.is_some() {
        return (4u16.min(area_height), BottomPanel::ExecModeConfirm);
    }
    if state.plan_dialog.is_some() {
        // 计划正文已作为普通消息并入应用内聊天流，
        // 这里只剩三选一选择器，贴在输入框上方，宽度对齐 Permission。
        //
        // 手动输入框是多行的（Shift+Enter / Ctrl+J / 行尾 `\`+Enter 都能换行，
        // 见 `apply_edit_key`），高度必须跟着实际行数走：此前写死 5 行、渲染
        // 又只画 `lines.first()`，用户写三行只看见一行，回车却把三行一起发
        // 出去——所见非所发。同批改动已把 AskQuestion 的自由输入改成逐行渲染，
        // 这里是对齐遗漏的那个兄弟面板。
        let freetext_rows = state
            .plan_dialog
            .as_ref()
            .and_then(|dlg| dlg.freetext_input().map(|input| input.lines.len()))
            .unwrap_or(0) as u16;
        let rows = if freetext_rows > 0 {
            3 + freetext_rows
        } else {
            5
        };
        return (rows.min(area_height), BottomPanel::PlanApproval);
    }
    // 后台任务面板：与子 Agent 面板互斥（底部只有一个位置），判定规则是
    // **「用户焦点所在面板优先，其次才看有没有运行中任务」**。
    //
    // 为什么不能简单按"有运行中"排先后：子 Agent 和后台 shell 经常同时跑
    // （典型场景就是「派子 Agent 去改代码，同时自己起 dev server」），
    // 若固定让子 Agent 面板占位，后台任务就永远看不见、也按不了 k。
    // 焦点优先保证用户想看哪个就能通过 Shift+↑ + ↓ 导航切过去。
    if state.ui_focus == UiFocus::Shells && state.has_running_shells() {
        return (
            shell_panel_height(state, area_height),
            BottomPanel::ShellProcesses,
        );
    }
    if state.ui_focus == UiFocus::SubAgents && state.has_running_sub_agents() {
        // 焦点在子 Agent 面板时保持可见；列表只含运行中的条目
        // （`visible_sub_agents`），全部完成后由 `reconcile_sub_agent_selection`
        // 把焦点交还输入框，这里自然收起。
        let list_rows = state.visible_sub_agents().len().min(SUB_AGENT_LIST_MAX) as u16;
        let max_h = (area_height * 7 / 10).max(list_rows + 2);
        return (
            (list_rows + 2).clamp(list_rows + 2, max_h).min(area_height),
            BottomPanel::SubAgents,
        );
    }
    if state.has_running_shells() {
        return (
            shell_panel_height(state, area_height),
            BottomPanel::ShellProcesses,
        );
    }

    // 子 Agent 聚合面板：运行期间自动显示，全部结束后自动收起。选中条目的执行
    // 内容由内容区 `draw_sub_agent_content` 接管，面板本身只负责列表与选中高亮。
    let visible = state.visible_sub_agents();
    let show_sub_agents = state.has_running_sub_agents();
    if show_sub_agents && !visible.is_empty() {
        let list_rows = visible.len().min(SUB_AGENT_LIST_MAX) as u16;
        let max_h = (area_height * 7 / 10).max(list_rows + 2);
        let h = (list_rows + 2).clamp(list_rows + 2, max_h);
        return (h.min(area_height), BottomPanel::SubAgents);
    }
    (0, BottomPanel::None)
}

// ─── 对话区 ──────────────────────────────────────────────────────────────────

/// 渲染子 Agent 的内部工具调用明细行（⏺ 工具名(参数) ✓/✗ 耗时）。
/// 只被 agents 面板详情区调用（与聊天流的 ToolResult 渲染是两条独立路径）。
fn push_sub_agent_tool_log(
    lines: &mut Vec<Line<'static>>,
    tool_log: &[SubToolLine],
    max_content_width: usize,
) {
    for tl in tool_log {
        let (mark, mark_style) = match (tl.elapsed_secs, tl.is_error) {
            (None, _) => ("…".to_string(), Theme::dim()),
            (Some(e), true) => (format!("✗ {}", format_hms(e)), Theme::error()),
            (Some(e), false) => (format!("✓ {}", format_hms(e)), Theme::success()),
        };
        let display_name = crate::app::tool_display_name(&tl.tool_name);
        let call = if tl.arg_summary.is_empty() {
            display_name
        } else {
            format!("{display_name}({})", tl.arg_summary)
        };
        // 与主聊天流同一口径：Grep 行尾标出这次跑的是 rg 还是内置 Rust 实现
        let call = format!("{call}{}", crate::app::tool_backend_badge(&tl.tool_name));
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
    sub_agents: &'a std::collections::BTreeMap<u64, SubAgentUiState>,
    spinner_frame: usize,
    /// `Ctrl+O` 手动强制展开**所有**可折叠正文：thinking 块与工具调用结果。
    ///
    /// `false` 不是「全折叠」而是**自动模式**——正在流式输出的 thinking 与正在
    /// 执行的工具实时展开，跑完立即收起，因此最终判定是 `output_expanded || is_live`。
    /// 折叠态一律不得触碰 `content`（见 `render_thinking_block` 的 `live` 分支）。
    output_expanded: bool,
    /// 正在执行中的工具调用的实时输出：`sequence_no` → 累计输出的尾部快照。
    ///
    /// 由 `build_pending_chat_lines` 从 `AppState.tool_info` ×
    /// `AppState.live_tool_out` 求交集后按 `sequence_no` 重建，因此天然只含
    /// 仍在飞的调用；回合被中断后遗留的孤儿快照进不来。
    live_tool_out: &'a std::collections::HashMap<usize, String>,
}

/// 执行中实时输出块显示的尾部行数。
///
/// 定这个值是终端高度的直觉：够看清一段命令在干什么，又不至于把屏幕刷满。
/// 完整正文始终保留在工具结果里，跑完后自动收起，需要回看再按 Ctrl+O。
const LIVE_TAIL_LINES: usize = 15;

/// 取正文尾部最多 [`LIVE_TAIL_LINES`] 行，并返回被截掉的行数。
///
/// 工具实时输出与流式 thinking 共用这一个取尾逻辑（两者都只关心「最新的一段
/// 正在发生什么」）。用 `rsplit` 是因为它是反向惰性迭代器：取 N 行只解码 N 行，
/// 正文再长也不必全文折行——thinking 缓冲能长到几十 KB，每帧全量重排正是当初
/// 「整屏换行比机器慢」的成因。
fn live_tail(body: &str) -> (Vec<&str>, usize) {
    let total = body.bytes().filter(|b| *b == b'\n').count() + 1;
    let skipped = total.saturating_sub(LIVE_TAIL_LINES);
    let mut lines: Vec<&str> = body
        .rsplit('\n')
        .take(LIVE_TAIL_LINES)
        .map(str::trim_end)
        .collect();
    lines.reverse();
    (lines, skipped)
}

/// 被 `live_tail` 切掉的那段前缀里，是否留下了**未闭合**的围栏。
///
/// 只扫被丢掉的前缀，代价与尾部窗口无关。判据与 `markdown::split_unclosed_fence`
/// 同源：按行数围栏开合，最后仍处于开启状态即视为未闭合。命中时流式尾部要补
/// 一个开启行——否则尾部里的那个闭合围栏会被 markdown 当成新的开启围栏，
/// 把它后面的所有正常思考都吞进代码块。
fn fence_open_before(body: &str, skipped: usize) -> bool {
    if skipped == 0 {
        return false;
    }
    let mut open: Option<char> = None;
    for line in body.lines().take(skipped) {
        let trimmed = line.trim_start();
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let Some(marker) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') else {
            continue;
        };
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if run < 3 {
            continue;
        }
        open = match open {
            Some(current) if current == marker => None,
            Some(current) => Some(current),
            None => Some(marker),
        };
    }
    open.is_some()
}

/// 渲染「正在执行」的工具实时输出块，挂在对应 ⏺ 行下方。
///
/// 这是**唯一**允许在折叠态触碰工具输出的地方，理由与风险：内容按定义处于
/// 展开态，且只取尾部 [`LIVE_TAIL_LINES`] 行折行——不做全文重排，因此不受
/// `collapsed_tool_result_never_touches_content` 那条性能不变式的约束（那条
/// 钉的是 100KB 正文的 Bash 结果在折叠态零成本）。
fn render_live_tool_output(lines: &mut Vec<Line<'static>>, tail: &str, ctx: &ChatRenderCtx) {
    let width = ctx.max_content_width.saturating_sub(7).max(1);
    let body = tail.trim_end();
    if body.is_empty() {
        return;
    }
    let (tail_lines, skipped) = live_tail(body);

    let frame = SPINNER_FRAMES[ctx.spinner_frame % SPINNER_FRAMES.len()];
    let mut first = true;
    for raw in tail_lines {
        if raw.trim().is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for wrapped in wrap_line(raw, width) {
            let (prefix, style) = if first {
                ("    ⎿ ", Theme::tool_result())
            } else {
                ("       ", Theme::dim())
            };
            let text = if first {
                format!("{prefix}{frame} {wrapped}")
            } else {
                format!("{prefix}{wrapped}")
            };
            lines.push(Line::from(Span::styled(text, style)));
            first = false;
        }
    }
    if skipped > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "       {}",
                wyj_i18n::tr_fmt("live_output.omitted", &[("count", &skipped.to_string())])
            ),
            Theme::dim(),
        )));
    }
}

/// thinking 块的渲染方式。流式（`AppState.thinking_buf`）与定稿
/// （`ChatMessage::Thinking`）两条数据路径共用同一个渲染入口。
struct ThinkingRenderOpts<'a> {
    /// 仍在流式累积：只画一行状态，**完全不碰 `content`**。
    live: bool,
    /// 定稿后是否展开正文。
    expanded: bool,
    /// 该段 thinking 的累计耗时。`None` = 本次会话没有记录到
    /// （`/resume` 恢复的旧会话：耗时从未持久化过）。
    elapsed_secs: Option<f64>,
    /// 实时 spinner 帧序号（`live` 时注入 `thinking.running` 的 `{frame}`）。
    spinner_frame: usize,
    /// 展开态正文的按宽度缓存（复用 `ChatMessage::render_cache`）。
    ///
    /// thinking 展开后走完整 markdown 解析，长思考几十行、历史里又有十几段，
    /// 每帧重解析一次就是与 assistant 消息同一类的浪费；而它一旦定稿正文
    /// 不再变化，正是缓存的最佳对象。`None` = 不缓存（测试与一次性渲染）。
    cache: Option<&'a crate::app::RenderCache>,
}

fn clean_thinking_lines(content: &str) -> Vec<&str> {
    content
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// thinking 块的两个态（`ThinkingRenderOpts.live`）：
///
/// - **流式中**：一行状态 + 尾部 [`LIVE_TAIL_LINES`] 行思考正文，跑完即消失。
/// - **定稿后**：折叠成一行元信息（耗时 + 行数 + 展开提示），Ctrl+O 展开全文。
///
/// 旧实现是"固定显示前 3 行 + ` · N lines` 标记"，有三个问题：`✻`(U+273B,
/// Dingbats) 在终端字体里常缺字形画成豆腐块；`N lines` 数的是去掉空行后的
/// **原始**行数，与屏幕上折行后再截断到 3 行的**视觉**行数毫无对应；只取头
/// 3 行意味着几十行思考的结论永远看不到（thinking 是流式追加的，最新结论在
/// 尾部）。更糟的是流式期间每帧都要把整个 buf 重新折行一遍、再只取前 3 行
/// 扔掉，纯属浪费——现在 `live` 分支只 `rsplit` 取尾部若干行，两端都不扫全文。
fn render_thinking_block(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    max_content_width: usize,
    opts: ThinkingRenderOpts<'_>,
) {
    if opts.live {
        // `{frame}` 由渲染层注入当前 spinner 帧：i18n 文案里写死 `⠋` 会让这行
        // 在流式期间永远停在第一帧，用户往回滚看历史时整屏只有标题栏在动。
        let frame = spinner_frame_str(opts.spinner_frame);
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr_fmt("thinking.running", &[("frame", frame.as_str())]),
            Theme::info(),
        )));
        // 流式期间把尾部思考显出来，让「正在想什么」可见；定稿后立刻收起。
        // 取尾逻辑与工具实时输出块共用 `live_tail`。
        let body = content.trim_end();
        if body.is_empty() {
            return;
        }
        let (tail_lines, skipped) = live_tail(body);
        let width = max_content_width.saturating_sub(4).max(1);
        // 尾部同样是 markdown：流式期间模型已经在写列表 / 围栏了，逐行原样
        // 输出会让这几行在半秒内先"乱"一遍再变整齐，比不格式化更刺眼。
        // 输入是尾部 30 行，解析成本与整段思考长度无关。
        let mut tail_text = tail_lines.join("\n");
        // 取尾可能把围栏的**开启行**切掉，只剩闭合行。markdown 会把它当成
        // 新的开启行，把后面所有正常思考都吃进代码块。补一个开启行把它配平。
        if fence_open_before(body, skipped) {
            tail_text.insert_str(0, "```\n");
        }
        let mut rendered: Vec<Line<'static>> = vec![];
        render_streaming_thinking_markdown(&mut rendered, &tail_text, width);
        for line in &mut rendered {
            if line.spans.is_empty() {
                continue;
            }
            let mut spans = vec![Span::raw("    ")];
            spans.append(&mut line.spans);
            *line = Line::from(spans);
        }
        lines.append(&mut rendered);
        if skipped > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    wyj_i18n::tr_fmt("thinking.live_omitted", &[("count", &skipped.to_string())])
                ),
                Theme::dim(),
            )));
        }
        return;
    }

    let clean_lines = clean_thinking_lines(content);
    let total = clean_lines.len().to_string();
    let header = match opts.elapsed_secs {
        Some(secs) => wyj_i18n::tr_fmt(
            "thinking.done",
            &[("secs", &format!("{secs:.1}")), ("lines", &total)],
        ),
        // 无耗时（`/resume` 的旧会话）只报行数，不显示一个假的 "0.0s"
        None => wyj_i18n::tr_fmt("thinking.done_no_elapsed", &[("lines", &total)]),
    };
    lines.push(Line::from(vec![
        Span::styled(header, Theme::info()),
        Span::styled(
            if opts.expanded {
                String::new()
            } else {
                wyj_i18n::tr("thinking.expand_hint")
            },
            Theme::dim(),
        ),
    ]));

    if !opts.expanded {
        return;
    }
    // 展开即全文：用户主动行为，不再截断；超出视口由既有的 chat_scroll 滚动。
    //
    // 正文走 markdown 而不是逐行纯文本：真实 thinking 里约 15% 的行带 `- ` /
    // `1.` / `|` / `**` / 围栏，原样打出来就是一屏"乱码"。行数统计仍用
    // `clean_lines`（去空行的原始行数），那是元信息，与这里的渲染无关。
    let width = max_content_width.saturating_sub(4).max(1);
    let mut rendered = match opts.cache {
        Some(cache) => {
            let mut cache = cache.borrow_mut();
            match cache.as_ref() {
                Some((w, cached)) if *w == width => cached.clone(),
                _ => {
                    let mut fresh: Vec<Line<'static>> = vec![];
                    render_thinking_markdown(&mut fresh, content, width);
                    *cache = Some((width, fresh));
                    cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
                }
            }
        }
        None => {
            let mut fresh: Vec<Line<'static>> = vec![];
            render_thinking_markdown(&mut fresh, content, width);
            fresh
        }
    };
    // 统一缩进 4 格：thinking 是挂在 `⎿` 行下方的旁白，顶格会让它与正文
    // 平起平坐；而 markdown 自己已经处理了块内层级（bullet / 引用 / 代码），
    // 这里只加一层外层缩进，不参与块内排版。
    for line in &mut rendered {
        if line.spans.is_empty() {
            continue;
        }
        let mut spans = vec![Span::raw("    ")];
        spans.append(&mut line.spans);
        *line = Line::from(spans);
    }
    lines.append(&mut rendered);
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
            let first_style = Theme::highlight();
            for (idx, wrapped) in wrap_line(first_line, max_content_width.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
            {
                let prefix = if idx == 0 { "❯ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Theme::user_prefix()),
                    Span::styled(wrapped, first_style),
                ]));
            }
            for l in content_lines {
                for wrapped in wrap_line(l, max_content_width.saturating_sub(2).max(1)) {
                    lines.push(Line::from(Span::styled(
                        format!("  {wrapped}"),
                        Theme::body(),
                    )));
                }
            }
        }

        MessageRole::Assistant => {
            if msg.is_error {
                for l in msg.content.lines() {
                    for (idx, wrapped) in wrap_line(l, max_content_width.saturating_sub(4).max(1))
                        .into_iter()
                        .enumerate()
                    {
                        let prefix = if idx == 0 { "  ✗ " } else { "    " };
                        lines.push(Line::from(Span::styled(
                            format!("{prefix}{wrapped}"),
                            Theme::error(),
                        )));
                    }
                }
            } else {
                // 已定稿消息的 markdown 渲染结果按宽度缓存：避免每帧对
                // 全部历史重跑 markdown 解析（长对话下的主要渲染开销）
                let mut cache = msg.render_cache.borrow_mut();
                let mut rendered = match cache.as_ref() {
                    Some((w, cached)) if *w == max_content_width => cached.clone(),
                    _ => {
                        let mut fresh: Vec<Line<'static>> = vec![];
                        render_markdown(&mut fresh, &msg.content, max_content_width);
                        *cache = Some((max_content_width, fresh));
                        cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
                    }
                };
                lines.append(&mut rendered);
            }
        }

        MessageRole::Thinking => {
            render_thinking_block(
                lines,
                &msg.content,
                max_content_width,
                ThinkingRenderOpts {
                    live: false,
                    expanded: ctx.output_expanded,
                    elapsed_secs: msg.thinking_elapsed_secs,
                    spinner_frame: ctx.spinner_frame,
                    cache: Some(&msg.render_cache),
                },
            );
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

            // 正在执行中的工具：把实时输出挂在 ⏺ 行下方，完成后由 ⎿ 结果行接手。
            // 没有这一段的话，执行中的工具在聊天区里只有一行标题，用户只能盯着
            // 状态栏的 spinner 猜它在干什么。
            if let Some(tail) = msg.sequence_no.and_then(|seq| ctx.live_tool_out.get(&seq)) {
                render_live_tool_output(lines, tail, ctx);
            }

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
                            Span::styled(format!("{frame} {stats}"), Theme::info()),
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
            render_tool_result_block(lines, None, msg, ctx);
        }

        MessageRole::BashOutput => {
            let (icon, style) = if msg.is_error {
                ("✗", Theme::error())
            } else {
                ("$", Theme::success())
            };
            let elapsed_str = msg
                .elapsed_secs
                .filter(|&s| s > 0.0)
                .map(|s| format!(" · {}", format_hms(s)))
                .unwrap_or_default();
            let total = msg.content.lines().count();
            lines.push(Line::from(vec![
                Span::styled(format!("  {icon} bash"), style),
                Span::styled(elapsed_str, Theme::dim()),
                Span::styled(
                    if total > 0 {
                        format!(" · {total} lines")
                    } else {
                        String::new()
                    },
                    Theme::dim(),
                ),
            ]));
            let body_style = if msg.is_error {
                Theme::error()
            } else {
                Theme::tool_result()
            };
            // 全文渲染，不折叠、不截断：`!` bash 的输出是用户**亲手执行**的那条
            // 命令的原始回显，3 行预览等于「我让你做的事你没让我看到」。
            // 代价（整段输出每帧都要折行）由 `render_cache` 收掉——消息定稿后
            // 内容不再变化，折行结果按宽度缓存一次即可。
            let body_width = max_content_width.saturating_sub(7).max(1);
            let mut cache = msg.render_cache.borrow_mut();
            let mut rendered = match cache.as_ref() {
                Some((w, cached)) if *w == body_width => cached.clone(),
                _ => {
                    let fresh = render_bash_output_body(&msg.content, body_width, body_style);
                    *cache = Some((body_width, fresh));
                    cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
                }
            };
            lines.append(&mut rendered);
        }

        MessageRole::System => {
            let (marker, style) = if msg.is_error {
                ("  ⚠ ", Theme::warning())
            } else {
                ("  ⚙ ", Theme::info())
            };
            lines.push(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(msg.content.clone(), style),
            ]));
        }
        MessageRole::Preformatted => {
            // 逐行原样渲染：换行是排版的一部分，折行会毁掉列对齐。
            // 单行超宽时按显示宽度截断（CJK 安全），而不是折成两行。
            //
            // **不加任何前缀缩进**——内容自己决定版式（`/cost` 的表格已经自带
            // 两格缩进）。渲染层再补一截会让标题行与表格行错开。
            let budget = max_content_width.max(1);
            for raw in msg.content.lines() {
                lines.push(Line::from(Span::styled(
                    truncate_line(raw, budget),
                    Theme::code_body(),
                )));
            }
        }
        MessageRole::TurnSummary => {
            lines.push(Line::from(vec![Span::styled(
                format!("  {}", msg.content),
                Theme::dim(),
            )]));
        }

        // ─── 📋 计划正文  ────────────────────────────────────────────
        // 并入正常消息流；批准/继续规划/手动输入
        // 的交互留在贴底的 draw_plan_approval_selector。
        MessageRole::PlanProposal => {
            let divider = "─".repeat(max_content_width.saturating_sub(2));
            lines.push(Line::from(Span::styled(
                "  📋 计划",
                Style::default()
                    .fg(Theme::plan_color())
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {divider}"),
                Theme::plan(),
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
                Theme::plan(),
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
    let raw = msg.content.lines().collect::<Vec<_>>();
    let is_diff = is_diff_result(msg);
    let content = if is_diff {
        raw.iter()
            .position(|line| {
                // `@@` 是 diff 正文的**第一行**（每个变更块的头），必须一并认作
                // diff 特征行，否则定位会落在第一个 `  ` 上下文行上、把块头
                // 当成回执前缀留在外面——而行号正是从块头推出来的。
                line.starts_with("@@")
                    || line.starts_with("+ ")
                    || line.starts_with("- ")
                    || line.starts_with("  ")
                    || line.starts_with("...(truncated")
                    || line.starts_with("…(truncated")
            })
            .map(|idx| raw[idx..].to_vec())
            .unwrap_or(raw)
    } else {
        raw
    };
    let is_read = msg
        .tool_name
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case("Read"));
    content
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

/// `bash.rs` 在非零退出时会把 `退出码 N` 拼在输出最前面。这行是**给模型看的**——它让模型
/// 无需解析 shell 语义就知道退出状态。对人类读者它是纯冗余：标题行早已用红色 ` · failed`
/// 标出失败，真正有用的错误信息（第三方 API 的 JSON 错误体等）却要等读者越过这一行。
/// 渲染层识别并跳过它，让 `⎿` 行直接展示真实错误首行。
fn is_exit_code_banner(line: &str) -> bool {
    line.trim()
        .strip_prefix("退出码 ")
        .is_some_and(|rest| rest.trim().parse::<i64>().is_ok())
}

/// 渲染工具结果的**正文**（不含标题行）。只在展开态被调用——折叠态由
/// `render_tool_result_block` 提前 return，正文一个字符都不渲染。
///
/// 出全文、不做任何行数截断：展开是用户的显式 opt-in 行为，全文才是它的意义。
/// 仍保留少量表达层规范化（剔除「退出码 N」横幅、Read 剥行号、Edit 只显示 diff 段、
/// 错误首行红其余 dim），这些只改样式与前缀、不丢用户关心的内容。
fn render_tool_result_preview(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    ctx: &ChatRenderCtx,
) {
    let max_content_width = ctx.max_content_width;
    let is_diff = is_diff_result(msg);
    let content_lines = tool_result_content_lines(msg);
    let fallback = message_summary(msg);
    // 空行**不再**被当成「浪费名额」丢掉：展开态出全文，空行是 AskQuestion /
    // 多段 bash 输出的段落分隔，删掉会把结构压平。这里只判定「是否整段皆空」以决定
    // 是否回落到摘要（全部内容都是空行时渲染成空白毫无意义）。
    let has_visible = content_lines.iter().any(|line| !line.trim().is_empty());
    let mut full_content = content_lines;
    // 错误场景：丢掉工具自拼的 `退出码 N` 横幅，把首行让给真实错误信息。
    if msg.is_error && full_content.first().is_some_and(|l| is_exit_code_banner(l)) {
        full_content.remove(0);
    }
    let source = if !has_visible {
        vec![fallback.as_str()]
    } else {
        full_content
    };
    // diff 走独立的解析/渲染路径：行号必须在**逻辑行**层面算完再折行，
    // 逐行折行后再判类型会把一段长代码行的后半截误认成新行。
    if is_diff {
        let (rows, width) = parse_diff_rows(&source);
        lines.extend(render_diff_rows(
            &rows,
            width,
            max_content_width,
            rows.len(),
        ));
        return;
    }
    let mut preview = Vec::new();
    let mut first = true;
    for raw in source {
        let style = if msg.is_error {
            // 只有第一视觉行保持红色作为「失败」锚点，其余降为 dim：一次 curl 参数
            // 错误后 AI 会自行重试，属预期内的可自愈噪音，整块刷红会把它渲染成
            // 程序崩溃级别的告警，真正的 shell 失败反而被淹没在同样的红里。
            // 展开态下这个论证更强——400 行的错误块刷成一片红比 3 行时糟糕得多。
            if first {
                Theme::error()
            } else {
                // 非首行降到次级正文而不是 dim：错误块展开后常有几十行，
                // dim 在暗底上已接近「看不清」，而用户恰恰需要读完它才能
                // 判断这次失败要不要处理。
                Theme::text_dim()
            }
        } else {
            Theme::tool_result()
        };
        // 空行必须渲染成**真正的空 Line**，不能是带缩进 span 的空白行：
        // `trim_trailing_blank_lines` 只弹 `spans.is_empty()` 的行，带 prefix 的
        // 空白行弹不掉，会在每个工具结果尾部留下一行拖尾空格。
        if raw.trim().is_empty() {
            preview.push(Line::from(""));
            continue;
        }
        for wrapped in wrap_line(raw, max_content_width.saturating_sub(7).max(1)) {
            let prefix = if first { "    ⎿ " } else { "       " };
            preview.push(Line::from(Span::styled(
                format!("{prefix}{wrapped}"),
                style,
            )));
            first = false;
        }
    }
    // 展开即全文：不截断、不补 `...`、错误也不做 head+tail。旧的 `⋮` 省略逻辑存在时
    // 曾专门用来保住尾部的 `request_id` / `reason`，展开后尾部天然可见，保留它反而
    // 会主动删掉用户明确要求看到的中间内容。
    lines.extend(preview);
}

fn render_tool_result_block(
    lines: &mut Vec<Line<'static>>,
    call: Option<&ChatMessage>,
    result: &ChatMessage,
    ctx: &ChatRenderCtx,
) {
    let max_content_width = ctx.max_content_width;
    let elapsed_str = result
        .elapsed_secs
        .filter(|&s| s > 0.0)
        .map(|s| format!(" · {}", format_hms(s)))
        .unwrap_or_default();

    if let Some(call) = call {
        lines.push(Line::from(vec![
            Span::styled("  ⏺ ", Theme::tool_call()),
            Span::styled(
                truncate_line(&call.content, max_content_width.saturating_sub(16)),
                Theme::tool_call(),
            ),
            Span::styled(elapsed_str, Theme::dim()),
            if result.is_error {
                Span::styled(" · failed", Theme::error())
            } else {
                Span::raw("")
            },
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  ⎿ result", Theme::dim()),
            Span::styled(elapsed_str, Theme::dim()),
        ]));
    }
    // 折叠态：Edit/Write 是**唯一例外**——就地渲染一段截断的彩色 diff，其余
    // 工具结果正文一个字符都不出。
    //
    // 为什么它是例外：`Edit`/`Write` 的 ToolResult 正文本身就是 diff
    // （`tools/src/edit.rs` 调 `make_diff` 拼进回执），而 diff **才是**这次
    // 调用的结果，回执「已替换 1 处」什么信息量都没有。折叠掉它等于让用户
    // 只能看到「文件被改了」而看不到「改成了什么」，必须先按 Ctrl+O 才知道
    // 这次编辑到底做了什么——对一个以改代码为主业的 CLI 来说这是不可接受的
    // （对齐 Claude Code：Edit/Write 的 diff 内联可见，长 diff 折叠）。
    //
    // 为什么其余工具仍然一个字符都不碰：`tool_result_content_lines` 会把整个
    // `content` `lines().collect()` 成 Vec。旧实现每帧为每条工具结果构建完整
    // preview 再只取 3 行扔掉，是纯粹的浪费——`Bash` 的输出可以有几 MB。
    // diff 例外不破坏这条不变式：Edit/Write 的回执被 `MAX_DIFF_LINES = 200`
    // 在工具层就封了顶，构造开销有硬上界，且下面还只渲染前 N 行。
    if !ctx.output_expanded {
        if is_diff_result(result) {
            render_collapsed_diff_preview(lines, result, ctx);
        }
        return;
    }
    render_tool_result_preview(lines, result, ctx);
}

/// 是否是 Edit/Write 的**成功**回执（失败时正文是错误文本，不是 diff）。
fn is_diff_result(msg: &ChatMessage) -> bool {
    !msg.is_error
        && msg
            .tool_name
            .as_deref()
            .is_some_and(|name| matches!(name, "Edit" | "Write"))
}

/// 折叠态 Edit/Write 的 diff 预览行数上限。超出的部分折成一行
/// 「… 还有 N 行（Ctrl+O 展开全文）」。
///
/// 取 20 是权衡：够看完一次局部编辑的全部改动行，又不至于让新建大文件时
/// 200 行 diff 糊满屏幕（Write 新建文件时整份内容都是 `+` 行，最容易触发）。
const COLLAPSED_DIFF_MAX_LINES: usize = 20;

/// 一行 diff 在渲染前的解析结果。
struct DiffRow<'a> {
    /// `(旧侧行号, 新侧行号)`：删除行只有旧侧、新增行只有新侧、上下文两侧都有。
    /// 缺的那侧渲染成空白，与 `git diff` 的行号列同义。
    numbers: (Option<usize>, Option<usize>),
    text: &'a str,
    role: DiffRowRole,
}

enum DiffRowRole {
    Context,
    Removed,
    Added,
    /// `@@` 块头、`… N unchanged lines` 跳过提示、`…(truncated)` 截断提示。
    /// 这些行不带行号（它们本身就在描述行号），整体 dim。
    Meta,
}

/// 解析 `@@ -248,7 +248,7 @@` → `(248, 248)`。
///
/// 块头是行号的**唯一来源**：第一个变更块之前被丢掉的未改动内容没有留下任何
/// 标记，从 1 开始数会把第 250 行的改动标成第 3 行。
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@")?;
    // 形如 ` -248,7 +248,7 @@`
    let mut parts = rest.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let parse = |s: &str| s.split(',').next()?.parse::<usize>().ok();
    Some((parse(old)?, parse(new)?))
}

/// 把 diff 正文解析成带行号的行序列，同时算出行号列宽。
///
/// 列宽必须扫完全部行才能定——`@@` 头里的起点可能比任何一行实际渲染出的行号
/// 都大（块被截断时），先按最大行号算宽度，才能保证右边对齐。
fn parse_diff_rows<'a>(content: &[&'a str]) -> (Vec<DiffRow<'a>>, usize) {
    let mut rows: Vec<DiffRow> = Vec::with_capacity(content.len());
    let mut old_no = 1usize;
    let mut new_no = 1usize;
    let mut width = 1usize;

    for raw in content {
        if let Some((o, n)) = parse_hunk_header(raw) {
            old_no = o;
            new_no = n;
            rows.push(DiffRow {
                numbers: (None, None),
                text: raw,
                role: DiffRowRole::Meta,
            });
            continue;
        }
        let (marker, rest): (&str, &str) = match raw.split_at_checked(2) {
            Some(("- ", rest)) => ("-", rest),
            Some(("+ ", rest)) => ("+", rest),
            Some(("  ", rest)) => (" ", rest),
            _ => ("", raw),
        };
        let role = match marker {
            "-" => DiffRowRole::Removed,
            "+" => DiffRowRole::Added,
            " " => DiffRowRole::Context,
            _ => DiffRowRole::Meta,
        };
        let numbers = match role {
            DiffRowRole::Removed => (Some(old_no), None),
            DiffRowRole::Added => (None, Some(new_no)),
            DiffRowRole::Context => (Some(old_no), Some(new_no)),
            DiffRowRole::Meta => (None, None),
        };
        for n in [numbers.0, numbers.1].into_iter().flatten() {
            width = width.max(n.to_string().len());
        }
        match role {
            DiffRowRole::Removed => old_no += 1,
            DiffRowRole::Added => new_no += 1,
            DiffRowRole::Context => {
                old_no += 1;
                new_no += 1;
            }
            DiffRowRole::Meta => {}
        }
        rows.push(DiffRow {
            numbers,
            // `+ ` / `- ` 标记**必须留在正文里**：颜色是唯一的区分手段在
            // 无色终端（NO_COLOR、被降级成单色）下直接失效，删掉标记会让
            // 「删了这段」和「加了这段」在视觉上完全一样。上下文的两个空格
            // 只是对齐用的，反而要剥掉，否则整块右侧多出两列。
            text: match role {
                DiffRowRole::Context => rest,
                _ => raw,
            },
            role,
        });
    }
    (rows, width)
}

/// 渲染 diff 行序列（折叠态与展开态共用，保证两态视觉一致）。
///
/// 返回 `None` 表示内容为空——调用方需要画出 `⎿` 空块而不是凭空多一行。
fn render_diff_rows(
    rows: &[DiffRow],
    width: usize,
    max_width: usize,
    limit: usize,
) -> Vec<Line<'static>> {
    // 行号列形如 `10 10 │ `：两侧定宽各 w + 中间分隔空格 + `│ ` 两列
    // —— 即 2w+4。少算这一列的后果是所有 diff 行恒定溢出 1 列，窄终端
    // 下右边会被 TUI 静默裁掉，看起来像代码莫名少了一截。
    let gutter = width * 2 + 4;
    let indent = "    ";
    let body_width = max_width.saturating_sub(indent.len() + gutter).max(1);
    let mut out = Vec::new();

    for row in rows.iter().take(limit) {
        if let DiffRowRole::Meta = row.role {
            out.push(Line::from(Span::styled(
                format!("{indent}{}", row.text),
                Theme::muted(),
            )));
            continue;
        }
        let (old, new) = row.numbers;
        let numbers = format!(
            "{:>width$} {:>width$} │ ",
            old.map(|v| v.to_string()).unwrap_or_default(),
            new.map(|v| v.to_string()).unwrap_or_default(),
            width = width
        );
        let style = match row.role {
            DiffRowRole::Removed => Theme::error(),
            DiffRowRole::Added => Theme::success(),
            // 上下文行是「代码」，不是「旁注」：用次级正文而不是 `dim()`。
            // 旧值（#999 灰）与增删行同处一屏时，未改动的代码看起来像是被禁用。
            DiffRowRole::Context => Theme::text_dim(),
            DiffRowRole::Meta => unreachable!(),
        };
        for (seg, wrapped) in wrap_line(row.text, body_width).into_iter().enumerate() {
            // 折行续行不重复行号，但仍要补出等宽的竖线，右边缘才不会随折行跳动
            let nums = if seg == 0 {
                numbers.clone()
            } else {
                format!("{} │ ", " ".repeat(width * 2 + 1))
            };
            out.push(Line::from(vec![
                Span::styled(format!("{indent}{nums}"), Theme::muted()),
                Span::styled(wrapped, style),
            ]));
        }
    }
    out
}

/// 折叠态渲染 Edit/Write 的 diff 前 [`COLLAPSED_DIFF_MAX_LINES`] **条正文行**。
///
/// 与展开态共用同一套解析（`parse_diff_rows`）与同一套渲染（`render_diff_rows`），
/// 因此两态的行号、缩进、配色完全一致，切换 Ctrl+O 不会让已经看习惯的行忽然变形。
///
/// **预算只算正文**（增/删/上下文）。`@@` 块头与 `… N unchanged lines` 跳过提示是
/// 元信息，不占名额——否则一个改动分散在 5 个 hunk 的 diff 会用掉 9 行预算去显示
/// 元信息，折叠态一屏几乎看不到任何实际改动内容。
fn render_collapsed_diff_preview(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    ctx: &ChatRenderCtx,
) {
    let max_content_width = ctx.max_content_width;
    let content_lines = tool_result_content_lines(msg);
    if content_lines.is_empty() {
        lines.push(Line::from(""));
        return;
    }
    let (rows, width) = parse_diff_rows(&content_lines);
    let body_total = rows
        .iter()
        .filter(|r| !matches!(r.role, DiffRowRole::Meta))
        .count();
    let keep = body_total.min(COLLAPSED_DIFF_MAX_LINES);
    // 截到"第 keep+1 条正文"之前；它前面的元信息顺带带出，否则末行会孤零零
    // 挂一个 `@@` 头却没有对应内容。
    let mut cut = rows.len();
    let mut seen = 0usize;
    for (idx, row) in rows.iter().enumerate() {
        if matches!(row.role, DiffRowRole::Meta) {
            continue;
        }
        if seen == keep {
            cut = idx;
            break;
        }
        seen += 1;
    }
    lines.extend(render_diff_rows(&rows, width, max_content_width, cut));
    let remaining = body_total - keep;
    if remaining > 0 {
        lines.push(Line::from(Span::styled(
            wyj_i18n::tr_fmt("diff.more_lines", &[("n", &remaining.to_string())]),
            Theme::dim(),
        )));
    }
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
    // 只保留仍在飞的调用的实时输出：与 `tool_info` 求交集即完成「还在执行」
    // 的判定，无需额外维护 in-flight 集合（两份真相迟早会对不上）。入飞时按
    // `sequence_no` 建索引，渲染侧就能直接按消息序号查表。
    let live_tool_out: HashMap<usize, String> = state
        .tool_info
        .iter()
        .filter_map(|(id, (_, seq))| state.live_tool_out.get(id).map(|t| (*seq, t.clone())))
        .collect();
    lines.extend(render_message_range(
        MessageRangeRenderArgs {
            messages: &state.messages,
            range: start..state.messages.len(),
            max_content_width,
            sub_agents: &state.sub_agents,
            spinner_frame: state.spinner_frame,
            output_expanded: state.output_expanded,
            live_tool_out: &live_tool_out,
        },
        &mut is_first_user,
    ));

    if !state.thinking_buf.is_empty() {
        render_thinking_block(
            &mut lines,
            &state.thinking_buf,
            max_content_width,
            ThinkingRenderOpts {
                live: true,
                expanded: state.output_expanded,
                elapsed_secs: state.thinking_started.map(|t| t.elapsed().as_secs_f64()),
                spinner_frame: state.spinner_frame,
                cache: None,
            },
        );
    }

    // 流式文本（实时输出中）
    if !state.streaming_buf.is_empty() {
        // 流式路径走 streaming 变体：未定稿的内容按中间态渲染（未闭合围栏不画框、
        // 未成形表格不合并），闭合那一刻才不会整块重排。定稿消息走 `render_markdown`
        // 并按宽度缓存，两者互不影响。
        render_streaming_markdown(&mut lines, &state.streaming_buf, max_content_width);
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
            matches!(state.ui_focus, UiFocus::Todos | UiFocus::TodoDetail),
            state.selected_todo_id.as_deref(),
            state.todo_detail_open,
            state.todo_detail_scroll,
            &state.todo_stats,
            max_content_width,
            state.output_expanded,
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
    output_expanded: bool,
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
        format!(
            "{} [{done}/{total}]{stats_suffix}",
            wyj_i18n::tr("spinner.todo_done")
        )
    } else {
        format!(
            "{spinner_prefix}{} [{done}/{total}]{stats_suffix}",
            wyj_i18n::tr("spinner.todo_title")
        )
    };

    lines.push(Line::from(""));
    // 全部完成：**默认**只保留这一行摘要（不画分隔线、不列条目）——2 行折叠
    // 面板挂在聊天流里纯粹占地方，而 `TurnDone` 刻意不清 current_todos，它会
    // 一直挂到用户发下一条消息才消失。收成一行既保留"这轮做了什么、耗时多少
    // token"的可追溯信息，又不干扰阅读。
    //
    // 但完成 ≠ 不可读。用户在这个窗口里最想知道的恰恰是"刚才做了哪几步、
    // 每步多少 token"——`todo_stats` 与 `todo_execution_logs` 是唯一的审计
    // 凭据且不落盘，所以用户主动 Ctrl+T 时必须还能把它叫回来。
    if all_done && collapsed {
        lines.push(Line::from(vec![
            Span::styled("  ", Theme::dim()),
            Span::styled(
                title,
                Style::default()
                    .fg(Theme::claude_color())
                    .add_modifier(Modifier::BOLD),
            ),
            // 折叠态照常给展开提示，否则用户无从知道这一行还能被叫回来。
            Span::styled(wyj_i18n::tr("spinner.todo_expand"), Theme::dim()),
        ]));
        return (detail_scroll, 0);
    }

    lines.push(Line::from(vec![
        Span::styled("  ", Theme::dim()),
        Span::styled(
            title,
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ),
        if collapsible {
            Span::styled(
                if collapsed {
                    wyj_i18n::tr("spinner.todo_expand")
                } else {
                    wyj_i18n::tr("spinner.todo_collapse")
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
                st.bg(Theme::selected_bg_color())
                    .add_modifier(Modifier::BOLD)
            } else {
                st
            }
        };
        let (icon, item_style) = match item.status {
            TodoStatus::Pending => ("○".to_string(), Theme::muted()),
            TodoStatus::InProgress => {
                let frame = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
                (
                    frame.to_string(),
                    Style::default()
                        .fg(Theme::info_color())
                        .add_modifier(Modifier::BOLD),
                )
            }
            TodoStatus::Completed => (
                "✓".to_string(),
                Style::default()
                    .fg(Theme::muted_color())
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

    if focused && !detail_open {
        lines.push(Line::from(Span::styled(
            format!("  {}", wyj_i18n::tr("todo.focus_hint")),
            Theme::dim(),
        )));
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
                output_expanded,
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
    output_expanded: bool,
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
            .fg(Theme::claude_color())
            .add_modifier(Modifier::BOLD),
    )));

    if log.is_empty() {
        detail_lines.push(Line::from(Span::styled(
            "  no captured execution events yet",
            Theme::dim(),
        )));
    } else {
        let ctx = ChatRenderCtx {
            max_content_width: detail_width,
            sub_agents,
            spinner_frame: 0,
            // 跟随全局 Ctrl+O flag：详情区通过 `TodoExecutionEntry::Message` 重放的
            // **恰恰是工具消息**（不是 thinking），若这里硬编码 false，面板会只剩
            // 一排 `⏺ Tool(...)` 标题、正文全空，等于摧毁这个面板的存在意义。
            output_expanded,
            // 详情区渲染的是**历史**执行记录，那些调用早已 ToolEnd 完毕，
            // 实时输出块不会、也不该出现在这里。
            live_tool_out: &HashMap::new(),
        };
        for entry in log {
            match entry {
                TodoExecutionEntry::Message(id) => {
                    if let Some(msg) = messages.iter().find(|m| m.id == *id) {
                        let mut msg_lines = Vec::new();
                        let mut is_first_user = true;
                        render_chat_message(&mut msg_lines, msg, 0, &mut is_first_user, &ctx);
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
    lines.push(Line::from(Span::styled(
        format!("  {}", wyj_i18n::tr("todo.detail_hint")),
        Theme::dim(),
    )));
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
        Span::styled("  AskQuestion", Style::default().fg(Theme::claude_color())),
        Span::styled("  ", Theme::dim()),
        Span::styled(
            title,
            Style::default()
                .fg(Theme::claude_color())
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
    output_expanded: bool,
    live_tool_out: &'a std::collections::HashMap<usize, String>,
}

/// 渲染 `messages[range]` 为 `Vec<Line>`。`is_first_user` 携带"区间开始前是否已
/// 出现过 User 消息"的状态，供完整消息流与流式尾部共用同一份逻辑。
fn render_message_range(
    args: MessageRangeRenderArgs<'_>,
    is_first_user: &mut bool,
) -> Vec<Line<'static>> {
    let MessageRangeRenderArgs {
        messages,
        range,
        max_content_width,
        sub_agents,
        spinner_frame,
        live_tool_out,
        output_expanded,
    } = args;
    let ctx = ChatRenderCtx {
        max_content_width,
        sub_agents,
        spinner_frame,
        output_expanded,
        live_tool_out,
    };
    let mut lines = vec![];
    let mut i = range.start;
    while i < range.end {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let msg = &messages[i];
        if matches!(msg.role, MessageRole::ToolCall) {
            if let Some(next) = messages.get(i + 1) {
                if i + 1 < range.end
                    && matches!(next.role, MessageRole::ToolResult)
                    && next.sequence_no == msg.sequence_no
                {
                    render_tool_result_block(&mut lines, Some(msg), next, &ctx);
                    trim_trailing_blank_lines(&mut lines);
                    i += 2;
                    continue;
                }
            }
        }
        render_chat_message(&mut lines, msg, i, is_first_user, &ctx);
        trim_trailing_blank_lines(&mut lines);
        i += 1;
    }
    lines
}

/// 聊天区可用内容列数。
///
/// **与 [`draw_chat`] 的 `max_content_width` 必须同源**：两者一旦各算各的，
/// `/cost` 这类按宽度排版的命令会算得比实际可渲染的宽，表格就被换行拆散。
/// 抽成函数就是为了让这种漂移变成编译期可见（`draw_chat` 也走这里）。
pub fn chat_content_width(frame_width: u16) -> usize {
    frame_width.saturating_sub(2) as usize
}

/// 一条逻辑行在 `width` 列下占几行物理行。
///
/// markdown 层已经按内容宽度预折行（见 `crates/tui/CLAUDE.md`「Markdown 排版：
/// 七条不可回退的规则」），所以绝大多数逻辑行天然不超过视口宽度——一次
/// `Line::width()`（逐 span 的 `UnicodeWidthStr` 求和）就能判定为 1 行。
/// 这比 `Paragraph::line_count()` 便宜一个量级：后者对**全历史**跑一遍
/// `WordWrapper`，逐 grapheme 迭代并逐行分配。
///
/// 只有 System 通知、工具标题、Preformatted 这类不参与 markdown 折行的
/// 超宽内容才会真正走到 `WordWrapper`，每帧也就那么几行。
fn logical_line_height(line: &Line<'_>, width: u16) -> usize {
    let width = width.max(1);
    if line.width() <= width as usize {
        return 1;
    }
    Paragraph::new(Text::from(vec![line.clone()]))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

/// 物理行偏移 → (起始逻辑行下标, 该逻辑行内已被滚过的物理行数)。
fn locate_scroll_start(heights: &[u16], scroll: usize) -> (usize, usize) {
    let mut acc = 0usize;
    for (idx, height) in heights.iter().enumerate() {
        let height = *height as usize;
        if acc + height > scroll {
            return (idx, scroll - acc);
        }
        acc += height;
    }
    (heights.len(), 0)
}

/// 从第 `first` 条逻辑行起，截出覆盖 `need` 个物理行的连续区间。
fn visible_chat_lines(
    mut lines: Vec<Line<'static>>,
    heights: &[u16],
    first: usize,
    need: usize,
) -> Vec<Line<'static>> {
    if first >= lines.len() || need == 0 {
        return Vec::new();
    }
    let mut covered = 0usize;
    let mut end = first;
    while end < lines.len() && covered < need {
        covered += heights[end] as usize;
        end += 1;
    }
    lines.drain(..first);
    lines.truncate(end - first);
    lines
}

/// 内容区右侧的应用内滚动条。
///
/// alternate screen 里终端**没有**自己的滚动条（真正的原生滚动条只存在于主屏
/// scrollback，而那套 Inline 方案已因画面撕裂被放弃两次），所以这里画一条与
/// less/vim 观感一致的细条：轨道 `│`、滑块 `┃`，不带上下箭头。
///
/// 参数语义与 [`Paragraph::scroll`] 一致：
/// - `position`：从正文第一行往下跳过的行数（0 = 顶部）；
/// - `max_scroll`：可滚动的最大偏移（内容高度 − 视口高度）；为 0 表示装得下，
///   此时不画——一个永远满格的滚动条只会误导用户去拖它；
/// - `viewport`：视口物理行数，决定滑块长度。
///
/// 只在 `area.width >= 3` 时绘制，占最右 1 列。`chat_content_width` 已经
/// 预留了 2 列边距，因此滚动条不会压到正文。
fn draw_scrollbar(f: &mut Frame, area: Rect, position: usize, max_scroll: usize, viewport: usize) {
    if max_scroll == 0 || area.width < 3 || area.height == 0 {
        return;
    }
    let track = Rect {
        x: area.x + area.width - 1,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let mut state = ScrollbarState::new(max_scroll + viewport)
        .position(position.min(max_scroll))
        .viewport_content_length(viewport.max(1));
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_symbol("┃")
            .track_symbol(Some("│"))
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(Theme::dim())
            .track_style(Theme::border()),
        track,
        &mut state,
    );
}

fn draw_chat(f: &mut Frame, state: &mut AppState, area: Rect) {
    let max_content_width = chat_content_width(area.width);
    let lines = build_pending_chat_lines(state, max_content_width);

    // 高度测量与滚动上限：一次逐行的宽度检查，而不是 `Paragraph::line_count()`。
    // `line_count` 会对全部历史跑一遍 `WordWrapper`，而 `Paragraph::scroll` 只跳过
    // **绘制**、不跳过折行——旧实现因此每帧付两趟全历史折行，成本随会话长度线性
    // 增长。滚轮事件密集时（每个事件一帧）就是用户看到的「滚几下就卡死」。
    let heights: Vec<u16> = lines
        .iter()
        .map(|line| logical_line_height(line, area.width).min(u16::MAX as usize) as u16)
        .collect();
    let total_height: usize = heights.iter().map(|h| *h as usize).sum();

    let content_height = total_height.min(area.height as usize).max(1);
    let max_scroll = total_height.saturating_sub(content_height);
    state.chat_view_height = content_height;
    state.chat_max_scroll = max_scroll;
    if state.chat_follow_tail {
        state.chat_scroll = max_scroll;
    } else {
        state.chat_scroll = state.chat_scroll.min(max_scroll);
    }

    // 只把覆盖视口的那几条逻辑行交给 `Paragraph`：预折行已经把每行的高度算好，
    // 这里按累积高度定位首行，再把 `scroll` 换算成行内偏移。渲染成本从
    // O(全部历史) 降到 O(视口行数)，且 `scroll` 始终小于一屏，不会再在大历史
    // 里撞上 `u16` 上限。
    let (first, intra) = locate_scroll_start(&heights, state.chat_scroll);
    let visible = visible_chat_lines(lines, &heights, first, intra + area.height as usize);
    let para = Paragraph::new(Text::from(visible))
        .wrap(Wrap { trim: false })
        .scroll((intra.min(u16::MAX as usize) as u16, 0));
    f.render_widget(para, area);
    if state.show_scrollbar {
        draw_scrollbar(f, area, state.chat_scroll, max_scroll, content_height);
    }

    if state.unseen_messages {
        let hint = Paragraph::new(Line::from(Span::styled(
            " new messages below · cmd+down ",
            Style::default()
                .fg(Theme::info_color())
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

/// 详情面板滚动偏移的收敛，返回真正喂给 `Paragraph::scroll` 的行偏移。
///
/// `scroll` 是**从正文第一行往下跳过的行数**，与 `Paragraph::scroll` 同向。
/// `follow_tail` 为真时视口钉在末尾跟随新内容（滚动值每帧重算成 `max_scroll`）；
/// 为假时保留用户的绝对偏移，后续追加的事件只在下方堆积、不再把视口拽走。
/// 两个值都会写回，让调用方下一次按键看到的就是实际渲染位置。
pub(crate) fn resolve_detail_scroll(
    scroll: &mut u16,
    follow_tail: &mut bool,
    max_scroll: u16,
) -> u16 {
    let value = if *follow_tail {
        max_scroll
    } else {
        (*scroll).min(max_scroll)
    };
    *scroll = value;
    value
}

/// 选中子 Agent 时接管整个内容区，把它的执行过程（trace）铺开显示。
///
/// 执行内容不再挤在底部面板的详情带里：那条带子受 `bottom_panel_size` 的高度预算
/// 限制，列表本身还要占行，实际几乎永远显示不全。滚动语义见
/// [`resolve_detail_scroll`]：默认贴底跟随新事件，PageUp/PgDn 翻阅后停在历史位置。
fn draw_sub_agent_content(f: &mut Frame, state: &mut AppState, area: Rect) {
    let Some(id) = state.selected_sub_agent else {
        draw_chat(f, state, area);
        return;
    };
    let detail_width = chat_content_width(area.width);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 标题：与面板列表行同款，让"内容区这一屏属于哪个子 Agent"一眼可辨。
    if let Some(s) = state.sub_agents.get(&id) {
        let bg_tag = if s.background { " ◇bg" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!(" a{id} "), Theme::dim()),
            Span::styled(
                truncate_line(&s.description, detail_width.saturating_sub(16)),
                Style::default()
                    .fg(Theme::info_color())
                    .add_modifier(Modifier::BOLD),
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
        ]));
        lines.push(Line::from(Span::styled(
            "─".repeat(detail_width),
            Theme::dim(),
        )));
    }

    // 交接摘要：未完成（中断/触顶/失败）时，这是「做到哪了」最快的一眼——
    // trace 里有全部工具动作，但要滚很久才知道停在哪；结论与改过的文件在这里。
    if let Some(handoff) = state.sub_agents.get(&id).and_then(|s| s.handoff.as_ref()) {
        push_sub_agent_handoff_lines(&mut lines, handoff, detail_width);
    }

    // 正文：优先 trace（含工具入参/出参），落盘 trace 缺失时才退回面板那份摘要流水。
    let events = state.sub_agent_trace_events(id);
    match events {
        Some(events) if !events.is_empty() => {
            push_sub_agent_trace_lines(&mut lines, &events, detail_width);
        }
        _ => {
            let (tool_log, current_tool, status, final_result) = state
                .sub_agents
                .get(&id)
                .map(|s| {
                    (
                        s.tool_log.clone(),
                        s.current_tool.clone(),
                        s.status,
                        s.final_result.clone(),
                    )
                })
                .unwrap_or_else(|| (Vec::new(), None, SubAgentStatus::Running, None));
            push_sub_agent_tool_log(&mut lines, &tool_log, detail_width);
            if let Some(cur) = &current_tool {
                push_wrapped_detail_line(
                    &mut lines,
                    &wyj_i18n::tr("subagent.detail_running"),
                    cur,
                    Theme::dim(),
                    detail_width,
                );
            }
            match status {
                SubAgentStatus::Running => {}
                SubAgentStatus::Done | SubAgentStatus::Failed => {
                    let style = if status == SubAgentStatus::Failed {
                        Theme::error()
                    } else {
                        Theme::tool_result()
                    };
                    let result = final_result.unwrap_or_else(|| wyj_i18n::tr("subagent.no_output"));
                    push_wrapped_detail_line(&mut lines, "result", &result, style, detail_width);
                }
                SubAgentStatus::Interrupted => {
                    push_wrapped_detail_line(
                        &mut lines,
                        "status",
                        &wyj_i18n::tr("subagent.interrupted"),
                        Theme::error(),
                        detail_width,
                    );
                }
            }
        }
    }

    // 末行留给固定的返回/滚动提示，不随正文滚动。
    let body_height = area.height.saturating_sub(1).max(1);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = para.line_count(area.width.max(1)).min(u16::MAX as usize) as u16;
    let max_scroll = total.saturating_sub(body_height);
    state.sub_agent_detail_max_scroll = max_scroll;
    let scroll_y = resolve_detail_scroll(
        &mut state.sub_agent_detail_scroll,
        &mut state.sub_agent_detail_follow_tail,
        max_scroll,
    );
    f.render_widget(para.scroll((scroll_y, 0)), area);
    if state.show_scrollbar {
        // 末行留给固定提示，滚动条也只覆盖正文那一段，避免滑块长度把提示行算进去。
        draw_scrollbar(
            f,
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: body_height,
            },
            scroll_y as usize,
            max_scroll as usize,
            body_height as usize,
        );
    }

    // 用户主动上滚后视口脱离实时尾部：正文下方还会持续堆新事件，用提示行说明
    // 「往下还有内容」以及如何回到跟随态，免得看起来像子 Agent 卡住不动了。
    let detached = scroll_y < max_scroll;
    let hint = Paragraph::new(Line::from(Span::styled(
        if detached {
            wyj_i18n::tr("subagent.content_more_below")
        } else {
            wyj_i18n::tr("subagent.content_hint")
        },
        Theme::info(),
    )));
    f.render_widget(
        hint,
        Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        },
    );
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

/// 交接摘要块：未完成状态下「做到哪了」的一屏速览。
///
/// 只渲染**结论性**信息（状态、停下的原因、最后陈述、改过的文件、错误）。
/// 读过的文件只给计数——一屏塞 30 个路径既挤又没用，需要细看时下面就是 trace。
fn push_sub_agent_handoff_lines(
    lines: &mut Vec<Line<'static>>,
    handoff: &wyj_tools::handoff::SubAgentHandoff,
    detail_width: usize,
) {
    use wyj_tools::handoff::HandoffStatus;

    let (status_label, style) = match handoff.status {
        HandoffStatus::Completed => (wyj_i18n::tr("subagents.status_completed"), Theme::dim()),
        HandoffStatus::Interrupted => {
            (wyj_i18n::tr("subagents.status_interrupted"), Theme::error())
        }
        HandoffStatus::TurnLimit => (wyj_i18n::tr("subagents.status_turn_limit"), Theme::error()),
        HandoffStatus::Failed => (wyj_i18n::tr("subagents.status_failed"), Theme::error()),
    };
    push_wrapped_detail_line(
        lines,
        &wyj_i18n::tr_fmt(
            "subagents.handoff_line",
            &[
                ("status", &status_label),
                ("calls", &handoff.digest.total_calls.to_string()),
            ],
        ),
        "",
        style,
        detail_width,
    );
    if let Some(reason) = &handoff.reason {
        push_wrapped_detail_line(lines, "reason", reason, Theme::dim(), detail_width);
    }
    if !handoff.conclusion.trim().is_empty() {
        push_wrapped_detail_line(
            lines,
            "last position",
            &handoff.conclusion,
            Theme::tool_result(),
            detail_width,
        );
    } else {
        push_wrapped_detail_line(
            lines,
            "last position",
            &wyj_i18n::tr("subagent.handoff_no_conclusion"),
            Theme::dim(),
            detail_width,
        );
    }
    if !handoff.digest.files_modified.is_empty() {
        push_wrapped_detail_line(
            lines,
            "modified",
            &handoff.digest.files_modified.join(", "),
            Theme::dim(),
            detail_width,
        );
    }
    if !handoff.digest.files_read.is_empty() {
        push_wrapped_detail_line(
            lines,
            &wyj_i18n::tr_fmt(
                "subagents.handoff_files_read",
                &[("count", &handoff.digest.files_read.len().to_string())],
            ),
            "",
            Theme::dim(),
            detail_width,
        );
    }
    if !handoff.digest.errors.is_empty() {
        push_wrapped_detail_line(
            lines,
            "errors",
            &handoff.digest.errors.join("; "),
            Theme::error(),
            detail_width,
        );
    }
    if handoff.status.is_incomplete() {
        push_wrapped_detail_line(
            lines,
            "",
            &wyj_i18n::tr("subagents.handoff_hint"),
            Theme::info(),
            detail_width,
        );
    }
    lines.push(Line::from(Span::styled(
        "─".repeat(detail_width.min(120)),
        Theme::dim(),
    )));
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
            TraceEvent::Assistant { text } => {
                // 每轮落点：这是详情面板里唯一能看出「它得出了什么」的地方，
                // 工具行只说明「它做了什么」。
                push_wrapped_detail_line(lines, "say", text, Theme::dim(), detail_width);
            }
            TraceEvent::Control { action, accepted } => {
                push_wrapped_detail_line(
                    lines,
                    "control",
                    &format!("{action} · accepted={accepted}"),
                    if *accepted {
                        Theme::dim()
                    } else {
                        Theme::error()
                    },
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
    // 只列**运行中**的子 Agent（`visible_sub_agents` 是唯一口径）。完成/失败/
    // 中断的条目不再展示：结果已经回灌对话，完整轨迹仍可用
    // `wyj-code subagent-trace` 从落盘文件查阅。
    // 按 id 升序（BTreeMap 天然启动顺序），用 owned Vec 避免可变借用冲突。
    let ids: Vec<u64> = state
        .visible_sub_agents()
        .iter()
        .map(|(id, _)| **id)
        .collect();

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
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let selected_idx = state
        .selected_sub_agent
        .and_then(|id| ids.iter().position(|&i| i == id));

    let list_area = inner;

    let max_content_width = list_area.width.saturating_sub(2) as usize;
    let max_show = (list_area.height as usize).clamp(1, SUB_AGENT_LIST_MAX);
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
                st = st.bg(Theme::selected_bg_color());
            }
            st
        };

        // 面板只列运行中的条目（见函数头的 `visible_sub_agents`），因此图标恒为
        // spinner——完成/失败/中断的状态由聊天流与 `bg_agent_done_reminder` 承载。
        let icon = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()].to_string();
        let item_style = Style::default()
            .fg(Theme::info_color())
            .add_modifier(Modifier::BOLD);

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
}

/// 后台 Shell 任务面板：列表（id / 命令 / 状态 / 耗时）+ 详情（实时输出）。
/// 结构与 `draw_sub_agents_panel` 完全对称，便于两处对照维护。
fn draw_shell_panel(f: &mut Frame, state: &mut AppState, area: Rect) {
    // 只列**运行中**的 shell（`running_shells` 是唯一口径）。已完成条目对用户
    // 已无操作价值：输出早已通过 reminder 回灌模型、聊天流里也留了退出记录。
    // 用 owned Vec 拿 id 快照，避免和后面 `get(id)` 的可变借用冲突。
    let ids: Vec<String> = state
        .running_shells()
        .iter()
        .map(|s| s.id.clone())
        .collect();

    let title = wyj_i18n::tr_fmt(
        "shell.panel_title",
        &[("count", ids.len().to_string().as_str())],
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::border())
        .title(Span::styled(
            title,
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let selected_idx = state
        .selected_shell
        .as_deref()
        .and_then(|id| ids.iter().position(|i| i == id));
    let detail_open = state.shell_detail_open && selected_idx.is_some();

    let list_rows = (ids.len().min(SHELL_LIST_MAX) as u16).min(inner.height);
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
        let Some(s) = state.shells.iter().find(|s| &s.id == id) else {
            continue;
        };
        let is_selected = selected_idx == Some(start + row_i);
        let sel_bg = |mut st: Style| -> Style {
            if is_selected {
                st = st.bg(Theme::selected_bg_color());
            }
            st
        };

        // 面板只列运行中的条目（见函数头的 `running_shells`），因此图标恒为
        // spinner——不存在「已完成」分支可匹配，退出状态由聊天流里的
        // `bash_N 已退出` 记录承载。
        let icon = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()].to_string();
        let item_style = Style::default()
            .fg(Theme::info_color())
            .add_modifier(Modifier::BOLD);

        lines.push(Line::from(vec![
            Span::styled(format!(" {id} "), sel_bg(Theme::dim())),
            Span::styled(format!("{icon} "), sel_bg(item_style)),
            Span::styled(
                truncate_line(&s.command, max_content_width.saturating_sub(24)),
                sel_bg(item_style),
            ),
            Span::styled(
                format!(" ⏱ {}", format_hms(s.elapsed_secs())),
                sel_bg(Theme::dim()),
            ),
        ]));
    }
    if state.ui_focus == UiFocus::Shells && !detail_open {
        lines.push(Line::from(Span::styled(
            format!("  {}", wyj_i18n::tr("shell.focus_hint")),
            Theme::dim(),
        )));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), list_area);

    // 详情区：实时输出。
    let Some(detail_area) = detail_area else {
        return;
    };
    let Some(id) = selected_idx.map(|idx| ids[idx].clone()) else {
        return;
    };
    let detail_width = detail_area.width.saturating_sub(2) as usize;
    let output = wyj_tools::BashSessionManager::global()
        .get(&id)
        .map(|j| j.tail(SHELL_TAIL_BYTES))
        .unwrap_or_default();

    let mut detail_lines: Vec<Line<'static>> = Vec::new();
    if output.trim().is_empty() {
        detail_lines.push(Line::from(Span::styled(
            format!("  {}", wyj_i18n::tr("shell.no_output")),
            Theme::dim(),
        )));
    } else {
        for l in output.lines() {
            detail_lines.push(Line::from(Span::styled(
                format!("  {}", truncate_line(l, detail_width.saturating_sub(2))),
                Theme::tool_result(),
            )));
        }
    }
    detail_lines.push(Line::from(Span::styled(
        format!("  {}", wyj_i18n::tr("shell.detail_hint")),
        Theme::dim(),
    )));

    let text = Text::from(detail_lines);
    let dw = detail_area.width.max(1);
    let para = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
    let total = para.line_count(dw).min(u16::MAX as usize) as u16;
    let visible_height = detail_area.height;
    let max_scroll = total.saturating_sub(visible_height);
    state.shell_detail_max_scroll = max_scroll;
    let scroll = resolve_detail_scroll(
        &mut state.shell_detail_scroll,
        &mut state.shell_detail_follow_tail,
        max_scroll,
    );
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        detail_area,
    );
}

// ─── 输入框 ──────────────────────────────────────────────────────────────────

fn thinking_elapsed_secs(state: &AppState) -> f64 {
    state
        .turn_start_time
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0)
}

/// 标题栏显示的耗时：有工具步骤在跑时算**这一步**的耗时，没有则回落整轮耗时。
///
/// 标题栏的耗时紧挨着某个操作名（`Bash(cargo test)`），语义必须是"这一步跑了多久"，
/// 否则用户会把整轮的 3 分钟误读成这条命令卡了 3 分钟。步骤切换归零、无步骤时用
/// `thinking_elapsed_secs` 兜底。
fn current_step_elapsed_secs(state: &AppState) -> f64 {
    state
        .current_op_started
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or_else(|| thinking_elapsed_secs(state))
}

/// 标题栏显示的 token 消耗：**本轮**实时累计（`↑` input / `↓` output）。
///
/// 数据源是 `turn_live_*`（由 `AgentEvent::UsageDelta` 逐请求累加），**不是**
/// `total_* - turn_start_*`：后者在整个回合里恒为 0——`AgentEvent::Usage` 只在回合
/// 收尾时推送一次 session 累计（见回合结束处的 send），而标题栏恰恰只在回合进行中
/// 显示。详见 `AppState::turn_live_input_tokens` 的注释。
fn title_token_text(state: &AppState) -> String {
    format!(
        "{}↑ {}↓",
        crate::app::fmt_tokens(state.turn_live_input_tokens),
        crate::app::fmt_tokens(state.turn_live_output_tokens)
    )
}

/// 标题栏的实时生成速率（`42 tok/s`）；不在生成中时返回空串。
///
/// 精度按量级收敛：< 10 tok/s 保留一位小数（3.5 与 4 是有区别的，弱模型 /
/// 长思考时恰恰落在这个区间），其余取整——生成中末位小数每帧都在跳，
/// 反而看不清量级。
fn title_stream_rate_text(state: &AppState) -> String {
    let Some(rate) = state.stream_rate.rate() else {
        return String::new();
    };
    let rate = if rate < 10.0 {
        format!("{rate:.1}")
    } else {
        format!("{rate:.0}")
    };
    wyj_i18n::tr_fmt("spinner.stream_rate", &[("rate", rate.as_str())])
}

/// 输入框标题栏 thinking 指示器的基础文案（即"这一步在做什么"，不含 spinner 帧）。
///
/// 优先级（从最具辨识度到默认）：
///   1. `permission_dialog` → "Waiting for approval"
///   2. `plan_dialog`       → "Reviewing plan"
///   3. 当前 InProgress TodoItem 的 `active_form` 或 `content` —— 用户视角的**意图**：
///      "修复登录 bug" 比 "Edit" 更能回答"这一轮到底在干什么"
///   4. `current_tool_name` → 动词短语（[`tool_activity_label`]）：`Bash` → "运行命令"。
///      参数已经完整画在聊天区的 `⏺ Bash(cargo test)` 上，标题栏复述一遍就是
///      逐字节重复：底部那行与聊天区最后一条永远一模一样，用户看着像卡住了。
///      但只显示工具名（`Bash`）同样答不了"在做什么"——动词短语补的正是这段。
///   5. `current_op` 原文 —— 仅剩非工具型后台任务（`/compact` 等，见
///      `begin_background_task`，那里 `current_tool_name` 为 None）
///   6. `pending_queue`    → "Queuing message"
///   7. fallback           → "Thinking"
///
/// **两个对话框必须排在前面**：`Agent::exec_tool_call` 是先
/// `emit ToolEvent::Start`（TUI 据此写 `current_op`）再走 `ensure_tool_permission`
/// ——见 `crates/core/src/agent.rs`。所以权限弹窗打开期间 `current_op` 一定
/// 非空，旧顺序下标题栏永远显示 `Bash(cargo test) · 12.3s` 且秒表继续走，
/// 看着"在忙"实则整轮卡在等用户点 y；`plan_dialog` 同理。
///
/// 由 `draw_input` 标题栏调用，格式化为 `⠋ {label} · {耗时} · {token}`。
fn thinking_status_label(state: &AppState) -> String {
    // 1/2. 正在等用户交互：这是**阻塞在用户身上**而非在跑工具，必须先于
    //      `current_op` 判定，否则秒表会把"等审批的时间"算成"命令跑了多久"。
    if state.permission_dialog.is_some() {
        return wyj_i18n::tr("spinner.waiting_approval");
    }
    if state.plan_dialog.is_some() {
        return wyj_i18n::tr("spinner.reviewing_plan");
    }

    // 3. 有任务清单时先答"为哪件事"：工具名每几步就会换，任务名才是用户
    //    真正在追的进度。缺这一步的话，标题栏与聊天区的工具行仍是同义反复。
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

    // 4. 工具动词短语（不带参数）：参数在聊天区 `⏺ Bash(cargo test)` 上已经完整
    //    可见，这里只补"在做什么"这一层——只写工具名（`Bash`）等于把工具注册名
    //    端给用户看，答不了"它在跑什么"。
    if let Some(name) = state.current_tool_name.as_deref() {
        return tool_activity_label(name);
    }

    // 5. 非工具型后台任务（`/compact` / `/checkpoint` …）只有 `current_op`，
    //    它本身就是那句标签（`/compact`），没有聊天区行与之重复。
    if let Some(op) = state.current_op.as_deref() {
        return op.to_string();
    }

    if !state.pending_queue.is_empty() {
        return wyj_i18n::tr("spinner.queuing_message");
    }

    wyj_i18n::tr("spinner.thinking")
}

/// 工具名 → 标题栏动词短语（`Bash` → "运行命令"）。
///
/// **为什么不复用 `current_op` 原文**：那是 `Bash(cargo test)`，与聊天区的 `⏺`
/// 行逐字节相同，底部只是在抄上面最后一条（v0.3.7 修过一次的形态）。
/// **为什么不直接显示工具名**：`Bash` / `Read` 是注册名，不是动作——用户得先知道
/// 这个 CLI 有哪些工具才读得懂。动词短语两头都避开。
///
/// 未收录的工具回落工具名本身：新增工具不该因为漏配一条 i18n 就显示成空白。
fn tool_activity_label(name: &str) -> String {
    let key = match name.to_ascii_lowercase().as_str() {
        "bash" => "tool_activity.bash",
        "read" => "tool_activity.read",
        "write" => "tool_activity.write",
        "edit" => "tool_activity.edit",
        "glob" => "tool_activity.glob",
        "grep" => "tool_activity.grep",
        "webfetch" => "tool_activity.webfetch",
        "websearch" => "tool_activity.websearch",
        "agent" => "tool_activity.agent",
        "todowrite" => "tool_activity.todo",
        "askquestion" => "tool_activity.ask",
        "exitplanmode" => "tool_activity.plan",
        "toolsearch" => "tool_activity.tool_search",
        "codesearch" => "tool_activity.code_search",
        "memory" => "tool_activity.memory",
        "bashoutput" => "tool_activity.bash_output",
        "killshell" => "tool_activity.kill_shell",
        "contextrecall" => "tool_activity.context_recall",
        "computer" | "app_computer" => "tool_activity.computer",
        _ => return name.to_string(),
    };
    let label = wyj_i18n::tr(key);
    // rust-i18n 对拼错的 key 是**静默回退成 key 字面量**，那样标题栏会出现
    // "tool_activity.bash" 这种字符串。宁可退回工具名也不要把它露给用户。
    if label.is_empty() || label.starts_with("tool_activity.") {
        name.to_string()
    } else {
        label
    }
}

/// 排队提示：写清「已排队」+ 当前在挡着的东西。
///
/// 点名当前步骤（`Bash(sleep 540)` / `Agent(...)`）比笼统的「当前操作完成后」
/// 有用得多：用户看到 `Agent(...)` 就知道在等子 Agent，可以选择再等等、还是
/// ESC 中断重来。
///
/// **刻意不承诺具体送达时刻**：前台子 Agent 会在**它自己的**下一个工具边界
/// 把排队内容取走（见 `wyj_core::SteerQueue`），通常早于「主 Agent 这一步
/// 结束」。写死"X 结束后送达"要么说早了要么说晚了。
fn queue_hint_text(state: &AppState) -> String {
    if state.pending_queue.is_empty() {
        return String::new();
    }
    let running = state
        .current_op
        .as_deref()
        .filter(|op| !op.is_empty())
        .map(|op| wyj_i18n::tr_fmt("spinner.queue_op_running", &[("op", op)]))
        .unwrap_or_else(|| wyj_i18n::tr("spinner.queue_pending"));
    wyj_i18n::tr_fmt(
        "spinner.queue_hint",
        &[
            ("count", &state.pending_queue.len().to_string()),
            ("running", running.as_str()),
        ],
    )
}

/// 标题栏分段拼接：`budget` 列内放不下时，按"整段丢弃"而不是从尾部硬截断。
///
/// 旧实现先 `format!` 拼成一整串再交给 [`truncate_line`]，而 [`truncate_line`]
/// 是从**尾部**截的——排队提示恰好拼在最后，于是窄终端上第一个消失的必然是
/// 「我的补充还没送达」，与 `queue_hint_text` 上方"这正是用户最需要看到的信息"
/// 的判断正好相反（实测 80 列以下就已发生）。
///
/// 保留优先级（rank 越大越留得住）：`tokens`(0) < `elapsed`(1) < `rate`(2)
/// < `queue`(3)。token 是本轮用量、另有 `/cost` 可查；耗时可由状态栏旁证；
/// 速率是"服务端现在到底多快"的唯一现场证据，比前两者更该留下；唯独排队
/// 提示一旦整段消失用户就无从确认自己的补充有没有被接收。
///
/// 全部丢完仍超宽时，最后才对 `label` 做省略号截断——它是唯一不可丢弃的一段。
#[allow(clippy::too_many_arguments)]
fn build_thinking_title(
    frame: char,
    label: &str,
    queue: &str,
    rate: &str,
    elapsed: &str,
    tokens: &str,
    budget: usize,
) -> String {
    // 固定开销：前导空格 + spinner 帧 + 帧后空格 + 尾随空格
    let shell = 4;
    let label_w = || label.chars().map(char_display_width).sum::<usize>();
    // 每段自身的宽度 + " · " 分隔符
    let seg_w = |s: &str| s.chars().map(char_display_width).sum::<usize>() + 3;

    // (保留优先级, 文本)，Vec 顺序即显示顺序：耗时 → 速率 → token → 排队提示。
    // 速率插在耗时之后而不是末尾：它是「这一秒多快」，与「这一步跑了多久」
    // 属于同一类时效信息；token 是本轮累计，放最后。
    let mut optional: Vec<(usize, &str)> = vec![(1, elapsed)];
    if !rate.is_empty() {
        optional.push((2, rate));
    }
    optional.push((0, tokens));
    if !queue.is_empty() {
        optional.push((3, queue));
    }

    let cost = |segs: &[(usize, &str)]| segs.iter().map(|(_, s)| seg_w(s)).sum::<usize>();
    while shell + label_w() + cost(&optional) > budget {
        let Some(pos) = optional
            .iter()
            .enumerate()
            .min_by_key(|(_, (rank, _))| *rank)
            .map(|(i, _)| i)
        else {
            break;
        };
        optional.remove(pos);
    }

    let tail = optional
        .iter()
        .map(|(_, s)| format!(" · {s}"))
        .collect::<String>();
    let label = truncate_line(label, budget.saturating_sub(shell + cost(&optional)));
    format!(" {frame} {label}{tail} ")
}

fn draw_input(
    f: &mut Frame,
    state: &AppState,
    input: &InputBox,
    area: Rect,
    paste_hint_position: Option<(usize, usize)>,
) {
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

    // 标题栏可用列数：Block::border 两侧各吃掉 1 列。
    let title_budget = area.width.saturating_sub(2) as usize;

    let (title_content, title_style) = if state.is_thinking {
        // AI 工作中：标题栏是唯一的"实时进度条"，内容为
        //   `⠋ <当前这一步的名字> · <这一步耗时> · <本轮 token>`
        // spinner 帧提供运动感（每 tick 前进一格），不再叠 animated dots ——
        // 点数尾巴与新加的耗时/用量两段信息同时动会让整行读起来发飘，且
        // "..." 在窄终端里最容易把真正重要的操作名挤出可视区。
        let frame = SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()];
        let label = thinking_status_label(state);
        let elapsed = crate::app::format_hms(current_step_elapsed_secs(state));
        let rate = title_stream_rate_text(state);
        let tokens = title_token_text(state);
        // 排队中：挂在标题栏而不是只靠状态栏右对齐那一句——窄终端上右对齐
        // 会被左边的「模型 · 模式 · cwd」挤掉后半句（实测只剩「…将在」），
        // 而「我的补充还没送达」正是用户最需要看到的信息。窄屏下由
        // `build_thinking_title` 按优先级整段丢弃，而不是从尾部截掉它。
        let queue = queue_hint_text(state);
        (
            build_thinking_title(
                frame,
                &label,
                &queue,
                &rate,
                &elapsed,
                &tokens,
                title_budget,
            ),
            Style::default()
                .fg(Theme::claude_color())
                .add_modifier(Modifier::BOLD),
        )
    } else if is_bang {
        (
            truncate_line(" $ bash · Enter to run ", title_budget),
            Style::default()
                .fg(Theme::success_color())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        // 非思考态：标题栏只保留"Enter to send"一个最简洁锚点，按模式着色，
        // 把 "↑↓ history / Shift+Enter newline / ! bash / / commands / Shift+Tab mode"
        // 等历史提示全部砍掉（这些在 TUI 顶部 /help 与 docs 里有完整说明，
        // 在标题栏塞一长串会让底部看起来很拥挤）。
        let raw = match &state.mode {
            AgentMode::Plan => " [plan] Enter to send ",
            AgentMode::Bypass => " [bypass] Enter to send ",
            AgentMode::Auto => " [auto] Enter to send ",
        };
        let style = match &state.mode {
            AgentMode::Plan => Style::default()
                .fg(Theme::plan_color())
                .add_modifier(Modifier::BOLD),
            // 与状态栏的 [bypass] 芯片同色：Bypass 会跳过全部权限确认，
            // 属于"危险但合法"的模式，黄色偏中性、不足以承担提醒职责。
            // 三处（状态栏芯片 / 标题栏 / 输入框边框）必须同色，
            // 否则用户会以为底部两处说的是不同的模式。
            AgentMode::Bypass => Theme::danger().add_modifier(Modifier::BOLD),
            // Auto 是默认档，走中性灰：绿色会把「默认」读成「已授予某种许可」，
            // 而它实际上仍然会拦（hard_deny）也仍会弹窗（判 AskUser 时）。
            // 手动模式并入 auto 之后，原先区分两者的颜色失去了对象。
            AgentMode::Auto => Theme::dim().add_modifier(Modifier::BOLD),
        };
        (truncate_line(raw, title_budget), style)
    };

    let border_style = if is_bang {
        Style::default().fg(Theme::success_color())
    } else {
        match &state.mode {
            AgentMode::Plan if !state.is_thinking => Theme::plan(),
            AgentMode::Bypass if !state.is_thinking => Theme::danger(),
            // 默认档与普通边框同色：Auto 不再是一个"值得高亮"的状态。
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
        Style::default().fg(Theme::success_color())
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

    // is_thinking 时仍然调用末尾的 `f.set_cursor_position`：用户在 AI 思考中向
    // 输入框继续打字 / 移动光标 / 删字，硬件光标必须跟随用户实际操作。
    // spinner 动画与"esc to interrupt"提示已迁到状态栏（`draw_status`），
    // 主输入框标题栏也不再画 braille spinner，所以这里不再有"光标 vs spinner
    // 字符位置打架"的顾虑。原有 `if is_thinking { return; }` 注释误以为
    // ratatui 会隐藏光标，实际上只是不调 `set_cursor_position`、让终端光标
    // 停留在上一次成功位置不动，导致用户主观感觉"光标卡住/错位"。

    // 光标位置：考虑长行折行后的视觉坐标
    let (vis_row, vis_col) = input.cursor_visual_pos(inner.width as usize);
    let cursor_x = (inner.x + vis_col as u16).min(inner.x + inner.width.saturating_sub(1));
    let cursor_y = (inner.y + vis_row as u16).min(inner.y + inner.height.saturating_sub(1));

    // 多行文字粘贴的瞬时提示。图片/文件使用输入框内持久占位符，不再叠加提示。
    if let Some(hint) = &state.paste_hint {
        if hint.expires_at > Instant::now() {
            let (source_row, source_col) =
                paste_hint_position.unwrap_or((hint.cursor_row, hint.cursor_col));
            let (hint_row, hint_col) =
                input.visual_pos_for(source_row, source_col, inner.width as usize);
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
                            .fg(Theme::hint_color())
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

// ─── @ 文件选取器下拉 ─────────────────────────────────────────────────────────

fn draw_file_completions(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Theme::hint())
        .title(Span::styled(
            " @ 文件 ",
            Style::default()
                .fg(Theme::hint_color())
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
                        Theme::selection_on(Theme::hint_color()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(hint, Theme::selection_on(Theme::hint_color())),
                ])
            } else {
                let name_style = if entry.is_dir {
                    Theme::hint()
                } else {
                    Theme::body()
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
        .border_style(Theme::plan())
        .title(Span::styled(
            " / 命令 & Skill  ↑↓ 选择  Tab/Enter 填入  Esc 关闭 ",
            Theme::plan(),
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
                            .bg(Theme::selected_bg_color())
                            .fg(Theme::text_color())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        desc_str,
                        Style::default()
                            .bg(Theme::selected_bg_color())
                            .fg(Theme::text_dim_color()),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(name_pad, Theme::body()),
                    Span::styled(desc_str, Theme::dim()),
                ])
            }
        })
        .collect();

    let para = Paragraph::new(Text::from(lines));
    f.render_widget(para, inner);
}

// ─── 状态栏 ──────────────────────────────────────────────────────────────────

/// 状态栏左侧的模式芯片：`[plan]` / `[bypass]` / `[default]`，返回 (文本, 样式)。
///
/// 三种模式都必须显式展示：旧实现里 Normal 不显示任何标记，于是"看不到标记"
/// 既可能是 Normal、也可能是芯片被挤出可视区，用户无法区分自己是否正处在
/// 跳过全部权限确认的 Bypass 模式下——而这恰恰是最需要一眼确认的状态。
///
/// Bypass 用红色而非黄色：它等价于「本次会话里 Edit/Write/Bash 全部不再询问」，
/// 是需要用户知情承担的取舍，黄色读起来仍像中性提示。文本与样式一并返回而不是
/// 直接给 `Span`，是因为调用方还要拿纯文本按字符宽度算右对齐补位。
fn status_mode_chip(mode: &AgentMode) -> (&'static str, Style) {
    match mode {
        AgentMode::Plan => (
            "[plan]",
            Style::default()
                .fg(Theme::plan_color())
                .add_modifier(Modifier::BOLD),
        ),
        AgentMode::Bypass => ("[bypass]", Theme::danger().add_modifier(Modifier::BOLD)),
        // 默认档用中性灰（`dim`），与标题栏一致：绿色是 success 语义，
        // auto 并没有"成功通过什么"，它只是默认档。
        AgentMode::Auto => ("[auto]", Theme::dim().add_modifier(Modifier::BOLD)),
    }
}

/// 状态栏左右两段的排版：返回 `(实际要画的 cwd 文本, 右侧提示是否保留)`。
///
/// **宽度一律按显示列宽算，绝不能用 `chars().count()`。** 旧实现两个 side 都按
/// char 数算，而中文与全宽符号是双宽：`● 1 条已排队（等待送达）` char 数 14、显示
/// 宽度 24，补位少了 10 列。宽度算错 → Paragraph 右端溢出 → 溢出是**从右侧硬切**
/// 的，切点正好落在全角 `（` 中间，用户看到的就是右下角莫名其妙挂着一个半截的
/// `(`（本条不变式的起因）。
///
/// 修宽度只解决"算错"，解决不了"根本放不下"：右段自身就超预算时同样会硬切。
/// 因此分两级降级：
///
/// 1. 右段优先保（它只在排队 / Ctrl+C 待确认时出现，是用户此刻最需要确认的信息），
///    cwd 吃剩下的预算，超出就 [`truncate_line`] 加省略号，连 2 列都分不出时整段留空
///    ——半截路径比没有路径更难认；
/// 2. 右段**自身**就塞不下时整段丢弃，绝不交给 Paragraph 硬切。与
///    [`build_thinking_title`] 的"整段丢弃而非尾部截断"同一条道理。排队提示在输入框
///    标题栏还有一份（那里是 rank 最高的一段），状态栏丢掉不算信息全失。
///
/// 保证的不变式：`left_fixed + disp_w(cwd_display) + (keep_right ? right_w : 0) + 1
/// <= area_width`，即任何宽度下都不溢出、不硬切。
fn status_bar_layout(
    area_width: usize,
    left_fixed: usize,
    right_disp_w: usize,
    cwd: &str,
) -> (String, bool) {
    // 末尾那 1 列是右段专属的尾随空格，与 `+ 1 <` 等价
    let keep_right = left_fixed + right_disp_w < area_width;
    let right_len = if keep_right { right_disp_w } else { 0 };
    let avail_cwd = area_width.saturating_sub(left_fixed + right_len + 1);
    let cwd_display = if avail_cwd >= 2 {
        truncate_line(cwd, avail_cwd)
    } else {
        String::new()
    };
    (cwd_display, keep_right)
}

fn draw_status(f: &mut Frame, state: &AppState, area: Rect) {
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
            format!("● {}", queue_hint_text(state)),
            Style::default()
                .fg(Theme::queue_color())
                .add_modifier(Modifier::BOLD),
        )
    } else if state.ctrl_c_pressed {
        (
            "ctrl+c again to exit".to_string(),
            Style::default()
                .fg(Theme::warning_color())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        // 默认状态：状态栏右侧不占位，只在排队 / Ctrl+C 待确认时给出对应提示。
        (String::new(), Theme::dim())
    };

    // 左侧固定三段：模型 · 工作模式 · 工作目录。
    //
    // 用量已迁到输入框标题栏（与"当前这一步"同处一行，见 `title_token_text`），
    // 自动压缩次数也从这里移除——状态栏是常驻信息，该留给"我在哪 / 我以什么
    // 身份在跑"这类每一眼都要确认的常量，而不是会不断变长的累计数字。
    let (chip_text, chip_style) = status_mode_chip(&state.mode);
    // 会话级分组标记：只有「仅本会话使用」生效时才出现，紧跟模型名。
    // 必须可见——这个功能的全部风险就是用户忘了它只在本次会话生效，
    // 或者忘了自己切过、以为全局默认值被改了。状态栏是唯一每帧都在的位置。
    let session_tag = state
        .session_profile_active()
        .map(|name| format!(" [{name}]"))
        .unwrap_or_default();
    // 芯片只带前导空格（不带尾随），否则和紧随其后的 " · " 会叠成两个空格。
    // 宽度一律按**显示列宽**算，绝不能用 `chars().count()`。
    //
    // 旧实现两个 side 都按 char 数算，而中文与全角符号是双宽：`● 1 条已排队（等待送达）`
    // char 数 14、显示宽度 24，预留少了 10 列。补位不足 → Paragraph 右端溢出 →
    // 溢出是**从右侧硬切**的，切点正好落在全角 `（` 中间，用户看到的就是右下角
    // 莫名其妙挂着一个半截的 `(`。
    //
    // 修宽度只解决"算错"，解决不了"根本放不下"：此时同样会溢出硬切。所以右侧提示
    // （排队 / Ctrl+C 待确认，只在特定时刻出现）先占预算，cwd 吃剩下的、超出就省略号
    // 截断，**连 2 列都分不出时整段留空**——半截路径比没有路径更难认。
    let disp_w = |s: &str| s.chars().map(char_display_width).sum::<usize>();
    // " ◆ " + 模型名 + 会话标签 + " {chip}" + " · "，末尾另留 1 列给尾部空格
    let left_fixed =
        3 + disp_w(&state.model_name) + disp_w(&session_tag) + 1 + disp_w(chip_text) + 3;
    let (cwd_display, keep_right) = status_bar_layout(
        area.width as usize,
        left_fixed,
        disp_w(&right_help),
        &cwd_str,
    );
    let right_len = if keep_right { disp_w(&right_help) } else { 0 };
    let left_text = format!(
        " ◆ {}{} {} · {}",
        state.model_name, session_tag, chip_text, cwd_display
    );

    let pad = (area.width as usize).saturating_sub(disp_w(&left_text) + right_len + 1);

    // thinking 指示器已迁回 `draw_input` 标题栏（"⠋ <步骤> · 3.2s · 1.2k↑ 3↓"
    // 紧贴用户输入框），状态栏保持单一职责，不再额外画 spinner，避免上下两处
    // 同时闪烁 + 与标题栏文字打架。
    //
    // 这里**刻意不显示上下文占用百分比**（v0.2.26 移除）。三个理由：
    //   1. 旧实现算的是 `estimate_tokens(messages)`，压缩决策算的是
    //      `estimate_request_tokens`（含 system + 工具 schema + 输出预留），
    //      两者差 1 万+ token，在 200K 窗口下就是 6 个以上百分点的系统性
    //      低报——条的颜色和实际压缩时机对不上号。Anthropic 官方文档也
    //      明确警告过 statusline 的 `used_percentage` 不指示压缩时机。
    //   2. 压缩已完全自动化，百分比对用户没有可操作性，只是持续制造焦虑。
    //   3. 想看的人可以主动查 `/context`，那里是同源口径的精确分解。
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        " ◆ ",
        Style::default()
            .fg(Theme::claude_color())
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(state.model_name.clone(), Theme::dim()));
    if !session_tag.is_empty() {
        // 与模型名同色：它是模型名的限定语，不是另一条独立状态。
        spans.push(Span::styled(session_tag, Theme::dim()));
    }
    spans.push(Span::styled(format!(" {chip_text}"), chip_style));
    spans.push(Span::styled(" · ".to_string(), Theme::dim()));
    spans.push(Span::styled(cwd_display, Theme::dim()));
    spans.push(Span::raw(" ".repeat(pad)));
    if keep_right {
        spans.push(Span::styled(right_help, right_style));
        spans.push(Span::raw(" "));
    }

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

// ─── 会话占用裁决框 ───────────────────────────────────────────────────────────

/// 「恢复的会话正被另一个实例使用」裁决框。与权限框同为底部常驻面板（不居中浮层）：
/// 它挡在聊天区输入框之上、不抢整屏，用户仍能往上翻看被恢复出来的历史。
///
/// 文案全部走 `tr()`——用户可见 UI 走 i18n 是仓库硬约定（历史上 plan / exec-mode
/// 面板硬编码中文是遗留，不要照抄）。
fn draw_session_conflict_panel(f: &mut Frame, dlg: &SessionConflictDialog, area: Rect) {
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
fn choice_row(selected: bool, label: &str, max_w: usize) -> Line<'static> {
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

/// 项目级 skill 信任面板。展示内容取自
/// `wyj_commands::skill::project_skill_infos`（走与真正加载完全相同的 walk），
/// 因此这里列的命令名与批准后实际生效的完全一致。
fn draw_skill_trust_panel(
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
fn draw_plan_approval_panel(f: &mut Frame, dlg: &PlanApprovalDialog, area: Rect) {
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
fn plan_option_row(idx: usize, cursor: usize, label: &str, max_w: usize) -> Line<'static> {
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

/// 自由文本的一行：`❯ ` 前缀 + 真实块状光标 + 超宽时按光标横向滚动。
///
/// 光标用**反色块**而不是移动终端硬件光标：弹窗是浮层，硬件光标同时被底部
/// 主输入框占用，两者会打架；反色块在任何焦点状态下都稳定可见。
fn freetext_line(input: &InputBox, row: usize, line: &str, max_w: usize) -> Line<'static> {
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
fn visible_window(chars: &[char], cursor_col: usize, inner_w: usize) -> (usize, usize) {
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

fn draw_evolution_dialog(f: &mut Frame, dialog: &EvolutionDialog, area: Rect) {
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
fn format_bytes(bytes: u64) -> String {
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

fn draw_profile_text_overlay(f: &mut Frame, area: Rect, title_key: &str, body: &str) {
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

    let lines: Vec<Line<'static>> = items
        .iter()
        .enumerate()
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
pub fn draw_action_menu<T, A: PartialEq>(
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
    let height = ((menu.items.len() as u16) + 2)
        .min(area.height.saturating_sub(4))
        .max(4);
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
            format!(" {title} "),
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
fn draw_confirm_overlay(f: &mut Frame, area: Rect, anchor: Rect, title: &str, action_label: &str) {
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
mod freetext_window_tests {
    use super::*;

    fn cursor_is_painted(chars: &[char], cursor_col: usize, inner_w: usize) -> bool {
        let (start, end) = visible_window(chars, cursor_col, inner_w);
        (start..end).any(|i| i == cursor_col) || cursor_col == chars.len()
    }

    /// 光标必须**落在窗口内**：`freetext_line` 只给 `start..end` 里的
    /// `cursor_col` 画反色块，光标不在区间里、又不等于行尾时，**整块光标消失**，
    /// 用户失去所有光标位置提示。
    ///
    /// 旧实现向后走时减的是「即将丢到左窗外的字符」、比的却是「光标前总宽度」，
    /// 于是循环在「左侧丢弃量塞得下」时就停——与意图相反，光标偏移量无界增长，
    /// 行宽超过约 `2 × inner_w` 后必然消失（inner_w=70 时 ASCII 第 139 个字符、
    /// 中文第 70 个字起就看不见了）。中英混排的项目里一行稍长的回答就会触发。
    #[test]
    fn cursor_stays_visible_on_long_lines() {
        for (name, text) in [
            ("ascii", "a".repeat(400)),
            ("cjk", "中".repeat(400)),
            ("emoji", "😀".repeat(300)),
            (
                "mixed",
                "这是一段中英 mixed 混排的测试文本用来检查滚动窗口的行为是否正确".repeat(12),
            ),
        ] {
            let chars: Vec<char> = text.chars().collect();
            for inner_w in [4usize, 10, 20, 40, 56, 70, 110] {
                for cursor in 0..=chars.len() {
                    // 退化区间跳过：单个字符本身就比整列还宽（1 列窗口里的 CJK），
                    // 任何实现都画不下，这不是本函数该保证的事。
                    let cell = if cursor < chars.len() {
                        crate::input::char_width(chars[cursor])
                    } else {
                        1
                    };
                    if cell > inner_w || inner_w < 2 {
                        continue;
                    }
                    assert!(
                        cursor_is_painted(&chars, cursor, inner_w),
                        "{name}: inner_w={inner_w} cursor={cursor} 处光标不可见"
                    );
                }
            }
        }
    }

    /// 窗口宽度本身也必须守恒：向前丢字符不能把总显示宽度撑破 inner_w，
    /// 否则 ratatui 会把整行裁掉，右侧内容连带光标一起消失。
    #[test]
    fn window_never_exceeds_available_width() {
        let texts = ["a".repeat(300), "中".repeat(300), "中a中b".repeat(80)];
        for text in texts {
            let chars: Vec<char> = text.chars().collect();
            for inner_w in [2usize, 4, 10, 40, 70] {
                for cursor in 0..=chars.len().min(200) {
                    let (start, end) = visible_window(&chars, cursor, inner_w);
                    assert!(start <= end, "start={start} end={end}");
                    let width: usize = chars[start..end]
                        .iter()
                        .map(|c| crate::input::char_width(*c))
                        .sum();
                    assert!(width <= inner_w, "width={width} > inner_w={inner_w}");
                }
            }
        }
    }
}

#[cfg(test)]
mod plan_approval_render_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 计划批准面板的手动输入必须**逐行渲染**。
    ///
    /// 该输入已接上 `apply_edit_key`（Shift+Enter / Ctrl+J / 行尾 `\`+Enter
    /// 都能换行），提交时取的是 `input.text()`。此前渲染只取
    /// `lines.first()` 并把一个假的 `_` 永远贴在行尾，面板高度还写死 5 行：
    /// 用户写三行只看见一行，回车却把三行一起发出去——所见非所发。
    #[test]
    fn freetext_lines_are_all_rendered() {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let mut dlg = PlanApprovalDialog::new(tx);
        // 进入「手动输入反馈…」并敲三行
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for line in ["第一行内容", "第二行内容", "第三行内容"] {
            for ch in line.chars() {
                dlg.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            dlg.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        }
        // 末尾那次 Ctrl+J 会多出一个空行，正好也验证它不被吞
        assert_eq!(
            dlg.freetext_input().map(|i| i.lines.len()),
            Some(4),
            "手动输入必须真的能存下多行"
        );

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");
        terminal
            .draw(|f| {
                let area = f.area();
                draw_plan_approval_panel(f, &dlg, area);
            })
            .expect("draw ok");

        let buf = terminal.backend().buffer();
        // TestBuffer 把宽字符（CJK）的占位空格也存成 cell，逐 cell 拼出来是
        // "第 一 行 内 容" 这种带空隙的样子，比对前先去掉所有空白。
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        for expected in ["第一行内容", "第二行内容", "第三行内容"] {
            assert!(
                compact.contains(expected),
                "面板必须画出每一行，缺了 {expected:?}\n实际渲染：\n{text}"
            );
        }
    }
}

#[cfg(test)]
mod tool_result_fold_tests {
    use super::*;
    use crate::app::ShellStatus;
    use crate::test_support::{make_state, message, render_to_text, rendered_text};
    // 生产代码不再出现裸 `Color`（见 theme_tests.rs 的扫描钉子）；测试需要它
    // 来断言"渲染结果确实用了某个具体颜色"。
    use ratatui::style::Color;

    /// `/cost` 的表格依赖换行与列对齐。TUI 的 assistant 消息走 markdown，
    /// 而 markdown 把「连续非空行」当一个段落重新折行——曾把一张 50 行的
    /// 成本表塌成 6 行糊在一起。这条测试锁死「预排版消息必须逐行原样保留」。
    #[test]
    fn preformatted_message_keeps_every_line_and_its_alignment() {
        // 用程序构造对齐的样本（手写容易把右边缘数错 1 列）。
        let body = [
            format!("{:<14}{:>9}  {:>9}", "Item", "Tokens", "Cost"),
            format!("{:<14}{:>9}  {:>9}", "Input", "289,112", "~$0.0867"),
            format!("{:<14}{:>9}  {:>9}", "Output", "40,440", "~$0.0485"),
            format!("{:<14}{:>9}  {:>9}", "Total", "329,552", "~$0.1352"),
        ]
        .join("\n");
        let msg = ChatMessage::preformatted(body.clone());
        let lines = render_messages(&[msg], 100);

        // 行数必须与源文本一致——markdown 段落合并会把多行揉成一行再折行。
        assert_eq!(
            lines.len(),
            body.lines().count(),
            "预排版内容被重新折行了:\n源:\n{body}\n渲染:\n{lines:?}"
        );
        // 数字列右边缘必须齐平（渲染层统一加了 2 空格缩进）。
        let edge = |l: &str| crate::markdown::display_width(l);
        let widths: Vec<usize> = lines.iter().map(|l| edge(l)).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "各行宽度不齐（缩进/折行被破坏）: {widths:?}\n{lines:?}"
        );
    }

    /// 预排版消息超宽时必须**截断**而不是折行——折行会凭空多出一行，
    /// 把下面的表格行顶歪。
    #[test]
    fn preformatted_message_truncates_instead_of_wrapping() {
        let long = "x".repeat(300);
        let msg = ChatMessage::preformatted(long);
        let lines = render_messages(&[msg], 60);
        assert_eq!(lines.len(), 1, "超宽行被折成了多行:\n{lines:?}");
        assert!(
            lines[0].chars().count() <= 60,
            "截断后仍超宽: {}",
            lines[0].chars().count()
        );
    }

    /// 多行预排版消息：行数必须与源文本一致，且**不出现空行**——
    /// markdown 的段落合并正是靠空行分段，这里一旦混进去就会被重新折行。
    #[test]
    fn preformatted_message_has_no_blank_lines_injected() {
        let body = "a\nb\nc";
        let msg = ChatMessage::preformatted(body.to_string());
        let lines = render_messages(&[msg], 60);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines.iter().all(|l| !l.trim().is_empty()),
            "被插入了空行:\n{lines:?}"
        );
    }

    /// 把一段预排版文本送进真实渲染管线，返回渲染后的各行文本。
    pub(super) fn render_preformatted(body: &str, width: usize) -> Vec<String> {
        render_messages(&[ChatMessage::preformatted(body.to_string())], width)
    }

    fn render_messages(messages: &[ChatMessage], width: usize) -> Vec<String> {
        render_messages_full(messages, width, false, &HashMap::new())
    }

    fn render_messages_with_output_expanded(
        messages: &[ChatMessage],
        width: usize,
        output_expanded: bool,
    ) -> Vec<String> {
        render_messages_full(messages, width, output_expanded, &HashMap::new())
    }

    fn render_messages_full(
        messages: &[ChatMessage],
        width: usize,
        output_expanded: bool,
        live_tool_out: &HashMap<usize, String>,
    ) -> Vec<String> {
        let mut is_first_user = true;
        rendered_text(render_message_range(
            MessageRangeRenderArgs {
                messages,
                range: 0..messages.len(),
                max_content_width: width,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                output_expanded,
                live_tool_out,
            },
            &mut is_first_user,
        ))
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
            handoff: None,
        }
    }

    fn shell(status: ShellStatus) -> crate::app::ShellUiState {
        crate::app::ShellUiState {
            id: "bash_1".to_string(),
            command: "npm run dev".to_string(),
            status,
            started_at: Instant::now(),
            final_elapsed: (status != ShellStatus::Running).then_some(1.0),
            wake_sent: false,
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

    // ── 后台任务面板可见性 ─────────────────────────────────────────────
    //
    // 回归背景：用户报告「所有 shell 后台执行的任务界面上看不见执行的内容」。
    // 面板显示规则与子 Agent 面板同构：有运行中任务时自动出现、全部结束后
    // 自动收起，但用户仍可用 /shells 或方向键显式打开查看历史。

    #[test]
    fn running_shells_keep_automatic_panel_visible() {
        let mut state = make_state();
        state.shells.push(shell(ShellStatus::Running));

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(height > 0, "有运行中后台任务时面板必须自动显示");
        assert!(matches!(panel, BottomPanel::ShellProcesses));
    }

    #[test]
    fn completed_shells_do_not_keep_passive_panel_visible() {
        let mut state = make_state();
        state.shells.push(shell(ShellStatus::Exited(0)));

        let (height, panel) = bottom_panel_size(&state, 40);

        assert_eq!(height, 0, "全部结束后不应继续占用底部空间");
        assert!(matches!(panel, BottomPanel::None));
    }

    #[test]
    fn completed_shells_are_not_listed_even_when_panel_is_focused() {
        // 显式聚焦（Shift+↑ 或 /shells）**不再**让已结束的任务回到列表里：
        // 面板口径统一为「只列运行中」。原先这里断言已完成条目仍可见，那是用户
        // 明确要求去掉的行为——已退出的任务输出早已回灌模型、聊天流也留了
        // `bash_N 已退出` 记录，留在面板里只是噪音。
        let mut state = make_state();
        state.shells.push(shell(ShellStatus::Exited(0)));
        state.selected_shell = Some("bash_1".to_string());
        state.ui_focus = UiFocus::Shells;

        let (height, panel) = bottom_panel_size(&state, 40);

        assert_eq!(height, 0, "已结束的任务不应继续占用底部空间");
        assert!(matches!(panel, BottomPanel::None));
    }

    #[test]
    fn running_shell_stays_listed_when_focus_moves_away() {
        // 反向钉子：口径收紧到「只列运行中」不能误伤「有运行中就显示」——
        // 焦点不在 shells 上时，底部面板仍应自动显示运行中的任务。
        let mut state = make_state();
        state.shells.push(shell(ShellStatus::Running));
        state.ui_focus = UiFocus::Composer;

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(height > 0);
        assert!(matches!(panel, BottomPanel::ShellProcesses));
    }

    #[test]
    fn shell_focus_outranks_running_sub_agents() {
        // 子 Agent 和后台 shell 经常同时跑（派 agent 改代码 + 自己起 dev
        // server）。若按"有运行中"固定排先后，子 Agent 会永远占住底部位置，
        // 后台任务就再也看不见、也按不了 k —— 用户显式导航过去必须能赢。
        let mut state = make_state();
        state
            .sub_agents
            .insert(1, sub_agent(SubAgentStatus::Running));
        state.shells.push(shell(ShellStatus::Running));
        state.ui_focus = UiFocus::Shells;

        let (_, panel) = bottom_panel_size(&state, 40);

        assert!(matches!(panel, BottomPanel::ShellProcesses));
    }

    #[test]
    fn sub_agent_focus_outranks_running_shells() {
        // 上一条的镜像：焦点在子 Agent 面板时也不该被后台任务抢走。
        let mut state = make_state();
        state
            .sub_agents
            .insert(1, sub_agent(SubAgentStatus::Running));
        state.shells.push(shell(ShellStatus::Running));
        state.ui_focus = UiFocus::SubAgents;

        let (_, panel) = bottom_panel_size(&state, 40);

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

    fn skill_info(name: &str, desc: &str) -> wyj_commands::skill::ProjectSkillInfo {
        wyj_commands::skill::ProjectSkillInfo {
            name: name.to_string(),
            description: desc.to_string(),
        }
    }

    #[test]
    fn pending_skill_trust_shows_skill_trust_panel() {
        let mut state = make_state();
        state.pending_skill_trust = Some(vec![skill_info("review", "Review code")]);

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(height > 0);
        assert!(matches!(panel, BottomPanel::SkillTrust));
    }

    /// 两者都是安全门槛，MCP 能执行任意命令、级别更高，必须先弹。
    #[test]
    fn project_mcp_trust_outranks_skill_trust() {
        let mut state = make_state();
        state.pending_mcp_trust = Some(vec![]);
        state.pending_skill_trust = Some(vec![skill_info("review", "Review code")]);

        let (_, panel) = bottom_panel_size(&state, 40);

        assert!(matches!(panel, BottomPanel::ProjectTrust));
    }

    /// skill 门控同属安全门槛，必须压过 ExecModeConfirm 这类流程性面板。
    #[test]
    fn skill_trust_outranks_exec_mode_confirm() {
        let mut state = make_state();
        state.pending_skill_trust = Some(vec![skill_info("review", "Review code")]);
        state.exec_mode_confirm = Some(ExecModeConfirmDialog {
            pending_message: "go".to_string(),
            pending_attachments: Vec::new(),
            pending_display: "go".to_string(),
        });

        let (_, panel) = bottom_panel_size(&state, 40);

        assert!(matches!(panel, BottomPanel::SkillTrust));
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
    fn completed_sub_agents_are_not_listed_even_when_panel_is_focused() {
        // 与 shells 同构：面板口径统一为「只列运行中」。结果早已通过
        // `bg_agent_done_reminder` 回灌对话，完整轨迹仍可用
        // `wyj-code subagent-trace` 从落盘文件查阅。
        let mut state = make_state();
        state.sub_agents.insert(1, sub_agent(SubAgentStatus::Done));
        state.selected_sub_agent = Some(1);
        state.ui_focus = UiFocus::SubAgents;

        let (height, panel) = bottom_panel_size(&state, 40);

        assert_eq!(height, 0, "已完成的子 Agent 不应继续占用底部空间");
        assert!(matches!(panel, BottomPanel::None));
    }

    #[test]
    fn visible_sub_agents_filters_out_finished_ones() {
        let mut state = make_state();
        state.sub_agents.insert(1, sub_agent(SubAgentStatus::Done));
        state
            .sub_agents
            .insert(2, sub_agent(SubAgentStatus::Running));
        state
            .sub_agents
            .insert(3, sub_agent(SubAgentStatus::Failed));
        state
            .sub_agents
            .insert(4, sub_agent(SubAgentStatus::Interrupted));
        state
            .sub_agents
            .insert(5, sub_agent(SubAgentStatus::Running));

        let visible: Vec<u64> = state
            .visible_sub_agents()
            .iter()
            .map(|(id, _)| **id)
            .collect();

        assert_eq!(
            visible,
            vec![2, 5],
            "只有 Running 的子 Agent 应当进入面板列表"
        );
    }

    // ── 任务全部完成 → 默认收成一行摘要，但可再展开 ─────────────────────
    //
    // 回归背景：用户报告「tasklist 多任务全部完成后，折叠态的面板一直挂在
    // 聊天流里不消失」。根因是 `TurnDone` 刻意不清 `current_todos`（现有测试
    // 锁死了这个行为），而原渲染把「全部完成」画成 2 行（头部 + ─ 分隔线），
    // 于是它会一直挂到用户发下一条消息。改成只画一行摘要后既保留了「这轮
    // 做了什么 / 耗时多少 token」的可追溯信息，又不再占地方。
    //
    // 但「默认折叠」不等于「不可读」：每项耗时/token 与执行回放是唯一的审计
    // 凭据且不落盘，用户按 Ctrl+T 必须还能把完整清单叫回来。

    #[test]
    fn completed_todo_panel_renders_single_summary_line() {
        let mut state = make_state();
        state.current_todos = Some(vec![
            wyj_tools::todo::TodoItem {
                id: "a".to_string(),
                content: "task a".to_string(),
                status: wyj_tools::todo::TodoStatus::Completed,
                priority: None,
                active_form: None,
            },
            wyj_tools::todo::TodoItem {
                id: "b".to_string(),
                content: "task b".to_string(),
                status: wyj_tools::todo::TodoStatus::Completed,
                priority: None,
                active_form: None,
            },
        ]);

        let rendered: Vec<String> = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // 从面板头部往后看：只剩摘要这一行，后面不能再有任何内容
        // （不能再有 ─ 分隔线、条目列表和 ctrl+t 展开提示）。
        let head = rendered
            .iter()
            .position(|l| l.contains(&wyj_i18n::tr("spinner.todo_done")))
            .expect("面板未渲染");
        let tail = &rendered[head..];
        assert_eq!(
            tail.len(),
            1,
            "全部完成后面板应只剩一行摘要，实际是 {tail:?}"
        );
        assert!(tail[0].contains(&format!("{} [2/2]", wyj_i18n::tr("spinner.todo_done"))));
        assert!(
            !tail.iter().any(|l| l.contains('─')),
            "摘要态不应再画分隔线：{tail:?}"
        );
        assert!(
            tail[0].contains(&wyj_i18n::tr("spinner.todo_expand")),
            "摘要态必须提示还能展开，否则用户不知道这一行可以被叫回来：{tail:?}"
        );
    }

    #[test]
    fn completed_todo_panel_is_still_expandable_by_ctrl_t() {
        // 上一条的对照：全部完成只是**默认**折叠，`todo_panel_expanded`
        // （即 Ctrl+T 的作用结果）为 true 时必须画回完整条目清单。
        let mut state = make_state();
        state.current_todos = Some(vec![
            wyj_tools::todo::TodoItem {
                id: "a".to_string(),
                content: "task a".to_string(),
                status: wyj_tools::todo::TodoStatus::Completed,
                priority: None,
                active_form: None,
            },
            wyj_tools::todo::TodoItem {
                id: "b".to_string(),
                content: "task b".to_string(),
                status: wyj_tools::todo::TodoStatus::Completed,
                priority: None,
                active_form: None,
            },
        ]);
        state.todo_panel_expanded = true;

        let rendered: Vec<String> = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        let head = rendered
            .iter()
            .position(|l| l.contains(&wyj_i18n::tr("spinner.todo_done")))
            .expect("面板未渲染");
        let tail = &rendered[head..];
        assert!(
            tail.iter().any(|l| l.contains("task a")),
            "完成后按 Ctrl+T 应能重新看到条目清单：{tail:?}"
        );
        assert!(
            tail.iter().any(|l| l.contains("task b")),
            "完成后按 Ctrl+T 应能重新看到条目清单：{tail:?}"
        );
        assert!(
            tail.iter().any(|l| l.contains('✓')),
            "完成项应渲染为 ✓：{tail:?}"
        );
        assert!(
            tail.iter()
                .any(|l| l.contains(&wyj_i18n::tr("spinner.todo_collapse"))),
            "展开态应给出折叠提示：{tail:?}"
        );
    }

    #[test]
    fn in_progress_todo_panel_still_renders_separator_and_items() {
        // 上一条的对照：未全部完成时行为完全不变（仍有分隔线与条目）。
        let mut state = make_state();
        state.current_todos = Some(vec![
            wyj_tools::todo::TodoItem {
                id: "a".to_string(),
                content: "task a".to_string(),
                status: wyj_tools::todo::TodoStatus::Completed,
                priority: None,
                active_form: None,
            },
            wyj_tools::todo::TodoItem {
                id: "b".to_string(),
                content: "task b".to_string(),
                status: wyj_tools::todo::TodoStatus::InProgress,
                priority: None,
                active_form: None,
            },
        ]);

        let rendered: Vec<String> = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        let head = rendered
            .iter()
            .position(|l| l.contains(&wyj_i18n::tr("spinner.todo_title")))
            .expect("面板未渲染");
        let tail = &rendered[head..];
        assert!(
            tail.iter().any(|l| l.contains('─')),
            "未完成时保留分隔线：{tail:?}"
        );
        assert!(
            tail.iter().any(|l| l.contains("task b")),
            "未完成时列出条目：{tail:?}"
        );
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
        state.ui_focus = UiFocus::TodoDetail;
        state.todo_detail_open = true;
        state.todo_detail_scroll = 0;
        // 未渲染前先塞一个陈旧的非零值，验证渲染层确实会重新计算并回写。
        state.todo_detail_max_scroll = 99;

        let rendered = build_pending_chat_lines(&mut state, 100)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(state.todo_detail_max_scroll, 0);
        assert!(rendered
            .iter()
            .any(|line| line.contains("task: short task")));
        assert!(rendered.iter().any(|line| line.contains("PageUp/PageDown")));
    }

    fn running_sub_agent_with_tool_log() -> SubAgentUiState {
        let mut sub = sub_agent(SubAgentStatus::Running);
        sub.current_tool = Some("WebFetch(url)".to_string());
        sub.tool_log = vec![SubToolLine {
            tool_name: "Grep".to_string(),
            arg_summary: "pattern=needle".to_string(),
            is_error: false,
            elapsed_secs: Some(0.1),
        }];
        sub
    }

    /// 回归钉子：选中子 Agent 后**内容区**必须换成它的执行内容。
    ///
    /// 此前执行内容只画在底部面板的详情带里，而 `bottom_panel_size` 在
    /// `ui_focus == SubAgents` 分支压根不给详情预留行数，`draw_sub_agents_panel`
    /// 的 `inner.height > list_rows` 恒为 false —— 选中后什么也看不到。
    #[test]
    fn selected_sub_agent_replaces_content_area_with_its_execution() {
        let mut state = make_state();
        state
            .sub_agents
            .insert(1, running_sub_agent_with_tool_log());
        state.selected_sub_agent = Some(1);
        state.sub_agent_detail_open = true;
        state.ui_focus = UiFocus::SubAgents;

        let text = render_to_text(|f| draw_sub_agent_content(f, &mut state, f.area()), 100, 20);

        assert!(
            text.contains("test task"),
            "内容区应标出该子 Agent：\n{text}"
        );
        assert!(
            text.contains("Grep(pattern=needle)"),
            "内容区应列出工具流水：\n{text}"
        );
        assert!(
            text.contains("WebFetch(url)"),
            "内容区应显示当前动作：\n{text}"
        );
    }

    /// 焦点离开面板后内容区必须切回聊天流，否则会话正文会被子 Agent 视图永久顶掉。
    #[test]
    fn chat_area_returns_when_sub_agent_panel_loses_focus() {
        let mut state = make_state();
        state
            .sub_agents
            .insert(1, running_sub_agent_with_tool_log());
        let mut msg = ChatMessage::system("hello from the main conversation".to_string());
        msg.role = MessageRole::User;
        state.messages.push(msg);
        state.chat_follow_tail = true;

        // 复现 `leave_content_focus` 的收尾动作：焦点回输入框、选中清空。
        state.ui_focus = UiFocus::Composer;
        state.selected_sub_agent = None;
        state.sub_agent_detail_open = false;

        let text = render_to_text(|f| draw_chat(f, &mut state, f.area()), 100, 20);

        assert!(
            text.contains("hello from the main conversation"),
            "退出面板后聊天流应重新出现：\n{text}"
        );
    }

    /// 面板高度只覆盖列表：详情由内容区承担，`bottom_panel_size` 不再为它预留行数。
    #[test]
    fn focused_sub_agent_panel_reserves_rows_for_list_only() {
        let mut state = make_state();
        state
            .sub_agents
            .insert(1, running_sub_agent_with_tool_log());
        state.selected_sub_agent = Some(1);
        state.sub_agent_detail_open = true;
        state.ui_focus = UiFocus::SubAgents;

        let (height, panel) = bottom_panel_size(&state, 40);

        assert!(matches!(panel, BottomPanel::SubAgents));
        assert_eq!(height, 3, "1 条运行中列表 + 上下边框；详情不再占行");
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
    fn thinking_status_label_prefers_todo_then_tool_activity_then_dialogs() {
        let mut state = make_state();
        // 工具在跑，同时有 in-progress todo：标题栏答"为哪件事"而不是复述工具行
        // —— 聊天区的 `⏺ Read(...)` 已经把动作讲清楚了，两处说同一句话等于没有
        // 第二处信息。这是「spinner 与输出区最后一条一模一样」的正面钉子。
        state.current_op = Some("Read(crates/tui/src/render.rs)".to_string());
        state.current_tool_name = Some("Read".to_string());
        state.current_todos = Some(vec![wyj_tools::todo::TodoItem {
            id: "a".to_string(),
            content: "检查交互焦点".to_string(),
            status: TodoStatus::InProgress,
            priority: Some("high".to_string()),
            active_form: Some("正在检查交互焦点".to_string()),
        }]);
        assert_eq!(thinking_status_label(&state), "正在检查交互焦点");

        // 没有 todo 时退到**动词短语**：既不是参数（那是聊天区的事），也不是
        // 工具注册名（`Read` 答不了"在做什么"）。
        state.current_todos = None;
        assert_eq!(
            thinking_status_label(&state),
            wyj_i18n::tr("tool_activity.read")
        );
        assert!(
            !thinking_status_label(&state).contains("render.rs"),
            "标题栏不得重复聊天区工具行里的参数"
        );

        state.current_todos = Some(vec![wyj_tools::todo::TodoItem {
            id: "b".to_string(),
            content: "检查 fallback".to_string(),
            status: TodoStatus::InProgress,
            priority: None,
            active_form: None,
        }]);
        assert_eq!(thinking_status_label(&state), "检查 fallback");

        // 非 InProgress 状态的 todo 不参与优先级；工具名之后逐级兜底。
        state.current_todos = Some(vec![wyj_tools::todo::TodoItem {
            id: "c".to_string(),
            content: "已完成".to_string(),
            status: TodoStatus::Completed,
            priority: None,
            active_form: Some("不应被选中".to_string()),
        }]);
        assert_eq!(
            thinking_status_label(&state),
            wyj_i18n::tr("tool_activity.read")
        );

        // 工具也跑完了：没有 current_tool_name / current_op 时落到 "Thinking"。
        state.current_tool_name = None;
        state.current_op = None;
        assert_eq!(thinking_status_label(&state), "Thinking");
    }

    #[test]
    fn thinking_status_label_falls_back_to_thinking() {
        let mut state = make_state();
        // 无 todo / current_op / permission / plan / queue 时返回静态 "Thinking"
        assert_eq!(thinking_status_label(&state), "Thinking");

        // turn_start_time / spinner_frame 都不影响 label（label 与时间/动画无关）
        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_secs(60));
        state.spinner_frame = 9;
        assert_eq!(thinking_status_label(&state), "Thinking");
    }

    #[test]
    fn title_token_text_shows_this_turn_only_not_session_totals() {
        let mut state = make_state();
        // session 累计已经很大（历史 + 上一轮），标题栏只显示本轮的实时累加值。
        state.total_input_tokens = 12_345;
        state.total_output_tokens = 678;
        state.turn_live_input_tokens = 345;
        state.turn_live_output_tokens = 178;

        assert_eq!(title_token_text(&state), "345↑ 178↓");
    }

    #[test]
    fn title_token_text_ignores_session_usage_that_only_lands_at_turn_end() {
        // 回归钉子：`AgentEvent::Usage` 在回合进行中**不会**推任何值，session 累计
        // 停在回合开始那一刻。若标题栏改回读 `total_*`，这里会从 "0↑ 0↓" 变成
        // 把上一轮的量显示成本轮开销。
        let mut state = make_state();
        state.total_input_tokens = 99_000;
        state.total_output_tokens = 4_000;
        assert_eq!(title_token_text(&state), "0↑ 0↓");
    }

    #[test]
    fn subagent_inline_running_does_not_double_the_seconds_suffix() {
        // 回归钉子：`format_hms` 返回值**自带** `s`（"38s" / "1m 5s"），模板里
        // 再拼一个 `{elapsed}s` 就会渲染成 "38ss"。秒级用例最容易被当成
        // 笔误放过，60s 以上更是变成 "1m 5ss"——所以两级都要断言。
        for (secs, needle) in [(38.0_f64, "38s"), (65.0, "1m 5s")] {
            let elapsed = format_hms(secs);
            let rendered = wyj_i18n::tr_fmt(
                "subagent.inline_running",
                &[
                    ("elapsed", elapsed.as_str()),
                    ("tokens", "1.2k"),
                    ("count", "31"),
                ],
            );
            assert!(
                rendered.contains(needle),
                "耗时 {elapsed} 应原样出现在状态行里，实际：{rendered}"
            );
            assert!(!rendered.contains("ss"), "耗时单位重复成 `ss`：{rendered}");
        }
    }

    #[test]
    fn current_step_elapsed_falls_back_to_turn_when_no_tool_running() {
        // 纯思考阶段没有 ToolStart，只有 turn_start_time：耗时必须仍然有值，
        // 不能退化成常驻 0.0s（那会让用户以为界面卡死）。
        let mut state = make_state();
        state.current_op = None;
        state.current_op_started = None;
        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_secs(7));
        assert!(current_step_elapsed_secs(&state) >= 6.9);

        // 工具在跑时用步骤起点，与整轮耗时无关。
        state.current_op = Some("Bash(cargo test)".to_string());
        state.current_op_started = Some(Instant::now() - std::time::Duration::from_millis(1500));
        let step = current_step_elapsed_secs(&state);
        assert!(
            (1.4..1.6).contains(&step),
            "步骤耗时应约 1.5s 而不是整轮的 7s，实际 {step}"
        );
    }

    #[test]
    fn title_stream_rate_text_hides_when_idle_and_formats_by_magnitude() {
        // 空闲态（工具执行 / 回合间隙）不得显示速率：`StreamRateMeter` 的
        // 空样本与 stale 判定都返回 None，标题栏必须整段不出现。
        let state = make_state();
        assert_eq!(title_stream_rate_text(&state), "");

        // 低速保留一位小数（3.5 与 4 对弱模型是两种体验），高速取整。
        let mut state = make_state();
        state.stream_rate.samples.push_back((Instant::now(), 7.0));
        state.stream_rate.burst_started = Some(Instant::now() - std::time::Duration::from_secs(2));
        state.stream_rate.last_delta_at = Some(Instant::now());
        let text = title_stream_rate_text(&state);
        assert!(text.contains("3.5") && text.contains("tok/s"), "{text:?}");

        let mut state = make_state();
        state.stream_rate.samples.push_back((Instant::now(), 84.0));
        state.stream_rate.burst_started = Some(Instant::now() - std::time::Duration::from_secs(2));
        state.stream_rate.last_delta_at = Some(Instant::now());
        let text = title_stream_rate_text(&state);
        assert!(text.contains("42") && !text.contains('.'), "{text:?}");
    }

    #[test]
    fn thinking_status_label_prefers_tool_activity_over_op_arguments() {
        // 回归钉子：标题栏曾直接回显 `current_op`（`"Bash(cargo test -p wyj-core)"`），
        // 而聊天区最后一条恰好也是这个字符串 —— 用户看到的就是"底部把上面那条
        // 又抄了一遍"，动的是同一行文字，看起来像界面卡住。
        let mut state = make_state();
        state.current_op = Some("Bash(cargo test -p wyj-core)".to_string());
        state.current_tool_name = Some("Bash".to_string());
        assert_eq!(
            thinking_status_label(&state),
            wyj_i18n::tr("tool_activity.bash")
        );
        assert!(!thinking_status_label(&state).contains("cargo test"));
        assert_ne!(
            thinking_status_label(&state),
            "Bash",
            "标题栏不该把工具注册名端给用户：`Bash` 答不了「在做什么」"
        );

        // 非工具型后台任务没有 current_tool_name，只有 current_op —— 那是它的
        // 全部标签（`/compact`），聊天区没有对应行，照原样显示。
        state.current_tool_name = None;
        state.current_op = Some("/compact".to_string());
        assert_eq!(thinking_status_label(&state), "/compact");

        // 没有在跑的东西时才逐级回落到 todo / 对话框 / 排队提示。
        state.current_op = None;
        state.permission_dialog = None;
        state.plan_dialog = None;
        state.pending_queue.clear();
        assert_eq!(thinking_status_label(&state), "Thinking");
    }

    #[test]
    fn tool_activity_label_covers_every_registered_tool_and_falls_back_to_name() {
        // 标题栏的动词短语必须覆盖当前全部内置工具——漏一个就等于那个工具在跑时
        // 底部又退回显示注册名（`Bash` 那种读不懂的形态）。清单与
        // `crates/tools` 里各 Tool::name() 以及 core 的 ToolSearch/CodeSearch 同源。
        for name in [
            "Read",
            "Write",
            "Edit",
            "Bash",
            "Glob",
            "Grep",
            "WebFetch",
            "WebSearch",
            "Agent",
            "TodoWrite",
            "AskQuestion",
            "ExitPlanMode",
            "ToolSearch",
            "CodeSearch",
            "Memory",
            "BashOutput",
            "KillShell",
            "ContextRecall",
            "computer",
            "app_computer",
        ] {
            let label = tool_activity_label(name);
            assert_ne!(label, name, "{name} 没有对应的动词短语");
            assert!(
                !label.starts_with("tool_activity."),
                "{name} 的 i18n key 未解析：{label:?}"
            );
        }
        // 未收录的工具回落注册名本身：新增工具不该因为漏配一条 i18n 显示成空白。
        assert_eq!(tool_activity_label("BrandNewTool"), "BrandNewTool");
    }

    #[test]
    fn thinking_status_label_prefers_dialogs_over_current_op() {
        // 回归钉子：`Agent::exec_tool_call` 是先 `emit ToolEvent::Start`、**之后**
        // 才 `ensure_tool_permission`（见 crates/core/src/agent.rs）。所以权限
        // 弹窗打开期间 `current_op` 必然非空——旧顺序下标题栏永远停在
        // `Bash(cargo test) · 12.3s` 且秒表继续走，看着在忙实则卡在等用户点 y。
        let mut state = make_state();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.current_op = Some("Bash(cargo test)".to_string());
        state.permission_dialog = Some(PermissionDialog {
            tool_name: "Bash".to_string(),
            action_summary: "cargo test".to_string(),
            response_tx: tx,
        });
        assert_eq!(
            thinking_status_label(&state),
            wyj_i18n::tr("spinner.waiting_approval"),
            "等审批时必须显示'等用户'而不是工具名，否则秒表把等待时间算成了命令耗时"
        );
        assert!(!thinking_status_label(&state).contains("cargo test"));

        // 计划审批同构：ExitPlanMode 也先 Start 再弹窗。
        state.permission_dialog = None;
        let (plan_tx, _plan_rx) = tokio::sync::oneshot::channel();
        state.plan_dialog = Some(PlanApprovalDialog::new(plan_tx));
        assert_eq!(
            thinking_status_label(&state),
            wyj_i18n::tr("spinner.reviewing_plan")
        );
    }

    #[test]
    fn build_thinking_title_drops_queue_only_as_last_resort() {
        // 回归钉子：旧实现先 format! 拼整串再 truncate_line（从尾部截），而排队
        // 提示恰好拼在最后 —— 窄屏下第一个消失的必然是「我的补充还没送达」。
        // 现在按整段丢弃 + 优先级：token 先丢，耗时次之，排队提示最后。
        let queue = "2 queued (Bash(sleep 540) running)";
        let rate = "42 tok/s";
        let wide = build_thinking_title(
            '⠋',
            "Bash(cargo test)",
            queue,
            rate,
            "1.5s",
            "1,000↑ 40↓",
            200,
        );
        assert!(
            wide.contains(queue)
                && wide.contains("1.5s")
                && wide.contains(rate)
                && wide.contains("1,000↑")
        );
        // 速率紧跟耗时、排在 token 之前（时效信息聚在一起，累计量放最后）
        let rate_pos = wide.find(rate).expect("应含速率");
        let elapsed_pos = wide.find("1.5s").expect("应含耗时");
        let tokens_pos = wide.find("1,000↑").expect("应含 token");
        assert!(
            elapsed_pos < rate_pos && rate_pos < tokens_pos,
            "顺序应为 耗时 → 速率 → token：{wide:?}"
        );

        // 逐级丢段的预算阈值（全量 86 列）：丢 token → 74 → 丢耗时 → 67
        // → 丢速率 → 56 → 丢排队提示 → 20
        let mid = build_thinking_title(
            '⠋',
            "Bash(cargo test)",
            queue,
            rate,
            "1.5s",
            "1,000↑ 40↓",
            80,
        );
        assert!(!mid.contains("1,000↑"), "token 应先被丢：{mid:?}");
        assert!(mid.contains(queue), "排队提示必须留下：{mid:?}");
        assert!(mid.contains("1.5s"), "耗时仍应保留：{mid:?}");
        assert!(mid.contains(rate), "速率仍应保留：{mid:?}");

        let tight = build_thinking_title(
            '⠋',
            "Bash(cargo test)",
            queue,
            rate,
            "1.5s",
            "1,000↑ 40↓",
            70,
        );
        assert!(
            !tight.contains("1.5s"),
            "耗时应在排队提示之前被丢：{tight:?}"
        );
        assert!(tight.contains(rate), "速率比耗时更该留下：{tight:?}");
        assert!(tight.contains(queue), "排队提示是最后可丢的段：{tight:?}");

        // 再窄一档轮到速率，且它消失后耗时也不回来（不是"轮换显示"）
        let narrower = build_thinking_title(
            '⠋',
            "Bash(cargo test)",
            queue,
            rate,
            "1.5s",
            "1,000↑ 40↓",
            60,
        );
        assert!(
            !narrower.contains(rate),
            "速率的保留优先级低于排队提示：{narrower:?}"
        );
        assert!(
            narrower.contains(queue),
            "排队提示是最后可丢的段：{narrower:?}"
        );

        // 最后一档才轮到排队提示
        let tiny = build_thinking_title(
            '⠋',
            "Bash(cargo test)",
            queue,
            rate,
            "1.5s",
            "1,000↑ 40↓",
            50,
        );
        assert!(
            !tiny.contains(queue) && !tiny.contains(rate) && !tiny.contains("1.5s"),
            "{tiny:?}"
        );
    }

    #[test]
    fn build_thinking_title_never_exceeds_budget_and_keeps_label_readable() {
        // 全部可丢段都丢光后仍超宽时，只对 label 做省略号截断（它不可丢弃），
        // 且任何预算下都不得溢出——溢出会在 Block::title 上直接画到边框外。
        for budget in [8usize, 12, 20, 40, 80] {
            let title = build_thinking_title(
                '⠋',
                "Bash(cargo test -p wyj-core --all-features)",
                "3 queued",
                "42 tok/s",
                "1m 5s",
                "12,345↑ 678↓",
                budget,
            );
            let w = title.chars().map(char_display_width).sum::<usize>();
            assert!(w <= budget, "预算 {budget} 下溢出到 {w} 列：{title:?}");
        }
        // 极窄预算下 label 至少留下 spinner 帧本身，不整段消失。
        let tiny = build_thinking_title('⠋', "Bash(cargo test)", "", "", "1.5s", "1,000↑", 6);
        assert!(tiny.contains('⠋'), "极窄下仍应保留 spinner 帧：{tiny:?}");
    }

    #[test]
    fn build_thinking_title_without_queue_matches_legacy_layout() {
        // 无排队、无速率时的排版与旧实现逐字符一致（回归保护：底部进度条的
        // 形态是用户长期习惯的，改版不该顺手改掉）。速率只在真正生成中才出现，
        // 空串必须整段不占位——否则每个工具执行阶段都会多出两个分隔符。
        assert_eq!(
            build_thinking_title('⠋', "Read(src/app.rs)", "", "", "3.2s", "1,024↑ 40↓", 200),
            " ⠋ Read(src/app.rs) · 3.2s · 1,024↑ 40↓ "
        );
        assert_eq!(
            build_thinking_title(
                '⠋',
                "Read(src/app.rs)",
                "",
                "42 tok/s",
                "3.2s",
                "1,024↑ 40↓",
                200
            ),
            " ⠋ Read(src/app.rs) · 3.2s · 42 tok/s · 1,024↑ 40↓ "
        );
    }

    #[test]
    fn thinking_running_line_uses_live_spinner_frame_not_baked_first_frame() {
        // 回归钉子：`thinking.running` 的文案里曾把 `⠋` 写死，于是聊天流里的
        // 思考状态行在流式期间永远停在第一帧——往上滚看历史时整屏只有标题栏
        // 在动，看起来像一行静态文字。帧号必须由渲染层注入。
        let render_at = |frame: usize| {
            let mut lines: Vec<Line> = Vec::new();
            render_thinking_block(
                &mut lines,
                "",
                80,
                ThinkingRenderOpts {
                    live: true,
                    expanded: false,
                    elapsed_secs: None,
                    spinner_frame: frame,
                    cache: None,
                },
            );
            rendered_text(lines).join("\n")
        };
        let first = render_at(0);
        let later = render_at(4);
        assert!(
            first.contains(spinner_frame_str(0).as_str()),
            "第 0 帧应是 ⠋，实际 {first:?}"
        );
        assert!(
            later.contains(spinner_frame_str(4).as_str()),
            "第 4 帧应是 ⠼，实际 {later:?}"
        );
        assert_ne!(first, later, "帧号没生效：thinking 状态行不随 spinner 推进");
    }

    #[test]
    fn spinner_text_keys_resolve_with_their_placeholders() {
        // 底部进度区的文案必须全部走 i18n：一旦某条 key 拼错，rust-i18n 会静默
        // 回退成 "spinner.xxx" 字面量，正好让"漏翻译"以最难发现的形态出现。
        //
        // 只在**当前 locale** 下断言：本文件里的其他测试并行跑，任何一个调
        // `wyj_i18n::set_locale` 都会与它们串味（见 wyj-i18n 里的同款说明）。
        // zh 侧的对照断言放在 `wyj-i18n` 的单测里。
        for key in [
            "spinner.thinking",
            "spinner.waiting_approval",
            "spinner.reviewing_plan",
            "spinner.queuing_message",
            "spinner.stream_rate",
            "spinner.queue_pending",
            "spinner.todo_title",
            "spinner.todo_done",
            "spinner.todo_expand",
            "spinner.todo_collapse",
        ] {
            let v = wyj_i18n::tr(key);
            assert!(
                !v.is_empty() && !v.starts_with("spinner."),
                "{key} 未解析：{v:?}"
            );
        }
        let hint = wyj_i18n::tr_fmt(
            "spinner.queue_hint",
            &[
                ("count", "2"),
                ("running", wyj_i18n::tr("spinner.queue_pending").as_str()),
            ],
        );
        assert!(hint.contains('2'), "queue_hint 丢了 {{count}}：{hint:?}");
        let op = wyj_i18n::tr_fmt("spinner.queue_op_running", &[("op", "Bash(x)")]);
        assert!(
            op.contains("Bash(x)"),
            "queue_op_running 丢了 {{op}}：{op:?}"
        );
        let rate = wyj_i18n::tr_fmt("spinner.stream_rate", &[("rate", "42")]);
        assert!(
            rate.contains("42") && rate.contains("tok/s"),
            "stream_rate 丢了 {{rate}} 或单位：{rate:?}"
        );
        let running = wyj_i18n::tr_fmt(
            "thinking.running",
            &[("frame", spinner_frame_str(2).as_str())],
        );
        assert!(
            running.contains('⠹'),
            "thinking.running 丢了 {{frame}}：{running:?}"
        );
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
        let mut call = message(MessageRole::ToolCall, "Read(crates/tui/src/render.rs)");
        call.sequence_no = Some(1);
        call.tool_name = Some("Read".to_string());
        let mut result = message(MessageRole::ToolResult, "1\tfirst\n2\tsecond\n3\tthird");
        result.id = 2;
        result.elapsed_secs = Some(0.2);
        result.sequence_no = Some(1);
        result.tool_name = Some("Read".to_string());
        result.display_summary = "read 3 lines".to_string();
        let messages = vec![call, result];

        // 折叠态：只剩标题行，正文一个字都不渲染
        let rendered = render_messages(&messages, 100);
        assert_eq!(rendered.len(), 1, "{rendered:?}");
        assert!(rendered[0].contains("⏺ Read("));
        assert!(
            !rendered.iter().any(|line| line.contains("first")),
            "折叠态不得泄露任何正文: {rendered:?}"
        );
        // 折叠头不加任何元信息——这条断言现在才真正承重（需求明确要求"只留标题行"）
        assert!(rendered
            .iter()
            .all(|line| !line.contains("ctrl+o") && !line.contains('▶')));

        // 展开态：全文出齐
        let expanded = render_messages_with_output_expanded(&messages, 100, true);
        assert_eq!(expanded.len(), 4, "{expanded:?}");
        assert_eq!(expanded[1], "    ⎿ first");
        assert_eq!(expanded[2], "       second");
        assert_eq!(expanded[3], "       third");
    }

    #[test]
    fn ask_question_result_keeps_blank_separator_lines_when_expanded() {
        let mut call = message(MessageRole::ToolCall, "AskQuestion");
        call.sequence_no = Some(1);
        call.tool_name = Some("AskQuestion".to_string());
        let mut result = message(
            MessageRole::ToolResult,
            "访谈已完成，用户作答如下：\n\nQ1（粒度）: 扫描后如何处理？\n→ 用户选择: 立即执行\n\nQ2: 是否继续？",
        );
        result.id = 2;
        result.sequence_no = Some(1);
        result.tool_name = Some("AskQuestion".to_string());

        // 展开态：空行是访谈问答的段落分隔，必须保留（旧实现会把它当"浪费名额"丢掉），
        // 且不再有 `...` 截断
        let rendered = render_messages_with_output_expanded(&[call, result], 100, true);

        assert!(rendered[0].contains("AskQuestion"));
        assert_eq!(rendered[1], "    ⎿ 访谈已完成，用户作答如下：");
        assert_eq!(rendered[2], "", "空行应作为真正的空行保留");
        assert_eq!(rendered[3], "       Q1（粒度）: 扫描后如何处理？");
        assert_eq!(rendered[4], "       → 用户选择: 立即执行");
        assert_eq!(rendered[5], "", "Q1 与 Q2 之间的分隔空行同样保留");
        assert_eq!(rendered[6], "       Q2: 是否继续？");
        assert!(
            !rendered.iter().any(|line| line.trim() == "..."),
            "展开态不得再补省略号: {rendered:?}"
        );
    }

    #[test]
    fn message_blocks_have_one_blank_line_between_them() {
        let mut second = message(MessageRole::Assistant, "second response");
        second.id = 2;
        let messages = vec![message(MessageRole::Assistant, "first response"), second];
        let rendered = render_messages(&messages, 100);

        assert_eq!(rendered, vec!["  first response", "", "  second response"]);
    }

    #[test]
    fn completed_tool_call_pair_stays_compact_before_next_block_separator() {
        let mut call = message(MessageRole::ToolCall, "Read(crates/tui/src/render.rs)");
        call.sequence_no = Some(1);
        let mut result = message(MessageRole::ToolResult, "1\tfirst\n2\tsecond");
        result.id = 2;
        result.elapsed_secs = Some(0.2);
        result.sequence_no = Some(1);
        result.tool_name = Some("Read".to_string());
        let mut next = message(MessageRole::Assistant, "after tool");
        next.id = 3;
        let messages = vec![call, result, next];
        let rendered = render_messages(&messages, 100);

        // 折叠态：工具块塌成一行标题，但块间空行分隔符语义与折叠正交、必须保留
        assert_eq!(rendered.len(), 3, "{rendered:?}");
        assert!(rendered[0].contains("Read("));
        assert!(rendered[1].trim().is_empty());
        assert_eq!(rendered[2], "  after tool");

        // 展开态：完整块 + 分隔符
        let expanded = render_messages_with_output_expanded(&messages, 100, true);
        assert_eq!(expanded.len(), 5, "{expanded:?}");
        assert!(expanded[0].contains("Read("));
        assert_eq!(expanded[1], "    ⎿ first");
        assert_eq!(expanded[2], "       second");
        assert!(expanded[3].trim().is_empty());
        assert_eq!(expanded[4], "  after tool");
    }

    #[test]
    fn thinking_is_folded_to_a_single_metadata_line_by_default() {
        let mut thinking = message(
            MessageRole::Thinking,
            "\n\none\n\ntwo\nthree\n\nfour\nfive\nsix\nseven\n",
        );
        thinking.thinking_elapsed_secs = Some(1.5);
        let rendered = render_messages(&[thinking], 100);

        // 折叠态只出一行头（耗时 + 行数 + 展开提示），正文一律不渲染
        assert_eq!(rendered.len(), 1, "{rendered:?}");
        assert!(rendered[0].contains("1.5"), "{}", rendered[0]);
        assert!(rendered[0].contains('7'), "{}", rendered[0]);
        assert!(rendered[0].contains("Ctrl+O"), "{}", rendered[0]);
        assert!(
            !rendered[0].contains('✻'),
            "U+273B(Dingbats) 在终端字体里常缺字形画成豆腐块，改用 ⎿: {}",
            rendered[0]
        );
    }

    #[test]
    fn thinking_expanded_renders_full_body_not_just_first_three_lines() {
        let mut thinking = message(
            MessageRole::Thinking,
            "one\ntwo\nthree\nfour\nfive\nsix\nseven",
        );
        thinking.thinking_elapsed_secs = Some(12.0);
        let rendered = render_messages_with_output_expanded(&[thinking], 100, true);

        // 展开后出全文（旧实现固定只给前 3 行，几十行思考的结论永远看不到）
        assert_eq!(rendered.len(), 8, "{rendered:?}");
        assert!(rendered[0].contains("12.0"), "{}", rendered[0]);
        assert!(
            !rendered[0].contains("Ctrl+O"),
            "已展开时不该再提示如何展开: {}",
            rendered[0]
        );
        assert_eq!(rendered[7], "    seven");
    }

    #[test]
    fn thinking_expanded_renders_markdown_structure_instead_of_literal_markers() {
        // 真实 thinking 里约 15% 的行带 `- ` / `1.` / `|` / 围栏。原实现逐行
        // 纯文本输出，这些标记原样打在屏幕上，整块看起来像乱码。
        let body = "\
关键点：
1. 第一点
2. 第二点

- **粗体条目**
- 普通条目

```rust
fn foo() {}
```

| 列一 | 列二 |
|------|------|
| a    | b    |";
        let mut thinking = message(MessageRole::Thinking, body);
        thinking.thinking_elapsed_secs = Some(3.0);
        let rendered = render_messages_with_output_expanded(&[thinking], 70, true);
        let all = rendered.join("\n");

        // 有序列表：`1. ` 变成渲染出的序号，而不是原样保留
        assert!(all.contains("1. 第一点"), "{all}");
        // 无序列表：`- ` 变成 bullet
        assert!(all.contains("• "), "{all}");
        assert!(!all.contains("- **"), "列表标记应被解析：{all}");
        // 围栏：thinking 里退化成 `│` 引用式，不画带行号的方框
        assert!(all.contains("│ fn foo() {}"), "{all}");
        assert!(!all.contains("╭─"), "thinking 的围栏不该画方框：{all}");
        // 表格：走网格而不是原样 `| a | b |`
        assert!(!all.contains("|------|"), "表格分隔行应被消费：{all}");
        assert!(all.contains("┌") || all.contains("│ 列一"), "{all}");
        // 粗体标记不落地
        assert!(!all.contains("**"), "粗体标记应被消费：{all}");
    }

    #[test]
    fn thinking_keeps_intra_paragraph_line_breaks() {
        // 模型的思考是一行一个念头。正文那条「段内换行折叠成空格」的规则
        // 会把它们揉成一整段，层次全没了。
        let mut thinking = message(MessageRole::Thinking, "第一行\n第二行\n第三行");
        thinking.thinking_elapsed_secs = Some(1.0);
        let rendered = render_messages_with_output_expanded(&[thinking], 60, true);

        assert_eq!(rendered.len(), 4, "{rendered:?}");
        assert!(rendered[1].ends_with("第一行"), "{}", rendered[1]);
        assert!(rendered[2].ends_with("第二行"), "{}", rendered[2]);
        assert!(rendered[3].ends_with("第三行"), "{}", rendered[3]);
    }

    #[test]
    fn thinking_body_is_indented_exactly_four_columns() {
        // 外层缩进只补一层：markdown 内部对顶层段落不再补 2 格（否则叠加成
        // 6 格，比列表 bullet 还靠右，层级关系反了）。
        let mut thinking = message(MessageRole::Thinking, "普通段落");
        thinking.thinking_elapsed_secs = Some(1.0);
        let rendered = render_messages_with_output_expanded(&[thinking], 60, true);

        assert_eq!(rendered[1], "    普通段落", "{rendered:?}");
    }

    #[test]
    fn thinking_body_render_is_cached_by_width() {
        // 展开态走完整 markdown 解析，历史里几十段 thinking 每帧重解析就是
        // 与 assistant 消息同一类的浪费。缓存按宽度失效。
        let mut thinking = message(MessageRole::Thinking, "第一段\n\n第二段");
        thinking.thinking_elapsed_secs = Some(1.0);
        let first = render_messages_with_output_expanded(std::slice::from_ref(&thinking), 60, true);
        assert!(
            thinking.render_cache.borrow().is_some(),
            "首次渲染应写入缓存"
        );

        let width = thinking.render_cache.borrow().as_ref().unwrap().0;
        let second = render_messages_with_output_expanded(&[thinking], 60, true);
        assert_eq!(first, second);
        assert_eq!(width, 60 - 4, "缓存键应是正文可用宽度");
    }

    #[test]
    fn live_thinking_tail_is_formatted_too() {
        // 流式期间模型已经在写列表，逐行原样输出会让这几行先"乱"一遍再变整齐。
        let mut lines = vec![];
        render_thinking_block(
            &mut lines,
            "先想想\n\n1. 第一步\n2. 第二步",
            80,
            ThinkingRenderOpts {
                live: true,
                expanded: false,
                elapsed_secs: None,
                spinner_frame: 0,
                cache: None,
            },
        );
        let text = rendered_text(lines).join("\n");
        assert!(text.contains("1. 第一步"), "{text}");
        assert!(!text.contains("1. 第一步\n2."), "不应原样堆叠：{text}");
    }

    #[test]
    fn live_thinking_tail_closes_a_fence_cut_by_the_tail_window() {
        // 取尾可能只留下围栏的闭合行。markdown 会把它当成新的开启行，把
        // 后面所有正常思考吞进代码块——补一个开启行配平。
        let body = format!(
            "```\n{}\n```\n这行必须是普通思考，不能被吞进代码块",
            (1..=LIVE_TAIL_LINES)
                .map(|i| format!("code-{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut lines = vec![];
        render_thinking_block(
            &mut lines,
            &body,
            80,
            ThinkingRenderOpts {
                live: true,
                expanded: false,
                elapsed_secs: None,
                spinner_frame: 0,
                cache: None,
            },
        );
        let text = rendered_text(lines);
        // 末行是「上面还有 N 行」提示；正文最后一行的普通思考必须是独立一行。
        let body: Vec<&String> = text.iter().filter(|l| !l.contains("more lines")).collect();
        let last = body.last().unwrap();
        assert!(
            last.contains("普通思考"),
            "围栏配平失败，尾部被当成代码块：{text:?}"
        );
        assert!(!last.contains("│"), "尾行不该带代码块左边线：{last}");
    }

    #[test]
    fn thinking_without_elapsed_omits_the_seconds_placeholder() {
        // `/resume` 恢复的旧会话没有耗时记录（该字段从未持久化过），
        // 不能显示一个假的 "0.0s"
        let thinking = message(MessageRole::Thinking, "one\ntwo");
        let rendered = render_messages(&[thinking], 100);

        assert_eq!(rendered.len(), 1, "{rendered:?}");
        assert!(!rendered[0].contains("0.0s"), "{}", rendered[0]);
        assert!(rendered[0].contains('2'), "{}", rendered[0]);
    }

    #[test]
    fn live_thinking_block_shows_tail_only_and_stays_cheap() {
        // 流式期间要把思考显出来，但**只取尾部 LIVE_TAIL_LINES 行**。
        // 这条钉的是性能不变式而非视觉：thinking_buf 能长到几十 KB，若每帧把
        // 全文折行一遍再截断，就回到了「整屏换行比机器慢」的老问题。
        let body = (1..=5_000)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut lines = vec![];
        render_thinking_block(
            &mut lines,
            &body,
            80,
            ThinkingRenderOpts {
                live: true,
                expanded: false,
                elapsed_secs: Some(3.0),
                spinner_frame: 0,
                cache: None,
            },
        );

        let text = rendered_text(lines);
        // 状态行 + 尾窗行 + 1 行截断提示
        assert_eq!(text.len(), LIVE_TAIL_LINES + 2, "{}", text.len());
        // 首行是状态行（文案走 i18n，这里只断言它不含正文）
        assert!(!text[0].contains("line-"), "{}", text[0]);
        // 尾部是最新内容，不是开头
        let last = &text[LIVE_TAIL_LINES];
        assert!(last.contains("line-5000"), "{}", last);
        // 尾窗第一行与被折叠行数都由 LIVE_TAIL_LINES 推出：改尾窗不必手改这两处
        let first_tail = 5_000 - LIVE_TAIL_LINES + 1;
        assert!(
            text[1].contains(&format!("line-{first_tail}")),
            "{}",
            text[1]
        );
        assert!(!text[1].contains("line-1 "), "不应渲染开头内容");
        assert!(
            text[text.len() - 1].contains(&(5_000 - LIVE_TAIL_LINES).to_string()),
            "截断提示应给出被折叠的行数：{}",
            text[text.len() - 1]
        );
    }

    #[test]
    fn live_thinking_block_tolerates_empty_buffer() {
        let mut lines = vec![];
        render_thinking_block(
            &mut lines,
            "",
            80,
            ThinkingRenderOpts {
                live: true,
                expanded: false,
                elapsed_secs: None,
                spinner_frame: 0,
                cache: None,
            },
        );
        assert_eq!(rendered_text(lines).len(), 1, "空缓冲只出状态行");
    }

    #[test]
    fn live_output_survives_the_full_state_to_render_path() {
        // 端到端：state.tool_info × state.live_tool_out 求交集 → 挂到 ⏺ 行下。
        // 单测里手工拼 live_tool_out 的那张表，这里必须走真实构造路径。
        let mut state = make_state();
        state.is_thinking = true;
        let mut call = message(MessageRole::ToolCall, "Bash(cargo build)");
        call.sequence_no = Some(5);
        call.id = 42;
        state.messages.push(call);
        state.tool_info.insert("toolu_x".into(), ("Bash".into(), 5));

        // 快照通道尚未送达：还没有实时输出，⏺ 行下面必须是空的
        let rendered = |state: &mut AppState| -> Vec<String> {
            build_pending_chat_lines(state, 100)
                .into_iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect()
        };
        assert!(
            !rendered(&mut state).iter().any(|l| l.contains("⎿")),
            "无快照时不得凭空多出输出块"
        );

        state.apply_tool_output_chunk(wyj_core::tool::ToolOutputChunk {
            tool_use_id: "toolu_x".into(),
            tail: "Compiling serde v1.0".into(),
        });
        let after = rendered(&mut state);
        assert!(
            after.iter().any(|l| l.contains("Compiling serde")),
            "实时输出应出现在聊天区：{after:?}"
        );

        // 通道里的快照由 tool_info 把门：登记被撤掉（回合中断）后立即消失
        state.tool_info.clear();
        let after_interrupt = rendered(&mut state);
        assert!(
            !after_interrupt
                .iter()
                .any(|l| l.contains("Compiling serde")),
            "调用不再在飞时不得残留实时块：{after_interrupt:?}"
        );
    }

    #[test]
    fn long_single_line_tool_result_wraps_fully_when_expanded() {
        let mut result = message(
            MessageRole::ToolResult,
            "abcdefghijklmnopqrstuvwxyz0123456789",
        );
        result.tool_name = Some("Grep".to_string());

        // 折叠态：连折行都不做，正文一个字符都不碰
        let folded = render_messages(&[result.clone()], 14);
        assert_eq!(folded.len(), 1, "{folded:?}");

        // 展开态：完整折行、无省略号
        let rendered = render_messages_with_output_expanded(&[result], 14, true);
        assert!(
            !rendered.iter().any(|line| line.trim() == "..."),
            "展开态不得补 `...`: {rendered:?}"
        );
        // 剥掉首行的 `    ⎿ ` 与后续行的 7 空格缩进后，拼回必须逐字等于原文
        let joined: String = rendered
            .iter()
            .skip(1) // 首行是标题 `  ⎿ result`
            .map(|line| {
                line.strip_prefix("    ⎿ ")
                    .or_else(|| line.strip_prefix("       "))
                    .unwrap_or(line.as_str())
            })
            .collect();
        assert_eq!(
            joined, "abcdefghijklmnopqrstuvwxyz0123456789",
            "折行拼回必须逐字还原原文"
        );
    }

    #[test]
    fn edit_result_automatically_renders_colored_diff_preview() {
        // 用**真实** `make_diff` 输出而不是手写样本：行号是从 `@@` 块头推出来的，
        // 手写一个无块的 diff 会让测试绿在一个真实链路里根本走不到的分支上。
        let diff = wyj_tools::make_diff("a\nb\nc\n", "a\nB\nc\n", 200);
        let mut result = message(
            MessageRole::ToolResult,
            &format!("已替换 1 处: demo.rs\n{diff}"),
        );
        result.tool_name = Some("Edit".to_string());
        let mut is_first_user = true;
        let lines = render_message_range(
            MessageRangeRenderArgs {
                messages: &[result.clone()],
                range: 0..1,
                max_content_width: 100,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                output_expanded: true,
                live_tool_out: &HashMap::new(),
            },
            &mut is_first_user,
        );
        let rendered = rendered_text(lines.clone());

        // 块头原样透出（它就是行号的来源，也顺带告诉用户改动落在哪）
        assert_eq!(rendered[0], "  ⎿ result");
        assert_eq!(rendered[1], "    @@ -1,3 +1,3 @@");
        // 双列行号：删除行只有旧侧、新增行只有新侧、上下文两侧都有
        assert_eq!(rendered[2], "    1 1 │ a", "{rendered:?}");
        assert_eq!(rendered[3], "    2   │ - b", "{rendered:?}");
        assert_eq!(rendered[4], "      2 │ + B", "{rendered:?}");
        assert_eq!(rendered[5], "    3 3 │ c", "{rendered:?}");
        // 断言槽位而不是具体色值：diff 的绿与 ✓ 的绿必须是**同一个**绿，
        // 否则"新增=好/删除=坏"这套视觉语言会分裂成两套近似色。
        assert_eq!(lines[3].spans[1].style, Theme::error());
        assert_eq!(lines[4].spans[1].style, Theme::success());
        // 上下文行是次级正文而不是 dim：未改动的代码不该看起来像被禁用。
        assert_eq!(lines[5].spans[1].style, Theme::text_dim());
        assert!(
            !rendered.iter().any(|line| line.contains("已替换")),
            "回执头不该出现在 diff 里"
        );

        // 折叠态：diff 仍然内联可见（Edit/Write 是折叠规则的唯一例外），
        // 但渲染的行必须与展开态**逐字节相同**——切换 Ctrl+O 不会让已看惯的
        // 行忽然换一种缩进、换一种颜色，行号更不该凭空消失。
        let folded = render_messages(&[result.clone()], 100);
        assert_eq!(folded, rendered, "{folded:?}");
    }

    #[test]
    fn collapsed_edit_diff_is_truncated_with_an_expand_hint() {
        // 折叠态的截断：长 diff 只出前 N 行 + 一行提示，而不是糊满屏幕。
        let long_diff = {
            let mut m = message(
                MessageRole::ToolResult,
                &format!(
                    "已替换 1 处: src/app.rs\n{}",
                    (1..=60)
                        .map(|i| format!("+ added line {i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            );
            m.tool_name = Some("Edit".to_string());
            m
        };

        let rendered = render_messages(&[long_diff], 100);

        assert_eq!(
            rendered[0], "  ⎿ result",
            "回执行仍被 diff 起点定位跳过：{rendered:?}"
        );
        // 回执前缀不占名额：60 行 diff 全部来自 `+` 行本体
        let diff_lines: Vec<&String> = rendered
            .iter()
            .skip(1)
            .filter(|l| !l.contains("more lines"))
            .collect();
        assert_eq!(
            diff_lines.len(),
            COLLAPSED_DIFF_MAX_LINES,
            "折叠态必须截断到上限: {rendered:?}"
        );
        let hint = rendered.last().expect("截断提示行");
        assert!(
            hint.contains(&format!("{}", 60 - COLLAPSED_DIFF_MAX_LINES)) && hint.contains("Ctrl+O"),
            "截断提示必须报出剩余行数并指明如何展开: {hint}"
        );
    }

    #[test]
    fn short_collapsed_edit_diff_has_no_truncation_hint() {
        // 反向钉子：没超上限就不该出现「还有 0 行」这种占位提示。
        let mut m = message(MessageRole::ToolResult, "已写入: a.rs\n- old\n+ new");
        m.tool_name = Some("Write".to_string());

        let rendered = render_messages(&[m], 100);

        assert_eq!(rendered.len(), 3, "{rendered:?}");
        assert!(
            !rendered.iter().any(|l| l.contains("more lines")),
            "未截断时不得出现截断提示: {rendered:?}"
        );
    }

    #[test]
    fn failed_edit_result_is_not_rendered_as_diff() {
        // 失败回执正文是错误文本而非 diff，不能被当成 diff 染色内联展示。
        let mut m = message(
            MessageRole::ToolResult,
            "安全检查失败：写入 `a.rs` 前必须先用 Read 读取该文件",
        );
        m.tool_name = Some("Edit".to_string());
        m.is_error = true;

        let rendered = render_messages(&[m], 100);

        assert_eq!(rendered.len(), 1, "错误回执不适用 diff 例外: {rendered:?}");
        assert_eq!(rendered[0], "  ⎿ result");
    }

    /// 展开态必须给出**全文**，且行号跨增删两侧各自推进：旧侧 `21` 之后新侧是
    /// `22`，而紧跟的上下文行是 `22/23`（旧侧也 +1）。
    #[test]
    fn long_edit_diff_renders_all_lines_when_expanded() {
        let old = (0..40)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replace("line20", "line20_patched\nanother");
        let diff = wyj_tools::make_diff(&old, &new, 200);
        let mut result = message(
            MessageRole::ToolResult,
            &format!("已替换 1 处: demo.rs\n{diff}"),
        );
        result.tool_name = Some("Edit".to_string());
        let rendered = render_messages_with_output_expanded(&[result], 100, true);

        let patched = rendered
            .iter()
            .find(|line| line.contains("+ another"))
            .unwrap_or_else(|| panic!("尾部补丁行必须可见：{rendered:?}"));
        assert!(patched.contains("22 │"), "末行应带新侧行号：{patched:?}");
        assert!(
            rendered.iter().any(|l| l.contains("24 25 │ line23")),
            "上下文行的双侧行号应各自推进：{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.trim() == "..."),
            "展开态不得补 `...`: {rendered:?}"
        );
    }

    /// 折叠态的 20 行预算必须**只算正文**：多 hunk 的 diff 带一堆 `@@` 头与跳过
    /// 提示，若元信息也占名额，折叠态一屏全是 `@@` 而看不到任何改动内容。
    #[test]
    fn collapsed_diff_budget_counts_body_lines_not_meta_lines() {
        let old = (0..200)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..10).fold(old.clone(), |acc, i| {
            acc.replacen(&format!("line{}", i * 15), &format!("line{}_X", i * 15), 1)
        });
        let diff = wyj_tools::make_diff(&old, &new, 200);
        let mut result = message(
            MessageRole::ToolResult,
            &format!("已替换 10 处: demo.rs\n{diff}"),
        );
        result.tool_name = Some("Edit".to_string());

        let rendered = render_messages(&[result.clone()], 100);
        // 正文行 = 带行号 gutter 的行。`@@` 头与 `… N unchanged lines` 都没有
        // gutter，标题行 `⎿ result` 与截断提示行同样没有——比按文案过滤稳健。
        let body = rendered.iter().filter(|l| l.contains('│')).count();
        assert_eq!(
            body, COLLAPSED_DIFF_MAX_LINES,
            "折叠态应恰好给出 {COLLAPSED_DIFF_MAX_LINES} 行正文：{rendered:?}"
        );

        // 截断提示的数字必须是**剩余正文**数，不是「总行数 - 20」——那会把
        // 元信息行算进「还有多少内容」，提示的数字偏大。期望值按 i18n key 取，
        // 不硬编码文案（测试环境 locale 由外部决定）。
        let all = render_messages_with_output_expanded(&[result], 100, true);
        let body_total = all.iter().filter(|l| l.contains('│')).count();
        let expected = wyj_i18n::tr_fmt(
            "diff.more_lines",
            &[("n", &(body_total - COLLAPSED_DIFF_MAX_LINES).to_string())],
        );
        assert_eq!(
            rendered.last().map(|l| l.as_str()),
            Some(expected.as_str()),
            "截断提示数字应等于剩余正文数 {body_total} - {COLLAPSED_DIFF_MAX_LINES}"
        );
    }

    /// **两条宽度路径必须对同一批字符给出同一个答案**：
    /// `crate::markdown::display_width`（unicode-width）与本文件的
    /// `char_display_width`（手写 CJK 区间表）。
    ///
    /// 分歧的后果不是"某个字符宽度差一点"，而是**宽度算错后右边线裂开**：
    /// 按 A 算可用宽度、按 B 决定在哪折行，两者不一致时必然差若干列。
    /// 框线字符、图标、emoji、中文各自落在哪条路径上，取决于调用点。
    #[test]
    fn the_two_width_paths_agree_on_every_character_we_render() {
        let samples = "abc123 \u{2502}\u{250c}\u{252c}\u{2510}\u{2514}\u{2518}\u{256d}\u{256e}\u{2570}\u{2571}\u{2713}\u{2717}\u{25cb}\u{2699}\u{25cf}\u{25b6}\u{276f}\u{23bf}\u{2500}\u{00b7}\u{2026}\u{4e2d}\u{6f22}\u{5b57}\u{52fe}\u{2713}\u{1f680} \u{1f600} \u{200b} \u{0301} \u{9ad8}";
        let mut mismatches: Vec<String> = Vec::new();
        let mut check = |ch: char| {
            let a = crate::markdown::display_width(&ch.to_string());
            let b = char_display_width(ch);
            if a != b {
                mismatches.push(format!(
                    "U+{:04X} {:?}: unicode={a} table={b}",
                    ch as u32, ch
                ));
            }
        };
        for ch in samples.chars() {
            check(ch);
        }
        // 再按区段全量扫一遍——逐个手列关键字符只能挡住"我知道的"，
        // 区段扫描才能挡住"我没想过会分叉的那些"（零宽字符就是这么漏进来的）。
        for range in [
            0x0300u32..=0x036F,   // 组合附加符号
            0x2000u32..=0x206F,   // 通用标点（含零宽空格 U+200B、ZWSP、ZWJ）
            0x20D0u32..=0x20FF,   // 组合记号
            0xFE00u32..=0xFE0F,   // 变体选择符
            0xE0100u32..=0xE01EF, // 变体选择符补充
            0x2500u32..=0x257F,   // 制表符（框线，全应用渲染量最大的一批）
            0x2580u32..=0x259F,   // 块元素
        ] {
            for cp in range {
                if let Some(ch) = char::from_u32(cp) {
                    check(ch);
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "两条宽度路径在 {} 个字符上不一致：\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }

    /// diff 行**不得超出**聊天区宽度。行号 gutter 是固定开销，折行宽度必须把它
    /// 扣掉——漏扣的后果是长行把右边顶出屏幕（而 TUI 会静默裁掉，用户看到的
    /// 是代码莫名其妙少了一截）。
    #[test]
    fn diff_rows_never_overflow_the_chat_width() {
        let old = (0..30)
            .map(|i| format!("    let value_{i} = compute(\"payload\", {i})?;"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replacen(
            "let value_12 = compute(\"payload\", 12)?;",
            "let value_12 = compute(\"payload_with_a_much_longer_name\", 12)?; // 中文注释",
            1,
        );
        let diff = wyj_tools::make_diff(&old, &new, 200);
        let mut result = message(
            MessageRole::ToolResult,
            &format!("已替换 1 处: demo.rs\n{diff}"),
        );
        result.tool_name = Some("Edit".to_string());

        for width in [40usize, 60, 80, 100, 200] {
            let rendered = render_messages(&[result.clone()], width);
            for line in &rendered {
                let w = crate::markdown::display_width(line);
                assert!(w <= width, "width={width} 时该行宽 {w} 溢出：{line:?}");
            }
        }
    }

    #[test]
    fn error_bash_result_skips_exit_code_banner_in_preview() {
        let mut result = message(
            MessageRole::ToolResult,
            "退出码 1\n{\"error\":{\"code\":\"invalid_request\"}}",
        );
        result.tool_name = Some("Bash".to_string());
        result.is_error = true;
        let rendered = render_messages_with_output_expanded(&[result], 100, true);

        assert_eq!(rendered[0], "  ⎿ result");
        assert_eq!(
            rendered[1], "    ⎿ {\"error\":{\"code\":\"invalid_request\"}}",
            "⎿ 行应直接展示真实错误首行，而不是被「退出码 1」占掉"
        );
    }

    #[test]
    fn error_tool_result_keeps_only_first_line_red() {
        let mut result = message(MessageRole::ToolResult, "退出码 1\nboom\ndetails here");
        result.tool_name = Some("Bash".to_string());
        result.is_error = true;
        let mut is_first_user = true;
        let lines = render_message_range(
            MessageRangeRenderArgs {
                messages: &[result],
                range: 0..1,
                max_content_width: 100,
                sub_agents: &std::collections::BTreeMap::new(),
                spinner_frame: 0,
                output_expanded: true,
                live_tool_out: &HashMap::new(),
            },
            &mut is_first_user,
        );

        assert_eq!(
            lines[1].spans[0].style,
            Theme::error(),
            "首行红色作失败锚点"
        );
        assert_eq!(
            lines[2].spans[0].style,
            Theme::text_dim(),
            "其余行降为次级正文，避免整块刷红"
        );
    }

    #[test]
    fn error_tool_result_renders_every_line_when_expanded() {
        let mut result = message(
            MessageRole::ToolResult,
            "{\"error\":\"bad request\"}\ndetail1\ndetail2\ndetail3\nrequest_id: abc-123",
        );
        result.tool_name = Some("Bash".to_string());
        result.is_error = true;
        let rendered = render_messages_with_output_expanded(&[result], 100, true);

        assert_eq!(rendered[1], "    ⎿ {\"error\":\"bad request\"}");
        assert_eq!(rendered[2], "       detail1");
        assert_eq!(rendered[3], "       detail2");
        assert_eq!(rendered[4], "       detail3");
        assert_eq!(
            rendered[5], "       request_id: abc-123",
            "尾部字段（request_id / reason）必须可见"
        );
        assert_eq!(rendered.len(), 6, "展开态出全文，不应再省略中间行");
        // 旧的 head+tail 逻辑会在中间插 `⋮`。展开已是全文，保留它等于主动删掉
        // 用户明确要求看到的内容
        assert!(
            !rendered.iter().any(|line| line.contains('⋮')),
            "展开态不得再有 `⋮` 中间省略: {rendered:?}"
        );
    }

    #[test]
    fn user_and_assistant_messages_are_not_preview_capped() {
        let user = message(
            MessageRole::User,
            "u1-long-content-that-must-wrap-without-loss\nu2\nu3\nu4\nu5",
        );
        let mut assistant = message(MessageRole::Assistant, "a1\na2\na3\na4\na5");
        assistant.id = 2;
        let rendered = render_messages(&[user, assistant], 18);
        let user_joined = rendered
            .iter()
            .take_while(|line| !line.trim().is_empty())
            .map(|line| line.trim_start_matches(['❯', ' ']))
            .collect::<String>();

        assert!(user_joined.contains("u1-long-content-that-must-wrap-without-loss"));
        assert!(rendered.iter().any(|line| line.contains("u5")));
        assert!(rendered.iter().any(|line| line.contains("a5")));
        // 末条断言现在恒真（User/Assistant 路径本来就不截断，且工具结果的截断
        // 逻辑已整体移除）——保留作无害的兜底，它现在只覆盖 User/Assistant 路径。
        assert!(!rendered.iter().any(|line| line.trim() == "..."));
    }

    /// 折叠态**完全不碰 content**——性能属性的回归钉子。
    ///
    /// `render_tool_result_block` 必须在调 `render_tool_result_preview`（它会把整个
    /// `content` `lines().collect()` 成 Vec）之前 return。旧实现每帧为每条工具结果
    /// 构建完整 preview 再只取 3 行扔掉，长会话下是纯粹的浪费——与
    /// `live_thinking_block_renders_one_line_and_never_touches_content` 消灭的
    /// 是同一类问题。
    #[test]
    fn collapsed_tool_result_never_touches_content() {
        let mut result = message(MessageRole::ToolResult, &"x".repeat(100_000));
        result.tool_name = Some("Bash".to_string());
        let rendered = render_messages(&[result.clone()], 100);

        assert_eq!(rendered.len(), 1, "{rendered:?}");
        assert!(
            !rendered.iter().any(|line| line.contains("xxx")),
            "折叠态不得渲染任何正文"
        );
    }

    #[test]
    fn running_tool_renders_its_live_output_under_the_call_line() {
        // 「正在执行的步骤默认展开」：⏺ 行下方应直接跟出实时输出块，
        // 即便 output_expanded = false（自动模式）。
        let mut call = message(MessageRole::ToolCall, "Bash(cargo build)");
        call.sequence_no = Some(7);
        let live = HashMap::from([(7usize, "compiling foo\nwarning: unused\n".to_string())]);

        let rendered = render_messages_full(&[call], 100, false, &live);

        assert_eq!(rendered.len(), 3, "{rendered:?}");
        assert!(rendered[0].contains("cargo build"), "{}", rendered[0]);
        assert!(
            rendered[1].contains("⎿"),
            "实时块需挂在 ⎿ 下：{}",
            rendered[1]
        );
        assert!(rendered[1].contains("compiling foo"), "{}", rendered[1]);
        assert!(rendered[2].contains("unused"), "{}", rendered[2]);
    }

    #[test]
    fn live_output_shows_in_auto_mode_and_in_force_expanded_mode() {
        // Ctrl+O 是在「自动」与「全展开」之间切换，不是「折叠 ↔ 展开」：
        // 自动模式下执行中的步骤本就展开，所以两个模式下实时块都必须存在；
        // 而已完成的结果只在全展开模式下才摊开全文。切回自动模式（Ctrl+O
        // 再按一次）不得让正在跑的那一步突然消失。
        let mut call = message(MessageRole::ToolCall, "Bash(make)");
        call.sequence_no = Some(4);
        let mut result = message(MessageRole::ToolResult, "final body text");
        result.tool_name = Some("Bash".to_string());
        result.sequence_no = Some(4);
        let live = HashMap::from([(4usize, "in-flight tail".to_string())]);

        let auto = render_messages_full(&[call.clone()], 100, false, &live);
        assert!(
            auto.iter().any(|l| l.contains("in-flight tail")),
            "自动模式下执行中的步骤应展开：{auto:?}"
        );

        let expanded = render_messages_full(&[call.clone()], 100, true, &live);
        assert!(
            expanded.iter().any(|l| l.contains("in-flight tail")),
            "全展开模式下实时块同样在：{expanded:?}"
        );

        // 已完成的结果：自动模式折叠、全展开模式摊开。
        let auto_done = render_messages_full(&[call.clone(), result.clone()], 100, false, &live);
        assert!(
            !auto_done.iter().any(|l| l.contains("final body text")),
            "自动模式下完成即收：{auto_done:?}"
        );
        let expanded_done = render_messages_full(&[call, result], 100, true, &live);
        assert!(
            expanded_done.iter().any(|l| l.contains("final body text")),
            "全展开模式下应看到完整结果：{expanded_done:?}"
        );
    }

    #[test]
    fn live_output_only_attaches_to_the_matching_call() {
        // 按 sequence_no 配对：seq 不匹配的 ⏺ 行不得挂上别人的实时输出
        let mut other = message(MessageRole::ToolCall, "Read(a.rs)");
        other.sequence_no = Some(1);
        let mut running = message(MessageRole::ToolCall, "Bash(make)");
        running.sequence_no = Some(2);
        let live = HashMap::from([(2usize, "gcc -O2 foo.c".to_string())]);

        let rendered = render_messages_full(&[other, running], 100, false, &live);

        assert!(!rendered[0].contains("gcc"), "{}", rendered[0]);
        assert!(
            rendered.iter().any(|l| l.contains("gcc -O2")),
            "seq=2 的实时输出应出现：{rendered:?}"
        );
    }

    #[test]
    fn live_output_block_is_capped_to_tail_lines() {
        // 100KB 正文的 Bash 结果在折叠态零成本（见上一条测试）；实时块是唯一
        // 例外，因此它必须自带硬上界，且明确告知用户上方还有多少行。
        let mut call = message(MessageRole::ToolCall, "Bash(make)");
        call.sequence_no = Some(0);
        let live = HashMap::from([(
            0usize,
            (1..=100_000)
                .map(|i| format!("compile {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )]);

        let rendered = render_messages_full(&[call], 100, false, &live);

        assert_eq!(rendered.len(), LIVE_TAIL_LINES + 2, "{}", rendered.len());
        assert!(
            rendered[rendered.len() - 1].contains(&(100_000 - LIVE_TAIL_LINES).to_string()),
            "末行应是截断提示：{}",
            rendered[rendered.len() - 1]
        );
        assert!(
            rendered[rendered.len() - 2].contains("compile 100000"),
            "尾部应是最新一行：{}",
            rendered[rendered.len() - 2]
        );
    }

    #[test]
    fn finished_tool_has_no_live_block_even_with_a_stale_snapshot() {
        // 快照的正常回收点是 `ToolEnd` 里的 `live_tool_out.remove`。这里钉的是
        // 回收点万一失效时的兜底：结果行已经落地 = 这次调用已完成，
        // `render_message_range` 会把 ToolCall + ToolResult 合并成一块渲染，
        // 那条路径根本不查 `live_tool_out`——即使哈希表里还躺着快照也画不出来。
        let mut call = message(MessageRole::ToolCall, "Bash(make)");
        call.sequence_no = Some(3);
        let mut result = message(MessageRole::ToolResult, "done");
        result.tool_name = Some("Bash".to_string());
        result.sequence_no = Some(3);
        let live = HashMap::from([(3usize, "stale tail".to_string())]);

        let rendered = render_messages_full(&[call, result], 100, false, &live);

        assert!(
            !rendered.iter().any(|l| l.contains("stale")),
            "已完成的调用不应再画实时块：{rendered:?}"
        );
    }

    /// 展开态不得出现任何形式的省略标记。
    ///
    /// 旧的截断机制有两套：`push_capped_preview_lines` 的 `...`（所有超长结果）和
    /// 错误结果的 head+tail `⋮`（专程保住尾部的 `request_id` / `reason`）。展开已是
    /// 全文，两者都必须消失——`⋮` 尤其危险，它会主动删掉用户明确要求看到的中间内容。
    #[test]
    fn expanded_result_has_no_ellipsis_of_any_kind() {
        let long_err = {
            let mut m = message(
                MessageRole::ToolResult,
                &(1..=30)
                    .map(|i| format!("err line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            m.tool_name = Some("Bash".to_string());
            m.is_error = true;
            m
        };
        let long_diff = {
            let mut m = message(
                MessageRole::ToolResult,
                &(1..=30)
                    .map(|i| format!("+ added line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            m.tool_name = Some("Edit".to_string());
            m
        };
        let long_plain = {
            let mut m = message(
                MessageRole::ToolResult,
                &(1..=30)
                    .map(|i| format!("plain line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            m.tool_name = Some("Bash".to_string());
            m
        };

        for result in [long_err, long_diff, long_plain] {
            let rendered = render_messages_with_output_expanded(&[result], 100, true);
            assert!(
                !rendered.iter().any(|line| line.trim() == "..."),
                "展开态不得出现 `...`: {rendered:?}"
            );
            assert!(
                !rendered.iter().any(|line| line.contains('⋮')),
                "展开态不得出现 `⋮`: {rendered:?}"
            );
            assert!(
                rendered.last().is_some_and(|l| !l.trim().is_empty()),
                "展开态最后一行应是真实内容而非省略标记: {rendered:?}"
            );
        }
    }

    /// `!` bash 命令输出（`MessageRole::BashOutput`）**出全文**：不折叠、
    /// 不截断、不补 `...`，且与 `Ctrl+O` 开关无关。
    ///
    /// 这条替代了旧的「三行预览」钉子。`!` 是用户亲手敲的命令，它的输出是
    /// 用户明确要看的回显——按屏幕高度裁掉就等于替用户决定他该看到什么。
    /// 展开态/折叠态都不该出现省略标记，也必须能一直滚到最后一行。
    #[test]
    fn bangs_bash_output_renders_every_line_regardless_of_expand() {
        let output = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut msg = message(MessageRole::BashOutput, &output);
        msg.elapsed_secs = Some(0.4);

        for expanded in [false, true] {
            let rendered = render_messages_with_output_expanded(&[msg.clone()], 100, expanded);
            assert!(
                !rendered.iter().any(|line| line.trim() == "..."),
                "`!` 输出不得再出现三行预览的省略标记（expanded={expanded}）: {rendered:?}"
            );
            assert!(
                rendered.last().is_some_and(|l| l.contains("line 10")),
                "末行必须是输出的最后一行而非省略标记（expanded={expanded}）: {rendered:?}"
            );
            for i in 1..=10 {
                assert!(
                    rendered
                        .iter()
                        .any(|line| line.contains(&format!("line {i}"))),
                    "第 {i} 行必须可见（expanded={expanded}）: {rendered:?}"
                );
            }
        }
    }

    /// `!` 输出里超过屏幕宽度的长行必须折行，且**不能被静默截断**——全文渲染
    /// 的意义包含长行，丢了就是又变回「用户没看到的那部分」。
    #[test]
    fn bangs_bash_output_wraps_long_lines_instead_of_dropping_them() {
        let long_line = "x".repeat(250);
        let msg = message(MessageRole::BashOutput, &long_line);
        let rendered = render_messages_with_output_expanded(&[msg], 80, false);

        let xs: usize = rendered.iter().map(|l| l.matches('x').count()).sum();
        assert_eq!(xs, 250, "折行后所有 x 都必须还在: {rendered:?}");
        assert!(
            rendered.len() > 4,
            "250 字符的长行在 80 宽下必须折成多行: {rendered:?}"
        );
    }

    /// 空的 `!` 输出必须显式回落到 `(no output)`，而不是渲染成一块空白。
    #[test]
    fn bangs_bash_output_falls_back_to_no_output_placeholder() {
        let msg = message(MessageRole::BashOutput, "   \n  \n");
        let rendered = render_messages_with_output_expanded(&[msg], 100, false);
        assert!(
            rendered.iter().any(|l| l.contains("(no output)")),
            "{rendered:?}"
        );
    }

    /// 回归测试：AI 思考期间用户继续在主输入框打字 / 移动光标时，硬件光标
    /// 必须跟随用户实际操作，不能停在"上一条消息提交瞬间"的位置。
    ///
    /// 旧实现（修复前）的 `draw_input` 在 `state.is_thinking` 时直接 `return`，
    /// 跳过了末尾的 `f.set_cursor_position(...)`，导致 crossterm 的硬件光标
    /// 永远保留上一次成功调用时的位置——用户主观感觉"光标卡住 / 错位"。
    ///
    /// 这个测试用 ratatui 的 `TestBackend` 跑一帧 `is_thinking=true` 状态下的
    /// `draw`，断言 `get_cursor_position()` 返回 `Some(...)`，任何未来重新
    /// 在 `draw_input` 里加 `if is_thinking { return; }` 屏蔽光标的回归都会
    /// 让这里失败（因为后续 `set_cursor_position` 永远不会被调用）。
    #[test]
    fn draw_input_keeps_cursor_visible_while_thinking() {
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::Terminal;

        // 120x10 给足横向空间，is_thinking 时输入框标题栏放"⠋ Thinking..."
        // 指示器（spinner + label + animated dots），状态栏保持简洁——
        // 只显示模型/进度/用量/cwd。高度 10 容下 chat/输入框/状态栏。
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = true;
        let mut input = InputBox::new();
        input.insert_text("abc 测试");
        input.move_left(); // 光标停在 "abc 测|" 之后

        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        let cursor = terminal.backend_mut().get_cursor_position();
        assert!(
            cursor.is_ok(),
            "is_thinking 时 draw_input 必须调用 set_cursor_position，\
             bug 复发会让 crossterm cursor 永远停在旧位置或返回错误"
        );
    }

    /// 输入框标题栏在 is_thinking=true 时必须显示
    /// `⠋ <当前步骤> · <耗时> · <本轮 token>`，且**不带** animated dots 尾巴。
    ///
    /// 四段信息各由独立状态驱动（spinner 帧 / `thinking_status_label` /
    /// `current_step_elapsed_secs` / `title_token_text`），这里整帧渲染一次全部验掉：
    ///   1. spinner 帧打头（SPINNER_FRAMES[0] = ⠋）—— 唯一的运动效果来源
    ///   2. label 是动词短语 `运行命令`，而非工具注册名 `Bash`，也不是
    ///      聊天区那条带参数的 `Bash(cargo test)`
    ///   3. 耗时取**这一步**的起点（1.5s），不是整轮的 3s
    ///   4. token 是本轮增量 `1,000↑ 40↓`，不是 session 累计
    ///   5. 不含 "..." 点数尾巴（与耗时/用量同时动会发飘，窄屏最先被挤掉）
    ///   6. 不含「第 N 步」——步号既不回答"还剩多少"也不回答"在做什么"
    #[test]
    fn draw_input_title_shows_step_elapsed_and_turn_tokens_when_thinking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = true;
        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_secs(3));
        state.current_op = Some("Bash(cargo test)".to_string());
        state.current_tool_name = Some("Bash".to_string());
        state.current_op_started = Some(Instant::now() - std::time::Duration::from_millis(1500));
        state.turn_live_input_tokens = 1_000;
        state.turn_live_output_tokens = 40;
        state.total_input_tokens = 1_100;
        state.total_output_tokens = 50;
        let input = InputBox::new();

        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        // 把 ratatui buffer 里的整张图 dump 成字符串，找到输入框第一行(标题栏)
        let buffer = terminal.backend().buffer().clone();
        let mut rendered = Vec::new();
        for y in 0..buffer.area.height {
            let line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string();
            if !line.trim().is_empty() {
                rendered.push(line);
            }
        }
        let activity = wyj_i18n::tr("tool_activity.bash");
        let title_line = rendered
            .iter()
            .rev()
            .find(|l| l.contains(&activity) || l.contains('⠋'))
            .expect("is_thinking 时标题栏必须出现当前动作短语 + ⠋ spinner");

        assert!(
            title_line.contains('⠋'),
            "is_thinking 时标题栏应包含 ⠋ spinner 帧，实际行: {title_line:?}"
        );
        assert!(
            title_line.contains(&activity),
            "标题栏应是动词短语「{activity}」, 实际行: {title_line:?}"
        );
        assert!(
            !title_line.contains("cargo test"),
            "标题栏不得复述聊天区工具行里的参数（那正是「和输出区最后一条一样」的成因）, \
             实际行: {title_line:?}"
        );
        assert!(
            !title_line.contains("第") && !title_line.contains("Bash"),
            "标题栏既不该显示步号，也不该显示工具注册名, 实际行: {title_line:?}"
        );
        assert!(
            title_line.contains("1.5s") && !title_line.contains(" 3s"),
            "标题栏耗时应是这一步的 1.5s 而非整轮的 3s, 实际行: {title_line:?}"
        );
        assert!(
            title_line.contains("1,000↑") && title_line.contains("40↓"),
            "标题栏应显示本轮 token 增量, 实际行: {title_line:?}"
        );
        assert!(
            !title_line.contains("..."),
            "标题栏不应再带 animated dots 尾巴, 实际行: {title_line:?}"
        );
        // 不应再包含旧的"esc to interrupt"提示文案(已迁到主输入框光标区注释)
        assert!(
            !title_line.contains("esc to interrupt"),
            "标题栏不应再出现旧的 'esc to interrupt' 硬编码文案, 实际行: {title_line:?}"
        );
    }

    /// 状态栏(is_thinking=false)必须不再出现 "ctrl+d or ctrl+c twice to exit" 文案,
    /// 与右侧 "/help" 提示一并删除；这些信息已经在 /help 命令里有完整说明, 底部状态栏
    /// 留给"模型 / 进度 / 用量 / cwd"等更重要的运行时信息。
    #[test]
    fn draw_status_hides_ctrl_d_exit_hint_by_default() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = false;
        state.ctrl_c_pressed = false;
        state.pending_queue.clear();
        let input = InputBox::new();

        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        let buffer = terminal.backend().buffer().clone();
        let mut merged = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                merged.push_str(buffer[(x, y)].symbol());
            }
        }

        assert!(
            !merged.contains("ctrl+d"),
            "默认状态不应再渲染 'ctrl+d or ctrl+c twice to exit' 提示, 实际渲染包含 ctrl+d 字串"
        );
        assert!(
            !merged.contains("ctrl+c twice"),
            "默认状态不应再渲染 'ctrl+c twice to exit' 提示"
        );
        // /help 本身在内部 register 内是正常出现字串的, 这里只检查底部状态栏,
        // 用 ratatui buffer 提取最后一行(状态栏高度=1)做严格断言
        let last_line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - 1)].symbol().to_string())
            .collect();
        assert!(
            !last_line.contains("/help"),
            "状态栏最末行不应再渲染 /help 入口提示, 实际: {last_line:?}"
        );
    }

    /// v0.2.26：状态栏不再显示上下文占用百分比与进度条。
    ///
    /// 回归背景有两条，缺一不可：
    ///   1. 旧实现算的是 `estimate_tokens(messages)`，而自动压缩决策算的是
    ///      `estimate_request_tokens`（含 system + 工具 schema + 输出预留），
    ///      两者差 1 万+ token —— 条的颜色和实际压缩时机对不上号。
    ///   2. 压缩已完全自动化，占比对用户没有可操作性；精确分解改走 `/context`。
    #[test]
    fn draw_status_omits_context_percentage_and_bar() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = false;
        // 故意把占用顶到接近满格，确保不是"因为占比低所以没显示"。
        state.context_window = 200_000;
        let input = InputBox::new();

        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        let buffer = terminal.backend().buffer().clone();
        let last_line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - 1)].symbol().to_string())
            .collect();

        assert!(
            !last_line.contains('%'),
            "状态栏不应再出现上下文占用百分比, 实际: {last_line:?}"
        );
        assert!(
            !last_line.contains('█') && !last_line.contains('░'),
            "状态栏不应再出现进度条, 实际: {last_line:?}"
        );
    }

    /// 反向锁：状态栏只保留「模型 · 模式 · 工作目录」三段常驻信息，
    /// 自动压缩计数**不再**回到这里（v0.2.29 起从状态栏移除）。
    ///
    /// 移除理由：状态栏每帧都在，它该回答"我在哪 / 我以什么身份在跑"；压缩次数
    /// 是低频、需要主动查询的信息，塞在这里只会把 cwd 往右挤窄屏。压缩是否发生
    /// 仍可通过 `/context` 查看（那边是同源口径的精确分解）。
    #[test]
    fn draw_status_omits_compact_chip_even_after_summarizing() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = false;
        state.compact_count = 3;
        let input = InputBox::new();
        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");
        let last_line: String = {
            let buffer = terminal.backend().buffer().clone();
            (0..buffer.area.width)
                .map(|x| buffer[(x, buffer.area.height - 1)].symbol().to_string())
                .collect()
        };
        assert!(
            !last_line.contains('×') && !last_line.contains("已自动压缩"),
            "状态栏不应再显示压缩计数, 实际: {last_line:?}"
        );
        // 三段常驻信息必须仍在
        assert!(
            last_line.contains(&state.model_name)
                && last_line.contains("[auto]")
                && last_line.contains("·"),
            "状态栏应保留 模型 / 模式 / 工作目录 三段, 实际: {last_line:?}"
        );
    }

    /// 三种模式在状态栏都必须显式标出，且 Bypass 必须是**红色**——
    /// 它等价于「本次会话跳过全部权限确认」，是唯一需要用户知情承担的模式。
    #[test]
    fn draw_status_labels_every_mode_and_paints_bypass_red() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        for (mode, expect) in [
            (AgentMode::Auto, "[auto]"),
            (AgentMode::Plan, "[plan]"),
            (AgentMode::Bypass, "[bypass]"),
        ] {
            let backend = TestBackend::new(120, 10);
            let mut terminal = Terminal::new(backend).expect("TestBackend init");
            let mut state = make_state();
            state.is_thinking = false;
            state.mode = mode.clone();
            let input = InputBox::new();
            terminal
                .draw(|f| draw(f, &mut state, &input))
                .expect("draw ok");

            let buffer = terminal.backend().buffer().clone();
            let row = buffer.area.height - 1;
            let line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, row)].symbol().to_string())
                .collect();
            assert!(
                line.contains(expect),
                "模式 {mode:?} 必须在状态栏显式标注 {expect}, 实际: {line:?}"
            );

            // Bypass 必须染成 danger 槽位（提醒语义），其余模式不受此约束。
            if mode == AgentMode::Bypass {
                let idx = line.find("[bypass]").expect("bypass 芯片存在");
                assert_eq!(
                    buffer[(idx as u16, row)].fg,
                    Theme::danger_color(),
                    "Bypass 芯片必须是 danger 槽位而不是中性黄, 实际 fg={:?}",
                    buffer[(idx as u16, row)].fg
                );

                // 「三处同色」是硬不变式（见 tui/CLAUDE.md）：状态栏芯片 +
                // 标题栏 + 输入框边框。只查状态栏等于没查——标题栏与边框
                // 各写各的颜色时，用户看到的是两种红/非红，无法判断当前模式。
                let mut hits = Vec::new();
                for y in 0..buffer.area.height {
                    let row_text: String = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect();
                    if let Some(i) = row_text.find("[bypass]") {
                        hits.push((y, i as u16));
                    }
                }
                assert_eq!(
                    hits.len(),
                    2,
                    "[bypass] 应在状态栏与输入框标题栏各出现一次，实际 {hits:?}"
                );
                for (y, x) in &hits {
                    assert_eq!(
                        buffer[(*x, *y)].fg,
                        Theme::danger_color(),
                        "bypass 文字 ({x},{y}) 必须与芯片同色"
                    );
                }

                // 输入框底边框（状态栏正上方那行）的左端框角
                let border_row = row - 1;
                let corner_x = (0..buffer.area.width)
                    .find(|x| buffer[(*x, border_row)].symbol() != " ")
                    .expect("输入框底边框应至少有一个可见 cell");
                assert_eq!(
                    buffer[(corner_x, border_row)].fg,
                    Theme::danger_color(),
                    "输入框边框必须与 bypass 芯片同色"
                );
            }
        }
    }

    /// 默认档（auto）必须走**中性灰**，不能是 success 绿。
    ///
    /// 合并掉 manual 之后 auto 就是唯一日常档，它不再"通过了什么"，只是一档
    /// 默认设置；染成绿色会让用户把当前状态读成"已获授权"。同时标题栏的
    /// `[auto] Enter to send` 与输入框边框必须同属中性档——边框与普通框线同色，
    /// 标题栏与状态栏芯片同色（`dim`），三处读起来是同一个意思。
    #[test]
    fn auto_mode_is_painted_neutral_gray_not_success_green() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");
        let mut state = make_state();
        state.is_thinking = false;
        state.mode = AgentMode::Auto;
        let input = InputBox::new();
        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        let buffer = terminal.backend().buffer().clone();
        let status_row = buffer.area.height - 1;
        let status_text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, status_row)].symbol().to_string())
            .collect();
        let idx = status_text.find("[auto]").expect("auto 芯片存在") as u16;
        assert_eq!(
            buffer[(idx, status_row)].fg,
            Theme::inactive_color(),
            "auto 芯片必须是中性灰（inactive 槽位），实际 fg={:?}",
            buffer[(idx, status_row)].fg
        );

        // 标题栏的 `[auto] Enter to send` 同色。
        let mut title_hits = Vec::new();
        for y in 0..status_row.saturating_sub(1) {
            let row_text: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if let Some(i) = row_text.find("[auto] Enter") {
                title_hits.push((y, i as u16));
            }
        }
        assert_eq!(
            title_hits.len(),
            1,
            "[auto] Enter to send 应恰好出现一次，实际 {title_hits:?}"
        );
        let (ty, tx) = title_hits[0];
        assert_eq!(
            buffer[(tx, ty)].fg,
            Theme::inactive_color(),
            "标题栏 auto 标签必须与状态栏芯片同色"
        );

        // 输入框边框回到默认框线色（不再是 success 绿）。
        let border_row = status_row - 1;
        let corner_x = (0..buffer.area.width)
            .find(|x| buffer[(*x, border_row)].symbol() != " ")
            .expect("输入框底边框应至少有一个可见 cell");
        assert_eq!(
            buffer[(corner_x, border_row)].fg,
            Theme::border_color(),
            "默认档的输入框边框不应被染色"
        );
    }

    /// 状态栏不应再设置显式背景色（之前 `Theme::status_bar()` 用 `Rgb(30, 30, 30)`
    /// 让最末行在终端默认黑底上呈现深灰带，与上方标题栏/聊天区割裂）。这里断言
    /// 最后一行的所有非空 cell 都是 `Color::Reset`（= 未设置前景外的任何背景），
    /// 防止后续误写 `bg(...)` 又把这条黑条加回来。
    /// 状态栏宽度必须按显示列宽算，且任何宽度下都不得右端溢出。
    ///
    /// 用户现场：右下角出现 `● 1 条已排队 (`。那个 `(` 不是提示的一部分，而是全角
    /// `（` 被右端硬切的残留——旧实现按 `chars().count()` 算宽，中文与全宽符号是
    /// 双宽，`● 1 条已排队（等待送达）` char 数 14、显示宽度 24，少预留 10 列。
    #[test]
    fn status_bar_layout_never_overflows_at_any_width() {
        // 现场样本：char 数与显示宽度差 10 列的提示串。
        let hint = "● 1 条已排队（等待送达）";
        let hint_w = hint.chars().map(char_display_width).sum::<usize>();
        assert!(
            hint_w > hint.chars().count(),
            "样本必须含双宽字符，否则测不到宽度算错：{hint_w} vs {}",
            hint.chars().count()
        );
        // " ◆ " + 模型名 + 会话标签 + " {chip}" + " · "
        let left_fixed = 3 + 10 + 7 + 1 + 6 + 3;
        for width in [12usize, 20, 40, 50, 60, 80, 120] {
            let (cwd_display, keep_right) =
                status_bar_layout(width, left_fixed, hint_w, "/Users/me/项目/wyj-code");
            // `left_fixed` 自身超预算时，左侧被裁掉的是**末尾的 cwd**，而 cwd 此时
            // 已经是空串（helper 在 avail_cwd < 2 时留空），所以按 min 计即可。
            let total = left_fixed.min(width)
                + cwd_display
                    .chars()
                    .map(char_display_width)
                    .sum::<usize>()
                // 右段保留时它的尾随空格也算一列（helper 的 avail_cwd 已扣过这一列）
                + if keep_right { hint_w + 1 } else { 0 };
            assert!(total <= width, "宽度 {width} 下内容超宽：{total}");
        }

        // 宽屏：右段保留、cwd 原样。
        let (cwd, keep) = status_bar_layout(120, left_fixed, hint_w, "/tmp");
        assert!(keep, "120 列放得下，右段不该被丢");
        assert_eq!(cwd, "/tmp");

        // 窄屏：右段整段丢弃，而不是留给 Paragraph 从右侧硬切。
        let (_, keep) = status_bar_layout(20, left_fixed, hint_w, "/tmp");
        assert!(!keep, "右段塞不下时必须整段丢弃");
    }

    /// 端到端：排队提示要么完整出现，要么整段不出现，绝不留半截括号。
    ///
    /// 默认 locale 是 en，右段为 `● 1 queued (awaiting delivery)`；中文 locale 下
    /// 换的是全角括号，同一条不变式由上一个测试按显示宽度覆盖。
    /// `draw_status` 里的宽度必须来自 [`char_display_width`]，不许再出现
    /// `chars().count()`。
    ///
    /// 这是「半个 `(`」的真正源头，而且**只在中文文案下暴露**：英文
    /// `● 1 queued (awaiting delivery)` 全是 ASCII，char 数恰好等于显示宽度，
    /// 于是纯英文的快照测试对这条完全不敏感——必须用中文样本 + 源码钉子双保险。
    /// 本仓库其它宽度计算（`truncate_line` / `wrap_line` / `build_thinking_title`）
    /// 都已按显示宽度写，唯独状态栏这条漏了。
    /// 注意：这个源码扫描**必须跳过注释行**——解释「为什么不能用 chars().count()」
    /// 的注释里就写着这个词，否则测试自己把自己判红。
    #[test]
    fn draw_status_never_measures_width_in_chars() {
        let source = include_str!("render.rs");
        let lines: Vec<&str> = source.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.trim_start().starts_with("fn draw_status("))
            .expect("render.rs 应有 draw_status");
        let end = (start + 1..)
            .find(|offset| lines[*offset].starts_with("fn ") || lines[*offset].starts_with("/// "))
            .unwrap_or(lines.len());
        // 跳过注释行：函数上方那段解释"为什么不能用 chars().count()"里就写着它。
        let body = lines[start..end]
            .iter()
            .filter(|l| !l.trim_start().starts_with("//"))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains("chars().count()"),
            "状态栏宽度必须按显示列宽算（chars().count() 在中文下少算约一半）：\n{body}"
        );
        assert!(
            body.contains("status_bar_layout("),
            "状态栏的两段排版应统一走 status_bar_layout，别处再算一遍宽度必然漂移"
        );
    }

    #[test]
    fn status_bar_queue_hint_is_never_cut_in_half() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        for width in [50u16, 60, 80, 120] {
            let backend = TestBackend::new(width, 10);
            let mut terminal = Terminal::new(backend).expect("TestBackend init");
            let mut state = make_state();
            state.is_thinking = true;
            state
                .pending_queue
                .push(crate::app::OutgoingMessage::plain("顺便看看"));
            let input = InputBox::new();
            terminal
                .draw(|f| draw(f, &mut state, &input))
                .expect("draw ok");
            let buffer = terminal.backend().buffer().clone();
            let last_line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, buffer.area.height - 1)].symbol().to_string())
                .collect();
            assert!(
                last_line.contains("(awaiting delivery)") || !last_line.contains('('),
                "宽度 {width}：排队提示被从右侧硬切，留下了半个括号：{last_line:?}"
            );
        }
    }

    #[test]
    fn draw_status_does_not_set_explicit_background() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = false;
        state.ctrl_c_pressed = false;
        state.pending_queue.clear();
        let input = InputBox::new();

        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        let buffer = terminal.backend().buffer().clone();
        let status_row = buffer.area.height - 1;

        for x in 0..buffer.area.width {
            let cell = &buffer[(x, status_row)];
            let symbol = cell.symbol();
            if symbol.is_empty() || symbol.chars().all(|c| c == ' ') {
                // 状态栏尾部 padding 可能没有渲染符号，跳过避免误报
                continue;
            }
            assert_eq!(
                cell.style().bg,
                Some(Color::Reset),
                "状态栏最末行的 cell ({x},{status_row}) 背景被显式设成了 {:?}, \
                 不应再显式设置背景；恢复透传终端默认底色更协调",
                cell.style().bg
            );
        }
    }

    /// 标题栏(尤其 is_thinking 时的 `⠋ Thinking...` 动态指示)所有 cell 必须
    /// 维持 `bg = Color::Reset`(即"无显式背景色")，最终视觉上透传终端默认底色。
    /// 之前状态栏那条 `Rgb(30,30,30)` 黑带已经移除；这里把标题栏也一并钉死，
    /// 防止后续误给 thinking 状态加 `bg(Color::Black)` 之类的样式，让标题栏
    /// 又退化成"一块独立的黑条"。Status bar 那条测试已经覆盖了最末行；
    /// 这里专门盯标题栏那行（`⠋ Thinking...` 落点）。
    #[test]
    fn draw_input_title_bar_keeps_no_explicit_background() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).expect("TestBackend init");

        let mut state = make_state();
        state.is_thinking = true;
        state.turn_start_time = Some(Instant::now() - std::time::Duration::from_secs(3));
        let input = InputBox::new();

        terminal
            .draw(|f| draw(f, &mut state, &input))
            .expect("draw ok");

        let buffer = terminal.backend().buffer().clone();
        // 找到包含 '⠋' spinner 那一行（输入框标题栏）。
        let title_row = (0..buffer.area.height)
            .find(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].symbol() == "⠋"))
            .expect("is_thinking 时 buffer 里必须出现 ⠋ spinner 那一行");

        let is_forbidden_black = |bg: Option<Color>| -> bool {
            matches!(
                bg,
                Some(Color::Black) | Some(Color::Rgb(0, 0, 0)) | Some(Color::Rgb(30, 30, 30))
            )
        };

        for x in 0..buffer.area.width {
            let cell = &buffer[(x, title_row)];
            let symbol = cell.symbol();
            if symbol.is_empty() || symbol.chars().all(|c| c == ' ') {
                // padding cell 可以不渲染符号，跳过避免误报
                continue;
            }
            assert!(
                !is_forbidden_black(cell.style().bg),
                "标题栏 cell ({x},{title_row}) sym={symbol:?} 不应带显式黑色背景 bg={:?}, \
                 必须是 Color::Reset（透传终端默认底色）",
                cell.style().bg
            );
        }
    }
}

/// `/cost` 真实输出 → TUI 渲染管线的端到端回归。
///
/// 这条测试锁死的是 2026-10-13 实际发生的那次事故：`/cost` 重排后表格看着
/// 正常，但 TUI 把 assistant 消息交给 markdown 渲染，而 markdown 按段落合并
/// 连续行再折行，一张 50 行的成本表在界面上塌成 6 行糊在一起。headless 路径
/// 是裸 `println`，**不经过**这一步，所以只在 headless 验证会全部漏过。
#[cfg(test)]
mod cost_panel_render_tests {
    use super::tool_result_fold_tests::render_preformatted;
    use wyj_commands::Command;

    fn cost_ctx(width: usize) -> wyj_commands::CommandContext {
        wyj_commands::CommandContext {
            cwd: std::path::PathBuf::new(),
            model: "MiniMax-M3.1-Flash-Preview".to_string(),
            terminal_width: width,
            input_tokens: 289_112,
            output_tokens: 40_440,
            cache_read_tokens: 3_030_912,
            cache_write_tokens: 0,
            api_calls: 50,
            prompt_cache_state: 1,
            context_window: 200_000,
            estimated_tokens: 159_164,
            context_audit: None,
            last_latency: Default::default(),
            compact_count: 0,
            context_edit: (0, 0),
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 51_795,
            sub_output_tokens: 31_723,
            sub_cache_read_tokens: 1_159_231,
            sub_cache_write_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands: vec![],
        }
    }

    #[tokio::test]
    async fn real_cost_output_survives_the_tui_render_pipeline() {
        let cmd = wyj_commands::builtin::CostCmd;
        let raw = match cmd.run("", &cost_ctx(100)).await.unwrap() {
            wyj_commands::CommandResult::OutputPlain(text) => text,
            other => panic!("/cost 必须返回 OutputPlain（否则排版会被 markdown 揉碎）: {other:?}"),
        };
        let source_lines = raw.lines().count();
        assert!(source_lines >= 12, "源文本行数异常: {source_lines}");

        let lines = render_preformatted(&raw, 100);

        // 核心断言：渲染行数 == 源行数。markdown 段落合并会让这个数字塌下来。
        assert_eq!(
            lines.len(),
            source_lines,
            "渲染后行数变了，说明内容被重新折行:\n源:\n{raw}\n渲染:\n{lines:?}"
        );
        // 且每一行的 Tokens 列右边缘必须齐平。
        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains("~$") || l.contains('—'))
            .map(|l| crate::markdown::display_width(l))
            .collect();
        assert!(widths.len() >= 5, "带金额的行太少: {widths:?}\n{raw}");
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "表格列对齐被破坏: {widths:?}\n{raw}"
        );
    }
}

/// 聊天区视口滑动的回归钉子。
///
/// 旧实现每帧把**整个会话历史**交给 `Paragraph`：`line_count()` 全量折行一次，
/// `Paragraph::scroll` 在绘制时再全量折行一次（scroll 只跳过绘制、不跳过折行）。
/// 长会话下这两趟 O(历史) 是滚轮卡顿的直接来源，且 `scroll` 是 `u16`，历史超过
/// 65535 物理行后底部内容根本滚不到。现在先按逻辑行算高度，只把覆盖视口的
/// 那几条交给 `Paragraph`。
#[cfg(test)]
mod chat_viewport_tests {
    use super::*;
    use crate::app::ChatMessage;
    use crate::test_support::{make_state, render_to_text};

    #[test]
    fn logical_line_height_counts_wrapped_physical_rows() {
        assert_eq!(logical_line_height(&Line::from("x".repeat(100)), 40), 3);
        assert_eq!(logical_line_height(&Line::from("short"), 40), 1);
        // 宽度为 0 时按 1 列兜底，不能除零也不能返回 0。
        assert!(logical_line_height(&Line::from("abc"), 0) >= 1);
    }

    #[test]
    fn locate_scroll_start_maps_physical_offset_to_logical_line() {
        let heights = [1u16, 3, 1];
        assert_eq!(locate_scroll_start(&heights, 0), (0, 0));
        assert_eq!(locate_scroll_start(&heights, 1), (1, 0));
        assert_eq!(locate_scroll_start(&heights, 2), (1, 1));
        assert_eq!(locate_scroll_start(&heights, 4), (2, 0));
        assert_eq!(locate_scroll_start(&heights, 5), (3, 0));
    }

    fn history_state(count: usize) -> AppState {
        let mut state = make_state();
        state.welcome_frozen = true;
        for i in 0..count {
            state
                .messages
                .push(ChatMessage::system(format!("msg-{i:03}")));
        }
        state
    }

    /// 直接从渲染管线量出「全部历史折算成多少物理行」，避免测试里硬编码
    /// 消息块之间的空行等排版细节。
    fn total_physical_height(state: &mut AppState, width: u16) -> usize {
        build_pending_chat_lines(state, chat_content_width(width))
            .iter()
            .map(|line| logical_line_height(line, width))
            .sum()
    }

    #[test]
    fn long_history_renders_only_the_tail_when_following() {
        let mut state = history_state(200);
        state.chat_follow_tail = true;
        let total = total_physical_height(&mut state, 60);

        let text = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);

        assert!(text.contains("msg-199"), "贴底时应看到最后一条:\n{text}");
        assert!(
            !text.contains("msg-000"),
            "视口外的历史不应参与绘制:\n{text}"
        );
        assert_eq!(state.chat_view_height, 20);
        assert_eq!(state.chat_max_scroll, total - 20);
    }

    #[test]
    fn scrolling_up_shows_earlier_history() {
        let mut state = history_state(200);
        state.chat_follow_tail = false;
        state.chat_scroll = 0;

        let text = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);

        assert!(text.contains("msg-000"), "滚到顶部应看到第一条:\n{text}");
        assert!(
            !text.contains("msg-199"),
            "非贴底时不应把尾部拽进视口:\n{text}"
        );
    }

    /// 折行高度必须计入 `max_scroll`：否则贴底时最后几条会被裁掉。
    #[test]
    fn wrapped_lines_contribute_their_physical_height() {
        let mut state = make_state();
        state.welcome_frozen = true;
        // 每条 200 字符的系统消息在 60 列下会折成多行。
        for i in 0..10 {
            state.messages.push(ChatMessage::system(format!(
                "wrapped-{i:02}-{}",
                "x".repeat(200)
            )));
        }
        state.chat_follow_tail = true;

        let logical = build_pending_chat_lines(&mut state, chat_content_width(60)).len();
        let total = total_physical_height(&mut state, 60);
        assert!(
            total > logical,
            "长行必须折算成多行物理行: logical={logical} total={total}"
        );

        let text = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);

        assert_eq!(state.chat_max_scroll, total - 20);
        assert!(
            text.contains("wrapped-09"),
            "最后一条消息必须完整落在视口内:\n{text}"
        );
    }

    /// 右侧滚动条：内容装不下时出现，随滚动位置移动。
    #[test]
    fn scrollbar_appears_only_when_content_overflows_and_tracks_scroll() {
        let mut state = history_state(200);
        state.chat_follow_tail = false;
        state.chat_scroll = 0;
        let top = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);
        assert!(
            top.lines().any(|line| line.ends_with('┃')),
            "长内容必须画出滚动条滑块:\n{top}"
        );

        state.chat_scroll = state.chat_max_scroll;
        let bottom = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);
        assert!(
            bottom.lines().any(|line| line.ends_with('┃')),
            "滚到底后滑块仍应可见:\n{bottom}"
        );
        // 滑块位置必须真的变了：顶部时首行是滑块，底部时首行是轨道。
        let top_first = top.lines().next().unwrap_or_default();
        let bottom_first = bottom.lines().next().unwrap_or_default();
        assert_ne!(
            top_first.ends_with('┃'),
            bottom_first.ends_with('┃'),
            "滚动位置变化时滑块位置必须跟随:\n top={top_first:?}\nbottom={bottom_first:?}"
        );
    }

    /// 内容装得下时不画滚动条：满格滑块会误导用户去拖它。
    #[test]
    fn scrollbar_is_hidden_when_content_fits() {
        let mut state = make_state();
        state.welcome_frozen = true;
        state
            .messages
            .push(ChatMessage::system("short".to_string()));
        let text = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);
        assert!(
            !text.contains('┃') && !text.contains('│'),
            "短内容不应出现滚动条:\n{text}"
        );
    }

    /// `[ui].scrollbar = false` 时完全不画。
    #[test]
    fn scrollbar_respects_the_ui_switch() {
        let mut state = history_state(200);
        state.chat_follow_tail = false;
        state.show_scrollbar = false;
        let text = render_to_text(|f| draw_chat(f, &mut state, f.area()), 60, 20);
        assert!(
            !text.contains('┃') && !text.contains('│'),
            "开关关闭时不应画滚动条:\n{text}"
        );
    }
}
