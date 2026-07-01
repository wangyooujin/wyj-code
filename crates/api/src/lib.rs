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
