//! 花费熔断 —— 把「成本闸」从轮数维度扩到金额与输出 token 维度。
//!
//! # 为什么需要
//!
//! 现有的成本闸全是**轮数**闸（`max_turns` / `forced_thinking_max_turns`）。
//! 但轮数与花费不是一回事：
//!
//! | 场景 | 轮数 | 花费 |
//! |---|---|---|
//! | Opus 带 200K 上下文跑 50 轮 | 50 | 极高 |
//! | Haiku 带 8K 上下文跑 50 轮 | 50 | 可忽略 |
//!
//! 用户看到的失控现场往往是「模型很聪明、每轮都在推进，但每轮都很贵」——
//! 轮数闸完全拦不住。金额闸拦得住。
//!
//! # 三条设计约束
//!
//! 1. **先收尾交付，再硬中止**：与轮数闸同一套机制。模型会收到一条
//!    「预算已用尽，立即总结」的指令；若它继续调工具，下一轮才真正中止。
//!    工作树里的成果仍能被交付，而不是被判成失败。
//! 2. **未知模型不熔断**：单价表是硬编码的，查不到就是「不知道价格」。
//!    在没有价格依据时截断用户的任务比放行更糟。
//! 3. **与 `/cost` 面板同一份价格表**（`wyj_core::pricing`）：否则会出现
//!    「面板显示花了 3 美元、熔断器以为只花了 0.2 美元」——熔断永远不触发，
//!    而用户以为它一直在保护自己。

use crate::pricing::{self, TokenCounts};

/// 预算闸的当前状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetState {
    /// 未设置任何闸（默认）。
    Disabled,
    /// 仍在预算内。
    Within,
    /// 某一维度已超限，应收尾交付。
    Exceeded(BudgetBreach),
}

/// 具体是哪个维度超了。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetBreach {
    /// 累计花费超过 `max_cost_per_turn_usd`。
    Cost { spent_usd: f64, limit_usd: f64 },
    /// 累计输出 token 超过 `max_output_tokens_per_turn`。
    OutputTokens { spent: u32, limit: u32 },
}

impl BudgetBreach {
    /// 人类可读的原因（用于收尾提示与错误信息）。
    pub fn describe(&self) -> String {
        match self {
            BudgetBreach::Cost {
                spent_usd,
                limit_usd,
            } => format!("累计花费 ${spent_usd:.4} 已超过本轮上限 ${limit_usd:.4}"),
            BudgetBreach::OutputTokens { spent, limit } => {
                format!("累计输出 {spent} tokens 已超过本轮上限 {limit} tokens")
            }
        }
    }
}

/// 预算闸配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetLimits {
    /// 单次用户提交内允许的最大花费（美元）。`0` = 不限制。
    pub max_cost_usd: f64,
    /// 单次用户提交内允许的最大输出 token。`0` = 不限制。
    pub max_output_tokens: u32,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            max_cost_usd: 0.0,
            max_output_tokens: 0,
        }
    }
}

impl BudgetLimits {
    /// 全部闸都关闭时返回 true（此时不必每轮算钱）。
    pub fn is_disabled(&self) -> bool {
        self.max_cost_usd <= 0.0 && self.max_output_tokens == 0
    }
}

