//! 定时任务面板：`/schedule` 命令触发。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── 定时任务面板：/schedule 命令触发 ─────────────────────────────────────────
//
// 与 ProfileDialog 同一套"批量编辑 + Ctrl+S/Esc 三选一保存"模型：面板内的增删
// 改都只改内存里的 `tasks` 草稿，直到显式保存才一次性写盘（`schedule::save`）
// 并同步系统 crontab（`cron_sync::sync_crontab`）。字段编辑统一先弹一个只有
// 1 项（Name/Prompt/Cwd 的"编辑"）或多项（Cron 的频率预设/NotifyOnFailure 的
// 切换）的小菜单再决定是否借用输入框，比 ProfileDialog 对不同字段类型各自特判
// 简单，代价是多一次 Enter，可接受。

pub const SCHEDULE_FIELD_COUNT: usize = 6;
pub(super) const SCHEDULE_FIELD_NAME: usize = 0;
pub(super) const SCHEDULE_FIELD_PROMPT: usize = 1;
pub(super) const SCHEDULE_FIELD_CRON: usize = 2;
pub(super) const SCHEDULE_FIELD_CWD: usize = 3;
pub const SCHEDULE_FIELD_NOTIFY: usize = 4;
pub(super) const SCHEDULE_FIELD_ALLOWED_TOOLS: usize = 5;

