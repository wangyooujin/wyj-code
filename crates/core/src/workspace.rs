//! Git worktree-backed execution workspaces.
//!
//! The manager never stashes, checks out, resets, stages, or cleans the user's checkout.  Every
//! destructive operation first proves ownership through a manager-side manifest.  Normal dispose
//! refuses a dirty worktree; callers must opt into [`GitWorktreeManager::dispose_force`] after
//! review if they intentionally want to discard remaining changes.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::interfaces::{
    ExecutionWorkspace, ExecutionWorkspaceKind, ExecutionWorkspaceManager,
    ExecutionWorkspaceRequest, WorkspaceAcceptResult, WorkspaceDiff, WorkspaceDiffSummary,
};

const UNTRACKED_PATCH_LIMIT: u64 = 256 * 1024;

#[derive(Debug, Clone)]
pub struct GitWorktreeManager {
    state_root: PathBuf,
    /// worktree 自动 prune 的过期天数(builder 注入;0 = 仅 dispose 时清理)。
    max_age_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceManifest {
    schema_version: u32,
    workspace: ExecutionWorkspace,
    repository_root: PathBuf,
    created_at: String,
}

impl GitWorktreeManager {
    pub fn new(state_root: impl Into<PathBuf>) -> Result<Self> {
        let state_root = state_root.into();
        fs::create_dir_all(state_root.join("worktrees"))?;
        fs::create_dir_all(state_root.join("manifests"))?;
        Ok(Self {
            state_root,
            max_age_days: 0,
        })
    }

    /// 注入 worktree 自动 prune 过期天数(0 = 不 prune)。`create()` 末尾
    /// 会调 `git worktree prune --expire=<N days>` 清理过期条目。
    pub fn with_storage_retention(mut self, max_age_days: u32) -> Self {
        self.max_age_days = max_age_days;
        self
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn list(&self) -> Result<Vec<ExecutionWorkspace>> {
        let mut workspaces = Vec::new();
        for entry in fs::read_dir(self.state_root.join("manifests"))? {
            let path = entry?.path();
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            if let Ok(manifest) = self.read_manifest_path(&path) {
                workspaces.push(manifest.workspace);
            }
        }
        workspaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(workspaces)
    }

    pub fn dispose_force(&self, workspace: &ExecutionWorkspace) -> Result<()> {
        let manifest = self.prove_ownership(workspace)?;
        let output = git(
            &manifest.repository_root,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                manifest.workspace.root.as_os_str(),
            ],
        )?;
        ensure_success(output, "remove worktree")?;
        let manifest_path = self.manifest_path(&workspace.id);
        if manifest_path.exists() {
            fs::remove_file(manifest_path)?;
        }
        Ok(())
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.state_root.join("manifests").join(format!("{id}.json"))
    }

    fn read_manifest_path(&self, path: &Path) -> Result<WorkspaceManifest> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid manifest {}", path.display()))
    }

    fn prove_ownership(&self, workspace: &ExecutionWorkspace) -> Result<WorkspaceManifest> {
        if workspace.kind != ExecutionWorkspaceKind::GitWorktree || !workspace.disposable {
            bail!("workspace is not a disposable Git worktree")
        }
        let manifest = self.read_manifest_path(&self.manifest_path(&workspace.id))?;
        if manifest.workspace != *workspace {
            bail!("workspace ownership manifest does not match the request")
        }
        let worktrees_root = canonical_or_self(&self.state_root.join("worktrees"));
        let root = canonical_or_self(&workspace.root);
        if !root.starts_with(&worktrees_root) || root == worktrees_root {
            bail!("workspace root is outside the manager-owned directory")
        }
        Ok(manifest)
    }

    fn changed_paths(&self, workspace: &ExecutionWorkspace) -> Result<BTreeSet<PathBuf>> {
        self.prove_ownership(workspace)?;
        let output = ensure_success(
            git(
                &workspace.root,
                [
                    OsStr::new("status"),
                    OsStr::new("--porcelain=v1"),
                    OsStr::new("-z"),
                ],
            )?,
            "read worktree status",
        )?;
        parse_porcelain_paths(&output.stdout)
    }

