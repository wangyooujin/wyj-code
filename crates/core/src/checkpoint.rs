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
/// messages 增量链的最大**读取**折叠深度。
///
/// 这是防御性上限，不是正常水位：写侧由 [`MAX_MESSAGES_DELTA_WRITE_DEPTH`]
/// 主动切段，正常情况链长不会到这里。超过它说明磁盘上有病态链（历史遗留 /
/// 手工编辑），此时宁可放弃折叠（`load` 报错）也不要让一次 `load` 变成
/// 几十次同步读盘。
const MAX_MESSAGES_DELTA_DEPTH: usize = 20;

/// messages 增量链的**写侧**上限：到深度就改写全量，把长链切成若干段。
///
/// 为什么必须存在：retention 按时间淘汰**最旧**的 checkpoint，而增量链的
/// 根恰好是最旧的那个。链一旦长过保留窗口，根被淘汰后所有子节点都折叠
/// 失败、只能退回「本节点尾部」——`/rewind` 会拿到 1~2 条消息的伪历史。
///
/// 取 8：默认保留 20 条时最多同时存在 3 段，且任何一段的根被淘汰时，
/// 受影响的只有该段（下一段的第一个节点是全量，能独立折叠）。
const MAX_MESSAGES_DELTA_WRITE_DEPTH: u32 = 8;

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
    /// 历史正文。增量形态下只有「相对父 checkpoint 新增的尾部」。
    ///
    /// 落盘时由 [`CheckpointStore::create`] 决定存全量还是尾部；读回时由
    /// `load_raw` 沿父链折叠成完整历史，因此**上层拿到的永远是完整
    /// `messages`**，增量形态只在磁盘上存在。
    pub messages: Vec<Message>,
    /// 非 `None` 表示 `messages` 只是**尾部增量**，完整历史要沿父链折叠。
    ///
    /// 引入原因（实测 2026-10-28）：本机 5,703 个 checkpoint 合计 1,836 MB，
    /// 抽样 30 个发现 **96% 的体积是内联的完整 `messages` 快照**。根因是
    /// 每个有副作用的工具在**前后各写一份全量历史**（`PreTool` / `PostTool`），
    /// 而一次回合里这类工具动辄几十个。
    ///
    /// 文件快照早就有 `WorkspaceSnapshot::Delta` 了，messages 没有——这里补齐。
    /// 增量只对「父快照是本 checkpoint 的严格前缀」成立时才用；不满足时
    /// 退回全量（`None`），因此旧的读写路径语义完全不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_delta: Option<MessagesDelta>,
    /// 工作区快照（Git 引用 / 全量 Files / 相对父节点的 Delta）。
    pub workspace: WorkspaceSnapshot,
}

/// messages 的父链增量描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesDelta {
    /// 父 checkpoint id。折叠时从它取完整历史再追加本 checkpoint 的尾部。
    pub parent_checkpoint_id: String,
    /// 父 checkpoint 的历史条数。折叠时用它校验「父链确实是本快照的前缀」——
    /// 长度对不上说明中间发生过压缩 / 分支，此时**不能**拼，必须退回全量。
    pub parent_len: usize,
    /// 本节点在父链上的深度（父节点自身的 `depth + 1`；父为全量时是 1）。
    ///
    /// 写侧用它**主动封顶**：链越深，一次 `load` 的递归读盘次数越多，
    /// 而 retention 淘汰父节点时能「断链」的范围也越大。到
    /// [`MAX_MESSAGES_DELTA_WRITE_DEPTH`] 就改写全量，把链切成若干段。
    ///
    /// `serde(default)`：字段引入前写下的增量按 0 处理，下一次写入会自然
    /// 按「父深度未知」保守处理。
    #[serde(default)]
    pub depth: u32,
}

impl Checkpoint {
    pub fn workspace_root(&self) -> &Path {
        match &self.workspace {
            WorkspaceSnapshot::Git(snapshot) => &snapshot.repo_root,
            WorkspaceSnapshot::Files(snapshot) => &snapshot.root,
            WorkspaceSnapshot::Delta(snapshot) => &snapshot.root,
        }
    }

    /// 本 checkpoint 是否为增量形态（`messages` 只是尾部）。
    pub fn is_messages_delta(&self) -> bool {
        self.messages_delta.is_some()
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
    /// 该 checkpoint 文件落盘后的字节数（写入时记录）。
    ///
    /// 存在的理由：按**条数**淘汰挡不住体积膨胀。实测（2026-10-28）本机
    /// 5,703 个 checkpoint 合计 1,836 MB，平均 322 KB，其中 96% 是内联的
    /// 完整 `messages` 快照——`checkpoints_per_session = 20` 对长会话来说
    /// 意味着「留 20 份几 MB 的历史」，总量随会话长度线性上涨。
    ///
    /// `0` = 老版本写入的条目（字段引入前），无法参与字节预算计算，按
    /// 「不占预算」处理：宁可少删也不误删用户仅剩的回滚点。
    #[serde(default)]
    pub bytes: u64,
    /// 本 checkpoint 的 messages 增量父节点（全量形态为 `None`）。
    ///
    /// 冗余存进 manifest 是为了让 retention **不必读 checkpoint 正文**就能
    /// 找出「父节点即将被删」的子节点——淘汰发生在每次 create 之后，若
    /// 每次都把 20 个 checkpoints 的完整 JSON 读一遍，长会话的写入成本会
    /// 直接被 retention 拖垮。字段引入前的条目一律是**全量**形态（delta 与
    /// 该字段同时出现），因此 `None` 的语义天然正确。
    #[serde(default)]
    pub messages_parent: Option<String>,
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
    /// 同一工作目录上还活着的**其他** wyj-code 实例 pid（见 [`crate::instance`]）。
    ///
    /// 回滚走 `git restore --worktree`，直接覆盖磁盘上的文件，完全不感知这些实例
    /// 手里的改动——对方既没有 checkpoint 记录，也无从得知自己刚写的半成品被换成了
    /// 旧版本。非空时 [`RewindPreview::requires_confirmation`] 恒为 `true`：不是禁止，
    /// 是必须让人明确看着这个事实点一次确认。
    #[serde(default)]
    pub shared_peers: Vec<u32>,
}

#[derive(Clone)]
pub struct CheckpointStore {
    session_id: String,
    dir: PathBuf,
    /// 单 session checkpoint 上限(builder 注入;0 = 不限)。
    max_per_session: usize,
    /// 单 session checkpoint 字节预算(builder 注入;0 = 不按字节淘汰)。
    max_bytes_per_session: u64,
    /// 可选 CAS 池。capture_files / restore_files_snapshot 走 CAS 减少 checkpoint 体积。
    /// None 表示不接 CAS(测试或回滚场景),全部 inline_bytes。
    cas: Option<Arc<WorkspaceCas>>,
    /// 本进程所处的执行面**无人可问**（headless / `-p` / 定时任务 / 子 Agent）。
    ///
    /// 见 [`CheckpointStore::restore_files`]：同一工作目录上还有别的实例时，
    /// 「有没有人能被问一句」决定的是**拒绝还是确认**，不是提示的措辞。
    non_interactive: bool,
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
    /// 单 session checkpoint **字节**预算(0 = 不按字节淘汰)。
    ///
    /// 与 `max_per_session` 是两个独立闸门,任一超标即淘汰。只按条数控制
    /// 挡不住体积膨胀:实测 5,703 个 checkpoint 合计 1,836 MB、平均 322 KB,
    /// 其中 96% 是内联的完整 `messages` 快照。
    pub max_bytes_per_session: u64,
    /// 可选 CAS 池,用于 capture_files / restore_files_snapshot 去重。
    pub cas: Option<Arc<WorkspaceCas>>,
    /// 本进程的交互面是否**无人可问**（headless / `-p` / 定时任务 / 子 Agent）。
    /// 与 `permission::ExecutionSurface::is_interactive` 同源语义，只是 checkpoint
    /// 层拿不到那个枚举，故由装配阶段平移注入。
    pub non_interactive: bool,
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
        store.max_bytes_per_session = cfg.max_bytes_per_session;
        store.cas = cfg.cas;
        store.non_interactive = cfg.non_interactive;
        Ok(store)
    }

