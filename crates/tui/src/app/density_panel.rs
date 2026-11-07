//! `/density` 面板：聊天区活动密度下拉选择。
//!
//! 三档固定：`compact` / `balanced` / `detailed`。选中后写回
//! `config.toml` 的 `[ui].activity_density`（与 `/effort` 的「只随会话」相反）。

use wyj_config::ActivityDensity;

/// 活动密度面板（`/density` 无参命令触发时 Some）。
pub struct DensityPickerDialog {
    /// 当前光标位置（0..2）
    pub selected: usize,
    /// 打开面板时已生效的档位（列表里用 ● 标出）
    pub current: ActivityDensity,
}

impl DensityPickerDialog {
    pub(super) fn new(current: ActivityDensity) -> Self {
        let selected = Self::levels()
            .iter()
            .position(|&d| d == current)
            .unwrap_or(1);
        Self { selected, current }
    }

    pub fn levels() -> [ActivityDensity; 3] {
        [
            ActivityDensity::Compact,
            ActivityDensity::Balanced,
            ActivityDensity::Detailed,
        ]
    }

    pub fn item_count(&self) -> usize {
        Self::levels().len()
    }

    pub fn item_value(&self, idx: usize) -> Option<ActivityDensity> {
        Self::levels().get(idx).copied()
    }

    pub fn item_label(&self, idx: usize) -> String {
        match self.item_value(idx) {
            Some(ActivityDensity::Compact) => wyj_i18n::tr("density.picker.compact"),
            Some(ActivityDensity::Balanced) => wyj_i18n::tr("density.picker.balanced"),
            Some(ActivityDensity::Detailed) => wyj_i18n::tr("density.picker.detailed"),
            None => String::new(),
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(self.item_count().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_defaults_selection_to_current_density() {
        let d = DensityPickerDialog::new(ActivityDensity::Detailed);
        assert_eq!(d.selected, 2);
        assert_eq!(d.item_value(d.selected), Some(ActivityDensity::Detailed));
    }

    #[test]
    fn picker_moves_within_bounds() {
        let mut d = DensityPickerDialog::new(ActivityDensity::Balanced);
        assert_eq!(d.selected, 1);
        d.move_up();
        assert_eq!(d.selected, 0);
        d.move_up();
        assert_eq!(d.selected, 0);
        d.move_down();
        d.move_down();
        d.move_down();
        assert_eq!(d.selected, 2);
    }
}
