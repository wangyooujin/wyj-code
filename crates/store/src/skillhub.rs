//! SkillHub HTTP 技能注册表客户端：关键字搜索 + zip 下载 + 安全解压安装。
//!
//! 与 [`crate::marketplace`]（git clone 整个仓库再本地过滤）的区别：这里**不落地
//! 任何缓存仓库**，直接走注册表的 `/search` 与 `/download` 两个只读端点，因此
//! "搜一个关键字"不需要先 clone 几百 MB 的 git 仓库。
//!
//! 与 [`crate::registry`]（MCP Registry）的关系：两者是同一个 HTTP 客户端模式
//! 的两个实例，都刻意复刻 `RegistryClient` 的形状（`new(base_url)` + 一个
//! `search_*` 方法），以便可自建/私有部署。
//!
//! ## 接口契约（已对照真实响应核实）
//!
//! 搜索：`GET {base}/search?q=<keyword>&limit=<n>` → `{"results": [ ... ]}`。
//! **响应没有 total、没有 offset/游标**——`page`/`pageSize` 参数会被服务端静默
//! 忽略，只有 `limit` 生效。因此这里只做单页取回，不做分页；界面上的"没有更多"
//! 是接口本身的限制，不是 bug。
//!
//! 下载：`GET {base}/download?slug=<slug>&namespace=<handle>` → zip。**只支持
//! GET**，对同一 URL 发 HEAD 会得到 405，因此这里不能用 HEAD 探大小，改用
//! `Content-Length` 缺失时退回流式读取上限兜底。

use crate::lockfile::{self, InstallScope, InstalledManifest, InstalledSkillEntry, SkillSource};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// 公共实例的 API 根。自建部署可换成任意同构服务（`/search` + `/download`）。
pub const SKILLHUB_BASE_URL: &str = "https://api.skillhub.cn/api/v1";

const SEARCH_TIMEOUT_SECS: u64 = 20;
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;
const DEFAULT_SEARCH_LIMIT: u32 = 30;
/// 注册表对 limit 没有硬上限，但结果过多对 TUI 列表毫无意义，这里自行收敛。
const MAX_SEARCH_LIMIT: u32 = 100;
/// 下载体积上限。真实 skill 包通常几十 KB；1 MiB 已远超绝大多数场景。
const MAX_ARCHIVE_BYTES: usize = 8 * 1024 * 1024;
/// 解压后总字节上限，防止 zip bomb（压缩比攻击）。
const MAX_EXTRACTED_BYTES: u64 = 64 * 1024 * 1024;
/// 解压后文件数上限。
const MAX_ARCHIVE_ENTRIES: usize = 512;
/// 安装目录名长度上限。
const MAX_NAME_LEN: usize = 64;

// ── 搜索响应 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct SearchEnvelope {
    #[serde(default)]
    results: Vec<SearchItem>,
}

/// 单个搜索结果。字段大小写在真实响应里是混的（`displayName`/`canonicalName`
/// 是 camelCase，`owner_name`/`publicSlug` 一半一半），所以逐个显式 rename，
/// 不依赖 `rename_all` 规则。
#[derive(Debug, Clone, Deserialize)]
struct SearchItem {
    #[serde(default)]
    slug: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "description_zh")]
    description_zh: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    source: String,
    #[serde(default, rename = "owner_name")]
    owner_name: String,
    #[serde(default)]
    stars: u64,
    #[serde(default)]
    installs: u64,
    #[serde(default)]
    namespace: Option<SearchNamespace>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchNamespace {
    #[serde(default, rename = "canonicalName")]
    canonical_name: String,
    #[serde(default)]
    handle: String,
}

/// 面向 UI 的搜索结果（已从原始 JSON 归一化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHubSkill {
    pub slug: String,
    /// 展示名。优先 `displayName`，其次 `name`，最后退回 slug。
    pub display_name: String,
    /// 面向用户的中英文描述拼接结果（优先中文描述，因为产品面向中文用户）。
    pub description: String,
    pub version: String,
    /// `clawhub` / `community` / `enterprise`——第三方镜像与官方源的区分依据。
    pub source: String,
    /// namespace handle，下载接口用它定位。
    pub namespace: String,
    /// 完整限定名，如 `@clawhub_pskoett/self-improving-agent`。
    pub canonical_name: String,
    pub owner_name: String,
    pub stars: u64,
    pub installs: u64,
}

impl SkillHubSkill {
    /// 安装用的本地目录名。以 `slug` 为准而不是 `name`——真实响应里 `name`
    /// 常是 "self-improving agent" 这种带空格的人类可读名，不能直接当目录名。
    pub fn install_name(&self) -> String {
        sanitize_install_name(&self.slug)
    }
}

