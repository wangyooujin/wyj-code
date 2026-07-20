//! 色彩主题（暗色系，RGB 精确值，参照 smj-code darkTheme）

use ratatui::style::{Color, Modifier, Style};

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

    // ── 样式方法 ─────────────────────────────────────────────────────────────

    /// 用户消息前缀 "▶ 你"
    pub fn user_prefix() -> Style {
        Style::default()
            .fg(Self::SUGGESTION)
            .add_modifier(Modifier::BOLD)
    }

    /// 助手消息前缀 "◆ AI"
    pub fn assistant_prefix() -> Style {
        Style::default()
            .fg(Self::CLAUDE)
            .add_modifier(Modifier::BOLD)
    }

    /// 工具调用状态行（运行中）
    pub fn tool_call() -> Style {
        Style::default().fg(Self::CLAUDE)
    }

    /// 工具结果正常内容
    pub fn tool_result() -> Style {
        Style::default().fg(Self::SUGGESTION)
    }

    /// 成功状态（✓）
    pub fn success() -> Style {
        Style::default().fg(Self::SUCCESS)
    }

    /// 错误状态（✗）
    pub fn error() -> Style {
        Style::default().fg(Self::ERROR)
    }

    /// 警告状态（⚠）
    pub fn warning() -> Style {
        Style::default().fg(Self::WARNING)
    }

    /// 淡化（不活跃内容、截断提示）
    pub fn dim() -> Style {
        Style::default().fg(Self::INACTIVE)
    }

    /// 列表/管理面板里"当前选中行"的通用样式（品牌橙前景 + 深灰背景 + 加粗）。
    /// Todo 列表、子 Agent 面板、会话选择器、斜杠命令补全、Profile/Mcp/Skills/
    /// Plugins/Extensions/Import/Schedule/Agents 等全部管理面板的列表行选中态
    /// 统一走这一个函数，避免各处各自定义颜色导致视觉语言不一致。
    pub fn selected_row() -> Style {
        Style::default()
            .fg(Self::CLAUDE)
            .bg(Self::SELECTED_BG)
            .add_modifier(Modifier::BOLD)
    }

    /// 边框
    pub fn border() -> Style {
        Style::default().fg(Self::BORDER)
    }

    /// 状态栏
    pub fn status_bar() -> Style {
        Style::default().bg(Self::STATUS_BG).fg(Self::INACTIVE)
    }

    /// 输入框文字
    pub fn input_box() -> Style {
        Style::default().fg(Self::TEXT)
    }

    /// 权限对话框（边框和标题）
    pub fn permission_dialog() -> Style {
        Style::default()
            .fg(Self::SUGGESTION)
            .add_modifier(Modifier::BOLD)
    }

    /// 流式文本（助手实时输出）
    pub fn streaming() -> Style {
        Style::default().fg(Self::CLAUDE)
    }

    /// 高亮文字（对话框内提示键等）
    pub fn highlight() -> Style {
        Style::default().fg(Self::TEXT).add_modifier(Modifier::BOLD)
    }

    /// 代码块标记行（```）
    pub fn code_fence() -> Style {
        Style::default().fg(Self::INACTIVE)
    }

    /// 代码块内容行
    pub fn code_body() -> Style {
        Style::default().fg(Self::CODE_FG)
    }

    /// 状态栏品牌标志 ◆ 的颜色
    pub fn claude_brand() -> Style {
        Style::default().fg(Self::CLAUDE)
    }

    /// 进度条已填充颜色（< 70%）
    pub fn progress_normal() -> Style {
        Style::default().fg(Self::SUGGESTION)
    }

    /// 进度条告警颜色（70-90%）
    pub fn progress_warn() -> Style {
        Style::default().fg(Self::WARNING)
    }

    /// 进度条危险颜色（>= 90%）
    pub fn progress_danger() -> Style {
        Style::default().fg(Self::ERROR)
    }

    /// 进度条空余部分
    pub fn progress_empty() -> Style {
        Style::default().fg(Self::INACTIVE)
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

    /// tips 提示行样式
    pub fn welcome_tip() -> Style {
        Style::default().fg(Self::WELCOME_TIP)
    }

    /// 示例提问建议行样式：dim + 斜体，区别于输入框真实 placeholder
    pub fn welcome_suggestion() -> Style {
        Style::default()
            .fg(Self::INACTIVE)
            .add_modifier(Modifier::ITALIC)
    }
}
