use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Subcommand;
use wyj_core::{
    ExecutionWorkspace, ExecutionWorkspaceManager, ExecutionWorkspaceRequest, GitWorktreeManager,
};

#[derive(Subcommand, Debug)]
pub enum WorkspaceCommand {
    /// Create a detached, manager-owned Git worktree.
    Create {
        #[arg(long, default_value = "HEAD")]
        base: String,
        #[arg(long, default_value = "manual")]
        session_id: String,
        #[arg(long, default_value = "isolated coding task")]
        purpose: String,
        #[arg(long)]
        parent_checkpoint_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List manager-owned worktrees for this repository.
    List {
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
            json,
        } => {
            let workspace = manager.create(&ExecutionWorkspaceRequest {
                session_id,
                repository_root,
                base_revision: base,
                parent_checkpoint_id,
                purpose,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&workspace)?);
            } else {
                println!("{}\t{}", workspace.id, workspace.root.display());
            }
        }
        WorkspaceCommand::List { json } => {
            let workspaces = manager.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&workspaces)?);
            } else if workspaces.is_empty() {
                println!("no managed worktrees");
            } else {
                for workspace in workspaces {
                    println!(
                        "{}\t{}\t{}",
                        workspace.id,
                        workspace.base_revision,
                        workspace.root.display()
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

fn manager(repository_root: &Path) -> Result<GitWorktreeManager> {
    GitWorktreeManager::new(
        wyj_config::config_dir()?
            .join("workspaces")
            .join(wyj_core::project_id(repository_root)),
    )
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
