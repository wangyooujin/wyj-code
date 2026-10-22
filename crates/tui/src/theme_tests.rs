//! 主题层的三个不变式钉子。
//!
//! **为什么单列一个文件**：`theme_tests` 检查的不是运行时行为而是**源码文本**，
//! 所以它必须与被检查的文件在同一 crate 里（`include_str!` 拿不到相对路径）。
//! 而它检查的规则是「新增颜色前先读」的那三条——把它们写成测试之后，
//! 规则就不再依赖下一个人记得。

use ratatui::style::Color;
use serde_json::json;

use crate::theme::{TerminalCaps, Theme, ThemePalette};

/// 生产代码里出现的所有槽位。
///
/// **新增槽位必须同时改这里**——这是刻意的：三个钉子全都按这个列表逐项验证，
/// 少加一项等于三个测试同时失去对新槽位的覆盖，且没有任何报错。
const SLOTS: &[&str] = &[
    "success",
    "error",
    "warning",
    "claude",
    "inactive",
    "suggestion",
    "text",
    "text_dim",
    "border",
    "code_fg",
    "inline_code",
    "selected_bg",
    "plan",
    "danger",
    "hint",
    "info",
    "queue",
    "selection_fg",
    "cursor",
    "muted",
    "code_keyword",
    "code_string",
    "code_number",
    "code_comment",
    "welcome_logo_gradient_start",
    "welcome_logo_gradient_mid",
    "welcome_logo_gradient_end",
    "welcome_tip",
];

fn slot_color(slot: &str) -> Color {
    match slot {
        "success" => Theme::SUCCESS,
        "error" => Theme::ERROR,
        "warning" => Theme::WARNING,
        "claude" => Theme::CLAUDE,
        "inactive" => Theme::INACTIVE,
        "suggestion" => Theme::SUGGESTION,
        "text" => Theme::TEXT,
        "text_dim" => Theme::TEXT_DIM,
        "border" => Theme::BORDER,
        "code_fg" => Theme::CODE_FG,
        "inline_code" => Theme::INLINE_CODE,
        "selected_bg" => Theme::SELECTED_BG,
        "plan" => Theme::PLAN,
        "danger" => Theme::DANGER,
        "hint" => Theme::HINT,
        "info" => Theme::INFO,
        "queue" => Theme::QUEUE,
        "selection_fg" => Theme::SELECTION_FG,
        "cursor" => Theme::CURSOR,
        "muted" => Theme::MUTED,
        "code_keyword" => Theme::CODE_KEYWORD,
        "code_string" => Theme::CODE_STRING,
        "code_number" => Theme::CODE_NUMBER,
        "code_comment" => Theme::CODE_COMMENT,
        "welcome_logo_gradient_start" => Theme::WELCOME_LOGO_GRADIENT_START,
        "welcome_logo_gradient_mid" => Theme::WELCOME_LOGO_GRADIENT_MID,
        "welcome_logo_gradient_end" => Theme::WELCOME_LOGO_GRADIENT_END,
        "welcome_tip" => Theme::WELCOME_TIP,
        other => panic!("SLOTS 里有未知槽位 `{other}`"),
    }
}

