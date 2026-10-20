//! MCP server 管理面板：`/mcp` 命令触发。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── MCP server 管理面板：/mcp 命令触发 ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDialogTab {
    Installed,
    Registries,
    Browse,
}

/// 后台启动连接一个配置的 MCP server 的实时状态（与 `/mcp` 面板是否打开无关）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpConnStatus {
    Connecting,
    Connected { tool_count: usize },
    Failed,
    TimedOut,
}

/// Installed tab 的一行：config 里的 server 条目 + 所在 scope + lockfile 纳管信息（None=未纳管/手动配置）
pub struct McpInstalledRow {
    pub config: wyj_config::McpServerConfig,
    pub scope: wyj_store::InstallScope,
    pub managed: Option<wyj_store::lockfile::InstalledMcpEntry>,
}

pub enum McpOverlay {
    None,
    Searching,
    InstallConfirm {
        server: Box<wyj_store::registry::RegistryServerSummary>,
        package: wyj_store::mcp_install::PackageChoice,
        scope: wyj_store::InstallScope,
    },
    Upgrading {
        row_idx: usize,
    },
    /// 文本内容存在 `McpDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddRegistry,
    /// 只读详情浮层（Installed 行菜单的"查看详情"），Enter/Esc 都直接关闭
    Detail {
        title: String,
        lines: Vec<String>,
    },
}

/// Installed 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpRowAction {
    ToggleEnabled,
    Upgrade,
    Uninstall,
    ViewDetail,
}

/// Registries 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpSourceAction {
    OpenBrowse,
    Remove,
}

/// Browse 结果行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpBrowseAction {
    Install,
}

/// 三个 tab 各自的行菜单动作，用外层枚举包住（同一 dialog 不同 tab 菜单类型不同）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpMenuAction {
    Installed(McpRowAction),
    Registries(McpSourceAction),
    Browse(McpBrowseAction),
}