pub const SCHEDULE_FIELD_LABEL_KEYS: [&str; SCHEDULE_FIELD_COUNT] = [
    "schedule.field.name",
    "schedule.field.prompt",
    "schedule.field.cron",
    "schedule.field.cwd",
    "schedule.field.notify_on_failure",
    "schedule.field.allowed_tools",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleRow {
    Header(usize),
    Field(usize, usize),
    AddNew,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleHeaderAction {
    ToggleExpand,
    ToggleEnabled,
    RunNow,
    ReviewPermissions,
    Delete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleFrequencyKind {
    Daily,
    Hourly,
    Weekly,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleFieldAction {
    ManualEdit,
    Freq(ScheduleFrequencyKind),
    ToggleNotify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAddAction {
    NewBlank,
    NewFromSession,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleMenuAction {
    Header(ScheduleHeaderAction),
    Field(ScheduleFieldAction),
    Add(ScheduleAddAction),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleInputField {
    Field {
        task_idx: usize,
        field_idx: usize,
    },
    Frequency {
        task_idx: usize,
        kind: ScheduleFrequencyKind,
    },
}

/// `ScheduleDialog` 里当前展示的浮层。危险操作（删除任务）的二次确认复用
/// `ActionMenu.confirming`（同 ProfileDialog 的 Delete），不需要专属 overlay。
pub enum ScheduleOverlay {
    None,
    /// Esc 关闭面板时存在未保存修改，三选一确认（保存并关闭/不保存关闭/取消）
    UnsavedChanges {
        selected: usize,
    },
    /// 保存成功但同步系统 crontab 失败：数据已落盘，仅提示用户手动重试
    SyncError {
        message: String,
    },
}

/// 定时任务面板状态（/schedule 无参命令触发）
pub struct ScheduleDialog {
    pub tasks: Vec<wyj_store::schedule::ScheduleTask>,
    pub cursor: usize,
    pub expanded: Option<usize>,
    pub overlay: ScheduleOverlay,
    pub error: Option<String>,
    pub live_input: InputBox,
    pub menu: Option<ActionMenu<ScheduleRow, ScheduleMenuAction>>,
    default_cwd: std::path::PathBuf,
    /// 当前会话最近一条用户消息，供"从当前对话固化为模板"预填 prompt 草稿。
    last_user_prompt: Option<String>,
    /// 打开面板时的快照，供 Esc 时判断"是否有未保存改动"。
    saved_snapshot: Vec<wyj_store::schedule::ScheduleTask>,
}

impl ScheduleDialog {
    pub(super) fn new(cwd: &std::path::Path, last_user_prompt: Option<String>) -> Self {
        let tasks = wyj_store::schedule::load()
            .map(|m| m.tasks)
            .unwrap_or_default();
        Self {
            saved_snapshot: tasks.clone(),
            tasks,
            cursor: 0,
            expanded: None,
            overlay: ScheduleOverlay::None,
            error: None,
            live_input: InputBox::new(),
            menu: None,
            default_cwd: cwd.to_path_buf(),
            last_user_prompt,
        }
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.tasks != self.saved_snapshot
    }

    pub fn rows(&self) -> Vec<ScheduleRow> {
        let mut rows = Vec::new();
        for i in 0..self.tasks.len() {
            rows.push(ScheduleRow::Header(i));
            if self.expanded == Some(i) {
                for f in 0..SCHEDULE_FIELD_COUNT {
                    rows.push(ScheduleRow::Field(i, f));
                }
            }
        }
        rows.push(ScheduleRow::AddNew);
        rows
    }

    pub(super) fn selected_row(&self) -> ScheduleRow {
        self.rows()
            .get(self.cursor)
            .copied()
            .unwrap_or(ScheduleRow::AddNew)
    }

    pub(super) fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn field_text(&self, task_idx: usize, field_idx: usize) -> String {
        let t = &self.tasks[task_idx];
        match field_idx {
            SCHEDULE_FIELD_NAME => t.name.clone(),
            SCHEDULE_FIELD_PROMPT => t.prompt.clone(),
            SCHEDULE_FIELD_CRON => t.cron.clone(),
            SCHEDULE_FIELD_CWD => t.cwd.display().to_string(),
            SCHEDULE_FIELD_ALLOWED_TOOLS => t.permissions.allowed_tools.join(","),
            _ => String::new(),
        }
    }

    pub(super) fn set_field_text(&mut self, task_idx: usize, field_idx: usize, value: String) {
        let t = &mut self.tasks[task_idx];
        match field_idx {
            SCHEDULE_FIELD_NAME => t.name = value,
            SCHEDULE_FIELD_PROMPT => t.prompt = value,
            SCHEDULE_FIELD_CRON => t.cron = value,
            SCHEDULE_FIELD_CWD => t.cwd = std::path::PathBuf::from(value),
            SCHEDULE_FIELD_ALLOWED_TOOLS => {
                t.permissions.allowed_tools = comma_values(&value);
            }
            _ => {}
        }
    }

    /// 校验全部任务，返回第一个不合法任务的下标 + 错误文案。
    pub(super) fn validate(&self) -> Option<(usize, String)> {
        for (i, t) in self.tasks.iter().enumerate() {
            if t.name.trim().is_empty() {
                return Some((i, wyj_i18n::tr("schedule.error.empty_name")));
            }
            if t.prompt.trim().is_empty() {
                return Some((i, wyj_i18n::tr("schedule.error.empty_prompt")));
            }
            if t.cwd.as_os_str().is_empty() {
                return Some((i, wyj_i18n::tr("schedule.error.empty_cwd")));
            }
            if let Err(e) = wyj_store::cron_sync::validate_cron(&t.cron) {
                return Some((
                    i,
                    wyj_i18n::tr_fmt("schedule.error.invalid_cron", &[("err", &e.to_string())]),
                ));
            }
        }
        None
    }

    /// 选中条目回车后弹出的操作菜单。头行 → 展开/收起、启用/停用、立即运行、删除；
    /// Cron 字段 → 频率预设四选一；NotifyOnFailure 字段 → 切换；其余字段 → 编辑；
    /// AddNew → 新建空白/从当前对话固化为模板（后者仅当会话内有过用户消息时展示）。
    pub(super) fn build_menu(&self) -> Option<ActionMenu<ScheduleRow, ScheduleMenuAction>> {
        let row = self.selected_row();
        match row {
            ScheduleRow::Header(task_idx) => {
                let expanded = self.expanded == Some(task_idx);
                let task = &self.tasks[task_idx];
                let saved = self.saved_snapshot.iter().find(|t| t.id == task.id);
                let run_disabled = saved != Some(task) || task.needs_permission_review;
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if expanded {
                            "schedule.menu.collapse"
                        } else {
                            "schedule.menu.expand"
                        }),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleExpand),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr(if task.enabled {
                            "schedule.menu.disable"
                        } else {
                            "schedule.menu.enable"
                        }),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.run_now"),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::RunNow),
                        dangerous: false,
                        disabled: run_disabled,
                        disabled_reason: run_disabled
                            .then(|| wyj_i18n::tr("schedule.menu.run_now_needs_save")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.review_permissions"),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::ReviewPermissions),
                        dangerous: true,
                        disabled: !task.needs_permission_review,
                        disabled_reason: (!task.needs_permission_review)
                            .then(|| wyj_i18n::tr("schedule.menu.permissions_reviewed")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.delete"),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::Delete),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            ScheduleRow::Field(_, SCHEDULE_FIELD_NOTIFY) => Some(ActionMenu::new(
                row,
                vec![ActionMenuItem {
                    label: wyj_i18n::tr("schedule.menu.toggle"),
                    action: ScheduleMenuAction::Field(ScheduleFieldAction::ToggleNotify),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }],
            )),
            ScheduleRow::Field(_, SCHEDULE_FIELD_CRON) => {
                let mk = |label_key: &str, kind: ScheduleFrequencyKind| ActionMenuItem {
                    label: wyj_i18n::tr(label_key),
                    action: ScheduleMenuAction::Field(ScheduleFieldAction::Freq(kind)),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                };
                Some(ActionMenu::new(
                    row,
                    vec![
                        mk("schedule.menu.freq_daily", ScheduleFrequencyKind::Daily),
                        mk("schedule.menu.freq_hourly", ScheduleFrequencyKind::Hourly),
                        mk("schedule.menu.freq_weekly", ScheduleFrequencyKind::Weekly),
                        mk("schedule.menu.freq_custom", ScheduleFrequencyKind::Custom),
                    ],
                ))
            }
            ScheduleRow::Field(..) => Some(ActionMenu::new(
                row,
                vec![ActionMenuItem {
                    label: wyj_i18n::tr("schedule.menu.edit"),
                    action: ScheduleMenuAction::Field(ScheduleFieldAction::ManualEdit),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }],
            )),
            ScheduleRow::AddNew => {
                let mut items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("schedule.menu.new_blank"),
                    action: ScheduleMenuAction::Add(ScheduleAddAction::NewBlank),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }];
                if self.last_user_prompt.is_some() {
                    items.push(ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.new_from_session"),
                        action: ScheduleMenuAction::Add(ScheduleAddAction::NewFromSession),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    });
                }
                Some(ActionMenu::new(row, items))
            }
        }
    }
}

/// `/schedule` 面板操作菜单的按键分发，结构与 `profile_handle_menu_key` 完全对称。
pub(super) fn schedule_handle_menu_key(state: &mut AppState, code: KeyCode) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(ScheduleMenuAction),
        Execute(ScheduleMenuAction, ScheduleRow),
    }

    let step = {
        let Some(dialog) = &mut state.schedule_dialog else {
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
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.schedule_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.schedule_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.menu = None;
            }
            schedule_execute_menu_action(state, action, target);
        }
    }
}

