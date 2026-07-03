//! TUI 欢迎页面（聊天区为空时显示）
//!
//! 整体布局（自上而下）：
//!
//! - 顶部 1 行留白（让 logo 落在聊天区上 1/3 位置）
//! - `WYJ-CODE` standard 字体艺术字（6 行 × 52 列，靠左对齐）
//!   - 横向 RGB 渐变（橙 215,119,87 → 中间橙黄 225,160,85 → 暖黄 240,200,80）
//!   - BOLD 强调，无背景填充（避免与下方终端默认背景形成突兀色块）
//!   - 左缩进 `INFO_INDENT`（2 空格），与下方信息行共用同一左基准
//! - 1 行间隔
//! - `Profile · Model`（左对齐，dim 灰）
//! - `‹ cwd ›`（左对齐，dim 灰）
//! - 1 行间隔
//! - 💡 tip 行（进程启动时固定选定一条，dim 琥珀色）
//! - 示例提问引导行（dim + 斜体）
//! - 1 行间隔
//! - 快捷键速查行（dim，窄终端下按优先级渐进丢弃）
//!
//! 渲染入口 [`render_welcome`]，由 `crates/tui/src/render.rs` 的
//! `draw_chat` 在 `messages.is_empty() && streaming_buf.is_empty()` 时调用。

use crate::render::{char_display_width, truncate_line};
use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wyj_i18n::tr;

/// 欢迎页所需的运行时上下文（由调用方从 `AppState` 投影而来）
///
/// - `model`：当前激活的 model 名称（已 fallback 到 profile 默认）
/// - `cwd`：工作目录（建议调用方先缩短为 `~/...` 形式）
/// - `profile`：当前激活的 Profile 名；为 None 或 `"default"` 时省略该段
/// - `tip_index`：稳定的 tip 索引，由 `AppState::new()` 用 `pick_tip_index`
///   在进程启动时生成一次并终身不变；`render_welcome` 内部对 `TIPS.len()`
///   防御性取模，调用方传入任意值都不会 panic。
pub struct WelcomeContext {
    pub model: String,
    pub cwd: String,
    pub profile: Option<String>,
    pub tip_index: usize,
}

/// 顶部留白行数：让 logo 落在聊天区上 1/3 而不是几何中心。
const TOP_BLANK_LINES: usize = 1;
/// logo 与 Profile/Model 信息行之间的间隔行数。
const LOGO_INFO_BLANK_LINES: usize = 1;
/// Profile/CWD 信息行与 tip 行之间的间隔行数。
const INFO_TIP_BLANK_LINES: usize = 1;
/// tip/示例提问行与快捷键速查行之间的间隔行数。
const TIP_SHORTCUT_BLANK_LINES: usize = 1;
/// 信息行左缩进：与 logo 和下方信息行的左对齐基准一致，
/// 让所有欢迎页元素在同一列起首（统一靠左）。
const INFO_INDENT: &str = "  ";
/// 隐藏默认 Profile 名：仅当 profile 名非空且不是 "default" 时才显示。
const DEFAULT_PROFILE: &str = "default";

/// 轮播 tips 的 i18n key 列表（文案见 `crates/i18n/locales/{zh,en}.yml` 的 `welcome.tip_*`）
const TIPS: &[&str] = &[
    "welcome.tip_0",
    "welcome.tip_1",
    "welcome.tip_2",
    "welcome.tip_3",
    "welcome.tip_4",
];

/// 快捷键速查行的 i18n key，按优先级从高到低排列（窄终端降级时从末尾开始丢弃）
const SHORTCUT_HINTS: &[&str] = &[
    "welcome.hint.help",
    "welcome.hint.mode",
    "welcome.hint.compact",
    "welcome.hint.quit",
];

/// 根据外部熵值选取一个 tip 索引，纯函数、无副作用，便于单测。
pub fn pick_tip_index(seed: u64) -> usize {
    (seed as usize) % TIPS.len()
}

