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
    pub text_dim: Color,
    pub border: Color,
    pub code_fg: Color,
    pub selected_bg: Color,
    // ── 以下按「用途」而非「颜色」命名 ────────────────────────────────────
    // 槽位是按"这一处该长什么样"切的，不是按"它是青的还是蓝的"。同一个青色
    // 出现在 @file 引用、System 标记、排队提示、选中项背景四处时，它们是四种
    // 不同的语义：换成亮色主题时输入提示可能要变亮、System 标记可能要变深。
    // 若只留一个 cyan 槽位，四处只能一起变——这正是本轮迁移前"插件换肤半残"
    // 的根因，也正是 Codex 把语义色写进 styles.md 的原因。
    pub plan: Color,
    pub danger: Color,
    pub hint: Color,
    pub info: Color,
    pub queue: Color,
    pub selection_fg: Color,
    pub cursor: Color,
    pub muted: Color,
    pub code_keyword: Color,
    pub code_string: Color,
    pub code_number: Color,
    pub code_comment: Color,
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
            text_dim: Theme::TEXT_DIM,
            border: Theme::BORDER,
            code_fg: Theme::CODE_FG,
            selected_bg: Theme::SELECTED_BG,
            plan: Theme::PLAN,
            danger: Theme::DANGER,
            hint: Theme::HINT,
            info: Theme::INFO,
            queue: Theme::QUEUE,
            selection_fg: Theme::SELECTION_FG,
            cursor: Theme::CURSOR,
            muted: Theme::MUTED,
            code_keyword: Theme::CODE_KEYWORD,
            code_string: Theme::CODE_STRING,
            code_number: Theme::CODE_NUMBER,
            code_comment: Theme::CODE_COMMENT,
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
                "textdim" | "dimtext" | "mutedtext" => palette.text_dim = color,
                "border" => palette.border = color,
                "codefg" | "codeforeground" => palette.code_fg = color,
                "codekeyword" | "syntaxkeyword" => palette.code_keyword = color,
                "codestring" | "syntaxstring" => palette.code_string = color,
                "codenumber" | "syntaxnumber" => palette.code_number = color,
                "codecomment" | "syntaxcomment" => palette.code_comment = color,
                "plan" | "planning" => palette.plan = color,
                "danger" | "bypass" => palette.danger = color,
                "hint" => palette.hint = color,
                "info" | "notification" => palette.info = color,
                "queue" => palette.queue = color,
                "selectionfg" | "selectionforeground" => palette.selection_fg = color,
                "cursor" => palette.cursor = color,
                "muted" | "faint" => palette.muted = color,
                "selectedbg" | "selectedbackground" => palette.selected_bg = color,
                "welcomelogogradientstart" => palette.welcome_logo_gradient_start = color,
                "welcomelogogradientmid" => palette.welcome_logo_gradient_mid = color,
                "welcomelogogradientend" => palette.welcome_logo_gradient_end = color,
                "welcometip" => palette.welcome_tip = color,
                // 未知键**静默忽略**（含已下线的 `statusBg`）：插件主题来自
                // 用户环境，报错会让整个主题被丢弃，而少配一个已废弃的键
                // 不该有惩罚。代价是拼错键名也拿不到提示——`theme_tests.rs` 的
                // `every_palette_slot_is_wired_to_a_json_key` 用一个"全部键名"
                // 的 JSON 把这条静默路径兜住。
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

/// 应用插件/用户主题。**解析后立刻按终端能力降级**。
///
/// 降级必须发生在这里而不是只在启动时做一次：本函数是**整体替换**调色板的，
/// 若降级只跑在它之前，插件主题会把降级结果原样覆盖掉——一个 24 位主题丢进
/// 16 色终端就是一团糊，而这恰恰是最需要降级的场景。
pub fn apply_theme_json(value: &Value) -> Result<(), String> {
    let palette = ThemePalette::from_json(value)?.degraded(current_caps());
    *active_palette().write().unwrap() = palette;
    Ok(())
}