// ── 客户端 ──────────────────────────────────────────────────────────────────

pub struct SkillHubClient {
    http: Client,
    base_url: String,
}

impl SkillHubClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        // 搜索与下载的超时不同：搜索要快（阻塞 TUI 列表），下载可能慢（整包 zip）。
        let http = Client::builder()
            .user_agent(concat!("wyj-code/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self { http, base_url }
    }

    pub fn official() -> Self {
        Self::new(SKILLHUB_BASE_URL)
    }

    /// 关键字搜索。返回空 `Vec` 表示"注册表没匹配到"，与网络错误区分开——
    /// UI 上前者是提示，后者是错误行。
    pub async fn search(&self, keyword: &str, limit: u32) -> Result<Vec<SkillHubSkill>> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(SEARCH_TIMEOUT_SECS))
            .query(&[("q", keyword), ("limit", &limit.to_string())])
            .send()
            .await
            .with_context(|| format!("请求技能注册表失败: {url}"))?
            .error_for_status()
            .with_context(|| format!("技能注册表返回错误状态: {url}"))?;
        let envelope: SearchEnvelope = resp
            .json()
            .await
            .with_context(|| format!("解析技能注册表响应失败: {url}"))?;
        Ok(envelope.results.into_iter().map(normalize).collect())
    }

    /// 用默认 limit 搜索。
    pub async fn search_default(&self, keyword: &str) -> Result<Vec<SkillHubSkill>> {
        self.search(keyword, DEFAULT_SEARCH_LIMIT).await
    }

    /// 下载单个技能包（zip 原始字节）。
    pub async fn download(&self, slug: &str, namespace: &str) -> Result<Vec<u8>> {
        let url = format!("{}/download", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
            .query(&[("slug", slug), ("namespace", namespace)])
            .send()
            .await
            .with_context(|| format!("下载技能包失败: {slug}"))?
            .error_for_status()
            .with_context(|| format!("技能注册表返回错误状态: {slug}"))?;

        // 该端点不支持 HEAD，Content-Length 可能缺失，因此两种情况都要兜底：
        // 有长度则提前拒绝超限；没有则流式读取并在累计超限时中止。
        if let Some(len) = resp.content_length() {
            if len > MAX_ARCHIVE_BYTES as u64 {
                bail!("技能包体积超限（{len} 字节，上限 {MAX_ARCHIVE_BYTES}）");
            }
        }
        let mut buf = Vec::new();
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("读取技能包失败: {slug}"))?;
            if buf.len() + chunk.len() > MAX_ARCHIVE_BYTES {
                bail!("技能包体积超限（上限 {MAX_ARCHIVE_BYTES} 字节）");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

fn normalize(item: SearchItem) -> SkillHubSkill {
    let namespace = item.namespace.unwrap_or(SearchNamespace {
        canonical_name: String::new(),
        handle: String::new(),
    });
    let display_name = if !item.display_name.trim().is_empty() {
        item.display_name.trim().to_string()
    } else if !item.name.trim().is_empty() {
        item.name.trim().to_string()
    } else {
        item.slug.clone()
    };
    // 中文描述优先：这个特性面向中文用户，而英文 description 常是英文原句。
    let description = if !item.description_zh.trim().is_empty() {
        item.description_zh.trim().to_string()
    } else if !item.summary.trim().is_empty() {
        item.summary.trim().to_string()
    } else {
        item.description.trim().to_string()
    };
    let owner_name = if item.owner_name.trim().is_empty() {
        namespace.handle.clone()
    } else {
        item.owner_name.trim().to_string()
    };
    SkillHubSkill {
        slug: item.slug,
        canonical_name: if namespace.canonical_name.is_empty() {
            namespace.handle.clone()
        } else {
            namespace.canonical_name
        },
        display_name,
        description,
        version: item.version,
        source: item.source,
        namespace: namespace.handle,
        owner_name,
        stars: item.stars,
        installs: item.installs,
    }
}

// ── 包预览（安装前确认用）──────────────────────────────────────────────────

/// 解压**前**对包内容的摘要。安装确认界面要把这些展示给用户：技能包会往
/// `scripts/`、`hooks/` 里放可执行内容，用户在按下安装前必须知道。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillArchivePreview {
    pub file_count: usize,
    pub total_bytes: u64,
    /// 命中 `scripts/`、`hooks/`、`bin/` 等可执行目录的条目数。
    pub executable_count: usize,
    /// 命中的可执行文件路径（最多保留前若干个用于展示）。
    pub executable_paths: Vec<String>,
}

impl SkillArchivePreview {
    pub fn has_executables(&self) -> bool {
        self.executable_count > 0
    }
}

fn is_executable_path(path: &str) -> bool {
    const EXECUTABLE_DIRS: [&str; 4] = ["scripts/", "hooks/", "bin/", "exec/"];
    let lower = path.to_ascii_lowercase();
    EXECUTABLE_DIRS.iter().any(|dir| lower.starts_with(dir))
}

/// 只读扫描 zip，不落盘。
pub fn preview_archive(archive_bytes: &[u8]) -> Result<SkillArchivePreview> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("读取技能包失败")?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!(
            "技能包条目过多（{} 条，上限 {MAX_ARCHIVE_ENTRIES}）",
            archive.len()
        );
    }
    let mut preview = SkillArchivePreview::default();
    for i in 0..archive.len() {
        let file = archive.by_index(i).context("读取技能包条目失败")?;
        if file.is_dir() {
            continue;
        }
        let name = safe_entry_name(file.name())
            .with_context(|| format!("技能包含有非法路径条目: {}", file.name()))?;
        reject_symlink(&file, &name)?;
        preview.file_count += 1;
        preview.total_bytes = preview.total_bytes.saturating_add(file.size());
        if is_executable_path(&name) {
            preview.executable_count += 1;
            if preview.executable_paths.len() < 5 {
                preview.executable_paths.push(name);
            }
        }
    }
    Ok(preview)
}