pub(super) fn schedule_add_new_task(state: &mut AppState, prefill_prompt: Option<String>) {
    let Some(dialog) = &mut state.schedule_dialog else {
        return;
    };
    let now = chrono::Utc::now();
    let task = wyj_store::schedule::ScheduleTask {
        id: uuid::Uuid::new_v4().to_string(),
        name: wyj_i18n::tr("schedule.new_task_default_name"),
        prompt: prefill_prompt.unwrap_or_default(),
        cron: "0 8 * * *".to_string(),
        cwd: dialog.default_cwd.clone(),
        enabled: true,
        needs_permission_review: false,
        permissions: wyj_store::schedule::SchedulePermissions::default(),
        notify_on_failure: false,
        created_at: now,
        updated_at: now,
        last_run: None,
    };
    dialog.tasks.push(task);
    let new_idx = dialog.tasks.len() - 1;
    dialog.expanded = Some(new_idx);
    dialog.cursor = dialog
        .rows()
        .iter()
        .position(|r| *r == ScheduleRow::Header(new_idx))
        .unwrap_or(0);
}

pub(super) fn schedule_execute_menu_action(
    state: &mut AppState,
    action: ScheduleMenuAction,
    target: ScheduleRow,
) {
    match action {
        ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleExpand) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.expanded = if dialog.expanded == Some(idx) {
                    None
                } else {
                    Some(idx)
                };
                dialog.clamp_cursor();
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleEnabled) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                if dialog.tasks[idx].needs_permission_review {
                    dialog.error = Some(
                        "该任务由旧版本迁移而来，需先执行 `wyj-code schedule review <id> ...` 完成权限审查"
                            .to_string(),
                    );
                } else {
                    dialog.tasks[idx].enabled = !dialog.tasks[idx].enabled;
                }
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::RunNow) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            // 只对"已保存、未脏"的任务生效（菜单项在脏态下已 disabled，这里再兜底一次），
            // 直接以子进程调用自身 `schedule run <id>`——与系统 crontab 触发路径完全
            // 一致，不在 TUI 进程内重新装配 Agent。
            let id = state
                .schedule_dialog
                .as_ref()
                .and_then(|d| d.tasks.get(idx))
                .map(|t| t.id.clone());
            if let Some(id) = id {
                if let Ok(exe) = std::env::current_exe() {
                    tokio::spawn(async move {
                        // headless 前台接管已统一失败关闭，后台 app_computer
                        // 可与用户并行。
                        let _ = tokio::process::Command::new(exe)
                            .arg("schedule")
                            .arg("run")
                            .arg(&id)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .await;
                    });
                }
            }
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.error = Some(wyj_i18n::tr("schedule.run_now.triggered"));
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::ReviewPermissions) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                let task = &mut dialog.tasks[idx];
                task.needs_permission_review = false;
                task.enabled = false;
                dialog.error = Some(wyj_i18n::tr("schedule.permissions.review_complete"));
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::Delete) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.tasks.remove(idx);
                match dialog.expanded {
                    Some(e) if e == idx => dialog.expanded = None,
                    Some(e) if e > idx => dialog.expanded = Some(e - 1),
                    _ => {}
                }
                dialog.clamp_cursor();
            }
        }
        ScheduleMenuAction::Field(ScheduleFieldAction::ManualEdit) => {
            let ScheduleRow::Field(task_idx, field_idx) = target else {
                return;
            };
            let prefill = state
                .schedule_dialog
                .as_ref()
                .map(|d| d.field_text(task_idx, field_idx))
                .unwrap_or_default();
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.live_input = InputBox::new();
                dialog.live_input.insert_text(&prefill);
            }
            state.input_owner = Some(InputOwner::Schedule(ScheduleInputField::Field {
                task_idx,
                field_idx,
            }));
        }
        ScheduleMenuAction::Field(ScheduleFieldAction::ToggleNotify) => {
            let ScheduleRow::Field(task_idx, _) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.tasks[task_idx].notify_on_failure =
                    !dialog.tasks[task_idx].notify_on_failure;
            }
        }
        ScheduleMenuAction::Field(ScheduleFieldAction::Freq(kind)) => {
            let ScheduleRow::Field(task_idx, _) = target else {
                return;
            };
            let prefill = if matches!(kind, ScheduleFrequencyKind::Custom) {
                state
                    .schedule_dialog
                    .as_ref()
                    .map(|d| d.tasks[task_idx].cron.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.live_input = InputBox::new();
                dialog.live_input.insert_text(&prefill);
            }
            state.input_owner = Some(InputOwner::Schedule(ScheduleInputField::Frequency {
                task_idx,
                kind,
            }));
        }
        ScheduleMenuAction::Add(ScheduleAddAction::NewBlank) => {
            schedule_add_new_task(state, None);
        }
        ScheduleMenuAction::Add(ScheduleAddAction::NewFromSession) => {
            let prompt = state
                .schedule_dialog
                .as_ref()
                .and_then(|d| d.last_user_prompt.clone());
            schedule_add_new_task(state, prompt);
        }
    }
}

