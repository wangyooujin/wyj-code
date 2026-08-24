//! `wyj-code update` 的纯逻辑层：查询 GitHub Release、版本比较、下载校验、
//! 解压、原子替换当前正在运行的可执行文件。不做任何 `println!`/交互式
//! stdin 读取——所有用户可见文案与确认流程留给 `crates/cli` 编排。
//!
//! 与本 crate 其余模块（`registry`/`marketplace`）"下载安装类"配置数据不同，
//! 这里替换的是 wyj-code 自身的可执行文件，不涉及 shell out 安装第三方依赖。

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const GITHUB_OWNER: &str = "wangyooujin";
pub const GITHUB_REPO: &str = "wyj-code";
const BINARY_NAME: &str = "wyj-code";

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    #[serde(default)]
    pub body: String,
    pub assets: Vec<ReleaseAsset>,
}

impl ReleaseInfo {
    /// 去掉 tag 的 `v` 前缀得到裸版本号（asset 文件名里用的就是这个）。
    pub fn version(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }

    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReleaseAssets<'a> {
    pub archive: &'a ReleaseAsset,
    pub checksum: &'a ReleaseAsset,
}

/// 查询 GitHub 最新正式 Release（`/releases/latest` 已自动排除 draft/prerelease）。
pub async fn fetch_latest_release(client: &reqwest::Client) -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .with_context(|| format!("请求 GitHub Release 失败: {url}"))?
        .error_for_status()
        .with_context(|| format!("GitHub Release 接口返回错误状态: {url}"))?;
    resp.json::<ReleaseInfo>()
        .await
        .with_context(|| format!("解析 GitHub Release 响应失败: {url}"))
}

/// `remote_version` 是否比 `current_version` 新；两者相等或本地领先（开发中
/// 版本号已经 bump 但还没打 tag）一律视为"已是最新"，不报"需要更新"。
pub fn is_newer(remote_version: &str, current_version: &str) -> Result<bool> {
    let remote = semver::Version::parse(remote_version)
        .with_context(|| format!("无法解析远端版本号: {remote_version}"))?;
    let current = semver::Version::parse(current_version)
        .with_context(|| format!("无法解析本地版本号: {current_version}"))?;
    Ok(remote > current)
}

/// 映射到 `.github/workflows/release.yml` 构建矩阵里的 5 个 target triple 之一。
/// 不认识的 OS/ARCH 组合返回 `None`，调用方应引导用户去 Releases 页面手动下载。
pub fn current_target() -> Option<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

fn is_windows_target(target: &str) -> bool {
    target.contains("windows")
}

/// 返回 (归档文件名, sha256 sidecar 文件名)。注意：文件名里的版本号**不带** `v`
/// 前缀（release.yml 里 `VERSION="${GITHUB_REF_NAME#v}"`），而下载 tag 路径带 `v`。
pub fn asset_names(version: &str, target: &str) -> (String, String) {
    let ext = if is_windows_target(target) {
        "zip"
    } else {
        "tar.gz"
    };
    let archive = format!("{BINARY_NAME}-{version}-{target}.{ext}");
    let sha256 = format!("{archive}.sha256");
    (archive, sha256)
}

/// 找到当前平台的安装包与校验和资产。新 release 会上传独立 `.sha256`
/// sidecar；为兼容已经只上传 `SHA256SUMS` 的 release，也接受合并校验和文件。
pub fn release_assets<'a>(
    release: &'a ReleaseInfo,
    version: &str,
    target: &str,
) -> Option<ReleaseAssets<'a>> {
    let (archive_name, sha256_name) = asset_names(version, target);
    let archive = release.asset(&archive_name)?;
    let checksum = release
        .asset(&sha256_name)
        .or_else(|| release.asset("SHA256SUMS"))?;
    Some(ReleaseAssets { archive, checksum })
}

