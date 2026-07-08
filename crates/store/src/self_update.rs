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

/// 下载归档 + `.sha256` sidecar，校验一致后返回归档原始字节。
pub async fn download_and_verify(
    client: &reqwest::Client,
    archive_url: &str,
    sha256_url: &str,
) -> Result<Vec<u8>> {
    let archive_bytes = client
        .get(archive_url)
        .send()
        .await
        .with_context(|| format!("下载安装包失败: {archive_url}"))?
        .error_for_status()
        .with_context(|| format!("下载安装包返回错误状态: {archive_url}"))?
        .bytes()
        .await
        .with_context(|| format!("读取安装包内容失败: {archive_url}"))?;

    let sha256_text = client
        .get(sha256_url)
        .send()
        .await
        .with_context(|| format!("下载校验和文件失败: {sha256_url}"))?
        .error_for_status()
        .with_context(|| format!("下载校验和文件返回错误状态: {sha256_url}"))?
        .text()
        .await
        .with_context(|| format!("读取校验和文件内容失败: {sha256_url}"))?;

    verify_sha256(&archive_bytes, &sha256_text)?;
    Ok(archive_bytes.to_vec())
}

/// `sha256_text` 格式为 `<hex hash>  <filename>`（`shasum -a 256`/`Get-FileHash` 输出）。
fn verify_sha256(bytes: &[u8], sha256_text: &str) -> Result<()> {
    let expected = sha256_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("校验和文件内容为空或格式不正确"))?
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex_encode(&hasher.finalize());
    if actual != expected {
        bail!("校验和不匹配：期望 {expected}，实际 {actual}");
    }
    Ok(())
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
        assert!(verify_sha256(bytes, &sidecar).is_ok());

        let bad_sidecar = format!("{}  wyj-code.tar.gz\n", "0".repeat(64));
        assert!(verify_sha256(bytes, &bad_sidecar).is_err());
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
}