/// WYJ-CODE 艺术字（figlet standard 字体，6 行高 × 52 列宽）
///
/// 字符来源：本地 figlet `standard.flf` 渲染 "WYJ-CODE" 输出，
/// 每行严格 52 列；第 6 行为空行（与 figlet 一致，保留原字体形状）。
const ASCII_LOGO: &[&str] = &[
    "__        ____   __  _        ____ ___  ____  _____ ",
    "\\ \\      / /\\ \\ / / | |      / ___/ _ \\|  _ \\| ____|",
    " \\ \\ /\\ / /  \\ V /  | |_____| |  | | | | | | |  _|  ",
    "  \\ V  V /    | | |_| |_____| |__| |_| | |_| | |___ ",
    "   \\_/\\_/     |_|\\___/       \\____\\___/|____/|_____|",
    "                                                    ",
];

/// 生成欢迎页所有 Line
///
/// 布局（自上而下）：
/// - 顶部 `TOP_BLANK_LINES` 行留白
/// - logo 行（按列渐变橙→黄，靠左对齐，左缩进 `INFO_INDENT`）
/// - `LOGO_INFO_BLANK_LINES` 行间隔
/// - 1 行 Profile/Model（左缩进 `INFO_INDENT`，dim 灰）
/// - 1 行 CWD（左缩进 `INFO_INDENT`，dim 灰，‹ › 包裹）
/// - `INFO_TIP_BLANK_LINES` 行间隔
/// - 1 行 tip（💡 + 轮播文案，dim 琥珀色）
/// - 1 行示例提问引导（dim + 斜体）
/// - `TIP_SHORTCUT_BLANK_LINES` 行间隔
/// - 1 行快捷键速查（dim，窄终端下按优先级渐进丢弃 segment）
pub fn render_welcome(ctx: &WelcomeContext, area_width: u16) -> Vec<Line<'static>> {
    let total = TOP_BLANK_LINES
        + ASCII_LOGO.len()
        + LOGO_INFO_BLANK_LINES
        + 2
        + INFO_TIP_BLANK_LINES
        + 1
        + 1
        + TIP_SHORTCUT_BLANK_LINES
        + 1;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total);

    // 0) 顶部留白（让 logo 落在聊天区上 1/3）
    for _ in 0..TOP_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 1) logo 行（按可用宽度截断 + 横向渐变 fg + BOLD）
    //
    // 靠左对齐：使用 INFO_INDENT 缩进，与下方 Profile/Model、CWD 信息行共用
    // 同一左基准，让 logo 与文字在视觉层上同列起首。
    let logo_width = ASCII_LOGO[0].chars().count();
    let available = area_width as usize;
    let visible_logo_width = logo_width.min(available.saturating_sub(INFO_INDENT.len()));
    let mut spans = logo_line_spans(ASCII_LOGO[0], visible_logo_width);
    spans.insert(0, Span::raw(INFO_INDENT));
    let first_logo_row = Line::from(spans);

    // logo 第 1 行已经构好，剩余行复用同缩进 + 各自可见宽度的渐变 spans
    let mut logo_lines: Vec<Line<'static>> = Vec::with_capacity(ASCII_LOGO.len());
    logo_lines.push(first_logo_row);
    for row in ASCII_LOGO.iter().skip(1) {
        let mut row_spans = logo_line_spans(row, visible_logo_width);
        row_spans.insert(0, Span::raw(INFO_INDENT));
        logo_lines.push(Line::from(row_spans));
    }
    lines.extend(logo_lines);

    // 2) logo 与信息行之间的间隔
    for _ in 0..LOGO_INFO_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 3) Profile/Model 行（左缩进）
    let profile_text = format_profile_line(&ctx.model, ctx.profile.as_deref());
    lines.push(Line::from(Span::styled(
        format!("{INFO_INDENT}{}", profile_text),
        Theme::dim(),
    )));

    // 4) CWD 行（左缩进，‹ › 包裹）
    let cwd_text = shorten_cwd(&ctx.cwd);
    lines.push(Line::from(Span::styled(
        format!("{INFO_INDENT}‹ {cwd_text} ›"),
        Theme::dim(),
    )));

    // 5) 信息行与 tip 行之间的间隔
    for _ in 0..INFO_TIP_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 6) tip 行（💡 + 轮播文案，越界索引防御性取模）
    let available_width = (area_width as usize).saturating_sub(INFO_INDENT.len());
    let tip_key = TIPS[ctx.tip_index % TIPS.len()];
    let tip_text = truncate_line(&tr(tip_key), available_width.saturating_sub(2));
    lines.push(Line::from(Span::styled(
        format!("{INFO_INDENT}💡 {tip_text}"),
        Theme::welcome_tip(),
    )));

    // 7) 示例提问引导行
    let placeholder_text = truncate_line(&tr("welcome.placeholder"), available_width);
    lines.push(Line::from(Span::styled(
        format!("{INFO_INDENT}{placeholder_text}"),
        Theme::welcome_suggestion(),
    )));

    // 8) tip/示例提问行与快捷键行之间的间隔
    for _ in 0..TIP_SHORTCUT_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 9) 快捷键速查行（窄终端下按优先级渐进丢弃 segment）
    let shortcuts_text = shortcuts_line(available_width);
    lines.push(Line::from(Span::styled(
        format!("{INFO_INDENT}{shortcuts_text}"),
        Theme::dim(),
    )));

    lines
}