/// 下载归档 + 校验和文件，校验一致后返回归档原始字节。
///
/// `softprops/action-gh-release` 在所有 asset upload 完成前就会 publish
/// release metadata,所以刚 release 完跑 `wyj-code update` 经常撞上
/// "release 已存在但 archive / sha256 还没出现"的窗口,GitHub 返回 404。
/// 这里对 archive 和 sha256 都做指数退避 retry(5s / 10s / 20s),覆盖 CI
/// 收尾的最后阶段;非 4xx/5xx 的瞬时网络错误也复用同一条 retry 路径。
pub async fn download_and_verify(
    client: &reqwest::Client,
    archive_url: &str,
    sha256_url: &str,
    archive_name: &str,
) -> Result<Vec<u8>> {
    const MAX_RETRIES: u32 = 3;
    const INITIAL_BACKOFF_SECS: u64 = 5;
    let archive_bytes = download_with_retry(
        client,
        archive_url,
        MAX_RETRIES,
        INITIAL_BACKOFF_SECS,
        "下载安装包",
    )
    .await?;
    let sha256_text = download_with_retry(
        client,
        sha256_url,
        MAX_RETRIES,
        INITIAL_BACKOFF_SECS,
        "下载校验和文件",
    )
    .await?;

    let sha256_text = std::str::from_utf8(&sha256_text)
        .with_context(|| format!("下载校验和文件不是合法 UTF-8: {sha256_url}"))?;
    verify_sha256(&archive_bytes, sha256_text, archive_name)?;
    Ok(archive_bytes.to_vec())
}

/// 下载一个文件并把字节/文本返回。404 / 429 / 5xx 走指数退避 retry,
/// 其它 HTTP 错误一次性报错。`action_label` 用于错误文案(如"下载安装包")。
/// `initial_backoff_secs` 暴露出来便于测试用 0 跳过等待,prod 固定 5s 起步。
async fn download_with_retry(
    client: &reqwest::Client,
    url: &str,
    max_retries: u32,
    initial_backoff_secs: u64,
    action_label: &str,
) -> Result<Vec<u8>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay_secs = initial_backoff_secs << (attempt - 1);
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        }
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err =
                    Some(anyhow::Error::new(e).context(format!("{action_label} 网络错误: {url}")));
                continue;
            }
        };
        let status = resp.status();
        // 404 / 429 / 5xx 视为可重试;其它 4xx 直接报错。
        if status.as_u16() == 404 || status.as_u16() == 429 || status.is_server_error() {
            last_err = Some(anyhow!(
                "{action_label} 返回可重试状态 {status}: {url} (第 {}/{} 次)",
                attempt + 1,
                max_retries
            ));
            continue;
        }
        if !status.is_success() {
            return Err(anyhow!("{action_label} 返回错误状态 {status}: {url}"));
        }
        return resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .with_context(|| format!("读取 {action_label} 内容失败: {url}"));
    }
    Err(last_err.unwrap_or_else(|| anyhow!("{action_label} 失败,已重试 {max_retries} 次: {url}")))
}

/// `sha256_text` 可以是独立 sidecar，也可以是包含多平台条目的 `SHA256SUMS`。
fn verify_sha256(bytes: &[u8], sha256_text: &str, archive_name: &str) -> Result<()> {
    let expected = expected_sha256(sha256_text, archive_name)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex_encode(&hasher.finalize());
    if actual != expected {
        bail!("校验和不匹配：期望 {expected}，实际 {actual}");
    }
    Ok(())
}

fn expected_sha256(sha256_text: &str, archive_name: &str) -> Result<String> {
    let mut bare_single_hash = None;
    let mut nonempty_count = 0usize;

    for line in sha256_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        nonempty_count += 1;

        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let filename = parts.next();
        match filename {
            Some(name) if checksum_filename_matches(name, archive_name) => {
                return Ok(hash.to_lowercase());
            }
            Some(_) => {}
            None => {
                bare_single_hash = Some(hash.to_lowercase());
            }
        }
    }

    if nonempty_count == 1 {
        if let Some(hash) = bare_single_hash {
            return Ok(hash);
        }
    }

    if nonempty_count == 0 {
        bail!("校验和文件内容为空或格式不正确");
    }
    bail!("校验和文件未包含 {archive_name}")
}

fn checksum_filename_matches(name: &str, archive_name: &str) -> bool {
    let name = name.trim_start_matches('*');
    Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == archive_name)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 从下载的归档字节中取出 wyj-code 二进制。unix 归档是 `.tar.gz`，内部嵌了一层
