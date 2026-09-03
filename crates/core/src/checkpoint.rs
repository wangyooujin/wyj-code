//! 会话 Checkpoint / Rewind：保存对话边界与工作区文件状态，不修改真实 Git index。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use wyj_api::types::Message;

use crate::workspace_cas::WorkspaceCas;

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
            WorkspaceSnapshot::Delta(snapshot) => &snapshot.root,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSnapshot {
    Git(GitSnapshot),
    /// 完整 snapshot(baseline 节点 / 切换 cwd 后第一个 checkpoint)
    Files(FileSnapshot),
    /// 相对父 checkpoint 的 structural diff(Phase 2 优化)。
    /// restore 时沿 parent_id 链向上折叠直至 baseline。
    Delta(DeltaSnapshot),
}

/// Delta snapshot:只存相对父 checkpoint 的文件级 diff(Added/Removed/Modified)。
/// Unchanged 不入 ops。restore 时通过 `resolve_snapshot_chain` 沿父链折叠。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaSnapshot {
    pub root: PathBuf,
    /// 父 checkpoint id;链上直到 baseline。
    pub parent_checkpoint_id: String,
    /// path -> op;用 BTreeMap 便于 deterministic 序列化。
    pub ops: BTreeMap<PathBuf, DeltaOp>,
    pub captured_bytes: u64,
    pub skipped_files: usize,
    #[serde(default)]
    pub sensitive_files_skipped: usize,
    /// baseline 后该 chain 的实际文件总数(用于完整性判定)。
    pub baseline_file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DeltaOp {
    Added {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inline: Vec<u8>,
        size: u64,
        sha256: String,
    },
    Removed,
    Modified {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inline: Vec<u8>,
        size: u64,
        sha256: String,
    },
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
    /// CAS 内容寻址哈希(SHA-256 hex)。None 表示 inline_bytes 路径(空文件 /
    /// 超阈值文件 / CAS root 不可写 fallback)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// 仅 hash == None 时使用:超阈值的内联字节或空文件。
    /// 老 v1 文件直接含 `bytes: Vec<u8>` 字段,通过 `#[serde(default, alias)]` 兼容读。
    #[serde(default, alias = "bytes", skip_serializing_if = "Vec::is_empty")]
    pub inline_bytes: Vec<u8>,
    #[serde(default)]
    pub size: u64,
    /// SHA-256 hex —— 沿用旧字段供 `changed_file_paths` 做 structural diff
    /// (Phase 2 delta 用),不重新计算。
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub id: String,
    pub name: Option<String>,
    pub kind: CheckpointKind,
    pub timestamp: String,
    pub message_count: usize,
    /// Phase 2:cwd canonicalize 后的绝对路径,用于 pick_baseline 同 cwd 优先
    #[serde(default)]
    pub cwd_root: String,
    /// Phase 2:若本 checkpoint 是 Delta 形式,指向其父 baseline checkpoint id;
    /// baseline 节点或 Git snapshot 时为 None。
    #[serde(default)]
    pub baseline_id: Option<String>,
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
    /// 单 session checkpoint 上限(builder 注入;0 = 不限)。
    max_per_session: usize,
    /// 可选 CAS 池。capture_files / restore_files_snapshot 走 CAS 减少 checkpoint 体积。
    /// None 表示不接 CAS(测试或回滚场景),全部 inline_bytes。
    cas: Option<Arc<WorkspaceCas>>,
}

impl CheckpointStore {
    pub fn new(sessions_dir: &Path, session_id: impl Into<String>) -> Result<Self> {
        let session_id = session_id.into();
        let dir = sessions_dir.join(format!("{session_id}.checkpoints"));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create checkpoint directory {}", dir.display()))?;
        Ok(Self {
            session_id,
            dir,
            max_per_session: 0,
            cas: None,
        })
    }

    /// 注入单 session checkpoint 上限(0 = 不限)。`create()` 末尾会自动调
    /// `enforce_retention()` 淘汰超限最老 checkpoint + 同步 manifest。
    pub fn with_max_per_session(mut self, max: usize) -> Self {
        self.max_per_session = max;
        self
    }

    /// 注入 CAS 池(builder 模式)。CAS root 不可用时 capture_files 走 inline 兜底,
    /// 不阻断 checkpoint 写入。
    pub fn with_cas(mut self, cas: Arc<WorkspaceCas>) -> Self {
        self.cas = Some(cas);
        self
    }

