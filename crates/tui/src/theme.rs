//! 色彩主题（暗色系，RGB 精确值，参照 smj-code darkTheme）

use std::sync::{OnceLock, RwLock};

use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub claude: Color,
    pub inactive: Color,
    pub suggestion: Color,
    pub text: Color,
    pub border: Color,
    pub status_bg: Color,
    pub code_fg: Color,
    pub selected_bg: Color,
    pub welcome_logo_gradient_start: Color,
    pub welcome_logo_gradient_mid: Color,
    pub welcome_logo_gradient_end: Color,
    pub welcome_tip: Color,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            success: Theme::SUCCESS,
            error: Theme::ERROR,
            warning: Theme::WARNING,
            claude: Theme::CLAUDE,
            inactive: Theme::INACTIVE,
            suggestion: Theme::SUGGESTION,
            text: Theme::TEXT,
            border: Theme::BORDER,
            status_bg: Theme::STATUS_BG,
            code_fg: Theme::CODE_FG,
            selected_bg: Theme::SELECTED_BG,
            welcome_logo_gradient_start: Theme::WELCOME_LOGO_GRADIENT_START,
            welcome_logo_gradient_mid: Theme::WELCOME_LOGO_GRADIENT_MID,
            welcome_logo_gradient_end: Theme::WELCOME_LOGO_GRADIENT_END,
            welcome_tip: Theme::WELCOME_TIP,
        }
    }
}

impl ThemePalette {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value
            .get("colors")
            .unwrap_or(value)
            .as_object()
            .ok_or_else(|| "theme palette must be a JSON object".to_string())?;
        let mut palette = Self::default();
        for (name, value) in object {
            let Some(color) = parse_color(value) else {
                return Err(format!("invalid color value for `{name}`"));
            };
            match normalized_key(name).as_str() {
                "success" => palette.success = color,
                "error" => palette.error = color,
                "warning" => palette.warning = color,
                "claude" | "brand" | "assistant" => palette.claude = color,
                "inactive" | "dim" => palette.inactive = color,
                "suggestion" | "accent" => palette.suggestion = color,
                "text" | "foreground" => palette.text = color,
                "border" => palette.border = color,
                "statusbg" | "statusbackground" => palette.status_bg = color,
                "codefg" | "codeforeground" => palette.code_fg = color,
                "selectedbg" | "selectedbackground" => palette.selected_bg = color,
                "welcomelogogradientstart" => palette.welcome_logo_gradient_start = color,
                "welcomelogogradientmid" => palette.welcome_logo_gradient_mid = color,
                "welcomelogogradientend" => palette.welcome_logo_gradient_end = color,
                "welcometip" => palette.welcome_tip = color,
                _ => continue,
            }
        }
        Ok(palette)
    }
}

static ACTIVE_PALETTE: OnceLock<RwLock<ThemePalette>> = OnceLock::new();

fn active_palette() -> &'static RwLock<ThemePalette> {
    ACTIVE_PALETTE.get_or_init(|| RwLock::new(ThemePalette::default()))
}

pub fn apply_theme_json(value: &Value) -> Result<(), String> {
    let palette = ThemePalette::from_json(value)?;
    *active_palette().write().unwrap() = palette;
    Ok(())
}

fn normalized_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_color(value: &Value) -> Option<Color> {
    if let Some(text) = value.as_str() {
        let hex = text.strip_prefix('#').unwrap_or(text);
        if hex.len() == 6 {
            let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(red, green, blue));
        }
        return match text.to_ascii_lowercase().as_str() {
            "black" => Some(Color::Black),
            "white" => Some(Color::White),
            "red" => Some(Color::Red),
            "green" => Some(Color::Green),
            "blue" => Some(Color::Blue),
            "yellow" => Some(Color::Yellow),
            "magenta" => Some(Color::Magenta),
            "cyan" => Some(Color::Cyan),
            "gray" | "grey" => Some(Color::Gray),
            _ => None,
        };
    }
    if let Some(values) = value.as_array().filter(|values| values.len() == 3) {
        return Some(Color::Rgb(
            u8::try_from(values[0].as_u64()?).ok()?,
            u8::try_from(values[1].as_u64()?).ok()?,
            u8::try_from(values[2].as_u64()?).ok()?,
        ));
    }
    let object = value.as_object()?;
    Some(Color::Rgb(
        u8::try_from(object.get("r")?.as_u64()?).ok()?,
        u8::try_from(object.get("g")?.as_u64()?).ok()?,
        u8::try_from(object.get("b")?.as_u64()?).ok()?,
    ))
}

