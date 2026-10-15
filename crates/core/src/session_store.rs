//! Session 持久化存储：~/.wyj-code/sessions/<session-id>.json

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use wyj_api::types::{ContentBlock, Message, Role};

use crate::session::{RoutingEvent, Session};

/// 会话占用租约。持有期间别的 `wyj-code` 会把它读成「这个会话正被使用」，
/// 从而在 `--resume` 时询问用户；`drop`（正常退出、panic unwind）时撤销。
///
/// 进程被 `SIGKILL` / 断电时撤销不会执行，残留文件由
/// [`wyj_config::filelock::active_holder`] 的存活判定忽略。
pub struct SessionClaim {
    path: PathBuf,
}

impl SessionClaim {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::serialize::write_atomic(path, format!("{}\n", std::process::id()).as_bytes())?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        // 只删自己写的那个：万一中途被别的进程接管了同一文件（极端情况），
        // 删掉会让对方失去占用标记。这里以内容比对做最后一道保险。
        let ours = format!("{}\n", std::process::id());
        if std::fs::read_to_string(&self.path).is_ok_and(|text| text == ours) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 持久化到磁盘的完整会话数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub session_id: String,
    pub title: String,
    pub last_preview: String,
    pub cwd: String,
    pub timestamp: String,
    pub turns: usize,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub routing_events: Vec<RoutingEvent>,
    #[serde(default)]
    pub current_checkpoint_id: Option<String>,
    #[serde(default)]
    pub branch_parent_session_id: Option<String>,
    #[serde(default)]
    pub branch_parent_checkpoint_id: Option<String>,
    /// 是否已通过 LLM 生成过标题（首轮后生成一次，之后固定）
    #[serde(default)]
    pub title_generated: bool,
    /// 本会话自动压缩次数。落盘是为了 `/resume` 后状态栏的"已自动压缩 ×N"
    /// 指示不消失——压缩对用户不可见，少了这个数字就等于黑盒。
    #[serde(default)]
    pub compact_count: u32,
    /// 本会话被 context editing 外部化到 CAS 的 blob hash。落盘是为了会话被
    /// 丢弃时能逐个 `release` —— CAS 的 `gc` 只回收 `ref_count == 0` 的 blob。
    #[serde(default)]
    pub elided_blobs: Vec<String>,
    /// 本会话 context editing 累计释放的估算 token。
    #[serde(default)]
    pub context_edit_freed_tokens: u32,
    /// 累计命中 prompt 缓存的输入 token 数（按约 0.1x 输入价计费，**不**计入
    /// `input_tokens`）。落盘是为了 `/resume` 后 `/cost` 仍能算缓存命中率——
    /// 旧文件没有这两个字段，`resume` 回来的缓存恒为 0，成本被系统性低估。
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// 累计写入 prompt 缓存的输入 token 数（按约 1.25x 输入价计费，同样
    /// 不计入 `input_tokens`）。
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// 本会话累计模型推理次数（每次 provider.stream 计 1，压缩/摘要往返也计）。
    /// 落盘后才能算出「每次调用的平均 input」——input 消耗的一阶乘数是这个数，
    /// 不是单次 token 数。
    #[serde(default)]
    pub api_calls: u32,
    /// **累计**随请求发送的工具 schema 估算 token（全会话求和）。
    ///
    /// ⚠️ 这是**累加值**，不是单请求稳态值——200 次调用的会话会累到百万级
    /// （实测最高 1,457,192），真实单请求值只有 6,281。展示"当前上下文里
    /// 工具定义占了多少"必须改用 `tool_schema_tokens_last`，或直接读
    /// `ContextAudit::tool_schema_tokens`（`/context` 面板走的就是后者）。
    #[serde(default)]
    pub tool_schema_tokens: u32,
    /// **最近一次请求**发送的工具 schema 估算 token（稳态值）。
    ///
    /// 不落盘：`/resume` 回来的会话尚未发起过新请求，没有任何"最近一次"
    /// 可言，填一个陈旧值比留 0 更糟（用户会以为当前请求真发了那么多）。
    /// 下一次请求发出后本字段自然被刷新。
    #[serde(default, skip_serializing)]
    pub tool_schema_tokens_last: u32,
    /// lazy tool schema 相对全量工具集合累计避免发送的估算 token。
    #[serde(default)]
    pub tool_schema_tokens_saved: u32,
    /// 子 Agent 扇出累计输入 token。与 `input_tokens` 不重叠。
    #[serde(default)]
    pub sub_input_tokens: u32,
    /// 子 Agent 扇出累计输出 token。
    #[serde(default)]
    pub sub_output_tokens: u32,
    /// 子 Agent 扇出累计缓存命中 token。
    #[serde(default)]
    pub sub_cache_read_tokens: u32,
    /// 子 Agent 扇出累计缓存写入 token。
    #[serde(default)]
    pub sub_cache_write_tokens: u32,
    /// 本会话实际生效的 prompt cache 模式（见 [`PromptCacheState`]）。
    /// 存 u8 而非 enum 是为了旧文件 `#[serde(default)]` 直接落到 0，
    /// 也避免将来加枚举值时反序列化失败。
    #[serde(default)]
    pub prompt_cache_state: u8,
}

