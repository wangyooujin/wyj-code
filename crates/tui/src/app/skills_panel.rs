//! Skill 管理面板：`/skills` 命令触发。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── Skill 管理面板：/skills 命令触发 ───────────────────────────────────────────

pub(super) const BUILTIN_SKILL_NAMES: [&str; 5] = ["run", "review", "fix", "explain", "commit"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsDialogTab {
    Installed,
    Marketplaces,
    Browse,
}

/// Installed tab 的一行：内置 skill / 全局或项目 skill 文件 + lockfile 纳管信息
pub struct SkillInstalledRow {
    pub name: String,
    pub description: String,
    /// None = 内置 skill
    pub scope: Option<wyj_store::InstallScope>,
    pub builtin: bool,
    pub managed: Option<wyj_store::lockfile::InstalledSkillEntry>,
}

pub enum SkillsOverlay {
    None,
    /// 文本内容存在 `SkillsDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddMarketplace,
    /// 注册表搜索进行中（吞掉所有按键，等待 `SkillRegistrySearched`）
    Searching,
    /// 已下载技能包、正在等用户确认是否落盘（`RegistryInstallConfirm` 的前置态）
    FetchingArchive,
    Syncing {
        marketplace_id: String,
        git_url: String,
    },
    InstallConfirm {
        marketplace_id: String,
        git_url: String,
        entry: wyj_store::marketplace::MarketplaceSkillEntry,
        scope: wyj_store::InstallScope,
    },
    /// 注册表（SkillHub）来源的技能安装确认。**必须在解压落盘之前**停下来让
    /// 用户过目来源/作者/版本与包内容摘要——技能正文会整段作为模型指令进入
    /// 对话，而包里可能夹带 `scripts/` 可执行内容，属于不可逆的信任动作。
    RegistryInstallConfirm {
        skill: Box<wyj_store::skillhub::SkillHubSkill>,
        preview: wyj_store::skillhub::SkillArchivePreview,
        /// 已下载的 zip 原始字节，确认后直接解压，无需二次下载
        archive: Vec<u8>,
        scope: wyj_store::InstallScope,
    },
    Upgrading {
        row_idx: usize,
    },
    /// 只读详情浮层（Installed 行菜单的"查看详情"），Enter/Esc 都直接关闭
    Detail {
        title: String,
        lines: Vec<String>,
    },
}

/// Browse tab 当前列表的数据来源。同一张表可以由 git marketplace 同步填充，
/// 也可以由 HTTP 技能注册表搜索填充——两者条目类型不同，安装路径也不同，
/// 因此必须记住"当前这份结果是哪儿来的"，不能靠 `browse_results` 非空来猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsBrowseSource {
    /// 还没有任何来源
    None,
    /// 来自某个 git marketplace（Marketplaces tab 选中"浏览"触发同步）
    Marketplace,
    /// 来自 HTTP 技能注册表（Browse tab 顶部搜索框）
    Registry,
}

/// Installed 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsRowAction {
    ToggleEnabled,
    Upgrade,
    Uninstall,
    ViewDetail,
}

/// Marketplaces 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsSourceAction {
    OpenBrowse,
    Remove,
}

/// Browse 结果行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsBrowseAction {
    Install,
}

/// 三个 tab 各自的行菜单动作，用外层枚举包住
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsMenuAction {
    Installed(SkillsRowAction),
    Marketplaces(SkillsSourceAction),
    Browse(SkillsBrowseAction),
}

/// 扫描一个 skill 目录，返回 (name, description) 列表（description 取首个 `# 标题` 行）
pub(super) fn scan_skill_dir_for_display(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // 目录式技能 `<name>/SKILL.md`（HTTP 注册表安装的形态）与单文件 `<name>.md`
        //（git marketplace 安装的形态）都要收录，否则注册表装来的技能在
        // Installed tab 里完全看不见——装了却看不见是最糟的一种 bug。
        let (skill_file, name) = if path.is_dir() {
            // 目录式技能：命令名是**目录名**，不是 SKILL.md 的文件名
            // （用 file_stem 取会得到 "SKILL"）。
            let candidate = path.join("SKILL.md");
            if !candidate.is_file() {
                continue;
            }
            let Some(name) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            (candidate, name)
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            (path.clone(), name)
        } else {
            continue;
        };
        let description = std::fs::read_to_string(&skill_file)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
            })
            .unwrap_or_else(|| name.clone());
        out.push((name, description));
    }
    out
}

