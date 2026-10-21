//! `/effort` 面板：思考强度档位选择。
//!
//! ## 档位列表按端点能力过滤
//!
//! 面板只列**当前模型真正支持**的档位（`wyj_api::supported_effort_levels`）：
//! Claude 有五档、DeepSeek 有四档、MiniMax M3 只有 low/high/max、GLM 干脆
//! 根本不认 effort（只认开关）。列出端点不认的档位只会被静默丢弃，
//! 用户还以为设上了。
//!
//! ## 作用域：随会话落盘
//!
//! 与 `/model`（只存内存）相反：思考档位**随会话文件落盘**，
//! `--resume` 后继续生效。理由是档位属于「这次对话要想多深」的工作
//! 状态，不该每次恢复会话都要重设一遍。但它**不写 `config.toml`**——
//! 档位不是「这个分组是什么端点」的一部分。
//!
//! ## `off` 的语义
//!
//! `off` = `reasoning_effort` 清空 + `thinking_switch` 置 `disabled`
//! ——**彻底关思考**，而不只是「降到最低档」。`thinking_budget` 不动：
//! 官方 Anthropic 上 budget 与 effort 控制的是不同维度，可以共存。

use super::*;

/// 思考强度面板状态（`/effort` 无参命令触发时 Some）。
pub struct EffortPickerDialog {
    /// 可选档位（按强弱阶梯排序，来自 `supported_effort_levels`）
    pub levels: Vec<String>,
    /// 是否提供「关闭思考」这一项（端点允许关时才提供）
    pub can_disable: bool,
    /// 当前光标位置
    pub selected: usize,
    /// 当前生效的档位（面板顶部展示，方便对照）
    pub current: Option<String>,
    /// 拉取失败 / 端点不支持任何档位时的说明
    pub error: Option<String>,
}

impl EffortPickerDialog {
    pub(super) fn new(levels: Vec<String>, can_disable: bool, current: Option<String>) -> Self {
        Self {
            levels,
            can_disable,
            selected: 0,
            current,
            error: None,
        }
    }

    /// 面板可选项 = 档位列表 + （可选的）「关闭思考」。
    pub fn item_count(&self) -> usize {
        self.levels.len() + usize::from(self.can_disable)
    }

    /// 选中项对应的**档位值**（`"off"` 表示关闭思考）。
    pub fn item_value(&self, idx: usize) -> Option<String> {
        if idx < self.levels.len() {
            Some(self.levels[idx].clone())
        } else if self.can_disable {
            Some("off".to_string())
        } else {
            None
        }
    }

    pub fn item_label(&self, idx: usize) -> String {
        if idx < self.levels.len() {
            self.levels[idx].clone()
        } else {
            wyj_i18n::tr("effort.picker.off").to_string()
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(self.item_count().saturating_sub(1));
    }
}

/// 解析当前模型**真正支持**的 effort 档位与「能否关闭思考」。
///
/// 走 `ModelCatalog::resolve` 拿到 `ModelIdentity`（vendor / wire / model），
/// 再查 `wyj_api::supported_effort_levels` —— 判据只有 `ThinkingSpec`
/// 这一处，与 provider 序列化请求体时用的是同一张表，避免「面板说支持、
/// 请求里悄悄丢掉」。
pub fn resolve_effort_choices(cfg: &Config, model: &str) -> (Vec<String>, bool) {
    let profile = cfg.active_profile();
    let resolution = wyj_api::ModelCatalog::resolve(profile, Some(model));
    let levels = wyj_api::supported_effort_levels(&resolution.identity)
        .into_iter()
        .map(|level| level.as_str().to_string())
        .collect::<Vec<_>>();
    let can_disable = wyj_api::can_disable_thinking(&resolution.identity);
    (levels, can_disable)
}

/// 应用一次会话级思考档位切换。
///
/// 档位**随会话落盘**（写进 `Session.session_effort`），但**不写
/// `config.toml`**。
///
/// 优先级是 **env > 会话级 > profile 字段**：
/// - `WYJ_CODE_THINKING_EFFORT` / `WYJ_CODE_THINKING_SWITCH` 是用户显式设的
///   运行期开关，优先级最高（见 `Profile::effective_reasoning_effort`）；
/// - 其次是本会话的 `/effort` 覆盖；
/// - 最后才是 profile 里配的 `reasoning_effort`。
///
/// `off` 的展开：`reasoning_effort` 清空 + `thinking_switch = "disabled"`。
///
/// 返回 `true` = 切换成功。
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_session_effort(
    state: &mut AppState,
    value: String,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    session_store: Option<&Arc<SessionStore>>,
    current_session_id: &str,
    session: &Arc<Mutex<Session>>,
) -> bool {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return false;
    }