// ── 安全解压 ────────────────────────────────────────────────────────────────

/// 校验 zip 条目名，拒绝绝对路径与 `..` 穿越，返回规范化相对路径。
///
/// `zip::ZipArchive::enclosed_name()` 也能防穿越，但它是**静默修正**（把 `../`
/// 剥掉后照常解压），这里要的是**明确拒绝**并告诉用户包有问题，所以自己判。
fn safe_entry_name(raw: &str) -> Result<String> {
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    if trimmed.is_empty() {
        bail!("技能包含有空路径条目");
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in trimmed.split('/') {
        match part {
            "" | "." => continue,
            ".." => bail!("技能包含有路径穿越条目: {raw}"),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        bail!("技能包含有无效路径条目: {raw}");
    }
    if Path::new(trimmed).is_absolute() || trimmed.contains(':') {
        bail!("技能包含有绝对路径条目: {raw}");
    }
    Ok(parts.join("/"))
}

/// 拒绝符号链接条目：解压出符号链接等于给后续写入开了「任意文件覆盖」的口子。
fn reject_symlink(file: &zip::read::ZipFile<'_>, name: &str) -> Result<()> {
    if let Some(mode) = file.unix_mode() {
        // 0o170000 是 unix 文件类型掩码，0o120000 是 S_IFLNK。
        if mode & 0o170000 == 0o120000 {
            bail!("技能包含有符号链接条目: {name}");
        }
    }
    Ok(())
}

/// 需要跳过的打包噪音（macOS `zip` 产物）。
fn is_noise_entry(name: &str) -> bool {
    name.starts_with("__MACOSX/") || name.ends_with("/.DS_Store") || name == ".DS_Store"
}

/// 把技能包解压到 `dest_dir`。
///
/// 拒绝项：路径穿越、绝对路径、符号链接、条目数/体积超限、缺 `SKILL.md`。
/// 调用方需保证 `dest_dir` 尚不存在或可整体替换——本函数不删除既有内容。
fn extract_archive(archive_bytes: &[u8], dest_dir: &Path) -> Result<SkillArchivePreview> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("读取技能包失败")?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!(
            "技能包条目过多（{} 条，上限 {MAX_ARCHIVE_ENTRIES}）",
            archive.len()
        );
    }

    let mut preview = SkillArchivePreview::default();
    let mut written: u64 = 0;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("读取技能包条目失败")?;
        if file.is_dir() {
            continue;
        }
        let raw_name = file.name().to_string();
        let name =
            safe_entry_name(&raw_name).with_context(|| format!("拒绝非法条目: {raw_name}"))?;
        if is_noise_entry(&name) {
            continue;
        }
        reject_symlink(&file, &name)?;

        let target =
            resolve_within(dest_dir, &name).with_context(|| format!("拒绝越界条目: {raw_name}"))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .context("读取技能包条目内容失败")?;
        written = written.saturating_add(buf.len() as u64);
        if written > MAX_EXTRACTED_BYTES {
            bail!("技能包解压后体积超限（上限 {MAX_EXTRACTED_BYTES} 字节）");
        }
        std::fs::write(&target, &buf)
            .with_context(|| format!("写入技能文件失败: {}", target.display()))?;

        preview.file_count += 1;
        preview.total_bytes = preview.total_bytes.saturating_add(buf.len() as u64);
        if is_executable_path(&name) {
            preview.executable_count += 1;
            if preview.executable_paths.len() < 5 {
                preview.executable_paths.push(name);
            }
        }
    }

    if !dest_dir.join("SKILL.md").is_file() {
        bail!("技能包内没有 SKILL.md，不是有效的技能包");
    }
    Ok(preview)
}

