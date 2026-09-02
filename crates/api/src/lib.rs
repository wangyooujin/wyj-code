//! wyj-api — LLM 供应商客户端（Anthropic / OpenAI 双格式）

pub mod anthropic;
pub mod capabilities;
pub mod capability_cache;
pub mod doctor;
pub mod error;
pub mod model_catalog;
pub mod models;
pub mod openai;
pub mod prompt_policy;
pub mod provider;
pub mod request_plan;
pub mod retry;
pub mod thinking;
pub mod types;

pub use anthropic::AnthropicProvider;
pub use capabilities::*;
pub use capability_cache::{CapabilityCache, CapabilityCacheRecord, PROBE_VERSION};
pub use doctor::ModelDoctorReport;
pub use error::{ProviderError, ProviderErrorKind};
pub use model_catalog::{CatalogResolution, ModelCatalog, VerificationStatus};
pub use models::{fetch_model_ids, ProfileTemplate, PROFILE_TEMPLATES};
pub use openai::OpenAIProvider;
pub use prompt_policy::PromptPolicy;
pub use provider::Provider;
pub use request_plan::*;
pub use thinking::{
    adapter_for, apply_thinking_to_openai_body, should_emit_interleaved_beta, ReasoningEffort,
    ThinkingAdapter, ThinkingControl, ThinkingResponseFields, ThinkingSpec,
};
pub use types::*;

use anyhow::Result;
use std::sync::Arc;
use wyj_config::{Config, Provider as CfgProvider};

/// 根据配置构建对应的 Provider（Arc 包装，可跨线程共享）
pub fn build_provider(cfg: &Config) -> Result<Arc<dyn Provider>> {
    build_provider_with_model(cfg, &cfg.active_profile().model.clone())
}

/// 以指定模型构建 Provider（用于 per-mode 模型覆盖）
pub fn build_provider_with_model(cfg: &Config, model: &str) -> Result<Arc<dyn Provider>> {
    match cfg.provider() {
        CfgProvider::Anthropic => Ok(Arc::new(AnthropicProvider::with_model(cfg, model)?)),
        CfgProvider::OpenAI => Ok(Arc::new(OpenAIProvider::with_model(cfg, model)?)),
    }
}

/// 以指定分组构建 Provider（子 Agent 按 Profile 覆盖供应商/端点/Key 时使用）。
/// 通过构造一个只含该分组的临时 Config 复用现有构造器，避免改动 Provider 内部。
pub fn build_provider_from_profile(
    profile: &wyj_config::Profile,
    model_override: Option<&str>,
) -> Result<Arc<dyn Provider>> {
    let runtime_api_key = profile
        .api_key_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|key| !key.is_empty());
    let cfg = Config {
        active_profile: profile.name.clone(),
        profiles: vec![profile.clone()],
        runtime_api_key,
        ..Config::default()
    };
    let model = model_override.unwrap_or(&profile.model).to_string();
    match profile.provider {
        CfgProvider::Anthropic => Ok(Arc::new(AnthropicProvider::with_model(&cfg, &model)?)),
        CfgProvider::OpenAI => Ok(Arc::new(OpenAIProvider::with_model(&cfg, &model)?)),
    }
}

/// 首次启动 + `~/.wyj-code` 缺失 + 用户尚未填入 API Key 时,作为 Agent
/// 的占位 Provider。**所有方法都返回 `ProviderErrorKind::MissingApiKey`**,
/// 真正的请求路径上 `rebuild_fn` 会在用户填写 ProfileDialog 之前替换掉它,
/// 所以本占位永不被实际触发——TUI 通过 ProfileDialog 浮层拦截用户输入,
/// 而 `agent.run_turn` 在浮层打开期间不会被调用。
///
/// 暴露在 crate 根以便 `wyj-cli` 装配 Agent 时直接引用。
pub struct MissingKeyProvider;

const MISSING_API_KEY_MESSAGE: &str =
    "API key not configured; please open /model to set one before chatting.";

#[async_trait::async_trait]
impl Provider for MissingKeyProvider {
    async fn stream(
        &self,
        _system: &str,
        _messages: &[crate::types::Message],
        _tools: &[crate::types::ToolDefinition],
        _opts: &crate::provider::RequestOptions,
    ) -> Result<crate::provider::EventStream> {
        use futures::stream;
        let err = ProviderError::new(ProviderErrorKind::MissingApiKey, MISSING_API_KEY_MESSAGE);
        let s = stream::once(async move { Err(anyhow::Error::new(err)) });
        Ok(Box::pin(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RequestOptions;
    use futures::StreamExt;

    #[tokio::test]
    async fn missing_key_provider_stream_emits_missing_api_key_error() {
        let provider = MissingKeyProvider;
        let opts = RequestOptions::text_only(32);
        let mut stream = provider
            .stream("", &[], &[], &opts)
            .await
            .expect("stream 自身应 Ok(stream of Err)");
        let first = stream.next().await.expect("占位流应恰好发出一个 Err 项");
        let err = first.expect_err("占位流的唯一项应为 Err");
        let provider_err = err
            .downcast_ref::<ProviderError>()
            .expect("Err 应为 ProviderError");
        assert_eq!(provider_err.kind, ProviderErrorKind::MissingApiKey);
        assert!(!provider_err.retryable, "MissingApiKey 不应被自动重试");
        // 流不应再有第二个事件,防止调用方误以为可继续接收。
        assert!(stream.next().await.is_none());
    }
}
