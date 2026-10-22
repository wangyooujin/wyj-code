use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Subcommand;
use wyj_core::{
    ExecutionWorkspace, ExecutionWorkspaceManager, ExecutionWorkspaceRequest, GitWorktreeManager,
    ReconcileReport, WorkspaceHealth,
};

#[derive(Subcommand, Debug)]
pub enum WorkspaceCommand {
    /// Create a manager-owned Git worktree (detached by default, or on a new branch).
    Create {
        #[arg(long, default_value = "HEAD")]
        base: String,
        #[arg(long, default_value = "manual")]
        session_id: String,
        #[arg(long, default_value = "isolated coding task")]
        purpose: String,
        #[arg(long)]
        parent_checkpoint_id: Option<String>,
        /// Check out a new branch in the worktree instead of a detached HEAD.
        /// Parallel instances are much easier to reconcile with this on: output can
        /// be pushed or merged instead of hand-picking files into the main checkout.
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List manager-owned worktrees for this repository, flagging stale entries.
    List {
        /// Drop manifests whose worktree directory is gone and prune git metadata.
        #[arg(long)]
        reconcile: bool,
        #[arg(long)]
        json: bool,
    },
    /// Review the complete binary-capable patch and omitted paths.
    Diff {
        id: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        summary_only: bool,
    },
    /// Accept selected repository-relative paths into the current checkout.
    Accept {
        id: String,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Remove a clean worktree; --force explicitly discards remaining changes.
    Dispose {
        id: String,
        #[arg(long)]
        force: bool,
    },
}

pub fn run(command: WorkspaceCommand, cwd: &Path) -> Result<()> {
    let repository_root = std::fs::canonicalize(wyj_core::project_root(cwd))?;
    let manager = manager(&repository_root)?;
    match command {
        WorkspaceCommand::Create {
            base,
            session_id,
            purpose,
            parent_checkpoint_id,
            branch,
            json,
        } => {
            let workspace = manager.create(&ExecutionWorkspaceRequest {
                session_id,
                repository_root: repository_root.clone(),
                base_revision: base,
                parent_checkpoint_id,
                purpose,
                branch,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&workspace)?);
            } else {
                println!("{}\t{}", workspace.id, workspace.root.display());
                if let Some(branch) = &workspace.branch {
                    println!("branch\t{branch}");
                }
                println!("\n# start an isolated instance in this worktree:");
                println!("{}", launch_command(&workspace, &repository_root));
                if let Some(hint) = shared_target_hint(&workspace.root, &repository_root) {
                    print!("{hint}");
                }
            }
        }
        WorkspaceCommand::List { reconcile, json } => {
            if reconcile {
                let report = manager.reconcile(&repository_root)?;
                if !json {
                    print!("{}", format_reconcile_report(&report));
                }
            }
            let entries = manager.list_with_health()?;
            if json {
                let payload: Vec<_> = entries
                    .iter()
                    .map(|(workspace, health)| {
                        serde_json::json!({
                            "workspace": workspace,
                            "health": health,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else if entries.is_empty() {
                println!("no managed worktrees");
            } else {
                let stale = entries
                    .iter()
                    .filter(|(_, health)| *health != WorkspaceHealth::Healthy)
                    .count();
                for (workspace, health) in &entries {
                    println!(
                        "{}\t{}\t{}\t{}",
                        workspace.id,
                        workspace.branch.as_deref().unwrap_or("(detached)"),
                        workspace.root.display(),
                        health_label(*health)
                    );
                }
                if stale > 0 {
                    println!(
                        "\n{stale} entr{} out of sync with git; run `wyj-code workspace list --reconcile`",
                        if stale == 1 { "y is" } else { "ies are" }
                    );
                }
            }
        }
        WorkspaceCommand::Diff {
            id,
            json,
            summary_only,
        } => {
            let workspace = find(&manager, &id)?;
            let review = manager.review(&workspace)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&review)?);
            } else {
                println!(
                    "{} files, +{} -{}",
                    review.summary.changed_files,
                    review.summary.insertions,
                    review.summary.deletions
                );
                for path in &review.summary.paths {
                    println!("  {}", path.display());
                }
                if !review.omitted_paths.is_empty() {
                    println!("omitted binary/oversized paths:");
                    for path in &review.omitted_paths {
                        println!("  {}", path.display());
                    }
                }
                if !summary_only && !review.patch.is_empty() {
                    print!("{}", review.patch);
                }
            }
        }
        WorkspaceCommand::Accept { id, paths, json } => {
            let workspace = find(&manager, &id)?;
            let result = manager.accept(&workspace, &paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                for path in result.accepted {
                    println!("accepted\t{}", path.display());
                }
                for path in result.deleted {
                    println!("deleted\t{}", path.display());
                }
                for path in result.rejected {
                    println!("unchanged\t{}", path.display());
                }
            }
        }
        WorkspaceCommand::Dispose { id, force } => {
            let workspace = find(&manager, &id)?;
            if force {
                manager.dispose_force(&workspace)?;
            } else {
                manager.dispose(&workspace)?;
            }
            println!("disposed {}", workspace.id);
        }
    }
    Ok(())
}

/// 「在这个 worktree 里开一个隔离实例」的命令行。
///
/// `--cwd` 指向 worktree（文件读写、checkpoint、git 操作都在那里），`--project-root`
/// 指回主仓库（项目身份、授权、记忆、信任、会话分区）。两者组合起来才是 worktree
/// 在本项目里的正确用法：物理隔离文件、逻辑上仍是同一个项目。
///
/// 直接把 `--project-root` 印出来不是省事，而是**必须**：worktree 里 `.git` 是个
/// **文件**，而 `config::resolve_project_root` 用 `.exists()` 判仓库根，于是不带
/// 这个参数时 wyj-code 会把 worktree 当成另一个项目 —— AllowAlways、项目记忆、
/// MCP/skill 信任、演化记录、代码索引全部重新开始，`-c` 也看不到主仓库会话。
fn launch_command(workspace: &ExecutionWorkspace, repository_root: &Path) -> String {
    format!(
        "wyj-code --cwd {} --project-root {}",
        workspace.root.display(),
        repository_root.display()
    )
}

/// `git worktree add` 只 checkout **已提交**的内容：被 `.gitignore` 忽略的产物目录
/// （Rust 的 `target/`、`node_modules/`、`.venv/`）和未提交的本地文件都不会跟过去。
/// 于是新建的 worktree 里第一次 `cargo build` 是**完整冷构建**——对本仓库这种体量
/// 就是几十分钟，而主仓库的 `target/` 明明就躺在旁边。
///
/// 这里在 create 后提示把 `CARGO_TARGET_DIR` 指回主仓库。cargo 对同一 target 目录是
/// 阻塞排队而非写坏，所以多个 worktree 共享一个 target 是安全的。
///
/// 只在「确实是 Rust 项目 + worktree 里没有 target + 主仓库有」时提示；其他语言
/// 的等价物（`node_modules`、`venv`）不猜，避免给出无意义的建议。
fn shared_target_hint(worktree_root: &Path, repository_root: &Path) -> Option<String> {
    if !worktree_root.join("Cargo.toml").exists() {
        return None;
    }
    let main_target = repository_root.join("target");
    if !main_target.is_dir() || worktree_root.join("target").is_dir() {
        return None;
    }
    Some(format!(
        "\n# note: this worktree has no target/ (git only checks out tracked files), so the\n\
         # first cargo build here is a full cold build. Reuse the main repo's artifacts:\n\
         # export CARGO_TARGET_DIR={}\n",
        main_target.display()
    ))
}

fn health_label(health: WorkspaceHealth) -> &'static str {
    match health {
        WorkspaceHealth::Healthy => "ok",
        WorkspaceHealth::MissingDirectory => "missing-directory",
        WorkspaceHealth::MissingGitRegistration => "unregistered-in-git",
        WorkspaceHealth::OrphanInGit => "orphan-in-git",
    }
}

fn format_reconcile_report(report: &ReconcileReport) -> String {
    if report.is_clean() {
        return "nothing to reconcile\n".to_string();
    }
    let mut out = String::new();
    if !report.stale_manifests.is_empty() {
        out.push_str(&format!(
            "dropped {} manifest(s) whose worktree directory is gone: {}\n",
            report.stale_manifests.len(),
            report.stale_manifests.join(", ")
        ));
    }
    if !report.unregistered_but_present.is_empty() {
        out.push_str(
            "directories still present but no longer registered in git (left untouched):\n",
        );
        for path in &report.unregistered_but_present {
            out.push_str(&format!("  {}\n", path.display()));
        }
    }
    if !report.orphans_in_git.is_empty() {
        out.push_str(
            "registered in git but no manifest (left untouched — remove manually if stale):\n",
        );
        for path in &report.orphans_in_git {
            out.push_str(&format!("  {}\n", path.display()));
        }
    }
    out
}

fn manager(repository_root: &Path) -> Result<GitWorktreeManager> {
    let manager = GitWorktreeManager::new(
        wyj_config::layout::workspaces_root(&wyj_config::config_dir()?)
            .join(wyj_core::project_key(repository_root)),
    )?;
    // 接上 `storage.workspace_worktree_max_age_days`。此前 `with_storage_retention`
    // 是零调用的死代码，而配置项被文档写成生效 cap —— 文档在骗人。
    Ok(match wyj_config::Config::load() {
        Ok(config) => {
            manager.with_storage_retention(config.storage.workspace_worktree_max_age_days)
        }
        // 配置读不出来不该让 `workspace` 子命令整个不可用 —— retention 只是清理策略。
        Err(_) => manager,
    })
}

fn find(manager: &GitWorktreeManager, id: &str) -> Result<ExecutionWorkspace> {
    let matches: Vec<_> = manager
        .list()?
        .into_iter()
        .filter(|workspace| workspace.id == id || workspace.id.starts_with(id))
        .collect();
    match matches.as_slice() {
        [workspace] => Ok(workspace.clone()),
        [] => bail!("unknown managed workspace: {id}"),
        _ => bail!("workspace id prefix is ambiguous: {id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::shared_target_hint;

    /// 提示只在「Rust 项目 + worktree 无 target + 主仓库有 target」时出现。
    /// 反过来两个方向的误报都很烦：每次 create 都喊一遍冷构建，用户就学会无视它了。
    #[test]
    fn target_hint_only_when_a_cold_build_is_actually_pending() {
        let base = tempfile::tempdir().unwrap();
        let main = base.path().join("main");
        let worktree = base.path().join("wt");
        std::fs::create_dir_all(main.join("target/debug")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();

        // 非 Rust 项目：不提示（`node_modules` / `venv` 的等价物不猜）。
        assert!(shared_target_hint(&worktree, &main).is_none());

        std::fs::write(worktree.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let hint = shared_target_hint(&worktree, &main)
            .expect("Rust 项目 + 主仓库有 target + worktree 没有 → 应提示");
        assert!(hint.contains("CARGO_TARGET_DIR"));
        assert!(hint.contains(&main.join("target").display().to_string()));

        // worktree 里已经有 target：提示是多余的。
        std::fs::create_dir_all(worktree.join("target")).unwrap();
        assert!(shared_target_hint(&worktree, &main).is_none());

        // 主仓库没有 target：没什么可复用，提示无从指向。
        std::fs::remove_dir_all(worktree.join("target")).unwrap();
        std::fs::remove_dir_all(&main).unwrap();
        assert!(shared_target_hint(&worktree, &main).is_none());
    }
}
