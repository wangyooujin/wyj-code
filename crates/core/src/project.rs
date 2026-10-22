//! 项目键派生：按 git 仓库根把会话、权限等资源按「项目」分组。
//! 找不到 `.git` 时回退到规范化后的目录本身。被会话隔离（session_store）、
//! 逐调用权限持久化（tools::ctx）、记忆、演化、代码索引共用，保证它们对
//! 「同一项目」的判定一致。
//!
//! # 键必须稳定
//!
//! 键是**磁盘目录名**，一旦变化，`sessions/<key>/`、`memory-v3/projects/<key>/`
//! 等等全部对不上，表现为「项目状态凭空消失」而没有任何报错。因此：
//!
//! - 哈希用 SHA-256，不用 `DefaultHasher`——后者是 `std` 的内部实现，
//!   **明确不保证跨 Rust 版本稳定**，换工具链就可能让全部项目键漂移。
//! - 键带 `v1-` 前缀。将来算法再改时可以同时识别两代目录，而不是静默丢失。
//!
//! 旧算法（`DefaultHasher` + 8 位十六进制、无前缀）保留为
//! [`legacy_project_key`]，只供 `storage migrate` 把老目录搬到新键下。

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 项目键的当前版本前缀。
const KEY_VERSION: &str = "v1";

/// 返回给定路径所属的「项目根」。与 `wyj_config::project_config_dir` 共用同一
/// 根解析，保证会话、权限、MCP 信任和 `.wyj-code` 资源始终按同一项目分组。
pub fn project_root(path: &Path) -> PathBuf {
    wyj_config::project_root(path)
}

/// 项目的稳定标识：`v1-<目录名>-<sha256 前 16 位十六进制>`，可用作目录名。
///
/// 输入可以是仓库内任意路径（子目录、文件所在目录），内部先归一到项目根。
pub fn project_key(path: &Path) -> String {
    let root = project_root(path);
    key_of_root(&root)
}

/// 已知项目根时直接算键，省掉一次 `project_root` 的向上查找。
pub fn key_of_root(root: &Path) -> String {
    let dir_name = root.file_name().and_then(|s| s.to_str()).unwrap_or("root");
    format!(
        "{KEY_VERSION}-{}-{}",
        sanitize(dir_name),
        short_hash(&root.to_string_lossy())
    )
}

/// 迁移前的旧键：`<目录名>-<DefaultHasher 低 32 位十六进制>`。
///
/// **不要在新代码里用它落盘**——它存在的唯一目的是让 `storage migrate`
/// 找得到老目录。同样先归一到项目根。
pub fn legacy_project_key(path: &Path) -> String {
    legacy_key_of_root(&project_root(path))
}

/// 同 [`legacy_project_key`]，但输入已经是项目根。
pub fn legacy_key_of_root(root: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let dir_name = root.file_name().and_then(|s| s.to_str()).unwrap_or("root");
    format!("{}-{:08x}", sanitize(dir_name), hasher.finish() as u32)
}

/// 迁移前的 `memory::project_id`：**直接哈希传入路径，不做根归一**。
///
/// 哈希函数与 [`legacy_key_of_root`] 完全相同，差别只在**输入**：这里原样
/// 使用调用方给的路径，而旧 `project_key` 会先走 `project_root`。因此从仓库
/// 子目录启动时，`memory/`、`indexes/` 的目录名与 `projects/`、`memory-v3/`
/// 并不一致——迁移时必须按各自的历史输入分别算，不能统一按仓库根推。
pub fn legacy_path_hash(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let dir_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    format!("{}-{:08x}", sanitize(&dir_name), hasher.finish() as u32)
}

fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_walks_up_to_dot_git() {
        let tmp = std::env::temp_dir().join(format!("wyj-proj-{}", std::process::id()));
        let sub = tmp.join("a").join("b");
        std::fs::create_dir_all(sub.join(".gitkeep").parent().unwrap()).unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        assert_eq!(project_root(&sub), tmp.canonicalize().unwrap());
        // 子目录与仓库根属于同一项目
        assert_eq!(project_key(&sub), project_key(&tmp));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn distinct_repos_have_distinct_keys() {
        let base = std::env::temp_dir().join(format!("wyj-proj2-{}", std::process::id()));
        let r1 = base.join("repo1");
        let r2 = base.join("repo2");
        std::fs::create_dir_all(r1.join(".git")).unwrap();
        std::fs::create_dir_all(r2.join(".git")).unwrap();
        assert_ne!(project_key(&r1), project_key(&r2));
        std::fs::remove_dir_all(&base).ok();
    }

    /// 键是磁盘目录名，必须自带版本前缀，且对同一路径稳定。
    #[test]
    fn key_is_versioned_and_stable() {
        let root = Path::new("/Volumes/some/repo");
        let first = key_of_root(root);
        let second = key_of_root(root);
        assert_eq!(first, second);
        assert!(first.starts_with("v1-repo-"), "实际 {first}");
        // 8 字节 = 16 个十六进制字符
        assert_eq!(first.rsplit('-').next().unwrap().len(), 16);
        // 只含目录名安全字符
        assert!(first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    /// 旧键与新键必须不同，否则迁移会自己覆盖自己。
    #[test]
    fn legacy_key_differs_from_current_key() {
        let root = Path::new("/Volumes/some/repo");
        assert_ne!(key_of_root(root), legacy_key_of_root(root));
    }

    /// 路径哈希（旧 memory/indexes 算法）直接吃传入路径，旧项目键先归一到仓库根。
    /// 子目录启动时两者必须不同——否则迁移会把两类目录混成一处。
    #[test]
    fn legacy_path_hash_uses_raw_path_while_legacy_key_uses_root() {
        let base = std::env::temp_dir().join(format!("wyj-proj3-{}", std::process::id()));
        let repo = base.join("repo");
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        assert_ne!(legacy_path_hash(&sub), legacy_key_of_root(&repo));
        // 根上两者输入相同，结果也相同——这正是「不能按名字猜、必须按历史输入算」
        // 的原因：同一条路径在两种算法下可能是同一个键。
        assert_eq!(legacy_path_hash(&repo), legacy_key_of_root(&repo));
        std::fs::remove_dir_all(&base).ok();
    }
}