/// 生成快捷键速查行文本：按 `SHORTCUT_HINTS` 优先级从高到低尝试拼接，
/// 放不下时从最低优先级（数组末尾）开始依次丢弃 segment；
/// 若连 1 个 segment 都放不下，返回空字符串（该行整体空白占位）。
fn shortcuts_line(available_width: usize) -> String {
    let hints: Vec<String> = SHORTCUT_HINTS.iter().map(|key| tr(key)).collect();
    for take in (1..=hints.len()).rev() {
        let candidate = hints[..take].join(" · ");
        let width: usize = candidate.chars().map(char_display_width).sum();
        if width <= available_width {
            return candidate;
        }
    }
    String::new()
}

/// 计算 logo 第 `col` 列的 RGB 颜色（在 [START..END] 范围内线性插值）
///
/// `total_cols` 为 logo 实际可见列数（窄终端截断后的宽度），
/// 渐变在可见宽度内均匀过渡，避免窄终端下渐变被压缩或截断。
///
/// 边界点（col=0 与 col=total_cols-1）直接返回 START/END 常量本身，
/// 避免 f32 插值 + round 引入 ±1 误差导致测试断言失败。
fn logo_color_at(col: usize, total_cols: usize) -> Color {
    if total_cols <= 1 {
        return Theme::WELCOME_LOGO_GRADIENT_MID;
    }
    if col == 0 {
        return Theme::WELCOME_LOGO_GRADIENT_START;
    }
    if col == total_cols - 1 {
        return Theme::WELCOME_LOGO_GRADIENT_END;
    }

    let (sr, sg, sb) = match Theme::WELCOME_LOGO_GRADIENT_START {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (215.0, 119.0, 87.0),
    };
    let (mr, mg, mb) = match Theme::WELCOME_LOGO_GRADIENT_MID {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (225.0, 160.0, 85.0),
    };
    let (er, eg, eb) = match Theme::WELCOME_LOGO_GRADIENT_END {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (240.0, 200.0, 80.0),
    };
    let t = col as f32 / (total_cols - 1) as f32;
    // 两段线性插值：0..0.5 用 START→MID，0.5..1 用 MID→END
    let (r, g, b) = if t <= 0.5 {
        let k = t * 2.0;
        (sr + (mr - sr) * k, sg + (mg - sg) * k, sb + (mb - sb) * k)
    } else {
        let k = (t - 0.5) * 2.0;
        (mr + (er - mr) * k, mg + (eg - mg) * k, mb + (eb - mb) * k)
    };
    Color::Rgb(r.round() as u8, g.round() as u8, b.round() as u8)
}

