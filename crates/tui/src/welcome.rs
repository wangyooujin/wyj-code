//! TUI 欢迎页面（会话尚无真实对话时显示，参见 [`render_welcome`] 文档）
//!
//! 整体布局（自上而下）：
//!
//! - 顶部 1 行留白（让 logo 落在聊天区上 1/3 位置）
//! - `WYJ-CODE` standard 字体艺术字（5 行 × 52 列，靠左对齐）
//!   - 横向 RGB 渐变（品牌橙 → 中间橙黄 → 暖黄）
//!   - BOLD 强调，无背景填充（避免与下方终端默认背景形成突兀色块）
//!   - 左缩进 `CONTENT_INDENT`（2 空格），与聊天区助手正文同列起首
//! - 版本号行（紧贴 logo 下方，左缩进，dim 灰）
//! - 2 行间隔（把「品牌区」与「操作提示区」切成两块）
//! - 💡 tip 行（进程启动时固定选定一条，dim 琥珀色）
//! - 快捷键速查行（dim，窄终端下按优先级渐进丢弃）
//!
//! ## 为什么页面上没有 model / cwd
//!
//! 两者在**状态栏**里已经常驻（`◆ model · [mode] · cwd`，每帧都在，见
//! `render::draw_status`）。欢迎页再写一遍就是同一个事实占两个位置：用户读
//! 状态栏时它被欢迎页的同名文本稀释，欢迎页因此多占两行却没多给信息。
//! 状态栏是「我现在在哪 / 我以什么身份在跑」的常驻位置，欢迎页退回
//! 「这是什么 / 从哪开始」——**品牌区 + 操作提示区**两块，职责不重叠。
//!
//! 渲染入口 [`render_welcome`]，由 `crates/tui/src/render.rs` 的
//! `draw_chat` 在会话尚无真实对话（消息列表为空，或仅含 System 角色的系统提示，
//! 如 MCP 连接通知）且未在流式输出时调用，行内容会被拼进消息列表顶部一起渲染。

use crate::render::{char_display_width, truncate_line};
use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wyj_i18n::{tr, tr_fmt};

/// 欢迎页所需的运行时上下文（由调用方从 `AppState` 投影而来）
///
/// - `tip_index`：稳定的 tip 索引，由 `AppState::new()` 用 `pick_tip_index`
///   在进程启动时生成一次并终身不变；`render_welcome` 内部对 `TIPS.len()`
///   防御性取模，调用方传入任意值都不会 panic。
pub struct WelcomeContext {
    pub tip_index: usize,
}

/// 顶部留白行数：让 logo 落在聊天区上 1/3 而不是几何中心。
const TOP_BLANK_LINES: usize = 1;
/// logo/版本号区与 tip/快捷键区之间的间隔行数。
///
/// 2 行而不是 1 行：这是全页唯一的**分组边界**，把「品牌标识」与「操作提示」
/// 切成两块。1 行间隔在 5 行高的 logo 之后会被读成 logo 的一部分（figlet
/// 艺术字本身是 5 行实心块，紧跟一行文字会像 logo 的下沿），2 行才拉开层次。
const SECTION_BLANK_LINES: usize = 2;
/// 内容左缩进：与聊天区助手正文（`markdown::Ctx::block_indent` 的 base=2）
/// 同列起首，欢迎页与后续第一条消息因此在同一条竖线上。
const CONTENT_INDENT: &str = "  ";
/// tip 行的图标前缀（emoji + 空格），宽度按 2 列算（emoji 在终端里是双宽）。
const TIP_ICON: &str = "💡 ";
/// tip 图标占用的显示列数：emoji 本体 2 列 + 后随空格 1 列。
const TIP_ICON_WIDTH: usize = 3;

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

/// WYJ-CODE 艺术字（figlet standard 字体，5 行高 × 52 列宽）
///
/// 字符来源：本地 figlet `standard.flf` 渲染 "WYJ-CODE" 输出，每行严格 52 列。
/// figlet 原输出还有第 6 行（整行空格），这里**刻意不收进来**——它不是字形，
/// 只是一条尾部空行；留着会让 logo 与紧随其下的版本号行之间凭空多出一行
/// 无意义的空白，而"logo 与版本号同属品牌区、应紧密相邻"的意图反被削弱。
/// 品牌区与提示区之间的分隔由 [`SECTION_BLANK_LINES`] 负责。
const ASCII_LOGO: &[&str] = &[
    "__        ____   __  _        ____ ___  ____  _____ ",
    "\\ \\      / /\\ \\ / / | |      / ___/ _ \\|  _ \\| ____|",
    " \\ \\ /\\ / /  \\ V /  | |_____| |  | | | | | | |  _|  ",
    "  \\ V  V /    | | |_| |_____| |__| |_| | |_| | |___ ",
    "   \\_/\\_/     |_|\\___/       \\____\\___/|____/|_____|",
];

