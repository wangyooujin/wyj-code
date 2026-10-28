//! 辅助 LLM 往返的**跨模块用量累加器**。
//!
//! ## 为什么需要它
//!
//! 主推理循环的用量走 `Session::add_usage`（`agent.rs` 每趟 stream 后入账），
//! 但还有五类调用**根本拿不到 `Session`**：
//!
//! | 调用 | 位置 | 为什么拿不到 |
//! |---|---|---|
//! | 会话标题生成 | `summary.rs` | `tokio::spawn` 出去，生命周期独立 |
//! | Auto 权限分类器 | `auto_mode.rs` | 挂在 `ToolContext`，不是 `Agent` |
//! | 记忆提取（v3） | `memory_v3.rs` | 后台 durable job，可能跨回合完成 |
//! | 记忆提取（v2 兼容） | `memory.rs` | 同上 |
//! | 演化分析 | `evolution.rs` | 独立 store，从 CLI 子命令也能触发 |
//!
//! 旧实现把这些往返的 `CompletionResult.usage` **直接丢弃**，`/cost` 与
//! `wyj-code usage` 因此系统性低估真实花费。Auto 模式下分类器每拦一个工具
//! 就是一次往返，一次会话几十次很常见——漏掉的不是零头。
//!
//! ## 为什么是全局 sink 而不是改签名
//!
//! 让这五个模块都接收 `&mut Session` 会把「记账」这件事塞进它们的参数列表，
//! 而这五处没有一处需要会话状态。用一个 `Arc` 原子累加器，调用方只在装配时
//! 注入一次，各模块在自己的调用点 `record()` 一行即可。
//!
//! ## 与 `api_calls` 的关系
//!
//! 辅助往返**不计入** `api_calls`。那个字段的语义是「主推理循环跑了几趟」，
//! 是 `input/api_call` 派生指标的分母；把辅助往返混进去会让「每次调用带多少
//! 上下文」被系统性低估。
//!
//! ## 抽干时机
//!
//! `Agent` 在每个回合开始与结束时各调一次 [`AuxUsageSink::take`]。用
//! `swap(0)` 而非读后清零，保证并发路径（后台记忆 job 与主回合同时结束）
//! 下每个 token 只被计入一次。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// 辅助 LLM 往返的用量快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuxUsage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub calls: u32,
}

impl AuxUsage {
    /// 累加另一次往返。
    pub fn add(&mut self, input: u32, output: u32, cache_read: u32) {
        self.input = self.input.saturating_add(input);
        self.output = self.output.saturating_add(output);
        self.cache_read = self.cache_read.saturating_add(cache_read);
        self.calls = self.calls.saturating_add(1);
    }

    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }
}

/// 共享句柄。`None`（未注入）时各调用点静默跳过记账——辅助调用的正确性
/// 不依赖它，缺了只是统计少一块，不能因此让标题生成或记忆提取失败。
pub type SharedAuxUsage = Arc<AuxUsageTotals>;

/// 原子累加器。
#[derive(Debug, Default)]
pub struct AuxUsageTotals {
    input: AtomicU32,
    output: AtomicU32,
    cache_read: AtomicU32,
    calls: AtomicU32,
}

impl AuxUsageTotals {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一次辅助往返。调用点必须在**检查返回值之前**调它：即使解析失败、
    /// 摘要为空、模型回了一句废话，这轮 token 也已经花掉了。
    pub fn record(&self, input: u32, output: u32, cache_read: u32) {
        self.input.fetch_add(input, Ordering::Relaxed);
        self.output.fetch_add(output, Ordering::Relaxed);
        self.cache_read.fetch_add(cache_read, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
    }

    /// 从 `CompletionResult` 直接记账（省掉调用点各自拆字段）。
    pub fn record_result(&self, result: &wyj_api::CompletionResult) {
        self.record(
            result.input_tokens,
            result.output_tokens,
            result.cache_read_input_tokens,
        );
    }

    /// 抽走并清零。多次并发调用不会重复计账。
    pub fn take(&self) -> AuxUsage {
        AuxUsage {
            input: self.input.swap(0, Ordering::Relaxed),
            output: self.output.swap(0, Ordering::Relaxed),
            cache_read: self.cache_read.swap(0, Ordering::Relaxed),
            calls: self.calls.swap(0, Ordering::Relaxed),
        }
    }

    /// 只看不清零（供 `/cost` 面板在回合中途取当前累计值）。
    pub fn snapshot(&self) -> AuxUsage {
        AuxUsage {
            input: self.input.load(Ordering::Relaxed),
            output: self.output.load(Ordering::Relaxed),
            cache_read: self.cache_read.load(Ordering::Relaxed),
            calls: self.calls.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_drains_so_tokens_are_counted_once() {
        let sink = AuxUsageTotals::new();
        sink.record(100, 20, 30);
        sink.record(50, 5, 0);

        let first = sink.take();
        assert_eq!(
            first,
            AuxUsage {
                input: 150,
                output: 25,
                cache_read: 30,
                calls: 2,
            }
        );

        // 第二次抽干必须是空的——否则同一次往返会被主会话重复计入。
        assert!(sink.take().is_empty());
    }

    #[test]
    fn snapshot_does_not_drain() {
        let sink = AuxUsageTotals::new();
        sink.record(10, 1, 2);
        assert_eq!(sink.snapshot().calls, 1);
        assert_eq!(sink.snapshot().calls, 1);
        assert_eq!(sink.take().input, 10);
    }

    #[test]
    fn calls_are_counted_even_when_tokens_are_zero() {
        // 端点可能不回 usage；次数仍要记，否则 `/cost` 看不出「花了 N 次调用」。
        let sink = AuxUsageTotals::new();
        sink.record(0, 0, 0);
        assert_eq!(sink.take().calls, 1);
    }
}
