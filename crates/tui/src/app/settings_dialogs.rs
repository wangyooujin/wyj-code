//! 设置、记忆、演化三块面板的状态与动作。
//!
//! 从 `app.rs` 拆出。三者的共同点：都是**只读展示 + 少量命令式动作**的浮层，
//! 数据来自 `AppState.config` 或 core 的 store，动作要么写回 config、要么调
//! store 的方法。它们不参与回合调度，也不改聊天区。
//!
//! `execute_memory_clear_all` 是唯一有破坏性的动作：它要求用户二次确认
//! （`pending_clear_all`），且只由 TUI 按键处理调用，模型无法触发。

use super::*;

/// 设置面板字段的交互形态。
///
/// 与 `ProfileFieldKind` 分开是有意的：`/config` 管的是**全局**设置（日志级别、
/// 界面语言、搜索后端），`/model` 管的是**分组级**调用参数，两者的字段集合与
/// 保存路径（`Config` 顶层 vs `Profile`）都不同，硬合并只会让两边互相牵制。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsFieldKind {
    /// 枚举：Enter / ←→ 循环切换
    Enum(&'static [&'static str]),
    /// 普通文本：Enter 借用底部输入框
    Text,
    /// 敏感文本：同 Text，但渲染打码（api_key）
    Secret,
    /// 数字文本：Enter 借用输入框，保存前按区间校验
    Number { min: u32, max: u32 },
}

pub struct SettingsFieldSpec {
    /// i18n label key，同时作为 `text_value` 的分派键
    pub key: &'static str,
    pub kind: SettingsFieldKind,
}

pub(super) const fn sfield(key: &'static str, kind: SettingsFieldKind) -> SettingsFieldSpec {
    SettingsFieldSpec { key, kind }
}

/// 搜索后端候选。**直接复用** `wyj_config::SearchCfg::KNOWN_PROVIDERS`：
/// 面板只负责展示顺序，合法性与后端列表都归配置层判定，避免新增后端时
/// 两处常量漂移（一份能选、另一份报「不是已知后端」）。
pub(super) const SEARCH_PROVIDER_CHOICES: &[&str] = wyj_config::SearchCfg::KNOWN_PROVIDERS;

/// 设置面板的字段表——**顺序即渲染顺序**，也是唯一真相源。
///
/// 与 `PROFILE_FIELD_SPECS` 同一模式：插入/重排字段只改这张表，读写按 `key`
/// 分派、渲染与键位按 `kind` 分派。历史版本用 `SETTINGS_FIELD_COUNT` + 平行数组
/// + 散落的 `idx == 1` 魔数，加一个字段要改五处且漏一处就是静默错位。
pub const SETTINGS_FIELD_SPECS: &[SettingsFieldSpec] = &[
    sfield(
        "settings.field.log_level",
        SettingsFieldKind::Enum(LOG_LEVELS),
    ),
    sfield("settings.field.language", SettingsFieldKind::Enum(&[])),
    sfield(
        "settings.field.search_provider",
        SettingsFieldKind::Enum(SEARCH_PROVIDER_CHOICES),
    ),
    sfield("settings.field.search_api_key", SettingsFieldKind::Secret),
    sfield(
        "settings.field.search_max_results",
        SettingsFieldKind::Number { min: 1, max: 10 },
    ),
];

pub const SETTINGS_FIELD_COUNT: usize = SETTINGS_FIELD_SPECS.len();