/// 生成欢迎页所有 Line
///
/// 布局（自上而下）：
/// - 顶部 `TOP_BLANK_LINES` 行留白
/// - logo 行（按列渐变橙→黄，靠左对齐，左缩进 `CONTENT_INDENT`）
/// - 1 行版本号（左缩进 `CONTENT_INDENT`，dim 灰，紧贴 logo 下方）
/// - `SECTION_BLANK_LINES` 行间隔
/// - 1 行 tip（💡 + 轮播文案，dim 琥珀色）
/// - 1 行快捷键速查（dim，窄终端下按优先级渐进丢弃 segment）
///
/// **宽度契约**：返回的每一行显示宽度都 `<= area_width`。旧实现有三处溢出
/// （CWD 行完全没截断、tip 行只给图标留了 2 列却要 3 列、快捷键行按 segment
/// 整块丢弃导致要么超宽要么整行消失），窄终端下会被 Paragraph 从右侧硬切，
/// 切点落在半截字符上。
pub fn render_welcome(ctx: &WelcomeContext, area_width: u16) -> Vec<Line<'static>> {
    let total = TOP_BLANK_LINES + ASCII_LOGO.len() + 1 + SECTION_BLANK_LINES + 1 + 1;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total);

    // 整行宽度预算 = 终端宽度。**缩进本身也算在内**——旧实现把缩进当
    // "额外开销"、只对正文算预算，于是窄终端下 `缩进 + 图标` 那一段就
    // 超宽了（宽度 4 时 `  💡 ` 是 5 列）。窄到连缩进都放不下时，缩进也
    // 要让位。
    let budget = area_width as usize;
    let indent = &CONTENT_INDENT[..CONTENT_INDENT.len().min(budget)];
    let indent_width = indent.chars().count();
    // logo / 正文可用的显示列数。logo 是纯 ASCII 宽字符，char 数即显示宽度。
    let available = budget.saturating_sub(indent_width);

    // 0) 顶部留白（让 logo 落在聊天区上 1/3）
    for _ in 0..TOP_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 1) logo 行（按可用宽度裁剪 + 横向渐变 fg + BOLD）
    //
    // 靠左对齐：使用 CONTENT_INDENT 缩进，与聊天区助手正文同列起首。
    let mut logo_lines: Vec<Line<'static>> = Vec::with_capacity(ASCII_LOGO.len());
    for row in ASCII_LOGO.iter() {
        let mut row_spans = logo_line_spans(row, available);
        row_spans.insert(0, Span::raw(indent));
        logo_lines.push(Line::from(row_spans));
    }
    lines.extend(logo_lines);

    // 2) 版本号行：紧贴 logo 正下方、左缩进与其余内容同列起首。
    //
    // 版本号取编译期 `CARGO_PKG_VERSION`（workspace 统一继承，与 `/help`、
    // `/doctor` 显示的版本同源），无需从 AppState 透传。
    let version_text = tr_fmt("welcome.version", &[("version", env!("CARGO_PKG_VERSION"))]);
    lines.push(Line::from(Span::styled(
        format!("{indent}{}", truncate_line(&version_text, available)),
        Theme::dim(),
    )));

    // 3) 品牌区与提示区之间的间隔
    for _ in 0..SECTION_BLANK_LINES {
        lines.push(Line::from(""));
    }

    // 4) tip 行（💡 + 轮播文案，越界索引防御性取模）
    //
    // 宽度预算：整行 = 缩进 + 图标 + 文案，三者加起来不超过终端宽度。
    // 图标是双宽 emoji，**连同后随空格共 3 列**——旧实现只减了 2 列，
    // 于是每行都超宽 1 列，窄终端下正好把末尾字符切掉半个。放不下 3 列
    // 时整个图标省略，只留文案（宁可少个装饰，也不能让行溢出去）。
    let tip_key = TIPS[ctx.tip_index % TIPS.len()];
    let (tip_icon, tip_budget) = if available >= TIP_ICON_WIDTH {
        (TIP_ICON, available - TIP_ICON_WIDTH)
    } else {
        ("", available)
    };
    let tip_text = truncate_line(&tr(tip_key), tip_budget);
    lines.push(Line::from(Span::styled(
        format!("{indent}{tip_icon}{tip_text}"),
        Theme::welcome_tip(),
    )));

    // 5) 快捷键速查行（窄终端下按优先级渐进丢弃 segment）
    let shortcuts_text = shortcuts_line(available);
    lines.push(Line::from(Span::styled(
        format!("{indent}{shortcuts_text}"),
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
/// `total_cols` 为 logo 实际可见列数（窄终端裁剪后的宽度），
/// 渐变在可见宽度内均匀过渡，避免窄终端下渐变被压缩或截断。
///
/// 边界点（col=0 与 col=total_cols-1）直接返回 START/END 常量本身，
/// 避免 f32 插值 + round 引入 ±1 误差导致测试断言失败。
fn logo_color_at(col: usize, total_cols: usize) -> Color {
    if total_cols <= 1 {
        return Theme::welcome_logo_gradient_mid_color();
    }
    if col == 0 {
        return Theme::welcome_logo_gradient_start_color();
    }
    if col == total_cols - 1 {
        return Theme::welcome_logo_gradient_end_color();
    }

    // 分量与插值都交给 theme 层：这里只负责"第几列"，不该再自己 match RGB
    // 或写死品牌橙数字——否则渐变就成了全应用唯一一处绕过槽位的硬编码颜色。
    let (sr, sg, sb) = Theme::rgb_components_or(
        Theme::welcome_logo_gradient_start_color(),
        Theme::WELCOME_LOGO_GRADIENT_START,
    );
    let (mr, mg, mb) = Theme::rgb_components_or(
        Theme::welcome_logo_gradient_mid_color(),
        Theme::WELCOME_LOGO_GRADIENT_MID,
    );
    let (er, eg, eb) = Theme::rgb_components_or(
        Theme::welcome_logo_gradient_end_color(),
        Theme::WELCOME_LOGO_GRADIENT_END,
    );
    let t = col as f32 / (total_cols - 1) as f32;
    // 两段线性插值：0..0.5 用 START→MID，0.5..1 用 MID→END
    let (r, g, b) = if t <= 0.5 {
        let k = t * 2.0;
        (sr + (mr - sr) * k, sg + (mg - sg) * k, sb + (mb - sb) * k)
    } else {
        let k = (t - 0.5) * 2.0;
        (mr + (er - mr) * k, mg + (eg - mg) * k, mb + (eb - mb) * k)
    };
    Theme::rgb_from_components(r, g, b)
}

/// 窄终端下 logo 的**字母边界**（显示列宽），按前缀长度递增排列。
///
/// figlet 艺术字每个字母占一段固定列区间，硬切会把字母劈成两半（`V` 变成
/// `|`、`O` 变成半个圆），比整块裁掉更难认。窄终端下改为**只切到完整的
/// 字母边界**——宁可少显示几个字母，也不显示残缺的字形。
///
/// 数值由 `figlet -f standard` 逐前缀实测得出（去掉尾部空格后的行宽）：
/// `W`=13、`WY`=20、`WYJ`=23、`WYJ-`=23、`WYJ-C`=35、`WYJ-CO`=39、
/// `WYJ-COD`=45、`WYJ-CODE`=52。
const LOGO_LETTER_BOUNDARIES: &[usize] = &[13, 20, 23, 35, 39, 45, 52];

/// 把可用宽度收敛到最近的字母边界：能放下完整前缀就取最大的那个边界，
/// 一个字母都放不下（`available < 13`）时返回 0——此时 logo 整块不显示，
/// 留着半截字母不如留白。
fn snap_to_letter_boundary(available: usize) -> usize {
    LOGO_LETTER_BOUNDARIES
        .iter()
        .copied()
        .filter(|w| *w <= available)
        .max()
        .unwrap_or(0)
}

/// 生成单行 logo 的 spans 列表（每非空字符一个 span，fg 横向渐变色 + BOLD）
///
/// 不再为整行添加背景色（避免亮色 fg 被亮色 bg 掩盖，导致艺术字看不清）。
/// 空格字符也不带任何 style，沿用终端默认背景。
///
/// 窄终端裁剪：先按**显示列**裁到字母边界（[`snap_to_letter_boundary`]），
/// 再按列取字符。logo 是纯 ASCII 宽字符，char 数即显示宽度。
fn logo_line_spans(row: &str, available_width: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = row.chars().collect();
    let visible = chars.len().min(snap_to_letter_boundary(available_width));

    // 计算参与渐变的总列数：取可见范围内最后一个非空格字符的列号 + 1。
    // 若可见范围内全空，渐变列数为 1（取中点即可）。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx() -> WelcomeContext {
        WelcomeContext { tip_index: 0 }
    }

    fn collect_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// 一行的显示宽度（与 `render::char_display_width` 同口径）。
    fn line_width(line: &Line) -> usize {
        collect_text(line).chars().map(char_display_width).sum()
    }

    #[test]
    fn render_welcome_total_lines_is_fixed() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let expected = TOP_BLANK_LINES + ASCII_LOGO.len() + 1 + SECTION_BLANK_LINES + 1 + 1;
        assert_eq!(
            lines.len(),
            expected,
            "欢迎页应固定 {} 行（顶留白 {TOP_BLANK_LINES} + logo {} + 版本 1 + 间隔 {SECTION_BLANK_LINES} + tip 1 + 快捷键 1）",
            expected,
            ASCII_LOGO.len()
        );
    }

    #[test]
    fn render_welcome_first_lines_are_blank_padding() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        for (i, line) in lines.iter().enumerate().take(TOP_BLANK_LINES) {
            assert_eq!(collect_text(line), "", "顶部第 {} 行应为留白空行", i + 1);
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

        // 每行可见字符数应一致（figlet standard 是固定宽字体）
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
            Some(Theme::welcome_logo_gradient_start_color()),
            "logo 首字符 fg 应等于渐变起点"
        );
        assert_eq!(
            non_blank.last().unwrap().style.fg,
            Some(Theme::welcome_logo_gradient_end_color()),
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
            .filter(|s| s.content.as_ref() != " " && s.style.fg.is_some())
            .collect();
        let start = match non_blank.first().unwrap().style.fg.unwrap() {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("渐变起点应返回 RGB"),
        };
        let end = match non_blank.last().unwrap().style.fg.unwrap() {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("渐变终点应返回 RGB"),
        };
        // 渐变必须**单调**：橙 → 橙黄 → 暖黄，r/g/b 三个分量都应非递减。
        // 起点 (217,142,95) → 终点 (232,192,122)，三项都递增；若某一项在
        // 中途回落，说明插值写错了段。
        for (i, ch) in non_blank.iter().enumerate() {
            let cur = match ch.style.fg.unwrap() {
                Color::Rgb(r, g, b) => (r, g, b),
                _ => unreachable!(),
            };
            assert!(
                cur.0 >= start.0 && cur.1 >= start.1 && cur.2 >= start.2,
                "logo 第 {} 个字符 {cur:?} 的渐变色应不早于起点 {start:?}",
                i
            );
            assert!(
                cur.0 <= end.0 && cur.1 <= end.1 && cur.2 <= end.2,
                "logo 第 {} 个字符 {cur:?} 的渐变色应不晚于终点 {end:?}",
                i
            );
        }
    }

    #[test]
    fn render_welcome_blank_line_between_brand_and_hint_sections() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        // 空行起点 = 版本行之后
        let inter_start = version_line_idx() + 1;
        for i in 0..SECTION_BLANK_LINES {
            let blank = &lines[inter_start + i];
            assert_eq!(
                collect_text(blank),
                "",
                "品牌区与提示区之间第 {} 行应为空行；实际：{:?}",
                i + 1,
                collect_text(blank)
            );
        }
    }

    #[test]
    fn render_welcome_version_line_sits_directly_below_logo() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        let idx = version_line_idx();
        assert_eq!(
            idx,
            TOP_BLANK_LINES + ASCII_LOGO.len(),
            "版本行应紧跟在 logo 块之后（logo 与版本号同属品牌区）"
        );
        assert_eq!(
            collect_text(&lines[idx]),
            format!(
                "{CONTENT_INDENT}{}",
                tr_fmt("welcome.version", &[("version", env!("CARGO_PKG_VERSION"))])
            ),
            "版本行应为左缩进的当前版本号；实际：{:?}",
            collect_text(&lines[idx])
        );
    }

    #[test]
    fn render_welcome_version_line_uses_dim_style() {
        let ctx = sample_ctx();
        let lines = render_welcome(&ctx, 120);
        assert_eq!(
            lines[version_line_idx()].spans[0].style.fg,
            Some(Theme::inactive_color()),
            "版本行应使用 dim 色（INACTIVE 灰）"
        );
    }

    #[test]
    fn render_welcome_version_line_does_not_overflow_narrow_width() {
        // 窄终端（比 "v0.2.25" 还窄）下应截断为省略号，不溢出右边界。
        for width in [CONTENT_INDENT.len(), 4, 8] {
            let lines = render_welcome(&sample_ctx(), width as u16);
            let text = collect_text(&lines[version_line_idx()]);
            let measured: usize = text.chars().map(char_display_width).sum();
            assert!(
                measured <= width,
                "宽度 {} 下版本行不应超宽；实际宽度 {}：{:?}",
                width,
                measured,
                text
            );
        }
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

    /// **宽度契约**：任何宽度下每一行都不得超宽。
    ///
    /// 旧实现有三处溢出：CWD 行完全没截断、tip 行只给双宽 emoji 留了 2 列
    /// 却要 3 列、快捷键行按 segment 整块丢弃。这些在窄终端下会被 Paragraph
    /// 从右侧硬切，切点落在半截字符上（项目里已因此出过「半个括号」事故）。
    #[test]
    fn no_line_exceeds_the_given_width_at_any_terminal_size() {
        for width in [4u16, 8, 12, 20, 30, 40, 60, 80, 100, 120, 200] {
            for tip_index in 0..TIPS.len() {
                let ctx = WelcomeContext { tip_index };
                let lines = render_welcome(&ctx, width);
                for (i, line) in lines.iter().enumerate() {
                    let w = line_width(line);
                    assert!(
                        w <= width as usize,
                        "宽度 {width} 下第 {} 行超宽（{w} 列）：{:?}",
                        i + 1,
                        collect_text(line)
                    );
                }
            }
        }
    }

    /// 欢迎页不再重复状态栏已有的 model / cwd。
    ///
    /// 状态栏常驻 `◆ model · [mode] · cwd`（`render::draw_status`），欢迎页
    /// 再写一遍就是同一个事实占两个位置。钉死这条：**页面上不得出现路径**。
    #[test]
    fn welcome_page_does_not_repeat_the_status_bar_model_and_cwd() {
        let lines = render_welcome(&sample_ctx(), 120);
        for (i, line) in lines.iter().enumerate() {
            let text = collect_text(line);
            assert!(
                !text.contains("/Users/") && !text.contains("~/"),
                "第 {} 行不应出现工作目录（状态栏已常驻）：{:?}",
                i + 1,
                text
            );
        }
    }

    /// 欢迎页不应再出现示例提问引导行。
    ///
    /// 旧版有一行 `Try "fix this bug" or ask anything…`，而输入框就在正下方，
    /// 两行说的是同一件事（"随便问点啥"），且该文案只有欢迎页在用、
    /// `welcome.placeholder` 这个 i18n key 会被整体废弃。
    #[test]
    fn welcome_page_has_no_suggestion_prompt_line() {
        let lines = render_welcome(&sample_ctx(), 120);
        let expected_len = TOP_BLANK_LINES + ASCII_LOGO.len() + 1 + SECTION_BLANK_LINES + 1 + 1;
        assert_eq!(
            lines.len(),
            expected_len,
            "欢迎页总行数应等于「留白 + logo + 版本 + 间隔 + tip + 快捷键」，\
             不含示例提问行"
        );
        for (i, line) in lines.iter().enumerate() {
            assert!(
                !collect_text(line).contains("Try \""),
                "第 {} 行不应出现示例提问引导文案：{:?}",
                i + 1,
                collect_text(line)
            );
        }
    }

    /// logo 与版本号同属品牌区，两行之间**不应**有空行。
    ///
    /// figlet 原输出的第 6 行是整行空格（不是字形），旧实现把它收进 ASCII_LOGO，
    /// 于是 logo 与版本号之间凭空多出一行空白。裁掉后品牌区收紧。
    #[test]
    fn version_line_has_no_blank_line_between_it_and_the_logo() {
        let lines = render_welcome(&sample_ctx(), 120);
        let logo_end = TOP_BLANK_LINES + ASCII_LOGO.len() - 1;
        let last_logo = &lines[logo_end];
        assert!(
            !collect_text(last_logo).trim().is_empty(),
            "logo 最后一行应是有字形的一行（figlet 的尾部空行已被裁掉）：{:?}",
            collect_text(last_logo)
        );
        assert_eq!(
            collect_text(&lines[version_line_idx()]),
            format!(
                "{CONTENT_INDENT}{}",
                tr_fmt("welcome.version", &[("version", env!("CARGO_PKG_VERSION"))])
            ),
            "版本行应紧跟 logo 最后一行"
        );
    }

    /// 窄终端下 logo 按列裁剪，且不超宽。
    ///
    /// 裁剪点按列走：figlet 每行的字母起始列不同，硬切会留下半个字母
    /// （`V` 变成 `|`）。这里保证「不超宽」；字母边界由下面的
    /// `logo_is_truncated_at_whole_letter_boundaries` 单独钉。
    #[test]
    fn logo_is_truncated_by_column_and_never_exceeds_width() {
        for width in [10u16, 20, 30, 40, 60, 80] {
            let lines = render_welcome(&sample_ctx(), width);
            let start = TOP_BLANK_LINES;
            let end = start + ASCII_LOGO.len();
            for (i, line) in lines[start..end].iter().enumerate() {
                let w = line_width(line);
                assert!(
                    w <= width as usize,
                    "宽度 {width} 下 logo 第 {} 行超宽（{w} 列）：{:?}",
                    i + 1,
                    collect_text(line)
                );
            }
        }
    }

    /// 窄终端下 logo 只能被切到**完整字母的边界**，不得出现半个字形。
    ///
    /// figlet 艺术字里一个字母横跨若干列（如 `O` 占 6 列），按列硬切会切出
    /// 半圆。规则是：可用宽度不够放下下一个完整字母时，就退回到上一个字母
    /// 的边界——宁可少显示几个字母，也不显示残缺的字形。
    #[test]
    fn logo_is_truncated_at_whole_letter_boundaries() {
        for (width, expected_cols) in [
            (10u16, 0usize), // 连一个字母都放不下 → 整块不显示
            (14, 13),        // 刚好放下 "W"
            (22, 20),        // 放下 "WY"
            (30, 23),        // 放下 "WYJ"
            (40, 39),        // 放下 "WYJ-CO"
            (60, 52),        // 放下完整 "WYJ-CODE"
        ] {
            let snapped = snap_to_letter_boundary(width as usize);
            assert_eq!(
                snapped, expected_cols,
                "宽度 {width} 应收敛到字母边界 {expected_cols}，实际 {snapped}"
            );
            // 收敛后的宽度必须不超过可用宽度，且是某个真实边界
            // （0 = 整块不显示，也是合法结果）
            assert!(snapped <= width as usize);
            assert!(
                snapped == 0 || LOGO_LETTER_BOUNDARIES.contains(&snapped),
                "收敛结果 {snapped} 应是 0 或某个字母边界"
            );
        }
    }

    /// 一个字母都放不下时，logo 整块留白——不留半截字形。
    #[test]
    fn logo_disappears_entirely_when_not_even_one_letter_fits() {
        let lines = render_welcome(&sample_ctx(), 10);
        let start = TOP_BLANK_LINES;
        let end = start + ASCII_LOGO.len();
        for (i, line) in lines[start..end].iter().enumerate() {
            let text = collect_text(line);
            assert!(
                text.trim().is_empty(),
                "宽度 10 下 logo 第 {} 行应留白；实际：{:?}",
                i + 1,
                text
            );
        }
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
            let ctx = WelcomeContext { tip_index: idx };
            let lines = render_welcome(&ctx, 120);
            assert_eq!(
                lines.len(),
                base_total,
                "tip_index={idx} 不应改变总行数；实际 {}",
                lines.len()
            );
        }
    }

    fn version_line_idx() -> usize {
        TOP_BLANK_LINES + ASCII_LOGO.len()
    }

    fn tip_line_idx() -> usize {
        version_line_idx() + 1 + SECTION_BLANK_LINES
    }

    fn shortcuts_line_idx() -> usize {
        tip_line_idx() + 1
    }
}