/// 生产区源码（跳过 `#[cfg(test)] mod` 之后的内容）。
///
/// 测试模块里必然有裸 `Color::`——它们就是在断言这些颜色，不该被这条规则扫到。
fn production_source(file: &str) -> String {
    let source = match file {
        "render.rs" => include_str!("render.rs"),
        "markdown.rs" => include_str!("markdown.rs"),
        "welcome.rs" => include_str!("welcome.rs"),
        "hyperlink.rs" => include_str!("hyperlink.rs"),
        "input.rs" => include_str!("input.rs"),
        "app.rs" => include_str!("app.rs"),
        // 从 render.rs 拆出的对话框子模块：**必须一起登记**。漏了它，
        // 近 3000 行绘制代码就脱离这条配色守卫，而这正是它当初被拆出来的那
        // 部分实现。
        "render/dialogs.rs" => include_str!("render/dialogs.rs"),
        "app/agents_panel.rs" => include_str!("app/agents_panel.rs"),
        "app/clipboard.rs" => include_str!("app/clipboard.rs"),
        "app/dialogs.rs" => include_str!("app/dialogs.rs"),
        "app/effort_panel.rs" => include_str!("app/effort_panel.rs"),
        "app/events.rs" => include_str!("app/events.rs"),
        "app/mcp_panel.rs" => include_str!("app/mcp_panel.rs"),
        "app/menu.rs" => include_str!("app/menu.rs"),
        "app/model_panel.rs" => include_str!("app/model_panel.rs"),
        "app/navigation.rs" => include_str!("app/navigation.rs"),
        "app/panels.rs" => include_str!("app/panels.rs"),
        "app/plugins_panel.rs" => include_str!("app/plugins_panel.rs"),
        "app/profile_panel.rs" => include_str!("app/profile_panel.rs"),
        "app/settings_dialogs.rs" => include_str!("app/settings_dialogs.rs"),
        "app/skills_panel.rs" => include_str!("app/skills_panel.rs"),
        "app/sub_agent_view.rs" => include_str!("app/sub_agent_view.rs"),
        "app/terminal.rs" => include_str!("app/terminal.rs"),
        "app/tool_view.rs" => include_str!("app/tool_view.rs"),
        "app/util.rs" => include_str!("app/util.rs"),
        other => panic!("未登记的待扫描文件 `{other}`"),
    };
    match source.find("#[cfg(test)]") {
        Some(at) => source[..at].to_string(),
        None => source.to_string(),
    }
}

/// ① 生产代码里不许出现裸 `Color::`。
///
/// 这条能落地是因为它检查**源码文本**而非运行时行为——真想写别的颜色，
/// 作者得先来改这条规则，那一刻就已经是有意识的决定。
///
/// 唯一豁免：欢迎页 logo 的渐变插值，它必须处理 RGB 分量，走
/// `Theme::rgb_components_or` / `Theme::rgb_from_components`。
#[test]
fn no_hardcoded_colors_outside_theme() {
    let files = [
        ("render.rs", production_source("render.rs")),
        ("markdown.rs", production_source("markdown.rs")),
        ("welcome.rs", production_source("welcome.rs")),
        ("hyperlink.rs", production_source("hyperlink.rs")),
        ("input.rs", production_source("input.rs")),
        ("app.rs", production_source("app.rs")),
        ("render/dialogs.rs", production_source("render/dialogs.rs")),
        (
            "app/agents_panel.rs",
            production_source("app/agents_panel.rs"),
        ),
        ("app/clipboard.rs", production_source("app/clipboard.rs")),
        ("app/dialogs.rs", production_source("app/dialogs.rs")),
        ("app/events.rs", production_source("app/events.rs")),
        ("app/mcp_panel.rs", production_source("app/mcp_panel.rs")),
        ("app/menu.rs", production_source("app/menu.rs")),
        ("app/navigation.rs", production_source("app/navigation.rs")),
        ("app/panels.rs", production_source("app/panels.rs")),
        (
            "app/plugins_panel.rs",
            production_source("app/plugins_panel.rs"),
        ),
        (
            "app/profile_panel.rs",
            production_source("app/profile_panel.rs"),
        ),
        (
            "app/settings_dialogs.rs",
            production_source("app/settings_dialogs.rs"),
        ),
        (
            "app/skills_panel.rs",
            production_source("app/skills_panel.rs"),
        ),
        (
            "app/sub_agent_view.rs",
            production_source("app/sub_agent_view.rs"),
        ),
        ("app/terminal.rs", production_source("app/terminal.rs")),
        ("app/tool_view.rs", production_source("app/tool_view.rs")),
        ("app/util.rs", production_source("app/util.rs")),
    ];

    // `Color::Rgb(` 只在 welcome.rs 的 logo 渐变里合法（它 match 分量、算插值）
    let gradient_allowed = |file: &str, line: &str| {
        file == "welcome.rs" && matches!(line.trim(), "Color::Rgb(r, g, b) => (r, g, b),")
    };

    for (name, source) in files {
        for (idx, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            // 注释里的 `Color::` 是说明文字，不是真在用
            if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("*") {
                continue;
            }
            if gradient_allowed(name, line) {
                continue;
            }
            assert!(
                !line.contains("Color::"),
                "{name}:{} 生产代码里出现裸 `Color::`，改走 Theme 槽位：\n  {line}",
                idx + 1
            );
        }
    }
}

