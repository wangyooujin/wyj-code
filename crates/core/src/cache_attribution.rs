//! 缓存命中率归因 —— 回答「缓存为什么掉了」。
//!
//! # 解决什么问题
//!
//! `/cost` 能显示「缓存命中率从 82% 掉到 0%」，但**回答不了为什么**。
//! 而「为什么」恰恰是可行动的：
//!
//! - prompt 的 `main` 段变了 → 版本升级或 locale 切换，**重启也无解**；
//! - prompt 的 `tools` 段变了 → 装了/删了插件或 MCP server，**关掉它就好**；
//! - 两者都没变 → 是端点侧的事（缓存 TTL 过期、端点不缓存），
//!   **再改本地配置也没用**。
//!
//! 不做归因时，用户只能凭猜测试：「是不是该重启」「是不是该删配置」——
//! 而这两种猜测的方向经常相反。
//!
//! # 判据
//!
//! 缓存前缀是 `tools → system → messages`。所以**只有 `main` 与 `tools`
//! 两段的变化会让缓存失效**（`memory` / `project_rules` / `volatile`
//! 本来就每轮变，报它们是噪音——这与 `prompt_fingerprint` 的
//! `COMPARABLE_SECTIONS` 是同一个理由）。
//!
//! 归因采用**相邻两次请求**的对比，而不是全局统计：缓存是「前缀命中」
//! 机制，一次失效只影响它之后的那几趟。

use crate::prompt_fingerprint::{PromptFingerprint, SECTION_MAIN, SECTION_TOOLS};

/// 一次请求的缓存观测。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheSample {
    /// 本次请求的序号（从 1 开始）。
    #[serde(default)]
    pub request_index: u32,
    /// `main` 段的 digest。
    #[serde(default)]
    pub main_digest: String,
    /// `tools` 段的 digest。
    #[serde(default)]
    pub tools_digest: String,
    /// 本次命中的缓存 token 数。
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// 本次未命中缓存的输入 token 数。
    #[serde(default)]
    pub input_tokens: u32,
}

impl CacheSample {
    /// 本次请求的缓存命中率（0.0 ~ 1.0）。
    ///
    /// 分母是「输入侧总量」= 未命中 + 命中。**不含输出**：输出 token 与
    /// 缓存前缀无关，混进来会让命中率的解释变模糊。
    pub fn hit_ratio(&self) -> f32 {
        let total = self.input_tokens + self.cache_read_tokens;
        if total == 0 {
            return 0.0;
        }
        self.cache_read_tokens as f32 / total as f32
    }
}

/// 缓存掉量的归因结论。
#[derive(Debug, Clone, PartialEq)]
pub enum CacheCause {
    /// `tools` 段变了 —— 工具集合变了（插件 / MCP / lazy 折叠）。
    ToolsChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// `main` 段变了 —— 主提示词变了（版本升级 / locale 切换）。
    MainChanged,
    /// 两段都没变 —— 归因于端点侧（缓存 TTL 过期、端点不缓存、
    /// 上一次请求本身就没产生缓存）。
    EndpointSide,
    /// 样本不足，无法归因（首次请求必然没有缓存命中）。
    NotEnoughSamples,
}

impl CacheCause {
    /// 人类可读的原因。
    pub fn describe(&self) -> String {
        match self {
            CacheCause::ToolsChanged { added, removed } => {
                let mut parts = Vec::new();
                if !added.is_empty() {
                    parts.push(format!("新增 {}", added.join(", ")));
                }
                if !removed.is_empty() {
                    parts.push(format!("移除 {}", removed.join(", ")));
                }
                if parts.is_empty() {
                    "工具集合变了（同名工具的描述或 schema 改了）".to_string()
                } else {
                    format!("工具集合变了（{}）", parts.join("；"))
                }
            }
            CacheCause::MainChanged => "主提示词变了（版本升级或语言切换）".to_string(),
            CacheCause::EndpointSide => {
                "prompt 的可比段没变 —— 归因于端点侧（缓存 TTL 过期或端点不缓存）".to_string()
            }
            CacheCause::NotEnoughSamples => "样本不足，无法归因".to_string(),
        }
    }
}

