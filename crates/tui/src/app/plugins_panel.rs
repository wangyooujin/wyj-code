//! 插件管理面板：`/plugins` 命令触发。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── 插件管理面板：/plugins 命令触发 ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginsDialogTab {
    Installed,
    Marketplaces,
    Browse,
}

/// Installed tab 的一行：整体启用/禁用，不拆分内部 commands/agents/mcpServers。
pub struct PluginInstalledRow {
    pub name: String,
    pub version: Option<String>,
    pub scope: wyj_store::InstallScope,
    pub enabled: bool,
    pub is_local_dev: bool,
    /// 如 "cmd:3 agent:1 mcp:2"，供展示用（不用 emoji，对齐全部面板的纯文字风格）
    pub resource_summary: String,
    pub entry: wyj_store::lockfile::InstalledPluginEntry,
}

pub enum PluginOverlay {
    None,
    /// 文本内容存在 `PluginsDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddMarketplace,
    Syncing {
        marketplace_id: String,
    },
    InstallConfirm {
        marketplace_id: String,
        location: String,
        entry: Box<wyj_store::plugin_manifest::PluginMarketplaceEntry>,
        scope: wyj_store::InstallScope,
    },
    Installing,
    InstallReport {
        report: wyj_store::plugin_install::PluginInstallReport,
    },
    Upgrading {
        row_idx: usize,
    },
    /// 文本内容存在 `PluginsDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddLocalPlugin,
    /// 只读详情浮层（Installed 行菜单的"查看详情"），Enter/Esc 都直接关闭
    Detail {
        title: String,
        lines: Vec<String>,
    },
}

/// Installed 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsRowAction {
    ToggleEnabled,
    Upgrade,
    Uninstall,
    ViewDetail,
}

/// Marketplaces 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsSourceAction {
    OpenBrowse,
    Remove,
}

/// Browse 结果行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsBrowseAction {
    Install,
}

/// 三个 tab 各自的行菜单动作，用外层枚举包住
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsMenuAction {
    Installed(PluginsRowAction),
    Marketplaces(PluginsSourceAction),
    Browse(PluginsBrowseAction),
}

