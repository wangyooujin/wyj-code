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
    /// 本会话临时使用的调用分组名（`/model` 面板「仅本会话使用」）。
    ///
    /// `None` = 跟随 `Config.active_profile`。落盘是为了 `--resume` 后继续
    /// 用同一套端点/模型——否则用户为这个项目临时挑的模型，一恢复就悄悄
    /// 变回全局默认值，而聊天记录里全是另一个模型的产出。
    #[serde(default)]
    pub session_profile: Option<String>,
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
    /// 字符启发式估算的校准系数（实测 prompt token / 估算值）。
    ///
    /// 必须落盘：它描述的是「这套端点 + 这类内容下估算偏多少」，是一个需要
    /// 若干轮样本才收敛的量。`--resume` 后从 1.0 重新学，等于每次恢复会话都
    /// 先经历一段阈值失准期——而这恰好发生在历史已经很长的时候。
    #[serde(default)]
    pub input_estimate_ratio: Option<f32>,
    /// 端点亲口拒绝过一次时记下的真实 prompt 规模，用于收紧有效窗口。
    ///
    /// 必须落盘：否则 resume 之后压缩阈值又回到那个「配置写的、够不着」的
    /// 位置，同一个 400 每回合复现一次。
    #[serde(default)]
    pub overflow_window_hint: Option<u32>,
    /// **上一次运行**发出请求时的 system prompt 指纹（见
    /// [`crate::prompt_fingerprint`]）。
    ///
    /// 落盘的唯一目的是「resume 后能回答『这次和上次注入的东西一样吗』」。
    /// 同一个会话隔天再开，模型行为变了而没有任何地方能解释——记忆快照、
    /// 规约链、Skill 名单、工具集、主提示词都会随环境与版本漂移，四者叠加
    /// 足以让同一个 `--resume` 出来的上下文差出几千 token。
    ///
    /// 只存指纹不存全文：全文等于给 system prompt 造第二份事实来源，且本仓库
    /// 的 prompt 30KB+、会话文件每轮整份重写。
    ///
    /// `None` = 本会话还没有落过指纹（新版首次启动 / 升级前的老会话）。
    #[serde(default)]
    pub prompt_fingerprint: Option<crate::prompt_fingerprint::PromptFingerprint>,
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
            input_estimate_ratio: session.input_estimate_ratio,
            overflow_window_hint: session.overflow_window_hint,
            compact_count: session.compact_count,
            context_edit_freed_tokens: session.context_edit_freed_tokens,
            elided_blobs: session.elided_blobs.clone(),
            session_profile: session.session_profile.clone(),
            routing_events: session.routing_events.clone(),
            current_checkpoint_id: session.current_checkpoint_id.clone(),
            branch_parent_session_id: session.branch_parent_session_id.clone(),
            branch_parent_checkpoint_id: session.branch_parent_checkpoint_id.clone(),
            messages: session.messages.clone(),
            // 指纹由 `Agent` 每轮写进 `Session`，这里原样搬运——不在此处重新采集：
            // 采集需要本次请求的真实 tools / system 字节，`Session` 看不到它们。
            prompt_fingerprint: session.prompt_fingerprint.clone(),
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
///
/// `Clone` 只复制一个 `PathBuf`，用于 `save_async` 把 owned 副本交给
/// `spawn_blocking`（那里要求 `'static`）。
#[derive(Clone)]
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
        // **脱敏必须在序列化之前、作用于结构而不是文本**。
        //
        // 此前这里是对 `serde_json::to_string` 之后的**文本**跑 `redact_cow`，
        // 而那条正则的前缀 `\\?["']?` 会把 JSON 转义引号 `\"` 拆开，留下未转义的
        // 裸引号 —— 整个文件随即解析失败（实测 120 个 checkpoint / 133 MB 因此
        // 永久损坏）。
        //
        // 顺序不能反：先脱敏再 `to_string`。序列化器随后会把值里的一切按 JSON
        // 规则转义，结构天然不会被破坏。
        //
        // **必须覆盖整份 structure 而不是只处理 `messages`**：旧实现对整份 JSON
        // 跑正则，顺带覆盖了 `title` / `last_preview` / `routing_events` 等顶层
        // 字段。手工逐字段列举的做法会在新增字段时静默漏掉——走
        // `to_value` → 递归脱敏 → `to_string` 才能保证「凡字符串皆覆盖」，
        // 与旧的文本级覆盖面等价而结构安全。
        //
        // 多一次 `Value` 转换的内存成本：会话文件本来就整份序列化，量级相同。
        let mut as_value = serde_json::to_value(&owned)?;
        crate::secret::redact_json_value(&mut as_value);
        let json = serde_json::to_string(&as_value)?;
        let path = self.path(&file.session_id);
        // 两个进程 resume 同一个 session 时，此前是裸 `fs::write` 整文件截断：
        // 并发写会截出半截 JSON，`load` 直接 Err，该会话再也恢复不了。锁把写入
        // 串行化，tmp+rename 保证任何时刻磁盘上的文件都是完整的一版。
        let _guard =
            wyj_config::filelock::acquire(&self.lock_path(&file.session_id), "session file")?;
        crate::serialize::write_atomic(&path, json.as_bytes()).inspect(|_| self.write_meta(&owned))
    }

    /// [`Self::save`] 的 async 包装：把阻塞 IO（含最长 30 秒的跨进程锁等待）
    /// 挪出 tokio worker 线程。
    ///
    /// 为什么必须这样：`save` 内部是同步 `fs` + `filelock::acquire`，而后者在
    /// 锁被占用时是 `std::thread::sleep` 轮询（`filelock.rs` 的 `RETRY_INTERVAL`），
    /// 最长等 `ACQUIRE_TIMEOUT` = 30 秒。TUI 主循环的 autosave 分支在 async
    /// 上下文里直接调 `save`，双实例争用同一会话时会把整个 worker 线程钉住——
    /// 界面停止重绘、按键不再处理，连 ESC 都救不回来（主循环停在 `terminal.draw`
    /// 之后、`event::poll` 之前）。
    ///
    /// 走 `spawn_blocking` 而不是把 `save` 本身改成 async：`save` 还被
    /// `summary.rs` 等同步路径调用，保持同步签名才能不牵动那些调用点。
    pub async fn save_async(&self, file: SessionFile) -> Result<()> {
        // `SessionFile` 是 owned：`spawn_blocking` 要求 `'static`。
        // 调用方本来就要构造一份（autosave 从 `&Session` 派生），不额外付出克隆。
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.save(&file))
            .await
            .map_err(|error| anyhow::anyhow!("会话保存任务异常退出: {error}"))?
    }

    /// 旁路元数据文件路径：`list()` 只需要 `SessionMeta` 那十来个标量，
    /// 没必要为了它们解析整份 transcript。
    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.meta.json"))
    }

    /// 写元数据旁路文件。
    ///
    /// 与 `.json` 的关系是**派生关系**：真正的权威是 `.json`，meta 只为列表
    /// 服务。写 meta 失败绝不能让 save 失败（否则一次磁盘配额波动就会丢会话），
    /// 所以只 warn。
    fn write_meta(&self, file: &SessionFile) {
        // 先脱敏再序列化，理由同 `save`：对序列化后的文本跑正则会把 JSON 的
        // 转义引号拆开。`SessionMeta` 只有几个标量字段，改成结构化的成本极低。
        let mut meta = SessionMeta::from(file.clone());
        meta.title = crate::secret::redact_sensitive_text(&meta.title);
        meta.last_preview = crate::secret::redact_sensitive_text(&meta.last_preview);
        let json = match serde_json::to_string(&meta) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("session meta 序列化失败 {}: {e}", file.session_id);
                return;
            }
        };
        if let Err(e) =
            crate::serialize::write_atomic(&self.meta_path(&file.session_id), json.as_bytes())
        {
            tracing::warn!("session meta 写入失败 {}: {e}", file.session_id);
        }
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
        let cas = crate::serialize::current_externalize_cas();
        crate::context_edit::materialize_elided(&mut file.messages, cas.as_deref());
        // `save()` 还做过第二类外化：超大 image（base64 > 32KB）与长 thinking
        // （> 16KB）被换成 `cas://<hash>`。这两类必须在这里对称还原，否则
        // resume 后 model 收到的是字面量 `cas://abc...`，而不是图片或思考原文
        // ——`materialize_block_with` 此前**零生产调用点**（只被单测引用），
        // 这条链路一直是断的。
        //
        // 顺序无所谓（两类占位符前缀不同、互不匹配），放在 `materialize_elided`
        // 之后是为了让「工具结果整体占位」优先还原：整块 JSON 里可能还嵌着
        // 图片占位，先展开外层再逐块还原内层更直观。
        if cas.is_some() {
            for message in &mut file.messages {
                for block in &mut message.content {
                    crate::serialize::materialize_block_with(block, cas.as_deref());
                }
            }
        }
        Ok(file)
    }

    /// 列出所有会话，按时间戳倒序排列（最新在前）
    ///
    /// **只读 `.meta.json` 旁路文件**，不为取十个标量字段解析整份 transcript——
    /// 长会话的 `.json` 动辄几 MB，几百个会话就是几百 MB 读盘 + 全量 JSON 解析，
    /// 而 `/resume` 面板只要标题和时间。meta 缺失（老会话、或 meta 写失败）时
    /// 回退到解析 `.json`，保证升级后老会话仍然可见。
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut metas = Vec::new();
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if stem.ends_with(".meta") {
                continue;
            }
            if let Some(meta) = self.load_meta(stem) {
                metas.push(meta);
            }
        }
        metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(metas)
    }

    /// 读一个会话的元数据，优先旁路文件。
    fn load_meta(&self, session_id: &str) -> Option<SessionMeta> {
        if let Ok(content) = std::fs::read_to_string(self.meta_path(session_id)) {
            if let Ok(meta) = serde_json::from_str::<SessionMeta>(&content) {
                return Some(meta);
            }
        }
        let content = std::fs::read_to_string(self.path(session_id)).ok()?;
        let file: SessionFile = serde_json::from_str(&content).ok()?;
        Some(SessionMeta::from(file))
    }

    /// 返回最近一次会话
    pub fn last(&self) -> Result<Option<SessionMeta>> {
        Ok(self.list()?.into_iter().next())
    }

    /// 列出属于 `cwd` 所在项目（git 仓库根，非 git 回退 cwd）的会话，时间倒序。
    ///
    /// 会话按项目隔离：不同仓库互不可见，同仓库不同子目录视为同一项目。
    /// 先按 `cwd` 字符串去重再算 `project_key`——后者会做一次 `canonicalize`
    /// 系统调用，几百个会话逐条算就是几百次 syscall。
    pub fn list_for_project(&self, cwd: &Path) -> Result<Vec<SessionMeta>> {
        let target = crate::project::project_key(cwd);
        let mut seen = std::collections::HashSet::new();
        Ok(self
            .list()?
            .into_iter()
            .filter(|m| {
                seen.insert(m.cwd.clone())
                    && crate::project::project_key(Path::new(&m.cwd)) == target
            })
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
        // 分支继承父会话的临时分组：分支是「在同一次上下文里另开一条线」，
        // 换回全局默认端点会让后续对话与已有的模型产出错配。
        let session_profile = self
            .load(parent_session_id)
            .ok()
            .and_then(|f| f.session_profile);
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
            input_estimate_ratio: None,
            overflow_window_hint: None,
            messages: checkpoint.messages.clone(),
            routing_events: vec![],
            current_checkpoint_id: Some(checkpoint.id.clone()),
            branch_parent_session_id: Some(parent_session_id.to_string()),
            branch_parent_checkpoint_id: Some(checkpoint.id.clone()),
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            session_profile,
            context_edit_freed_tokens: 0,
            prompt_fingerprint: None,
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

    /// resume 必须把 `cas://` 占位符还原成真实内容。
    ///
    /// 回归钉子：`materialize_block_with` 此前**零生产调用点**（只被单测引用），
    /// 于是 `save` 外化出去的图片与长 thinking 在 `--resume` 之后仍是字面量
    /// `cas://<hash>`——模型收到一串 URL 而不是图片。这条测试同时钉住
    /// 「存什么还原什么」，不经过第二次 base64 编码。
    #[test]
    fn load_materializes_externalized_images_and_thinking() {
        let dir = tempfile::tempdir().unwrap();
        let cas_dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
        let cas = std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(cas_dir.path(), 0).unwrap(),
        );

        let image_b64 = "QUJD".repeat(16 * 1024); // 64KB base64
        let long_thinking = "想".repeat(8 * 1024); // 24KB UTF-8
        let mut file = mk_file("sess-cas", Path::new("/tmp"), "2026-01-01T00:00:00Z");
        file.messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: wyj_api::types::ToolResultContent::Parts(vec![
                        wyj_api::types::ToolResultPart::Image {
                            media_type: "image/png".to_string(),
                            data: image_b64.clone(),
                        },
                    ]),
                    is_error: false,
                },
                ContentBlock::Thinking {
                    thinking: long_thinking.clone(),
                    signature: "sig".to_string(),
                    reasoning_details: None,
                },
            ],
        }];

        // 落盘路径读全局 CAS，先注入再还原。
        crate::serialize::set_externalize_cas(Some(cas));
        let result = std::panic::catch_unwind(|| {
            store.save(&file).unwrap();
            let raw = std::fs::read_to_string(store.path("sess-cas")).unwrap();
            assert!(raw.contains("cas://"), "落盘应外化成占位符");
            let loaded = store.load("sess-cas").unwrap();
            match &loaded.messages[0].content[0] {
                ContentBlock::ToolResult {
                    content: wyj_api::types::ToolResultContent::Parts(parts),
                    ..
                } => match &parts[0] {
                    wyj_api::types::ToolResultPart::Image { data, .. } => {
                        assert_eq!(data, &image_b64)
                    }
                    other => panic!("unexpected part: {other:?}"),
                },
                other => panic!("unexpected block: {other:?}"),
            }
            match &loaded.messages[0].content[1] {
                ContentBlock::Thinking { thinking, .. } => assert_eq!(thinking, &long_thinking),
                other => panic!("unexpected block: {other:?}"),
            }
            // 返回原值，避免 catch_unwind 之后全局状态未复位。
        });
        crate::serialize::set_externalize_cas(None);
        result.unwrap();
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
            input_estimate_ratio: None,
            overflow_window_hint: None,
            messages: vec![],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            session_profile: None,
            context_edit_freed_tokens: 0,
            prompt_fingerprint: None,
        }
    }

    /// `list()` 必须读旁路 meta，而不是为取十个标量解析整份 transcript。
    /// 断言方式：把正文写成**非法 JSON**——走 meta 路径照样列得出来，
    /// 走 `.json` 路径必然解析失败。
    #[test]
    fn list_reads_sidecar_meta_without_parsing_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
        store
            .save(&mk_file("s1", dir.path(), "2026-01-01T00:00:00Z"))
            .unwrap();

        // 正文损坏，meta 完好 → 仍必须列出
        std::fs::write(dir.path().join("s1.json"), "{ not json at all").unwrap();

        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].session_id, "s1");
        assert_eq!(metas[0].title, "s1");
    }

    /// meta 缺失（老会话升级前落盘）时回退解析 `.json`，老会话不能消失。
    #[test]
    fn list_falls_back_to_transcript_when_meta_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf()).unwrap();
        store
            .save(&mk_file("s2", dir.path(), "2026-01-02T00:00:00Z"))
            .unwrap();
        std::fs::remove_file(dir.path().join("s2.meta.json")).unwrap();

        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].session_id, "s2");
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
        s.input_estimate_ratio = Some(1.28);
        s.overflow_window_hint = Some(262_144);
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
        assert_eq!(file.input_estimate_ratio, Some(1.28));
        assert_eq!(file.overflow_window_hint, Some(262_144));
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
        // 校准系数与撞线证据必须跟着会话走：resume 后从 1.0 重学，等于在最需要
        // 阈值准确的时候（历史已经很长）先失准一段。
        assert_eq!(restored.input_estimate_ratio, Some(1.28));
        assert_eq!(restored.overflow_window_hint, Some(262_144));
        assert_eq!(restored.compact_count, 2);
        assert_eq!(restored.context_edit_freed_tokens, 9_000);
    }

    /// `/resume` 是在**同一个** `Session` 实例上调用 `restore_usage_from` 切换
    /// 会话的。只搬 `prompt_fingerprint` 而不重置检测状态，会让 `Agent` 里
    /// `if !session.prompt_fingerprint_checked` 直接跳过首轮检测，而 `/context`
    /// 继续显示**上一个会话**的漂移结论——张冠李戴比不显示更糟。
    #[test]
    fn restore_usage_from_resets_prompt_drift_detection_state() {
        let mut s = Session::new();
        s.messages.push(wyj_api::types::Message::user("hi"));
        let file = SessionFile::from_session(
            &s,
            SessionFileMeta {
                session_id: "s2".to_string(),
                title: "t".to_string(),
                last_preview: "p".to_string(),
                cwd: "/tmp".to_string(),
                timestamp: "2026-07-01T00:00:00Z".to_string(),
                turns: 1,
                title_generated: false,
            },
        );

        let mut restored = Session::new();
        // 模拟「本进程已经跑过一轮、且上一个会话留下了漂移结论」。
        restored.prompt_fingerprint_checked = true;
        restored.prompt_drift_since_resume = Some(crate::prompt_fingerprint::PromptDrift {
            previous_recorded_at: "2026-06-01T00:00:00Z".to_string(),
            sections: Vec::new(),
        });

        restored.restore_usage_from(&file);

        assert!(
            !restored.prompt_fingerprint_checked,
            "切换会话后必须重新做一次漂移检测"
        );
        assert!(
            restored.prompt_drift_since_resume.is_none(),
            "上一个会话的漂移结论不能带到新会话"
        );
    }

    /// 会话级分组必须完整走完「内存 Session → SessionFile → 磁盘 → 恢复」，
    /// 否则 `--resume` 会悄悄换回全局默认端点，而聊天记录里全是另一个模型的产出。
    #[test]
    fn session_profile_survives_save_load_round_trip() {
        let base = tempfile::tempdir().unwrap();
        let store = SessionStore::new(base.path().join("sessions")).unwrap();
        let mut session = Session::new();
        session.push_user("hello");
        session.session_profile = Some("minimax".to_string());

        let file = SessionFile::from_session(
            &session,
            SessionFileMeta {
                session_id: "sp".to_string(),
                title: "t".to_string(),
                last_preview: "p".to_string(),
                cwd: base.path().display().to_string(),
                timestamp: "2026-10-18T00:00:00Z".to_string(),
                turns: 1,
                title_generated: false,
            },
        );
        assert_eq!(file.session_profile.as_deref(), Some("minimax"));
        store.save(&file).unwrap();

        let loaded = store.load("sp").unwrap();
        assert_eq!(loaded.session_profile.as_deref(), Some("minimax"));

        let mut restored = Session::new();
        restored.restore_usage_from(&loaded);
        assert_eq!(restored.session_profile.as_deref(), Some("minimax"));
    }

    /// 旧会话文件没有这个字段，必须反序列化成 `None` 而不是报错——
    /// 否则升级后所有历史会话都打不开。
    #[test]
    fn legacy_session_file_without_session_profile_loads_as_none() {
        let mut value =
            serde_json::to_value(mk_file("old", Path::new("/tmp"), "2026-07-01T00:00:00Z"))
                .unwrap();
        value.as_object_mut().unwrap().remove("session_profile");
        let file: SessionFile = serde_json::from_value(value).unwrap();
        assert_eq!(file.session_profile, None);
    }

    /// `/clear` 换的是对话内容，不是端点：会话级分组必须留下来，
    /// 否则用户刚挑好的模型在清空对话后凭空消失。
    #[test]
    fn clear_conversation_keeps_session_profile() {
        let mut s = Session::new();
        s.session_profile = Some("minimax".to_string());
        s.clear_conversation();
        assert_eq!(s.session_profile.as_deref(), Some("minimax"));
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
            input_estimate_ratio: None,
            overflow_window_hint: None,
            messages: vec![Message::user("first"), Message::assistant_text("answer")],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            session_profile: None,
            context_edit_freed_tokens: 0,
            prompt_fingerprint: None,
        };
        store.save(&parent).unwrap();
        let checkpoints =
            crate::checkpoint::CheckpointStore::configured(store.dir(), "parent").unwrap();
        let summary = checkpoints
            .create(
                workspace.path(),
                parent.messages[..1].to_vec(),
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
            input_estimate_ratio: None,
            overflow_window_hint: None,
            messages: vec![Message::user(format!("credential: {secret}"))],
            routing_events: Vec::new(),
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
            compact_count: 0,
            elided_blobs: vec![],
            session_profile: None,
            context_edit_freed_tokens: 0,
            prompt_fingerprint: None,
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