/// 生成单行 logo 的 spans 列表（每非空字符一个 span，fg 横向渐变色 + BOLD）
///
/// 不再为整行添加背景色（避免亮色 fg 被亮色 bg 掩盖，导致艺术字看不清）。
/// 空格字符也不带任何 style，沿用终端默认背景。
///
/// 窄终端截断：若 `available_width` < logo 列数，按可见宽度切分右侧字符。
fn logo_line_spans(row: &str, available_width: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = row.chars().collect();
    let visible = chars.len().min(available_width.max(1));

    // 计算参与渐变的总列数：取可见范围内最后一个非空格字符的列号 + 1。
    // 若可见范围内全空（如 logo 第 5 行就是纯空格），渐变列数为 1（取中点即可）。
    let last_non_space = chars
        .iter()
        .take(visible)
        .rposition(|c| *c != ' ')
        .unwrap_or(0);
    let gradient_cols = last_non_space + 1;

    chars
        .into_iter()
        .take(visible)
        .enumerate()
        .map(|(col, ch)| {
            if ch == ' ' {
                // 空格不带任何 style：默认 fg + 默认 bg（终端原色）
                Span::raw(ch.to_string())
            } else {
                let fg = logo_color_at(col, gradient_cols);
                Span::styled(
                    ch.to_string(),
                    Style::default().fg(fg).add_modifier(Modifier::BOLD),
                )
            }
        })
        .collect()
}

/// 格式化 Profile/Model 行（profile 为 None 或 default 时省略 profile 段）
fn format_profile_line(model: &str, profile: Option<&str>) -> String {
    match profile {
        Some(p) if p != DEFAULT_PROFILE => format!("{p} · {model}"),
        _ => model.to_string(),
    }
}