/// 插件管理面板状态（/plugins 命令触发）
pub struct PluginsDialog {
    pub tab: PluginsDialogTab,
    pub installed: Vec<PluginInstalledRow>,
    /// 扁平化行游标，语义随当前 tab 变化（见 `rows()`），Left/Right 切 tab 时归零
    pub cursor: usize,
    pub marketplaces: Vec<wyj_store::lockfile::PluginMarketplaceSource>,
    /// Browse tab 当前展示的条目来自哪个 marketplace（Marketplaces tab 里选中
    /// "浏览"菜单项触发同步后记录，供后续安装该 tab 里条目时回填来源）
    pub active_marketplace_id: String,
    pub active_marketplace_location: String,
    pub browse_results: Vec<wyj_store::plugin_manifest::PluginMarketplaceEntry>,
    pub overlay: PluginOverlay,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<FlatRow, PluginsMenuAction>>,
    pub error: Option<String>,
    pub status: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Plugins(_)` 生效期间使用）
    pub live_input: InputBox,
}

impl PluginsDialog {
    pub(super) fn new(cwd: &std::path::Path) -> Self {
        let global_lock = wyj_store::lockfile::load_global().unwrap_or_default();
        let project_lock = wyj_store::lockfile::load_project(cwd).unwrap_or_default();

        let mut installed = Vec::new();
        for entry in global_lock
            .plugins
            .iter()
            .chain(project_lock.plugins.iter())
        {
            installed.push(PluginInstalledRow {
                name: entry.name.clone(),
                version: entry.version.clone(),
                scope: entry.scope,
                enabled: entry.enabled,
                is_local_dev: entry.is_local_dev(),
                resource_summary: format!(
                    "cmd:{} agent:{} mcp:{}",
                    entry.contributes.skill_paths.len(),
                    entry.contributes.agent_paths.len(),
                    entry.contributes.mcp_servers.len(),
                ),
                entry: entry.clone(),
            });
        }

        // 与 skill/mcp 不同：不预置默认插件市场源（没有已知稳定维护、遵循官方
        // marketplace.json 格式的公开仓库可以放心硬编码），面板首次打开时可能
        // 一个市场源都没有，需要用户自行通过"+ 添加"新增。
        let marketplaces =
            wyj_store::plugin_install::list_plugin_marketplaces().unwrap_or_default();

        Self {
            tab: PluginsDialogTab::Installed,
            installed,
            cursor: 0,
            marketplaces,
            active_marketplace_id: String::new(),
            active_marketplace_location: String::new(),
            browse_results: Vec::new(),
            overlay: PluginOverlay::None,
            menu: None,
            error: None,
            status: None,
            live_input: InputBox::new(),
        }
    }

    pub(super) fn refresh_installed(&mut self, cwd: &std::path::Path) {
        let fresh = Self::new(cwd);
        self.installed = fresh.installed;
        self.marketplaces = fresh.marketplaces;
        self.clamp_cursor();
    }

    /// 扁平化行列表：当前 tab 下的条目下标，或该 tab 的固定"+ 添加"行。
    /// tab 头不算进 rows()，单独渲染；Installed 的 AddNew 行语义是"添加本地
    /// 插件"（不同于 Mcp/Skills 的"添加 source"，但落到同一个 FlatRow 形状上）；
    /// Browse 没有 AddNew。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            PluginsDialogTab::Installed => {
                let mut r: Vec<_> = (0..self.installed.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            PluginsDialogTab::Marketplaces => {
                let mut r: Vec<_> = (0..self.marketplaces.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            PluginsDialogTab::Browse => {
                (0..self.browse_results.len()).map(FlatRow::Entry).collect()
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

    pub fn build_menu(&self) -> Option<ActionMenu<FlatRow, PluginsMenuAction>> {
        let row = *self.rows().get(self.cursor)?;
        match (self.tab, row) {
            (PluginsDialogTab::Installed, FlatRow::Entry(idx)) => {
                let r = self.installed.get(idx)?;
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if r.enabled {
                            "plugins.menu.disable"
                        } else {
                            "plugins.menu.enable"
                        }),
                        action: PluginsMenuAction::Installed(PluginsRowAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.upgrade"),
                        action: PluginsMenuAction::Installed(PluginsRowAction::Upgrade),
                        dangerous: false,
                        disabled: r.is_local_dev,
                        disabled_reason: r
                            .is_local_dev
                            .then(|| wyj_i18n::tr("plugins.error.local_no_upgrade")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.uninstall"),
                        action: PluginsMenuAction::Installed(PluginsRowAction::Uninstall),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.view_detail"),
                        action: PluginsMenuAction::Installed(PluginsRowAction::ViewDetail),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (PluginsDialogTab::Marketplaces, FlatRow::Entry(_)) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.browse_source"),
                        action: PluginsMenuAction::Marketplaces(PluginsSourceAction::OpenBrowse),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.remove_source"),
                        action: PluginsMenuAction::Marketplaces(PluginsSourceAction::Remove),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (PluginsDialogTab::Browse, FlatRow::Entry(_)) => {
                let items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("plugins.menu.install"),
                    action: PluginsMenuAction::Browse(PluginsBrowseAction::Install),
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

/// `/plugins` 面板操作菜单的按键分发，结构与 `mcp_handle_menu_key` 完全对称。
pub(super) fn plugins_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(PluginsMenuAction),
        Execute(PluginsMenuAction, FlatRow),
    }

    let step = {
        let Some(dialog) = &mut state.plugins_dialog else {
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
            if let Some(dialog) = &mut state.plugins_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.plugins_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.plugins_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.plugins_dialog {
                dialog.menu = None;
            }
            plugins_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

pub(super) fn plugins_execute_menu_action(
    state: &mut AppState,
    action: PluginsMenuAction,
    target: FlatRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    let FlatRow::Entry(idx) = target else {
        return;
    };
    match action {
        PluginsMenuAction::Installed(PluginsRowAction::ToggleEnabled) => {
            let info = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| (row.name.clone(), row.scope, row.enabled));
            if let Some((name, scope, enabled)) = info {
                let result = wyj_store::plugin_install::set_plugin_enabled(
                    &name, scope, &state.cwd, !enabled,
                );
                if let Some(dialog) = &mut state.plugins_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status =
                                Some(wyj_i18n::tr("plugins.toggle.restart_required_hint"));
                            dialog.refresh_installed(&state.cwd);
                        }
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        PluginsMenuAction::Installed(PluginsRowAction::Upgrade) => {
            let info = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .filter(|row| !row.is_local_dev)
                .map(|row| (row.name.clone(), row.scope));
            if let Some((name, scope)) = info {
                if let Some(dialog) = &mut state.plugins_dialog {
                    dialog.overlay = PluginOverlay::Upgrading { row_idx: idx };
                }
                let tx = agent_tx.clone();
                let cwd = state.cwd.clone();
                tokio::spawn(async move {
                    let result = wyj_store::plugin_install::upgrade_plugin(&name, scope, &cwd)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::PluginUpgraded {
                            row_idx: idx,
                            result,
                        })
                        .await;
                });
            } else if let Some(dialog) = &mut state.plugins_dialog {
                dialog.error = Some(wyj_i18n::tr("plugins.error.local_no_upgrade"));
            }
        }
        PluginsMenuAction::Installed(PluginsRowAction::Uninstall) => {
            let info = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| (row.name.clone(), row.scope));
            if let Some((name, scope)) = info {
                let result = wyj_store::plugin_install::uninstall_plugin(&name, scope, &state.cwd);
                if let Some(dialog) = &mut state.plugins_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status = Some(wyj_i18n::tr("plugins.uninstall.done"));
                            dialog.refresh_installed(&state.cwd);
                        }
                        Err(e) => {
                            dialog.error = Some(wyj_i18n::tr_fmt(
                                "plugins.error.uninstall_failed",
                                &[("err", &e.to_string())],
                            ));
                        }
                    }
                }
            }
        }
        PluginsMenuAction::Installed(PluginsRowAction::ViewDetail) => {
            if let Some(dialog) = &mut state.plugins_dialog {
                if let Some(row) = dialog.installed.get(idx) {
                    let mut lines = vec![
                        wyj_i18n::tr_fmt("plugins.detail.name_line", &[("name", &row.name)]),
                        wyj_i18n::tr_fmt(
                            "plugins.detail.scope_line",
                            &[("scope", &mcp_scope_label_text(row.scope))],
                        ),
                    ];
                    if let Some(version) = &row.version {
                        lines.push(wyj_i18n::tr_fmt(
                            "plugins.detail.version_line",
                            &[("version", version)],
                        ));
                    }
                    lines.push(wyj_i18n::tr_fmt(
                        "plugins.detail.resources_line",
                        &[("summary", &row.resource_summary)],
                    ));
                    lines.push(wyj_i18n::tr_fmt(
                        "plugins.detail.path_line",
                        &[("path", &row.entry.plugin_root.display().to_string())],
                    ));
                    dialog.overlay = PluginOverlay::Detail {
                        title: wyj_i18n::tr("plugins.detail.title"),
                        lines,
                    };
                }
            }
        }
        PluginsMenuAction::Marketplaces(PluginsSourceAction::OpenBrowse) => {
            let marketplace_id = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| m.id.clone());
            if let Some(marketplace_id) = marketplace_id {
                if let Some(dialog) = &mut state.plugins_dialog {
                    dialog.overlay = PluginOverlay::Syncing {
                        marketplace_id: marketplace_id.clone(),
                    };
                }
                let tx = agent_tx.clone();
                let id_for_task = marketplace_id.clone();
                tokio::spawn(async move {
                    let result = wyj_store::plugin_install::sync_plugin_marketplace(&id_for_task)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::PluginMarketplaceSynced {
                            marketplace_id,
                            result,
                        })
                        .await;
                });
            }
        }
        PluginsMenuAction::Marketplaces(PluginsSourceAction::Remove) => {
            let id = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| m.id.clone());
            if let Some(id) = id {
                let result = wyj_store::plugin_install::remove_plugin_marketplace(&id);
                if let Some(dialog) = &mut state.plugins_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        PluginsMenuAction::Browse(PluginsBrowseAction::Install) => {
            let entry = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.browse_results.get(idx).cloned());
            if let Some(entry) = entry {
                if let Some(dialog) = &mut state.plugins_dialog {
                    dialog.overlay = PluginOverlay::InstallConfirm {
                        marketplace_id: dialog.active_marketplace_id.clone(),
                        location: dialog.active_marketplace_location.clone(),
                        entry: Box::new(entry),
                        scope: wyj_store::InstallScope::Global,
                    };
                }
            }
        }
    }
}

/// @ 文件选取器候选项
#[derive(Clone)]
pub struct FileEntry {
    pub display: String,
    pub rel_path: String,
    pub is_dir: bool,
}

/// 待发送附件（图片或文件）
#[derive(Debug, Clone)]
pub enum Attachment {
    Image {
        media_type: String,
        data: String,
        preview_label: String,
    },
    File {
        path: std::path::PathBuf,
    },
}

/// 一段被折叠起来的长文本粘贴。
///
/// 输入框里只留一个 `PASTE_MARKER` 原子占位（渲染成 `[Pasted text #n +N lines]`），
/// 原文存在这里，直到发送时才由 `InputBox::text_with_pastes` 按标记位置填回正文。
///
/// **为什么不留在输入框里**：输入框高度被 `render::draw` 硬 clamp 到 10 行，
/// 且 `draw_input` 的 `Paragraph` 没有滚动、只从第 0 行开始画。几百行内容一进去
/// 就等于消失——用户看到的是「粘贴满了但打不了字」，真实原因既不是长度上限也不是
/// 截断，而是渲染层从第一行起画且永不跟随光标。折叠把问题从「看不见」降级成
/// 「占一行」，与 Claude Code 的 `[Pasted text #1 +250 lines]` 一致。
#[derive(Debug, Clone)]
pub struct PasteBlock {
    /// 原文。发送时逐字回填，一个字符都不能少。
    pub text: String,
    /// 行数，只用于占位符文案 `[Pasted text #n +N lines]`。
    pub line_count: usize,
}

/// 一条待发送的用户消息。
///
/// `display` 必须在**输入框还存在的那一刻**算好：附件占位符的位置只存在于
/// 输入框内部的原子标记里，`InputBox::text()` 会把它们过滤掉，一旦离开
/// 输入框就再也拼不出与输入框一致的展示文本。因此凡是消息要排队/等确认
/// 的路径（`pending_queue`、`ExecModeConfirmDialog`）都必须把它一起带着走，
/// 不能等到消费时再从 `text + attachments` 现拼。
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// 发给模型的正文（不含任何展示用占位符）
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// 聊天区展示文本，附件占位符已按输入框里的位置展开
    pub display: String,
}

impl OutgoingMessage {
    /// 无附件的消息（测试与内部注入路径用），展示文本即正文。
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            display: text.clone(),
            text,
            attachments: Vec::new(),
        }
    }
}
