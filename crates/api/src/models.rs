//! 分组（Profile）新建模板，以及"拉取模型列表"辅助函数。
//!
//! 模板里的 base_url/model 取自各供应商公开文档，仅作为新建分组时的预填起点——
//! 供应商随时可能调整接口地址或下线旧模型，用户创建后应自行核对。

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use wyj_config::Provider;

/// 新建分组时可选的内置模板。
pub struct ProfileTemplate {
    /// 模板标识（同时作为新建分组的默认名称）
    pub key: &'static str,
    /// 展示名称
    pub label: &'static str,
    pub provider: Provider,
    pub base_url: &'static str,
    pub example_model: &'static str,
    /// 供应商特殊说明，展示在模板选择器里
    pub note: &'static str,
}

pub const PROFILE_TEMPLATES: &[ProfileTemplate] = &[
    ProfileTemplate {
        key: "glm",
        label: "GLM（智谱/Z.ai Coding Plan）",
        provider: Provider::Anthropic,
        base_url: "https://open.bigmodel.cn/api/anthropic",
        example_model: "glm-4.6",
        note: "",
    },
    ProfileTemplate {
        key: "volcengine",
        label: "火山引擎（Ark Agent 计划）",
        provider: Provider::OpenAI,
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
        example_model: "doubao-seed-1-6",
        note: "模型名通常是控制台创建的推理接入点 ID（形如 ep-xxxxxxxx-xxxxx），非固定模型名",
    },
    ProfileTemplate {
        key: "minimax",
        label: "MiniMax（Coding Plan）",
        provider: Provider::OpenAI,
        base_url: "https://api.minimaxi.com/v1",
        example_model: "MiniMax-M2",
        note: "",
    },
    ProfileTemplate {
        key: "kimi",
        label: "Kimi（Moonshot Coding Plan）",
        provider: Provider::Anthropic,
        base_url: "https://api.moonshot.cn/anthropic",
        example_model: "kimi-k2-turbo-preview",
        note: "",
    },
    ProfileTemplate {
        key: "deepseek",
        label: "DeepSeek（官方计费 API）",
        provider: Provider::OpenAI,
        base_url: "https://api.deepseek.com",
        example_model: "deepseek-chat",
        note:
            "deepseek-chat/deepseek-reasoner 后续会作为新模型的兼容别名，供应商公告后可能需要改名",
    },
    ProfileTemplate {
        key: "custom",
        label: "自定义",
        provider: Provider::Anthropic,
        base_url: "",
        example_model: "",
        note: "",
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
