//! 应用内文本选择与滚动 / 焦点导航。
//!
//! 从 `app.rs` 拆出。`AppState` 的方法按关注点分成几簇，这一簇是**导航**：
//! 鼠标拖选（`begin/update/end_selection` + 屏幕快照）、聊天区与详情面板的滚动、
//! 会话首尾跳转、输入历史的上下键召回、待发送附件/粘贴块清理。
//!
//! 为什么单独成文件：这一簇的唯一状态来源是 `self`（选区、滚动偏移、焦点、
//! 输入历史），不碰任何 IO，也没有跨簇的前置条件。它是「动线」而不是「数据」，
//! 改输入框交互时不需要同时读完回合调度那一大块。

use super::*;

impl AppState {
    // ── 应用内文本选择 ────────────────────────────────────────────────
    //
    // 开鼠标报告后终端不再处理拖选，选区必须在应用内完成。三个入口：
    // `begin_selection`（左键按下）/ `update_selection`（拖拽）/ `end_selection`
    // （松开，写剪贴板）。高亮与复制都走同一份屏幕快照，保证「选到的就是看到的」。

    /// 左键按下：建立新选区；同一位置 400ms 内第二次按下按双击选词处理。
    pub(crate) fn begin_selection(&mut self, x: u16, y: u16, rows: &[ScreenRow]) {
        // 有浮层时不做应用内选择：`frame_rows` 抓的是叠加后的最终画面，
        // 此时拖选会选中浮层里的文字，但浮层自己有选中行/高亮语义，两者打架。
        if self.has_modal_overlay() {
            self.selection = None;
            return;
        }
        if !self
            .chat_area
            .contains(ratatui::layout::Position::new(x, y))
        {
            self.selection = None;
            return;
        }
        let now = Instant::now();
        let is_double = self.last_click.is_some_and(|(at, lx, ly)| {
            lx == x && ly == y && now.duration_since(at) <= DOUBLE_CLICK_WINDOW
        });
        self.last_click = Some((now, x, y));

        if is_double {
            let (start, end) = word_bounds_at(rows, x, y);
            self.selection = Some(TextSelection {
                anchor: start,
                cursor: end,
                dragging: false,
            });
            // 双击即复制（与 Claude Code / iTerm2 一致：双击选词后松手就可用）。
            self.copy_selection(rows);
        } else {
            self.selection = Some(TextSelection::new((x, y)));
        }
        self.selection_notice = None;
    }

