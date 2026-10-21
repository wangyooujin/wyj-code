//! 面板通用交互组件：`InputOwner` / `FlatRow` / `ActionMenu`。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── 面板通用交互组件：InputOwner / FlatRow / ActionMenu ─────────────────────────
//
// 供 McpDialog/SkillsDialog/PluginsDialog 共享的"方向键菜单导航 + 底部主输入框
// 借用"模式。不做成跨三个面板的泛型 dialog trait——三者的行 payload 形状完全
// 不同，硬抽象只会牺牲可读性；这里只共享"输入借用去哪儿写"和"操作菜单长什么样"
// 这两个真正通用的部分。

/// 主输入框（屏幕底部、用户平时打字发消息那个）当前借给了谁。
/// None = 属于聊天（默认）。Some(_) 时 `tui_main` 对 `Event::Key`/`Event::Paste`
/// 的分发在最前面拦截，聊天草稿本身不受影响（用户输入到一半被打断也不丢）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputOwner {
    Mcp(McpInputField),
    Skills(SkillsInputField),
    Plugins(PluginsInputField),
    Profile(ProfileInputField),
    Schedule(ScheduleInputField),
    /// `/config` 面板的文本字段编辑借用态（search api_key / max_results）。
    Settings(SettingsInputField),
    /// `/model` 面板的「手动输入模型名」借用态。
    ///
    /// 端点不一定实现 `/models` 接口（第三方代理常见），拉不到列表时
    /// 用户仍要能直接敲模型名——这一项是那个场景的唯一出路。
    Model(ModelInputField),
}

/// `/model` 面板可借用输入框的场景。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModelInputField {
    /// 手动输入模型名
    ManualName,
}

/// `/config` 面板可借用输入框的文本字段。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsInputField {
    /// 字段下标（`SETTINGS_FIELD_SPECS` 顺序）
    Field { field_idx: usize },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpInputField {
    AddRegistryUrl,
    BrowseSearch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsInputField {
    AddMarketplaceUrl,
    /// Browse tab 顶部的注册表搜索框：输入关键字，回车后发起 HTTP 搜索
    BrowseSearch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsInputField {
    AddMarketplaceUrl,
    AddLocalPluginPath,
}

/// `/model` 面板字段编辑借用态：重命名分组只需 `entry_idx`；编辑具体字段
/// 需要 `entry_idx` + `field_idx`（字段的 i18n label 可通过 `field_idx` 静态
/// 查表 `PROFILE_FIELD_LABEL_KEYS` 得到，不需要访问 entry 名字或其它 state）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileInputField {
    Rename { entry_idx: usize },
    Field { entry_idx: usize, field_idx: usize },
}

impl InputOwner {
    /// 借用期间输入框标题文案 + 主题色，如 "[MCP] 输入 registry URL (Enter 提交 / Esc 取消)"
    pub fn prompt(&self) -> (String, UiColor) {
        match self {
            InputOwner::Mcp(McpInputField::AddRegistryUrl) => (
                wyj_i18n::tr("dialog.input_owner.mcp_add_registry"),
                Theme::claude_color(),
            ),
            InputOwner::Mcp(McpInputField::BrowseSearch) => (
                wyj_i18n::tr("dialog.input_owner.mcp_browse_search"),
                Theme::claude_color(),
            ),
            InputOwner::Skills(SkillsInputField::AddMarketplaceUrl) => (
                wyj_i18n::tr("dialog.input_owner.skills_add_marketplace"),
                Theme::claude_color(),
            ),
            InputOwner::Skills(SkillsInputField::BrowseSearch) => (
                wyj_i18n::tr("dialog.input_owner.skills_search"),
                Theme::claude_color(),
            ),
            InputOwner::Plugins(PluginsInputField::AddMarketplaceUrl) => (
                wyj_i18n::tr("dialog.input_owner.plugins_add_marketplace"),
                Theme::claude_color(),
            ),
            InputOwner::Plugins(PluginsInputField::AddLocalPluginPath) => (
                wyj_i18n::tr("dialog.input_owner.plugins_add_local"),
                Theme::claude_color(),
            ),
            InputOwner::Profile(ProfileInputField::Rename { .. }) => (
                wyj_i18n::tr("dialog.input_owner.profile_rename"),
                Theme::claude_color(),
            ),
            InputOwner::Profile(ProfileInputField::Field { field_idx, .. }) => (
                wyj_i18n::tr_fmt(
                    "dialog.input_owner.profile_field",
                    &[(
                        "field",
                        &wyj_i18n::tr(
                            PROFILE_FIELD_SPECS
                                .get(*field_idx)
                                .map(|s| s.key)
                                .unwrap_or(""),
                        ),
                    )],
                ),
                Theme::claude_color(),
            ),
            InputOwner::Schedule(ScheduleInputField::Field { field_idx, .. }) => (
                wyj_i18n::tr_fmt(
                    "dialog.input_owner.schedule_field",
                    &[(
                        "field",
                        &wyj_i18n::tr(SCHEDULE_FIELD_LABEL_KEYS[*field_idx]),
                    )],
                ),
                Theme::claude_color(),
            ),
            InputOwner::Schedule(ScheduleInputField::Frequency { kind, .. }) => (
                wyj_i18n::tr(match kind {
                    ScheduleFrequencyKind::Daily => "dialog.input_owner.schedule_freq_daily",
                    ScheduleFrequencyKind::Hourly => "dialog.input_owner.schedule_freq_hourly",
                    ScheduleFrequencyKind::Weekly => "dialog.input_owner.schedule_freq_weekly",
                    ScheduleFrequencyKind::Custom => "dialog.input_owner.schedule_freq_custom",
                }),
                Theme::claude_color(),
            ),
            InputOwner::Settings(SettingsInputField::Field { field_idx }) => (
                wyj_i18n::tr_fmt(
                    "dialog.input_owner.settings_field",
                    &[(
                        "field",
                        &wyj_i18n::tr(
                            SETTINGS_FIELD_SPECS
                                .get(*field_idx)
                                .map(|s| s.key)
                                .unwrap_or(""),
                        ),
                    )],
                ),
                Theme::claude_color(),
            ),
            InputOwner::Model(ModelInputField::ManualName) => (
                wyj_i18n::tr("dialog.input_owner.model_manual"),
                Theme::claude_color(),
            ),
        }
    }

    /// 取出该字段对应的可变草稿 InputBox —— 统一指向各 dialog 自己持有的
    /// `live_input` 槽位（每个 dialog 只留一个槽位，当前被哪个字段占用由
    /// `AppState.input_owner` 决定，不再各表单各开一份 InputBox）。
    pub fn live_input_mut<'a>(&self, state: &'a mut AppState) -> Option<&'a mut InputBox> {
        match self {
            InputOwner::Mcp(_) => state.mcp_dialog.as_mut().map(|d| &mut d.live_input),
            InputOwner::Skills(_) => state.skills_dialog.as_mut().map(|d| &mut d.live_input),
            InputOwner::Plugins(_) => state.plugins_dialog.as_mut().map(|d| &mut d.live_input),
            InputOwner::Profile(_) => state.profile_dialog.as_mut().map(|d| &mut d.live_input),
            InputOwner::Schedule(_) => state.schedule_dialog.as_mut().map(|d| &mut d.live_input),
            InputOwner::Settings(_) => state.settings_dialog.as_mut().map(|d| &mut d.live_input),
            InputOwner::Model(_) => state.model_picker.as_mut().map(|d| &mut d.live_input),
        }
    }

    /// 只读版本，供渲染层（`render::draw_input`）决定当前该画哪个 InputBox 使用。
    pub fn live_input<'a>(&self, state: &'a AppState) -> Option<&'a InputBox> {
        match self {
            InputOwner::Mcp(_) => state.mcp_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Skills(_) => state.skills_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Plugins(_) => state.plugins_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Profile(_) => state.profile_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Schedule(_) => state.schedule_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Settings(_) => state.settings_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Model(_) => state.model_picker.as_ref().map(|d| &d.live_input),
        }
    }

    /// 借用态下 Esc 取消时，清空对应 dialog 的 live_input 草稿。
    pub fn clear_live_input(&self, state: &mut AppState) {
        if let Some(ib) = self.live_input_mut(state) {
            *ib = InputBox::new();
        }
    }
}

