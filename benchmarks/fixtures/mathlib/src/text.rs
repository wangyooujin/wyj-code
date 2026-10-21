/// Split a line into whitespace-separated fields, dropping empty fields.
pub fn fields(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

/// Truncate to at most `max` characters (not bytes), appending an ellipsis
/// when something was actually cut. Returns an empty string for `max == 0`.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Count how many times `needle` occurs in `haystack`, without overlapping.
pub fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_drops_empty() {
        assert_eq!(fields("  a   b  "), vec!["a", "b"]);
        assert!(fields("   ").is_empty());
    }

    #[test]
    fn truncate_is_char_aware() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abc", 0), "");
        // 多字节字符不能被切成半个
        assert_eq!(truncate("中文测试", 2), "中文…");
    }

    #[test]
    fn count_occurrences_non_overlapping() {
        assert_eq!(count_occurrences("aaaa", "aa"), 2);
        assert_eq!(count_occurrences("abc", ""), 0);
    }
}
