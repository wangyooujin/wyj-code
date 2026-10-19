//! 分组（Profile）新建模板，以及"拉取模型列表"辅助函数。
//!
//! 模板里的 base_url/model 取自各供应商公开文档，仅作为新建分组时的预填起点——
//! 供应商随时可能调整接口地址或下线旧模型，用户创建后应自行核对。

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use wyj_config::{Provider, WireProtocol};

/// 已知模型的容量组合（上下文窗口 / 单次输出上限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapacity {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl ModelCapacity {
    const fn new(context_window: u32, max_output_tokens: u32) -> Self {
        Self {
            context_window,
            max_output_tokens,
        }
    }
}

/// 按模型族名给出容量。**这是静态猜测表，不是探测结果。**
///
/// 为什么必须保守：`context_window` 直接决定压缩触发线（window -
/// max(max_tokens+8192, window/10)），填大了保护等于没跑、填小了每几轮就压缩。
/// 所以这里只收录**能确定下限**的常见族，且给出的值取保守侧的公开值；命中
/// 不到的模型返回 `None`，让面板保持用户已填的数字（默认 200k/32k）而不是
/// 猜一个可能错的数。
///
/// 注意：供应商的 `/models` 列表接口不返回任何容量元数据（OpenAI 只给 id/owned_by，
/// Anthropic 只给 id/display_name），所以「拉列表 → 自动填容量」只能靠这张表。
pub fn known_model_capacity(model: &str) -> Option<ModelCapacity> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    // 顺序有意义：先匹配更具体的前缀，避免 "gpt-4" 吃掉 "gpt-4o"。
    const TABLE: &[(&str, ModelCapacity)] = &[
        // ── Anthropic ──
        ("claude-opus-4", ModelCapacity::new(200_000, 32_000)),
        ("claude-sonnet-4", ModelCapacity::new(200_000, 32_000)),
        ("claude-3-7", ModelCapacity::new(200_000, 32_000)),
        ("claude-3-5", ModelCapacity::new(200_000, 8_192)),
        ("claude-3", ModelCapacity::new(200_000, 4_096)),
        // ── OpenAI ──
        ("gpt-4.1", ModelCapacity::new(1_047_576, 32_768)),
        ("gpt-4o", ModelCapacity::new(128_000, 16_384)),
        ("gpt-4-turbo", ModelCapacity::new(128_000, 4_096)),
        ("gpt-3.5", ModelCapacity::new(16_385, 4_096)),
        ("o1", ModelCapacity::new(200_000, 32_768)),
        ("o3", ModelCapacity::new(200_000, 32_768)),
        // ── 国内厂商 ──
        ("deepseek-chat", ModelCapacity::new(64_000, 8_192)),
        ("deepseek-reasoner", ModelCapacity::new(64_000, 8_192)),
        ("deepseek", ModelCapacity::new(64_000, 8_192)),
        ("qwen3", ModelCapacity::new(131_072, 8_192)),
        ("qwen2.5", ModelCapacity::new(131_072, 8_192)),
        ("qwen", ModelCapacity::new(131_072, 8_192)),
        ("glm-4.6", ModelCapacity::new(200_000, 16_384)),
        ("glm-4.5", ModelCapacity::new(131_072, 16_384)),
        ("glm-4", ModelCapacity::new(131_072, 4_096)),
        ("kimi-k2", ModelCapacity::new(131_072, 8_192)),
        ("kimi", ModelCapacity::new(131_072, 8_192)),
        ("minimax-m2", ModelCapacity::new(204_800, 32_000)),
        ("minimax-m3", ModelCapacity::new(204_800, 32_000)),
        ("minimax", ModelCapacity::new(204_800, 32_000)),
        ("doubao", ModelCapacity::new(131_072, 8_192)),
        // ── Google ──
        ("gemini-2.5", ModelCapacity::new(1_048_576, 65_536)),
        ("gemini-2", ModelCapacity::new(1_048_576, 8_192)),
        ("gemini", ModelCapacity::new(1_048_576, 8_192)),
    ];
    TABLE
        .iter()
        .find(|(prefix, _)| m.starts_with(prefix) || m.contains(prefix))
        .map(|(_, cap)| *cap)
}

/// 新建分组时可选的内置模板。
pub struct ProfileTemplate {
    /// 模板标识（同时作为新建分组的默认名称）
    pub key: &'static str,
    /// 展示名称
    pub label: &'static str,
    pub provider: Provider,
    pub vendor: &'static str,
    pub wire_protocol: WireProtocol,
    pub base_url: &'static str,
    pub example_model: &'static str,
    /// 供应商特殊说明，展示在模板选择器里
    pub note: &'static str,
    pub vision: bool,
    pub prompt_cache: Option<bool>,
    pub openai_stream_options: Option<bool>,
}