/// ② 每个槽位都必须接到一个 JSON 键。
///
/// `from_json` 对未知键是**静默忽略**的（插件主题来自用户环境，不能因为拼错
/// 一个键就整份作废），代价是「漏接槽位」和「拼错键名」都拿不到任何提示——
/// 插件主题设了 `plan` 却毫无反应，直到有人报「换肤只生效一半」。
///
/// 用一个**每个槽位颜色都不同**的 JSON 反查：只要有一个槽位没接上，
/// 从 JSON 得到的调色板就会有两个字段相等，或与默认值相等。
#[test]
fn every_palette_slot_is_wired_to_a_json_key() {
    let mut colors = serde_json::Map::new();
    for (idx, slot) in SLOTS.iter().enumerate() {
        // idx 参与颜色取值，保证所有槽位两两不同；且刻意不等于任何默认值
        let v = 10 + idx * 8;
        colors.insert((*slot).to_string(), json!([v, v, v]));
    }
    let palette = ThemePalette::from_json(&json!({ "colors": colors }))
        .expect("每个槽位都该能被 from_json 接住");

    let mut seen: Vec<(&str, Color)> = Vec::new();
    for slot in SLOTS {
        let actual = match *slot {
            "success" => palette.success,
            "error" => palette.error,
            "warning" => palette.warning,
            "claude" => palette.claude,
            "inactive" => palette.inactive,
            "suggestion" => palette.suggestion,
            "text" => palette.text,
            "text_dim" => palette.text_dim,
            "border" => palette.border,
            "code_fg" => palette.code_fg,
            "inline_code" => palette.inline_code,
            "selected_bg" => palette.selected_bg,
            "plan" => palette.plan,
            "danger" => palette.danger,
            "hint" => palette.hint,
            "info" => palette.info,
            "queue" => palette.queue,
            "selection_fg" => palette.selection_fg,
            "cursor" => palette.cursor,
            "muted" => palette.muted,
            "code_keyword" => palette.code_keyword,
            "code_string" => palette.code_string,
            "code_number" => palette.code_number,
            "code_comment" => palette.code_comment,
            "welcome_logo_gradient_start" => palette.welcome_logo_gradient_start,
            "welcome_logo_gradient_mid" => palette.welcome_logo_gradient_mid,
            "welcome_logo_gradient_end" => palette.welcome_logo_gradient_end,
            "welcome_tip" => palette.welcome_tip,
            other => panic!("SLOTS 里有未知槽位 `{other}`"),
        };
        let expected_idx = SLOTS.iter().position(|s| s == slot).expect("槽位在表里");
        let v = 10 + expected_idx * 8;
        assert_eq!(
            actual,
            Color::Rgb(v as u8, v as u8, v as u8),
            "槽位 `{slot}` 没有接到 JSON 键：from_json 返回了默认值"
        );
        assert!(
            !seen
                .iter()
                .any(|(name, color)| name != slot && *color == actual),
            "槽位 `{slot}` 与另一个槽位拿到了同一个颜色，说明它们写到了同一字段"
        );
        seen.push((slot, actual));
    }
    assert_eq!(seen.len(), SLOTS.len());
}

