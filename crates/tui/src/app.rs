//! TUI 应用主循环

use crate::event::{is_quit, AgentEvent};
use crate::input::InputBox;
use crate::render;
use crate::theme::Theme;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::backend::CrosstermBackend;
use ratatui::style::Color as UiColor;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use wyj_api::types::{ContentBlock, Message, Role, ToolResultContent};
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer};
use wyj_core::{
    discover_files, extract_preview, extract_title, new_session_id, now_iso, Agent,
    AgentDefinition, DiscoveredFile, HistoryEntry, HistoryStore, InjectionKind, Session,
    SessionFile, SessionMeta, SessionStore, ToolEvent,
};
use wyj_tools::todo::{is_todo_collapsible, TodoItem, TodoStatus};
use wyj_tools::trace::TraceEvent;
use wyj_tools::{ctx::UiAskRequest, PermissionMode};
use wyj_tools::{ExitPlanModeTool, TodoStore, ToolCtx};

/// 用于 /model 热切换 / 设置面板保存后重建 Agent 的函数类型
pub type RebuildFn = Arc<dyn Fn(&Config, &str) -> anyhow::Result<Agent> + Send + Sync>;

/// 消息角色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    /// ! Bash 命令输出
    BashOutput,
    /// 系统通知（模式切换、会话事件等）
    System,
    /// 每轮结束后的耗时/token 摘要行
    TurnSummary,
    /// AI extended thinking 内容，作为普通消息流直接显示
    Thinking,
    /// ExitPlanMode 提交的计划正文，作为普通消息并入应用内聊天流；批准/拒绝/
    /// 手动输入的交互留在贴底的 `PlanApprovalDialog`。
    PlanProposal,
}

/// 渲染用消息
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// TUI 内部稳定 id。0 表示尚未分配，渲染前由 AppState 补齐。
    pub id: u64,
    pub role: MessageRole,
    /// ToolCall = "ToolName(arg)"；ToolResult/BashOutput = 原始输出；User/Assistant = 正文
    pub content: String,
    pub is_error: bool,
    /// 工具执行耗时（ToolResult/BashOutput 专用）
    pub elapsed_secs: Option<f64>,
    /// 本次工具调用的序号（ToolCall 和 ToolResult 共用，从 1 开始）
    pub sequence_no: Option<usize>,
    /// 工具名（ToolResult 专用）
    pub tool_name: Option<String>,
    /// ToolResult 的一行摘要（Claude Code ⎿ 行）
    pub display_summary: String,
    /// `display_summary` 是否直接复用了 `content` 的第一行原文（ToolResult 专用）。
    /// 为真时，展开正文渲染需跳过第一行，避免摘要行与正文首行重复展示。
    pub summary_is_first_line: bool,
    /// 工具结果是否已展开（ToolResult 专用）
    pub expanded: bool,
    /// 绑定的子 Agent id（Agent 工具的 ToolCall/ToolResult 专用）
    pub sub_agent_id: Option<u64>,
    /// Assistant 消息的 markdown 渲染缓存（宽度, 渲染行）。消息定稿后内容
    /// 不再变化，缓存避免每帧（约 20fps）对全部历史重跑 markdown 解析，
    /// 长对话下这是交互延迟的主要来源。宽度变化时自动失效。
    pub md_cache: std::cell::RefCell<Option<(usize, Vec<ratatui::text::Line<'static>>)>>,
}

impl ChatMessage {
    fn base(role: MessageRole, content: String) -> Self {
        Self {
            id: 0,
            role,
            content,
            is_error: false,
            elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        }
    }

    fn user(content: String) -> Self {
        Self::base(MessageRole::User, content)
    }

    fn assistant(content: String) -> Self {
        Self::base(MessageRole::Assistant, content)
    }

    fn thinking(content: String) -> Self {
        Self::base(MessageRole::Thinking, content)
    }

    fn assistant_err(content: String) -> Self {
        let mut m = Self::base(MessageRole::Assistant, content);
        m.is_error = true;
        m
    }

    fn tool_call(display: String, seq: usize) -> Self {
        let mut m = Self::base(MessageRole::ToolCall, display);
        m.sequence_no = Some(seq);
        m
    }

    fn tool_result(
        output: String,
        is_error: bool,
        elapsed_secs: f64,
        seq: usize,
        name: String,
        summary: String,
        summary_is_first_line: bool,
    ) -> Self {
        Self {
            id: 0,
            role: MessageRole::ToolResult,
            content: output,
            is_error,
            elapsed_secs: Some(elapsed_secs),
            sequence_no: Some(seq),
            tool_name: Some(name),
            display_summary: summary,
            summary_is_first_line,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        }
    }

    fn bash_output(output: String, exit_code: i32, elapsed_secs: f64) -> Self {
        Self {
            id: 0,
            role: MessageRole::BashOutput,
            content: output,
            is_error: exit_code != 0,
            elapsed_secs: Some(elapsed_secs),
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            summary_is_first_line: false,
            expanded: false,
            sub_agent_id: None,
            md_cache: std::cell::RefCell::new(None),
        }
    }

    pub fn system(content: String) -> Self {
        Self::base(MessageRole::System, content)
    }

    fn turn_summary(elapsed_secs: f64, d_input: u32, d_output: u32) -> Self {
        let content = format!(
            "⏱ {} · ↑{} ↓{}",
            format_hms(elapsed_secs),
            fmt_tokens(d_input),
            fmt_tokens(d_output),
        );
        Self::base(MessageRole::TurnSummary, content)
    }

    fn plan_proposal(plan: String) -> Self {
        Self::base(MessageRole::PlanProposal, plan)
    }
}

/// 子 Agent 状态（TUI 展示用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
    Failed,
    Interrupted,
}

/// 子 Agent 内部单次工具调用的展示行（展开明细用）
#[derive(Debug, Clone)]
pub struct SubToolLine {
    pub tool_name: String,
    pub arg_summary: String,
    pub is_error: bool,
    /// None = 仍在执行
    pub elapsed_secs: Option<f64>,
}

/// 单个子 Agent 的 TUI 实时状态（key 为 Hub 分配的 id）
#[derive(Debug, Clone)]
pub struct SubAgentUiState {
    pub agent_type: String,
    pub description: String,
    pub background: bool,
    pub status: SubAgentStatus,
    pub started_at: Instant,
    /// 完成/中断时定格的耗时；运行中用 started_at.elapsed() 实时算
    pub final_elapsed: Option<f64>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub tool_calls: usize,
    /// 当前正在执行的内部工具（"Grep(pattern)"）
    pub current_tool: Option<String>,
    /// 内部工具调用明细（展开查看）
    pub tool_log: Vec<SubToolLine>,
    /// 父 Agent 的 ToolResult 消息是否已生成（决定是否还要画动态 ⎿ 行）
    pub has_result: bool,
    /// 完成/中断时间；None 表示仍在运行，用于面板定格计时
    pub finished_at: Option<Instant>,
    /// Done 事件里无条件填充的最终结果全文（前台/后台一致）。
    /// 后台子 Agent 的 ToolResult 消息内容只是"已后台启动"占位文本，
    /// 真实结果只经这个字段，供 agents 面板详情区展示。
    pub final_result: Option<String>,
}

impl SubAgentUiState {
    /// 当前应展示的耗时秒数
    pub fn elapsed_secs(&self) -> f64 {
        self.final_elapsed
            .unwrap_or_else(|| self.started_at.elapsed().as_secs_f64())
    }
}

/// 单条任务的运行时统计（耗时 + token），与 TodoItem 分层存储，按 TodoItem.id 索引。
/// 用 started_at 累加而非单段 final_elapsed，支持同一 id 理论上多次进出 in_progress
/// 时耗时正确累加；没有条目 = 该任务从未进入过 in_progress。
#[derive(Debug, Default)]
pub struct TodoRuntimeStats {
    started_at: Option<Instant>,
    elapsed_secs: f64,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl TodoRuntimeStats {
    pub fn elapsed_secs(&self) -> f64 {
        self.elapsed_secs
            + self
                .started_at
                .map(|t| t.elapsed().as_secs_f64())
                .unwrap_or(0.0)
    }
}

/// 单个 Todo 在执行期间关联到的主消息流事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoExecutionEntry {
    Message(u64),
    Note(String),
}

pub(crate) fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000)
    } else if n >= 1000 {
        format!("{},{:03}", n / 1000, n % 1000)
    } else {
        n.to_string()
    }
}

/// 将秒数格式化为 xh ym zs，便于一眼辨识长耗时。
/// - 小于 10 秒保留一位小数（0.3s / 1.2s）
/// - 10-60 秒显示整数秒
/// - 超过 1 分钟显示 m s；超过 1 小时显示 h m s
pub(crate) fn format_hms(secs: f64) -> String {
    if secs < 60.0 {
        if secs < 10.0 {
            format!("{:.1}s", secs)
        } else {
            format!("{:.0}s", secs)
        }
    } else {
        let total = secs as u64;
        let h = total / 3600;
        let m = (total % 3600) / 60;
        let s = total % 60;
        if h > 0 {
            format!("{}h {}m {}s", h, m, s)
        } else {
            format!("{}m {}s", m, s)
        }
    }
}

/// 权限确认对话框状态
#[derive(Debug)]
pub struct PermissionDialog {
    pub tool_name: String,
    /// 操作摘要（bash 命令 / 目标文件），展示在对话框正文
    pub action_summary: String,
    /// 决策回传通道（AllowOnce / AllowAlways / Deny）
    pub response_tx: tokio::sync::oneshot::Sender<wyj_tools::PermissionDecision>,
}

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
            response_tx,
        }
    }

    fn freetext_input_mut(&mut self) -> Option<&mut InputBox> {
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
                        let text = input.lines.join("").trim().to_string();
                        if text.is_empty() {
                            ConfirmAction::Ignore
                        } else {
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
    fn take_answers(&mut self) -> Vec<QuestionAnswer> {
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
    current: InProgressAnswer,
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

    fn freetext_input_mut(&mut self) -> Option<&mut InputBox> {
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
                    let text = input.lines.join("").trim().to_string();
                    if text.is_empty() {
                        PlanApprovalOutcome::Continue
                    } else {
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

/// 设置面板的可编辑字段索引（对应渲染顺序）——现仅剩 log_level/language，
/// 调用相关字段（provider/model/base_url/api_key 等）已迁到 /model 的 ProfileDialog。
pub const SETTINGS_FIELD_COUNT: usize = 2;

/// 每个字段对应的 i18n label key，下标即字段索引
pub const SETTINGS_FIELD_LABEL_KEYS: [&str; SETTINGS_FIELD_COUNT] =
    ["settings.field.log_level", "settings.field.language"];

/// 设置表单草稿（/config 命令触发时从 AppState.config 初始化，Ctrl+S 时写回）
pub struct SettingsDraft {
    /// 对应 LOG_LEVELS 下标
    pub log_level_idx: usize,
    /// 对应 wyj_i18n::AVAILABLE_LOCALES 下标
    pub language_idx: usize,
}

impl SettingsDraft {
    fn from_config(cfg: &Config) -> Self {
        let log_level_idx = LOG_LEVELS
            .iter()
            .position(|l| *l == cfg.log_level)
            .unwrap_or(3); // 默认 warn
        let current_lang = cfg
            .language
            .clone()
            .unwrap_or_else(|| wyj_i18n::detect_system_locale().to_string());
        let language_idx = wyj_i18n::AVAILABLE_LOCALES
            .iter()
            .position(|l| *l == current_lang)
            .unwrap_or(0);
        Self {
            log_level_idx,
            language_idx,
        }
    }

    /// 用草稿覆盖出一份新 Config（基于 base 保留未编辑的字段，如 profiles/mcp_servers）
    fn to_config(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        cfg.log_level = LOG_LEVELS[self.log_level_idx].to_string();
        cfg.language = Some(wyj_i18n::AVAILABLE_LOCALES[self.language_idx].to_string());
        cfg
    }

    /// 枚举字段循环切换（log_level=0 / language=1）
    fn cycle_enum(&mut self, idx: usize, forward: bool) {
        match idx {
            0 => self.log_level_idx = cycle_index(self.log_level_idx, LOG_LEVELS.len(), forward),
            1 => {
                self.language_idx = cycle_index(
                    self.language_idx,
                    wyj_i18n::AVAILABLE_LOCALES.len(),
                    forward,
                )
            }
            _ => {}
        }
    }

    /// 供渲染用：返回某字段当前值的展示字符串
    pub fn display_value(&self, idx: usize) -> String {
        match idx {
            0 => LOG_LEVELS[self.log_level_idx].to_string(),
            1 => wyj_i18n::locale_display_name(wyj_i18n::AVAILABLE_LOCALES[self.language_idx])
                .to_string(),
            _ => String::new(),
        }
    }
}

fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

/// 判断新到达的任务列表是否属于"新一轮"（相对旧列表没有任何 (id, content) 都匹配的延续任务）。
fn is_new_todo_round(old: Option<&[TodoItem]>, new: &[TodoItem]) -> bool {
    let Some(old) = old else {
        return true;
    };
    !new.iter()
        .any(|n| old.iter().any(|o| o.id == n.id && o.content == n.content))
}

/// 将 total 均分给 n 份，各份之和严格等于 total（余数分给前几份）。
fn split_evenly(total: u32, n: usize) -> Vec<u32> {
    let n32 = n as u32;
    let base = total / n32;
    let rem = total % n32;
    (0..n32)
        .map(|i| if i < rem { base + 1 } else { base })
        .collect()
}

/// 设置面板状态（/config 命令触发，现仅管理 log_level/language 两项全局设置）
pub struct SettingsDialog {
    pub draft: SettingsDraft,
    /// 0..SETTINGS_FIELD_COUNT，当前高亮字段
    pub selected: usize,
    /// 设置面板字段目前全部是枚举字段，此字段始终为 None，保留以复用渲染/交互模式
    pub editing: Option<InputBox>,
    /// 校验失败时的提示，保存成功后清空
    pub error: Option<String>,
}

impl SettingsDialog {
    fn new(cfg: &Config) -> Self {
        Self {
            draft: SettingsDraft::from_config(cfg),
            selected: 0,
            editing: None,
            error: None,
        }
    }
}

// ── CLAUDE.md 记忆面板：/memory 命令触发 ──────────────────────────────────────

/// 记忆面板里的一行：CLAUDE.md 系候选文件 / auto-memory 开关 / auto-memory 索引入口
pub enum MemoryRow {
    File(DiscoveredFile),
    AutoMemoryToggle,
    AutoMemoryIndex { path: PathBuf, exists: bool },
}

pub struct MemoryDialog {
    pub rows: Vec<MemoryRow>,
    pub selected: usize,
    pub auto_memory_enabled: bool,
    pub error: Option<String>,
}

impl MemoryDialog {
    fn new(cwd: &std::path::Path, memory_index_path: PathBuf, auto_memory_enabled: bool) -> Self {
        let mut rows: Vec<MemoryRow> = discover_files(cwd)
            .into_iter()
            .map(MemoryRow::File)
            .collect();
        rows.push(MemoryRow::AutoMemoryToggle);
        let exists = memory_index_path.exists();
        rows.push(MemoryRow::AutoMemoryIndex {
            path: memory_index_path,
            exists,
        });
        Self {
            rows,
            selected: 0,
            auto_memory_enabled,
            error: None,
        }
    }
}

/// 设置面板/分组面板里可编辑字段的类型
pub enum SettingsFieldKind {
    /// 枚举字段：Enter/Left/Right 原地循环切换
    Enum,
    /// 文本字段：Enter 进入行内编辑
    Text,
}

pub fn settings_field_kind(_idx: usize) -> SettingsFieldKind {
    // /config 现仅剩 log_level/language，两者都是枚举字段
    SettingsFieldKind::Enum
}

// ── 分组（Profile）管理面板：/model 命令触发 ──────────────────────────────────

/// 单个分组的可编辑字段数（provider/model/plan_model/exec_model/base_url/api_key/max_tokens/context_window）
pub const PROFILE_FIELD_COUNT: usize = 8;

/// 分组字段对应的 i18n label key，复用 /config 时代已有的 settings.field.* key
pub const PROFILE_FIELD_LABEL_KEYS: [&str; PROFILE_FIELD_COUNT] = [
    "settings.field.provider",
    "settings.field.model",
    "settings.field.plan_model",
    "settings.field.exec_model",
    "settings.field.base_url",
    "settings.field.api_key",
    "settings.field.max_tokens",
    "settings.field.context_window",
];

/// api_key 字段索引（渲染时需要打码）
pub const PROFILE_API_KEY_FIELD_IDX: usize = 5;

/// model/plan_model/exec_model 字段索引集合（这几个字段支持"拉取模型列表"）
pub const PROFILE_MODEL_FIELD_IDXS: [usize; 3] = [1, 2, 3];

pub fn profile_field_kind(idx: usize) -> SettingsFieldKind {
    match idx {
        0 => SettingsFieldKind::Enum, // provider
        _ => SettingsFieldKind::Text,
    }
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
    pub base_url: String,
    pub api_key: String,
    pub max_tokens: String,
    pub context_window: String,
    /// 是否支持图片输入（面板暂不暴露编辑入口，仅透传保留原值）
    pub vision: bool,
    /// thinking 配置（面板暂不暴露编辑入口，仅透传保留原值）
    pub thinking_budget: Option<u32>,
    pub interleaved_thinking: bool,
    pub prompt_cache: Option<bool>,
    pub openai_stream_options: Option<bool>,
}

impl ProfileEntryDraft {
    fn from_profile(p: &wyj_config::Profile) -> Self {
        Self {
            name: p.name.clone(),
            provider_idx: match p.provider {
                wyj_config::Provider::Anthropic => 0,
                wyj_config::Provider::OpenAI => 1,
            },
            model: p.model.clone(),
            plan_model: p.plan_model.clone().unwrap_or_default(),
            exec_model: p.exec_model.clone().unwrap_or_default(),
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone().unwrap_or_default(),
            max_tokens: p.max_tokens.to_string(),
            context_window: p.context_window.to_string(),
            vision: p.vision,
            thinking_budget: p.thinking_budget,
            interleaved_thinking: p.interleaved_thinking,
            prompt_cache: p.prompt_cache,
            openai_stream_options: p.openai_stream_options,
        }
    }

    fn from_template(t: &wyj_api::ProfileTemplate, existing_names: &[String]) -> Self {
        let mut name = t.key.to_string();
        let mut n = 2;
        while existing_names.iter().any(|e| e == &name) {
            name = format!("{}-{}", t.key, n);
            n += 1;
        }
        Self {
            name,
            provider_idx: match t.provider {
                wyj_config::Provider::Anthropic => 0,
                wyj_config::Provider::OpenAI => 1,
            },
            model: t.example_model.to_string(),
            plan_model: String::new(),
            exec_model: String::new(),
            base_url: t.base_url.to_string(),
            api_key: String::new(),
            max_tokens: "8192".to_string(),
            context_window: "200000".to_string(),
            vision: t.vision,
            thinking_budget: None,
            interleaved_thinking: true,
            prompt_cache: t.prompt_cache,
            openai_stream_options: t.openai_stream_options,
        }
    }

    fn provider(&self) -> wyj_config::Provider {
        if self.provider_idx == 0 {
            wyj_config::Provider::Anthropic
        } else {
            wyj_config::Provider::OpenAI
        }
    }

    pub fn text_value(&self, idx: usize) -> &str {
        match idx {
            1 => &self.model,
            2 => &self.plan_model,
            3 => &self.exec_model,
            4 => &self.base_url,
            5 => &self.api_key,
            6 => &self.max_tokens,
            7 => &self.context_window,
            _ => "",
        }
    }

    fn set_text_value(&mut self, idx: usize, value: String) {
        match idx {
            1 => self.model = value,
            2 => self.plan_model = value,
            3 => self.exec_model = value,
            4 => self.base_url = value,
            5 => self.api_key = value,
            6 => self.max_tokens = value,
            7 => self.context_window = value,
            _ => {}
        }
    }

    fn cycle_provider(&mut self, forward: bool) {
        self.provider_idx = cycle_index(self.provider_idx, 2, forward);
    }

    pub fn display_value(&self, idx: usize) -> String {
        match idx {
            0 => {
                if self.provider_idx == 0 {
                    "Anthropic".to_string()
                } else {
                    "OpenAI".to_string()
                }
            }
            _ => self.text_value(idx).to_string(),
        }
    }

    /// 校验 max_tokens/context_window 是否为合法 u32，非法则返回 i18n key
    fn validate(&self) -> Option<&'static str> {
        if self.max_tokens.trim().parse::<u32>().is_err() {
            return Some("settings.error.invalid_max_tokens");
        }
        if self.context_window.trim().parse::<u32>().is_err() {
            return Some("settings.error.invalid_context_window");
        }
        None
    }

    fn to_profile(&self) -> wyj_config::Profile {
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
            base_url: self.base_url.clone(),
            api_key: if self.api_key.trim().is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
            max_tokens: self.max_tokens.trim().parse().unwrap_or(8192),
            context_window: self.context_window.trim().parse().unwrap_or(200_000),
            vision: self.vision,
            thinking_budget: self.thinking_budget,
            interleaved_thinking: self.interleaved_thinking,
            prompt_cache: self.prompt_cache,
            openai_stream_options: self.openai_stream_options,
        }
    }
}

/// ProfileDialog 里当前展示的浮层。重命名/删除确认/模型选择已分别迁移到
/// `InputOwner::Profile` 借用与 `ActionMenu`（危险确认自带 `confirming` 二级
/// 确认、模型选择复用 `dialog.menu`），不再需要专属浮层变体。
pub enum ProfileOverlay {
    None,
    /// 新建分组模板选择器
    TemplatePicker {
        selected: usize,
    },
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

/// 分组管理面板状态（/model 无参命令触发）
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
    /// 打开面板时的快照，供 Esc 时判断"是否有未保存改动"
    saved_snapshot: (Vec<ProfileEntryDraft>, usize),
}

impl ProfileDialog {
    fn new(cfg: &Config) -> Self {
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
            saved_snapshot,
        }
    }

    /// 是否存在未保存的修改（Esc 关闭前的脏检查）
    fn is_dirty(&self) -> bool {
        self.entries != self.saved_snapshot.0 || self.active_idx != self.saved_snapshot.1
    }

    /// 扁平化行列表：entry 头行 + 展开后的字段行 + 末尾固定"+ 新建分组"行。
    pub fn rows(&self) -> Vec<ProfileRow> {
        let mut rows = Vec::new();
        for i in 0..self.entries.len() {
            rows.push(ProfileRow::Header(i));
            if self.expanded == Some(i) {
                for f in 0..PROFILE_FIELD_COUNT {
                    rows.push(ProfileRow::Field(i, f));
                }
            }
        }
        rows.push(ProfileRow::AddNew);
        rows
    }

    /// 当前游标所在行
    fn selected_row(&self) -> ProfileRow {
        self.rows()
            .get(self.cursor)
            .copied()
            .unwrap_or(ProfileRow::AddNew)
    }

    fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// 名字非空且唯一性校验，返回冲突/为空的 i18n key
    fn validate_names(&self) -> Option<&'static str> {
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
            ProfileRow::Field(_, f) if PROFILE_MODEL_FIELD_IDXS.contains(&f) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.manual_edit"),
                        action: ProfileMenuAction::Field(ProfileFieldAction::ManualEdit),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("profile.menu.fetch_models"),
                        action: ProfileMenuAction::Field(ProfileFieldAction::FetchFromServer),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            _ => None,
        }
    }
}

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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpInputField {
    AddRegistryUrl,
    BrowseSearch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsInputField {
    AddMarketplaceUrl,
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
                Theme::CLAUDE,
            ),
            InputOwner::Mcp(McpInputField::BrowseSearch) => (
                wyj_i18n::tr("dialog.input_owner.mcp_browse_search"),
                Theme::CLAUDE,
            ),
            InputOwner::Skills(SkillsInputField::AddMarketplaceUrl) => (
                wyj_i18n::tr("dialog.input_owner.skills_add_marketplace"),
                Theme::CLAUDE,
            ),
            InputOwner::Plugins(PluginsInputField::AddMarketplaceUrl) => (
                wyj_i18n::tr("dialog.input_owner.plugins_add_marketplace"),
                Theme::CLAUDE,
            ),
            InputOwner::Plugins(PluginsInputField::AddLocalPluginPath) => (
                wyj_i18n::tr("dialog.input_owner.plugins_add_local"),
                Theme::CLAUDE,
            ),
            InputOwner::Profile(ProfileInputField::Rename { .. }) => (
                wyj_i18n::tr("dialog.input_owner.profile_rename"),
                Theme::CLAUDE,
            ),
            InputOwner::Profile(ProfileInputField::Field { field_idx, .. }) => (
                wyj_i18n::tr_fmt(
                    "dialog.input_owner.profile_field",
                    &[("field", &wyj_i18n::tr(PROFILE_FIELD_LABEL_KEYS[*field_idx]))],
                ),
                Theme::CLAUDE,
            ),
        }
    }

    /// 是否"边输入边驱动重算"（目前只有 Mcp::BrowseSearch 的实时过滤）；
    /// false 则是"提交式"，仅在 Enter 时触发动作。
    pub fn is_live_filter(&self) -> bool {
        matches!(self, InputOwner::Mcp(McpInputField::BrowseSearch))
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
        }
    }

    /// 只读版本，供渲染层（`render::draw_input`）决定当前该画哪个 InputBox 使用。
    pub fn live_input<'a>(&self, state: &'a AppState) -> Option<&'a InputBox> {
        match self {
            InputOwner::Mcp(_) => state.mcp_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Skills(_) => state.skills_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Plugins(_) => state.plugins_dialog.as_ref().map(|d| &d.live_input),
            InputOwner::Profile(_) => state.profile_dialog.as_ref().map(|d| &d.live_input),
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

/// `/model` 面板操作菜单的按键分发，结构与 `mcp_handle_menu_key` 完全对称。
fn profile_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(ProfileMenuAction),
        Execute(ProfileMenuAction, ProfileRow),
    }

    let step = {
        let Some(dialog) = &mut state.profile_dialog else {
            return;
        };
        let Some(menu) = &mut dialog.menu else {
            return;
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
            profile_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

fn profile_execute_menu_action(
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
            let Some(entry) = state
                .profile_dialog
                .as_ref()
                .map(|d| d.entries[entry_idx].clone())
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
        ProfileMenuAction::ModelChoice(i) => {
            let ProfileRow::Field(entry_idx, field_idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.profile_dialog {
                if let Some(name) = dialog.pending_models.get(i).cloned() {
                    dialog.entries[entry_idx].set_text_value(field_idx, name);
                }
            }
        }
    }
}

/// Ctrl+S 与"保存并关闭"菜单选项共用的保存逻辑：校验 → 写盘 → 重建 agent。
/// 返回 `true` 表示已成功保存（调用方据此决定是否关闭面板）。
#[allow(clippy::too_many_arguments)]
fn profile_try_save(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
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
            let mut new_cfg = state.config.clone();
            new_cfg.profiles = dialog.entries.iter().map(|e| e.to_profile()).collect();
            new_cfg.active_profile = dialog.entries[dialog.active_idx].name.clone();
            match new_cfg.save() {
                Ok(()) => {
                    saved = true;
                    state.config = new_cfg.clone();
                    let model_for_mode = state.config.model_for_mode(&state.mode).to_string();
                    match rebuild_fn(&state.config, &model_for_mode) {
                        Ok(new_agent) => {
                            // rebuild_fn 已装配完整 system prompt，只拼回模式追加段
                            let new_agent = new_agent
                                .append_system(system_prompt_extra.trim_start().to_string());
                            let new_agent =
                                wire_tool_callback(new_agent, agent_tx.clone(), todo_store.clone());
                            *shared_agent.write().unwrap() = Arc::new(new_agent);
                            state.model_name = model_for_mode;
                            state.context_window = state.config.active_profile().context_window;
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
    saved
}

// ── MCP server 管理面板：/mcp 命令触发 ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDialogTab {
    Installed,
    Registries,
    Browse,
}

/// 后台启动连接一个配置的 MCP server 的实时状态（与 `/mcp` 面板是否打开无关）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpConnStatus {
    Connecting,
    Connected { tool_count: usize },
    Failed,
    TimedOut,
}

/// Installed tab 的一行：config 里的 server 条目 + 所在 scope + lockfile 纳管信息（None=未纳管/手动配置）
pub struct McpInstalledRow {
    pub config: wyj_config::McpServerConfig,
    pub scope: wyj_store::InstallScope,
    pub managed: Option<wyj_store::lockfile::InstalledMcpEntry>,
}

pub enum McpOverlay {
    None,
    Searching,
    InstallConfirm {
        server: Box<wyj_store::registry::RegistryServerSummary>,
        package: wyj_store::mcp_install::PackageChoice,
        scope: wyj_store::InstallScope,
    },
    Upgrading {
        row_idx: usize,
    },
    /// 文本内容存在 `McpDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddRegistry,
    /// 只读详情浮层（Installed 行菜单的"查看详情"），Enter/Esc 都直接关闭
    Detail {
        title: String,
        lines: Vec<String>,
    },
}

/// Installed 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpRowAction {
    ToggleEnabled,
    Upgrade,
    Uninstall,
    ViewDetail,
}

/// Registries 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpSourceAction {
    OpenBrowse,
    Remove,
}

/// Browse 结果行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpBrowseAction {
    Install,
}

/// 三个 tab 各自的行菜单动作，用外层枚举包住（同一 dialog 不同 tab 菜单类型不同）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum McpMenuAction {
    Installed(McpRowAction),
    Registries(McpSourceAction),
    Browse(McpBrowseAction),
}

/// MCP server 管理面板状态（/mcp 命令触发）
pub struct McpDialog {
    pub tab: McpDialogTab,
    pub installed: Vec<McpInstalledRow>,
    /// 扁平化行游标，语义随当前 tab 变化（见 `rows()`），Left/Right 切 tab 时归零
    pub cursor: usize,
    /// 已添加的 registry 源（首次打开面板时自动预置官方源，见 `ensure_default_registry`）
    pub registries: Vec<wyj_store::lockfile::McpRegistrySource>,
    /// Browse tab 当前实际查询的源（Registries tab 里选中"浏览"菜单项切换）
    pub active_registry: wyj_store::lockfile::McpRegistrySource,
    pub browse_results: Vec<wyj_store::registry::RegistryServerSummary>,
    pub overlay: McpOverlay,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<FlatRow, McpMenuAction>>,
    pub error: Option<String>,
    pub status: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Mcp(_)` 生效期间使用）
    pub live_input: InputBox,
}

impl McpDialog {
    fn new(cfg: &Config, cwd: &std::path::Path) -> Self {
        let merged = wyj_config::merged_mcp_servers(cfg, cwd);
        let global_lock = wyj_store::lockfile::load_global().unwrap_or_default();
        let project_lock = wyj_store::lockfile::load_project(cwd).unwrap_or_default();
        let project_names: std::collections::HashSet<String> = wyj_config::load_project_mcp(cwd)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();

        let installed = merged
            .into_iter()
            .map(|config| {
                let scope = if project_names.contains(&config.name) {
                    wyj_store::InstallScope::Project
                } else {
                    wyj_store::InstallScope::Global
                };
                let managed = match scope {
                    wyj_store::InstallScope::Global => global_lock
                        .mcp_servers
                        .iter()
                        .find(|e| e.name == config.name)
                        .cloned(),
                    wyj_store::InstallScope::Project => project_lock
                        .mcp_servers
                        .iter()
                        .find(|e| e.name == config.name)
                        .cloned(),
                };
                McpInstalledRow {
                    config,
                    scope,
                    managed,
                }
            })
            .collect();

        let registries = wyj_store::registry::ensure_default_registry()
            .unwrap_or_else(|_| vec![wyj_store::registry::official_registry_source()]);
        let active_registry = registries
            .first()
            .cloned()
            .unwrap_or_else(wyj_store::registry::official_registry_source);

        Self {
            tab: McpDialogTab::Installed,
            installed,
            cursor: 0,
            registries,
            active_registry,
            live_input: InputBox::new(),
            browse_results: Vec::new(),
            overlay: McpOverlay::None,
            menu: None,
            error: None,
            status: None,
        }
    }

    fn refresh_installed(&mut self, cfg: &Config, cwd: &std::path::Path) {
        let fresh = Self::new(cfg, cwd);
        self.installed = fresh.installed;
        self.clamp_cursor();
    }

    /// 增删 registry 源后重新读盘刷新列表；尽量保留当前选中的 `active_registry`
    /// （按 id 匹配），若它被删除了则回退到列表第一项。
    fn refresh_registries(&mut self) {
        self.registries = wyj_store::registry::ensure_default_registry().unwrap_or_default();
        self.clamp_cursor();
        if !self
            .registries
            .iter()
            .any(|r| r.id == self.active_registry.id)
        {
            if let Some(first) = self.registries.first() {
                self.active_registry = first.clone();
            }
        }
    }

    /// 扁平化行列表：当前 tab 下的条目下标，或该 tab 的固定"+ 添加"/"搜索"行。
    /// tab 头不算进 rows()，单独渲染；Installed 没有 AddNew（新增 MCP server
    /// 走 Browse+安装，不是手工添加）。Browse 把"搜索"行固定放在最前面（下标
    /// 0），复用 `FlatRow::AddNew` 这个"触发底部输入框借用"的通用语义。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            McpDialogTab::Installed => (0..self.installed.len()).map(FlatRow::Entry).collect(),
            McpDialogTab::Registries => {
                let mut r: Vec<_> = (0..self.registries.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            McpDialogTab::Browse => {
                let mut r = vec![FlatRow::AddNew];
                r.extend((0..self.browse_results.len()).map(FlatRow::Entry));
                r
            }
        }
    }

    pub fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// 选中条目回车后弹出的操作菜单，None 表示当前行不支持弹菜单（不应发生，
    /// 调用方已确保 cursor 落在 Entry 行上）
    pub fn build_menu(&self) -> Option<ActionMenu<FlatRow, McpMenuAction>> {
        let row = *self.rows().get(self.cursor)?;
        match (self.tab, row) {
            (McpDialogTab::Installed, FlatRow::Entry(idx)) => {
                let r = self.installed.get(idx)?;
                let enabled = r.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                let can_upgrade = r.managed.as_ref().is_some_and(|m| m.is_managed());
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if enabled {
                            "mcp.menu.disable"
                        } else {
                            "mcp.menu.enable"
                        }),
                        action: McpMenuAction::Installed(McpRowAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.upgrade"),
                        action: McpMenuAction::Installed(McpRowAction::Upgrade),
                        dangerous: false,
                        disabled: !can_upgrade,
                        disabled_reason: (!can_upgrade)
                            .then(|| wyj_i18n::tr("mcp.error.manual_no_upgrade")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.uninstall"),
                        action: McpMenuAction::Installed(McpRowAction::Uninstall),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.view_detail"),
                        action: McpMenuAction::Installed(McpRowAction::ViewDetail),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (McpDialogTab::Registries, FlatRow::Entry(_)) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.browse_source"),
                        action: McpMenuAction::Registries(McpSourceAction::OpenBrowse),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("mcp.menu.remove_source"),
                        action: McpMenuAction::Registries(McpSourceAction::Remove),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (McpDialogTab::Browse, FlatRow::Entry(_)) => {
                let items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("mcp.menu.install"),
                    action: McpMenuAction::Browse(McpBrowseAction::Install),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }];
                Some(ActionMenu::new(row, items))
            }
            _ => None,
        }
    }
}

/// `/mcp` 面板操作菜单的按键分发：Up/Down 选、Enter 确认/二次确认、Esc 逐级返回。
/// 危险操作（卸载/删除源）先进 `confirming` 二级确认，再次 Enter/y 才真正执行。
fn mcp_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(McpMenuAction),
        Execute(McpMenuAction, FlatRow),
    }

    let step = {
        let Some(dialog) = &mut state.mcp_dialog else {
            return;
        };
        let Some(menu) = &mut dialog.menu else {
            return;
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
            if let Some(dialog) = &mut state.mcp_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                dialog.menu = None;
            }
            mcp_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

fn mcp_scope_label_text(scope: wyj_store::InstallScope) -> String {
    wyj_i18n::tr(match scope {
        wyj_store::InstallScope::Global => "mcp.dialog.scope_global",
        wyj_store::InstallScope::Project => "mcp.dialog.scope_project",
    })
}

/// 执行操作菜单选中项对应的动作（在按键分发之外单独拆出，便于危险操作走
/// 二级确认后再调用同一套逻辑）。`target` 是菜单弹出时记录的目标行，用于
/// 定位 `installed`/`registries`/`browse_results` 里具体是哪一条。
fn mcp_execute_menu_action(
    state: &mut AppState,
    action: McpMenuAction,
    target: FlatRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    let FlatRow::Entry(idx) = target else {
        return;
    };
    match action {
        McpMenuAction::Installed(McpRowAction::ToggleEnabled) => {
            let info = state.mcp_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).map(|row| {
                    let enabled = row.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                    (row.config.name.clone(), row.scope, enabled)
                })
            });
            if let Some((name, scope, currently_enabled)) = info {
                let result = wyj_store::mcp_install::set_mcp_enabled(
                    &name,
                    scope,
                    &state.cwd,
                    !currently_enabled,
                );
                if let Some(dialog) = &mut state.mcp_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&state.config, &state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        McpMenuAction::Installed(McpRowAction::Upgrade) => {
            let info = state.mcp_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).and_then(|row| {
                    row.managed
                        .as_ref()
                        .is_some_and(|m| m.is_managed())
                        .then(|| (row.config.name.clone(), row.scope))
                })
            });
            if let Some((name, scope)) = info {
                if let Some(dialog) = &mut state.mcp_dialog {
                    dialog.overlay = McpOverlay::Upgrading { row_idx: idx };
                }
                let tx = agent_tx.clone();
                let cwd = state.cwd.clone();
                tokio::spawn(async move {
                    let result = wyj_store::mcp_install::upgrade_mcp_server(&name, scope, &cwd)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::McpUpgraded {
                            row_idx: idx,
                            result,
                        })
                        .await;
                });
            } else if let Some(dialog) = &mut state.mcp_dialog {
                dialog.error = Some(wyj_i18n::tr("mcp.error.manual_no_upgrade"));
            }
        }
        McpMenuAction::Installed(McpRowAction::Uninstall) => {
            let info = state
                .mcp_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| (row.config.name.clone(), row.scope));
            if let Some((name, scope)) = info {
                let result = wyj_store::mcp_install::uninstall_mcp_server(&name, scope, &state.cwd);
                if let Some(dialog) = &mut state.mcp_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status = Some(wyj_i18n::tr("mcp.uninstall.done"));
                            dialog.refresh_installed(&state.config, &state.cwd);
                        }
                        Err(e) => {
                            dialog.error = Some(wyj_i18n::tr_fmt(
                                "mcp.error.uninstall_failed",
                                &[("err", &e.to_string())],
                            ));
                        }
                    }
                }
            }
        }
        McpMenuAction::Installed(McpRowAction::ViewDetail) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(row) = dialog.installed.get(idx) {
                    let mut lines = vec![
                        wyj_i18n::tr_fmt("mcp.detail.name_line", &[("name", &row.config.name)]),
                        wyj_i18n::tr_fmt(
                            "mcp.detail.scope_line",
                            &[("scope", &mcp_scope_label_text(row.scope))],
                        ),
                        wyj_i18n::tr_fmt(
                            "mcp.detail.command_line",
                            &[("command", row.config.command.as_deref().unwrap_or(""))],
                        ),
                    ];
                    if !row.config.args.is_empty() {
                        lines.push(wyj_i18n::tr_fmt(
                            "mcp.detail.args_line",
                            &[("args", &row.config.args.join(" "))],
                        ));
                    }
                    if !row.config.env.is_empty() {
                        let env_str = row
                            .config
                            .env
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        lines.push(wyj_i18n::tr_fmt(
                            "mcp.detail.env_line",
                            &[("env", &env_str)],
                        ));
                    }
                    dialog.overlay = McpOverlay::Detail {
                        title: wyj_i18n::tr("mcp.detail.title"),
                        lines,
                    };
                }
            }
        }
        McpMenuAction::Registries(McpSourceAction::OpenBrowse) => {
            if let Some(dialog) = &mut state.mcp_dialog {
                if let Some(source) = dialog.registries.get(idx).cloned() {
                    dialog.active_registry = source;
                    dialog.browse_results.clear();
                    dialog.tab = McpDialogTab::Browse;
                    dialog.cursor = 0;
                }
            }
        }
        McpMenuAction::Registries(McpSourceAction::Remove) => {
            let id = state
                .mcp_dialog
                .as_ref()
                .and_then(|d| d.registries.get(idx))
                .map(|r| r.id.clone());
            if let Some(id) = id {
                let result = wyj_store::registry::remove_registry(&id);
                if let Some(dialog) = &mut state.mcp_dialog {
                    match result {
                        Ok(()) => dialog.refresh_registries(),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        McpMenuAction::Browse(McpBrowseAction::Install) => {
            let server = state
                .mcp_dialog
                .as_ref()
                .and_then(|d| d.browse_results.get(idx).cloned());
            if let Some(server) = server {
                let package = wyj_store::mcp_install::choose_package(&server.packages);
                if let Some(dialog) = &mut state.mcp_dialog {
                    if matches!(
                        package,
                        wyj_store::mcp_install::PackageChoice::Unsupported { .. }
                    ) {
                        dialog.error = Some(wyj_i18n::tr("mcp.install.unsupported_package"));
                    } else {
                        dialog.overlay = McpOverlay::InstallConfirm {
                            server: Box::new(server),
                            package,
                            scope: wyj_store::InstallScope::Global,
                        };
                    }
                }
            }
        }
    }
}

// ── Skill 管理面板：/skills 命令触发 ───────────────────────────────────────────

const BUILTIN_SKILL_NAMES: [&str; 5] = ["run", "review", "fix", "explain", "commit"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsDialogTab {
    Installed,
    Marketplaces,
    Browse,
}

/// Installed tab 的一行：内置 skill / 全局或项目 skill 文件 + lockfile 纳管信息
pub struct SkillInstalledRow {
    pub name: String,
    pub description: String,
    /// None = 内置 skill
    pub scope: Option<wyj_store::InstallScope>,
    pub builtin: bool,
    pub managed: Option<wyj_store::lockfile::InstalledSkillEntry>,
}

pub enum SkillsOverlay {
    None,
    /// 文本内容存在 `SkillsDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddMarketplace,
    Syncing {
        marketplace_id: String,
        git_url: String,
    },
    InstallConfirm {
        marketplace_id: String,
        git_url: String,
        entry: wyj_store::marketplace::MarketplaceSkillEntry,
        scope: wyj_store::InstallScope,
    },
    Upgrading {
        row_idx: usize,
    },
    /// 只读详情浮层（Installed 行菜单的"查看详情"），Enter/Esc 都直接关闭
    Detail {
        title: String,
        lines: Vec<String>,
    },
}

/// Installed 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsRowAction {
    ToggleEnabled,
    Upgrade,
    Uninstall,
    ViewDetail,
}

/// Marketplaces 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsSourceAction {
    OpenBrowse,
    Remove,
}

/// Browse 结果行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsBrowseAction {
    Install,
}

/// 三个 tab 各自的行菜单动作，用外层枚举包住
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillsMenuAction {
    Installed(SkillsRowAction),
    Marketplaces(SkillsSourceAction),
    Browse(SkillsBrowseAction),
}

/// 扫描一个 skill 目录，返回 (name, description) 列表（description 取首个 `# 标题` 行）
fn scan_skill_dir_for_display(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let description = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
            })
            .unwrap_or_else(|| name.clone());
        out.push((name, description));
    }
    out
}

/// Skill 管理面板状态（/skills 命令触发）
pub struct SkillsDialog {
    pub tab: SkillsDialogTab,
    pub installed: Vec<SkillInstalledRow>,
    /// 扁平化行游标，语义随当前 tab 变化（见 `rows()`），Left/Right 切 tab 时归零
    pub cursor: usize,
    pub marketplaces: Vec<wyj_store::lockfile::MarketplaceSource>,
    /// Browse tab 当前展示的条目来自哪个 marketplace（Marketplaces tab 里选中
    /// "浏览"菜单项触发同步后记录，供后续安装该 tab 里条目时回填来源）
    pub active_marketplace_id: String,
    pub active_marketplace_git_url: String,
    pub browse_results: Vec<wyj_store::marketplace::MarketplaceSkillEntry>,
    pub overlay: SkillsOverlay,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<FlatRow, SkillsMenuAction>>,
    pub error: Option<String>,
    pub status: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Skills(_)` 生效期间使用）
    pub live_input: InputBox,
}

impl SkillsDialog {
    fn new(home: &std::path::Path, cwd: &std::path::Path) -> Self {
        let global_lock = wyj_store::lockfile::load_global().unwrap_or_default();
        let project_lock = wyj_store::lockfile::load_project(cwd).unwrap_or_default();

        let mut installed = Vec::new();
        for name in BUILTIN_SKILL_NAMES {
            installed.push(SkillInstalledRow {
                name: name.to_string(),
                description: wyj_i18n::tr(&format!("skill.{name}.desc")),
                scope: None,
                builtin: true,
                managed: None,
            });
        }
        let global_dir = home.join(".wyj-code").join("skills");
        for (name, description) in scan_skill_dir_for_display(&global_dir) {
            let managed = global_lock.skills.iter().find(|e| e.name == name).cloned();
            installed.push(SkillInstalledRow {
                name,
                description,
                scope: Some(wyj_store::InstallScope::Global),
                builtin: false,
                managed,
            });
        }
        let project_dir = cwd.join(".wyj").join("skills");
        for (name, description) in scan_skill_dir_for_display(&project_dir) {
            let managed = project_lock.skills.iter().find(|e| e.name == name).cloned();
            installed.push(SkillInstalledRow {
                name,
                description,
                scope: Some(wyj_store::InstallScope::Project),
                builtin: false,
                managed,
            });
        }

        // 首次打开面板（全局 lockfile 里一条 marketplace 源都没有）时自动预置
        // 默认源，逻辑与 McpDialog 的 `ensure_default_registry` 完全对应。
        let marketplaces = wyj_store::marketplace::ensure_default_marketplace()
            .unwrap_or_else(|_| global_lock.marketplaces.clone());

        Self {
            tab: SkillsDialogTab::Installed,
            installed,
            cursor: 0,
            marketplaces,
            active_marketplace_id: String::new(),
            active_marketplace_git_url: String::new(),
            browse_results: Vec::new(),
            overlay: SkillsOverlay::None,
            menu: None,
            error: None,
            status: None,
            live_input: InputBox::new(),
        }
    }

    fn refresh_installed(&mut self, home: &std::path::Path, cwd: &std::path::Path) {
        let fresh = Self::new(home, cwd);
        self.installed = fresh.installed;
        self.marketplaces = fresh.marketplaces;
        self.clamp_cursor();
    }

    /// 扁平化行列表：当前 tab 下的条目下标，或该 tab 的固定"+ 添加"行。
    /// tab 头不算进 rows()，单独渲染；Installed/Browse 没有 AddNew。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            SkillsDialogTab::Installed => (0..self.installed.len()).map(FlatRow::Entry).collect(),
            SkillsDialogTab::Marketplaces => {
                let mut r: Vec<_> = (0..self.marketplaces.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            SkillsDialogTab::Browse => (0..self.browse_results.len()).map(FlatRow::Entry).collect(),
        }
    }

    pub fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn build_menu(&self) -> Option<ActionMenu<FlatRow, SkillsMenuAction>> {
        let row = *self.rows().get(self.cursor)?;
        match (self.tab, row) {
            (SkillsDialogTab::Installed, FlatRow::Entry(idx)) => {
                let r = self.installed.get(idx)?;
                let enabled = r.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                let can_upgrade = r.managed.as_ref().is_some_and(|m| m.is_managed());
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if enabled {
                            "skills.menu.disable"
                        } else {
                            "skills.menu.enable"
                        }),
                        action: SkillsMenuAction::Installed(SkillsRowAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.upgrade"),
                        action: SkillsMenuAction::Installed(SkillsRowAction::Upgrade),
                        dangerous: false,
                        disabled: !can_upgrade,
                        disabled_reason: (!can_upgrade)
                            .then(|| wyj_i18n::tr("skills.error.manual_no_upgrade")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.uninstall"),
                        action: SkillsMenuAction::Installed(SkillsRowAction::Uninstall),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.view_detail"),
                        action: SkillsMenuAction::Installed(SkillsRowAction::ViewDetail),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (SkillsDialogTab::Marketplaces, FlatRow::Entry(_)) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.browse_source"),
                        action: SkillsMenuAction::Marketplaces(SkillsSourceAction::OpenBrowse),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("skills.menu.remove_source"),
                        action: SkillsMenuAction::Marketplaces(SkillsSourceAction::Remove),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (SkillsDialogTab::Browse, FlatRow::Entry(_)) => {
                let items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("skills.menu.install"),
                    action: SkillsMenuAction::Browse(SkillsBrowseAction::Install),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }];
                Some(ActionMenu::new(row, items))
            }
            _ => None,
        }
    }
}

/// `/skills` 面板操作菜单的按键分发，结构与 `mcp_handle_menu_key` 完全对称。
fn skills_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(SkillsMenuAction),
        Execute(SkillsMenuAction, FlatRow),
    }

    let step = {
        let Some(dialog) = &mut state.skills_dialog else {
            return;
        };
        let Some(menu) = &mut dialog.menu else {
            return;
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
            if let Some(dialog) = &mut state.skills_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.skills_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.skills_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.skills_dialog {
                dialog.menu = None;
            }
            skills_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

fn skills_execute_menu_action(
    state: &mut AppState,
    action: SkillsMenuAction,
    target: FlatRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    let FlatRow::Entry(idx) = target else {
        return;
    };
    let home = wyj_config::home_dir().unwrap_or_default();
    match action {
        SkillsMenuAction::Installed(SkillsRowAction::ToggleEnabled) => {
            let info = state.skills_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).map(|row| {
                    let enabled = row.managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                    (
                        row.name.clone(),
                        row.scope.unwrap_or(wyj_store::InstallScope::Global),
                        enabled,
                    )
                })
            });
            if let Some((name, scope, currently_enabled)) = info {
                let result = wyj_store::skill_install::set_skill_enabled(
                    &name,
                    scope,
                    &state.cwd,
                    !currently_enabled,
                );
                if let Some(dialog) = &mut state.skills_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&home, &state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        SkillsMenuAction::Installed(SkillsRowAction::Upgrade) => {
            let info = state.skills_dialog.as_ref().and_then(|d| {
                d.installed.get(idx).and_then(|row| {
                    row.managed
                        .as_ref()
                        .is_some_and(|m| m.is_managed())
                        .then(|| {
                            (
                                row.name.clone(),
                                row.scope.unwrap_or(wyj_store::InstallScope::Global),
                            )
                        })
                })
            });
            if let Some((name, scope)) = info {
                if let Some(dialog) = &mut state.skills_dialog {
                    dialog.overlay = SkillsOverlay::Upgrading { row_idx: idx };
                }
                let tx = agent_tx.clone();
                let cwd = state.cwd.clone();
                tokio::spawn(async move {
                    let result = wyj_store::skill_install::upgrade_skill(&name, scope, &cwd)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::SkillUpgraded {
                            row_idx: idx,
                            result,
                        })
                        .await;
                });
            } else if let Some(dialog) = &mut state.skills_dialog {
                dialog.error = Some(wyj_i18n::tr("skills.error.manual_no_upgrade"));
            }
        }
        SkillsMenuAction::Installed(SkillsRowAction::Uninstall) => {
            let info = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| {
                    (
                        row.name.clone(),
                        row.scope.unwrap_or(wyj_store::InstallScope::Global),
                    )
                });
            if let Some((name, scope)) = info {
                let result = wyj_store::skill_install::uninstall_skill(&name, scope, &state.cwd);
                if let Some(dialog) = &mut state.skills_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status = Some(wyj_i18n::tr("skills.uninstall.done"));
                            dialog.refresh_installed(&home, &state.cwd);
                        }
                        Err(e) => {
                            dialog.error = Some(wyj_i18n::tr_fmt(
                                "skills.error.uninstall_failed",
                                &[("err", &e.to_string())],
                            ));
                        }
                    }
                }
            }
        }
        SkillsMenuAction::Installed(SkillsRowAction::ViewDetail) => {
            if let Some(dialog) = &mut state.skills_dialog {
                if let Some(row) = dialog.installed.get(idx) {
                    let mut lines = vec![
                        wyj_i18n::tr_fmt("skills.detail.name_line", &[("name", &row.name)]),
                        wyj_i18n::tr_fmt(
                            "skills.detail.description_line",
                            &[("description", &row.description)],
                        ),
                    ];
                    let scope_text = if row.builtin {
                        wyj_i18n::tr("agents.builtin_tag")
                    } else {
                        row.scope.map(mcp_scope_label_text).unwrap_or_default()
                    };
                    lines.push(wyj_i18n::tr_fmt(
                        "skills.detail.scope_line",
                        &[("scope", &scope_text)],
                    ));
                    if let Some(version) = row.managed.as_ref().and_then(|m| m.version.clone()) {
                        lines.push(wyj_i18n::tr_fmt(
                            "skills.detail.version_line",
                            &[("version", &version)],
                        ));
                    }
                    dialog.overlay = SkillsOverlay::Detail {
                        title: wyj_i18n::tr("skills.detail.title"),
                        lines,
                    };
                }
            }
        }
        SkillsMenuAction::Marketplaces(SkillsSourceAction::OpenBrowse) => {
            let info = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| (m.id.clone(), m.git_url.clone()));
            if let Some((marketplace_id, git_url)) = info {
                if let Some(dialog) = &mut state.skills_dialog {
                    dialog.overlay = SkillsOverlay::Syncing {
                        marketplace_id: marketplace_id.clone(),
                        git_url: git_url.clone(),
                    };
                }
                let tx = agent_tx.clone();
                let git_url_for_task = git_url.clone();
                tokio::spawn(async move {
                    let result = wyj_store::marketplace::sync_marketplace(&git_url_for_task)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::SkillMarketplaceSynced {
                            marketplace_id,
                            git_url: git_url_for_task,
                            result,
                        })
                        .await;
                });
            }
        }
        SkillsMenuAction::Marketplaces(SkillsSourceAction::Remove) => {
            let id = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| m.id.clone());
            if let Some(id) = id {
                let result = wyj_store::marketplace::remove_marketplace(&id);
                if let Some(dialog) = &mut state.skills_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&home, &state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        SkillsMenuAction::Browse(SkillsBrowseAction::Install) => {
            let entry = state
                .skills_dialog
                .as_ref()
                .and_then(|d| d.browse_results.get(idx).cloned());
            if let Some(entry) = entry {
                if let Some(dialog) = &mut state.skills_dialog {
                    dialog.overlay = SkillsOverlay::InstallConfirm {
                        marketplace_id: dialog.active_marketplace_id.clone(),
                        git_url: dialog.active_marketplace_git_url.clone(),
                        entry,
                        scope: wyj_store::InstallScope::Global,
                    };
                }
            }
        }
    }
}

// ── 插件管理面板：/plugins 命令触发 ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginsDialogTab {
    Installed,
    Marketplaces,
    Browse,
}

/// Installed tab 的一行：整体启用/禁用，不拆分内部 commands/agents/mcpServers。
pub struct PluginInstalledRow {
    pub name: String,
    pub version: Option<String>,
    pub scope: wyj_store::InstallScope,
    pub enabled: bool,
    pub is_local_dev: bool,
    /// 如 "cmd:3 agent:1 mcp:2"，供展示用（不用 emoji，对齐全部面板的纯文字风格）
    pub resource_summary: String,
    pub entry: wyj_store::lockfile::InstalledPluginEntry,
}

pub enum PluginOverlay {
    None,
    /// 文本内容存在 `PluginsDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddMarketplace,
    Syncing {
        marketplace_id: String,
    },
    InstallConfirm {
        marketplace_id: String,
        location: String,
        entry: Box<wyj_store::plugin_manifest::PluginMarketplaceEntry>,
        scope: wyj_store::InstallScope,
    },
    Installing,
    InstallReport {
        report: wyj_store::plugin_install::PluginInstallReport,
    },
    Upgrading {
        row_idx: usize,
    },
    /// 文本内容存在 `PluginsDialog.live_input`（借用底部主输入框，见 `InputOwner`）
    AddLocalPlugin,
    /// 只读详情浮层（Installed 行菜单的"查看详情"），Enter/Esc 都直接关闭
    Detail {
        title: String,
        lines: Vec<String>,
    },
}

/// Installed 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsRowAction {
    ToggleEnabled,
    Upgrade,
    Uninstall,
    ViewDetail,
}

/// Marketplaces 行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsSourceAction {
    OpenBrowse,
    Remove,
}

/// Browse 结果行菜单动作
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsBrowseAction {
    Install,
}

/// 三个 tab 各自的行菜单动作，用外层枚举包住
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PluginsMenuAction {
    Installed(PluginsRowAction),
    Marketplaces(PluginsSourceAction),
    Browse(PluginsBrowseAction),
}

/// 插件管理面板状态（/plugins 命令触发）
pub struct PluginsDialog {
    pub tab: PluginsDialogTab,
    pub installed: Vec<PluginInstalledRow>,
    /// 扁平化行游标，语义随当前 tab 变化（见 `rows()`），Left/Right 切 tab 时归零
    pub cursor: usize,
    pub marketplaces: Vec<wyj_store::lockfile::PluginMarketplaceSource>,
    /// Browse tab 当前展示的条目来自哪个 marketplace（Marketplaces tab 里选中
    /// "浏览"菜单项触发同步后记录，供后续安装该 tab 里条目时回填来源）
    pub active_marketplace_id: String,
    pub active_marketplace_location: String,
    pub browse_results: Vec<wyj_store::plugin_manifest::PluginMarketplaceEntry>,
    pub overlay: PluginOverlay,
    /// 选中列表条目回车后弹出的操作菜单；None = 未打开
    pub menu: Option<ActionMenu<FlatRow, PluginsMenuAction>>,
    pub error: Option<String>,
    pub status: Option<String>,
    /// 底部主输入框借用态下的草稿内容（`InputOwner::Plugins(_)` 生效期间使用）
    pub live_input: InputBox,
}

impl PluginsDialog {
    fn new(cwd: &std::path::Path) -> Self {
        let global_lock = wyj_store::lockfile::load_global().unwrap_or_default();
        let project_lock = wyj_store::lockfile::load_project(cwd).unwrap_or_default();

        let mut installed = Vec::new();
        for entry in global_lock
            .plugins
            .iter()
            .chain(project_lock.plugins.iter())
        {
            installed.push(PluginInstalledRow {
                name: entry.name.clone(),
                version: entry.version.clone(),
                scope: entry.scope,
                enabled: entry.enabled,
                is_local_dev: entry.is_local_dev(),
                resource_summary: format!(
                    "cmd:{} agent:{} mcp:{}",
                    entry.contributes.skill_paths.len(),
                    entry.contributes.agent_paths.len(),
                    entry.contributes.mcp_servers.len(),
                ),
                entry: entry.clone(),
            });
        }

        // 与 skill/mcp 不同：不预置默认插件市场源（没有已知稳定维护、遵循官方
        // marketplace.json 格式的公开仓库可以放心硬编码），面板首次打开时可能
        // 一个市场源都没有，需要用户自行通过"+ 添加"新增。
        let marketplaces =
            wyj_store::plugin_install::list_plugin_marketplaces().unwrap_or_default();

        Self {
            tab: PluginsDialogTab::Installed,
            installed,
            cursor: 0,
            marketplaces,
            active_marketplace_id: String::new(),
            active_marketplace_location: String::new(),
            browse_results: Vec::new(),
            overlay: PluginOverlay::None,
            menu: None,
            error: None,
            status: None,
            live_input: InputBox::new(),
        }
    }

    fn refresh_installed(&mut self, cwd: &std::path::Path) {
        let fresh = Self::new(cwd);
        self.installed = fresh.installed;
        self.marketplaces = fresh.marketplaces;
        self.clamp_cursor();
    }

    /// 扁平化行列表：当前 tab 下的条目下标，或该 tab 的固定"+ 添加"行。
    /// tab 头不算进 rows()，单独渲染；Installed 的 AddNew 行语义是"添加本地
    /// 插件"（不同于 Mcp/Skills 的"添加 source"，但落到同一个 FlatRow 形状上）；
    /// Browse 没有 AddNew。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            PluginsDialogTab::Installed => {
                let mut r: Vec<_> = (0..self.installed.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            PluginsDialogTab::Marketplaces => {
                let mut r: Vec<_> = (0..self.marketplaces.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            PluginsDialogTab::Browse => {
                (0..self.browse_results.len()).map(FlatRow::Entry).collect()
            }
        }
    }

    pub fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn build_menu(&self) -> Option<ActionMenu<FlatRow, PluginsMenuAction>> {
        let row = *self.rows().get(self.cursor)?;
        match (self.tab, row) {
            (PluginsDialogTab::Installed, FlatRow::Entry(idx)) => {
                let r = self.installed.get(idx)?;
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if r.enabled {
                            "plugins.menu.disable"
                        } else {
                            "plugins.menu.enable"
                        }),
                        action: PluginsMenuAction::Installed(PluginsRowAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.upgrade"),
                        action: PluginsMenuAction::Installed(PluginsRowAction::Upgrade),
                        dangerous: false,
                        disabled: r.is_local_dev,
                        disabled_reason: r
                            .is_local_dev
                            .then(|| wyj_i18n::tr("plugins.error.local_no_upgrade")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.uninstall"),
                        action: PluginsMenuAction::Installed(PluginsRowAction::Uninstall),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.view_detail"),
                        action: PluginsMenuAction::Installed(PluginsRowAction::ViewDetail),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (PluginsDialogTab::Marketplaces, FlatRow::Entry(_)) => {
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.browse_source"),
                        action: PluginsMenuAction::Marketplaces(PluginsSourceAction::OpenBrowse),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("plugins.menu.remove_source"),
                        action: PluginsMenuAction::Marketplaces(PluginsSourceAction::Remove),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            (PluginsDialogTab::Browse, FlatRow::Entry(_)) => {
                let items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("plugins.menu.install"),
                    action: PluginsMenuAction::Browse(PluginsBrowseAction::Install),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }];
                Some(ActionMenu::new(row, items))
            }
            _ => None,
        }
    }
}

/// `/plugins` 面板操作菜单的按键分发，结构与 `mcp_handle_menu_key` 完全对称。
fn plugins_handle_menu_key(
    state: &mut AppState,
    code: KeyCode,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(PluginsMenuAction),
        Execute(PluginsMenuAction, FlatRow),
    }

    let step = {
        let Some(dialog) = &mut state.plugins_dialog else {
            return;
        };
        let Some(menu) = &mut dialog.menu else {
            return;
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
            if let Some(dialog) = &mut state.plugins_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.plugins_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.plugins_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.plugins_dialog {
                dialog.menu = None;
            }
            plugins_execute_menu_action(state, action, target, agent_tx);
        }
    }
}

fn plugins_execute_menu_action(
    state: &mut AppState,
    action: PluginsMenuAction,
    target: FlatRow,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) {
    let FlatRow::Entry(idx) = target else {
        return;
    };
    match action {
        PluginsMenuAction::Installed(PluginsRowAction::ToggleEnabled) => {
            let info = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| (row.name.clone(), row.scope, row.enabled));
            if let Some((name, scope, enabled)) = info {
                let result = wyj_store::plugin_install::set_plugin_enabled(
                    &name, scope, &state.cwd, !enabled,
                );
                if let Some(dialog) = &mut state.plugins_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status =
                                Some(wyj_i18n::tr("plugins.toggle.restart_required_hint"));
                            dialog.refresh_installed(&state.cwd);
                        }
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        PluginsMenuAction::Installed(PluginsRowAction::Upgrade) => {
            let info = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .filter(|row| !row.is_local_dev)
                .map(|row| (row.name.clone(), row.scope));
            if let Some((name, scope)) = info {
                if let Some(dialog) = &mut state.plugins_dialog {
                    dialog.overlay = PluginOverlay::Upgrading { row_idx: idx };
                }
                let tx = agent_tx.clone();
                let cwd = state.cwd.clone();
                tokio::spawn(async move {
                    let result = wyj_store::plugin_install::upgrade_plugin(&name, scope, &cwd)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::PluginUpgraded {
                            row_idx: idx,
                            result,
                        })
                        .await;
                });
            } else if let Some(dialog) = &mut state.plugins_dialog {
                dialog.error = Some(wyj_i18n::tr("plugins.error.local_no_upgrade"));
            }
        }
        PluginsMenuAction::Installed(PluginsRowAction::Uninstall) => {
            let info = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.installed.get(idx))
                .map(|row| (row.name.clone(), row.scope));
            if let Some((name, scope)) = info {
                let result = wyj_store::plugin_install::uninstall_plugin(&name, scope, &state.cwd);
                if let Some(dialog) = &mut state.plugins_dialog {
                    match result {
                        Ok(()) => {
                            dialog.status = Some(wyj_i18n::tr("plugins.uninstall.done"));
                            dialog.refresh_installed(&state.cwd);
                        }
                        Err(e) => {
                            dialog.error = Some(wyj_i18n::tr_fmt(
                                "plugins.error.uninstall_failed",
                                &[("err", &e.to_string())],
                            ));
                        }
                    }
                }
            }
        }
        PluginsMenuAction::Installed(PluginsRowAction::ViewDetail) => {
            if let Some(dialog) = &mut state.plugins_dialog {
                if let Some(row) = dialog.installed.get(idx) {
                    let mut lines = vec![
                        wyj_i18n::tr_fmt("plugins.detail.name_line", &[("name", &row.name)]),
                        wyj_i18n::tr_fmt(
                            "plugins.detail.scope_line",
                            &[("scope", &mcp_scope_label_text(row.scope))],
                        ),
                    ];
                    if let Some(version) = &row.version {
                        lines.push(wyj_i18n::tr_fmt(
                            "plugins.detail.version_line",
                            &[("version", version)],
                        ));
                    }
                    lines.push(wyj_i18n::tr_fmt(
                        "plugins.detail.resources_line",
                        &[("summary", &row.resource_summary)],
                    ));
                    lines.push(wyj_i18n::tr_fmt(
                        "plugins.detail.path_line",
                        &[("path", &row.entry.plugin_root.display().to_string())],
                    ));
                    dialog.overlay = PluginOverlay::Detail {
                        title: wyj_i18n::tr("plugins.detail.title"),
                        lines,
                    };
                }
            }
        }
        PluginsMenuAction::Marketplaces(PluginsSourceAction::OpenBrowse) => {
            let marketplace_id = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| m.id.clone());
            if let Some(marketplace_id) = marketplace_id {
                if let Some(dialog) = &mut state.plugins_dialog {
                    dialog.overlay = PluginOverlay::Syncing {
                        marketplace_id: marketplace_id.clone(),
                    };
                }
                let tx = agent_tx.clone();
                let id_for_task = marketplace_id.clone();
                tokio::spawn(async move {
                    let result = wyj_store::plugin_install::sync_plugin_marketplace(&id_for_task)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(AgentEvent::PluginMarketplaceSynced {
                            marketplace_id,
                            result,
                        })
                        .await;
                });
            }
        }
        PluginsMenuAction::Marketplaces(PluginsSourceAction::Remove) => {
            let id = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.marketplaces.get(idx))
                .map(|m| m.id.clone());
            if let Some(id) = id {
                let result = wyj_store::plugin_install::remove_plugin_marketplace(&id);
                if let Some(dialog) = &mut state.plugins_dialog {
                    match result {
                        Ok(()) => dialog.refresh_installed(&state.cwd),
                        Err(e) => dialog.error = Some(e.to_string()),
                    }
                }
            }
        }
        PluginsMenuAction::Browse(PluginsBrowseAction::Install) => {
            let entry = state
                .plugins_dialog
                .as_ref()
                .and_then(|d| d.browse_results.get(idx).cloned());
            if let Some(entry) = entry {
                if let Some(dialog) = &mut state.plugins_dialog {
                    dialog.overlay = PluginOverlay::InstallConfirm {
                        marketplace_id: dialog.active_marketplace_id.clone(),
                        location: dialog.active_marketplace_location.clone(),
                        entry: Box::new(entry),
                        scope: wyj_store::InstallScope::Global,
                    };
                }
            }
        }
    }
}

/// @ 文件选取器候选项
#[derive(Clone)]
pub struct FileEntry {
    pub display: String,
    pub rel_path: String,
    pub is_dir: bool,
}

/// 待发送附件（图片或文件）
#[derive(Debug, Clone)]
pub enum Attachment {
    Image {
        media_type: String,
        data: String,
        preview_label: String,
    },
    File {
        path: std::path::PathBuf,
    },
}

// ── 工具展示帮助函数 ─────────────────────────────────────────────────────────

/// 从工具输入 JSON 中提取用于展示的参数字符串（如文件路径或命令）
fn tool_display_arg(name: &str, input: &serde_json::Value) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let shorten = |s: &str| -> String {
        if !home.is_empty() && s.starts_with(&home) {
            format!("~{}", &s[home.len()..])
        } else {
            s.to_string()
        }
    };
    let trunc = |s: String, max: usize| -> String {
        if s.chars().count() > max {
            format!(
                "{}…",
                s.chars().take(max.saturating_sub(1)).collect::<String>()
            )
        } else {
            s
        }
    };

    // 工具注册名是 PascalCase（如 "Read"/"WebFetch"，见 crates/tools/src/*.rs 的
    // Tool::name()），这里统一转小写后再匹配，否则下面的分支永远命中不了。
    let name = name.to_lowercase();
    match name.as_str() {
        "read" | "write" | "edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            trunc(shorten(path), 55)
        }
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            trunc(cmd.to_string(), 50)
        }
        "glob" => {
            let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
            trunc(p.to_string(), 40)
        }
        "grep" => {
            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            trunc(format!("{pat} in {}", shorten(path)), 50)
        }
        "webfetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            trunc(url.to_string(), 55)
        }
        _ => String::new(),
    }
}

/// 从工具输出生成一行摘要（用于 ⎿ 行）
fn tool_result_summary(name: &str, output: &str, is_error: bool) -> String {
    let trunc1 = |s: &str| -> String {
        let s = s.trim();
        if s.chars().count() > 65 {
            format!("{}…", s.chars().take(64).collect::<String>())
        } else {
            s.to_string()
        }
    };

    if is_error {
        return trunc1(output.lines().next().unwrap_or(output));
    }

    // 同上：工具注册名是 PascalCase，先转小写再匹配。
    let name = name.to_lowercase();
    match name.as_str() {
        "read" => {
            let n = output.lines().count();
            format!("read {n} lines")
        }
        "edit" => trunc1(output.lines().next().unwrap_or("updated")),
        "write" => trunc1(output.lines().next().unwrap_or("written")),
        "bash" => {
            let nonempty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            let n = nonempty.len();
            let first = nonempty.first().copied().unwrap_or("(no output)");
            if n > 1 {
                format!("{} (+{} lines)", trunc1(first), n - 1)
            } else {
                trunc1(first)
            }
        }
        "grep" => {
            let n = output.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} matches")
        }
        "glob" => {
            let n = output.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{n} files")
        }
        "webfetch" => format!("fetched {} bytes", output.len()),
        _ => trunc1(
            first_informative_line(output)
                .or_else(|| output.lines().next())
                .unwrap_or(output),
        ),
    }
}

/// 兜底摘要用：跳过无信息量的结构性行（JSON/数组输出的 `{`、`[` 等），
/// 取第一条含字母/数字/CJK 的行。全都是结构行时返回 None（由调用方回退首行）。
fn first_informative_line(output: &str) -> Option<&str> {
    output.lines().find(|l| {
        let t = l.trim();
        !t.is_empty() && t.chars().any(|c| c.is_alphanumeric())
    })
}

/// 判断 [`tool_result_summary`] 对给定工具/结果是否直接复用了 `content` 的第一行原文
/// （而非合成的统计文案，如 "read N lines"/"N matches"）。用于展开正文时跳过重复的首行，
/// 必须与 `tool_result_summary` 的分支保持一致——兜底分支跳过结构性首行取更有信息量
/// 的行时（此时摘要不等于首行），同样不做去重。
fn summary_reuses_first_line(name: &str, output: &str, is_error: bool) -> bool {
    if is_error {
        return true;
    }
    match name.to_lowercase().as_str() {
        "read" | "grep" | "glob" | "webfetch" => false,
        _ => {
            let first_nonempty = output.lines().find(|l| !l.trim().is_empty());
            match (first_nonempty, first_informative_line(output)) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
    }
}

#[cfg(test)]
mod tool_summary_tests {
    use super::*;

    /// 回归测试：工具注册名是 PascalCase（Tool::name()），这两个函数曾用小写/下划线
    /// 字符串匹配，导致真实调用（"Read"/"WebFetch" 等）永远落到兜底分支——
    /// Read 的摘要变成"文件第一行内容"，和下面展开的详情第一行完全重复。
    #[test]
    fn tool_result_summary_matches_pascal_case_tool_names() {
        let read_output = "1\tfirst line\n2\tsecond line\n";
        assert_eq!(
            tool_result_summary("Read", read_output, false),
            "read 2 lines"
        );
        assert_ne!(
            tool_result_summary("Read", read_output, false),
            read_output.lines().next().unwrap()
        );

        assert_eq!(tool_result_summary("Grep", "a\nb\nc\n", false), "3 matches");
        assert_eq!(tool_result_summary("Glob", "a\nb\n", false), "2 files");
        assert_eq!(
            tool_result_summary("WebFetch", "hello", false),
            "fetched 5 bytes"
        );
    }

    /// 回归测试：`summary_reuses_first_line` 必须与 `tool_result_summary` 的分支
    /// 保持一致——合成统计文案（read/grep/glob/webfetch）不会与正文重复，无需去重；
    /// 其余分支（含 is_error）都是直接复用正文首行，展开正文时必须跳过首行。
    #[test]
    fn summary_reuses_first_line_matches_tool_result_summary_branches() {
        let plain = "first line\nsecond line\n";
        for name in ["Read", "Grep", "Glob", "WebFetch"] {
            assert!(
                !summary_reuses_first_line(name, plain, false),
                "{name} 的摘要是合成统计文案，不应判定为复用首行"
            );
        }
        for name in ["Bash", "Edit", "Write", "SomeUnknownTool"] {
            assert!(
                summary_reuses_first_line(name, plain, false),
                "{name} 的摘要复用了正文首行原文"
            );
        }
        assert!(
            summary_reuses_first_line("Read", plain, true),
            "is_error 时摘要恒为正文首行，与工具名无关"
        );
    }

    /// MCP/JSON 输出的首行往往是无信息量的 `{`：兜底摘要必须跳过结构性行取
    /// 第一条有内容的行，且此时不做展开去重（摘要不等于正文首行）。
    #[test]
    fn fallback_summary_skips_structural_first_line() {
        let json = "{\n  \"code\": 0,\n  \"data\": []\n}\n";
        assert_eq!(
            tool_result_summary("mcp__a-stock__get_quote", json, false),
            "\"code\": 0,"
        );
        assert!(
            !summary_reuses_first_line("mcp__a-stock__get_quote", json, false),
            "摘要取的不是首行原文，展开正文不应再去重"
        );

        // 纯结构输出（不含任何字母数字）回退到首行，仍按复用首行去重
        let braces = "{\n}\n";
        assert_eq!(tool_result_summary("SomeUnknownTool", braces, false), "{");
        assert!(summary_reuses_first_line("SomeUnknownTool", braces, false));
    }

    #[test]
    fn tool_display_arg_matches_pascal_case_tool_names() {
        let input = serde_json::json!({"file_path": "/tmp/x.rs"});
        assert_eq!(tool_display_arg("Read", &input), "/tmp/x.rs");

        let bash_input = serde_json::json!({"command": "ls -la"});
        assert_eq!(tool_display_arg("Bash", &bash_input), "ls -la");

        let fetch_input = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            tool_display_arg("WebFetch", &fetch_input),
            "https://example.com"
        );
    }
}

#[cfg(test)]
mod navigation_focus_tests {
    use super::*;

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        )
    }

    #[test]
    fn repeated_direction_keys_move_selection_without_debounce() {
        let mut state = make_state();
        state.messages.push(ChatMessage::user("one".to_string()));
        state
            .messages
            .push(ChatMessage::assistant("two".to_string()));
        state
            .messages
            .push(ChatMessage::system("three".to_string()));

        state.move_focus_selection(1);
        assert_eq!(state.selected_message_id, Some(state.messages[0].id));

        state.move_focus_selection(1);
        assert_eq!(state.selected_message_id, Some(state.messages[1].id));

        state.move_focus_selection(1);
        assert_eq!(state.selected_message_id, Some(state.messages[2].id));
    }

    #[test]
    fn message_selection_and_toggle_targets_current_summary() {
        let mut state = make_state();
        state.messages.push(ChatMessage::tool_result(
            "first\nbody".to_string(),
            false,
            0.1,
            1,
            "Read".to_string(),
            "read 2 lines".to_string(),
            false,
        ));
        state.messages.push(ChatMessage::tool_result(
            "second\nbody".to_string(),
            false,
            0.2,
            2,
            "Bash".to_string(),
            "second".to_string(),
            true,
        ));

        state.move_message_selection(1);
        let first_id = state.selected_message_id;
        state.toggle_selected_message();
        assert!(state.messages[0].expanded);
        assert!(!state.messages[1].expanded);

        state.move_message_selection(1);
        assert_ne!(state.selected_message_id, first_id);
        state.toggle_selected_message();
        assert!(state.messages[0].expanded);
        assert!(state.messages[1].expanded);
    }

    #[test]
    fn message_selection_visits_plain_messages_without_toggling_them() {
        let mut state = make_state();
        state.messages.push(ChatMessage::user("hi".to_string()));
        state
            .messages
            .push(ChatMessage::assistant("hello".to_string()));
        state
            .messages
            .push(ChatMessage::system("system notice".to_string()));

        state.move_message_selection(1);
        assert_eq!(state.selected_message_id, Some(state.messages[0].id));

        state.toggle_selected_message();
        assert_eq!(state.selected_message_id, Some(state.messages[0].id));
        assert_eq!(state.last_toggled_message_id, None);

        state.move_message_selection(1);
        assert_eq!(state.selected_message_id, Some(state.messages[1].id));
        state.move_message_selection(1);
        assert_eq!(state.selected_message_id, Some(state.messages[2].id));
    }

    #[test]
    fn selected_expanded_message_detail_scrolls_independently() {
        let mut state = make_state();
        let id = state.push_message(ChatMessage::bash_output(
            (1..=30)
                .map(|i| format!("line-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0.2,
        ));
        state.selected_message_id = Some(id);
        state.messages[0].expanded = true;

        assert!(state.scroll_selected_message_detail(8));
        assert_eq!(state.message_detail_scroll.get(&id), Some(&8));

        assert!(state.scroll_selected_message_detail(-3));
        assert_eq!(state.message_detail_scroll.get(&id), Some(&5));
    }

    #[test]
    fn mouse_scroll_moves_chat_view_and_restores_follow_tail_at_bottom() {
        let mut state = make_state();
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;
        state.chat_follow_tail = true;
        state.selected_message_id = Some(42);

        state.scroll_chat_lines(-3);
        assert_eq!(state.chat_scroll, 7);
        assert!(!state.chat_follow_tail);
        assert_eq!(state.selected_message_id, None);

        state.unseen_messages = true;
        state.scroll_chat_lines(50);
        assert_eq!(state.chat_scroll, 20);
        assert!(state.chat_follow_tail);
        assert!(!state.unseen_messages);
    }

    #[test]
    fn scroll_focus_routes_to_selected_message_detail_before_chat() {
        let mut state = make_state();
        let id = state.push_message(ChatMessage::bash_output(
            (1..=30)
                .map(|i| format!("line-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            0,
            0.2,
        ));
        state.messages[0].expanded = true;
        state.selected_message_id = Some(id);
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.message_detail_scroll.get(&id), Some(&3));
        assert_eq!(state.chat_scroll, 10);
    }

    #[test]
    fn focus_selection_routes_to_todo_items() {
        let mut state = make_state();
        state.current_todos = Some(vec![
            wyj_tools::todo::TodoItem {
                id: "a".to_string(),
                content: "first".to_string(),
                status: wyj_tools::todo::TodoStatus::Pending,
                priority: None,
                active_form: None,
            },
            wyj_tools::todo::TodoItem {
                id: "b".to_string(),
                content: "second".to_string(),
                status: wyj_tools::todo::TodoStatus::Pending,
                priority: None,
                active_form: None,
            },
        ]);
        state.selected_todo_id = Some("a".to_string());
        state.ui_focus = UiFocus::Todos;

        state.move_focus_selection(1);

        assert_eq!(state.selected_todo_id.as_deref(), Some("b"));
        assert_eq!(state.ui_focus, UiFocus::Todos);
    }

    #[test]
    fn focus_selection_and_scroll_routes_to_agents_catalog() {
        let mut state = make_state();
        state.agents_dialog = Some(AgentsDialog::new(wyj_core::builtin_defs()));
        state.ui_focus = UiFocus::AgentsCatalog;

        assert!(state.agents_dialog.as_ref().unwrap().detail_open);

        state.move_focus_selection(1);
        assert_eq!(state.agents_dialog.as_ref().unwrap().selected, 1);
        state.scroll_focus_lines(8);

        assert_eq!(state.agents_dialog.as_ref().unwrap().selected, 1);
        assert_eq!(state.agents_dialog.as_ref().unwrap().detail_scroll, 8);
    }

    #[test]
    fn explicit_conversation_jump_sets_selection_anchor() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::thinking("a\nb\nc".to_string()));
        state.messages.push(ChatMessage::tool_result(
            "result".to_string(),
            false,
            0.1,
            1,
            "Read".to_string(),
            "read".to_string(),
            false,
        ));

        state.select_conversation_start();
        assert_eq!(
            state.selected_message_anchor,
            Some(ChatSelectionAnchor::Top)
        );
        assert_eq!(state.selected_message_id, Some(state.messages[0].id));

        state.select_conversation_end();
        assert_eq!(
            state.selected_message_anchor,
            Some(ChatSelectionAnchor::Bottom)
        );
        assert_eq!(state.selected_message_id, Some(state.messages[1].id));
        assert!(!state.chat_follow_tail);
    }

    #[test]
    fn toggle_returns_to_last_toggled_message_after_selection_is_cleared() {
        let mut state = make_state();
        state.messages.push(ChatMessage::tool_result(
            "result\nbody".to_string(),
            false,
            0.1,
            1,
            "Read".to_string(),
            "read".to_string(),
            false,
        ));

        state.move_message_selection(1);
        let id = state.selected_message_id;
        state.toggle_selected_message();
        assert!(state.messages[0].expanded);
        assert_eq!(state.last_toggled_message_id, id);

        state.selected_message_id = None;
        state.toggle_selected_message();
        assert!(!state.messages[0].expanded);
        assert_eq!(state.selected_message_id, id);
        assert_eq!(
            state.selected_message_anchor,
            Some(ChatSelectionAnchor::Top)
        );
    }

    #[test]
    fn new_session_reset_unfreezes_welcome() {
        let mut state = make_state();
        state.welcome_frozen = true;
        state.frozen_up_to = 7;
        state.chat_scroll = 4;
        state.chat_max_scroll = 9;
        state.chat_follow_tail = false;
        state.unseen_messages = true;
        state.total_input_tokens = 11;
        state.total_output_tokens = 13;
        state.context_tokens = 17;
        state.turns = 2;
        state
            .messages
            .push(ChatMessage::assistant("old".to_string()));
        state.pending_queue.push(("queued".to_string(), vec![]));

        state.reset_for_new_session();
        state
            .messages
            .push(ChatMessage::system("已开始新会话".to_string()));

        assert!(!state.welcome_frozen);
        assert_eq!(state.frozen_up_to, 0);
        assert_eq!(state.chat_scroll, 0);
        assert!(state.chat_follow_tail);
        assert!(!state.unseen_messages);
        assert_eq!(state.total_input_tokens, 0);
        assert_eq!(state.total_output_tokens, 0);
        assert_eq!(state.context_tokens, 0);
        assert_eq!(state.turns, 0);
        assert!(state.pending_queue.is_empty());
        assert_eq!(state.messages.len(), 1);
        assert!(matches!(state.messages[0].role, MessageRole::System));
    }

    #[test]
    fn push_user_keeps_welcome_available_until_scrollback_freezes() {
        let mut state = make_state();
        state.push_user("hello".to_string());

        assert!(!state.welcome_frozen);
        assert_eq!(state.frozen_up_to, 0);
        assert!(matches!(state.messages[0].role, MessageRole::User));
    }
}

// ── 附件辅助函数 ──────────────────────────────────────────────────────────────

/// 将 arboard 返回的 RGBA 字节编码为 PNG 字节序列
fn encode_rgba_to_png(bytes: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut encoder = png::Encoder::new(&mut buf, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(bytes)?;
    drop(writer);
    Ok(buf)
}

/// 判断粘贴文本是否像文件路径（绝对路径或以 ~/ ./ 开头，单行）
fn looks_like_file_path(s: &str) -> bool {
    let s = s.trim();
    !s.contains('\n') && (s.starts_with('/') || s.starts_with("~/") || s.starts_with("./"))
}

/// 展开 ~/ 前缀并检查文件是否存在，仅当是普通文件时返回 Some
fn try_resolve_path(s: &str) -> Option<std::path::PathBuf> {
    let s = s.trim();
    if !looks_like_file_path(s) {
        return None;
    }
    let path = if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        std::path::PathBuf::from(format!("{home}/{rest}"))
    } else {
        std::path::PathBuf::from(s)
    };
    if path.exists() && path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// 粘贴后在输入框光标处显示的瞬时提示
#[derive(Debug, Clone)]
pub(crate) struct PasteHint {
    pub(crate) text: String,
    pub(crate) expires_at: Instant,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
}

const PASTE_HINT_DURATION: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSelectionAnchor {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiFocus {
    Chat,
    Todos,
    AgentsCatalog,
    SubAgents,
}

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
            error: None,
        };
        dialog.refresh(cwd);
        dialog
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
        }
    }
}

/// 全局 UI 状态
pub struct AppState {
    pub messages: Vec<ChatMessage>,
    /// 下一个待分配的消息 id。
    pub next_message_id: u64,
    pub streaming_buf: String,
    /// extended thinking 流式累积（正文开始时固化为 Thinking 消息）
    pub thinking_buf: String,
    pub thinking_started: Option<std::time::Instant>,
    pub is_thinking: bool,
    pub permission_dialog: Option<PermissionDialog>,
    pub ask_question_dialog: Option<AskQuestionDialog>,
    /// ExitPlanMode 触发的计划批准对话框
    pub plan_dialog: Option<PlanApprovalDialog>,
    /// 检测到计划已批准仍在 plan 模式发消息时的确认对话框
    pub exec_mode_confirm: Option<ExecModeConfirmDialog>,
    /// 真实 scrollback 冻结边界。`messages[..frozen_up_to]` 已通过
    /// `Terminal::insert_before` 写入终端原生回滚区，不再参与每帧重绘。
    pub frozen_up_to: usize,
    /// 欢迎页是否已被显式抑制，如 /clear、会话切换/恢复。
    pub welcome_frozen: bool,
    /// 当前选中的可展开概要消息 id。
    pub selected_message_id: Option<u64>,
    /// 应用内消息流纵向滚动偏移。
    pub chat_scroll: usize,
    /// 当前聊天区可视行数，由渲染层每帧回写，供活跃尾部滚动/选中定位使用。
    pub chat_view_height: usize,
    /// 当前聊天区最大滚动偏移，由渲染层每帧回写。
    pub chat_max_scroll: usize,
    /// 是否跟随最新消息到底部。用户选中历史消息后置 false。
    pub chat_follow_tail: bool,
    /// 渲染时记录的选中消息起始行，用于保持选中项可见。
    pub selected_message_line: Option<usize>,
    /// 选中消息的滚动锚点。Home/End 在输入框为空时使用它把选中项贴到首/末行。
    pub selected_message_anchor: Option<ChatSelectionAnchor>,
    /// 最近一次展开/收起的消息。展开后若用户滚走，下一次 Enter 仍可回到该块并收起。
    pub last_toggled_message_id: Option<u64>,
    /// 每条可展开消息的详情区滚动偏移，按消息 id 保存。
    pub message_detail_scroll: HashMap<u64, u16>,
    /// 是否有新消息在用户查看历史时到达。
    pub unseen_messages: bool,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// 当前会话实际上下文大小估算（`estimate_tokens(&session.messages)`），
    /// 用于状态栏上下文占比显示。与 total_input_tokens（跨轮次累加的历史
    /// 用量总和，用于 /cost 与单轮增量展示）是两个不同的量：后者只增不减，
    /// 压缩后也不会反映真实上下文缩小，因此不能拿来算占比。
    pub context_tokens: u32,
    pub cwd: PathBuf,
    pub should_quit: bool,
    pub turns: usize,
    /// 当前 spinner 动画帧索引
    pub spinner_frame: usize,
    /// 累计工具调用次数
    pub tool_call_count: usize,
    /// 工具 id → (名称, 序号)，用于将 ToolEnd 与 ToolStart 关联
    pub tool_info: HashMap<String, (String, usize)>,
    pub model_name: String,
    pub context_window: u32,
    pub mode: AgentMode,
    pub current_task: Option<AbortHandle>,
    pub ctrl_c_pressed: bool,
    pub last_ctrl_c: Option<Instant>,
    pub last_esc: Option<Instant>,
    /// Slash 命令补全候选列表（命令名, 描述）
    pub slash_completions: Vec<(String, String)>,
    /// 当前选中的补全项索引
    pub slash_selected: usize,
    /// 输入历史（每次 submit 追加）
    pub input_history: Vec<String>,
    /// 当前历史导航索引（None = 未在导航模式）
    pub history_idx: Option<usize>,
    /// 进入历史导航态之前用户正在编辑的草稿快照（Down 翻回最新之后恢复用）
    pub history_draft: Option<String>,
    /// 当前任务列表快照（TodoWrite 更新），用于底部固定面板渲染
    pub current_todos: Option<Vec<TodoItem>>,
    /// 任务面板是否处于展开态（仅在 is_todo_collapsible 为真时生效）
    pub todo_panel_expanded: bool,
    /// 任务列表当前选中项（按 id 而非 index，避免列表变动时错位）
    pub selected_todo_id: Option<String>,
    /// 选中任务详情是否展开
    pub todo_detail_open: bool,
    /// 任务详情滚动偏移（预留给长详情，渲染层按需 clamp）
    pub todo_detail_scroll: u16,
    /// 每条任务的运行时统计（耗时/token），按 TodoItem.id 索引
    pub todo_stats: HashMap<String, TodoRuntimeStats>,
    /// 每条任务执行期间产生的消息流事件，按 TodoItem.id 索引。
    pub todo_execution_logs: HashMap<String, Vec<TodoExecutionEntry>>,
    /// 会话选择器（/sessions 命令触发时 Some）
    pub session_picker: Option<SessionPickerState>,
    /// 设置面板（/config 命令触发时 Some）
    pub settings_dialog: Option<SettingsDialog>,
    /// 分组管理面板（/model 无参命令触发时 Some）
    pub profile_dialog: Option<ProfileDialog>,
    /// CLAUDE.md 记忆面板（/memory 命令触发时 Some）
    pub memory_dialog: Option<MemoryDialog>,
    /// MCP server 管理面板（/mcp 命令触发时 Some）
    pub mcp_dialog: Option<McpDialog>,
    /// Skill 管理面板（/skills 命令触发时 Some）
    pub skills_dialog: Option<SkillsDialog>,
    /// 插件管理面板（/plugins 命令触发时 Some）
    pub plugins_dialog: Option<PluginsDialog>,
    /// 可用 Agent 类型面板（/agents 命令触发时 Some）
    pub agents_dialog: Option<AgentsDialog>,
    /// 统一 Skill/MCP/Plugin 资源面板（/extensions 命令触发时 Some）
    pub extensions_dialog: Option<ExtensionsDialog>,
    /// 标记当前轮次完成后需保存 session 文件
    pub save_needed: bool,
    /// 待发送附件列表（图片或文件，发送时附到消息）
    pub pending_attachments: Vec<Attachment>,
    /// @ 文件选取器候选列表
    pub file_completions: Vec<FileEntry>,
    /// 当前选中的文件选取器项索引
    pub file_selected: usize,
    /// @ 选取器当前浏览目录
    pub at_browse_dir: PathBuf,
    /// 当前正在执行的操作名（工具调用时为 "ToolName(arg)"，LLM 思考时为 None）
    pub current_op: Option<String>,
    /// 本轮对话开始时间（用于计算耗时）
    pub turn_start_time: Option<Instant>,
    /// 本轮对话开始时的 input_tokens 快照
    pub turn_start_input_tokens: u32,
    /// 本轮对话开始时的 output_tokens 快照
    pub turn_start_output_tokens: u32,
    /// 最近一轮 AI 交互耗时。当前轮运行时由 `turn_start_time` 实时计算，完成后落这里。
    pub last_turn_elapsed_secs: Option<f64>,
    /// 最近一轮 AI 交互输入 token 增量。
    pub last_turn_input_tokens: u32,
    /// 最近一轮 AI 交互输出 token 增量。
    pub last_turn_output_tokens: u32,
    /// 当前运行中 Agent 任务的补充信息注入通道（is_thinking 期间提交的消息走这里）
    pub injector: Option<mpsc::UnboundedSender<(Vec<ContentBlock>, InjectionKind)>>,
    /// 最近一次粘贴的瞬时提示（在输入框光标处显示）
    pub(crate) paste_hint: Option<PasteHint>,
    /// 排队中尚未被 Agent 消费的补充消息（文本 + 附件），用于状态栏提示计数、
    /// 消费后回放到对话流、以及轮次已结束但仍有残留时的兜底重发
    pub pending_queue: Vec<(String, Vec<Attachment>)>,
    /// 当前生效的完整配置（/config 设置面板的数据来源与保存目标）
    pub config: Config,
    /// 子 Agent 实时状态（key = Hub 分配的 id，BTreeMap 保证面板按启动顺序排列）
    pub sub_agents: BTreeMap<u64, SubAgentUiState>,
    /// 当前会话的子 Agent trace 事件缓存，按子 Agent id 索引。
    pub sub_agent_trace_cache: BTreeMap<u64, Vec<TraceEvent>>,
    /// 当前 TUI 对应的 session id，供 /subagents 详情按需读取 trace 文件。
    pub current_session_id: String,
    /// session 文件目录，也是子 Agent trace 旁路文件所在根目录。
    pub sessions_dir: Option<PathBuf>,
    /// agents 面板当前选中项（按 id 而非 index，避免列表变动时错位）
    pub selected_sub_agent: Option<u64>,
    /// 选中项是否已展开详情（工具调用流水 + 最终结果）
    pub sub_agent_detail_open: bool,
    /// 详情内容的行级滚动偏移（渲染时按可视行数 clamp 并写回）
    pub sub_agent_detail_scroll: u16,
    /// 后台子 Agent 完成时主 Agent 空闲，暂存的 system-reminder，下轮起手注入
    pub pending_bg_reminders: Vec<String>,
    /// 子 Agent 累计 token 用量（与主 session 分开统计，/cost 单列）
    pub sub_input_tokens: u32,
    pub sub_output_tokens: u32,
    /// 子 Agent 生命周期管理中心（中断/退出清理用）
    pub hub: Arc<wyj_tools::SubAgentHub>,
    /// 欢迎页 tip 轮播索引：进程启动时选定一次，本次生命周期内保持不变
    pub welcome_tip_idx: usize,
    /// 空输入时方向键的归属区域
    pub ui_focus: UiFocus,
    /// 主输入框当前借给了谁（None = 属于聊天），见 `InputOwner`
    pub input_owner: Option<InputOwner>,
    /// 每个已配置 MCP server 的后台连接状态，供 `/mcp` Installed tab 逐行展示；
    /// 与 mcp_dialog 是否打开无关（面板打开前后台可能早已连完/连挂）
    pub mcp_connection_status: HashMap<String, McpConnStatus>,
    /// 主 Agent 装配的 Hooks 执行器（子 Agent 不装配），供 `/hooks` 命令
    /// 展示与状态栏判断。
    pub hook_runner: Option<Arc<wyj_core::HookRunner>>,
}

/// 计算 `messages` 中从 `frozen_up_to` 起最多可以安全推进到的新冻结边界
/// （`messages[..new_bound]` 可以整体 `insert_before` 写入终端真实 scrollback，
/// 不再参与每帧重绘）。三条阻塞规则，任一命中就停在该位置（不含）：
///
/// 1. 该位置是 `ToolCall` 但其 `ToolResult` 尚未出现——并发工具调用可能乱序
///    完成，`ToolEnd` 会把结果 `insert` 在这条 `ToolCall` 之后，未落定前这一位置
///    之后的一切都可能被后续插入打乱，不能冻结。
/// 2. `collapsible_idx` 命中的位置（及其后）——Ctrl+O 仍需要能切换到它。
/// 3. 该位置关联的子 Agent（`sub_agent_id`）仍处于 `Running`——对应 ToolCall/
///    ToolResult 下面还在画实时状态行，不能冻结。
///
/// `collapsible_idx` 必须由调用方在 draw() 之前、drain agent_rx/sub_rx 之前算好
/// 传入（`render::last_collapsible_tool_result_idx` 的结果），不在这里自行重新
/// 扫描——多 Agent 并发场景下，若各处独立扫描，drain 期间新插入的 ToolResult
/// 会让"最后一条可折叠"发生跨帧漂移，导致规则②在两次调用之间保护到不同的
/// 位置。调用方需要保证这个下标与 Ctrl+O 实际读取的目标（`AppState.
/// last_collapsible_seq`）来自同一次扫描结果。
///
/// 返回值只增不减（`max(frozen_up_to)` 兜底），永远不会把已经冻结的内容
/// "退冻"。
fn compute_freezable_up_to(
    messages: &[ChatMessage],
    frozen_up_to: usize,
    sub_agents: &BTreeMap<u64, SubAgentUiState>,
    collapsible_idx: Option<usize>,
) -> usize {
    let collapsible_bound = collapsible_idx.unwrap_or(messages.len());
    let mut bound = messages.len().min(collapsible_bound);

    for (i, m) in messages.iter().enumerate().skip(frozen_up_to) {
        if i >= bound {
            break;
        }
        let sub_agent_running = m
            .sub_agent_id
            .and_then(|id| sub_agents.get(&id))
            .is_some_and(|s| s.status == SubAgentStatus::Running);
        match m.role {
            MessageRole::ToolCall => {
                let resolved = messages[i + 1..].iter().any(|r| {
                    matches!(r.role, MessageRole::ToolResult) && r.sequence_no == m.sequence_no
                });
                if !resolved || sub_agent_running {
                    bound = bound.min(i);
                    break;
                }
            }
            MessageRole::ToolResult if sub_agent_running => {
                bound = bound.min(i);
                break;
            }
            _ => {}
        }
    }
    bound.max(frozen_up_to)
}

/// Ctrl+O 的实际翻转逻辑：按 `sequence_no`（而非下标，见 `AppState.
/// last_collapsible_seq` 字段文档）精确定位目标 `ToolResult` 并翻转其
/// `expanded`。`seq` 为 `None`，或消息数组里找不到匹配的 `ToolResult`
/// （防御性场景，正常不会触发）时 no-op，不 panic、不误翻转其他消息。
fn is_selectable_message(_msg: &ChatMessage) -> bool {
    true
}

fn is_expandable_message(msg: &ChatMessage) -> bool {
    matches!(
        msg.role,
        MessageRole::Thinking | MessageRole::ToolResult | MessageRole::BashOutput
    )
}

#[cfg(test)]
fn toggle_last_collapsible(messages: &mut [ChatMessage], seq: Option<usize>) {
    let Some(seq) = seq else { return };
    if let Some(m) = messages
        .iter_mut()
        .find(|m| matches!(m.role, MessageRole::ToolResult) && m.sequence_no == Some(seq))
    {
        m.expanded = !m.expanded;
    }
}

impl AppState {
    pub(crate) fn new(
        cwd: PathBuf,
        model_name: String,
        context_window: u32,
        mode: AgentMode,
        config: Config,
        hub: Arc<wyj_tools::SubAgentHub>,
    ) -> Self {
        let welcome_tip_idx = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
            crate::welcome::pick_tip_index(nanos ^ (std::process::id() as u64))
        };
        Self {
            messages: vec![],
            next_message_id: 1,
            streaming_buf: String::new(),
            thinking_buf: String::new(),
            thinking_started: None,
            is_thinking: false,
            permission_dialog: None,
            ask_question_dialog: None,
            plan_dialog: None,
            exec_mode_confirm: None,
            frozen_up_to: 0,
            welcome_frozen: false,
            selected_message_id: None,
            chat_scroll: 0,
            chat_view_height: 0,
            chat_max_scroll: 0,
            chat_follow_tail: true,
            selected_message_line: None,
            selected_message_anchor: None,
            last_toggled_message_id: None,
            message_detail_scroll: HashMap::new(),
            unseen_messages: false,
            total_input_tokens: 0,
            total_output_tokens: 0,
            context_tokens: 0,
            cwd,
            should_quit: false,
            turns: 0,
            spinner_frame: 0,
            tool_call_count: 0,
            tool_info: HashMap::new(),
            model_name,
            context_window,
            mode,
            current_task: None,
            ctrl_c_pressed: false,
            last_ctrl_c: None,
            last_esc: None,
            slash_completions: vec![],
            slash_selected: 0,
            input_history: vec![],
            history_idx: None,
            history_draft: None,
            current_todos: None,
            todo_panel_expanded: false,
            selected_todo_id: None,
            todo_detail_open: false,
            todo_detail_scroll: 0,
            todo_stats: HashMap::new(),
            todo_execution_logs: HashMap::new(),
            session_picker: None,
            settings_dialog: None,
            profile_dialog: None,
            memory_dialog: None,
            mcp_dialog: None,
            skills_dialog: None,
            plugins_dialog: None,
            agents_dialog: None,
            extensions_dialog: None,
            save_needed: false,
            config,
            pending_attachments: vec![],
            file_completions: vec![],
            file_selected: 0,
            at_browse_dir: PathBuf::new(),
            current_op: None,
            turn_start_time: None,
            turn_start_input_tokens: 0,
            turn_start_output_tokens: 0,
            last_turn_elapsed_secs: None,
            last_turn_input_tokens: 0,
            last_turn_output_tokens: 0,
            injector: None,
            paste_hint: None,
            pending_queue: vec![],
            sub_agents: BTreeMap::new(),
            sub_agent_trace_cache: BTreeMap::new(),
            current_session_id: String::new(),
            sessions_dir: None,
            selected_sub_agent: None,
            sub_agent_detail_open: false,
            sub_agent_detail_scroll: 0,
            pending_bg_reminders: vec![],
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            hub,
            welcome_tip_idx,
            ui_focus: UiFocus::Chat,
            input_owner: None,
            mcp_connection_status: HashMap::new(),
            hook_runner: None,
        }
    }

    fn reset_for_new_session(&mut self) {
        self.messages.clear();
        self.next_message_id = 1;
        self.streaming_buf.clear();
        self.thinking_buf.clear();
        self.thinking_started = None;
        self.is_thinking = false;
        self.frozen_up_to = 0;
        // Historical sessions and /clear intentionally freeze the welcome screen;
        // a new session must return to the opening state instead of inheriting it.
        self.welcome_frozen = false;
        self.selected_message_id = None;
        self.chat_scroll = 0;
        self.chat_max_scroll = 0;
        self.chat_follow_tail = true;
        self.selected_message_line = None;
        self.selected_message_anchor = None;
        self.last_toggled_message_id = None;
        self.message_detail_scroll.clear();
        self.unseen_messages = false;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.context_tokens = 0;
        self.turns = 0;
        self.tool_call_count = 0;
        self.tool_info.clear();
        self.history_idx = None;
        self.history_draft = None;
        self.current_todos = None;
        self.todo_panel_expanded = false;
        self.selected_todo_id = None;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_stats.clear();
        self.todo_execution_logs.clear();
        self.agents_dialog = None;
        self.extensions_dialog = None;
        self.pending_attachments.clear();
        self.current_op = None;
        self.turn_start_time = None;
        self.turn_start_input_tokens = 0;
        self.turn_start_output_tokens = 0;
        self.last_turn_elapsed_secs = None;
        self.last_turn_input_tokens = 0;
        self.last_turn_output_tokens = 0;
        self.injector = None;
        self.pending_queue.clear();
        self.sub_agents.clear();
        self.sub_agent_trace_cache.clear();
        self.selected_sub_agent = None;
        self.sub_agent_detail_open = false;
        self.sub_agent_detail_scroll = 0;
        self.pending_bg_reminders.clear();
        self.sub_input_tokens = 0;
        self.sub_output_tokens = 0;
        self.ui_focus = UiFocus::Chat;
        self.input_owner = None;
    }

    pub fn ensure_message_ids(&mut self) {
        let mut inserted = false;
        for msg in &mut self.messages {
            if msg.id == 0 {
                msg.id = self.next_message_id;
                self.next_message_id += 1;
                inserted = true;
            }
        }
        if inserted && !self.chat_follow_tail {
            self.unseen_messages = true;
        }
        if let Some(id) = self.selected_message_id {
            let still_exists = self.selectable_message_ids().into_iter().any(|x| x == id);
            if !still_exists {
                self.selected_message_id = self.last_selectable_message_id();
            }
        }
    }

    fn selectable_message_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        let start = self.frozen_up_to.min(self.messages.len());
        let mut i = start;
        while i < self.messages.len() {
            let msg = &self.messages[i];
            if matches!(msg.role, MessageRole::ToolCall)
                && self.messages.get(i + 1).is_some_and(|next| {
                    matches!(next.role, MessageRole::ToolResult)
                        && next.sequence_no == msg.sequence_no
                })
            {
                i += 1;
                continue;
            }
            if is_selectable_message(msg) {
                ids.push(msg.id);
            }
            i += 1;
        }
        ids
    }

    fn first_selectable_message_id(&self) -> Option<u64> {
        self.selectable_message_ids().into_iter().next()
    }

    fn last_selectable_message_id(&self) -> Option<u64> {
        self.selectable_message_ids().into_iter().last()
    }

    fn last_expandable_message_id(&self) -> Option<u64> {
        self.messages
            .iter()
            .skip(self.frozen_up_to.min(self.messages.len()))
            .rev()
            .find(|m| is_expandable_message(m))
            .map(|m| m.id)
    }

    fn move_message_selection(&mut self, delta: i32) {
        self.ensure_message_ids();
        let ids = self.selectable_message_ids();
        if ids.is_empty() {
            self.selected_message_id = None;
            return;
        }
        let current = self
            .selected_message_id
            .and_then(|id| ids.iter().position(|x| *x == id));
        let idx = match (current, delta.cmp(&0)) {
            (Some(i), std::cmp::Ordering::Less) => i.saturating_sub(1),
            (Some(i), std::cmp::Ordering::Greater) => (i + 1).min(ids.len() - 1),
            (Some(i), _) => i,
            (None, std::cmp::Ordering::Less) => ids.len() - 1,
            (None, _) => 0,
        };
        self.selected_message_id = Some(ids[idx]);
        self.selected_message_anchor = None;
        self.chat_follow_tail = false;
        self.unseen_messages = false;
    }

    fn select_conversation_start(&mut self) {
        self.ensure_message_ids();
        self.selected_message_id = self.first_selectable_message_id();
        self.selected_message_anchor = Some(ChatSelectionAnchor::Top);
        self.chat_scroll = 0;
        self.chat_follow_tail = false;
        self.unseen_messages = false;
    }

    fn select_conversation_end(&mut self) {
        self.ensure_message_ids();
        self.selected_message_id = self.last_selectable_message_id();
        self.selected_message_anchor = Some(ChatSelectionAnchor::Bottom);
        self.chat_follow_tail = false;
        self.chat_scroll = self.chat_max_scroll;
        self.unseen_messages = false;
    }

    fn scroll_chat_lines(&mut self, delta: i32) {
        let amount = delta.unsigned_abs() as usize;
        if amount == 0 {
            return;
        }
        self.selected_message_id = None;
        self.selected_message_anchor = None;
        if delta < 0 {
            self.chat_scroll = self.chat_scroll.saturating_sub(amount);
            self.chat_follow_tail = false;
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

    fn scroll_selected_message_detail(&mut self, delta: i32) -> bool {
        let Some(id) = self.selected_message_id else {
            return false;
        };
        let Some(msg) = self
            .messages
            .iter()
            .skip(self.frozen_up_to.min(self.messages.len()))
            .find(|m| m.id == id && is_expandable_message(m) && m.expanded)
        else {
            return false;
        };
        let amount = delta.unsigned_abs() as u16;
        if amount == 0 {
            return true;
        }
        let entry = self.message_detail_scroll.entry(msg.id).or_insert(0);
        if delta < 0 {
            *entry = entry.saturating_sub(amount);
        } else {
            *entry = entry.saturating_add(amount);
        }
        self.selected_message_anchor = Some(ChatSelectionAnchor::Top);
        self.chat_follow_tail = false;
        self.unseen_messages = false;
        true
    }

    fn adjust_u16_scroll(value: &mut u16, delta: i32) {
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

    fn move_focus_selection(&mut self, delta: i32) {
        match self.ui_focus {
            UiFocus::Todos => self.move_selected_todo(delta),
            UiFocus::AgentsCatalog => {
                if let Some(dialog) = &mut self.agents_dialog {
                    dialog.move_selected(delta);
                } else {
                    self.ui_focus = UiFocus::Chat;
                    self.move_message_selection(delta);
                }
            }
            UiFocus::SubAgents if !self.sub_agents.is_empty() => {
                self.move_selected_sub_agent(delta);
            }
            _ if self.should_enter_sub_agent_focus_from_arrows() => {
                self.move_selected_sub_agent(delta);
            }
            _ => self.move_message_selection(delta),
        }
    }

    fn scroll_focus_lines(&mut self, delta: i32) {
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
                    self.ui_focus = UiFocus::Chat;
                    self.scroll_chat_lines(delta);
                }
            }
            UiFocus::Todos if self.todo_detail_open => {
                Self::adjust_u16_scroll(&mut self.todo_detail_scroll, delta);
            }
            UiFocus::SubAgents if self.sub_agent_detail_open => {
                Self::adjust_u16_scroll(&mut self.sub_agent_detail_scroll, delta);
            }
            UiFocus::Chat => {
                if !self.scroll_selected_message_detail(delta) {
                    self.scroll_chat_lines(delta);
                }
            }
            UiFocus::Todos | UiFocus::SubAgents => self.scroll_chat_lines(delta),
        }
    }

    fn assign_message_id(&mut self, msg: &mut ChatMessage) {
        if msg.id == 0 {
            msg.id = self.next_message_id;
            self.next_message_id += 1;
        }
    }

    fn push_message(&mut self, mut msg: ChatMessage) -> u64 {
        self.assign_message_id(&mut msg);
        let id = msg.id;
        self.messages.push(msg);
        id
    }

    fn insert_message_after(&mut self, idx: usize, mut msg: ChatMessage) -> u64 {
        self.assign_message_id(&mut msg);
        let id = msg.id;
        self.messages.insert(idx + 1, msg);
        id
    }

    fn active_todo_ids(&self) -> Vec<String> {
        self.current_todos
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .map(|t| t.id.clone())
            .collect()
    }

    fn link_message_to_active_todos(&mut self, message_id: u64) {
        for todo_id in self.active_todo_ids() {
            self.todo_execution_logs
                .entry(todo_id)
                .or_default()
                .push(TodoExecutionEntry::Message(message_id));
        }
    }

    fn push_tracked_message(&mut self, msg: ChatMessage) -> u64 {
        let id = self.push_message(msg);
        self.link_message_to_active_todos(id);
        id
    }

    fn insert_tracked_message_after(&mut self, idx: usize, msg: ChatMessage) -> u64 {
        let id = self.insert_message_after(idx, msg);
        self.link_message_to_active_todos(id);
        id
    }

    fn default_todo_id(&self) -> Option<String> {
        let items = self.current_todos.as_deref()?;
        items
            .iter()
            .find(|t| t.status == TodoStatus::InProgress)
            .or_else(|| items.iter().find(|t| t.status != TodoStatus::Completed))
            .or_else(|| items.last())
            .map(|t| t.id.clone())
    }

    fn ensure_selected_todo(&mut self) {
        let Some(items) = self.current_todos.as_deref() else {
            self.selected_todo_id = None;
            self.todo_detail_open = false;
            self.todo_detail_scroll = 0;
            return;
        };
        let selected_valid = self
            .selected_todo_id
            .as_deref()
            .is_some_and(|id| items.iter().any(|t| t.id == id));
        if !selected_valid {
            self.selected_todo_id = self.default_todo_id();
            self.todo_detail_scroll = 0;
        }
    }

    fn move_selected_todo(&mut self, delta: i32) {
        self.ensure_selected_todo();
        let Some(items) = self.current_todos.as_deref() else {
            return;
        };
        if items.is_empty() {
            self.selected_todo_id = None;
            return;
        }
        let current = self
            .selected_todo_id
            .as_deref()
            .and_then(|id| items.iter().position(|t| t.id == id))
            .unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, items.len() as i32 - 1) as usize;
        self.selected_todo_id = Some(items[next].id.clone());
        self.todo_detail_scroll = 0;
        self.chat_follow_tail = true;
        self.unseen_messages = false;
    }

    fn toggle_todo_detail(&mut self) {
        self.ensure_selected_todo();
        if self.selected_todo_id.is_some() {
            self.todo_detail_open = !self.todo_detail_open;
            self.todo_detail_scroll = 0;
            self.ui_focus = UiFocus::Todos;
        }
    }

    fn close_panel_focus(&mut self) -> bool {
        match self.ui_focus {
            UiFocus::Todos => {
                if self.todo_detail_open {
                    self.todo_detail_open = false;
                } else {
                    self.ui_focus = UiFocus::Chat;
                    self.selected_todo_id = None;
                }
                true
            }
            UiFocus::AgentsCatalog => {
                if let Some(dialog) = &mut self.agents_dialog {
                    if dialog.detail_open {
                        dialog.detail_open = false;
                    } else {
                        self.agents_dialog = None;
                        self.ui_focus = UiFocus::Chat;
                    }
                } else {
                    self.ui_focus = UiFocus::Chat;
                }
                true
            }
            UiFocus::SubAgents => {
                if self.selected_sub_agent.is_some() {
                    if self.sub_agent_detail_open {
                        self.sub_agent_detail_open = false;
                    } else {
                        self.selected_sub_agent = None;
                        self.ui_focus = UiFocus::Chat;
                    }
                    true
                } else {
                    self.ui_focus = UiFocus::Chat;
                    false
                }
            }
            UiFocus::Chat => false,
        }
    }

    fn move_selected_sub_agent(&mut self, delta: i32) {
        let ids: Vec<u64> = self.sub_agents.keys().copied().collect();
        if ids.is_empty() {
            self.selected_sub_agent = None;
            return;
        }
        let current = self
            .selected_sub_agent
            .and_then(|id| ids.iter().position(|x| *x == id))
            .unwrap_or_else(|| ids.len().saturating_sub(1));
        let next = (current as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize;
        self.selected_sub_agent = Some(ids[next]);
        self.sub_agent_detail_scroll = 0;
        self.ui_focus = UiFocus::SubAgents;
    }

    fn should_enter_sub_agent_focus_from_arrows(&self) -> bool {
        !self.sub_agents.is_empty()
            && self.ui_focus == UiFocus::Chat
            && self.selected_message_id.is_none()
            && self.selectable_message_ids().is_empty()
    }

    fn has_message_toggle_target(&mut self) -> bool {
        self.ensure_message_ids();
        if let Some(id) = self.selected_message_id {
            if self
                .messages
                .iter()
                .skip(self.frozen_up_to.min(self.messages.len()))
                .any(|m| m.id == id && is_expandable_message(m))
            {
                return true;
            }
            return false;
        }
        if let Some(id) = self.last_toggled_message_id {
            return self
                .messages
                .iter()
                .skip(self.frozen_up_to.min(self.messages.len()))
                .any(|m| m.id == id && is_expandable_message(m));
        }
        self.last_expandable_message_id().is_some()
    }

    fn toggle_selected_message(&mut self) {
        self.ensure_message_ids();
        if self.selected_message_id.is_none() {
            self.selected_message_id = self.last_toggled_message_id.and_then(|id| {
                self.messages
                    .iter()
                    .skip(self.frozen_up_to.min(self.messages.len()))
                    .any(|m| m.id == id && is_expandable_message(m))
                    .then_some(id)
            });
        }
        if self.selected_message_id.is_none() {
            self.selected_message_id = self.last_expandable_message_id();
        }
        let Some(id) = self.selected_message_id else {
            return;
        };
        if let Some(msg) = self
            .messages
            .iter_mut()
            .skip(self.frozen_up_to)
            .find(|m| m.id == id && is_expandable_message(m))
        {
            msg.expanded = !msg.expanded;
            self.last_toggled_message_id = Some(id);
            self.message_detail_scroll.insert(id, 0);
            self.selected_message_anchor = Some(ChatSelectionAnchor::Top);
            self.chat_follow_tail = false;
            self.unseen_messages = false;
        }
    }

    /// 是否有仍在运行的子 Agent（驱动 spinner 与底部聚合面板）
    pub fn has_running_sub_agents(&self) -> bool {
        self.sub_agents
            .values()
            .any(|s| s.status == SubAgentStatus::Running)
    }

    /// 是否有任意一个"重量级"管理对话框打开，需要切到全屏 alternate screen
    /// 渲染（这些都是用户主动触发的低频 slash 命令，需要比 Inline 常驻区更大的
    /// 空间；PermissionDialog 高频出现，已降级为底部常驻面板，不在此列）。
    pub fn wants_fullscreen(&self) -> bool {
        self.session_picker.is_some()
            || self.settings_dialog.is_some()
            || self.profile_dialog.is_some()
            || self.memory_dialog.is_some()
            || self.mcp_dialog.is_some()
            || self.skills_dialog.is_some()
            || self.plugins_dialog.is_some()
            || self.extensions_dialog.is_some()
            || self.agents_dialog.is_some()
    }

    /// 面板可见的子 Agent：本会话生命周期内全部保留（不再按完成时长过滤），
    /// 按 BTreeMap 自然顺序（启动顺序）排列
    pub fn visible_sub_agents(&self) -> Vec<(&u64, &SubAgentUiState)> {
        self.sub_agents.iter().collect()
    }

    pub fn sub_agent_trace_events(&mut self, id: u64) -> Option<Vec<TraceEvent>> {
        if !self.sub_agent_trace_cache.contains_key(&id) {
            let sessions_dir = self.sessions_dir.as_ref()?;
            let path = wyj_tools::trace::trace_file(sessions_dir, &self.current_session_id, id);
            if let Ok(events) = wyj_tools::trace::read_trace(&path) {
                self.sub_agent_trace_cache.insert(id, events);
            }
        }
        self.sub_agent_trace_cache.get(&id).cloned()
    }

    /// 中断当前正在运行的 Agent，保留已输出内容并标记 [已中断]
    fn interrupt(&mut self) {
        if let Some(h) = self.current_task.take() {
            h.abort();
        }
        // 前台子 Agent 一并中断（后台任务不受影响，继续运行）
        for id in self.hub.abort_foreground() {
            if let Some(s) = self.sub_agents.get_mut(&id) {
                if s.status == SubAgentStatus::Running {
                    s.status = SubAgentStatus::Interrupted;
                    s.final_elapsed = Some(s.started_at.elapsed().as_secs_f64());
                    s.current_tool = None;
                    s.finished_at = Some(Instant::now());
                }
            }
        }
        if self.is_thinking {
            if self.streaming_buf.is_empty() {
                self.streaming_buf = "[已中断]".to_string();
            } else {
                self.streaming_buf.push_str("\n\n[已中断]");
            }
            self.flush_streaming();
            self.is_thinking = false;
        }
        if let Some(dlg) = self.permission_dialog.take() {
            let _ = dlg.response_tx.send(wyj_tools::PermissionDecision::Deny);
        }
        if let Some(dlg) = self.ask_question_dialog.take() {
            let _ = dlg.response_tx.send(None);
        }
        if let Some(dlg) = self.plan_dialog.take() {
            let _ = dlg.response_tx.send(false);
        }
        self.exec_mode_confirm = None;
        self.pending_attachments.clear();
        self.injector = None;
        if !self.pending_queue.is_empty() {
            let n = self.pending_queue.len();
            self.pending_queue.clear();
            self.messages
                .push(ChatMessage::system(format!("已中断，{n} 条排队消息已丢弃")));
        }
    }

    fn push_user(&mut self, text: String) {
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        self.push_message(ChatMessage::user(text));
    }

    fn flush_streaming(&mut self) {
        self.flush_thinking();
        if !self.streaming_buf.is_empty() {
            let text = std::mem::take(&mut self.streaming_buf);
            self.push_tracked_message(ChatMessage::assistant(text));
        }
    }

    /// 把累积的 thinking 内容固化为一条消息流内容。
    fn flush_thinking(&mut self) {
        if !self.thinking_buf.is_empty() {
            let text = std::mem::take(&mut self.thinking_buf);
            self.thinking_buf.clear();
            self.thinking_started = None;
            self.push_tracked_message(ChatMessage::thinking(text));
        }
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(d) => {
                // thinking 结束、正文开始：先折叠思考行
                self.flush_thinking();
                self.streaming_buf.push_str(&d);
            }
            AgentEvent::ThinkingDelta(d) => {
                if self.thinking_buf.is_empty() {
                    self.thinking_started = Some(std::time::Instant::now());
                }
                self.thinking_buf.push_str(&d);
            }

            AgentEvent::ToolStart {
                id,
                name,
                input_json,
            } => {
                self.tool_call_count += 1;
                let seq = self.tool_call_count;
                let arg = tool_display_arg(&name, &input_json);
                let display = if arg.is_empty() {
                    name.clone()
                } else {
                    format!("{name}({arg})")
                };
                self.current_op = Some(display.clone());
                self.tool_info.insert(id, (name.clone(), seq));
                self.flush_streaming();
                let mut msg = ChatMessage::tool_call(display, seq);
                // ToolCall 也记录工具名，供 SubAgent Started 事件 FIFO 配对
                msg.tool_name = Some(name);
                self.push_tracked_message(msg);
            }

            AgentEvent::ToolEnd {
                id,
                output,
                is_error,
                elapsed_secs,
            } => {
                self.current_op = None;
                let (name, seq) = self.tool_info.remove(&id).unwrap_or_default();
                let summary = tool_result_summary(&name, &output, is_error);
                let summary_is_first_line = summary_reuses_first_line(&name, &output, is_error);
                // 找到对应的 ToolCall 消息位置：多个工具调用在同一轮里并发执行时，
                // ToolEnd 到达顺序未必与 ToolStart 一致，必须按 seq 定位插入点，
                // 而不是无条件 push 到列表尾部——否则并发调用会导致所有 ⏺ 标题
                // 先聚在一起，之后所有 ⎿ 结果再聚在一起，无法一一对应。
                let call_idx = self.messages.iter().rposition(|m| {
                    matches!(m.role, MessageRole::ToolCall) && m.sequence_no == Some(seq)
                });
                // Agent 工具：把 ToolCall 上绑定的子 Agent id 带到 ToolResult，
                // 供展开时渲染内部工具调用明细
                let sub_id = if name == "Agent" {
                    call_idx.and_then(|i| self.messages[i].sub_agent_id)
                } else {
                    None
                };
                let mut msg = ChatMessage::tool_result(
                    output,
                    is_error,
                    elapsed_secs,
                    seq,
                    name,
                    summary,
                    summary_is_first_line,
                );
                msg.sub_agent_id = sub_id;
                match call_idx {
                    Some(i) => {
                        self.insert_tracked_message_after(i, msg);
                    }
                    None => {
                        self.push_tracked_message(msg);
                    }
                }
                if let Some(said) = sub_id {
                    if let Some(s) = self.sub_agents.get_mut(&said) {
                        s.has_result = true;
                    }
                }
            }

            AgentEvent::ToolPermissionRequest {
                tool_name,
                action_summary,
                response_tx,
            } => {
                self.permission_dialog = Some(PermissionDialog {
                    tool_name,
                    action_summary,
                    response_tx,
                });
            }

            AgentEvent::TurnDone => {
                self.flush_streaming();
                self.is_thinking = false;
                self.current_op = None;
                self.turns += 1;
                self.save_needed = true;
                self.injector = None;
                if let Some(start) = self.turn_start_time.take() {
                    let elapsed = start.elapsed().as_secs_f64();
                    let d_in = self
                        .total_input_tokens
                        .saturating_sub(self.turn_start_input_tokens);
                    let d_out = self
                        .total_output_tokens
                        .saturating_sub(self.turn_start_output_tokens);
                    self.last_turn_elapsed_secs = Some(elapsed);
                    self.last_turn_input_tokens = d_in;
                    self.last_turn_output_tokens = d_out;
                    self.push_tracked_message(ChatMessage::turn_summary(elapsed, d_in, d_out));
                }
            }

            AgentEvent::Error(e) => {
                self.flush_streaming();
                if let Some(start) = self.turn_start_time.take() {
                    self.last_turn_elapsed_secs = Some(start.elapsed().as_secs_f64());
                    self.last_turn_input_tokens = self
                        .total_input_tokens
                        .saturating_sub(self.turn_start_input_tokens);
                    self.last_turn_output_tokens = self
                        .total_output_tokens
                        .saturating_sub(self.turn_start_output_tokens);
                }
                self.is_thinking = false;
                self.injector = None;
                self.push_tracked_message(ChatMessage::assistant_err(format!("[错误] {e}")));
            }

            AgentEvent::Injected => {
                if !self.pending_queue.is_empty() {
                    let (text, attachments) = self.pending_queue.remove(0);
                    let display_text = build_display_text(&text, &attachments);
                    self.push_user(display_text);
                }
            }

            AgentEvent::Usage {
                input,
                output,
                context_tokens,
            } => {
                self.total_input_tokens = input;
                self.total_output_tokens = output;
                self.context_tokens = context_tokens;
            }

            AgentEvent::UsageDelta {
                input_tokens,
                output_tokens,
            } => {
                let active_ids: Vec<String> = self
                    .current_todos
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter(|t| t.status == TodoStatus::InProgress)
                    .map(|t| t.id.clone())
                    .collect();
                if !active_ids.is_empty() {
                    let in_shares = split_evenly(input_tokens, active_ids.len());
                    let out_shares = split_evenly(output_tokens, active_ids.len());
                    for (i, id) in active_ids.iter().enumerate() {
                        let s = self.todo_stats.entry(id.clone()).or_default();
                        s.input_tokens += in_shares[i];
                        s.output_tokens += out_shares[i];
                    }
                }
                // active_ids 为空：没有任务处于 in_progress，静默丢弃，不计入任何任务
            }

            AgentEvent::TodoUpdate(items) => {
                let old_all_done = self.current_todos.as_ref().is_some_and(|old| {
                    !old.is_empty() && old.iter().all(|t| t.status == TodoStatus::Completed)
                });
                let new_all_done =
                    !items.is_empty() && items.iter().all(|t| t.status == TodoStatus::Completed);
                let is_new_round = is_new_todo_round(self.current_todos.as_deref(), &items);
                if is_new_round || (new_all_done && !old_all_done) {
                    // 新一轮任务开始，或本轮刚全部完成：重置为默认折叠态
                    self.todo_panel_expanded = false;
                }
                if is_new_round {
                    self.todo_stats.clear();
                    self.todo_execution_logs.clear();
                }

                // 状态转换检测：新一轮时旧状态视为空表（所有任务都视为之前不是
                // in_progress），否则用更新前的 current_todos 按 id 查旧状态。
                let old_status: HashMap<&str, &TodoStatus> = if is_new_round {
                    HashMap::new()
                } else {
                    self.current_todos
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|t| (t.id.as_str(), &t.status))
                        .collect()
                };
                for item in &items {
                    let was_in_progress = old_status
                        .get(item.id.as_str())
                        .is_some_and(|s| **s == TodoStatus::InProgress);
                    let is_in_progress = item.status == TodoStatus::InProgress;
                    if is_in_progress && !was_in_progress {
                        self.todo_stats
                            .entry(item.id.clone())
                            .or_default()
                            .started_at = Some(Instant::now());
                    } else if !is_in_progress && was_in_progress {
                        if let Some(s) = self.todo_stats.get_mut(&item.id) {
                            if let Some(start) = s.started_at.take() {
                                s.elapsed_secs += start.elapsed().as_secs_f64();
                            }
                        }
                    }
                }

                self.current_todos = Some(items);
                self.ensure_selected_todo();
            }

            AgentEvent::AskQuestions {
                questions,
                response_tx,
            } => {
                // 面板打开后拦截全部按键，用户不再有任何滚动手段；若沿用此前
                // 上滚/选中留下的视口位置，附加在聊天区尾部的选项区会被裁出
                // 可视范围且无法找回。这里强制回到贴底跟随并清掉选中锚点，
                // 保证选项与操作提示立即可见（同时满足 freeze_ready_scrollback
                // 的冻结前置条件，配合其面板期间的 collapsible 豁免生效）。
                self.chat_follow_tail = true;
                self.unseen_messages = false;
                self.selected_message_id = None;
                self.selected_message_anchor = None;
                self.ask_question_dialog = Some(AskQuestionDialog::new(questions, response_tx));
            }

            AgentEvent::BashResult {
                output,
                exit_code,
                elapsed_secs,
            } => {
                self.push_tracked_message(ChatMessage::bash_output(
                    output,
                    exit_code,
                    elapsed_secs,
                ));
            }

            AgentEvent::PlanApprovalRequest { plan, response_tx } => {
                // 注意：不要在这里把 is_thinking 设为 false。Agent 此刻仍在
                // `exit_plan_mode().await` 上挂起，TaskList / spinner 仍应继续
                // 表示"调研中"。若预先清掉 is_thinking，弹窗期间主循环的
                // `if state.is_thinking` 守卫会让 Ctrl+C 中断路径直接失效。
                // 批准分支会显式写回 true；取消分支走 interrupt() 自行恢复。
                // 计划正文作为普通消息并入应用内聊天流，
                // plan_dialog 只保留贴底的三选一选择器状态。
                self.push_tracked_message(ChatMessage::plan_proposal(plan));
                self.plan_dialog = Some(PlanApprovalDialog::new(response_tx));
            }

            AgentEvent::SubAgent(ev) => self.apply_sub_agent_event(ev),

            AgentEvent::TitleGenerated(title) => {
                // 后台标题生成完成 → 更新终端窗口标题（OSC 0）
                // 标题已由 SummaryGenerator 直接写盘，这里仅更新终端显示
                let _ = write!(io::stdout(), "\x1b]0;{title}\x07");
                let _ = io::stdout().flush();
            }

            AgentEvent::ModelsFetched {
                entry_idx,
                field_idx,
                result,
            } => {
                if let Some(dialog) = &mut self.profile_dialog {
                    let matches_pending = matches!(
                        dialog.overlay,
                        ProfileOverlay::FetchingModels {
                            entry_idx: e,
                            field_idx: f,
                        } if e == entry_idx && f == field_idx
                    );
                    if matches_pending {
                        dialog.overlay = ProfileOverlay::None;
                        match result {
                            Ok(models) if models.is_empty() => {
                                dialog.error = Some(wyj_i18n::tr("profile.fetch.empty"));
                            }
                            Ok(models) => {
                                let items = models
                                    .iter()
                                    .enumerate()
                                    .map(|(i, name)| ActionMenuItem {
                                        label: name.clone(),
                                        action: ProfileMenuAction::ModelChoice(i),
                                        dangerous: false,
                                        disabled: false,
                                        disabled_reason: None,
                                    })
                                    .collect();
                                dialog.pending_models = models;
                                dialog.menu = Some(ActionMenu::new(
                                    ProfileRow::Field(entry_idx, field_idx),
                                    items,
                                ));
                            }
                            Err(e) => {
                                dialog.error =
                                    Some(wyj_i18n::tr_fmt("profile.fetch.failed", &[("err", &e)]));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpRegistryFetched { result } => {
                if let Some(dialog) = &mut self.mcp_dialog {
                    if matches!(dialog.overlay, McpOverlay::Searching) {
                        dialog.overlay = McpOverlay::None;
                        match result {
                            Ok(results) => {
                                dialog.status = if results.is_empty() {
                                    Some(wyj_i18n::tr("mcp.dialog.no_results"))
                                } else {
                                    None
                                };
                                dialog.browse_results = results;
                                // rows() 里 Browse tab 的搜索行固定占位置 0，
                                // 第一条真实结果落在位置 1；没有结果时留在搜索行。
                                dialog.cursor = if dialog.browse_results.is_empty() {
                                    0
                                } else {
                                    1
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "mcp.error.search_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::SkillMarketplaceSynced {
                marketplace_id,
                git_url,
                result,
            } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    let matches_pending = matches!(
                        &dialog.overlay,
                        SkillsOverlay::Syncing { marketplace_id: id, .. } if *id == marketplace_id
                    );
                    if matches_pending {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok(entries) => {
                                dialog.active_marketplace_id = marketplace_id;
                                dialog.active_marketplace_git_url = git_url;
                                dialog.browse_results = entries;
                                dialog.tab = SkillsDialogTab::Browse;
                                dialog.cursor = 0;
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.sync_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpUpgraded { row_idx, result } => {
                if let Some(dialog) = &mut self.mcp_dialog {
                    let matches_pending = matches!(dialog.overlay, McpOverlay::Upgrading { row_idx: r } if r == row_idx);
                    if matches_pending {
                        dialog.overlay = McpOverlay::None;
                        match result {
                            Ok(wyj_store::UpgradeOutcome::Upgraded { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "mcp.upgrade.done",
                                    &[("version", &version)],
                                ));
                                dialog.refresh_installed(&self.config, &self.cwd);
                            }
                            Ok(wyj_store::UpgradeOutcome::AlreadyLatest { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "mcp.upgrade.already_latest",
                                    &[("version", &version)],
                                ));
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "mcp.error.upgrade_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::SkillUpgraded { row_idx, result } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    let matches_pending = matches!(
                        dialog.overlay,
                        SkillsOverlay::Upgrading { row_idx: r } if r == row_idx
                    );
                    if matches_pending {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok(wyj_store::UpgradeOutcome::Upgraded { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "skills.upgrade.done",
                                    &[("version", &version)],
                                ));
                                let home = wyj_config::home_dir().unwrap_or_default();
                                dialog.refresh_installed(&home, &self.cwd);
                            }
                            Ok(wyj_store::UpgradeOutcome::AlreadyLatest { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "skills.upgrade.already_latest",
                                    &[("version", &version)],
                                ));
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.upgrade_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpBackgroundConnected { name, tool_count } => {
                self.mcp_connection_status
                    .insert(name.clone(), McpConnStatus::Connected { tool_count });
                self.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                    "mcp.background.connected",
                    &[("name", &name), ("count", &tool_count.to_string())],
                )));
            }

            AgentEvent::McpBackgroundFailed { name, reason } => {
                let status = match &reason {
                    crate::event::McpConnFailReason::Error(_) => McpConnStatus::Failed,
                    crate::event::McpConnFailReason::Timeout => McpConnStatus::TimedOut,
                };
                self.mcp_connection_status.insert(name.clone(), status);
                let key = match &reason {
                    crate::event::McpConnFailReason::Error(_) => "mcp.background.failed_error",
                    crate::event::McpConnFailReason::Timeout => "mcp.background.failed_timeout",
                };
                let err_text = match &reason {
                    crate::event::McpConnFailReason::Error(e) => e.clone(),
                    crate::event::McpConnFailReason::Timeout => String::new(),
                };
                let mut msg = ChatMessage::system(wyj_i18n::tr_fmt(
                    key,
                    &[("name", &name), ("err", &err_text)],
                ));
                msg.is_error = true;
                self.messages.push(msg);
            }

            AgentEvent::PluginMarketplaceSynced {
                marketplace_id,
                result,
            } => {
                if let Some(dialog) = &mut self.plugins_dialog {
                    let matches_pending = matches!(
                        &dialog.overlay,
                        PluginOverlay::Syncing { marketplace_id: id } if *id == marketplace_id
                    );
                    if matches_pending {
                        dialog.overlay = PluginOverlay::None;
                        match result {
                            Ok(manifest) => {
                                dialog.active_marketplace_id = marketplace_id.clone();
                                dialog.active_marketplace_location = dialog
                                    .marketplaces
                                    .iter()
                                    .find(|m| m.id == marketplace_id)
                                    .map(|m| m.location.clone())
                                    .unwrap_or_default();
                                dialog.browse_results = manifest.plugins;
                                dialog.tab = PluginsDialogTab::Browse;
                                dialog.cursor = 0;
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "plugins.error.sync_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                        // sync 成功会更新 marketplace 的 display_name/owner_name/plugin_count，
                        // 重新读盘刷新一下列表展示。
                        dialog.marketplaces = wyj_store::plugin_install::list_plugin_marketplaces()
                            .unwrap_or_default();
                    }
                }
            }

            AgentEvent::PluginInstalled { result } => {
                if let Some(dialog) = &mut self.plugins_dialog {
                    if matches!(dialog.overlay, PluginOverlay::Installing) {
                        match result {
                            Ok(report) => {
                                dialog.refresh_installed(&self.cwd);
                                dialog.overlay = PluginOverlay::InstallReport { report };
                            }
                            Err(e) => {
                                dialog.overlay = PluginOverlay::None;
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "plugins.error.install_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::PluginUpgraded { row_idx, result } => {
                if let Some(dialog) = &mut self.plugins_dialog {
                    let matches_pending = matches!(
                        dialog.overlay,
                        PluginOverlay::Upgrading { row_idx: r } if r == row_idx
                    );
                    if matches_pending {
                        dialog.overlay = PluginOverlay::None;
                        match result {
                            Ok(wyj_store::UpgradeOutcome::Upgraded { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "plugins.upgrade.done",
                                    &[("version", &version)],
                                ));
                                dialog.refresh_installed(&self.cwd);
                            }
                            Ok(wyj_store::UpgradeOutcome::AlreadyLatest { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "plugins.upgrade.already_latest",
                                    &[("version", &version)],
                                ));
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "plugins.error.upgrade_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    /// 处理子 Agent 生命周期事件（更新 sub_agents 状态、绑定消息、路由后台结果）
    fn apply_sub_agent_event(&mut self, ev: wyj_tools::SubAgentEvent) {
        use wyj_tools::SubAgentEvent as E;
        match ev {
            E::Started {
                id,
                agent_type,
                description,
                background,
                // 落盘 trace 关联用，TUI 内存态摘要不需要
                parent_tool_use_id,
            } => {
                self.sub_agent_trace_cache
                    .entry(id)
                    .or_default()
                    .push(TraceEvent::Started {
                        agent_type: agent_type.clone(),
                        description: description.clone(),
                        background,
                        parent_tool_use_id,
                    });
                // FIFO 绑定最早一条未绑定的 Agent ToolCall 消息，并把内容改写为 类型(描述)
                if let Some(msg) = self.messages.iter_mut().find(|m| {
                    matches!(m.role, MessageRole::ToolCall)
                        && m.tool_name.as_deref() == Some("Agent")
                        && m.sub_agent_id.is_none()
                }) {
                    msg.sub_agent_id = Some(id);
                    msg.content = format!("{agent_type}({description})");
                }
                self.sub_agents.insert(
                    id,
                    SubAgentUiState {
                        agent_type,
                        description,
                        background,
                        status: SubAgentStatus::Running,
                        started_at: Instant::now(),
                        final_elapsed: None,
                        input_tokens: 0,
                        output_tokens: 0,
                        tool_calls: 0,
                        current_tool: None,
                        tool_log: vec![],
                        has_result: false,
                        finished_at: None,
                        final_result: None,
                    },
                );
            }
            E::ToolStart {
                id,
                tool_name,
                arg_summary,
                // 完整 input 只落盘（trace.rs），TUI 内存态摘要不保留全文（见
                // CLAUDE.md/plan：避免长会话下常驻全文内存暴涨）
                input,
            } => {
                let (input_json, truncated) = wyj_tools::trace::truncate_input(&input);
                self.sub_agent_trace_cache
                    .entry(id)
                    .or_default()
                    .push(TraceEvent::ToolStart {
                        tool_name: tool_name.clone(),
                        input_json,
                        truncated,
                    });
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.tool_calls += 1;
                    s.current_tool = Some(if arg_summary.is_empty() {
                        tool_name.clone()
                    } else {
                        format!("{tool_name}({arg_summary})")
                    });
                    s.tool_log.push(SubToolLine {
                        tool_name,
                        arg_summary,
                        is_error: false,
                        elapsed_secs: None,
                    });
                }
            }
            E::ToolEnd {
                id,
                tool_name,
                is_error,
                elapsed_secs,
                // 完整 output 只落盘（trace.rs），TUI 内存态摘要不保留全文
                output,
            } => {
                let (output, truncated) = wyj_tools::trace::truncate_output(&output);
                self.sub_agent_trace_cache
                    .entry(id)
                    .or_default()
                    .push(TraceEvent::ToolEnd {
                        tool_name: tool_name.clone(),
                        is_error,
                        elapsed_secs,
                        output,
                        truncated,
                    });
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.current_tool = None;
                    if let Some(line) = s
                        .tool_log
                        .iter_mut()
                        .rev()
                        .find(|l| l.elapsed_secs.is_none() && l.tool_name == tool_name)
                    {
                        line.is_error = is_error;
                        line.elapsed_secs = Some(elapsed_secs);
                    }
                }
            }
            E::Usage {
                id,
                input_tokens,
                output_tokens,
            } => {
                self.sub_agent_trace_cache
                    .entry(id)
                    .or_default()
                    .push(TraceEvent::Usage {
                        input_tokens,
                        output_tokens,
                    });
                self.sub_input_tokens += input_tokens;
                self.sub_output_tokens += output_tokens;
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.input_tokens += input_tokens;
                    s.output_tokens += output_tokens;
                }
            }
            E::Done {
                id,
                agent_type,
                description,
                result,
                is_error,
                elapsed_secs,
                background,
            } => {
                self.sub_agent_trace_cache
                    .entry(id)
                    .or_default()
                    .push(TraceEvent::Done {
                        result: result.clone(),
                        is_error,
                        elapsed_secs,
                    });
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.status = if is_error {
                        SubAgentStatus::Failed
                    } else {
                        SubAgentStatus::Done
                    };
                    s.final_elapsed = Some(elapsed_secs);
                    s.current_tool = None;
                    s.finished_at = Some(Instant::now());
                    s.final_result = Some(result.clone());
                }
                if background {
                    // 结果包成 system-reminder：主 Agent 忙则经注入通道在工具边界
                    // 送达；空闲则暂存，下一轮起手合并进 user 消息
                    let reminder = wyj_core::prompts::bg_agent_done_reminder(
                        &format!("a{id}"),
                        &agent_type,
                        &description,
                        &format_hms(elapsed_secs),
                        &result,
                    );
                    match &self.injector {
                        Some(tx) => {
                            let _ = tx.send((
                                vec![ContentBlock::Text { text: reminder }],
                                InjectionKind::SystemReminder,
                            ));
                        }
                        None => self.pending_bg_reminders.push(reminder),
                    }
                    self.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                        "subagent.bg_done_notice",
                        &[
                            ("id", format!("a{id}").as_str()),
                            ("type", &agent_type),
                            ("desc", &description),
                        ],
                    )));
                }
            }
        }
    }
}

/// Inline viewport 初始高度的保守估计（首帧渲染前用，随后主循环每帧都会
/// 按实际布局需要重新计算并按需重建 Terminal，见 `render::fixed_footer_height`
/// + `render::pending_chat_visual_height`）。
const INITIAL_INLINE_HEIGHT: u16 = 12;

/// 启动 TUI 主界面
#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    agent: Agent,
    rebuild_fn: RebuildFn,
    cwd: PathBuf,
    history_store: Option<HistoryStore>,
    session_store: Option<Arc<SessionStore>>,
    initial_messages: Vec<Message>,
    session_id: String,
    model_name: String,
    context_window: u32,
    mode: AgentMode,
    todo_store: Arc<std::sync::Mutex<TodoStore>>,
    // append_system() 追加到 system prompt 的内容（含前导 "\n\n"），语言切换时
    // 需要在新语言的默认提示词后原样拼回，避免冲掉 Plan 模式说明。
    system_prompt_extra: String,
    // 当前生效的完整配置（供 /config 设置面板展示初始值、保存后重建 Agent 用）
    config: Config,
    // 子 Agent 生命周期管理中心（注册事件回调、中断、退出清理）
    hub: Arc<wyj_tools::SubAgentHub>,
    // `--plugin-dir` 临时加载的本地开发插件贡献（不落盘、仅当次进程生效）
    local_plugin: Option<wyj_store::lockfile::PluginContributions>,
    // 当前已连接 MCP 工具的共享快照：后台连接成功时 push 进来，供子 Agent
    // 工厂与 `/model` 重建读取（见 `wyj-cli` 侧 `make_sub_agent_factory`）
    mcp_tools: wyj_tools::SharedMcpTools,
    shared_agent_defs: wyj_tools::SharedAgentDefinitions,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnableBracketedPaste, DisableMouseCapture)?;
    // Inline viewport：稳定历史通过 Terminal::insert_before 写入终端真实
    // scrollback，鼠标选择/滚轮滚动交给终端；TUI 只重绘未冻结尾部和底部
    // 交互区。Category B 重量级管理对话框（/mcp /model 等）打开期间会
    // 临时切到 Fullscreen + alternate screen，关闭后再切回来，见
    // tui_main 内 wants_fullscreen 分支。
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(INITIAL_INLINE_HEIGHT),
        },
    )?;

    let result = tui_main(
        &mut terminal,
        agent,
        rebuild_fn,
        cwd,
        history_store,
        session_store,
        initial_messages,
        session_id,
        model_name,
        context_window,
        mode,
        todo_store,
        system_prompt_extra,
        config,
        hub,
        local_plugin,
        mcp_tools,
        shared_agent_defs,
    )
    .await;

    disable_raw_mode()?;
    // LeaveAlternateScreen 在正常路径下大多是无操作（tui_main 退出前若曾切到
    // Fullscreen 早已切回 Inline）；退出时若恰好卡在某个 Category B 全屏对话框
    // 里也是安全兜底——终端对"退出未进入过的 alternate screen"是无害的。
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    match result {
        Ok(Some(resumable_session_id)) => {
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                Print("\n"),
                SetForegroundColor(Color::DarkGrey),
                Print("恢复此会话："),
                ResetColor,
                Print("\n  wyj-code --resume "),
                SetForegroundColor(Color::Cyan),
                Print(&resumable_session_id),
                ResetColor,
                Print("\n"),
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }
}

/// 循环切换 Agent 模式：Normal → Plan → Bypass → Normal
fn cycle_mode(mode: &AgentMode) -> AgentMode {
    match mode {
        AgentMode::Normal => AgentMode::Plan,
        AgentMode::Plan => AgentMode::Bypass,
        AgentMode::Bypass => AgentMode::Normal,
    }
}

/// 检测历史消息中最后一次 ExitPlanMode 调用是否已被用户批准
fn has_plan_approved(messages: &[Message]) -> bool {
    let mut last_id: Option<String> = None;
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                if name == "ExitPlanMode" {
                    last_id = Some(id.clone());
                }
            }
        }
    }
    let target_id = match last_id {
        Some(id) => id,
        None => return false,
    };
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            {
                if tool_use_id == &target_id && !is_error {
                    if let ToolResultContent::Text(s) = content {
                        // 新版英文结果 + 旧版中文结果（恢复历史会话时仍可能遇到）
                        return s.contains("User approved the plan") || s.contains("已批准计划");
                    }
                }
            }
        }
    }
    false
}

/// Inline viewport 中聊天区的可视高度上限（终端高度的 70%，理由见
/// `render::pending_chat_visual_height` 文档）。主循环的 desired_height 计算
/// 与冻结豁免判定共用，保证"显示得下/显示不下"的判断口径一致。
fn chat_viewport_cap(term_height: u16) -> u16 {
    (term_height * 7 / 10).max(3)
}

/// 冻结判定用的"最后可折叠 ToolResult"封顶（`compute_freezable_up_to` 的
/// `collapsible_idx` 参数）。保留封顶的目的是让用户还能对最后一条 ToolResult
/// Ctrl+O 展开/收起，但它会把该 ToolResult 之后的全部内容（如模型输出的长
/// markdown 正文）一起困在 Inline viewport 的待定尾部——尾部一旦超过聊天区
/// 可视上限，超出部分既不在屏幕上也不在终端 scrollback 里，用户完全看不到。
/// 因此**可见性优先**，以下两种情况豁免封顶、放行冻结进 scrollback（此后可
/// 用鼠标滚轮回看，代价是这些消息成为静态历史、不能再展开/收起）：
/// - 待定尾部实际渲染高度超过 `chat_viewport_cap`（本帧注定显示不全）；
/// - AskQuestion 面板打开期间（面板拦截全部按键，Ctrl+O 本就不可用）。
///
/// 未完成 ToolCall / 运行中子 Agent 的冻结阻塞（规则①③）不受此豁免影响。
fn freeze_collapsible_bound(
    state: &mut AppState,
    term_width: u16,
    term_height: u16,
) -> Option<usize> {
    if state.ask_question_dialog.is_some() {
        return None;
    }
    if render::pending_chat_visual_height(state, term_width) > chat_viewport_cap(term_height) {
        return None;
    }
    render::last_collapsible_tool_result_idx(&state.messages)
}

fn freeze_ready_scrollback(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
) -> Result<bool> {
    // Do not move content out from under the user's keyboard selection/detail
    // view. Once they return to the tail, stable content can enter native
    // scrollback again.
    if !state.chat_follow_tail || state.selected_message_id.is_some() {
        return Ok(false);
    }

    state.frozen_up_to = state.frozen_up_to.min(state.messages.len());
    let term_size = terminal.size()?;
    let collapsible_bound = freeze_collapsible_bound(state, term_size.width, term_size.height);
    let new_bound = compute_freezable_up_to(
        &state.messages,
        state.frozen_up_to,
        &state.sub_agents,
        collapsible_bound,
    );
    if new_bound <= state.frozen_up_to {
        return Ok(false);
    }

    let max_content_width = term_size.width.saturating_sub(2) as usize;
    let lines = render::build_frozen_chat_lines(state, new_bound, max_content_width);
    if lines.is_empty() {
        state.frozen_up_to = new_bound;
        return Ok(false);
    }

    let width = term_size.width.max(1);
    let height = Paragraph::new(Text::from(lines.clone()))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .min(u16::MAX as usize) as u16;
    terminal.insert_before(height.max(1), |buf| {
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .render(buf.area, buf);
    })?;

    state.frozen_up_to = new_bound;
    state.welcome_frozen = true;
    state.chat_scroll = 0;
    state.chat_max_scroll = 0;
    Ok(true)
}

/// plan 模式下返回注入了 ExitPlanMode 工具和 system prompt 的 agent 副本，否则直接 Arc::clone
fn plan_turn_agent(base: &Arc<Agent>, mode: &AgentMode) -> Arc<Agent> {
    if !matches!(mode, AgentMode::Plan) {
        return Arc::clone(base);
    }
    let mut a = (**base).clone();
    a.register_tool(Arc::new(ExitPlanModeTool));
    let a = a.append_system(wyj_core::prompts::PLAN_TURN);
    Arc::new(a)
}

/// 给重建出的 Agent 挂上工具事件回调（ToolStart/ToolEnd → AgentEvent，
/// TodoWrite 额外广播快照），/model 热切换与设置面板保存后重建 Agent 共用。
fn wire_tool_callback(
    agent: Agent,
    tool_tx: mpsc::Sender<AgentEvent>,
    todo_store: Arc<std::sync::Mutex<TodoStore>>,
) -> Agent {
    let usage_tx = tool_tx.clone();
    let title_tx = tool_tx.clone();
    let thinking_tx = tool_tx.clone();
    agent
        .with_thinking_callback(move |d| {
            let _ = thinking_tx.try_send(AgentEvent::ThinkingDelta(d.to_string()));
        })
        .with_tool_callback(move |event: ToolEvent| match event {
            ToolEvent::Start { id, name, input } => {
                let _ = tool_tx.try_send(AgentEvent::ToolStart {
                    id,
                    name,
                    input_json: input,
                });
            }
            ToolEvent::End {
                id,
                name,
                is_error,
                elapsed_secs,
                output,
            } => {
                if name == "TodoWrite" && !is_error {
                    if let Ok(store) = todo_store.lock() {
                        let items = store.items.clone();
                        let _ = tool_tx.try_send(AgentEvent::TodoUpdate(items));
                    }
                }
                let _ = tool_tx.try_send(AgentEvent::ToolEnd {
                    id,
                    output,
                    is_error,
                    elapsed_secs,
                });
            }
        })
        .with_usage_callback(move |input_tokens, output_tokens| {
            let _ = usage_tx.try_send(AgentEvent::UsageDelta {
                input_tokens,
                output_tokens,
            });
        })
        .with_title_callback(move |title: String| {
            let _ = title_tx.try_send(AgentEvent::TitleGenerated(title));
        })
}

/// 挂起 TUI（离开 alternate screen + 关闭 raw mode），交给 $EDITOR（未设置回退 vi）
/// 打开指定文件，编辑完成后恢复 TUI 并强制整屏重绘。文件父目录若不存在会先创建，
/// 方便直接对着尚不存在的候选路径按 Enter 新建。
async fn open_path_in_editor<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
    path: &std::path::Path,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let path_buf = path.to_path_buf();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&editor).arg(&path_buf).status()
    })
    .await;

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;

    match status {
        Ok(Ok(s)) if s.success() => Ok(()),
        Ok(Ok(s)) => anyhow::bail!("编辑器退出码 {:?}", s.code()),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
}

/// 构建消息的聊天区显示文本（含附件摘要）
fn build_display_text(text: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return text.to_string();
    }
    let mut s = text.to_string();
    for att in attachments {
        match att {
            Attachment::Image { preview_label, .. } => {
                s.push_str(&format!("\n[图片 {preview_label}]"));
            }
            Attachment::File { path } => {
                s.push_str(&format!("\n[附件 {}]", path.display()));
            }
        }
    }
    s
}

/// 把文本 + 附件转换为发给 Provider 的内容块（图片直接内联，文件读内容包装为 `<file>` 标签）。
/// 供正常发送与 Agent 忙碌期间的补充信息注入两处复用。
async fn build_user_blocks(text: String, attachments: Vec<Attachment>) -> Vec<ContentBlock> {
    if attachments.is_empty() {
        return vec![ContentBlock::Text { text }];
    }
    let mut blocks = vec![ContentBlock::Text { text }];
    for att in attachments {
        match att {
            Attachment::Image {
                media_type, data, ..
            } => {
                blocks.push(ContentBlock::Image { media_type, data });
            }
            Attachment::File { path } => {
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .unwrap_or_else(|e| format!("[文件读取失败 {}: {e}]", path.display()));
                blocks.push(ContentBlock::Text {
                    text: format!("\n\n<file path=\"{}\">\n{content}\n</file>", path.display()),
                });
            }
        }
    }
    blocks
}

/// 构建并 spawn 一轮 Agent 对话任务，返回可用于中断的 AbortHandle 及补充信息注入通道
#[allow(clippy::too_many_arguments)]
fn spawn_agent_turn(
    text: String,
    attachments: Vec<Attachment>,
    agent_c: Arc<Agent>,
    session_c: Arc<Mutex<Session>>,
    tx: mpsc::Sender<AgentEvent>,
    ctx_cwd: PathBuf,
    mode_arc: Arc<tokio::sync::Mutex<AgentMode>>,
    // 与 mode_arc 同步更新的实时权限句柄：直接共享（而非拷贝值）给 ctx，
    // 使得本轮运行期间的模式切换（如 Plan 审批、Shift+Tab）能立即影响
    // 后续工具调用的权限判定，无需等待下一轮 spawn_agent_turn。
    shared_permission: Arc<std::sync::RwLock<PermissionMode>>,
    ui_ask_tx_clone: mpsc::Sender<UiAskRequest>,
    // 主 Agent 空闲期间积累的后台子 Agent 结果 reminder，起手合并进本轮 user 消息
    preface_reminders: Vec<String>,
) -> (
    AbortHandle,
    mpsc::UnboundedSender<(Vec<ContentBlock>, InjectionKind)>,
) {
    let (inject_tx, mut inject_rx) =
        mpsc::unbounded_channel::<(Vec<ContentBlock>, InjectionKind)>();
    let handle = tokio::spawn(async move {
        let mut sess = session_c.lock().await;
        if attachments.is_empty() {
            sess.push_user(text);
        } else {
            let blocks = build_user_blocks(text, attachments).await;
            sess.push_user_with_blocks(blocks);
        }
        if !preface_reminders.is_empty() {
            sess.prepend_to_last_user(
                preface_reminders
                    .into_iter()
                    .map(|text| ContentBlock::Text { text })
                    .collect(),
            );
        }
        let current_mode = mode_arc.lock().await.clone();
        let mut ctx = ToolCtx::new(&ctx_cwd);
        ctx.permission_mode = shared_permission;
        ctx.ui_ask_tx = Some(ui_ask_tx_clone);
        // 逐调用权限确认：按当前项目载入「始终允许」列表并设定持久化路径
        if let Ok(config_base) = wyj_config::config_dir() {
            ctx.load_allowed_tools(&config_base);
        }
        let turn_agent = plan_turn_agent(&agent_c, &current_mode);
        let tx2 = tx.clone();
        let mut on_text = move |d: &str| {
            let _ = tx2.try_send(AgentEvent::TextDelta(d.to_string()));
        };
        let tx3 = tx.clone();
        // 仅用户排队消息需要同步 UI（弹出 pending_queue 回放）；
        // system-reminder 注入（如后台子 Agent 结果）对用户消息队列不可见。
        let on_inject = move |kind: InjectionKind| {
            if kind == InjectionKind::UserMessage {
                let _ = tx3.try_send(AgentEvent::Injected);
            }
        };
        match turn_agent
            .run_turn_with_injection(
                &mut sess,
                &ctx,
                &mut on_text,
                Some(&mut inject_rx),
                on_inject,
            )
            .await
        {
            Ok(_) => {
                let _ = tx
                    .send(AgentEvent::Usage {
                        input: sess.total_input_tokens,
                        output: sess.total_output_tokens,
                        context_tokens: wyj_core::estimate_tokens(&sess.messages),
                    })
                    .await;
                let _ = tx.send(AgentEvent::TurnDone).await;
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e.to_string())).await;
            }
        }
    });
    (handle.abort_handle(), inject_tx)
}

/// RunPromptScoped（自定义命令 allowed-tools）临时收紧 permission_mode 的 RAII 兜底还原。
/// 正常跑完当轮或 ESC 中断（tokio task 被 abort，future 直接 drop）都会触发 `Drop::drop`，
/// 保证不会把权限模式永久卡在临时收紧的 Allowlist 上。
struct RestorePermissionOnDrop {
    handle: Arc<std::sync::RwLock<PermissionMode>>,
    prev: PermissionMode,
}

impl Drop for RestorePermissionOnDrop {
    fn drop(&mut self) {
        *self.handle.write().unwrap() = self.prev.clone();
    }
}

/// 根据 AgentMode 构建对应的 PermissionMode
fn mode_to_permission(mode: &AgentMode) -> PermissionMode {
    match mode {
        AgentMode::Plan => {
            let set: HashSet<String> = [
                "Read",
                "Glob",
                "Grep",
                "WebFetch",
                "WebSearch",
                "AskQuestion",
                "Bash",         // 只读命令，由 system prompt 约束
                "BashOutput",   // 后台任务输出读取（纯读）
                "ExitPlanMode", // 提交计划并请求批准（计划文本作为参数直传，不落盘）
                "TodoWrite",    // 任务追踪，plan 模式同样有用
                "Agent",        // 子 Agent（继承同一白名单，不会绕过 plan 模式限制）
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            PermissionMode::Allowlist(set)
        }
        AgentMode::Bypass => PermissionMode::AutoApprove,
        AgentMode::Normal => PermissionMode::Prompt,
    }
}

/// 统一的模式切换入口：同步更新 shared_mode 与 shared_permission，
/// 确保任何切换途径（Plan 审批、Shift+Tab、/mode、/plan、resume 确认）
/// 都不会遗漏 shared_permission 的更新。shared_permission 与正在运行的
/// turn 共享同一个 Arc<RwLock<..>>，因此这里的写入对当轮剩余的工具调用
/// 立即生效，无需等待下一轮 spawn_agent_turn。
async fn switch_mode(
    shared_mode: &Arc<tokio::sync::Mutex<AgentMode>>,
    shared_permission: &Arc<std::sync::RwLock<PermissionMode>>,
    new_mode: AgentMode,
) {
    *shared_mode.lock().await = new_mode.clone();
    *shared_permission.write().unwrap() = mode_to_permission(&new_mode);
}

/// 扫描目录并按过滤词返回文件候选列表
fn scan_files(
    dir: &std::path::Path,
    filter: &str,
    cwd: &std::path::Path,
    depth: usize,
) -> Vec<FileEntry> {
    use ignore::WalkBuilder;

    let mut entries: Vec<FileEntry> = Vec::new();
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(depth))
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if path == dir {
            continue;
        }
        let is_dir = path.is_dir();
        let display = path
            .strip_prefix(dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        if display.is_empty() {
            continue;
        }
        if !filter.is_empty() {
            let lf = filter.to_lowercase();
            let ld = display.to_lowercase();
            if !ld.contains(lf.as_str()) {
                continue;
            }
        }
        let rel_path = path
            .strip_prefix(cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.display().to_string());
        entries.push(FileEntry {
            display,
            rel_path,
            is_dir,
        });
        if entries.len() >= 200 {
            break;
        }
    }

    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.display.cmp(&b.display)));
    entries
}

/// 根据输入框光标前的 @ 触发词更新文件候选列表
/// 在 agents 面板里按 delta(-1=Up/+1=Down) 移动选中项。
/// 未选中时统一默认跳到最新（id 最大）一项；移动到边界后 clamp，不 wrap。
/// 把某会话落盘的子 Agent trace（`wyj_tools::trace`）重建为 `SubAgentUiState`
/// 摘要，回灌进 `sub_agents`——复用现有 `draw_sub_agents_panel`，跨会话查看
/// 不需要专门的展示代码路径。全文 input/output 只留在磁盘（避免长会话下
/// 常驻内存暴涨），这里只截断出与实时 `arg_summary` 观感一致的摘要行。
/// trace 目录不存在（新会话，或该会话从未跑过子 Agent）时返回空 map。
fn reload_persisted_sub_agents(
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> std::collections::BTreeMap<u64, SubAgentUiState> {
    use wyj_tools::trace::{list_trace_ids, read_trace, trace_file, TraceEvent};

    let mut out = std::collections::BTreeMap::new();
    for id in list_trace_ids(sessions_dir, session_id) {
        let path = trace_file(sessions_dir, session_id, id);
        let Ok(events) = read_trace(&path) else {
            continue;
        };
        let mut reconstructed: Option<SubAgentUiState> = None;
        for ev in events {
            match ev {
                TraceEvent::Started {
                    agent_type,
                    description,
                    background,
                    ..
                } => {
                    reconstructed = Some(SubAgentUiState {
                        agent_type,
                        description,
                        background,
                        // 兜底态：没有 Done 事件（如进程被强杀）时保持"已中断"，
                        // 下面 Done 分支命中时会覆盖为 Done/Failed。
                        status: SubAgentStatus::Interrupted,
                        started_at: Instant::now(),
                        final_elapsed: None,
                        input_tokens: 0,
                        output_tokens: 0,
                        tool_calls: 0,
                        current_tool: None,
                        tool_log: vec![],
                        has_result: false,
                        finished_at: Some(Instant::now()),
                        final_result: None,
                    });
                }
                TraceEvent::ToolStart {
                    tool_name,
                    input_json,
                    ..
                } => {
                    if let Some(s) = &mut reconstructed {
                        s.tool_calls += 1;
                        s.tool_log.push(SubToolLine {
                            tool_name,
                            arg_summary: wyj_tools::textutil::truncate_str(&input_json, 60)
                                .to_string(),
                            is_error: false,
                            elapsed_secs: None,
                        });
                    }
                }
                TraceEvent::ToolEnd {
                    tool_name,
                    is_error,
                    elapsed_secs,
                    ..
                } => {
                    if let Some(s) = &mut reconstructed {
                        if let Some(line) = s
                            .tool_log
                            .iter_mut()
                            .rev()
                            .find(|l| l.elapsed_secs.is_none() && l.tool_name == tool_name)
                        {
                            line.is_error = is_error;
                            line.elapsed_secs = Some(elapsed_secs);
                        }
                    }
                }
                TraceEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    if let Some(s) = &mut reconstructed {
                        s.input_tokens += input_tokens;
                        s.output_tokens += output_tokens;
                    }
                }
                TraceEvent::Done {
                    result,
                    is_error,
                    elapsed_secs,
                } => {
                    if let Some(s) = &mut reconstructed {
                        s.status = if is_error {
                            SubAgentStatus::Failed
                        } else {
                            SubAgentStatus::Done
                        };
                        s.final_elapsed = Some(elapsed_secs);
                        s.has_result = true;
                        s.final_result = Some(result);
                    }
                }
            }
        }
        if let Some(s) = reconstructed {
            out.insert(id, s);
        }
    }
    out
}

/// `/subagents [id]` 命令：无参数时定位到最近一个子 Agent 并展开详情
/// （本会话历史子 Agent 已在启动/`/resume` 时由 `reload_persisted_sub_agents`
/// 回灌进 `state.sub_agents`，故这里天然覆盖跨会话查看）；带 id 时校验存在性。
fn apply_open_subagents_panel(state: &mut AppState, target_id: Option<u64>) {
    if state.sub_agents.is_empty() {
        state
            .messages
            .push(ChatMessage::system(wyj_i18n::tr("subagents.empty")));
        return;
    }
    match target_id {
        Some(id) if state.sub_agents.contains_key(&id) => {
            state.selected_sub_agent = Some(id);
            state.sub_agent_detail_open = true;
            state.sub_agent_detail_scroll = 0;
            state.ui_focus = UiFocus::SubAgents;
        }
        Some(id) => {
            state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                "subagents.not_found",
                &[("id", id.to_string().as_str())],
            )));
        }
        None => {
            if let Some(&last_id) = state.sub_agents.keys().next_back() {
                state.selected_sub_agent = Some(last_id);
                state.sub_agent_detail_open = true;
                state.sub_agent_detail_scroll = 0;
                state.ui_focus = UiFocus::SubAgents;
            }
        }
    }
}

fn update_file_completions(state: &mut AppState, input: &InputBox, cwd: &std::path::Path) {
    let line = input
        .lines
        .get(input.cursor_row)
        .map(|s| s.as_str())
        .unwrap_or("");
    let before_cursor: String = line.chars().take(input.cursor_col).collect();

    // 找最后一个 @ —— 须在行首或紧跟空白（避免误触 email）
    let at_byte = before_cursor.rfind('@');
    let at_byte = match at_byte {
        None => {
            state.file_completions.clear();
            return;
        }
        Some(p) => p,
    };
    let before_at = &before_cursor[..at_byte];
    if !before_at.is_empty() && !before_at.ends_with(|c: char| c.is_whitespace()) {
        state.file_completions.clear();
        return;
    }

    let query = &before_cursor[at_byte + 1..];
    let (browse_dir, filter, depth) = if let Some(slash_pos) = query.rfind('/') {
        let dir_part = &query[..slash_pos];
        (
            cwd.join(dir_part),
            query[slash_pos + 1..].to_string(),
            1usize,
        )
    } else {
        (cwd.to_path_buf(), query.to_string(), 3usize)
    };

    if !browse_dir.exists() {
        state.file_completions.clear();
        return;
    }

    state.file_completions = scan_files(&browse_dir, &filter, cwd, depth);
    state.file_selected = 0;
    state.at_browse_dir = browse_dir;
}

/// 将输入框当前行最后一个 @ 后的 query 替换为 new_path
fn replace_at_query(input: &mut InputBox, new_path: &str) {
    let line = input
        .lines
        .get(input.cursor_row)
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let before_cursor: String = line.chars().take(input.cursor_col).collect();
    if let Some(at_byte) = before_cursor.rfind('@') {
        let chars_before_at = before_cursor[..at_byte].chars().count();
        let chars_from_at = input.cursor_col - chars_before_at;
        for _ in 0..chars_from_at {
            input.backspace();
        }
        for c in format!("@{new_path}").chars() {
            input.insert_char(c);
        }
        if !new_path.ends_with('/') {
            input.insert_char(' ');
        }
    }
}

/// 扫描消息中的 @file 引用，将存在的文件路径追加到 pending_attachments
fn expand_at_refs_to_attachments(
    msg: &str,
    cwd: &std::path::Path,
    attachments: &mut Vec<Attachment>,
) {
    let mut rest = msg;
    while let Some(at_pos) = rest.find('@') {
        let after = &rest[at_pos + 1..];
        let end = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        let token = &after[..end];
        if !token.is_empty() {
            let path = cwd.join(token);
            if path.exists() && path.is_file() {
                let already = attachments
                    .iter()
                    .any(|a| matches!(a, Attachment::File { path: p } if p == &path));
                if !already {
                    attachments.push(Attachment::File { path });
                }
            }
        }
        if end == 0 {
            rest = &rest[at_pos + 1..];
        } else {
            rest = &after[end..];
        }
    }
}

/// 根据输入框内容更新 slash 补全候选列表
fn update_slash_completions(
    state: &mut AppState,
    input: &InputBox,
    registry: &wyj_commands::CommandRegistry,
) {
    let first_line = input.lines.first().map(|s| s.as_str()).unwrap_or("");
    if input.lines.len() == 1 && first_line.starts_with('/') {
        let mut completions = registry.complete(first_line);
        completions.sort_by(|a, b| a.0.cmp(&b.0));
        if completions != state.slash_completions {
            state.slash_completions = completions;
            state.slash_selected = 0;
        }
    } else if !state.slash_completions.is_empty() {
        state.slash_completions.clear();
        state.slash_selected = 0;
    }
}

fn effective_mcp_servers_for_runtime(
    cfg: &wyj_config::Config,
    cwd: &std::path::Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) -> Vec<wyj_config::McpServerConfig> {
    let mut servers = wyj_store::mcp_install::effective_mcp_servers(cfg, cwd);
    if let Some(local) = local_plugin {
        let mut names: std::collections::HashSet<String> =
            servers.iter().map(|server| server.name.clone()).collect();
        for server in &local.mcp_servers {
            if names.insert(server.name.clone()) {
                servers.push(server.clone());
            }
        }
    }
    servers
}

fn refresh_tui_agent_definitions(
    shared_defs: &wyj_tools::SharedAgentDefinitions,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    cwd: &Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) {
    let mut sources = wyj_store::plugin_install::enabled_plugin_agent_paths(cwd);
    if let Some(local) = local_plugin {
        sources.extend(local.agent_paths.clone());
    }
    let defs = wyj_core::load_agent_defs(cwd, &sources);
    if let Ok(mut current) = shared_defs.write() {
        *current = defs;
    }
    let mut agent = (**shared_agent.read().unwrap()).clone();
    agent.refresh_tool_definitions();
    *shared_agent.write().unwrap() = Arc::new(agent);
}

/// Reconcile external MCP resources only at an Agent turn boundary.  The
/// runtime itself is always allowed to finish/abort connection work in the
/// background, but an active turn keeps its immutable Agent snapshot.  This is
/// what makes disable/uninstall safe even when a tool call is currently in
/// flight.
fn refresh_tui_mcp_runtime(
    runtime: &mut wyj_mcp::McpRuntime,
    state: &mut AppState,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    mcp_tools: &wyj_tools::SharedMcpTools,
    cwd: &std::path::Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) {
    let live_cfg = wyj_config::Config::load().unwrap_or_else(|e| {
        tracing::debug!("读取运行时配置失败，继续使用当前配置: {e}");
        state.config.clone()
    });
    let servers = effective_mcp_servers_for_runtime(&live_cfg, cwd, local_plugin);
    for server in &servers {
        state
            .mcp_connection_status
            .entry(server.name.clone())
            .or_insert(McpConnStatus::Connecting);
    }
    for name in runtime.connected_names() {
        if !servers.iter().any(|server| server.name == name) {
            state.mcp_connection_status.remove(&name);
        }
    }

    let mut events = runtime.reconcile(&servers);
    events.extend(runtime.drain());
    for event in events {
        match event {
            wyj_mcp::McpRuntimeEvent::Connected { name, tool_count } => {
                state
                    .mcp_connection_status
                    .insert(name.clone(), McpConnStatus::Connected { tool_count });
                state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                    "mcp.background.connected",
                    &[("name", &name), ("count", &tool_count.to_string())],
                )));
            }
            wyj_mcp::McpRuntimeEvent::Failed { name, reason } => {
                state
                    .mcp_connection_status
                    .insert(name.clone(), McpConnStatus::Failed);
                let mut msg = ChatMessage::system(format!("[MCP {name}] 连接失败: {reason}"));
                msg.is_error = true;
                state.messages.push(msg);
            }
            wyj_mcp::McpRuntimeEvent::Removed { name } => {
                state.mcp_connection_status.remove(&name);
                state.messages.push(ChatMessage::system(format!(
                    "[MCP {name}] 已从下一回合工具快照移除"
                )));
            }
        }
    }

    let snapshot = runtime.tools();
    {
        let mut shared = mcp_tools.write().unwrap();
        *shared = snapshot;
    }
    let tools = mcp_tools.read().unwrap().clone();
    let mut new_agent = (**shared_agent.read().unwrap()).clone();
    new_agent.remove_tools_where(|name| name.starts_with("mcp__"));
    for tool in tools {
        new_agent.register_tool(tool);
    }
    *shared_agent.write().unwrap() = Arc::new(new_agent);
}

#[allow(clippy::too_many_arguments)]
async fn tui_main(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: Agent,
    rebuild_fn: RebuildFn,
    cwd: PathBuf,
    history_store: Option<HistoryStore>,
    session_store: Option<Arc<SessionStore>>,
    initial_messages: Vec<Message>,
    session_id: String,
    model_name: String,
    context_window: u32,
    mode: AgentMode,
    todo_store: Arc<std::sync::Mutex<TodoStore>>,
    system_prompt_extra: String,
    config: Config,
    hub: Arc<wyj_tools::SubAgentHub>,
    local_plugin: Option<wyj_store::lockfile::PluginContributions>,
    mcp_tools: wyj_tools::SharedMcpTools,
    shared_agent_defs: wyj_tools::SharedAgentDefinitions,
) -> Result<Option<String>> {
    let shared_mode = Arc::new(tokio::sync::Mutex::new(mode.clone()));
    // 与 shared_mode 同步更新的实时权限句柄，见 switch_mode() 与 spawn_agent_turn()
    // 顶部说明：解决"运行中切换模式对本轮已生效"的问题。
    let shared_permission = Arc::new(std::sync::RwLock::new(mode_to_permission(&mode)));
    let mut state = AppState::new(
        cwd.clone(),
        model_name,
        context_window,
        mode,
        config,
        hub.clone(),
    );
    state.hook_runner = agent.hook_runner_ref().cloned();
    let mut input = InputBox::new();
    let mut current_session_id = session_id;
    state.current_session_id = current_session_id.clone();
    state.sessions_dir = session_store.as_ref().map(|s| s.dir().to_path_buf());

    // 启动/`-c`/`--resume` 统一路径：把落盘的子 Agent trace（若存在）回灌进
    // 面板，使跨会话查看无需区分新会话/恢复会话——新会话的 trace 目录尚不
    // 存在，`reload_persisted_sub_agents` 天然返回空 map，是个 no-op。
    if let Some(store) = &session_store {
        state.sub_agents = reload_persisted_sub_agents(store.dir(), &current_session_id);
    }

    // 设置初始终端窗口标题（退出时恢复）
    let _ = write!(io::stdout(), "\x1b]0;wyj-code\x07");
    let _ = io::stdout().flush();

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (ui_ask_tx, mut ui_ask_rx) = mpsc::channel::<UiAskRequest>(8);

    // 子 Agent 事件走独立的无界通道（不丢事件、保序），主循环里排空
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<wyj_tools::SubAgentEvent>();
    hub.set_event_cb(move |ev| {
        let _ = sub_tx.send(ev);
    });
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let disabled_skills = wyj_store::disabled_skill_names(&cwd);
    let mut plugin_skill_sources = wyj_store::plugin_install::enabled_plugin_skill_paths(&cwd);
    if let Some(local) = &local_plugin {
        plugin_skill_sources.extend(local.skill_paths.clone());
    }
    let mut cmd_registry =
        standard_registry_with_skills(&home_dir, &cwd, &disabled_skills, &plugin_skill_sources);

    // 工具回调：ToolStart/ToolEnd/Usage → AgentEvent，同时拦截 TodoWrite 读取快照
    // （title_cb 也在 wire_tool_callback 内部设置，确保 /model 重建后仍生效）
    let agent = wire_tool_callback(agent, agent_tx.clone(), todo_store.clone());

    // 用 RwLock 包装 agent，支持 /model 热切换
    let shared_agent = Arc::new(std::sync::RwLock::new(Arc::new(agent)));

    let mut mcp_runtime = wyj_mcp::McpRuntime::new();
    let initial_mcp_servers =
        effective_mcp_servers_for_runtime(&state.config, &cwd, local_plugin.as_ref());
    for mcp_cfg in &initial_mcp_servers {
        state
            .mcp_connection_status
            .insert(mcp_cfg.name.clone(), McpConnStatus::Connecting);
    }
    mcp_runtime.reconcile(&initial_mcp_servers);

    // 初始化 Session：若有历史消息则恢复，并重建 TUI 显示
    let has_initial = !initial_messages.is_empty();
    let mut init_sess = Session::new();
    init_sess.messages = initial_messages;
    if has_initial {
        state.welcome_frozen = true;
        state.context_tokens = wyj_core::estimate_tokens(&init_sess.messages);
        state.messages = reconstruct_display(&init_sess.messages);
        state.messages.push(ChatMessage::system(format!(
            "已恢复会话  共 {} 条消息",
            init_sess.messages.len()
        )));
        if state.mode == AgentMode::Plan && has_plan_approved(&init_sess.messages) {
            state.messages.push(ChatMessage::system(
                "上轮会话计划已批准，继续输入时会提示切换执行模式".to_string(),
            ));
        }
    }
    let session = Arc::new(Mutex::new(init_sess));

    let mut last_spinner_advance = Instant::now();
    // 是否处于 Fullscreen + alternate screen 态（Category B 重量级管理对话框
    // 打开期间）；ratatui 的 Terminal 没有原地切换 Viewport 变体的公开 API，
    // 只能整个重建 Terminal，见下方 wants_fullscreen 分支。
    let mut in_fullscreen = false;
    // 当前 Inline viewport 高度（仅在 !in_fullscreen 时有意义），用于判断是否
    // 需要因为布局变化（输入框增高、面板开关等）重建 Terminal。
    let mut current_inline_height: u16 = INITIAL_INLINE_HEIGHT;

    loop {
        // Ctrl+C 首次按下超过 3 秒未确认则重置
        if let Some(t) = state.last_ctrl_c {
            if t.elapsed() > Duration::from_secs(3) {
                state.ctrl_c_pressed = false;
                state.last_ctrl_c = None;
            }
        }

        // 推进 spinner 动画帧（每 ~80ms 一帧，与 Claude Code 节奏一致）；
        // 后台子 Agent 运行期间即使主 Agent 空闲也要驱动动画
        if (state.is_thinking || state.has_running_sub_agents())
            && last_spinner_advance.elapsed().as_millis() >= 80
        {
            state.spinner_frame = (state.spinner_frame + 1) % render::SPINNER_FRAMES.len();
            last_spinner_advance = Instant::now();
        }

        // Category B 对话框开关转换：整个重建 Terminal 切到/切回 Fullscreen +
        // alternate screen（复用现有 7 个大型管理面板的渲染代码，它们假设
        // 拥有整个终端）。只在真正发生"打开/关闭"转换时才重建，避免每帧重建。
        let wants_fs = state.wants_fullscreen();
        if wants_fs != in_fullscreen {
            // 重建前先清掉旧 Terminal 实例在真实终端上留下的内容：新 Terminal
            // 是全新构造的结构体（`Terminal::with_options`/`Terminal::new`），
            // 不知道旧视口在屏幕上的位置，直接重建会导致新旧画面重叠/重影。
            terminal.clear()?;
            if wants_fs {
                execute!(io::stdout(), EnterAlternateScreen)?;
                *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            } else {
                execute!(io::stdout(), LeaveAlternateScreen)?;
                *terminal = Terminal::with_options(
                    CrosstermBackend::new(io::stdout()),
                    TerminalOptions {
                        viewport: Viewport::Inline(current_inline_height),
                    },
                )?;
            }
            terminal.clear()?;
            in_fullscreen = wants_fs;
        }

        let froze_this_frame = if !in_fullscreen {
            freeze_ready_scrollback(terminal, &mut state)?
        } else {
            false
        };

        if !in_fullscreen && !froze_this_frame {
            let term_size = terminal.size()?;
            let footer_fixed =
                render::fixed_footer_height(&state, &input, term_size.width, term_size.height);
            let chat_h = render::pending_chat_visual_height(&mut state, term_size.width)
                .clamp(3, chat_viewport_cap(term_size.height));
            let desired_height = (footer_fixed + chat_h).min(term_size.height).max(1);
            if desired_height != current_inline_height {
                terminal.clear()?;
                *terminal = Terminal::with_options(
                    CrosstermBackend::new(io::stdout()),
                    TerminalOptions {
                        viewport: Viewport::Inline(desired_height),
                    },
                )?;
                current_inline_height = desired_height;
            }
        }

        terminal.draw(|f| render::draw(f, &mut state, &input))?;

        // 清空 agent 事件队列
        while let Ok(ev) = agent_rx.try_recv() {
            state.apply_agent_event(ev);
        }

        // 清空子 Agent 事件队列（在 agent 事件之后排空，保证父 ToolStart 先于 Started 应用）
        while let Ok(ev) = sub_rx.try_recv() {
            state.apply_agent_event(AgentEvent::SubAgent(ev));
        }

        // Resource mutations are applied only after the previous Agent turn is
        // no longer active. The current turn keeps its immutable snapshot; the
        // next turn gets the reconciled MCP set atomically.
        if !state.is_thinking {
            refresh_tui_agent_definitions(
                &shared_agent_defs,
                &shared_agent,
                &cwd,
                local_plugin.as_ref(),
            );
            refresh_tui_mcp_runtime(
                &mut mcp_runtime,
                &mut state,
                &shared_agent,
                &mcp_tools,
                &cwd,
                local_plugin.as_ref(),
            );
        }

        // 清除过期的粘贴提示
        if state
            .paste_hint
            .as_ref()
            .is_some_and(|h| h.expires_at <= Instant::now())
        {
            state.paste_hint = None;
        }

        // 竞态兜底：极小概率下，用户提交补充消息的时机恰好晚于 run_turn 最后一次
        // 排空注入队列的检查，导致该轮 TurnDone/Error 已到达而消息仍留在
        // pending_queue 里未被消费。此时视同用户在轮次结束的瞬间正常发送。
        if !state.is_thinking && !state.pending_queue.is_empty() {
            let queued = std::mem::take(&mut state.pending_queue);
            state.injector = None;
            let mut combined_text = String::new();
            let mut combined_attachments = vec![];
            for (i, (text, atts)) in queued.into_iter().enumerate() {
                if i > 0 {
                    combined_text.push_str("\n---\n");
                }
                combined_text.push_str(&text);
                combined_attachments.extend(atts);
            }
            let display_text = build_display_text(&combined_text, &combined_attachments);
            state.push_user(display_text);
            state.input_history.push(combined_text.clone());
            state.is_thinking = true;
            state.spinner_frame = 0;
            state.turn_start_time = Some(Instant::now());
            state.turn_start_input_tokens = state.total_input_tokens;
            state.turn_start_output_tokens = state.total_output_tokens;
            let agent_c = shared_agent.read().unwrap().clone();
            let (handle, injector) = spawn_agent_turn(
                combined_text,
                combined_attachments,
                agent_c,
                session.clone(),
                agent_tx.clone(),
                cwd.clone(),
                shared_mode.clone(),
                shared_permission.clone(),
                ui_ask_tx.clone(),
                std::mem::take(&mut state.pending_bg_reminders),
            );
            state.current_task = Some(handle);
            state.injector = Some(injector);
        }

        // 每轮对话结束后自动保存 session 文件
        if state.save_needed {
            state.save_needed = false;
            if let Some(store) = &session_store {
                let sess = session.lock().await;
                if !sess.messages.is_empty() {
                    // 保留已有的 LLM 生成标题（若已生成则不覆盖为启发式 title）
                    let (title, title_generated) = match session_store
                        .as_ref()
                        .and_then(|s| s.load(&current_session_id).ok())
                    {
                        Some(f) if f.title_generated => (f.title, true),
                        _ => (extract_title(&sess.messages), false),
                    };
                    let sf = SessionFile {
                        session_id: current_session_id.clone(),
                        title,
                        last_preview: extract_preview(&sess.messages),
                        cwd: cwd.display().to_string(),
                        timestamp: now_iso(),
                        turns: state.turns,
                        input_tokens: sess.total_input_tokens,
                        output_tokens: sess.total_output_tokens,
                        messages: sess.messages.clone(),
                        title_generated,
                    };
                    let _ = store.save(&sf);
                }
            }
        }

        // 消费 ui_ask 请求，分发为对应 AgentEvent
        loop {
            match ui_ask_rx.try_recv() {
                Ok(UiAskRequest::Questions {
                    questions,
                    response_tx,
                }) => {
                    state.apply_agent_event(AgentEvent::AskQuestions {
                        questions,
                        response_tx,
                    });
                }
                Ok(UiAskRequest::ExitPlanMode { plan, response_tx }) => {
                    state.apply_agent_event(AgentEvent::PlanApprovalRequest { plan, response_tx });
                }
                Ok(UiAskRequest::ToolPermission {
                    tool_name,
                    action_summary,
                    response_tx,
                }) => {
                    state.apply_agent_event(AgentEvent::ToolPermissionRequest {
                        tool_name,
                        action_summary,
                        response_tx,
                    });
                }
                Err(_) => break,
            }
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            let ev = event::read()?;
            match ev {
                Event::Paste(pasted) => {
                    // /mcp /skills /plugins 面板借用主输入框做配置输入时（见
                    // `InputOwner`），粘贴内容应进借用态草稿，而不是穿透到聊天
                    // 输入框触发图片/文件/文字判定——此前这三个面板完全没有拦截
                    // Event::Paste，是一个真实存在的 bug，而不只是缺功能。
                    if let Some(owner) = state.input_owner {
                        if let Some(ib) = owner.live_input_mut(&mut state) {
                            ib.insert_text(&pasted);
                        }
                        continue;
                    }

                    let expires_at = Instant::now() + PASTE_HINT_DURATION;
                    let mut hint: Option<PasteHint> = None;

                    // 优先检查剪贴板是否有图片
                    let has_image = match arboard::Clipboard::new() {
                        Ok(mut cb) => match cb.get_image() {
                            Ok(img) => {
                                match encode_rgba_to_png(
                                    &img.bytes,
                                    img.width as u32,
                                    img.height as u32,
                                ) {
                                    Ok(png_bytes) => {
                                        use base64::Engine as _;
                                        let b64 = base64::engine::general_purpose::STANDARD
                                            .encode(&png_bytes);
                                        let label = format!("{}×{}", img.width, img.height);
                                        state.pending_attachments.push(Attachment::Image {
                                            media_type: "image/png".to_string(),
                                            data: b64,
                                            preview_label: label,
                                        });
                                        let text = format!(
                                            "[{}]",
                                            wyj_i18n::tr_fmt(
                                                "input.paste_image",
                                                &[
                                                    ("width", &img.width.to_string()),
                                                    ("height", &img.height.to_string()),
                                                ]
                                            )
                                        );
                                        hint = Some(PasteHint {
                                            text,
                                            expires_at,
                                            cursor_row: input.cursor_row,
                                            cursor_col: input.cursor_col,
                                        });
                                        true
                                    }
                                    Err(_) => false,
                                }
                            }
                            Err(_) => false,
                        },
                        Err(_) => false,
                    };

                    if !has_image {
                        // 文件路径检测
                        if let Some(path) = try_resolve_path(pasted.trim()) {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            state.pending_attachments.push(Attachment::File { path });
                            let text = format!(
                                "[{}]",
                                wyj_i18n::tr_fmt("input.paste_file", &[("name", &name)])
                            );
                            hint = Some(PasteHint {
                                text,
                                expires_at,
                                cursor_row: input.cursor_row,
                                cursor_col: input.cursor_col,
                            });
                        } else {
                            // 普通文字粘贴
                            input.insert_text(&pasted);
                            update_slash_completions(&mut state, &input, &cmd_registry);
                            if !pasted.is_empty() {
                                let count = pasted.chars().count().to_string();
                                let text = format!(
                                    "[{}]",
                                    wyj_i18n::tr_fmt("input.paste_text", &[("count", &count)])
                                );
                                hint = Some(PasteHint {
                                    text,
                                    expires_at,
                                    cursor_row: input.cursor_row,
                                    cursor_col: input.cursor_col,
                                });
                            }
                        }
                    }

                    if let Some(h) = hint {
                        state.paste_hint = Some(h);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => state.scroll_focus_lines(-3),
                    MouseEventKind::ScrollDown => state.scroll_focus_lines(3),
                    _ => {}
                },
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // 任意按键都清除粘贴提示（提示本就是瞬时的）
                    state.paste_hint = None;

                    // ⓪ Session Picker 拦截（最高优先级，思考中时不允许打开）
                    if state.session_picker.is_some() {
                        match key.code {
                            KeyCode::Up => {
                                if let Some(picker) = &mut state.session_picker {
                                    if picker.selected > 0 {
                                        picker.selected -= 1;
                                    }
                                }
                            }
                            KeyCode::Down => {
                                if let Some(picker) = &mut state.session_picker {
                                    let max = picker.sessions.len(); // 0=新建，1..=n=sessions
                                    if picker.selected < max {
                                        picker.selected += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(picker) = state.session_picker.take() {
                                    if picker.selected == 0 {
                                        // 新建会话：自动保存当前后重置
                                        if let Some(store) = &session_store {
                                            let sess = session.lock().await;
                                            if !sess.messages.is_empty() {
                                                let (title, title_generated) = match store
                                                    .load(&current_session_id)
                                                    .ok()
                                                {
                                                    Some(f) if f.title_generated => (f.title, true),
                                                    _ => (extract_title(&sess.messages), false),
                                                };
                                                let _ = store.save(&SessionFile {
                                                    session_id: current_session_id.clone(),
                                                    title,
                                                    last_preview: extract_preview(&sess.messages),
                                                    cwd: cwd.display().to_string(),
                                                    timestamp: now_iso(),
                                                    turns: state.turns,
                                                    input_tokens: sess.total_input_tokens,
                                                    output_tokens: sess.total_output_tokens,
                                                    messages: sess.messages.clone(),
                                                    title_generated,
                                                });
                                            }
                                        }
                                        let mut sess = session.lock().await;
                                        *sess = Session::new();
                                        drop(sess);
                                        current_session_id = new_session_id();
                                        state.reset_for_new_session();
                                        state.current_session_id = current_session_id.clone();
                                        state
                                            .messages
                                            .push(ChatMessage::system("已开始新会话".to_string()));
                                    } else {
                                        // 切换到选定历史会话
                                        let meta = &picker.sessions[picker.selected - 1];
                                        if let Some(store) = &session_store {
                                            // 自动保存当前会话
                                            {
                                                let sess = session.lock().await;
                                                if !sess.messages.is_empty() {
                                                    let (title, title_generated) = match store
                                                        .load(&current_session_id)
                                                        .ok()
                                                    {
                                                        Some(f) if f.title_generated => {
                                                            (f.title, true)
                                                        }
                                                        _ => (extract_title(&sess.messages), false),
                                                    };
                                                    let _ = store.save(&SessionFile {
                                                        session_id: current_session_id.clone(),
                                                        title,
                                                        last_preview: extract_preview(
                                                            &sess.messages,
                                                        ),
                                                        cwd: cwd.display().to_string(),
                                                        timestamp: now_iso(),
                                                        turns: state.turns,
                                                        input_tokens: sess.total_input_tokens,
                                                        output_tokens: sess.total_output_tokens,
                                                        messages: sess.messages.clone(),
                                                        title_generated,
                                                    });
                                                }
                                            }
                                            // 加载目标会话
                                            match store.load(&meta.session_id) {
                                                Ok(file) => {
                                                    let display_msgs =
                                                        reconstruct_display(&file.messages);
                                                    let mut sess = session.lock().await;
                                                    sess.total_input_tokens = file.input_tokens;
                                                    sess.total_output_tokens = file.output_tokens;
                                                    sess.messages = file.messages;
                                                    let context_tokens =
                                                        wyj_core::estimate_tokens(&sess.messages);
                                                    let plan_approved =
                                                        has_plan_approved(&sess.messages);
                                                    drop(sess);
                                                    current_session_id = file.session_id.clone();
                                                    state.current_session_id =
                                                        current_session_id.clone();
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
                                                    state.context_tokens = context_tokens;
                                                    state.turns = file.turns;
                                                    state.frozen_up_to = 0;
                                                    state.welcome_frozen = true;
                                                    state.selected_message_id = None;
                                                    state.selected_message_anchor = None;
                                                    state.last_toggled_message_id = None;
                                                    state.message_detail_scroll.clear();
                                                    state.current_todos = None;
                                                    state.todo_stats.clear();
                                                    state.todo_execution_logs.clear();
                                                    state.todo_panel_expanded = false;
                                                    state.selected_todo_id = None;
                                                    state.todo_detail_open = false;
                                                    state.todo_detail_scroll = 0;
                                                    state.sub_agent_trace_cache.clear();
                                                    state.sub_agents = reload_persisted_sub_agents(
                                                        store.dir(),
                                                        &current_session_id,
                                                    );
                                                    state.selected_sub_agent = None;
                                                    state.sub_agent_detail_open = false;
                                                    state.sub_agent_detail_scroll = 0;
                                                    state.messages.push(ChatMessage::system(
                                                        format!(
                                                            "已切换至会话 {}  共 {} 轮对话",
                                                            file.session_id, file.turns
                                                        ),
                                                    ));
                                                    if state.mode == AgentMode::Plan
                                                        && plan_approved
                                                    {
                                                        state.messages.push(ChatMessage::system(
                                                            "该会话计划已批准，继续输入时会提示切换执行模式"
                                                                .to_string(),
                                                        ));
                                                    }
                                                }
                                                Err(e) => {
                                                    state.messages.push(
                                                        ChatMessage::assistant_err(format!(
                                                            "[加载会话失败] {e}"
                                                        )),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                state.session_picker = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.1 主输入框借用态拦截（/mcp /skills /plugins 面板的配置输入，
                    // 见 `InputOwner`）：Esc 取消并归还输入框、Enter 提交交给各字段
                    // 自己的提交逻辑、其余按键转发进 dialog.live_input。
                    if let Some(owner) = state.input_owner {
                        match key.code {
                            KeyCode::Esc => {
                                owner.clear_live_input(&mut state);
                                state.input_owner = None;
                                match owner {
                                    InputOwner::Mcp(_) => {
                                        if let Some(dialog) = &mut state.mcp_dialog {
                                            dialog.overlay = McpOverlay::None;
                                        }
                                    }
                                    InputOwner::Skills(_) => {
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            dialog.overlay = SkillsOverlay::None;
                                        }
                                    }
                                    InputOwner::Plugins(_) => {
                                        if let Some(dialog) = &mut state.plugins_dialog {
                                            dialog.overlay = PluginOverlay::None;
                                        }
                                    }
                                    InputOwner::Profile(_) => {
                                        // Profile 借用不对应任何需要重置的 overlay
                                        // （重命名/字段编辑都是直接借用，不经过
                                        // 独立浮层），Esc 只需清空 live_input +
                                        // 归还 input_owner（上面已做）。
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let text = owner
                                    .live_input_mut(&mut state)
                                    .map(|ib| ib.display_lines().join("\n"));
                                match owner {
                                    InputOwner::Mcp(McpInputField::AddRegistryUrl) => {
                                        let url = text.filter(|t| !t.trim().is_empty());
                                        owner.clear_live_input(&mut state);
                                        state.input_owner = None;
                                        if let Some(base_url) = url {
                                            let result =
                                                wyj_store::registry::add_registry(&base_url);
                                            if let Some(dialog) = &mut state.mcp_dialog {
                                                dialog.overlay = McpOverlay::None;
                                                match result {
                                                    Ok(_) => dialog.refresh_registries(),
                                                    Err(e) => dialog.error = Some(e.to_string()),
                                                }
                                            }
                                        } else if let Some(dialog) = &mut state.mcp_dialog {
                                            dialog.overlay = McpOverlay::None;
                                        }
                                    }
                                    InputOwner::Mcp(McpInputField::BrowseSearch) => {
                                        let query = text.unwrap_or_default();
                                        if !query.trim().is_empty() {
                                            let base_url = state
                                                .mcp_dialog
                                                .as_ref()
                                                .map(|d| d.active_registry.base_url.clone())
                                                .unwrap_or_default();
                                            state.input_owner = None;
                                            if let Some(dialog) = &mut state.mcp_dialog {
                                                dialog.overlay = McpOverlay::Searching;
                                                dialog.error = None;
                                            }
                                            let tx = agent_tx.clone();
                                            tokio::spawn(async move {
                                                let client =
                                                    wyj_store::registry::RegistryClient::new(
                                                        base_url,
                                                    );
                                                let result = client
                                                    .search_servers(&query)
                                                    .await
                                                    .map_err(|e| e.to_string());
                                                let _ = tx
                                                    .send(AgentEvent::McpRegistryFetched { result })
                                                    .await;
                                            });
                                        }
                                        // 查询关键字留在 live_input 里不清空，方便同一次会话里
                                        // 再次微调关键字重新搜索（不像 AddRegistryUrl 提交后
                                        // 就不再需要看到原文本）。
                                    }
                                    InputOwner::Skills(SkillsInputField::AddMarketplaceUrl) => {
                                        let url = text.filter(|t| !t.trim().is_empty());
                                        state.input_owner = None;
                                        if let Some(git_url) = url {
                                            let add_result =
                                                wyj_store::marketplace::add_marketplace(&git_url);
                                            match add_result {
                                                Ok(source) => {
                                                    let marketplace_id = source.id.clone();
                                                    if let Some(dialog) = &mut state.skills_dialog {
                                                        dialog.overlay = SkillsOverlay::Syncing {
                                                            marketplace_id: marketplace_id.clone(),
                                                            git_url: git_url.clone(),
                                                        };
                                                        dialog.marketplaces.push(source);
                                                    }
                                                    let tx = agent_tx.clone();
                                                    let git_url_for_task = git_url.clone();
                                                    tokio::spawn(async move {
                                                        let result =
                                                            wyj_store::marketplace::sync_marketplace(
                                                                &git_url_for_task,
                                                            )
                                                            .await
                                                            .map_err(|e| e.to_string());
                                                        let _ = tx
                                                            .send(AgentEvent::SkillMarketplaceSynced {
                                                                marketplace_id,
                                                                git_url: git_url_for_task,
                                                                result,
                                                            })
                                                            .await;
                                                    });
                                                }
                                                Err(e) => {
                                                    if let Some(dialog) = &mut state.skills_dialog {
                                                        dialog.error = Some(e.to_string());
                                                        dialog.overlay = SkillsOverlay::None;
                                                    }
                                                }
                                            }
                                        } else if let Some(dialog) = &mut state.skills_dialog {
                                            dialog.overlay = SkillsOverlay::None;
                                        }
                                    }
                                    InputOwner::Plugins(PluginsInputField::AddMarketplaceUrl) => {
                                        let url = text.filter(|t| !t.trim().is_empty());
                                        state.input_owner = None;
                                        if let Some(location) = url {
                                            let add_result =
                                                wyj_store::plugin_install::add_plugin_marketplace(
                                                    &location,
                                                );
                                            match add_result {
                                                Ok(source) => {
                                                    let marketplace_id = source.id.clone();
                                                    if let Some(dialog) = &mut state.plugins_dialog
                                                    {
                                                        dialog.overlay = PluginOverlay::Syncing {
                                                            marketplace_id: marketplace_id.clone(),
                                                        };
                                                        dialog.marketplaces.push(source);
                                                    }
                                                    let tx = agent_tx.clone();
                                                    tokio::spawn(async move {
                                                        let result = wyj_store::plugin_install::sync_plugin_marketplace(&marketplace_id)
                                                            .await
                                                            .map_err(|e| e.to_string());
                                                        let _ = tx
                                                            .send(AgentEvent::PluginMarketplaceSynced {
                                                                marketplace_id,
                                                                result,
                                                            })
                                                            .await;
                                                    });
                                                }
                                                Err(e) => {
                                                    if let Some(dialog) = &mut state.plugins_dialog
                                                    {
                                                        dialog.error = Some(e.to_string());
                                                        dialog.overlay = PluginOverlay::None;
                                                    }
                                                }
                                            }
                                        } else if let Some(dialog) = &mut state.plugins_dialog {
                                            dialog.overlay = PluginOverlay::None;
                                        }
                                    }
                                    InputOwner::Plugins(PluginsInputField::AddLocalPluginPath) => {
                                        let path_text = text.filter(|t| !t.trim().is_empty());
                                        state.input_owner = None;
                                        if let Some(path_text) = path_text {
                                            let path = std::path::PathBuf::from(path_text);
                                            let result =
                                                wyj_store::plugin_install::install_local_plugin(
                                                    &path,
                                                    wyj_store::InstallScope::Global,
                                                    &state.cwd,
                                                );
                                            if let Some(dialog) = &mut state.plugins_dialog {
                                                match result {
                                                    Ok(report) => {
                                                        dialog.refresh_installed(&state.cwd);
                                                        dialog.overlay =
                                                            PluginOverlay::InstallReport { report };
                                                    }
                                                    Err(e) => {
                                                        dialog.overlay = PluginOverlay::None;
                                                        dialog.error = Some(wyj_i18n::tr_fmt(
                                                            "plugins.error.install_failed",
                                                            &[("err", &e.to_string())],
                                                        ));
                                                    }
                                                }
                                            }
                                        } else if let Some(dialog) = &mut state.plugins_dialog {
                                            dialog.overlay = PluginOverlay::None;
                                        }
                                    }
                                    InputOwner::Profile(ProfileInputField::Rename {
                                        entry_idx,
                                    }) => {
                                        let name = text.unwrap_or_default();
                                        owner.clear_live_input(&mut state);
                                        state.input_owner = None;
                                        if !name.trim().is_empty() {
                                            if let Some(dialog) = &mut state.profile_dialog {
                                                dialog.entries[entry_idx].name = name;
                                            }
                                        }
                                    }
                                    InputOwner::Profile(ProfileInputField::Field {
                                        entry_idx,
                                        field_idx,
                                    }) => {
                                        let value = text.unwrap_or_default();
                                        owner.clear_live_input(&mut state);
                                        state.input_owner = None;
                                        if let Some(dialog) = &mut state.profile_dialog {
                                            dialog.entries[entry_idx]
                                                .set_text_value(field_idx, value);
                                        }
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.backspace();
                                }
                            }
                            KeyCode::Delete => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.delete_char_forward();
                                }
                            }
                            KeyCode::Left => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.move_left();
                                }
                            }
                            KeyCode::Right => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.move_right();
                                }
                            }
                            KeyCode::Home => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.move_to_start_of_line();
                                }
                            }
                            KeyCode::End => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.move_to_end_of_line();
                                }
                            }
                            KeyCode::Char(c) => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    ib.insert_char(c);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.5 设置面板拦截（/config 命令触发，现仅 log_level/language 两个枚举字段）
                    if state.settings_dialog.is_some() {
                        match key.code {
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    if dialog.selected > 0 {
                                        dialog.selected -= 1;
                                    }
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    if dialog.selected + 1 < SETTINGS_FIELD_COUNT {
                                        dialog.selected += 1;
                                    }
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Left => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    let idx = dialog.selected;
                                    dialog.draft.cycle_enum(idx, false);
                                }
                            }
                            KeyCode::Right | KeyCode::Enter => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    let idx = dialog.selected;
                                    dialog.draft.cycle_enum(idx, true);
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let mut should_close = false;
                                if let Some(dialog) = &mut state.settings_dialog {
                                    {
                                        let new_cfg = dialog.draft.to_config(&state.config);
                                        match new_cfg.save() {
                                            Ok(()) => {
                                                should_close = true;
                                                state.config = new_cfg.clone();
                                                let lang = state
                                                    .config
                                                    .language
                                                    .clone()
                                                    .unwrap_or_else(|| {
                                                        wyj_i18n::detect_system_locale().to_string()
                                                    });
                                                wyj_i18n::set_locale(&lang);

                                                let model_for_mode = state
                                                    .config
                                                    .model_for_mode(&state.mode)
                                                    .to_string();
                                                match rebuild_fn(&state.config, &model_for_mode) {
                                                    Ok(new_agent) => {
                                                        // rebuild_fn 已装配完整 system prompt
                                                        //（英文主提示 + env 块），这里只拼回
                                                        // 模式追加段（如 Plan 限制说明）
                                                        let new_agent = new_agent.append_system(
                                                            system_prompt_extra
                                                                .trim_start()
                                                                .to_string(),
                                                        );
                                                        let new_agent = wire_tool_callback(
                                                            new_agent,
                                                            agent_tx.clone(),
                                                            todo_store.clone(),
                                                        );
                                                        *shared_agent.write().unwrap() =
                                                            Arc::new(new_agent);
                                                        state.model_name = model_for_mode;
                                                        state.context_window = state
                                                            .config
                                                            .active_profile()
                                                            .context_window;
                                                        state.messages.push(ChatMessage::system(
                                                            wyj_i18n::tr("settings.saved"),
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        // 提示词已不随 locale 变化，重建失败时
                                                        // 保留原 Agent 即可，无需改 system prompt
                                                        state.messages.push(
                                                            ChatMessage::assistant_err(
                                                                wyj_i18n::tr_fmt(
                                                                    "settings.rebuild_failed",
                                                                    &[("err", &e.to_string())],
                                                                ),
                                                            ),
                                                        );
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
                                if should_close {
                                    state.settings_dialog = None;
                                }
                            }
                            KeyCode::Esc => {
                                state.settings_dialog = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.55 CLAUDE.md 记忆面板拦截（/memory 命令触发）
                    if state.memory_dialog.is_some() {
                        match key.code {
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.memory_dialog {
                                    if dialog.selected > 0 {
                                        dialog.selected -= 1;
                                    }
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.memory_dialog {
                                    if dialog.selected + 1 < dialog.rows.len() {
                                        dialog.selected += 1;
                                    }
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                enum MemoryRowAction {
                                    Open(PathBuf),
                                    Toggle,
                                    None,
                                }
                                let action = match state
                                    .memory_dialog
                                    .as_ref()
                                    .and_then(|d| d.rows.get(d.selected))
                                {
                                    Some(MemoryRow::File(f)) => {
                                        MemoryRowAction::Open(f.path.clone())
                                    }
                                    Some(MemoryRow::AutoMemoryIndex { path, exists: true }) => {
                                        MemoryRowAction::Open(path.clone())
                                    }
                                    Some(MemoryRow::AutoMemoryToggle) => MemoryRowAction::Toggle,
                                    _ => MemoryRowAction::None,
                                };
                                match action {
                                    MemoryRowAction::Open(path) => {
                                        let result = open_path_in_editor(terminal, &path).await;
                                        if let Some(dialog) = &mut state.memory_dialog {
                                            match result {
                                                Ok(()) => dialog.error = None,
                                                Err(e) => {
                                                    dialog.error = Some(wyj_i18n::tr_fmt(
                                                        "memory.dialog.editor_failed",
                                                        &[("err", &e.to_string())],
                                                    ))
                                                }
                                            }
                                        }
                                    }
                                    MemoryRowAction::Toggle => {
                                        let new_value = !state
                                            .memory_dialog
                                            .as_ref()
                                            .map(|d| d.auto_memory_enabled)
                                            .unwrap_or(true);
                                        state.config.auto_memory_enabled = new_value;
                                        let save_result = state.config.save();
                                        if let Some(mem) = shared_agent.read().unwrap().memory_ref()
                                        {
                                            mem.set_enabled(new_value);
                                        }
                                        if let Some(dialog) = &mut state.memory_dialog {
                                            dialog.auto_memory_enabled = new_value;
                                            dialog.error = save_result.err().map(|e| e.to_string());
                                        }
                                    }
                                    MemoryRowAction::None => {}
                                }
                            }
                            KeyCode::Esc => {
                                state.memory_dialog = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.6 MCP server 管理面板拦截（/mcp 命令触发）
                    if state.mcp_dialog.is_some() {
                        // AddRegistry/Browse 搜索的文本输入统一走上面的 ⓪.1 输入借用
                        // 拦截（state.input_owner == Some(Mcp(_))），不会进入这里。

                        // ── 操作菜单已打开：Up/Down 选、Enter 确认/二次确认、Esc 逐级返回
                        if state.mcp_dialog.as_ref().unwrap().menu.is_some() {
                            mcp_handle_menu_key(&mut state, key.code, &agent_tx);
                            continue;
                        }

                        // ── 纯展示态（loading/详情）：吞掉按键，仅 Detail 支持 Enter/Esc 关闭
                        match &state.mcp_dialog.as_ref().unwrap().overlay {
                            McpOverlay::Searching | McpOverlay::Upgrading { .. } => {
                                continue;
                            }
                            McpOverlay::Detail { .. } => {
                                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                                    if let Some(dialog) = &mut state.mcp_dialog {
                                        dialog.overlay = McpOverlay::None;
                                    }
                                }
                                continue;
                            }
                            McpOverlay::AddRegistry => {
                                // 正常情况下 input_owner 已在上面拦截；这里只是防御性兜底。
                                continue;
                            }
                            McpOverlay::InstallConfirm { .. } => {
                                match key.code {
                                    KeyCode::Left | KeyCode::Right => {
                                        if let Some(dialog) = &mut state.mcp_dialog {
                                            if let McpOverlay::InstallConfirm { scope, .. } =
                                                &mut dialog.overlay
                                            {
                                                *scope = match scope {
                                                    wyj_store::InstallScope::Global => {
                                                        wyj_store::InstallScope::Project
                                                    }
                                                    wyj_store::InstallScope::Project => {
                                                        wyj_store::InstallScope::Global
                                                    }
                                                };
                                            }
                                        }
                                    }
                                    KeyCode::Char('y') | KeyCode::Enter => {
                                        let to_install = state.mcp_dialog.as_ref().and_then(|d| {
                                            if let McpOverlay::InstallConfirm {
                                                server,
                                                package,
                                                scope,
                                            } = &d.overlay
                                            {
                                                Some((
                                                    server.clone(),
                                                    package.clone(),
                                                    *scope,
                                                    d.active_registry.base_url.clone(),
                                                ))
                                            } else {
                                                None
                                            }
                                        });
                                        if let Some((server, package, scope, registry_url)) =
                                            to_install
                                        {
                                            let req = wyj_store::mcp_install::McpInstallRequest {
                                                server: *server,
                                                package,
                                                scope,
                                                name_override: None,
                                                registry_url,
                                            };
                                            let result = wyj_store::mcp_install::install_mcp_server(
                                                &req, &state.cwd,
                                            );
                                            if let Some(dialog) = &mut state.mcp_dialog {
                                                dialog.overlay = McpOverlay::None;
                                                match result {
                                                    Ok(()) => {
                                                        dialog.status =
                                                            Some(wyj_i18n::tr("mcp.install.done"));
                                                        dialog.refresh_installed(
                                                            &state.config,
                                                            &state.cwd,
                                                        );
                                                    }
                                                    Err(e) => {
                                                        dialog.error = Some(wyj_i18n::tr_fmt(
                                                            "mcp.error.install_failed",
                                                            &[("err", &e.to_string())],
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Esc => {
                                        if let Some(dialog) = &mut state.mcp_dialog {
                                            dialog.overlay = McpOverlay::None;
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                            McpOverlay::None => {}
                        }

                        // ── 无 overlay/菜单：方向键导航 ─────────────────────────
                        match key.code {
                            KeyCode::Esc => {
                                state.mcp_dialog = None;
                            }
                            KeyCode::Left => {
                                if let Some(dialog) = &mut state.mcp_dialog {
                                    dialog.tab = match dialog.tab {
                                        McpDialogTab::Installed => McpDialogTab::Browse,
                                        McpDialogTab::Registries => McpDialogTab::Installed,
                                        McpDialogTab::Browse => McpDialogTab::Registries,
                                    };
                                    dialog.cursor = 0;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.mcp_dialog {
                                    dialog.tab = match dialog.tab {
                                        McpDialogTab::Installed => McpDialogTab::Registries,
                                        McpDialogTab::Registries => McpDialogTab::Browse,
                                        McpDialogTab::Browse => McpDialogTab::Installed,
                                    };
                                    dialog.cursor = 0;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.mcp_dialog {
                                    dialog.cursor = dialog.cursor.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.mcp_dialog {
                                    let len = dialog.rows().len();
                                    if dialog.cursor + 1 < len {
                                        dialog.cursor += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let row = state
                                    .mcp_dialog
                                    .as_ref()
                                    .and_then(|d| d.rows().get(d.cursor).copied());
                                match row {
                                    Some(FlatRow::AddNew) => {
                                        let tab = state.mcp_dialog.as_ref().unwrap().tab;
                                        match tab {
                                            McpDialogTab::Registries => {
                                                if let Some(dialog) = &mut state.mcp_dialog {
                                                    dialog.overlay = McpOverlay::AddRegistry;
                                                    dialog.live_input = InputBox::new();
                                                }
                                                state.input_owner = Some(InputOwner::Mcp(
                                                    McpInputField::AddRegistryUrl,
                                                ));
                                            }
                                            McpDialogTab::Browse => {
                                                state.input_owner = Some(InputOwner::Mcp(
                                                    McpInputField::BrowseSearch,
                                                ));
                                            }
                                            McpDialogTab::Installed => {}
                                        }
                                    }
                                    Some(FlatRow::Entry(_)) => {
                                        if let Some(dialog) = &mut state.mcp_dialog {
                                            dialog.menu = dialog.build_menu();
                                        }
                                    }
                                    None => {}
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.65 Skill 管理面板拦截（/skills 命令触发）
                    if state.skills_dialog.is_some() {
                        // AddMarketplace 的文本输入统一走上面的 ⓪.1 输入借用拦截
                        // （state.input_owner == Some(Skills(_))），不会进入这里。

                        // ── 操作菜单已打开：Up/Down 选、Enter 确认/二次确认、Esc 逐级返回
                        if state.skills_dialog.as_ref().unwrap().menu.is_some() {
                            skills_handle_menu_key(&mut state, key.code, &agent_tx);
                            continue;
                        }

                        // ── 纯展示态（loading/详情）：吞掉按键，仅 Detail 支持 Enter/Esc 关闭
                        match &state.skills_dialog.as_ref().unwrap().overlay {
                            SkillsOverlay::Syncing { .. } | SkillsOverlay::Upgrading { .. } => {
                                continue;
                            }
                            SkillsOverlay::Detail { .. } => {
                                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                                    if let Some(dialog) = &mut state.skills_dialog {
                                        dialog.overlay = SkillsOverlay::None;
                                    }
                                }
                                continue;
                            }
                            SkillsOverlay::AddMarketplace => {
                                // 正常情况下 input_owner 已在上面拦截；这里只是防御性兜底。
                                continue;
                            }
                            SkillsOverlay::InstallConfirm { .. } => {
                                match key.code {
                                    KeyCode::Left | KeyCode::Right => {
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            if let SkillsOverlay::InstallConfirm { scope, .. } =
                                                &mut dialog.overlay
                                            {
                                                *scope = match scope {
                                                    wyj_store::InstallScope::Global => {
                                                        wyj_store::InstallScope::Project
                                                    }
                                                    wyj_store::InstallScope::Project => {
                                                        wyj_store::InstallScope::Global
                                                    }
                                                };
                                            }
                                        }
                                    }
                                    KeyCode::Char('y') | KeyCode::Enter => {
                                        let home = wyj_config::home_dir().unwrap_or_default();
                                        let req = state.skills_dialog.as_ref().and_then(|d| {
                                            if let SkillsOverlay::InstallConfirm {
                                                marketplace_id,
                                                git_url,
                                                entry,
                                                scope,
                                            } = &d.overlay
                                            {
                                                Some(
                                                    wyj_store::skill_install::SkillInstallRequest {
                                                        marketplace_id: marketplace_id.clone(),
                                                        marketplace_url: git_url.clone(),
                                                        entry: entry.clone(),
                                                        scope: *scope,
                                                        name_override: None,
                                                    },
                                                )
                                            } else {
                                                None
                                            }
                                        });
                                        if let Some(req) = req {
                                            let result = wyj_store::skill_install::install_skill(
                                                &req, &state.cwd,
                                            );
                                            if let Some(dialog) = &mut state.skills_dialog {
                                                dialog.overlay = SkillsOverlay::None;
                                                match result {
                                                    Ok(()) => {
                                                        dialog.status = Some(wyj_i18n::tr(
                                                            "skills.install.done",
                                                        ));
                                                        dialog.refresh_installed(&home, &state.cwd);
                                                    }
                                                    Err(e) => {
                                                        dialog.error = Some(wyj_i18n::tr_fmt(
                                                            "skills.error.install_failed",
                                                            &[("err", &e.to_string())],
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Esc => {
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            dialog.overlay = SkillsOverlay::None;
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                            SkillsOverlay::None => {}
                        }

                        // ── 无 overlay/菜单：方向键导航 ─────────────────────────
                        match key.code {
                            KeyCode::Esc => {
                                state.skills_dialog = None;
                            }
                            KeyCode::Left => {
                                if let Some(dialog) = &mut state.skills_dialog {
                                    dialog.tab = match dialog.tab {
                                        SkillsDialogTab::Installed => SkillsDialogTab::Browse,
                                        SkillsDialogTab::Marketplaces => SkillsDialogTab::Installed,
                                        SkillsDialogTab::Browse => SkillsDialogTab::Marketplaces,
                                    };
                                    dialog.cursor = 0;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.skills_dialog {
                                    dialog.tab = match dialog.tab {
                                        SkillsDialogTab::Installed => SkillsDialogTab::Marketplaces,
                                        SkillsDialogTab::Marketplaces => SkillsDialogTab::Browse,
                                        SkillsDialogTab::Browse => SkillsDialogTab::Installed,
                                    };
                                    dialog.cursor = 0;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.skills_dialog {
                                    dialog.cursor = dialog.cursor.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.skills_dialog {
                                    let len = dialog.rows().len();
                                    if dialog.cursor + 1 < len {
                                        dialog.cursor += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let row = state
                                    .skills_dialog
                                    .as_ref()
                                    .and_then(|d| d.rows().get(d.cursor).copied());
                                match row {
                                    Some(FlatRow::AddNew) => {
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            dialog.overlay = SkillsOverlay::AddMarketplace;
                                            dialog.live_input = InputBox::new();
                                        }
                                        state.input_owner = Some(InputOwner::Skills(
                                            SkillsInputField::AddMarketplaceUrl,
                                        ));
                                    }
                                    Some(FlatRow::Entry(_)) => {
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            dialog.menu = dialog.build_menu();
                                        }
                                    }
                                    None => {}
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.55 插件管理面板拦截（/plugins 命令触发）
                    if state.plugins_dialog.is_some() {
                        // AddMarketplace/AddLocalPlugin 的文本输入统一走上面的 ⓪.1 输入
                        // 借用拦截（state.input_owner == Some(Plugins(_))），不会进入这里。

                        // ── 操作菜单已打开：Up/Down 选、Enter 确认/二次确认、Esc 逐级返回
                        if state.plugins_dialog.as_ref().unwrap().menu.is_some() {
                            plugins_handle_menu_key(&mut state, key.code, &agent_tx);
                            continue;
                        }

                        // ── 纯展示态（loading/结果/详情）：吞掉按键，除 InstallReport/
                        //    Detail 支持 Enter/Esc 关闭外都直接吞掉（无交互）
                        match &state.plugins_dialog.as_ref().unwrap().overlay {
                            PluginOverlay::Syncing { .. }
                            | PluginOverlay::Installing
                            | PluginOverlay::Upgrading { .. } => {
                                continue;
                            }
                            PluginOverlay::InstallReport { .. } | PluginOverlay::Detail { .. } => {
                                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                                    if let Some(dialog) = &mut state.plugins_dialog {
                                        dialog.overlay = PluginOverlay::None;
                                    }
                                }
                                continue;
                            }
                            PluginOverlay::AddMarketplace | PluginOverlay::AddLocalPlugin => {
                                // 正常情况下 input_owner 已在上面拦截；这里只是防御性兜底。
                                continue;
                            }
                            PluginOverlay::InstallConfirm { .. } => {
                                match key.code {
                                    KeyCode::Left | KeyCode::Right => {
                                        if let Some(dialog) = &mut state.plugins_dialog {
                                            if let PluginOverlay::InstallConfirm { scope, .. } =
                                                &mut dialog.overlay
                                            {
                                                *scope = match scope {
                                                    wyj_store::InstallScope::Global => {
                                                        wyj_store::InstallScope::Project
                                                    }
                                                    wyj_store::InstallScope::Project => {
                                                        wyj_store::InstallScope::Global
                                                    }
                                                };
                                            }
                                        }
                                    }
                                    KeyCode::Char('y') | KeyCode::Enter => {
                                        let install_args =
                                            state.plugins_dialog.as_ref().and_then(|d| {
                                                if let PluginOverlay::InstallConfirm {
                                                    marketplace_id,
                                                    location,
                                                    entry,
                                                    scope,
                                                } = &d.overlay
                                                {
                                                    Some((
                                                        marketplace_id.clone(),
                                                        location.clone(),
                                                        entry.clone(),
                                                        *scope,
                                                    ))
                                                } else {
                                                    None
                                                }
                                            });
                                        if let Some((marketplace_id, location, entry, scope)) =
                                            install_args
                                        {
                                            if let Some(dialog) = &mut state.plugins_dialog {
                                                dialog.overlay = PluginOverlay::Installing;
                                            }
                                            let tx = agent_tx.clone();
                                            let cwd = state.cwd.clone();
                                            tokio::spawn(async move {
                                                let result = wyj_store::plugin_install::resolve_and_install_from_marketplace(
                                                    &marketplace_id,
                                                    &location,
                                                    &entry,
                                                    scope,
                                                    None,
                                                    &cwd,
                                                )
                                                .await
                                                .map_err(|e| e.to_string());
                                                let _ = tx
                                                    .send(AgentEvent::PluginInstalled { result })
                                                    .await;
                                            });
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Esc => {
                                        if let Some(dialog) = &mut state.plugins_dialog {
                                            dialog.overlay = PluginOverlay::None;
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                            PluginOverlay::None => {}
                        }

                        // ── 无 overlay/菜单：方向键导航 ─────────────────────────
                        match key.code {
                            KeyCode::Esc => {
                                state.plugins_dialog = None;
                            }
                            KeyCode::Left => {
                                if let Some(dialog) = &mut state.plugins_dialog {
                                    dialog.tab = match dialog.tab {
                                        PluginsDialogTab::Installed => PluginsDialogTab::Browse,
                                        PluginsDialogTab::Marketplaces => {
                                            PluginsDialogTab::Installed
                                        }
                                        PluginsDialogTab::Browse => PluginsDialogTab::Marketplaces,
                                    };
                                    dialog.cursor = 0;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.plugins_dialog {
                                    dialog.tab = match dialog.tab {
                                        PluginsDialogTab::Installed => {
                                            PluginsDialogTab::Marketplaces
                                        }
                                        PluginsDialogTab::Marketplaces => PluginsDialogTab::Browse,
                                        PluginsDialogTab::Browse => PluginsDialogTab::Installed,
                                    };
                                    dialog.cursor = 0;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.plugins_dialog {
                                    dialog.cursor = dialog.cursor.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.plugins_dialog {
                                    let len = dialog.rows().len();
                                    if dialog.cursor + 1 < len {
                                        dialog.cursor += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let row = state
                                    .plugins_dialog
                                    .as_ref()
                                    .and_then(|d| d.rows().get(d.cursor).copied());
                                match row {
                                    Some(FlatRow::AddNew) => {
                                        let tab = state.plugins_dialog.as_ref().unwrap().tab;
                                        match tab {
                                            PluginsDialogTab::Installed => {
                                                if let Some(dialog) = &mut state.plugins_dialog {
                                                    dialog.overlay = PluginOverlay::AddLocalPlugin;
                                                    dialog.live_input = InputBox::new();
                                                }
                                                state.input_owner = Some(InputOwner::Plugins(
                                                    PluginsInputField::AddLocalPluginPath,
                                                ));
                                            }
                                            PluginsDialogTab::Marketplaces => {
                                                if let Some(dialog) = &mut state.plugins_dialog {
                                                    dialog.overlay = PluginOverlay::AddMarketplace;
                                                    dialog.live_input = InputBox::new();
                                                }
                                                state.input_owner = Some(InputOwner::Plugins(
                                                    PluginsInputField::AddMarketplaceUrl,
                                                ));
                                            }
                                            PluginsDialogTab::Browse => {}
                                        }
                                    }
                                    Some(FlatRow::Entry(_)) => {
                                        if let Some(dialog) = &mut state.plugins_dialog {
                                            dialog.menu = dialog.build_menu();
                                        }
                                    }
                                    None => {}
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.58 可用 Agent 类型面板拦截（/agents 命令触发）
                    if state.extensions_dialog.is_some() {
                        let pending_action = state
                            .extensions_dialog
                            .as_ref()
                            .and_then(|dialog| dialog.confirm);
                        if let Some(action) = pending_action {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Enter => {
                                    let target = state
                                        .extensions_dialog
                                        .as_ref()
                                        .and_then(|dialog| dialog.selected_record())
                                        .map(|record| (record.id.clone(), record.scope));
                                    if let Some((id, scope)) = target {
                                        let cwd = state.cwd.clone();
                                        let result = match action {
                                            ExtensionAction::Enable => {
                                                wyj_store::extensions::set_enabled(
                                                    &id, scope, &cwd, true,
                                                )
                                            }
                                            ExtensionAction::Disable => {
                                                wyj_store::extensions::set_enabled(
                                                    &id, scope, &cwd, false,
                                                )
                                            }
                                            ExtensionAction::Remove => {
                                                wyj_store::extensions::remove(&id, scope, &cwd)
                                            }
                                        };
                                        if let Some(dialog) = &mut state.extensions_dialog {
                                            dialog.confirm = None;
                                            match result {
                                                Ok(()) => {
                                                    dialog.refresh(&cwd);
                                                    state.messages.push(ChatMessage::system(
                                                        format!(
                                                            "{} {id} — applies at the next Agent boundary",
                                                            ExtensionsDialog::action_label(action)
                                                        ),
                                                    ));
                                                }
                                                Err(e) => dialog.error = Some(e.to_string()),
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('n') | KeyCode::Esc => {
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.confirm = None;
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            match key.code {
                                KeyCode::Esc => state.extensions_dialog = None,
                                KeyCode::Up => {
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.move_selected(-1);
                                    }
                                }
                                KeyCode::Down => {
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.move_selected(1);
                                    }
                                }
                                KeyCode::PageUp => {
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.detail_scroll =
                                            dialog.detail_scroll.saturating_sub(8);
                                    }
                                }
                                KeyCode::PageDown => {
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.detail_scroll =
                                            dialog.detail_scroll.saturating_add(8);
                                    }
                                }
                                KeyCode::Enter | KeyCode::Char(' ') => {
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.detail_open = !dialog.detail_open;
                                        dialog.detail_scroll = 0;
                                    }
                                }
                                KeyCode::Char('e') | KeyCode::Char('d') | KeyCode::Char('x') => {
                                    let action = match key.code {
                                        KeyCode::Char('e') => ExtensionAction::Enable,
                                        KeyCode::Char('d') => ExtensionAction::Disable,
                                        _ => ExtensionAction::Remove,
                                    };
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        if action == ExtensionAction::Remove
                                            || dialog.selected_record().is_some_and(|record| {
                                                (action == ExtensionAction::Enable
                                                    && !record.enabled)
                                                    || (action == ExtensionAction::Disable
                                                        && record.enabled)
                                            })
                                        {
                                            dialog.confirm = Some(action);
                                        }
                                    }
                                }
                                KeyCode::Char('r') => {
                                    let cwd = state.cwd.clone();
                                    if let Some(dialog) = &mut state.extensions_dialog {
                                        dialog.refresh(&cwd);
                                    }
                                }
                                _ => {}
                            }
                        }
                        continue;
                    }

                    // ⓪.58 可用 Agent 类型面板拦截（/agents 命令触发）
                    if state.agents_dialog.is_some() {
                        state.ui_focus = UiFocus::AgentsCatalog;
                        match key.code {
                            KeyCode::Esc => {
                                state.agents_dialog = None;
                                state.ui_focus = UiFocus::Chat;
                            }
                            KeyCode::Up => state.move_focus_selection(-1),
                            KeyCode::Down => state.move_focus_selection(1),
                            KeyCode::PageUp => state.scroll_focus_lines(-8),
                            KeyCode::PageDown => state.scroll_focus_lines(8),
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                if let Some(dialog) = &mut state.agents_dialog {
                                    if !dialog.defs.is_empty() {
                                        dialog.detail_open = !dialog.detail_open;
                                        dialog.detail_scroll = 0;
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.6 分组管理面板拦截（/model 无参命令触发）
                    if state.profile_dialog.is_some() {
                        // 字段编辑/重命名的文本输入统一走上面的 ⓪.1 输入借用拦截
                        // （state.input_owner == Some(Profile(_))），不会进入这里。

                        // ── 操作菜单已打开：Up/Down 选、Enter 确认/二次确认、Esc 逐级返回
                        if state.profile_dialog.as_ref().unwrap().menu.is_some() {
                            profile_handle_menu_key(&mut state, key.code, &agent_tx);
                            continue;
                        }

                        // ── 纯展示态（FetchingModels）：仅 Esc 可取消
                        if matches!(
                            state.profile_dialog.as_ref().unwrap().overlay,
                            ProfileOverlay::FetchingModels { .. }
                        ) {
                            if key.code == KeyCode::Esc {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    dialog.overlay = ProfileOverlay::None;
                                }
                            }
                            continue;
                        }

                        // ── 新建分组模板选择器（不是 ActionMenu：这是"选一个模板创建新
                        // entry"，选中后触发的是"新增+进入重命名借用"的复合动作，不是
                        // 对已有条目执行菜单项）
                        if matches!(
                            state.profile_dialog.as_ref().unwrap().overlay,
                            ProfileOverlay::TemplatePicker { .. }
                        ) {
                            let mut pending_rename: Option<(usize, String)> = None;
                            if let Some(dialog) = &mut state.profile_dialog {
                                if let ProfileOverlay::TemplatePicker { selected } =
                                    &mut dialog.overlay
                                {
                                    match key.code {
                                        KeyCode::Up => {
                                            if *selected > 0 {
                                                *selected -= 1;
                                            }
                                        }
                                        KeyCode::Down => {
                                            if *selected + 1 < wyj_api::PROFILE_TEMPLATES.len() {
                                                *selected += 1;
                                            }
                                        }
                                        KeyCode::Enter => {
                                            let idx = *selected;
                                            let existing_names: Vec<String> = dialog
                                                .entries
                                                .iter()
                                                .map(|e| e.name.clone())
                                                .collect();
                                            let template = &wyj_api::PROFILE_TEMPLATES[idx];
                                            let new_entry = ProfileEntryDraft::from_template(
                                                template,
                                                &existing_names,
                                            );
                                            let suggested_name = new_entry.name.clone();
                                            dialog.entries.push(new_entry);
                                            let new_idx = dialog.entries.len() - 1;
                                            dialog.expanded = Some(new_idx);
                                            dialog.overlay = ProfileOverlay::None;
                                            // 不依赖行数算术反推新头行位置（加了固定
                                            // AddNew 尾行后容易差 1），直接搜索。
                                            dialog.cursor = dialog
                                                .rows()
                                                .iter()
                                                .position(|r| *r == ProfileRow::Header(new_idx))
                                                .unwrap_or(0);
                                            pending_rename = Some((new_idx, suggested_name));
                                        }
                                        KeyCode::Esc => {
                                            dialog.overlay = ProfileOverlay::None;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            // 新建后立即进入重命名借用态，而不是让用户自己发现"重命名"
                            // 菜单项——`dialog` 的可变借用已在上面的块内结束，这里才能
                            // 安全地写 state.input_owner（同一时刻不能双重可变借用 state）。
                            if let Some((new_idx, suggested_name)) = pending_rename {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    dialog.live_input = InputBox::new();
                                    dialog.live_input.insert_text(&suggested_name);
                                }
                                state.input_owner =
                                    Some(InputOwner::Profile(ProfileInputField::Rename {
                                        entry_idx: new_idx,
                                    }));
                            }
                            continue;
                        }

                        // ── Esc 未保存修改三选一确认
                        if matches!(
                            state.profile_dialog.as_ref().unwrap().overlay,
                            ProfileOverlay::UnsavedChanges { .. }
                        ) {
                            enum Choice {
                                None,
                                SaveClose,
                                DiscardClose,
                                BackToPanel,
                            }
                            let mut choice = Choice::None;
                            if let Some(dialog) = &mut state.profile_dialog {
                                if let ProfileOverlay::UnsavedChanges { selected } =
                                    &mut dialog.overlay
                                {
                                    match key.code {
                                        KeyCode::Up => {
                                            if *selected > 0 {
                                                *selected -= 1;
                                            }
                                        }
                                        KeyCode::Down => {
                                            if *selected + 1 < 3 {
                                                *selected += 1;
                                            }
                                        }
                                        KeyCode::Enter => {
                                            choice = match *selected {
                                                0 => Choice::SaveClose,
                                                1 => Choice::DiscardClose,
                                                _ => Choice::BackToPanel,
                                            };
                                        }
                                        // Esc 在这里的语义与面板内所有其它 overlay 完全
                                        // 一致：只收起当前这一层浮层、回到面板本身，绝不
                                        // 等同于"选中取消"再级联关闭整个面板——回到面板
                                        // 后再按 Esc，is_dirty() 仍为真会再次弹出，这是
                                        // 预期行为，不是死循环 bug。
                                        KeyCode::Esc => {
                                            dialog.overlay = ProfileOverlay::None;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            match choice {
                                Choice::None => {}
                                Choice::SaveClose => {
                                    let saved = profile_try_save(
                                        &mut state,
                                        &agent_tx,
                                        &rebuild_fn,
                                        &system_prompt_extra,
                                        &todo_store,
                                        &shared_agent,
                                    );
                                    if saved {
                                        state.input_owner = None;
                                        state.profile_dialog = None;
                                    } else if let Some(dialog) = &mut state.profile_dialog {
                                        // 保存失败：回到面板展示 error，而不是卡在三选一
                                        dialog.overlay = ProfileOverlay::None;
                                    }
                                }
                                Choice::DiscardClose => {
                                    state.input_owner = None;
                                    state.profile_dialog = None;
                                }
                                Choice::BackToPanel => {
                                    if let Some(dialog) = &mut state.profile_dialog {
                                        dialog.overlay = ProfileOverlay::None;
                                    }
                                }
                            }
                            continue;
                        }

                        // ── 无 overlay/菜单：方向键导航 ─────────────────────────
                        match key.code {
                            KeyCode::Esc => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    if dialog.is_dirty() {
                                        dialog.overlay =
                                            ProfileOverlay::UnsavedChanges { selected: 0 };
                                    } else {
                                        state.profile_dialog = None;
                                    }
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    dialog.cursor = dialog.cursor.saturating_sub(1);
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let len = dialog.rows().len();
                                    if dialog.cursor + 1 < len {
                                        dialog.cursor += 1;
                                    }
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Left => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    if let ProfileRow::Field(entry_idx, f) = dialog.selected_row() {
                                        if matches!(profile_field_kind(f), SettingsFieldKind::Enum)
                                        {
                                            dialog.entries[entry_idx].cycle_provider(false);
                                        }
                                    }
                                }
                            }
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    if let ProfileRow::Field(entry_idx, f) = dialog.selected_row() {
                                        if matches!(profile_field_kind(f), SettingsFieldKind::Enum)
                                        {
                                            dialog.entries[entry_idx].cycle_provider(true);
                                        }
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                let row = state.profile_dialog.as_ref().map(|d| d.selected_row());
                                match row {
                                    Some(ProfileRow::Header(_)) => {
                                        if let Some(dialog) = &mut state.profile_dialog {
                                            dialog.menu = dialog.build_menu();
                                        }
                                    }
                                    Some(ProfileRow::Field(entry_idx, f)) => {
                                        if PROFILE_MODEL_FIELD_IDXS.contains(&f) {
                                            // model/plan_model/exec_model：先弹"手动编辑 /
                                            // 从服务器拉取列表"小菜单（原 Ctrl+L 并入此处）。
                                            if let Some(dialog) = &mut state.profile_dialog {
                                                dialog.menu = dialog.build_menu();
                                            }
                                        } else if matches!(
                                            profile_field_kind(f),
                                            SettingsFieldKind::Enum
                                        ) {
                                            // provider：Enter 循环切换，与 Left/Right 等价，
                                            // 已经是方向键交互，不纳入菜单化/借用输入框。
                                            if let Some(dialog) = &mut state.profile_dialog {
                                                dialog.entries[entry_idx].cycle_provider(true);
                                            }
                                        } else {
                                            // base_url/api_key/max_tokens/context_window：
                                            // 直接借用底部输入框，不经过小菜单。
                                            let prefill = state
                                                .profile_dialog
                                                .as_ref()
                                                .map(|d| {
                                                    d.entries[entry_idx].text_value(f).to_string()
                                                })
                                                .unwrap_or_default();
                                            if let Some(dialog) = &mut state.profile_dialog {
                                                dialog.live_input = InputBox::new();
                                                dialog.live_input.insert_text(&prefill);
                                            }
                                            state.input_owner = Some(InputOwner::Profile(
                                                ProfileInputField::Field {
                                                    entry_idx,
                                                    field_idx: f,
                                                },
                                            ));
                                        }
                                    }
                                    Some(ProfileRow::AddNew) => {
                                        if let Some(dialog) = &mut state.profile_dialog {
                                            dialog.overlay =
                                                ProfileOverlay::TemplatePicker { selected: 0 };
                                        }
                                    }
                                    None => {}
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let saved = profile_try_save(
                                    &mut state,
                                    &agent_tx,
                                    &rebuild_fn,
                                    &system_prompt_extra,
                                    &todo_store,
                                    &shared_agent,
                                );
                                if saved {
                                    state.profile_dialog = None;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ① plan 批准对话框最高优先级：↑/↓ 选中 批准/继续规划/手动输入，
                    // Enter 确认（手动输入位先展开文本框，再次 Enter 提交）。
                    if let Some(dlg) = state.plan_dialog.as_mut() {
                        // Ctrl+C 在 plan 弹窗期间直接走 interrupt()。Agent 仍在
                        // `exit_plan_mode().await` 上挂起（ExitPlanMode 工具阻塞等
                        // 用户响应），弹窗分支以 `continue` 收尾会吞掉外层 Ctrl+C
                        // 处理，因此必须在此处显式识别。
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            if let Some(dlg) = state.plan_dialog.take() {
                                let _ = dlg.response_tx.send(false);
                            }
                            state.interrupt();
                            continue;
                        }
                        match dlg.handle_key(key.code) {
                            PlanApprovalOutcome::Continue => {}
                            PlanApprovalOutcome::Approve => {
                                if let Some(dlg) = state.plan_dialog.take() {
                                    let _ = dlg.response_tx.send(true);
                                    // 切换至执行模式；switch_mode 同步更新 shared_permission，
                                    // 对正在运行的这一轮（ExitPlanMode 调用所在的 turn）立即生效。
                                    let new_mode = AgentMode::Normal;
                                    switch_mode(&shared_mode, &shared_permission, new_mode.clone())
                                        .await;
                                    state.mode = new_mode;
                                    // Agent 会继续执行后续工具，恢复 spinner；批准分支
                                    // 也负责把 is_thinking 写回 true（PlanApprovalRequest
                                    // 阶段刻意不再预设 false，避免吞掉 Ctrl+C 中断路径）。
                                    state.is_thinking = true;
                                    state.messages.push(ChatMessage::system(
                                        "已批准计划，切换至执行模式。".to_string(),
                                    ));
                                }
                            }
                            PlanApprovalOutcome::Reject => {
                                // 拒绝 = 中断当前计划调研。用户随后可在输入框补充新指令
                                // 或修改方向；不再向 Agent 发送 "keep planning" 反馈让
                                // 它再写一版。仅解 oneshot + abort current_task 即可。
                                if let Some(dlg) = state.plan_dialog.take() {
                                    let _ = dlg.response_tx.send(false);
                                }
                                state.interrupt();
                                state.messages.push(ChatMessage::system(
                                    "已中断当前计划调研，可补充指令后继续。".to_string(),
                                ));
                            }
                            PlanApprovalOutcome::Feedback(text) => {
                                // 手动输入反馈：中断当前调研后，把反馈文本作为下一条用户
                                // 消息推进 pending_queue——主循环里 !is_thinking 且队列非空
                                // 的兜底分支会在下一帧自动发起新一轮对话（见该处理逻辑）。
                                if let Some(dlg) = state.plan_dialog.take() {
                                    let _ = dlg.response_tx.send(false);
                                }
                                state.interrupt();
                                state.pending_queue.push((text, vec![]));
                            }
                        }
                        continue;
                    }

                    // ①.5 检测到计划已批准仍在 plan 模式发消息 → 确认切换执行模式
                    if state.exec_mode_confirm.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if let Some(dlg) = state.exec_mode_confirm.take() {
                                    let new_mode = AgentMode::Normal;
                                    switch_mode(&shared_mode, &shared_permission, new_mode.clone())
                                        .await;
                                    state.mode = new_mode;
                                    state.messages.push(ChatMessage::system(
                                        "已切换至执行模式。".to_string(),
                                    ));
                                    let display_text = build_display_text(
                                        &dlg.pending_message,
                                        &dlg.pending_attachments,
                                    );
                                    state.push_user(display_text);
                                    state.is_thinking = true;
                                    state.spinner_frame = 0;
                                    state.turn_start_time = Some(Instant::now());
                                    state.turn_start_input_tokens = state.total_input_tokens;
                                    state.turn_start_output_tokens = state.total_output_tokens;
                                    let agent_c = shared_agent.read().unwrap().clone();
                                    let (handle, injector) = spawn_agent_turn(
                                        dlg.pending_message,
                                        dlg.pending_attachments,
                                        agent_c,
                                        session.clone(),
                                        agent_tx.clone(),
                                        cwd.clone(),
                                        shared_mode.clone(),
                                        shared_permission.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                    );
                                    state.current_task = Some(handle);
                                    state.injector = Some(injector);
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                if let Some(dlg) = state.exec_mode_confirm.take() {
                                    let display_text = build_display_text(
                                        &dlg.pending_message,
                                        &dlg.pending_attachments,
                                    );
                                    state.push_user(display_text);
                                    state.is_thinking = true;
                                    state.spinner_frame = 0;
                                    state.turn_start_time = Some(Instant::now());
                                    state.turn_start_input_tokens = state.total_input_tokens;
                                    state.turn_start_output_tokens = state.total_output_tokens;
                                    let agent_c = shared_agent.read().unwrap().clone();
                                    let (handle, injector) = spawn_agent_turn(
                                        dlg.pending_message,
                                        dlg.pending_attachments,
                                        agent_c,
                                        session.clone(),
                                        agent_tx.clone(),
                                        cwd.clone(),
                                        shared_mode.clone(),
                                        shared_permission.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                    );
                                    state.current_task = Some(handle);
                                    state.injector = Some(injector);
                                }
                            }
                            KeyCode::Esc => {
                                state.exec_mode_confirm = None;
                                state
                                    .messages
                                    .push(ChatMessage::system("已取消发送。".to_string()));
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ② AskQuestion 对话框优先拦截全部按键
                    if let Some(dlg) = &mut state.ask_question_dialog {
                        match dlg.handle_key(key.code) {
                            AskQuestionKeyOutcome::Continue => {}
                            AskQuestionKeyOutcome::Cancel => {
                                if let Some(dlg) = state.ask_question_dialog.take() {
                                    let _ = dlg.response_tx.send(None);
                                }
                            }
                            AskQuestionKeyOutcome::Submit => {
                                if let Some(mut dlg) = state.ask_question_dialog.take() {
                                    let answers = dlg.take_answers();
                                    let _ = dlg.response_tx.send(Some(answers));
                                }
                            }
                        }
                        continue;
                    }

                    // ③ 逐调用工具权限确认拦截：y/Enter=允许一次，a=始终允许，
                    //    d/Esc/其它=拒绝。决策经 oneshot 回传给挂起的 Agent 回合。
                    if state.permission_dialog.is_some() {
                        use wyj_tools::PermissionDecision;
                        let decision = match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                Some(PermissionDecision::AllowOnce)
                            }
                            KeyCode::Char('a') | KeyCode::Char('A') => {
                                Some(PermissionDecision::AllowAlways)
                            }
                            KeyCode::Char('d')
                            | KeyCode::Char('D')
                            | KeyCode::Char('n')
                            | KeyCode::Char('N')
                            | KeyCode::Esc => Some(PermissionDecision::Deny),
                            _ => None,
                        };
                        if let Some(decision) = decision {
                            if let Some(dlg) = state.permission_dialog.take() {
                                let _ = dlg.response_tx.send(decision);
                                if decision == PermissionDecision::AllowAlways {
                                    state.messages.push(ChatMessage::system(format!(
                                        "已始终允许工具 `{}`（已记入本项目）。",
                                        dlg.tool_name
                                    )));
                                } else if decision == PermissionDecision::Deny {
                                    state.messages.push(ChatMessage::system(format!(
                                        "已拒绝工具 `{}` 的执行。",
                                        dlg.tool_name
                                    )));
                                }
                            }
                        }
                        continue;
                    }

                    // ② @ 文件选取器拦截 ↑/↓/Tab/Enter/Esc
                    if !state.file_completions.is_empty() {
                        let fc_len = state.file_completions.len();
                        match key.code {
                            KeyCode::Up => {
                                if state.file_selected > 0 {
                                    state.file_selected -= 1;
                                }
                                continue;
                            }
                            KeyCode::Down => {
                                if state.file_selected + 1 < fc_len {
                                    state.file_selected += 1;
                                }
                                continue;
                            }
                            KeyCode::Tab | KeyCode::Enter => {
                                let entry = state.file_completions[state.file_selected].clone();
                                if entry.is_dir {
                                    replace_at_query(&mut input, &format!("{}/", entry.rel_path));
                                } else {
                                    replace_at_query(&mut input, &entry.rel_path);
                                    state.file_completions.clear();
                                }
                                update_file_completions(&mut state, &input, &cwd);
                                continue;
                            }
                            KeyCode::Esc => {
                                state.file_completions.clear();
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // ③ Slash 补全列表拦截 ↑/↓/Tab/Esc
                    if !state.slash_completions.is_empty() {
                        match key.code {
                            KeyCode::Up => {
                                if state.slash_selected > 0 {
                                    state.slash_selected -= 1;
                                }
                                continue;
                            }
                            KeyCode::Down => {
                                if state.slash_selected + 1 < state.slash_completions.len() {
                                    state.slash_selected += 1;
                                }
                                continue;
                            }
                            KeyCode::Tab => {
                                let selected =
                                    state.slash_completions[state.slash_selected].0.clone();
                                input = InputBox::new();
                                for c in selected.chars() {
                                    input.insert_char(c);
                                }
                                input.insert_char(' ');
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                continue;
                            }
                            KeyCode::Esc => {
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                continue;
                            }
                            _ => {} // 其他键落穿到下方正常处理
                        }
                    }

                    // Ctrl+D → 立即退出
                    if is_quit(&key) {
                        state.should_quit = true;
                        continue;
                    }

                    // Ctrl+C → 中断 Agent / 清空输入框 / 二次确认退出
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if state.is_thinking {
                            state.interrupt();
                        } else if !input.is_empty() {
                            input = InputBox::new();
                            state.slash_completions.clear();
                            state.file_completions.clear();
                            state.ctrl_c_pressed = false;
                            state.last_ctrl_c = None;
                        } else if state.ctrl_c_pressed {
                            state.should_quit = true;
                        } else {
                            state.ctrl_c_pressed = true;
                            state.last_ctrl_c = Some(Instant::now());
                        }
                        continue;
                    }

                    // ESC → 中断 Agent / 连按两次清空输入框
                    if key.code == KeyCode::Esc {
                        if state.close_panel_focus() {
                            continue;
                        }
                        if state.is_thinking {
                            state.interrupt();
                            state.last_esc = None;
                        } else {
                            let double_esc = state
                                .last_esc
                                .map(|t| t.elapsed() < Duration::from_millis(500))
                                .unwrap_or(false);
                            if double_esc && !input.is_empty() {
                                input = InputBox::new();
                                state.slash_completions.clear();
                                state.file_completions.clear();
                                state.last_esc = None;
                            } else {
                                state.last_esc = Some(Instant::now());
                            }
                        }
                        continue;
                    }

                    // Enter/Space → 展开/收起当前列表焦点的详情（仅输入框为空时生效）
                    if input.is_empty()
                        && state.slash_completions.is_empty()
                        && state.file_completions.is_empty()
                        && state.input_owner.is_none()
                        && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                    {
                        match state.ui_focus {
                            UiFocus::Todos => {
                                state.toggle_todo_detail();
                                continue;
                            }
                            UiFocus::SubAgents if state.selected_sub_agent.is_some() => {
                                state.sub_agent_detail_open = !state.sub_agent_detail_open;
                                state.sub_agent_detail_scroll = 0;
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // Shift+Tab → 循环切换模式
                    if key.code == KeyCode::BackTab {
                        let new_mode = cycle_mode(&state.mode);
                        let label = new_mode.label();
                        switch_mode(&shared_mode, &shared_permission, new_mode.clone()).await;
                        state.mode = new_mode;
                        state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                            "mode.switched",
                            &[("mode", label)],
                        )));
                        continue;
                    }

                    // Enter → 展开/收起当前选中的消息流概要项。若展开后用户滚走，
                    // 仍可再次 Enter 回到最近展开的块并收起。
                    if key.code == KeyCode::Enter
                        && key.modifiers.is_empty()
                        && input.is_empty()
                        && state.slash_completions.is_empty()
                        && state.file_completions.is_empty()
                        && state.input_owner.is_none()
                        && state.has_message_toggle_target()
                    {
                        state.toggle_selected_message();
                        continue;
                    }

                    // 其他按键重置 Ctrl+C 计数
                    state.ctrl_c_pressed = false;
                    state.last_ctrl_c = None;

                    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
                        input.insert_newline();
                        state.slash_completions.clear();
                    } else if key.code == KeyCode::Enter && !state.is_thinking {
                        if !input.is_empty() {
                            let text = input.take();
                            state.slash_completions.clear();
                            state.file_completions.clear();
                            state.history_idx = None;
                            state.history_draft = None;

                            let trimmed = text.trim().to_string();

                            // ── ! Bash 内联执行 ──────────────────────────────
                            if let Some(cmd_str) = trimmed.strip_prefix('!') {
                                let cmd_str = cmd_str.trim().to_string();
                                state.push_user(text.clone());
                                state.input_history.push(text);
                                let tx = agent_tx.clone();
                                let start = Instant::now();
                                tokio::spawn(async move {
                                    let elapsed;
                                    let (output, exit_code) =
                                        match tokio::process::Command::new("sh")
                                            .arg("-c")
                                            .arg(&cmd_str)
                                            .output()
                                            .await
                                        {
                                            Ok(out) => {
                                                elapsed = start.elapsed().as_secs_f64();
                                                let mut combined = String::new();
                                                let so = String::from_utf8_lossy(&out.stdout);
                                                let se = String::from_utf8_lossy(&out.stderr);
                                                if !so.is_empty() {
                                                    combined.push_str(&so);
                                                }
                                                if !se.is_empty() {
                                                    if !combined.is_empty() {
                                                        combined.push('\n');
                                                    }
                                                    combined.push_str(&se);
                                                }
                                                (combined, out.status.code().unwrap_or(-1))
                                            }
                                            Err(e) => {
                                                elapsed = start.elapsed().as_secs_f64();
                                                (format!("执行失败: {e}"), -1)
                                            }
                                        };
                                    let _ = tx
                                        .send(AgentEvent::BashResult {
                                            output,
                                            exit_code,
                                            elapsed_secs: elapsed,
                                        })
                                        .await;
                                });
                                continue;
                            }

                            // ── /plan 命令：直接切换至 plan 模式 ────────────
                            if trimmed == "/plan" {
                                let new_mode = AgentMode::Plan;
                                switch_mode(&shared_mode, &shared_permission, new_mode.clone())
                                    .await;
                                state.mode = new_mode;
                                state.messages.push(ChatMessage::system(wyj_i18n::tr(
                                    "mode.switched_to_plan",
                                )));
                                state.input_history.push(trimmed);
                                continue;
                            }

                            // ── /mode 命令：运行时切换模式 ───────────────────
                            // 注意：必须要求 "/mode" 后紧跟空格或结尾，否则 "/model" 这类
                            // 以 "/mode" 为前缀的命令会被误吞（strip_prefix 不检查词边界）。
                            if trimmed == "/mode" || trimmed.starts_with("/mode ") {
                                let args = trimmed.strip_prefix("/mode").unwrap().trim();
                                let new_mode = match args {
                                    "plan" => Some(AgentMode::Plan),
                                    "bypass" => Some(AgentMode::Bypass),
                                    "normal" | "" => Some(AgentMode::Normal),
                                    _ => None,
                                };
                                match new_mode {
                                    Some(m) => {
                                        let label = m.label();
                                        switch_mode(&shared_mode, &shared_permission, m.clone())
                                            .await;
                                        state.mode = m;
                                        state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                                            "mode.switched",
                                            &[("mode", label)],
                                        )));
                                    }
                                    None => {
                                        state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                                            "mode.unknown",
                                            &[("args", args)],
                                        )));
                                    }
                                }
                                state.input_history.push(trimmed);
                                continue;
                            }

                            // ── 其他 slash 命令 ─────────────────────────────
                            // Skill/Plugin 文件和 lockfile 可能刚刚由 `/extensions`、
                            // `/skills` 或 `/plugins` 修改；在每次 slash dispatch 前重建
                            // 轻量命令注册表，使下一次命令立即看到新资源，不要求重启。
                            let disabled_skills = wyj_store::disabled_skill_names(&cwd);
                            let mut current_plugin_skill_sources =
                                wyj_store::plugin_install::enabled_plugin_skill_paths(&cwd);
                            if let Some(local) = &local_plugin {
                                current_plugin_skill_sources.extend(local.skill_paths.clone());
                            }
                            cmd_registry = standard_registry_with_skills(
                                &std::env::var("HOME")
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_default(),
                                &cwd,
                                &disabled_skills,
                                &current_plugin_skill_sources,
                            );
                            let (estimated, cache_read, cache_write) = {
                                let sess = session.lock().await;
                                (
                                    wyj_core::estimate_tokens(&sess.messages),
                                    sess.total_cache_read_tokens,
                                    sess.total_cache_write_tokens,
                                )
                            };
                            let cmd_ctx = CommandContext {
                                cwd: cwd.clone(),
                                model: state.model_name.clone(),
                                input_tokens: state.total_input_tokens,
                                output_tokens: state.total_output_tokens,
                                cache_read_tokens: cache_read,
                                cache_write_tokens: cache_write,
                                context_window,
                                estimated_tokens: estimated,
                                home_dir: std::env::var("HOME")
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_default(),
                                sub_input_tokens: state.sub_input_tokens,
                                sub_output_tokens: state.sub_output_tokens,
                                effective_mcp_count: wyj_store::mcp_install::effective_mcp_servers(
                                    &state.config,
                                    &cwd,
                                )
                                .len()
                                    + local_plugin
                                        .as_ref()
                                        .map(|p| p.mcp_servers.len())
                                        .unwrap_or(0),
                                plugin_agent_paths: {
                                    let mut paths =
                                        wyj_store::plugin_install::enabled_plugin_agent_paths(&cwd);
                                    if let Some(local) = &local_plugin {
                                        paths.extend(local.agent_paths.clone());
                                    }
                                    paths
                                },
                                hooks_enabled: state
                                    .hook_runner
                                    .as_ref()
                                    .is_some_and(|r| r.is_enabled()),
                                dynamic_commands: cmd_registry
                                    .list()
                                    .iter()
                                    .filter(|c| c.is_dynamic())
                                    .map(|c| (c.name().to_string(), c.description(), c.usage()))
                                    .collect(),
                            };
                            if let Some(result) = cmd_registry.dispatch(&trimmed, &cmd_ctx).await {
                                match result {
                                    Ok(CommandResult::Output(out)) => {
                                        state.messages.push(ChatMessage::assistant(out));
                                    }
                                    Ok(CommandResult::ClearHistory) => {
                                        state.messages.clear();
                                        state.total_input_tokens = 0;
                                        state.total_output_tokens = 0;
                                        state.context_tokens = 0;
                                        state.pending_attachments.clear();
                                        state.turns = 0;
                                        state.tool_call_count = 0;
                                        state.tool_info.clear();
                                        state.selected_message_id = None;
                                        state.selected_message_anchor = None;
                                        state.last_toggled_message_id = None;
                                        state.message_detail_scroll.clear();
                                        state.current_todos = None;
                                        state.todo_stats.clear();
                                        state.todo_execution_logs.clear();
                                        state.todo_panel_expanded = false;
                                        state.selected_todo_id = None;
                                        state.todo_detail_open = false;
                                        state.todo_detail_scroll = 0;
                                        state.agents_dialog = None;
                                        state.ui_focus = UiFocus::Chat;
                                        state.sub_input_tokens = 0;
                                        state.sub_output_tokens = 0;
                                        state.sub_agent_trace_cache.clear();
                                        state.pending_queue.clear();
                                        state.pending_bg_reminders.clear();
                                        state.streaming_buf.clear();
                                        state.thinking_buf.clear();
                                        state.thinking_started = None;
                                        // 已冻结写入终端真实 scrollback 的历史消息（Inline
                                        // viewport 架构下 insert_before 直接落进终端原生回
                                        // 滚缓冲区，state.messages.clear() 管不到它）不额外
                                        // Purge 的话 /clear 后终端里仍能翻到旧对话；这里连
                                        // 带把 scrollback 真正清空，frozen_up_to 归零重来。
                                        state.frozen_up_to = 0;
                                        // /clear 不重新显示欢迎页（对齐 Claude Code：清空对话
                                        // 不等于回到全新会话开屏），否则每次 /clear 都会在
                                        // 下一次冻结时把欢迎页重新画一遍。
                                        state.welcome_frozen = true;
                                        execute!(io::stdout(), Clear(ClearType::Purge))?;
                                        let mut sess = session.lock().await;
                                        *sess = Session::new();
                                        state.messages.push(ChatMessage::assistant(
                                            "对话已清空。".to_string(),
                                        ));
                                    }
                                    Ok(CommandResult::CompactHistory) => {
                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let mut sess = session.lock().await;
                                        match agent_c.compact_context(&mut sess).await {
                                            Ok(r) if r.messages_removed > 0 => {
                                                state.context_tokens =
                                                    wyj_core::estimate_tokens(&sess.messages);
                                                state.messages.push(ChatMessage::assistant(
                                                    format!(
                                                        "已压缩：移除 {} 条消息，节省约 {} tokens",
                                                        r.messages_removed, r.tokens_saved_estimate
                                                    ),
                                                ));
                                            }
                                            Ok(_) => {
                                                state.messages.push(ChatMessage::assistant(
                                                    "上下文较短，无需压缩。".to_string(),
                                                ));
                                            }
                                            Err(e) => {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    format!("[压缩失败] {e}"),
                                                ));
                                            }
                                        }
                                    }
                                    Ok(CommandResult::OpenProfileDialog) => {
                                        state.profile_dialog =
                                            Some(ProfileDialog::new(&state.config));
                                    }
                                    Ok(CommandResult::SwitchProfile(name)) => {
                                        if !state.config.profiles.iter().any(|p| p.name == name) {
                                            state.messages.push(ChatMessage::assistant_err(
                                                wyj_i18n::tr_fmt(
                                                    "profile.not_found",
                                                    &[("name", &name)],
                                                ),
                                            ));
                                        } else {
                                            let mut new_cfg = state.config.clone();
                                            new_cfg.active_profile = name.clone();
                                            match new_cfg.save() {
                                                Ok(()) => {
                                                    state.config = new_cfg.clone();
                                                    let model_for_mode = state
                                                        .config
                                                        .model_for_mode(&state.mode)
                                                        .to_string();
                                                    match rebuild_fn(&state.config, &model_for_mode)
                                                    {
                                                        Ok(new_agent) => {
                                                            let new_agent = wire_tool_callback(
                                                                new_agent,
                                                                agent_tx.clone(),
                                                                todo_store.clone(),
                                                            );
                                                            *shared_agent.write().unwrap() =
                                                                Arc::new(new_agent);
                                                            state.model_name = model_for_mode;
                                                            state.context_window = state
                                                                .config
                                                                .active_profile()
                                                                .context_window;
                                                            state.messages.push(
                                                                ChatMessage::assistant(
                                                                    wyj_i18n::tr_fmt(
                                                                        "profile.switched",
                                                                        &[("name", &name)],
                                                                    ),
                                                                ),
                                                            );
                                                        }
                                                        Err(e) => {
                                                            state.messages.push(
                                                                ChatMessage::assistant_err(
                                                                    wyj_i18n::tr_fmt(
                                                                        "profile.switch_failed",
                                                                        &[("err", &e.to_string())],
                                                                    ),
                                                                ),
                                                            );
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    state.messages.push(
                                                        ChatMessage::assistant_err(
                                                            wyj_i18n::tr_fmt(
                                                                "profile.switch_failed",
                                                                &[("err", &e.to_string())],
                                                            ),
                                                        ),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Ok(CommandResult::OpenSessionPicker) => {
                                        if state.is_thinking {
                                            state.messages.push(ChatMessage::assistant(
                                                "请等待当前任务完成后再切换会话。".to_string(),
                                            ));
                                        } else if let Some(store) = &session_store {
                                            // 按项目隔离：只列当前项目（git 仓库根）的会话
                                            match store.list_for_project(&state.cwd) {
                                                Ok(sessions) if sessions.is_empty() => {
                                                    state.messages.push(ChatMessage::assistant(
                                                        "当前项目还没有历史会话。".to_string(),
                                                    ));
                                                }
                                                Ok(sessions) => {
                                                    state.session_picker =
                                                        Some(SessionPickerState {
                                                            sessions,
                                                            selected: 0,
                                                        });
                                                }
                                                Err(e) => {
                                                    state.messages.push(
                                                        ChatMessage::assistant_err(format!(
                                                            "[会话列表失败] {e}"
                                                        )),
                                                    );
                                                }
                                            }
                                        } else {
                                            state.messages.push(ChatMessage::assistant(
                                                "会话存储未初始化，无法加载会话列表。".to_string(),
                                            ));
                                        }
                                    }
                                    Ok(CommandResult::RunPrompt(prompt)) => {
                                        // Skill 展开后的 prompt → 当作用户消息发给 agent
                                        state.push_user(prompt.clone());
                                        state.is_thinking = true;
                                        state.spinner_frame = 0;
                                        state.turn_start_time = Some(Instant::now());
                                        state.turn_start_input_tokens = state.total_input_tokens;
                                        state.turn_start_output_tokens = state.total_output_tokens;

                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let session_c = session.clone();
                                        let tx = agent_tx.clone();
                                        let ctx_cwd = cwd.clone();
                                        let mode_arc = shared_mode.clone();
                                        let shared_permission_c = shared_permission.clone();
                                        let ui_ask_tx_clone = ui_ask_tx.clone();

                                        let handle = tokio::spawn(async move {
                                            let mut sess = session_c.lock().await;
                                            sess.push_user(prompt);
                                            let current_mode = mode_arc.lock().await.clone();
                                            let mut ctx = ToolCtx::new(&ctx_cwd);
                                            ctx.permission_mode = shared_permission_c;
                                            ctx.ui_ask_tx = Some(ui_ask_tx_clone);
                                            let turn_agent =
                                                plan_turn_agent(&agent_c, &current_mode);
                                            let tx2 = tx.clone();
                                            let mut on_text = move |d: &str| {
                                                let _ = tx2
                                                    .try_send(AgentEvent::TextDelta(d.to_string()));
                                            };
                                            match turn_agent
                                                .run_turn(&mut sess, &ctx, &mut on_text)
                                                .await
                                            {
                                                Ok(_) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Usage {
                                                            input: sess.total_input_tokens,
                                                            output: sess.total_output_tokens,
                                                            context_tokens:
                                                                wyj_core::estimate_tokens(
                                                                    &sess.messages,
                                                                ),
                                                        })
                                                        .await;
                                                    let _ = tx.send(AgentEvent::TurnDone).await;
                                                }
                                                Err(e) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Error(e.to_string()))
                                                        .await;
                                                }
                                            }
                                        });
                                        state.current_task = Some(handle.abort_handle());
                                    }
                                    Ok(CommandResult::RunPromptScoped {
                                        text,
                                        allowed_tools,
                                        profile,
                                    }) => {
                                        // 带 allowed-tools 的自定义命令：临时把 permission_mode
                                        // 收紧为 Allowlist，这一轮跑完（含 ESC 中断的场景，靠
                                        // RestorePermissionOnDrop 的 Drop 兜底）自动还原快照；
                                        // 不改 shared_mode，状态栏 Normal/Plan/Bypass 显示不受
                                        // 影响。`profile` 若存在，则只为这一轮构造一个临时
                                        // Agent；主会话 active profile 不会被修改。
                                        state.push_user(text.clone());
                                        state.is_thinking = true;
                                        state.spinner_frame = 0;
                                        state.turn_start_time = Some(Instant::now());
                                        state.turn_start_input_tokens = state.total_input_tokens;
                                        state.turn_start_output_tokens = state.total_output_tokens;

                                        let agent_c = if let Some(profile_name) = profile {
                                            let mut scoped_cfg = state.config.clone();
                                            if scoped_cfg.profile_by_name(&profile_name).is_none() {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    format!(
                                                        "[skill] 未找到 Profile: {profile_name}"
                                                    ),
                                                ));
                                                continue;
                                            }
                                            scoped_cfg.active_profile = profile_name;
                                            let scoped_model =
                                                scoped_cfg.model_for_mode(&state.mode).to_string();
                                            match rebuild_fn(&scoped_cfg, &scoped_model) {
                                                Ok(agent) => Arc::new(agent),
                                                Err(e) => {
                                                    state.messages.push(
                                                        ChatMessage::assistant_err(format!(
                                                        "[skill] 构造临时 Profile Agent 失败: {e}"
                                                    )),
                                                    );
                                                    continue;
                                                }
                                            }
                                        } else {
                                            shared_agent.read().unwrap().clone()
                                        };
                                        let session_c = session.clone();
                                        let tx = agent_tx.clone();
                                        let ctx_cwd = cwd.clone();
                                        let mode_arc = shared_mode.clone();
                                        let shared_permission_c = shared_permission.clone();
                                        let ui_ask_tx_clone = ui_ask_tx.clone();

                                        let handle = tokio::spawn(async move {
                                            let mut sess = session_c.lock().await;
                                            sess.push_user(text);
                                            let current_mode = mode_arc.lock().await.clone();
                                            let mut ctx = ToolCtx::new(&ctx_cwd);
                                            ctx.permission_mode = shared_permission_c.clone();
                                            ctx.ui_ask_tx = Some(ui_ask_tx_clone);
                                            let turn_agent =
                                                plan_turn_agent(&agent_c, &current_mode);

                                            // 临时收紧 + RAII 兜底还原（ESC 中断会直接 drop 这个
                                            // future，只有 Drop 才保证一定跑到，plain 的
                                            // "跑完再还原" 在中断路径下不会执行）。
                                            let _restore_guard = allowed_tools.map(|tools| {
                                                let prev =
                                                    shared_permission_c.read().unwrap().clone();
                                                *shared_permission_c.write().unwrap() =
                                                    PermissionMode::Allowlist(
                                                        tools.into_iter().collect(),
                                                    );
                                                RestorePermissionOnDrop {
                                                    handle: shared_permission_c.clone(),
                                                    prev,
                                                }
                                            });

                                            let tx2 = tx.clone();
                                            let mut on_text = move |d: &str| {
                                                let _ = tx2
                                                    .try_send(AgentEvent::TextDelta(d.to_string()));
                                            };
                                            match turn_agent
                                                .run_turn(&mut sess, &ctx, &mut on_text)
                                                .await
                                            {
                                                Ok(_) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Usage {
                                                            input: sess.total_input_tokens,
                                                            output: sess.total_output_tokens,
                                                            context_tokens:
                                                                wyj_core::estimate_tokens(
                                                                    &sess.messages,
                                                                ),
                                                        })
                                                        .await;
                                                    let _ = tx.send(AgentEvent::TurnDone).await;
                                                }
                                                Err(e) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Error(e.to_string()))
                                                        .await;
                                                }
                                            }
                                        });
                                        state.current_task = Some(handle.abort_handle());
                                    }
                                    Ok(CommandResult::ResumeSession(id)) => {
                                        if state.is_thinking {
                                            state.messages.push(ChatMessage::assistant(
                                                "请等待当前任务完成后再切换会话。".to_string(),
                                            ));
                                        } else if let Some(store) = &session_store {
                                            // 自动保存当前会话
                                            {
                                                let sess = session.lock().await;
                                                if !sess.messages.is_empty() {
                                                    let (title, title_generated) = match store
                                                        .load(&current_session_id)
                                                        .ok()
                                                    {
                                                        Some(f) if f.title_generated => {
                                                            (f.title, true)
                                                        }
                                                        _ => (extract_title(&sess.messages), false),
                                                    };
                                                    let _ = store.save(&SessionFile {
                                                        session_id: current_session_id.clone(),
                                                        title,
                                                        last_preview: extract_preview(
                                                            &sess.messages,
                                                        ),
                                                        cwd: cwd.display().to_string(),
                                                        timestamp: now_iso(),
                                                        turns: state.turns,
                                                        input_tokens: sess.total_input_tokens,
                                                        output_tokens: sess.total_output_tokens,
                                                        messages: sess.messages.clone(),
                                                        title_generated,
                                                    });
                                                }
                                            }
                                            // 加载目标会话
                                            match store.load(&id) {
                                                Ok(file) => {
                                                    let display_msgs =
                                                        reconstruct_display(&file.messages);
                                                    let mut sess = session.lock().await;
                                                    sess.total_input_tokens = file.input_tokens;
                                                    sess.total_output_tokens = file.output_tokens;
                                                    sess.messages = file.messages;
                                                    let context_tokens =
                                                        wyj_core::estimate_tokens(&sess.messages);
                                                    let plan_approved =
                                                        has_plan_approved(&sess.messages);
                                                    drop(sess);
                                                    current_session_id = file.session_id.clone();
                                                    state.current_session_id =
                                                        current_session_id.clone();
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
                                                    state.context_tokens = context_tokens;
                                                    state.turns = file.turns;
                                                    state.frozen_up_to = 0;
                                                    state.welcome_frozen = true;
                                                    state.selected_message_id = None;
                                                    state.selected_message_anchor = None;
                                                    state.last_toggled_message_id = None;
                                                    state.message_detail_scroll.clear();
                                                    state.current_todos = None;
                                                    state.todo_stats.clear();
                                                    state.todo_execution_logs.clear();
                                                    state.todo_panel_expanded = false;
                                                    state.selected_todo_id = None;
                                                    state.todo_detail_open = false;
                                                    state.todo_detail_scroll = 0;
                                                    state.sub_agent_trace_cache.clear();
                                                    state.sub_agents = reload_persisted_sub_agents(
                                                        store.dir(),
                                                        &current_session_id,
                                                    );
                                                    state.selected_sub_agent = None;
                                                    state.sub_agent_detail_open = false;
                                                    state.sub_agent_detail_scroll = 0;
                                                    state.messages.push(ChatMessage::system(
                                                        format!(
                                                            "已恢复会话 {}  共 {} 轮对话",
                                                            file.session_id, file.turns
                                                        ),
                                                    ));
                                                    if state.mode == AgentMode::Plan
                                                        && plan_approved
                                                    {
                                                        state.messages.push(ChatMessage::system(
                                                            "该会话计划已批准，继续输入时会提示切换执行模式"
                                                                .to_string(),
                                                        ));
                                                    }
                                                }
                                                Err(e) => {
                                                    state.messages.push(
                                                        ChatMessage::assistant_err(format!(
                                                            "[会话不存在或加载失败] {e}"
                                                        )),
                                                    );
                                                }
                                            }
                                        } else {
                                            state.messages.push(ChatMessage::assistant(
                                                "会话存储未初始化。".to_string(),
                                            ));
                                        }
                                    }
                                    Ok(CommandResult::OpenSettingsDialog) => {
                                        state.settings_dialog =
                                            Some(SettingsDialog::new(&state.config));
                                    }
                                    Ok(CommandResult::OpenMemoryDialog) => {
                                        let pid = wyj_core::project_id(&state.cwd);
                                        let index_path = wyj_config::home_dir()
                                            .unwrap_or_default()
                                            .join(".wyj-code")
                                            .join("memory")
                                            .join(pid)
                                            .join("MEMORY.md");
                                        state.memory_dialog = Some(MemoryDialog::new(
                                            &state.cwd,
                                            index_path,
                                            state.config.auto_memory_enabled,
                                        ));
                                    }
                                    Ok(CommandResult::OpenMcpDialog) => {
                                        state.mcp_dialog =
                                            Some(McpDialog::new(&state.config, &state.cwd));
                                    }
                                    Ok(CommandResult::OpenSkillsDialog) => {
                                        let home = wyj_config::home_dir().unwrap_or_default();
                                        state.skills_dialog =
                                            Some(SkillsDialog::new(&home, &state.cwd));
                                    }
                                    Ok(CommandResult::OpenPluginsDialog) => {
                                        state.plugins_dialog = Some(PluginsDialog::new(&state.cwd));
                                    }
                                    Ok(CommandResult::OpenExtensionsDialog) => {
                                        state.extensions_dialog =
                                            Some(ExtensionsDialog::new(&state.cwd));
                                    }
                                    Ok(CommandResult::OpenAgentsDialog { defs, .. }) => {
                                        state.agents_dialog = Some(AgentsDialog::new(defs));
                                        state.ui_focus = UiFocus::AgentsCatalog;
                                    }
                                    Ok(CommandResult::OpenSubAgentsPanel(target_id)) => {
                                        apply_open_subagents_panel(&mut state, target_id);
                                    }
                                    Ok(CommandResult::Quit) | Ok(CommandResult::None) => {
                                        state.should_quit = true;
                                    }
                                    Err(e) => {
                                        state.messages.push(ChatMessage::assistant_err(format!(
                                            "[命令错误] {e}"
                                        )));
                                    }
                                }
                                state.input_history.push(trimmed);
                            } else {
                                // ── 普通消息 → 发给 agent ───────────────────
                                // 展开 @file 引用 → 追加到 pending_attachments
                                expand_at_refs_to_attachments(
                                    &text,
                                    &cwd,
                                    &mut state.pending_attachments,
                                );

                                // plan 模式下若历史中已有获批的 ExitPlanMode → 拦截，弹确认框
                                let plan_already_approved = matches!(state.mode, AgentMode::Plan)
                                    && session
                                        .try_lock()
                                        .map(|sess| has_plan_approved(&sess.messages))
                                        .unwrap_or(false);

                                if plan_already_approved {
                                    state.exec_mode_confirm = Some(ExecModeConfirmDialog {
                                        pending_message: text.clone(),
                                        pending_attachments: std::mem::take(
                                            &mut state.pending_attachments,
                                        ),
                                    });
                                    state.input_history.push(text);
                                } else {
                                    let display_text =
                                        build_display_text(&text, &state.pending_attachments);
                                    state.push_user(display_text);
                                    state.input_history.push(text.clone());
                                    state.is_thinking = true;
                                    state.spinner_frame = 0;
                                    state.turn_start_time = Some(Instant::now());
                                    state.turn_start_input_tokens = state.total_input_tokens;
                                    state.turn_start_output_tokens = state.total_output_tokens;

                                    // 捕获并清空附件列表（移入 async task）
                                    let attachments =
                                        std::mem::take(&mut state.pending_attachments);
                                    let agent_c = shared_agent.read().unwrap().clone();
                                    let (handle, injector) = spawn_agent_turn(
                                        text,
                                        attachments,
                                        agent_c,
                                        session.clone(),
                                        agent_tx.clone(),
                                        cwd.clone(),
                                        shared_mode.clone(),
                                        shared_permission.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                    );
                                    state.current_task = Some(handle);
                                    state.injector = Some(injector);
                                }
                            }
                        }
                    } else if key.code == KeyCode::Enter && state.is_thinking {
                        // Agent 忙碌期间提交 → 排队，不打断当前操作
                        if !input.is_empty() || !state.pending_attachments.is_empty() {
                            let text = input.take();
                            state.slash_completions.clear();
                            state.file_completions.clear();
                            state.history_idx = None;
                            state.history_draft = None;
                            expand_at_refs_to_attachments(
                                &text,
                                &cwd,
                                &mut state.pending_attachments,
                            );
                            let attachments = std::mem::take(&mut state.pending_attachments);
                            if let Some(tx) = &state.injector {
                                let blocks =
                                    build_user_blocks(text.clone(), attachments.clone()).await;
                                let _ = tx.send((blocks, InjectionKind::UserMessage));
                                state.pending_queue.push((text, attachments));
                            } else {
                                // 理论上不应发生：is_thinking 但没有活跃任务，按普通消息兜底处理
                                let display_text = build_display_text(&text, &attachments);
                                state.push_user(display_text);
                                state.input_history.push(text);
                                state.pending_attachments = attachments;
                            }
                        }
                    } else if key.code == KeyCode::Up {
                        if input.is_empty()
                            && state.slash_completions.is_empty()
                            && state.file_completions.is_empty()
                            && state.input_owner.is_none()
                        {
                            state.move_focus_selection(-1);
                        } else if state.slash_completions.is_empty() && input.cursor_row == 0 {
                            if !state.is_thinking && !state.input_history.is_empty() {
                                if state.history_idx.is_none() {
                                    state.history_draft = Some(input.lines.join("\n"));
                                }
                                let hist_len = state.input_history.len();
                                let new_idx = match state.history_idx {
                                    None => hist_len - 1,
                                    Some(i) => i.saturating_sub(1),
                                };
                                state.history_idx = Some(new_idx);
                                let recalled = state.input_history[new_idx].clone();
                                input.set_text(&recalled);
                            }
                            // is_thinking 中或无历史记录 → 无操作，不 fallback 到滚动会话
                        } else if state.slash_completions.is_empty() {
                            input.move_cursor_up();
                        }
                    } else if key.code == KeyCode::Down {
                        if input.is_empty()
                            && state.slash_completions.is_empty()
                            && state.file_completions.is_empty()
                            && state.input_owner.is_none()
                        {
                            state.move_focus_selection(1);
                        } else if state.slash_completions.is_empty()
                            && input.cursor_row + 1 == input.lines.len()
                        {
                            if !state.is_thinking {
                                if let Some(idx) = state.history_idx {
                                    if idx + 1 < state.input_history.len() {
                                        let new_idx = idx + 1;
                                        state.history_idx = Some(new_idx);
                                        let recalled = state.input_history[new_idx].clone();
                                        input.set_text(&recalled);
                                    } else {
                                        // 超出历史末尾 → 退出导航态，恢复进入导航前的草稿
                                        state.history_idx = None;
                                        let draft = state.history_draft.take().unwrap_or_default();
                                        input.set_text(&draft);
                                    }
                                }
                            }
                        } else if state.slash_completions.is_empty() {
                            input.move_cursor_down();
                        }
                    } else if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        if input.is_empty()
                            && state.slash_completions.is_empty()
                            && state.file_completions.is_empty()
                            && state.input_owner.is_none()
                        {
                            let page = state.chat_view_height.max(1) as i32;
                            let delta = if key.code == KeyCode::PageUp {
                                -page
                            } else {
                                page
                            };
                            state.scroll_focus_lines(delta);
                        }
                    } else if key.code == KeyCode::Backspace {
                        if key.modifiers.contains(KeyModifiers::ALT) {
                            // Alt+Backspace — 删词
                            input.delete_word_backward();
                        } else {
                            input.backspace();
                        }
                        update_slash_completions(&mut state, &input, &cmd_registry);
                        update_file_completions(&mut state, &input, &cwd);
                    } else if key.code == KeyCode::Delete {
                        input.delete_char_forward();
                        update_slash_completions(&mut state, &input, &cmd_registry);
                        update_file_completions(&mut state, &input, &cwd);
                    } else if key.code == KeyCode::Home {
                        if input.is_empty()
                            && state.slash_completions.is_empty()
                            && state.file_completions.is_empty()
                            && state.input_owner.is_none()
                        {
                            state.select_conversation_start();
                        } else {
                            input.move_to_start_of_line();
                        }
                    } else if key.code == KeyCode::End {
                        if input.is_empty()
                            && state.slash_completions.is_empty()
                            && state.file_completions.is_empty()
                            && state.input_owner.is_none()
                        {
                            state.select_conversation_end();
                        } else {
                            input.move_to_end_of_line();
                        }
                    } else if key.code == KeyCode::Left {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            || key.modifiers.contains(KeyModifiers::ALT)
                        {
                            input.move_word_backward();
                        } else {
                            input.move_left();
                        }
                    } else if key.code == KeyCode::Right {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            || key.modifiers.contains(KeyModifiers::ALT)
                        {
                            input.move_word_forward();
                        } else {
                            input.move_right();
                        }
                    } else if let KeyCode::Char(c) = key.code {
                        // Ctrl 组合键
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                'a' => input.move_to_start_of_line(),
                                'e' => input.move_to_end_of_line(),
                                'k' => {
                                    input.kill_to_end_of_line();
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                    update_file_completions(&mut state, &input, &cwd);
                                }
                                'u' => {
                                    input.kill_to_start_of_line();
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                    update_file_completions(&mut state, &input, &cwd);
                                }
                                'w' => {
                                    input.delete_word_backward();
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                    update_file_completions(&mut state, &input, &cwd);
                                }
                                'l' => {
                                    // Ctrl+L：对齐大多数 shell/终端的"清屏重绘"语义。
                                    let _ = terminal.clear();
                                }
                                'o' => {
                                    state.toggle_selected_message();
                                }
                                't' => {
                                    // Ctrl+T — 进入任务列表焦点，并保留原折叠/展开语义
                                    if let Some(items) = state.current_todos.as_deref() {
                                        let collapsible = is_todo_collapsible(items);
                                        if collapsible && state.ui_focus == UiFocus::Todos {
                                            state.todo_panel_expanded = !state.todo_panel_expanded;
                                        } else {
                                            state.todo_panel_expanded = true;
                                        }
                                        state.ensure_selected_todo();
                                        if state.selected_todo_id.is_some() {
                                            state.ui_focus = UiFocus::Todos;
                                            state.chat_follow_tail = true;
                                            state.unseen_messages = false;
                                        }
                                    }
                                }
                                'y' => {
                                    // Ctrl+Y — 复制最后一条 AI 回复到系统剪贴板
                                    if let Some(text) = state
                                        .messages
                                        .iter()
                                        .rev()
                                        .find(|m| {
                                            matches!(m.role, MessageRole::Assistant) && !m.is_error
                                        })
                                        .map(|m| m.content.clone())
                                    {
                                        match arboard::Clipboard::new() {
                                            Ok(mut cb) => match cb.set_text(text) {
                                                Ok(()) => {
                                                    state.messages.push(ChatMessage::system(
                                                        "已复制最后一条 AI 回复到剪贴板"
                                                            .to_string(),
                                                    ));
                                                }
                                                Err(e) => {
                                                    state.messages.push(ChatMessage::system(
                                                        format!("复制失败: {e}"),
                                                    ));
                                                }
                                            },
                                            Err(e) => {
                                                state.messages.push(ChatMessage::system(format!(
                                                    "剪贴板访问失败: {e}"
                                                )));
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            state.history_idx = None;
                            state.history_draft = None;
                            input.insert_char(c);
                            update_slash_completions(&mut state, &input, &cmd_registry);
                            update_file_completions(&mut state, &input, &cwd);
                        }
                    }
                }
                _ => {}
            }
        }

        if state.should_quit {
            break;
        }
    }

    // 退出前中断所有仍在运行的子 Agent（含后台任务，结果随进程退出丢弃）
    hub.abort_all();
    // 杀掉全部后台 Bash 任务的进程组，防止孤儿进程
    wyj_tools::BashSessionManager::global().kill_all();

    // 退出时保存会话历史元数据
    if let Some(hs) = history_store {
        let _ = hs.append(&HistoryEntry {
            timestamp: now_iso(),
            session_id: current_session_id.clone(),
            input_tokens: state.total_input_tokens,
            output_tokens: state.total_output_tokens,
            turns: state.turns,
            cwd: cwd.display().to_string(),
        });
    }

    // 退出时保存完整会话文件
    let mut resumable_session_id = None;
    if let Some(store) = &session_store {
        if let Ok(sess) = session.try_lock() {
            if !sess.messages.is_empty() {
                let (title, title_generated) = match store.load(&current_session_id).ok() {
                    Some(f) if f.title_generated => (f.title, true),
                    _ => (extract_title(&sess.messages), false),
                };
                let _ = store.save(&SessionFile {
                    session_id: current_session_id.clone(),
                    title,
                    last_preview: extract_preview(&sess.messages),
                    cwd: cwd.display().to_string(),
                    timestamp: now_iso(),
                    turns: state.turns,
                    input_tokens: sess.total_input_tokens,
                    output_tokens: sess.total_output_tokens,
                    messages: sess.messages.clone(),
                    title_generated,
                });
                resumable_session_id = Some(current_session_id.clone());
            }
        }
    }

    // 恢复终端窗口标题
    let _ = write!(io::stdout(), "\x1b]0;\x07");
    let _ = io::stdout().flush();

    Ok(resumable_session_id)
}

/// 将 API Message 列表重建为 TUI 显示用的 ChatMessage 列表
fn reconstruct_display(messages: &[Message]) -> Vec<ChatMessage> {
    let mut result = Vec::new();
    let mut tool_seq = 0usize;
    // tool_use_id → 分配给该 ToolCall 的 seq，供同一助手回合内并行工具调用各自
    // 找到自己的 ToolResult（不能像以前那样直接复用循环里"当前"的 tool_seq——
    // 一个回合有多个并行调用时，所有 ToolResult 会被错误地全部对应到最后一个
    // 调用的 seq，导致其余调用在 compute_freezable_up_to 眼里"永远没有结果"，
    // 冻结进度卡死在那里，--resume/-c 恢复大会话时表现为界面长时间空白）。
    let mut seq_by_tool_use_id: HashMap<String, usize> = HashMap::new();

    for msg in messages {
        match &msg.role {
            Role::User => {
                let mut has_text = false;
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            result.push(ChatMessage::user(text.clone()));
                            has_text = true;
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let text = match content {
                                ToolResultContent::Text(s) => s.clone(),
                                ToolResultContent::Parts(_) => content.display_text(),
                                ToolResultContent::Blocks(v) => {
                                    serde_json::to_string_pretty(v).unwrap_or_default()
                                }
                            };
                            let summary = text
                                .lines()
                                .next()
                                .map(|l| l.trim().to_string())
                                .unwrap_or_default();
                            let seq = seq_by_tool_use_id.get(tool_use_id).copied().unwrap_or(0);
                            result.push(ChatMessage::tool_result(
                                text,
                                *is_error,
                                0.0,
                                seq,
                                String::new(),
                                summary,
                                true,
                            ));
                        }
                        _ => {}
                    }
                }
                let _ = has_text;
            }
            Role::Assistant => {
                let mut text_buf = String::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_buf.push_str(text),
                        ContentBlock::ToolUse { id, name, .. } => {
                            if !text_buf.trim().is_empty() {
                                result.push(ChatMessage::assistant(std::mem::take(&mut text_buf)));
                            } else {
                                text_buf.clear();
                            }
                            tool_seq += 1;
                            seq_by_tool_use_id.insert(id.clone(), tool_seq);
                            result.push(ChatMessage::tool_call(name.clone(), tool_seq));
                        }
                        _ => {}
                    }
                }
                if !text_buf.trim().is_empty() {
                    result.push(ChatMessage::assistant(text_buf));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod reconstruct_display_tests {
    use super::*;

    /// 回归测试：一个助手回合里有多个并行工具调用时，`reconstruct_display`
    /// 曾经用循环里"当前"的 tool_seq 给所有 ToolResult 赋值——多个并行调用的
    /// 结果会全部错误地对应到最后一个调用的 seq，导致除最后一个以外的
    /// ToolCall 永远找不到自己的 ToolResult。这在 compute_freezable_up_to
    /// 眼里等价于"工具调用永远未落定"，会让冻结进度永久卡在那里，
    /// --resume/--continue 恢复带并行工具调用的历史会话时表现为界面卡住。
    /// 现在改用 tool_use_id 精确配对，每个调用都应该找到自己的结果。
    #[test]
    fn parallel_tool_calls_get_distinct_matching_sequence_numbers() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "call-a".to_string(),
                        name: "Read".to_string(),
                        input: serde_json::json!({"file_path": "a.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "call-b".to_string(),
                        name: "Read".to_string(),
                        input: serde_json::json!({"file_path": "b.rs"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call-a".to_string(),
                        content: ToolResultContent::Text("content-a".to_string()),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call-b".to_string(),
                        content: ToolResultContent::Text("content-b".to_string()),
                        is_error: false,
                    },
                ],
            },
        ];

        let display = reconstruct_display(&messages);
        let roles: Vec<_> = display.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::ToolCall,
                MessageRole::ToolCall,
                MessageRole::ToolResult,
                MessageRole::ToolResult,
            ]
        );

        // 每条 ToolResult 的 sequence_no 必须匹配它自己 ToolCall 的 sequence_no，
        // 而不是两条结果都指向同一个（比如最后一个）调用的 seq。
        let call_a_seq = display[0].sequence_no;
        let call_b_seq = display[1].sequence_no;
        assert_ne!(call_a_seq, call_b_seq, "两个并行调用必须分配不同的 seq");
        assert_eq!(display[2].sequence_no, call_a_seq);
        assert_eq!(display[2].content, "content-a");
        assert_eq!(display[3].sequence_no, call_b_seq);
        assert_eq!(display[3].content, "content-b");

        // 用真正的冻结判定复演一遍：修复前这里会永久卡在下标 0（call-a 找不到
        // 自己的结果），修复后应该能一路冻结到底。
        let sub_agents = std::collections::BTreeMap::new();
        let bound = compute_freezable_up_to(
            &display,
            0,
            &sub_agents,
            render::last_collapsible_tool_result_idx(&display),
        );
        assert_eq!(bound, display.len());
    }
}

#[cfg(test)]
mod sub_agent_ui_tests {
    use super::*;
    use wyj_tools::{SubAgentEvent, SubAgentHub};

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            Config::default(),
            Arc::new(SubAgentHub::new()),
        )
    }

    fn tool_start(state: &mut AppState, id: &str) {
        state.apply_agent_event(AgentEvent::ToolStart {
            id: id.to_string(),
            name: "Agent".to_string(),
            input_json: serde_json::json!({"prompt": "x", "description": "d"}),
        });
    }

    fn started(state: &mut AppState, id: u64, desc: &str, background: bool) {
        state.apply_agent_event(AgentEvent::SubAgent(SubAgentEvent::Started {
            id,
            agent_type: "Explore".to_string(),
            description: desc.to_string(),
            background,
            parent_tool_use_id: None,
        }));
    }

    #[test]
    fn format_hms_buckets() {
        assert_eq!(format_hms(0.0), "0.0s");
        assert_eq!(format_hms(0.3), "0.3s");
        assert_eq!(format_hms(9.9), "9.9s");
        assert_eq!(format_hms(10.0), "10s");
        assert_eq!(format_hms(59.9), "60s");
        assert_eq!(format_hms(60.0), "1m 0s");
        assert_eq!(format_hms(65.0), "1m 5s");
        assert_eq!(format_hms(3_661.0), "1h 1m 1s");
        assert_eq!(format_hms(3_730.0), "1h 2m 10s");
    }

    #[test]
    fn started_binds_agent_tool_calls_fifo() {
        let mut state = make_state();
        tool_start(&mut state, "call-1");
        tool_start(&mut state, "call-2");
        started(&mut state, 1, "第一个任务", false);
        started(&mut state, 2, "第二个任务", false);

        let bound: Vec<(Option<u64>, String)> = state
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::ToolCall))
            .map(|m| (m.sub_agent_id, m.content.clone()))
            .collect();
        assert_eq!(bound.len(), 2);
        // FIFO：先出现的 ToolCall 绑定先 Started 的 agent，内容改写为 类型(描述)
        assert_eq!(bound[0], (Some(1), "Explore(第一个任务)".to_string()));
        assert_eq!(bound[1], (Some(2), "Explore(第二个任务)".to_string()));
    }

    #[test]
    fn bg_done_without_injector_stashes_reminder() {
        let mut state = make_state();
        tool_start(&mut state, "call-1");
        started(&mut state, 1, "后台任务", true);
        assert!(state.injector.is_none());
        state.apply_agent_event(AgentEvent::SubAgent(SubAgentEvent::Done {
            id: 1,
            agent_type: "Explore".to_string(),
            description: "后台任务".to_string(),
            result: "调查结论".to_string(),
            is_error: false,
            elapsed_secs: 1.2,
            background: true,
        }));
        assert_eq!(state.pending_bg_reminders.len(), 1);
        assert!(state.pending_bg_reminders[0].contains("调查结论"));
        assert_eq!(
            state.sub_agents.get(&1).unwrap().status,
            SubAgentStatus::Done
        );
    }

    #[test]
    fn tool_events_update_ui_state_and_log() {
        let mut state = make_state();
        tool_start(&mut state, "call-1");
        started(&mut state, 1, "任务", false);
        state.apply_agent_event(AgentEvent::SubAgent(SubAgentEvent::ToolStart {
            id: 1,
            tool_name: "Grep".to_string(),
            arg_summary: "foo".to_string(),
            input: serde_json::json!({"pattern": "foo"}),
        }));
        {
            let s = state.sub_agents.get(&1).unwrap();
            assert_eq!(s.tool_calls, 1);
            assert_eq!(s.current_tool.as_deref(), Some("Grep(foo)"));
        }
        state.apply_agent_event(AgentEvent::SubAgent(SubAgentEvent::ToolEnd {
            id: 1,
            tool_name: "Grep".to_string(),
            is_error: false,
            elapsed_secs: 0.3,
            output: "1 match".to_string(),
        }));
        let s = state.sub_agents.get(&1).unwrap();
        assert!(s.current_tool.is_none());
        assert_eq!(s.tool_log.len(), 1);
        assert_eq!(s.tool_log[0].elapsed_secs, Some(0.3));
        let trace = state.sub_agent_trace_cache.get(&1).unwrap();
        assert!(matches!(trace[0], TraceEvent::Started { .. }));
        assert!(matches!(trace[1], TraceEvent::ToolStart { .. }));
        assert!(matches!(
            trace[2],
            TraceEvent::ToolEnd {
                elapsed_secs: 0.3,
                ..
            }
        ));
    }

    #[test]
    fn arrows_do_not_steal_focus_when_timeline_has_messages() {
        let mut state = make_state();
        tool_start(&mut state, "call-1");
        started(&mut state, 1, "任务", false);

        assert_eq!(state.ui_focus, UiFocus::Chat);
        assert!(!state.should_enter_sub_agent_focus_from_arrows());

        state.move_selected_sub_agent(1);

        assert_eq!(state.ui_focus, UiFocus::SubAgents);
        assert_eq!(state.selected_sub_agent, Some(1));
    }
}

#[cfg(test)]
mod cross_session_subagents_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use wyj_tools::trace::{trace_dir, TraceEvent};
    use wyj_tools::SubAgentHub;

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            Config::default(),
            Arc::new(SubAgentHub::new()),
        )
    }

    fn unique_tmp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("wyj-tui-subagents-test-{label}-{nanos}-{n}"))
    }

    fn write_trace(
        sessions_dir: &std::path::Path,
        session_id: &str,
        id: u64,
        events: &[TraceEvent],
    ) {
        let dir = trace_dir(sessions_dir, session_id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut content = String::new();
        for ev in events {
            content.push_str(&serde_json::to_string(ev).unwrap());
            content.push('\n');
        }
        std::fs::write(dir.join(format!("a{id}.jsonl")), content).unwrap();
    }

    #[test]
    fn reload_reconstructs_completed_agent_with_tool_log() {
        let tmp = unique_tmp_dir("basic");
        let session_id = "sess-reload";
        write_trace(
            &tmp,
            session_id,
            1,
            &[
                TraceEvent::Started {
                    agent_type: "general-purpose".into(),
                    description: "找 bug".into(),
                    background: false,
                    parent_tool_use_id: Some("toolu_1".into()),
                },
                TraceEvent::ToolStart {
                    tool_name: "Bash".into(),
                    input_json: "{\"command\":\"echo hi\"}".into(),
                    truncated: false,
                },
                TraceEvent::ToolEnd {
                    tool_name: "Bash".into(),
                    is_error: false,
                    elapsed_secs: 0.2,
                    output: "hi\n".into(),
                    truncated: false,
                },
                TraceEvent::Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                },
                TraceEvent::Done {
                    result: "done".into(),
                    is_error: false,
                    elapsed_secs: 3.5,
                },
            ],
        );

        let reloaded = reload_persisted_sub_agents(&tmp, session_id);
        let s = reloaded.get(&1).expect("id 1 should be reconstructed");
        assert_eq!(s.agent_type, "general-purpose");
        assert_eq!(s.status, SubAgentStatus::Done);
        assert_eq!(s.final_result.as_deref(), Some("done"));
        assert_eq!(s.input_tokens, 100);
        assert_eq!(s.output_tokens, 20);
        assert_eq!(s.tool_log.len(), 1);
        assert_eq!(s.tool_log[0].tool_name, "Bash");
        assert_eq!(s.tool_log[0].elapsed_secs, Some(0.2));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sub_agent_trace_events_reads_persisted_trace() {
        let tmp = unique_tmp_dir("cache");
        let session_id = "sess-cache";
        write_trace(
            &tmp,
            session_id,
            7,
            &[
                TraceEvent::Started {
                    agent_type: "Explore".into(),
                    description: "trace me".into(),
                    background: false,
                    parent_tool_use_id: None,
                },
                TraceEvent::Done {
                    result: "final".into(),
                    is_error: false,
                    elapsed_secs: 1.0,
                },
            ],
        );

        let mut state = make_state();
        state.sessions_dir = Some(tmp.clone());
        state.current_session_id = session_id.to_string();
        let events = state.sub_agent_trace_events(7).unwrap();
        assert_eq!(events.len(), 2);
        assert!(state.sub_agent_trace_cache.contains_key(&7));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reload_marks_missing_done_event_as_interrupted() {
        let tmp = unique_tmp_dir("crashed");
        let session_id = "sess-crashed";
        write_trace(
            &tmp,
            session_id,
            1,
            &[TraceEvent::Started {
                agent_type: "general-purpose".into(),
                description: "被强杀".into(),
                background: false,
                parent_tool_use_id: None,
            }],
        );

        let reloaded = reload_persisted_sub_agents(&tmp, session_id);
        let s = reloaded.get(&1).unwrap();
        assert_eq!(s.status, SubAgentStatus::Interrupted);
        assert!(s.final_result.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reload_on_nonexistent_dir_returns_empty() {
        let tmp = unique_tmp_dir("missing");
        let reloaded = reload_persisted_sub_agents(&tmp, "sess-none");
        assert!(reloaded.is_empty());
    }

    #[test]
    fn open_panel_without_id_selects_most_recent_and_opens_detail() {
        let mut state = make_state();
        state.sub_agents.insert(
            1,
            SubAgentUiState {
                agent_type: "t".into(),
                description: "d".into(),
                background: false,
                status: SubAgentStatus::Done,
                started_at: Instant::now(),
                final_elapsed: Some(1.0),
                input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                current_tool: None,
                tool_log: vec![],
                has_result: true,
                finished_at: Some(Instant::now()),
                final_result: Some("r1".into()),
            },
        );
        state.sub_agents.insert(
            2,
            SubAgentUiState {
                agent_type: "t".into(),
                description: "d2".into(),
                background: false,
                status: SubAgentStatus::Done,
                started_at: Instant::now(),
                final_elapsed: Some(1.0),
                input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                current_tool: None,
                tool_log: vec![],
                has_result: true,
                finished_at: Some(Instant::now()),
                final_result: Some("r2".into()),
            },
        );

        apply_open_subagents_panel(&mut state, None);
        assert_eq!(state.selected_sub_agent, Some(2));
        assert!(state.sub_agent_detail_open);
    }

    #[test]
    fn open_panel_with_missing_id_reports_error_without_panic() {
        let mut state = make_state();
        state.sub_agents.insert(
            1,
            SubAgentUiState {
                agent_type: "t".into(),
                description: "d".into(),
                background: false,
                status: SubAgentStatus::Done,
                started_at: Instant::now(),
                final_elapsed: Some(1.0),
                input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                current_tool: None,
                tool_log: vec![],
                has_result: true,
                finished_at: Some(Instant::now()),
                final_result: Some("r1".into()),
            },
        );
        apply_open_subagents_panel(&mut state, Some(99));
        assert!(state
            .messages
            .iter()
            .any(|m| m.content.contains("99") || m.content.contains("a99")));
        // 请求不存在的 id 不应误改当前选中项
        assert_eq!(state.selected_sub_agent, None);
    }

    #[test]
    fn open_panel_on_empty_session_reports_empty_message() {
        let mut state = make_state();
        apply_open_subagents_panel(&mut state, None);
        assert_eq!(state.selected_sub_agent, None);
        assert!(!state.messages.is_empty());
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
            AgentMode::Normal,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        )
    }

    /// 面板打开后拦截全部按键，用户不再有滚动手段：打开时必须强制回到贴底
    /// 跟随并清掉选中锚点，否则此前上滚/选中留下的视口位置会把附加在聊天区
    /// 尾部的选项区裁出可视范围且无法找回（遮挡回归，见 AskQuestions 事件处理）。
    #[test]
    fn ask_questions_event_forces_follow_tail_and_clears_selection() {
        let mut state = make_state();
        state.chat_follow_tail = false;
        state.unseen_messages = true;
        state.selected_message_id = Some(7);
        state.selected_message_anchor = Some(ChatSelectionAnchor::Top);

        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.apply_agent_event(AgentEvent::AskQuestions {
            questions: vec![single_spec("Q1")],
            response_tx: tx,
        });

        assert!(state.ask_question_dialog.is_some());
        assert!(state.chat_follow_tail);
        assert!(!state.unseen_messages);
        assert_eq!(state.selected_message_id, None);
        assert!(state.selected_message_anchor.is_none());
    }

    fn push_call_result_pair(state: &mut AppState) {
        state.push_message(ChatMessage::tool_call("Read(x)".to_string(), 1));
        state.push_message(ChatMessage::tool_result(
            (1..=10)
                .map(|i| format!("line-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            false,
            0.1,
            1,
            "Read".to_string(),
            "line-1".to_string(),
            false,
        ));
    }

    /// AskQuestion 面板打开期间豁免"最后可折叠 ToolResult"对冻结边界的封顶，
    /// 让面板之前的长正文得以冻结进终端 scrollback（规则①仍会把边界卡在
    /// 未完成的 AskQuestion ToolCall 上）；面板关闭后封顶恢复。
    #[test]
    fn freeze_collapsible_bound_exempted_while_dialog_open() {
        let mut state = make_state();
        state.welcome_frozen = true;
        push_call_result_pair(&mut state);
        state.push_message(ChatMessage::assistant("提问前的短正文".to_string()));

        // 120x60 → 聊天区上限 42 行，十几行内容显示得下，封顶保留
        assert_eq!(freeze_collapsible_bound(&mut state, 120, 60), Some(1));

        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.ask_question_dialog = Some(AskQuestionDialog::new(vec![single_spec("Q1")], tx));
        assert_eq!(freeze_collapsible_bound(&mut state, 120, 60), None);

        state.ask_question_dialog = None;
        assert_eq!(freeze_collapsible_bound(&mut state, 120, 60), Some(1));
    }

    /// 待定尾部实际渲染高度超过聊天区可视上限时豁免封顶（可见性优先）：
    /// 长 markdown 正文不再被"最后可折叠 ToolResult"困在 Inline viewport
    /// 里裁掉——冻结进终端 scrollback 后可用鼠标滚轮回看。
    #[test]
    fn freeze_collapsible_bound_exempted_when_pending_tail_overflows() {
        let mut state = make_state();
        state.welcome_frozen = true;
        push_call_result_pair(&mut state);
        let long_md = (1..=40)
            .map(|i| format!("第 {i} 节：很长的分析正文"))
            .collect::<Vec<_>>()
            .join("\n\n");
        state.push_message(ChatMessage::assistant(long_md));

        // 120x24 → 上限 16 行，约 90 行内容注定显示不全 → 豁免封顶
        assert_eq!(freeze_collapsible_bound(&mut state, 120, 24), None);
        // 120x300 → 上限 210 行，显示得下 → 封顶保留（Ctrl+O 交互不牺牲）
        assert_eq!(freeze_collapsible_bound(&mut state, 120, 300), Some(1));
    }
}

#[cfg(test)]
mod plan_approval_dialog_tests {
    use super::*;

    fn make_dialog() -> (PlanApprovalDialog, tokio::sync::oneshot::Receiver<bool>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (PlanApprovalDialog::new(tx), rx)
    }

    #[test]
    fn starts_with_approve_highlighted() {
        let (dlg, _rx) = make_dialog();
        assert_eq!(dlg.cursor(), 0);
    }

    #[test]
    fn down_moves_through_all_three_options_and_clamps() {
        let (mut dlg, _rx) = make_dialog();
        assert!(matches!(
            dlg.handle_key(KeyCode::Down),
            PlanApprovalOutcome::Continue
        ));
        assert_eq!(dlg.cursor(), 1);
        dlg.handle_key(KeyCode::Down);
        assert_eq!(dlg.cursor(), 2);
        // 已经在最后一项，继续按 Down 不应越界
        dlg.handle_key(KeyCode::Down);
        assert_eq!(dlg.cursor(), 2);
        dlg.handle_key(KeyCode::Up);
        assert_eq!(dlg.cursor(), 1);
    }

    #[test]
    fn enter_on_first_option_approves() {
        let (mut dlg, _rx) = make_dialog();
        assert!(matches!(
            dlg.handle_key(KeyCode::Enter),
            PlanApprovalOutcome::Approve
        ));
    }

    #[test]
    fn enter_on_second_option_rejects() {
        let (mut dlg, _rx) = make_dialog();
        dlg.handle_key(KeyCode::Down);
        assert!(matches!(
            dlg.handle_key(KeyCode::Enter),
            PlanApprovalOutcome::Reject
        ));
    }

    #[test]
    fn enter_on_third_option_opens_freetext_then_submits() {
        let (mut dlg, _rx) = make_dialog();
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key(KeyCode::Down);
        assert_eq!(dlg.cursor(), 2);
        // 第一次 Enter：展开自由文本输入框，尚未提交
        assert!(matches!(
            dlg.handle_key(KeyCode::Enter),
            PlanApprovalOutcome::Continue
        ));
        assert!(dlg.freetext_input().is_some());
        for c in "多加个错误处理".chars() {
            dlg.handle_key(KeyCode::Char(c));
        }
        // 第二次 Enter：提交反馈文本
        let PlanApprovalOutcome::Feedback(text) = dlg.handle_key(KeyCode::Enter) else {
            panic!("expected Feedback outcome");
        };
        assert_eq!(text, "多加个错误处理");
    }

    #[test]
    fn empty_freetext_submit_is_ignored() {
        let (mut dlg, _rx) = make_dialog();
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key(KeyCode::Enter); // 进入自由文本子模式
        assert!(matches!(
            dlg.handle_key(KeyCode::Enter),
            PlanApprovalOutcome::Continue
        ));
    }

    #[test]
    fn esc_in_freetext_returns_to_selector_without_rejecting() {
        let (mut dlg, _rx) = make_dialog();
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key(KeyCode::Down);
        dlg.handle_key(KeyCode::Enter); // 进入自由文本子模式
        assert!(dlg.freetext_input().is_some());
        dlg.handle_key(KeyCode::Esc);
        assert!(dlg.freetext_input().is_none());
        assert_eq!(dlg.cursor(), 2);
    }

    #[tokio::test]
    async fn approve_sends_true_via_response_tx() {
        let (mut dlg, rx) = make_dialog();
        let outcome = dlg.handle_key(KeyCode::Enter);
        assert!(matches!(outcome, PlanApprovalOutcome::Approve));
        let _ = dlg.response_tx.send(true);
        assert!(rx.await.unwrap());
    }
}

#[cfg(test)]
mod todo_stats_tests {
    use super::*;
    use wyj_tools::todo::{TodoItem, TodoStatus};
    use wyj_tools::SubAgentHub;

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            Config::default(),
            Arc::new(SubAgentHub::new()),
        )
    }

    fn todo(id: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: id.to_string(),
            content: format!("task-{id}"),
            status,
            priority: None,
            active_form: None,
        }
    }

    #[test]
    fn split_evenly_sum_invariant_with_remainder() {
        let shares = split_evenly(10, 3);
        assert_eq!(shares.iter().sum::<u32>(), 10);
        assert_eq!(shares, vec![4, 3, 3]);
    }

    #[test]
    fn split_evenly_n_one_gets_all() {
        assert_eq!(split_evenly(7, 1), vec![7]);
    }

    #[test]
    fn split_evenly_zero_total() {
        assert_eq!(split_evenly(0, 3), vec![0, 0, 0]);
    }

    #[test]
    fn todo_update_then_usage_delta_attributes_to_in_progress_task() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![
            todo("a", TodoStatus::InProgress),
            todo("b", TodoStatus::Pending),
        ]));
        state.apply_agent_event(AgentEvent::UsageDelta {
            input_tokens: 100,
            output_tokens: 50,
        });
        let s = state.todo_stats.get("a").unwrap();
        assert_eq!(s.input_tokens, 100);
        assert_eq!(s.output_tokens, 50);
        assert!(!state.todo_stats.contains_key("b"));
    }

    #[test]
    fn usage_delta_splits_evenly_across_multiple_in_progress_tasks() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![
            todo("a", TodoStatus::InProgress),
            todo("b", TodoStatus::InProgress),
        ]));
        state.apply_agent_event(AgentEvent::UsageDelta {
            input_tokens: 5,
            output_tokens: 5,
        });
        let a = state.todo_stats.get("a").unwrap();
        let b = state.todo_stats.get("b").unwrap();
        assert_eq!(a.input_tokens + b.input_tokens, 5);
        assert_eq!(a.output_tokens + b.output_tokens, 5);
    }

    #[test]
    fn usage_delta_with_no_in_progress_task_is_dropped() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![todo("a", TodoStatus::Pending)]));
        state.apply_agent_event(AgentEvent::UsageDelta {
            input_tokens: 100,
            output_tokens: 50,
        });
        assert!(state.todo_stats.is_empty());
    }

    #[test]
    fn elapsed_freezes_when_task_leaves_in_progress() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![todo(
            "a",
            TodoStatus::InProgress,
        )]));
        std::thread::sleep(std::time::Duration::from_millis(10));
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![todo(
            "a",
            TodoStatus::Completed,
        )]));
        let frozen = state.todo_stats.get("a").unwrap().elapsed_secs();
        assert!(frozen > 0.0);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let still = state.todo_stats.get("a").unwrap().elapsed_secs();
        assert_eq!(frozen, still);
    }

    #[test]
    fn new_round_clears_todo_stats() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![todo(
            "a",
            TodoStatus::InProgress,
        )]));
        state.apply_agent_event(AgentEvent::UsageDelta {
            input_tokens: 10,
            output_tokens: 10,
        });
        assert!(!state.todo_stats.is_empty());

        // 全新一轮：id/content 都不同
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![todo(
            "x1",
            TodoStatus::Pending,
        )]));
        assert!(state.todo_stats.is_empty());
        assert!(state.todo_execution_logs.is_empty());
    }

    #[test]
    fn active_todo_captures_tool_execution_messages() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![todo(
            "a",
            TodoStatus::InProgress,
        )]));
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "a.rs"}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: "content".to_string(),
            is_error: false,
            elapsed_secs: 0.1,
        });

        let log = state.todo_execution_logs.get("a").unwrap();
        assert_eq!(log.len(), 2);
        let message_ids: Vec<u64> = log
            .iter()
            .filter_map(|entry| match entry {
                TodoExecutionEntry::Message(id) => Some(*id),
                TodoExecutionEntry::Note(_) => None,
            })
            .collect();
        assert!(message_ids.iter().any(|id| state
            .messages
            .iter()
            .any(|m| m.id == *id && matches!(m.role, MessageRole::ToolCall))));
        assert!(message_ids.iter().any(|id| state
            .messages
            .iter()
            .any(|m| m.id == *id && matches!(m.role, MessageRole::ToolResult))));
    }
}

#[cfg(test)]
mod tool_event_ordering_tests {
    use super::*;

    fn make_state() -> AppState {
        AppState::new(
            PathBuf::from("/tmp"),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        )
    }

    /// 同一轮内并发执行的多个工具调用，ToolEnd 到达顺序可能与 ToolStart 不同，
    /// 但每条 ToolResult 都必须紧跟在自己的 ToolCall 后面，而不是全部堆到列表尾部。
    #[test]
    fn tool_end_out_of_order_still_pairs_with_its_own_call() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "a.rs"}),
        });
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-2".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "b.rs"}),
        });
        // call-2 先完成（乱序），call-1 后完成
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-2".to_string(),
            output: "content-b".to_string(),
            is_error: false,
            elapsed_secs: 0.1,
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: "content-a".to_string(),
            is_error: false,
            elapsed_secs: 0.2,
        });

        let roles: Vec<_> = state.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::ToolCall,
                MessageRole::ToolResult,
                MessageRole::ToolCall,
                MessageRole::ToolResult,
            ]
        );
        // call-1 的结果紧跟 call-1 自己的调用（即使 call-2 先完成），
        // call-2 的结果紧跟 call-2 自己的调用。
        assert_eq!(state.messages[1].content, "content-a");
        assert_eq!(state.messages[3].content, "content-b");
    }

    /// 没有任何工具调用时，纯文本消息应该可以整体冻结（推进到列表末尾）。
    #[test]
    fn freezable_up_to_advances_past_plain_messages() {
        let mut state = make_state();
        state.messages.push(ChatMessage::user("hi".to_string()));
        state
            .messages
            .push(ChatMessage::assistant("hello".to_string()));
        let bound = compute_freezable_up_to(
            &state.messages,
            0,
            &state.sub_agents,
            render::last_collapsible_tool_result_idx(&state.messages),
        );
        assert_eq!(bound, 2);
    }

    /// 并发批次里只要还有一个 ToolCall 没等到自己的 ToolResult，冻结边界必须
    /// 停在它这里，不能越过——否则乱序到达的 ToolResult 就没法再插到正确位置。
    #[test]
    fn freezable_up_to_stops_before_unresolved_tool_call() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "a.rs"}),
        });
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-2".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "b.rs"}),
        });
        // 只有 call-1 落定，call-2 仍未完成
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: "content-a".to_string(),
            is_error: false,
            elapsed_secs: 0.1,
        });
        // messages = [ToolCall(call-1), ToolResult(call-1), ToolCall(call-2)]
        // call-2 在下标 2，尚未落定，冻结边界必须停在 2。
        let bound = compute_freezable_up_to(
            &state.messages,
            0,
            &state.sub_agents,
            render::last_collapsible_tool_result_idx(&state.messages),
        );
        assert_eq!(bound, 2);

        // call-2 落定后，整批都可以冻结了
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-2".to_string(),
            output: "content-b".to_string(),
            is_error: false,
            elapsed_secs: 0.1,
        });
        let bound = compute_freezable_up_to(
            &state.messages,
            0,
            &state.sub_agents,
            render::last_collapsible_tool_result_idx(&state.messages),
        );
        assert_eq!(bound, state.messages.len());
    }

    /// 关联的子 Agent 仍在 Running 状态时，其 ToolCall/ToolResult 不可冻结，
    /// 即使工具调用本身已经"完成"（结果已经插入列表）。
    #[test]
    fn freezable_up_to_blocks_on_running_sub_agent() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "Agent".to_string(),
            input_json: serde_json::json!({"subagent_type": "general-purpose", "prompt": "do x"}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: "started".to_string(),
            is_error: false,
            elapsed_secs: 0.0,
        });
        let call_idx = state
            .messages
            .iter()
            .position(|m| matches!(m.role, MessageRole::ToolCall))
            .unwrap();
        state.messages[call_idx].sub_agent_id = Some(1);
        state.sub_agents.insert(
            1,
            SubAgentUiState {
                agent_type: "general-purpose".to_string(),
                description: "do x".to_string(),
                background: false,
                status: SubAgentStatus::Running,
                started_at: std::time::Instant::now(),
                final_elapsed: None,
                input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                current_tool: None,
                tool_log: vec![],
                has_result: false,
                finished_at: None,
                final_result: None,
            },
        );

        let bound = compute_freezable_up_to(
            &state.messages,
            0,
            &state.sub_agents,
            render::last_collapsible_tool_result_idx(&state.messages),
        );
        assert_eq!(bound, call_idx);

        // 子 Agent 结束后恢复可冻结
        state.sub_agents.get_mut(&1).unwrap().status = SubAgentStatus::Done;
        let bound = compute_freezable_up_to(
            &state.messages,
            0,
            &state.sub_agents,
            render::last_collapsible_tool_result_idx(&state.messages),
        );
        assert_eq!(bound, state.messages.len());
    }

    fn long_output(label: &str) -> String {
        // > TOOL_RESULT_FOLD_LINES(5) 行，触发 is_collapsible_tool_result_content。
        (1..=7)
            .map(|i| format!("{label}-line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 多 Agent 并发下的核心回归：主循环顶部缓存的 last_collapsible_seq 一旦算出，
    /// 之后（同一轮循环内）drain 出的新 ToolResult 不能让 Ctrl+O 的翻转目标发生
    /// 漂移——用旧缓存值查找，命中的必须始终是缓存时刻的那一条，而不是新插入的。
    #[test]
    fn cached_collapsible_seq_is_immune_to_later_insertions_in_same_frame() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "a.rs"}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: long_output("first"),
            is_error: false,
            elapsed_secs: 0.1,
        });

        // 模拟主循环顶部：draw() 之前、drain 之前算好本轮缓存值。
        let last_idx = render::last_collapsible_tool_result_idx(&state.messages);
        let cached_seq = last_idx.and_then(|i| state.messages[i].sequence_no);
        let first_result_seq = state.messages[1].sequence_no;
        assert_eq!(cached_seq, first_result_seq);

        // 模拟同一轮循环内、draw 之后 drain agent_rx 插入了另一个并发子 Agent
        // 刚完成的、下标更靠后且同样可折叠的 ToolResult。
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-2".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "b.rs"}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-2".to_string(),
            output: long_output("second"),
            is_error: false,
            elapsed_secs: 0.1,
        });

        // 如果此刻重新扫描，"最后一条可折叠"已经变成新插入的那条——修复前的 bug
        // 正是在这里：Ctrl+O 处理独立重新扫描会命中它，而不是用户屏幕上看到的那条。
        let rescanned = render::last_collapsible_tool_result_idx(&state.messages);
        assert_ne!(rescanned, last_idx, "前置条件：确实发生了漂移");

        // 用本轮开始时缓存的旧值翻转，必须命中第一条，不是新插入的那条。
        toggle_last_collapsible(&mut state.messages, cached_seq);
        assert!(state.messages[1].expanded, "缓存目标（第一条）应被翻转");
        let second_result = state
            .messages
            .iter()
            .find(|m| m.sequence_no == state.messages[3].sequence_no)
            .unwrap();
        assert!(!second_result.expanded, "新插入的那条不应被误翻转");
    }

    /// `compute_freezable_up_to` 必须真正使用外部传入的 `collapsible_idx`，而不是
    /// 内部又独立扫描一遍——传入一个人为更早的下标，冻结边界必须被它限制住。
    #[test]
    fn compute_freezable_up_to_is_bounded_by_passed_in_collapsible_idx() {
        let mut state = make_state();
        state.messages.push(ChatMessage::user("hi".to_string()));
        state
            .messages
            .push(ChatMessage::assistant("hello".to_string()));
        state
            .messages
            .push(ChatMessage::assistant("world".to_string()));

        // 真实场景下这里没有任何可折叠 ToolResult，天花板本应是 messages.len()；
        // 故意传入一个更早的下标，验证返回值确实被它卡住，而不是无视它、按内部
        // 重新扫描的结果（那样会得到 messages.len()）。
        let bound = compute_freezable_up_to(&state.messages, 0, &state.sub_agents, Some(1));
        assert_eq!(bound, 1);
    }

    /// 防御性回归：`toggle_last_collapsible` 面对不存在的 `sequence_no`（理论上不
    /// 会发生，见 last_collapsible_seq 字段文档）必须是无操作，不 panic、不误翻转
    /// 任何消息。
    #[test]
    fn toggle_last_collapsible_no_op_when_seq_not_found() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "a.rs"}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: long_output("only"),
            is_error: false,
            elapsed_secs: 0.1,
        });
        let before: Vec<bool> = state.messages.iter().map(|m| m.expanded).collect();

        toggle_last_collapsible(&mut state.messages, Some(999_999));
        let after: Vec<bool> = state.messages.iter().map(|m| m.expanded).collect();
        assert_eq!(before, after);

        toggle_last_collapsible(&mut state.messages, None);
        let after_none: Vec<bool> = state.messages.iter().map(|m| m.expanded).collect();
        assert_eq!(before, after_none);
    }
}