pub struct Theme;

impl Theme {
    // ── 语义颜色（RGB 精确值）─────────────────────────────────────────────────
    pub const SUCCESS: Color = Color::Rgb(78, 186, 101); // 绿色
    pub const ERROR: Color = Color::Rgb(255, 107, 128); // 红色
    pub const WARNING: Color = Color::Rgb(200, 155, 30); // 琥珀
    pub const CLAUDE: Color = Color::Rgb(215, 119, 87); // 品牌橙（助手/spinner）
    pub const INACTIVE: Color = Color::Rgb(153, 153, 153); // 灰色（dim/border）
    pub const SUGGESTION: Color = Color::Rgb(177, 185, 249); // 蓝紫（用户前缀/权限）
    pub const TEXT: Color = Color::Rgb(255, 255, 255); // 白色
    pub const BORDER: Color = Color::Rgb(102, 102, 102); // 深灰边框
    pub const STATUS_BG: Color = Color::Rgb(30, 30, 30); // 状态栏背景
    pub const CODE_FG: Color = Color::Rgb(147, 165, 255); // 代码块内容浅蓝
    /// 列表/管理面板里"当前选中行"的通用背景色：中深灰，与 STATUS_BG(30,30,30) 拉开
    /// 足够对比度，在黑色/深色终端背景下能清楚看到选中条，同时不像饱和色（原先
    /// 部分面板用的 Color::Blue）那样过于刺眼。全应用统一用这一个值（而非各面板
    /// 各自选色），是 v1.3.3 为解决"选中态视觉语言不一致导致不好辨认"问题引入的。
    pub const SELECTED_BG: Color = Color::Rgb(66, 66, 66);

    pub fn success_color() -> Color {
        active_palette().read().unwrap().success
    }

    pub fn error_color() -> Color {
        active_palette().read().unwrap().error
    }

    pub fn warning_color() -> Color {
        active_palette().read().unwrap().warning
    }

    pub fn claude_color() -> Color {
        active_palette().read().unwrap().claude
    }

    pub fn inactive_color() -> Color {
        active_palette().read().unwrap().inactive
    }

    pub fn suggestion_color() -> Color {
        active_palette().read().unwrap().suggestion
    }

    pub fn text_color() -> Color {
        active_palette().read().unwrap().text
    }

    pub fn border_color() -> Color {
        active_palette().read().unwrap().border
    }

    pub fn status_bg_color() -> Color {
        active_palette().read().unwrap().status_bg
    }

    pub fn code_fg_color() -> Color {
        active_palette().read().unwrap().code_fg
    }

    pub fn selected_bg_color() -> Color {
        active_palette().read().unwrap().selected_bg
    }

    // ── 样式方法 ─────────────────────────────────────────────────────────────

    /// 用户消息前缀 "▶ 你"
    pub fn user_prefix() -> Style {
        Style::default()
            .fg(Self::suggestion_color())
            .add_modifier(Modifier::BOLD)
    }

    /// 助手消息前缀 "◆ AI"
    pub fn assistant_prefix() -> Style {
        Style::default()
            .fg(Self::claude_color())
            .add_modifier(Modifier::BOLD)
    }

    /// 工具调用状态行（运行中）
    pub fn tool_call() -> Style {
        Style::default().fg(Self::claude_color())
    }

    /// 工具结果正常内容
    pub fn tool_result() -> Style {
        Style::default().fg(Self::suggestion_color())
    }

    /// 成功状态（✓）
    pub fn success() -> Style {
        Style::default().fg(Self::success_color())
    }

    /// 错误状态（✗）
    pub fn error() -> Style {
        Style::default().fg(Self::error_color())
    }

    /// 警告状态（⚠）
    pub fn warning() -> Style {
        Style::default().fg(Self::warning_color())
    }

    /// 淡化（不活跃内容、截断提示）
    pub fn dim() -> Style {
        Style::default().fg(Self::inactive_color())
    }

    /// 列表/管理面板里"当前选中行"的通用样式（品牌橙前景 + 深灰背景 + 加粗）。
    /// Todo 列表、子 Agent 面板、会话选择器、斜杠命令补全、Profile/Mcp/Skills/
    /// Plugins/Extensions/Import/Schedule/Agents 等全部管理面板的列表行选中态
    /// 统一走这一个函数，避免各处各自定义颜色导致视觉语言不一致。
    pub fn selected_row() -> Style {
        Style::default()
            .fg(Self::claude_color())
            .bg(Self::selected_bg_color())
            .add_modifier(Modifier::BOLD)
    }

