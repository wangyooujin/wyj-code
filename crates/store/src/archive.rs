//! zip 归档的共用安全校验。
//!
//! 仓库里有两个地方会**解压外部来源的 zip**：`skillhub`（从技能注册表下载技能包）
//! 与 [`crate::bundle`]（用户自己的配置备份）。两者的威胁模型不同——前者是陌生人
//! 上传的包，后者是用户自己导出的文件——但**拒绝哪些条目**完全一致：路径穿越、
//! 绝对路径、符号链接、macOS 打包噪音。
//!
//! 这段判定原先只写在 `skillhub` 里。复制一份到 `bundle` 会让"哪天补了一条新的
//! 攻击面"变成两处都要改、漏一处就是漏洞；因此抽到这里，两边共用同一套实现。
//!
//! 错误措辞刻意中性（「归档」而不是「技能包」）：`skillhub` 侧的调用方会自己
//! `with_context` 补上"技能包"字样，而备份导入需要它说"备份文件"。

use anyhow::{bail, Context, Result};
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};

/// 校验 zip 条目名，拒绝绝对路径与 `..` 穿越，返回规范化相对路径。
///
/// `zip::ZipArchive::enclosed_name()` 也能防穿越，但它是**静默修正**（把 `../`
/// 剥掉后照常解压），这里要的是**明确拒绝**并告诉用户包有问题，所以自己判。
pub fn safe_entry_name(raw: &str) -> Result<String> {
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    if trimmed.is_empty() {
        bail!("归档包含有空路径条目");
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in trimmed.split('/') {
        match part {
            "" | "." => continue,
            ".." => bail!("归档包含有路径穿越条目: {raw}"),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        bail!("归档包含有无效路径条目: {raw}");
    }
    if Path::new(trimmed).is_absolute() || trimmed.contains(':') {
        bail!("归档包含有绝对路径条目: {raw}");
    }
    Ok(parts.join("/"))
}

/// 拒绝符号链接条目：解压出符号链接等于给后续写入开了「任意文件覆盖」的口子。
pub fn reject_symlink(file: &zip::read::ZipFile<'_>, name: &str) -> Result<()> {
    if let Some(mode) = file.unix_mode() {
        // 0o170000 是 unix 文件类型掩码，0o120000 是 S_IFLNK。
        if mode & 0o170000 == 0o120000 {
            bail!("归档包含有符号链接条目: {name}");
        }
    }
    Ok(())
}

/// 需要跳过的打包噪音（macOS `zip` 产物）。
pub fn is_noise_entry(name: &str) -> bool {
    name.starts_with("__MACOSX/") || name.ends_with("/.DS_Store") || name == ".DS_Store"
}

/// 把相对条目路径拼到 `base` 下，并**再确认一次**结果仍在 `base` 内部。
///
/// 前面的 [`safe_entry_name`] 已经挡掉了 `..` 与绝对路径；这里做第二道校验是
/// 因为「解压外部内容」这件事一旦出一次错就是任意文件写入，冗余是故意的。
pub fn resolve_within(base: &Path, relative: &str) -> Result<PathBuf> {
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

pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 只读扫描任意 zip，返回每个条目的规范化名字（跳过打包噪音）。
///
/// 供"解压前先决定要拿哪些条目"的场景使用；不做体积/条目数上限——那是调用方
/// 按各自的威胁模型定的。
pub fn list_entries<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).context("读取归档条目失败")?;
        if file.is_dir() {
            continue;
        }
        let name = safe_entry_name(file.name())
            .with_context(|| format!("拒绝非法条目: {}", file.name()))?;
        if is_noise_entry(&name) {
            continue;
        }
        reject_symlink(&file, &name)?;
        names.push(name);
    }
    Ok(names)
}