/// MCP server 管理面板状态（/mcp 命令触发）
pub struct McpDialog {
    pub tab: McpDialogTab,
    pub installed: Vec<McpInstalledRow>,
    /// 扁平化行游标，语义随当前 tab 变化（见 `rows()`），Left/Right 切 tab 时归零
    pub cursor: usize,
    /// 已添加的 registry 源（首次打开面板时自动预置官方源，见 `ensure_default_registry`）
    pub registries: Vec<wyj_store::lockfile::McpRegistrySource>,
    /// Browse tab 当前实际查询的源（Registries tab 里选中"浏览"菜单项切换）
    pub active_registry: wyj_store::lockfile::McpRegistrySource,
    pub browse_results: Vec<wyj_store::registry::RegistryServerSummary>,
    pub overlay: McpOverlay,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<FlatRow, McpMenuAction>>,
    pub error: Option<String>,
    pub status: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Mcp(_)` 生效期间使用）
    pub live_input: InputBox,
}

impl McpDialog {
    pub(super) fn new(cfg: &Config, cwd: &std::path::Path) -> Self {
        let merged = wyj_config::merged_mcp_servers(cfg, cwd);
        let global_lock = wyj_store::lockfile::load_global().unwrap_or_default();
        let project_lock = wyj_store::lockfile::load_project(cwd).unwrap_or_default();
        // 项目级来源只剩 `<git-root>/.wyj-code/mcp.toml` 一个（v0.2.24 起不再
        // 读取 `<cwd>/.mcp.json`），所以 Project scope 直接由 `load_project_mcp`
        // 判定。
        let project_names: std::collections::HashSet<String> = wyj_config::load_project_mcp(cwd)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();

        let installed = merged
            .into_iter()
            .filter_map(|config| {
                let scope = if project_names.contains(&config.name) {
                    wyj_store::InstallScope::Project
                } else {
                    wyj_store::InstallScope::Global
                };
                let managed = match scope {
                    wyj_store::InstallScope::Global => global_lock
                        .mcp_servers
                        .iter()
                        .find(|e| e.name == config.name)
                        .cloned(),
                    wyj_store::InstallScope::Project => project_lock
                        .mcp_servers
                        .iter()
                        .find(|e| e.name == config.name)
                        .cloned(),
                };
                let enabled = managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                // 隐藏"已禁用 + 非真正受管理"的条目：受管理的 server
                // （`is_managed()`）走正常禁用/启用流程要能在列表里找回，
                // 不能被一并隐藏。
                let is_managed = managed.as_ref().is_some_and(|m| m.is_managed());
                if !enabled && !is_managed {
                    return None;
                }
                Some(McpInstalledRow {
                    config,
                    scope,
                    managed,
                })
            })
            .collect();

        let registries = wyj_store::registry::ensure_default_registry()
            .unwrap_or_else(|_| vec![wyj_store::registry::official_registry_source()]);
        let active_registry = registries
            .first()
            .cloned()
            .unwrap_or_else(wyj_store::registry::official_registry_source);

        Self {
            tab: McpDialogTab::Installed,
            installed,
            cursor: 0,
            registries,
            active_registry,
            live_input: InputBox::new(),
            browse_results: Vec::new(),
            overlay: McpOverlay::None,
            menu: None,
            error: None,
            status: None,
        }
    }

    pub(super) fn refresh_installed(&mut self, cfg: &Config, cwd: &std::path::Path) {
        let fresh = Self::new(cfg, cwd);
        self.installed = fresh.installed;
        self.clamp_cursor();
    }

    /// 增删 registry 源后重新读盘刷新列表；尽量保留当前选中的 `active_registry`
    /// （按 id 匹配），若它被删除了则回退到列表第一项。
    pub(super) fn refresh_registries(&mut self) {
        self.registries = wyj_store::registry::ensure_default_registry().unwrap_or_default();
        self.clamp_cursor();
        if !self
            .registries
            .iter()
            .any(|r| r.id == self.active_registry.id)
        {
            if let Some(first) = self.registries.first() {
                self.active_registry = first.clone();
            }
        }
    }

    /// 扁平化行列表：当前 tab 下的条目下标，或该 tab 的固定"+ 添加"/"搜索"行。
    /// tab 头不算进 rows()，单独渲染；Installed 没有 AddNew（新增 MCP server
    /// 走 Browse+安装，不是手工添加）。Browse 把"搜索"行固定放在最前面（下标
    /// 0），复用 `FlatRow::AddNew` 这个"触发底部输入框借用"的通用语义。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            McpDialogTab::Installed => (0..self.installed.len()).map(FlatRow::Entry).collect(),
            McpDialogTab::Registries => {
                let mut r: Vec<_> = (0..self.registries.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            McpDialogTab::Browse => {
                let mut r = vec![FlatRow::AddNew];
                r.extend((0..self.browse_results.len()).map(FlatRow::Entry));
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

    /// 选中条目回车后弹出的操作菜单，None 表示当前行不支持弹菜单（不应发生，
    /// 调用方已确保 cursor 落在 Entry 行上）
    pub fn build_menu(&self) -> Option<ActionMenu<FlatRow, McpMenuAction>> {
        let row = *self.rows().get(self.cursor)?;
        match (self.tab, row) {
            (McpDialogTab::Installed, FlatRow::Entry(idx)) => {
                let r = self.installed.get(idx)?;
                let enabled = r.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                let can_upgrade = r.managed.as_ref().is_some_and(|m| m.is_managed());
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if enabled {
                            "mcp.menu.disable"
                        } else {
                            "mcp.menu.enable"
                        }),
                        action: McpMenuAction::Installed(McpRowAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.upgrade"),
                        action: McpMenuAction::Installed(McpRowAction::Upgrade),
                        dangerous: false,
                        disabled: !can_upgrade,
                        disabled_reason: (!can_upgrade)
                            .then(|| wyj_i18n::tr("mcp.error.manual_no_upgrade")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.uninstall"),
                        action: McpMenuAction::Installed(McpRowAction::Uninstall),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.view_detail"),
                        action: McpMenuAction::Installed(McpRowAction::ViewDetail),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (McpDialogTab::Registries, FlatRow::Entry(_)) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.browse_source"),
                        action: McpMenuAction::Registries(McpSourceAction::OpenBrowse),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.remove_source"),
                        action: McpMenuAction::Registries(McpSourceAction::Remove),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (McpDialogTab::Browse, FlatRow::Entry(_)) => {
                let items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("mcp.menu.install"),
                    action: McpMenuAction::Browse(McpBrowseAction::Install),
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

/// `/mcp` 面板操作菜单的按键分发：Up/Down 选、Enter 确认/二次确认、Esc 逐级返回。
/// 危险操作（卸载/删除源）先进 `confirming` 二级确认，再次 Enter/y 才真正执行。
pub(super) fn mcp_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(McpMenuAction),
        Execute(McpMenuAction, FlatRow),
    }

    let step = {
        let Some(dialog) = &mut state.mcp_dialog else {
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
            if let Some(dialog) = &mut state.mcp_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                dialog.menu = None;
            }
            mcp_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

pub(super) fn mcp_scope_label_text(scope: wyj_store::InstallScope) -> String {
    wyj_i18n::tr(match scope {
        wyj_store::InstallScope::Global => "mcp.dialog.scope_global",
        wyj_store::InstallScope::Project => "mcp.dialog.scope_project",
    })
}

/// Global-scope 的 MCP install/uninstall/upgrade 直接读写磁盘上的
/// `~/.wyj-code/config.toml`（`wyj_store::mcp_install` 内部各自 `Config::load()`
/// 后原地 `save()`），完全绕过 `AppState.config` 这份进程启动时加载、此后只在
/// `/model` `/config` 面板保存时才会更新的内存快照。`McpDialog::new`/
/// `refresh_installed` 却是拿 `&state.config` 当参数构建"已安装"列表——Project
/// scope 那部分没事（`merged_mcp_servers` 内部对项目配置本来就是每次现读盘），
/// 但 Global scope 那部分永远读的是这份过期快照，于是卸载/安装/升级全局 MCP
/// server 在磁盘上明明已经成功，`/mcp` 面板却像没生效一样（条目卸载不掉、装
/// 不上、升级后版本号不刷新），用户体感就是"无法正常卸载"。这里在每次这类
/// 操作成功后把内存快照拉回最新，读盘失败（罕见）时保留旧值，不让偶发 IO
/// 错误打断整个面板。
pub(super) fn reload_config_into_state(state: &mut AppState) {
    if let Ok(fresh) = Config::load() {
        state.config = fresh;
    }
}

/// 执行操作菜单选中项对应的动作（在按键分发之外单独拆出，便于危险操作走
/// 二级确认后再调用同一套逻辑）。`target` 是菜单弹出时记录的目标行，用于
/// 定位 `installed`/`registries`/`browse_results` 里具体是哪一条。
pub(super) fn mcp_execute_menu_action(
    state: &mut AppState,
    action: McpMenuAction,
    target: FlatRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    let FlatRow::Entry(idx) = target else {
        return;
    };
    match action {
        McpMenuAction::Installed(McpRowAction::ToggleEnabled) => {
            let info = state.mcp_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).map(|row| {
                    let enabled = row.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                    (row.config.name.clone(), row.scope, enabled)
                })
            });
            if let Some((name, scope, currently_enabled)) = info {
                let result = wyj_store::mcp_install::set_mcp_enabled(
                    &name,
                    scope,
                    &state.cwd,
                    !currently_enabled,
                );
                if let Some(dialog) = &mut state.mcp_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&state.config, &state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        McpMenuAction::Installed(McpRowAction::Upgrade) => {
            let info = state.mcp_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).and_then(|row| {
                    row.managed
                        .as_ref()
                        .is_some_and(|m| m.is_managed())
                        .then(|| (row.config.name.clone(), row.scope))
                })
            });
            if let Some((name, scope)) = info {
                if let Some(dialog) = &mut state.mcp_dialog {
                    dialog.overlay = McpOverlay::Upgrading { row_idx: idx };
                }
                let tx = agent_tx.clone();
                let cwd = state.cwd.clone();
                tokio::spawn(async move {
                    let result = wyj_store::mcp_install::upgrade_mcp_server(&name, scope, &cwd)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::McpUpgraded {
                            row_idx: idx,
                            result,
                        })
                        .await;
                });
            } else if let Some(dialog) = &mut state.mcp_dialog {
                dialog.error = Some(wyj_i18n::tr("mcp.error.manual_no_upgrade"));
            }
        }
        McpMenuAction::Installed(McpRowAction::Uninstall) => {
            let info = state
                .mcp_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| (row.config.name.clone(), row.scope));
            if let Some((name, scope)) = info {
                let result = wyj_store::mcp_install::uninstall_mcp_server(&name, scope, &state.cwd);
                if result.is_ok() {
                    reload_config_into_state(state);
                }
                if let Some(dialog) = &mut state.mcp_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status = Some(wyj_i18n::tr("mcp.uninstall.done"));
                            dialog.refresh_installed(&state.config, &state.cwd);
                        }
                        Err(e) => {
                            dialog.error = Some(wyj_i18n::tr_fmt(
                                "mcp.error.uninstall_failed",
                                &[("err", &e.to_string())],
                            ));
                        }
                    }
                }
            }
        }
        McpMenuAction::Installed(McpRowAction::ViewDetail) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(row) = dialog.installed.get(idx) {
                    let mut lines = vec![
                        wyj_i18n::tr_fmt("mcp.detail.name_line", &[("name", &row.config.name)]),
                        wyj_i18n::tr_fmt(
                            "mcp.detail.scope_line",
                            &[("scope", &mcp_scope_label_text(row.scope))],
                        ),
                        wyj_i18n::tr_fmt(
                            "mcp.detail.command_line",
                            &[("command", row.config.command.as_deref().unwrap_or(""))],
                        ),
                    ];
                    if !row.config.args.is_empty() {
                        lines.push(wyj_i18n::tr_fmt(
                            "mcp.detail.args_line",
                            &[("args", &row.config.args.join(" "))],
                        ));
                    }
                    if !row.config.env.is_empty() {
                        let env_str = row
                            .config
                            .env
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        lines.push(wyj_i18n::tr_fmt(
                            "mcp.detail.env_line",
                            &[("env", &env_str)],
                        ));
                    }
                    dialog.overlay = McpOverlay::Detail {
                        title: wyj_i18n::tr("mcp.detail.title"),
                        lines,
                    };
                }
            }
        }
        McpMenuAction::Registries(McpSourceAction::OpenBrowse) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(source) = dialog.registries.get(idx).cloned() {
                    dialog.active_registry = source;
                    dialog.browse_results.clear();
                    dialog.tab = McpDialogTab::Browse;
                    dialog.cursor = 0;
                }
            }
        }
        McpMenuAction::Registries(McpSourceAction::Remove) => {
            let id = state
                .mcp_dialog
                .as_ref()
                .and_then(|d| d.registries.get(idx))
                .map(|r| r.id.clone());
            if let Some(id) = id {
                let result = wyj_store::registry::remove_registry(&id);
                if let Some(dialog) = &mut state.mcp_dialog {
                    match result {
                        Ok(()) => dialog.refresh_registries(),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        McpMenuAction::Browse(McpBrowseAction::Install) => {
            let server = state
                .mcp_dialog
                .as_ref()
                .and_then(|d| d.browse_results.get(idx).cloned());
            if let Some(server) = server {
                let package = wyj_store::mcp_install::choose_package(&server.packages);
                if let Some(dialog) = &mut state.mcp_dialog {
                    if matches!(
                        package,
                        wyj_store::mcp_install::PackageChoice::Unsupported { .. }
                    ) {
                        dialog.error = Some(wyj_i18n::tr("mcp.install.unsupported_package"));
                    } else {
                        dialog.overlay = McpOverlay::InstallConfirm {
                            server: Box::new(server),
                            package,
                            scope: wyj_store::InstallScope::Global,
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod mcp_dialog_tests {
    use super::*;

    /// 回归测试：wyj 自己项目级来源（`<git-root>/.wyj-code/mcp.toml`）的
    /// server 被卸载后（v0.2.24 起走"从来源文件删行 + lockfile 移除"统一
    /// 逻辑，不再有原生来源的"禁用 shadow"兜底分支），应该从 `/mcp` 面板
    /// "已安装"列表里消失。
    #[test]
    fn uninstalled_wyj_project_server_disappears_from_installed_list() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(
            dir.path(),
            &[wyj_config::McpServerConfig {
                name: "wyj-srv".to_string(),
                transport: wyj_config::McpTransport::Stdio,
                command: Some("npx".to_string()),
                args: vec!["-y".to_string(), "wyj-srv-mcp".to_string()],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
        )
        .unwrap();

        let cfg = Config::default();
        let dialog = McpDialog::new(&cfg, dir.path());
        assert!(dialog.installed.iter().any(|r| r.config.name == "wyj-srv"));

        wyj_store::mcp_install::uninstall_mcp_server(
            "wyj-srv",
            wyj_store::InstallScope::Project,
            dir.path(),
        )
        .unwrap();

        let dialog = McpDialog::new(&cfg, dir.path());
        assert!(!dialog.installed.iter().any(|r| r.config.name == "wyj-srv"));
    }

    /// 回归测试：真正 wyj 自己 registry 安装、受 lockfile 管理的 server，
    /// 即使名字恰好和 `.mcp.json`/`~/.claude.json` 里的原生条目撞名，禁用后
    /// 也不应该被当成"卸载不掉的原生条目"一起从列表里隐藏——否则用户没法
    /// 再找到它重新启用。区分点是 `managed.is_managed()`：原生 shadow 记录
    /// 永远不是 managed，真正 registry 安装的记录永远是。
    #[test]
    fn disabling_managed_server_stays_visible_even_if_name_collides_with_native() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers": {"shared-name": {"command": "npx", "args": []}}}"#,
        )
        .unwrap();

        let server = wyj_store::registry::RegistryServerSummary {
            name: "io.modelcontextprotocol/shared-name".to_string(),
            description: "test".to_string(),
            version: "1.0.0".to_string(),
            packages: vec![wyj_store::registry::RegistryPackage {
                registry_type: "npm".to_string(),
                identifier: "shared-name-mcp".to_string(),
                version: "1.0.0".to_string(),
                registry_base_url: None,
                runtime_hint: Some("npx".to_string()),
                package_arguments: vec![],
                runtime_arguments: vec![],
                environment_variables: vec![],
            }],
            remotes: vec![],
        };
        let package = wyj_store::mcp_install::choose_package(&server.packages);
        let req = wyj_store::mcp_install::McpInstallRequest {
            server,
            package,
            scope: wyj_store::InstallScope::Project,
            name_override: Some("shared-name".to_string()),
            registry_url: "https://registry.modelcontextprotocol.io".to_string(),
        };
        wyj_store::mcp_install::install_mcp_server(&req, dir.path()).unwrap();

        wyj_store::mcp_install::set_mcp_enabled(
            "shared-name",
            wyj_store::InstallScope::Project,
            dir.path(),
            false,
        )
        .unwrap();

        let cfg = Config::default();
        let dialog = McpDialog::new(&cfg, dir.path());
        let row = dialog
            .installed
            .iter()
            .find(|r| r.config.name == "shared-name")
            .expect("受管理的 server 禁用后仍应留在已安装列表里");
        assert!(row.managed.as_ref().is_some_and(|m| m.is_managed()));
        assert!(!row.managed.as_ref().unwrap().enabled);
    }
}
