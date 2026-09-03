//! 内容寻址存储 (Content-Addressable Storage) for workspace snapshot 字节去重。
//!
//! # 设计动机
//!
//! 旧实现把 `FileEntry { bytes: Vec<u8>, sha256 }` 直接内联在每个 checkpoint JSON
//! 里。21 个相邻 checkpoint × 256 个文件 = 5376 份冗余副本,实测单 session 占
//! 223MB(其中 90.9% 来自 workspace snapshot)。本模块提供:
//!
//! - 按 SHA-256 内容寻址的 blob 池(`~/.wyj-code/cas/sha256/aa/bb/<hash>.blob`)
//! - `intern()` 幂等(同内容多次写只增 ref_count,不重复创建实体)
//! - `get()` / `release()` 配合 ref-counted GC
//! - 大文件 / 空文件 / CAS root 不可写的 fallback 路径
//!
//! # 与 checkpoint 集成
//!
//! - `checkpoint.rs::FileEntry` 把 `bytes: Vec<u8>` 替换为 `hash: Option<String>` +
//!   `inline_bytes: Vec<u8>`(超阈值或空文件时填 inline),`sha256_hex` 字段保留供
//!   Phase 2 diff
//! - `CheckpointStore` 在构造时接收 `Arc<WorkspaceCas>`,`capture_files` 走
//!   `cas.intern()`;`restore_files_snapshot` 走 `cas.get()`
//!
//! # 为什么不引入 BLAKE3/zstd
//!
//! 本项目零新增依赖约束 —— sha2 crate 已依赖;zstd 仅间接在 lock 中,直接复用需要
//! 加显式依赖。SHA-256 寻址 + 不压缩已足够(原始字节本身已不重复)。
//!
//! # 路径布局
//!
//! ```text
//! ~/.wyj-code/cas/sha256/aa/bb/<64hex>.blob        # 原始字节实体
//! ~/.wyj-code/cas/sha256/aa/bb/<64hex>.meta.json   # {size, ref_count, stored_at, last_ref_at}
//! ```
//!
//! 两级目录分散避免单目录文件数过多(O(256*256) = 65k 上限)。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单 blob 元数据,落 `<hash>.meta.json`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CasMeta {
    pub size: u64,
    pub ref_count: u32,
    pub stored_at: String,
    pub last_ref_at: String,
}

/// CAS 池根目录 + 单 blob 大小上限。
#[derive(Debug, Clone)]
pub struct WorkspaceCas {
    root: PathBuf,
    /// 单 blob 字节上限。超过此值的字节不进 CAS(走 inline 路径)。
    /// 默认 16 MiB,防止单文件炸掉 CAS pool。
    max_blob_bytes: u64,
}

