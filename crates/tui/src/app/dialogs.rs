//! 交互对话框：AskQuestion、计划批准、执行模式确认、会话冲突。
//!
//! 从 `app.rs` 拆出。这四个对话框共享同一种形状：**自己是完整的状态机**，
//! 只通过一个 `*Outcome` 枚举告诉外层「该做什么」，外层据此决定是否 `take()`
//! 面板、是否发 `oneshot` 回执。它们不持有 `AppState`，也不碰 IO。
//!
//! 为什么这四个放在一起而不是各自一个文件：它们共用 `InProgressAnswer`
//! （单选/多选/"其他"自由文本的三态游标），而计划批准面板正是复用它做三选一。
//! 拆开就要么把这套游标再复制一遍，要么提成公共依赖——两者都比留在一起差。
//!
//! 与 `super::navigation` / `super::panels` 的分工：那两个管 `AppState` 的
//! 视口与面板状态，这里管**模态对话框自己的状态机**。

use super::*;

/// 单题当前"进行中"的作答状态（未确认，随光标/勾选实时变化）
#[derive(Clone)]

pub enum InProgressAnswer {
    /// 单选：当前高亮下标（`options.len()` 代表落在"其他"虚拟位上）
    Single { cursor: usize },
    /// 多选：当前高亮下标 + 已勾选集合（下标同样可能是"其他"虚拟位）
    Multi {
        cursor: usize,
        checked: BTreeSet<usize>,
    },
    /// "其他"自由文本输入子模式：`prior` 记录进入子模式前的选择态，供 Esc 退回
    FreeText {
        prior: Box<InProgressAnswer>,
        input: InputBox,
    },
}

/// 由题目的 multi_select 决定该题的初始进行中作答态
fn default_in_progress(spec: &AskQuestionSpec) -> InProgressAnswer {
    if spec.multi_select {
        InProgressAnswer::Multi {
            cursor: 0,
            checked: Default::default(),
        }
    } else {
        InProgressAnswer::Single { cursor: 0 }
    }
}

/// 一题已确认的最终答案，用于总览页展示 + 退回编辑时的状态恢复
#[derive(Clone)]
pub struct ConfirmedAnswer {
    pub answer: QuestionAnswer,
    restore: InProgressAnswer,
}

/// AskQuestion 面板所处阶段
#[derive(Clone, Copy)]
pub enum AskQuestionStage {
    /// 逐题作答，index 指向 questions 中的当前题
    Answering { index: usize },
    /// 总览确认页，index 指向当前高亮的行（== questions.len() 代表"确认提交"虚拟行）
    Overview { index: usize },
}

/// AskQuestion 对话框状态（多题交互式访谈）
pub struct AskQuestionDialog {
    pub questions: Vec<AskQuestionSpec>,
    pub stage: AskQuestionStage,
    pub current: InProgressAnswer,
    pub confirmed: Vec<Option<ConfirmedAnswer>>,
    /// 标记当前是否是从总览页跳回来编辑某一题（确认后应回到总览页而不是继续往下一题走）
    pub entered_from_overview: bool,
    /// "其他"自由输入的 `↑` `↓` 历史（跨题共享一条，符合"我在反复改同一段反馈"的用法）
    pub history: InputHistory,
    pub response_tx: tokio::sync::oneshot::Sender<Option<Vec<QuestionAnswer>>>,
}

/// 单次按键处理后，AskQuestion 面板要求外层（AppState）采取的动作
pub enum AskQuestionKeyOutcome {
    /// 面板内部状态已更新，继续展示
    Continue,
    /// 用户整体取消访谈
    Cancel,
    /// 用户在总览页确认提交，外层应 take() 面板并发送最终结果
    Submit,
}

/// `confirm()` 内部计算出的"待执行动作"，用于避免在持有 `&self.current` 借用期间
/// 直接调用需要 `&mut self` 的 `advance()`（先算出动作，再统一执行）
enum ConfirmAction {
    Ignore,
    EnterFreeText(usize),
    Advance(QuestionAnswer, InProgressAnswer),
}

impl AskQuestionDialog {
    pub fn new(
        questions: Vec<AskQuestionSpec>,
        response_tx: tokio::sync::oneshot::Sender<Option<Vec<QuestionAnswer>>>,
    ) -> Self {
        let n = questions.len();
        let current = default_in_progress(&questions[0]);
        Self {
            questions,
            stage: AskQuestionStage::Answering { index: 0 },
            current,
            confirmed: vec![None; n],
            entered_from_overview: false,
            history: InputHistory::default(),
            response_tx,
        }
    }

    pub(super) fn freetext_input_mut(&mut self) -> Option<&mut InputBox> {
        if let InProgressAnswer::FreeText { input, .. } = &mut self.current {
            Some(input)
        } else {
            None
        }
    }