/// 设置表单草稿（/config 命令触发时从 AppState.config 初始化，Ctrl+S 时写回）
pub struct SettingsDraft {
    /// 对应 LOG_LEVELS 下标
    pub log_level_idx: usize,
    /// 对应 wyj_i18n::AVAILABLE_LOCALES 下标
    pub language_idx: usize,
    /// 搜索后端（直接存字符串，取值来自 SEARCH_PROVIDER_CHOICES）
    pub search_provider: String,
    /// 搜索 API Key（文本态）
    pub search_api_key: String,
    /// 每次搜索返回条数（文本态，保存时校验）
    pub search_max_results: String,
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
            search_provider: cfg.effective_search_provider().to_string(),
            // 只展示**配置文件里**的 key，不展示环境变量那份：env 是运行期槽位，
            // 一旦在面板里显示并随 Ctrl+S 落盘，就违背了「env 不进 config.toml」。
            search_api_key: cfg.search.api_key.clone().unwrap_or_default(),
            search_max_results: cfg.search.max_results.to_string(),
        }
    }

    /// 用草稿覆盖出一份新 Config（基于 base 保留未编辑的字段，如 profiles/mcp_servers）
    pub(super) fn to_config(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        cfg.log_level = LOG_LEVELS[self.log_level_idx].to_string();
        cfg.language = Some(wyj_i18n::AVAILABLE_LOCALES[self.language_idx].to_string());
        cfg.search.provider = self.search_provider.clone();
        let key = self.search_api_key.trim();
        cfg.search.api_key = (!key.is_empty()).then(|| key.to_string());
        if let Ok(n) = self.search_max_results.trim().parse::<u32>() {
            cfg.search.max_results = n;
        }
        cfg
    }

    /// 某字段当前值的展示字符串。按 `key` 分派而非下标。
    pub fn display_value(&self, idx: usize) -> String {
        match SETTINGS_FIELD_SPECS.get(idx).map(|s| s.key) {
            Some("settings.field.log_level") => LOG_LEVELS[self.log_level_idx].to_string(),
            Some("settings.field.language") => {
                wyj_i18n::locale_display_name(wyj_i18n::AVAILABLE_LOCALES[self.language_idx])
                    .to_string()
            }
            // 搜索后端与 max_results 是普通文本值；api_key 的打码在渲染层做
            // （与 /model 面板一致：display_value 给原文，渲染决定是否 mask）。
            Some(_) => self.text_value(idx).to_string(),
            None => String::new(),
        }
    }

    /// 文本字段的原始值。枚举字段（log_level/language）不走这里。
    pub fn text_value(&self, idx: usize) -> &str {
        match SETTINGS_FIELD_SPECS.get(idx).map(|s| s.key) {
            Some("settings.field.search_provider") => &self.search_provider,
            Some("settings.field.search_api_key") => &self.search_api_key,
            Some("settings.field.search_max_results") => &self.search_max_results,
            _ => "",
        }
    }

    pub(super) fn set_text_value(&mut self, idx: usize, value: String) {
        match SETTINGS_FIELD_SPECS.get(idx).map(|s| s.key) {
            Some("settings.field.search_provider") => self.search_provider = value,
            Some("settings.field.search_api_key") => self.search_api_key = value,
            Some("settings.field.search_max_results") => self.search_max_results = value,
            _ => {}
        }
    }

    /// 枚举字段的候选值。空切片 = 该字段不是枚举。
    ///
    /// language 的候选在 `SettingsFieldKind::Enum(&[])` 里留空，实际取值从
    /// `wyj_i18n::AVAILABLE_LOCALES` 动态取——locale 列表是运行时决定的面板
    /// 不该把它抄成静态数组。
    pub(super) fn enum_choices(&self, idx: usize) -> Vec<String> {
        let Some(spec) = SETTINGS_FIELD_SPECS.get(idx) else {
            return Vec::new();
        };
        match spec.kind {
            SettingsFieldKind::Enum(_) if spec.key == "settings.field.language" => {
                wyj_i18n::AVAILABLE_LOCALES
                    .iter()
                    .map(|l| (*l).to_string())
                    .collect()
            }
            SettingsFieldKind::Enum(choices) => choices.iter().map(|c| (*c).to_string()).collect(),
            _ => Vec::new(),
        }
    }

    /// 枚举字段循环切换。非枚举字段是空操作（调用点按 kind 分派，这里再兜一层，
    /// 避免键位分支漏判时静默改错字段）。
    pub(super) fn cycle_enum(&mut self, idx: usize, forward: bool) {
        let choices = self.enum_choices(idx);
        if choices.is_empty() {
            return;
        }
        match SETTINGS_FIELD_SPECS.get(idx).map(|s| s.key) {
            Some("settings.field.log_level") => {
                self.log_level_idx = cycle_index(self.log_level_idx, LOG_LEVELS.len(), forward)
            }
            Some("settings.field.language") => {
                self.language_idx = cycle_index(
                    self.language_idx,
                    wyj_i18n::AVAILABLE_LOCALES.len(),
                    forward,
                )
            }
            Some("settings.field.search_provider") => {
                let current = self.search_provider.as_str();
                let pos = choices.iter().position(|c| c == current).unwrap_or(0);
                let next = cycle_index(pos, choices.len(), forward);
                self.search_provider = choices[next].clone();
            }
            _ => {}
        }
    }

    /// 校验全部字段。返回 i18n key 与错误参数。
    pub(super) fn validate(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        match self.search_max_results.trim().parse::<u32>() {
            Ok(n) if (1..=10).contains(&n) => {}
            _ => {
                return Some((
                    "settings.error.invalid_search_max_results",
                    vec![("value", self.search_max_results.trim().to_string())],
                ))
            }
        }
        if self.search_provider.trim().is_empty() {
            return Some(("settings.error.empty_search_provider", Vec::new()));
        }
        None
    }
}

