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
    //
    // 可选中区域有两个：内容区与输入框正文。**拖选不跨区**——起手在哪就在哪，
    // 夹取边界也按那个区域算（见 [`SelectionArea`]）。

    /// 左键按下：建立新选区；同一位置 400ms 内第二次按下按双击选词处理。
    ///
    /// 返回值是「输入框里该把光标放到哪」（屏幕坐标）。只有落在输入框里的点击
    /// 才返回 `Some`：输入框的 `InputBox` 由主循环持有，`AppState` 改不了它，
    /// 只能把目标位置算好交回去。双击返回词尾，单击返回点击处。
    pub(crate) fn begin_selection(
        &mut self,
        x: u16,
        y: u16,
        rows: &[ScreenRow],
    ) -> Option<(u16, u16)> {
        // 有浮层时不做应用内选择：`frame_rows` 抓的是叠加后的最终画面，
        // 此时拖选会选中浮层里的文字，但浮层自己有选中行/高亮语义，两者打架。
        if self.has_modal_overlay() {
            self.selection = None;
            return None;
        }
        // 滚动条优先：它压在内容区最右一列上，命中时只开始拖动，不建选区。
        if let Some(track) = self.content_scrollbar {
            if track.area.contains(ratatui::layout::Position::new(x, y)) {
                self.selection = None;
                self.begin_scrollbar_drag(y);
                return None;
            }
        }
        let position = ratatui::layout::Position::new(x, y);
        let area = if self.chat_area.contains(position) {
            SelectionArea::Chat
        } else if self.composer_area.contains(position) {
            SelectionArea::Composer
        } else {
            self.selection = None;
            return None;
        };

        let now = Instant::now();
        let is_double = self.last_click.is_some_and(|(at, lx, ly)| {
            lx == x && ly == y && now.duration_since(at) <= DOUBLE_CLICK_WINDOW
        });
        self.last_click = Some((now, x, y));
        self.selection_notice = None;

        if is_double {
            let (start, end) = word_bounds_at(rows, x, y);
            self.selection = Some(TextSelection {
                area,
                anchor: start,
                cursor: end,
                scroll_anchor: self.selection_scroll_anchor(area),
                dragging: false,
            });
            // 双击即复制（与 Claude Code / iTerm2 一致：双击选词后松手就可用）。
            self.copy_selection(rows);
            return (area == SelectionArea::Composer).then_some(end);
        }

        self.selection = Some(TextSelection::new(
            area,
            (x, y),
            self.selection_scroll_anchor(area),
        ));
        (area == SelectionArea::Composer).then_some((x, y))
    }

    /// 某区域当前的纵向滚动偏移（建立选区时记下来做锚点）。
    fn selection_scroll_anchor(&self, area: SelectionArea) -> usize {
        match area {
            SelectionArea::Chat => self.chat_scroll,
            SelectionArea::Composer => self.composer_scroll,
        }
    }

    /// 拖拽：更新选区末端。夹取边界按**起手区域**算，所以从内容区拖到输入框
    /// 不会把输入框文字混进来。
    pub(crate) fn update_selection(&mut self, x: u16, y: u16) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        if !sel.dragging {
            return;
        }
        let area = match sel.area {
            SelectionArea::Chat => self.chat_area,
            SelectionArea::Composer => self.composer_area,
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let cx = x.clamp(area.x, area.x + area.width - 1);
        let cy = y.clamp(area.y, area.y + area.height - 1);
        sel.cursor = (cx, cy);
    }

    /// 输入框里某个屏幕格子对应的原始文本位置 `(row, col)`。
    ///
    /// 坐标系要过两层换算：屏幕格子 → 本帧渲染内容的视觉坐标（加上
    /// `composer_scroll`）→ 展开占位符后的字符索引 → 原始 `InputBox` 的列号。
    /// 少了任何一层，光标都会落在用户没点的地方。
    pub(crate) fn composer_text_pos_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self.composer_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let visual_row = (y - area.y) as usize + self.composer_scroll;
        let visual_col = (x - area.x) as usize;
        let (row, col) =
            self.composer_display
                .text_pos_for_visual(visual_row, visual_col, area.width as usize);
        let col = self
            .composer_display_map
            .get(row)
            .and_then(|map| map.get(col).copied())
            .unwrap_or(col);
        Some((row, col))
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
        let text = selection_text(&sel, rows, self.selection_scroll_delta(&sel));
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

    /// 选区建立至今，它所在区域滚过了多少行（正数 = 内容上移）。
    pub(crate) fn selection_scroll_delta(&self, sel: &TextSelection) -> i32 {
        let current = match sel.area {
            SelectionArea::Chat => self.chat_scroll,
            SelectionArea::Composer => self.composer_scroll,
        };
        current as i32 - sel.scroll_anchor as i32
    }

    /// 清除选区（按任意导航键 / 内容变化时）。
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    // ── 滚动条拖动 ────────────────────────────────────────────────────

    /// 在内容区滚动条上按下：记下抓取位置，开始拖动。
    ///
    /// `grab_offset` 是按下点在滑块内的相对位置——用户抓在滑块中间时，拖动
    /// 过程中滑块应该跟着手走而不是跳到按下点。
    pub(crate) fn begin_scrollbar_drag(&mut self, y: u16) {
        let Some(metrics) = self.content_scrollbar else {
            return;
        };
        let thumb_top = metrics.thumb_top_for(y);
        self.scrollbar_drag = Some(ScrollbarDrag {
            grab_offset: thumb_top.saturating_sub(metrics.thumb_top),
            metrics,
        });
        // 点轨道（而非滑块）时先跳过去，符合滚动条的常规预期。
        self.scroll_to(metrics.target, metrics.scroll_for_thumb_top(thumb_top));
    }

    /// 拖动滚动条：把指针位置换算成滚动偏移。
    pub(crate) fn drag_scrollbar(&mut self, y: u16) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        let pointer = drag.metrics.thumb_top_for(y);
        let thumb_top = pointer.saturating_sub(drag.grab_offset);
        let target = drag.metrics.scroll_for_thumb_top(thumb_top);
        self.scroll_to(drag.metrics.target, target);
    }

    /// 松开滚动条。
    pub(crate) fn end_scrollbar_drag(&mut self) {
        self.scrollbar_drag = None;
    }

    /// 把某一滚动状态直接设到 `offset`（拖动滚动条用）。
    ///
    /// 与 `scroll_chat_lines` 的区别是**允许跳到任意位置**，而不是按步长增减；
    /// 跟随标志的收敛规则保持一致：贴底恢复跟随，离开底部就脱离。
    pub(super) fn scroll_to(&mut self, target: ScrollbarTarget, offset: usize) {
        match target {
            ScrollbarTarget::Chat => {
                let clamped = offset.min(self.chat_max_scroll);
                self.chat_scroll = clamped;
                self.chat_follow_tail = clamped >= self.chat_max_scroll;
                if self.chat_follow_tail {
                    self.unseen_messages = false;
                }
            }
            ScrollbarTarget::SubAgentDetail => {
                let max = self.sub_agent_detail_max_scroll as usize;
                let clamped = offset.min(max);
                self.sub_agent_detail_scroll = clamped.min(u16::MAX as usize) as u16;
                self.sub_agent_detail_follow_tail = clamped >= max;
            }
        }
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
    }

    /// 从渲染后的 Buffer 抓一屏文本快照，供选区复制使用。
    ///
    /// 抓**两段**：内容区（`chat_area`）与输入框正文（`composer_area`）。两段
    /// 的 y 范围互不重叠，`ScreenRow.y` 本身就是屏幕绝对行，复制时按选区所属
    /// 区域的行区间过滤即可。
    ///
    /// 内容区跳过右侧滚动条列——否则复制出来的每行末尾都挂着 `│`/`┃`。宽字符
    /// 占两格，第二格在 ratatui 里是空 symbol，跳过即可。
    ///
    /// 分配策略：每行一个 `String` + 一个 `(列, 偏移)` 数组，复用上一帧的容量。
    /// 主循环空闲时也按 50ms 轮询重绘，逐格 `to_string()` 会在内容区 120×40 时
    /// 每帧产生近 5000 次分配，直接体现在空闲 CPU 上。
    pub(crate) fn capture_frame_rows(&mut self, buffer: &ratatui::buffer::Buffer) {
        // 不 `clear()`：容量要跨帧复用，由 `capture_area_rows` 逐段写入后
        // 在末尾 `truncate` 到实际行数。
        let mut used = 0usize;
        let chat = self.chat_area.intersection(buffer.area);
        if chat.width > 0 && chat.height > 0 {
            // 只有滚动条**真的画出来了**才跳过最右列。内容装得下时不画滚动条，
            // 那一列就是正文——无条件跳过会让最右一个字符永远复制不出来。
            let right = if self.content_scrollbar.is_some() && chat.width >= 3 {
                chat.right() - 1
            } else {
                chat.right()
            };
            used = Self::capture_area_rows(&mut self.frame_rows, used, buffer, chat, right);
        }
        // 输入框正文整段都可选（没有滚动条），只跳过边框之外的部分。
        let composer = self.composer_area.intersection(buffer.area);
        if composer.width > 0 && composer.height > 0 {
            let right = composer.right();
            used = Self::capture_area_rows(&mut self.frame_rows, used, buffer, composer, right);
        }
        self.frame_rows.truncate(used);
    }

    /// 把 `area` 内的字符逐格写进 `rows`（从下标 `start` 开始），返回写入后的行数。
    ///
    /// 复用 `rows` 里已有的 `ScreenRow` 容量：只清每个 `String` / `Vec` 的内容，
    /// 保留它们的堆分配，下一帧 `push_str` / `push` 就不再重新分配。这是
    /// 「空闲时也每 50ms 重绘一帧」下唯一能接受的写法——每帧新建 40 个
    /// `ScreenRow` 会在内容区 120×40 时产生近 5000 次分配。
    fn capture_area_rows(
        rows: &mut Vec<ScreenRow>,
        start: usize,
        buffer: &ratatui::buffer::Buffer,
        area: ratatui::layout::Rect,
        right: u16,
    ) -> usize {
        let mut used = start;
        for y in area.top()..area.bottom() {
            if used == rows.len() {
                rows.push(ScreenRow::default());
            }
            let row = &mut rows[used];
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
            used += 1;
        }
        used
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
        // 会话级思考档位必须跟着走：任何一次重建 Agent（切分组、切模型、
        // 改设置）都要保住当前档位，否则用户设了 high、切个模型就悄悄
        // 回落成端点默认。
        cfg.runtime_session_effort = self.session_effort_override.clone();
        cfg
    }

    /// 会话级分组当前是否生效（用于状态栏与 `/profile` 面板提示）。
    pub fn session_profile_active(&self) -> Option<&str> {
        self.session_profile_override
            .as_deref()
            .filter(|name| self.config.profile_by_name(name).is_some())
    }

    /// 本会话生效的思考档位（`/effort` 设置）。
    ///
    /// `None` = 跟随分组里配的 `reasoning_effort`（此时状态栏不显示
    /// effort 片段——用户没设过，就不该占地方）。
    ///
    /// 注意它与 `session_effort_override`（内存态，用于重建 Agent）的关系：
    /// 前者是**落盘态**（`Session.session_effort`），后者是**运行态**。
    /// 状态栏读前者，重建 Agent 读后者——两者由 `/effort` 的执行路径同时
    /// 写入，理论上恒等；分开是因为「读状态栏」不该去拿 session 锁。
    pub fn session_effort_active(&self) -> Option<&str> {
        self.session_effort_override.as_deref()
    }

    /// 把思考档位写入内存 Session（落盘时由 `SessionFile::from_session` 带走）。
    pub(super) fn set_session_effort_in_session(
        &self,
        session: &Arc<Mutex<Session>>,
        value: Option<String>,
    ) {
        // 主循环里不能 await：拿不到锁说明回合正在跑，而回合本身是持锁者。
        // 思考档位只影响下一轮请求，晚一帧写入没有副作用，直接跳过即可。
        if let Some(mut sess) = try_session_lock(session) {
            sess.session_effort = value;
        }
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

    /// 输入框视口的滚动（滚轮落在输入框上时走这里）。
    ///
    /// 与 `composer_scroll_for` 的关系：那个函数在渲染时按光标位置**强制**开窗，
    /// 这里只处理用户主动滚动。两者都写同一个 `composer_scroll`，所以用户滚到
    /// 别处后一打字，光标行会被重新拉进视口——符合编辑器直觉。
    ///
    /// 返回是否真的滚动了：内容装得下时返回 false，调用方可以把这次滚动转发
    /// 给聊天区，不让滚轮事件无声消失。
    pub(super) fn scroll_composer_lines(&mut self, delta: i32) -> bool {
        let viewport = self.composer_area.height as usize;
        let total = self
            .composer_display
            .visual_height(self.composer_area.width as usize);
        let max_scroll = total.saturating_sub(viewport.max(1));
        if viewport == 0 || max_scroll == 0 {
            return false;
        }
        let before = self.composer_scroll.min(max_scroll);
        let next = if delta < 0 {
            before.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            before.saturating_add(delta as usize).min(max_scroll)
        };
        self.composer_scroll = next;
        next != before
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
