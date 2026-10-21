//! 分组（Profile）管理面板：`/model` 命令触发。
//!
//! 从 `app.rs` 拆出。

use super::*;

// ── 分组（Profile）管理面板：/model 命令触发 ──────────────────────────────────

/// 分组面板单个字段的交互形态。
///
/// 与 `SettingsFieldKind` 分开：后者服务 `/config`（只剩 log_level / language 两个
/// 枚举），语义是"Enter/←→ 切换 vs 进入编辑"；这里的形态更细，且**渲染、键位、
/// 校验三处都从它派生**，不再靠散落的 `idx == 5` 之类魔数判断。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileFieldKind {
    /// 接口类型（Anthropic Messages / OpenAI Chat Completions），Enter 或 ←→ 循环
    Provider,
    /// 普通文本：Enter 借用底部输入框
    Text,
    /// 敏感文本：同 Text，但渲染打码
    Secret,
    /// 模型名：Enter 直接拉取服务器模型列表（菜单首项仍是"手动输入"兜底）
    Model,
    /// 固定候选值循环切换；空串表示"不设置，用端点默认"
    Enum(&'static [&'static str]),
    /// 布尔开关
    Bool,
}

/// 字段在哪种线协议下才有意义。
///
/// * `thinking_budget` / `interleaved_thinking` —— Anthropic 线专属：OpenAI
///   Chat Completions 没有 `budget_tokens` 语义。
/// * `thinking_switch` —— OpenAI 线专属：Anthropic 线的开关旋钮是
///   `thinking.budget_tokens`（`thinking.type` 只有 enabled/disabled 两态，
///   且官方端点不接受 disabled），`thinking_switch` 在那边无处落地。
/// * `reasoning_effort` —— **两条线都有**：OpenAI 线发顶层 `reasoning_effort`，
///   Anthropic 线发 `output_config.effort`。它是协议无关的思考投入档位。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileFieldScope {
    Always,
    AnthropicWire,
    OpenAiWire,
}

pub struct ProfileFieldSpec {
    /// i18n label key（同时作为 `text_value` 的分派键，见下方的 key match）
    pub key: &'static str,
    pub kind: ProfileFieldKind,
    pub scope: ProfileFieldScope,
}

pub(super) const fn field(
    key: &'static str,
    kind: ProfileFieldKind,
    scope: ProfileFieldScope,
) -> ProfileFieldSpec {
    ProfileFieldSpec { key, kind, scope }
}

/// `reasoning_effort` 的全档位。真正能上送哪些由 vendor 决定（`ThinkingSpec::
/// effort_levels`），渲染时会再按 vendor 过滤一次——这里只保证值是 wire 可识别的。
pub(super) const REASONING_EFFORT_CHOICES: &[&str] =
    &["low", "medium", "high", "xhigh", "max", "adaptive"];

/// `thinking.type` 候选。`adaptive` 只有 MiniMax M3.1 认，其余 vendor 选它会被
/// adapter 折算成默认形态（不会 400），因此仍列出。
pub(super) const THINKING_SWITCH_CHOICES: &[&str] = &["enabled", "disabled", "adaptive"];

/// `max_tools_per_turn` 候选。用枚举而不是自由文本：该字段一旦写成非法数字，
/// `to_profile` 的 `parse().ok()` 会把它静默吞成 `None`（= 回落端点默认），
/// 用户填了却看不到任何效果。空串由 `enum_choices` 自动补在首项，表示
/// "跟随端点默认"。
pub(super) const MAX_TOOLS_PER_TURN_CHOICES: &[&str] = &["1", "2", "4", "8"];

/// 从模板新建分组时预填的每轮工具数上限。
///
/// 静态目录把所有非 anthropic/openai vendor 一律判成 `max_tools_per_turn = 1`，
/// 而 `None` 会让这个猜测直接生效——新建的国内模型分组于是永远只能一次发一个
/// 工具调用。预填 4 把这个默认值显式写进 profile，用户清空即可回到端点默认。
pub(super) const NEW_PROFILE_MAX_TOOLS_PER_TURN: usize = 4;

/// 分组面板的字段表——**顺序即渲染顺序**，也是唯一真相源。
///
/// 插入/重排字段只需改这张表：`text_value`/`set_text_value`/`display_value` 按
/// `key` 分派（不是按下标），渲染与键位按 `kind` 分派，可见性按 `scope` 过滤。
/// 历史版本把同样的信息抄在 6 个平行数组里，插一个字段要改 8 处且漏一处就是
/// 静默错位（`settings.field.reasoning_effort` 这种 key 漏配也是这么来的）。
pub const PROFILE_FIELD_SPECS: &[ProfileFieldSpec] = &[
    field(
        "settings.field.provider",
        ProfileFieldKind::Provider,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.base_url",
        ProfileFieldKind::Text,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.api_key",
        ProfileFieldKind::Secret,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.model",
        ProfileFieldKind::Model,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.plan_model",
        ProfileFieldKind::Model,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.exec_model",
        ProfileFieldKind::Model,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.aux_model",
        ProfileFieldKind::Model,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.context_window",
        ProfileFieldKind::Text,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.max_tokens",
        ProfileFieldKind::Text,
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.max_tools_per_turn",
        ProfileFieldKind::Enum(MAX_TOOLS_PER_TURN_CHOICES),
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.thinking_budget",
        ProfileFieldKind::Text,
        ProfileFieldScope::AnthropicWire,
    ),
    field(
        "settings.field.interleaved_thinking",
        ProfileFieldKind::Bool,
        ProfileFieldScope::AnthropicWire,
    ),
    field(
        "settings.field.reasoning_effort",
        ProfileFieldKind::Enum(REASONING_EFFORT_CHOICES),
        // **两种线协议都显示**：`reasoning_effort` 现在是协议无关的——
        // OpenAI 线发顶层 `reasoning_effort`，Anthropic 线发 `output_config.effort`
        // （官方 Anthropic 与 MiniMax `/anthropic` 端点同构）。此前它被标成
        // `OpenAiWire`，于是 Anthropic 线下这个字段**根本不显示**，用户无从
        // 配置——那正是「Anthropic 线上 effort 是死配置」的 UI 层根因。
        ProfileFieldScope::Always,
    ),
    field(
        "settings.field.thinking_switch",
        ProfileFieldKind::Enum(THINKING_SWITCH_CHOICES),
        ProfileFieldScope::OpenAiWire,
    ),
];

pub const PROFILE_FIELD_COUNT: usize = PROFILE_FIELD_SPECS.len();

/// 按 key 查字段下标。找不到返回 None（字段表与调用点不同步时据此暴露，
/// 而不是静默落到下标 0 去改 provider）。
pub fn profile_field_idx(key: &str) -> Option<usize> {
    PROFILE_FIELD_SPECS.iter().position(|s| s.key == key)
}

/// api_key 字段下标（渲染打码、onboarding 光标预置共用）
pub fn profile_api_key_field_idx() -> usize {
    profile_field_idx("settings.field.api_key").expect("字段表必须含 api_key")
}

/// 单个分组的编辑草稿
#[derive(Clone, PartialEq)]
pub struct ProfileEntryDraft {
    pub name: String,
    /// 0 = Anthropic, 1 = OpenAI
    pub provider_idx: usize,
    pub model: String,
    pub plan_model: String,
    pub exec_model: String,
    /// 辅助模型（空串 = 不设置，辅助调用走主模型）。只作用于会话标题生成
    /// 与跨会话记忆提取，**不影响上下文压缩**——压缩要复用主对话前缀才能
    /// 命中 prompt cache。
    pub aux_model: String,
    pub base_url: String,
    pub api_key: String,
    /// 设置面板不读取或展示环境变量值，只透传 secret reference 名称。
    pub api_key_env: Option<String>,
    pub max_tokens: String,
    pub context_window: String,
    /// 是否支持图片输入（面板暂不暴露编辑入口，仅透传保留原值）
    pub vision: bool,
    /// Anthropic 协议 extended thinking 预算（空串 = 不设置）。存字符串而非
    /// `Option<u32>`：面板要能表达"用户正在输入的中间态"，且校验与写盘分离。
    pub thinking_budget: String,
    pub interleaved_thinking: bool,
    pub prompt_cache: Option<bool>,
    pub openai_stream_options: Option<bool>,
    /// OpenAI-vendor reasoning_effort 档位（low/medium/high/max/xhigh/adaptive）
    pub reasoning_effort: String,
    /// OpenAI-vendor thinking.type 字符串（enabled/disabled/auto/adaptive）
    pub thinking_switch: String,
    /// 每轮工具数上限。存字符串而非 `Option<usize>`：面板里它是 Enum 字段，
    /// 空串表达「跟随端点默认」，与 `reasoning_effort` / `thinking_switch` 同构。
    /// 漏在这里会被 `to_profile` 静默重置成 None 且无任何报错。
    pub max_tools_per_turn: String,
}