    /// 显式声明「本进程没有人可以被问一句」。
    pub fn with_non_interactive(mut self, non_interactive: bool) -> Self {
        self.non_interactive = non_interactive;
        self
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
            max_bytes_per_session: 0,
            cas: None,
            non_interactive: false,
        })
    }

    /// 注入 CAS 池(builder 模式)。CAS root 不可用时 capture_files 走 inline 兜底,
    /// 不阻断 checkpoint 写入。
    pub fn with_cas(mut self, cas: Arc<WorkspaceCas>) -> Self {
        self.cas = Some(cas);
        self
    }

    /// 设置单 session 保留上限(builder 模式，0 = 不限)。
    ///
    /// `new()` 给的是 0，而 `enforce_retention` 在 `max_per_session == 0` 时
    /// 直接返回——所以**用 `new()` 构造的 store 调 `enforce_retention` 是空操作**。
    /// 生产路径必须走 `configured()`（从进程全局读配置）；这条 builder 是给
    /// 测试与 `storage prune` 这类「按显式参数跑一次裁剪」的调用方用的。
    pub fn with_max_per_session(mut self, max_per_session: usize) -> Self {
        self.max_per_session = max_per_session;
        self
    }

    /// 设置单 session 的 checkpoint **字节**预算（builder 模式，0 = 不按字节淘汰）。
    ///
    /// 与 [`Self::with_max_per_session`] 是两个独立闸门，任一超标即淘汰。
    /// `storage prune` 这类「按显式参数跑一次裁剪」的调用方必须两个都设，
    /// 否则长会话的体积膨胀清不掉。
    pub fn with_max_bytes_per_session(mut self, max_bytes: u64) -> Self {
        self.max_bytes_per_session = max_bytes;
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
        // 落盘前两步**都必须做**，且顺序固定：先按 persist_cap 截断文本，再把
        // 超大二进制/image 外化到 CAS。
        //
        // **为什么 externalize 这一步曾经缺失、代价有多大**：`SessionStore::save`
        // 一直有 `externalize_block`，而 `CheckpointStore::create` 只有
        // `truncate_content_block`。`truncate` 对 `ToolResultPart::Image` 明确
        // 不动（"避免引入运行时去重逻辑"，见 `serialize::truncate_tool_result`），
        // 于是**同一批工具截图在保留的 20 个 checkpoint 里各存一份完整 base64**。
        // 实测（2026-10-20）：`~/.wyj-code/sessions` 3.02 GB 中 checkpoint 占
        // 2.93 GB，其中内联 base64 图片 1.21 GB（40%）、image 块另有 0.28 GB。
        // 而 CAS 池同期只有 310 MB（零引用仅 21 MB）——内容寻址池本来是健康的，
        // 膨胀全在 checkpoint 内联。
        //
        // 外化后盘上留 `cas://<hash>` 占位，restore 时由
        // `materialize_block_with` 原样还原（与 session 的 resume 语义一致）。
        // CAS 不可写时 `externalize_block_with` 内部只 warn 并保留原值，不阻断
        // checkpoint 写入。
        let truncated_messages: Vec<Message> = {
            // **没有配置上限时也要走外化**，所以不能只在 `Some(cfg)` 分支里处理。
            // 反过来说，`PersistCapCfg` 全零时仍需要外化——这正是此前漏掉的形态。
            let cap = current_checkpoint_persist_cap();
            let cas = self.cas.as_deref();
            for msg in &mut messages {
                for block in &mut msg.content {
                    if let Some(cfg) = cap.as_ref() {
                        crate::serialize::truncate_content_block(block, cfg);
                    }
                    crate::serialize::externalize_block_with(block, cas);
                }
            }
            messages
        };
        let workspace = self.capture_workspace_with_delta(&cwd_root, cwd)?;
        // ── messages 增量 ────────────────────────────────────────────────────
        //
        // 判定「上一个 checkpoint 的完整历史是不是本快照的严格前缀」：
        // 是 → 只存尾部（`messages_delta`），否则存全量。
        //
        // 这个判据把**所有不安全的情形**都挡在门外，不需要逐个枚举：
        // - 压缩后历史被整体重写 → 前缀不成立 → 全量；
        // - 分支 / rewind 后历史改向 → 前缀不成立 → 全量；
        // - 首个 checkpoint、父节点已被淘汰 → 拿不到父历史 → 全量；
        // - 历史反而变短（不该发生）→ `len()` 比较自然排除。
        //
        // 为什么值得做：实测 5,703 个 checkpoint / 1,836 MB，抽样 30 个发现
        // 96% 的体积是内联的完整 `messages`。一次回合里有副作用的工具动辄
        // 几十个，每个都存两份全量历史（PreTool + PostTool）。
        let (stored_messages, messages_delta) = self.plan_messages_storage(&truncated_messages);
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            id: id.clone(),
            session_id: self.session_id.clone(),
            name: name.clone(),
            kind: kind.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            messages: stored_messages,
            messages_delta,
            workspace,
        };
        let written_bytes = write_json_atomic_counted(&self.checkpoint_path(&id), &checkpoint)?;

        let baseline_id = match &checkpoint.workspace {
            WorkspaceSnapshot::Delta(delta) => Some(delta.parent_checkpoint_id.clone()),
            _ => None,
        };
        let summary = CheckpointSummary {
            id,
            name,
            kind,
            timestamp: checkpoint.timestamp,
            // 对外报告的是**完整**历史条数，不是磁盘上那份尾部——
            // `/rewind` 列表与 `message_count` 校验都按完整值走。
            message_count: checkpoint.messages.len()
                + checkpoint
                    .messages_delta
                    .as_ref()
                    .map(|delta| delta.parent_len)
                    .unwrap_or(0),
            cwd_root,
            baseline_id,
            owner_pid: std::process::id(),
            bytes: written_bytes,
            // 冗余记录增量父节点：retention 靠它找出「父节点即将被删」的
            // 子节点，从而在删除前把它们改写为全量（见 `prune_manifest_locked`）。
            messages_parent: checkpoint
                .messages_delta
                .as_ref()
                .map(|delta| delta.parent_checkpoint_id.clone()),
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
            .and_then(|id| self.load_raw(&id).ok())
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
        let parent_cp = match self.load_raw(&parent_id) {
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

    /// 预估 [`Self::enforce_retention`] 会删掉几条，**不改动任何文件**。
    ///
    /// 判据必须与 `prune_manifest_locked` 完全一致，否则 dry-run 会骗人：
    /// 实测某会话 manifest 有 33 条、上限 10，dry-run 报「将删除 23 条」，
    /// 而 `--yes` 报「已删除 0 条」——那 33 条的 `owner_pid` 全是 0（旧版本
    /// 写的），`prune_manifest_locked` 把 `owner_pid == 0` 当成「可能活着」
    /// 保守保留，dry-run 却只算了 `before - max`。
    ///
    /// 用**不可删条数**反推而不是复制一份淘汰逻辑：后者会随 retention 规则
    /// 演进而漂移，前者只依赖 `prune_manifest_locked` 自己维护的那个判据。
    pub fn estimate_retention_drops(&self) -> Result<usize> {
        if self.max_per_session == 0 && self.max_bytes_per_session == 0 {
            return Ok(0);
        }
        let manifest = self.load_manifest()?;
        let self_pid = std::process::id();
        // 与 `prune_manifest_locked` 的 `foreign_live` **逐字对应**：别人创建的
        // 且（pid 未知为 0 或仍存活）。自己创建的条目不算保护对象——那边会把
        // 它们照常淘汰。
        let foreign_live: BTreeSet<&str> = manifest
            .checkpoints
            .iter()
            .filter(|entry| entry.owner_pid != self_pid)
            .filter(|entry| {
                entry.owner_pid == 0 || wyj_config::filelock::is_process_alive(entry.owner_pid)
            })
            .map(|entry| entry.id.as_str())
            .collect();
        let removable: Vec<&CheckpointSummary> = manifest
            .checkpoints
            .iter()
            .filter(|entry| !foreign_live.contains(entry.id.as_str()))
            .collect();

        let by_count = if self.max_per_session == 0 {
            0
        } else {
            removable.len().saturating_sub(self.max_per_session)
        };
        // 字节闸门：与 `prune_manifest_locked` 同一份判据（跳过 bytes == 0）。
        let mut byte_drops = 0usize;
        if self.max_bytes_per_session > 0 {
            let total: u64 = removable.iter().map(|entry| entry.bytes).sum();
            let mut over = total.saturating_sub(self.max_bytes_per_session);
            for entry in &removable {
                if over == 0 {
                    break;
                }
                if entry.bytes == 0 {
                    continue;
                }
                over = over.saturating_sub(entry.bytes);
                byte_drops += 1;
            }
        }
        Ok(by_count.max(byte_drops))
    }

    /// 把已落盘的 checkpoint **重新外化**一遍，返回 `(处理数, 释放字节数)`。
    ///
    /// # 为什么需要这个（而 `create` 的修复不够）
    ///
    /// `create` 修好之后只影响**新写入**。存量 checkpoint 是旧版本写的，
    /// 里面内联着完整 base64——实测 `~/.wyj-code/sessions` 3.02 GB 中
    /// 1.21 GB 是这类图片，且**同一条记录在 20 个快照里各存一份**。
    /// 这些文件不会因为「以后新写的变小了」而缩小。
    ///
    /// 本方法对每个 checkpoint 文件做一次「读 → 外化 → 原子写回」，不改
    /// `messages` 的语义、不动 manifest、不删任何条目——纯粹把内联字节换成
    /// `cas://` 占位符。副作用是 `intern` 让对应 blob 的 ref_count +1，而
    /// `prune_manifest_locked` 淘汰时已经会 `release`，两者对称。
    ///
    /// **幂等**：已经外化过的 block 前缀是 `cas://`，`externalize_block_with`
    /// 不会二次处理（`data.len() <= 32*1024` 的检查之外，`cas://` 前缀本身
    /// 也远短于阈值）。
    ///
    /// 不可解析的 checkpoint（旧 schema / 损坏）跳过并计入返回值之外，
    /// 由调用方从日志看。
    pub fn reexternalize_in_place(&self) -> Result<(usize, u64)> {
        let Some(cas) = self.cas.clone() else {
            anyhow::bail!("reexternalize 需要 CAS 池；当前 store 未接 CAS");
        };
        let summary = self.list()?;
        let mut processed = 0usize;
        let mut freed = 0u64;
        for entry in summary {
            let path = self.checkpoint_path(&entry.id);
            let Ok(before_bytes) = std::fs::metadata(&path).map(|m| m.len()) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&path) else {
                tracing::warn!("读取 checkpoint 失败: {}", path.display());
                continue;
            };
            let mut checkpoint: Checkpoint = match serde_json::from_slice(&bytes) {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    tracing::warn!("checkpoint 解析失败，跳过 {}: {error}", path.display());
                    continue;
                }
            };
            let mut changed = false;
            for message in &mut checkpoint.messages {
                for block in &mut message.content {
                    let original = serde_json::to_string(block).map(|s| s.len()).unwrap_or(0);
                    crate::serialize::externalize_block_with(block, Some(cas.as_ref()));
                    let after = serde_json::to_string(block).map(|s| s.len()).unwrap_or(0);
                    if after < original {
                        changed = true;
                    }
                }
            }
            if !changed {
                continue;
            }
            write_json_atomic(&path, &checkpoint)?;
            let after_bytes = std::fs::metadata(&path)
                .map(|m| m.len())
                .unwrap_or(before_bytes);
            freed += before_bytes.saturating_sub(after_bytes);
            processed += 1;
        }
        Ok((processed, freed))
    }

    /// 列出内容已损坏（无法解析）的 checkpoint，**不改动任何文件**。
    ///
    /// 供 `storage doctor` / `prune --dry-run` 报计划用：删不删是用户的决定，
    /// 但「有多少、占多少」必须先看得见。返回 `(个数, 字节数)`。
    ///
    /// 与 [`Self::purge_corrupt`] 的区别只是不落地——两者共用同一判据
    /// （`serde_json::from_slice::<Checkpoint>` 失败，或条目在 manifest 里
    /// 而文件不存在）。
    pub fn scan_corrupt(&self) -> Result<(usize, u64)> {
        let manifest = self.load_manifest()?;
        let mut count = 0usize;
        let mut bytes = 0u64;
        for entry in &manifest.checkpoints {
            let path = self.checkpoint_path(&entry.id);
            match std::fs::read(&path) {
                Ok(content) => {
                    if serde_json::from_slice::<Checkpoint>(&content).is_err() {
                        count += 1;
                        bytes += content.len() as u64;
                    }
                }
                // 条目在 manifest 里但文件不存在：也是僵尸，计入。
                Err(_) => count += 1,
            }
        }
        Ok((count, bytes))
    }

    /// 删除**内容已损坏**（无法解析）的 checkpoint 文件，并同步清掉 manifest
    /// 里的对应条目。返回 `(删除数, 释放字节数)`。
    ///
    /// # 为什么必须同时清 manifest
    ///
    /// 只删文件会留下「manifest 说还在、文件已经没了」的僵尸条目：`/rewind`
    /// 的目标列表里会出现选不中的项，`load` 才报错——用户看到的是一个坏掉的
    /// 回滚点而不是「这个点已经没了」。反过来只清 manifest 不删文件则是纯垃圾
    /// 占盘。两者必须在**同一个临界区**里完成。
    ///
    /// # 为什么会损坏
    ///
    /// 2026-10-20 实测 120 个（约 133 MB）：`write_json_atomic` 曾对**序列化后
    /// 的文本**跑 `redact_cow`，而那条正则的前缀 `\\?["']?` 会拆开 JSON 转义
    /// 引号，留下裸引号导致文件不可解析。该写入路径已改为结构化脱敏
    /// （见 `Secret::redact_json_value`），但**已经写坏的这些无法修复**——
    /// 原始 URL 与引号边界已经丢失，只能删除。
    pub fn purge_corrupt(&self) -> Result<(usize, u64)> {
        let manifest = self.load_manifest()?;
        // 判据与 `scan_corrupt` 必须一致（含「文件不存在的僵尸条目」）。
        // 这里内联一份而不是调用 `scan_corrupt`：后者返回的是计数，
        // 而删除需要具体 id 集合。
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for entry in &manifest.checkpoints {
            let path = self.checkpoint_path(&entry.id);
            let corrupt = match std::fs::read(&path) {
                Ok(content) => serde_json::from_slice::<Checkpoint>(&content).is_err(),
                Err(_) => true,
            };
            if corrupt {
                ids.insert(entry.id.clone());
            }
        }
        if ids.is_empty() {
            return Ok((0, 0));
        }
        let mut removed = 0usize;
        let mut freed = 0u64;
        self.mutate_manifest(|manifest| {
            for id in &ids {
                let path = self.checkpoint_path(id);
                let Ok(meta) = std::fs::metadata(&path) else {
                    // 文件本就不存在（僵尸条目）：只清 manifest，不计删除数。
                    continue;
                };
                if let Err(error) = std::fs::remove_file(&path) {
                    tracing::warn!("删除损坏 checkpoint 失败 {}: {error}", path.display());
                } else {
                    freed += meta.len();
                    removed += 1;
                }
            }
            manifest
                .checkpoints
                .retain(|entry| !ids.contains(&entry.id));
            Ok(())
        })?;
        Ok((removed, freed))
    }

    /// 保留上限之内的裁剪逻辑。**必须在 [`Self::mutate_manifest`] 的临界区内调用**
    /// ——它会就地改 `manifest` 并删除磁盘上的 checkpoint 文件，中途被别的进程
    /// 插一脚就会留下「manifest 说还在、文件已经没了」的僵尸条目。
    ///
    /// 跨进程保护：只淘汰**自己创建的**或**创建者已死**的 checkpoint。别人正在用的
    /// 会话快照不是本进程的垃圾，哪怕它比自己创建得早——删掉它等于让另一个终端的
    /// `/rewind` 目标凭空消失。单进程场景下 `owner_pid` 就是自己，行为与旧版完全一致。
    fn prune_manifest_locked(&self, manifest: &mut CheckpointManifest) -> Result<()> {
        // 两个闸门任一开启就要跑；都为 0 才是「完全不管」。
        if self.max_per_session == 0 && self.max_bytes_per_session == 0 {
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
        // 按 timestamp 升序，从最旧的开始淘汰。
        manifest
            .checkpoints
            .sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        // ── 两个闸门一起算出「要删几个」────────────────────────────────────
        //
        // 条数闸门：能删的条目数超过上限时，删掉最旧的差额。
        // 字节闸门：可删条目的字节总和超过预算时，从最旧的开始删，直到落回预算内。
        //
        // 取两者中**更大的**那个删除量：任一超标都要满足，只满足其一等于没治住。
        let removable: Vec<&CheckpointSummary> = manifest
            .checkpoints
            .iter()
            .filter(|entry| !foreign_live.contains(&entry.id))
            .collect();
        // 条数闸门：能删的条目数超过上限时，删掉最旧的差额。
        let by_count = if self.max_per_session == 0 {
            0
        } else {
            removable.len().saturating_sub(self.max_per_session)
        };
        // 字节闸门：可删条目的字节总和超过预算时，从最旧的开始删到落回预算内。
        //
        // `bytes == 0` 的老条目（字段引入前写入）跳过：它们不占预算，砍掉
        // 纯粹是白丢用户仅剩的回滚点。这条判据必须与下面实际的删除循环
        // **共用同一份待删集合**，否则会出现「按字节算要删 1 个，实际删掉的
        // 却是那条不占空间的老条目」。
        let mut drop_by_bytes: BTreeSet<String> = BTreeSet::new();
        if self.max_bytes_per_session > 0 {
            let total: u64 = removable.iter().map(|entry| entry.bytes).sum();
            let mut over = total.saturating_sub(self.max_bytes_per_session);
            for entry in &removable {
                if over == 0 {
                    break;
                }
                if entry.bytes == 0 {
                    continue;
                }
                over = over.saturating_sub(entry.bytes);
                drop_by_bytes.insert(entry.id.clone());
            }
        }

        // 两个闸门取**更严**的：任一超标都要满足，只满足其一等于没治住。
        // 条数闸门按「最旧的 N 条」选（与旧行为一致），字节闸门按上面的集合，
        // 两者并集就是要删的。
        let mut drop_ids: BTreeSet<String> = drop_by_bytes;
        if by_count > 0 {
            for entry in removable.iter().take(by_count) {
                drop_ids.insert(entry.id.clone());
            }
        }
        if drop_ids.is_empty() {
            return Ok(());
        }
        // 条数闸门可能要求删超过「最旧的 N 条」——当字节闸门选中的是较新的
        // 条目时，并集已经覆盖；这里只需保证总数不低于 by_count 的要求。
        if by_count > drop_ids.len() {
            for entry in removable.iter() {
                if drop_ids.len() >= by_count {
                    break;
                }
                drop_ids.insert(entry.id.clone());
            }
        }

        // ── 删除前：把「父将被删」的增量节点升级为自包含形态 ─────────────
        //
        // messages 与 workspace **各有一条父链**，父文件一删，子节点的折叠
        // 就永久失败：
        //   * messages 链断 → `load` 只能返回尾部（1~2 条），`/rewind` 静默
        //     丢掉整段对话；
        //   * workspace Delta 链断 → `resolve_to_files_caller` 读不到父文件，
        //     `/rewind --scope files` 直接失败。
        // 所以两条链都要在**删除之前**折叠成自包含形态。此刻父文件还在盘上。
        //
        // 只需处理**直接子节点**：写侧用 `MAX_MESSAGES_DELTA_WRITE_DEPTH`
        // 主动切段，删一层的代价是每段最多升级一个节点，而不是整条链。
        for entry in manifest.checkpoints.iter_mut() {
            if drop_ids.contains(&entry.id) {
                continue;
            }
            let messages_parent = entry
                .messages_parent
                .clone()
                .filter(|parent| drop_ids.contains(parent));
            let workspace_parent = entry
                .baseline_id
                .clone()
                .filter(|parent| drop_ids.contains(parent));
            if messages_parent.is_none() && workspace_parent.is_none() {
                continue;
            }
            let Ok(mut checkpoint) = self.read_checkpoint_file(&entry.id) else {
                tracing::warn!(checkpoint = %entry.id, "checkpoint 无法读取，跳过升级");
                continue;
            };
            let mut rewritten = false;

            if let Some(parent) = messages_parent.as_deref() {
                let Some(full) = self.messages_of(&checkpoint) else {
                    // 折叠失败说明这条链**已经**断了（例如历史遗留的多层增量）：
                    // 不能把尾部当成全量写回——那会永久丢掉前缀。
                    tracing::warn!(
                        checkpoint = %entry.id,
                        parent = %parent,
                        "增量父链不完整，无法升级为全量；删除父节点后该回滚点会被拒绝"
                    );
                    drop_ids.remove(parent);
                    continue;
                };
                checkpoint.messages = full;
                checkpoint.messages_delta = None;
                rewritten = true;
            }

            if let Some(parent) = workspace_parent.as_deref() {
                match resolve_to_files_caller(&self.dir, &checkpoint) {
                    Ok(resolved) => {
                        // **必须重新 intern**：升级后的 Files 快照会引用一批
                        // 原本只由父 checkpoint 持有的 blob（父链上 Unchanged
                        // 的文件）。不在这里 +1，删除父节点时的 release 会把
                        // 计数减到 0，`gc` 随后删掉升级后仍在引用的内容。
                        if let Some(cas) = self.cas.as_deref() {
                            for file in resolved.files.values() {
                                let Some(hash) = file.hash.as_deref() else {
                                    continue;
                                };
                                if let Ok(bytes) = cas.get(hash) {
                                    let _ = cas.intern(&bytes);
                                }
                            }
                        }
                        checkpoint.workspace = WorkspaceSnapshot::Files(resolved);
                        rewritten = true;
                    }
                    Err(error) => {
                        tracing::warn!(
                            checkpoint = %entry.id,
                            parent = %parent,
                            %error,
                            "工作区父链无法折叠，无法升级为全量；保留父节点"
                        );
                        drop_ids.remove(parent);
                        continue;
                    }
                }
            }

            if !rewritten {
                continue;
            }
            match write_json_atomic_counted(&self.checkpoint_path(&entry.id), &checkpoint) {
                Ok(_) => {
                    tracing::debug!(
                        checkpoint = %entry.id,
                        "父 checkpoint 即将被淘汰，已把增量节点升级为自包含形态"
                    );
                    if messages_parent.is_some() {
                        entry.messages_parent = None;
                    }
                    if workspace_parent.is_some() {
                        entry.baseline_id = None;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        checkpoint = %entry.id,
                        %error,
                        "checkpoint 升级失败；保留父节点以免子节点报废"
                    );
                    if let Some(parent) = messages_parent {
                        drop_ids.remove(&parent);
                    }
                    if let Some(parent) = workspace_parent {
                        drop_ids.remove(&parent);
                    }
                }
            }
        }
        if drop_ids.is_empty() {
            return Ok(());
        }

        // 先把待删的 checkpoint 全部折叠出来（`release` 需要完整历史里的
        // 全部 `cas://` 引用）。必须在任何 `remove_file` 之前做：删除是逐条
        // 进行的，先删了父节点，后面那条就折叠失败，只能看到尾部 →
        // 前缀 blob 的引用永远不会被释放。
        let mut dropped: Vec<(CheckpointSummary, Option<Checkpoint>)> = Vec::new();
        let mut kept: Vec<CheckpointSummary> = Vec::new();
        for mut entry in std::mem::take(&mut manifest.checkpoints) {
            if drop_ids.contains(&entry.id) && !foreign_live.contains(&entry.id) {
                let loaded = self.load_raw(&entry.id).ok();
                dropped.push((entry, loaded));
            } else {
                // 父节点已被（或即将被）删除：manifest 里的父指针也要一起
                // 清掉，否则下一次 prune 还会按旧指针去找一个不存在的父节点。
                if entry
                    .messages_parent
                    .as_deref()
                    .is_some_and(|parent| drop_ids.contains(parent))
                {
                    entry.messages_parent = None;
                }
                kept.push(entry);
            }
        }

        for (entry, loaded) in dropped {
            validate_checkpoint_id(&entry.id)?;
            let path = self.checkpoint_path(&entry.id);
            // **删除前先释放它引用的 CAS blob**。checkpoint 一旦外化
            // （image / thinking / 非 git 文件快照），它的正文只有 `cas://`
            // 占位符，真正的数据在 CAS 池里。只删文件不 release，那些 blob
            // 的 ref_count 永远停在 ≥1，`WorkspaceCas::gc` 永远删不掉——
            // 用「删 checkpoint 省磁盘」换「CAS 无界增长」，总量不减反增。
            //
            // 顺序：先 release 再删文件。反过来一旦删成功而 release 失败
            // （进程被 kill 在这个窗口），blob 就永久失去引用者，只能等
            // 人工 `storage prune` 之外的路径处理。先 release 最坏是计数多减
            // 一次（文件还在、内容仍可读）——这与 `release` 的幂等语义一致。
            if let (Some(cas), Some(checkpoint)) = (self.cas.as_deref(), loaded.as_ref()) {
                release_checkpoint_cas_refs(cas, &checkpoint.messages);
                release_workspace_cas_refs(cas, &checkpoint.workspace);
            }
            if let Err(error) = std::fs::remove_file(&path) {
                // 删不掉就留在 manifest 里：manifest 有条目而文件在，是无害的
                // （`load` 能读出来）；反过来才是数据损坏。
                tracing::warn!("删除旧 checkpoint 失败 {}: {error}", path.display());
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
        let mut checkpoint = self.load_raw(checkpoint_id)?;
        // 折叠失败时 `load_raw` 会保留本节点的尾部（`messages_delta` 仍为
        // `Some`）。**绝不能**把这半份历史交给 `/rewind`：它会静默覆盖用户
        // 的当前会话，而丢掉的正是被压缩/被淘汰之前的全部上下文。
        // 宁可报错让用户换一个更新的回滚点。
        anyhow::ensure!(
            checkpoint.messages_delta.is_none(),
            "checkpoint {checkpoint_id} 的增量父链不完整（父节点可能已被 retention 淘汰），\
             拒绝用半份历史回滚；请选择更新的回滚点，或改用 `--scope files`"
        );
        // 与 `create` 的 externalize 对称：盘上超大 image / 长 thinking 是
        // `cas://<hash>` 占位，回滚对话时要还原成真实内容，否则 `/rewind` 之后
        // 模型看到的是字面量 URL。blob 已被 gc 时 `materialize_block_with` 保留
        // 占位符（可诊断，不静默变成空）。
        //
        // 只在本方法还原、不在 `load_raw` 里还原：`capture_workspace_with_delta`
        // 等内部调用只需要 `workspace` 字段，对整段历史做一遍 blob 读取是纯浪费
        // ——长会话一次 checkpoint 就可能触发几十次 CAS 读盘。
        if self.cas.is_some() {
            let cas = self.cas.as_deref();
            for message in &mut checkpoint.messages {
                for block in &mut message.content {
                    crate::serialize::materialize_block_with(block, cas);
                }
            }
        }
        Ok(checkpoint)
    }

    /// 只反序列化、不做 CAS 还原。内部「只关心 `workspace`」的调用点走这里。
    ///
    /// 增量形态在这里**折叠回完整历史**：上层拿到的 `messages` 永远是完整的，
    /// 增量只是磁盘上的表示（见 [`MessagesDelta`]）。折叠失败（父 checkpoint
    /// 已被 retention 淘汰 / 父链长度对不上）时**不清空**，保留本文件里已有的
    /// 尾部并 `warn`——半份历史仍然比空历史有用，且 `rewind` 会因
    /// `message_count` 不匹配被上层拒绝，不会静默给出错误现场。
    fn load_raw(&self, checkpoint_id: &str) -> Result<Checkpoint> {
        validate_checkpoint_id(checkpoint_id)?;
        let mut checkpoint = self.read_checkpoint_file(checkpoint_id)?;
        if checkpoint.messages_delta.is_some() {
            self.fold_messages_delta(&mut checkpoint);
        }
        Ok(checkpoint)
    }

    /// 直接读文件，**不做**增量折叠。折叠自身要用它，否则会无限递归。
    fn read_checkpoint_file(&self, checkpoint_id: &str) -> Result<Checkpoint> {
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

    /// 决定这次 checkpoint 的 `messages` 存全量还是尾部增量。
    ///
    /// 返回 `(要落盘的 messages, 增量描述)`。增量描述的 `parent_len` 是**父
    /// checkpoint 的完整条数**，折叠时用它校验前缀关系仍然成立。
    ///
    /// 判据只有一条：**上一个 checkpoint 的完整历史必须是本快照的严格前缀**
    /// （`parent.len() < self.len()` 且逐条相等）。这一条同时覆盖了压缩、
    /// 分支、rewind、父节点被淘汰等所有不安全情形——它们都会让前缀不成立。
    ///
    /// 逐条比较用「引用相等 + 内容相等」而不是 hash：`Message` 没有实现
    /// `Hash`，而这里比较的是内存里刚拿到的两个 `Vec`，条数通常在千级，
    /// 内容比较的代价远低于一次全量序列化落盘。
    fn plan_messages_storage(&self, messages: &[Message]) -> (Vec<Message>, Option<MessagesDelta>) {
        let Some(parent_id) = self.last_written_id() else {
            return (messages.to_vec(), None);
        };
        // 父节点的**原始**深度：`load_raw` 折叠后会把 `messages_delta` 抹成
        // `None`，所以必须单独读一次未折叠的形态来取 `depth`。
        let parent_depth = match self.read_checkpoint_file(&parent_id) {
            Ok(raw) => raw.messages_delta.map(|delta| delta.depth).unwrap_or(0),
            // 父文件读不出来（被淘汰 / 损坏）→ 本次写全量。
            Err(_) => return (messages.to_vec(), None),
        };
        // 链到上限就切段：长链的根一定是最旧的 checkpoint，而 retention
        // 正是从最旧开始删。不主动切段的话，根一被淘汰，整条链上所有节点
        // 都只能退回「本节点尾部」——`/rewind` 拿到的是 1~2 条伪历史。
        if parent_depth >= MAX_MESSAGES_DELTA_WRITE_DEPTH {
            return (messages.to_vec(), None);
        }
        let Ok(parent) = self.load_raw(&parent_id) else {
            return (messages.to_vec(), None);
        };
        // 严格前缀：父历史比本快照短，且逐条相同。
        if parent.messages.len() >= messages.len() {
            return (messages.to_vec(), None);
        }
        if !parent
            .messages
            .iter()
            .zip(messages.iter())
            .all(|(a, b)| a == b)
        {
            return (messages.to_vec(), None);
        }
        let tail = messages[parent.messages.len()..].to_vec();
        (
            tail,
            Some(MessagesDelta {
                parent_checkpoint_id: parent_id,
                parent_len: parent.messages.len(),
                depth: parent_depth.saturating_add(1),
            }),
        )
    }

    /// 本 store 最近一次**成功写入**的 checkpoint id（用于增量父节点判定）。
    ///
    /// 读 manifest 而不是在 `CheckpointStore` 上挂内存字段：TUI 会反复
    /// 重建 store（`/model`、模式切换），内存字段一重建就丢，增量会退化成
    /// 全量；manifest 是磁盘上的单一事实源，重建后照样能接上前一个。
    fn last_written_id(&self) -> Option<String> {
        let manifest = self.load_manifest().ok()?;
        manifest
            .checkpoints
            .iter()
            .max_by(|a, b| a.timestamp.cmp(&b.timestamp))
            .map(|entry| entry.id.clone())
    }

    /// 把增量形态折叠成完整历史：递归取父链的完整 `messages`，再接上本节点的尾部。
    ///
    /// 三道防线：
    /// 1. 父文件不存在（被 retention 淘汰）→ 放弃折叠，保留尾部；
    /// 2. 父链折叠后的长度 ≠ `parent_len`（中间发生过压缩 / 分支）→ 放弃折叠，
    ///    因为此时「本节点尾部」未必接在父历史后面；
    /// 3. 链深度超过 [`MAX_MESSAGES_DELTA_DEPTH`] → 放弃折叠，防止病态长链
    ///    把一次 `load` 变成几十次递归读盘。
    fn fold_messages_delta(&self, checkpoint: &mut Checkpoint) {
        let Some(delta) = checkpoint.messages_delta.clone() else {
            return;
        };
        let tail = std::mem::take(&mut checkpoint.messages);
        match self.fold_parent_chain(&delta.parent_checkpoint_id, 0) {
            Some(mut parent) if parent.len() == delta.parent_len => {
                parent.extend(tail);
                checkpoint.messages = parent;
                checkpoint.messages_delta = None;
            }
            Some(parent) => {
                tracing::warn!(
                    checkpoint = %checkpoint.id,
                    expected = delta.parent_len,
                    actual = parent.len(),
                    "checkpoint 增量父链长度不符（可能中途压缩/分支），保留本节点尾部而不拼合"
                );
                checkpoint.messages = tail;
            }
            None => {
                tracing::warn!(
                    checkpoint = %checkpoint.id,
                    parent = %delta.parent_checkpoint_id,
                    "checkpoint 增量父节点不可用（已被淘汰？），保留本节点尾部而不拼合"
                );
                checkpoint.messages = tail;
            }
        }
    }

    /// 取某个 checkpoint 的完整历史，必要时沿父链继续折叠。`depth` 防病态长链。
    fn fold_parent_chain(&self, checkpoint_id: &str, depth: usize) -> Option<Vec<Message>> {
        if depth >= MAX_MESSAGES_DELTA_DEPTH {
            tracing::warn!(
                checkpoint = checkpoint_id,
                depth,
                "checkpoint 增量父链过深，停止折叠"
            );
            return None;
        }
        let mut parent = self.read_checkpoint_file(checkpoint_id).ok()?;
        if let Some(delta) = parent.messages_delta.clone() {
            let tail = std::mem::take(&mut parent.messages);
            let mut base = self.fold_parent_chain(&delta.parent_checkpoint_id, depth + 1)?;
            if base.len() != delta.parent_len {
                return None;
            }
            base.extend(tail);
            return Some(base);
        }
        Some(parent.messages)
    }

    /// 把单个 checkpoint（可能已是增量形态）还原成**完整历史**。
    ///
    /// `None` = 链断了（父节点被淘汰 / 长度对不上 / 深度超限）。与
    /// [`Self::fold_messages_delta`] 的区别：后者在失败时会**保留尾部**继续
    /// 返回 `Ok`（供 `/rewind` 列表等「能看一点算一点」的场景），而本方法
    /// 是给**写回全量**用的——失败时绝不能让调用方把尾部当成完整历史写盘，
    /// 那会把前缀永久删掉。
    fn messages_of(&self, checkpoint: &Checkpoint) -> Option<Vec<Message>> {
        match checkpoint.messages_delta.as_ref() {
            None => Some(checkpoint.messages.clone()),
            Some(delta) => {
                let mut base = self.fold_parent_chain(&delta.parent_checkpoint_id, 0)?;
                if base.len() != delta.parent_len {
                    return None;
                }
                base.extend(checkpoint.messages.iter().cloned());
                Some(base)
            }
        }
    }

    pub fn preview_files(&self, checkpoint_id: &str, cwd: &Path) -> Result<RewindPreview> {
        let checkpoint = self.load(checkpoint_id)?;
        preview_workspace(&checkpoint, cwd, self.cas.as_deref(), &self.dir)
    }

    /// 写回 checkpoint 文件状态。调用方必须先展示 [`RewindPreview`]，只有用户
    /// 显式确认后才传 `confirmed = true`；无变化时不要求确认。
    ///
    /// **同一工作目录上还有别的存活实例时，无人可问就必须拒绝**（`non_interactive`
    /// 为真，即 headless / `-p` / 定时任务 / 子 Agent）。确认门只对「有人能看着
    /// 预览按下确认」有意义；无人值守的运行里 `confirmed = true` 是调用方写死的，
    /// 它不构成任何同意。此时继续 `git restore` 等于单方面覆盖另一个实例尚未
    /// checkpoint 的编辑——对方的改动既没备份，也无从得知。
    ///
    /// 这与 `main.session_in_use_headless` 同一取向：拒绝比静默毁掉别人的工作安全。
    pub fn restore_files(
        &self,
        checkpoint_id: &str,
        cwd: &Path,
        confirmed: bool,
    ) -> Result<RewindPreview> {
        let checkpoint = self.load(checkpoint_id)?;
        let preview = preview_workspace(&checkpoint, cwd, self.cas.as_deref(), &self.dir)?;
        if !preview.shared_peers.is_empty() && self.non_interactive {
            anyhow::bail!(
                "refusing to restore files: another wyj-code instance (pid {}) is working in \
                 this tree, and this run has no interactive surface to confirm overwriting its \
                 uncommitted edits; stop the other instance, or run the rewind from an \
                 interactive session",
                preview
                    .shared_peers
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
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
    let mut preview = preview_workspace_inner(checkpoint, cwd, cas, self_dir)?;
    annotate_shared_peers(&mut preview, cwd);
    Ok(preview)
}

/// 把「同一工作目录上还有别的实例在跑」这一事实写进预览。
///
/// 只在**真的会覆盖文件**（`affected_files` 非空）时升级为「必须确认」：没有文件
/// 受影响时 `restore_files` 一个字节都不写，此时多要一次确认只会让人习惯性按 y。
fn annotate_shared_peers(preview: &mut RewindPreview, cwd: &Path) {
    if preview.affected_files.is_empty() {
        return;
    }
    preview.shared_peers = crate::instance::live_peers(cwd)
        .into_iter()
        .map(|record| record.pid)
        .collect();
    if preview.shared_peers.is_empty() {
        return;
    }
    preview.requires_confirmation = true;
    let peers = preview
        .shared_peers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let warning = format!(
        "another wyj-code instance (pid {peers}) is running in this same working tree; \
         restoring files will overwrite edits it has not checkpointed"
    );
    preview.note = Some(match preview.note.take() {
        Some(existing) => format!("{existing}; {warning}"),
        None => warning,
    });
}

fn preview_workspace_inner(
    checkpoint: &Checkpoint,
    cwd: &Path,
    _cas: Option<&WorkspaceCas>,
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
                shared_peers: Vec::new(),
            })
        }
        WorkspaceSnapshot::Files(snapshot) => {
            // 预览**不得**走 CAS：`capture_files` 会把每个文件内容 `intern()`
            // 一次并让 ref_count +1，而预览只是算 sha256 差异，不保存任何
            // 东西。走 CAS 的话「看一眼 /rewind 计划」就会永久泄漏一批引用，
            // `gc` 永远回收不掉。`cas = None` 时走内联字节，比较用的 sha256
            // 与大小完全相同。
            let current = capture_files(&snapshot.root, None)?;
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
                shared_peers: Vec::new(),
            })
        }
        WorkspaceSnapshot::Delta(_delta) => {
            // Delta checkpoint 的 preview:沿父链折叠到完整 snapshot 后再 diff
            // 当前 live 状态。注意折叠已读盘 N 次,O(N) 延迟。
            let resolved = resolve_to_files_caller(self_dir, checkpoint)?;
            // 同 `Files` 分支：预览不写 CAS，避免白增引用计数。
            let current = capture_files(&resolved.root, None)?;
            let affected_files = changed_file_paths(&resolved, &current);
            Ok(RewindPreview {
                checkpoint_id: checkpoint.id.clone(),
                requires_confirmation: !affected_files.is_empty(),
                affected_files,
                snapshot_complete: resolved.complete,
                note: Some("delta snapshot resolved across parent chain".into()),
                shared_peers: Vec::new(),
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
                //
                // **`hash` 在、`cas` 不在时绝不能回落到 `inline_bytes`**：外化
                // 过的 entry 其 `inline_bytes` 是空的，回落等于把文件写成 0
                // 字节——一次静默的数据销毁。CAS 打不开（目录不可写 / 只读
                // $HOME）是真实会发生的环境，此时必须整体失败。
                let bytes = match (&entry.hash, cas) {
                    (Some(hash), Some(c)) => match c.get(hash) {
                        Ok(b) => b,
                        Err(error) => {
                            tracing::warn!("CAS get 失败 (hash={hash}): {error} —— 跳过此文件恢复");
                            continue;
                        }
                    },
                    (Some(hash), None) => anyhow::bail!(
                        "checkpoint 的文件 {} 已外化到 CAS（{hash}），但当前 CAS 不可用；\
                         拒绝把它写成空文件。请检查 ~/.wyj-code/cas 是否可写",
                        relative.display()
                    ),
                    (None, _) => entry.inline_bytes.clone(),
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

/// checkpoint 的 JSON 落盘：**紧凑**格式 + 结构安全的敏感信息脱敏。
///
/// 与 `serialize::write_json_atomic` 的两点差异都是有意的，别顺手统一掉：
/// 1. 脱敏 —— checkpoint 会快照整个 workspace（含未提交的源码），比会话文件
///    更容易扫到用户硬编码的 token；会话侧同样脱敏，两者必须一致。
/// 2. 紧凑（`to_string` 而非 pretty）—— 每个 checkpoint 含一份完整 workspace 快照，
///    而保留条数由 `max_per_session` 控制（默认 21），缩进带来的膨胀会直接换成磁盘。
///
/// **脱敏走 `serde_json::Value` 而不是对文本跑正则**。此前这里是
/// `redact_cow(&json)`，那条正则的前缀 `\\?["']?` 会咬住 JSON 转义引号 `\"` 的
/// 反斜杠，而结尾引号被匹配掉却不回填——文件里于是出现未转义的裸引号，
/// **整个 checkpoint 解析失败**。实测（2026-10-20）有 120 个 checkpoint /
/// 约 133 MB 因此永久损坏（`Invalid \escape`）。
///
/// 转成 `Value` 再递归脱敏多一次内存拷贝，但换到的是「结构不可能被破坏」：
/// 改写只发生在 string 内容里，引号由序列化器统一处理。checkpoint 落盘本来
/// 就不是热路径，这个代价可以接受。
///
/// tmp + rename 的原子替换逻辑委托给 `serialize::write_atomic`，全仓只此一份。
fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    write_json_atomic_counted(path, value).map(|_| ())
}

/// [`write_json_atomic`] 的变体，返回落盘字节数。
///
/// 字节数用于 retention 的字节预算判定（见 `CheckpointSummary::bytes`）：
/// 只按条数淘汰挡不住长会话的 checkpoint 膨胀，实测 96% 的体积是内联的
/// 完整 `messages` 快照。
fn write_json_atomic_counted(path: &Path, value: &impl Serialize) -> Result<u64> {
    let mut as_value = serde_json::to_value(value)?;
    crate::secret::redact_json_value(&mut as_value);
    let json = serde_json::to_string(&as_value)?;
    let len = json.len() as u64;
    crate::serialize::write_atomic(path, json.as_bytes())?;
    Ok(len)
}

/// 释放一份 checkpoint 历史里所有 `cas://` 引用。
///
/// 与 `create` 的外化成对：外化时 `intern` 让 ref_count +1，checkpoint 被淘汰
/// （retention 裁剪 / 用户手动删）时必须 `release` 让计数 -1，否则
/// `WorkspaceCas::gc` 只回收 `ref_count == 0` 的 blob，这些数据会**永远**留在
/// 池子里。这是「checkpoint 删了、磁盘却没少」的成因。
///
/// 只处理本函数认识的两种外化形态（image 的 `cas://<hash>`、thinking 的
/// `[externalized to cas://<hash>, N bytes]`）。解析失败一律跳过而不是报错：
/// 这里处在删除路径上，一个畸形 checkpoint 不该阻止其它 checkpoint 被回收。
fn release_checkpoint_cas_refs(cas: &WorkspaceCas, messages: &[Message]) {
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        match block {
            wyj_api::types::ContentBlock::ToolResult {
                content: wyj_api::types::ToolResultContent::Parts(parts),
                ..
            } => {
                for part in parts {
                    if let wyj_api::types::ToolResultPart::Image { data, .. } = part {
                        if let Some(hash) = data.strip_prefix(crate::serialize::CAS_URI_PREFIX) {
                            if let Err(error) = cas.release(hash) {
                                tracing::warn!("释放 checkpoint image blob 失败 {hash}: {error}");
                            }
                        }
                    }
                }
            }
            wyj_api::types::ContentBlock::Thinking { thinking, .. } => {
                const PREFIX: &str = "[externalized to ";
                let Some(rest) = thinking.strip_prefix(PREFIX) else {
                    continue;
                };
                let Some(rest) = rest.strip_prefix(crate::serialize::CAS_URI_PREFIX) else {
                    continue;
                };
                // 占位符形态：`cas://<64hex>, N bytes]`
                let Some(hash) = rest.split(',').next().map(str::trim) else {
                    continue;
                };
                if hash.len() != 64 {
                    continue;
                }
                if let Err(error) = cas.release(hash) {
                    tracing::warn!("释放 checkpoint thinking blob 失败 {hash}: {error}");
                }
            }
            _ => {}
        }
    }
}

/// 释放一份 checkpoint 的**工作区快照**占用的 CAS 引用。
///
/// 与 [`release_checkpoint_cas_refs`] 成对，但覆盖的是另一半引用：`capture_files`
/// 对每个文件内容 `intern()`（非 git 项目），`capture_workspace_with_delta` 的
/// `Added`/`Modified` 也一样。只释放 messages 里的 image/thinking 引用，会让
/// 这部分 blob 的 ref_count 永远 ≥1 ——「删了 checkpoint、磁盘却没少」的另一半
/// 成因。Git 快照只存 object id，不占 CAS。
fn release_workspace_cas_refs(cas: &WorkspaceCas, snapshot: &WorkspaceSnapshot) {
    let release = |hash: &Option<String>| {
        if let Some(hash) = hash.as_deref() {
            if let Err(error) = cas.release(hash) {
                tracing::warn!("释放 checkpoint 文件 blob 失败 {hash}: {error}");
            }
        }
    };
    match snapshot {
        WorkspaceSnapshot::Git(_) => {}
        WorkspaceSnapshot::Files(files) => {
            for entry in files.files.values() {
                release(&entry.hash);
            }
        }
        WorkspaceSnapshot::Delta(delta) => {
            for op in delta.ops.values() {
                match op {
                    DeltaOp::Added { hash, .. } | DeltaOp::Modified { hash, .. } => {
                        release(hash);
                    }
                    DeltaOp::Removed => {}
                }
            }
        }
    }
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

    /// 同一工作目录上还有别的实例时，回滚**必须**升级为显式确认，并把对方 pid
    /// 写进预览。回滚是 `git restore` 直接覆盖文件，对方没有任何 checkpoint 可退。
    #[test]
    fn preview_warns_when_another_instance_shares_the_working_tree() {
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

        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s-shared").unwrap();
        let checkpoint = store
            .create(
                repo.path(),
                Vec::new(),
                CheckpointKind::Manual,
                Some("before".into()),
            )
            .unwrap();
        // 干净工作区：没有文件受影响，此时不该因为「有别人」就多要一次确认。
        let clean = store.preview_files(&checkpoint.id, repo.path()).unwrap();
        assert!(!clean.requires_confirmation);
        assert!(clean.shared_peers.is_empty());

        // 模拟另一个实例登记在同一 checkout 上。登记落在真实 `~/.wyj-code` 下，
        // 但 project_key 由本测试专属的 tempdir 派生，用例之间互不影响。
        // pid 用 1：本进程自己的登记会被 `collect` 过滤掉。
        let config_base = wyj_config::config_dir().unwrap();
        let peer_record = crate::instance::register_foreign_for_tests(&config_base, repo.path(), 1);

        std::fs::write(repo.path().join("tracked.txt"), "changed").unwrap();
        let preview = store.preview_files(&checkpoint.id, repo.path()).unwrap();
        assert!(
            preview.requires_confirmation,
            "有别的实例在同一 checkout 上时，回滚必须要求确认"
        );
        assert!(
            !preview.shared_peers.is_empty(),
            "预览必须点名对方 pid，用户才能判断该不该继续"
        );
        assert!(preview
            .note
            .as_deref()
            .is_some_and(|note| note.contains("another wyj-code instance")));
        crate::instance::remove_for_tests(&peer_record);
    }

    /// 无人可问时必须**拒绝**，而不是「确认一下就行」。
    ///
    /// `confirmed = true` 在 headless / `-p` / 定时任务里是调用方写死的常量，它不
    /// 代表任何人的同意。此时同一 checkout 上还有别的实例，继续 `git restore`
    /// 等于单方面抹掉对方尚未 checkpoint 的编辑。
    #[test]
    fn non_interactive_rewind_refuses_when_another_instance_shares_the_tree() {
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

        let sessions = tempfile::tempdir().unwrap();
        let interactive = CheckpointStore::new(sessions.path(), "s-ni").unwrap();
        let checkpoint = interactive
            .create(
                repo.path(),
                Vec::new(),
                CheckpointKind::Manual,
                Some("before".into()),
            )
            .unwrap();

        let config_base = wyj_config::config_dir().unwrap();
        let peer_record = crate::instance::register_foreign_for_tests(&config_base, repo.path(), 1);
        std::fs::write(repo.path().join("tracked.txt"), "changed").unwrap();

        // 交互面：显式确认后可以继续（这是 TUI 的路径）。
        interactive
            .restore_files(&checkpoint.id, repo.path(), true)
            .expect("interactive + confirmed 应当放行");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "base"
        );

        std::fs::write(repo.path().join("tracked.txt"), "changed again").unwrap();
        let headless = interactive.clone().with_non_interactive(true);
        let error = headless
            .restore_files(&checkpoint.id, repo.path(), true)
            .expect_err("无人可问 + 有别的实例，必须拒绝");
        assert!(
            error.to_string().contains("refusing to restore files"),
            "拒绝理由要说清是别人的改动会被覆盖: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "changed again",
            "拒绝时一个字节都不能写"
        );

        crate::instance::remove_for_tests(&peer_record);
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

    /// 损坏的 checkpoint 能被检出并清理，且**同时清掉 manifest 条目**。
    ///
    /// 只删文件不同步 manifest 会留下僵尸条目：`/rewind` 列表里出现选不中的
    /// 项，用户看到的是一个坏掉的回滚点而不是「这个点已经没了」。
    #[test]
    fn purge_corrupt_removes_both_file_and_manifest_entry() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "corrupt").unwrap();

        // 一份正常 checkpoint
        let good = store
            .create(
                root.path(),
                vec![Message::user("fine")],
                CheckpointKind::Manual,
                None,
            )
            .unwrap();
        // 一份手工写坏的（模拟脱敏正则破坏 JSON 结构）
        let bad_id = "corrupt000000000";
        let bad_path = store.checkpoint_path(bad_id);
        std::fs::write(&bad_path, br#"{"version":1,"id":"x","broken":"#).unwrap();
        store
            .mutate_manifest(|manifest| {
                manifest.checkpoints.push(CheckpointSummary {
                    id: bad_id.to_string(),
                    name: None,
                    kind: CheckpointKind::Manual,
                    timestamp: "2026-01-02T00:00:00Z".to_string(),
                    message_count: 1,
                    cwd_root: root.path().display().to_string(),
                    baseline_id: None,
                    owner_pid: std::process::id(),
                    bytes: 0,
                    messages_parent: None,
                });
                Ok(())
            })
            .unwrap();

        // 扫描：只报不改
        let (count, bytes) = store.scan_corrupt().unwrap();
        assert_eq!(count, 1, "应检出 1 个损坏文件");
        assert!(bytes > 0);
        assert!(bad_path.exists(), "scan_corrupt 不得删文件");

        // 清理：文件与 manifest 条目都消失
        let (removed, freed) = store.purge_corrupt().unwrap();
        assert_eq!(removed, 1);
        assert!(freed > 0);
        assert!(!bad_path.exists(), "损坏文件应被删除");
        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1, "manifest 应只剩正常那条");
        assert_eq!(remaining[0].id, good.id);
        assert!(
            store.checkpoint_path(&good.id).exists(),
            "正常 checkpoint 不受影响"
        );
    }

    /// 存量 checkpoint 能被就地重新外化，且**幂等**。
    ///
    /// 这是给「代码修好之后已写在盘上的那批」用的：`create` 的修复只影响
    /// 新写入，存量文件里的内联 base64 不会自己变小。
    #[test]
    fn reexternalize_shrinks_legacy_inline_checkpoints_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_dir = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_dir.path(), 0).unwrap(),
        );

        // 先造一份「旧版本写的」checkpoint：手工构造 Checkpoint 并直接落盘，
        // 绕过 `create` 的外化（模拟修复前产出）。
        let store = CheckpointStore::new(sessions.path(), "legacy")
            .unwrap()
            .with_cas(cas.clone());
        let big = "D".repeat(64 * 1024);
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            id: "legacy0000000000".to_string(),
            session_id: "legacy".to_string(),
            name: None,
            kind: CheckpointKind::Manual,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            messages_delta: None,
            messages: vec![Message {
                role: wyj_api::types::Role::User,
                content: vec![wyj_api::types::ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: wyj_api::types::ToolResultContent::Parts(vec![
                        wyj_api::types::ToolResultPart::Image {
                            media_type: "image/png".to_string(),
                            data: big.clone(),
                        },
                    ]),
                    is_error: false,
                }],
            }],
            workspace: WorkspaceSnapshot::Files(FileSnapshot {
                root: root.path().to_path_buf(),
                files: BTreeMap::new(),
                complete: true,
                skipped_files: 0,
                sensitive_files_skipped: 0,
                captured_bytes: 0,
            }),
        };
        let path = store.checkpoint_path(&checkpoint.id);
        write_json_atomic(&path, &checkpoint).unwrap();
        // 直接写 manifest，让 `list()` 能看见它。
        store
            .mutate_manifest(|manifest| {
                manifest.checkpoints.push(CheckpointSummary {
                    id: checkpoint.id.clone(),
                    name: None,
                    kind: CheckpointKind::Manual,
                    timestamp: checkpoint.timestamp.clone(),
                    message_count: 1,
                    cwd_root: root.path().display().to_string(),
                    baseline_id: None,
                    owner_pid: std::process::id(),
                    bytes: 0,
                    messages_parent: None,
                });
                Ok(())
            })
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        assert!(before > 64 * 1024, "初始应内联完整 base64: {before}");

        let (count, freed) = store.reexternalize_in_place().unwrap();
        assert_eq!(count, 1, "应处理 1 个文件");
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(freed > 0, "应报告释放字节");
        assert!(after < before / 2, "文件应显著缩小: {before} -> {after}");

        // 幂等：再跑一次不该有任何变化。
        let (count2, freed2) = store.reexternalize_in_place().unwrap();
        assert_eq!(count2, 0, "已外化的文件不应重复处理");
        assert_eq!(freed2, 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), after);
    }

    /// checkpoint 被 retention 淘汰时必须 release 它引用的 CAS blob。
    ///
    /// 否则「删 checkpoint 省磁盘」会换出「CAS 无界增长」：blob 的 ref_count
    /// 永远停在 ≥1，`WorkspaceCas::gc` 只回收 `ref_count == 0`，那些数据永远
    /// 留在池子里，总量不减反增。
    #[test]
    fn retention_releases_cas_blobs_of_dropped_checkpoints() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_dir = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_dir.path(), 0).unwrap(),
        );
        // 上限 1：每建一个新 checkpoint 就淘汰最老的那个。
        let store = CheckpointStore::new(sessions.path(), "retention-cas")
            .unwrap()
            .with_cas(cas.clone())
            .with_max_per_session(1);

        // 两份内容**不同**的外化 image：不同 hash，才能分别观察各自的引用计数。
        let message = |fill: char| Message {
            role: wyj_api::types::Role::User,
            content: vec![wyj_api::types::ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: wyj_api::types::ToolResultContent::Parts(vec![
                    wyj_api::types::ToolResultPart::Image {
                        media_type: "image/png".to_string(),
                        data: fill.to_string().repeat(64 * 1024),
                    },
                ]),
                is_error: false,
            }],
        };

        let first = store
            .create(
                root.path(),
                vec![message('B')],
                CheckpointKind::Manual,
                None,
            )
            .unwrap();
        assert_eq!(cas.stats().unwrap().orphan_blobs, 0, "第一份 blob 有引用");

        store
            .create(
                root.path(),
                vec![message('C')],
                CheckpointKind::AutoUser,
                None,
            )
            .unwrap();

        assert!(
            !store.checkpoint_path(&first.id).exists(),
            "最老的 checkpoint 应被 retention 删除"
        );
        // 第一份 blob 已无引用者 -> 进入 orphan，可被 gc 回收。
        // 第二个 checkpoint 的那份仍有 1 个引用，不算 orphan。
        let stats = cas.stats().unwrap();
        assert_eq!(
            stats.orphan_blobs, 1,
            "被淘汰 checkpoint 的 blob 应已 release，另一个仍在用：{stats:?}"
        );
    }

    /// checkpoint 里的超大 image 必须外化到 CAS，而不是在每个快照里内联一份。
    ///
    /// 回归钉子：`create` 曾经只有 `truncate_content_block`，而它对
    /// `ToolResultPart::Image` 明确不动（"避免引入运行时去重逻辑"）。于是同一批
    /// 工具截图在保留的 20 个 checkpoint 里各存一份完整 base64 —— 实测占
    /// `~/.wyj-code/sessions` 的 40%（1.21 GB / 3.02 GB）。
    #[test]
    fn large_images_are_externalized_instead_of_inlined() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let cas_dir = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_dir.path(), 0).unwrap(),
        );
        let store = CheckpointStore::new(sessions.path(), "img-session")
            .unwrap()
            .with_cas(cas.clone());

        // 64KB base64：超过 externalize 的 32KB 阈值。
        let big = "A".repeat(64 * 1024);
        let message = Message {
            role: wyj_api::types::Role::User,
            content: vec![wyj_api::types::ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: wyj_api::types::ToolResultContent::Parts(vec![
                    wyj_api::types::ToolResultPart::Image {
                        media_type: "image/png".to_string(),
                        data: big.clone(),
                    },
                ]),
                is_error: false,
            }],
        };
        let summary = store
            .create(root.path(), vec![message], CheckpointKind::Manual, None)
            .unwrap();

        let raw = std::fs::read_to_string(store.checkpoint_path(&summary.id)).unwrap();
        assert!(
            !raw.contains(&big),
            "checkpoint 不应内联完整 base64（这正是 1.2GB 膨胀的来源）"
        );
        assert!(
            raw.contains(crate::serialize::CAS_URI_PREFIX),
            "应留下 cas:// 占位符"
        );

        // 读回来必须还原成原始数据，否则 `/rewind` 之后模型看到的是字面量 URL。
        let loaded = store.load(&summary.id).unwrap();
        match &loaded.messages[0].content[0] {
            wyj_api::types::ContentBlock::ToolResult {
                content: wyj_api::types::ToolResultContent::Parts(parts),
                ..
            } => match &parts[0] {
                wyj_api::types::ToolResultPart::Image { data, .. } => assert_eq!(data, &big),
                other => panic!("unexpected part: {other:?}"),
            },
            other => panic!("unexpected block: {other:?}"),
        }
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
        // **脱敏后必须仍是合法 JSON**。这是原 bug 缺的那条断言：此前只查
        // 「不含明文 secret」，而恰恰是脱敏本身把 JSON 结构改坏了——120 个
        // checkpoint 因此永久读不出。断言「能解析回来」才是真正的不变量。
        let reparsed: Checkpoint =
            serde_json::from_str(&raw).expect("脱敏后的 checkpoint 必须是合法 JSON");
        assert_eq!(reparsed.id, summary.id);
    }

    /// 脱敏不能破坏 JSON 结构：明文 secret 出现在**含引号与反斜杠的上下文**里
    /// 时（真实的 `curl -H "X-Token: ..."` 形态），仍必须产出可解析的 JSON。
    ///
    /// 这是 120 个 checkpoint 损坏事故的最小复现形态：旧实现用正则替换
    /// **序列化后的文本**，把 `\"` 拆成 `\` + 消失的引号，留下裸引号。
    #[test]
    fn redaction_keeps_json_parseable_with_quoted_secret_context() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "quote-session").unwrap();
        let secret = format!("{}{}", "sk-test-", "F".repeat(24));
        // 关键：secret 前面有 `-H \"X-Subscription-Token: ` 这种带转义引号的上下文：
        // 与真实事故现场一致——HTTP 客户端命令行把 header 值用引号括起来。
        let payload =
            format!("req -H \\\"X-Subscription-Token: {secret}\\\" https://example.invalid");
        let summary = store
            .create(
                root.path(),
                vec![Message::user(payload)],
                CheckpointKind::Manual,
                None,
            )
            .unwrap();
        let raw = std::fs::read_to_string(store.checkpoint_path(&summary.id)).unwrap();
        assert!(!raw.contains(&secret), "明文 secret 不应落盘");
        let reparsed = serde_json::from_str::<Checkpoint>(&raw);
        assert!(
            reparsed.is_ok(),
            "带引号上下文的 secret 脱敏后 JSON 必须仍可解析，实际: {:?}",
            reparsed.err()
        );
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
            max_bytes_per_session: 0,
            cas: None,
            non_interactive: false,
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
            max_bytes_per_session: 0,
            cas: None,
            non_interactive: false,
        });
        set_checkpoint_config(CheckpointConfig {
            max_per_session: 5,
            max_bytes_per_session: 0,
            cas: None,
            non_interactive: false,
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
                bytes: 0,
                messages_parent: None,
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
                    bytes: 0,
                    messages_parent: None,
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
                bytes: 0,
                messages_parent: None,
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

    /// 字节闸门：条数没超，但总量超预算时也必须淘汰最旧的。
    ///
    /// 钉的是「按条数留 20 个挡不住长会话」这个实测缺陷：本机 5,703 个
    /// checkpoint 合计 1,836 MB，平均 322 KB，其中 96% 是内联的完整
    /// `messages` 快照。只配条数时，一个 1,400 条消息的会话照样能留 20 份
    /// 几百 KB 的历史。
    #[test]
    fn byte_budget_prunes_even_when_count_is_within_limit() {
        let sessions = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::new(sessions.path(), "s-bytes").unwrap();
        store.max_per_session = 10; // 条数闸门很宽松，永不触发
        store.max_bytes_per_session = 250; // 每个 100 字节 → 只装得下 2 个
        let mut manifest = CheckpointManifest::default();
        for i in 0..5 {
            let id = format!("cp{i}");
            std::fs::write(
                sessions
                    .path()
                    .join(format!("s-bytes.checkpoints/{id}.json")),
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
                bytes: 100,
                messages_parent: None,
            });
        }
        std::fs::write(
            sessions.path().join("s-bytes.checkpoints/manifest.json"),
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
        // 500 字节总量、250 预算 → 砍掉最旧的 3 个，留下最新的 2 个。
        assert_eq!(remaining, vec!["cp3".to_string(), "cp4".to_string()]);
    }

    /// 条数与字节两个闸门取**更严**的那个：任一超标都要满足。
    #[test]
    fn retention_takes_the_stricter_of_count_and_byte_gates() {
        let sessions = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::new(sessions.path(), "s-both").unwrap();
        store.max_per_session = 4; // 5 条 → 需删 1
        store.max_bytes_per_session = 150; // 5×100=500 → 需删 4
        let mut manifest = CheckpointManifest::default();
        for i in 0..5 {
            let id = format!("cp{i}");
            std::fs::write(
                sessions
                    .path()
                    .join(format!("s-both.checkpoints/{id}.json")),
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
                bytes: 100,
                messages_parent: None,
            });
        }
        std::fs::write(
            sessions.path().join("s-both.checkpoints/manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        store.enforce_retention().unwrap();
        // 字节闸门更严（要删 4），所以只剩最新的 1 个。
        assert_eq!(store.list().unwrap().len(), 1);
    }

    /// `bytes == 0` 的老条目（字段引入前写入）不占预算，也不该被字节闸门选中。
    ///
    /// 否则升级后第一次启动就会把用户仅剩的老回滚点全部清掉——它们不占空间，
    /// 砍掉纯粹是白丢历史。
    #[test]
    fn legacy_entries_without_byte_count_are_never_pruned_by_byte_gate() {
        let sessions = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::new(sessions.path(), "s-legacy").unwrap();
        store.max_per_session = 0; // 只开字节闸门
        store.max_bytes_per_session = 150;
        let mut manifest = CheckpointManifest::default();
        // 3 条老条目（bytes=0）+ 2 条新条目（各 100 字节）
        for (i, bytes) in [0u64, 0, 0, 100, 100].into_iter().enumerate() {
            let id = format!("cp{i}");
            std::fs::write(
                sessions
                    .path()
                    .join(format!("s-legacy.checkpoints/{id}.json")),
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
                bytes,
                messages_parent: None,
            });
        }
        std::fs::write(
            sessions.path().join("s-legacy.checkpoints/manifest.json"),
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
        // 200 字节 ≤ 150？不，超了 50 → 砍掉最旧的**一个带字节的**条目（cp3）。
        // 三条老条目（bytes=0）全部保留。
        assert_eq!(
            remaining,
            vec![
                "cp0".to_string(),
                "cp1".to_string(),
                "cp2".to_string(),
                "cp4".to_string()
            ]
        );
    }

    /// 增量必须真正减小落盘体积，且读回后折叠成完整历史。
    ///
    /// 钉的是实测缺陷（2026-10-28）：5,703 个 checkpoint / 1,836 MB，抽样
    /// 30 个发现 96% 的体积是内联的完整 `messages` 快照。这里用「历史逐步
    /// 增长」模拟一次回合里连续几次有副作用的工具调用。
    #[test]
    fn incremental_messages_shrink_disk_and_fold_back() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s-inc").unwrap();

        // 造一段有实质体积的历史，让「全量 vs 尾部」的差异可测。
        let filler = "x".repeat(2_000);
        let mut history: Vec<Message> = Vec::new();
        let mut sizes = Vec::new();
        for i in 0..4 {
            history.push(Message::user(format!("turn {i} {filler}")));
            history.push(Message {
                role: wyj_api::types::Role::Assistant,
                content: vec![wyj_api::types::ContentBlock::Text {
                    text: format!("reply {i} {filler}"),
                }],
            });
            let summary = store
                .create(root.path(), history.clone(), CheckpointKind::PreTool, None)
                .unwrap();
            sizes.push(
                std::fs::metadata(store.checkpoint_path(&summary.id))
                    .unwrap()
                    .len(),
            );
        }

        // 后三个必须是增量（只存尾部 2 条），所以体积**不随历史增长**：
        // 第 1 个是全量（2 条）、第 4 个若也是全量就该是 8 条，差出 3 倍。
        // 这里比的是「增量之间彼此接近」而不是「增量 < 首个全量」——首个
        // checkpoint 含 baseline 工作区快照（体积固定开销），可能反而更大。
        assert!(
            sizes[2] <= sizes[1] + 512 && sizes[3] <= sizes[1] + 512,
            "增量体积不应随历史增长：{sizes:?}"
        );
        assert!(
            sizes[3] < sizes[0] * 2,
            "第四个 checkpoint 若是全量应约为首个的 4 倍：{sizes:?}"
        );

        // 读回必须折叠成完整历史，条数与内容都对。
        let latest = store.list().unwrap().last().cloned().unwrap();
        assert_eq!(latest.message_count, history.len(), "summary 报完整条数");
        let loaded = store.load(&latest.id).unwrap();
        assert_eq!(loaded.messages.len(), history.len());
        assert_eq!(loaded.messages, history, "折叠后必须逐条等于原始历史");
        assert!(!loaded.is_messages_delta(), "折叠后不再是增量形态");
    }

    /// 历史被整体重写（压缩 / 分支）时必须退回全量，不能拼错。
    #[test]
    fn non_prefix_history_falls_back_to_full_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s-prefix").unwrap();

        let first = store
            .create(
                root.path(),
                vec![
                    Message::user("a"),
                    Message {
                        role: wyj_api::types::Role::Assistant,
                        content: vec![wyj_api::types::ContentBlock::Text {
                            text: "b".to_string(),
                        }],
                    },
                ],
                CheckpointKind::AutoUser,
                None,
            )
            .unwrap();
        // 完全换掉历史（模拟压缩后的单条摘要），前缀不成立。
        let rewritten = store
            .create(
                root.path(),
                vec![Message::user("compressed summary")],
                CheckpointKind::PreTool,
                None,
            )
            .unwrap();

        let raw = std::fs::read_to_string(store.checkpoint_path(&rewritten.id)).unwrap();
        assert!(
            !raw.contains("messages_delta"),
            "前缀不成立时必须存全量，不能写增量描述"
        );
        assert_eq!(store.load(&rewritten.id).unwrap().messages.len(), 1);
        assert_eq!(store.load(&first.id).unwrap().messages.len(), 2);
    }

    /// 父 checkpoint 缺失时，`load` 必须**报错**而不是交出半份历史。
    ///
    /// 历史行为是「保留本节点尾部继续返回 Ok」，而 `/rewind` 会拿这份 1~2 条
    /// 的伪历史直接覆盖用户当前会话——静默丢掉整段对话。现在内部
    /// `load_raw` 仍保留尾部（供诊断 / 内部对比），但面向回滚的 `load`
    /// 拒绝返回不完整的历史。
    #[test]
    fn delta_with_missing_parent_is_refused_by_load() {
        let root = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(sessions.path(), "s-orphan").unwrap();

        let first = store
            .create(
                root.path(),
                vec![
                    Message::user("a"),
                    Message {
                        role: wyj_api::types::Role::Assistant,
                        content: vec![wyj_api::types::ContentBlock::Text {
                            text: "b".to_string(),
                        }],
                    },
                ],
                CheckpointKind::AutoUser,
                None,
            )
            .unwrap();
        let second = store
            .create(
                root.path(),
                vec![
                    Message::user("a"),
                    Message {
                        role: wyj_api::types::Role::Assistant,
                        content: vec![wyj_api::types::ContentBlock::Text {
                            text: "b".to_string(),
                        }],
                    },
                    Message::user("c"),
                ],
                CheckpointKind::PreTool,
                None,
            )
            .unwrap();
        // 确认第二个确实是增量。
        let raw = std::fs::read_to_string(store.checkpoint_path(&second.id)).unwrap();
        assert!(raw.contains("messages_delta"), "应写成增量");

        // 手动删掉父 checkpoint，模拟被 retention 淘汰。
        std::fs::remove_file(store.checkpoint_path(&first.id)).unwrap();

        // 内部容错读取仍保留本节点尾部（1 条）——诊断路径需要一个「能读到
        // 一点」的结果。
        let raw = store.load_raw(&second.id).unwrap();
        assert_eq!(raw.messages.len(), 1, "load_raw 应保留自身尾部");
        assert_eq!(raw.messages[0], Message::user("c"));

        // 但面向 `/rewind` 的 `load` 必须拒绝：半份历史覆盖会话是不可逆的。
        let error = store.load(&second.id).expect_err("父链断了必须报错");
        assert!(
            error.to_string().contains("增量父链不完整"),
            "错误信息应说明原因，实际: {error}"
        );
    }

    /// `estimate_retention_drops` 必须与 `enforce_retention` 的实际删除数一致。
    ///
    /// 这条钉的是 dry-run 的诚实性：此前 dry-run 算 `before - max`，把
    /// `owner_pid == 0` 的旧条目也算成可删，实测某会话报「将删除 23 条」而
    /// `--yes` 实删 0 条。
    #[test]
    fn estimate_matches_actual_retention_drops() {
        let sessions = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::new(sessions.path(), "s-est").unwrap();
        store.max_per_session = 1;
        let mut manifest = CheckpointManifest::default();
        // 3 条 owner_pid == 0（旧版本写的，保守视为可能活着）+ 2 条自己创建的
        for (i, owner) in [0u32, 0, 0, std::process::id(), std::process::id()]
            .into_iter()
            .enumerate()
        {
            let id = format!("cp{i}");
            std::fs::write(
                sessions.path().join(format!("s-est.checkpoints/{id}.json")),
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
                owner_pid: owner,
                bytes: 0,
                messages_parent: None,
            });
        }
        std::fs::write(
            sessions.path().join("s-est.checkpoints/manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let estimated = store.estimate_retention_drops().unwrap();
        let before = store.list().unwrap().len();
        store.enforce_retention().unwrap();
        let actual = before - store.list().unwrap().len();
        assert_eq!(
            estimated, actual,
            "dry-run 预估 {estimated} 条，实际删除 {actual} 条——两者必须同源"
        );
        // 3 条 owner_pid==0 被保守保留，自己创建的 2 条只能留 1 条
        assert_eq!(actual, 1);
    }
}