    fn ensure_target_unchanged(
        &self,
        manifest: &WorkspaceManifest,
        paths: &[PathBuf],
    ) -> Result<()> {
        let head = git_text(
            &manifest.repository_root,
            [OsStr::new("rev-parse"), OsStr::new("HEAD")],
            "resolve target HEAD",
        )?;
        if head.trim() != manifest.workspace.base_revision {
            bail!(
                "target checkout advanced from workspace base {} to {}; rebase/recreate the worktree before accepting",
                manifest.workspace.base_revision,
                head.trim()
            )
        }
        for path in paths {
            let output = ensure_success(
                git(
                    &manifest.repository_root,
                    [
                        OsStr::new("status"),
                        OsStr::new("--porcelain=v1"),
                        OsStr::new("--"),
                        path.as_os_str(),
                    ],
                )?,
                "check target path",
            )?;
            if !output.stdout.is_empty() {
                bail!(
                    "refusing to overwrite a path modified in the user's checkout: {}",
                    path.display()
                )
            }
        }
        Ok(())
    }
}

impl ExecutionWorkspaceManager for GitWorktreeManager {
    fn create(&self, request: &ExecutionWorkspaceRequest) -> Result<ExecutionWorkspace> {
        let repository_root = fs::canonicalize(&request.repository_root).with_context(|| {
            format!(
                "repository root does not exist: {}",
                request.repository_root.display()
            )
        })?;
        let actual_root = git_text(
            &repository_root,
            [OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
            "resolve repository root",
        )?;
        if fs::canonicalize(actual_root.trim())? != repository_root {
            bail!("repository_root must be the Git top-level directory")
        }
        let revision_spec = format!("{}^{{commit}}", request.base_revision);
        let base_revision = git_text(
            &repository_root,
            [
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new(&revision_spec),
            ],
            "resolve base revision",
        )?
        .trim()
        .to_string();
        let session = sanitize_id(&request.session_id);
        let id = format!("{}-{}", session, &Uuid::new_v4().simple().to_string()[..12]);
        let root = self.state_root.join("worktrees").join(&id);
        if root.exists() {
            bail!(
                "generated workspace path already exists: {}",
                root.display()
            )
        }
        let output = git(
            &repository_root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                root.as_os_str(),
                OsStr::new(&base_revision),
            ],
        )?;
        if let Err(error) = ensure_success(output, "create worktree") {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
        let workspace = ExecutionWorkspace {
            id: id.clone(),
            root,
            kind: ExecutionWorkspaceKind::GitWorktree,
            base_revision,
            parent_checkpoint_id: request.parent_checkpoint_id.clone(),
            disposable: true,
        };
        let manifest = WorkspaceManifest {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            workspace: workspace.clone(),
            repository_root: repository_root.clone(),
            created_at: Utc::now().to_rfc3339(),
        };
        fs::write(
            self.manifest_path(&id),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        // 周期 prune 过期 worktree 引用;只动 git metadata,不删 worktree 目录
        if self.max_age_days > 0 {
            let _ = git(
                &repository_root,
                [
                    OsStr::new("worktree"),
                    OsStr::new("prune"),
                    OsStr::new("--expire"),
                    OsStr::new(&format!("{}.days ago", self.max_age_days)),
                ],
            );
        }
        Ok(workspace)
    }

    fn diff_summary(&self, workspace: &ExecutionWorkspace) -> Result<WorkspaceDiffSummary> {
        let paths = self.changed_paths(workspace)?;
        let output = ensure_success(
            git(
                &workspace.root,
                [
                    OsStr::new("diff"),
                    OsStr::new("--numstat"),
                    OsStr::new(&workspace.base_revision),
                    OsStr::new("--"),
                ],
            )?,
            "summarize worktree diff",
        )?;
        let mut insertions = 0_u64;
        let mut deletions = 0_u64;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.splitn(3, '\t');
            insertions =
                insertions.saturating_add(fields.next().and_then(parse_numstat).unwrap_or(0));
            deletions =
                deletions.saturating_add(fields.next().and_then(parse_numstat).unwrap_or(0));
        }
        for path in &paths {
            let abs = workspace.root.join(path);
            let tracked = git(
                &workspace.root,
                [
                    OsStr::new("ls-files"),
                    OsStr::new("--error-unmatch"),
                    path.as_os_str(),
                ],
            )?
            .status
            .success();
            if !tracked && abs.is_file() {
                let bytes = fs::read(&abs)?;
                insertions = insertions
                    .saturating_add(bytes.iter().filter(|b| **b == b'\n').count() as u64 + 1);
            }
        }
        Ok(WorkspaceDiffSummary {
            changed_files: paths.len(),
            insertions,
            deletions,
            paths: paths.into_iter().collect(),
        })
    }

    fn review(&self, workspace: &ExecutionWorkspace) -> Result<WorkspaceDiff> {
        let summary = self.diff_summary(workspace)?;
        let output = ensure_success(
            git(
                &workspace.root,
                [
                    OsStr::new("diff"),
                    OsStr::new("--binary"),
                    OsStr::new("--no-ext-diff"),
                    OsStr::new(&workspace.base_revision),
                    OsStr::new("--"),
                ],
            )?,
            "render worktree diff",
        )?;
        let mut patch = String::from_utf8_lossy(&output.stdout).into_owned();
        let mut omitted_paths = Vec::new();
        for path in &summary.paths {
            let tracked = git(
                &workspace.root,
                [
                    OsStr::new("ls-files"),
                    OsStr::new("--error-unmatch"),
                    path.as_os_str(),
                ],
            )?
            .status
            .success();
            if tracked {
                continue;
            }
            let abs = workspace.root.join(path);
            let metadata = match fs::symlink_metadata(&abs) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if !metadata.file_type().is_file() || metadata.len() > UNTRACKED_PATCH_LIMIT {
                omitted_paths.push(path.clone());
                continue;
            }
            let bytes = fs::read(&abs)?;
            let Ok(text) = String::from_utf8(bytes) else {
                omitted_paths.push(path.clone());
                continue;
            };
            patch.push_str(&untracked_patch(path, &text));
        }
        Ok(WorkspaceDiff {
            summary,
            patch,
            omitted_paths,
        })
    }

    fn accept(
        &self,
        workspace: &ExecutionWorkspace,
        paths: &[PathBuf],
    ) -> Result<WorkspaceAcceptResult> {
        let manifest = self.prove_ownership(workspace)?;
        if paths.is_empty() {
            bail!("at least one path must be selected")
        }
        let changed = self.changed_paths(workspace)?;
        let mut selected = Vec::new();
        let mut rejected = Vec::new();
        for path in paths {
            let clean = normalize_relative(path)?;
            if changed.contains(&clean) {
                selected.push(clean);
            } else {
                rejected.push(clean);
            }
        }
        if selected.is_empty() {
            bail!("none of the selected paths are changed in the worktree")
        }
        self.ensure_target_unchanged(&manifest, &selected)?;

        let mut accepted = Vec::new();
        let mut deleted = Vec::new();
        for path in &selected {
            let source = workspace.root.join(path);
            let target = manifest.repository_root.join(path);
            ensure_target_has_no_symlink(&manifest.repository_root, path)?;
            match fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "refusing to accept a symlink from an execution worktree: {}",
                        path.display()
                    )
                }
                Ok(metadata) if metadata.is_file() => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&source, &target).with_context(|| {
                        format!("copy {} to {}", source.display(), target.display())
                    })?;
                    accepted.push(path.clone());
                }
                Ok(_) => bail!("only regular files can be accepted: {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if target.is_file() {
                        fs::remove_file(&target)?;
                    } else if target.exists() {
                        bail!("refusing to remove a non-file target: {}", target.display())
                    }
                    deleted.push(path.clone());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(WorkspaceAcceptResult {
            accepted,
            deleted,
            rejected,
        })
    }

    fn dispose(&self, workspace: &ExecutionWorkspace) -> Result<()> {
        let summary = self.diff_summary(workspace)?;
        if summary.changed_files != 0 {
            bail!(
                "worktree still has {} changed files; review/accept them or explicitly force disposal",
                summary.changed_files
            )
        }
        self.dispose_force(workspace)
    }
}

