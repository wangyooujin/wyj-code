//! 持久化边界使用的通用 secret 脱敏。
//!
//! 只处理高置信度的 Key/token 形态，不修改内存中的当前对话，因此模型仍能在
//! 用户明确提供凭证的当前回合使用它；会话、checkpoint 和 trace 落盘前则替换
//! token body，避免设置面板以外的旁路存储长期保留 secret。

use std::sync::OnceLock;

use regex::Regex;

pub const REDACTED_SECRET: &str = "[REDACTED_SECRET]";

fn sk_key_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)\bsk-[A-Za-z0-9_-]{16,}\b").unwrap())
}

fn assignment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?i)((?:api[_-]?key|apikey|authorization|secret|access[_-]?token|refresh[_-]?token|token)\\?["']?\s*[:=]\s*\\?["']?)([A-Za-z0-9_./+=-]{16,})"#,
        )
        .unwrap()
    })
}

fn bearer_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)(\bbearer\s+)([A-Za-z0-9_./+=-]{16,})").unwrap())
}

pub fn redact_sensitive_text(input: &str) -> String {
    let redacted = sk_key_pattern().replace_all(input, REDACTED_SECRET);
    let redacted = assignment_pattern().replace_all(&redacted, |captures: &regex::Captures<'_>| {
        format!("{}{}", &captures[1], REDACTED_SECRET)
    });
    bearer_pattern()
        .replace_all(&redacted, |captures: &regex::Captures<'_>| {
            format!("{}{}", &captures[1], REDACTED_SECRET)
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_key_assignments_bearer_tokens_and_sk_keys() {
        let sk = format!("{}{}", "sk-test-", "A".repeat(24));
        let assigned = "B".repeat(24);
        let bearer = "C".repeat(24);
        let input =
            format!("plain {sk}; api_key = \\\"{assigned}\\\"; Authorization: Bearer {bearer}");
        let output = redact_sensitive_text(&input);
        assert!(!output.contains(&sk));
        assert!(!output.contains(&assigned));
        assert!(!output.contains(&bearer));
        assert_eq!(output.matches(REDACTED_SECRET).count(), 3);
    }

    #[test]
    fn leaves_short_placeholders_and_normal_code_unchanged() {
        let input = "api_key = \"placeholder\"; let token_count = 42;";
        assert_eq!(redact_sensitive_text(input), input);
    }
}
