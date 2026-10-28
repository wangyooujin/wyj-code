//! 模型单价表与费用计算。
//!
//! # 为什么住在 core 而不是 commands
//!
//! `/cost` 面板在 `crates/commands`，而**花费熔断**必须在 `crates/core`
//! （它要在每次模型往返前判断「这一趟还该不该发」）。若两份价格表各写一份，
//! 就会出现「面板显示花了 3 美元、熔断器以为只花了 0.2 美元」这种
//! **用户无法察觉的不一致**——熔断永远不会触发，而用户以为它一直在保护自己。
//!
//! 所以价格表下沉到 core，`commands` 改为引用本模块：一份数据、两处消费。

/// 单价条目：`(模型名前缀, 输入价/百万token, 输出价/百万token, 缓存读倍率)`。
///
/// **匹配是子串匹配（`contains`），因此顺序即优先级**：更精确的前缀必须排在
/// 更宽的前缀之前，否则 `MiniMax-M3.1-Flash-Preview` 会先命中 `minimax-m3`
/// 而被算成 M3 的价（贵一倍）。
///
/// 硬编码单价，随官方调价可能过时——查不到单价时**必须显式提示**而不是
/// 静默降级成「只有 token 数」：用户看到没有金额时无法区分「真的免费」
/// 和「我没查到」。
pub static PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-opus-4", 15.0, 75.0, 0.1),
    ("claude-opus-3", 15.0, 75.0, 0.1),
    ("claude-sonnet-4", 3.0, 15.0, 0.1),
    ("claude-sonnet-3", 3.0, 15.0, 0.1),
    ("claude-haiku-4", 0.8, 4.0, 0.1),
    ("claude-haiku-3", 0.25, 1.25, 0.1),
    // MiniMax 走 Anthropic 兼容端点，单价远低于 Anthropic 同档。
    //
    // 官方价（人民币/百万 tokens，2026-10 核对，按 7.0 折美元）：
    //   M3.1-Flash  输入 1.00 / 输出 4.00 / 缓存读 0.10（= 输入价的 0.1x）
    //   M3          输入 2.10 / 输出 8.40 / 缓存读 0.42（= 输入价的 0.2x，>512k 输入翻倍）
    // M3.1-Flash 的条目**必须排在 `minimax-m3` 之前**（子串匹配，顺序即优先级）。
    // M3 用的是低档价（≤512k 输入）；超长输入按 2 倍计费，本表不区分。
    //
    // 放在列表里是因为第三方端点默认**不**开启缓存
    // （见 `Profile::effective_prompt_cache`），未显式配置时全额按 input 计。
    ("minimax-m3.1-flash", 0.15, 0.60, 0.1),
    ("minimax-m3", 0.30, 1.20, 0.2),
    ("minimax-m2", 0.30, 1.20, 0.1),
];

/// 查到模型单价：`(input_per_mtok, output_per_mtok, cache_read_ratio)`。
///
/// 缓存写入按输入价的 1.25x 计（Anthropic 的写缓存倍率），本函数不返回它——
/// 调用方需要时用 [`write_cache_multiplier`]。
pub fn lookup_price(model: &str) -> Option<(f64, f64, f64)> {
    let model_lower = model.to_lowercase();
    PRICES
        .iter()
        .find(|(prefix, _, _, _)| model_lower.contains(prefix))
        .map(|(_, i, o, cr)| (*i, *o, *cr))
}

/// 缓存写入相对输入价的倍率。
pub const WRITE_CACHE_MULTIPLIER: f64 = 1.25;

/// 一组 token 计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

/// 按单价表算一组 token 的费用（美元）。
///
/// `None` = 该模型没有单价条目。此时调用方**必须显式提示**用户「未查到单价」，
/// 不能显示 0——「免费」与「不知道」是两件事。
pub fn cost_of(counts: TokenCounts, price: Option<(f64, f64, f64)>) -> Option<f64> {
    let (input_price, output_price, cache_read_ratio) = price?;
    let per_mtok = 1_000_000.0;
    Some(
        counts.input as f64 / per_mtok * input_price
            + counts.output as f64 / per_mtok * output_price
            + counts.cache_read as f64 / per_mtok * input_price * cache_read_ratio
            + counts.cache_write as f64 / per_mtok * input_price * WRITE_CACHE_MULTIPLIER,
    )
}

/// 按模型名直接算费用。
pub fn cost_for_model(model: &str, counts: TokenCounts) -> Option<f64> {
    cost_of(counts, lookup_price(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precise_prefix_wins_over_the_wider_one() {
        // M3.1-Flash 必须按 M3.1 的价（0.15/0.60），不能落到 M3（0.30/1.20）。
        let price = lookup_price("minimax-m3.1-flash").expect("应有单价");
        assert!((price.0 - 0.15).abs() < f64::EPSILON);
        assert!((price.1 - 0.60).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert!(lookup_price("some-unknown-model-xyz").is_none());
    }

    #[test]
    fn cache_read_bills_at_the_configured_ratio() {
        let counts = TokenCounts {
            cache_read: 1_000_000,
            ..Default::default()
        };
        // claude-sonnet-4：输入 3.0，缓存读 0.1x → 0.3 美元。
        let cost = cost_for_model("claude-sonnet-4", counts).expect("有单价");
        assert!((cost - 0.3).abs() < 1e-9, "实际 {cost}");
    }

    #[test]
    fn cache_write_bills_at_1_25x_the_input_price() {
        let counts = TokenCounts {
            cache_write: 1_000_000,
            ..Default::default()
        };
        let cost = cost_for_model("claude-sonnet-4", counts).expect("有单价");
        assert!((cost - 3.75).abs() < 1e-9, "实际 {cost}");
    }

    #[test]
    fn an_unknown_model_yields_none_not_zero() {
        let counts = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(cost_for_model("mystery-model", counts), None);
    }
}
