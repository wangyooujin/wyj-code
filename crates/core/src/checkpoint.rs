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
    /// 触顶推理轮数上限、被迫中止**之前**自动落的工作点存档。
    ///
    /// 它和 [`CheckpointKind::AutoUser`] 的分工是：AutoUser 存的是「这次用户
    /// 提交**开始前**」的状态，所以 `/rewind` 回 AutoUser 等于把这一轮几十次
    /// 工具调用全部推倒重来——对 reasoning 模型（上限仅 32 轮）来说这可能就是一
    /// 个半小时的工作。而本变体存的是「撞上限的那一刻已经做成什么样」，让用户
    /// 能 `/rewind` 回到最后一步继续，而不是回到起点。
    TurnLimit,
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
    /// 创建者 pid。`enforce_retention` 据此跳过**别的活跃进程**的 checkpoint。
    ///
    /// 没有这个字段时，同一 session 被两个 wyj-code resume 会互相删对方的
    /// checkpoint：A 刚建好、正要 `/rewind` 回去的那一个，被 B 的保留策略淘汰，
    /// 表现为「刚才还在的回滚点突然没了」，且无法从数据侧解释。
    #[serde(default)]
    pub owner_pid: u32,
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

/// 进程内 checkpoint 构造配置,由 CLI 装配阶段 `set_checkpoint_config` 注入,
/// `CheckpointStore::configured` 读取。
///
/// 存在的理由:TUI 侧有 7 处用裸 `CheckpointStore::new(...)` 构造 store
/// (`attach_agent_session` + 6 处直接写路径),而裸 `new()` 的
/// `max_per_session` 硬编码 0(= 不限)、`cas` 恒为 `None`。其中
/// `attach_agent_session` 还会**覆盖**掉 CLI 装配阶段已经正确配置好的
/// store —— 结果是 TUI 用户只要切换过一次模型 / 模式,checkpoint 就不再
/// 封顶、文件快照也不再走 CAS 去重(同 session 从 <5 MB 退回 200 MB 量级)。
///
/// 走进程级配置后,所有构造点拿到同一份配置,不再依赖"谁最后 set"。
/// 用 `Mutex<Option<>>` 而非 `OnceLock` 是为了允许单测重置
/// (`OnceLock` 只能 set 一次,跨测试用例不够用)。
#[derive(Clone, Default)]
pub struct CheckpointConfig {
    /// 单 session checkpoint 条数上限(0 = 不限)。
    pub max_per_session: usize,
    /// 可选 CAS 池,用于 capture_files / restore_files_snapshot 去重。
    pub cas: Option<Arc<WorkspaceCas>>,
}

static CHECKPOINT_CONFIG: std::sync::Mutex<Option<CheckpointConfig>> = std::sync::Mutex::new(None);

/// CLI 装配阶段(主进程 `main` 入口)调用一次,把
/// `cfg.storage.checkpoints_per_session` 与已打开的 CAS 池注入全局。
pub fn set_checkpoint_config(cfg: CheckpointConfig) {
    match CHECKPOINT_CONFIG.lock() {
        Ok(mut slot) => *slot = Some(cfg),
        Err(poisoned) => {
            tracing::warn!("checkpoint config 锁被 poison，沿用既有配置");
            *poisoned.into_inner() = Some(cfg);
        }
    }
}

/// 仅供单测:清空进程内配置,避免用例之间互相污染。
#[cfg(test)]
pub fn clear_checkpoint_config_for_tests() {
    if let Ok(mut slot) = CHECKPOINT_CONFIG.lock() {
        *slot = None;
    }
}

