//! 会话 Checkpoint / Rewind：保存对话边界与工作区文件状态，不修改真实 Git index。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use wyj_api::types::Message;

const CHECKPOINT_VERSION: u32 = 1;
const NON_GIT_MAX_FILES: usize = 256;
const NON_GIT_MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    AutoUser,
    PreTool,
    PostTool,
    PlanApproval,
    Manual,
    PreRewind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RewindScope {
    Conversation,
    Files,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub id: String,
    pub session_id: String,
    pub name: Option<String>,
    pub kind: CheckpointKind,
    pub timestamp: String,
    pub messages: Vec<Message>,
    pub workspace: WorkspaceSnapshot,
}

impl Checkpoint {
    pub fn workspace_root(&self) -> &Path {
        match &self.workspace {
            WorkspaceSnapshot::Git(snapshot) => &snapshot.repo_root,
            WorkspaceSnapshot::Files(snapshot) => &snapshot.root,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSnapshot {
    Git(GitSnapshot),
    Files(FileSnapshot),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSnapshot {
    pub repo_root: PathBuf,
    pub head: Option<String>,
    pub tree: String,
    pub commit: String,
    pub files: BTreeMap<PathBuf, GitFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitFileEntry {
    pub mode: String,
    pub object_id: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub root: PathBuf,
    pub files: BTreeMap<PathBuf, FileEntry>,
    pub complete: bool,
    pub skipped_files: usize,
    #[serde(default)]
    pub sensitive_files_skipped: usize,
    pub captured_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub id: String,
    pub name: Option<String>,
    pub kind: CheckpointKind,
    pub timestamp: String,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointManifest {
    version: u32,
    checkpoints: Vec<CheckpointSummary>,
}

impl Default for CheckpointManifest {
    fn default() -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            checkpoints: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RewindPreview {
    pub checkpoint_id: String,
    pub affected_files: Vec<PathBuf>,
    pub requires_confirmation: bool,
    pub snapshot_complete: bool,
    pub note: Option<String>,
}

#[derive(Clone)]
pub struct CheckpointStore {
    session_id: String,
    dir: PathBuf,
}

impl CheckpointStore {
    pub fn new(sessions_dir: &Path, session_id: impl Into<String>) -> Result<Self> {
        let session_id = session_id.into();
        let dir = sessions_dir.join(format!("{session_id}.checkpoints"));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create checkpoint directory {}", dir.display()))?;
        Ok(Self { session_id, dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn create(
        &self,
        cwd: &Path,
        messages: &[Message],
        kind: CheckpointKind,
        name: Option<String>,
    ) -> Result<CheckpointSummary> {
        let id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ"),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            id: id.clone(),
            session_id: self.session_id.clone(),
            name: name.clone(),
            kind: kind.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            messages: messages.to_vec(),
            workspace: capture_workspace(cwd)?,
        };
        write_json_atomic(&self.checkpoint_path(&id), &checkpoint)?;

        let summary = CheckpointSummary {
            id,
            name,
            kind,
            timestamp: checkpoint.timestamp,
            message_count: checkpoint.messages.len(),
        };
        let mut manifest = self.load_manifest().unwrap_or_default();
        manifest.checkpoints.push(summary.clone());
        write_json_atomic(&self.manifest_path(), &manifest)?;
        Ok(summary)
    }

    pub fn list(&self) -> Result<Vec<CheckpointSummary>> {
        Ok(self.load_manifest()?.checkpoints)
    }

    pub fn latest(&self) -> Result<Option<CheckpointSummary>> {
        Ok(self.list()?.into_iter().last())
    }

    pub fn load(&self, checkpoint_id: &str) -> Result<Checkpoint> {
        validate_checkpoint_id(checkpoint_id)?;
        let path = self.checkpoint_path(checkpoint_id);
        let bytes =
            std::fs::read(&path).with_context(|| format!("read checkpoint {}", path.display()))?;
        let checkpoint: Checkpoint = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse checkpoint {}", path.display()))?;
        anyhow::ensure!(
            checkpoint.session_id == self.session_id,
            "checkpoint belongs to another session"
        );
        Ok(checkpoint)
    }

    pub fn preview_files(&self, checkpoint_id: &str, cwd: &Path) -> Result<RewindPreview> {
        let checkpoint = self.load(checkpoint_id)?;
        preview_workspace(&checkpoint, cwd)
    }

    /// 写回 checkpoint 文件状态。调用方必须先展示 [`RewindPreview`]，只有用户
    /// 显式确认后才传 `confirmed = true`；无变化时不要求确认。
    pub fn restore_files(
        &self,
        checkpoint_id: &str,
        cwd: &Path,
        confirmed: bool,
    ) -> Result<RewindPreview> {
        let checkpoint = self.load(checkpoint_id)?;
        let preview = preview_workspace(&checkpoint, cwd)?;
        if preview.requires_confirmation && !confirmed {
            anyhow::bail!(
                "rewind affects {} file(s); explicit confirmation required",
                preview.affected_files.len()
            );
        }
        match &checkpoint.workspace {
            WorkspaceSnapshot::Git(snapshot) => restore_git(snapshot, &preview.affected_files)?,
            WorkspaceSnapshot::Files(snapshot) => {
                restore_files_snapshot(snapshot, &preview.affected_files)?
            }
        }
        Ok(preview)
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn load_manifest(&self) -> Result<CheckpointManifest> {
        let path = self.manifest_path();
        if !path.exists() {
            return Ok(CheckpointManifest::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
}

fn validate_checkpoint_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
        "invalid checkpoint id"
    );
    Ok(())
}

fn capture_workspace(cwd: &Path) -> Result<WorkspaceSnapshot> {
    if let Some(root) = git_root(cwd) {
        return Ok(WorkspaceSnapshot::Git(capture_git(&root)?));
    }
    Ok(WorkspaceSnapshot::Files(capture_files(cwd)?))
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        .canonicalize()
        .ok()
}

fn capture_git(root: &Path) -> Result<GitSnapshot> {
    let temp_index = std::env::temp_dir().join(format!(
        "wyj-checkpoint-index-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let head = git_output(root, None, &["rev-parse", "--verify", "HEAD"])
        .ok()
        .map(|value| value.trim().to_string());
    let read_tree_args: Vec<&str> = if head.is_some() {
        vec!["read-tree", "HEAD"]
    } else {
        vec!["read-tree", "--empty"]
    };
    let result = (|| {
        git_status(root, Some(&temp_index), &read_tree_args)?;
        git_status(root, Some(&temp_index), &["add", "-A", "--", "."])?;
        let tree = git_output(root, Some(&temp_index), &["write-tree"])?
            .trim()
            .to_string();
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(parent) = head.as_deref() {
            args.extend(["-p", parent]);
        }
        let commit = git_commit_tree(root, Some(&temp_index), &args)?;
        let files = git_tree_entries(root, &commit)?;
        Ok(GitSnapshot {
            repo_root: root.to_path_buf(),
            head,
            tree,
            commit,
            files,
        })
    })();
    let _ = std::fs::remove_file(&temp_index);
    result
}

fn git_commit_tree(root: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = git_command(root, index);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    command
        .env("GIT_AUTHOR_NAME", "wyj-code checkpoint")
        .env("GIT_AUTHOR_EMAIL", "checkpoint@wyj-code.local")
        .env("GIT_COMMITTER_NAME", "wyj-code checkpoint")
        .env("GIT_COMMITTER_EMAIL", "checkpoint@wyj-code.local");
    let mut child = command.spawn().context("spawn git commit-tree")?;
    child
        .stdin
        .take()
        .context("open git commit-tree stdin")?
        .write_all(b"wyj-code session checkpoint\n")?;
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "git commit-tree failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_tree_entries(root: &Path, commit: &str) -> Result<BTreeMap<PathBuf, GitFileEntry>> {
    let output = git_output_bytes(root, None, &["ls-tree", "-r", "-z", "--long", commit])?;
    let mut entries = BTreeMap::new();
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&record[..tab]);
        let mut fields = meta.split_whitespace();
        let mode = fields.next().unwrap_or_default().to_string();
        let object_type = fields.next().unwrap_or_default();
        let object_id = fields.next().unwrap_or_default().to_string();
        let size = fields.next().and_then(|value| value.parse().ok());
        if object_type != "blob" {
            continue;
        }
        let path = bytes_to_path(&record[tab + 1..]);
        if safe_relative(&path) {
            entries.insert(
                path,
                GitFileEntry {
                    mode,
                    object_id,
                    size,
                },
            );
        }
    }
    Ok(entries)
}

fn preview_workspace(checkpoint: &Checkpoint, cwd: &Path) -> Result<RewindPreview> {
    match &checkpoint.workspace {
        WorkspaceSnapshot::Git(snapshot) => {
            let current_root =
                git_root(cwd).context("current directory is no longer a git repo")?;
            anyhow::ensure!(
                current_root == snapshot.repo_root,
                "checkpoint belongs to a different git repository"
            );
            let current = capture_git(&current_root)?;
            let output = git_output_bytes(
                &current_root,
                None,
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    snapshot.commit.as_str(),
                    current.commit.as_str(),
                    "--",
                ],
            )?;
            let affected_files = output
                .split(|byte| *byte == 0)
                .filter(|record| !record.is_empty())
                .map(bytes_to_path)
                .filter(|path| safe_relative(path))
                .collect::<Vec<_>>();
            Ok(RewindPreview {
                checkpoint_id: checkpoint.id.clone(),
                requires_confirmation: !affected_files.is_empty(),
                affected_files,
                snapshot_complete: true,
                note: Some(
                    "Git index is not modified; external side effects are not restored".into(),
                ),
            })
        }
        WorkspaceSnapshot::Files(snapshot) => {
            let current = capture_files(&snapshot.root)?;
            let affected_files = changed_file_paths(snapshot, &current);
            Ok(RewindPreview {
                checkpoint_id: checkpoint.id.clone(),
                requires_confirmation: !affected_files.is_empty(),
                affected_files,
                snapshot_complete: snapshot.complete,
                note: (!snapshot.complete).then(|| {
                    if snapshot.sensitive_files_skipped > 0 {
                        format!(
                            "non-git snapshot excluded {} sensitive file(s) and/or exceeded limits; rewind covers captured files only",
                            snapshot.sensitive_files_skipped
                        )
                    } else {
                        "non-git snapshot exceeded limits; rewind covers captured files only".into()
                    }
                }),
            })
        }
    }
}

fn restore_git(snapshot: &GitSnapshot, affected: &[PathBuf]) -> Result<()> {
    for relative in affected {
        anyhow::ensure!(safe_relative(relative), "unsafe checkpoint path");
        let target = snapshot.repo_root.join(relative);
        anyhow::ensure!(
            target.starts_with(&snapshot.repo_root),
            "path escaped repository"
        );
        if snapshot.files.contains_key(relative) {
            let status = Command::new("git")
                .arg("-C")
                .arg(&snapshot.repo_root)
                .args(["restore", "--worktree", "--source"])
                .arg(&snapshot.commit)
                .arg("--")
                .arg(relative)
                .status()?;
            anyhow::ensure!(
                status.success(),
                "git restore failed for {}",
                relative.display()
            );
        } else if target.is_symlink() || target.is_file() {
            std::fs::remove_file(&target)?;
        } else if target.is_dir() {
            std::fs::remove_dir_all(&target)?;
        }
    }
    prune_empty_parents(&snapshot.repo_root, affected);
    Ok(())
}

fn capture_files(root: &Path) -> Result<FileSnapshot> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut files = BTreeMap::new();
    let mut captured_bytes = 0u64;
    let mut skipped_files = 0usize;
    let mut sensitive_files_skipped = 0usize;
    for entry in ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .build()
        .flatten()
    {
        let path = entry.path();
        if path == root || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = match path.strip_prefix(&root) {
            Ok(relative) if safe_relative(relative) => relative.to_path_buf(),
            _ => continue,
        };
        if sensitive_snapshot_path(&relative) {
            sensitive_files_skipped += 1;
            continue;
        }
        let bytes = std::fs::read(path)?;
        if files.len() >= NON_GIT_MAX_FILES
            || captured_bytes.saturating_add(bytes.len() as u64) > NON_GIT_MAX_BYTES
        {
            skipped_files += 1;
            continue;
        }
        captured_bytes += bytes.len() as u64;
        files.insert(
            relative,
            FileEntry {
                sha256: sha256(&bytes),
                bytes,
            },
        );
    }
    Ok(FileSnapshot {
        root,
        files,
        complete: skipped_files == 0 && sensitive_files_skipped == 0,
        skipped_files,
        sensitive_files_skipped,
        captured_bytes,
    })
}

fn sensitive_snapshot_path(path: &Path) -> bool {
    const SENSITIVE_DIRS: &[&str] = &[".ssh", ".aws", ".azure", ".kube", ".gnupg", ".wyj-code"];
    if path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| SENSITIVE_DIRS.contains(&name))
    }) {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name.as_str(),
            ".netrc"
                | "credentials"
                | "credentials.json"
                | "id_rsa"
                | "id_ed25519"
                | "secrets.toml"
                | "secrets.json"
                | "secrets.yaml"
                | "secrets.yml"
        )
        || ["pem", "key", "p12", "pfx", "kdbx"]
            .iter()
            .any(|extension| path.extension().and_then(|value| value.to_str()) == Some(extension))
}

fn changed_file_paths(before: &FileSnapshot, after: &FileSnapshot) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    paths.extend(before.files.keys().cloned());
    paths.extend(after.files.keys().cloned());
    paths
        .into_iter()
        .filter(|path| {
            before.files.get(path).map(|entry| &entry.sha256)
                != after.files.get(path).map(|entry| &entry.sha256)
        })
        .collect()
}

fn restore_files_snapshot(snapshot: &FileSnapshot, affected: &[PathBuf]) -> Result<()> {
    for relative in affected {
        anyhow::ensure!(safe_relative(relative), "unsafe checkpoint path");
        let target = snapshot.root.join(relative);
        anyhow::ensure!(
            target.starts_with(&snapshot.root),
            "path escaped snapshot root"
        );
        match snapshot.files.get(relative) {
            Some(entry) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, &entry.bytes)?;
            }
            None if target.is_symlink() || target.is_file() => std::fs::remove_file(target)?,
            None if target.is_dir() => std::fs::remove_dir_all(target)?,
            None => {}
        }
    }
    prune_empty_parents(&snapshot.root, affected);
    Ok(())
}

fn prune_empty_parents(root: &Path, affected: &[PathBuf]) {
    let mut directories = affected
        .iter()
        .filter_map(|path| path.parent())
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    for directory in directories {
        if directory != root {
            let _ = std::fs::remove_dir(&directory);
        }
    }
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn git_command(root: &Path, index: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
}

fn git_status(root: &Path, index: Option<&Path>, args: &[&str]) -> Result<()> {
    let output = git_command(root, index).args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn git_output(root: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_output_bytes(root, index, args)?)?)
}

fn git_output_bytes(root: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let output = git_command(root, index).args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let json = serde_json::to_string(value)?;
    let bytes = crate::secret::redact_sensitive_text(&json).into_bytes();
    if let Err(error) = std::fs::write(&temp, bytes) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_checkpoint_preserves_real_index_and_requires_confirmation() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(
            repo.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repo.path().join("tracked.txt"), "base").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        git(repo.path(), &["commit", "-m", "base"]);
        std::fs::write(repo.path().join("staged.txt"), "staged").unwrap();
        git(repo.path(), &["add", "staged.txt"]);
        let index_before = std::fs::read(repo.path().join(".git/index")).unwrap();

        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s1").unwrap();
        let checkpoint = store
            .create(
                repo.path(),
                &[],
                CheckpointKind::Manual,
                Some("before".into()),
            )
            .unwrap();
        assert_eq!(
            index_before,
            std::fs::read(repo.path().join(".git/index")).unwrap()
        );

        std::fs::write(repo.path().join("tracked.txt"), "changed").unwrap();
        std::fs::write(repo.path().join("new.txt"), "new").unwrap();
        let preview = store.preview_files(&checkpoint.id, repo.path()).unwrap();
        assert!(preview.requires_confirmation);
        assert!(preview
            .affected_files
            .contains(&PathBuf::from("tracked.txt")));
        assert!(preview.affected_files.contains(&PathBuf::from("new.txt")));
        assert!(store
            .restore_files(&checkpoint.id, repo.path(), false)
            .is_err());
        store
            .restore_files(&checkpoint.id, repo.path(), true)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "base"
        );
        assert!(!repo.path().join("new.txt").exists());
        assert_eq!(
            index_before,
            std::fs::read(repo.path().join(".git/index")).unwrap()
        );
    }

    #[test]
    fn non_git_checkpoint_restores_captured_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "one").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s1").unwrap();
        let checkpoint = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        std::fs::write(root.path().join("a.txt"), "two").unwrap();
        store
            .restore_files(&checkpoint.id, root.path(), true)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn both_rewind_restores_files_and_checkpoint_conversation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "checkpoint").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s1").unwrap();
        let before = vec![Message::user("before")];
        let summary = store
            .create(root.path(), &before, CheckpointKind::Manual, None)
            .unwrap();

        std::fs::write(root.path().join("a.txt"), "after").unwrap();
        let mut conversation = vec![Message::user("after")];
        let checkpoint = store.load(&summary.id).unwrap();
        store.restore_files(&summary.id, root.path(), true).unwrap();
        conversation = checkpoint.messages;

        assert_eq!(
            serde_json::to_value(&conversation).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "checkpoint"
        );
    }

    #[test]
    fn checkpoint_messages_are_redacted_before_persistence() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "secret-session").unwrap();
        let secret = format!("{}{}", "sk-test-", "E".repeat(24));
        let summary = store
            .create(
                root.path(),
                &[Message::user(format!("credential: {secret}"))],
                CheckpointKind::Manual,
                None,
            )
            .unwrap();
        let raw = std::fs::read_to_string(store.checkpoint_path(&summary.id)).unwrap();
        assert!(!raw.contains(&secret));
        assert!(raw.contains(crate::secret::REDACTED_SECRET));
    }

    #[test]
    fn non_git_checkpoint_excludes_common_secret_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("safe.txt"), "safe").unwrap();
        std::fs::write(root.path().join(".env"), "credential").unwrap();
        std::fs::create_dir_all(root.path().join(".ssh")).unwrap();
        std::fs::write(root.path().join(".ssh/id_ed25519"), "credential").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "sensitive-files").unwrap();
        let summary = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        let checkpoint = store.load(&summary.id).unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!("expected non-git snapshot");
        };
        assert!(snapshot.files.contains_key(Path::new("safe.txt")));
        assert!(!snapshot.files.contains_key(Path::new(".env")));
        assert!(!snapshot.files.contains_key(Path::new(".ssh/id_ed25519")));
        assert_eq!(snapshot.sensitive_files_skipped, 2);
        assert!(!snapshot.complete);
    }
}
