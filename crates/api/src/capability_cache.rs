//! 不含密钥和请求正文的模型能力探测缓存。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ModelCapabilities, ModelIdentity};

pub const PROBE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityCacheRecord {
    pub probe_version: u32,
    pub fingerprint: String,
    pub identity: ModelIdentity,
    pub capabilities: ModelCapabilities,
    pub probed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

pub struct CapabilityCache {
    dir: PathBuf,
    ttl: Duration,
}

impl CapabilityCache {
    pub fn new(config_base: &Path) -> Self {
        Self {
            dir: config_base.join("model-capabilities"),
            ttl: Duration::days(7),
        }
    }

    pub fn fingerprint(identity: &ModelIdentity) -> String {
        let identity = identity.clone().sanitized();
        let mut hasher = Sha256::new();
        hasher.update(identity.vendor.as_bytes());
        hasher.update([0]);
        hasher.update(identity.wire_protocol.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(identity.base_url.trim_end_matches('/').as_bytes());
        hasher.update([0]);
        hasher.update(identity.model.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub fn load(&self, identity: &ModelIdentity) -> Result<Option<CapabilityCacheRecord>> {
        let fingerprint = Self::fingerprint(identity);
        let path = self.dir.join(format!("{fingerprint}.json"));
        if !path.is_file() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("读取能力缓存失败: {}", path.display()))?;
        let record: CapabilityCacheRecord = serde_json::from_str(&content)
            .with_context(|| format!("解析能力缓存失败: {}", path.display()))?;
        if record.probe_version != PROBE_VERSION
            || record.fingerprint != fingerprint
            || Self::fingerprint(&record.identity) != fingerprint
            || record.expires_at <= Utc::now()
        {
            return Ok(None);
        }
        Ok(Some(record))
    }

    pub fn store(
        &self,
        identity: ModelIdentity,
        capabilities: ModelCapabilities,
    ) -> Result<CapabilityCacheRecord> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建能力缓存目录失败: {}", self.dir.display()))?;
        let now = Utc::now();
        let identity = identity.sanitized();
        let record = CapabilityCacheRecord {
            probe_version: PROBE_VERSION,
            fingerprint: Self::fingerprint(&identity),
            identity,
            capabilities,
            probed_at: now,
            expires_at: now + self.ttl,
        };
        let path = self.dir.join(format!("{}.json", record.fingerprint));
        let tmp = self.dir.join(format!(
            ".{}.{}.tmp",
            record.fingerprint,
            std::process::id()
        ));
        let content = serde_json::to_vec_pretty(&record)?;
        std::fs::write(&tmp, content)
            .with_context(|| format!("写入能力缓存临时文件失败: {}", tmp.display()))?;
        if let Err(error) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("原子替换能力缓存失败: {}", path.display()));
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelCatalog;
    use wyj_config::Profile;

    #[test]
    fn cache_roundtrip_contains_no_api_key_and_is_identity_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile {
            api_key: Some("must-not-be-stored".to_string()),
            ..Profile::default()
        };
        let resolution = ModelCatalog::resolve(&profile, None);
        let cache = CapabilityCache::new(dir.path());
        let stored = cache
            .store(resolution.identity.clone(), resolution.capabilities.clone())
            .unwrap();
        let loaded = cache.load(&resolution.identity).unwrap().unwrap();
        assert_eq!(stored.fingerprint, loaded.fingerprint);
        let raw = std::fs::read_to_string(
            dir.path()
                .join("model-capabilities")
                .join(format!("{}.json", stored.fingerprint)),
        )
        .unwrap();
        assert!(!raw.contains("must-not-be-stored"));

        let mut changed = resolution.identity;
        changed.model.push_str("-new");
        assert!(cache.load(&changed).unwrap().is_none());
    }

    #[test]
    fn expired_mismatched_and_corrupt_records_never_promote_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let resolution = ModelCatalog::resolve(&Profile::default(), None);
        let cache = CapabilityCache::new(dir.path());
        let stored = cache
            .store(resolution.identity.clone(), resolution.capabilities.clone())
            .unwrap();
        let path = dir
            .path()
            .join("model-capabilities")
            .join(format!("{}.json", stored.fingerprint));

        let mut expired = stored.clone();
        expired.expires_at = Utc::now() - Duration::seconds(1);
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        assert!(cache.load(&resolution.identity).unwrap().is_none());

        let mut mismatched = stored.clone();
        mismatched.probe_version = PROBE_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&mismatched).unwrap()).unwrap();
        assert!(cache.load(&resolution.identity).unwrap().is_none());

        let mut mismatched = stored.clone();
        mismatched.identity.model.push_str("-other");
        std::fs::write(&path, serde_json::to_vec(&mismatched).unwrap()).unwrap();
        assert!(cache.load(&resolution.identity).unwrap().is_none());

        std::fs::write(&path, b"{not-json").unwrap();
        assert!(cache.load(&resolution.identity).is_err());
    }
}