/// 三个面板的 `rows()` 都返回这个统一形状：当前 tab 下的条目下标，
/// 或该 tab 末尾固定的"+ 添加"行。tab 头本身**不算进 rows()**——
/// tab 头单独渲染，光标只在 rows() 范围内移动；Left/Right 切 tab 时
/// rows() 整体替换、cursor 归零，Up/Down 走列表，两个维度完全正交。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlatRow {
    Entry(usize),
    AddNew,
}

/// 三个面板共用的"选中条目后弹出的操作菜单"（Installed 行的启停/升级/卸载/
/// 查看详情，Registries/Marketplaces 行的浏览/删除，等等）。
///
/// `target` 类型泛化为 `T`（而非硬编码 `FlatRow`）：Mcp/Skills/Plugins 是纯扁平
/// 列表，一个 `FlatRow::Entry(usize)` 就够定位；但 ProfileDialog 是"entry 头行 +
/// 展开字段行"两层结构，需要同时装下 entry_idx + field_idx，只能用专属的
/// `ProfileRow` 作为 `T`。`draw_action_menu` 从不读取 `target`（只读
/// `confirming`/`items`/`selected`），泛化对已有三面板是零风险的机械签名变更，
/// 所有既有调用点靠类型推导自动落地 `T=FlatRow`，不需要改任何调用代码。
pub struct ActionMenu<T, A> {
    /// 菜单是对哪一行弹出的，用于标题里显示条目名 + 执行动作时定位数据
    pub target: T,
    pub items: Vec<ActionMenuItem<A>>,
    pub selected: usize,
    /// Some(a) 表示已选中 items 里的危险动作 a，进入"确定要 xxx 吗？[是/否]"二级确认
    pub confirming: Option<A>,
}

pub struct ActionMenuItem<A> {
    pub label: String,
    pub action: A,
    /// true：选中执行前先进 confirming 二级确认（卸载/删除源等破坏性操作）
    pub dangerous: bool,
    /// 置灰仍可见（如"手动配置的 server 不可升级"）
    pub disabled: bool,
    pub disabled_reason: Option<String>,
}

impl<T, A> ActionMenu<T, A> {
    pub fn new(target: T, items: Vec<ActionMenuItem<A>>) -> Self {
        Self {
            target,
            items,
            selected: 0,
            confirming: None,
        }
    }

    pub fn move_up(&mut self) {
        if self.confirming.is_none() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub fn move_down(&mut self) {
        if self.confirming.is_none() {
            let len = self.items.len();
            self.selected = (self.selected + 1).min(len.saturating_sub(1));
        }
    }

    /// 当前选中项，跳过永远不该越界的空菜单
    pub fn selected_item(&self) -> Option<&ActionMenuItem<A>> {
        self.items.get(self.selected)
    }
}
