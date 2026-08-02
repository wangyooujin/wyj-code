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
}
