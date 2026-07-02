//! TUI 欢迎页面（聊天区为空时显示）
//!
//! 极简布局：
//! - `WYJ-CODE` 阴影块状艺术字（figlet ANSI Shadow 风格，6 行高，主题橙 + BOLD）
//! - 空 2 行
//! - `欢迎回来`（灰色 dim，走 i18n）
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

/// 生成欢迎页所有 Line（不含自动垂直居中，调用方负责）。
///
/// 布局（自上而下）：
/// - 6 行 logo（橙色 + BOLD）
/// - 空 2 行
/// - `欢迎回来`（dim，走 i18n）
pub fn render_welcome(_ctx: &WelcomeContext, _area_width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(ASCII_LOGO.len() + 3);

    // 1) logo 6 行（橙色 + BOLD）
    let logo_style = Style::default()
        .fg(Theme::CLAUDE)
        .add_modifier(Modifier::BOLD);
    for row in ASCII_LOGO {
        lines.push(Line::from(Span::styled(row.to_string(), logo_style)));
    }

    // 2) 空 2 行
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    // 3) 「欢迎回来」/「Welcome back」（i18n）
    lines.push(Line::from(Span::styled(
        wyj_i18n::tr("welcome.greeting"),
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
    fn render_welcome_first_line_is_logo() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let first = lines.first().expect("welcome 不能为空");
        assert!(
            first.spans[0].content.contains("██"),
            "首行应是 logo 第一行（含 ██ 块字符）；实际：{:?}",
            first.spans[0].content
        );
    }

    #[test]
    fn render_welcome_total_lines_is_logo_plus_three() {
        // logo 6 行 + 2 空行 + 1 问候 = 9 行
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        assert_eq!(
            lines.len(),
            ASCII_LOGO.len() + 3,
            "欢迎页应固定 {} 行；实际 {} 行",
            ASCII_LOGO.len() + 3,
            lines.len()
        );
    }

    #[test]
    fn render_welcome_logo_has_six_rows() {
        // 总行数 = 9，但 logo 本身固定 6 行
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let logo_lines = &lines[..ASCII_LOGO.len()];
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
    fn render_welcome_logo_first_row_starts_at_column_zero() {
        // 第一行绝对贴左（无前导空格），是 logo 在视觉上对齐的标志。
        // logo 内部行（含第 6 行）允许有内部空格，但首行贴顶。
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let first = &lines[0];
        let content = &first.spans[0].content;
        assert!(
            !content.starts_with(' '),
            "logo 第 1 行应贴左（无前导空格）；实际：{:?}",
            content
        );
    }

    #[test]
    fn render_welcome_logo_uses_claude_color() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let logo = &lines[0];
        assert_eq!(
            logo.spans[0].style.fg,
            Some(Theme::CLAUDE),
            "logo 应使用主题橙 CLAUDE"
        );
        assert!(
            logo.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "logo 应为 BOLD"
        );
    }

    #[test]
    fn render_welcome_includes_two_blank_lines_between_logo_and_greeting() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let blank1 = &lines[ASCII_LOGO.len()];
        let blank2 = &lines[ASCII_LOGO.len() + 1];
        assert_eq!(collect_text(blank1), "", "logo 后第 1 行应为空行");
        assert_eq!(collect_text(blank2), "", "logo 后第 2 行应为空行");
    }

    #[test]
    fn render_welcome_greeting_is_last_line() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let last = lines.last().expect("welcome 不能为空");
        assert!(!last.spans[0].content.is_empty(), "问候行内容不能为空");
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
        let widths: Vec<usize> = lines
            .iter()
            .take(ASCII_LOGO.len())
            .map(|l| collect_text(l).chars().count())
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