pub const PROFILE_TEMPLATES: &[ProfileTemplate] = &[
    ProfileTemplate {
        key: "glm",
        label: "GLM（智谱/Z.ai Coding Plan）",
        provider: Provider::Anthropic,
        vendor: "zhipu",
        wire_protocol: WireProtocol::AnthropicMessages,
        base_url: "https://open.bigmodel.cn/api/anthropic",
        example_model: "glm-5.2",
        note: "prompt_cache 强制 false（第三方 Anthropic 兼容端点）；interleaved_thinking 不支持；usage 来自 message_delta",
        vision: false,
        prompt_cache: Some(false),
        openai_stream_options: None,
    },
    ProfileTemplate {
        key: "volcengine",
        label: "火山引擎（Ark Agent 计划）",
        provider: Provider::OpenAI,
        vendor: "volcengine",
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
        example_model: "doubao-seed-1-6",
        note: "模型名通常是控制台创建的推理接入点 ID（形如 ep-xxxxxxxx-xxxxx），非固定模型名；openai_stream_options=false（Ark 不支持 stream_options.include_usage）",
        vision: false,
        prompt_cache: None,
        openai_stream_options: Some(false),
    },
    ProfileTemplate {
        key: "minimax",
        label: "MiniMax（Coding Plan）",
        provider: Provider::OpenAI,
        vendor: "minimax",
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        base_url: "https://api.minimaxi.com/v1",
        example_model: "MiniMax-M2",
        note: "openai_stream_options=true（依赖供应商返回精确 usage,usage 仅在最后 message_delta chunk 出现）",
        vision: false,
        prompt_cache: None,
        openai_stream_options: Some(true),
    },
    ProfileTemplate {
        key: "kimi",
        label: "Kimi（Moonshot Coding Plan）",
        provider: Provider::Anthropic,
        vendor: "moonshot",
        wire_protocol: WireProtocol::AnthropicMessages,
        base_url: "https://api.moonshot.cn/anthropic",
        example_model: "kimi-k2-turbo-preview",
        note: "prompt_cache 强制 false（第三方 Anthropic 兼容端点）；interleaved_thinking 不支持",
        vision: false,
        prompt_cache: Some(false),
        openai_stream_options: None,
    },
    ProfileTemplate {
        key: "deepseek",
        label: "DeepSeek（官方计费 API）",
        provider: Provider::OpenAI,
        vendor: "deepseek",
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        base_url: "https://api.deepseek.com",
        example_model: "deepseek-chat",
        note: "deepseek-reasoner 模型返回 reasoning_content 字段（v0.2.16+ 自动落盘为 thinking 块）；reasoner 多轮工具循环默认 max_turns=32；usage 仅在最后 message_delta chunk 出现",
        vision: false,
        prompt_cache: None,
        openai_stream_options: Some(true),
    },
    ProfileTemplate {
        key: "qwen-bailian",
        label: "通义千问（阿里云百炼）",
        provider: Provider::OpenAI,
        vendor: "alibaba",
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        example_model: "qwen3-coder-plus",
        note: "静态兼容模板；请以百炼控制台当前可用模型为准；Qwen3-Coder 系列不支持 extended thinking",
        vision: false,
        prompt_cache: None,
        openai_stream_options: Some(true),
    },
    ProfileTemplate {
        key: "ollama",
        label: "Ollama（实验性 OpenAI 兼容）",
        provider: Provider::OpenAI,
        vendor: "ollama",
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        base_url: "http://127.0.0.1:11434/v1",
        example_model: "qwen3-coder",
        note: "能力取决于本地模型；创建后请运行 model doctor probe；openai_stream_options=false（Ollama 不支持 stream_options.include_usage）",
        vision: false,
        prompt_cache: None,
        openai_stream_options: Some(false),
    },
    ProfileTemplate {
        key: "vllm",
        label: "vLLM（实验性 OpenAI 兼容）",
        provider: Provider::OpenAI,
        vendor: "vllm",
        wire_protocol: WireProtocol::OpenAiChatCompletions,
        base_url: "http://127.0.0.1:8000/v1",
        example_model: "",
        note: "能力取决于 served model 和启动参数；创建后请运行 model doctor probe；openai_stream_options=false（vLLM 不支持 stream_options.include_usage）",
        vision: false,
        prompt_cache: None,
        openai_stream_options: Some(false),
    },
    ProfileTemplate {
        key: "custom",
        label: "自定义",
        provider: Provider::Anthropic,
        vendor: "custom",
        wire_protocol: WireProtocol::AnthropicMessages,
        base_url: "",
        example_model: "",
        note: "完全开放,用户自配;建议 prompt_cache/openai_stream_options/vision 与 base_url 协议匹配;非官方 Anthropic 端点 prompt_cache 应显式 false",
        vision: true,
        prompt_cache: None,
        openai_stream_options: None,
    },
];

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModelEntry>,
}