/// 归因结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CacheAttribution {
    /// 掉量发生的请求序号。
    pub request_index: u32,
    /// 掉量前的命中率。
    pub previous_hit_ratio: f32,
    /// 掉量后的命中率。
    pub current_hit_ratio: f32,
    pub cause: CacheCause,
}

/// 判定「缓存是否掉了」。
///
/// ## 「掉了」的判据
///
/// 用**绝对量**而不是相对比例：命中率从 80% 掉到 70% 在绝对量上仍是
/// 「大量命中」，把它报成失效会让面板上出现一堆假警报。而从「大量命中」
/// 掉到「几乎没有命中」是真正的信号——用户会立刻感觉到成本变了。
///
/// 门槛：`previous.cache_read >= MIN_CACHE_TOKENS_FOR_SIGNAL` 且
/// `current.cache_read` 显著低于它。取 1000 token 是为了滤掉「最后一小段
/// 恰好没命中」这种噪声。
pub const MIN_CACHE_TOKENS_FOR_SIGNAL: u32 = 1000;

/// 在相邻样本之间归因缓存掉量。
///
/// 返回 `None` 表示「没掉」或「样本不足」——不是每次请求都该有一条归因。
pub fn attribute(
    samples: &[CacheSample],
    fingerprints: &[Option<PromptFingerprint>],
) -> Option<CacheAttribution> {
    if samples.len() < 2 {
        return None;
    }
    let previous = &samples[samples.len() - 2];
    let current = &samples[samples.len() - 1];

    // 必须真的掉了一档：从「大量命中」掉到「几乎没有」。
    if previous.cache_read_tokens < MIN_CACHE_TOKENS_FOR_SIGNAL {
        return None;
    }
    if current.cache_read_tokens >= previous.cache_read_tokens / 2 {
        return None;
    }

    let cause = classify(previous, current, fingerprints);
    Some(CacheAttribution {
        request_index: current.request_index,
        previous_hit_ratio: previous.hit_ratio(),
        current_hit_ratio: current.hit_ratio(),
        cause,
    })
}