/// Skill 管理面板状态（/skills 命令触发）
pub struct SkillsDialog {
    pub tab: SkillsDialogTab,
    pub installed: Vec<SkillInstalledRow>,
    /// 扁平化行游标，语义随当前 tab 变化（见 `rows()`），Left/Right 切 tab 时归零
    pub cursor: usize,
    pub marketplaces: Vec<wyj_store::lockfile::MarketplaceSource>,
    /// Browse tab 当前展示的条目来自哪个 marketplace（Marketplaces tab 里选中
    /// "浏览"菜单项触发同步后记录，供后续安装该 tab 里条目时回填来源）
    pub active_marketplace_id: String,
    pub active_marketplace_git_url: String,
    pub browse_results: Vec<wyj_store::marketplace::MarketplaceSkillEntry>,
    /// Browse tab 当前的 HTTP 注册表搜索结果。与 `browse_results` 是互斥的两份
    /// 列表：同一时刻只有一份有效，由 `browse_source` 指明是哪一份。
    pub registry_results: Vec<wyj_store::skillhub::SkillHubSkill>,
    /// `browse_results` 的来源，决定选中条目按哪条安装路径处理
    pub browse_source: SkillsBrowseSource,
    /// 正在下载/等待确认的那个注册表技能的 slug。下载期间用户仍可能移动光标，
    /// 确认框回来时要按 slug 回查是哪一条，而不是信任当时的 cursor。
    pub pending_registry_slug: String,
    /// HTTP 技能注册表 API 根。留作后续支持多注册表/自建实例；当前固定用官方
    /// 公共实例，不进 lockfile（与 SkillHub 的部署地址绑死无实际意义）。
    pub registry_base_url: String,
    pub overlay: SkillsOverlay,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<FlatRow, SkillsMenuAction>>,
    pub error: Option<String>,
    pub status: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Skills(_)` 生效期间使用）
    pub live_input: InputBox,
}

impl SkillsDialog {
    pub(super) fn new(home: &std::path::Path, cwd: &std::path::Path) -> Self {
        let global_lock = wyj_store::lockfile::load_global().unwrap_or_default();
        let project_lock = wyj_store::lockfile::load_project(cwd).unwrap_or_default();

        let mut installed = Vec::new();
        for name in BUILTIN_SKILL_NAMES {
            installed.push(SkillInstalledRow {
                name: name.to_string(),
                description: wyj_i18n::tr(&format!("skill.{name}.desc")),
                scope: None,
                builtin: true,
                managed: None,
            });
        }
        let global_dir = wyj_config::global_config_dir_in(home).join("skills");
        for (name, description) in scan_skill_dir_for_display(&global_dir) {
            let managed = global_lock.skills.iter().find(|e| e.name == name).cloned();
            installed.push(SkillInstalledRow {
                name,
                description,
                scope: Some(wyj_store::InstallScope::Global),
                builtin: false,
                managed,
            });
        }
        let project_dir = wyj_config::project_config_dir(cwd).join("skills");
        for (name, description) in scan_skill_dir_for_display(&project_dir) {
            let managed = project_lock.skills.iter().find(|e| e.name == name).cloned();
            installed.push(SkillInstalledRow {
                name,
                description,
                scope: Some(wyj_store::InstallScope::Project),
                builtin: false,
                managed,
            });
        }

        // 首次打开面板（全局 lockfile 里一条 marketplace 源都没有）时自动预置
        // 默认源，逻辑与 McpDialog 的 `ensure_default_registry` 完全对应。
        let marketplaces = wyj_store::marketplace::ensure_default_marketplace()
            .unwrap_or_else(|_| global_lock.marketplaces.clone());

        Self {
            tab: SkillsDialogTab::Installed,
            installed,
            cursor: 0,
            marketplaces,
            active_marketplace_id: String::new(),
            active_marketplace_git_url: String::new(),
            browse_results: Vec::new(),
            registry_results: Vec::new(),
            browse_source: SkillsBrowseSource::None,
            pending_registry_slug: String::new(),
            registry_base_url: wyj_store::skillhub::SKILLHUB_BASE_URL.to_string(),
            overlay: SkillsOverlay::None,
            menu: None,
            error: None,
            status: None,
            live_input: InputBox::new(),
        }
    }

    pub(super) fn refresh_installed(&mut self, home: &std::path::Path, cwd: &std::path::Path) {
        let fresh = Self::new(home, cwd);
        self.installed = fresh.installed;
        self.marketplaces = fresh.marketplaces;
        self.clamp_cursor();
    }

    /// 扁平化行列表：当前 tab 下的条目下标，或该 tab 的固定"+ 添加"行。
    /// tab 头不算进 rows()，单独渲染；Installed 没有 AddNew。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            SkillsDialogTab::Installed => (0..self.installed.len()).map(FlatRow::Entry).collect(),
            SkillsDialogTab::Marketplaces => {
                let mut r: Vec<_> = (0..self.marketplaces.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            SkillsDialogTab::Browse => {
                // 搜索框固定占第 0 行（沿用 MCP 面板 Browse 的 AddNew 约定）。
                // `FlatRow::Entry(i)` 携带的是**结果列表下标**而非行号，因此
                // 搜索结果不需要整体偏移 1。
                let count = match self.browse_source {
                    SkillsBrowseSource::Registry => self.registry_results.len(),
                    _ => self.browse_results.len(),
                };
                let mut r = vec![FlatRow::AddNew];
                r.extend((0..count).map(FlatRow::Entry));
                r
            }
        }
    }

    pub fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn build_menu(&self) -> Option<ActionMenu<FlatRow, SkillsMenuAction>> {
        let row = *self.rows().get(self.cursor)?;
        match (self.tab, row) {
            (SkillsDialogTab::Installed, FlatRow::Entry(idx)) => {
                let r = self.installed.get(idx)?;
                let enabled = r.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                let can_upgrade = r.managed.as_ref().is_some_and(|m| m.is_managed());
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if enabled {
                            "skills.menu.disable"
                        } else {
                            "skills.menu.enable"
                        }),
                        action: SkillsMenuAction::Installed(SkillsRowAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.upgrade"),
                        action: SkillsMenuAction::Installed(SkillsRowAction::Upgrade),
                        dangerous: false,
                        disabled: !can_upgrade,
                        disabled_reason: (!can_upgrade)
                            .then(|| wyj_i18n::tr("skills.error.manual_no_upgrade")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.uninstall"),
                        action: SkillsMenuAction::Installed(SkillsRowAction::Uninstall),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.view_detail"),
                        action: SkillsMenuAction::Installed(SkillsRowAction::ViewDetail),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (SkillsDialogTab::Marketplaces, FlatRow::Entry(_)) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.browse_source"),
                        action: SkillsMenuAction::Marketplaces(SkillsSourceAction::OpenBrowse),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.remove_source"),
                        action: SkillsMenuAction::Marketplaces(SkillsSourceAction::Remove),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (SkillsDialogTab::Browse, FlatRow::Entry(_)) => {
                let items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("skills.menu.install"),
                    action: SkillsMenuAction::Browse(SkillsBrowseAction::Install),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }];
                Some(ActionMenu::new(row, items))
            }
            _ => None,
        }
    }
}

/// `/skills` 面板操作菜单的按键分发，结构与 `mcp_handle_menu_key` 完全对称。
pub(super) fn skills_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(SkillsMenuAction),
        Execute(SkillsMenuAction, FlatRow),
    }

    let step = {
        let Some(dialog) = &mut state.skills_dialog else {
            return;
        };
        let Some(menu) = &mut dialog.menu else {
            return;
        };
        let target = menu.target;
        if let Some(confirming) = menu.confirming {
            match code {
                KeyCode::Enter | KeyCode::Char('y') => Step::Execute(confirming, target),
                KeyCode::Esc | KeyCode::Char('n') => Step::Cancel,
                _ => Step::None,
            }
        } else {
            match code {
                KeyCode::Up => {
                    menu.move_up();
                    Step::None
                }
                KeyCode::Down => {
                    menu.move_down();
                    Step::None
                }
                KeyCode::Esc => Step::Close,
                KeyCode::Enter => match menu.selected_item() {
                    Some(item) if item.disabled => Step::None,
                    Some(item) if item.dangerous => Step::Confirm(item.action),
                    Some(item) => Step::Execute(item.action, target),
                    None => Step::None,
                },
                _ => Step::None,
            }
        }
    };

    match step {
        Step::None => {}
        Step::Close => {
            if let Some(dialog) = &mut state.skills_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.skills_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.skills_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.skills_dialog {
                dialog.menu = None;
            }
            skills_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

pub(super) fn skills_execute_menu_action(
    state: &mut AppState,
    action: SkillsMenuAction,
    target: FlatRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    let FlatRow::Entry(idx) = target else {
        return;
    };
    let home = wyj_config::home_dir().unwrap_or_default();
    match action {
        SkillsMenuAction::Installed(SkillsRowAction::ToggleEnabled) => {
            let info = state.skills_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).map(|row| {
                    let enabled = row.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                    (
                        row.name.clone(),
                        row.scope.unwrap_or(wyj_store::InstallScope::Global),
                        enabled,
                    )
                })
            });
            if let Some((name, scope, currently_enabled)) = info {
                let result = wyj_store::skill_install::set_skill_enabled(
                    &name,
                    scope,
                    &state.cwd,
                    !currently_enabled,
                );
                if let Some(dialog) = &mut state.skills_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&home, &state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        SkillsMenuAction::Installed(SkillsRowAction::Upgrade) => {
            let info = state.skills_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).and_then(|row| {
                    row.managed
                        .as_ref()
                        .is_some_and(|m| m.is_managed())
                        .then(|| {
                            (
                                row.name.clone(),
                                row.scope.unwrap_or(wyj_store::InstallScope::Global),
                            )
                        })
                })
            });
            if let Some((name, scope)) = info {
                if let Some(dialog) = &mut state.skills_dialog {
                    dialog.overlay = SkillsOverlay::Upgrading { row_idx: idx };
                }
                let tx = agent_tx.clone();
                let cwd = state.cwd.clone();
                tokio::spawn(async move {
                    let result = wyj_store::skill_install::upgrade_skill(&name, scope, &cwd)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::SkillUpgraded {
                            row_idx: idx,
                            result,
                        })
                        .await;
                });
            } else if let Some(dialog) = &mut state.skills_dialog {
                dialog.error = Some(wyj_i18n::tr("skills.error.manual_no_upgrade"));
            }
        }
        SkillsMenuAction::Installed(SkillsRowAction::Uninstall) => {
            let info = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| {
                    (
                        row.name.clone(),
                        row.scope.unwrap_or(wyj_store::InstallScope::Global),
                    )
                });
            if let Some((name, scope)) = info {
                let result = wyj_store::skill_install::uninstall_skill(&name, scope, &state.cwd);
                if let Some(dialog) = &mut state.skills_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status = Some(wyj_i18n::tr("skills.uninstall.done"));
                            dialog.refresh_installed(&home, &state.cwd);
                        }
                        Err(e) => {
                            dialog.error = Some(wyj_i18n::tr_fmt(
                                "skills.error.uninstall_failed",
                                &[("err", &e.to_string())],
                            ));
                        }
                    }
                }
            }
        }
        SkillsMenuAction::Installed(SkillsRowAction::ViewDetail) => {
            if let Some(dialog) = &mut state.skills_dialog {
                if let Some(row) = dialog.installed.get(idx) {
                    let mut lines = vec![
                        wyj_i18n::tr_fmt("skills.detail.name_line", &[("name", &row.name)]),
                        wyj_i18n::tr_fmt(
                            "skills.detail.description_line",
                            &[("description", &row.description)],
                        ),
                    ];
                    let scope_text = if row.builtin {
                        wyj_i18n::tr("agents.builtin_tag")
                    } else {
                        row.scope.map(mcp_scope_label_text).unwrap_or_default()
                    };
                    lines.push(wyj_i18n::tr_fmt(
                        "skills.detail.scope_line",
                        &[("scope", &scope_text)],
                    ));
                    if let Some(version) = row.managed.as_ref().and_then(|m| m.version.clone()) {
                        lines.push(wyj_i18n::tr_fmt(
                            "skills.detail.version_line",
                            &[("version", &version)],
                        ));
                    }
                    dialog.overlay = SkillsOverlay::Detail {
                        title: wyj_i18n::tr("skills.detail.title"),
                        lines,
                    };
                }
            }
        }
        SkillsMenuAction::Marketplaces(SkillsSourceAction::OpenBrowse) => {
            let info = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| (m.id.clone(), m.git_url.clone()));
            if let Some((marketplace_id, git_url)) = info {
                if let Some(dialog) = &mut state.skills_dialog {
                    dialog.overlay = SkillsOverlay::Syncing {
                        marketplace_id: marketplace_id.clone(),
                        git_url: git_url.clone(),
                    };
                }
                let tx = agent_tx.clone();
                let git_url_for_task = git_url.clone();
                tokio::spawn(async move {
                    let result = wyj_store::marketplace::sync_marketplace(&git_url_for_task)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::SkillMarketplaceSynced {
                            marketplace_id,
                            git_url: git_url_for_task,
                            result,
                        })
                        .await;
                });
            }
        }
        SkillsMenuAction::Marketplaces(SkillsSourceAction::Remove) => {
            let id = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| m.id.clone());
            if let Some(id) = id {
                let result = wyj_store::marketplace::remove_marketplace(&id);
                if let Some(dialog) = &mut state.skills_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&home, &state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        SkillsMenuAction::Browse(SkillsBrowseAction::Install) => {
            // 同一张 Browse 表可能装的是 git marketplace 条目，也可能是 HTTP
            // 注册表搜索结果——两条安装路径完全不同（前者直接拷贝 .md，后者要
            // 下载 zip、过安全校验、解压成目录），按 browse_source 分流。
            let source = state.skills_dialog.as_ref().map(|d| d.browse_source);
            match source {
                Some(SkillsBrowseSource::Registry) => {
                    let skill = state
                        .skills_dialog
                        .as_ref()
                        .and_then(|d| d.registry_results.get(idx).cloned());
                    if let Some(skill) = skill {
                        start_registry_archive_fetch(state, agent_tx, skill);
                    }
                }
                _ => {
                    let entry = state
                        .skills_dialog
                        .as_ref()
                        .and_then(|d| d.browse_results.get(idx).cloned());
                    if let Some(entry) = entry {
                        if let Some(dialog) = &mut state.skills_dialog {
                            dialog.overlay = SkillsOverlay::InstallConfirm {
                                marketplace_id: dialog.active_marketplace_id.clone(),
                                git_url: dialog.active_marketplace_git_url.clone(),
                                entry,
                                scope: wyj_store::InstallScope::Global,
                            };
                        }
                    }
                }
            }
        }
    }
}

/// 在后台发起一次注册表关键字搜索，结果经 `SkillRegistrySearched` 回到 UI 线程。
///
/// 不在主循环里 `block_on`：搜索是一次网络往返，卡住事件循环会让整个 TUI 冻结。
pub(super) fn start_registry_search(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    query: String,
) {
    let base_url = state
        .skills_dialog
        .as_ref()
        .map(|d| d.registry_base_url.clone())
        .unwrap_or_else(|| wyj_store::skillhub::SKILLHUB_BASE_URL.to_string());
    if let Some(dialog) = &mut state.skills_dialog {
        dialog.overlay = SkillsOverlay::Searching;
        dialog.error = None;
        dialog.status = None;
    }
    let tx = agent_tx.clone();
    tokio::spawn(async move {
        let client = wyj_store::skillhub::SkillHubClient::new(base_url);
        let result = client
            .search_default(&query)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AgentEvent::SkillRegistrySearched { result }).await;
    });
}

/// 下载技能包并扫描内容预览，**全程不落盘**。
///
/// 刻意把「下载」和「安装」拆成两步：确认框要展示来源/作者/版本与包里到底有
/// 什么（尤其 `scripts/`、`hooks/` 下的可执行内容），所以必须先把包拉到手上看
/// 一眼，再由用户决定是否解压进 skills 目录。
pub(super) fn start_registry_archive_fetch(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    skill: wyj_store::skillhub::SkillHubSkill,
) {
    let base_url = state
        .skills_dialog
        .as_ref()
        .map(|d| d.registry_base_url.clone())
        .unwrap_or_else(|| wyj_store::skillhub::SKILLHUB_BASE_URL.to_string());
    if let Some(dialog) = &mut state.skills_dialog {
        dialog.overlay = SkillsOverlay::FetchingArchive;
        dialog.error = None;
        dialog.pending_registry_slug = skill.slug.clone();
    }
    let tx = agent_tx.clone();
    tokio::spawn(async move {
        let client = wyj_store::skillhub::SkillHubClient::new(base_url);
        let result = async {
            let archive = client.download(&skill.slug, &skill.namespace).await?;
            let preview = wyj_store::skillhub::preview_archive(&archive)?;
            anyhow::Ok((preview, archive))
        }
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = tx
            .send(AgentEvent::SkillRegistryArchiveFetched { result })
            .await;
    });
}

#[cfg(test)]
mod skills_registry_tests {
    use super::*;

    fn sample_skill(slug: &str) -> wyj_store::skillhub::SkillHubSkill {
        wyj_store::skillhub::SkillHubSkill {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            description: "测试技能".to_string(),
            version: "1.2.3".to_string(),
            source: "clawhub".to_string(),
            namespace: "someone".to_string(),
            canonical_name: format!("@someone/{slug}"),
            owner_name: "someone".to_string(),
            stars: 42,
            installs: 100,
        }
    }

    /// 目录扫描必须认 `<name>/SKILL.md` 这种目录式技能，且命令名取**目录名**。
    ///
    /// 回归点：HTTP 注册表装来的技能是目录形态，早期实现只扫 `*.md`，
    /// 结果是「装成功了但 Installed tab 里看不见、/help 里也没有」。
    #[test]
    fn directory_shaped_skills_are_scanned_with_their_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(skills.join("demo-skill")).unwrap();
        std::fs::write(
            skills.join("demo-skill").join("SKILL.md"),
            "# 目录式技能\n正文",
        )
        .unwrap();
        // references/ 下的 .md 是技能私有资源，不该被当成独立技能收录。
        std::fs::create_dir_all(skills.join("demo-skill").join("references")).unwrap();
        std::fs::write(skills.join("demo-skill/references/api.md"), "# api").unwrap();
        // 单文件形态仍要照常收录。
        std::fs::write(skills.join("flat.md"), "# 单文件技能\n正文").unwrap();

        let found = scan_skill_dir_for_display(&skills);
        let names: Vec<_> = found.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&"demo-skill".to_string()), "got {names:?}");
        assert!(names.contains(&"flat".to_string()), "got {names:?}");
        assert!(
            !names.contains(&"api".to_string()),
            "references/*.md 不应被当成技能: {names:?}"
        );
        assert!(!names.contains(&"SKILL".to_string()), "got {names:?}");
    }

    /// 搜索无结果时不能因为 `rows()` 首行是搜索框就越界。
    #[test]
    fn browse_tab_always_keeps_a_search_row_at_index_zero() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        let dialog = state.skills_dialog.as_mut().unwrap();
        dialog.tab = SkillsDialogTab::Browse;
        dialog.browse_source = SkillsBrowseSource::None;

        let rows = dialog.rows();
        assert_eq!(rows.len(), 1, "没有任何结果时仍应保留搜索行");
        assert!(matches!(rows[0], FlatRow::AddNew));

        dialog.registry_results = vec![sample_skill("alpha"), sample_skill("beta")];
        dialog.browse_source = SkillsBrowseSource::Registry;
        let rows = dialog.rows();
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], FlatRow::AddNew));
        // Entry 携带的是结果下标，不是行号——否则渲染会取错元素。
        assert!(matches!(rows[1], FlatRow::Entry(0)));
        assert!(matches!(rows[2], FlatRow::Entry(1)));
    }

    /// 搜索结果回来后要同时切 tab、标来源、把光标停在第一条结果上。
    #[test]
    fn registry_search_event_populates_browse_tab() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        state.skills_dialog.as_mut().unwrap().overlay = SkillsOverlay::Searching;

        state.apply_agent_event(AgentEvent::SkillRegistrySearched {
            result: Ok(vec![sample_skill("alpha"), sample_skill("beta")]),
        });

        let dialog = state.skills_dialog.as_ref().unwrap();
        assert!(matches!(dialog.overlay, SkillsOverlay::None));
        assert_eq!(dialog.browse_source, SkillsBrowseSource::Registry);
        assert_eq!(dialog.tab, SkillsDialogTab::Browse);
        assert_eq!(dialog.registry_results.len(), 2);
        // git marketplace 那份列表必须被清空，否则安装会按错的来源走。
        assert!(dialog.browse_results.is_empty());
        assert_eq!(dialog.cursor, 1);
    }

    /// 迟到的旧搜索响应不能覆盖当前界面状态。
    #[test]
    fn stale_registry_search_response_is_ignored() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        // 用户已经关掉搜索浮层、转而干别的事了。
        state.skills_dialog.as_mut().unwrap().overlay = SkillsOverlay::None;

        state.apply_agent_event(AgentEvent::SkillRegistrySearched {
            result: Ok(vec![sample_skill("alpha")]),
        });

        let dialog = state.skills_dialog.as_ref().unwrap();
        assert!(dialog.registry_results.is_empty());
    }

    /// 搜索空结果：留在搜索行，给出提示文案，而不是把 cursor 推到越界位置。
    #[test]
    fn empty_registry_search_keeps_cursor_on_search_row() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        state.skills_dialog.as_mut().unwrap().overlay = SkillsOverlay::Searching;

        state.apply_agent_event(AgentEvent::SkillRegistrySearched {
            result: Ok(Vec::new()),
        });

        let dialog = state.skills_dialog.as_mut().unwrap();
        assert_eq!(dialog.cursor, 0);
        assert!(dialog.status.is_some(), "空结果应给出提示而不是静默");
        dialog.clamp_cursor();
        assert_eq!(dialog.cursor, 0);
    }

    /// 下载完成的包要按 slug 回查成确认框——下载期间用户可能已经移动了光标。
    #[test]
    fn archive_fetch_event_opens_confirm_for_the_pending_slug() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        {
            let dialog = state.skills_dialog.as_mut().unwrap();
            dialog.tab = SkillsDialogTab::Browse;
            dialog.browse_source = SkillsBrowseSource::Registry;
            dialog.registry_results = vec![sample_skill("alpha"), sample_skill("beta")];
            dialog.overlay = SkillsOverlay::FetchingArchive;
            dialog.pending_registry_slug = "beta".to_string();
            // 下载期间光标被挪到了第一条
            dialog.cursor = 1;
        }

        let preview = wyj_store::skillhub::SkillArchivePreview {
            file_count: 3,
            total_bytes: 2048,
            executable_count: 1,
            executable_paths: vec!["scripts/run.sh".to_string()],
        };
        state.apply_agent_event(AgentEvent::SkillRegistryArchiveFetched {
            result: Ok((preview, vec![0u8, 1, 2])),
        });

        let dialog = state.skills_dialog.as_ref().unwrap();
        match &dialog.overlay {
            SkillsOverlay::RegistryInstallConfirm {
                skill,
                preview,
                scope,
                ..
            } => {
                // 必须命中 beta，而不是光标所在的 alpha。
                assert_eq!(skill.slug, "beta");
                assert_eq!(preview.executable_count, 1);
                assert!(preview.has_executables());
                assert_eq!(*scope, wyj_store::InstallScope::Global);
            }
            _ => panic!("expected a RegistryInstallConfirm overlay"),
        }
    }

    /// 注册表来源与 git marketplace 来源必须能在 lockfile 层面区分开，
    /// 否则升级会对一个 https URL 执行 git clone。
    #[test]
    fn registry_install_is_recorded_as_a_registry_source() {
        let dir = tempfile::tempdir().unwrap();
        let req = wyj_store::skillhub::SkillHubInstallRequest {
            base_url: "https://example.invalid/api/v1".to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: "1.0.0".to_string(),
            scope: wyj_store::InstallScope::Project,
            source_label: "someone/demo".to_string(),
        };
        wyj_store::skillhub::install_skillhub_archive(&build_test_archive(), &req, dir.path())
            .unwrap();

        let manifest = wyj_store::lockfile::load_project(dir.path()).unwrap();
        let entry = manifest.skills.iter().find(|s| s.name == "demo").unwrap();
        let source = entry.source.as_ref().unwrap();
        assert!(source.is_registry());
        assert_eq!(source.registry.as_ref().unwrap().slug, "demo");
        assert_eq!(source.marketplace_url, "");

        // 卸载要连目录一起删干净，否则 references/*.md 会被加载器当成
        // 额外的 slash command 重新注册。
        wyj_store::skill_install::uninstall_skill(
            "demo",
            wyj_store::InstallScope::Project,
            dir.path(),
        )
        .unwrap();
        assert!(!dir.path().join(".wyj-code/skills/demo").exists());
    }

    fn build_test_archive() -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            w.start_file("SKILL.md", opts).unwrap();
            w.write_all(b"---\nname: demo\ndescription: d\n---\nbody")
                .unwrap();
            w.start_file("references/api.md", opts).unwrap();
            w.write_all(b"# api").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Auto,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        )
    }

    /// 核心回归钉子：task **活着但长时间无进展**时，看门狗必须把状态收回来。
    ///
    /// 这是 `reap_lost_turn_task` 补不了的洞——它只看 `is_finished()`，而
    /// hang 住的 task 该标志恒为 false。漏掉本条的症状就是用户报的
    /// 「跑着跑着停住，之后输入任何东西都没反应」：`is_thinking` 永为 true
    /// 时用户输入全进 `pending_queue`，连主循环的兜底分支都被同一标志挡住。
    #[tokio::test]
    async fn stalled_turn_is_aborted_even_while_task_is_alive() {
        let mut state = make_state();
        state.is_thinking = true;
        // 造一个「活着但卡住」的 task：spawn 一个永不完成的 future，
        // 用它证明看门狗判的是静默时长，而不是 is_finished()。
        let task = spawn_turn_task(std::future::pending::<()>());
        assert!(
            !task.is_finished(),
            "前提：task 必须仍活着，否则测的是 reap 而非心跳"
        );
        state.current_task = Some(task);
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT + Duration::from_secs(5));

        assert!(state.abort_stalled_turn(), "静默超阈值必须触发兜底");
        assert!(
            !state.is_thinking,
            "兜底后必须解锁 thinking，否则用户后续输入仍会滞留"
        );
        assert!(
            state.current_task.is_none(),
            "task 句柄必须清掉，避免下帧重复 abort"
        );
        assert!(
            state
                .messages
                .iter()
                .any(|m| m.is_error && m.content.contains("900")),
            "必须给用户一条可见提示，不能静默中止"
        );
    }

    /// 兜底之后 pending_queue 必须能被排空——这正是用户「输入无响应」的
    /// 直接成因：is_thinking 卡住时，主循环 `!is_thinking &&
    /// !pending_queue.is_empty()` 这条唯一的排空路径也进不去。
    #[tokio::test]
    async fn stalled_turn_recovery_unblocks_pending_queue() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));
        state.pending_queue.push(OutgoingMessage::plain("继续"));
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT + Duration::from_secs(5));

        state.abort_stalled_turn();

        assert!(!state.is_thinking, "解锁后主循环才可能排空 pending_queue");
        assert!(!state.pending_queue.is_empty(), "队列内容应保留待下轮消费");
    }

    /// 不误杀：静默未超阈值时必须什么都不做。
    #[tokio::test]
    async fn active_turn_is_never_aborted_early() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));

        assert!(!state.abort_stalled_turn());
        assert!(state.is_thinking, "健康回合不得被误杀");
        assert!(state.current_task.is_some());
    }

    /// 关键的反误杀回归：弹窗打开期间（用户去倒杯水）**不得**判死。
    ///
    /// 回合此时合法地 await 在 oneshot 上，这不是卡死。若把这段静默计入
    /// 心跳，一次正常的离开几分钟就会把回合杀掉——那是比原 bug 更糟的
    /// 体验（用户在毫不知情的情况下丢掉一段长工作）。
    #[tokio::test]
    async fn permission_dialog_pauses_the_stall_watchdog() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.permission_dialog = Some(PermissionDialog {
            tool_name: "Bash".to_string(),
            action_summary: "cargo test".to_string(),
            response_tx: tx,
        });
        // 静默远超阈值，但弹窗开着
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT * 3);

        assert!(!state.abort_stalled_turn(), "等待用户交互不算卡死");
        assert!(state.is_thinking);
        assert!(state.permission_dialog.is_some(), "弹窗不得被心跳吃掉");
    }

    /// 空闲态（没在思考）不该刷出「回合被中止」的提示。
    #[test]
    fn idle_state_is_never_reported_as_stalled() {
        let mut state = make_state();
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT * 10);
        assert!(!state.abort_stalled_turn());
        assert!(state.messages.is_empty(), "空闲态不得产生任何提示");
    }

    /// 心跳基准必须由事件驱动重置，否则「持续吐 delta 的长回合」会被误杀。
    #[tokio::test]
    async fn any_agent_event_resets_the_stall_clock() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT * 3);

        state.apply_agent_event(AgentEvent::TextDelta("还在输出".to_string()));

        assert!(
            !state.abort_stalled_turn(),
            "刚收到事件说明回合健康，不得判死"
        );
        assert!(state.is_thinking);
    }

    /// 阈值必须显著大于任何合法静默窗口（前台 Bash 最大 600s、LLM 流
    /// idle 看门狗 120s），否则工具侧超时还没触发、心跳先把回合杀了，
    /// 健康的长时间构建会被误杀。
    #[test]
    fn stall_threshold_exceeds_the_longest_legal_silent_window() {
        assert!(
            TURN_STALL_TIMEOUT > std::time::Duration::from_secs(600),
            "必须大于 Bash 的 600s 上限，否则长构建会被心看门狗抢先杀掉"
        );
        assert!(
            TURN_STALL_TIMEOUT > wyj_api::stream_guard::DEFAULT_STREAM_IDLE_TIMEOUT * 2,
            "应显著大于流级 idle 看门狗，让工具侧超时先触发"
        );
    }
}