/// 设置面板状态（/config 命令触发）
pub struct SettingsDialog {
    pub draft: SettingsDraft,
    /// 0..SETTINGS_FIELD_COUNT，当前高亮字段
    pub selected: usize,
    /// 底部主输入框借用态下 Text/Secret 字段的草稿（`InputOwner::Settings` 生效期间）
    pub live_input: InputBox,
    /// 校验失败时的提示，保存成功后清空
    pub error: Option<String>,
}

impl SettingsDialog {
    pub(super) fn new(cfg: &Config) -> Self {
        Self {
            draft: SettingsDraft::from_config(cfg),
            selected: 0,
            live_input: InputBox::new(),
            error: None,
        }
    }
}

// ── AGENTS.md 记忆面板：/memory 命令触发 ──────────────────────────────────────

#[cfg(test)]
mod settings_dialog_tests {
    use super::*;

    fn draft_with(provider: &str, key: &str, max_results: &str) -> SettingsDraft {
        SettingsDraft {
            log_level_idx: 3,
            language_idx: 0,
            search_provider: provider.to_string(),
            search_api_key: key.to_string(),
            search_max_results: max_results.to_string(),
        }
    }

    /// 字段表里每个 key 都要在 i18n 里有真 label，否则面板渲染出字面量 key。
    #[test]
    fn every_settings_field_label_key_resolves() {
        for spec in SETTINGS_FIELD_SPECS {
            let label = wyj_i18n::tr(spec.key);
            assert_ne!(
                label, spec.key,
                "字段 {} 的 i18n label 缺失（渲染成字面量）",
                spec.key
            );
            assert!(!label.trim().is_empty(), "字段 {} 的 label 为空", spec.key);
        }
    }

    /// 每个文本类字段都要在 `text_value`/`set_text_value` 里有分支，否则该字段
    /// 在面板里永远是空串、且编辑结果被静默丢弃。
    #[test]
    fn every_text_field_round_trips_through_accessors() {
        for (idx, spec) in SETTINGS_FIELD_SPECS.iter().enumerate() {
            if !matches!(
                spec.kind,
                SettingsFieldKind::Text
                    | SettingsFieldKind::Secret
                    | SettingsFieldKind::Number { .. }
            ) {
                continue;
            }
            let mut draft = draft_with("tavily", "", "5");
            draft.set_text_value(idx, "probe-value".to_string());
            assert_eq!(
                draft.text_value(idx),
                "probe-value",
                "字段 {} 的读写分支不一致",
                spec.key
            );
        }
    }