/// 终端的颜色能力，决定槽位色怎么降级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCaps {
    /// `NO_COLOR`（https://no-color.org）：存在即完全不输出颜色。
    pub color_enabled: bool,
    /// 真 24 位色。关掉时 RGB 槽位量化到 xterm 256 色索引。
    pub truecolor: bool,
}

impl TerminalCaps {
    fn from_env() -> Self {
        // 任何非空值都算「不要颜色」，连 `NO_COLOR=0` 也一样——规范就是这么定的，
        // 用户写它就是不想看颜色，把 `0` 当成"要"会让人无从关闭。
        let color_enabled = std::env::var_os("NO_COLOR").is_none();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        let term = std::env::var("TERM").unwrap_or_default();
        let truecolor = color_enabled
            && (colorterm.eq_ignore_ascii_case("truecolor")
                || colorterm.eq_ignore_ascii_case("24bit")
                || term.contains("direct")
                || term.contains("truecolor"));
        Self {
            color_enabled,
            truecolor,
        }
    }
}

impl ThemePalette {
    /// 按终端能力降级后的副本。**纯函数**，不碰全局单例——这样它可以被单测
    /// 直接断言"27 个槽位一个都没漏降"。
    ///
    /// 字段是逐个写出来的：将来新增槽位必须在这里补一行，漏了就会在
    /// 16 色终端里漏一个槽位降级（表现为「某个元素颜色糊了」）。
    /// `theme_tests` 的 `every_slot_is_degraded_not_just_some_of_them` 兜住。
    pub fn degraded(&self, caps: TerminalCaps) -> Self {
        let f = |c: Color| degrade(c, caps);
        Self {
            success: f(self.success),
            error: f(self.error),
            warning: f(self.warning),
            claude: f(self.claude),
            inactive: f(self.inactive),
            suggestion: f(self.suggestion),
            text: f(self.text),
            text_dim: f(self.text_dim),
            border: f(self.border),
            code_fg: f(self.code_fg),
            selected_bg: f(self.selected_bg),
            plan: f(self.plan),
            danger: f(self.danger),
            hint: f(self.hint),
            info: f(self.info),
            queue: f(self.queue),
            selection_fg: f(self.selection_fg),
            cursor: f(self.cursor),
            muted: f(self.muted),
            code_keyword: f(self.code_keyword),
            code_string: f(self.code_string),
            code_number: f(self.code_number),
            code_comment: f(self.code_comment),
            welcome_logo_gradient_start: f(self.welcome_logo_gradient_start),
            welcome_logo_gradient_mid: f(self.welcome_logo_gradient_mid),
            welcome_logo_gradient_end: f(self.welcome_logo_gradient_end),
            welcome_tip: f(self.welcome_tip),
        }
    }
}

static TERMINAL_CAPS: OnceLock<TerminalCaps> = OnceLock::new();

fn current_caps() -> TerminalCaps {
    *TERMINAL_CAPS.get_or_init(TerminalCaps::from_env)
}

/// 探测并降级当前调色板，在任何 [`apply_theme_json`] **之前**调用一次。
///
/// 插件主题自身也走降级（见 [`apply_theme_json`]），所以这里的作用是让
/// `NO_COLOR` / 非 truecolor 这类能力对「没有插件主题」的默认路径同样生效。
pub fn apply_terminal_capabilities() {
    let caps = current_caps();
    let mut guard = active_palette().write().unwrap();
    *guard = guard.degraded(caps);
}

/// 单个槽位色的降级。**具名色（ANSI 索引）不动**——任何终端都认它，
/// 只有 RGB 才可能超出终端能力。
fn degrade(color: Color, caps: TerminalCaps) -> Color {
    if !caps.color_enabled {
        // `Color::Reset` = 透传终端默认前景色。Modifier（BOLD 等）不受影响，
        // 所以 NO_COLOR 下仍有粗体/斜体可用，只是没有颜色。
        return Color::Reset;
    }
    match color {
        Color::Rgb(r, g, b) if !caps.truecolor => quantize_to_256(r, g, b),
        other => other,
    }
}