/// ③ `degraded()` 必须逐字段降级，一个都不能漏。
///
/// 漏一个字段的后果极隐蔽：`NO_COLOR` 下只有一个元素颜色不对，且很难联想到
/// 「`degraded()` 里少写了一行」。
#[test]
fn every_slot_is_degraded_not_just_some_of_them() {
    // 给每个槽位一个不同的 RGB，量化后互不相同，才能逐字段比对
    let mut base = ThemePalette::default();
    for (idx, slot) in SLOTS.iter().enumerate() {
        let v = 30 + idx * 7;
        let color = Color::Rgb(v as u8, (v + 1) as u8, (v + 2) as u8);
        match *slot {
            "success" => base.success = color,
            "error" => base.error = color,
            "warning" => base.warning = color,
            "claude" => base.claude = color,
            "inactive" => base.inactive = color,
            "suggestion" => base.suggestion = color,
            "text" => base.text = color,
            "text_dim" => base.text_dim = color,
            "border" => base.border = color,
            "code_fg" => base.code_fg = color,
            "inline_code" => base.inline_code = color,
            "selected_bg" => base.selected_bg = color,
            "plan" => base.plan = color,
            "danger" => base.danger = color,
            "hint" => base.hint = color,
            "info" => base.info = color,
            "queue" => base.queue = color,
            "selection_fg" => base.selection_fg = color,
            "cursor" => base.cursor = color,
            "muted" => base.muted = color,
            "code_keyword" => base.code_keyword = color,
            "code_string" => base.code_string = color,
            "code_number" => base.code_number = color,
            "code_comment" => base.code_comment = color,
            "welcome_logo_gradient_start" => base.welcome_logo_gradient_start = color,
            "welcome_logo_gradient_mid" => base.welcome_logo_gradient_mid = color,
            "welcome_logo_gradient_end" => base.welcome_logo_gradient_end = color,
            "welcome_tip" => base.welcome_tip = color,
            other => panic!("SLOTS 里有未知槽位 `{other}`"),
        }
    }

    // NO_COLOR：全部槽位变成 Reset
    let off = base.degraded(TerminalCaps {
        color_enabled: false,
        truecolor: false,
    });
    for slot in SLOTS {
        let actual = field(&off, slot);
        assert_eq!(
            actual,
            Color::Reset,
            "NO_COLOR 下槽位 `{slot}` 没有降级成 Reset"
        );
    }

    // 非 truecolor：RGB 槽位必须变成 Indexed，具名色不动
    let quantized = base.degraded(TerminalCaps {
        color_enabled: true,
        truecolor: false,
    });
    for slot in SLOTS {
        let before = field(&base, slot);
        let after = field(&quantized, slot);
        match before {
            Color::Rgb(..) => assert!(
                matches!(after, Color::Indexed(_)),
                "非 truecolor 下槽位 `{slot}` 应量化成 xterm 256 色索引，实际 {after:?}"
            ),
            _ => assert_eq!(
                after, before,
                "具名色在非 truecolor 终端里应当保持不变（槽位 `{slot}`）"
            ),
        }
    }

    // truecolor：原样返回，一个字段都不动
    let passthrough = base.degraded(TerminalCaps {
        color_enabled: true,
        truecolor: true,
    });
    for slot in SLOTS {
        assert_eq!(
            field(&passthrough, slot),
            field(&base, slot),
            "truecolor 下槽位 `{slot}` 不该被改动"
        );
    }
}

fn field(palette: &ThemePalette, slot: &str) -> Color {
    match slot {
        "success" => palette.success,
        "error" => palette.error,
        "warning" => palette.warning,
        "claude" => palette.claude,
        "inactive" => palette.inactive,
        "suggestion" => palette.suggestion,
        "text" => palette.text,
        "text_dim" => palette.text_dim,
        "border" => palette.border,
        "code_fg" => palette.code_fg,
        "inline_code" => palette.inline_code,
        "selected_bg" => palette.selected_bg,
        "plan" => palette.plan,
        "danger" => palette.danger,
        "hint" => palette.hint,
        "info" => palette.info,
        "queue" => palette.queue,
        "selection_fg" => palette.selection_fg,
        "cursor" => palette.cursor,
        "muted" => palette.muted,
        "code_keyword" => palette.code_keyword,
        "code_string" => palette.code_string,
        "code_number" => palette.code_number,
        "code_comment" => palette.code_comment,
        "welcome_logo_gradient_start" => palette.welcome_logo_gradient_start,
        "welcome_logo_gradient_mid" => palette.welcome_logo_gradient_mid,
        "welcome_logo_gradient_end" => palette.welcome_logo_gradient_end,
        "welcome_tip" => palette.welcome_tip,
        other => panic!("SLOTS 里有未知槽位 `{other}`"),
    }
}

/// 槽位常量表与 `Default` 一致：默认调色板必须等于常量表的值。
///
/// 上面三个钉子都基于「槽位能被接住 / 能被降级」，而这一条守住**起点**：
/// 槽位常量被改过但 `Default` 没跟上时，插件主题之前的所有界面会跟着变，
/// 而快照只比对文字与样式槽位、不比对具体 RGB。
#[test]
fn default_palette_matches_the_constant_table() {
    let default = ThemePalette::default();
    for slot in SLOTS {
        assert_eq!(
            field(&default, slot),
            slot_color(slot),
            "槽位 `{slot}` 的默认值与常量表不一致"
        );
    }
}