fn git<I, S>(cwd: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", cwd.display()))
}

fn git_text<I, S>(cwd: &Path, args: I, action: &str) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git(cwd, args)?;
    let output = ensure_success(output, action)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ensure_success(output: Output, action: &str) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    bail!(
        "git {action} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn sanitize_id(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(32)
        .collect();
    if value.is_empty() {
        "session".to_string()
    } else {
        value
    }
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_relative(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        bail!(
            "selected paths must be repository-relative: {}",
            path.display()
        )
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => clean.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("selected path escapes the repository: {}", path.display())
            }
        }
    }
    if clean.as_os_str().is_empty() {
        bail!("selected path cannot be empty")
    }
    Ok(clean)
}

fn ensure_target_has_no_symlink(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            bail!("invalid repository-relative target: {}", relative.display())
        };
        current.push(value);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to accept through a symlink in the target checkout: {}",
                    relative.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn parse_porcelain_paths(bytes: &[u8]) -> Result<BTreeSet<PathBuf>> {
    let fields: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|v| !v.is_empty())
        .collect();
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.len() < 4 || field[2] != b' ' {
            bail!("unexpected git status --porcelain entry")
        }
        let status = &field[..2];
        let path = PathBuf::from(String::from_utf8_lossy(&field[3..]).into_owned());
        paths.insert(path);
        if status.contains(&b'R') || status.contains(&b'C') {
            index += 1;
            if index < fields.len() {
                paths.insert(PathBuf::from(
                    String::from_utf8_lossy(fields[index]).into_owned(),
                ));
            }
        }
        index += 1;
    }
    Ok(paths)
}