/// RGB → xterm 256 色索引。
///
/// 走标准布局：0..15 是 ANSI 具名色（跳过，槽位本来就该用具名色），
/// 16..231 是 6×6×6 立方体，232..255 是 24 级灰阶。等亮度灰优先走灰阶，
/// 否则带色调的暗色会被立方体量化成偏绿的脏色。
fn quantize_to_256(r: u8, g: u8, b: u8) -> Color {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    if max - min < 12 {
        // 近似中性色 → 灰阶
        let gray = (r as u32 + g as u32 + b as u32) / 3;
        let idx = 232 + ((gray as i32 - 8) * 24 / 247).clamp(0, 23);
        return Color::Indexed(idx as u8);
    }
    let step = |v: u8| ((v as i32 * 5 + 127) / 255).clamp(0, 5);
    Color::Indexed((16 + 36 * step(r) + 6 * step(g) + step(b)) as u8)
}

/// sRGB 相对亮度（WCAG 定义）
fn relative_luminance(color: Color) -> f64 {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        // 具名色按「终端会画成什么样」无从得知，取中性 0.5：
        // 宁可判定"对比够"也不擅自改用户指定的颜色。
        _ => return 0.5,
    };
    let f = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// 对比度比值（WCAG），范围 1.0（完全同色）到 21.0（黑白）
fn contrast_ratio(a: Color, b: Color) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 最低可接受对比度。
///
/// 取 3.0 而不是 WCAG AA 正文的 4.5：4.5 管的是**正文长文**，而对比度兜底
/// 只用在选中行、块状光标这类**加粗的短标签**上，WCAG 对大号/加粗文本的门槛
/// 正是 3.0。用 4.5 会把默认主题里品牌橙的选中行判成不合格、换成白色——
/// 那不是修复，是改设计。
const MIN_CONTRAST: f64 = 3.0;