#[cfg(test)]
mod schedule_permissions_tests {
    use super::*;

    pub(super) fn migrated_task(cwd: &std::path::Path) -> wyj_store::schedule::ScheduleTask {
        let now = chrono::Utc::now();
        wyj_store::schedule::ScheduleTask {
            id: "legacy".to_string(),
            name: "legacy".to_string(),
            prompt: "review me".to_string(),
            cron: "0 8 * * *".to_string(),
            cwd: cwd.to_path_buf(),
            enabled: false,
            needs_permission_review: true,
            permissions: wyj_store::schedule::SchedulePermissions {
                allowed_tools: vec!["Read".to_string(), "Bash".to_string()],
            },
            notify_on_failure: false,
            created_at: now,
            updated_at: now,
            last_run: None,
        }
    }

    #[test]
    pub(super) fn reviewing_migrated_permissions_keeps_task_disabled_and_preserves_manifest() {
        let cwd = tempfile::tempdir().unwrap();
        let task = migrated_task(cwd.path());
        let expected_permissions = task.permissions.clone();
        let mut state = AppState::new(
            cwd.path().to_path_buf(),
            "test-model".to_string(),
            200_000,
            AgentMode::Auto,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        );
        state.schedule_dialog = Some(ScheduleDialog {
            tasks: vec![task.clone()],
            cursor: 0,
            expanded: None,
            overlay: ScheduleOverlay::None,
            error: None,
            live_input: InputBox::new(),
            menu: None,
            default_cwd: cwd.path().to_path_buf(),
            last_user_prompt: None,
            saved_snapshot: vec![task],
        });

        schedule_execute_menu_action(
            &mut state,
            ScheduleMenuAction::Header(ScheduleHeaderAction::ReviewPermissions),
            ScheduleRow::Header(0),
        );

        let reviewed = &state.schedule_dialog.as_ref().unwrap().tasks[0];
        assert!(!reviewed.needs_permission_review);
        assert!(!reviewed.enabled);
        assert_eq!(reviewed.permissions, expected_permissions);
    }
}

