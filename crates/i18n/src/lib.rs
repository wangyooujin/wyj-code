//! wyj-code 多语言支持：集中封装 rust-i18n，其他 crate 只调用普通函数，
//! 不直接使用 `t!` 宏，避免宏跨 crate 使用的复杂性。

use rust_i18n::t;

rust_i18n::i18n!("locales", fallback = "en");

/// 当前版本支持的语言列表
pub const AVAILABLE_LOCALES: &[&str] = &["en", "zh"];

/// 切换全局当前语言（影响后续所有 tr()/tr_* 调用）
pub fn set_locale(locale: &str) {
    rust_i18n::set_locale(locale);
}

/// 返回当前生效的语言标识
pub fn current_locale() -> String {
    rust_i18n::locale().to_string()
}

/// 按 key 查询翻译（无插值参数）
pub fn tr(key: &str) -> String {
    t!(key).to_string()
}

/// 按 key 查询翻译，并用 `args` 对模板里的 `{name}` 占位符做字符串替换。
/// 用普通字符串替换而非 rust-i18n 的 `%{}` 宏插值，因为调用方的插值参数
/// （如动态的模型名、数字）在编译期未知，宏插值要求参数名在编译期固定。
pub fn tr_fmt(key: &str, args: &[(&str, &str)]) -> String {
    let mut s = tr(key);
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 语言的本地化原生名称展示（不随当前 UI 语言变化，如系统语言选择器的习惯）
pub fn locale_display_name(locale: &str) -> &'static str {
    match locale {
        "zh" => "中文",
        _ => "English",
    }
}

/// 读取系统 locale 环境变量（LC_ALL/LC_MESSAGES/LANG），按 POSIX 优先级取
/// **第一个非空值**：前缀匹配 zh 则返回 "zh"，否则回退 "en"。
///
/// 旧实现是「任一变量以 zh 开头就返回 zh」，于是 `LC_ALL=en_US` + `LANG=zh_CN`
/// 会返回中文——既违背 POSIX 语义，也让本 crate 的 locale 测试在中文开发机上
/// 必红（`LANG=zh_CN.UTF-8` 是常见默认值）。
pub fn detect_system_locale() -> &'static str {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            if val.trim().is_empty() {
                continue;
            }
            return if val.to_lowercase().starts_with("zh") {
                "zh"
            } else {
                "en"
            };
        }
    }
    "en"
}

