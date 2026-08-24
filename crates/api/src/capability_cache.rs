//! 不含密钥和请求正文的模型能力探测缓存。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{thinking::VendorVariant, ModelCapabilities, ModelIdentity};

pub const PROBE_VERSION: u32 = 2;

/// 端点运行时拒绝过的参数记录。Agent 降级分支（`parameter_degraded`）在撤掉
/// 不安全参数重试成功后调 `record_rejection` 写入本结构，下次同 fingerprint
/// load 时让 `model_catalog.resolve` 把 `capabilities.thinking` 等标为
/// `Unsupported`，避免反复发端点不认的字段触发 400。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RejectedParamRecord {
    pub parameter: String,
    pub reason: String,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityCacheRecord {
    pub probe_version: u32,
    pub fingerprint: String,
    pub identity: ModelIdentity,
    pub capabilities: ModelCapabilities,
    pub probed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Agent 运行时曾被端点拒绝过的参数清单。serde default 保证 v2 之前
    /// 落盘但缺此字段的记录仍能反序列化，但 PROBE_VERSION bump 让旧 v1 cache
    /// 整体被忽略（见 `CapabilityCache::load`）。
    #[serde(default)]
    pub rejected_parameters: Vec<RejectedParamRecord>,
    /// 探测确认的字段名版本变体。serde default 让老记录兼容读出（默认
    /// `StandardV1`），但 fingerprint 算法 model 名归一化（去 `-thinking`、
    /// `-coder-plus` 等后缀）会让 hash 改变——需要 PROBE_VERSION bump 才会
    /// 整体失效。
    #[serde(default)]
    pub variant: VendorVariant,
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
        // model 名归一化：去常见 vendor-specific 后缀（`-thinking`/`-coder-plus`
        // 等），让 fingerprint 不被模型版本号微小变化击穿。PROBE_VERSION 已 bump
        // 到 2，老 v1 cache 整体失效，不会出现 hash 迁移期兼容问题。
        hasher.update(normalize_model_name(&identity.model).as_bytes());
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
            rejected_parameters: Vec::new(),
            variant: VendorVariant::default(),
        };
        self.write_record(&record)
    }

    /// 写入已有 record（含更新后的 rejected_parameters）。原 store() 仅在
    /// `record_rejection`/`clear` 后内部调用，避免每次都重新构造完整 record。
    fn write_record(&self, record: &CapabilityCacheRecord) -> Result<CapabilityCacheRecord> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建能力缓存目录失败: {}", self.dir.display()))?;
        let path = self.dir.join(format!("{}.json", record.fingerprint));
        let tmp = self.dir.join(format!(
            ".{}.{}.tmp",
            record.fingerprint,
            std::process::id()
        ));
        let content = serde_json::to_vec_pretty(record)?;
        std::fs::write(&tmp, content)
            .with_context(|| format!("写入能力缓存临时文件失败: {}", tmp.display()))?;
        if let Err(error) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("原子替换能力缓存失败: {}", path.display()));
        }
        Ok(record.clone())
    }

    /// 记录端点运行时拒绝过的参数。若已有 record 则合并写入；否则写一个只有
    /// rejected_parameters 的 stub record（capabilities 为空，方便下次 load
    /// 时直接看到原因）。失败也不阻塞主流程——参数降级是优化项，不应让主对话
    /// 路径因为 cache 写失败而中断。
    pub fn record_rejection(&self, identity: &ModelIdentity, parameter: &str, reason: &str) {
        let result: Result<()> = (|| {
            let identity = identity.clone().sanitized();
            let fingerprint = Self::fingerprint(&identity);
            let path = self.dir.join(format!("{fingerprint}.json"));
            let mut record = if path.is_file() {
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("读取能力缓存失败: {}", path.display()))?;
                serde_json::from_str::<CapabilityCacheRecord>(&raw).unwrap_or_else(|_| {
                    // 解析失败：构造新 stub
                    let now = Utc::now();
                    CapabilityCacheRecord {
                        probe_version: PROBE_VERSION,
                        fingerprint: fingerprint.clone(),
                        identity: identity.clone(),
                        capabilities: crate::ModelCapabilities::conservative(
                            identity.vendor.len().max(1) as u32 * 1000,
                            4096,
                        ),
                        probed_at: now,
                        expires_at: now + self.ttl,
                        rejected_parameters: Vec::new(),
                        variant: VendorVariant::default(),
                    }
                })
            } else {
                std::fs::create_dir_all(&self.dir)
                    .with_context(|| format!("创建能力缓存目录失败: {}", self.dir.display()))?;
                let now = Utc::now();
                CapabilityCacheRecord {
                    probe_version: PROBE_VERSION,
                    fingerprint,
                    identity: identity.clone(),
                    capabilities: crate::ModelCapabilities::conservative(
                        identity.vendor.len().max(1) as u32 * 1000,
                        4096,
                    ),
                    probed_at: now,
                    expires_at: now + self.ttl,
                    rejected_parameters: Vec::new(),
                    variant: VendorVariant::default(),
                }
            };
            // 已记录过同 parameter 不重复写
            if record
                .rejected_parameters
                .iter()
                .any(|r| r.parameter == parameter)
            {
                return Ok(());
            }
            record.rejected_parameters.push(RejectedParamRecord {
                parameter: parameter.to_string(),
                reason: reason.to_string(),
                recorded_at: Utc::now(),
            });
            self.write_record(&record).map(|_| ())
        })();
        if let Err(error) = result {
            tracing::warn!(parameter, "记录 rejected_parameters 失败: {error}");
        }
    }

    /// 清空整个能力缓存（CLI `wyj-code model doctor --clear-cache`）。
    pub fn clear(&self) -> Result<usize> {
        if !self.dir.is_dir() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in std::fs::read_dir(&self.dir)
            .with_context(|| format!("读取能力缓存目录失败: {}", self.dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                std::fs::remove_file(&path)
                    .with_context(|| format!("删除能力缓存失败: {}", path.display()))?;
                count += 1;
            }
        }
        Ok(count)
    }
}

