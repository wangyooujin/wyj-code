//! 人可读的数值格式化（耗时 / token 数）。
//!
//! **为什么单独成模块**：TUI 聊天区的回合摘要、状态栏、通知正文三处都要把
//! 「耗时」与「token 数」压成短字符串。此前 `format_hms` / `fmt_tokens` 住在
//! `crates/tui/src/app.rs` 里（`pub(crate)`），于是 `crates/core` 的通知渲染
//! 用不了同一套口径——通知正文写的是「耗时 83000 毫秒」，聊天区写的是
//! 「⏱ 1m 23s」，同一件事两种量纲。搬到 core 后三处共用同一份实现，
//! 通知与界面永远一致。

/// 把秒数格式化为 `xh ym zs`，便于一眼辨识长耗时。
///
/// - 小于 10 秒保留一位小数（`0.3s` / `1.2s`）
/// - 10-60 秒显示整数秒（`23s`）
/// - 超过 1 分钟显示 `m s`（`1m 23s`）；超过 1 小时显示 `h m s`（`1h 2m 3s`）
pub fn fmt_duration(secs: f64) -> String {
    if secs < 60.0 {
        if secs < 10.0 {
            format!("{:.1}s", secs)
        } else {
            format!("{:.0}s", secs)
        }
    } else {
        let total = secs as u64;
        let h = total / 3600;
        let m = (total % 3600) / 60;
        let s = total % 60;
        if h > 0 {
            format!("{}h {}m {}s", h, m, s)
        } else {
            format!("{}m {}s", m, s)
        }
    }
}

/// 毫秒 → `fmt_duration` 的秒表示（通知用；事件载荷统一以毫秒为单位）。
pub fn fmt_duration_ms(ms: u64) -> String {
    fmt_duration(ms as f64 / 1000.0)
}

/// 把 token 数压成短字符串。唯一目标是**宽度**：状态栏、输入框标题栏、
/// 子 Agent 行内状态、Todo 详情、`/cost` 面板、通知正文都要把 token 与其它
/// 字段排在同一行，位数越少越好。
///
/// - `>= 1_000_000` → 一位小数的 M（`1.5M`）
/// - `>= 10_000` → 一位小数的 k（`12.3k`）。这一步是 v0.2.31 加的：会话累计
///   轻易破六位数，原来的 `format!("{},{:03}")` 会把 `123,456`（7 字符）整段
///   塞进标题栏，在窄终端里直接把模型名和耗时挤出视野。1000~9999 仍在千分位
///   区间，量级还没到需要牺牲精度的时候。
/// - `>= 1_000` → 千分位（`1,234`）
/// - 其余原样（`678`）
pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n >= 1000 {
        format!("{},{:03}", n / 1000, n % 1000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_switches_unit_at_readable_thresholds() {
        assert_eq!(fmt_duration(0.34), "0.3s");
        assert_eq!(fmt_duration(1.25), "1.2s");
        assert_eq!(fmt_duration(23.4), "23s");
        assert_eq!(fmt_duration(83.0), "1m 23s");
        assert_eq!(fmt_duration(3723.0), "1h 2m 3s");
    }

    #[test]
    fn duration_ms_is_the_millisecond_entry_point() {
        assert_eq!(fmt_duration_ms(83_000), "1m 23s");
        assert_eq!(fmt_duration_ms(0), "0.0s");
    }

    #[test]
    fn tokens_switch_to_k_at_ten_thousand() {
        assert_eq!(fmt_tokens(678), "678");
        assert_eq!(fmt_tokens(1_234), "1,234");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }
}
