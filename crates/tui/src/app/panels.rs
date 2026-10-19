//! Todo、子 Agent、后台 Shell 三类面板的状态与导航。
//!
//! 从 `app.rs` 拆出的第二簇。它与 [`super::navigation`] 的分工是：
//! navigation 管「视口怎么动」（滚动、选区、焦点在聊天区里往哪走），这里管
//! 「侧边面板里选中的是谁、它还活着吗」（Todo 条目、子 Agent 运行态、后台
//! 任务与退出回收）。
//!
//! 三者放在一起是因为它们共享同一类不变式：面板列出的东西必须和后台真实
//! 存活的东西一致。`poll_shells` / `reconcile_*_selection` 就是这条不变式的
//! 执行点——面板显示一个已死的任务、和显示一个活着但选不中的任务，是同一类 bug。

use super::*;

impl AppState {
    pub(super) fn active_todo_ids(&self) -> Vec<String> {
        self.current_todos
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .map(|t| t.id.clone())
            .collect()
    }

    pub(super) fn link_message_to_active_todos(&mut self, message_id: u64) {
        for todo_id in self.active_todo_ids() {
            let follow_detail_tail = self.ui_focus == UiFocus::TodoDetail
                && self.selected_todo_id.as_deref() == Some(todo_id.as_str())
                && self.todo_detail_scroll >= self.todo_detail_max_scroll;
            self.todo_execution_logs
                .entry(todo_id)
                .or_default()
                .push(TodoExecutionEntry::Message(message_id));
            if follow_detail_tail {
                // 下一帧渲染会按新的 max_scroll clamp 到真实尾部；这样只有原本
                // 已经位于详情底部的用户才继续跟随，向上阅读时保持当前位置。
                self.todo_detail_scroll = u16::MAX;
            }
        }
    }

    pub(super) fn push_tracked_message(&mut self, msg: ChatMessage) -> u64 {
        let id = self.push_message(msg);
        self.link_message_to_active_todos(id);
        id
    }

    pub(super) fn insert_tracked_message_after(&mut self, idx: usize, msg: ChatMessage) -> u64 {
        let id = self.insert_message_after(idx, msg);
        self.link_message_to_active_todos(id);
        id
    }

    pub(super) fn default_todo_id(&self) -> Option<String> {
        let items = self.current_todos.as_deref()?;
        items
            .iter()
            .find(|t| t.status == TodoStatus::InProgress)
            .or_else(|| items.iter().find(|t| t.status != TodoStatus::Completed))
            .or_else(|| items.last())
            .map(|t| t.id.clone())
    }

    pub(super) fn ensure_selected_todo(&mut self) {
        let Some(items) = self.current_todos.as_deref() else {
            self.selected_todo_id = None;
            self.todo_detail_open = false;
            self.todo_detail_scroll = 0;
            self.todo_detail_max_scroll = 0;
            if self.ui_focus == UiFocus::TodoDetail {
                self.ui_focus = UiFocus::Todos;
            }
            return;
        };
        let selected_valid = self
            .selected_todo_id
            .as_deref()
            .is_some_and(|id| items.iter().any(|t| t.id == id));
        if !selected_valid {
            self.selected_todo_id = self.default_todo_id();
            self.todo_detail_open = false;
            self.todo_detail_scroll = 0;
            self.todo_detail_max_scroll = 0;
            if self.ui_focus == UiFocus::TodoDetail {
                self.ui_focus = UiFocus::Todos;
            }
        }
    }

    pub(super) fn focus_todo_edge(&mut self, first: bool) -> bool {
        let id = self.current_todos.as_deref().and_then(|items| {
            if first {
                items.first()
            } else {
                items.last()
            }
        });
        let Some(id) = id.map(|item| item.id.clone()) else {
            return false;
        };
        self.selected_todo_id = Some(id);
        self.todo_panel_expanded = true;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.selected_sub_agent = None;
        self.sub_agent_detail_open = false;
        self.reset_sub_agent_detail_view();
        self.ui_focus = UiFocus::Todos;
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        true
    }