/// 把相对条目路径拼到 `base` 下，并**再确认一次**结果仍在 `base` 内部。
///
/// 前面的 `safe_entry_name` 已经挡掉了 `..` 与绝对路径；这里做第二道校验是因为
/// 「解压任意远程内容」这件事一旦出一次错就是任意文件写入，冗余是故意的。
fn resolve_within(base: &Path, relative: &str) -> Result<PathBuf> {
    // 判越界要看**相对部分**，不能看拼好之后的绝对路径——绝对路径必然含
    // `Component::RootDir`，拿它去比对会误判每一个正常条目。
    let rel = Path::new(relative);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("条目越界: {relative}");
    }
    let candidate = base.join(rel);
    if !normalize_lexically(&candidate).starts_with(normalize_lexically(base)) {
        bail!("条目越界: {relative}");
    }
    Ok(candidate)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ── 安装编排 ────────────────────────────────────────────────────────────────

pub struct SkillHubInstallRequest {
    pub base_url: String,
    pub slug: String,
    pub namespace: String,
    /// 本地安装目录名。为空时由 [`sanitize_install_name`] 从 slug 推导。
    pub install_name: String,
    pub version: String,
    pub scope: InstallScope,
    /// 展示给确认界面的来源描述，写入 lockfile 供后续升级/诊断使用。
    pub source_label: String,
}

fn skill_dir(scope: InstallScope, cwd: &Path) -> Result<PathBuf> {
    match scope {
        InstallScope::Global => Ok(wyj_config::config_dir()?.join("skills")),
        InstallScope::Project => Ok(wyj_config::project_config_dir(cwd).join("skills")),
    }
}

/// 把远程 slug 规整成本地目录名：只保留 `[A-Za-z0-9_-]`，其余（含 `.`、空格、
/// 路径分隔符）一律折叠成 `-`。
///
/// 不能直接用 slug——真实注册表里 slug 是受发布者控制的任意字符串，可能带 `/`、
/// `..`、空格甚至控制字符，而这个值最终会成为 `~/.wyj-code/skills/` 下的目录名。
/// 刻意**不保留 `.`**：一旦允许点号，`..`、`...` 这类串就能原样成为目录名，
/// 换来的是一层几乎无收益的兼容性。
pub fn sanitize_install_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    let capped: String = trimmed.chars().take(MAX_NAME_LEN).collect();
    if capped.is_empty() {
        "unnamed-skill".to_string()
    } else {
        capped
    }
}