/// `SessionFile::prompt_cache_state` 的取值。序列化为 u8 以保持
/// `#[serde(default)]` 的前向兼容（枚举将来加值也不会让旧文件读不出来）。
pub mod prompt_cache_state {
    /// 未启用（默认，含第三方端点未显式配置 `prompt_cache = true` 的情况）。
    pub const OFF: u8 = 0;
    /// 已启用且正常工作。
    pub const ON: u8 = 1;
    /// 曾启用，但该端点返回 400（不支持 cache_control / beta 头），已自动降级。
    /// 持久化这个状态是为了 `/cost` 能解释「为什么缓存是 0」——否则用户只能
    /// 看到全零却无从判断是没用还是用不了。
    pub const DOWNGRADED: u8 = 2;
}

/// 调用方各自计算、但不属于 `Session` 的会话元数据。
///
/// 之所以要单独一个 struct：12 处 `SessionFile { .. }` 构造点各自算
/// `title` / `last_preview` / `timestamp` / `turns`（不同路径算法略有差异），
/// 而计量字段必须由 `Session` 单点搬运。拆开后 `from_session` 成为唯一的
/// 搬运入口，将来再加计量字段只需要改这一处。
#[derive(Debug, Clone)]
pub struct SessionFileMeta {
    pub session_id: String,
    pub title: String,
    pub last_preview: String,
    pub cwd: String,
    pub timestamp: String,
    pub turns: usize,
    pub title_generated: bool,
}

impl SessionFile {
    /// 从内存 `Session` 构造落盘结构：**所有计量字段的唯一搬运入口**。
    ///
    /// 12 处调用点原先各自手写全量字段，新增计量字段时必然漏改——这正是
    /// v0.2.9~v0.2.33 期间 `cache_read_tokens` / `api_calls` 只在内存里活着、
    /// `resume` 后 `/cost` 全零的根因。统一走这里后，漏改在编译期就会暴露。
    pub fn from_session(session: &Session, meta: SessionFileMeta) -> Self {
        Self {
            session_id: meta.session_id,
            title: meta.title,
            last_preview: meta.last_preview,
            cwd: meta.cwd,
            timestamp: meta.timestamp,
            turns: meta.turns,
            title_generated: meta.title_generated,
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            cache_read_tokens: session.total_cache_read_tokens,
            cache_write_tokens: session.total_cache_write_tokens,
            api_calls: session.api_calls,
            tool_schema_tokens: session.tool_schema_tokens,
            tool_schema_tokens_last: session.tool_schema_tokens_last,
            tool_schema_tokens_saved: session.tool_schema_tokens_saved,
            sub_input_tokens: session.sub_input_tokens,
            sub_output_tokens: session.sub_output_tokens,
            sub_cache_read_tokens: session.sub_cache_read_tokens,
            sub_cache_write_tokens: session.sub_cache_write_tokens,
            prompt_cache_state: session.prompt_cache_state,
            compact_count: session.compact_count,
            context_edit_freed_tokens: session.context_edit_freed_tokens,
            elided_blobs: session.elided_blobs.clone(),
            routing_events: session.routing_events.clone(),
            current_checkpoint_id: session.current_checkpoint_id.clone(),
            branch_parent_session_id: session.branch_parent_session_id.clone(),
            branch_parent_checkpoint_id: session.branch_parent_checkpoint_id.clone(),
            messages: session.messages.clone(),
        }
    }
}