/// 缩短 CWD 路径：`/Users/foo/bar` → `~/bar`（如 `$HOME` 为 `/Users/foo`）
fn shorten_cwd(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx() -> WelcomeContext {
        WelcomeContext {
            model: "claude-opus-4-8".to_string(),
            cwd: "/Users/foo/projects/bar".to_string(),
            profile: Some("default".to_string()),
            tip_index: 0,
        }
    }

    fn sample_ctx_with_profile() -> WelcomeContext {
        WelcomeContext {
            model: "glm-5.2".to_string(),
            cwd: "/Users/foo".to_string(),
            profile: Some("glm-cheap".to_string()),
            tip_index: 0,
        }
    }

    fn collect_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn render_welcome_total_lines_is_fixed() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let expected = TOP_BLANK_LINES
            + ASCII_LOGO.len()
            + LOGO_INFO_BLANK_LINES
            + 2
            + INFO_TIP_BLANK_LINES
            + 1
            + 1
            + TIP_SHORTCUT_BLANK_LINES
            + 1;
        assert_eq!(
            lines.len(),
            expected,
            "欢迎页应固定 {} 行（顶留白 {TOP_BLANK_LINES} + logo {} + 间隔 {LOGO_INFO_BLANK_LINES} + 信息 2 + 间隔 {INFO_TIP_BLANK_LINES} + tip 1 + 示例 1 + 间隔 {TIP_SHORTCUT_BLANK_LINES} + 快捷键 1）",
            expected,
            ASCII_LOGO.len()
        );
    }

    #[test]
    fn render_welcome_first_lines_are_blank_padding() {
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
    fn render_welcome_logo_has_five_rows_with_consistent_width() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let start = TOP_BLANK_LINES;
        let end = start + ASCII_LOGO.len();
        let logo_lines = &lines[start..end];
        assert_eq!(
            logo_lines.len(),
            ASCII_LOGO.len(),
            "logo 行数应与 ASCII_LOGO 一致"
        );

        // 每行可见字符数应一致（figlet shadow 是固定宽字体）
        let first_visible = logo_lines[0]
            .spans
            .iter()
            .filter(|s| !s.content.is_empty())
            .map(|s| s.content.chars().count())
            .sum::<usize>();
        for (i, line) in logo_lines.iter().enumerate() {
            let visible = line
                .spans
                .iter()
                .filter(|s| !s.content.is_empty())
                .map(|s| s.content.chars().count())
                .sum::<usize>();
            assert_eq!(
                visible,
                first_visible,
                "logo 第 {} 行可见宽度应等于首行宽度 {}；实际 {}",
                i + 1,
                first_visible,
                visible
            );
        }
    }

    #[test]
    fn render_welcome_logo_has_no_background_fill() {
        // 去掉整块背景后，logo 所有 span 的 bg 都应保持为 None，
        // 避免亮色 fg 被亮色 bg 掩盖导致艺术字看不清。
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let first_logo_row = &lines[TOP_BLANK_LINES];
        for span in &first_logo_row.spans {
            assert_eq!(
                span.style.bg, None,
                "logo span 不应填充背景色；content={:?}",
                span.content
            );
        }
    }

    #[test]
    fn render_welcome_logo_uses_gradient_fg_on_non_blank_chars() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        // 用 logo 第 1 行做渐变起止验证——首字符 fg 应等于 START，
        // 最后一个非空字符 fg 应等于 END
        let first_logo_row = &lines[TOP_BLANK_LINES];
        let non_blank: Vec<&Span> = first_logo_row
            .spans
            .iter()
            .filter(|s| {
                // 过滤掉空格字符（Span::raw，无 style）
                s.content.as_ref() != " " && s.style.fg.is_some()
            })
            .collect();
        assert!(non_blank.len() >= 2, "logo 首行至少 2 个有渐变 fg 的字符");
        assert_eq!(
            non_blank.first().unwrap().style.fg,
            Some(Theme::WELCOME_LOGO_GRADIENT_START),
            "logo 首字符 fg 应等于渐变起点"
        );
        assert_eq!(
            non_blank.last().unwrap().style.fg,
            Some(Theme::WELCOME_LOGO_GRADIENT_END),
            "logo 末字符 fg 应等于渐变终点"
        );
    }

    #[test]
    fn render_welcome_logo_mid_color_is_between_start_and_end() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let first_logo_row = &lines[TOP_BLANK_LINES];
        let non_blank: Vec<&Span> = first_logo_row
            .spans
            .iter()
            .filter(|s| s.content.as_ref() != " ")
            .collect();
        let mid = non_blank.len() / 2;
        let mid_color = non_blank[mid].style.fg.expect("中点 span 应有 fg");
        let (r, g, b) = match mid_color {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("中点 fg 应为 RGB"),
        };
        let (sr, sg, sb) = match Theme::WELCOME_LOGO_GRADIENT_START {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => unreachable!(),
        };
        let (er, eg, eb) = match Theme::WELCOME_LOGO_GRADIENT_END {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => unreachable!(),
        };
        assert!(
            r >= sr.min(er).saturating_sub(5) && r <= sr.max(er).saturating_add(5),
            "中点 R {} 应在 [{}, {}] 范围内",
            r,
            sr,
            er
        );
        assert!(
            g >= sg.min(eg).saturating_sub(5) && g <= sg.max(eg).saturating_add(5),
            "中点 G {} 应在 [{}, {}] 范围内",
            g,
            sg,
            eg
        );
        assert!(
            b >= sb.min(eb).saturating_sub(5) && b <= sb.max(eb).saturating_add(5),
            "中点 B {} 应在 [{}, {}] 范围内",
            b,
            sb,
            eb
        );
    }

    #[test]
    fn render_welcome_blank_line_between_logo_and_info() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let inter_start = TOP_BLANK_LINES + ASCII_LOGO.len();
        for i in 0..LOGO_INFO_BLANK_LINES {
            let blank = &lines[inter_start + i];
            assert_eq!(collect_text(blank), "", "logo 后第 {} 行应为空行", i + 1);
        }
    }

    #[test]
    fn render_welcome_profile_line_uses_dim_style() {
        let ctx = sample_ctx_with_profile();
        let lines = render_welcome(&ctx, 120);
        let profile_line = &lines[TOP_BLANK_LINES + ASCII_LOGO.len() + LOGO_INFO_BLANK_LINES];
        assert_eq!(
            profile_line.spans[0].style.fg,
            Some(Theme::INACTIVE),
            "Profile/Model 行应使用 dim 色（INACTIVE 灰）"
        );
    }

    #[test]
    fn render_welcome_profile_line_omits_default_profile() {
        let ctx = sample_ctx(); // profile = "default"
        let lines = render_welcome(&ctx, 120);
        let profile_line_idx = TOP_BLANK_LINES + ASCII_LOGO.len() + LOGO_INFO_BLANK_LINES;
        let text = collect_text(&lines[profile_line_idx]);
        assert!(
            !text.contains("default"),
            "default profile 应被省略；实际：{:?}",
            text
        );
        assert!(
            text.contains("claude-opus-4-8"),
            "model 名应保留；实际：{:?}",
            text
        );
    }

    #[test]
    fn render_welcome_profile_line_includes_custom_profile() {
        let ctx = sample_ctx_with_profile(); // profile = "glm-cheap"
        let lines = render_welcome(&ctx, 120);
        let profile_line_idx = TOP_BLANK_LINES + ASCII_LOGO.len() + LOGO_INFO_BLANK_LINES;
        let text = collect_text(&lines[profile_line_idx]);
        assert!(
            text.contains("glm-cheap") && text.contains("glm-5.2"),
            "非默认 profile 应同时显示 profile 名与 model；实际：{:?}",
            text
        );
    }

    #[test]
    fn render_welcome_cwd_line_uses_bracketed_format() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let cwd_line_idx = TOP_BLANK_LINES + ASCII_LOGO.len() + LOGO_INFO_BLANK_LINES + 1;
        let text = collect_text(&lines[cwd_line_idx]);
        assert!(
            text.contains("‹ ") && text.contains(" ›"),
            "CWD 行应使用 ‹ › 包裹；实际：{:?}",
            text
        );
        assert!(
            text.contains("/Users/foo/projects/bar") || text.contains("~/projects/bar"),
            "CWD 行应包含完整或缩短的路径；实际：{:?}",
            text
        );
    }

    #[test]
    fn render_welcome_info_lines_are_left_indented() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let base = TOP_BLANK_LINES + ASCII_LOGO.len() + LOGO_INFO_BLANK_LINES;
        let profile_text = collect_text(&lines[base]);
        let cwd_text = collect_text(&lines[base + 1]);
        assert!(
            profile_text.starts_with(INFO_INDENT),
            "Profile/Model 行应以 `{}` 缩进起首；实际：{:?}",
            INFO_INDENT,
            profile_text
        );
        assert!(
            cwd_text.starts_with(INFO_INDENT),
            "CWD 行应以 `{}` 缩进起首；实际：{:?}",
            INFO_INDENT,
            cwd_text
        );
    }

    #[test]
    fn render_welcome_narrows_logo_when_terminal_is_narrow() {
        // 窄终端（width < 25）下 logo 应被截断，不溢出
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 10);
        let first_logo_row = &lines[TOP_BLANK_LINES];
        let visible: usize = first_logo_row
            .spans
            .iter()
            .filter(|s| !s.content.is_empty())
            .map(|s| s.content.chars().count())
            .sum();
        assert!(
            visible <= 10,
            "窄终端下 logo 可见宽度应 <= 10；实际：{}",
            visible
        );
    }

    #[test]
    fn format_profile_line_omits_default() {
        assert_eq!(format_profile_line("m1", Some("default")), "m1");
        assert_eq!(format_profile_line("m1", None), "m1");
        assert_eq!(
            format_profile_line("m1", Some("glm-cheap")),
            "glm-cheap · m1"
        );
    }

    #[test]
    fn shorten_cwd_replaces_home_with_tilde() {
        std::env::set_var("HOME", "/Users/foo");
        assert_eq!(shorten_cwd("/Users/foo/projects/bar"), "~/projects/bar");
        assert_eq!(shorten_cwd("/Users/foo"), "~");
        assert_eq!(
            shorten_cwd("/tmp/somewhere"),
            "/tmp/somewhere",
            "非 HOME 下的路径不应被替换"
        );
    }

    #[test]
    fn logo_color_at_boundaries() {
        // 边界条件：col=0 应返回 START，col=total_cols-1 应返回 END，total_cols<=1 取中点
        assert_eq!(logo_color_at(0, 10), Theme::WELCOME_LOGO_GRADIENT_START);
        assert_eq!(logo_color_at(9, 10), Theme::WELCOME_LOGO_GRADIENT_END);
        let single = logo_color_at(0, 1);
        let (r, g, b) = match single {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("单列应返回 RGB"),
        };
        let (mr, mg, mb) = match Theme::WELCOME_LOGO_GRADIENT_MID {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => unreachable!(),
        };
        assert!(
            (r as i32 - mr as i32).abs() <= 5
                && (g as i32 - mg as i32).abs() <= 5
                && (b as i32 - mb as i32).abs() <= 5,
            "单列 fallback 应取中点 RGB；实际 ({}, {}, {}) 期望接近 ({}, {}, {})",
            r,
            g,
            b,
            mr,
            mg,
            mb
        );
    }

    fn tip_line_idx() -> usize {
        TOP_BLANK_LINES + ASCII_LOGO.len() + LOGO_INFO_BLANK_LINES + 2 + INFO_TIP_BLANK_LINES
    }

    fn placeholder_line_idx() -> usize {
        tip_line_idx() + 1
    }

    fn shortcuts_line_idx() -> usize {
        placeholder_line_idx() + 1 + TIP_SHORTCUT_BLANK_LINES
    }

    #[test]
    fn render_welcome_tip_line_contains_selected_tip_text() {
        let mut ctx = sample_ctx();
        ctx.tip_index = 2;
        let lines = render_welcome(&ctx, 120);
        let text = collect_text(&lines[tip_line_idx()]);
        assert!(
            text.contains(&tr(TIPS[2])),
            "tip 行应包含 tip_index=2 对应的文案；实际：{:?}",
            text
        );
    }

    #[test]
    fn render_welcome_tip_index_wraps_out_of_range() {
        let mut ctx = sample_ctx();
        ctx.tip_index = 999;
        let lines = render_welcome(&ctx, 120);
        let text = collect_text(&lines[tip_line_idx()]);
        let expected = tr(TIPS[999 % TIPS.len()]);
        assert!(
            text.contains(&expected),
            "越界 tip_index 应对 TIPS.len() 取模；实际：{:?}，期望包含：{:?}",
            text,
            expected
        );
    }

    #[test]
    fn render_welcome_placeholder_suggestion_line_present() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let text = collect_text(&lines[placeholder_line_idx()]);
        assert!(
            text.contains(&tr("welcome.placeholder")),
            "示例提问行应包含 welcome.placeholder 文案；实际：{:?}",
            text
        );
    }

    #[test]
    fn render_welcome_shortcuts_row_present() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let text = collect_text(&lines[shortcuts_line_idx()]);
        assert!(
            text.contains(&tr("welcome.hint.help")),
            "快捷键行应包含 help 提示；实际：{:?}",
            text
        );
        assert!(
            text.contains(&tr("welcome.hint.mode")),
            "快捷键行应包含 mode 切换提示；实际：{:?}",
            text
        );
    }

    #[test]
    fn render_welcome_shortcuts_row_degrades_on_narrow_width() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 20);
        let text = collect_text(&lines[shortcuts_line_idx()]);
        let width: usize = text.chars().map(char_display_width).sum();
        assert!(
            width <= 20,
            "极窄终端下快捷键行不应超宽；实际宽度 {}：{:?}",
            width,
            text
        );
    }

    #[test]
    fn pick_tip_index_always_in_bounds() {
        for seed in [0u64, u64::MAX, 12345] {
            assert!(pick_tip_index(seed) < TIPS.len());
        }
    }

    #[test]
    fn render_welcome_total_lines_stable_across_all_tips() {
        let base_total = render_welcome(&sample_ctx(), 120).len();
        for idx in 0..TIPS.len() {
            let mut ctx = sample_ctx();
            ctx.tip_index = idx;
            let lines = render_welcome(&ctx, 120);
            assert_eq!(
                lines.len(),
                base_total,
                "tip_index={idx} 不应改变总行数；实际 {}",
                lines.len()
            );
        }
    }
}
