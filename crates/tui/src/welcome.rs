//! TUI 欢迎页面（聊天区为空时显示）
//!
//! 极简布局：
//! - 上方空 2 行（顶部留白）
//! - `WYJ-CODE` 阴影块状艺术字（figlet ANSI Shadow 风格，6 行高，主题橙 + BOLD），整体左缩进 2 格
//! - 空 2 行
//! - `欢迎回来`（灰色 dim，走 i18n），左缩进 4 格
//!
//! 渲染入口是 [`render_welcome`]，由 `crates/tui/src/render.rs` 的
//! `draw_chat` 在 `messages.is_empty() && streaming_buf.is_empty()` 时调用。

use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// 借用上下文，让单测不必构造完整 `AppState`（30+ 字段无 Default impl）。
///
/// 当前已无任何字段被读取——欢迎页只展示静态艺术字 + 一句本地化问候。
/// 保留 struct 仅为了让 `render_welcome` 的签名保持向后兼容（render.rs 仍在调用）。
pub struct WelcomeContext {}

/// 整体左侧缩进：logo 6 行统一前缀。
const LOGO_INDENT: &str = "  ";
/// 问候语左侧缩进：比 logo 多 2 格，视觉上与 logo 左缘错开、呼吸感更自然。
const GREETING_INDENT: &str = "    ";
/// logo 与问候语之间保持 2 行空行间隔。
const INTER_BLANK_LINES: usize = 2;
/// 欢迎页顶部留白行数（位于 logo 上方）。
const TOP_BLANK_LINES: usize = 2;

/// WYJ-CODE 艺术字（figlet ANSI Shadow 字体，6 行高，73 列宽）。
///
/// 字符来源：从 figlet 字体表 `ANSI Shadow.flf` 提取 W/Y/J/-/C/O/D/E 八个
/// 字符的 6 行定义，字符间以 1 列空格拼接（硬空白已替换为正常空格）。
const ASCII_LOGO: &[&str] = &[
    "██╗    ██╗ ██╗   ██╗      ██╗         ██████╗  ██████╗  ██████╗  ███████╗",
    "██║    ██║ ╚██╗ ██╔╝      ██║        ██╔════╝ ██╔═══██╗ ██╔══██╗ ██╔════╝",
    "██║ █╗ ██║  ╚████╔╝       ██║ █████╗ ██║      ██║   ██║ ██║  ██║ █████╗  ",
    "██║███╗██║   ╚██╔╝   ██   ██║ ╚════╝ ██║      ██║   ██║ ██║  ██║ ██╔══╝  ",
    "╚███╔███╔╝    ██║    ╚█████╔╝        ╚██████╗ ╚██████╔╝ ██████╔╝ ███████╗",
    " ╚══╝╚══╝     ╚═╝     ╚════╝          ╚═════╝  ╚═════╝  ╚═════╝  ╚══════╝",
];

