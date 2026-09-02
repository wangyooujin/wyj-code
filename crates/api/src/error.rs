//! 供应商错误的脱敏、可判定表示。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Authentication,
    PermissionDenied,
    RateLimited,
    Overloaded,
    ContextLengthExceeded,
    UnsupportedParameter,
    InvalidRequest,
    InvalidToolSchema,
    StreamTruncated,
    Timeout,
    Network,
    SafetyBlocked,
    /// 首次启动 + `~/.wyj-code` 缺失 + 用户尚未填入 API Key 的特殊状态。
    /// 只有 `MissingKeyProvider` 占位实现会返回这个 kind;TUI 通过它拦截
    /// 引导用户填写 Profile,而非抛普通 Authentication 错误。
    MissingApiKey,
    Unknown,
}

#[derive(Debug, Clone, Error)]
#[error("{kind:?}: {redacted_message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub provider_status: Option<u16>,
    pub retry_after: Option<Duration>,
    pub retryable: bool,
    pub parameter: Option<String>,
    pub request_id: Option<String>,
    /// 只允许保存有界、已脱敏的信息；不得放入认证 header 或完整响应正文。
    pub redacted_message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        const MAX_MESSAGE_CHARS: usize = 2_000;
        if message.chars().count() > MAX_MESSAGE_CHARS {
            message = message.chars().take(MAX_MESSAGE_CHARS).collect();
            message.push_str("...[truncated]");
        }
        Self {
            kind,
            provider_status: None,
            retry_after: None,
            retryable: matches!(
                kind,
                ProviderErrorKind::RateLimited
                    | ProviderErrorKind::Overloaded
                    | ProviderErrorKind::StreamTruncated
                    | ProviderErrorKind::Timeout
                    | ProviderErrorKind::Network
            ),
            parameter: None,
            request_id: None,
            redacted_message: message,
        }
    }

    pub fn from_http(
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> Self {
        let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
        let message = parsed
            .as_ref()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .or_else(|| value.get("error").filter(|value| value.is_string()))
            })
            .and_then(|value| value.as_str())
            .unwrap_or("provider returned an error");
        let parameter = parsed
            .as_ref()
            .and_then(|value| value.pointer("/error/param").or_else(|| value.get("param")))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let lower = message.to_ascii_lowercase();
        let kind = match status.as_u16() {
            401 => ProviderErrorKind::Authentication,
            403 => ProviderErrorKind::PermissionDenied,
            408 => ProviderErrorKind::Timeout,
            429 => ProviderErrorKind::RateLimited,
            529 => ProviderErrorKind::Overloaded,
            500..=599 => ProviderErrorKind::Overloaded,
            _ if lower.contains("context length")
                || lower.contains("maximum context")
                || lower.contains("too many tokens") =>
            {
                ProviderErrorKind::ContextLengthExceeded
            }
            _ if lower.contains("unsupported") && lower.contains("parameter") => {
                ProviderErrorKind::UnsupportedParameter
            }
            _ if lower.contains("tool") && lower.contains("schema") => {
                ProviderErrorKind::InvalidToolSchema
            }
            _ if status.is_client_error() => ProviderErrorKind::InvalidRequest,
            _ => ProviderErrorKind::Unknown,
        };
        let request_id = ["x-request-id", "request-id", "cf-ray"]
            .iter()
            .find_map(|name| headers.get(*name))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut error = Self::new(kind, redact_message(message));
        error.provider_status = Some(status.as_u16());
        error.retry_after = crate::retry::parse_retry_after(headers);
        error.parameter = parameter;
        error.request_id = request_id;
        error
    }

    pub fn from_transport(error: &reqwest::Error) -> Self {
        let kind = if error.is_timeout() {
            ProviderErrorKind::Timeout
        } else {
            ProviderErrorKind::Network
        };
        Self::new(kind, redact_message(&error.to_string()))
    }
}

fn redact_message(message: &str) -> String {
    let mut words = Vec::new();
    let mut redact_next = false;
    for word in message.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if redact_next {
            words.push("[redacted]".to_string());
            redact_next = false;
        } else if lower == "bearer" || lower.ends_with("api_key=") || lower.ends_with("api-key:") {
            words.push(word.to_string());
            redact_next = true;
        } else if word.starts_with("sk-") && word.len() > 12 {
            words.push("[redacted-key]".to_string());
        } else {
            words.push(word.to_string());
        }
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_is_classified_bounded_and_redacted() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-request-id", "req-1".parse().unwrap());
        let error = ProviderError::from_http(
            reqwest::StatusCode::BAD_REQUEST,
            &headers,
            r#"{"error":{"message":"unsupported parameter reasoning; Bearer sk-secret-value","param":"reasoning"}}"#,
        );
        assert_eq!(error.kind, ProviderErrorKind::UnsupportedParameter);
        assert_eq!(error.parameter.as_deref(), Some("reasoning"));
        assert_eq!(error.request_id.as_deref(), Some("req-1"));
        assert!(!error.redacted_message.contains("sk-secret-value"));
    }
}