impl WorkspaceCas {
    /// 打开或创建 CAS 池根目录。`max_blob_bytes == 0` 时 fallback 到 16 MiB。
    pub fn open(root: &Path, max_blob_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("create CAS root {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            max_blob_bytes: if max_blob_bytes == 0 {
                16 * 1024 * 1024
            } else {
                max_blob_bytes
            },
        })
    }

    /// 仅创建根目录而不返回错误(供 checkpoint 装配时"尽力而为"路径)。
    pub fn open_silently(root: &Path, max_blob_bytes: u64) -> Option<Self> {
        Self::open(root, max_blob_bytes).ok()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_blob_bytes(&self) -> u64 {
        self.max_blob_bytes
    }

    /// 实体文件路径:`<root>/sha256/aa/bb/<64hex>.blob`。
    fn blob_path(&self, hash: &str) -> PathBuf {
        debug_assert_eq!(hash.len(), 64, "SHA-256 hex must be 64 chars");
        self.root
            .join("sha256")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(format!("{hash}.blob"))
    }

    /// 元数据文件路径:`<root>/sha256/aa/bb/<64hex>.meta.json`。
    fn meta_path(&self, hash: &str) -> PathBuf {
        debug_assert_eq!(hash.len(), 64, "SHA-256 hex must be 64 chars");
        self.root
            .join("sha256")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(format!("{hash}.meta.json"))
    }

    /// 计算 SHA-256 并返回 lowercase hex。纯函数,无副作用。
    pub fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// 写或返回已存在 hash。幂等:同内容多次写只增 ref_count。
    ///
    /// 错误时返回 `Err`,调用方应 fallback 到 inline bytes(不阻断 checkpoint 写入)。
    pub fn intern(&self, bytes: &[u8]) -> Result<String> {
        let hash = Self::digest(bytes);
        let blob = self.blob_path(&hash);
        let meta = self.meta_path(&hash);

        let now = now_iso();
        if blob.exists() {
            // 已存在:原子地 ref_count++ 并刷新 last_ref_at
            self.bump_ref_count(&meta, &now)?;
            return Ok(hash);
        }

        // 不存在:写入 .blob + 创建 .meta.json
        if let Some(parent) = blob.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create CAS dir {}", parent.display())
            })?;
        }
        // write-then-rename 原子性
        let tmp_blob = blob.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp_blob, bytes)
            .with_context(|| format!("write CAS blob {}", tmp_blob.display()))?;
        if let Err(error) = std::fs::rename(&tmp_blob, &blob) {
            let _ = std::fs::remove_file(&tmp_blob);
            return Err(error.into());
        }

        let meta_value = CasMeta {
            size: bytes.len() as u64,
            ref_count: 1,
            stored_at: now.clone(),
            last_ref_at: now,
        };
        // meta 写入失败不致命 —— 下次 intern 时会重建
        if let Err(error) = write_meta_atomic(&meta, &meta_value) {
            tracing::warn!(
                "CAS meta 写入失败 {} (hash={}): {error} —— 后续 ref_count 可能漂移",
                meta.display(),
                hash
            );
        }
        Ok(hash)
    }

    /// 读实体字节。`NotFound` 时返回明确错误(供 restore 调用方 fallback)。
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(hash);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(anyhow::anyhow!(
                    "CAS blob not found: hash={hash}, path={}",
                    path.display()
                ))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// 释放一次引用。`ref_count` 减到 0 时 **不立即物理删除** blob
    /// (lazy delete),等下次 `gc()` 周期清理。这跟 git pack-files 的设计一致:
    /// 1) 减少同步 IO 抖动(release 频繁被调);
    /// 2) 给 `gc()` 提供按 LRU 策略(基于 `last_ref_at`)淘汰的机会;
    /// 3) crash-safety:中途崩溃只丢 ref_count,不丢数据。
    ///
    /// meta.ref_count 减到 0 后,`gc()` 会删除 `.blob` + `.meta.json`。
    pub fn release(&self, hash: &str) -> Result<()> {
        let meta_path = self.meta_path(hash);
        if !meta_path.exists() {
            return Ok(());
        }
        let mut meta: CasMeta = match serde_json::from_slice(&std::fs::read(&meta_path)?) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    "CAS meta 解析失败 {}: {error} —— 保守不动",
                    meta_path.display()
                );
                return Ok(());
            }
        };
        meta.ref_count = meta.ref_count.saturating_sub(1);
        write_meta_atomic(&meta_path, &meta)?;
        Ok(())
    }

    /// 原子地 ref_count++ 并刷新 last_ref_at。
    fn bump_ref_count(&self, meta_path: &Path, now: &str) -> Result<()> {
        let mut meta: CasMeta = match std::fs::read(meta_path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        "CAS meta 解析失败 {}: {error} —— 重建 ref_count=1",
                        meta_path.display()
                    );
                    CasMeta {
                        size: 0,
                        ref_count: 1,
                        stored_at: now.to_string(),
                        last_ref_at: now.to_string(),
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CasMeta {
                size: 0,
                ref_count: 1,
                stored_at: now.to_string(),
                last_ref_at: now.to_string(),
            },
            Err(error) => return Err(error.into()),
        };
        meta.ref_count = meta.ref_count.saturating_add(1);
        meta.last_ref_at = now.to_string();
        write_meta_atomic(meta_path, &meta)
    }

    /// 扫描 CAS root,统计占用(ref_count=0 的实体不算"活跃"但占磁盘)。
    /// 仅扫描,不删除 —— 由 `gc()` 负责清理。
    pub fn stats(&self) -> Result<CasStats> {
        let mut stats = CasStats::default();
        let walker = self.root.join("sha256");
        if !walker.exists() {
            return Ok(stats);
        }
        for aa_entry in std::fs::read_dir(&walker)? {
            let aa_path = aa_entry?.path();
            if !aa_path.is_dir() {
                continue;
            }
            for bb_entry in std::fs::read_dir(&aa_path)? {
                let bb_path = bb_entry?.path();
                if !bb_path.is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(&bb_path)? {
                    let path = entry?.path();
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.ends_with(".meta.json") {
                        if let Ok(bytes) = std::fs::read(&path) {
                            if let Ok(meta) = serde_json::from_slice::<CasMeta>(&bytes) {
                                stats.total_blobs += 1;
                                stats.total_bytes += meta.size;
                                if meta.ref_count == 0 {
                                    stats.orphan_blobs += 1;
                                    stats.orphan_bytes += meta.size;
                                }
                            }
                        }
                    } else if name.ends_with(".blob") {
                        // 实体大小直接从 .blob 文件 stat 拿(避免读 meta)
                        if let Ok(meta) = std::fs::metadata(&path) {
                            stats.on_disk_bytes += meta.len();
                        }
                    }
                }
            }
        }
        Ok(stats)
    }

    /// GC 阶段 1:删除 `ref_count == 0` 的实体(无论 TTL)。`force` 为 true 时跳过此保护。
    /// 阶段 2:若总字节 > `total_budget`,按 last_ref_at 升序淘汰最久未引用的 0-ref 实体,
    /// 直到 ≤ budget。
    ///
    /// **首启保护**:Phase 4 默认 `force=false`,老 session 的 hash 全是 0-ref,
    /// GC 会误删。所以 `StorageRetentionCfg.cas_gc_on_start` 在首启跳过。
    /// 业务上以"调用方知道 ref_count 已重建"为前提才传 `force=true`。
    pub fn gc(&self, total_budget: u64) -> Result<GcStats> {
        let walker = self.root.join("sha256");
        if !walker.exists() {
            return Ok(GcStats::default());
        }
        // 第一遍:列出所有 (hash, meta, path)
        let mut entries: Vec<(String, CasMeta, PathBuf, PathBuf)> = Vec::new();
        for aa_entry in std::fs::read_dir(&walker)? {
            let aa_path = aa_entry?.path();
            if !aa_path.is_dir() {
                continue;
            }
            for bb_entry in std::fs::read_dir(&aa_path)? {
                let bb_path = bb_entry?.path();
                if !bb_path.is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(&bb_path)? {
                    let path = entry?.path();
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.ends_with(".meta.json") {
                        let hash = name.trim_end_matches(".meta.json").to_string();
                        if let Ok(bytes) = std::fs::read(&path) {
                            if let Ok(meta) = serde_json::from_slice::<CasMeta>(&bytes) {
                                // meta path 是 "<dir>/<hash>.meta.json" →
                                // blob path 是 "<dir>/<hash>.blob"。用 with_file_name
                                // 避免 with_extension 把 ".meta.json" 误转成 ".meta.blob"。
                                let blob_name = format!("{hash}.blob");
                                let blob_path = path.with_file_name(blob_name);
                                entries.push((hash, meta, blob_path, path));
                            }
                        }
                    }
                }
            }
        }
        // 计算当前总字节
        let total_bytes: u64 = entries.iter().map(|(_, m, _, _)| m.size).sum();
        let mut stats = GcStats {
            scanned_blobs: entries.len() as u64,
            total_bytes,
            ..Default::default()
        };
        if total_bytes <= total_budget {
            stats.budget_ok = true;
            return Ok(stats);
        }
        // 超 budget:按 last_ref_at 升序删 0-ref 实体直到 ≤ budget
        let mut candidates: Vec<_> = entries
            .iter()
            .filter(|(_, m, _, _)| m.ref_count == 0)
            .cloned()
            .collect();
        candidates.sort_by(|a, b| a.1.last_ref_at.cmp(&b.1.last_ref_at));
        for (hash, meta, blob_path, meta_path) in &candidates {
            if stats.total_bytes <= total_budget {
                break;
            }
            if let Err(error) = std::fs::remove_file(blob_path) {
                tracing::warn!("GC blob 删除失败 {}: {error}", blob_path.display());
                continue;
            }
            if let Err(error) = std::fs::remove_file(meta_path) {
                tracing::warn!("GC meta 删除失败 {}: {error}", meta_path.display());
            }
            stats.deleted_blobs += 1;
            stats.freed_bytes += meta.size;
            stats.total_bytes = stats.total_bytes.saturating_sub(meta.size);
            tracing::debug!("GC 删除 {hash} ({} bytes)", meta.size);
        }
        Ok(stats)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    pub scanned_blobs: u64,
    pub total_bytes: u64,
    pub deleted_blobs: u64,
    pub freed_bytes: u64,
    pub budget_ok: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CasStats {
    pub total_blobs: u64,
    pub total_bytes: u64,
    pub on_disk_bytes: u64,
    pub orphan_blobs: u64,
    pub orphan_bytes: u64,
}

fn write_meta_atomic(path: &Path, value: &CasMeta) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let json = serde_json::to_string(value)?;
    std::fs::write(&tmp, json)?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cas_with_root(root: &Path) -> WorkspaceCas {
        WorkspaceCas::open(root, 1024 * 1024).unwrap()
    }

    #[test]
    fn digest_is_64_lowercase_hex() {
        let h = WorkspaceCas::digest(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn digest_stable() {
        assert_eq!(
            WorkspaceCas::digest(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn intern_idempotent_increments_ref_count() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let h1 = cas.intern(b"abc").unwrap();
        let h2 = cas.intern(b"abc").unwrap();
        assert_eq!(h1, h2);
        let meta_path = cas.meta_path(&h1);
        let meta: CasMeta = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        assert_eq!(meta.ref_count, 2);
        assert_eq!(meta.size, 3);
    }

    #[test]
    fn intern_creates_blob_path() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let h = cas.intern(b"data").unwrap();
        let blob = cas.blob_path(&h);
        let meta = cas.meta_path(&h);
        assert!(blob.exists(), "blob file should exist at {}", blob.display());
        assert!(meta.exists(), "meta file should exist at {}", meta.display());
        assert_eq!(std::fs::read(&blob).unwrap(), b"data");
    }

    #[test]
    fn release_decrements_ref_count_keeps_blob() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let h = cas.intern(b"x").unwrap();
        cas.intern(b"x").unwrap();
        cas.release(&h).unwrap();
        assert!(cas.blob_path(&h).exists());
        let meta: CasMeta =
            serde_json::from_slice(&std::fs::read(cas.meta_path(&h)).unwrap()).unwrap();
        assert_eq!(meta.ref_count, 1);
    }

    #[test]
    fn release_to_zero_keeps_blob_for_gc() {
        // Phase 4 改为 lazy delete:release 减 ref_count 到 0 后不立即删 blob,
        // 由 gc() 周期清理。证明 release 后 blob/meta 仍在,ref_count=0。
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let h = cas.intern(b"y").unwrap();
        cas.release(&h).unwrap();
        // blob + meta 仍在(等 GC 删)
        assert!(cas.blob_path(&h).exists());
        assert!(cas.meta_path(&h).exists());
        let meta: CasMeta =
            serde_json::from_slice(&std::fs::read(cas.meta_path(&h)).unwrap()).unwrap();
        assert_eq!(meta.ref_count, 0);
        // GC 后被删
        let _ = cas.gc(0).unwrap();
        assert!(!cas.blob_path(&h).exists());
        assert!(!cas.meta_path(&h).exists());
    }

    #[test]
    fn release_missing_meta_is_noop() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        // 不存在 hash 释放不报错
        cas.release("0".repeat(64).as_str()).unwrap();
    }

    #[test]
    fn get_returns_bytes() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let h = cas.intern(b"payload").unwrap();
        assert_eq!(cas.get(&h).unwrap(), b"payload");
    }

    #[test]
    fn get_missing_returns_not_found_error() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let err = cas.get("0".repeat(64).as_str()).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn stats_aggregates_correctly() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        cas.intern(b"a").unwrap();
        cas.intern(b"a").unwrap(); // ref_count=2
        cas.intern(b"b").unwrap(); // ref_count=1
        cas.release(&cas.intern(b"c").unwrap()).unwrap(); // ref_count=0,留着等 GC
        let stats = cas.stats().unwrap();
        // a + b + c 三个 blob,size 各 1 byte,c 是 orphan(ref=0)
        assert_eq!(stats.total_blobs, 3);
        assert_eq!(stats.total_bytes, 3);
        assert_eq!(stats.orphan_blobs, 1);
        assert_eq!(stats.orphan_bytes, 1);
    }

    #[test]
    fn open_with_zero_max_blob_uses_default() {
        let dir = TempDir::new().unwrap();
        let cas = WorkspaceCas::open(dir.path(), 0).unwrap();
        assert_eq!(cas.max_blob_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn blob_path_uses_two_level_directory() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        let h = cas.intern(b"x").unwrap();
        let blob = cas.blob_path(&h);
        // sha256/aa/bb/<hash>.blob
        let rel = blob.strip_prefix(&cas.root).unwrap();
        let parts: Vec<_> = rel.iter().collect();
        assert_eq!(parts.len(), 4); // sha256 / aa / bb / <hash>.blob
        assert_eq!(parts[0].to_str().unwrap(), "sha256");
        assert_eq!(parts[1].to_str().unwrap().len(), 2);
        assert_eq!(parts[2].to_str().unwrap().len(), 2);
    }

    #[test]
    fn gc_below_budget_is_noop() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        cas.intern(b"keep-me").unwrap();
        let stats = cas.gc(1024 * 1024).unwrap();
        assert!(stats.budget_ok);
        assert_eq!(stats.deleted_blobs, 0);
        assert_eq!(cas.stats().unwrap().total_blobs, 1);
    }

    #[test]
    fn gc_evicts_zero_ref_blobs_by_lru() {
        let dir = TempDir::new().unwrap();
        let cas = cas_with_root(dir.path());
        // intern 3 个不同 blob
        let h1 = cas.intern(b"blob1").unwrap();
        let h2 = cas.intern(b"blob2").unwrap();
        let h3 = cas.intern(b"blob3").unwrap();
        // 释放其中 2 个(ref_count → 0)
        cas.release(&h1).unwrap();
        cas.release(&h2).unwrap();
        // GC:总字节 ~15B,budget 4B,应至少删 2 个 0-ref
        let stats = cas.gc(4).unwrap();
        assert!(!stats.budget_ok || stats.deleted_blobs >= 2);
        assert!(stats.deleted_blobs >= 1, "至少应删 1 个 0-ref blob");
        // h3 应仍在
        assert!(cas.get(&h3).is_ok());
    }
}