/// `{binary}-{version}-{target}/` 目录；windows 是 `.zip`，内含 `wyj-code.exe`。
pub fn extract_binary(archive_bytes: &[u8], target: &str) -> Result<Vec<u8>> {
    if is_windows_target(target) {
        extract_from_zip(archive_bytes)
    } else {
        extract_from_tar_gz(archive_bytes)
    }
}

fn extract_from_tar_gz(archive_bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("读取 tar.gz 归档失败")? {
        let mut entry = entry.context("读取 tar.gz 归档条目失败")?;
        let path = entry.path().context("读取 tar.gz 条目路径失败")?;
        if path.file_name().and_then(|n| n.to_str()) == Some(BINARY_NAME) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).context("读取二进制内容失败")?;
            return Ok(buf);
        }
    }
    bail!("归档内未找到 {BINARY_NAME} 二进制")
}

fn extract_from_zip(archive_bytes: &[u8]) -> Result<Vec<u8>> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("读取 zip 归档失败")?;
    let target_name = format!("{BINARY_NAME}.exe");
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("读取 zip 归档条目失败")?;
        let matches = Path::new(file.name()).file_name().and_then(|n| n.to_str())
            == Some(target_name.as_str());
        if matches {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).context("读取二进制内容失败")?;
            return Ok(buf);
        }
    }
    bail!("归档内未找到 {target_name} 二进制")
}