/// 会话摘要（不含消息体，用于列表展示）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub title: String,
    pub last_preview: String,
    pub cwd: String,
    pub timestamp: String,
    pub turns: usize,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub title_generated: bool,
    #[serde(default)]
    pub branch_parent_session_id: Option<String>,
    #[serde(default)]
    pub branch_parent_checkpoint_id: Option<String>,
}

impl From<SessionFile> for SessionMeta {
    fn from(f: SessionFile) -> Self {
        Self {
            session_id: f.session_id,
            title: f.title,
            last_preview: f.last_preview,
            cwd: f.cwd,
            timestamp: f.timestamp,
            turns: f.turns,
            input_tokens: f.input_tokens,
            output_tokens: f.output_tokens,
            title_generated: f.title_generated,
            branch_parent_session_id: f.branch_parent_session_id,
            branch_parent_checkpoint_id: f.branch_parent_checkpoint_id,
        }
    }
}

/// 会话文件存储（~/.wyj-code/sessions/）
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.json"))
    }

    /// 会话写入的锁文件。与 `.json` 同目录，避免和 checkpoint 的旁路存储混淆。
    fn lock_path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.json.lock"))
    }

    /// 会话占用租约文件。与写入锁分开：写入锁的持有周期是一次 save，租约是整个
    /// 会话生命周期，两者语义完全不同。
    fn claim_path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.json.owner"))
    }

    /// 会话文件所在目录（`~/.wyj-code/sessions/`），供旁路存储（如子 Agent
    /// trace，见 `wyj_tools::trace`）按同一根目录 + `session_id` 定位文件。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save(&self, file: &SessionFile) -> Result<()> {
        // 落盘前做持久化截断(tool_result / thinking / tool_use.input 等),
        // 见 `serialize::truncate_session_for_persistence` 与
        // `PersistCapCfg` —— 任意字段为 0 即关闭对应截断,保持旧行为。
        //
        // Phase 3:再调 `externalize_block` 把超大 image data / thinking
        // 移到 CAS blob pool,避免 base64 大字段膨胀 session.json。
        //
        // 这里走 `clone` 而非原地 mutate,是因为 `&SessionFile` 借用不允
        // 许 truncate 调用原地改 messages。`SessionFile::clone` 主要成本在
        // `Vec<Message>`,而 truncate 正是要削减其体积;序列化前 clone 比
        // 序列化完整 messages 仍便宜得多。
        let mut owned = file.clone();
        if let Some(cfg) = current_persist_cap() {
            crate::serialize::truncate_session_for_persistence(&mut owned, &cfg);
        }
        for msg in &mut owned.messages {
            for block in &mut msg.content {
                crate::serialize::externalize_block(block);
            }
        }
        let json = serde_json::to_string(&owned)?;
        let path = self.path(&file.session_id);
        // 两个进程 resume 同一个 session 时，此前是裸 `fs::write` 整文件截断：
        // 并发写会截出半截 JSON，`load` 直接 Err，该会话再也恢复不了。锁把写入
        // 串行化，tmp+rename 保证任何时刻磁盘上的文件都是完整的一版。
        let _guard =
            wyj_config::filelock::acquire(&self.lock_path(&file.session_id), "session file")?;
        crate::serialize::write_atomic(
            &path,
            crate::secret::redact_sensitive_text(&json).as_bytes(),
        )
    }

    /// 声明「本进程正在使用这个会话」，返回的守卫 drop 时自动撤销声明。
    ///
    /// 为什么需要它而不直接看写入锁：写入锁（`filelock`）只在 `save()` 的那一瞬间
    /// 持有，读锁文件拿到的其实是**最后一个写者的 pid**。对方刚 `--resume`、还没保存
    /// 时那把锁根本不存在，`active_owner` 会漏检；等它保存过再退出，pid 又已失效。
    /// 语义要的是「从打开到关闭一直持有」，这只能靠一个独立于写入周期的租约表达。
    ///
    /// 崩溃/被 kill 留下的残留租约由 [`Self::active_owner`] 的存活判定自动忽略，
    /// 不需要额外的清理任务。
    pub fn claim(&self, session_id: &str) -> Result<SessionClaim> {
        SessionClaim::acquire(&self.claim_path(session_id))
    }

    /// 另一个**仍存活**的 wyj-code 进程是否正在使用该会话。
    ///
    /// 会话文件是「整文件覆盖」而非追加，两个进程各持载入时的快照，谁后保存谁覆盖
    /// 谁——`filelock` 只能防止写坏，防不了逻辑上的丢更新。所以 resume 侧必须主动
    /// 检出占用并让用户决定，见 CLI 的接管询问。
    pub fn active_owner(&self, session_id: &str) -> Option<u32> {
        wyj_config::filelock::active_holder(&self.claim_path(session_id))
    }

    pub fn load(&self, session_id: &str) -> Result<SessionFile> {
        let content = std::fs::read_to_string(self.path(session_id))?;
        let mut file: SessionFile = serde_json::from_str(&content)?;
        // Resume 路径也走 persist_cap:v0.2.19 之前落盘的 session 没有
        // text/tool_result/thinking 等任何上限;若不在 load 后重 cap,resume
        // 出来的 in-memory messages 会把运行时单次 LLM 请求撑爆。
        // 与 save() 同源(`truncate_session_for_persistence` 内部委托
        // `truncate_messages`),保证 disk / runtime / resume 三条路径共用
        // 同一上限。
        if let Some(cfg) = current_persist_cap() {
            crate::serialize::truncate_session_for_persistence(&mut file, &cfg);
        }
        // 落盘文件里 context editing 清理过的工具结果是占位符（体积小），原文在
        // CAS。Resume 的语义是「新会话从完整上下文开始」——这里把原文还原回内存，
        // 之后随会话增长再逐步被重新清理。blob 已被 gc 时保留占位符。
        crate::context_edit::materialize_elided(
            &mut file.messages,
            crate::serialize::current_externalize_cas().as_deref(),
        );
        Ok(file)
    }

    /// 列出所有会话，按时间戳倒序排列（最新在前）
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut metas = Vec::new();
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(file) = serde_json::from_str::<SessionFile>(&content) {
                        metas.push(SessionMeta::from(file));
                    }
                }
            }
        }
        metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(metas)
    }

    /// 返回最近一次会话
    pub fn last(&self) -> Result<Option<SessionMeta>> {
        Ok(self.list()?.into_iter().next())
    }

    /// 列出属于 `cwd` 所在项目（git 仓库根，非 git 回退 cwd）的会话，时间倒序。
    /// 会话按项目隔离：不同仓库互不可见，同仓库不同子目录视为同一项目。
    pub fn list_for_project(&self, cwd: &Path) -> Result<Vec<SessionMeta>> {
        let target = crate::project::project_key(cwd);
        Ok(self
            .list()?
            .into_iter()
            .filter(|m| crate::project::project_key(Path::new(&m.cwd)) == target)
            .collect())
    }

    /// 返回当前项目最近一次会话（供 `-c/--continue` 恢复当前项目而非全局最新）。
    pub fn last_for_project(&self, cwd: &Path) -> Result<Option<SessionMeta>> {
        Ok(self.list_for_project(cwd)?.into_iter().next())
    }

    pub fn branch_from_checkpoint(
        &self,
        parent_session_id: &str,
        checkpoint: &crate::checkpoint::Checkpoint,
    ) -> Result<SessionFile> {
        let session_id = crate::history::new_session_id();
        let file = SessionFile {
            session_id,
            title: extract_title(&checkpoint.messages),
            last_preview: extract_preview(&checkpoint.messages),
            cwd: checkpoint.workspace_root().display().to_string(),
            timestamp: crate::history::now_iso(),
            turns: checkpoint
                .messages
                .iter()
                .filter(|message| matches!(message.role, Role::User))
                .count(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            tool_schema_tokens: 0,
            tool_schema_tokens_last: 0,
            tool_schema_tokens_saved: 0,
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            prompt_cache_state: prompt_cache_state::OFF,
            messages: checkpoint.messages.clone(),
            routing_events: vec![],
            current_checkpoint_id: Some(checkpoint.id.clone()),
            branch_parent_session_id: Some(parent_session_id.to_string()),
            branch_parent_checkpoint_id: Some(checkpoint.id.clone()),
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            context_edit_freed_tokens: 0,
        };
        self.save(&file)?;
        Ok(file)
    }
}