    /// 确认当前题目的答案，写入 confirmed 并推进到下一题/总览页
    fn advance(&mut self, index: usize, answer: QuestionAnswer, restore: InProgressAnswer) {
        self.confirmed[index] = Some(ConfirmedAnswer { answer, restore });
        if self.entered_from_overview {
            self.entered_from_overview = false;
            self.stage = AskQuestionStage::Overview { index };
        } else if index + 1 < self.questions.len() {
            let next = index + 1;
            self.current = default_in_progress(&self.questions[next]);
            self.stage = AskQuestionStage::Answering { index: next };
        } else {
            self.stage = AskQuestionStage::Overview {
                index: self.questions.len(),
            };
        }
    }

    /// Up(-1)/Down(+1)：Answering 阶段移动选项高亮（含"其他"虚拟位），
    /// Overview 阶段移动题目行高亮（含"确认提交"虚拟行）
    fn move_cursor(&mut self, delta: i32) {
        match self.stage {
            AskQuestionStage::Answering { index } => {
                let max = self.questions[index].options.len();
                if let InProgressAnswer::Single { cursor }
                | InProgressAnswer::Multi { cursor, .. } = &mut self.current
                {
                    let new = *cursor as i32 + delta;
                    if (0..=max as i32).contains(&new) {
                        *cursor = new as usize;
                    }
                }
            }
            AskQuestionStage::Overview { index } => {
                let max = self.questions.len();
                let new = index as i32 + delta;
                if (0..=max as i32).contains(&new) {
                    self.stage = AskQuestionStage::Overview {
                        index: new as usize,
                    };
                }
            }
        }
    }

    /// Space：多选题里 toggle 高亮项的勾选状态；落在"其他"虚拟位上则切到自由文本子模式
    fn toggle_check(&mut self) {
        let index = match self.stage {
            AskQuestionStage::Answering { index } => index,
            AskQuestionStage::Overview { .. } => return,
        };
        let other = self.questions[index].options.len();
        if let InProgressAnswer::Multi { cursor, checked } = &mut self.current {
            if *cursor == other {
                let prior = InProgressAnswer::Multi {
                    cursor: *cursor,
                    checked: checked.clone(),
                };
                self.current = InProgressAnswer::FreeText {
                    prior: Box::new(prior),
                    input: InputBox::new(),
                };
            } else {
                let c = *cursor;
                if !checked.remove(&c) {
                    checked.insert(c);
                }
            }
        }
    }

    /// BackTab：退回上一题，恢复该题之前确认过的作答状态
    fn go_back(&mut self) {
        if let AskQuestionStage::Answering { index } = self.stage {
            if index > 0 {
                let prev = index - 1;
                if let Some(c) = &self.confirmed[prev] {
                    self.current = c.restore.clone();
                }
                self.stage = AskQuestionStage::Answering { index: prev };
            }
        }
    }

    /// Enter：按当前 stage/current 分支处理，仅在总览页"确认提交"行且全部题目已作答时返回 Submit
    fn confirm(&mut self) -> AskQuestionKeyOutcome {
        match self.stage {
            AskQuestionStage::Overview { index } => {
                if index == self.questions.len() {
                    if self.confirmed.iter().all(|c| c.is_some()) {
                        AskQuestionKeyOutcome::Submit
                    } else {
                        AskQuestionKeyOutcome::Continue
                    }
                } else {
                    self.entered_from_overview = true;
                    if let Some(c) = &self.confirmed[index] {
                        self.current = c.restore.clone();
                    }
                    self.stage = AskQuestionStage::Answering { index };
                    AskQuestionKeyOutcome::Continue
                }
            }
            AskQuestionStage::Answering { index } => {
                let other = self.questions[index].options.len();
                let action = match &self.current {
                    InProgressAnswer::Single { cursor } => {
                        let cursor = *cursor;
                        if cursor == other {
                            ConfirmAction::EnterFreeText(cursor)
                        } else {
                            ConfirmAction::Advance(
                                QuestionAnswer::Selected(vec![cursor]),
                                InProgressAnswer::Single { cursor },
                            )
                        }
                    }
                    InProgressAnswer::Multi { cursor, checked } => {
                        if checked.is_empty() {
                            ConfirmAction::Ignore
                        } else {
                            let cursor = *cursor;
                            let checked = checked.clone();
                            let indices: Vec<usize> = checked.iter().copied().collect();
                            ConfirmAction::Advance(
                                QuestionAnswer::Selected(indices),
                                InProgressAnswer::Multi { cursor, checked },
                            )
                        }
                    }
                    InProgressAnswer::FreeText { prior, input } => {
                        // join("\n") 而非 join("")：Shift+Enter 换出来的多行反馈
                        // 不能被抹成一行
                        let text = input.text().trim().to_string();
                        if text.is_empty() {
                            ConfirmAction::Ignore
                        } else {
                            self.history.push(&text);
                            let answer = match prior.as_ref() {
                                InProgressAnswer::Multi { checked, .. } if !checked.is_empty() => {
                                    let indices: Vec<usize> = checked.iter().copied().collect();
                                    QuestionAnswer::SelectedWithFreeText(indices, text)
                                }
                                _ => QuestionAnswer::FreeText(text),
                            };
                            ConfirmAction::Advance(answer, (**prior).clone())
                        }
                    }
                };
                match action {
                    ConfirmAction::Ignore => {}
                    ConfirmAction::EnterFreeText(cursor) => {
                        self.current = InProgressAnswer::FreeText {
                            prior: Box::new(InProgressAnswer::Single { cursor }),
                            input: InputBox::new(),
                        };
                    }
                    ConfirmAction::Advance(answer, restore) => {
                        self.advance(index, answer, restore);
                    }
                }
                AskQuestionKeyOutcome::Continue
            }
        }
    }

