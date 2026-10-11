//! HTTP 超时与 SSE idle 看门狗。
//!
//! # 背景
//!
//! 修复前 `AnthropicProvider` / `OpenAIProvider` 都用 `reqwest::Client::new()`，
//! 而 reqwest 的默认 client **没有任何超时**（connect / read / total 全为
//! `None`）。SSE 消费循环也没有 idle 看门狗。于是一条被中间设备静默吞掉、
//! 或处于半开状态的连接会让 `Provider::stream()` 永远不返回：
//!
//! ```text
//! Agent::run_turn 永不返回
//!   └─ TUI 里 spawn 出去的 task 全程持有 session Mutex（app.rs:9594）
//!        └─ is_thinking 永远为 true（清零只认 TurnDone / Error，
//!           而这两个事件只在 task 收尾时才发）
//!             └─ 用户后续输入全部进 pending_queue 且无人排空
//! ```
//!
//! 症状就是「任务跑到一半停住，之后输入什么都没反应」。长回合尤其容易命中：
//! 挂在同一条链路上的时间越长，遇到 stall 的概率越高。
//!
//! # 两条防线
//!
//! 1. [`build_client`]：建连超时。握手阶段卡死同样是无期限的。
//! 2. [`with_idle_timeout`]：**流级** idle 看门狗。刻意不设 client 级
//!    total timeout —— 那会把正常的长流式生成一起砍掉；也不依赖 reqwest 的
//!    `read_timeout`，因为它的错误类型不可控，而这里要的是一个能被上层识别为
//!    `StreamTruncated` 的错误，好让 agent 走既有的「丢弃半成品、整轮重生成」
//!    恢复路径（`core/src/agent.rs` 的 `MAX_STREAM_RETRIES`）。
//!
//! 超时值可以按需放宽（自托管端点首 token 很慢时）：
//! `WYJ_CODE_STREAM_IDLE_SECS=<秒>`，只读环境变量、绝不写回配置。

use crate::error::{ProviderError, ProviderErrorKind};
use crate::provider::EventStream;
use anyhow::Result;
use futures::StreamExt;
use std::time::Duration;

/// 建连超时。TLS 握手 + 请求头往返，30s 对公网端点足够宽松。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// SSE 流 idle 超时的默认值。
///
/// 取 120s 的理由：健康端点在生成期间是持续吐 delta 的（thinking 也一样按
/// delta 流出），连续 120s 零字节基本等价于连接已死；而首 token 前的排队、
/// reasoning 模型的沉默段都远在此之内，不会误杀。
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// 实际生效的流 idle 超时（首次读取时解析环境变量，之后固定）。
pub fn stream_idle_timeout() -> Duration {
    static IDLE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *IDLE.get_or_init(|| {
        let fallback = DEFAULT_STREAM_IDLE_TIMEOUT;
        let Some(raw) = std::env::var("WYJ_CODE_STREAM_IDLE_SECS").ok() else {
            return fallback;
        };
        match raw.trim().parse::<u64>() {
            // 0 = 显式关闭看门狗（保留给明知会长时间沉默的自建端点）。
            Ok(0) => Duration::MAX,
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "WYJ_CODE_STREAM_IDLE_SECS 不是合法秒数，回退到默认值"
                );
                fallback
            }
        }
    })
}

/// 统一构造 HTTP client。**不要**在这里设 total timeout。
pub fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()?)
}