    /// 拖拽：更新选区末端。拖到内容区外时夹到边界（这是「拖出屏幕继续选」
    /// 的最小可用版本，不做自动滚动）。
    pub(crate) fn update_selection(&mut self, x: u16, y: u16) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        if !sel.dragging {
            return;
        }
        let area = self.chat_area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let cx = x.clamp(area.x, area.x + area.width - 1);
        let cy = y.clamp(area.y, area.y + area.height - 1);
        sel.cursor = (cx, cy);
    }

    /// 松开左键：结束拖选并把文字写入系统剪贴板。
    pub(crate) fn end_selection(&mut self, rows: &[ScreenRow]) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        // 双击路径在按下时就已经复制并把 `dragging` 置假；随后的松开事件
        // 不该再复制一遍（内容相同，但状态栏提示会闪两次）。
        if !sel.dragging {
            return;
        }
        sel.dragging = false;
        if sel.is_empty() {
            self.selection = None;
            return;
        }
        self.copy_selection(rows);
    }

    /// 把当前选区按屏幕快照还原成文字并写剪贴板。
    ///
    /// 行的拼接规则：同一行内按列顺序取字符；跨行时**只对选区真正覆盖的部分**
    /// 加换行——首行从选区起点到行尾、末行从行首到选区终点、中间整行全取。
    /// 这避免了「复制一段文字却带出两侧的空白与边框」。
    pub(crate) fn copy_selection(&mut self, rows: &[ScreenRow]) {
        let Some(sel) = self.selection else {
            return;
        };
        let text = selection_text(&sel, rows);
        if text.is_empty() {
            self.selection = None;
            return;
        }
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.clone())) {
            Ok(()) => {
                self.selection_notice = Some((
                    wyj_i18n::tr_fmt(
                        "selection.copied",
                        &[("chars", &text.chars().count().to_string())],
                    ),
                    Instant::now() + SELECTION_NOTICE_DURATION,
                ));
            }
            Err(e) => {
                self.selection_notice = Some((
                    wyj_i18n::tr_fmt("selection.copy_failed", &[("error", &e.to_string())]),
                    Instant::now() + SELECTION_NOTICE_DURATION,
                ));
            }
        }
    }

    /// 清除选区（按任意导航键 / 内容变化时）。
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 当前是否有覆盖在聊天区之上的浮层。
    ///
    /// 只用于「要不要允许应用内拖选」这一个判断：浮层有自己的选中行与高亮，
    /// 在里面拖选会把面板边框、提示行一起复制出来，语义混乱。底部常驻面板
    /// （权限确认、计划批准等）不算——它们不覆盖内容区。
    pub(crate) fn has_modal_overlay(&self) -> bool {
        self.session_picker.is_some()
            || self.settings_dialog.is_some()
            || self.profile_dialog.is_some()
            || self.memory_dialog.is_some()
            || self.evolution_dialog.is_some()
            || self.mcp_dialog.is_some()
            || self.skills_dialog.is_some()
            || self.plugins_dialog.is_some()
            || self.agents_dialog.is_some()
            || self.extensions_dialog.is_some()
            || self.schedule_dialog.is_some()
    }

    /// 从渲染后的 Buffer 抓一屏文本快照，供选区复制使用。
    ///
    /// 只取内容区（`chat_area`），跳过右侧滚动条列——否则复制出来的每行末尾都
    /// 挂着 `│`/`┃`。宽字符占两格，第二格在 ratatui 里是空 symbol，跳过即可。
    ///
    /// 分配策略：每行一个 `String` + 一个 `(列, 偏移)` 数组，复用上一帧的容量。
    /// 主循环空闲时也按 50ms 轮询重绘，逐格 `to_string()` 会在内容区 120×40 时
    /// 每帧产生近 5000 次分配，直接体现在空闲 CPU 上。
    pub(crate) fn capture_frame_rows(&mut self, buffer: &ratatui::buffer::Buffer) {
        let area = self.chat_area.intersection(buffer.area);
        if area.width == 0 || area.height == 0 {
            self.frame_rows.clear();
            return;
        }
        // 最右 1 列是滚动条，不参与选文。
        let right = if self.show_scrollbar && area.width >= 3 {
            area.right() - 1
        } else {
            area.right()
        };
        let rows = area.height as usize;
        self.frame_rows.truncate(rows);
        for (i, y) in (area.top()..area.bottom()).enumerate() {
            if i == self.frame_rows.len() {
                self.frame_rows.push(ScreenRow::default());
            }
            let row = &mut self.frame_rows[i];
            row.y = y;
            row.text.clear();
            row.starts.clear();
            for x in area.left()..right {
                let symbol = buffer[(x, y)].symbol();
                if symbol.is_empty() {
                    continue;
                }
                row.starts.push((x, row.text.len()));
                row.text.push_str(symbol);
            }
        }
    }

    /// 构造**本次运行实际生效**的配置：在全局配置上叠加会话级分组。
    ///
    /// 所有"要用配置去造 Provider / 读 context_window / 算 model"的地方都必须
    /// 走这里，而不是直接用 `self.config`——后者是全局默认值，会话级分组只在
    /// 本字段里。反过来，任何写盘路径都必须用 `self.config`（全局值），两者
    /// 不能混。
    pub fn effective_config(&self) -> Config {
        let mut cfg = self.config.clone();
        if let Some(name) = &self.session_profile_override {
            if cfg.profile_by_name(name).is_some() {
                cfg.active_profile = name.clone();
            }
        }
        cfg
    }

    /// 会话级分组当前是否生效（用于状态栏与 `/model` 面板提示）。
    pub fn session_profile_active(&self) -> Option<&str> {
        self.session_profile_override
            .as_deref()
            .filter(|name| self.config.profile_by_name(name).is_some())
    }

    /// 把会话级分组写入内存 Session（落盘时由 `SessionFile::from_session` 带走）。
    pub(super) fn set_session_profile_in_session(
        &self,
        session: &Arc<Mutex<Session>>,
        name: Option<String>,
    ) {
        // 主循环里不能 await：拿不到锁说明回合正在跑，而回合本身是持锁者。
        // 会话级分组只影响下一轮请求，晚一帧写入没有副作用，直接跳过即可
        // （调用点都在 UI 线程上，且都是用户按键触发的离散操作）。
        if let Some(mut sess) = try_session_lock(session) {
            sess.session_profile = name;
        }
    }

    pub(super) fn focus_chat_edge(&mut self, first: bool) -> bool {
        self.ensure_message_ids();
        let start = self.frozen_up_to.min(self.messages.len());
        let has_chat_content = start < self.messages.len()
            || !self.thinking_buf.is_empty()
            || !self.streaming_buf.is_empty();
        if !has_chat_content {
            return false;
        }
        self.selected_todo_id = None;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.selected_sub_agent = None;
        self.sub_agent_detail_open = false;
        self.reset_sub_agent_detail_view();
        self.ui_focus = UiFocus::Chat;
        if first {
            self.chat_scroll = 0;
            self.chat_follow_tail = false;
        } else {
            self.chat_scroll = self.chat_max_scroll;
            self.chat_follow_tail = true;
            self.unseen_messages = false;
        }
        true
    }

    pub(super) fn select_conversation_start(&mut self) {
        let _ = self.focus_chat_edge(true);
    }

    pub(super) fn select_conversation_end(&mut self) {
        let _ = self.focus_chat_edge(false);
    }

    pub(super) fn scroll_chat_lines(&mut self, delta: i32) {
        let amount = delta.unsigned_abs() as usize;
        if amount == 0 {
            return;
        }
        if delta < 0 {
            let before = self.chat_scroll;
            self.chat_scroll = before.saturating_sub(amount);
            if self.chat_scroll < before {
                self.chat_follow_tail = false;
            }
        } else {
            self.chat_scroll = self
                .chat_scroll
                .saturating_add(amount)
                .min(self.chat_max_scroll);
            if self.chat_scroll >= self.chat_max_scroll {
                self.chat_follow_tail = true;
                self.unseen_messages = false;
            } else {
                self.chat_follow_tail = false;
            }
        }
    }

    pub(super) fn adjust_u16_scroll(value: &mut u16, delta: i32) {
        let amount = delta.unsigned_abs().min(u16::MAX as u32) as u16;
        if amount == 0 {
            return;
        }
        if delta < 0 {
            *value = value.saturating_sub(amount);
        } else {
            *value = value.saturating_add(amount);
        }
    }

    /// 在 [0, max] 范围内调整 value；若调整前后（均先按 max clamp）数值相同，
    /// 说明这次滚动已经被面板内部边界"吞掉"（含 max == 0 即内容根本不需要滚动
    /// 的情况），返回 false，调用方应把这次滚动量转发给别处（聊天区），而不是
    /// 让滚轮事件消失得无声无息。
    pub(super) fn adjust_bounded_u16_scroll(value: &mut u16, delta: i32, max: u16) -> bool {
        let before = (*value).min(max);
        *value = before;
        Self::adjust_u16_scroll(value, delta);
        let after = (*value).min(max);
        *value = after;
        before != after
    }

    /// 详情面板（子 Agent 执行内容 / 后台任务输出）的通用滚动，语义对齐
    /// [`AppState::scroll_chat_lines`]。
    ///
    /// `scroll` 是**从正文第一行往下跳过的行数**，与 `Paragraph::scroll` 同向：
    /// delta > 0 向下翻、delta < 0 向上翻。`follow_tail` 为真时视口钉在末尾跟随
    /// 新内容（此时 `scroll` 的取值由渲染每帧覆盖成 `max_scroll`），用户一旦
    /// 主动上滚就置假，此后新事件只在下方堆积、不再把正在读的位置拽走；一路
    /// 滚回底部再自动恢复跟随。
    ///
    /// 返回 false 表示详情面板内部无处可滚（`max == 0`，内容根本没超出视口），
    /// 调用方应把这次滚动量转发给聊天区，而不是让滚轮事件无声消失。
    pub(super) fn scroll_detail_lines(
        scroll: &mut u16,
        follow_tail: &mut bool,
        max: u16,
        delta: i32,
    ) -> bool {
        if delta == 0 || max == 0 {
            return false;
        }
        let current = if *follow_tail {
            max
        } else {
            (*scroll).min(max)
        };
        let amount = delta.unsigned_abs().min(u16::MAX as u32) as u16;
        let next = if delta < 0 {
            current.saturating_sub(amount)
        } else {
            current.saturating_add(amount).min(max)
        };
        *scroll = next;
        *follow_tail = next >= max;
        true
    }

    /// 选中项变化 / 关闭详情时把子 Agent 详情视图复位成「贴底跟随新事件」。
    pub(super) fn reset_sub_agent_detail_view(&mut self) {
        self.sub_agent_detail_scroll = 0;
        self.sub_agent_detail_follow_tail = true;
    }

    /// [`AppState::reset_sub_agent_detail_view`] 的后台任务版。
    pub(super) fn reset_shell_detail_view(&mut self) {
        self.shell_detail_scroll = 0;
        self.shell_detail_follow_tail = true;
    }

    pub(super) fn move_focus_selection(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        match self.ui_focus {
            UiFocus::Composer => {}
            UiFocus::Chat => {
                if delta < 0 {
                    if self.chat_scroll > 0 {
                        self.scroll_chat_lines(-1);
                    }
                } else if self.chat_scroll < self.chat_max_scroll {
                    self.scroll_chat_lines(1);
                } else if !self.focus_todo_edge(true) {
                    // 跨区链条：Chat → Todos → SubAgents → Shells，逐级回退
                    if !self.focus_sub_agent_edge(true) {
                        self.focus_shell_edge(true);
                    }
                }
            }
            UiFocus::Todos => {
                if !self.move_selected_todo(delta) {
                    if delta < 0 {
                        self.focus_chat_edge(false);
                    } else {
                        // 没有子 Agent 面板可去时，顺势落到后台任务面板，
                        // 保持 Chat ↓→ Todos ↓→ SubAgents ↓→ Shells 的链条连通。
                        if !self.focus_sub_agent_edge(true) {
                            self.focus_shell_edge(true);
                        }
                    }
                }
            }
            UiFocus::TodoDetail => {
                let _ = Self::adjust_bounded_u16_scroll(
                    &mut self.todo_detail_scroll,
                    delta,
                    self.todo_detail_max_scroll,
                );
            }
            UiFocus::AgentsCatalog => {
                if let Some(dialog) = &mut self.agents_dialog {
                    dialog.move_selected(delta);
                } else {
                    self.ui_focus = UiFocus::Composer;
                }
            }
            UiFocus::SubAgents if !self.sub_agents.is_empty() => {
                if !self.move_selected_sub_agent(delta) {
                    if delta < 0 {
                        if !self.focus_todo_edge(false) {
                            self.focus_chat_edge(false);
                        }
                    } else {
                        // 越过最后一条子 Agent → 后台任务面板
                        self.focus_shell_edge(true);
                    }
                }
            }
            UiFocus::SubAgents => self.ui_focus = UiFocus::Composer,
            // 后台任务面板排在子 Agent 面板下方（Chat ↓ → Todos ↓ → SubAgents ↓ → Shells），
            // 向上跨区时逐级回退：Shells → SubAgents → Todos → Chat。
            UiFocus::Shells if !self.shells.is_empty() => {
                if !self.move_selected_shell(delta)
                    && delta < 0
                    && !self.focus_sub_agent_edge(false)
                    && !self.focus_todo_edge(false)
                {
                    self.focus_chat_edge(false);
                }
            }
            UiFocus::Shells => self.ui_focus = UiFocus::Composer,
        }
    }

    pub(super) fn scroll_focus_lines(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        match self.ui_focus {
            UiFocus::AgentsCatalog => {
                if let Some(dialog) = &mut self.agents_dialog {
                    if dialog.detail_open {
                        Self::adjust_u16_scroll(&mut dialog.detail_scroll, delta);
                    } else {
                        dialog.move_selected(delta);
                    }
                } else {
                    self.ui_focus = UiFocus::Composer;
                    self.scroll_chat_lines(delta);
                }
            }
            UiFocus::TodoDetail => {
                let _ = Self::adjust_bounded_u16_scroll(
                    &mut self.todo_detail_scroll,
                    delta,
                    self.todo_detail_max_scroll,
                );
            }
            UiFocus::SubAgents if self.sub_agent_detail_open => {
                if !Self::scroll_detail_lines(
                    &mut self.sub_agent_detail_scroll,
                    &mut self.sub_agent_detail_follow_tail,
                    self.sub_agent_detail_max_scroll,
                    delta,
                ) {
                    self.scroll_chat_lines(delta);
                }
            }
            // 后台任务详情：与子 Agent 详情同款——内容装得下时把滚动量转发给
            // 聊天区，不让滚轮事件无声消失。
            UiFocus::Shells if self.shell_detail_open => {
                if !Self::scroll_detail_lines(
                    &mut self.shell_detail_scroll,
                    &mut self.shell_detail_follow_tail,
                    self.shell_detail_max_scroll,
                    delta,
                ) {
                    self.scroll_chat_lines(delta);
                }
            }
            UiFocus::Chat => self.scroll_chat_lines(delta),
            UiFocus::Composer => self.scroll_chat_lines(delta),
            UiFocus::Todos | UiFocus::SubAgents | UiFocus::Shells => self.scroll_chat_lines(delta),
        }
    }

    pub(super) fn recall_older_input(&mut self, input: &mut InputBox) -> bool {
        if self.input_history.is_empty() {
            return false;
        }
        if self.history_idx.is_none() {
            // 存草稿要走展开版：只含折叠粘贴块的输入框，`text()` 返回空串，
            // 翻历史回来就没了。
            self.history_draft = Some(input.text_with_pastes(&self.pending_pastes));
        }
        let next = match self.history_idx {
            None => self.input_history.len() - 1,
            Some(idx) => idx.saturating_sub(1),
        };
        self.history_idx = Some(next);
        self.load_history_entry(input, next);
        true
    }

    pub(super) fn recall_newer_input(&mut self, input: &mut InputBox) -> bool {
        let Some(idx) = self.history_idx else {
            return false;
        };
        if idx + 1 < self.input_history.len() {
            let next = idx + 1;
            self.history_idx = Some(next);
            self.load_history_entry(input, next);
        } else {
            self.history_idx = None;
            let draft = self.history_draft.take().unwrap_or_default();
            self.pending_pastes.clear();
            input.set_text(&draft);
            self.refold_loaded_text(input);
        }
        true
    }

    pub(super) fn load_history_entry(&mut self, input: &mut InputBox, idx: usize) {
        let Some(text) = self.input_history.get(idx).cloned() else {
            return;
        };
        self.pending_pastes.clear();
        input.set_text(&text);
        self.refold_loaded_text(input);
    }

    /// 历史里存的是**展开后**的正文（折叠只发生在编辑期，发送时已经还原），
    /// 所以回填一段长历史会重现粘贴长文本的老问题：内容全进输入框，而输入框只画
    /// 头 8 行且不滚动。同一套折叠在这里再跑一次，让 ↑ 翻出来的历史和粘贴进来的
    /// 文本有同样的观感。
    pub(super) fn refold_loaded_text(&mut self, input: &mut InputBox) {
        let text = input.text_with_pastes(&[]);
        if text.trim().is_empty() {
            return;
        }
        input.set_text("");
        self.pending_pastes.clear();
        // 整段作为一块折叠：历史正文已经是一条完整消息，按行切开再拼回去
        // 只会让 `[Pasted text #n]` 的编号与用户当初看到的对不上。
        if !fold_paste(self, input, &text) {
            input.set_text(&text);
        }
    }

    /// 普通 ↑/↓ 永远属于输入框：多行草稿先移动逻辑行，只有到达首/末行后才
    /// 进入输入历史。该逻辑不依赖 `is_thinking`，Agent 工作期间也可复用历史
    /// 并排队发送；附件存放在 `pending_attachments`，不会被历史文字替换影响。
    pub(super) fn navigate_input_or_history(&mut self, input: &mut InputBox, delta: i32) -> bool {
        if delta < 0 {
            if input.cursor_row > 0 {
                input.move_cursor_up();
                true
            } else {
                self.recall_older_input(input)
            }
        } else if delta > 0 {
            if input.cursor_row + 1 < input.lines.len() {
                input.move_cursor_down();
                true
            } else {
                self.recall_newer_input(input)
            }
        } else {
            false
        }
    }

    /// 清空「已粘/已贴但还没发出去」的全部待发送数据。
    ///
    /// 附件与折叠的粘贴块是**同一个原子机制的两半**（`ATTACHMENT_MARKER` /
    /// `PASTE_MARKER` ↔ `pending_attachments` / `pending_pastes`），只清其中一半
    /// 就会让编号与标记对不上：残留的 `pending_pastes` 在下一次粘贴时把新块挤到
    /// 错误的序号上。三个调用点（中断、新会话、/clear）必须走这一个方法，
    /// 新增待发送通道时也不会漏改。
    pub(super) fn clear_pending_uploads(&mut self) {
        self.pending_attachments.clear();
        self.pending_pastes.clear();
    }
}