pub(super) fn parse_hh_mm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

pub(super) fn comma_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// 频率预设的结构化文本输入解析：Daily 期望 "HH:MM"，Hourly 期望 "MM"，
/// Weekly 期望 "D HH:MM"（D=0..6，0=周日），Custom 直接校验为标准 5 段 cron。
pub(super) fn schedule_parse_frequency_input(
    kind: ScheduleFrequencyKind,
    text: &str,
) -> Result<String, String> {
    let text = text.trim();
    match kind {
        ScheduleFrequencyKind::Daily => {
            let (h, m) =
                parse_hh_mm(text).ok_or_else(|| wyj_i18n::tr("schedule.error.bad_time_format"))?;
            Ok(wyj_store::cron_sync::frequency_to_cron(
                &wyj_store::cron_sync::Frequency::Daily { hour: h, minute: m },
            ))
        }
        ScheduleFrequencyKind::Hourly => {
            let m: u32 = text
                .parse()
                .map_err(|_| wyj_i18n::tr("schedule.error.bad_minute_format"))?;
            if m > 59 {
                return Err(wyj_i18n::tr("schedule.error.bad_minute_format"));
            }
            Ok(wyj_store::cron_sync::frequency_to_cron(
                &wyj_store::cron_sync::Frequency::Hourly { minute: m },
            ))
        }
        ScheduleFrequencyKind::Weekly => {
            let mut parts = text.splitn(2, char::is_whitespace);
            let day = parts.next().unwrap_or_default();
            let time = parts.next().unwrap_or_default().trim();
            let weekday: u32 = day
                .parse()
                .map_err(|_| wyj_i18n::tr("schedule.error.bad_weekly_format"))?;
            if weekday > 6 {
                return Err(wyj_i18n::tr("schedule.error.bad_weekly_format"));
            }
            let (h, m) = parse_hh_mm(time)
                .ok_or_else(|| wyj_i18n::tr("schedule.error.bad_weekly_format"))?;
            Ok(wyj_store::cron_sync::frequency_to_cron(
                &wyj_store::cron_sync::Frequency::Weekly {
                    weekday,
                    hour: h,
                    minute: m,
                },
            ))
        }
        ScheduleFrequencyKind::Custom => {
            wyj_store::cron_sync::validate_cron(text).map_err(|e| e.to_string())?;
            Ok(text.to_string())
        }
    }
}

/// Ctrl+S 与"保存并关闭"菜单选项共用的保存逻辑：校验 → 整份写盘 → 同步系统
/// crontab。与 `profile_try_save` 不同，这里不需要重建 Agent（定时任务不影响
/// 当前会话的模型/工具配置）。返回 `true` 表示任务数据已成功落盘（即使随后的
/// crontab 同步失败，也视为"保存成功"——数据本身没有丢，调用方据此决定是否
/// 关闭面板：crontab 同步失败时会转入 `ScheduleOverlay::SyncError` 停留在面板上）。
pub(super) fn schedule_try_save(state: &mut AppState) -> bool {
    let mut saved = false;
    if let Some(dialog) = &mut state.schedule_dialog {
        if let Some((bad_idx, msg)) = dialog.validate() {
            dialog.expanded = Some(bad_idx);
            dialog.clamp_cursor();
            dialog.error = Some(msg);
        } else {
            // 用 `load()` 打底而不是从零构造：manifest 的 `baseline_json` 记录了
            // 「面板打开那一刻磁盘上有什么」，`save` 据此做三方 diff。没有基线就会
            // 退化成整份覆盖，把面板打开期间别的实例新建的任务整个抹掉。
            let mut manifest = wyj_store::schedule::load().unwrap_or_default();
            manifest.tasks = dialog.tasks.clone();
            match wyj_store::schedule::save(&manifest) {
                Ok(()) => {
                    saved = true;
                    dialog.saved_snapshot = dialog.tasks.clone();
                    dialog.error = None;
                    if let Err(e) = wyj_store::cron_sync::sync_crontab(
                        &manifest.tasks,
                        &wyj_config::StorageRetentionCfg::default(),
                    ) {
                        dialog.overlay = ScheduleOverlay::SyncError {
                            message: e.to_string(),
                        };
                    }
                }
                Err(e) => {
                    dialog.error = Some(wyj_i18n::tr_fmt(
                        "schedule.error.save_failed",
                        &[("err", &e.to_string())],
                    ));
                }
            }
        }
    }
    saved
}