impl ProfileEntryDraft {
    pub(super) fn from_profile(p: &wyj_config::Profile) -> Self {
        Self {
            name: p.name.clone(),
            provider_idx: match p.provider {
                wyj_config::Provider::Anthropic => 0,
                wyj_config::Provider::OpenAI => 1,
            },
            model: p.model.clone(),
            plan_model: p.plan_model.clone().unwrap_or_default(),
            exec_model: p.exec_model.clone().unwrap_or_default(),
            aux_model: p.aux_model.clone().unwrap_or_default(),
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone().unwrap_or_default(),
            api_key_env: p.api_key_env.clone(),
            max_tokens: p.max_tokens.to_string(),
            context_window: p.context_window.to_string(),
            vision: p.vision,
            thinking_budget: p.thinking_budget.map(|b| b.to_string()).unwrap_or_default(),
            interleaved_thinking: p.interleaved_thinking,
            prompt_cache: p.prompt_cache,
            openai_stream_options: p.openai_stream_options,
            reasoning_effort: p.reasoning_effort.clone().unwrap_or_default(),
            thinking_switch: p.thinking_switch.clone().unwrap_or_default(),
            max_tools_per_turn: p
                .max_tools_per_turn
                .map(|n| n.to_string())
                .unwrap_or_default(),
        }
    }

    /// 空白的新分组草稿。
    ///
    /// 此前这里有一整套「内置模板」（GLM / 火山 / MiniMax / Kimi / DeepSeek /
    /// 通义 / Ollama / vLLM / 自定义），新建分组要先选模板、再改名。模板的
    /// 作用其实是**预填端点与供应商特征**（base_url / vendor / wire_protocol /
    /// prompt_cache / openai_stream_options / vision），但这些值全都可以由
    /// 用户自己填 —— 模板反而把「我用的是哪家」固化成了二选一的菜单，
    /// 而真实世界的私有代理根本不在列表里。
    ///
    /// 模板承载的 `vendor` / `wire_protocol` 语义已随配置字段一并移除：
    /// vendor 由 `base_url` + 模型名**推断**（`wyj_api::models::infer_vendor`），
    /// 线协议由 `provider` 唯一决定。所以现在只需要一张空白表单：
    /// 用户填 provider → base_url → api_key → model 即可。
    ///
    /// `provider_idx` 默认 0（Anthropic）——面板首项就是 provider 字段，
    /// 按 Enter 或 ←→ 即可切到 OpenAI。
    pub(super) fn blank(existing_names: &[String]) -> Self {
        // 名字冲突时自动加后缀，避免用户在改名那一步撞上重复名校验。
        let mut name = "new".to_string();
        let mut n = 2;
        while existing_names.iter().any(|e| e == &name) {
            name = format!("new-{n}");
            n += 1;
        }
        Self {
            name,
            provider_idx: 0,
            model: String::new(),
            plan_model: String::new(),
            exec_model: String::new(),
            aux_model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: None,
            max_tokens: "32000".to_string(),
            context_window: "200000".to_string(),
            vision: true,
            thinking_budget: String::new(),
            interleaved_thinking: true,
            prompt_cache: None,
            openai_stream_options: None,
            // 新建分组预填 4：静态目录对国内 vendor 一律判成 1（每轮只能发一个
            // 工具调用），`None` 会让这个按 vendor 的猜测直接生效。显式写入后
            // 用户可在面板里改成其它档位，或清空回到端点默认。
            max_tools_per_turn: NEW_PROFILE_MAX_TOOLS_PER_TURN.to_string(),
            reasoning_effort: String::new(),
            thinking_switch: String::new(),
        }
    }

    pub(super) fn provider(&self) -> wyj_config::Provider {
        if self.provider_idx == 0 {
            wyj_config::Provider::Anthropic
        } else {
            wyj_config::Provider::OpenAI
        }
    }

    /// 按字段表下标读取文本值。`Provider` / `Bool` 字段没有文本形态，返回空串——
    /// 它们各有专用的读写入口（`display_value` / `toggle_bool_field`）。
    pub fn text_value(&self, idx: usize) -> &str {
        let Some(spec) = PROFILE_FIELD_SPECS.get(idx) else {
            return "";
        };
        // 按 key 分派而非按下标：字段表重排时这里无需同步。
        match spec.key {
            "settings.field.model" => &self.model,
            "settings.field.plan_model" => &self.plan_model,
            "settings.field.exec_model" => &self.exec_model,
            "settings.field.aux_model" => &self.aux_model,
            "settings.field.base_url" => &self.base_url,
            "settings.field.api_key" => &self.api_key,
            "settings.field.max_tokens" => &self.max_tokens,
            "settings.field.context_window" => &self.context_window,
            "settings.field.thinking_budget" => &self.thinking_budget,
            "settings.field.reasoning_effort" => &self.reasoning_effort,
            "settings.field.thinking_switch" => &self.thinking_switch,
            "settings.field.max_tools_per_turn" => &self.max_tools_per_turn,
            _ => "",
        }
    }

    pub(super) fn set_text_value(&mut self, idx: usize, value: String) {
        let Some(spec) = PROFILE_FIELD_SPECS.get(idx) else {
            return;
        };
        match spec.key {
            "settings.field.model" => self.model = value,
            "settings.field.plan_model" => self.plan_model = value,
            "settings.field.exec_model" => self.exec_model = value,
            "settings.field.aux_model" => self.aux_model = value,
            "settings.field.base_url" => self.base_url = value,
            "settings.field.api_key" => self.api_key = value,
            "settings.field.max_tokens" => self.max_tokens = value,
            "settings.field.context_window" => self.context_window = value,
            "settings.field.thinking_budget" => self.thinking_budget = value,
            "settings.field.reasoning_effort" => self.reasoning_effort = value,
            "settings.field.thinking_switch" => self.thinking_switch = value,
            "settings.field.max_tools_per_turn" => self.max_tools_per_turn = value,
            _ => {}
        }
    }

    pub(super) fn cycle_provider(&mut self, forward: bool) {
        self.provider_idx = cycle_index(self.provider_idx, 2, forward);
    }

    /// 当前字段的可切换候选值。空切片 = 该字段不是 Enum。
    ///
    /// 首项固定为空串（"不设置，用端点默认"）——配置里 `reasoning_effort` 与
    /// `thinking_switch` 都是 `Option<String>`，必须留出回到 `None` 的路径，
    /// 否则用户一旦点错就再也回不到"跟随端点默认"。
    pub(super) fn enum_choices(&self, idx: usize) -> Vec<&'static str> {
        let Some(spec) = PROFILE_FIELD_SPECS.get(idx) else {
            return Vec::new();
        };
        let ProfileFieldKind::Enum(choices) = spec.kind else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(choices.len() + 1);
        out.push("");
        out.extend_from_slice(choices);
        out
    }

    pub(super) fn cycle_enum_field(&mut self, idx: usize, forward: bool) {
        let choices = self.enum_choices(idx);
        if choices.is_empty() {
            return;
        }
        let current = self.text_value(idx);
        let pos = choices.iter().position(|c| *c == current).unwrap_or(0);
        let next = cycle_index(pos, choices.len(), forward);
        self.set_text_value(idx, choices[next].to_string());
    }

    pub(super) fn toggle_bool_field(&mut self, idx: usize) {
        let Some(spec) = PROFILE_FIELD_SPECS.get(idx) else {
            return;
        };
        if spec.key == "settings.field.interleaved_thinking" {
            self.interleaved_thinking = !self.interleaved_thinking;
        }
    }

    pub fn display_value(&self, idx: usize) -> String {
        let Some(spec) = PROFILE_FIELD_SPECS.get(idx) else {
            return String::new();
        };
        match spec.kind {
            ProfileFieldKind::Provider => {
                if self.provider_idx == 0 {
                    "Anthropic".to_string()
                } else {
                    "OpenAI".to_string()
                }
            }
            ProfileFieldKind::Bool => {
                if self.interleaved_thinking {
                    wyj_i18n::tr("profile.value.on")
                } else {
                    wyj_i18n::tr("profile.value.off")
                }
            }
            ProfileFieldKind::Enum(_) => {
                let raw = self.text_value(idx);
                if raw.is_empty() {
                    // 空串在 Enum 里表达的是"跟随端点默认"，直接显示成空会让人
                    // 以为字段没接上；用显式文案代替。
                    wyj_i18n::tr("profile.value.default")
                } else {
                    raw.to_string()
                }
            }
            _ => self.text_value(idx).to_string(),
        }
    }

    /// 选中模型后按静态目录补全容量字段。
    ///
    /// 只填「空或仍是默认值」的字段：`context_window == 200000` /
    /// `max_tokens == 32000` 是面板新建时的默认值，用户没动过它们才代表
    /// "可以帮我填"；改过的数字不能被静默覆盖。
    pub(super) fn autofill_capacity_from_catalog(&mut self) {
        let Some(cap) = wyj_api::known_model_capacity(&self.model) else {
            return;
        };
        if self.context_window.trim().is_empty() || self.context_window.trim() == "200000" {
            self.context_window = cap.context_window.to_string();
        }
        if self.max_tokens.trim().is_empty() || self.max_tokens.trim() == "32000" {
            self.max_tokens = cap.max_output_tokens.to_string();
        }
    }

    /// 校验数值字段。非法则返回 i18n key。
    pub(super) fn validate(&self) -> Option<&'static str> {
        if self.max_tokens.trim().parse::<u32>().is_err() {
            return Some("settings.error.invalid_max_tokens");
        }
        if self.context_window.trim().parse::<u32>().is_err() {
            return Some("settings.error.invalid_context_window");
        }
        if !self.thinking_budget.trim().is_empty()
            && self.thinking_budget.trim().parse::<u32>().is_err()
        {
            return Some("settings.error.invalid_thinking_budget");
        }
        None
    }

    pub(super) fn to_profile(&self) -> wyj_config::Profile {
        wyj_config::Profile {
            name: self.name.clone(),
            provider: self.provider(),
            model: self.model.clone(),
            plan_model: if self.plan_model.trim().is_empty() {
                None
            } else {
                Some(self.plan_model.clone())
            },
            exec_model: if self.exec_model.trim().is_empty() {
                None
            } else {
                Some(self.exec_model.clone())
            },
            aux_model: if self.aux_model.trim().is_empty() {
                None
            } else {
                Some(self.aux_model.clone())
            },
            base_url: self.base_url.clone(),
            api_key: if self.api_key.trim().is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
            api_key_env: self.api_key_env.clone(),
            max_tokens: self.max_tokens.trim().parse().unwrap_or(32_000),
            context_window: self.context_window.trim().parse().unwrap_or(200_000),
            vision: self.vision,
            thinking_budget: if self.thinking_budget.trim().is_empty() {
                None
            } else {
                self.thinking_budget.trim().parse().ok()
            },
            interleaved_thinking: self.interleaved_thinking,
            prompt_cache: self.prompt_cache,
            openai_stream_options: self.openai_stream_options,
            reasoning_effort: if self.reasoning_effort.trim().is_empty() {
                None
            } else {
                Some(self.reasoning_effort.trim().to_string())
            },
            thinking_switch: if self.thinking_switch.trim().is_empty() {
                None
            } else {
                Some(self.thinking_switch.trim().to_string())
            },
            // 枚举只产出合法数字；仍走 `parse().ok()` 兜底，避免未来改成文本
            // 输入后写盘一个解析不了的字符串（那会静默变成 None）。
            max_tools_per_turn: if self.max_tools_per_turn.trim().is_empty() {
                None
            } else {
                self.max_tools_per_turn.trim().parse().ok()
            },
        }
    }
}

