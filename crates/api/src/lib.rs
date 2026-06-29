//! wyj-api — LLM 供应商客户端（Anthropic / OpenAI 双格式）

pub mod anthropic;
pub mod openai;
pub mod provider;
pub mod types;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAIProvider;
pub use provider::Provider;
pub use types::*;

use anyhow::Result;
use wyj_config::{Config, Provider as CfgProvider};

/// 根据配置构建对应的 Provider（动态派发）
pub fn build_provider(cfg: &Config) -> Result<Box<dyn Provider>> {
    match cfg.provider {
        CfgProvider::Anthropic => Ok(Box::new(AnthropicProvider::new(cfg)?)),
        CfgProvider::OpenAI => Ok(Box::new(OpenAIProvider::new(cfg)?)),
    }
}