/// 给 SSE 流套上 idle 看门狗。
///
/// 语义：距上一个事件超过 `idle` 仍未收到新事件 → 产出**一个**
/// `StreamTruncated` 错误项，然后**结束**流（不会在超时后继续轮询，
/// 否则每轮都会重新计时、把一条死流拖成无限重试）。
///
/// 正常的流结束（`Ok(None)`）原样透传，不受影响。
pub fn with_idle_timeout(inner: EventStream, idle: Duration) -> EventStream {
    // unfold 的状态用 Option：None 表示流已终止（含刚超时那一刻）。
    let state = Some((inner, idle));
    Box::pin(futures::stream::unfold(state, |state| async move {
        let (mut stream, idle) = state?;
        match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(item)) => Some((item, Some((stream, idle)))),
            Ok(None) => None,
            Err(_) => {
                tracing::warn!(
                    idle_secs = %idle.as_secs(),
                    "SSE 流 idle 超时（无事件），按流中断处理"
                );
                let error = ProviderError::new(
                    ProviderErrorKind::StreamTruncated,
                    format!(
                        "provider SSE stream stalled: no event for {}s",
                        idle.as_secs()
                    ),
                );
                Some((Err(anyhow::Error::new(error)), None))
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StreamEvent;
    use futures::Stream;

    fn text_stream(chunks: Vec<(Duration, &'static str)>) -> EventStream {
        Box::pin(futures::stream::unfold(
            (chunks.into_iter(), Duration::ZERO),
            move |(mut it, waited)| async move {
                let (delay, text) = it.next()?;
                let waited = waited + delay;
                if !waited.is_zero() {
                    tokio::time::sleep(waited).await;
                }
                Some((Ok(StreamEvent::TextDelta(text.to_string())), (it, waited)))
            },
        ))
    }

    #[tokio::test]
    async fn idle_watchdog_passes_through_a_healthy_stream() {
        let stream = with_idle_timeout(
            text_stream(vec![
                (Duration::from_millis(10), "a"),
                (Duration::from_millis(10), "b"),
            ]),
            Duration::from_secs(5),
        );
        let collected: Vec<_> = stream.collect().await;
        assert_eq!(collected.len(), 2, "健康流应原样透传: {collected:?}");
        assert!(collected.iter().all(|item| item.is_ok()));
    }

    #[tokio::test]
    async fn idle_watchdog_terminates_a_stalled_stream_with_stream_truncated() {
        // 第一个事件正常到达，之后永久静默。
        let stream = with_idle_timeout(
            text_stream(vec![
                (Duration::ZERO, "a"),
                (Duration::from_secs(3600), "never"),
            ]),
            Duration::from_millis(80),
        );
        let mut stream = std::pin::pin!(stream);
        let first = stream.next().await.expect("首个事件应正常到达");
        assert!(first.is_ok(), "超时前的正常事件不应被误伤: {first:?}");

        let stalled = stream
            .next()
            .await
            .expect("超时必须产出错误项，而不是直接结束流");
        let error = stalled.expect_err("idle 超时应产出 Err");
        let provider_error = error
            .downcast_ref::<ProviderError>()
            .expect("Err 应为 ProviderError");
        assert_eq!(provider_error.kind, ProviderErrorKind::StreamTruncated);
        assert!(provider_error.retryable, "应让上层走既有重试/重生成路径");

        // 关键不变量：超时后流必须结束。否则每轮都会重新开一个 idle 窗口，
        // 一条死流会被拖成无限定时长的假活。
        assert!(
            stream.next().await.is_none(),
            "超时后必须终止流，不得继续轮询"
        );
    }

    #[tokio::test]
    async fn idle_watchdog_leaves_a_cleanly_finished_stream_alone() {
        let stream = with_idle_timeout(
            text_stream(vec![(Duration::ZERO, "only")]),
            Duration::from_secs(5),
        );
        let collected: Vec<_> = stream.collect().await;
        assert_eq!(collected.len(), 1);
        assert!(collected[0].is_ok(), "正常收尾不应被包装成错误");
    }

    /// 建连超时必须存在且有界：握手阶段卡死同样是无期限的。
    ///
    /// 注意 reqwest 的 `Client` **不暴露** `connect_timeout()` / `timeout()`
    /// getter（只有 `ClientBuilder` 上有 setter），所以这里断言的是常量
    /// 本身而非从 client 反读——想从 `Client` 读出来是做不到的。
    /// 「没有设 total timeout」这条不变式靠 `build_client` 的实现维持
    /// （见其文档），无法用运行时断言锁死，改由下面的 `no_client_total_
    /// timeout_in_source` 静态钉子把关。
    #[test]
    fn connect_timeout_is_set_and_bounded() {
        assert!(
            CONNECT_TIMEOUT >= Duration::from_secs(5),
            "过短会误杀慢网络下的正常握手"
        );
        assert!(
            CONNECT_TIMEOUT <= Duration::from_secs(60),
            "过长则失去建连保护的意义"
        );
    }

    /// 静态钉子：`build_client` 里**不得**出现 `.timeout(`。
    ///
    /// 这条不变式是刻意的（设了 total timeout 会把正常的长流式生成砍掉，
    /// 流级保护由 `with_idle_timeout` 负责），但 reqwest 不提供反读手段，
    /// 只能从源码文本钉住。任何人往 `build_client` 加 `.timeout(` 都会红。
    #[test]
    fn no_client_total_timeout_in_source() {
        let src = include_str!("stream_guard.rs");
        let body = src
            .split("pub fn build_client()")
            .nth(1)
            .expect("源码里必须有 build_client");
        let body = body
            .split("/// 给 SSE 流套上 idle 看门狗")
            .next()
            .expect("build_client 之后应有下一个条目");
        assert!(
            !body.contains(".timeout("),
            "build_client 绝不能设 client 级 total timeout：\
             那会把正常的长流式生成一起砍掉（流级保护由 with_idle_timeout 负责）"
        );
        assert!(
            body.contains(".connect_timeout("),
            "建连超时必须显式设置，否则握手卡死无期限"
        );
    }

    /// 仅验证 env 解析逻辑本身（OnceLock 是进程级的，不能在测试里改 env）。
    #[test]
    fn idle_timeout_default_is_positive_and_bounded() {
        let idle = DEFAULT_STREAM_IDLE_TIMEOUT;
        assert!(
            idle >= Duration::from_secs(60),
            "过短会误杀 reasoning 沉默段"
        );
        assert!(idle <= Duration::from_secs(300), "过长则失去看门狗意义");
    }

    fn _assert_stream_bounds<S: Stream>(_: &S) {}
}
