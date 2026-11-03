//! Agents / Extensions 两个列表型面板的状态与渲染数据。
//!
//! 从 `app.rs` 拆出。

use super::*;

pub struct AgentsDialog {
    pub defs: Vec<AgentDefinition>,
    pub selected: usize,
    pub detail_open: bool,
    pub detail_scroll: u16,
}

impl AgentsDialog {
    pub fn new(defs: Vec<AgentDefinition>) -> Self {
        Self {
            defs,
            selected: 0,
            detail_open: true,
            detail_scroll: 0,
        }
    }

    pub fn selected_def(&self) -> Option<&AgentDefinition> {
        self.defs.get(self.selected)
    }

    pub fn move_selected(&mut self, delta: i32) {
        if self.defs.is_empty() {
            self.selected = 0;
            return;
        }
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, self.defs.len() as i32 - 1) as usize;
        self.detail_scroll = 0;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExtensionAction {
    Enable,
    Disable,
    Remove,
    Refresh,
    ToggleDetail,
}

/// Unified Skill/MCP/Plugin inventory panel.  The specialized `/mcp`,
/// `/skills`, and `/plugins` panels remain available for marketplace browsing;
/// this panel is the fast daily-use surface for seeing the effective resource
/// set and applying enable/disable/remove actions consistently.
pub struct ExtensionsDialog {
    pub records: Vec<wyj_store::extensions::ExtensionRecord>,
    pub selected: usize,
    pub detail_open: bool,
    pub detail_scroll: u16,
    pub confirm: Option<ExtensionAction>,
    pub menu: Option<ActionMenu<usize, ExtensionAction>>,
    pub error: Option<String>,
}

impl ExtensionsDialog {
    pub fn new(cwd: &Path) -> Self {
        let mut dialog = Self {
            records: Vec::new(),
            selected: 0,
            detail_open: true,
            detail_scroll: 0,
            confirm: None,
            menu: None,
            error: None,
        };
        dialog.refresh(cwd);
        dialog
    }

    pub fn build_menu(&self) -> Option<ActionMenu<usize, ExtensionAction>> {
        let record = self.selected_record()?;
        let mut items = vec![ActionMenuItem {
            label: wyj_i18n::tr(if self.detail_open {
                "extensions.menu.hide_detail"
            } else {
                "extensions.menu.show_detail"
            }),
            action: ExtensionAction::ToggleDetail,
            dangerous: false,
            disabled: false,
            disabled_reason: None,
        }];
        if record.enabled {
            items.push(ActionMenuItem {
                label: wyj_i18n::tr("extensions.menu.disable"),
                action: ExtensionAction::Disable,
                dangerous: false,
                disabled: false,
                disabled_reason: None,
            });
        } else {
            items.push(ActionMenuItem {
                label: wyj_i18n::tr("extensions.menu.enable"),
                action: ExtensionAction::Enable,
                dangerous: false,
                disabled: false,
                disabled_reason: None,
            });
        }
        items.push(ActionMenuItem {
            label: wyj_i18n::tr("extensions.menu.remove"),
            action: ExtensionAction::Remove,
            dangerous: true,
            disabled: false,
            disabled_reason: None,
        });
        items.push(ActionMenuItem {
            label: wyj_i18n::tr("extensions.menu.refresh"),
            action: ExtensionAction::Refresh,
            dangerous: false,
            disabled: false,
            disabled_reason: None,
        });
        Some(ActionMenu::new(self.selected, items))
    }

    pub fn refresh(&mut self, cwd: &Path) {
        match wyj_store::extensions::list(cwd) {
            Ok(mut records) => {
                records.sort_by(|a, b| a.id.cmp(&b.id));
                self.records = records;
                self.selected = self.selected.min(self.records.len().saturating_sub(1));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.detail_scroll = 0;
    }

    pub fn selected_record(&self) -> Option<&wyj_store::extensions::ExtensionRecord> {
        self.records.get(self.selected)
    }

    pub fn move_selected(&mut self, delta: i32) {
        if self.records.is_empty() {
            self.selected = 0;
            return;
        }
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, self.records.len() as i32 - 1) as usize;
        self.detail_scroll = 0;
    }

    pub fn action_label(action: ExtensionAction) -> &'static str {
        match action {
            ExtensionAction::Enable => "enable",
            ExtensionAction::Disable => "disable",
            ExtensionAction::Remove => "remove",
            ExtensionAction::Refresh => "refresh",
            ExtensionAction::ToggleDetail => "detail",
        }
    }
}