/// 归一化 model 名为 fingerprint 用：去常见 vendor-specific 后缀（带连字符）
/// 与版本号尾缀，让 hash 不被微小变化击穿。多轮迭代直到稳定，处理
/// `qwen3-coder-plus-v2` 这类后缀 + 版本号叠加的情况。
fn normalize_model_name(model: &str) -> String {
    let suffixes = [
        "-thinking",
        "-reasoner",
        "-r1",
        "-code",
        "-instruct",
        "-chat",
        "-base",
        "-plus",
    ];
    let mut name = model.to_ascii_lowercase();
    // 多轮迭代：后缀与版本号交替剥离，直到一次循环内都不再变化
    loop {
        let prev = name.clone();
        // vendor-specific 变体后缀
        for suffix in suffixes {
            if let Some(stripped) = name.strip_suffix(suffix) {
                name = stripped.to_string();
                break;
            }
        }
        // 去尾部版本号：`-v4-pro`/`-20251124`/`-1.5`/`-2025-11-24`/`-v2`
        // 规则：tail 含字母但同时含 digit 且 digit 是主体 → 算版本号剥掉；
        // 纯字母如 `coder` 不剥。`v2`/`v1.5` 这类以 `v` 开头的版本号通过
        // strip 掉单独的 'v' 前缀后再判定。
        while let Some((prefix, tail)) = name.rsplit_once('-') {
            let stripped_v = tail
                .strip_prefix('v')
                .or_else(|| tail.strip_prefix('V'))
                .unwrap_or(tail);
            let tail_ok = !stripped_v.is_empty()
                && stripped_v
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
                && stripped_v.chars().any(|c| c.is_ascii_digit());
            if tail_ok && prefix != name {
                name = prefix.to_string();
            } else {
                break;
            }
        }
        if name == prev {
            break;
        }
    }
    name
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

    #[test]
    fn record_rejection_appends_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let resolution = ModelCatalog::resolve(&Profile::default(), None);
        let cache = CapabilityCache::new(dir.path());
        cache.record_rejection(&resolution.identity, "thinking", "endpoint returned 4xx");
        cache.record_rejection(
            &resolution.identity,
            "thinking",
            "endpoint returned 4xx again",
        );
        cache.record_rejection(
            &resolution.identity,
            "thinking_budget",
            "budget unsupported",
        );

        let loaded = cache
            .load(&resolution.identity)
            .unwrap()
            .expect("record present");
        // 同 parameter 二次写入去重
        assert_eq!(loaded.rejected_parameters.len(), 2);
        let names: Vec<_> = loaded
            .rejected_parameters
            .iter()
            .map(|r| r.parameter.as_str())
            .collect();
        assert!(names.contains(&"thinking"));
        assert!(names.contains(&"thinking_budget"));
    }

    #[test]
    fn rejected_thinking_marks_catalog_thinking_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile {
            vendor: Some("deepseek".to_string()),
            base_url: "https://api.deepseek.com".to_string(),
            model: "deepseek-v4-pro".to_string(),
            thinking_budget: Some(8000),
            ..Profile::default()
        };
        let resolution = ModelCatalog::resolve(&profile, None);
        // 端点运行时拒 thinking_budget，缓存到 cache
        let cache = CapabilityCache::new(dir.path());
        cache.record_rejection(
            &resolution.identity,
            "thinking_budget",
            "endpoint returned 400 for unsupported parameter",
        );

        // 再 resolve 一次（带 cache）→ capabilities.thinking 应为 Unsupported
        let resolved = ModelCatalog::resolve_with_cache(&profile, None, Some(&cache));
        assert_eq!(
            resolved.capabilities.thinking.value,
            crate::ThinkingMode::Unsupported,
            "rejected thinking 系列参数应让 capabilities.thinking 降级 Unsupported"
        );
        assert_eq!(
            resolved.capabilities.thinking.source,
            crate::capabilities::CapabilitySource::UserOverride
        );
    }

    #[test]
    fn clear_removes_all_records() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CapabilityCache::new(dir.path());
        let resolution = ModelCatalog::resolve(&Profile::default(), None);
        cache
            .store(resolution.identity.clone(), resolution.capabilities.clone())
            .unwrap();
        let removed = cache.clear().unwrap();
        assert_eq!(removed, 1);
        assert!(cache.load(&resolution.identity).unwrap().is_none());
    }

    /// 阶段 7：model 名归一化让相近模型走相同 fingerprint，避免微小版本号差异
    /// 击穿 cache。归一化目标是「变体后缀 + 尾部版本号」，不破坏模型系列核心名
    /// （如 `qwen3-coder` 仍是独立核心）。
    #[test]
    fn normalize_model_name_strips_vendor_suffixes_and_versions() {
        use super::normalize_model_name;
        assert_eq!(normalize_model_name("qwen3-coder-plus"), "qwen3-coder");
        assert_eq!(normalize_model_name("qwen3-coder-plus-v2"), "qwen3-coder");
        assert_eq!(normalize_model_name("deepseek-reasoner"), "deepseek");
        assert_eq!(normalize_model_name("deepseek-r1"), "deepseek");
        assert_eq!(normalize_model_name("kimi-k2.5-thinking"), "kimi-k2.5");
        assert_eq!(normalize_model_name("doubao-seed-code"), "doubao-seed");
        assert_eq!(normalize_model_name("MiniMax-M3"), "minimax-m3");
        // 不动普通模型名
        assert_eq!(normalize_model_name("gpt-4o"), "gpt-4o");
    }

    /// 阶段 7：fingerprint 在 model 名后缀变化时保持稳定。
    #[test]
    fn fingerprint_stable_across_vendor_suffix_variants() {
        let _dir = tempfile::tempdir().unwrap();
        let id_with_suffix = ModelIdentity {
            vendor: "deepseek".to_string(),
            model: "deepseek-reasoner".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            wire_protocol: wyj_config::WireProtocol::OpenAiChatCompletions,
        };
        let id_without_suffix = ModelIdentity {
            model: "deepseek".to_string(),
            ..id_with_suffix.clone()
        };
        assert_eq!(
            CapabilityCache::fingerprint(&id_with_suffix),
            CapabilityCache::fingerprint(&id_without_suffix),
            "model 名归一化后 fingerprint 应相同"
        );
    }

    /// 阶段 7：VendorVariant 默认 StandardV1。
    #[test]
    fn vendor_variant_default_is_standard_v1() {
        assert_eq!(VendorVariant::default(), VendorVariant::StandardV1);
    }
}