/// ProfileDialog 里当前展示的浮层。重命名/删除确认/模型选择已分别迁移到
/// `InputOwner::Profile` 借用与 `ActionMenu`（危险确认自带 `confirming` 二级
/// 确认、模型选择复用 `dialog.menu`），不再需要专属浮层变体。
pub enum ProfileOverlay {
    None,
    /// 拉取模型列表中（entry_idx 的 field_idx 字段）
    FetchingModels {
        entry_idx: usize,
        field_idx: usize,
    },
    /// Esc 关闭面板时存在未保存修改，三选一确认（保存并关闭/不保存关闭/取消）
    UnsavedChanges {
        selected: usize,
    },
}

/// 分组管理面板状态（/profile 无参命令触发）
pub struct ProfileDialog {
    pub entries: Vec<ProfileEntryDraft>,
    /// 保存后将写入 Config.active_profile 的 entries 下标
    pub active_idx: usize,
    /// 扁平化行游标（entry 头行 + 展开后的字段行统一编号）
    pub cursor: usize,
    /// 当前展开显示字段的 entry 下标
    pub expanded: Option<usize>,
    pub overlay: ProfileOverlay,
    pub error: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Profile(_)` 生效期间使用）
    pub live_input: InputBox,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<ProfileRow, ProfileMenuAction>>,
    /// 拉取到的模型名列表，供 `ProfileMenuAction::ModelChoice(下标)` 按下标取值
    pub pending_models: Vec<String>,
    /// 打开面板时当前会话的临时分组（`None` = 跟随全局）。
    ///
    /// 菜单据此把「仅本会话使用」画成禁用态（已经在用了），并决定要不要显示
    /// 「恢复跟随全局」——面板内的编辑都只是草稿，不能靠 `active_idx` 判断，
    /// 那个字段表达的是「保存后写进 config.toml 的全局默认值」。
    pub session_profile: Option<String>,
    /// 打开面板时的快照，供 Esc 时判断"是否有未保存改动"
    saved_snapshot: (Vec<ProfileEntryDraft>, usize),
}

impl ProfileDialog {
    pub(super) fn new(cfg: &Config, session_profile: Option<String>) -> Self {
        let entries = cfg
            .profiles
            .iter()
            .map(ProfileEntryDraft::from_profile)
            .collect::<Vec<_>>();
        let active_idx = cfg
            .profiles
            .iter()
            .position(|p| p.name == cfg.active_profile)
            .unwrap_or(0);
        let saved_snapshot = (entries.clone(), active_idx);
        Self {
            entries,
            active_idx,
            cursor: 0,
            expanded: None,
            overlay: ProfileOverlay::None,
            error: None,
            live_input: InputBox::new(),
            menu: None,
            pending_models: Vec::new(),
            session_profile,
            saved_snapshot,
        }
    }

    /// 首次启动引导专用构造器:默认展开 active profile,并把光标预置到
    /// `api_key` 字段行(`PROFILE_API_KEY_FIELD_IDX`),用户键入即可开始
    /// 编辑 API Key,无需先按 Enter 展开 entry。其它字段全部可访问,
    /// 但通过顶部 title / hint / api_key 必填校验引导用户聚焦 Key。
    pub fn new_for_onboarding(cfg: &Config) -> Self {
        let mut d = Self::new(cfg, None);
        let active_idx = cfg
            .profiles
            .iter()
            .position(|p| p.name == cfg.active_profile)
            .unwrap_or(0);
        d.expanded = Some(active_idx);
        // rows() = [Header(active_idx), 各可见字段行..., AddNew]
        d.cursor = 1 + profile_api_key_field_idx();
        d.clamp_cursor();
        d
    }

    /// 某分组当前应展示的字段下标（按字段表顺序）。
    ///
    /// `scope` 过滤在这里做，而不是在渲染层——键位处理、光标移动、`rows()`
    /// 三者必须看到同一份可见列表，否则按方向键会跳到看不见的行上去。
    pub fn visible_field_idxs(&self, entry_idx: usize) -> Vec<usize> {
        let Some(entry) = self.entries.get(entry_idx) else {
            return Vec::new();
        };
        let wire = Self::wire_for_provider(entry.provider_idx);
        PROFILE_FIELD_SPECS
            .iter()
            .enumerate()
            .filter(|(_, spec)| match spec.scope {
                ProfileFieldScope::Always => true,
                ProfileFieldScope::AnthropicWire => {
                    matches!(wire, wyj_config::WireProtocol::AnthropicMessages)
                }
                ProfileFieldScope::OpenAiWire => {
                    matches!(wire, wyj_config::WireProtocol::OpenAiChatCompletions)
                }
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// provider 决定的线协议。用户没在配置里显式写 `wire_protocol` 时，
    /// 它总是跟随 provider——面板里也只有 provider 这一个可切换项，
    /// 所以直接用同样的推导规则，不读 `entry.wire_protocol`（那个字段面板
    /// 不编辑，却可能被旧配置写成与 provider 不一致，届时隐藏规则会错乱）。
    pub(super) fn wire_for_provider(provider_idx: usize) -> wyj_config::WireProtocol {
        if provider_idx == 0 {
            wyj_config::WireProtocol::AnthropicMessages
        } else {
            wyj_config::WireProtocol::OpenAiChatCompletions
        }
    }

    /// 是否存在未保存的修改（Esc 关闭前的脏检查）
    pub(super) fn is_dirty(&self) -> bool {
        self.entries != self.saved_snapshot.0 || self.active_idx != self.saved_snapshot.1
    }

    /// 扁平化行列表：entry 头行 + 展开后的字段行 + 末尾固定"+ 新建分组"行。
    pub fn rows(&self) -> Vec<ProfileRow> {
        let mut rows = Vec::new();
        for i in 0..self.entries.len() {
            rows.push(ProfileRow::Header(i));
            if self.expanded == Some(i) {
                for f in self.visible_field_idxs(i) {
                    rows.push(ProfileRow::Field(i, f));
                }
            }
        }
        rows.push(ProfileRow::AddNew);
        rows
    }

    /// 当前游标所在行
    pub(super) fn selected_row(&self) -> ProfileRow {
        self.rows()
            .get(self.cursor)
            .copied()
            .unwrap_or(ProfileRow::AddNew)
    }

    pub(super) fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// 名字非空且唯一性校验，返回冲突/为空的 i18n key
    pub(super) fn validate_names(&self) -> Option<&'static str> {
        for (i, e) in self.entries.iter().enumerate() {
            if e.name.trim().is_empty() {
                return Some("profile.error.empty_name");
            }
            if self.entries[..i].iter().any(|o| o.name == e.name) {
                return Some("profile.error.duplicate_name");
            }
        }
        None
    }

    /// 选中条目回车后弹出的操作菜单。头行 → 展开/收起、设为当前、重命名、删除
    /// 四项；model/plan_model/exec_model 字段行 → 手动编辑、从服务器拉取列表
    /// 两项；其余行（非 model 字段、AddNew）不产生菜单。
    pub fn build_menu(&self) -> Option<ActionMenu<ProfileRow, ProfileMenuAction>> {
        let row = self.selected_row();
        match row {
            ProfileRow::Header(entry_idx) => {
                let is_active = entry_idx == self.active_idx;
                let expanded = self.expanded == Some(entry_idx);
                let can_delete = self.entries.len() > 1 && !is_active;
                // 会话级分组的判据只能看 `session_profile`（打开面板时的真实
                // 生效值），不能看 `active_idx`——后者是"保存后写进全局"的草稿。
                let in_session =
                    self.session_profile.as_deref() == Some(self.entries[entry_idx].name.as_str());
                let has_session_override = self.session_profile.is_some();
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if expanded {
                            "profile.menu.collapse"
                        } else {
                            "profile.menu.expand"
                        }),
                        action: ProfileMenuAction::Header(ProfileHeaderAction::ToggleExpand),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.activate"),
                        action: ProfileMenuAction::Header(ProfileHeaderAction::Activate),
                        dangerous: false,
                        disabled: is_active,
                        disabled_reason: is_active
                            .then(|| wyj_i18n::tr("profile.menu.already_active")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.use_for_session"),
                        action: ProfileMenuAction::Header(ProfileHeaderAction::UseForSession),
                        dangerous: false,
                        disabled: in_session,
                        disabled_reason: in_session
                            .then(|| wyj_i18n::tr("profile.menu.already_session")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.follow_global"),
                        action: ProfileMenuAction::Header(ProfileHeaderAction::FollowGlobal),
                        dangerous: false,
                        disabled: !has_session_override,
                        disabled_reason: (!has_session_override)
                            .then(|| wyj_i18n::tr("profile.menu.no_session_override")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.rename"),
                        action: ProfileMenuAction::Header(ProfileHeaderAction::Rename),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.delete"),
                        action: ProfileMenuAction::Header(ProfileHeaderAction::Delete),
                        dangerous: true,
                        disabled: !can_delete,
                        disabled_reason: (!can_delete).then(|| {
                            wyj_i18n::tr(if self.entries.len() <= 1 {
                                "profile.error.last_one"
                            } else {
                                "profile.error.delete_active"
                            })
                        }),
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            // Model 字段的 Enter 不再经过这里：它直接发起拉取，列表首项再兜底
            // 「手动输入」（见 `profile_start_fetch_models`）。
            _ => None,
        }
    }
}

/// `ProfileDialog`（`/model` 面板）专属的行类型：不同于 Mcp/Skills/Plugins 的纯
/// 扁平列表，这里是"entry 头行 + 展开后最多 `PROFILE_FIELD_COUNT` 个字段行 +
/// 末尾固定 1 个新建行"的两层结构，`FlatRow::Entry(usize)` 只有一个索引装不下
/// entry_idx + field_idx 两个维度，因此单独定义一个平行类型。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileRow {
    Header(usize),
    Field(usize, usize),
    AddNew,
}

/// 分组头行菜单动作（设为当前/重命名/删除 + "展开查看字段"这个原本是裸键
/// Enter 直接触发、现在并入菜单的导航类动作）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileHeaderAction {
    ToggleExpand,
    Activate,
    /// 仅本会话使用：切换本次运行的分组，**不写 `config.toml`**，
    /// 但随会话文件落盘，`--resume` 后继续生效。
    UseForSession,
    /// 取消会话级覆盖，恢复跟随全局默认分组。
    FollowGlobal,
    Rename,
    Delete,
}

/// model/plan_model/exec_model 字段的小菜单动作（原 Ctrl+L 拉取模型列表并入此处）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileFieldAction {
    ManualEdit,
    FetchFromServer,
}

/// 头行菜单 / 字段小菜单 / 拉取到的模型选择菜单，三者互斥，统一到
/// `ProfileDialog.menu` 一个字段（对齐 `McpMenuAction::Installed(..)/Registries(..)`
/// 的既有模式）。`ModelChoice` 只存下标而非模型名字符串——`ActionMenu<T, A>`
/// 要求 `A: Copy`，`String` 不是 `Copy`，真正的字符串另存在
/// `ProfileDialog.pending_models` 里，执行时按下标取值。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProfileMenuAction {
    Header(ProfileHeaderAction),
    Field(ProfileFieldAction),
    ModelChoice(usize),
}

/// `/model` 面板操作菜单里会改变"本次运行用哪套端点"的动作。
///
/// 需要重建 Agent（`rebuild_fn` 只在主循环作用域内可见），所以菜单分发只把
/// 意图返回给调用方，由主循环统一执行——与 `Step::Execute` 里其余纯内存动作
/// 分开，避免把 `rebuild_fn`/`session_store` 一路透传进菜单函数。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProfileSessionAction {
    /// 仅本会话使用该分组（不写 config.toml）。
    Use(String),
    /// 取消会话级覆盖，恢复跟随全局默认分组。
    FollowGlobal,
    /// 草稿已改好（如「设为当前」），请主循环走 `profile_try_save` 落盘。
    /// 菜单函数拿不到 `rebuild_fn`/`session_store`，只能把意图交回主循环——
    /// 与上面两个变体同一条通道。这样「设为当前」选中即保存，用户不再需要
    /// 记住 Ctrl+S。
    Save,
}

/// `/model` 面板操作菜单的按键分发，结构与 `mcp_handle_menu_key` 完全对称。
///
/// 返回 `Some(_)` 表示选中的是「仅本会话使用 / 恢复跟随全局」，调用方必须
/// 执行 [`apply_session_profile`]（菜单层拿不到 `rebuild_fn`）。
pub(super) fn profile_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> Option<ProfileSessionAction> {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(ProfileMenuAction),
        Execute(ProfileMenuAction, ProfileRow),
    }

    let step = {
        let Some(dialog) = &mut state.profile_dialog else {
            return None;
        };
        let Some(menu) = &mut dialog.menu else {
            return None;
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
            if let Some(dialog) = &mut state.profile_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.profile_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.profile_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.profile_dialog {
                dialog.menu = None;
            }
            // 「仅本会话使用 / 恢复跟随全局 / 设为当前后保存」需要重建 Agent 或
            // 落盘，交给主循环执行；其余动作纯改面板内存，就地完成。
            match action {
                ProfileMenuAction::Header(ProfileHeaderAction::UseForSession) => {
                    let ProfileRow::Header(entry_idx) = target else {
                        return None;
                    };
                    let name = state
                        .profile_dialog
                        .as_ref()
                        .and_then(|d| d.entries.get(entry_idx))
                        .map(|e| e.name.clone())?;
                    return Some(ProfileSessionAction::Use(name));
                }
                ProfileMenuAction::Header(ProfileHeaderAction::FollowGlobal) => {
                    return Some(ProfileSessionAction::FollowGlobal);
                }
                ProfileMenuAction::Header(ProfileHeaderAction::Activate) => {
                    // 「设为当前」选完即保存：旧流程只改 active_idx 草稿、还要求
                    // 用户再记一次 Ctrl+S，忘按就静默丢失。这里执行后就把保存
                    // 意图交回主循环走 `profile_try_save`（含名字/Key 校验，失败
                    // 留在面板展示 error）。注意它保存的是全局默认值，与
                    // UseForSession「仅本会话、不写盘」的语义仍是两条路径。
                    profile_execute_menu_action(state, action, target, agent_tx);
                    return Some(ProfileSessionAction::Save);
                }
                other => profile_execute_menu_action(state, other, target, agent_tx),
            }
        }
    }
    None
}

/// 发起一次模型列表拉取。Enter 直达（Model 字段）与菜单项共用这条路径。
pub(super) fn profile_start_fetch_models(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    entry_idx: usize,
    field_idx: usize,
) {
    let Some(entry) = state
        .profile_dialog
        .as_ref()
        .and_then(|d| d.entries.get(entry_idx))
        .cloned()
    else {
        return;
    };
    let api_key = entry.api_key.clone();
    if api_key.trim().is_empty() {
        if let Some(dialog) = &mut state.profile_dialog {
            dialog.error = Some(wyj_i18n::tr("profile.fetch.need_api_key"));
        }
        return;
    }
    let provider = entry.provider();
    let base_url = if entry.base_url.trim().is_empty() {
        match provider {
            wyj_config::Provider::Anthropic => "https://api.anthropic.com".to_string(),
            wyj_config::Provider::OpenAI => "https://api.openai.com/v1".to_string(),
        }
    } else {
        entry.base_url.clone()
    };
    if let Some(dialog) = &mut state.profile_dialog {
        dialog.overlay = ProfileOverlay::FetchingModels {
            entry_idx,
            field_idx,
        };
        dialog.error = None;
    }
    let tx = agent_tx.clone();
    tokio::spawn(async move {
        let result = wyj_api::fetch_model_ids(&provider, &base_url, &api_key)
            .await
            .map_err(|e| e.to_string());
        let _ = tx
            .send(AgentEvent::ModelsFetched {
                entry_idx,
                field_idx,
                result,
            })
            .await;
    });
}

pub(super) fn profile_execute_menu_action(
    state: &mut AppState,
    action: ProfileMenuAction,
    target: ProfileRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    match action {
        ProfileMenuAction::Header(ProfileHeaderAction::ToggleExpand) => {
            let ProfileRow::Header(entry_idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.profile_dialog {
                dialog.expanded = if dialog.expanded == Some(entry_idx) {
                    None
                } else {
                    Some(entry_idx)
                };
                dialog.clamp_cursor();
            }
        }
        ProfileMenuAction::Header(ProfileHeaderAction::Activate) => {
            let ProfileRow::Header(entry_idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.profile_dialog {
                dialog.active_idx = entry_idx;
                dialog.error = None;
            }
        }
        ProfileMenuAction::Header(ProfileHeaderAction::Rename) => {
            let ProfileRow::Header(entry_idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.profile_dialog {
                let name = dialog.entries[entry_idx].name.clone();
                dialog.live_input = InputBox::new();
                dialog.live_input.insert_text(&name);
            }
            state.input_owner = Some(InputOwner::Profile(ProfileInputField::Rename { entry_idx }));
        }
        ProfileMenuAction::Header(ProfileHeaderAction::Delete) => {
            let ProfileRow::Header(entry_idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.profile_dialog {
                dialog.entries.remove(entry_idx);
                if dialog.active_idx > entry_idx {
                    dialog.active_idx -= 1;
                }
                match dialog.expanded {
                    Some(e) if e == entry_idx => dialog.expanded = None,
                    Some(e) if e > entry_idx => dialog.expanded = Some(e - 1),
                    _ => {}
                }
                dialog.clamp_cursor();
            }
        }
        ProfileMenuAction::Field(ProfileFieldAction::ManualEdit) => {
            let ProfileRow::Field(entry_idx, field_idx) = target else {
                return;
            };
            let prefill = state
                .profile_dialog
                .as_ref()
                .map(|d| d.entries[entry_idx].text_value(field_idx).to_string())
                .unwrap_or_default();
            if let Some(dialog) = &mut state.profile_dialog {
                dialog.live_input = InputBox::new();
                dialog.live_input.insert_text(&prefill);
            }
            state.input_owner = Some(InputOwner::Profile(ProfileInputField::Field {
                entry_idx,
                field_idx,
            }));
        }
        ProfileMenuAction::Field(ProfileFieldAction::FetchFromServer) => {
            let ProfileRow::Field(entry_idx, field_idx) = target else {
                return;
            };
            profile_start_fetch_models(state, agent_tx, entry_idx, field_idx);
        }
        ProfileMenuAction::ModelChoice(i) => {
            let ProfileRow::Field(entry_idx, field_idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.profile_dialog {
                if let Some(name) = dialog.pending_models.get(i).cloned() {
                    dialog.entries[entry_idx].set_text_value(field_idx, name);
                    // 选中模型后顺手把容量字段填成目录给出的已知值。只填空的或
                    // 仍是默认值的字段——用户明确改过的数字不能被自动值覆盖。
                    dialog.entries[entry_idx].autofill_capacity_from_catalog();
                }
            }
        }
        // 会话级动作在 `profile_handle_menu_key` 里先行拦截（需要重建 Agent），
        // 不会走到这里。
        ProfileMenuAction::Header(ProfileHeaderAction::UseForSession)
        | ProfileMenuAction::Header(ProfileHeaderAction::FollowGlobal) => {}
    }
}

/// Ctrl+S 与"保存并关闭"菜单选项共用的保存逻辑：校验 → 写盘 → 重建 agent。
/// 返回 `true` 表示已成功保存（调用方据此决定是否关闭面板）。
#[allow(clippy::too_many_arguments)]
pub(super) fn profile_try_save(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    session_store: Option<&Arc<SessionStore>>,
    current_session_id: &str,
) -> bool {
    let mut saved = false;
    if let Some(dialog) = &mut state.profile_dialog {
        if let Some(err_key) = dialog.validate_names() {
            dialog.error = Some(wyj_i18n::tr(err_key));
        } else if let Some((bad_idx, err_key)) = dialog
            .entries
            .iter()
            .enumerate()
            .find_map(|(i, e)| e.validate().map(|k| (i, k)))
        {
            dialog.expanded = Some(bad_idx);
            dialog.clamp_cursor();
            dialog.error = Some(wyj_i18n::tr(err_key));
        } else {
            // 引导场景下的额外校验:如果 active profile 既没有 api_key 也没有
            // api_key_env,要求用户至少填一个,避免写盘后再启动仍进引导循环。
            let active_entry = &dialog.entries[dialog.active_idx];
            if active_entry.api_key.trim().is_empty()
                && active_entry
                    .api_key_env
                    .as_deref()
                    .map_or(true, str::is_empty)
            {
                dialog.expanded = Some(dialog.active_idx);
                dialog.clamp_cursor();
                dialog.error = Some(wyj_i18n::tr("profile.error.api_key_required"));
            } else {
                let mut new_cfg = state.config.clone();
                new_cfg.profiles = dialog.entries.iter().map(|e| e.to_profile()).collect();
                new_cfg.active_profile = dialog.entries[dialog.active_idx].name.clone();
                match new_cfg.save() {
                    Ok(()) => {
                        saved = true;
                        state.config = new_cfg.clone();
                        // 保存的是**全局默认值**；本会话若仍有临时分组，生效值
                        // 依旧由 `effective_config()` 叠加。`dialog.session_profile`
                        // 已经跟着面板内的重命名走过（见 Rename 提交分支），
                        // 这里以它为准；分组被这次保存删掉时覆盖自动失效。
                        if let Some(dialog) = &state.profile_dialog {
                            state.session_profile_override = dialog.session_profile.clone();
                        }
                        if let Some(name) = state.session_profile_override.clone() {
                            if new_cfg.profile_by_name(&name).is_none() {
                                state.session_profile_override = None;
                            }
                        }
                        let effective = state.effective_config();
                        let model_for_mode = effective.model_for_mode(&state.mode).to_string();
                        match rebuild_fn(&effective, &model_for_mode) {
                            Ok(new_agent) => {
                                // rebuild_fn 已装配完整 system prompt，只拼回模式追加段
                                let new_agent = new_agent
                                    .append_system(system_prompt_extra.trim_start().to_string());
                                let new_agent = attach_agent_session(
                                    new_agent,
                                    session_store,
                                    current_session_id,
                                );
                                let new_agent = wire_tool_callback(
                                    new_agent,
                                    agent_tx.clone(),
                                    todo_store.clone(),
                                );
                                *shared_agent.write().unwrap() = Arc::new(new_agent);
                                state.model_name = model_for_mode;
                                state.context_window = effective.active_profile().context_window;
                                if let Some(dialog) = &mut state.profile_dialog {
                                    dialog.session_profile = state.session_profile_override.clone();
                                }
                                state
                                    .messages
                                    .push(ChatMessage::system(wyj_i18n::tr("profile.saved")));
                            }
                            Err(e) => {
                                state
                                    .messages
                                    .push(ChatMessage::assistant_err(wyj_i18n::tr_fmt(
                                        "settings.rebuild_failed",
                                        &[("err", &e.to_string())],
                                    )));
                            }
                        }
                    }
                    Err(e) => {
                        dialog.error = Some(wyj_i18n::tr_fmt(
                            "settings.save_failed",
                            &[("err", &e.to_string())],
                        ));
                    }
                }
            }
        }
    }
    saved
}

#[cfg(test)]
mod profile_dialog_onboarding_tests {
    use super::*;

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

    /// `ProfileEntryDraft::to_profile` 是逐字段**重建**一个新 Profile，不是原地改字段。
    /// 任何只加在 `wyj_config::Profile` 上、却没在 draft 上透传的字段，都会在用户
    /// 每次 Ctrl+S 保存 ProfileDialog 时被静默重置成默认值，且没有任何报错。
    /// 与 CLAUDE.md 记过的 rebuild_fn 快照陷阱同类，这里用往返测试钉死。
    #[test]
    fn profile_dialog_round_trip_preserves_max_tools_per_turn() {
        for requested in [None, Some(1usize), Some(4), Some(8)] {
            let profile = wyj_config::Profile {
                name: "default".to_string(),
                max_tools_per_turn: requested,
                ..wyj_config::Profile::default()
            };
            let restored = ProfileEntryDraft::from_profile(&profile).to_profile();
            assert_eq!(
                restored.max_tools_per_turn, requested,
                "max_tools_per_turn 在 ProfileDialog 往返中丢失: {requested:?} → {:?}",
                restored.max_tools_per_turn
            );
        }
    }

    /// 辅助模型在 ProfileDialog 往返中必须原样保留。
    ///
    /// `to_profile` 是逐字段**重建**一个新 Profile，任何只加在 `Profile` 上
    /// 却没在 draft 上透传的字段，都会在用户每次 Ctrl+S 保存时被静默重置成
    /// 默认值（= 辅助模型消失，且没有任何报错）。
    #[test]
    fn profile_dialog_round_trip_preserves_aux_model() {
        for requested in [None, Some("MiniMax-M3.1-Flash-Preview".to_string())] {
            let profile = wyj_config::Profile {
                name: "default".to_string(),
                aux_model: requested.clone(),
                ..wyj_config::Profile::default()
            };
            let restored = ProfileEntryDraft::from_profile(&profile).to_profile();
            assert_eq!(
                restored.aux_model, requested,
                "aux_model 在 ProfileDialog 往返中丢失: {requested:?} → {:?}",
                restored.aux_model
            );
        }
    }

    /// 新建分组必须预填 4，否则 `None` 会让静态目录按 vendor 猜出的
    /// `max_tools_per_turn = 1` 直接生效——国内模型分组因此永远只能一轮发一个
    /// 工具调用，派生 subagent 也就一次只能起一个。
    #[test]
    fn new_profile_prefills_four_tools_per_turn() {
        let draft = ProfileEntryDraft::blank(&[]);
        assert_eq!(
            draft.to_profile().max_tools_per_turn,
            Some(4),
            "新建的分组应预填 4"
        );
    }

    /// 新建分组的草稿必须是**空白**的：除了有明确理由的默认值
    /// （容量、并行工具数、thinking 交错），其余字段一律留空——
    /// 端点、Key、模型名都要用户自己填。
    #[test]
    fn new_profile_starts_blank() {
        let draft = ProfileEntryDraft::blank(&[]);
        assert!(draft.base_url.is_empty(), "端点应留空由用户填");
        assert!(draft.api_key.is_empty(), "Key 应留空由用户填");
        assert!(draft.model.is_empty(), "模型名应留空由用户填");
        assert!(draft.reasoning_effort.is_empty(), "思考档位应留空跟随端点");
        assert!(draft.thinking_switch.is_empty(), "思考开关应留空跟随端点");
        // provider 默认 Anthropic，面板首项就是 provider 字段，可现场切换
        assert_eq!(draft.provider_idx, 0);
    }

    /// 名字冲突时自动加后缀，避免用户在改名那一步撞上重复名校验。
    #[test]
    fn new_profile_name_avoids_collisions() {
        let existing = vec!["new".to_string(), "new-2".to_string()];
        let draft = ProfileEntryDraft::blank(&existing);
        assert_eq!(draft.name, "new-3");
    }

    /// 该字段是枚举：候选首项为空串（回落端点默认），其余为合法数字，
    /// 循环一圈必须回到起点，且清空后写盘是 `None` 而不是 0。
    #[test]
    fn max_tools_per_turn_enum_cycles_and_clears_to_none() {
        let mut draft = ProfileEntryDraft::blank(&[]);
        let idx = super::profile_field_idx("settings.field.max_tools_per_turn").unwrap();
        assert_eq!(
            draft.enum_choices(idx),
            vec!["", "1", "2", "4", "8"],
            "枚举候选必须包含「未设置」首项"
        );

        let start = draft.text_value(idx).to_string();
        for _ in 0..draft.enum_choices(idx).len() {
            draft.cycle_enum_field(idx, true);
        }
        assert_eq!(draft.text_value(idx), start, "循环一圈未回到起点");

        draft.set_text_value(idx, String::new());
        assert_eq!(draft.to_profile().max_tools_per_turn, None);
    }

    #[test]
    fn new_for_onboarding_focuses_on_api_key_field() {
        let cfg = Config {
            active_profile: "default".to_string(),
            ..Config::default()
        };
        let d = ProfileDialog::new_for_onboarding(&cfg);
        assert_eq!(d.expanded, Some(0), "默认展开 active profile(0)");
        // Header(active_idx) + 各可见字段行；api_key 必须恰好落在这一行
        let api_key_idx = super::profile_api_key_field_idx();
        assert_eq!(d.cursor, 1 + api_key_idx, "光标应预置到 api_key 字段");
        // 该行确实指向 api_key
        let rows = d.rows();
        if let Some(crate::app::ProfileRow::Field(entry, field)) = rows.get(d.cursor) {
            assert_eq!(*entry, 0);
            assert_eq!(*field, api_key_idx);
        } else {
            panic!("cursor 指向的行不是 Field(active_idx, api_key)");
        }
    }

    #[test]
    fn new_for_onboarding_uses_active_profile_index_for_multi_profile_cfg() {
        let mut profiles = Config::default().profiles;
        profiles.push(wyj_config::Profile {
            name: "extra".to_string(),
            ..wyj_config::Profile::default()
        });
        let cfg = Config {
            active_profile: "extra".to_string(),
            profiles,
            ..Config::default()
        };
        let d = ProfileDialog::new_for_onboarding(&cfg);
        let active_idx = cfg
            .profiles
            .iter()
            .position(|p| p.name == cfg.active_profile)
            .unwrap_or(0);
        assert_eq!(d.expanded, Some(active_idx));
    }

    /// 「仅本会话使用」必须与「设为当前」严格区分：前者只看打开面板时
    /// `session_profile` 的真实生效值，后者看 `active_idx`（保存后写进全局
    /// 的草稿）。两者混用会出现「本会话在用 A，菜单却把 B 标成已启用」。
    #[test]
    fn session_profile_menu_marks_only_the_effective_session_profile() {
        let mut profiles = Config::default().profiles;
        for name in ["alpha", "beta"] {
            profiles.push(wyj_config::Profile {
                name: name.to_string(),
                ..wyj_config::Profile::default()
            });
        }
        let cfg = Config {
            active_profile: "alpha".to_string(),
            profiles,
            ..Config::default()
        };
        let mut dialog = ProfileDialog::new(&cfg, Some("beta".to_string()));

        fn use_item(
            dialog: &mut ProfileDialog,
            entry_idx: usize,
        ) -> ActionMenuItem<ProfileMenuAction> {
            dialog.cursor = entry_idx; // expanded = None 时 Header(i) 就是第 i 行
            dialog
                .build_menu()
                .into_iter()
                .flat_map(|menu| menu.items)
                .find(|item| {
                    matches!(
                        item.action,
                        ProfileMenuAction::Header(ProfileHeaderAction::UseForSession)
                    )
                })
                .expect("头行菜单必须有「仅本会话使用」")
        }

        let alpha_idx = cfg.profiles.iter().position(|p| p.name == "alpha").unwrap();
        let beta_idx = cfg.profiles.iter().position(|p| p.name == "beta").unwrap();
        // alpha 是全局默认值（active_idx），但本会话实际在用 beta：
        // 「仅本会话使用」对 alpha 必须可用，对 beta 必须禁用。
        assert!(!use_item(&mut dialog, alpha_idx).disabled);
        assert!(use_item(&mut dialog, beta_idx).disabled);

        // 跟随全局：本会话有覆盖时可用，没有时禁用。
        let follow_item = |d: &ProfileDialog| {
            let mut d = ProfileDialog {
                entries: d.entries.clone(),
                active_idx: d.active_idx,
                cursor: alpha_idx,
                expanded: None,
                overlay: ProfileOverlay::None,
                error: None,
                live_input: InputBox::new(),
                menu: None,
                pending_models: Vec::new(),
                session_profile: d.session_profile.clone(),
                saved_snapshot: (d.entries.clone(), d.active_idx),
            };
            d.cursor = alpha_idx;
            d.build_menu()
                .into_iter()
                .flat_map(|menu| menu.items)
                .find(|item| {
                    matches!(
                        item.action,
                        ProfileMenuAction::Header(ProfileHeaderAction::FollowGlobal)
                    )
                })
                .expect("头行菜单必须有「恢复跟随全局」")
        };
        assert!(!follow_item(&dialog).disabled);
        let no_override = ProfileDialog::new(&cfg, None);
        assert!(follow_item(&no_override).disabled);
    }

    /// 会话级覆盖只影响生效配置，绝不改动 `config.active_profile`（全局默认值）。
    #[test]
    fn effective_config_applies_override_without_touching_global_default() {
        let mut profiles = Config::default().profiles;
        profiles.push(wyj_config::Profile {
            name: "beta".to_string(),
            model: "beta-model".to_string(),
            ..wyj_config::Profile::default()
        });
        let cfg = Config {
            active_profile: "default".to_string(),
            profiles,
            ..Config::default()
        };
        let mut state = AppState::new(
            PathBuf::from("/tmp"),
            "default-model".to_string(),
            200_000,
            AgentMode::Auto,
            cfg,
            Arc::new(wyj_tools::SubAgentHub::new()),
        );
        state.session_profile_override = Some("beta".to_string());

        assert_eq!(state.effective_config().active_profile, "beta");
        assert_eq!(
            state.effective_config().active_profile().model,
            "beta-model"
        );
        // 全局默认值原封不动 —— 这是「不写盘」的实现保证。
        assert_eq!(state.config.active_profile, "default");
    }

    /// 分组被改名/删除后，残留的覆盖必须静默失效（回落全局），
    /// 而不是让 `build_provider` 在下一轮直接失败。
    #[test]
    fn stale_session_profile_override_falls_back_to_global() {
        let mut state = AppState::new(
            PathBuf::from("/tmp"),
            "m".to_string(),
            200_000,
            AgentMode::Auto,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        );
        state.session_profile_override = Some("deleted-profile".to_string());
        assert_eq!(state.effective_config().active_profile, "default");
        assert!(state.session_profile_active().is_none());
    }

    // ── 字段表不变量（声明式重构后只在这一处钉死） ──────────────────────────

    /// 每个字段的 i18n key 必须真的能解析。
    ///
    /// 回归点：`settings.field.reasoning_effort` / `thinking_switch` 曾只被字段表
    /// 引用、locale 里根本没有条目，rust-i18n 缺 key 时**原样回显 key**，面板上
    /// 于是出现了 `settings.field.reasoning_effort` 这串字面量。缺翻译不会 panic、
    /// 不会告警，只能靠这条断言兜住。
    #[test]
    fn every_profile_field_label_key_resolves() {
        for spec in super::PROFILE_FIELD_SPECS {
            let label = wyj_i18n::tr(spec.key);
            assert_ne!(
                label, spec.key,
                "字段 {} 的 i18n label 缺失（渲染成字面量）",
                spec.key
            );
            assert!(!label.trim().is_empty(), "字段 {} 的 label 为空", spec.key);
        }
    }

    /// 字段表里每个 key 都要在 `text_value` / `set_text_value` 里有对应分支，
    /// 否则该字段在面板里永远是空串、且编辑结果被静默丢弃。
    #[test]
    fn every_profile_field_round_trips_through_text_accessors() {
        for (idx, spec) in super::PROFILE_FIELD_SPECS.iter().enumerate() {
            let is_text = matches!(
                spec.kind,
                super::ProfileFieldKind::Text
                    | super::ProfileFieldKind::Secret
                    | super::ProfileFieldKind::Model
                    | super::ProfileFieldKind::Enum(_)
            );
            if !is_text {
                continue;
            }
            let mut draft = super::ProfileEntryDraft::blank(&[]);
            draft.set_text_value(idx, "probe-value".to_string());
            assert_eq!(
                draft.text_value(idx),
                "probe-value",
                "字段 {} 的读写分支不一致",
                spec.key
            );
        }
    }

    /// 顺序即渲染顺序：接口类型 → 连接 → 模型 → 容量 → 思考。
    ///
    /// 这条断言把「先选 API 类型，再填 base_url/model，再填容量，最后是思考」
    /// 的交互决策钉在测试里，避免下次重排时无意改回旧顺序。
    #[test]
    fn profile_field_order_follows_the_setup_flow() {
        let keys: Vec<&str> = super::PROFILE_FIELD_SPECS.iter().map(|s| s.key).collect();
        let pos = |k: &str| keys.iter().position(|x| *x == k).unwrap();
        assert!(
            pos("settings.field.provider") < pos("settings.field.base_url"),
            "接口类型必须在 base_url 之前: {keys:?}"
        );
        assert!(
            pos("settings.field.base_url") < pos("settings.field.model"),
            "base_url 必须在 model 之前: {keys:?}"
        );
        assert!(
            pos("settings.field.model") < pos("settings.field.context_window"),
            "model 必须在容量字段之前: {keys:?}"
        );
        assert!(
            pos("settings.field.context_window") < pos("settings.field.reasoning_effort"),
            "容量字段必须在思考字段之前: {keys:?}"
        );
    }

    /// 辅助模型：面板里留空 → 写盘为 `None`（辅助调用走主模型）；
    /// 填了值 → 原样写进 profile。
    ///
    /// 空串必须落成 `None` 而不是 `Some("")`——后者会让 `build_aux_provider`
    /// 拿到一个空模型名、构造出一个永远失败的 provider。
    #[test]
    fn aux_model_is_written_back_as_none_when_left_blank() {
        let mut draft = super::ProfileEntryDraft::blank(&[]);
        let idx = super::profile_field_idx("settings.field.aux_model").expect("字段表应含该项");

        // 模板新建的分组没有辅助模型 → 空串 → None
        assert_eq!(draft.text_value(idx), "");
        assert_eq!(draft.to_profile().aux_model, None);

        // 填了值 → 原样带出去
        draft.set_text_value(idx, "MiniMax-M3.1-Flash-Preview".to_string());
        assert_eq!(
            draft.to_profile().aux_model.as_deref(),
            Some("MiniMax-M3.1-Flash-Preview")
        );
    }

    /// 辅助模型是**分组级**字段，不是模式级：它不分 Plan / Exec，两种模式下
    /// 指向的都是同一个模型。面板里它属于常规字段（scope = Always）。
    #[test]
    fn aux_model_is_always_visible_regardless_of_wire_protocol() {
        let spec = super::PROFILE_FIELD_SPECS
            .iter()
            .find(|s| s.key == "settings.field.aux_model")
            .expect("字段表应含该项");
        assert!(
            matches!(spec.scope, super::ProfileFieldScope::Always),
            "辅助模型不受线协议限制"
        );
    }

    /// 思考字段按线协议分流：Anthropic 线隐藏 switch，OpenAI 线隐藏
    /// budget/interleaved。
    ///
    /// **`reasoning_effort` 两条线都显示**——它是协议无关的思考投入档位
    /// （OpenAI 线发顶层 `reasoning_effort`，Anthropic 线发
    /// `output_config.effort`）。此前它被标成 `OpenAiWire`，Anthropic 线下
    /// **根本不显示**：用户没法配，配了也不生效——这是「Anthropic 线上 effort
    /// 是死配置」在 UI 层的根因。
    #[test]
    fn thinking_fields_are_scoped_to_their_wire_protocol() {
        let cfg = Config::default();
        let mut dialog = super::ProfileDialog::new(&cfg, None);
        let anthropic_idx = super::profile_field_idx("settings.field.thinking_budget").unwrap();
        let effort_idx = super::profile_field_idx("settings.field.reasoning_effort").unwrap();
        let switch_idx = super::profile_field_idx("settings.field.thinking_switch").unwrap();
        let interleaved_idx =
            super::profile_field_idx("settings.field.interleaved_thinking").unwrap();

        let entry = 0;
        dialog.entries[entry].provider_idx = 0; // Anthropic
        let visible = dialog.visible_field_idxs(entry);
        assert!(
            visible.contains(&anthropic_idx),
            "Anthropic 线应显示思考预算"
        );
        assert!(
            visible.contains(&interleaved_idx),
            "Anthropic 线应显示交错思考"
        );
        assert!(
            visible.contains(&effort_idx),
            "Anthropic 线应显示思考强度（output_config.effort）"
        );
        assert!(
            !visible.contains(&switch_idx),
            "Anthropic 线不应显示 thinking switch（那边的旋钮是 budget）"
        );

        dialog.entries[entry].provider_idx = 1; // OpenAI
        let visible = dialog.visible_field_idxs(entry);
        assert!(visible.contains(&effort_idx), "OpenAI 线应显示思考强度");
        assert!(visible.contains(&switch_idx), "OpenAI 线应显示思考开关");
        assert!(
            !visible.contains(&anthropic_idx),
            "OpenAI 线不应显示思考预算"
        );
        assert!(
            !visible.contains(&interleaved_idx),
            "OpenAI 线不应显示交错思考"
        );

        // 与 provider 无关的字段在两种线协议下都要在。
        for key in [
            "settings.field.provider",
            "settings.field.base_url",
            "settings.field.api_key",
            "settings.field.model",
            "settings.field.context_window",
            "settings.field.max_tokens",
            "settings.field.max_tools_per_turn",
        ] {
            let idx = super::profile_field_idx(key).unwrap();
            assert!(
                dialog.visible_field_idxs(entry).contains(&idx),
                "{key} 在 OpenAI 线下应可见"
            );
        }
    }

    /// 枚举字段的候选必须首项为空串（回到 `None` 的路径），且循环能回到起点。
    ///
    /// 没有空串这一项时，用户一旦把 `thinking_switch` 从「默认」点成 `enabled`，
    /// 就再也回不到"跟随端点默认"——配置里该字段是 `Option<String>`，
    /// 空串是唯一能表达 `None` 的形态。
    #[test]
    fn enum_fields_keep_a_way_back_to_the_unset_value() {
        let mut draft = super::ProfileEntryDraft::blank(&[]);
        let idx = super::profile_field_idx("settings.field.reasoning_effort").unwrap();
        let choices = draft.enum_choices(idx);
        assert_eq!(
            choices.first(),
            Some(&""),
            "首项必须是「未设置」: {choices:?}"
        );
        assert!(choices.len() >= 3, "至少要有若干档位: {choices:?}");

        // 从空开始逐步前进，走满一圈必须回到空。
        let start = draft.text_value(idx).to_string();
        for _ in 0..choices.len() {
            draft.cycle_enum_field(idx, true);
        }
        assert_eq!(draft.text_value(idx), start, "循环一圈未回到起点");
    }

    /// thinking_budget 的草稿是字符串，写盘时必须正确映射回 `Option<u32>`。
    #[test]
    fn thinking_budget_draft_maps_to_option_u32() {
        let mut draft = super::ProfileEntryDraft::blank(&[]);
        let idx = super::profile_field_idx("settings.field.thinking_budget").unwrap();

        draft.set_text_value(idx, String::new());
        assert_eq!(draft.to_profile().thinking_budget, None, "空串应写回 None");

        draft.set_text_value(idx, "8000".to_string());
        assert_eq!(draft.to_profile().thinking_budget, Some(8_000));

        draft.set_text_value(idx, "abc".to_string());
        assert_eq!(
            draft.validate(),
            Some("settings.error.invalid_thinking_budget"),
            "非数字必须被校验拦下，而不是静默写回 None"
        );
    }

    /// 两个分组的配置：验证「切到别的分组」必须先有一个可切的。
    fn two_profile_config() -> Config {
        let mut cfg = Config::default();
        cfg.profiles.push(wyj_config::Profile {
            name: "beta".to_string(),
            ..wyj_config::Profile::default()
        });
        cfg
    }

    /// 菜单「设为当前」必须**同时**改草稿并交回保存意图（`Save`）。
    ///
    /// 旧行为只改 `active_idx` 草稿、还要用户自己记得按 Ctrl+S，忘按就是
    /// 静默丢失——这正是 /model 面板最常见的误操作。
    #[test]
    fn activate_menu_item_asks_the_main_loop_to_save() {
        // 两个分组才能验证「切换到别的分组后保存」——默认配置只有一个。
        let cfg = two_profile_config();
        let mut state = make_state();
        state.config = cfg.clone();
        state.profile_dialog = Some(super::ProfileDialog::new(&cfg, None));
        // 光标落到第 2 个分组的头行，并打开它的操作菜单。
        let dialog = state.profile_dialog.as_mut().unwrap();
        dialog.cursor = 1;
        dialog.menu = dialog.build_menu();

        let menu_len = state
            .profile_dialog
            .as_ref()
            .and_then(|d| d.menu.as_ref())
            .map(|m| m.items.len())
            .unwrap_or(0);
        assert!(menu_len > 0, "头行应能生成操作菜单");

        // 移到「设为当前」那一项并确认。
        let activate_pos = state
            .profile_dialog
            .as_ref()
            .and_then(|d| d.menu.as_ref())
            .and_then(|m| {
                m.items.iter().position(|it| {
                    it.action
                        == super::ProfileMenuAction::Header(super::ProfileHeaderAction::Activate)
                })
            })
            .unwrap();
        if let Some(m) = state.profile_dialog.as_mut().and_then(|d| d.menu.as_mut()) {
            m.selected = activate_pos;
        }

        let tx = tokio::sync::mpsc::channel(16).0;
        let action = super::profile_handle_menu_key(&mut state, KeyCode::Enter, &tx);

        assert_eq!(
            action,
            Some(super::ProfileSessionAction::Save),
            "「设为当前」应交回保存意图，而不是留草稿等 Ctrl+S"
        );
        assert_eq!(
            state.profile_dialog.as_ref().map(|d| d.active_idx),
            Some(1),
            "草稿里的当前分组应已切到第 2 个"
        );
    }

    /// 对照项：同一菜单里的「仅本会话使用」仍走会话通道，绝不能混进 `Save`
    /// ——它**不写 config.toml**，合并两条路径会把临时分组固化成全局默认。
    #[test]
    fn use_for_session_stays_off_the_save_channel() {
        let cfg = two_profile_config();
        let mut state = make_state();
        state.config = cfg.clone();
        state.profile_dialog = Some(super::ProfileDialog::new(&cfg, None));
        let dialog = state.profile_dialog.as_mut().unwrap();
        dialog.cursor = 0;
        dialog.menu = dialog.build_menu();

        let session_pos = state
            .profile_dialog
            .as_ref()
            .and_then(|d| d.menu.as_ref())
            .and_then(|m| {
                m.items.iter().position(|it| {
                    it.action
                        == super::ProfileMenuAction::Header(
                            super::ProfileHeaderAction::UseForSession,
                        )
                })
            })
            .unwrap();
        if let Some(m) = state.profile_dialog.as_mut().and_then(|d| d.menu.as_mut()) {
            m.selected = session_pos;
        }

        let tx = tokio::sync::mpsc::channel(16).0;
        let action = super::profile_handle_menu_key(&mut state, KeyCode::Enter, &tx);

        assert_eq!(
            action,
            Some(super::ProfileSessionAction::Use(cfg.active_profile.clone())),
            "会话级切换必须走自己的通道"
        );
    }

    // ── 切换成功后必须收起 /model 面板 ─────────────────────────────────────

    fn rebuild_ok() -> super::RebuildFn {
        // 占位 Provider：测试只关心 rebuild 成功这条分支，真实请求路径不会被
        // 触发（`MissingKeyProvider` 每次调用都返回缺 Key 错误）。
        Arc::new(|_cfg: &Config, _model: &str| {
            Ok(Agent::new(Arc::new(wyj_api::MissingKeyProvider)))
        })
    }

    fn rebuild_failing() -> super::RebuildFn {
        Arc::new(|_cfg: &Config, _model: &str| anyhow::bail!("boom"))
    }

    fn session_wiring() -> (
        Arc<tokio::sync::mpsc::Sender<AgentEvent>>,
        Arc<std::sync::Mutex<wyj_tools::TodoStore>>,
    ) {
        (
            Arc::new(tokio::sync::mpsc::channel(16).0),
            Arc::new(std::sync::Mutex::new(wyj_tools::TodoStore {
                items: vec![],
            })),
        )
    }

    fn agent_and_session() -> (Arc<std::sync::RwLock<Arc<Agent>>>, Arc<Mutex<Session>>) {
        (
            Arc::new(std::sync::RwLock::new(Arc::new(Agent::new(Arc::new(
                wyj_api::MissingKeyProvider,
            ))))),
            Arc::new(Mutex::new(Session::new())),
        )
    }

    fn switch(state: &mut AppState, name: &str, rebuild: &super::RebuildFn) -> bool {
        let (tx, todos) = session_wiring();
        let (agent, session) = agent_and_session();
        super::apply_session_profile(
            state,
            super::ProfileSessionAction::Use(name.to_string()),
            &tx,
            rebuild,
            "",
            &todos,
            &agent,
            None,
            "sid",
            &session,
        )
    }

    /// 「仅本会话使用」执行后面板必须消失。
    ///
    /// 回归：三条「立刻改变生效端点」的出口里只有「设为当前」在成功后关面板，
    /// 会话级两条漏了。面板的渲染条件（`render.rs`）与按键拦截条件（主循环）
    /// 都挂在 `state.profile_dialog.is_some()` 上，于是切换成功后留下一个
    /// 没有待办却继续吞掉全部按键的浮层——用户看着输入框光标在闪，却打不进
    /// 字，只能靠 Esc 才发现面板还在。
    #[test]
    fn use_for_session_closes_the_profile_dialog() {
        let cfg = two_profile_config();
        let mut state = make_state();
        state.config = cfg.clone();
        state.profile_dialog = Some(super::ProfileDialog::new(&cfg, None));

        assert!(switch(&mut state, "beta", &rebuild_ok()), "重建应当成功");
        super::finish_session_profile_switch(&mut state);

        assert_eq!(
            state.session_profile_override.as_deref(),
            Some("beta"),
            "本会话生效的分组应当已经写进 AppState"
        );
        assert!(
            state.profile_dialog.is_none(),
            "切换成功后 /model 面板必须收起，否则它继续吞掉全部按键"
        );
    }

    /// 面板里有未保存的字段草稿时不能顺手关掉：会话级切换刻意不写盘，
    /// 直接关等于静默丢掉用户刚敲进去的 Key。
    #[test]
    fn a_session_switch_never_discards_unsaved_field_edits() {
        let cfg = two_profile_config();
        let mut state = make_state();
        state.config = cfg.clone();
        state.profile_dialog = Some(super::ProfileDialog::new(&cfg, None));
        let api_key_idx = super::profile_field_idx("settings.field.api_key").unwrap();
        state.profile_dialog.as_mut().unwrap().entries[0]
            .set_text_value(api_key_idx, "sk-half-typed".to_string());

        assert!(switch(&mut state, "beta", &rebuild_ok()));
        super::finish_session_profile_switch(&mut state);

        assert_eq!(
            state
                .profile_dialog
                .as_ref()
                .map(|d| d.entries[0].text_value(api_key_idx)),
            Some("sk-half-typed"),
            "面板必须留下，用户才有地方按 Ctrl+S 保住这次编辑"
        );
    }

    /// 切换失败时保持面板：用户得留在面板里才看得到 `profile.switch_failed`。
    #[test]
    fn a_failed_switch_keeps_the_panel_open() {
        let cfg = two_profile_config();
        let mut state = make_state();
        state.config = cfg.clone();
        state.profile_dialog = Some(super::ProfileDialog::new(&cfg, None));

        assert!(
            !switch(&mut state, "beta", &rebuild_failing()),
            "重建失败不能宣称切换成功，否则调用方会关掉面板"
        );
        assert_eq!(state.session_profile_override, None, "失败时不得改生效值");
        assert!(state.profile_dialog.is_some());
    }

    /// 会话切换后同一个分组会走 `adopt_session_profile` 的早退分支（不重建
    /// Agent），面板同样必须收起——它的 entries 是打开那一刻的 config 快照。
    #[test]
    fn switching_sessions_closes_a_stale_profile_panel() {
        let cfg = two_profile_config();
        let mut state = make_state();
        state.config = cfg.clone();
        state.session_profile_override = Some("beta".to_string());
        state.profile_dialog = Some(super::ProfileDialog::new(&cfg, None));

        let (tx, todos) = session_wiring();
        let (agent, session) = agent_and_session();
        super::adopt_session_profile(
            &mut state,
            Some("beta".to_string()),
            &tx,
            &rebuild_failing(),
            "",
            &todos,
            &agent,
            None,
            "sid",
            &session,
        );

        assert!(
            state.profile_dialog.is_none(),
            "换会话后面板里的分组快照已过期，必须收起"
        );
    }

    /// 选中模型后，容量字段只在「用户没动过」时被自动填。
    #[test]
    fn catalog_autofill_only_overwrites_untouched_defaults() {
        let mut draft = super::ProfileEntryDraft::blank(&[]);
        draft.context_window = "200000".to_string();
        draft.max_tokens = "32000".to_string();
        draft.model = "claude-sonnet-4-5".to_string();
        draft.autofill_capacity_from_catalog();
        assert_eq!(draft.context_window, "200000");
        assert_eq!(draft.max_tokens, "32000");

        // 用户手改过的值不能被覆盖。
        draft.context_window = "100000".to_string();
        draft.max_tokens = "4096".to_string();
        draft.autofill_capacity_from_catalog();
        assert_eq!(draft.context_window, "100000");
        assert_eq!(draft.max_tokens, "4096");

        // 命中未知模型时保持原样。
        draft.context_window = "200000".to_string();
        draft.max_tokens = "32000".to_string();
        draft.model = "totally-unknown-model".to_string();
        draft.autofill_capacity_from_catalog();
        assert_eq!(draft.context_window, "200000");
        assert_eq!(draft.max_tokens, "32000");
    }
}