    /// 在任务列表内部移动；到达边界时返回 false，交给上层跨区域导航。
    pub(super) fn move_selected_todo(&mut self, delta: i32) -> bool {
        let ids: Vec<String> = self
            .current_todos
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|item| item.id.clone())
            .collect();
        if ids.is_empty() {
            self.selected_todo_id = None;
            return false;
        }
        let Some(current) = self
            .selected_todo_id
            .as_deref()
            .and_then(|id| ids.iter().position(|item_id| item_id == id))
        else {
            self.ensure_selected_todo();
            return self.selected_todo_id.is_some();
        };
        let next = current as i32 + delta;
        if next < 0 || next >= ids.len() as i32 {
            return false;
        }
        self.selected_todo_id = Some(ids[next as usize].clone());
        self.todo_detail_scroll = 0;
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        true
    }

    pub(super) fn enter_todo_detail(&mut self) -> bool {
        self.ensure_selected_todo();
        if self.selected_todo_id.is_none() {
            return false;
        }
        self.todo_detail_open = true;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.ui_focus = UiFocus::TodoDetail;
        // Todo 详情追加在聊天流尾部；进入时主动贴底，确保新增的详情 viewport
        // 立即出现在屏幕内。内部 scroll=0 仍表示从详情开头开始阅读。
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        true
    }

    pub(super) fn leave_todo_detail(&mut self) -> bool {
        if self.ui_focus != UiFocus::TodoDetail {
            return false;
        }
        self.ui_focus = UiFocus::Todos;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        true
    }

    pub(super) fn content_focus_active(&self) -> bool {
        matches!(
            self.ui_focus,
            UiFocus::Chat
                | UiFocus::Todos
                | UiFocus::TodoDetail
                | UiFocus::SubAgents
                | UiFocus::Shells
        )
    }

    /// 内容区焦点下，这个键是否必须**被面板吃掉**，绝不能继续下坠到全局快捷键处理。
    ///
    /// 内容区焦点分支只具名处理导航/翻页/Esc，`Char('c')`+CONTROL（中断 Agent）
    /// 与 is_quit（退出）会落进 `_` 兜底；兜底只 `leave_content_focus()` 而**没有
    /// continue**，于是继续下坠被全局分支接住：Ctrl+C 走 [`AppState::interrupt`]
    /// → `current_task.abort()` + `hub.abort_foreground()`，把当前回合和所有前台
    /// 子 Agent 一起中断。在执行轨迹/后台输出面板里按 Ctrl+C 想表达的是
    /// "停下、退出浏览"，不该顺手杀掉整个 agent 服务。
    ///
    /// 只对**面板**焦点生效：Chat 焦点下 Ctrl+C 仍保留"中断 Agent"的原义
    /// （焦点也可能只是滚动聊天区时顺手进来的）。
    pub(super) fn content_focus_swallows_global_key(&self, key: &KeyEvent) -> bool {
        matches!(self.ui_focus, UiFocus::SubAgents | UiFocus::Shells)
            && (is_quit(key)
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
    }

    /// 从输入框显式进入内容区。优先选择离输入框最近且当前可见的运行中 SubAgent
    /// 面板，其次是内联 Todo，最后是聊天区底部。
    ///
    /// **`UiFocus::Chat` 不算「已经进入」**：滚动聊天区（PageUp/滚轮）会把焦点
    /// 留在 Chat，若这里早退，用户在「滚过聊天区之后」按 Shift+↑ 就永远跳不到
    /// 正在跑的子 Agent 面板——这正是「Shift+↑ 没反应」的第二个根因。
    pub(super) fn enter_content_focus(&mut self) -> bool {
        if self.content_focus_active() && self.ui_focus != UiFocus::Chat {
            return true;
        }

        if self.has_running_sub_agents() {
            // 必须取 `visible_sub_agents` 的末项而不是 `sub_agents` 的末项：面板只列
            // 运行中的条目，若选中一个已完成的，内容区会显示它的内容而列表高亮无处可落。
            let last_id = self.visible_sub_agents().last().map(|(id, _)| **id);
            if let Some(last_id) = last_id {
                self.selected_todo_id = None;
                self.selected_sub_agent = Some(last_id);
                self.todo_detail_open = false;
                self.sub_agent_detail_open = true;
                self.reset_sub_agent_detail_view();
                self.ui_focus = UiFocus::SubAgents;
                return true;
            }
        }

        if self
            .current_todos
            .as_deref()
            .is_some_and(|items| !items.is_empty())
        {
            self.selected_sub_agent = None;
            self.todo_panel_expanded = true;
            self.todo_detail_open = false;
            self.ensure_selected_todo();
            if self.selected_todo_id.is_some() {
                self.todo_detail_scroll = 0;
                self.ui_focus = UiFocus::Todos;
                self.chat_follow_tail = true;
                self.unseen_messages = false;
                return true;
            }
        }

        // 后台任务面板排在最后：只有前面都没有可聚焦内容时才落进来
        // （有运行中任务时 shift+↑ 会先进 subagent 面板，再 ↓ 走过来）。
        if self.has_running_shells() {
            if let Some(last_id) = self.running_shells().last().map(|s| s.id.clone()) {
                self.selected_sub_agent = None;
                self.selected_todo_id = None;
                self.todo_detail_open = false;
                self.selected_shell = Some(last_id);
                self.reset_shell_detail_view();
                self.ui_focus = UiFocus::Shells;
                return true;
            }
        }

        self.focus_chat_edge(false)
    }

    pub(super) fn leave_content_focus(&mut self) -> bool {
        if !self.content_focus_active() {
            return false;
        }
        self.ui_focus = UiFocus::Composer;
        self.selected_todo_id = None;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.selected_sub_agent = None;
        self.sub_agent_detail_open = false;
        self.reset_sub_agent_detail_view();
        self.selected_shell = None;
        self.shell_detail_open = false;
        self.reset_shell_detail_view();
        self.shell_detail_max_scroll = 0;
        true
    }

    /// Ctrl+T — 展开/折叠任务列表面板，并把内容焦点交给它。
    ///
    /// 任务全部完成也照常展开：完成态只是**默认**折叠成一行摘要（见
    /// `render::push_inline_todo_lines` 的 `all_done && collapsed` 早退），而不是
    /// 不可读。每项耗时/token 与执行回放是唯一的审计凭据且不落盘，而复盘窗口
    /// 恰恰就是"这一轮干完了、下一轮还没发"的这一刻。
    pub(super) fn toggle_todo_panel(&mut self) {
        let Some(items) = self.current_todos.as_deref() else {
            return;
        };
        if is_todo_collapsible(items) && self.ui_focus == UiFocus::Todos {
            self.todo_panel_expanded = !self.todo_panel_expanded;
        } else {
            self.todo_panel_expanded = true;
        }
        self.ensure_selected_todo();
        if self.selected_todo_id.is_some() {
            self.selected_sub_agent = None;
            self.todo_detail_open = false;
            self.todo_detail_scroll = 0;
            self.todo_detail_max_scroll = 0;
            self.ui_focus = UiFocus::Todos;
            self.chat_follow_tail = true;
            self.unseen_messages = false;
        }
    }

    pub(super) fn toggle_focused_content_detail(&mut self) {
        match self.ui_focus {
            UiFocus::Todos => {
                let _ = self.enter_todo_detail();
            }
            UiFocus::TodoDetail => {
                let _ = self.leave_todo_detail();
            }
            UiFocus::SubAgents if self.selected_sub_agent.is_some() => {
                self.sub_agent_detail_open = !self.sub_agent_detail_open;
                self.reset_sub_agent_detail_view();
            }
            UiFocus::Shells if self.selected_shell.is_some() => {
                self.shell_detail_open = !self.shell_detail_open;
                self.reset_shell_detail_view();
            }
            UiFocus::Composer
            | UiFocus::Chat
            | UiFocus::AgentsCatalog
            | UiFocus::SubAgents
            | UiFocus::Shells => {}
        }
    }

    pub(super) fn close_panel_focus(&mut self) -> bool {
        if self.leave_todo_detail() {
            return true;
        }
        if self.leave_content_focus() {
            return true;
        }
        match self.ui_focus {
            UiFocus::AgentsCatalog => {
                if let Some(dialog) = &mut self.agents_dialog {
                    if dialog.detail_open {
                        dialog.detail_open = false;
                    } else {
                        self.agents_dialog = None;
                        self.ui_focus = UiFocus::Composer;
                    }
                } else {
                    self.ui_focus = UiFocus::Composer;
                }
                true
            }
            UiFocus::Composer
            | UiFocus::Chat
            | UiFocus::Todos
            | UiFocus::TodoDetail
            | UiFocus::SubAgents
            | UiFocus::Shells => false,
        }
    }

    /// 只在**运行中**的子 Agent 间取首/末（与 `visible_sub_agents` 同一口径）。
    pub(super) fn focus_sub_agent_edge(&mut self, first: bool) -> bool {
        let mut ids = self.visible_sub_agents().into_iter().map(|(id, _)| *id);
        let id = if first { ids.next() } else { ids.next_back() };
        let Some(id) = id else {
            return false;
        };
        self.selected_todo_id = None;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.selected_sub_agent = Some(id);
        self.sub_agent_detail_open = true;
        self.reset_sub_agent_detail_view();
        self.ui_focus = UiFocus::SubAgents;
        true
    }

    /// 聚焦后台任务面板的首/末项。`shells` 是 Vec，`first` 取首元素、
    /// 否则取末元素（Vec 保持 spawn 顺序，末项即最新启动的任务）。
    /// 只在**运行中**的条目间取边缘（与 `running_shells` 同一口径）。
    pub(super) fn focus_shell_edge(&mut self, first: bool) -> bool {
        let running = self.running_shells();
        let id = if first {
            running.first().map(|s| s.id.clone())
        } else {
            running.last().map(|s| s.id.clone())
        };
        let Some(id) = id else {
            return false;
        };
        self.selected_todo_id = None;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.selected_sub_agent = None;
        self.sub_agent_detail_open = false;
        self.reset_sub_agent_detail_view();
        self.selected_shell = Some(id);
        self.reset_shell_detail_view();
        self.ui_focus = UiFocus::Shells;
        true
    }

    /// 在后台任务列表内部移动；到达边界时返回 false，交给上层跨区域导航。
    /// 只在**运行中**的条目间移动（与 `running_shells` 同一口径）：已完成的
    /// 条目不在列表里，按 ↓ 跳过它只会让用户以为按键失灵。
    pub(super) fn move_selected_shell(&mut self, delta: i32) -> bool {
        let running = self.running_shells();
        if running.is_empty() {
            self.selected_shell = None;
            return false;
        }
        let current = self
            .selected_shell
            .as_deref()
            .and_then(|id| running.iter().position(|s| s.id == id));
        let Some(current) = current else {
            // 没有有效选中（首次进入面板）：直接落到首/末项
            return self.focus_shell_edge(delta >= 0);
        };
        let next = current as i32 + delta;
        if next < 0 || next >= running.len() as i32 {
            return false;
        }
        self.selected_shell = Some(running[next as usize].id.clone());
        self.reset_shell_detail_view();
        self.ui_focus = UiFocus::Shells;
        true
    }

    /// 是否有仍在运行的后台 shell（驱动 spinner 与底部面板自动显示）
    pub fn has_running_shells(&self) -> bool {
        self.shells.iter().any(|s| s.status == ShellStatus::Running)
    }

    /// 在 SubAgent 列表内部移动；到达边界时返回 false，交给上层跨区域导航。
    /// 只在**运行中**的条目间移动（与 `visible_sub_agents` 同一口径）。
    pub(super) fn move_selected_sub_agent(&mut self, delta: i32) -> bool {
        let ids: Vec<u64> = self
            .sub_agents
            .iter()
            .filter(|(_, s)| s.status == SubAgentStatus::Running)
            .map(|(id, _)| *id)
            .collect();
        if ids.is_empty() {
            self.selected_sub_agent = None;
            return false;
        }
        let Some(current) = self
            .selected_sub_agent
            .and_then(|id| ids.iter().position(|x| *x == id))
        else {
            return self.focus_sub_agent_edge(delta >= 0);
        };
        let next = current as i32 + delta;
        if next < 0 || next >= ids.len() as i32 {
            return false;
        }
        self.selected_sub_agent = Some(ids[next as usize]);
        self.sub_agent_detail_open = true;
        self.reset_sub_agent_detail_view();
        self.ui_focus = UiFocus::SubAgents;
        true
    }

    /// 是否有仍在运行的子 Agent（驱动 spinner 与底部聚合面板）
    pub fn has_running_sub_agents(&self) -> bool {
        self.sub_agents
            .values()
            .any(|s| s.status == SubAgentStatus::Running)
    }

    /// 面板里**应当列出**的后台 shell：只含仍在运行的。
    ///
    /// `self.shells` 本身只增不减（退出状态要留在里面等 `poll_shells` 读、
    /// 自动唤醒也要读），但面板列表只展示运行中的条目 —— 已完成的 shell 对
    /// 用户已经没有可操作价值（输出早已通过 reminder 回灌给模型、聊天流里
    /// 也留了 `bash_N 已退出` 记录），继续挂在列表里只是噪音。
    pub fn running_shells(&self) -> Vec<&ShellUiState> {
        self.shells
            .iter()
            .filter(|s| s.status == ShellStatus::Running)
            .collect()
    }

    /// 选中项若已不在运行列表里（刚好退出），落到最后一个运行项上；
    /// 一个都不剩时清空选中态，避免焦点悬空指向一个看不见的行。
    pub fn reconcile_shell_selection(&mut self) {
        let running: Vec<String> = self
            .shells
            .iter()
            .filter(|s| s.status == ShellStatus::Running)
            .map(|s| s.id.clone())
            .collect();
        let still_valid = self
            .selected_shell
            .as_ref()
            .is_some_and(|sel| running.iter().any(|id| id == sel));
        if !still_valid {
            self.selected_shell = running.last().cloned();
            self.reset_shell_detail_view();
        }
        if self.selected_shell.is_none() {
            self.shell_detail_open = false;
            if self.ui_focus == UiFocus::Shells {
                self.ui_focus = UiFocus::Composer;
            }
        }
    }

    /// 子 Agent 版的 [`AppState::reconcile_shell_selection`]：选中项完成/
    /// 中断后重新落到运行中的最后一项，没有运行项时收起面板并交还输入焦点。
    pub fn reconcile_sub_agent_selection(&mut self) {
        let running: Vec<u64> = self
            .sub_agents
            .iter()
            .filter(|(_, s)| s.status == SubAgentStatus::Running)
            .map(|(id, _)| *id)
            .collect();
        let still_valid = self
            .selected_sub_agent
            .is_some_and(|sel| running.contains(&sel));
        if !still_valid {
            self.selected_sub_agent = running.last().copied();
            self.reset_sub_agent_detail_view();
        }
        if self.selected_sub_agent.is_none() {
            self.sub_agent_detail_open = false;
            if self.ui_focus == UiFocus::SubAgents {
                self.ui_focus = UiFocus::Composer;
            }
        }
    }

    /// 把 `BashSessionManager` 的任务快照同步进 `shells`，返回**本次新检测到
    /// 退出**的任务 id。
    ///
    /// 新 spawn 的 job 在这里首次进入 `shells`（Running）。退出状态只翻转一次：
    /// 已翻成 Exited 的条目再次轮询到同样的 Exited 不会重复上报，因此调用方
    /// 拿到的每个 id 都对应一次真实的「完成」，天然去重。
    ///
    /// 选中项可能指向已不存在的 id（理论上不会发生，jobs map 只增不减），
    /// 这里顺手把选中态收敛到仍在运行的条目上，避免焦点悬空。
    pub fn poll_shells(&mut self) -> Vec<String> {
        let mut newly_exited = Vec::new();
        for (id, command, status) in wyj_tools::BashSessionManager::global().list() {
            let idx = match self.shells.iter().position(|s| s.id == id) {
                Some(i) => i,
                None => {
                    self.shells.push(ShellUiState {
                        id: id.clone(),
                        command: command.clone(),
                        status: ShellStatus::Running,
                        started_at: Instant::now(),
                        final_elapsed: None,
                        wake_sent: false,
                    });
                    self.shells.len() - 1
                }
            };
            if let (ShellStatus::Running, wyj_tools::bash_session::JobStatus::Exited(code)) =
                (self.shells[idx].status, status)
            {
                self.shells[idx].status = ShellStatus::Exited(code);
                self.shells[idx].final_elapsed =
                    Some(self.shells[idx].started_at.elapsed().as_secs_f64());
                newly_exited.push(id);
            }
        }
        // 本次可能刚有任务退出（newly_exited 非空），选中态需要重新对齐到
        // 仍在运行的条目上；否则用户会停在一个已经从列表里消失的行上。
        if !newly_exited.is_empty() {
            self.reconcile_shell_selection();
        }
        newly_exited
    }

    /// 终止当前选中的后台任务。`BashSessionManager::kill` 是 async（SIGTERM
    /// 后要等 2s 才兜底 SIGKILL），不能在同步的按键处理里 await，故 spawn 出去，
    /// 完成后把结果经 `agent_tx` 报回 UI 线程。
    pub(super) fn kill_selected_shell(&mut self) {
        let Some(id) = self.selected_shell.clone() else {
            self.messages.push(ChatMessage::system(wyj_i18n::tr(
                "shells.kill_nothing_selected",
            )));
            return;
        };
        let Some(job) = wyj_tools::BashSessionManager::global().get(&id) else {
            self.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                "shells.kill_missing",
                &[("id", id.as_str())],
            )));
            return;
        };
        if job.status() != wyj_tools::bash_session::JobStatus::Running {
            self.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                "shells.kill_missing",
                &[("id", id.as_str())],
            )));
            return;
        }
        let tx = self.kill_tx.clone();
        tokio::spawn(async move {
            let _ = wyj_tools::BashSessionManager::global().kill(&id).await;
            if let Some(tx) = tx {
                let _ = tx.try_send(AgentEvent::ShellKilled(id));
            }
        });
    }

    /// 可供面板展示的子 Agent：**只含仍在运行的**，按 BTreeMap 自然顺序
    /// （启动顺序）排列。
    ///
    /// 已完成 / 失败 / 中断的条目不再列出——它们的最终结果已经通过
    /// `bg_agent_done_reminder` 回灌进对话（前台则直接作为 ToolResult），
    /// 完整轨迹也仍落盘在 `~/.wyj-code/sessions/<id>.subagents/`，可用
    /// `wyj-code subagent-trace` 或 `/subagents <id>` 之外的落盘路径查阅。
    /// 留在面板里只是占位：用户唯一能对它做的操作（看 trace）已经不在列表上了。
    pub fn visible_sub_agents(&self) -> Vec<(&u64, &SubAgentUiState)> {
        self.sub_agents
            .iter()
            .filter(|(_, s)| s.status == SubAgentStatus::Running)
            .collect()
    }

    pub fn sub_agent_trace_events(&mut self, id: u64) -> Option<Arc<Vec<TraceEvent>>> {
        if !self.sub_agent_trace_cache.contains_key(&id) {
            let sessions_dir = self.sessions_dir.as_ref()?;
            let path = wyj_tools::trace::trace_file(sessions_dir, &self.current_session_id, id);
            if let Ok(events) = wyj_tools::trace::read_trace(&path) {
                self.sub_agent_trace_cache.insert(id, Arc::new(events));
            }
        }
        // 返回 `Arc` 克隆而不是深拷贝：详情面板每帧调用一次，trace 上限 256KiB，
        // 深拷贝等于每帧搬一次整个子 Agent 执行轨迹。
        self.sub_agent_trace_cache.get(&id).cloned()
    }

    /// 追加一条 trace 事件。`Arc::make_mut` 在只有一个持有者时就地追加，
    /// 不触发拷贝；详情面板那一帧的读者结束后引用计数回落到 1。
    pub(super) fn trace_push(&mut self, id: u64, event: TraceEvent) {
        Arc::make_mut(self.sub_agent_trace_cache.entry(id).or_default()).push(event);
    }
}
