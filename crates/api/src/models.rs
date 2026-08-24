//! 分组（Profile）新建模板，以及"拉取模型列表"辅助函数。
//!
//! 模板里的 base_url/model 取自各供应商公开文档，仅作为新建分组时的预填起点——
//! 供应商随时可能调整接口地址或下线旧模型，用户创建后应自行核对。

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use wyj_config::{Provider, WireProtocol};

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
        note: "deepseek-reasoner 模型返回 reasoning_content 字段（v1.5.7+ 自动落盘为 thinking 块）；reasoner 多轮工具循环默认 max_turns=32；usage 仅在最后 message_delta chunk 出现",
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
    let client = Client::new();
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
            let url = format!("{base_url}/v1/models");
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
