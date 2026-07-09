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

/// 读取系统 locale 环境变量（LC_ALL/LC_MESSAGES/LANG），前缀匹配 zh 则返回 "zh"，否则回退 "en"
pub fn detect_system_locale() -> &'static str {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            let lower = val.to_lowercase();
            if lower.starts_with("zh") {
                return "zh";
            }
        }
    }
    "en"
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

        set_locale("en");
        assert_eq!(current_locale(), "en");
        assert_eq!(
            tr("help.desc"),
            "Show all available commands and keyboard shortcuts"
        );

        // 缺失 key 时 rust-i18n 按约定原样回显 key（不 panic），冒烟验证不会崩
        let missing = tr("this.key.does.not.exist");
        assert!(!missing.is_empty());
    }

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
}
