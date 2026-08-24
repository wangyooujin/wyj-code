use serde::{Deserialize, Serialize};
use wyj_config::Profile;

use crate::{
    CapabilityCache, CatalogResolution, ModelCapabilities, ModelCatalog, ModelIdentity,
    VerificationStatus,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDoctorReport {
    pub profile: String,
    pub identity: ModelIdentity,
    pub endpoint_type: String,
    pub verification_status: VerificationStatus,
    pub capabilities: ModelCapabilities,
    pub known_degradations: Vec<String>,
    pub documentation_url: Option<String>,
    pub catalog_updated_at: String,
    pub probe_status: String,
    pub probed_at: Option<String>,
    // ── v1.5.7 国产配置体检 ────────────────────────────────────────────
    /// Profile 原始 prompt_cache 字段(`None` / `Some(true)` / `Some(false)`)
    pub profile_prompt_cache: Option<bool>,
    /// Profile 原始 openai_stream_options 字段
    pub profile_openai_stream_options: Option<bool>,
    /// Profile 原始 thinking_budget 字段
    pub profile_thinking_budget: Option<u32>,
    /// Profile 原始 vision 字段
    pub profile_vision: bool,
    /// Profile 原始 base_url(用于诊断 provider 协议与 base_url 协议是否匹配)
    pub profile_base_url: String,
    /// 推导后真正生效的 prompt_cache(走官方 Anthropic 端点时默认 true)
    pub effective_prompt_cache: bool,
    /// 推导后真正生效的 openai_stream_options(provider=OpenAI 或国产 vendor 时默认 true)
    pub effective_stream_options: bool,
    /// 推导后是否需要供应商返回精确 usage(MiniMax/GLM/DeepSeek 等)
    pub requires_supplier_usage: bool,
}

impl ModelDoctorReport {
    pub fn static_report(profile: &Profile, cache: Option<&CapabilityCache>) -> Self {
        let CatalogResolution {
            identity,
            capabilities,
            endpoint_type,
            verification_status,
            known_degradations,
            documentation_url,
            catalog_updated_at,
        } = ModelCatalog::resolve(profile, None);
        let cached = cache.and_then(|cache| cache.load(&identity).ok().flatten());
        let (capabilities, verification_status, probe_status, probed_at) = match cached {
            Some(record) => (
                record.capabilities,
                VerificationStatus::LiveVerified,
                "cached_live_probe".to_string(),
                Some(record.probed_at.to_rfc3339()),
            ),
            None => (
                capabilities,
                verification_status,
                "not_probed".to_string(),
                None,
            ),
        };
        Self {
            profile: profile.name.clone(),
            identity,
            endpoint_type,
            verification_status,
            capabilities,
            known_degradations,
            documentation_url,
            catalog_updated_at,
            probe_status,
            probed_at,
            profile_prompt_cache: profile.prompt_cache,
            profile_openai_stream_options: profile.openai_stream_options,
            profile_thinking_budget: profile.thinking_budget,
            profile_vision: profile.vision,
            profile_base_url: profile.base_url.clone(),
            effective_prompt_cache: profile.effective_prompt_cache(),
            effective_stream_options: profile
                .effective_openai_stream_options_for_model(&profile.model),
            requires_supplier_usage: profile
                .uses_provider_exact_token_usage_for_model(&profile.model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_cache_remains_static_only() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::default();
        let identity = ModelCatalog::resolve(&profile, None).identity;
        let cache = CapabilityCache::new(dir.path());
        let fingerprint = CapabilityCache::fingerprint(&identity);
        let cache_dir = dir.path().join("model-capabilities");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join(format!("{fingerprint}.json")), "not-json").unwrap();

        let report = ModelDoctorReport::static_report(&profile, Some(&cache));
        assert_ne!(report.verification_status, VerificationStatus::LiveVerified);
        assert_eq!(report.probe_status, "not_probed");
    }

    /// v1.5.7 国产配置体检:模拟 GLM Profile（Anthropic 兼容端点 + 显式
    /// prompt_cache=false），doctor 应填充 profile_* 字段 + 推导 effective 值。
    #[test]
    fn profile_fields_populated_for_third_party_anthropic() {
        let profile = Profile {
            base_url: "https://open.bigmodel.cn/api/anthropic".to_string(),
            provider: wyj_config::Provider::Anthropic,
            prompt_cache: Some(false),
            model: "glm-4.6".to_string(),
            ..Profile::default()
        };

        let report = ModelDoctorReport::static_report(&profile, None);
        assert_eq!(report.profile_prompt_cache, Some(false));
        assert_eq!(
            report.profile_base_url,
            "https://open.bigmodel.cn/api/anthropic"
        );
        // 第三方端点 → effective_prompt_cache 跟随用户显式 false
        assert!(!report.effective_prompt_cache);
    }

    /// 官方 Anthropic 端点 prompt_cache 默认 true,即使 Profile 未显式设置。
    #[test]
    fn effective_prompt_cache_true_for_official_anthropic() {
        let profile = Profile {
            provider: wyj_config::Provider::Anthropic,
            // base_url 留空 → 使用 api.anthropic.com
            model: "claude-sonnet-4-5".to_string(),
            ..Profile::default()
        };

        let report = ModelDoctorReport::static_report(&profile, None);
        // 用户未显式设置 → None → 官方端点默认 true
        assert_eq!(report.profile_prompt_cache, None);
        assert!(report.effective_prompt_cache);
    }

    /// DeepSeek (OpenAI 路径) 的 stream_options 推导为 true,且 requires_supplier_usage=true。
    #[test]
    fn deepseek_openai_effective_stream_options_and_supplier_usage() {
        let profile = Profile {
            provider: wyj_config::Provider::OpenAI,
            base_url: "https://api.deepseek.com".to_string(),
            model: "deepseek-chat".to_string(),
            ..Profile::default()
        };

        let report = ModelDoctorReport::static_report(&profile, None);
        assert_eq!(report.profile_openai_stream_options, None);
        // OpenAI provider + DeepSeek vendor → stream_options effective=true
        assert!(report.effective_stream_options);
        // DeepSeek 在 uses_provider_exact_token_usage_for_model 名单内
        assert!(report.requires_supplier_usage);
    }
}
