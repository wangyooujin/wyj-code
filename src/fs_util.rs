//! 文件系统工具:目录/文件权限、原子写。

use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 确保目录存在且权限为 0700。
pub fn ensure_secure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).with_context(|| format!("创建目录失败: {}", path.display()))?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置目录权限失败: {}", path.display()))?;
    Ok(())
}

/// 原子写文件:同目录临时文件 → 写入 → fsync → rename → chmod 0600。
pub fn write_atomic_secure(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定父目录: {}", path.display()))?;
    ensure_secure_dir(dir)?;

    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("profiles")
    ));

    let mut file = fs::File::create(&tmp).with_context(|| format!("创建临时文件失败: {}", tmp.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("写入临时文件失败: {}", tmp.display()))?;
    file.sync_all().context("fsync 失败")?;
    drop(file);

    fs::rename(&tmp, path).with_context(|| format!("重命名失败: {} -> {}", tmp.display(), path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置文件权限失败: {}", path.display()))?;
    Ok(())
}

/// 检查文件权限是否过宽(group/other 可读),返回是否需要警告。
pub fn is_mode_too_open(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.permissions().mode())
        .map(|mode| mode & 0o077 != 0)
        .unwrap_or(false)
}