/// 安装一个来自 HTTP 注册表的技能。返回解压后的包预览。
///
/// 安装是**全有或全无**：先解压到同级临时目录，校验通过后再 rename 到位；任何
/// 一步失败都清理临时目录，不在 skills 目录里留下半截内容。
pub fn install_skillhub_archive(
    archive_bytes: &[u8],
    req: &SkillHubInstallRequest,
    cwd: &Path,
) -> Result<SkillArchivePreview> {
    let install_name = if req.install_name.trim().is_empty() {
        sanitize_install_name(&req.slug)
    } else {
        sanitize_install_name(&req.install_name)
    };
    anyhow::ensure!(
        !install_name.is_empty() && install_name != "." && install_name != "..",
        "非法的技能安装名: {install_name}"
    );

    let dest_root = skill_dir(req.scope, cwd)?;
    let dest_path = dest_root.join(&install_name);

    // 已存在的同名目录：只有当它是我们自己装的（lockfile 里有纳管条目）才允许覆盖，
    // 否则可能覆盖用户手写的技能，必须让用户先卸载。
    let mut manifest = lockfile::load_scope(req.scope, cwd)?;
    let existing_entry = manifest
        .skills
        .iter()
        .find(|entry| entry.name == install_name)
        .cloned();
    let replacing_managed = existing_entry
        .as_ref()
        .map(|entry| entry.is_managed())
        .unwrap_or(false);
    anyhow::ensure!(
        !dest_path.exists() || replacing_managed,
        "技能 '{install_name}' 已存在且不是通过注册表安装的，请先卸载或换一个来源再试"
    );

    std::fs::create_dir_all(&dest_root)
        .with_context(|| format!("创建技能目录失败: {}", dest_root.display()))?;

    // 同级临时目录，保证最后的 rename 是同文件系统内的原子操作。
    let staging = dest_root.join(format!(
        ".{install_name}.staging-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = extract_archive(archive_bytes, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error).context("解压技能包失败");
    }

    // 备份旧目录以便失败回滚。
    let backup = if dest_path.exists() {
        let prev = dest_root.join(format!(
            ".{install_name}.backup-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::rename(&dest_path, &prev)
            .with_context(|| format!("备份已存在的技能目录失败: {}", dest_path.display()))?;
        Some(prev)
    } else {
        None
    };

    if let Err(error) = std::fs::rename(&staging, &dest_path) {
        // 换回旧目录，清理 staging。
        if let Some(prev) = &backup {
            let _ = std::fs::rename(prev, &dest_path);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error).context("技能目录替换失败，已回滚");
    }

    let now = Utc::now();
    let installed_at = existing_entry
        .as_ref()
        .map(|entry| entry.installed_at)
        .unwrap_or(now);
    let digest = format!("{:x}", Sha256::digest(archive_bytes));

    // 调用方（搜索结果）给的版本优先；没有就从包内 _meta.json 补。两者都没有就
    // 记 None——写空字符串会让升级逻辑永远判定「有新版本」，每次都重装一遍。
    let resolved_version = {
        let candidate = if req.version.trim().is_empty() {
            read_archive_version(archive_bytes).unwrap_or_default()
        } else {
            req.version.trim().to_string()
        };
        if candidate.is_empty() {
            None
        } else {
            Some(candidate)
        }
    };

    upsert_skill_entry(
        &mut manifest,
        InstalledSkillEntry {
            name: install_name.clone(),
            version: resolved_version.clone(),
            scope: req.scope,
            source: Some(SkillSource {
                marketplace_id: String::new(),
                marketplace_url: String::new(),
                entry_path: String::new(),
                registry: Some(lockfile::SkillRegistrySource {
                    base_url: req.base_url.clone(),
                    slug: req.slug.clone(),
                    namespace: req.namespace.clone(),
                }),
            }),
            enabled: true,
            installed_at,
            updated_at: now,
        },
    );
    lockfile::upsert_extension(
        &mut manifest,
        lockfile::ExtensionLockEntry {
            id: format!("skill:{install_name}"),
            kind: lockfile::ExtensionKind::Skill,
            scope: req.scope,
            source: Some(req.source_label.clone()),
            version: resolved_version.clone(),
            commit: None,
            digest: Some(digest),
            enabled: true,
            dependencies: Vec::new(),
            installed_at,
            updated_at: now,
        },
    );

    if let Err(error) = lockfile::save_scope(req.scope, cwd, &manifest) {
        // lockfile 写失败：把磁盘内容也回滚，避免「文件在但没记账」的幽灵状态。
        let _ = std::fs::remove_dir_all(&dest_path);
        if let Some(prev) = &backup {
            let _ = std::fs::rename(prev, &dest_path);
        }
        return Err(error).context("写入技能 lockfile 失败，已回滚安装");
    }

    // 记账成功后再删备份。
    if let Some(prev) = backup {
        let _ = std::fs::remove_dir_all(prev);
    }

    // preview 需要在 staging 被 rename 之后重建一次（这里直接重扫 zip，开销可忽略，
    // 避免为了拿 preview 提前解压一遍）。
    preview_archive(archive_bytes)
}

fn upsert_skill_entry(manifest: &mut InstalledManifest, entry: InstalledSkillEntry) {
    if let Some(existing) = manifest.skills.iter_mut().find(|e| e.name == entry.name) {
        *existing = entry;
    } else {
        manifest.skills.push(entry);
    }
}

/// 从注册表重新拉取并覆盖安装；版本没变则不动文件。
pub async fn upgrade_skillhub_skill(
    name: &str,
    scope: InstallScope,
    cwd: &Path,
) -> Result<crate::UpgradeOutcome> {
    let manifest = lockfile::load_scope(scope, cwd)?;
    let entry = manifest
        .skills
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("未找到已安装的 skill: {name}"))?;
    let source = entry
        .source
        .as_ref()
        .and_then(|source| source.registry.as_ref())
        .ok_or_else(|| anyhow::anyhow!("'{name}' 不是通过注册表安装的，无法在线升级"))?;

    let client = SkillHubClient::new(&source.base_url);
    let archive = client.download(&source.slug, &source.namespace).await?;
    let preview = preview_archive(&archive)?;

    // 版本号取自包内 `_meta.json`（注册表会在此写入真实版本）；取不到时退回
    // lockfile 里的旧值，避免把「未知」写回去让后续升级永远判定有新版本。
    let latest_version = read_archive_version(&archive).unwrap_or_default();

    if !latest_version.is_empty() && Some(latest_version.as_str()) == entry.version.as_deref() {
        return Ok(crate::UpgradeOutcome::AlreadyLatest {
            version: latest_version,
        });
    }

    let _ = preview;
    install_skillhub_archive(
        &archive,
        &SkillHubInstallRequest {
            base_url: source.base_url.clone(),
            slug: source.slug.clone(),
            namespace: source.namespace.clone(),
            install_name: name.to_string(),
            // 交给 install_skillhub_archive 去 _meta.json 解析，这里传空串。
            version: String::new(),
            scope,
            source_label: format!("{}:{}", source.base_url, source.slug),
        },
        cwd,
    )?;
    let upgraded_to = read_archive_version(&archive).unwrap_or(latest_version);
    Ok(crate::UpgradeOutcome::Upgraded {
        version: upgraded_to,
    })
}

/// 读取包内 `_meta.json` 的 `version` 字段（若存在）。
fn read_archive_version(archive_bytes: &[u8]) -> Option<String> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).ok()?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        if file.name() != "_meta.json" {
            continue;
        }
        let mut buf = String::new();
        file.read_to_string(&mut buf).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&buf).ok()?;
        return meta
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // 真实搜索响应形状（已对照 api.skillhub.cn 的实际返回核实）：
    // 注意 displayName/canonicalName 是 camelCase，owner_name 是 snake_case，
    // 且 `name` 常是带空格的人类可读名而非 slug。
    const SAMPLE_SEARCH: &str = r#"
    {
      "results": [
        {
          "slug": "self-improving-agent",
          "name": "self-improving agent",
          "displayName": "self-improving agent",
          "description": "A skill where the agent logs its own findings",
          "description_zh": "记录自身发现以实现自我改进的技能",
          "summary": "A skill where the agent logs it's own findings for self-improvement\n记录自身发现以实现自我改进的技能",
          "version": "3.0.24",
          "source": "clawhub",
          "owner_name": "pskoett",
          "stars": 4528,
          "installs": 86140,
          "downloads": 1359042,
          "namespace": {
            "canonicalName": "@clawhub_pskoett/self-improving-agent",
            "displayName": "pskoett",
            "handle": "clawhub_pskoett",
            "publicSlug": "self-improving-agent"
          }
        },
        {
          "slug": "minimal",
          "description": "only description, no displayName/name",
          "source": "community",
          "namespace": { "handle": "someone" }
        }
      ]
    }
    "#;

    fn parse_search(json: &str) -> Vec<SkillHubSkill> {
        let envelope: SearchEnvelope = serde_json::from_str(json).unwrap();
        envelope.results.into_iter().map(normalize).collect()
    }

    #[test]
    fn parses_real_search_shape_and_prefers_chinese_description() {
        let results = parse_search(SAMPLE_SEARCH);
        assert_eq!(results.len(), 2);

        let first = &results[0];
        assert_eq!(first.slug, "self-improving-agent");
        assert_eq!(first.display_name, "self-improving agent");
        assert_eq!(first.description, "记录自身发现以实现自我改进的技能");
        assert_eq!(first.version, "3.0.24");
        assert_eq!(first.source, "clawhub");
        // namespace.handle 是下载接口要的参数，不是 canonicalName。
        assert_eq!(first.namespace, "clawhub_pskoett");
        assert_eq!(
            first.canonical_name,
            "@clawhub_pskoett/self-improving-agent"
        );
        assert_eq!(first.owner_name, "pskoett");
        assert_eq!(first.stars, 4528);
    }

    #[test]
    fn missing_optional_fields_fall_back_without_panicking() {
        let results = parse_search(SAMPLE_SEARCH);
        let second = &results[1];
        // 没有 displayName/name → 退回 slug
        assert_eq!(second.display_name, "minimal");
        // 没有 summary/description_zh → 退回 description
        assert_eq!(second.description, "only description, no displayName/name");
        assert_eq!(second.namespace, "someone");
        // namespace 存在但没有 canonicalName → 退回 handle
        assert_eq!(second.canonical_name, "someone");
        assert_eq!(second.version, "");
    }

    #[test]
    fn namespace_entirely_absent_is_tolerated() {
        let results = parse_search(r#"{"results":[{"slug":"bare"}]}"#);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].namespace, "");
        assert_eq!(results[0].canonical_name, "");
    }

    #[test]
    fn empty_results_envelope_is_not_an_error() {
        assert!(parse_search(r#"{"results":[]}"#).is_empty());
        // 服务端连 results 字段都没给时也应视为空列表而不是解析失败。
        assert!(parse_search(r#"{}"#).is_empty());
    }

    #[test]
    fn install_name_uses_slug_not_display_name() {
        let results = parse_search(SAMPLE_SEARCH);
        // displayName 带空格，不能当目录名。
        assert_eq!(results[0].install_name(), "self-improving-agent");
    }

    #[test]
    fn sanitize_install_name_strips_path_traversal_and_odd_chars() {
        assert_eq!(sanitize_install_name("finance"), "finance");
        assert_eq!(sanitize_install_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_install_name("my skill!"), "my-skill");
        assert_eq!(sanitize_install_name("a/b\\c"), "a-b-c");
        // 点号一律折叠：`.`/`..` 绝不能原样成为目录名。
        assert_eq!(sanitize_install_name("..."), "unnamed-skill");
        assert_eq!(sanitize_install_name("v1.2.3"), "v1-2-3");
        assert_eq!(sanitize_install_name(""), "unnamed-skill");
        assert_eq!(sanitize_install_name(".."), "unnamed-skill");
        // 超长名字被截断，不会撑爆路径。
        assert_eq!(sanitize_install_name(&"x".repeat(200)).len(), MAX_NAME_LEN);
    }

    // ── zip 构造helper ────────────────────────────────────────────────────

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, content) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extracts_a_realistic_skill_package() {
        let archive = build_zip(&[
            ("SKILL.md", b"---\nname: demo\ndescription: d\n---\nbody"),
            ("references/api.md", b"# api"),
            ("scripts/run.sh", b"#!/bin/sh\necho hi"),
            ("_meta.json", br#"{"version":"1.2.3"}"#),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let preview = extract_archive(&archive, dir.path()).unwrap();

        assert_eq!(preview.file_count, 4);
        // scripts/ 与 references/：只有 scripts 算可执行。
        assert_eq!(preview.executable_count, 1);
        assert_eq!(preview.executable_paths, vec!["scripts/run.sh"]);
        assert!(dir.path().join("SKILL.md").is_file());
        assert!(dir.path().join("references/api.md").is_file());
        assert_eq!(read_archive_version(&archive).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn rejects_path_traversal_entries() {
        let archive = build_zip(&[("SKILL.md", b"ok"), ("../../../../tmp/pwned.txt", b"bad")]);
        let dir = tempfile::tempdir().unwrap();
        let err = extract_archive(&archive, dir.path()).unwrap_err();
        // `{err:#}` 打出完整 cause 链——`.to_string()` 只会拿到最外层 context，
        // 看不到「路径穿越」这个真正的拒绝理由。
        let message = format!("{err:#}");
        assert!(
            message.contains("穿越") || message.contains("越界"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn rejects_absolute_path_entries() {
        let archive = build_zip(&[("/etc/pwned", b"bad")]);
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_archive(&archive, dir.path()).is_err());
    }

    #[test]
    fn rejects_symlink_entries() {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file("SKILL.md", options).unwrap();
            writer.write_all(b"ok").unwrap();
            // unix mode 0o120777 = 符号链接
            let opts = options.unix_permissions(0o777);
            let _ = &opts;
            writer
                .add_symlink("evil-link", "/etc/passwd", options)
                .unwrap();
            writer.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let err = extract_archive(&buf, dir.path()).unwrap_err();
        assert!(err.to_string().contains("符号链接"), "unexpected: {err}");
    }

    #[test]
    fn rejects_package_without_skill_md() {
        let archive = build_zip(&[("README.md", b"nothing here")]);
        let dir = tempfile::tempdir().unwrap();
        let err = extract_archive(&archive, dir.path()).unwrap_err();
        assert!(err.to_string().contains("SKILL.md"), "unexpected: {err}");
    }

    #[test]
    fn skips_macos_packaging_noise() {
        let archive = build_zip(&[
            ("SKILL.md", b"ok"),
            ("__MACOSX/._SKILL.md", b"junk"),
            (".DS_Store", b"junk"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let preview = extract_archive(&archive, dir.path()).unwrap();
        assert_eq!(preview.file_count, 1, "打包噪音不应计入预览");
    }

    #[test]
    fn resolve_within_rejects_escaping_paths() {
        let base = Path::new("/tmp/skills/demo");
        assert!(resolve_within(base, "references/a.md").is_ok());
        assert!(resolve_within(base, "../evil.md").is_err());
        assert!(resolve_within(base, "a/../../evil.md").is_err());
    }

    #[test]
    fn preview_does_not_write_anything_to_disk() {
        let archive = build_zip(&[("SKILL.md", b"ok"), ("scripts/x.sh", b"#!/bin/sh")]);
        let dir = tempfile::tempdir().unwrap();
        let preview = preview_archive(&archive).unwrap();
        assert_eq!(preview.file_count, 2);
        assert!(preview.has_executables());
        // 只读扫描：目标目录应保持为空。
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn install_writes_directory_skill_and_records_lockfile() {
        let archive = build_zip(&[
            ("SKILL.md", b"---\nname: demo\ndescription: d\n---\nbody"),
            ("references/api.md", b"# api"),
        ]);
        let project = tempfile::tempdir().unwrap();
        let req = SkillHubInstallRequest {
            base_url: SKILLHUB_BASE_URL.to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: "1.0.0".to_string(),
            scope: InstallScope::Project,
            source_label: "skillhub:someone/demo".to_string(),
        };
        install_skillhub_archive(&archive, &req, project.path()).unwrap();

        let dest = project.path().join(".wyj-code/skills/demo");
        assert!(dest.join("SKILL.md").is_file());
        // 目录式技能：references 必须一起落地，否则技能正文里的引用路径全断。
        assert!(dest.join("references/api.md").is_file());

        let manifest = lockfile::load_project(project.path()).unwrap();
        assert_eq!(manifest.skills.len(), 1);
        assert_eq!(manifest.skills[0].name, "demo");
        let source = manifest.skills[0].source.as_ref().unwrap();
        assert!(source.is_registry());
        assert_eq!(source.registry.as_ref().unwrap().slug, "demo");
        assert_eq!(
            source.marketplace_url, "",
            "注册表来源不应伪造 marketplace 字段"
        );
    }

    /// 命令行直接安装时 `req.version` 是空的（没有搜索结果可参考），
    /// 版本必须能从包内 `_meta.json` 补齐——否则 lockfile 里会是空字符串，
    /// 升级逻辑每次都会判定「有新版本」而重复安装。
    #[test]
    fn install_falls_back_to_meta_json_version_when_caller_has_none() {
        let archive = build_zip(&[
            ("SKILL.md", b"---\nname: demo\n---\nbody"),
            ("_meta.json", br#"{"version":"4.5.6"}"#),
        ]);
        let project = tempfile::tempdir().unwrap();
        let req = SkillHubInstallRequest {
            base_url: SKILLHUB_BASE_URL.to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: String::new(),
            scope: InstallScope::Project,
            source_label: "skillhub".to_string(),
        };
        install_skillhub_archive(&archive, &req, project.path()).unwrap();

        let manifest = lockfile::load_project(project.path()).unwrap();
        assert_eq!(manifest.skills[0].version.as_deref(), Some("4.5.6"));
        assert_eq!(manifest.extensions[0].version.as_deref(), Some("4.5.6"));
    }

    /// 包里既没有调用方版本也没有 _meta.json 版本时，lockfile 记 None 而不是空串。
    #[test]
    fn install_records_no_version_when_the_package_declares_none() {
        let archive = build_zip(&[("SKILL.md", b"---\nname: demo\n---\nbody")]);
        let project = tempfile::tempdir().unwrap();
        let req = SkillHubInstallRequest {
            base_url: SKILLHUB_BASE_URL.to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: String::new(),
            scope: InstallScope::Project,
            source_label: "skillhub".to_string(),
        };
        install_skillhub_archive(&archive, &req, project.path()).unwrap();
        let manifest = lockfile::load_project(project.path()).unwrap();
        assert_eq!(manifest.skills[0].version, None);
    }

    #[test]
    fn install_refuses_to_clobber_a_handwritten_skill() {
        let archive = build_zip(&[("SKILL.md", b"remote")]);
        let project = tempfile::tempdir().unwrap();
        let skills = project.path().join(".wyj-code/skills/demo");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("SKILL.md"), b"handwritten").unwrap();

        let req = SkillHubInstallRequest {
            base_url: SKILLHUB_BASE_URL.to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: "1.0.0".to_string(),
            scope: InstallScope::Project,
            source_label: "skillhub".to_string(),
        };
        let err = install_skillhub_archive(&archive, &req, project.path()).unwrap_err();
        assert!(err.to_string().contains("已存在"), "unexpected: {err}");
        // 原有内容必须原样保留。
        assert_eq!(
            std::fs::read(skills.join("SKILL.md")).unwrap(),
            b"handwritten"
        );
    }

    #[test]
    fn failed_install_leaves_no_staging_directory_behind() {
        // 缺 SKILL.md 的包会在解压阶段失败，此时不应留下 .staging-* 残留。
        let archive = build_zip(&[("README.md", b"nope")]);
        let project = tempfile::tempdir().unwrap();
        let req = SkillHubInstallRequest {
            base_url: SKILLHUB_BASE_URL.to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: "1.0.0".to_string(),
            scope: InstallScope::Project,
            source_label: "skillhub".to_string(),
        };
        assert!(install_skillhub_archive(&archive, &req, project.path()).is_err());
        let entries: Vec<_> = std::fs::read_dir(project.path().join(".wyj-code/skills"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries.is_empty(), "残留文件: {entries:?}");
    }
}