    /// 带修饰键的统一入口（主循环用）。`handle_key` 只剩裸 `KeyCode` 的老签名，
    /// 保留给既有单测与"语义不变"的调用方。
    pub fn handle_key_event(&mut self, key: KeyEvent) -> AskQuestionKeyOutcome {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if let InProgressAnswer::FreeText { input, .. } = &mut self.current {
            // 编辑键（Backspace/Delete/左右/Home/End/Ctrl+A/E/K/U/W/Alt+←→
            // Shift+Enter…）与主输入框**同一份实现**，不再只有左右和退格
            if crate::input::apply_edit_key(input, &key) {
                return AskQuestionKeyOutcome::Continue;
            }
            // Ctrl+V：弹窗里也要能粘贴长文本。不在这里拦的话，带 CONTROL 的
            // 'v' 会掉进下面的 plain 分支之外、最终被 handle_key 当成字符 'v'
            // 直接敲进输入框。
            if is_clipboard_paste_key(key) {
                if let Some(text) = read_clipboard_text() {
                    input.insert_text(&text);
                }
                return AskQuestionKeyOutcome::Continue;
            }
            // 行尾 `\` + Enter = 换行（不支持 Shift+Enter 的终端里唯一稳的写法）
            if key.code == KeyCode::Enter
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && crate::input::trailing_backslash_before_cursor(input)
            {
                input.backspace();
                input.insert_newline();
                return AskQuestionKeyOutcome::Continue;
            }
            match key.code {
                KeyCode::Enter => return self.confirm(),
                // ↑↓：优先翻"其他"的历史；没有历史时才回退到翻题
                KeyCode::Up | KeyCode::Down => {
                    let moved = if key.code == KeyCode::Up {
                        self.history.prev(input)
                    } else {
                        self.history.next(input)
                    };
                    if !moved {
                        self.move_cursor(if key.code == KeyCode::Up { -1 } else { 1 });
                    }
                    return AskQuestionKeyOutcome::Continue;
                }
                KeyCode::Char(c) if plain => {
                    input.insert_char(c);
                    return AskQuestionKeyOutcome::Continue;
                }
                _ => {}
            }
            return self.handle_key(key.code);
        }

        // 选项态：敲任意非空白字符直接进"其他"输入框。
        // 比 ↓↓ 再回车少两步，也不用先看清选项列表（Claude Code 那边目前仍要
        // ↓ 到 Other 行才能打字，这里是更快的等价路径；↓+Enter 那条路保留不变）。
        if plain {
            if let KeyCode::Char(c) = key.code {
                if !c.is_whitespace() && self.enter_other_with_char(c) {
                    return AskQuestionKeyOutcome::Continue;
                }
            }
        }
        self.handle_key(key.code)
    }

    /// 直接进入"其他"自由输入，并把 `c` 作为首字符写入。返回是否成功进入。
    fn enter_other_with_char(&mut self, c: char) -> bool {
        let AskQuestionStage::Answering { index } = self.stage else {
            return false;
        };
        let prior = std::mem::replace(&mut self.current, InProgressAnswer::Single { cursor: 0 });
        let mut input = InputBox::new();
        input.insert_char(c);
        self.current = InProgressAnswer::FreeText {
            prior: Box::new(prior),
            input,
        };
        let _ = index;
        true
    }