    pub fn cas(&self) -> Option<&WorkspaceCas> {
        self.cas.as_deref()
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
        let cwd_root = cwd
            .canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .display()
            .to_string();
        let truncated_messages: Vec<Message> = {
            // 落盘前做持久化截断(tool_result / thinking / tool_use.input)
            let mut owned = messages.to_vec();
            if let Some(cfg) = current_checkpoint_persist_cap() {
                for msg in &mut owned {
                    for block in &mut msg.content {
                        crate::serialize::truncate_content_block(block, &cfg);
                    }
                }
            }
            owned
        };
        let workspace = self.capture_workspace_with_delta(&cwd_root, cwd)?;
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            id: id.clone(),
            session_id: self.session_id.clone(),
            name: name.clone(),
            kind: kind.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            messages: truncated_messages,
            workspace,
        };
        write_json_atomic(&self.checkpoint_path(&id), &checkpoint)?;

        let baseline_id = match &checkpoint.workspace {
            WorkspaceSnapshot::Delta(delta) => Some(delta.parent_checkpoint_id.clone()),
            _ => None,
        };
        let summary = CheckpointSummary {
            id,
            name,
            kind,
            timestamp: checkpoint.timestamp,
            message_count: checkpoint.messages.len(),
            cwd_root,
            baseline_id,
        };
        let mut manifest = self.load_manifest().unwrap_or_default();
        manifest.checkpoints.push(summary.clone());
        write_json_atomic(&self.manifest_path(), &manifest)?;
        if let Err(error) = self.enforce_retention() {
            tracing::warn!("Checkpoint retention 失败: {error}");
        }
        Ok(summary)
    }

    /// Phase 2:capture 完整 snapshot → 查 manifest 选 baseline → 写 Delta
    /// 或 Files。Git 路径总是写 Git snapshot(已是最优,无需 delta)。
    fn capture_workspace_with_delta(
        &self,
        cwd_root: &str,
        cwd: &Path,
    ) -> Result<WorkspaceSnapshot> {
        let snapshot = capture_workspace(cwd, self.cas.as_deref())?;
        // Git snapshot 永远全量(轻量,object_id 引用)
        if matches!(snapshot, WorkspaceSnapshot::Git(_)) {
            return Ok(snapshot);
        }
        let WorkspaceSnapshot::Files(after) = snapshot else {
            unreachable!("non-Git capture_workspace must yield Files")
        };
        // 尝试 Delta:选同 cwd 的最近 baseline 节点
        let manifest = self.load_manifest().unwrap_or_default();
        let Some(parent_id) = pick_baseline(&manifest, cwd_root) else {
            // 无祖先 baseline → 写完整 Files snapshot
            return Ok(WorkspaceSnapshot::Files(after));
        };
        // 加载父 checkpoint 的完整 file snapshot
        let parent_cp = match self.load(&parent_id) {
            Ok(cp) => cp,
            Err(error) => {
                tracing::warn!(
                    "parent baseline checkpoint {} 加载失败: {error} —— 降级为完整 snapshot",
                    parent_id
                );
                return Ok(WorkspaceSnapshot::Files(after));
            }
        };
        let before = match resolve_to_files_caller(&self.dir, &parent_cp) {
            Ok(s) => s,
            Err(error) => {
                tracing::warn!(
                    "parent checkpoint {} 沿父链折叠失败: {error} —— 降级为完整 snapshot",
                    parent_id
                );
                return Ok(WorkspaceSnapshot::Files(after));
            }
        };
        let ops = diff_files(&before, &after);
        Ok(WorkspaceSnapshot::Delta(DeltaSnapshot {
            root: after.root.clone(),
            parent_checkpoint_id: parent_id,
            ops,
            captured_bytes: after.captured_bytes,
            skipped_files: after.skipped_files,
            sensitive_files_skipped: after.sensitive_files_skipped,
            baseline_file_count: after.files.len(),
        }))
    }

    /// 按 manifest 中 timestamp 升序淘汰最老 checkpoint + 同步 manifest。
    /// `max_per_session == 0` 时跳过。失败仅 `tracing::warn`,不污染主流程。
    pub fn enforce_retention(&self) -> Result<()> {
        if self.max_per_session == 0 {
            return Ok(());
        }
        let mut manifest = self.load_manifest().unwrap_or_default();
        if manifest.checkpoints.len() <= self.max_per_session {
            return Ok(());
        }
        // 按 timestamp 升序,保留后 N 条(最新)
        manifest
            .checkpoints
            .sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        let to_remove: Vec<String> = manifest
            .checkpoints
            .drain(..manifest.checkpoints.len() - self.max_per_session)
            .map(|c| c.id)
            .collect();
        for id in &to_remove {
            validate_checkpoint_id(id)?;
            let path = self.checkpoint_path(id);
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!("删除旧 checkpoint 失败 {}: {error}", path.display());
            }
        }
        write_json_atomic(&self.manifest_path(), &manifest)?;
        Ok(())
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
        preview_workspace(&checkpoint, cwd, self.cas.as_deref(), &self.dir)
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
        let preview = preview_workspace(&checkpoint, cwd, self.cas.as_deref(), &self.dir)?;
        if preview.requires_confirmation && !confirmed {
            anyhow::bail!(
                "rewind affects {} file(s); explicit confirmation required",
                preview.affected_files.len()
            );
        }
        match &checkpoint.workspace {
            WorkspaceSnapshot::Git(snapshot) => restore_git(snapshot, &preview.affected_files)?,
            WorkspaceSnapshot::Files(snapshot) => {
                restore_files_snapshot(snapshot, &preview.affected_files, self.cas.as_deref())?
            }
            WorkspaceSnapshot::Delta(_) => {
                // Delta:折叠到完整 snapshot 再 restore
                let resolved = resolve_to_files_caller(&self.dir, &checkpoint)?;
                restore_files_snapshot(&resolved, &preview.affected_files, self.cas.as_deref())?
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

fn capture_workspace(cwd: &Path, cas: Option<&WorkspaceCas>) -> Result<WorkspaceSnapshot> {
    if let Some(root) = git_root(cwd) {
        return Ok(WorkspaceSnapshot::Git(capture_git(&root)?));
    }
    Ok(WorkspaceSnapshot::Files(capture_files(cwd, cas)?))
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

fn preview_workspace(
    checkpoint: &Checkpoint,
    cwd: &Path,
    cas: Option<&WorkspaceCas>,
    self_dir: &Path,
) -> Result<RewindPreview> {
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
            let current = capture_files(&snapshot.root, cas)?;
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
        WorkspaceSnapshot::Delta(_delta) => {
            // Delta checkpoint 的 preview:沿父链折叠到完整 snapshot 后再 diff
            // 当前 live 状态。注意折叠已读盘 N 次,O(N) 延迟。
            let resolved = resolve_to_files_caller(self_dir, checkpoint)?;
            let current = capture_files(&resolved.root, cas)?;
            let affected_files = changed_file_paths(&resolved, &current);
            Ok(RewindPreview {
                checkpoint_id: checkpoint.id.clone(),
                requires_confirmation: !affected_files.is_empty(),
                affected_files,
                snapshot_complete: resolved.complete,
                note: Some("delta snapshot resolved across parent chain".into()),
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

fn capture_files(root: &Path, cas: Option<&WorkspaceCas>) -> Result<FileSnapshot> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut files = BTreeMap::new();
    let mut captured_bytes = 0u64;
    let mut skipped_files = 0usize;
    let mut sensitive_files_skipped = 0usize;
    let max_blob = cas.map(|c| c.max_blob_bytes()).unwrap_or(0);
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
        let sha = sha256(&bytes);
        let size = bytes.len() as u64;
        // CAS 路径:空文件 / 超阈值文件走 inline_bytes;其余走 cas.intern。
        // CAS root 不可用(cas == None)时全部走 inline 兜底,不阻断 checkpoint。
        let (hash, inline) = match cas {
            None => (None, bytes),
            Some(_) if bytes.is_empty() || size > max_blob => (None, bytes),
            Some(c) => match c.intern(&bytes) {
                Ok(h) => (Some(h), Vec::new()),
                Err(error) => {
                    tracing::warn!(
                        "CAS intern 失败({}),文件 {} 走 inline 路径: {error}",
                        c.root().display(),
                        relative.display()
                    );
                    (None, bytes)
                }
            },
        };
        files.insert(
            relative,
            FileEntry {
                hash,
                inline_bytes: inline,
                size,
                sha256: sha,
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

/// Phase 2:从 manifest 中选祖先 baseline 节点。
/// 优先同 cwd 的最近 checkpoint;切换 cwd 强制写完整 snapshot。
/// 仅选择本身是 baseline(baseline_id == None)且 cwd 匹配的节点。
fn pick_baseline(manifest: &CheckpointManifest, cwd_root: &str) -> Option<String> {
    manifest
        .checkpoints
        .iter()
        .rev() // 最新优先
        .find(|c| c.cwd_root == cwd_root && c.baseline_id.is_none())
        .map(|c| c.id.clone())
}

/// Phase 2:沿 parent_checkpoint_id 链向上折叠 Delta 直至 baseline(Files)。
/// 用于 restore / preview 时的 on-demand 重建。
/// 限制最多向上追 20 层(防止祖先链被删导致的链爆炸)。
/// `self_dir` 是 CheckpointStore 的 dir(),用于定位父 checkpoint JSON 文件。
pub(crate) fn resolve_to_files_caller(
    self_dir: &Path,
    checkpoint: &Checkpoint,
) -> Result<FileSnapshot> {
    const MAX_CHAIN_DEPTH: usize = 20;
    let mut current = checkpoint.clone();
    let mut chain: Vec<DeltaSnapshot> = Vec::new();
    for _ in 0..MAX_CHAIN_DEPTH {
        match &current.workspace {
            WorkspaceSnapshot::Files(files) => {
                let mut result = files.clone();
                for delta in chain.iter().rev() {
                    apply_delta(&mut result, delta)?;
                }
                return Ok(result);
            }
            WorkspaceSnapshot::Delta(delta) => {
                let parent_id = delta.parent_checkpoint_id.clone();
                chain.push(delta.clone());
                let path = self_dir.join(format!("{parent_id}.json"));
                let bytes = std::fs::read(&path)
                    .with_context(|| format!("read parent checkpoint {}", path.display()))?;
                let cp: Checkpoint = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse parent checkpoint {}", path.display()))?;
                current = cp;
            }
            WorkspaceSnapshot::Git(_) => {
                anyhow::bail!("cannot resolve Delta chain ending at Git snapshot");
            }
        }
    }
    anyhow::bail!("Delta chain exceeds {MAX_CHAIN_DEPTH} levels")
}

/// 把 `DeltaSnapshot.ops` 应用到 `files` 视图上,返回该 delta 时刻的完整快照。
fn apply_delta(files: &mut FileSnapshot, delta: &DeltaSnapshot) -> Result<()> {
    files.captured_bytes = delta.captured_bytes;
    files.skipped_files = delta.skipped_files;
    files.sensitive_files_skipped = delta.sensitive_files_skipped;
    for (path, op) in &delta.ops {
        match op {
            DeltaOp::Added { hash, inline, size, sha256 } | DeltaOp::Modified { hash, inline, size, sha256 } => {
                files.files.insert(
                    path.clone(),
                    FileEntry {
                        hash: hash.clone(),
                        inline_bytes: inline.clone(),
                        size: *size,
                        sha256: sha256.clone(),
                    },
                );
            }
            DeltaOp::Removed => {
                files.files.remove(path);
            }
        }
    }
    Ok(())
}
/// Unchanged 不入 ops(由 restore 时继承父状态)。
fn diff_files(
    before: &FileSnapshot,
    after: &FileSnapshot,
) -> BTreeMap<PathBuf, DeltaOp> {
    let mut ops = BTreeMap::new();
    let all: BTreeSet<&PathBuf> = before.files.keys().chain(after.files.keys()).collect();
    for path in all {
        let b = before.files.get(path);
        let a = after.files.get(path);
        match (b, a) {
            (None, Some(e)) => {
                ops.insert(
                    path.clone(),
                    DeltaOp::Added {
                        hash: e.hash.clone(),
                        inline: e.inline_bytes.clone(),
                        size: e.size,
                        sha256: e.sha256.clone(),
                    },
                );
            }
            (Some(_), None) => {
                ops.insert(path.clone(), DeltaOp::Removed);
            }
            (Some(be), Some(ae)) if be.sha256 != ae.sha256 => {
                ops.insert(
                    path.clone(),
                    DeltaOp::Modified {
                        hash: ae.hash.clone(),
                        inline: ae.inline_bytes.clone(),
                        size: ae.size,
                        sha256: ae.sha256.clone(),
                    },
                );
            }
            _ => {} // Unchanged:由 restore 时继承
        }
    }
    ops
}

fn restore_files_snapshot(
    snapshot: &FileSnapshot,
    affected: &[PathBuf],
    cas: Option<&WorkspaceCas>,
) -> Result<()> {
    for relative in affected {
        anyhow::ensure!(safe_relative(relative), "unsafe checkpoint path");
        let target = snapshot.root.join(relative);
        anyhow::ensure!(
            target.starts_with(&snapshot.root),
            "path escaped snapshot root"
        );
        match snapshot.files.get(relative) {
            Some(entry) => {
                // CAS 路径:优先 hash 走 cas.get;fallback 到 inline_bytes(兼容老 v1
                // 直接含 bytes 的 checkpoint)。
                let bytes = match (&entry.hash, cas) {
                    (Some(hash), Some(c)) => match c.get(hash) {
                        Ok(b) => b,
                        Err(error) => {
                            tracing::warn!(
                                "CAS get 失败 (hash={hash}): {error} —— 跳过此文件恢复"
                            );
                            continue;
                        }
                    },
                    _ => entry.inline_bytes.clone(),
                };
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, &bytes)?;
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

/// 进程内全局 `PersistCapCfg` —— 由 CLI 装配阶段 `set_checkpoint_persist_cap`
/// 注入,`CheckpointStore::create` 落盘前对 `messages` 做截断。
static CHECKPOINT_PERSIST_CAP: std::sync::OnceLock<wyj_config::PersistCapCfg> =
    std::sync::OnceLock::new();

fn current_checkpoint_persist_cap() -> Option<wyj_config::PersistCapCfg> {
    CHECKPOINT_PERSIST_CAP.get().cloned()
}

/// CLI 装配阶段调用一次,注入当前用户的 `cfg.persist_cap`。
pub fn set_checkpoint_persist_cap(cfg: wyj_config::PersistCapCfg) {
    let _ = CHECKPOINT_PERSIST_CAP.set(cfg);
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

    /// M1 验证:CAS 路径下,200 个相同内容文件的 checkpoint JSON 体积应 < 50KB,
    /// 老 inline_bytes 路径下应是几 MB 量级。证明内容寻址去重生效。
    #[test]
    fn cas_dedupe_dramatically_reduces_checkpoint_size() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024)
                .unwrap(),
        );

        // 构造 200 个文件,每个 5KB 唯一内容(但全部相同) → 1 MB 总
        let payload = vec![b'x'; 5 * 1024];
        for i in 0..200 {
            std::fs::write(root.path().join(format!("file_{i:03}.txt")), &payload).unwrap();
        }

        let store = CheckpointStore::new(sessions.path(), "cas-test")
            .unwrap()
            .with_cas(cas.clone());
        let summary = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();

        let cp_path = store.checkpoint_path(&summary.id);
        let size = std::fs::metadata(&cp_path).unwrap().len();

        // CAS 去重后,checkpoint JSON 应该只含 hash + size + sha256,200 文件应 < 50KB
        // (实际: 200 * (64 + 20 + 64) ≈ 30KB)
        assert!(
            size < 50_000,
            "CAS checkpoint 应该 < 50KB,实际 {size} bytes (说明 CAS 没生效)"
        );

        // CAS 应只有 1 个 blob(ref_count=200)
        let stats = cas.stats().unwrap();
        assert_eq!(stats.total_blobs, 1, "CAS 应去重到 1 个 blob");
        assert_eq!(stats.total_bytes, 5 * 1024);
        assert_eq!(stats.orphan_blobs, 0);
    }

    /// M1 验证:不接 CAS 时,checkpoint 仍走 inline 路径(向后兼容旧 build)。
    #[test]
    fn no_cas_path_keeps_inline_bytes() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "hello").unwrap();
        let store = CheckpointStore::new(sessions.path(), "inline-test").unwrap();
        let summary = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        let checkpoint = store.load(&summary.id).unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!("expected Files snapshot");
        };
        let entry = snapshot.files.get(Path::new("a.txt")).unwrap();
        assert!(entry.hash.is_none(), "无 CAS 时 hash 应为 None");
        assert_eq!(entry.inline_bytes, b"hello");
        assert_eq!(entry.size, 5);
    }

    /// M1 兼容验证:老 v1.5.10 checkpoint JSON(含顶层 `bytes: Vec<u8>` 字段)
    /// 仍能加载,且 `inline_bytes` 字段被填充。
    #[test]
    fn load_legacy_v1_checkpoint_with_bytes_field() {
        let sessions = tempfile::tempdir().unwrap();
        let dir = sessions.path().join("legacy-sess.checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        // 手工写一份 v1.5.10 格式的 checkpoint(顶层 FileEntry.bytes)
        let legacy_json = r#"{
            "version": 1,
            "id": "legacy-cp",
            "session_id": "legacy-sess",
            "name": null,
            "kind": "manual",
            "timestamp": "2026-09-01T00:00:00Z",
            "messages": [],
            "workspace": {
                "kind": "files",
                "root": "/tmp",
                "files": {
                    "file.txt": {
                        "bytes": [104, 105],
                        "sha256": "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"
                    }
                },
                "complete": true,
                "skipped_files": 0,
                "sensitive_files_skipped": 0,
                "captured_bytes": 2
            }
        }"#;
        std::fs::write(dir.join("legacy-cp.json"), legacy_json).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"version":1,"checkpoints":[{"id":"legacy-cp","name":null,"kind":"manual","timestamp":"2026-09-01T00:00:00Z","message_count":0}]}"#,
        )
        .unwrap();

        let store = CheckpointStore::new(sessions.path(), "legacy-sess").unwrap();
        let checkpoint = store.load("legacy-cp").unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!("expected Files snapshot");
        };
        let entry = snapshot.files.get(Path::new("file.txt")).unwrap();
        assert!(entry.hash.is_none());
        assert_eq!(entry.inline_bytes, vec![104, 105]); // "hi"
        assert_eq!(entry.size, 0); // 老字段没 size,新结构默认 0
        assert_eq!(entry.sha256, "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4");
    }

    /// M2 验证:相邻 checkpoint 自动写 Delta 形式,且 Delta 体积 << 完整 Files。
    #[test]
    fn adjacent_checkpoints_write_delta_form() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024)
                .unwrap(),
        );
        // 100 文件 baseline 内容
        for i in 0..100 {
            std::fs::write(
                root.path().join(format!("f{i:03}.txt")),
                format!("baseline content for file {i}"),
            )
            .unwrap();
        }
        let store = CheckpointStore::new(sessions.path(), "delta-test")
            .unwrap()
            .with_cas(cas);
        // 第一个:必须是 Files (baseline)
        let s1 = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        let cp1 = store.load(&s1.id).unwrap();
        assert!(matches!(cp1.workspace, WorkspaceSnapshot::Files(_)));
        // 改 1 个文件 → 第二个应是 Delta
        std::fs::write(
            root.path().join("f005.txt"),
            "MODIFIED content for file 5",
        )
        .unwrap();
        let s2 = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        let cp2 = store.load(&s2.id).unwrap();
        let WorkspaceSnapshot::Delta(delta) = &cp2.workspace else {
            panic!("expected Delta, got {:?}", std::mem::discriminant(&cp2.workspace));
        };
        assert_eq!(delta.parent_checkpoint_id, s1.id);
        // 改动只有 1 个,ops 应只 1 项
        assert_eq!(delta.ops.len(), 1, "只改 1 文件,ops 应只 1 项");
        assert!(matches!(
            delta.ops.values().next().unwrap(),
            DeltaOp::Modified { .. }
        ));
    }

    /// M2 验证:Delta 链可被 resolve_to_files_caller 折叠回完整 FileSnapshot。
    #[test]
    fn delta_chain_resolves_to_full_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024)
                .unwrap(),
        );
        for i in 0..50 {
            std::fs::write(
                root.path().join(format!("x{i:03}.txt")),
                format!("v0 content {i}"),
            )
            .unwrap();
        }
        let store = CheckpointStore::new(sessions.path(), "delta-resolve")
            .unwrap()
            .with_cas(cas);
        // baseline
        let s0 = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        // 3 次连续小改动 → 3 个 delta
        for round in 1..=3 {
            for i in 0..3 {
                std::fs::write(
                    root.path().join(format!("x{i:03}.txt")),
                    format!("v{round} content {i}"),
                )
                .unwrap();
            }
            store
                .create(root.path(), &[], CheckpointKind::Manual, None)
                .unwrap();
        }
        // 拿最后(第 4 个)checkpoint,应能 resolve 到完整 snapshot
        let manifest_entries = store.list().unwrap();
        let last = manifest_entries.last().unwrap();
        let last_cp = store.load(&last.id).unwrap();
        let resolved = resolve_to_files_caller(&store.dir, &last_cp).unwrap();
        // 应有 50 个文件,内容为 v3
        assert_eq!(resolved.files.len(), 50);
        let entry = resolved.files.get(Path::new("x000.txt")).unwrap();
        assert_eq!(entry.sha256, sha256(b"v3 content 0"));
        // 路径一致性
        assert!(!s0.id.is_empty()); // 防 unused warning
    }

    /// M2 验证:切 cwd 后第一个 checkpoint 必须写完整 Files(baseline),
    /// 不会跨 cwd 共享 delta 链。
    #[test]
    fn cwd_change_writes_fresh_baseline() {
        let sessions = tempfile::tempdir().unwrap();
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        std::fs::write(root_a.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(root_b.path().join("b.txt"), "beta").unwrap();
        let store = CheckpointStore::new(sessions.path(), "cwd-test").unwrap();
        let s_a = store
            .create(root_a.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        assert!(matches!(
            store.load(&s_a.id).unwrap().workspace,
            WorkspaceSnapshot::Files(_)
        ));
        // 切到 root_b
        let s_b = store
            .create(root_b.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        assert!(
            matches!(store.load(&s_b.id).unwrap().workspace, WorkspaceSnapshot::Files(_)),
            "切换 cwd 后必须写 Files (baseline),不允许跨 cwd delta"
        );
    }

    /// M2 验证:Delta 形式的 restore 仍能正确还原文件。
    /// 注意 Files/Delta 分支的 preview 用的是 `snapshot.root`(创建 checkpoint 时的根),
    /// 不是 `cwd`。所以测试用同一个目录作"工作区",在第二个 create 之前
    /// 改一个文件,Delta 应能记录这一改动,restore 走 CAS.get 还原。
    #[test]
    fn delta_restore_files_works_end_to_end() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024)
                .unwrap(),
        );
        for i in 0..10 {
            std::fs::write(
                root.path().join(format!("d{i:02}.txt")),
                format!("delta content {i}"),
            )
            .unwrap();
        }
        let store = CheckpointStore::new(sessions.path(), "delta-restore")
            .unwrap()
            .with_cas(cas);
        // 1) baseline
        let _s1 = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        // 2) 改 d03.txt 触发 Delta
        std::fs::write(
            root.path().join("d03.txt"),
            "delta content 3 MODIFIED",
        )
        .unwrap();
        let s2 = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        // 3) 把 d03.txt 改成"非 MODIFIED 的脏内容",然后 preview 应识别差异
        std::fs::write(root.path().join("d03.txt"), "USER_DIRTY").unwrap();
        let preview = store.preview_files(&s2.id, root.path()).unwrap();
        assert!(
            preview.affected_files.iter().any(|p| p.to_str() == Some("d03.txt")),
            "preview 必须识别 d03.txt 的脏内容"
        );
        // 4) restore:confirmed=true 跳过确认,验证 d03.txt 还原成 MODIFIED
        store
            .restore_files(&s2.id, root.path(), true)
            .unwrap();
        let restored = std::fs::read_to_string(root.path().join("d03.txt")).unwrap();
        assert_eq!(restored, "delta content 3 MODIFIED");
    }

    /// M1 验证:大文件 (>max_blob_bytes) 走 inline 而不进 CAS。
    #[test]
    fn cas_inline_above_threshold() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        // max_blob = 1KB
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 1024).unwrap(),
        );
        // 2KB 文件 > 1KB 阈值 → 走 inline
        std::fs::write(root.path().join("big.txt"), vec![b'y'; 2048]).unwrap();
        let store = CheckpointStore::new(sessions.path(), "threshold-test")
            .unwrap()
            .with_cas(cas.clone());
        let summary = store
            .create(root.path(), &[], CheckpointKind::Manual, None)
            .unwrap();
        let checkpoint = store.load(&summary.id).unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!();
        };
        let entry = snapshot.files.get(Path::new("big.txt")).unwrap();
        assert!(entry.hash.is_none(), "超阈值文件 hash 应为 None");
        assert_eq!(entry.inline_bytes.len(), 2048);
        let stats = cas.stats().unwrap();
        assert_eq!(stats.total_blobs, 0, "超阈值不应进 CAS");
    }
}
