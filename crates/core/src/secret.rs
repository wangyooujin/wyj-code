//! 持久化边界使用的通用 secret 脱敏。
//!
//! 只处理高置信度的 Key/token 形态，不修改内存中的当前对话，因此模型仍能在
//! 用户明确提供凭证的当前回合使用它；会话、checkpoint 和 trace 落盘前则替换
//! token body，避免设置面板以外的旁路存储长期保留 secret。
//!
//! ## 预筛
//!
//! 本函数挂在所有落盘路径上（会话 / checkpoint / trace / message log / spill /
//! memory / evolution），而这些路径的输入动辄是几 MB 的 JSON。三条正则各扫一遍
//! 全文是纯浪费——绝大多数输出里根本没有任何 secret 形态。入口先做一次
//! `memchr` 风格的前缀探测，无命中直接返回借用值，命中才跑正则。

use std::borrow::Cow;
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
            r#"(?i)((?:api[_-]?key|apikey|authorization|secret|access[_-]?token|refresh[_-]?token|token|private[_-]?key)\s*[:=]\s*\\?["']?)([^\s"',;]{16,})"#,
        )
        .unwrap()
    })
}

/// 口令类字段阈值单独放宽到 8：真实口令常见 8-15 位，而 `token` / `api_key`
/// 这类字段名在代码里出现频率极高（`token_count = 42`），阈值降到 8 会大面积误伤。
fn password_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?i)((?:password|passwd|pwd)\s*[:=]\s*\\?["']?)([^\s"',;]{8,})"#).unwrap()
    })
}

fn bearer_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)(\bbearer\s+)([A-Za-z0-9_./+=-]{16,})").unwrap())
}

/// 云厂商 / SaaS 的固定前缀 token。
fn provider_token_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"\b(?:AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})\b",
        )
        .unwrap()
    })
}

/// PEM 私钥块：整块替换，避免只遮住首行、正文仍可被还原。
fn private_key_block_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----")
            .unwrap()
    })
}

/// 连接串里的密码：`postgres://user:pass@host` / `redis://:pass@host`。
fn url_credentials_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://[^:/@\s]+:)([^@\s/]{3,})(@)").unwrap()
    })
}

/// 预筛关键词。宁可多跑几次正则（假阳性），也不能漏掉真 secret（假阴性）——
/// 所以这里只做「快速排除」，命中集合刻意比正则本身更宽。
const PREFLIGHT_NEEDLES: &[&str] = &[
    "sk-",
    "AKIA",
    "ASIA",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xox",
    "eyJ",
    "Bearer",
    "bearer",
    "PRIVATE KEY",
    "api_key",
    "apikey",
    "API_KEY",
    "access_token",
    "refresh_token",
    "secret",
    "Secret",
    "SECRET",
    "token",
    "Token",
    "password",
    "Password",
    "passwd",
    "://",
];

fn might_contain_secret(input: &str) -> bool {
    PREFLIGHT_NEEDLES
        .iter()
        .any(|needle| input.contains(needle))
}

pub fn redact_sensitive_text(input: &str) -> String {
    redact_cow(input).into_owned()
}

/// 借用版：没有 secret 形态时返回 `Cow::Borrowed`，避免落盘路径上的整串复制。
pub fn redact_cow(input: &str) -> Cow<'_, str> {
    if !might_contain_secret(input) {
        return Cow::Borrowed(input);
    }
    let redacted = private_key_block_pattern().replace_all(input, REDACTED_SECRET);
    let redacted = sk_key_pattern().replace_all(&redacted, REDACTED_SECRET);
    let redacted = provider_token_pattern().replace_all(&redacted, REDACTED_SECRET);
    let redacted = assignment_pattern().replace_all(&redacted, |captures: &regex::Captures<'_>| {
        format!("{}{}", &captures[1], REDACTED_SECRET)
    });
    let redacted = password_pattern().replace_all(&redacted, |captures: &regex::Captures<'_>| {
        format!("{}{}", &captures[1], REDACTED_SECRET)
    });
    let redacted =
        url_credentials_pattern().replace_all(&redacted, |captures: &regex::Captures<'_>| {
            format!("{}{}{}", &captures[1], REDACTED_SECRET, &captures[3])
        });
    bearer_pattern()
        .replace_all(&redacted, |captures: &regex::Captures<'_>| {
            format!("{}{}", &captures[1], REDACTED_SECRET)
        })
        .into_owned()
        .into()
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

    #[test]
    fn redacts_provider_tokens_and_pem_blocks() {
        let aws = "AKIAIOSFODNN7EXAMPLE";
        let gh = format!("ghp_{}", "a".repeat(24));
        let slack = format!("xoxb-{}-{}", "1".repeat(12), "b".repeat(12));
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----";
        let input = format!("{aws} {gh} {slack}\n{pem}");
        let output = redact_sensitive_text(&input);
        for secret in [aws, gh.as_str(), slack.as_str(), "MIIEow"] {
            assert!(!output.contains(secret), "{secret} 应被脱敏: {output}");
        }
        assert_eq!(output.matches(REDACTED_SECRET).count(), 4);
    }

    #[test]
    fn redacts_passwords_and_url_credentials() {
        let input =
            "password = \"hunter2hunter2\"; DATABASE_URL=postgres://user:s3cretpw@db:5432/app";
        let output = redact_sensitive_text(input);
        assert!(!output.contains("hunter2hunter2"));
        assert!(!output.contains("s3cretpw"));
        assert!(output.contains("postgres://user:[REDACTED_SECRET]@db:5432/app"));
    }

    #[test]
    fn preflight_returns_borrowed_text_when_nothing_matches() {
        let plain = "fn main() { println!(\"hello\"); }".repeat(1000);
        assert!(matches!(redact_cow(&plain), std::borrow::Cow::Borrowed(_)));
        assert!(matches!(
            redact_cow("token = abcdefghijklmnop"),
            std::borrow::Cow::Owned(_)
        ));
    }
}