/// 一次用户提交内的累计用量。
///
/// 与 `Session` 的累计字段**分开**：Session 记的是「整个会话」，
/// 而预算闸管的是「单次提交」——一次长会话里的十次提交各自独立计预算。
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnSpend {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

impl TurnSpend {
    /// 把一轮的实际用量累进来。
    pub fn add(&mut self, input: u32, output: u32, cache_read: u32, cache_write: u32) {
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(cache_read);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(cache_write);
    }

    /// 累计花费（美元）。`None` = 该模型查不到单价。
    pub fn cost_usd(&self, model: &str) -> Option<f64> {
        pricing::cost_for_model(
            model,
            TokenCounts {
                input: self.input_tokens,
                output: self.output_tokens,
                cache_read: self.cache_read_tokens,
                cache_write: self.cache_write_tokens,
            },
        )
    }
}

/// 判断当前花费是否已越闸。
///
/// 返回 `BudgetState::Exceeded` 时应当**收尾交付**（见 `core::agent` 的
/// `wrap_up` 路径），而不是直接报错——用户的工作树里已经有成果了。
///
/// 金额维度只在**能查到单价**时参与判定：查不到就是「不知道价格」，
/// 宁可放行也不能在没有价格依据时截断任务。
pub fn evaluate(spend: &TurnSpend, limits: &BudgetLimits, model: &str) -> BudgetState {
    if limits.is_disabled() {
        return BudgetState::Disabled;
    }
    // 输出 token 维度先判：它不依赖价格表，是确定性的。
    if limits.max_output_tokens > 0 && spend.output_tokens > limits.max_output_tokens {
        return BudgetState::Exceeded(BudgetBreach::OutputTokens {
            spent: spend.output_tokens,
            limit: limits.max_output_tokens,
        });
    }
    if limits.max_cost_usd > 0.0 {
        if let Some(spent) = spend.cost_usd(model) {
            if spent > limits.max_cost_usd {
                return BudgetState::Exceeded(BudgetBreach::Cost {
                    spent_usd: spent,
                    limit_usd: limits.max_cost_usd,
                });
            }
        }
    }
    BudgetState::Within
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spend(input: u32, output: u32) -> TurnSpend {
        TurnSpend {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn no_limits_means_no_gate() {
        let limits = BudgetLimits::default();
        assert!(limits.is_disabled());
        assert_eq!(
            evaluate(&spend(10_000_000, 10_000_000), &limits, "claude-opus-4"),
            BudgetState::Disabled
        );
    }

    /// 金额闸：1M 输入 token 的 Opus 是 15 美元，超过 1 美元的闸。
    #[test]
    fn cost_breach_is_detected() {
        let limits = BudgetLimits {
            max_cost_usd: 1.0,
            max_output_tokens: 0,
        };
        let state = evaluate(&spend(1_000_000, 0), &limits, "claude-opus-4");
        assert_eq!(
            state,
            BudgetState::Exceeded(BudgetBreach::Cost {
                spent_usd: 15.0,
                limit_usd: 1.0,
            })
        );
    }

    /// 未知模型不熔断：没有价格依据就不能截断用户的任务。
    #[test]
    fn an_unknown_model_never_breaches_the_cost_gate() {
        let limits = BudgetLimits {
            max_cost_usd: 0.01,
            max_output_tokens: 0,
        };
        assert_eq!(
            evaluate(&spend(100_000_000, 0), &limits, "mystery-model"),
            BudgetState::Within
        );
    }

    /// 输出 token 闸不依赖价格表，确定性更高。
    #[test]
    fn output_token_breach_works_without_a_price() {
        let limits = BudgetLimits {
            max_cost_usd: 0.0,
            max_output_tokens: 1000,
        };
        assert_eq!(
            evaluate(&spend(0, 1001), &limits, "mystery-model"),
            BudgetState::Exceeded(BudgetBreach::OutputTokens {
                spent: 1001,
                limit: 1000,
            })
        );
    }

    /// 恰好等于上限不算越闸（`>` 而非 `>=`）：用户把上限设成 5 美元，
    /// 花到正好 5 美元时中断他会是纯粹的意外。
    #[test]
    fn exactly_at_the_limit_is_not_a_breach() {
        let limits = BudgetLimits {
            max_cost_usd: 15.0,
            max_output_tokens: 0,
        };
        assert_eq!(
            evaluate(&spend(1_000_000, 0), &limits, "claude-opus-4"),
            BudgetState::Within
        );
    }

    /// 缓存命中按 0.1x 计费，所以同样的 token 数在有缓存时不该越闸。
    #[test]
    fn cache_hits_are_cheaper_than_fresh_input() {
        let limits = BudgetLimits {
            max_cost_usd: 2.0,
            max_output_tokens: 0,
        };
        let cached = TurnSpend {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 0,
        };
        // claude-opus-4：缓存读 0.1x × $15 = $1.5 < $2。
        assert_eq!(
            evaluate(&cached, &limits, "claude-opus-4"),
            BudgetState::Within
        );
        let fresh = spend(1_000_000, 0);
        assert!(matches!(
            evaluate(&fresh, &limits, "claude-opus-4"),
            BudgetState::Exceeded(_)
        ));
    }

    #[test]
    fn spend_accumulates_without_overflowing() {
        let mut s = TurnSpend::default();
        s.add(u32::MAX, 1, 0, 0);
        s.add(1, 1, 0, 0);
        assert_eq!(s.input_tokens, u32::MAX);
        assert_eq!(s.output_tokens, 2);
    }
}
