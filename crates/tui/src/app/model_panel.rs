//! `/model` 面板：当前分组下的模型快捷切换。
//!
//! 与 `/profile` 面板（分组管理）严格分工：**分组**是一整套端点凭据，
//! **模型**是分组下的某一个模型名。这里只管后者。
//!
//! ## 作用域：只存内存
//!
//! 切换后的模型**不写 `config.toml`、不落会话文件**——`--resume` 后回到
//! 分组里配置的模型。理由是这个命令的全部价值就是「为手头这个任务临时
//! 换个模型」：写进全局配置会污染其它项目，写进会话文件则让「当前在跑
//! 哪个模型」在恢复后变得不可解释（聊天记录里明明是另一个模型的产出）。
//!
//! 与「会话级分组」不同：后者**是**落盘的（`Session.session_profile`），
//! 因为「这个会话跑在哪套端点上」是会话的属性；模型不是。
//!
//! ## 生效范围：当前模式实际读的那个字段
//!
//! auto/bypass 读 `exec_model`、plan 读 `plan_model`（为空时回退 `model`）。
//! 所以在 plan 模式挑模型不会顺手改掉 auto 模式用的那个。

use super::*;

/// 模型选择面板状态（`/model` 无参命令触发时 Some）。
pub struct ModelPickerDialog {
    /// 端点返回的模型 ID 列表
    pub models: Vec<String>,
    /// 当前光标位置（`models` 的下标）
    pub selected: usize,
    /// 正在拉取模型列表
    pub loading: bool,
    /// 拉取失败时的错误信息（面板顶部展示，仍可手填）
    pub error: Option<String>,
    /// 底部主输入框借用态下的草稿内容（手填模型名时用）
    pub live_input: InputBox,
    /// 当前生效的模型（面板顶部展示，方便对照）
    pub current_model: String,
    /// 手动输入模型名时的借用态标记
    pub manual_entry: bool,
}

impl ModelPickerDialog {
    pub(super) fn new(current_model: String) -> Self {
        Self {
            models: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            live_input: InputBox::new(),
            current_model,
            manual_entry: false,
        }
    }

    /// 面板可选项 = 模型列表 + 「手动输入」。
    ///
    /// 「手动输入」永远是最后一项：拉取失败时它是唯一的出路，成功时它
    /// 也得在——第三方代理不一定实现 `/models` 接口。
    pub fn item_count(&self) -> usize {
        self.models.len() + 1
    }

    /// 选中项的显示文本。
    pub fn item_label(&self, idx: usize) -> String {
        if idx < self.models.len() {
            self.models[idx].clone()
        } else {
            wyj_i18n::tr("model.picker.manual").to_string()
        }
    }

    /// 当前光标指向的模型名（末项是「手动输入」，返回 None）。
    pub fn selected_model(&self) -> Option<&str> {
        self.models.get(self.selected).map(|s| s.as_str())
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(self.item_count().saturating_sub(1));
    }
}

/// `/model` 面板：拉取当前分组端点的可用模型列表。
///
/// **必须走后台 task**——这是网络请求，而主循环绝不能做阻塞等待
/// （`crates/tui/CLAUDE.md` 第 19 条：inline await 一个完成时间不可预期的
/// 操作会让界面停在那一帧，ESC / Ctrl+C 全部失效）。结果经
/// `AgentEvent::ModelListFetched` 回主循环渲染。
pub(super) fn model_start_fetch_models(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    // 拉的是**当前生效分组**的端点：会话级分组切换后 base_url / key 都变了，
    // 用全局配置去拉会拿到另一个供应商的模型列表。
    let cfg = state.effective_config();
    let profile = cfg.active_profile();
    let provider = profile.provider.clone();
    let base_url = if profile.base_url.trim().is_empty() {
        match provider {
            wyj_config::Provider::Anthropic => "https://api.anthropic.com".to_string(),
            wyj_config::Provider::OpenAI => "https://api.openai.com/v1".to_string(),
        }
    } else {
        profile.base_url.trim_end_matches('/').to_string()
    };
    // Key 优先取环境变量里的那个（`api_key_env`），其次是配置里明写的。
    let api_key = profile
        .api_key_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|k| !k.trim().is_empty())
        .or_else(|| profile.api_key.clone())
        .unwrap_or_default();

    if api_key.trim().is_empty() {
        if let Some(dialog) = &mut state.model_picker {
            dialog.loading = false;
            dialog.error = Some(wyj_i18n::tr("model.picker.need_api_key").to_string());
        }
        return;
    }

    if let Some(dialog) = &mut state.model_picker {
        dialog.loading = true;
        dialog.error = None;
    }
    let tx = agent_tx.clone();
    tokio::spawn(async move {
        let result = wyj_api::fetch_model_ids(&provider, &base_url, &api_key)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(AgentEvent::ModelListFetched { result }).await;
    });
}

/// 应用一次会话级模型切换。
///
/// **只改内存**：不写 `config.toml`、不落会话文件。
///
/// 做法是构造一份**临时 Config**（全局配置 + 会话级分组 + 新的模型字段），
/// 交给 `rebuild_fn` 重建 Agent，然后把这个临时值记在
/// `AppState.session_model_override` 里。全局 `state.config` 一字未动——
/// 后续任何一次无关的 `config.save()` 都不会把临时模型固化下来。
///
/// 返回 `true` = 切换成功（调用方据此收起面板）。
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_session_model(
    state: &mut AppState,
    model: String,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    session_store: Option<&Arc<SessionStore>>,
    current_session_id: &str,
) -> bool {
    let model = model.trim().to_string();
    if model.is_empty() {
        return false;
    }
    // 全局配置 + 会话级分组（后者决定 base_url / key 用哪一套）。
    let mut candidate = state.effective_config();
    // 只改当前模式实际读的那个字段（见 `Config::model_field_for_mode`）。
    candidate.set_model_for_mode(&state.mode, &model);

    let new_agent = match rebuild_fn(&candidate, &model) {
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

    state.session_model_override = Some(model.clone());
    state.model_name = model.clone();
    state.context_window = candidate.active_profile().context_window;
    state.messages.push(ChatMessage::assistant(wyj_i18n::tr_fmt(
        "model.switched",
        &[("name", &model)],
    )));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog_with(models: &[&str]) -> ModelPickerDialog {
        let mut d = ModelPickerDialog::new("old".to_string());
        d.loading = false;
        d.models = models.iter().map(|s| s.to_string()).collect();
        d
    }

    /// 「手动输入」永远是最后一项 —— 第三方代理不一定实现 `/models`，
    /// 拉取失败时它是唯一的出路。
    #[test]
    fn manual_entry_is_always_the_last_item() {
        let d = dialog_with(&["a", "b"]);
        assert_eq!(d.item_count(), 3);
        assert_eq!(d.item_label(0), "a");
        assert_eq!(d.item_label(2), wyj_i18n::tr("model.picker.manual"));
    }

    /// 空列表时也要有「手动输入」这一项，否则面板是个空壳。
    #[test]
    fn an_empty_list_still_offers_manual_entry() {
        let d = dialog_with(&[]);
        assert_eq!(d.item_count(), 1);
        assert_eq!(d.selected_model(), None);
    }

    /// 光标不能越界。
    #[test]
    fn the_cursor_never_escapes_the_list() {
        let mut d = dialog_with(&["a", "b"]);
        d.move_up();
        assert_eq!(d.selected, 0);
        d.move_down();
        d.move_down();
        d.move_down();
        assert_eq!(d.selected, 2, "不能越过「手动输入」那一项");
    }
}