/// 从消息列表中提取会话标题（第一条 user 文字消息，截取 60 字符）
pub fn extract_title(messages: &[Message]) -> String {
    for msg in messages {
        if matches!(msg.role, Role::User) {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    let t = text.trim();
                    if !t.is_empty() {
                        let chars: Vec<char> = t.chars().collect();
                        return if chars.len() > 60 {
                            format!("{}…", chars[..59].iter().collect::<String>())
                        } else {
                            t.to_string()
                        };
                    }
                }
            }
        }
    }
    "(空会话)".to_string()
}

/// 从消息列表中提取最后一条 assistant 文字内容，截取 100 字符
pub fn extract_preview(messages: &[Message]) -> String {
    for msg in messages.iter().rev() {
        if matches!(msg.role, Role::Assistant) {
            for block in msg.content.iter().rev() {
                if let ContentBlock::Text { text } = block {
                    let t = text.trim();
                    if !t.is_empty() {
                        let chars: Vec<char> = t.chars().collect();
                        return if chars.len() > 100 {
                            format!("{}…", chars[..99].iter().collect::<String>())
                        } else {
                            t.to_string()
                        };
                    }
                }
            }
        }
    }
    String::new()
}

/// 进程内全局 `PersistCapCfg` —— 由 CLI 装配阶段 `set_session_persist_cap`
/// 注入,`SessionStore::save` 落盘前读。`None` 时不做截断(等价于 cfg 全 0)。
///
/// 用 `OnceLock` 而非 `Mutex<Option>` 是因为 cfg 在启动后只设置一次,
/// `save` 路径读多写少,`get`/clone 比每次 lock 更便宜。
static SESSION_PERSIST_CAP: std::sync::OnceLock<wyj_config::PersistCapCfg> =
    std::sync::OnceLock::new();

