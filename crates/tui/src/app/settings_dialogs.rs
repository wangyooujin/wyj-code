//! 设置、记忆、演化三块面板的状态与动作。
//!
//! 从 `app.rs` 拆出。三者的共同点：都是**只读展示 + 少量命令式动作**的浮层，
//! 数据来自 `AppState.config` 或 core 的 store，动作要么写回 config、要么调
//! store 的方法。它们不参与回合调度，也不改聊天区。
//!
//! `execute_memory_clear_all` 是唯一有破坏性的动作：它要求用户二次确认
//! （`pending_clear_all`），且只由 TUI 按键处理调用，模型无法触发。

use super::*;

/// 设置面板的可编辑字段索引（对应渲染顺序）——现仅剩 log_level/language，
/// 调用相关字段（provider/model/base_url/api_key 等）已迁到 /model 的 ProfileDialog。
pub const SETTINGS_FIELD_COUNT: usize = 2;

/// 每个字段对应的 i18n label key，下标即字段索引
pub const SETTINGS_FIELD_LABEL_KEYS: [&str; SETTINGS_FIELD_COUNT] =
    ["settings.field.log_level", "settings.field.language"];

/// 设置表单草稿（/config 命令触发时从 AppState.config 初始化，Ctrl+S 时写回）
pub struct SettingsDraft {
    /// 对应 LOG_LEVELS 下标
    pub log_level_idx: usize,
    /// 对应 wyj_i18n::AVAILABLE_LOCALES 下标
    pub language_idx: usize,
}

impl SettingsDraft {
    pub(super) fn from_config(cfg: &Config) -> Self {
        let log_level_idx = LOG_LEVELS
            .iter()
            .position(|l| *l == cfg.log_level)
            .unwrap_or(3); // 默认 warn
        let current_lang = wyj_i18n::resolve_locale(cfg.language.as_deref());
        let language_idx = wyj_i18n::AVAILABLE_LOCALES
            .iter()
            .position(|l| *l == current_lang)
            .unwrap_or(0);
        Self {
            log_level_idx,
            language_idx,
        }
    }

    /// 用草稿覆盖出一份新 Config（基于 base 保留未编辑的字段，如 profiles/mcp_servers）
    pub(super) fn to_config(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        cfg.log_level = LOG_LEVELS[self.log_level_idx].to_string();
        cfg.language = Some(wyj_i18n::AVAILABLE_LOCALES[self.language_idx].to_string());
        cfg
    }

    /// 枚举字段循环切换（log_level=0 / language=1）
    pub(super) fn cycle_enum(&mut self, idx: usize, forward: bool) {
        match idx {
            0 => self.log_level_idx = cycle_index(self.log_level_idx, LOG_LEVELS.len(), forward),
            1 => {
                self.language_idx = cycle_index(
                    self.language_idx,
                    wyj_i18n::AVAILABLE_LOCALES.len(),
                    forward,
                )
            }
            _ => {}
        }
    }

    /// 供渲染用：返回某字段当前值的展示字符串
    pub fn display_value(&self, idx: usize) -> String {
        match idx {
            0 => LOG_LEVELS[self.log_level_idx].to_string(),
            1 => wyj_i18n::locale_display_name(wyj_i18n::AVAILABLE_LOCALES[self.language_idx])
                .to_string(),
            _ => String::new(),
        }
    }
}

/// 设置面板状态（/config 命令触发，现仅管理 log_level/language 两项全局设置）
pub struct SettingsDialog {
    pub draft: SettingsDraft,
    /// 0..SETTINGS_FIELD_COUNT，当前高亮字段
    pub selected: usize,
    /// 设置面板字段目前全部是枚举字段，此字段始终为 None，保留以复用渲染/交互模式
    pub editing: Option<InputBox>,
    /// 校验失败时的提示，保存成功后清空
    pub error: Option<String>,
}

impl SettingsDialog {
    pub(super) fn new(cfg: &Config) -> Self {
        Self {
            draft: SettingsDraft::from_config(cfg),
            selected: 0,
            editing: None,
            error: None,
        }
    }
}

// ── AGENTS.md 记忆面板：/memory 命令触发 ──────────────────────────────────────