/// 生成欢迎页所有 Line（含自动垂直居中？否——调用方负责垂直/水平定位）。
///
/// 布局（自上而下）：
/// - 顶部 `TOP_BLANK_LINES` 行空行（视觉上把 logo 推到屏幕偏中偏下一点的位置）
/// - 6 行 logo（橙色 + BOLD，**整体左缩进 `LOGO_INDENT`**）
/// - `INTER_BLANK_LINES` 行空行
/// - `欢迎回来`（dim，走 i18n，**左缩进 `GREETING_INDENT`**）
pub fn render_welcome(_ctx: &WelcomeContext, _area_width: u16) -> Vec<Line<'static>> {
    let total = TOP_BLANK_LINES + ASCII_LOGO.len() + INTER_BLANK_LINES + 1;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total);

    // 0) 顶部留白（让 logo 距聊天框顶 2 行）
    for _ in 0..TOP_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 1) logo 6 行（橙色 + BOLD，左缩进 LOGO_INDENT）
    let logo_style = Style::default()
        .fg(Theme::CLAUDE)
        .add_modifier(Modifier::BOLD);
    for row in ASCII_LOGO {
        lines.push(Line::from(Span::styled(
            format!("{LOGO_INDENT}{row}"),
            logo_style,
        )));
    }

    // 2) logo 与问候语之间空 INTER_BLANK_LINES 行
    for _ in 0..INTER_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 3) 「欢迎回来」/「Welcome back」（i18n），左缩进 GREETING_INDENT
    lines.push(Line::from(Span::styled(
        format!("{GREETING_INDENT}{}", wyj_i18n::tr("welcome.greeting")),
        Theme::dim(),
    )));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx() -> WelcomeContext {
        WelcomeContext {}
    }

    fn collect_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn render_welcome_first_line_is_blank_padding() {
        // 顶部留白（logo 之上）——首行是空行，方便 ratatui 把 logo 推到聊天框中段
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        for i in 0..TOP_BLANK_LINES {
            assert_eq!(
                collect_text(&lines[i]),
                "",
                "顶部第 {} 行应为留白空行",
                i + 1
            );
        }
    }

    #[test]
    fn render_welcome_total_lines_is_top_pad_plus_logo_plus_inter_plus_greeting() {
        // 顶部 2 行 + logo 6 行 + 中间 2 行 + 1 行问候 = 11 行
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let expected = TOP_BLANK_LINES + ASCII_LOGO.len() + INTER_BLANK_LINES + 1;
        assert_eq!(
            lines.len(),
            expected,
            "欢迎页应固定 {} 行；实际 {} 行",
            expected,
            lines.len()
        );
    }

    #[test]
    fn render_welcome_logo_has_six_rows() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let start = TOP_BLANK_LINES;
        let end = start + ASCII_LOGO.len();
        let logo_lines = &lines[start..end];
        assert_eq!(logo_lines.len(), 6, "logo 应为 6 行");
        for (i, line) in logo_lines.iter().enumerate() {
            assert_eq!(
                line.spans.len(),
                1,
                "logo 第 {} 行应为单 span；实际 {} 个",
                i + 1,
                line.spans.len()
            );
        }
    }

    #[test]
    fn render_welcome_logo_first_row_starts_with_indent() {
        // logo 整体左缩进 LOGO_INDENT（2 空格）；首字符应是块状字符 '█' 之外的空格
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let first_logo_row = &lines[TOP_BLANK_LINES];
        let content = &first_logo_row.spans[0].content;
        assert!(
            content.starts_with(LOGO_INDENT),
            "logo 第 1 行应以 `{LOGO_INDENT}` 缩进起首；实际：{:?}",
            content
        );
    }

    #[test]
    fn render_welcome_logo_uses_claude_color() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let first_logo_row = &lines[TOP_BLANK_LINES];
        assert_eq!(
            first_logo_row.spans[0].style.fg,
            Some(Theme::CLAUDE),
            "logo 应使用主题橙 CLAUDE"
        );
        assert!(
            first_logo_row.spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "logo 应为 BOLD"
        );
    }

    #[test]
    fn render_welcome_includes_two_blank_lines_between_logo_and_greeting() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let inter_start = TOP_BLANK_LINES + ASCII_LOGO.len();
        for i in 0..INTER_BLANK_LINES {
            let blank = &lines[inter_start + i];
            assert_eq!(
                collect_text(blank),
                "",
                "logo 后第 {} 行应为空行",
                i + 1
            );
        }
    }

    #[test]
    fn render_welcome_greeting_is_last_line_and_indented() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let last = lines.last().expect("welcome 不能为空");
        let content = &last.spans[0].content;
        // 问候语左缩进 GREETING_INDENT（4 空格），紧跟一句本地化文案
        assert!(
            content.starts_with(GREETING_INDENT),
            "问候行应以 `{GREETING_INDENT}` 缩进起首；实际：{:?}",
            content
        );
        assert!(
            content.len() > GREETING_INDENT.len(),
            "问候行除缩进外还应有本地化文本；实际：{:?}",
            content
        );
    }

    #[test]
    fn render_welcome_greeting_is_dim() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let last = lines.last().expect("welcome 不能为空");
        assert_eq!(
            last.spans[0].style.fg,
            Some(Theme::INACTIVE),
            "问候行应使用 dim 色（INACTIVE 灰）"
        );
    }

    #[test]
    fn render_welcome_logo_lines_have_uniform_width() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let start = TOP_BLANK_LINES;
        let widths: Vec<usize> = (start..start + ASCII_LOGO.len())
            .map(|i| collect_text(&lines[i]).chars().count())
            .collect();
        let first = widths[0];
        for (i, w) in widths.iter().enumerate() {
            assert_eq!(
                *w,
                first,
                "logo 第 {} 行宽度应等于首行宽度 {}；实际 {}",
                i + 1,
                first,
                w
            );
        }
    }
}