    // 档位合法性由端点能力裁决：给 GLM 填 `xhigh` 会被静默丢弃，
    // 用户还以为设上了。这里直接拒绝并说明当前端点支持哪些档位。
    if value != "off" {
        let cfg = state.effective_config();
        let (levels, _) = resolve_effort_choices(&cfg, &state.model_name);
        if !levels.iter().any(|level| level == &value) {
            let supported = if levels.is_empty() {
                wyj_i18n::tr("effort.picker.none_supported").to_string()
            } else {
                levels.join(" / ")
            };
            state
                .messages
                .push(ChatMessage::assistant_err(wyj_i18n::tr_fmt(
                    "effort.unsupported_level",
                    &[("level", &value), ("supported", &supported)],
                )));
            return false;
        }
    }

    // 落进内存 Session（随会话文件落盘）。
    state.set_session_effort_in_session(session, Some(value.clone()));
    // 运行态副本：Agent 重建时从 `Config.runtime_session_effort` 读档位
    // （`Config::effective_reasoning_effort` 的 env > 会话 > profile 三级
    // 优先级就靠它）。**不写 `config.toml`**——它落在 `#[serde(skip)]` 槽位上。
    state.session_effort_override = Some(value.clone());

    // 重建 Agent：effort 存在 Agent 的 `reasoning_effort` / `thinking_switch`
    // 字段里，不重建就不生效。
    let mut candidate = state.effective_config();
    candidate.runtime_session_effort = Some(value.clone());
    let model_for_mode = candidate.model_for_mode(&state.mode).to_string();
    let new_agent = match rebuild_fn(&candidate, &model_for_mode) {
        Ok(agent) => agent,
        Err(e) => {
            state
                .messages
                .push(ChatMessage::assistant_err(wyj_i18n::tr_fmt(
                    "model.switch_failed",
                    &[("err", &e.to_string())],
                )));
            return false;
        }
    };
    let new_agent = new_agent.append_system(system_prompt_extra.trim_start().to_string());
    let new_agent = attach_agent_session(new_agent, session_store, current_session_id);
    let new_agent = wire_tool_callback(new_agent, agent_tx.clone(), todo_store.clone());
    *shared_agent.write().unwrap() = Arc::new(new_agent);

    state.messages.push(ChatMessage::assistant(wyj_i18n::tr_fmt(
        "effort.switched",
        &[("level", &value)],
    )));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_off_item_is_only_offered_when_the_endpoint_allows_disabling() {
        let d = EffortPickerDialog::new(vec!["low".into(), "high".into()], false, None);
        assert_eq!(d.item_count(), 2, "不支持关闭时不该多出第三项");
        assert_eq!(d.item_value(0).as_deref(), Some("low"));
        assert_eq!(d.item_value(1).as_deref(), Some("high"));
        assert_eq!(d.item_value(2), None);
    }

    #[test]
    fn the_off_item_maps_to_the_off_value() {
        let d = EffortPickerDialog::new(vec!["low".into()], true, None);
        assert_eq!(d.item_count(), 2);
        assert_eq!(d.item_value(1).as_deref(), Some("off"));
    }

    /// 光标不能越界。
    #[test]
    fn the_cursor_never_escapes_the_list() {
        let mut d = EffortPickerDialog::new(vec!["low".into(), "high".into()], true, None);
        d.move_up();
        assert_eq!(d.selected, 0);
        for _ in 0..10 {
            d.move_down();
        }
        assert_eq!(d.selected, 2, "不能越过「关闭思考」那一项");
    }
}