/// 仅供单测:串行化那些读写 `CHECKPOINT_CONFIG` 的用例。
///
/// cargo test 默认多线程跑用例,而 `CHECKPOINT_CONFIG` 是**进程内全局可变状态**。
/// 三个用例都在同一进程里 set/clear 它,并行执行时一个用例的 `clear` 会落在另一个
/// 用例的 `set` 与读取之间——`configured_defaults_to_unlimited_and_no_cas_when_unset`
/// 偶发读到别人注入的 7 就是这么来的。用例串行化是唯一正确的修法:让它们依赖「当前
/// 没人改过配置」这个前提,就必须真正独占这个全局。
#[cfg(test)]
pub(crate) fn checkpoint_config_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    match TEST_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl CheckpointStore {
    /// 按进程内全局配置构造 store。**TUI 的所有构造点都应走这里**,
    /// 而不是 `new()`。
    ///
    /// 未配置(单测、库调用方)时退化为 `new()` 语义:不限条数、不接 CAS。
    pub fn configured(sessions_dir: &Path, session_id: impl Into<String>) -> Result<Self> {
        let cfg = CHECKPOINT_CONFIG
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_default();
        let mut store = Self::new(sessions_dir, session_id)?;
        store.max_per_session = cfg.max_per_session;
        store.cas = cfg.cas;
        Ok(store)
    }
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

    /// `messages` 是**owned** 的历史快照，调用方负责 clone。
    ///
    /// 之所以要 owned 而非 `&[Message]`：落盘前要做 persist cap 截断
    /// （`truncate_content_block` 需要可变副本），此前接收 `&[Message]` 再
    /// 内部 `to_vec()`——于是**调用方的 clone 完全白费**，一次 checkpoint
    /// 要付两次整段历史的深拷贝。改为 owned 后调用方的 clone 变成唯一一次。
    pub fn create(
        &self,
        cwd: &Path,
        mut messages: Vec<Message>,
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
            // 落盘前做持久化截断(tool_result / thinking / tool_use.input)。
            // **没有配置上限时直接用调用方给的 owned 副本**：多一次 `to_vec()`
            // 会让每个工具调用的 checkpoint 多付一次整段历史深拷贝
            // （长会话里工具输出动辄 MB 级）。
            match current_checkpoint_persist_cap() {
                Some(cfg) => {
                    for msg in &mut messages {
                        for block in &mut msg.content {
                            crate::serialize::truncate_content_block(block, &cfg);
                        }
                    }
                    messages
                }
                None => messages,
            }
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
            owner_pid: std::process::id(),
        };
        // push 与 retention 合并在**同一个**临界区里：分成两步的话，中间会被
        // 另一个进程的 push 插进来，把裁剪结果整个盖回去，manifest 于是反复在
        // 「超过上限」和「丢条目」之间来回横跳。
        match self.mutate_manifest(|manifest| {
            manifest.checkpoints.push(summary.clone());
            self.prune_manifest_locked(manifest)
        }) {
            Ok(()) => {}
            Err(error) => tracing::warn!("Checkpoint manifest 更新/retention 失败: {error}"),
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
        // 上一次 checkpoint 的 git 快照：工作树没变时 `capture_git` 可以直接
        // 复用它，省掉 add -A / write-tree / commit-tree 三次全树哈希。
        let previous_git = self
            .load_manifest()
            .ok()
            .and_then(|manifest| pick_baseline(&manifest, cwd_root))
            .and_then(|id| self.load(&id).ok())
            .and_then(|cp| match cp.workspace {
                WorkspaceSnapshot::Git(snapshot) => Some(snapshot),
                _ => None,
            });
        let snapshot = capture_workspace(cwd, self.cas.as_deref(), previous_git.as_ref())?;
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
        self.mutate_manifest(|manifest| self.prune_manifest_locked(manifest))
            .map(|_| ())
    }

    /// 保留上限之内的裁剪逻辑。**必须在 [`Self::mutate_manifest`] 的临界区内调用**
    /// ——它会就地改 `manifest` 并删除磁盘上的 checkpoint 文件，中途被别的进程
    /// 插一脚就会留下「manifest 说还在、文件已经没了」的僵尸条目。
    ///
    /// 跨进程保护：只淘汰**自己创建的**或**创建者已死**的 checkpoint。别人正在用的
    /// 会话快照不是本进程的垃圾，哪怕它比自己创建得早——删掉它等于让另一个终端的
    /// `/rewind` 目标凭空消失。单进程场景下 `owner_pid` 就是自己，行为与旧版完全一致。
    fn prune_manifest_locked(&self, manifest: &mut CheckpointManifest) -> Result<()> {
        if self.max_per_session == 0 {
            return Ok(());
        }
        let self_pid = std::process::id();
        // 先把「别人活着的」摘出来数一遍：它们占着配额却不能删，所以上限针对本进程
        // 实际能删的那些计算，否则两个实例各留 21 个、总数直接翻倍。
        // 收集 owned 的 id 而不是借 `&str`——下面要 `mem::take` 整个 vector。
        let foreign_live: BTreeSet<String> = manifest
            .checkpoints
            .iter()
            .filter(|entry| entry.owner_pid != self_pid)
            .filter(|entry| {
                entry.owner_pid == 0 || wyj_config::filelock::is_process_alive(entry.owner_pid)
            })
            .map(|entry| entry.id.clone())
            .collect();
        let removable = manifest.checkpoints.len() - foreign_live.len();
        if removable <= self.max_per_session {
            return Ok(());
        }
        // 按 timestamp 升序,保留后 N 条(最新);被跳过的条目原样留回 manifest。
        manifest
            .checkpoints
            .sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        let drop_count = removable - self.max_per_session;
        let mut dropped = 0;
        let mut kept = Vec::with_capacity(manifest.checkpoints.len());
        for entry in std::mem::take(&mut manifest.checkpoints) {
            if dropped < drop_count && !foreign_live.contains(&entry.id) {
                validate_checkpoint_id(&entry.id)?;
                let path = self.checkpoint_path(&entry.id);
                if let Err(error) = std::fs::remove_file(&path) {
                    // 删不掉就留在 manifest 里：manifest 有条目而文件在，是无害的
                    // （`load` 能读出来）；反过来才是数据损坏。
                    tracing::warn!("删除旧 checkpoint 失败 {}: {error}", path.display());
                    kept.push(entry);
                } else {
                    dropped += 1;
                }
            } else {
                kept.push(entry);
            }
        }
        manifest.checkpoints = kept;
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

    fn manifest_lock_path(&self) -> PathBuf {
        self.dir.join("manifest.json.lock")
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

    /// 在跨进程排他锁内完成「读 manifest → 修改 → 原子写回」整个 read-modify-write。
    ///
    /// manifest 是 checkpoint 唯一的索引，任何一次「读到旧版本 → 写回自己的版本」
    /// 都会静默抹掉别的进程刚 push 的条目。两处写者（`create` 的 push 与
    /// `enforce_retention` 的裁剪）共用这把锁，所以「新增 + 裁剪」也是一次原子的
    /// 状态迁移，不会出现裁剪结果被别人的 push 覆盖回去。
    ///
    /// 注意这**只**解决索引一致性，不解决 checkpoint 文件本身的归属——同一个
    /// session 被两个进程 resume 时，两边都会 create，那是另一个问题（见
    /// `SessionStore::active_owner` 与 `enforce_retention` 的 owner 保护）。
    fn mutate_manifest<R>(
        &self,
        mutate: impl FnOnce(&mut CheckpointManifest) -> Result<R>,
    ) -> Result<R> {
        let _guard =
            wyj_config::filelock::acquire(&self.manifest_lock_path(), "checkpoint manifest")?;
        let mut manifest = self.load_manifest()?;
        let result = mutate(&mut manifest)?;
        write_json_atomic(&self.manifest_path(), &manifest)?;
        Ok(result)
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

fn capture_workspace(
    cwd: &Path,
    cas: Option<&WorkspaceCas>,
    previous_git: Option<&GitSnapshot>,
) -> Result<WorkspaceSnapshot> {
    if let Some(root) = git_root(cwd) {
        return Ok(WorkspaceSnapshot::Git(capture_git(&root, previous_git)?));
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

fn capture_git(root: &Path, previous: Option<&GitSnapshot>) -> Result<GitSnapshot> {
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
        // 快路径：工作区相对 HEAD 干净，且本次会话已经有过一次 git 快照。
        // 此时 `add -A` + `write-tree` + `commit-tree` 会产出与上一次**完全相同**的
        // tree/commit，直接复用即可——省下 3 个 git 子进程与一次全树哈希。
        //
        // 每轮工具调用固定建 2 个 checkpoint（PreTool / PostTool），而只读工具
        // （Bash 跑测试、Grep、Read）根本不碰工作树：这正是「每个工具调用都卡
        // 一下」的主要来源。判据是一次 `git status --porcelain`，比哈希便宜得多。
        if let Some(previous) = previous.filter(|p| p.head == head) {
            if !has_worktree_changes(root)? {
                return Ok(GitSnapshot {
                    repo_root: root.to_path_buf(),
                    head: previous.head.clone(),
                    tree: previous.tree.clone(),
                    commit: previous.commit.clone(),
                    files: previous.files.clone(),
                });
            }
        }
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

/// 工作区相对 HEAD 有没有任何改动。
///
/// 一次 `git status --porcelain`：空输出即干净。比 `git diff` 便宜，且不需要
/// 临时 index——这里只判断「有没有变」，不需要知道「变成了什么」。
fn has_worktree_changes(root: &Path) -> Result<bool> {
    let output = git_output(root, None, &["status", "--porcelain"])?;
    Ok(!output.trim().is_empty())
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
            // 恢复时必须采一份**当下**的真实状态，不能复用任何缓存快照。
            let current = capture_git(&current_root, None)?;
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
        // 按仓库位置动态决定 `require_git`，而不是写死 false：
        // - 非 git 仓库：`ignore` 的 `git_ignore(true)` 默认要求在 git 仓库内，
        //   否则**整份丢弃 .gitignore**，被项目显式排除的目录（典型：`.venv/`、
        //   数据产物目录）照样被遍历、被读取。checkpoint 的快照范围应由项目的
        //   忽略规则决定，而不是「一律全量」。
        // - git 仓库 / **managed worktree**：必须为 true，否则主仓库的
        //   `.git/info/exclude` 失效。`ignore` 解析 worktree common git dir
        //   （`.git` 是 `gitdir:` 指针文件）只在 `require_git == true` 的分支里，
        //   写死 false 会让它去读 `<worktree>/.git/info/exclude`——一个文件。
        //   后果是 checkpoint 把用户明确排除的目录整个快照进会话历史。
        .require_git(wyj_config::gitignore::is_inside_git_repo(&root))
        .ignore(true)
        .git_global(true)
        .git_exclude(true)
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
        // **先看大小再决定要不要读**。曾经的写法是 `std::fs::read(path)?` 之后才
        // 判断是否超过 NON_GIT_MAX_FILES / NON_GIT_MAX_BYTES，于是配额早就满了之后
        // 仍会把余下每一个文件完整读进内存再丢掉：在一个「数据文件占 2.4GB」的项目
        // 里，一次 checkpoint 因此要读 2.39GB 却只留 7.6MB，实测 3.56s；
        // 而 checkpoint 在每次工具调用的 PreTool / PostTool 各跑一次——
        // 单次工具调用白白多出约 7s，占整场分析的一半时间。
        //
        // 计数上限先判，是为了连 `stat` 都不必做：项目再大，剩下的每个文件
        // 都会走同一条「跳过」分支，而 `skipped_files` 的含义仍然是
        // 「遍历到但没进快照的文件数」，不受影响。
        if files.len() >= NON_GIT_MAX_FILES {
            skipped_files += 1;
            continue;
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if captured_bytes.saturating_add(metadata.len()) > NON_GIT_MAX_BYTES {
            skipped_files += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
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
            DeltaOp::Added {
                hash,
                inline,
                size,
                sha256,
            }
            | DeltaOp::Modified {
                hash,
                inline,
                size,
                sha256,
            } => {
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
fn diff_files(before: &FileSnapshot, after: &FileSnapshot) -> BTreeMap<PathBuf, DeltaOp> {
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
                            tracing::warn!("CAS get 失败 (hash={hash}): {error} —— 跳过此文件恢复");
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

/// checkpoint 的 JSON 落盘：**紧凑**格式 + 敏感信息脱敏。
///
/// 与 `serialize::write_json_atomic` 的两点差异都是有意的，别顺手统一掉：
/// 1. 脱敏 —— checkpoint 会快照整个 workspace（含未提交的源码），比会话文件
///    更容易扫到用户硬编码的 token；会话侧同样脱敏，两者必须一致。
/// 2. 紧凑（`to_string` 而非 pretty）—— 每个 checkpoint 含一份完整 workspace 快照，
///    而保留条数由 `max_per_session` 控制（默认 21），缩进带来的膨胀会直接换成磁盘。
///
/// tmp + rename 的原子替换逻辑委托给 `serialize::write_atomic`，全仓只此一份。
fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let json = serde_json::to_string(value)?;
    crate::serialize::write_atomic(path, crate::secret::redact_cow(&json).as_bytes())
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

    /// 工作树干净时 `capture_git` 必须复用上一份快照（跳过 add -A /
    /// write-tree / commit-tree）；工作树一变就必须重新采。
    #[test]
    fn clean_worktree_reuses_the_previous_git_snapshot() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(
            repo.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repo.path().join("a.txt"), "one").unwrap();
        git(repo.path(), &["add", "a.txt"]);
        git(repo.path(), &["commit", "-m", "base"]);
        let root = git_root(repo.path()).unwrap();

        let first = capture_git(&root, None).unwrap();

        // 干净工作树：复用同一 commit/tree
        let reused = capture_git(&root, Some(&first)).unwrap();
        assert_eq!(reused.commit, first.commit);
        assert_eq!(reused.tree, first.tree);

        // 工作树有改动：必须重新采集
        std::fs::write(repo.path().join("a.txt"), "two").unwrap();
        let changed = capture_git(&root, Some(&first)).unwrap();
        assert_ne!(changed.commit, first.commit);

        // 改回来（相对 HEAD 又干净）：仍应复用旧快照——因为它就是当下的真实状态
        std::fs::write(repo.path().join("a.txt"), "one").unwrap();
        let back = capture_git(&root, Some(&first)).unwrap();
        assert_eq!(back.commit, first.commit);
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
                Vec::new(),
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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
        let before_for_checkpoint = before.clone();
        let summary = store
            .create(
                root.path(),
                before_for_checkpoint,
                CheckpointKind::Manual,
                None,
            )
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
                vec![Message::user(format!("credential: {secret}"))],
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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

    /// 钉子：**非 git 目录里 `.gitignore` 必须生效**。
    ///
    /// `ignore` 的 `git_ignore(true)` 默认 `require_git = true`，在非 git 仓库里
    /// 会把整份 `.gitignore` 丢掉。于是一个明确写了 `.venv/`、`archive/`、
    /// `data/*.duckdb` 的数据型项目，checkpoint 依然把那些大文件全读一遍——
    /// 实测一次 checkpoint 读 2.39GB 只留 7.6MB、耗时 3.56s，而 checkpoint 在
    /// 每次工具调用的 PreTool / PostTool 各跑一次。
    #[test]
    fn non_git_checkpoint_honours_project_gitignore() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), ".venv/\narchive/\ndata/\n").unwrap();
        std::fs::create_dir_all(root.path().join(".venv/lib")).unwrap();
        std::fs::write(root.path().join(".venv/lib/pkg.py"), "ignored").unwrap();
        std::fs::create_dir_all(root.path().join("archive")).unwrap();
        std::fs::write(root.path().join("archive/old.json"), "ignored").unwrap();
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        std::fs::write(root.path().join("data/market.duckdb"), "ignored").unwrap();
        std::fs::write(root.path().join("keep.py"), "kept").unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "gitignore-non-git").unwrap();
        let summary = store
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        let checkpoint = store.load(&summary.id).unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!("expected non-git snapshot");
        };
        assert!(snapshot.files.contains_key(Path::new("keep.py")));
        for ignored in [".venv/lib/pkg.py", "archive/old.json", "data/market.duckdb"] {
            assert!(
                !snapshot.files.contains_key(Path::new(ignored)),
                "{ignored} 被 .gitignore 排除，不该进快照（否则每次工具调用都要重读一遍）"
            );
        }
    }

    /// managed worktree 里主仓库的 `.git/info/exclude` 必须仍然生效。
    ///
    /// worktree 的 `.git` 是 `gitdir:` 指针文件，`info/exclude` 始终躺在**主仓库**
    /// 的 common git dir 下。`ignore` crate 解析这条路径只在 `require_git == true`
    /// 的分支里；本模块此前写死 `require_git(false)`（为了修非 git 目录的
    /// `.gitignore` 失效），于是 worktree 里会去读 `<worktree>/.git/info/exclude`
    /// ——那是个文件。症状是 checkpoint 把用户用 info/exclude 明确排除的目录
    /// （典型：本地密钥笔记、`*.local` 配置）整个快照进**会话历史并落盘**。
    #[test]
    fn worktree_checkpoint_honours_the_main_repo_info_exclude() {
        let base = tempfile::tempdir().unwrap();
        let main_git = base.path().join("main/.git");
        let work_git = main_git.join("worktrees/wt");
        std::fs::create_dir_all(main_git.join("info")).unwrap();
        std::fs::create_dir_all(&work_git).unwrap();
        std::fs::write(main_git.join("info/exclude"), "secrets/\n").unwrap();
        // worktree 的 git dir 通过 `commondir` 指回主仓库的 `.git`（worktree 实测就是 `../..`）。
        std::fs::write(work_git.join("commondir"), "../..\n").unwrap();

        let worktree = base.path().join("wt");
        std::fs::create_dir_all(worktree.join("secrets")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", work_git.display()),
        )
        .unwrap();
        std::fs::write(worktree.join("secrets/token.txt"), "hunter2").unwrap();
        std::fs::write(worktree.join("keep.py"), "kept").unwrap();

        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "gitignore-worktree").unwrap();
        let summary = store
            .create(&worktree, Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        let checkpoint = store.load(&summary.id).unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!("expected files snapshot");
        };
        assert!(
            !snapshot.files.contains_key(Path::new("secrets/token.txt")),
            "主仓库 info/exclude 排除的目录不该被快照进会话历史"
        );
        assert!(snapshot.files.contains_key(Path::new("keep.py")));
    }

    /// 钉子：**超配额的文件一个字节都不该读**。
    ///
    /// 曾经的顺序是「`fs::read` → 判断是否超配额 → 超了就丢」，于是配额满之后
    /// 仍会把余下每个文件完整读进内存。这里用一个稀疏大文件把这条钉死：它
    /// `set_len` 出来不占磁盘，但真去读就要分配同等大小的缓冲区。
    #[test]
    fn non_git_checkpoint_stops_reading_once_the_byte_budget_is_spent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("first.bin"), vec![0u8; 1024]).unwrap();
        let hog = root.path().join("zz-huge.bin");
        let file = std::fs::File::create(&hog).unwrap();
        file.set_len(512 * 1024 * 1024).unwrap();
        drop(file);

        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "byte-budget").unwrap();
        let start = std::time::Instant::now();
        let summary = store
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        let elapsed = start.elapsed();

        let checkpoint = store.load(&summary.id).unwrap();
        let WorkspaceSnapshot::Files(snapshot) = checkpoint.workspace else {
            panic!("expected non-git snapshot");
        };
        assert!(snapshot.files.contains_key(Path::new("first.bin")));
        assert!(
            !snapshot.files.contains_key(Path::new("zz-huge.bin")),
            "超过 NON_GIT_MAX_BYTES 的文件不该被收进快照"
        );
        // 真去读那个 512MiB 稀疏文件，光是分配与填充就要数百毫秒到数秒。
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "超配额的大文件必须只 stat 不读，本次 capture 耗时 {elapsed:?}"
        );
    }

    /// M1 验证:CAS 路径下,200 个相同内容文件的 checkpoint JSON 体积应 < 50KB,
    /// 老 inline_bytes 路径下应是几 MB 量级。证明内容寻址去重生效。
    #[test]
    fn cas_dedupe_dramatically_reduces_checkpoint_size() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024).unwrap(),
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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

    /// M1 兼容验证:老 v0.2.19 checkpoint JSON(含顶层 `bytes: Vec<u8>` 字段)
    /// 仍能加载,且 `inline_bytes` 字段被填充。
    #[test]
    fn load_legacy_v1_checkpoint_with_bytes_field() {
        let sessions = tempfile::tempdir().unwrap();
        let dir = sessions.path().join("legacy-sess.checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        // 手工写一份 v0.2.19 格式的 checkpoint(顶层 FileEntry.bytes)
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
        assert_eq!(
            entry.sha256,
            "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"
        );
    }

    /// M2 验证:相邻 checkpoint 自动写 Delta 形式,且 Delta 体积 << 完整 Files。
    #[test]
    fn adjacent_checkpoints_write_delta_form() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024).unwrap(),
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        let cp1 = store.load(&s1.id).unwrap();
        assert!(matches!(cp1.workspace, WorkspaceSnapshot::Files(_)));
        // 改 1 个文件 → 第二个应是 Delta
        std::fs::write(root.path().join("f005.txt"), "MODIFIED content for file 5").unwrap();
        let s2 = store
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        let cp2 = store.load(&s2.id).unwrap();
        let WorkspaceSnapshot::Delta(delta) = &cp2.workspace else {
            panic!(
                "expected Delta, got {:?}",
                std::mem::discriminant(&cp2.workspace)
            );
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
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024).unwrap(),
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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
                .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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
            .create(root_a.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        assert!(matches!(
            store.load(&s_a.id).unwrap().workspace,
            WorkspaceSnapshot::Files(_)
        ));
        // 切到 root_b
        let s_b = store
            .create(root_b.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        assert!(
            matches!(
                store.load(&s_b.id).unwrap().workspace,
                WorkspaceSnapshot::Files(_)
            ),
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
            crate::workspace_cas::WorkspaceCas::open(cas_root.path(), 16 * 1024 * 1024).unwrap(),
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        // 2) 改 d03.txt 触发 Delta
        std::fs::write(root.path().join("d03.txt"), "delta content 3 MODIFIED").unwrap();
        let s2 = store
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
            .unwrap();
        // 3) 把 d03.txt 改成"非 MODIFIED 的脏内容",然后 preview 应识别差异
        std::fs::write(root.path().join("d03.txt"), "USER_DIRTY").unwrap();
        let preview = store.preview_files(&s2.id, root.path()).unwrap();
        assert!(
            preview
                .affected_files
                .iter()
                .any(|p| p.to_str() == Some("d03.txt")),
            "preview 必须识别 d03.txt 的脏内容"
        );
        // 4) restore:confirmed=true 跳过确认,验证 d03.txt 还原成 MODIFIED
        store.restore_files(&s2.id, root.path(), true).unwrap();
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
            .create(root.path(), Vec::new(), CheckpointKind::Manual, None)
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

    // ── configured() 进程级配置 ─────────────────────────────────────────
    //
    // 回归背景：TUI 的 `attach_agent_session` 曾用裸 `CheckpointStore::new()`
    // 覆盖 CLI 装配好的 store，而裸 new() 的 max_per_session 恒为 0(不限)、
    // cas 恒为 None。用户切换一次模型后 checkpoint 就不再封顶、文件快照
    // 也不再走 CAS 去重。

    #[test]
    fn configured_defaults_to_unlimited_and_no_cas_when_unset() {
        let _guard = checkpoint_config_test_guard();
        clear_checkpoint_config_for_tests();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::configured(sessions.path(), "s-default").unwrap();
        assert_eq!(store.max_per_session, 0, "未注入配置时应等同裸 new()");
        assert!(store.cas().is_none());
    }

    #[test]
    fn configured_picks_up_injected_max_per_session() {
        let _guard = checkpoint_config_test_guard();
        clear_checkpoint_config_for_tests();
        let sessions = tempfile::tempdir().unwrap();
        set_checkpoint_config(CheckpointConfig {
            max_per_session: 7,
            cas: None,
        });
        let store = CheckpointStore::configured(sessions.path(), "s-injected").unwrap();
        assert_eq!(store.max_per_session, 7);
        // 无 CAS 时不应 panic，capture_files 走 inline 兜底
        assert!(store.cas().is_none());
        clear_checkpoint_config_for_tests();
    }

    #[test]
    fn set_checkpoint_config_replaces_previous_value() {
        let _guard = checkpoint_config_test_guard();
        clear_checkpoint_config_for_tests();
        let sessions = tempfile::tempdir().unwrap();
        set_checkpoint_config(CheckpointConfig {
            max_per_session: 3,
            cas: None,
        });
        set_checkpoint_config(CheckpointConfig {
            max_per_session: 5,
            cas: None,
        });
        let store = CheckpointStore::configured(sessions.path(), "s-replace").unwrap();
        assert_eq!(store.max_per_session, 5, "后一次注入应覆盖前一次");
        clear_checkpoint_config_for_tests();
    }

    /// retention 必须跳过**别的活跃进程**创建的 checkpoint。
    ///
    /// 同一 session 被两个 wyj-code resume 时，两边都会 create。此前没有 `owner_pid`，
    /// A 的保留策略会连同 B 刚建的、用户正要 `/rewind` 回去的那个一起淘汰——用户
    /// 看到的是「刚才还在的回滚点凭空消失」，且日志里只有一条普通的 retention 记录。
    #[test]
    fn retention_never_deletes_another_live_processs_checkpoint() {
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s-owner").unwrap();
        let mut store = store;
        store.max_per_session = 2;

        // 伪造三条「属于别的活进程」的记录。用本测试二进制的 pid —— 它当然是活着的。
        let live_pid = std::process::id();
        let mut manifest = CheckpointManifest::default();
        for i in 0..3 {
            let id = format!("cp-foreign-{i}");
            std::fs::write(
                sessions
                    .path()
                    .join(format!("s-owner.checkpoints/{id}.json")),
                b"{}",
            )
            .unwrap();
            manifest.checkpoints.push(CheckpointSummary {
                id,
                name: None,
                kind: CheckpointKind::Manual,
                timestamp: format!("2024-01-0{}T00:00:00Z", i + 1),
                message_count: 0,
                cwd_root: String::new(),
                baseline_id: None,
                owner_pid: live_pid,
            });
        }
        std::fs::write(
            sessions.path().join("s-owner.checkpoints/manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // 再加一条真正属于本进程的，触发裁剪路径。
        store
            .mutate_manifest(|manifest| {
                manifest.checkpoints.push(CheckpointSummary {
                    id: "cp-mine".to_string(),
                    name: None,
                    kind: CheckpointKind::Manual,
                    timestamp: "2024-02-01T00:00:00Z".to_string(),
                    message_count: 0,
                    cwd_root: String::new(),
                    baseline_id: None,
                    owner_pid: std::process::id(),
                });
                Ok(())
            })
            .unwrap();
        std::fs::write(
            sessions.path().join("s-owner.checkpoints/cp-mine.json"),
            b"{}",
        )
        .unwrap();

        let remaining: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert!(
            remaining.contains(&"cp-foreign-0".to_string())
                && remaining.contains(&"cp-foreign-2".to_string()),
            "别人的 checkpoint 必须全部保留，实际剩余: {remaining:?}"
        );
        assert!(
            sessions
                .path()
                .join("s-owner.checkpoints/cp-foreign-0.json")
                .is_file(),
            "文件也不能被删——manifest 有条目而文件没了才是真的损坏"
        );
    }

    /// 单进程行为不变：自己创建的、超过上限的最老那条照常被淘汰。
    #[test]
    fn retention_still_prunes_own_oldest_checkpoints() {
        let sessions = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::new(sessions.path(), "s-prune").unwrap();
        store.max_per_session = 2;
        let mut manifest = CheckpointManifest::default();
        for i in 0..4 {
            let id = format!("cp{i}");
            std::fs::write(
                sessions
                    .path()
                    .join(format!("s-prune.checkpoints/{id}.json")),
                b"{}",
            )
            .unwrap();
            manifest.checkpoints.push(CheckpointSummary {
                id,
                name: None,
                kind: CheckpointKind::Manual,
                timestamp: format!("2024-01-0{}T00:00:00Z", i + 1),
                message_count: 0,
                cwd_root: String::new(),
                baseline_id: None,
                owner_pid: std::process::id(),
            });
        }
        std::fs::write(
            sessions.path().join("s-prune.checkpoints/manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        store.enforce_retention().unwrap();
        let remaining: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(remaining, vec!["cp2".to_string(), "cp3".to_string()]);
        assert!(!sessions
            .path()
            .join("s-prune.checkpoints/cp0.json")
            .is_file());
        assert!(!sessions
            .path()
            .join("s-prune.checkpoints/cp1.json")
            .is_file());
    }
}