fn parse_numstat(value: &str) -> Option<u64> {
    (value != "-").then(|| value.parse().ok()).flatten()
}

fn untracked_patch(path: &Path, text: &str) -> String {
    let escaped = path.to_string_lossy();
    let lines = text.lines().count().max(1);
    let mut patch = format!(
        "diff --git a/{0} b/{0}\nnew file mode 100644\n--- /dev/null\n+++ b/{0}\n@@ -0,0 +1,{lines} @@\n",
        escaped
    );
    for line in text.split_inclusive('\n') {
        patch.push('+');
        patch.push_str(line);
    }
    if !text.ends_with('\n') {
        patch.push_str("\n\\ No newline at end of file\n");
    }
    patch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        run(dir.path(), &["config", "user.name", "wyj-test"]);
        run(
            dir.path(),
            &["config", "user.email", "wyj-test@example.invalid"],
        );
        fs::write(dir.path().join("base.txt"), "base\n").unwrap();
        run(dir.path(), &["add", "base.txt"]);
        run(dir.path(), &["commit", "-qm", "base"]);
        dir
    }

    #[test]
    fn worktree_review_accept_and_safe_dispose_preserve_other_user_changes() {
        let repo = repo();
        let state = tempfile::tempdir().unwrap();
        let manager = GitWorktreeManager::new(state.path()).unwrap();
        let workspace = manager
            .create(&ExecutionWorkspaceRequest {
                session_id: "s/unsafe".to_string(),
                repository_root: repo.path().to_path_buf(),
                base_revision: "HEAD".to_string(),
                parent_checkpoint_id: None,
                purpose: "test".to_string(),
            })
            .unwrap();
        fs::write(workspace.root.join("base.txt"), "changed\n").unwrap();
        fs::write(workspace.root.join("new.txt"), "new\n").unwrap();
        fs::write(repo.path().join("user.txt"), "mine\n").unwrap();

        let review = manager.review(&workspace).unwrap();
        assert_eq!(review.summary.changed_files, 2);
        assert!(review.patch.contains("base.txt"));
        assert!(review.patch.contains("new file mode"));
        assert!(manager.dispose(&workspace).is_err());

        let accepted = manager
            .accept(
                &workspace,
                &[PathBuf::from("base.txt"), PathBuf::from("new.txt")],
            )
            .unwrap();
        assert_eq!(accepted.accepted.len(), 2);
        assert_eq!(
            fs::read_to_string(repo.path().join("base.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("user.txt")).unwrap(),
            "mine\n"
        );
        manager.dispose_force(&workspace).unwrap();
        assert!(!workspace.root.exists());
    }

    #[test]
    fn accept_refuses_to_overwrite_user_modified_path() {
        let repo = repo();
        let state = tempfile::tempdir().unwrap();
        let manager = GitWorktreeManager::new(state.path()).unwrap();
        let workspace = manager
            .create(&ExecutionWorkspaceRequest {
                session_id: "s1".to_string(),
                repository_root: repo.path().to_path_buf(),
                base_revision: "HEAD".to_string(),
                parent_checkpoint_id: None,
                purpose: "test".to_string(),
            })
            .unwrap();
        fs::write(workspace.root.join("base.txt"), "agent\n").unwrap();
        fs::write(repo.path().join("base.txt"), "user\n").unwrap();
        let error = manager
            .accept(&workspace, &[PathBuf::from("base.txt")])
            .unwrap_err();
        assert!(error.to_string().contains("user's checkout"));
        manager.dispose_force(&workspace).unwrap();
    }

    #[test]
    fn accept_deletes_a_selected_tracked_file() {
        let repo = repo();
        let state = tempfile::tempdir().unwrap();
        let manager = GitWorktreeManager::new(state.path()).unwrap();
        let workspace = manager
            .create(&ExecutionWorkspaceRequest {
                session_id: "delete".to_string(),
                repository_root: repo.path().to_path_buf(),
                base_revision: "HEAD".to_string(),
                parent_checkpoint_id: None,
                purpose: "test".to_string(),
            })
            .unwrap();
        fs::remove_file(workspace.root.join("base.txt")).unwrap();
        let result = manager
            .accept(&workspace, &[PathBuf::from("base.txt")])
            .unwrap();
        assert_eq!(result.deleted, vec![PathBuf::from("base.txt")]);
        assert!(!repo.path().join("base.txt").exists());
        manager.dispose_force(&workspace).unwrap();
    }

    #[test]
    fn review_reports_untracked_binary_as_omitted() {
        let repo = repo();
        let state = tempfile::tempdir().unwrap();
        let manager = GitWorktreeManager::new(state.path()).unwrap();
        let workspace = manager
            .create(&ExecutionWorkspaceRequest {
                session_id: "binary".to_string(),
                repository_root: repo.path().to_path_buf(),
                base_revision: "HEAD".to_string(),
                parent_checkpoint_id: None,
                purpose: "test".to_string(),
            })
            .unwrap();
        fs::write(workspace.root.join("image.bin"), [0, 159, 146, 150]).unwrap();
        let review = manager.review(&workspace).unwrap();
        assert_eq!(review.omitted_paths, vec![PathBuf::from("image.bin")]);
        manager.dispose_force(&workspace).unwrap();
    }

    #[test]
    fn accept_refuses_after_target_head_advances() {
        let repo = repo();
        let state = tempfile::tempdir().unwrap();
        let manager = GitWorktreeManager::new(state.path()).unwrap();
        let workspace = manager
            .create(&ExecutionWorkspaceRequest {
                session_id: "advanced".to_string(),
                repository_root: repo.path().to_path_buf(),
                base_revision: "HEAD".to_string(),
                parent_checkpoint_id: None,
                purpose: "test".to_string(),
            })
            .unwrap();
        fs::write(workspace.root.join("base.txt"), "agent\n").unwrap();
        fs::write(repo.path().join("other.txt"), "next\n").unwrap();
        run(repo.path(), &["add", "other.txt"]);
        run(repo.path(), &["commit", "-qm", "advance"]);
        let error = manager
            .accept(&workspace, &[PathBuf::from("base.txt")])
            .unwrap_err();
        assert!(error.to_string().contains("target checkout advanced"));
        manager.dispose_force(&workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn accept_refuses_to_replace_a_clean_tracked_symlink() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "-q"]);
        run(repo.path(), &["config", "user.name", "wyj-test"]);
        run(
            repo.path(),
            &["config", "user.email", "wyj-test@example.invalid"],
        );
        let outside = repo
            .path()
            .parent()
            .unwrap()
            .join(format!("wyj-code-outside-{}", Uuid::new_v4().simple()));
        fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, repo.path().join("link.txt")).unwrap();
        run(repo.path(), &["add", "link.txt"]);
        run(repo.path(), &["commit", "-qm", "symlink"]);

        let state = tempfile::tempdir().unwrap();
        let manager = GitWorktreeManager::new(state.path()).unwrap();
        let workspace = manager
            .create(&ExecutionWorkspaceRequest {
                session_id: "symlink".to_string(),
                repository_root: repo.path().to_path_buf(),
                base_revision: "HEAD".to_string(),
                parent_checkpoint_id: None,
                purpose: "test".to_string(),
            })
            .unwrap();
        fs::remove_file(workspace.root.join("link.txt")).unwrap();
        fs::write(workspace.root.join("link.txt"), "agent\n").unwrap();
        let error = manager
            .accept(&workspace, &[PathBuf::from("link.txt")])
            .unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside\n");
        manager.dispose_force(&workspace).unwrap();
        fs::remove_file(outside).unwrap();
    }
}
