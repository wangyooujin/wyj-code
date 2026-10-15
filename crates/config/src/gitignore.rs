//! 判定「某目录是否处在 Git 仓库内」，供 `ignore::WalkBuilder::require_git` 使用。
//!
//! # 为什么需要它
//!
//! `ignore` crate **原生支持 worktree**：`resolve_git_commondir`（`dir.rs`）会在
//! `.git` 是文件时读 `gitdir:` 指针、再读该目录下的 `commondir`（worktree 实际是
//! `../..`），从而正确找到**主仓库**的 `.git/info/exclude`。
//!
//! 但这条路径只在 `require_git == true` 时才会被走到：crate 内部先把
//! `dir.join(".git").metadata()` 的结果算成 `git_type`，而 `require_git == false`
//! 时该值恒为 `None`，`resolve_git_commondir` 于是把 `.git` 直接当目录拼路径，
//! 拼出 `<worktree>/.git/info/exclude` —— 那是个文件不是目录，规则一条都加载不到。
//!
//! 本项目有三处显式写了 `require_git(false)`，动机是「非 Git 仓库里 `.gitignore`
//! 也应生效」（否则 `ignore` 会跳过整个目录树的忽略规则）。这两个诉求并不冲突：
//! **在仓库/worktree 里要 true，不在仓库里要 false**。本模块就是那个判定。
//!
//! # 为什么不直接用 `add_custom_ignore_filename`
//!
//! `ignore` 提供的 `WalkBuilder::add_custom_ignore_filename` 会把自定义 ignore 文件
//! 挂到**每一个被访问的目录**上（root 被重置为当前目录），于是主仓库
//! `info/exclude` 里的 `target/` 会连 `a/b/target/` 一起忽略——比 Git 语义更宽，
//! 且每个目录多一次读盘 + 一次规则编译。按仓库位置切 `require_git` 语义更准。

use std::path::Path;

/// 从 `root` 逐级向上找 `.git`，命中即返回 true。
///
/// 「是文件」也算 —— 那正是 worktree 的形态（`gitdir: /path/to/main/.git/worktrees/x`）。
/// 这一点很重要：若只认目录，managed worktree 里 `~/.wyj-code/workspaces/<project>/
/// worktrees/<id>/` 会因为 `.git` 是文件而被判成「不在仓库内」，`info/exclude`
/// 与 `resolve_git_commondir` 就一起失效了。
pub fn is_inside_git_repo(root: &Path) -> bool {
    let mut current = Some(root);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// 解析 worktree 的 common git dir：`.git` 是文件时读 `gitdir:` 指针，再读该目录
/// 下的 `commondir`（相对路径按 git dir 解析）。`.git` 是目录时原样返回。
///
/// 本函数当前**没有生产调用点**（`ignore` crate 内部会自己做同样的事），保留它是因为
/// 「managed worktree 的忽略规则在 worktree 里失效」这类 bug 极难从上层复现，
/// 需要一个能直接断言的入口来写钉子测试。
pub fn resolve_git_common_dir(root: &Path) -> Option<std::path::PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let real_git_dir = std::path::PathBuf::from(pointer.trim().strip_prefix("gitdir: ")?.trim());
    // `commondir` 是 worktree 私有文件，内容通常是 `../..`（相对于 git dir）。
    let common = std::fs::read_to_string(real_git_dir.join("commondir")).ok()?;
    let common = common.trim();
    let resolved = if Path::new(common).is_absolute() {
        std::path::PathBuf::from(common)
    } else {
        real_git_dir.join(common)
    };
    Some(resolved)
}

/// `<common git dir>/info/exclude` 的路径；该文件不存在时返回 `None`。
pub fn git_info_exclude_path(root: &Path) -> Option<std::path::PathBuf> {
    let path = resolve_git_common_dir(root)?.join("info").join("exclude");
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// 构造一个 managed worktree：主仓库 `.git/` 是目录，`info/exclude` 在里面；
    /// worktree 目录下 `.git` 是**文件**，指向 `.git/worktrees/<name>`，后者带
    /// `commondir` = `../..`。形状照抄 `ignore` 自带的
    /// `git_info_exclude_in_linked_worktree` 用例。
    fn linked_worktree(base: &Path) -> (PathBuf, PathBuf) {
        let main_git = base.join("main/.git");
        let work_git = main_git.join("worktrees/wt");
        write(&work_git.join("commondir"), "../..\n");
        write(&main_git.join("info/exclude"), "secret-notes.txt\n");
        let worktree = base.join("wt");
        write(
            &worktree.join(".git"),
            &format!("gitdir: {}\n", work_git.display()),
        );
        (worktree, main_git)
    }

    #[test]
    fn plain_directory_without_git_is_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_inside_git_repo(dir.path()));
        assert!(!is_inside_git_repo(&dir.path().join("nested/deeper")));
    }

    #[test]
    fn dot_git_directory_marks_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert!(is_inside_git_repo(dir.path()));
    }

    /// worktree 的 `.git` 是**文件**：判「是否在仓库内」必须认这一形态。
    /// 只认目录会让 `info/exclude` 与 `resolve_git_commondir` 一起失效。
    #[test]
    fn dot_git_file_marks_a_worktree_as_inside_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, _) = linked_worktree(dir.path());
        assert!(
            is_inside_git_repo(&worktree),
            "worktree 的 .git 是指针文件，仍算在仓库内"
        );
        assert!(is_inside_git_repo(&worktree.join("src/nested")));
    }

    #[test]
    fn common_dir_resolves_through_the_worktree_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, main_git) = linked_worktree(dir.path());
        assert_eq!(
            resolve_git_common_dir(&worktree).map(|p| p.canonicalize().unwrap()),
            Some(main_git.canonicalize().unwrap())
        );
        assert_eq!(
            git_info_exclude_path(&worktree).map(|p| p.canonicalize().unwrap()),
            Some(main_git.join("info/exclude").canonicalize().unwrap())
        );
    }

    #[test]
    fn malformed_pointer_yields_none_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("wt");
        write(&worktree.join(".git"), "这不是 gitdir 指针\n");
        assert!(resolve_git_common_dir(&worktree).is_none());
        assert!(git_info_exclude_path(&worktree).is_none());
    }
}