    /// 每个枚举字段都要有非空候选，且循环切换确实会改变值——否则按键是空操作。
    #[test]
    fn every_enum_field_has_choices_and_cycles() {
        for (idx, spec) in SETTINGS_FIELD_SPECS.iter().enumerate() {
            if !matches!(spec.kind, SettingsFieldKind::Enum(_)) {
                continue;
            }
            let mut draft = draft_with("tavily", "", "5");
            assert!(
                !draft.enum_choices(idx).is_empty(),
                "枚举字段 {} 没有候选值",
                spec.key
            );
            let before = draft.display_value(idx);
            draft.cycle_enum(idx, true);
            // language 的候选是 locale 列表，可能与 before 相同（只有一个 locale）；
            // 其余枚举至少两个候选，必须变化。
            if spec.key != "settings.field.language" {
                assert_ne!(
                    before,
                    draft.display_value(idx),
                    "枚举字段 {} 循环后值未变化",
                    spec.key
                );
            }
        }
    }

    /// 搜索字段必须真的落到 `Config.search` 上——否则面板改了半天写不进配置。
    #[test]
    fn search_fields_are_written_back_into_config() {
        let base = Config::default();
        let draft = draft_with("brave", "tvly-abc", "8");
        let cfg = draft.to_config(&base);
        assert_eq!(cfg.search.provider, "brave");
        assert_eq!(cfg.search.api_key.as_deref(), Some("tvly-abc"));
        assert_eq!(cfg.search.max_results, 8);
        // 工具注册门控现在应该放行（后端合法 + key 存在）。
        assert!(cfg.search_provider_issue().is_none());
    }

    /// 清空 key 要在配置里落成 `None`（而不是空串）——空串会让
    /// `search_api_key()` 之外的读取路径看到「有值但为空」。
    #[test]
    fn blank_api_key_clears_the_config_value() {
        let base = Config::default();
        let cfg = draft_with("tavily", "   ", "5").to_config(&base);
        assert!(cfg.search.api_key.is_none());
        assert!(cfg.search_api_key().is_none());
    }

    /// 面板只展示/回写**配置文件**里的 key，不展示环境变量那份——否则一次
    /// Ctrl+S 就把 env 里的 key 物化进了 config.toml。
    #[test]
    fn dialog_ignores_the_runtime_env_key_when_initialising() {
        let cfg = Config {
            runtime_search_api_key: Some("from-env".to_string()),
            ..Config::default()
        };
        let draft = SettingsDraft::from_config(&cfg);
        assert_eq!(draft.search_api_key, "", "env key 不该出现在面板草稿里");
    }

    #[test]
    fn validate_rejects_out_of_range_max_results() {
        assert!(draft_with("tavily", "k", "5").validate().is_none());
        assert!(draft_with("tavily", "k", "0").validate().is_some());
        assert!(draft_with("tavily", "k", "11").validate().is_some());
        assert!(draft_with("tavily", "k", "abc").validate().is_some());
    }

    #[test]
    fn validate_rejects_empty_provider() {
        let (key, _) = draft_with("  ", "k", "5").validate().unwrap();
        assert_eq!(key, "settings.error.empty_search_provider");
    }

    /// 校验失败的提示必须能解析出真文案（不是字面量 key），否则用户看到的是
    /// `settings.error.invalid_search_max_results`。
    #[test]
    fn validation_error_keys_resolve_to_real_copy() {
        let (key, args) = draft_with("tavily", "k", "99").validate().unwrap();
        let refs: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let msg = wyj_i18n::tr_fmt(key, &refs);
        assert_ne!(msg, key);
        // 参数确实被插进去了，而不只是模板原文。
        assert!(msg.contains("99"), "{msg}");
    }
}

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