/// ④ 色板必须在设计基准背景上可读。
///
/// 这条钉住的是 `doc/plan/color-system-redesign.md` 的取值承诺：正文层四档
/// 在 `#1E2127` 上逐级变弱但都还能读，语义色则必须至少达到「加粗短标签」
/// 的 WCAG 门槛。**为什么需要它**：改色号是最容易被"顺手调亮一点"的地方，
/// 而亮度一旦失守，用户看到的是「某些字在深色终端里糊成一片」，
/// 却很难联想到是某次提交动了 `theme.rs` 里的一个数字。
#[test]
fn palette_stays_readable_on_the_dark_reference_background() {
    // 设计基准背景（One Dark / Tokyo Night 一类的冷灰暗底）
    let bg = Color::Rgb(30, 33, 39);

    let contrast = |fg: Color| {
        let (r1, g1, b1) = rgb(fg);
        let (r2, g2, b2) = rgb(bg);
        (luminance(r1, g1, b1) + 0.05) / (luminance(r2, g2, b2) + 0.05)
    };

    // 正文与次级正文：长文阅读，按 WCAG AA 正文门槛
    for (name, color) in [("text", Theme::TEXT), ("text_dim", Theme::TEXT_DIM)] {
        let ratio = contrast(color);
        assert!(
            ratio >= 4.5,
            "`{name}` 在基准背景上的对比度只有 {ratio:.2}:1，长文阅读需要 ≥ 4.5:1"
        );
    }

    // 弱化与极弱标记：非正文，但也不能低到看不见
    for (name, color) in [("inactive", Theme::INACTIVE), ("muted", Theme::MUTED)] {
        let ratio = contrast(color);
        assert!(
            ratio >= 2.0,
            "`{name}` 在基准背景上的对比度只有 {ratio:.2}:1，已低到无法辨认"
        );
    }

    // 语义色：用于图标与短标签，门槛取 3.0（与 `ensure_contrast` 一致）
    for (name, color) in [
        ("success", Theme::SUCCESS),
        ("error", Theme::ERROR),
        ("warning", Theme::WARNING),
        ("claude", Theme::CLAUDE),
        ("suggestion", Theme::SUGGESTION),
        ("danger", Theme::DANGER),
        ("hint", Theme::HINT),
        ("info", Theme::INFO),
        ("queue", Theme::QUEUE),
        ("plan", Theme::PLAN),
        ("code_fg", Theme::CODE_FG),
        ("inline_code", Theme::INLINE_CODE),
        ("code_keyword", Theme::CODE_KEYWORD),
        ("code_string", Theme::CODE_STRING),
        ("code_number", Theme::CODE_NUMBER),
        ("code_comment", Theme::CODE_COMMENT),
    ] {
        let ratio = contrast(color);
        assert!(
            ratio >= 3.0,
            "`{name}` 在基准背景上的对比度只有 {ratio:.2}:1，低于 3.0 的短标签门槛"
        );
    }
}

fn rgb(color: Color) -> (f64, f64, f64) {
    match color {
        Color::Rgb(r, g, b) => (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0),
        other => panic!("对比度测试只接受 RGB 槽位，实际 {other:?}"),
    }
}

fn luminance(r: f64, g: f64, b: f64) -> f64 {
    let f = |v: f64| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// ⑤ 正文层必须是四档明度递进，且品牌色不能铺到正文上。
///
/// 色板的核心承诺是「层级靠明度、不靠色相」。这条把它变成可执行的断言：
/// 任何一次调整若让 `text_dim` 比 `text` 还亮、或让正文重新用上品牌橙，
/// 都会立刻变红，而不是等用户反馈「看着有点乱」。
#[test]
fn neutral_ramp_descends_and_body_text_is_not_branded() {
    let lum = |c: Color| {
        let (r, g, b) = rgb(c);
        luminance(r, g, b)
    };
    assert!(
        lum(Theme::TEXT) > lum(Theme::TEXT_DIM),
        "正文必须比次级正文亮"
    );
    assert!(
        lum(Theme::TEXT_DIM) > lum(Theme::INACTIVE),
        "次级正文必须比 dim 亮"
    );
    assert!(
        lum(Theme::INACTIVE) > lum(Theme::MUTED),
        "dim 必须比极弱标记亮"
    );
    assert!(
        lum(Theme::BORDER) < lum(Theme::MUTED),
        "框线是最弱的一档，不能比正文层更亮"
    );

    // 工具结果与正文同级（次级正文），不能是品牌色 / 强调色
    assert_eq!(Theme::tool_result(), Theme::text_dim());
    assert_eq!(
        Theme::body(),
        ratatui::style::Style::default().fg(Theme::TEXT)
    );
}