/// 记忆面板里的一行：AGENTS.md 系候选文件 / auto-memory 开关 / auto-memory 索引入口
/// / Memory v3 clear-all 一键清空入口（按 y 二次确认执行）。
pub enum MemoryRow {
    File(DiscoveredFile),
    AutoMemoryToggle,
    AutoMemoryIndex {
        path: PathBuf,
        exists: bool,
    },
    ClearAll {
        active_count: usize,
        superseded_count: usize,
    },
}

pub struct MemoryDialog {
    pub rows: Vec<MemoryRow>,
    pub selected: usize,
    pub auto_memory_enabled: bool,
    pub error: Option<String>,
    /// 用户在 ClearAll 行上按 Enter 一次后置 Some，再按 y/Enter 才真正执行
    /// store.clear_all()；按 Esc/n 关闭。这与最终设计 §11 "AI 不能静默触发" 一致：
    /// 用户必须显式二次确认才执行破坏性批操作。
    pub pending_clear_all: bool,
}

impl MemoryDialog {
    pub(super) fn new(
        cwd: &std::path::Path,
        memory_index_path: PathBuf,
        auto_memory_enabled: bool,
        active_count: usize,
        superseded_count: usize,
    ) -> Self {
        let mut rows: Vec<MemoryRow> = discover_files(cwd)
            .into_iter()
            .map(MemoryRow::File)
            .collect();
        rows.push(MemoryRow::AutoMemoryToggle);
        let exists = memory_index_path.exists();
        rows.push(MemoryRow::AutoMemoryIndex {
            path: memory_index_path,
            exists,
        });
        rows.push(MemoryRow::ClearAll {
            active_count,
            superseded_count,
        });
        Self {
            rows,
            selected: 0,
            auto_memory_enabled,
            error: None,
            pending_clear_all: false,
        }
    }
}

/// 真正执行 Memory v3 clear_all：把对话框关闭、把执行结果以系统消息形式
/// 提示用户（成功则带上备份目录，失败则带上错误）。AI 不能触发此函数，
/// 仅 TUI key handler 在用户完成二次确认后调用。
pub(super) fn execute_memory_clear_all(
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    state: &mut AppState,
) {
    state.memory_dialog = None;
    // 先克隆 Arc 让 RwLock guard 尽早释放，避免 borrow 与消息推送路径打架。
    let memory_arc: Option<Arc<wyj_core::MemoryV3Store>> = {
        let agent_guard = shared_agent.read().unwrap();
        agent_guard.memory_v3_ref().cloned()
    };
    let memory: Arc<wyj_core::MemoryV3Store> = match memory_arc {
        Some(m) => m,
        None => {
            state.messages.push(ChatMessage::assistant_err(wyj_i18n::tr(
                "memory.headless_unsupported",
            )));
            return;
        }
    };
    match memory.clear_all() {
        Ok(report) => {
            let dir = report.backup_dir.display().to_string();
            state.messages.push(ChatMessage::assistant(wyj_i18n::tr_fmt(
                "memory.dialog.clear_all_done",
                &[("dir", &dir)],
            )));
        }
        Err(error) => {
            state.messages.push(ChatMessage::assistant_err(format!(
                "Memory v3 clear_all 失败: {error}"
            )));
        }
    }
}

// ── 证据化自进化治理中心：/evolve ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolutionView {
    Active,
    Candidates,
    Episodes,
    Health,
}

impl EvolutionView {
    pub const ALL: [Self; 4] = [Self::Active, Self::Candidates, Self::Episodes, Self::Health];

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Candidates => "Candidates",
            Self::Episodes => "Episodes",
            Self::Health => "Health",
        }
    }
}

#[derive(Debug, Clone)]
pub enum EvolutionRow {
    Memory(EvolutionMemory),
    Candidate(EvolutionCandidate),
    Episode(Episode),
    Health { label: String, value: String },
}

#[derive(Debug, Clone)]
pub enum EvolutionConfirmAction {
    ApproveMemory(String),
    ApproveCandidate(String),
    RejectCandidate(String),
    ForgetMemory(String),
    RollbackCandidate(String),
}

pub struct EvolutionDialog {
    pub store: Arc<EvolutionStore>,
    pub view: EvolutionView,
    pub rows: Vec<EvolutionRow>,
    pub selected: usize,
    pub detail_scroll: u16,
    pub error: Option<String>,
    pub confirm: Option<EvolutionConfirmAction>,
}