/// 原子替换当前正在运行的可执行文件，返回被替换的路径。
///
/// unix：在同目录内 `tempfile` 写入新二进制 + `chmod +x`，再 `rename` 到位——
/// 照搬 `build.sh install_local()` 已经踩过坑的手法：原地覆盖正在运行进程占用
/// 的可执行文件，在 macOS 上会让内核对该 vnode 的代码签名校验失效，导致后续
/// `exec` 该路径的新进程被 SIGKILL；`rename` 到新 inode 不会有这个问题。
///
/// windows：不能直接覆盖到运行中 exe 的原名（会被文件锁挡住），改用"先把自己
/// 挪开、再把新文件挪进来"的标准手法（Windows 允许重命名一个正在执行的文件，
/// 但不允许原地删除/覆盖）。
pub fn atomic_replace_current_exe(new_binary: &[u8]) -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .context("无法定位当前可执行文件路径")?
        .canonicalize()
        .context("无法解析当前可执行文件的真实路径")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("当前可执行文件没有父目录: {}", exe.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("无法在 {} 下创建临时文件", dir.display()))?;
    tmp.write_all(new_binary).context("写入新二进制内容失败")?;
    tmp.flush().context("刷新临时文件失败")?;
    set_executable(tmp.path())?;
    let tmp_path = tmp.into_temp_path();

    replace_exe(&tmp_path, &exe)?;
    // 已成功 rename，临时文件的生命周期管理交给上面 replace_exe 内的逻辑，
    // 这里避免 TempPath 的 Drop 尝试删除一个已经不存在的路径。
    std::mem::forget(tmp_path);

    Ok(exe)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)
        .with_context(|| format!("读取 {} 权限失败", path.display()))?
        .permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm)
        .with_context(|| format!("设置 {} 可执行权限失败", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_exe(tmp: &Path, exe: &Path) -> Result<()> {
    std::fs::rename(tmp, exe).with_context(|| format!("替换 {} 失败", exe.display()))
}

#[cfg(windows)]
fn replace_exe(tmp: &Path, exe: &Path) -> Result<()> {
    let old = windows_old_path(exe);
    // 上一次更新可能残留的 .old 文件（当时删除失败），尝试清理，忽略失败。
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).with_context(|| format!("将运行中的 {} 移开失败", exe.display()))?;
    std::fs::rename(tmp, exe).with_context(|| format!("替换 {} 失败", exe.display()))?;
    // best-effort：大概率仍被当前进程占用而删除失败，忽略——已知限制，
    // 残留的 .old 文件要到下次系统重启或用户手动清理才会消失。
    let _ = std::fs::remove_file(&old);
    Ok(())
}

#[cfg(windows)]
fn windows_old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver() {
        assert!(is_newer("1.1.0", "1.0.1").unwrap());
        assert!(!is_newer("1.0.1", "1.0.1").unwrap());
        assert!(!is_newer("1.0.0", "1.0.1").unwrap());
        assert!(is_newer("2.0.0", "1.9.9").unwrap());
        assert!(is_newer_invalid_errors());
    }

    fn is_newer_invalid_errors() -> bool {
        is_newer("not-a-version", "1.0.1").is_err()
    }

    #[test]
    fn target_for_known_platforms() {
        assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(
            target_for("linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            target_for("linux", "aarch64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            target_for("windows", "x86_64"),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(target_for("freebsd", "x86_64"), None);
        assert_eq!(target_for("linux", "arm"), None);
    }

    #[test]
    fn asset_names_unix_vs_windows() {
        let (archive, sha) = asset_names("1.0.1", "x86_64-apple-darwin");
        assert_eq!(archive, "wyj-code-1.0.1-x86_64-apple-darwin.tar.gz");
        assert_eq!(sha, "wyj-code-1.0.1-x86_64-apple-darwin.tar.gz.sha256");

        let (archive, sha) = asset_names("1.0.1", "x86_64-pc-windows-msvc");
        assert_eq!(archive, "wyj-code-1.0.1-x86_64-pc-windows-msvc.zip");
        assert_eq!(sha, "wyj-code-1.0.1-x86_64-pc-windows-msvc.zip.sha256");
    }

    #[test]
    fn verify_sha256_matches_and_rejects() {
        let bytes = b"hello wyj-code";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hex = hex_encode(&hasher.finalize());
        let sidecar = format!("{hex}  wyj-code-1.0.1-x86_64-apple-darwin.tar.gz\n");
        assert!(
            verify_sha256(bytes, &sidecar, "wyj-code-1.0.1-x86_64-apple-darwin.tar.gz").is_ok()
        );

        let bad_sidecar = format!("{}  wyj-code.tar.gz\n", "0".repeat(64));
        assert!(verify_sha256(
            bytes,
            &bad_sidecar,
            "wyj-code-1.0.1-x86_64-apple-darwin.tar.gz"
        )
        .is_err());
    }

    #[test]
    fn verify_sha256_selects_matching_entry_from_combined_file() {
        let bytes = b"hello wyj-code";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hex = hex_encode(&hasher.finalize());
        let sums = format!(
            "{}  wyj-code-1.0.1-x86_64-unknown-linux-musl.tar.gz\n{hex}  wyj-code-1.0.1-aarch64-apple-darwin.tar.gz\n",
            "0".repeat(64)
        );

        assert!(verify_sha256(bytes, &sums, "wyj-code-1.0.1-aarch64-apple-darwin.tar.gz").is_ok());
    }

    #[test]
    fn release_assets_falls_back_to_combined_checksums() {
        let archive = ReleaseAsset {
            name: "wyj-code-1.0.1-aarch64-apple-darwin.tar.gz".to_string(),
            browser_download_url: "https://example.com/archive".to_string(),
        };
        let sums = ReleaseAsset {
            name: "SHA256SUMS".to_string(),
            browser_download_url: "https://example.com/SHA256SUMS".to_string(),
        };
        let release = ReleaseInfo {
            tag_name: "v1.0.1".to_string(),
            body: String::new(),
            assets: vec![archive.clone(), sums.clone()],
        };

        let assets = release_assets(&release, "1.0.1", "aarch64-apple-darwin").unwrap();
        assert_eq!(assets.archive.name, archive.name);
        assert_eq!(assets.checksum.name, sums.name);
    }

    #[test]
    fn release_assets_prefers_sidecar_checksum() {
        let archive = ReleaseAsset {
            name: "wyj-code-1.0.1-aarch64-apple-darwin.tar.gz".to_string(),
            browser_download_url: "https://example.com/archive".to_string(),
        };
        let sidecar = ReleaseAsset {
            name: "wyj-code-1.0.1-aarch64-apple-darwin.tar.gz.sha256".to_string(),
            browser_download_url: "https://example.com/sidecar".to_string(),
        };
        let sums = ReleaseAsset {
            name: "SHA256SUMS".to_string(),
            browser_download_url: "https://example.com/SHA256SUMS".to_string(),
        };
        let release = ReleaseInfo {
            tag_name: "v1.0.1".to_string(),
            body: String::new(),
            assets: vec![archive.clone(), sums, sidecar.clone()],
        };

        let assets = release_assets(&release, "1.0.1", "aarch64-apple-darwin").unwrap();
        assert_eq!(assets.archive.name, archive.name);
        assert_eq!(assets.checksum.name, sidecar.name);
    }

    #[test]
    fn extract_binary_from_tar_gz() {
        let payload = b"fake binary contents";
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "wyj-code-1.0.1-x86_64-apple-darwin/wyj-code",
                    &payload[..],
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, &tar_bytes).unwrap();
            encoder.finish().unwrap();
        }

        let extracted = extract_from_tar_gz(&gz_bytes).unwrap();
        assert_eq!(extracted, payload);
    }

    #[test]
    fn extract_binary_from_zip() {
        let payload = b"fake windows binary";
        let mut zip_bytes = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_bytes);
            let mut writer = zip::ZipWriter::new(cursor);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            writer
                .start_file(
                    "wyj-code-1.0.1-x86_64-pc-windows-msvc/wyj-code.exe",
                    options,
                )
                .unwrap();
            std::io::Write::write_all(&mut writer, payload).unwrap();
            writer.finish().unwrap();
        }

        let extracted = extract_from_zip(&zip_bytes).unwrap();
        assert_eq!(extracted, payload);
    }

    #[test]
    fn release_info_version_strips_v_prefix() {
        let info = ReleaseInfo {
            tag_name: "v1.2.3".to_string(),
            body: String::new(),
            assets: vec![],
        };
        assert_eq!(info.version(), "1.2.3");
    }

    /// 404 是 `softprops/action-gh-release` 在 release metadata 已 publish 但
    /// archive / sha256 还没 upload 完成时 GitHub 返回的状态。retry 必须吃掉这个
    /// race window,最终成功。`initial_backoff_secs=0` 让测试不需要等 5s。
    #[tokio::test]
    async fn download_with_retry_succeeds_after_initial_404() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let n = hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = if n == 0 {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 256];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/archive.tar.gz");
        let bytes = download_with_retry(&client, &url, 3, 0, "test")
            .await
            .expect("404 后第二次返回 200 应最终成功");
        assert_eq!(bytes, b"hello");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "应恰好 2 次 HTTP 请求:首次 404,重试 200"
        );
        server.abort();
    }

    /// 持续 404 / 5xx 时,retry 耗尽应报错,而不是无限循环。
    #[tokio::test]
    async fn download_with_retry_gives_up_after_max_retries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 256];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/archive.tar.gz");
        let err = download_with_retry(&client, &url, 3, 0, "test")
            .await
            .expect_err("持续 404 必须失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("第 3/3") || msg.contains("第 2/3"),
            "错误信息应反映已重试到第 N 次: {msg}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "应恰好 3 次 HTTP 请求(MAX_RETRIES=3)"
        );
        server.abort();
    }

    /// 端到端：download_and_verify 内部已经分别调 download_with_retry
    /// 处理 archive 和 sha256。我们用 listener 直接 mock 一对 HTTP endpoint,
    /// 第一次 GET /archive 返回 404、第二次返回真实 tar 字节(校验和不匹配的
    /// archive 应被 verify_sha256 拒绝);sha256 第一次就返回正确 sidecar,
    /// 验证整体流程在 archive retry 后能拉到正确内容并通过校验。
    /// 这里不直接调用 download_and_verify(因为它 hardcode 5s backoff),
    /// 而是分别验证 archive 与 sha256 各自的 retry 行为已通过上面两个
    /// download_with_retry 测试覆盖,验证 sha256 单点路径(没有 retry 触发)。
    #[tokio::test]
    async fn download_with_retry_returns_sha256_sidecar_on_first_try() {
        let sidecar = "abc123  wyj-code-1.5.7.tar.gz\n";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sidecar_clone = sidecar.to_string();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = sidecar_clone.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 256];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/archive.tar.gz.sha256");
        let bytes = download_with_retry(&client, &url, 3, 0, "test")
            .await
            .expect("首次 200 应直接成功");
        assert_eq!(bytes, sidecar.as_bytes());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
    }
}