    /// 边框
    pub fn border() -> Style {
        Style::default().fg(Self::border_color())
    }

    /// 状态栏
    pub fn status_bar() -> Style {
        Style::default()
            .bg(Self::status_bg_color())
            .fg(Self::inactive_color())
    }

    /// 输入框文字
    pub fn input_box() -> Style {
        Style::default().fg(Self::text_color())
    }

    /// 权限对话框（边框和标题）
    pub fn permission_dialog() -> Style {
        Style::default()
            .fg(Self::suggestion_color())
            .add_modifier(Modifier::BOLD)
    }

    /// 流式文本（助手实时输出）
    pub fn streaming() -> Style {
        Style::default().fg(Self::claude_color())
    }

    /// 高亮文字（对话框内提示键等）
    pub fn highlight() -> Style {
        Style::default()
            .fg(Self::text_color())
            .add_modifier(Modifier::BOLD)
    }

    /// 代码块标记行（```）
    pub fn code_fence() -> Style {
        Style::default().fg(Self::inactive_color())
    }

    /// 代码块内容行
    pub fn code_body() -> Style {
        Style::default().fg(Self::code_fg_color())
    }

    /// 状态栏品牌标志 ◆ 的颜色
    pub fn claude_brand() -> Style {
        Style::default().fg(Self::claude_color())
    }

    /// 进度条已填充颜色（< 70%）
    pub fn progress_normal() -> Style {
        Style::default().fg(Self::suggestion_color())
    }

    /// 进度条告警颜色（70-90%）
    pub fn progress_warn() -> Style {
        Style::default().fg(Self::warning_color())
    }

    /// 进度条危险颜色（>= 90%）
    pub fn progress_danger() -> Style {
        Style::default().fg(Self::error_color())
    }

    /// 进度条空余部分
    pub fn progress_empty() -> Style {
        Style::default().fg(Self::inactive_color())
    }

    // ── 欢迎页 logo 专用（橙→黄渐变 + 整块反色填充）───────────────────────────
    /// 渐变起点：品牌橙（与 CLAUDE 同色，确保 logo 起始色与品牌一致）
    pub const WELCOME_LOGO_GRADIENT_START: Color = Color::Rgb(215, 119, 87);
    /// 渐变中点：橙黄过渡色（用于渐变插值 + 整块背景填充）
    pub const WELCOME_LOGO_GRADIENT_MID: Color = Color::Rgb(225, 160, 85);
    /// 渐变终点：暖黄
    pub const WELCOME_LOGO_GRADIENT_END: Color = Color::Rgb(240, 200, 80);

    // ── 欢迎页新增元素（tips / 示例提问 / 快捷键）───────────────────────────
    /// tips 提示行强调色：柔和琥珀黄，呼应 💡 图标，与 WARNING 语义色区分开
    pub const WELCOME_TIP: Color = Color::Rgb(229, 192, 123);

    pub fn welcome_logo_gradient_start_color() -> Color {
        active_palette().read().unwrap().welcome_logo_gradient_start
    }

    pub fn welcome_logo_gradient_mid_color() -> Color {
        active_palette().read().unwrap().welcome_logo_gradient_mid
    }

    pub fn welcome_logo_gradient_end_color() -> Color {
        active_palette().read().unwrap().welcome_logo_gradient_end
    }

    pub fn welcome_tip_color() -> Color {
        active_palette().read().unwrap().welcome_tip
    }

    /// tips 提示行样式
    pub fn welcome_tip() -> Style {
        Style::default().fg(Self::welcome_tip_color())
    }

    /// 示例提问建议行样式：dim + 斜体，区别于输入框真实 placeholder
    pub fn welcome_suggestion() -> Style {
        Style::default()
            .fg(Self::inactive_color())
            .add_modifier(Modifier::ITALIC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_palette_accepts_hex_rgb_arrays_and_aliases() {
        let palette = ThemePalette::from_json(&serde_json::json!({
            "brand": "#010203",
            "statusBg": [4, 5, 6],
            "selected_background": {"r": 7, "g": 8, "b": 9}
        }))
        .unwrap();
        assert_eq!(palette.claude, Color::Rgb(1, 2, 3));
        assert_eq!(palette.status_bg, Color::Rgb(4, 5, 6));
        assert_eq!(palette.selected_bg, Color::Rgb(7, 8, 9));
    }
}
