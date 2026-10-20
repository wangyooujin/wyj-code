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
///
/// # ⚠️ 只可用于未序列化的文本，不可用于 JSON 文档
///
/// 本函数对传入文本做正则替换，而 `assignment_pattern` / `password_pattern`
/// 的前缀含 `\\?["']?`——它会咬住一个反斜杠 + 一个引号。把它用在**已序列化的
/// JSON 文本**上时，JSON 的转义引号 `\"` 会被拆开：反斜杠留在 `captures[1]`
/// 里回填，而结尾的 `"` 被匹配掉却不回填。结果是输出里出现一个**未转义的裸
/// 引号**，字符串提前终止，整个文件解析失败。
///
/// 实测（2026-10-20）：`~/.wyj-code/sessions` 下 120 个 checkpoint（约 133 MB）
/// 因此损坏，`serde_json` 报 `Invalid \escape`，永久读不出。
///
/// 修正则治不了本：只要还对序列化后的文本做替换，任何 `\` + 引号的组合都可能
/// 被改写，而在 JSON 里「改写引号」就等于破坏结构。落盘 JSON 一律改用
/// [`redact_json_value`]（值级递归，会话与 checkpoint 两条落盘路径共用）。
/// 本函数保留给**字段值 / 单行文本**路径：memory / evolution / trace。
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

/// 对任意 JSON 值做**结构安全**的脱敏：只改写 string 叶子，不碰 JSON 语法。
///
/// 这是落盘路径的正确入口。与 [`redact_cow`] 的区别是关键：
/// `redact_cow` 作用在序列化后的**文本**上，正则可能改写引号与反斜杠；
/// 本函数作用在**已反序列化的值**上，改写只发生在字符串内容里，
/// 之后由序列化器统一转义，JSON 结构不会被破坏。
///
/// 覆盖两种形态：
/// - 值为字符串 → 直接脱敏；
/// - 值为对象 → 按 key 名判断是否**整值替换**（`api_key` / `token` /
///   `password` 等字段名本身可信，值再短也该遮）；
/// - 数组 / 嵌套对象 → 递归。
///
/// **跳过两类内容块**（整块不动）：
/// - `thinking` / `redacted_thinking`：正文必须连同 `signature` 原样回传，
///   改写一个字节就会让两端的签名校验失配（同 `persist_cap` 不截断 thinking
///   的理由）。这两类正文是模型自己生成的，不是用户粘贴的凭据来源。
/// - `image`：`data` 是 base64，改写任意字符都会损坏图片，而且 base64 字母表
///   里不含文本形态的凭据。
///
/// 就地修改，避免为几 MB 的会话再复制一份。
pub fn redact_json_value(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::String(text) => {
            if let Cow::Owned(redacted) = redact_cow(text) {
                *text = redacted;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        Value::Object(map) => {
            // 判据必须同时看 `type` 与块的特征字段：用户数据（tool_use 的
            // input JSON）里也可能有 `"type": "image"` 这种字段，只看 type
            // 会把它整块跳过，等于漏掉一次脱敏。
            let opaque = match map.get("type").and_then(Value::as_str) {
                Some("thinking") => map.contains_key("thinking"),
                Some("redacted_thinking") => map.contains_key("data"),
                Some("image") => map.contains_key("source"),
                _ => false,
            };
            if opaque {
                return;
            }
            for (key, item) in map.iter_mut() {
                // 字段名可信时整值替换：值可能短于正则阈值（`password = "abc"`），
                // 但它出现在一个明确的凭据字段下，语义上没有别的解释。
                if is_credential_field_name(key) && value_is_nonempty(item) {
                    *item = Value::String(REDACTED_SECRET.to_string());
                    continue;
                }
                redact_json_value(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn value_is_nonempty(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// 字段名是否本身就是凭据语义。**必须整词匹配**，否则 `token_count`
/// 这类常见字段会被大面积误伤（与 `password_pattern` 的阈值放宽同一考量）。
fn is_credential_field_name(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let normalized = lower.replace(['-', ' '], "_");
    matches!(
        normalized.as_str(),
        "api_key"
            | "apikey"
            | "api_secret"
            | "secret"
            | "secret_key"
            | "client_secret"
            | "access_token"
            | "refresh_token"
            | "auth_token"
            | "bearer_token"
            | "private_key"
            | "signing_key"
            | "password"
            | "passwd"
            | "pwd"
    )
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

    /// thinking 必须连同 signature 原样回传：脱敏改一个字节，resume 后签名校验
    /// 就会失配。旧实现（文本级 `redact_cow`）在这里是**改**的，等于把会话写坏；
    /// 值级脱敏必须显式跳过。
    #[test]
    fn thinking_blocks_are_never_rewritten() {
        let secret = format!("sk-{}", "A".repeat(32));
        let mut value = serde_json::json!({
            "type": "thinking",
            "thinking": format!("用户的 key 是 {secret}"),
            "signature": "sig=="
        });
        redact_json_value(&mut value);
        assert_eq!(value["thinking"], format!("用户的 key 是 {secret}"));
    }

    /// image 的 data 是 base64，改写任意字符都会损坏图片。
    #[test]
    fn image_blocks_are_never_rewritten() {
        let data = format!("{}{}", "QUJD".repeat(4), "train"); // 含预筛 needle 也无所谓
        let mut value = serde_json::json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": data }
        });
        redact_json_value(&mut value);
        assert_eq!(value["source"]["data"], data);
    }

    /// 跳过判据不能只看 `type`：用户 tool_use 的 input 里也可能有
    /// `"type": "image"`，那属于普通数据，必须照常脱敏。
    #[test]
    fn user_json_that_happens_to_use_an_image_type_is_still_redacted() {
        let secret = format!("sk-{}", "B".repeat(32));
        let mut value = serde_json::json!({
            "type": "image",
            "note": format!("api_key={secret}")
        });
        redact_json_value(&mut value);
        assert!(!value["note"].as_str().unwrap().contains(&secret));
    }
}
