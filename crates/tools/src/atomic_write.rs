//! 工具侧的原子文件写：tmp + rename，保留原文件权限。
//!
//! `wyj_core::serialize::write_atomic` 是同一套做法，但它住在 core 且用同步 IO
//! （状态文件都是小 JSON，同步更简单）。工具改的是**用户源码**，动辄几十 KB，
//! 必须在 async 上下文里走 tokio；同时它比状态文件多一条要求：**保留原文件的
//! 权限位**——`Write` 覆盖一个可执行的 `deploy.sh` 之后它不该变成 0644。

use std::path::Path;

use anyhow::{Context, Result};

/// 原子覆盖 `path`。
///
/// 为什么不能用 `tokio::fs::write`：它是 `open(TRUNC) + write`，两步之间进程
/// 被 ESC 中断、被心跳看门狗 abort、或机器断电，磁盘上就留下一个**被截断的
/// 源码文件**——比「没写成功」严重得多，用户看到的是文件凭空少了一半。
/// tmp + rename 让「旧内容」与「新内容」之间没有中间态：rename 之前旧文件
/// 完整，rename 之后新文件完整。
///
/// 顺序刻意为「先写 tmp、再改权限、最后 rename」：权限要在 rename 之前设好，
/// 否则会出现一个「已就位但权限不对」的窗口期。
pub async fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("创建目录失败: {}", parent.display()))?;

    // tmp 放在同一目录：跨文件系统的 rename 不是原子操作（会退化成 copy+unlink）。
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = parent.join(format!(".{file_name}.wyj-tmp-{}", std::process::id()));

    // 保留原权限：覆盖可执行脚本后它仍应是可执行的。
    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::metadata(path)
            .await
            .ok()
            .map(|meta| meta.permissions().mode())
    };

    let write_result = async {
        tokio::fs::write(&tmp, bytes).await?;
        #[cfg(unix)]
        if let Some(mode) = original_mode {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).await?;
        }
        tokio::fs::rename(&tmp, path).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;

    if let Err(error) = write_result {
        // 失败时别把 tmp 留在用户的工作目录里。
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow::Error::from(error))
            .with_context(|| format!("原子写入失败: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_write_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "old").unwrap();

        write_file_atomic(&path, b"new").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        // 不留 tmp 残骸。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("wyj-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "写入后不应残留 tmp 文件");
    }

    #[tokio::test]
    async fn atomic_write_creates_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/b.txt");
        write_file_atomic(&path, b"hi").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi");
    }

    /// 覆盖一个可执行脚本后，它必须仍然是可执行的——这是 `Write` 相对
    /// `write_atomic` 的额外要求（状态文件不需要权限位，用户源码需要）。
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.sh");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_file_atomic(&path, b"#!/bin/sh\necho hi\n")
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "覆盖后权限位必须保留");
    }
}
