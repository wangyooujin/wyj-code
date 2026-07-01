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