#[cfg(test)]
mod schedule_dialog_window_tests {
    use super::*;
    use crate::test_support::{make_state, render_to_text};
    use std::path::PathBuf;
    use wyj_store::schedule::ScheduleTask;

    fn task(i: usize) -> ScheduleTask {
        ScheduleTask {
            id: format!("t{i}"),
            name: format!("task-{i:02}"),
            prompt: "p".to_string(),
            cron: "0 9 * * *".to_string(),
            cwd: PathBuf::from("/tmp"),
            enabled: true,
            needs_permission_review: false,
            permissions: Default::default(),
            notify_on_failure: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_run: None,
        }
    }

    /// 与 `/model` 分组面板同一个根因的第三处：任务一多，`rows()` 全量塞进无
    /// scroll 的 Paragraph，光标走到屏幕外、末尾「+ 新建任务」和提示被裁掉。
    #[test]
    fn schedule_rows_scroll_to_keep_cursor_and_footer_visible() {
        let count = 40;
        let tasks: Vec<ScheduleTask> = (0..count).map(task).collect();
        let mut state = make_state();
        state.schedule_dialog = Some(ScheduleDialog {
            saved_snapshot: tasks.clone(),
            tasks,
            cursor: count, // 末行「+ 新建任务」
            expanded: None,
            overlay: ScheduleOverlay::None,
            error: None,
            live_input: crate::input::InputBox::new(),
            menu: None,
            default_cwd: PathBuf::from("/tmp"),
            last_user_prompt: None,
        });

        let input = crate::input::InputBox::new();
        let text = render_to_text(|f| crate::render::draw(f, &mut state, &input), 80, 24);

        let add_new = format!("+ {}", wyj_i18n::tr("schedule.dialog.add_new_row"));
        let add_line = text
            .lines()
            .find(|l| l.contains(&add_new))
            .unwrap_or_else(|| panic!("add-new row is off screen:\n{text}"));
        assert!(add_line.contains('▶'), "no cursor on {add_line:?}");

        // rows = 41，inner = 20，正文窗口 16 行 → 起点 25，task-25..task-39 可见。
        assert!(text.contains("task-39"), "window did not scroll:\n{text}");
        assert!(
            !text.contains("task-24"),
            "window start should be 25:\n{text}"
        );
        assert!(
            !text.contains("task-00"),
            "first page should be gone:\n{text}"
        );

        // 尾部固定区（分隔线 + 错误行 + 两条提示）不得被正文挤掉。
        let hint2: String = wyj_i18n::tr("schedule.dialog.hint2")
            .chars()
            .take(8)
            .collect();
        assert!(text.contains(&hint2), "footer hint2 is missing:\n{text}");
    }

    /// 任务少时行为不变：从第一行画起。
    #[test]
    fn small_schedule_dialog_starts_at_the_first_row() {
        let tasks: Vec<ScheduleTask> = (0..3).map(task).collect();
        let mut state = make_state();
        state.schedule_dialog = Some(ScheduleDialog {
            saved_snapshot: tasks.clone(),
            tasks,
            cursor: 0,
            expanded: None,
            overlay: ScheduleOverlay::None,
            error: None,
            live_input: crate::input::InputBox::new(),
            menu: None,
            default_cwd: PathBuf::from("/tmp"),
            last_user_prompt: None,
        });
        let input = crate::input::InputBox::new();
        let text = render_to_text(|f| crate::render::draw(f, &mut state, &input), 80, 24);
        assert!(text.contains("task-00"));
        assert!(text.contains("task-02"));
        assert!(text.contains(&format!(
            "+ {}",
            wyj_i18n::tr("schedule.dialog.add_new_row")
        )));
    }
}
