//! 模型身份与能力的中立表示。
//!
//! 这里不把“供应商”与“线协议”绑定。国内模型经常同时提供 Anthropic 或
//! OpenAI 兼容端点，能力解析必须以具体端点和模型为准。

use serde::{Deserialize, Serialize};

pub use wyj_config::WireProtocol;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub vendor: String,
    pub model: String,
    pub base_url: String,
    pub wire_protocol: WireProtocol,
}

impl ModelIdentity {
    pub fn sanitized(mut self) -> Self {
        self.base_url = sanitized_base_url(&self.base_url);
        self
    }
}

/// 能力 cache、doctor 与 telemetry 只需要端点身份，不需要 userinfo、query 或
/// fragment。兼容端点把 token 放在 URL 中时也不能让它进入持久化或终端输出。
pub fn sanitized_base_url(value: &str) -> String {
    let value = value.trim();
    if let Ok(mut url) = reqwest::Url::parse(value) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        return url.as_str().trim_end_matches('/').to_string();
    }
    value
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    UserOverride,
    LiveProbe,
    VerifiedCatalog,
    StaticCatalog,
    ProtocolDefault,
    ConservativeFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
    Verified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability<T> {
    pub value: T,
    pub source: CapabilitySource,
    pub confidence: Confidence,
}

impl<T> Capability<T> {
    pub fn new(value: T, source: CapabilitySource, confidence: Confidence) -> Self {
        Self {
            value,
            source,
            confidence,
        }
    }

    pub fn conservative(value: T) -> Self {
        Self::new(
            value,
            CapabilitySource::ConservativeFallback,
            Confidence::Low,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Unsupported,
    BudgetTokens,
    Effort,
    ProviderNative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Unsupported,
    ExplicitBreakpoints,
    Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDialect {
    ConciseEnglish,
    ConciseChinese,
    Bilingual,
    XmlStructured,
    MarkdownStructured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ModelQuirk {
    UnsupportedParameter(String),
    RequiresSingleTool,
    RequiresSimplifiedSchema,
    UsageOnlyInFinalChunk,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub vision: Capability<bool>,
    pub thinking: Capability<ThinkingMode>,
    pub interleaved_thinking: Capability<bool>,
    pub prompt_cache: Capability<PromptCacheMode>,
    pub stream_usage: Capability<bool>,
    pub tool_calling: Capability<bool>,
    pub parallel_tool_calls: Capability<bool>,
    pub tool_choice: Capability<bool>,
    pub strict_tool_schema: Capability<bool>,
    pub tool_result_images: Capability<bool>,
    pub structured_output: Capability<bool>,
    pub max_tools_per_turn: usize,
    pub preferred_prompt_dialect: PromptDialect,
    pub quirks: Vec<ModelQuirk>,
}

impl ModelCapabilities {
    /// 未命中目录、用户覆盖或探测结果时使用的 fail-closed 能力集。
    pub fn conservative(context_window: u32, max_output_tokens: u32) -> Self {
        Self {
            context_window,
            max_output_tokens,
            vision: Capability::conservative(false),
            thinking: Capability::conservative(ThinkingMode::Unsupported),
            interleaved_thinking: Capability::conservative(false),
            prompt_cache: Capability::conservative(PromptCacheMode::Unsupported),
            stream_usage: Capability::conservative(false),
            tool_calling: Capability::conservative(false),
            parallel_tool_calls: Capability::conservative(false),
            tool_choice: Capability::conservative(false),
            strict_tool_schema: Capability::conservative(false),
            tool_result_images: Capability::conservative(false),
            structured_output: Capability::conservative(false),
            max_tools_per_turn: 1,
            preferred_prompt_dialect: PromptDialect::ConciseEnglish,
            quirks: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_identity_removes_url_credentials_query_and_fragment() {
        let identity = ModelIdentity {
            vendor: "custom".to_string(),
            model: "model".to_string(),
            base_url: format!(
                "https://user:{}@example.com/v1?api_key={}#fragment",
                "P".repeat(20),
                "Q".repeat(20)
            ),
            wire_protocol: WireProtocol::OpenAiChatCompletions,
        }
        .sanitized();
        assert_eq!(identity.base_url, "https://example.com/v1");
    }
}
