//! wyj-api — LLM 供应商客户端（Anthropic / OpenAI 双格式）

pub mod anthropic;
pub mod models;
pub mod openai;
pub mod provider;
pub mod types;

pub use anthropic::AnthropicProvider;
pub use models::{fetch_model_ids, ProfileTemplate, PROFILE_TEMPLATES};
pub use openai::OpenAIProvider;
pub use provider::Provider;
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
    let cfg = Config {
        active_profile: profile.name.clone(),
        profiles: vec![profile.clone()],
        ..Config::default()
    };
    let model = model_override.unwrap_or(&profile.model).to_string();
    match profile.provider {
        CfgProvider::Anthropic => Ok(Arc::new(AnthropicProvider::with_model(&cfg, &model)?)),
        CfgProvider::OpenAI => Ok(Arc::new(OpenAIProvider::with_model(&cfg, &model)?)),
    }
}
