//! Session 持久化存储：~/.wyj-code/sessions/<session-id>.json

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use wyj_api::types::{ContentBlock, Message, Role};

use crate::session::RoutingEvent;

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
        // 这里走 `clone` 而非原地 mutate,是因为 `&SessionFile` 借用不允
        // 许 truncate 调用原地改 messages。`SessionFile::clone` 主要成本在
        // `Vec<Message>`,而 truncate 正是要削减其体积;序列化前 clone 比
        // 序列化完整 messages 仍便宜得多。
        let mut owned = file.clone();
        if let Some(cfg) = current_persist_cap() {
            crate::serialize::truncate_session_for_persistence(&mut owned, &cfg);
        }
        let json = serde_json::to_string(&owned)?;
        std::fs::write(
            self.path(&file.session_id),
            crate::secret::redact_sensitive_text(&json),
        )?;
        Ok(())
    }

    pub fn load(&self, session_id: &str) -> Result<SessionFile> {
        let content = std::fs::read_to_string(self.path(session_id))?;
        Ok(serde_json::from_str(&content)?)
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
            messages: checkpoint.messages.clone(),
            routing_events: vec![],
            current_checkpoint_id: Some(checkpoint.id.clone()),
            branch_parent_session_id: Some(parent_session_id.to_string()),
            branch_parent_checkpoint_id: Some(checkpoint.id.clone()),
            title_generated: false,
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
            messages: vec![],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
        }
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
            messages: vec![Message::user("first"), Message::assistant_text("answer")],
            routing_events: vec![],
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
        };
        store.save(&parent).unwrap();
        let checkpoints = crate::checkpoint::CheckpointStore::new(store.dir(), "parent").unwrap();
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
            messages: vec![Message::user(format!("credential: {secret}"))],
            routing_events: Vec::new(),
            current_checkpoint_id: None,
            branch_parent_session_id: None,
            branch_parent_checkpoint_id: None,
            title_generated: false,
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
