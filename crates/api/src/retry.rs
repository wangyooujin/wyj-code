//! HTTP 请求重试：指数退避 + 抖动 + Retry-After 支持。
//!
//! 只覆盖"流尚未开始消费"的连接前阶段（发请求 → 检查 status），此时重试
//! 对调用方完全透明。流中断（已消费部分事件）的重试由 core::agent 处理，
//! 因为需要丢弃半成品缓冲并重新发起整轮请求。

use anyhow::Result;
use std::time::Duration;

/// 重试策略参数
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 最大重试次数（不含首次尝试）
    pub max_retries: u32,
    /// 退避基数（指数增长起点）
    pub base_delay: Duration,
    /// 单次退避上限
    pub max_delay: Duration,
    /// Retry-After 响应头的接受上限：超过则不等待、直接失败
    pub retry_after_cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(32),
            retry_after_cap: Duration::from_secs(60),
        }
    }
}

/// 判断 HTTP 状态码是否可重试：
/// 408（请求超时）/409（冲突）/429（限流）/5xx（含 529 overloaded）可重试；
/// 400/401/403/404/413 等请求本身有问题的错误立即失败。
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429) || (500..=599).contains(&status)
}

/// 解析 Retry-After 响应头：支持秒数与 HTTP-date 两种格式。
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = v.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date 格式：与当前时间的差值
    let when = httpdate::parse_http_date(v).ok()?;
    when.duration_since(std::time::SystemTime::now()).ok()
}

/// 第 n 次重试（0-based）的退避时长：`min(base * 2^n, max)` ± 20% 抖动。
/// 抖动用时钟纳秒取模实现，避免引入 rand 依赖。
pub fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let exp = policy
        .base_delay
        .saturating_mul(2u32.saturating_pow(attempt))
        .min(policy.max_delay);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    // 抖动范围 [-20%, +20%]
    let jitter_pct = (nanos % 41) as i64 - 20;
    let base_ms = exp.as_millis() as i64;
    let jittered = base_ms + base_ms * jitter_pct / 100;
    Duration::from_millis(jittered.max(0) as u64)
}

/// 带重试地发送请求并校验状态码。`build` 每次调用须返回等价的新
/// RequestBuilder（reqwest 的 builder 不可复用）。返回已确认 2xx 的响应，
/// 流的后续消费错误不在此层处理。
pub async fn send_with_retry(
    policy: &RetryPolicy,
    provider_name: &str,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let mut attempt: u32 = 0;
    loop {
        let send_result = build().send().await;
        let can_retry = attempt < policy.max_retries;

        match send_result {
            // 连接层错误（DNS/超时/连接重置）：可重试
            Err(e) => {
                if !can_retry {
                    return Err(anyhow::Error::new(
                        crate::error::ProviderError::from_transport(&e),
                    ));
                }
                let delay = backoff_delay(policy, attempt);
                tracing::warn!(
                    "{provider_name} 连接失败（第 {} 次尝试），{delay:?} 后重试: {e}",
                    attempt + 1
                );
                tokio::time::sleep(delay).await;
            }
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                let retryable = is_retryable_status(status.as_u16());
                if !retryable || !can_retry {
                    let headers = resp.headers().clone();
                    let err = resp.text().await.unwrap_or_default();
                    return Err(anyhow::Error::new(crate::error::ProviderError::from_http(
                        status, &headers, &err,
                    )));
                }
                // 429/529 优先尊重 Retry-After；超过上限则不傻等，直接失败
                let delay = match parse_retry_after(resp.headers()) {
                    Some(ra) if ra > policy.retry_after_cap => {
                        let headers = resp.headers().clone();
                        let err = resp.text().await.unwrap_or_default();
                        let mut provider_error =
                            crate::error::ProviderError::from_http(status, &headers, &err);
                        provider_error.redacted_message = format!(
                            "{}; Retry-After {ra:?} exceeds configured wait cap",
                            provider_error.redacted_message
                        );
                        return Err(anyhow::Error::new(provider_error));
                    }
                    Some(ra) => ra,
                    None => backoff_delay(policy, attempt),
                };
                tracing::warn!(
                    "{provider_name} 返回 {status}（第 {} 次尝试），{delay:?} 后重试",
                    attempt + 1
                );
                tokio::time::sleep(delay).await;
            }
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        for s in [408u16, 409, 429, 500, 502, 503, 529, 599] {
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
        for s in [400u16, 401, 403, 404, 413, 422] {
            assert!(!is_retryable_status(s), "{s} should NOT be retryable");
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let p = RetryPolicy::default();
        let d0 = backoff_delay(&p, 0);
        let d3 = backoff_delay(&p, 3);
        let d9 = backoff_delay(&p, 9);
        // 抖动 ±20%，用宽松边界断言
        assert!(d0 >= Duration::from_millis(800) && d0 <= Duration::from_millis(1200));
        assert!(d3 >= Duration::from_millis(6400) && d3 <= Duration::from_millis(9600));
        assert!(d9 <= Duration::from_millis(38400)); // 封顶 32s +20%
    }

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_retry_after_http_date() {
        let when = std::time::SystemTime::now() + Duration::from_secs(30);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            httpdate::fmt_http_date(when).parse().unwrap(),
        );
        let d = parse_retry_after(&h).unwrap();
        assert!(d > Duration::from_secs(25) && d <= Duration::from_secs(30));
    }
}