/// 保证 `fg` 在 `bg` 上可读：对比度不足时换成黑或白里对比度更高的那个。
///
/// 只用于**背景已知**的场合（选中行、块状光标）。其余位置的背景是用户终端的
/// 背景色，我们无从探测，也就无从判断——那种情况保持原色，不猜。
pub fn ensure_contrast(fg: Color, bg: Color) -> Color {
    if contrast_ratio(fg, bg) >= MIN_CONTRAST {
        return fg;
    }
    let black = Color::Rgb(0, 0, 0);
    let white = Color::Rgb(255, 255, 255);
    if contrast_ratio(black, bg) >= contrast_ratio(white, bg) {
        black
    } else {
        white
    }
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
    /// 次级正文：比 `TEXT` 更淡，用于「描述性」文字（如补全项的说明）。
    /// 此前是散在 render.rs 里的 `Rgb(180,200,255)` 孤儿色——与 TEXT 同义却
    /// 不可调，插件主题改不动它。
    pub const TEXT_DIM: Color = Color::Rgb(180, 200, 255);
    pub const BORDER: Color = Color::Rgb(102, 102, 102); // 深灰边框
    pub const CODE_FG: Color = Color::Rgb(147, 165, 255); // 代码块内容浅蓝
                                                          // ── 用途型槽位（值等于本轮迁移前的硬编码色，故观感完全不变）────────────
    /// 计划相关：计划块标题、`[plan]` 芯片、计划模式的输入框边框、计划批准面板
    pub const PLAN: Color = Color::Blue;
    /// 危险语义：**bypass 三处**（状态栏芯片 + 标题栏 + 输入框边框）与破坏性确认
    pub const DANGER: Color = Color::Red;
    /// 输入提示：`@file` 引用高亮、补全面板选中项、输入框内联 hint
    pub const HINT: Color = Color::Cyan;
    /// 中性提示：System 标记、thinking 实时行、「下方还有新消息」、运行中任务行
    pub const INFO: Color = Color::Cyan;
    /// 排队消息提示（状态栏右侧的 ●）
    pub const QUEUE: Color = Color::Cyan;
    /// 选中项前景：压在 [HINT / INFO] 底色之上，故用终端最深色
    pub const SELECTION_FG: Color = Color::Black;
    /// 块状光标的字符前景（光标背景用品牌橙）
    pub const CURSOR: Color = Color::White;
    /// 极弱化的次要标记（Todo 的待办圆圈、「待发送：」标签）。
    ///
    /// 与 [Self::INACTIVE] **不是同一档**：后者是 RGB 灰（#999999，浅色系里的
    /// 主 dim 色），本槽位保留 ANSI DarkGray 以维持迁移前的观感。把它并进
    /// `inactive` 会让这几处整体亮一档——收敛批次只搬颜色，不改观感。
    pub const MUTED: Color = Color::DarkGray;
    pub const CODE_KEYWORD: Color = Color::Cyan;
    pub const CODE_STRING: Color = Color::Green;
    pub const CODE_NUMBER: Color = Color::Yellow;
    pub const CODE_COMMENT: Color = Color::DarkGray;
    /// 列表/管理面板里"当前选中行"的通用背景色：中深灰，与终端默认底色拉开
    /// 足够对比度，在黑色/深色终端背景下能清楚看到选中条，同时不像饱和色（原先
    /// 部分面板用的 Color::Blue）那样过于刺眼。全应用统一用这一个值（而非各面板
    /// 各自选色），是 v0.2.4 为解决"选中态视觉语言不一致导致不好辨认"问题引入的。
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

    pub fn text_dim_color() -> Color {
        active_palette().read().unwrap().text_dim
    }

    pub fn border_color() -> Color {
        active_palette().read().unwrap().border
    }

    pub fn plan_color() -> Color {
        active_palette().read().unwrap().plan
    }

    pub fn danger_color() -> Color {
        active_palette().read().unwrap().danger
    }

    pub fn hint_color() -> Color {
        active_palette().read().unwrap().hint
    }

    pub fn info_color() -> Color {
        active_palette().read().unwrap().info
    }

    pub fn queue_color() -> Color {
        active_palette().read().unwrap().queue
    }

    pub fn selection_fg_color() -> Color {
        active_palette().read().unwrap().selection_fg
    }

    pub fn cursor_color() -> Color {
        active_palette().read().unwrap().cursor
    }

    pub fn muted_color() -> Color {
        active_palette().read().unwrap().muted
    }

    pub fn code_keyword_color() -> Color {
        active_palette().read().unwrap().code_keyword
    }

    pub fn code_string_color() -> Color {
        active_palette().read().unwrap().code_string
    }

    pub fn code_number_color() -> Color {
        active_palette().read().unwrap().code_number
    }

    pub fn code_comment_color() -> Color {
        active_palette().read().unwrap().code_comment
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
        let bg = Self::selected_bg_color();
        Style::default()
            .fg(ensure_contrast(Self::claude_color(), bg))
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    }

    /// 压在 `background` 上的前景：选中项、光标、编辑行。
    ///
    /// 前景必须经过 [`ensure_contrast`]——这是**唯一一类背景已知的场合**（其余
    /// 位置的背景是用户终端的背景色，无从探测）。插件主题把 `selectedBg` 配成
    /// 浅色时，固定的 `selection_fg`（黑）会让选中行文字彻底消失。
    pub fn selection_on(background: Color) -> Style {
        Style::default()
            .fg(ensure_contrast(Self::selection_fg_color(), background))
            .bg(background)
    }

    /// 边框
    pub fn border() -> Style {
        Style::default().fg(Self::border_color())
    }

    /// 状态栏
    ///
    /// 不再显式设置背景：之前用 `Rgb(30, 30, 30)` 让状态栏在终端默认黑底上呈现
    /// 一条深灰带，与上方聊天区、输入框标题栏在视觉上显得割裂（"输入框上面
    /// 显示 Enter to send 那行" 也是黑底，凑在一起像两块不同的色块）。
    /// 现在完全透传终端默认底色，文字仍用 `inactive` 灰色保持低调，
    /// 整体与聊天区融为一体。
    pub fn status_bar() -> Style {
        Style::default().fg(Self::inactive_color())
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

    // ── 用途型样式 ───────────────────────────────────────────────────────────
    //
    // 命名一律按**这一处画的是什么**取，而不是按颜色取。收敛硬编码色时若写成
    // `fn cyan()` 之类，下一个要画青色的人会继续往里塞，于是几个月后又是
    // 82 处硬编码——同一块皮肤散在十几个名字下，任何换肤都只能改到一半。

    /// 计划相关（计划块标题、`[plan]` 芯片、plan 模式边框、计划批准面板）
    pub fn plan() -> Style {
        Style::default().fg(Self::plan_color())
    }

    /// 危险语义：**bypass 三处**与破坏性确认（见 `tui/CLAUDE.md` 的硬不变式）
    pub fn danger() -> Style {
        Style::default().fg(Self::danger_color())
    }

    /// 输入提示（`@file` 引用、补全选中项、输入框内联 hint）
    pub fn hint() -> Style {
        Style::default().fg(Self::hint_color())
    }

    /// 中性提示（System 标记、thinking 实时行、「下方还有新消息」、运行中任务行）
    pub fn info() -> Style {
        Style::default().fg(Self::info_color())
    }

    /// 排队消息提示（状态栏右侧 ●）
    pub fn queue() -> Style {
        Style::default().fg(Self::queue_color())
    }

    /// 选中项前景（压在 [hint/info] 底色之上）
    pub fn selection_fg() -> Style {
        Style::default().fg(Self::selection_fg_color())
    }

    /// 次级正文（描述性文字，比 [Self::input_box] 更淡）
    pub fn text_dim() -> Style {
        Style::default().fg(Self::text_dim_color())
    }

    /// 正文：列表未选中行、面板标签、文件名等一切"普通文字"。
    ///
    /// 迁移前这些位置散着二十多处 `Style::default().fg(Color::White)`。它们是
    /// **同一种东西**，因此也该是同一个函数——否则"正文是白的"这条约定会
    /// 散在十几个名字下，下次换主题时又得逐处找。
    pub fn body() -> Style {
        Style::default().fg(Self::text_color())
    }

    /// 块状光标的字符前景（光标底色用品牌橙，见 `render::freetext_line`）
    pub fn cursor() -> Style {
        Style::default().fg(Self::cursor_color())
    }

    /// 极弱化的次要标记（比 [Self::dim] 再暗一档，用于标记而非正文）
    pub fn muted() -> Style {
        Style::default().fg(Self::muted_color())
    }

    // ── 代码高亮四件套 ───────────────────────────────────────────────────────
    // 自研词法着色器（markdown.rs::highlight_code_line）只区分这四类，故也只有
    // 这四个槽位；未知语言不着色，这是「零语法高亮依赖」方案的边界。

    pub fn code_keyword() -> Style {
        Style::default()
            .fg(Self::code_keyword_color())
            .add_modifier(Modifier::BOLD)
    }

    pub fn code_string() -> Style {
        Style::default().fg(Self::code_string_color())
    }

    pub fn code_number() -> Style {
        Style::default().fg(Self::code_number_color())
    }

    pub fn code_comment() -> Style {
        Style::default()
            .fg(Self::code_comment_color())
            .add_modifier(Modifier::ITALIC)
    }

    /// 状态栏品牌标志 ◆ 的颜色
    pub fn claude_brand() -> Style {
        Style::default().fg(Self::claude_color())
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

    // ── 颜色运算 ─────────────────────────────────────────────────────────────
    //
    // 渐变插值这类**必须处理 RGB 分量**的场景（目前只有欢迎页 logo）需要的
    // 原语放在这一层，渲染层就只剩「取色 → 调 `gradient`」，不必再自己 match
    // `Color::Rgb` 或写死品牌橙数字——否则渐变就成了全应用唯一一处绕过槽位的
    // 硬编码颜色，插件主题设成 ANSI 具名色时也无人负责回退。

    /// 取 RGB 分量；`color` 不是 RGB 色时回退到 `fallback` 的分量
    /// （`fallback` 自己也不是 RGB 时返回全 0——那意味着主题配错了，
    /// 画出来是纯黑，比崩溃好定位）。
    pub fn rgb_components_or(color: Color, fallback: Color) -> (f32, f32, f32) {
        fn parts(color: Color) -> Option<(f32, f32, f32)> {
            match color {
                Color::Rgb(r, g, b) => Some((r as f32, g as f32, b as f32)),
                _ => None,
            }
        }
        parts(color)
            .or_else(|| parts(fallback))
            .unwrap_or((0.0, 0.0, 0.0))
    }

    /// 三通道颜色分量 → `Color::Rgb`（分量会被四舍五入并夹到 0..=255）
    pub fn rgb_from_components(r: f32, g: f32, b: f32) -> Color {
        let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        Color::Rgb(clamp(r), clamp(g), clamp(b))
    }

    /// 槽位色 → crossterm 颜色，供**不走 ratatui、直接往终端写字节**的场景使用
    /// （目前只有退出 alternate screen 后那行「恢复此会话」提示）。它绕过了
    /// 整个渲染管线，`Color::Cyan` 写在那里的话插件主题就彻底管不到它。
    pub fn to_crossterm(color: Color) -> crossterm::style::Color {
        use crossterm::style::Color as CrosstermColor;
        match color {
            Color::Reset => CrosstermColor::Reset,
            // ratatui 的 Red/Green/... 是 crossterm 的 **Dark** 变体；反过来
            // crossterm 的 Red 是 ratatui 的 LightRed。搞错会让整个提示行
            // 亮一档，所以这里逐个显式写出而不是靠 From 转换。
            Color::Black => CrosstermColor::Black,
            Color::Red => CrosstermColor::DarkRed,
            Color::Green => CrosstermColor::DarkGreen,
            Color::Yellow => CrosstermColor::DarkYellow,
            Color::Blue => CrosstermColor::DarkBlue,
            Color::Magenta => CrosstermColor::DarkMagenta,
            Color::Cyan => CrosstermColor::DarkCyan,
            Color::Gray => CrosstermColor::Grey,
            Color::DarkGray => CrosstermColor::DarkGrey,
            Color::LightRed => CrosstermColor::Red,
            Color::LightGreen => CrosstermColor::Green,
            Color::LightYellow => CrosstermColor::Yellow,
            Color::LightBlue => CrosstermColor::Blue,
            Color::LightMagenta => CrosstermColor::Magenta,
            Color::LightCyan => CrosstermColor::Cyan,
            Color::White => CrosstermColor::White,
            Color::Rgb(r, g, b) => CrosstermColor::Rgb { r, g, b },
            Color::Indexed(i) => CrosstermColor::AnsiValue(i),
        }
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
            "plan": [4, 5, 6],
            "selected_background": {"r": 7, "g": 8, "b": 9}
        }))
        .unwrap();
        assert_eq!(palette.claude, Color::Rgb(1, 2, 3));
        assert_eq!(palette.plan, Color::Rgb(4, 5, 6));
        assert_eq!(palette.selected_bg, Color::Rgb(7, 8, 9));
    }
}