    /// 统一按键入口：返回 Continue/Cancel/Submit，外层（AppState）只需据此决定是否 take() 面板
    pub fn handle_key(&mut self, code: KeyCode) -> AskQuestionKeyOutcome {
        match code {
            KeyCode::Esc => {
                if let InProgressAnswer::FreeText { prior, .. } = &self.current {
                    self.current = (**prior).clone();
                    AskQuestionKeyOutcome::Continue
                } else {
                    AskQuestionKeyOutcome::Cancel
                }
            }
            KeyCode::BackTab => {
                self.go_back();
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Down => {
                self.move_cursor(1);
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Char(c) => {
                if let Some(input) = self.freetext_input_mut() {
                    input.insert_char(c);
                } else if c == ' ' {
                    self.toggle_check();
                }
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Backspace => {
                if let Some(input) = self.freetext_input_mut() {
                    input.backspace();
                }
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Left => {
                if let Some(input) = self.freetext_input_mut() {
                    input.move_left();
                }
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Right => {
                if let Some(input) = self.freetext_input_mut() {
                    input.move_right();
                }
                AskQuestionKeyOutcome::Continue
            }
            KeyCode::Enter => self.confirm(),
            _ => AskQuestionKeyOutcome::Continue,
        }
    }

    /// 总览页确认提交时调用：取出全部已确认答案（要求 confirmed 已全部 Some）
    pub(super) fn take_answers(&mut self) -> Vec<QuestionAnswer> {
        std::mem::take(&mut self.confirmed)
            .into_iter()
            .map(|c| {
                c.expect("take_answers 只应在 confirmed 全部就绪后调用")
                    .answer
            })
            .collect()
    }
}

/// ExitPlanMode 工具触发的计划批准对话框状态
pub struct PlanApprovalDialog {
    /// 三选一高亮状态：复用 AskQuestion 的 `InProgressAnswer`——
    /// `Single { cursor }` 里 0=批准 1=继续规划 2=手动输入（虚拟位）；
    /// 落在虚拟位上回车后进入 `FreeText` 子模式就地输入反馈文本。
    pub(super) current: InProgressAnswer,
    /// 反馈文本的 `↑` `↓` 历史（计划往往反复改好几轮反馈）
    pub history: InputHistory,
    pub response_tx: tokio::sync::oneshot::Sender<bool>,
}

/// [`PlanApprovalDialog::handle_key`] 处理完一次按键后，外层（AppState）要采取的动作
pub enum PlanApprovalOutcome {
    /// 面板内部状态已更新（移动高亮/编辑文本），继续展示
    Continue,
    /// 用户选中「批准」
    Approve,
    /// 用户选中「继续规划」（拒绝）
    Reject,
    /// 用户在「手动输入」子模式提交了反馈文本
    Feedback(String),
}

impl PlanApprovalDialog {
    pub fn new(response_tx: tokio::sync::oneshot::Sender<bool>) -> Self {
        Self {
            current: InProgressAnswer::Single { cursor: 0 },
            history: InputHistory::default(),
            response_tx,
        }
    }

    /// 当前高亮的下标（0=批准 1=继续规划 2=手动输入），FreeText 子模式下取其 prior 的下标
    pub fn cursor(&self) -> usize {
        match &self.current {
            InProgressAnswer::Single { cursor } => *cursor,
            InProgressAnswer::FreeText { prior, .. } => match prior.as_ref() {
                InProgressAnswer::Single { cursor } => *cursor,
                _ => 2,
            },
            InProgressAnswer::Multi { .. } => 0,
        }
    }

    /// 处于「手动输入」自由文本子模式时返回输入框，供渲染层就地展开
    pub fn freetext_input(&self) -> Option<&InputBox> {
        match &self.current {
            InProgressAnswer::FreeText { input, .. } => Some(input),
            _ => None,
        }
    }

    pub(super) fn freetext_input_mut(&mut self) -> Option<&mut InputBox> {
        match &mut self.current {
            InProgressAnswer::FreeText { input, .. } => Some(input),
            _ => None,
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        if let InProgressAnswer::Single { cursor } = &mut self.current {
            let new = *cursor as i32 + delta;
            if (0..=2).contains(&new) {
                *cursor = new as usize;
            }
        }
    }

    /// 带修饰键的统一入口（主循环用）。编辑键与主输入框同一份实现。
    pub fn handle_key_event(&mut self, key: KeyEvent) -> PlanApprovalOutcome {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if let InProgressAnswer::FreeText { input, .. } = &mut self.current {
            if crate::input::apply_edit_key(input, &key) {
                return PlanApprovalOutcome::Continue;
            }
            // Ctrl+V 同 AskQuestion：拦截后再走编辑键，避免 'v' 被当字符敲进去
            if is_clipboard_paste_key(key) {
                if let Some(text) = read_clipboard_text() {
                    input.insert_text(&text);
                }
                return PlanApprovalOutcome::Continue;
            }
            // 行尾 `\` + Enter = 换行
            if key.code == KeyCode::Enter
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && crate::input::trailing_backslash_before_cursor(input)
            {
                input.backspace();
                input.insert_newline();
                return PlanApprovalOutcome::Continue;
            }
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    let moved = if key.code == KeyCode::Up {
                        self.history.prev(input)
                    } else {
                        self.history.next(input)
                    };
                    if !moved {
                        self.move_cursor(if key.code == KeyCode::Up { -1 } else { 1 });
                    }
                    return PlanApprovalOutcome::Continue;
                }
                KeyCode::Char(c) if plain => {
                    input.insert_char(c);
                    return PlanApprovalOutcome::Continue;
                }
                _ => {}
            }
        } else if plain {
            // 选项态直接打字 = 走"手动输入反馈"那条路（虚拟位 index 2），
            // 少一次 ↓↓ + 回车；↓+Enter 那条路保留不变。
            if let KeyCode::Char(c) = key.code {
                if !c.is_whitespace() {
                    let prior = std::mem::replace(
                        &mut self.current,
                        InProgressAnswer::Single { cursor: 2 },
                    );
                    let mut input = InputBox::new();
                    input.insert_char(c);
                    self.current = InProgressAnswer::FreeText {
                        prior: Box::new(prior),
                        input,
                    };
                    return PlanApprovalOutcome::Continue;
                }
            }
        }
        self.handle_key(key.code)
    }

    /// 统一按键入口：↑/↓ 移动高亮、Enter 确认当前项（手动输入位先展开文本框再提交）、
    /// Esc 仅用于从 FreeText 子模式退回三选一（不再等价于拒绝计划）。
    pub fn handle_key(&mut self, code: KeyCode) -> PlanApprovalOutcome {
        match code {
            KeyCode::Esc => {
                if let InProgressAnswer::FreeText { prior, .. } = &self.current {
                    self.current = (**prior).clone();
                }
                PlanApprovalOutcome::Continue
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                PlanApprovalOutcome::Continue
            }
            KeyCode::Down => {
                self.move_cursor(1);
                PlanApprovalOutcome::Continue
            }
            KeyCode::Enter => match &self.current {
                InProgressAnswer::Single { cursor: 0 } => PlanApprovalOutcome::Approve,
                InProgressAnswer::Single { cursor: 1 } => PlanApprovalOutcome::Reject,
                InProgressAnswer::Single { cursor: 2 } => {
                    self.current = InProgressAnswer::FreeText {
                        prior: Box::new(InProgressAnswer::Single { cursor: 2 }),
                        input: InputBox::new(),
                    };
                    PlanApprovalOutcome::Continue
                }
                InProgressAnswer::FreeText { input, .. } => {
                    let text = input.text().trim().to_string();
                    if text.is_empty() {
                        PlanApprovalOutcome::Continue
                    } else {
                        self.history.push(&text);
                        PlanApprovalOutcome::Feedback(text)
                    }
                }
                InProgressAnswer::Single { .. } | InProgressAnswer::Multi { .. } => {
                    PlanApprovalOutcome::Continue
                }
            },
            KeyCode::Char(c) => {
                if let Some(input) = self.freetext_input_mut() {
                    input.insert_char(c);
                }
                PlanApprovalOutcome::Continue
            }
            KeyCode::Backspace => {
                if let Some(input) = self.freetext_input_mut() {
                    input.backspace();
                }
                PlanApprovalOutcome::Continue
            }
            KeyCode::Left => {
                if let Some(input) = self.freetext_input_mut() {
                    input.move_left();
                }
                PlanApprovalOutcome::Continue
            }
            KeyCode::Right => {
                if let Some(input) = self.freetext_input_mut() {
                    input.move_right();
                }
                PlanApprovalOutcome::Continue
            }
            _ => PlanApprovalOutcome::Continue,
        }
    }
}

/// 检测到"计划已批准但仍在 plan 模式"时弹出的确认对话框
pub struct ExecModeConfirmDialog {
    pub pending_message: String,
    pub pending_attachments: Vec<Attachment>,
    /// 聊天区展示文本（附件占位符已按输入框位置展开，见 `OutgoingMessage`）
    pub pending_display: String,
}

/// 会话选择器状态（/sessions 命令触发）
pub struct SessionPickerState {
    /// 历史会话列表（index 0 对应显示项 1，显示项 0 固定为"新建会话"）
    pub sessions: Vec<SessionMeta>,
    /// 当前选中项：0 = 新建会话，1..=n = sessions[selected-1]
    pub selected: usize,
}

/// log_level 固定候选表（供设置面板循环切换/校验）
pub const LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// 「本次 resume 的会话正被另一个实例使用」这一事实，由 CLI 装配阶段检出后传进来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConflictInfo {
    pub session_id: String,
    pub owner_pid: u32,
}

/// 用户对会话占用冲突的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionConflictChoice {
    /// 接管：正常 resume 并照常保存。对方的未保存消息会被本进程的版本覆盖。
    TakeOver,
    /// 只读：把历史载入聊天区供阅读，但**不写回会话文件**，对方完全不受影响。
    /// 这不是 Plan 模式——Plan 只限制文件工具，会话持久化照旧发生。
    ReadOnly,
    /// 放弃这次恢复，退出进程。
    Abort,
}

/// 启动期「会话被占用」弹窗。
///
/// 与 `pending_mcp_trust` / `pending_skill_trust` 同构：启动横幅之后一次性出现、纯本地
/// 状态、**不走 `UiAskRequest`**。这里没有 Agent 回合在 await，造一个 oneshot 通道
/// 只会得到永远没人 send 的死路，还会把 ACP 那条穷尽 match 一起拖下水。
pub struct SessionConflictDialog {
    pub info: SessionConflictInfo,
    /// 0 = 接管 / 1 = 只读 / 2 = 退出
    pub selected: usize,
}

impl SessionConflictDialog {
    pub const OPTIONS: usize = 3;

    pub fn new(info: SessionConflictInfo) -> Self {
        Self {
            info,
            // 默认选中「只读」而不是「接管」：接管会毁掉另一个实例正在写的数据，
            // 破坏性更小的那一项该是回车默认。
            selected: 1,
        }
    }

    /// 处理一个按键。`None` 表示按键只是移动光标，或与本弹窗无关。
    pub fn handle_key(&mut self, key: KeyCode) -> Option<SessionConflictChoice> {
        match key {
            KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
                self.selected = (self.selected + Self::OPTIONS - 1) % Self::OPTIONS;
                None
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % Self::OPTIONS;
                None
            }
            // 数字键直达对应选项，省掉方向键。
            KeyCode::Char('1') | KeyCode::Char('t') => Some(SessionConflictChoice::TakeOver),
            KeyCode::Char('2') | KeyCode::Char('r') => Some(SessionConflictChoice::ReadOnly),
            KeyCode::Char('3') | KeyCode::Esc => Some(SessionConflictChoice::Abort),
            KeyCode::Enter | KeyCode::Char(' ') => Some(match self.selected {
                0 => SessionConflictChoice::TakeOver,
                1 => SessionConflictChoice::ReadOnly,
                _ => SessionConflictChoice::Abort,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod ask_question_dialog_tests {
    use super::*;
    use wyj_core::tool::AskQuestionOption;

    fn opt(label: &str) -> AskQuestionOption {
        AskQuestionOption {
            label: label.to_string(),
            description: None,
        }
    }

    fn single_spec(question: &str) -> AskQuestionSpec {
        AskQuestionSpec {
            question: question.to_string(),
            header: None,
            multi_select: false,
            options: vec![opt("A"), opt("B"), opt("C")],
        }
    }

    fn multi_spec(question: &str) -> AskQuestionSpec {
        AskQuestionSpec {
            question: question.to_string(),
            header: None,
            multi_select: true,
            options: vec![opt("A"), opt("B"), opt("C")],
        }
    }

    fn make_dialog(
        questions: Vec<AskQuestionSpec>,
    ) -> (
        AskQuestionDialog,
        tokio::sync::oneshot::Receiver<Option<Vec<QuestionAnswer>>>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (AskQuestionDialog::new(questions, tx), rx)
    }

    /// 选项态直接敲字符 = 进"其他"输入框并把它当首字符。少一次 ↓↓ + 回车。
    #[test]
    fn typing_a_character_enters_other_with_that_character() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('见'), KeyModifiers::NONE));
        let input = match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => input,
            _ => panic!("应当直接进入自由输入模式"),
        };
        assert_eq!(input.text(), "见");
    }

    /// 空格保留原语义（多选 toggle），不能被"打字就进 Other"吃掉。
    #[test]
    fn space_still_toggles_checkbox_instead_of_opening_other() {
        let (mut dlg, _rx) = make_dialog(vec![multi_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        match &dlg.current {
            InProgressAnswer::Multi { checked, .. } => assert_eq!(checked.len(), 1),
            _ => panic!("空格应当只做勾选，不该进入自由输入"),
        }
    }

    /// 自由文本里必须能用与主输入框同一套编辑键：Delete / Home / End /
    /// Ctrl+A / Ctrl+E / Ctrl+U / Ctrl+K / Ctrl+W / Alt+← →。
    /// 钉住"弹窗与主输入框同源"，否则改一处忘一处就又分叉。
    #[test]
    fn freetext_supports_the_same_editing_keys_as_the_composer() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        let enter_other = |dlg: &mut AskQuestionDialog| {
            dlg.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        };
        enter_other(&mut dlg);
        for c in "abcd".chars() {
            dlg.handle_key_event(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let text = |dlg: &AskQuestionDialog| match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => input.text(),
            _ => panic!("应处于自由输入"),
        };
        assert_eq!(text(&dlg), "xabcd");

        let k = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // 光标在末尾，左移一格后 Delete 删的是「光标处」那个字符
        dlg.handle_key_event(k(KeyCode::Left));
        dlg.handle_key_event(k(KeyCode::Delete));
        assert_eq!(text(&dlg), "xabc");
        dlg.handle_key_event(k(KeyCode::Home));
        dlg.handle_key_event(k(KeyCode::Delete));
        assert_eq!(text(&dlg), "abc");
        // End + Ctrl+E：都在行尾，文本不变
        dlg.handle_key_event(k(KeyCode::End));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(text(&dlg), "abc");
        // Ctrl+U 清到行首
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(text(&dlg), "");
        // 重新打字后：Ctrl+K 删到行尾、Alt+← 按词左移 + Ctrl+W 删词
        for c in "one two".chars() {
            dlg.handle_key_event(k(KeyCode::Char(c)));
        }
        assert_eq!(text(&dlg), "one two");
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(text(&dlg), "one two");
        // 光标在行尾；Alt+← 跳到 "two" 的行首，Ctrl+W 删掉它前面的 "one "
        dlg.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(text(&dlg), "two");
    }

    /// Shift+Enter 换行，且提交时**不能**把换行抹掉（原来 join("") 会拼成一行）。
    #[test]
    fn shift_enter_newline_survives_submission() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        let text = match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => input.text(),
            _ => panic!("应处于自由输入"),
        };
        assert_eq!(text, "a\nb");
        dlg.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match &dlg.confirmed[0] {
            Some(c) => assert!(matches!(&c.answer, QuestionAnswer::FreeText(t) if t == "a\nb")),
            None => panic!("应已确认"),
        }
    }

    /// ↑↓ 在自由文本里翻输入历史；没有历史时才回退成"翻题"。
    #[test]
    fn up_down_recall_freetext_history() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1"), single_spec("Q2")]);
        // 第一条：选项态打字 → 提交
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('甲'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // 已推进到第二题
        assert!(matches!(
            dlg.stage,
            AskQuestionStage::Answering { index: 1 }
        ));

        // 第二题里进入自由输入，按 ↑ 应翻出「甲」
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('乙'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => assert_eq!(input.text(), "甲"),
            _ => panic!("应处于自由输入"),
        }
        dlg.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => assert_eq!(input.text(), "乙"),
            _ => panic!("应处于自由输入"),
        }
    }

    /// Ctrl+V 是粘贴，不是"敲一个字母 v"。
    ///
    /// 两个分支都要钉死剪贴板来测：此前这条直接读**用户真实剪贴板**，断言
    /// "文本逐字不变"——在剪贴板为空的机器/CI 上过，在有内容的开发机上就红
    /// （本地实测就撞到了），是条没人能信任的环境相关测试。
    #[test]
    fn ctrl_v_in_freetext_pastes_instead_of_typing_a_literal_v() {
        // 有内容：应该粘贴进来，而不是敲进一个字母 v
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        with_clipboard_text_override(Some("剪贴板内容"), || {
            dlg.handle_key_event(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        });
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => {
                assert_eq!(input.text(), "x剪贴板内容", "Ctrl+V 应粘贴剪贴板内容");
            }
            _ => panic!("应处于自由输入"),
        }

        // 空剪贴板：粘贴不到内容，但更不能把字母 v 敲进去
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        with_clipboard_text_override(None, || {
            dlg.handle_key_event(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        });
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => {
                assert_eq!(input.text(), "x", "剪贴板为空时 Ctrl+V 也不该插入字符 v");
            }
            _ => panic!("应处于自由输入"),
        }
    }

    /// 换行三种写法都要能用：Shift+Enter（终端支持时）/ Ctrl+J /
    /// 行尾 `\` + Enter。第三种在不支持键盘协议的终端（tmux 默认、多数
    /// Windows 终端）里是唯一能换行的方式。
    #[test]
    fn newline_works_via_ctrl_j_and_trailing_backslash() {
        // Ctrl+J
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => assert_eq!(input.text(), "a\nb"),
            _ => panic!("应处于自由输入"),
        }

        // 行尾反斜杠 + Enter（反斜杠本身不进正文）
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match &dlg.current {
            // 仍然是自由输入（没被 Enter 提交掉），且多出一行
            InProgressAnswer::FreeText { input, .. } => assert_eq!(input.text(), "a\n"),
            _ => panic!("反斜杠 + Enter 应当换行而不是提交"),
        }
    }