/// 解析「当前生效的 locale」：`config.toml` 的 `language` 优先，留空回退系统检测。
///
/// 这是 `language` 字段的唯一语义入口。三处曾经各自复刻这段
/// `configured.unwrap_or_else(detect_system_locale)`，一旦某处漏读（比如只喂给
/// `set_locale` 却没喂给模型侧），语言就会在 UI 与模型输出之间劈叉——2026-10-15
/// 实测就是这种形态：UI 全中文、thinking 全英文，而配置里明明写着 `language = "zh"`。
pub fn resolve_locale(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| detect_system_locale().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // set_locale 修改的是 rust-i18n 的全局状态，同一进程内的测试若并行跑会互相
    // 干扰；把所有涉及切换 locale 的断言收进同一个测试函数里顺序执行，避免依赖
    // 测试执行顺序或引入额外的同步原语。
    #[test]
    fn tr_and_tr_fmt_work_across_both_locales_after_set_locale() {
        set_locale("zh");
        assert_eq!(current_locale(), "zh");
        assert_eq!(tr("help.desc"), "显示所有可用命令和快捷键");
        assert_eq!(
            tr_fmt("command.unknown", &[("name", "foo")]),
            tr_fmt("command.unknown", &[("name", "foo")]) // 存在性冒烟：不 panic 即可
        );

        // 底部进度区（输入框标题栏 spinner / TodoWrite 面板）：两种语言都得真的
        // 被翻译，且三种插值占位符都接得上。未解析时 rust-i18n 会原样回显 key，
        // 那正是"漏了一半翻译"最难被发现的形态，所以在两个 locale 各查一遍。
        assert_eq!(
            tr_fmt(
                "spinner.queue_hint",
                &[("count", "3"), ("running", "等待送达")]
            ),
            "3 条已排队（等待送达）"
        );
        assert_eq!(
            tr_fmt("spinner.queue_op_running", &[("op", "Bash(sleep 540)")]),
            "Bash(sleep 540) 进行中"
        );
        assert_eq!(
            tr_fmt("thinking.running", &[("frame", "⠹")]),
            "  ⎿ ⠹ 思考中…"
        );
        let zh: Vec<String> = SPINNER_KEYS.iter().map(|k| tr(k)).collect();

        set_locale("en");
        assert_eq!(current_locale(), "en");
        assert_eq!(
            tr("help.desc"),
            "Show all available commands and keyboard shortcuts"
        );
        assert_eq!(
            tr_fmt(
                "spinner.queue_hint",
                &[("count", "3"), ("running", "awaiting delivery")]
            ),
            "3 queued (awaiting delivery)"
        );
        assert_eq!(
            tr_fmt("thinking.running", &[("frame", "⠹")]),
            "  ⎿ ⠹ Thinking…"
        );
        let en: Vec<String> = SPINNER_KEYS.iter().map(|k| tr(k)).collect();
        for ((key, en), z) in SPINNER_KEYS.iter().zip(&en).zip(&zh) {
            assert!(!en.starts_with("spinner."), "en 的 {key} 未解析：{en:?}");
            assert!(!z.starts_with("spinner."), "zh 的 {key} 未翻译：{z:?}");
            assert_ne!(en, z, "{key} 在 zh 下没翻：{z:?}");
        }

        // 缺失 key 时 rust-i18n 按约定原样回显 key（不 panic），冒烟验证不会崩
        let missing = tr("this.key.does.not.exist");
        assert!(!missing.is_empty());
    }

    /// 底部进度区的全部 key。两 locale 必须**逐条都有**且措辞不同 —— 这些文案
    /// 曾以中英混排的形式硬编码在 `crates/tui/src/render.rs` 里，漏一条就会让
    /// 切换 locale 后底部看起来只翻译了一半。
    const SPINNER_KEYS: &[&str] = &[
        "spinner.thinking",
        "spinner.waiting_approval",
        "spinner.reviewing_plan",
        "spinner.queuing_message",
        "spinner.queue_pending",
        "spinner.todo_title",
        "spinner.todo_done",
        "spinner.todo_expand",
        "spinner.todo_collapse",
    ];

    #[test]
    fn locale_display_name_has_native_names() {
        assert_eq!(locale_display_name("zh"), "中文");
        assert_eq!(locale_display_name("en"), "English");
        assert_eq!(locale_display_name("unknown"), "English"); // 未知 locale 回退英文
    }

    #[test]
    fn detect_system_locale_prefers_zh_prefixed_env_and_falls_back_to_en() {
        // env::set_var 修改进程级全局状态，同一进程内其他测试线程可能同时读取，
        // 这里只操作本测试专用的 LC_ALL，跑完立即清理，把干扰面降到最小。
        unsafe {
            std::env::set_var("LC_ALL", "zh_CN.UTF-8");
        }
        assert_eq!(detect_system_locale(), "zh");

        unsafe {
            std::env::set_var("LC_ALL", "en_US.UTF-8");
        }
        assert_eq!(detect_system_locale(), "en");

        unsafe {
            std::env::remove_var("LC_ALL");
        }
    }

    /// `language` 字段的唯一语义入口：配置优先，空串/空白等同「未配置」。
    ///
    /// 回退分支**只断言「非空」**：它要读 `LC_ALL`，而同 crate 的
    /// `detect_system_locale_prefers_zh_prefixed_env_and_falls_back_to_en`
    /// 会并发改写同一个环境变量，断言具体值就是断言一个正在被改的值（实测
    /// 并行跑必 flaky）。回退正确性由那个测试负责，这里只守住本函数自己的
    /// 不变式——空白配置不得产出空 locale。
    #[test]
    fn resolve_locale_prefers_config_and_never_yields_an_empty_locale() {
        assert_eq!(resolve_locale(Some("zh")), "zh");
        assert_eq!(resolve_locale(Some(" en ")), "en");
        for blank in [Some(""), Some("   "), None] {
            let resolved = resolve_locale(blank);
            assert!(
                !resolved.trim().is_empty(),
                "未配置时不得产出空 locale（会让 <env> 里的语言声明消失）"
            );
        }
    }
}
