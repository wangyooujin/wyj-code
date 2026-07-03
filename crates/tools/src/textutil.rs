//! 文本截断工具 — UTF-8 安全，供各工具共享。
//!
//! 旧实现 `&s[..max]` 按字节切片，`max` 落在多字节字符（中文/emoji）中间
//! 会直接 panic；这里统一回退到最近的 char boundary。

/// 按字节上限截断，保证不切断多字节字符。`s.len() <= max_bytes` 时原样借用返回。
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 保头保尾截断：超限时保留开头 `head_bytes` + 结尾 `tail_bytes`，
/// 中间以标记替代。适合命令输出（报错信息几乎总在尾部）与子 Agent 结果
/// （结论按约定在尾部）。
pub fn truncate_head_tail(s: &str, head_bytes: usize, tail_bytes: usize) -> String {
    if s.len() <= head_bytes + tail_bytes {
        return s.to_string();
    }
    let head = truncate_str(s, head_bytes);
    // 尾部起点同样回退到 char boundary（向后走不会越过 tail_bytes 预算）
    let mut start = s.len() - tail_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let omitted = s.len() - head.len() - (s.len() - start);
    format!("{head}\n…[truncated {omitted} bytes]…\n{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_ascii() {
        assert_eq!(truncate_str("hello", 3), "hel");
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_multibyte_no_panic() {
        // "中" 是 3 字节，切在中间必须回退
        assert_eq!(truncate_str("中文", 4), "中");
        assert_eq!(truncate_str("中文", 2), "");
        assert_eq!(truncate_str("a中文", 3), "a");
        // emoji（4 字节）
        assert_eq!(truncate_str("🦀🦀", 5), "🦀");
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        let s = "AAAA".repeat(100) + &"Z".repeat(100);
        let out = truncate_head_tail(&s, 40, 40);
        assert!(out.starts_with("AAAA"));
        assert!(out.ends_with(&"Z".repeat(40)));
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn head_tail_multibyte_no_panic() {
        let s = "中".repeat(1000);
        let out = truncate_head_tail(&s, 100, 100);
        assert!(out.contains("[truncated"));
        assert!(out.starts_with('中'));
        assert!(out.ends_with('中'));
    }

    #[test]
    fn head_tail_short_passthrough() {
        assert_eq!(truncate_head_tail("short", 100, 100), "short");
    }
}