    /// 弹窗自由文本与主输入框、配置输入框共用 `input::apply_edit_key`。
    /// 这条钉住"同一份编辑实现"——否则改一处忘一处，弹窗就会重新退化成只能
    /// 打字/退格/左右的老样子。
    #[test]
    fn edit_key_table_is_shared_by_composer_and_dialogs() {
        use crate::input::apply_edit_key;
        let mut composer = InputBox::new();
        for c in "one two".chars() {
            composer.insert_char(c);
        }
        let mut dialog = InputBox::new();
        for c in "one two".chars() {
            dialog.insert_char(c);
        }

        let keys = [
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        ];
        for key in keys {
            assert!(apply_edit_key(&mut composer, &key), "按键应被消费");
            assert!(apply_edit_key(&mut dialog, &key));
        }
        assert_eq!(composer.text(), "two");
        assert_eq!(dialog.text(), "two");
    }

    /// readline 传统别名：Ctrl+B/F 左右、Ctrl+H 退格、Ctrl+D 前删。
    /// （Ctrl+D 在普通终端里可能被终端自己吃掉，绑上无坏处。）
    #[test]
    fn readline_aliases_work_in_freetext() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1")]);
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        for c in "bcd".chars() {
            dlg.handle_key_event(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        // "abcd" 光标在末尾；Ctrl+B 左移一格到 3，Ctrl+H 删掉光标前的 'c'
        dlg.handle_key_event(ctrl('b'));
        dlg.handle_key_event(ctrl('h'));
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => assert_eq!(input.text(), "abd"),
            _ => panic!("应处于自由输入"),
        }
        // Ctrl+D 删掉光标处的 'd'
        dlg.handle_key_event(ctrl('d'));
        match &dlg.current {
            InProgressAnswer::FreeText { input, .. } => assert_eq!(input.text(), "ab"),
            _ => panic!("应处于自由输入"),
        }
    }