impl EvolutionDialog {
    pub(super) fn new(store: Arc<EvolutionStore>) -> Self {
        let mut dialog = Self {
            store,
            view: EvolutionView::Active,
            rows: Vec::new(),
            selected: 0,
            detail_scroll: 0,
            error: None,
            confirm: None,
        };
        dialog.reload();
        dialog
    }

    pub(super) fn reload(&mut self) {
        let loaded: anyhow::Result<Vec<EvolutionRow>> = match self.view {
            EvolutionView::Active => self.store.list_memories().map(|memories| {
                let mut rows = memories
                    .into_iter()
                    .filter(|memory| memory.status == MemoryStatus::Active)
                    .map(EvolutionRow::Memory)
                    .collect::<Vec<_>>();
                rows.extend(
                    self.store
                        .list_candidates()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|candidate| candidate.status == CandidateStatus::Active)
                        .map(EvolutionRow::Candidate),
                );
                rows
            }),
            EvolutionView::Candidates => self.store.list_memories().map(|memories| {
                let mut rows = memories
                    .into_iter()
                    .filter(|memory| {
                        matches!(
                            memory.status,
                            MemoryStatus::Proposed | MemoryStatus::Conflict
                        )
                    })
                    .map(EvolutionRow::Memory)
                    .collect::<Vec<_>>();
                rows.extend(
                    self.store
                        .list_candidates()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|candidate| candidate.status != CandidateStatus::Active)
                        .map(EvolutionRow::Candidate),
                );
                rows
            }),
            EvolutionView::Episodes => self
                .store
                .list_episodes(200)
                .map(|episodes| episodes.into_iter().map(EvolutionRow::Episode).collect()),
            EvolutionView::Health => self.store.status().map(|status| {
                vec![
                    EvolutionRow::Health {
                        label: "project".into(),
                        value: status.project_id,
                    },
                    EvolutionRow::Health {
                        label: "store".into(),
                        value: status.directory.display().to_string(),
                    },
                    EvolutionRow::Health {
                        label: "episodes".into(),
                        value: status.episodes.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "pending candidates".into(),
                        value: status.pending_candidates.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "pending candidates".into(),
                        value: status.pending_candidates.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "store bytes".into(),
                        value: status.store_bytes.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "consecutive failures".into(),
                        value: status.health.consecutive_failures.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "last error".into(),
                        value: status.health.last_error.unwrap_or_else(|| "none".into()),
                    },
                ]
            }),
        };
        match loaded {
            Ok(rows) => {
                self.rows = rows;
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
                self.error = None;
            }
            Err(error) => {
                self.rows.clear();
                self.selected = 0;
                self.error = Some(error.to_string());
            }
        }
        self.detail_scroll = 0;
    }

    pub(super) fn cycle_view(&mut self, forward: bool) {
        let index = EvolutionView::ALL
            .iter()
            .position(|view| *view == self.view)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % EvolutionView::ALL.len()
        } else {
            (index + EvolutionView::ALL.len() - 1) % EvolutionView::ALL.len()
        };
        self.view = EvolutionView::ALL[next];
        self.selected = 0;
        self.confirm = None;
        self.reload();
    }
}

#[cfg(test)]
mod evolution_dialog_tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, Arc<EvolutionStore>) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let store = Arc::new(
            EvolutionStore::new(dir.path(), &repo, wyj_config::EvolutionCfg::default()).unwrap(),
        );
        (dir, store)
    }

    #[test]
    fn dialog_cycles_all_four_views_and_clears_confirmation() {
        let (_dir, store) = test_store();
        let mut session = wyj_core::Session::new();
        session.push_user("cancelled goal");
        let capture = store.begin_episode(
            "dialog-session",
            &session,
            "cancelled goal",
            "default",
            "test-vendor",
            "test-model",
        );
        store.cancel_episode(capture).unwrap();

        let mut dialog = EvolutionDialog::new(store);
        assert_eq!(dialog.view.label(), "Active");
        dialog.confirm = Some(EvolutionConfirmAction::ForgetMemory("memory-1".into()));

        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Candidates");
        assert!(dialog.confirm.is_none());
        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Episodes");
        assert!(matches!(
            dialog.rows.first(),
            Some(EvolutionRow::Episode(_))
        ));
        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Health");
        assert!(dialog
            .rows
            .iter()
            .any(|row| matches!(row, EvolutionRow::Health { label, .. } if label == "project")));
        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Active");
        dialog.cycle_view(false);
        assert_eq!(dialog.view.label(), "Health");
    }
}