fn current_persist_cap() -> Option<wyj_config::PersistCapCfg> {
    SESSION_PERSIST_CAP.get().cloned()
}

/// CLI 装配阶段(主进程 `main` 入口)调用一次,把当前用户的
/// `cfg.persist_cap` 注入到 `SessionStore` 的全局。
pub fn set_session_persist_cap(cfg: wyj_config::PersistCapCfg) {
    let _ = SESSION_PERSIST_CAP.set(cfg);
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 同一会话被多线程并发保存时，磁盘上必须始终是一份**能解析**的完整 JSON。
    ///
    /// 修复前 `save` 是裸 `fs::write` 整文件截断：并发写会互相截断中间态，
    /// `load` 随即 `Err`，那个会话就再也 `--resume` 不回来了。这条测试守的正是
    /// 「不再损坏」——注意它**不**保证不丢更新：两个进程各持载入时的快照，
    /// 语义上的丢更新只能靠 `active_owner` 提前警告用户来规避。
    #[test]
    fn concurrent_saves_never_leave_an_unparseable_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
        let session_id = "sess-concurrent";
        // 消息体撑大，让「写到一半」的窗口足够容易被撞上。
        let filler = "x".repeat(200_000);

        std::thread::scope(|scope| {
            for i in 0..8 {
                let store = &store;
                let session_id = session_id.to_string();
                let filler = filler.clone();
                scope.spawn(move || {
                    for _ in 0..10 {
                        let mut file =
                            mk_file(&session_id, Path::new("/tmp"), "2024-01-01T00:00:00Z");
                        file.title = format!("writer-{i}");
                        file.messages = vec![Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: filler.clone(),
                            }],
                        }];
                        store.save(&file).unwrap();
                        // 每次保存后立刻回读：损坏窗口一旦出现就会被抓住。
                        store.load(&session_id).unwrap();
                    }
                });
            }
        });

        // 落盘文件里不应残留任何 tmp 中间产物。
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
    }

    /// 占用检测只看「另一个活着的进程」，不能被本进程自己或死进程误导。
    #[test]
    fn active_owner_ignores_this_process_and_dead_pids() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
        let session_id = "sess-owner";

        assert_eq!(store.active_owner(session_id), None, "从未保存过");

        store
            .save(&mk_file(
                session_id,
                Path::new("/tmp"),
                "2024-01-01T00:00:00Z",
            ))
            .unwrap();
        assert_eq!(
            store.active_owner(session_id),
            None,
            "本进程不算「别人正在用」"
        );

        // 伪造一个几乎不可能存活的 pid。
        let lock = dir.path().join(format!("{session_id}.json.lock"));
        fs::write(&lock, "4294967294\n").unwrap();
        assert_eq!(store.active_owner(session_id), None, "死进程不算占用");
    }

    fn mk_file(id: &str, cwd: &Path, ts: &str) -> SessionFile {
        SessionFile {
            session_id: id.to_string(),
            title: id.to_string(),
            last_preview: String::new(),
            cwd: cwd.to_string_lossy().to_string(),
            timestamp: ts.to_string(),
            turns: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            tool_schema_tokens: 0,
            tool_schema_tokens_saved: 0,
            tool_schema_tokens_last: 0,
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            prompt_cache_state: prompt_cache_state::OFF,
            messages: vec![],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            context_edit_freed_tokens: 0,
        }
    }

    /// v0.2.28 之前落盘的会话文件没有这 6 个计量字段。必须仍能读出来，
    /// 且缺省值语义正确（计数为 0、缓存状态为 OFF 而不是"未知"）。
    #[test]
    fn legacy_file_without_measurement_fields_still_loads() {
        let legacy = serde_json::json!({
            "session_id": "old",
            "title": "t",
            "last_preview": "p",
            "cwd": "/tmp",
            "timestamp": "2026-07-01T00:00:00Z",
            "turns": 3,
            "input_tokens": 1234,
            "output_tokens": 567,
            "messages": [],
        });
        let f: SessionFile = serde_json::from_value(legacy).unwrap();
        assert_eq!(f.input_tokens, 1234);
        assert_eq!(f.output_tokens, 567);
        assert_eq!(f.cache_read_tokens, 0);
        assert_eq!(f.cache_write_tokens, 0);
        assert_eq!(f.api_calls, 0);
        assert_eq!(f.tool_schema_tokens, 0);
        assert_eq!(f.tool_schema_tokens_saved, 0);
        assert_eq!(f.prompt_cache_state, prompt_cache_state::OFF);
    }

    /// `from_session` ↔ `restore_usage_from` 是计量字段的唯一搬运通道。
    /// 逐字段比对，防止将来新增字段时只改了一端。
    #[test]
    fn from_session_and_restore_usage_from_round_trip_every_counter() {
        let mut s = Session::new();
        s.add_usage(11_000, 2_200);
        s.add_cache_usage(30_000, 4_000);
        s.api_calls = 17;
        s.tool_schema_tokens = 5_000;
        s.tool_schema_tokens_saved = 1_500;
        s.prompt_cache_state = prompt_cache_state::DOWNGRADED;
        s.compact_count = 2;
        s.context_edit_freed_tokens = 9_000;
        s.messages.push(wyj_api::types::Message::user("hi"));

        let file = SessionFile::from_session(
            &s,
            SessionFileMeta {
                session_id: "s1".to_string(),
                title: "t".to_string(),
                last_preview: "p".to_string(),
                cwd: "/tmp".to_string(),
                timestamp: "2026-07-01T00:00:00Z".to_string(),
                turns: 1,
                title_generated: false,
            },
        );
        assert_eq!(file.cache_read_tokens, 30_000);
        assert_eq!(file.cache_write_tokens, 4_000);
        assert_eq!(file.api_calls, 17);
        assert_eq!(file.tool_schema_tokens, 5_000);
        assert_eq!(file.tool_schema_tokens_saved, 1_500);
        assert_eq!(file.prompt_cache_state, prompt_cache_state::DOWNGRADED);
        assert_eq!(file.compact_count, 2);
        assert_eq!(file.context_edit_freed_tokens, 9_000);

        let mut restored = Session::new();
        restored.restore_usage_from(&file);
        assert_eq!(restored.total_input_tokens, 11_000);
        assert_eq!(restored.total_output_tokens, 2_200);
        assert_eq!(restored.total_cache_read_tokens, 30_000);
        assert_eq!(restored.total_cache_write_tokens, 4_000);
        assert_eq!(restored.api_calls, 17);
        assert_eq!(restored.tool_schema_tokens, 5_000);
        assert_eq!(restored.tool_schema_tokens_saved, 1_500);
        assert_eq!(restored.prompt_cache_state, prompt_cache_state::DOWNGRADED);
        assert_eq!(restored.compact_count, 2);
        assert_eq!(restored.context_edit_freed_tokens, 9_000);
    }

    /// 计量字段此前只活在内存里，`/resume` 后 `/cost` 的缓存与调用次数恒为 0。
    /// 这条钉住 save→load 往返不再丢数据。
    #[test]
    fn measurement_fields_survive_save_load_round_trip() {
        let base = std::env::temp_dir().join(format!("wyj-sess-rt-{}", std::process::id()));
        let sessions = base.join("sessions");
        let store = SessionStore::new(sessions).unwrap();
        let mut file = mk_file("rt", &base, "2026-07-06T10:00:00Z");
        file.input_tokens = 8_888;
        file.output_tokens = 1_111;
        file.cache_read_tokens = 7_777;
        file.cache_write_tokens = 222;
        file.api_calls = 42;
        file.tool_schema_tokens = 3_333;
        file.tool_schema_tokens_saved = 444;
        file.prompt_cache_state = prompt_cache_state::ON;
        store.save(&file).unwrap();

        let loaded = store.load("rt").unwrap();
        assert_eq!(loaded.cache_read_tokens, 7_777);
        assert_eq!(loaded.cache_write_tokens, 222);
        assert_eq!(loaded.api_calls, 42);
        assert_eq!(loaded.tool_schema_tokens, 3_333);
        assert_eq!(loaded.tool_schema_tokens_saved, 444);
        assert_eq!(loaded.prompt_cache_state, prompt_cache_state::ON);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `/clear` 换对话不换端点：缓存是运行环境的属性，不能被抹成"从未开启"。
    #[test]
    fn clear_conversation_keeps_prompt_cache_state() {
        let mut s = Session::new();
        s.add_usage(100, 10);
        s.prompt_cache_state = prompt_cache_state::DOWNGRADED;
        s.clear_conversation();
        assert_eq!(s.total_input_tokens, 0);
        assert_eq!(s.prompt_cache_state, prompt_cache_state::DOWNGRADED);
    }

    #[test]
    fn list_for_project_filters_by_git_root() {
        let base = std::env::temp_dir().join(format!("wyj-sess-{}", std::process::id()));
        let sessions = base.join("sessions");
        let repo_a = base.join("repoA");
        let repo_b = base.join("repoB");
        std::fs::create_dir_all(repo_a.join("sub").join(".keep").parent().unwrap()).unwrap();
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();

        let store = SessionStore::new(sessions).unwrap();
        // repoA 会话（在子目录发起）、repoB 会话
        store
            .save(&mk_file("a1", &repo_a.join("sub"), "2026-07-06T10:00:00Z"))
            .unwrap();
        store
            .save(&mk_file("b1", &repo_b, "2026-07-06T11:00:00Z"))
            .unwrap();

        let a = store.list_for_project(&repo_a).unwrap();
        assert_eq!(a.len(), 1, "repoA 只应看到自己的会话");
        assert_eq!(a[0].session_id, "a1");

        let b = store.list_for_project(&repo_b).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].session_id, "b1");

        // 全局 last() 是最新的 b1，但 repoA 的 last_for_project 应是 a1
        assert_eq!(store.last().unwrap().unwrap().session_id, "b1");
        assert_eq!(
            store.last_for_project(&repo_a).unwrap().unwrap().session_id,
            "a1"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn branch_from_checkpoint_keeps_parent_unchanged_and_records_lineage() {
        let base = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("file.txt"), "one").unwrap();
        let store = SessionStore::new(base.path().join("sessions")).unwrap();
        let parent = SessionFile {
            session_id: "parent".to_string(),
            title: "parent".to_string(),
            last_preview: String::new(),
            cwd: workspace.path().display().to_string(),
            timestamp: "2026-08-02T00:00:00Z".to_string(),
            turns: 2,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            tool_schema_tokens: 0,
            tool_schema_tokens_saved: 0,
            tool_schema_tokens_last: 0,
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            prompt_cache_state: prompt_cache_state::OFF,
            messages: vec![Message::user("first"), Message::assistant_text("answer")],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            context_edit_freed_tokens: 0,
        };
        store.save(&parent).unwrap();
        let checkpoints =
            crate::checkpoint::CheckpointStore::configured(store.dir(), "parent").unwrap();
        let summary = checkpoints
            .create(
                workspace.path(),
                &parent.messages[..1],
                crate::checkpoint::CheckpointKind::Manual,
                Some("branch point".to_string()),
            )
            .unwrap();
        let checkpoint = checkpoints.load(&summary.id).unwrap();
        let branch = store.branch_from_checkpoint("parent", &checkpoint).unwrap();

        assert_ne!(branch.session_id, parent.session_id);
        assert_eq!(
            serde_json::to_value(&branch.messages).unwrap(),
            serde_json::to_value(&parent.messages[..1]).unwrap()
        );
        assert_eq!(branch.branch_parent_session_id.as_deref(), Some("parent"));
        assert_eq!(
            branch.branch_parent_checkpoint_id.as_deref(),
            Some(summary.id.as_str())
        );
        assert_eq!(
            serde_json::to_value(&store.load("parent").unwrap().messages).unwrap(),
            serde_json::to_value(&parent.messages).unwrap()
        );
    }

    #[test]
    fn persisted_sessions_redact_secret_like_user_text() {
        let base = tempfile::tempdir().unwrap();
        let store = SessionStore::new(base.path().join("sessions")).unwrap();
        let secret = format!("{}{}", "sk-test-", "D".repeat(24));
        let file = SessionFile {
            session_id: "secret-session".to_string(),
            title: "secret".to_string(),
            last_preview: secret.clone(),
            cwd: base.path().display().to_string(),
            timestamp: "2026-08-02T00:00:00Z".to_string(),
            turns: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            api_calls: 0,
            tool_schema_tokens: 0,
            tool_schema_tokens_saved: 0,
            tool_schema_tokens_last: 0,
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            sub_cache_read_tokens: 0,
            sub_cache_write_tokens: 0,
            prompt_cache_state: prompt_cache_state::OFF,
            messages: vec![Message::user(format!("credential: {secret}"))],
            routing_events: Vec::new(),
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            context_edit_freed_tokens: 0,
        };
        store.save(&file).unwrap();
        let raw = std::fs::read_to_string(store.path("secret-session")).unwrap();
        assert!(!raw.contains(&secret));
        assert!(raw.contains(crate::secret::REDACTED_SECRET));
        assert!(store.load("secret-session").unwrap().messages[0]
            .text()
            .contains(crate::secret::REDACTED_SECRET));
    }
}