    #[test]
    fn single_select_enter_advances_to_next_question() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1"), single_spec("Q2")]);
        assert!(matches!(
            dlg.handle_key(KeyCode::Down),
            AskQuestionKeyOutcome::Continue
        ));
        let outcome = dlg.handle_key(KeyCode::Enter);
        assert!(matches!(outcome, AskQuestionKeyOutcome::Continue));
        assert!(matches!(
            dlg.stage,
            AskQuestionStage::Answering { index: 1 }
        ));
        match &dlg.confirmed[0] {
            Some(c) => assert!(matches!(&c.answer, QuestionAnswer::Selected(v) if v == &vec![1])),
            None => panic!("第一题应已确认"),
        }
    }

    #[test]
    fn multi_select_enter_with_empty_checked_is_ignored() {
        let (mut dlg, _rx) = make_dialog(vec![multi_spec("Q1")]);
        dlg.handle_key(KeyCode::Enter);
        assert!(matches!(
            dlg.stage,
            AskQuestionStage::Answering { index: 0 }
        ));
        assert!(dlg.confirmed[0].is_none());
    }

    #[test]
    fn other_option_freetext_submit_and_esc_back() {
        let (mut dlg, _rx) = make_dialog(vec![single_spec("Q1"), single_spec("Q2")]);
        // 移动到"其他"虚拟位（options.len() == 3）
        for _ in 0..3 {
            dlg.handle_key(KeyCode::Down);
        }
        dlg.handle_key(KeyCode::Enter); // 进入 FreeText
        assert!(matches!(dlg.current, InProgressAnswer::FreeText { .. }));

        // Esc 应退回选项列表，而不是取消整个访谈
        let outcome = dlg.handle_key(KeyCode::Esc);
        assert!(matches!(outcome, AskQuestionKeyOutcome::Continue));
        assert!(matches!(
            dlg.current,
            InProgressAnswer::Single { cursor: 3 }
        ));

        // 重新进入"其他"，输入文本并提交
        dlg.handle_key(KeyCode::Enter);
        dlg.handle_key(KeyCode::Char('嗨'));
        let outcome = dlg.handle_key(KeyCode::Enter);
        assert!(matches!(outcome, AskQuestionKeyOutcome::Continue));
        assert!(matches!(
            dlg.stage,
            AskQuestionStage::Answering { index: 1 }
        ));
        match &dlg.confirmed[0] {
            Some(c) => assert!(matches!(&c.answer, QuestionAnswer::FreeText(t) if t == "嗨")),
            None => panic!("第一题应已确认为自由文本"),
        }
    }

    #[test]
    fn overview_edit_then_confirm_returns_to_overview_not_next_question() {
        let (mut dlg, _rx) = make_dialog(vec![
            single_spec("Q1"),
            single_spec("Q2"),
            single_spec("Q3"),
        ]);
        for _ in 0..3 {
            dlg.handle_key(KeyCode::Enter); // 三题都选中标下标 0
        }
        assert!(matches!(dlg.stage, AskQuestionStage::Overview { index: 3 }));

        // 从"确认提交"行（index==3）Up 三次跳到 Q1 那一行（index==0）
        dlg.handle_key(KeyCode::Up);
        dlg.handle_key(KeyCode::Up);
        dlg.handle_key(KeyCode::Up);
        assert!(matches!(dlg.stage, AskQuestionStage::Overview { index: 0 }));

        // Enter 进入编辑
        dlg.handle_key(KeyCode::Enter);
        assert!(matches!(
            dlg.stage,
            AskQuestionStage::Answering { index: 0 }
        ));

        // 改选另一项后确认
        dlg.handle_key(KeyCode::Down);
        let outcome = dlg.handle_key(KeyCode::Enter);
        assert!(matches!(outcome, AskQuestionKeyOutcome::Continue));
        // 应回到总览页而不是继续走到 Q2
        assert!(matches!(dlg.stage, AskQuestionStage::Overview { index: 0 }));
        match &dlg.confirmed[0] {
            Some(c) => assert!(matches!(&c.answer, QuestionAnswer::Selected(v) if v == &vec![1])),
            None => panic!("第一题应已被覆盖确认"),
        }
    }

    #[tokio::test]
    async fn overview_confirm_submit_sends_all_answers() {
        let (mut dlg, rx) = make_dialog(vec![single_spec("Q1"), single_spec("Q2")]);
        dlg.handle_key(KeyCode::Enter);
        dlg.handle_key(KeyCode::Enter);
        assert!(matches!(dlg.stage, AskQuestionStage::Overview { index: 2 }));
        let outcome = dlg.handle_key(KeyCode::Enter);
        assert!(matches!(outcome, AskQuestionKeyOutcome::Submit));
        let answers = dlg.take_answers();
        let _ = dlg.response_tx.send(Some(answers));
        let received = rx.await.unwrap();
        assert_eq!(received.map(|v| v.len()), Some(2));
    }

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

    /// 面板打开后拦截全部按键，用户不再有滚动手段：打开时必须强制回到贴底，
    /// 否则此前上滚留下的视口位置会把尾部选项区裁出可视范围且无法找回。
    #[test]
    fn ask_questions_event_forces_follow_tail() {
        let mut state = make_state();
        state.chat_follow_tail = false;
        state.unseen_messages = true;

        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.apply_agent_event(AgentEvent::AskQuestions {
            questions: vec![single_spec("Q1")],
            response_tx: tx,
        });

        assert!(state.ask_question_dialog.is_some());
        assert!(state.chat_follow_tail);
        assert!(!state.unseen_messages);
    }
}