/// 判定掉量的具体原因。
fn classify(
    previous: &CacheSample,
    current: &CacheSample,
    fingerprints: &[Option<PromptFingerprint>],
) -> CacheCause {
    // 取最近两次带指纹的样本（不假设与 samples 一一对应：指纹只在
    // 进程内第一次发请求时采集，样本却是每趟都有）。
    let with_fp: Vec<&PromptFingerprint> = fingerprints.iter().filter_map(|f| f.as_ref()).collect();
    if with_fp.len() < 2 {
        return CacheCause::EndpointSide;
    }
    let last = with_fp[with_fp.len() - 1];
    let prev = with_fp[with_fp.len() - 2];

    // tools 段先判：它的变化更常见（插件 / MCP 变动），且后果更可行动。
    let tools_changed = prev
        .section(SECTION_TOOLS)
        .zip(last.section(SECTION_TOOLS))
        .map(|(a, b)| a.digest != b.digest)
        .unwrap_or(false);
    if tools_changed {
        let (added, removed) = match (prev.section(SECTION_TOOLS), last.section(SECTION_TOOLS)) {
            (Some(a), Some(b)) => (
                b.tool_names
                    .iter()
                    .filter(|n| !a.tool_names.contains(n))
                    .cloned()
                    .collect(),
                a.tool_names
                    .iter()
                    .filter(|n| !b.tool_names.contains(n))
                    .cloned()
                    .collect(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        return CacheCause::ToolsChanged { added, removed };
    }

    let main_changed = prev
        .section(SECTION_MAIN)
        .zip(last.section(SECTION_MAIN))
        .map(|(a, b)| a.digest != b.digest)
        .unwrap_or(false);
    if main_changed {
        return CacheCause::MainChanged;
    }

    // 指纹的 digest 也没变，但样本里记录到的 digest 变了 —— 说明指纹采集
    // 与样本采集不在同一时刻，两者不一致。以样本为准（它更接近实际发送）。
    if previous.tools_digest != current.tools_digest {
        return CacheCause::ToolsChanged {
            added: Vec::new(),
            removed: Vec::new(),
        };
    }
    if previous.main_digest != current.main_digest {
        return CacheCause::MainChanged;
    }

    CacheCause::EndpointSide
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_fingerprint::PromptFingerprint;
    use wyj_api::ToolDefinition;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: "d".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }

    fn fingerprint(tools: &[ToolDefinition]) -> PromptFingerprint {
        PromptFingerprint::capture("main", "", "", "", tools)
    }

    fn sample(index: u32, cache_read: u32, input: u32) -> CacheSample {
        CacheSample {
            request_index: index,
            main_digest: String::new(),
            tools_digest: String::new(),
            cache_read_tokens: cache_read,
            input_tokens: input,
        }
    }

    #[test]
    fn a_single_sample_cannot_be_attributed() {
        assert!(attribute(&[sample(1, 0, 1000)], &[]).is_none());
    }

    /// 缓存命中大幅下滑且 `tools` 变了 → 归因到工具集合。
    #[test]
    fn a_tools_change_is_attributed() {
        let samples = vec![sample(1, 50_000, 1_000), sample(2, 100, 50_000)];
        let fps = vec![
            Some(fingerprint(&[tool("Read")])),
            Some(fingerprint(&[tool("Read"), tool("mcp__x__y")])),
        ];
        let attribution = attribute(&samples, &fps).expect("应能归因");
        match attribution.cause {
            CacheCause::ToolsChanged { added, .. } => {
                assert_eq!(added, vec!["mcp__x__y".to_string()]);
            }
            other => panic!("应归因到工具变化，实际 {other:?}"),
        }
    }

    /// `main` 变了（版本升级）→ 归因到主提示词。
    #[test]
    fn a_main_change_is_attributed() {
        let samples = vec![sample(1, 50_000, 1_000), sample(2, 100, 50_000)];
        let fps = vec![
            Some(fingerprint(&[tool("Read")])),
            Some(fingerprint(&[tool("Read")])),
        ];
        let mut fps = fps;
        // 把 main 段换成不同的内容
        let mut second = fps[1].clone().unwrap();
        second.sections[0].digest = "different".to_string();
        fps[1] = Some(second);
        let attribution = attribute(&samples, &fps).expect("应能归因");
        assert_eq!(attribution.cause, CacheCause::MainChanged);
    }

    /// 两段都没变 → 端点侧（TTL 过期 / 端点不缓存）。
    #[test]
    fn an_unchanged_prompt_is_attributed_to_the_endpoint() {
        let samples = vec![sample(1, 50_000, 1_000), sample(2, 100, 50_000)];
        let fps = vec![
            Some(fingerprint(&[tool("Read")])),
            Some(fingerprint(&[tool("Read")])),
        ];
        let attribution = attribute(&samples, &fps).expect("应能归因");
        assert_eq!(attribution.cause, CacheCause::EndpointSide);
    }

    /// 从「几乎没命中」到「还是没命中」不算掉量 —— 否则每轮都会报一次。
    #[test]
    fn a_low_cache_baseline_is_not_a_drop() {
        let samples = vec![sample(1, 100, 50_000), sample(2, 50, 50_000)];
        assert!(attribute(&samples, &[]).is_none());
    }

    /// 命中率只小幅下滑不算掉量。
    #[test]
    fn a_small_relative_drop_is_not_a_drop() {
        let samples = vec![sample(1, 50_000, 1_000), sample(2, 30_000, 1_000)];
        assert!(attribute(&samples, &[]).is_none());
    }

    #[test]
    fn the_hit_ratio_ignores_output_tokens() {
        let s = CacheSample {
            request_index: 1,
            main_digest: String::new(),
            tools_digest: String::new(),
            cache_read_tokens: 3_000,
            input_tokens: 1_000,
        };
        assert!((s.hit_ratio() - 0.75).abs() < 1e-6);
    }
}