#[derive(Deserialize)]
struct AnthropicModelEntry {
    id: String,
}

/// 尽力而为地拉取供应商可用模型 ID 列表。
///
/// 第三方代理不一定实现模型列表接口，失败时返回 Err，调用方应展示内联错误
/// 并允许用户继续手填模型名，而不是阻塞其他操作。
pub async fn fetch_model_ids(
    provider: &Provider,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>> {
    let base_url = base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return Err(anyhow!("base_url 为空，无法拉取模型列表"));
    }
    let client = crate::stream_guard::build_client()?;
    let ids: Vec<String> = match provider {
        Provider::OpenAI => {
            let url = format!("{base_url}/models");
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .send()
                .await
                .with_context(|| format!("请求 {url} 失败"))?
                .error_for_status()
                .with_context(|| format!("{url} 返回错误状态"))?;
            let parsed: OpenAiModelsResponse = resp.json().await.context("解析模型列表响应失败")?;
            parsed.data.into_iter().map(|e| e.id).collect()
        }
        Provider::Anthropic => {
            // base_url 常被用户按 OpenAI 习惯填成 `.../v1`（本文件的 vllm 模板就是），
            // 再拼 `/v1/models` 会得到 `/v1/v1/models`。只补一次版本段。
            let url = if base_url.ends_with("/v1") {
                format!("{base_url}/models")
            } else {
                format!("{base_url}/v1/models")
            };
            let resp = client
                .get(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .with_context(|| format!("请求 {url} 失败"))?
                .error_for_status()
                .with_context(|| format!("{url} 返回错误状态"))?;
            let parsed: AnthropicModelsResponse =
                resp.json().await.context("解析模型列表响应失败")?;
            parsed.data.into_iter().map(|e| e.id).collect()
        }
    };
    if ids.is_empty() {
        return Err(anyhow!("模型列表为空"));
    }
    Ok(ids)
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    #[test]
    fn known_families_resolve_to_sane_capacities() {
        let claude = known_model_capacity("claude-sonnet-4-5").expect("claude 应命中");
        assert_eq!(claude.context_window, 200_000);
        assert_eq!(claude.max_output_tokens, 32_000);

        let gpt4o = known_model_capacity("gpt-4o-mini").expect("gpt-4o 应命中");
        assert_eq!(gpt4o.context_window, 128_000);

        let ds = known_model_capacity("deepseek-reasoner").expect("deepseek 应命中");
        assert_eq!(ds.context_window, 64_000);
    }

    /// 更具体的前缀必须先命中：`gpt-4o` 不能被 `gpt-4-turbo` 那条吃掉，
    /// `deepseek-reasoner` 也不该和 `deepseek-chat` 互相串。
    #[test]
    fn specific_prefixes_win_over_generic_ones() {
        let a = known_model_capacity("gpt-4o").unwrap();
        let b = known_model_capacity("gpt-4-turbo").unwrap();
        assert_ne!(a, b, "gpt-4o 与 gpt-4-turbo 必须区分开: {a:?} vs {b:?}");
        assert_eq!(a.max_output_tokens, 16_384);
        assert_eq!(b.max_output_tokens, 4_096);
    }

    /// 未知模型必须返回 None 而不是猜一个数——`context_window` 决定压缩触发线，
    /// 猜大了保护等于没跑，猜小了每几轮就压缩。
    #[test]
    fn unknown_models_are_left_alone() {
        assert!(known_model_capacity("totally-unknown-model").is_none());
        assert!(known_model_capacity("ep-20250101-abcdef").is_none());
        assert!(known_model_capacity("   ").is_none());
    }

    /// 大小写与空白不该影响命中。
    #[test]
    fn matching_is_case_and_whitespace_insensitive() {
        assert_eq!(
            known_model_capacity("  Claude-Opus-4  "),
            known_model_capacity("claude-opus-4")
        );
    }
}
