//! 项目键派生：按 git 仓库根把会话、权限等资源按「项目」分组。
//! 找不到 `.git` 时回退到规范化后的目录本身。被会话隔离（session_store）
//! 与逐调用权限持久化（tools::ctx）共用，保证二者对「同一项目」判定一致。

use std::path::{Path, PathBuf};

/// 返回给定路径所属的「项目根」。与 `wyj_config::project_config_dir` 共用同一
/// 根解析，保证会话、权限、MCP 信任和 `.wyj-code` 资源始终按同一项目分组。
pub fn project_root(path: &Path) -> PathBuf {
    wyj_config::project_root(path)
}

/// 项目的稳定标识：项目根目录名 + 路径哈希，可用作目录名或分组键。
/// 命名风格对齐 memory 的 `project_id`，但以 git 仓库根为粒度。
pub fn project_key(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let root = project_root(path);
    let mut h = DefaultHasher::new();
    root.hash(&mut h);
    let dir_name = root.file_name().and_then(|s| s.to_str()).unwrap_or("root");
    format!("{}-{:08x}", sanitize(dir_name), h.finish() as u32)
}

/// 判定两个路径是否属于同一项目（同一 git 仓库根，或非 git 下同一目录）。
pub fn same_project(a: &Path, b: &Path) -> bool {
    project_root(a) == project_root(b)
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
        assert!(same_project(&sub, &tmp));
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
        assert!(!same_project(&r1, &r2));
        std::fs::remove_dir_all(&base).ok();
    }
}
