//! TUI 应用主循环

use crate::event::{is_quit, AgentEvent};
use crate::input::InputBox;
use crate::render;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEvent,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use wyj_api::types::{ContentBlock, Message, Role, ToolResultContent};
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer};
use wyj_core::{
    discover_files, extract_preview, extract_title, new_session_id, now_iso, Agent, DiscoveredFile,
    HistoryEntry, HistoryStore, InjectionKind, Session, SessionFile, SessionMeta, SessionStore,
    ToolEvent,
};
use wyj_tools::todo::{is_todo_collapsible, TodoItem, TodoStatus};
use wyj_tools::{ctx::UiAskRequest, PermissionMode};
use wyj_tools::{ExitPlanModeTool, TodoStore, ToolCtx};

/// 用于 /model 热切换 / 设置面板保存后重建 Agent 的函数类型
pub type RebuildFn = Arc<dyn Fn(&Config, &str) -> anyhow::Result<Agent> + Send + Sync>;

/// 消息角色
#[derive(Debug, Clone)]
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
}

/// 渲染用消息
#[derive(Debug, Clone)]
pub struct ChatMessage {
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
    /// 工具结果是否已展开（ToolResult 专用）
    pub expanded: bool,
    /// 绑定的子 Agent id（Agent 工具的 ToolCall/ToolResult 专用）
    pub sub_agent_id: Option<u64>,
}

impl ChatMessage {
    fn base(role: MessageRole, content: String) -> Self {
        Self {
            role,
            content,
            is_error: false,
            elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            expanded: false,
            sub_agent_id: None,
        }
    }

    fn user(content: String) -> Self {
        Self::base(MessageRole::User, content)
    }

    fn assistant(content: String) -> Self {
        Self::base(MessageRole::Assistant, content)
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
    ) -> Self {
        Self {
            role: MessageRole::ToolResult,
            content: output,
            is_error,
            elapsed_secs: Some(elapsed_secs),
            sequence_no: Some(seq),
            tool_name: Some(name),
            display_summary: summary,
            expanded: false,
            sub_agent_id: None,
        }
    }

    fn bash_output(output: String, exit_code: i32, elapsed_secs: f64) -> Self {
        Self {
            role: MessageRole::BashOutput,
            content: output,
            is_error: exit_code != 0,
            elapsed_secs: Some(elapsed_secs),
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
            expanded: false,
            sub_agent_id: None,
        }
    }

    pub fn system(content: String) -> Self {
        Self::base(MessageRole::System, content)
    }

    fn turn_summary(elapsed_secs: f64, d_input: u32, d_output: u32) -> Self {
        let content = format!(
            "⏱ {:.1}s · ↑{} ↓{}",
            elapsed_secs,
            fmt_tokens(d_input),
            fmt_tokens(d_output),
        );
        Self::base(MessageRole::TurnSummary, content)
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

pub(crate) fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000)
    } else if n >= 1000 {
        format!("{},{:03}", n / 1000, n % 1000)
    } else {
        n.to_string()
    }
}

/// 权限确认对话框状态
#[derive(Debug)]
pub struct PermissionDialog {
    pub tool_name: String,
    pub input_preview: String,
    pub tx_id: String,
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
    pub plan: String,
    /// 已向下滚动的行数（0 = 顶部）
    pub scroll: u16,
    pub response_tx: tokio::sync::oneshot::Sender<bool>,
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
#[derive(Clone)]
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
        }
    }
}

/// ProfileDialog 里当前展示的浮层
pub enum ProfileOverlay {
    None,
    /// 重命名 entries[entry_idx]
    Renaming {
        entry_idx: usize,
        input: InputBox,
    },
    /// 新建分组模板选择器
    TemplatePicker {
        selected: usize,
    },
    /// 删除二次确认
    ConfirmDelete {
        entry_idx: usize,
    },
    /// 拉取模型列表中（entry_idx 的 field_idx 字段）
    FetchingModels {
        entry_idx: usize,
        field_idx: usize,
    },
    /// 模型列表拉取成功，供选择
    ModelsPicker {
        entry_idx: usize,
        field_idx: usize,
        models: Vec<String>,
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
    /// Some 表示当前字段正在行内文本编辑
    pub editing: Option<InputBox>,
    pub overlay: ProfileOverlay,
    pub error: Option<String>,
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
        Self {
            entries,
            active_idx,
            cursor: 0,
            expanded: None,
            editing: None,
            overlay: ProfileOverlay::None,
            error: None,
        }
    }

    /// 扁平化行列表：(entry_idx, field_idx)，field_idx = None 表示是 entry 头行
    pub fn rows(&self) -> Vec<(usize, Option<usize>)> {
        let mut rows = Vec::new();
        for i in 0..self.entries.len() {
            rows.push((i, None));
            if self.expanded == Some(i) {
                for f in 0..PROFILE_FIELD_COUNT {
                    rows.push((i, Some(f)));
                }
            }
        }
        rows
    }

    /// 当前游标所在行对应的 (entry_idx, field_idx)
    fn selected_row(&self) -> (usize, Option<usize>) {
        self.rows().get(self.cursor).copied().unwrap_or((0, None))
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

    match name {
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
        "web_fetch" => {
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

    match name {
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
        "web_fetch" => format!("fetched {} bytes", output.len()),
        _ => trunc1(output.lines().next().unwrap_or(output)),
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

/// 全局 UI 状态
pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub streaming_buf: String,
    pub is_thinking: bool,
    pub permission_dialog: Option<PermissionDialog>,
    pub ask_question_dialog: Option<AskQuestionDialog>,
    /// ExitPlanMode 触发的计划批准对话框
    pub plan_dialog: Option<PlanApprovalDialog>,
    /// 检测到计划已批准仍在 plan 模式发消息时的确认对话框
    pub exec_mode_confirm: Option<ExecModeConfirmDialog>,
    pub scroll_offset: u16,
    /// 上次渲染的聊天区可见行高（用于按页滚动）
    pub chat_height: u16,
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
    /// 本 Session 已授权的工具（按 s 键授权）
    pub session_allowed: HashSet<String>,
    /// 当前任务列表快照（TodoWrite 更新），用于底部固定面板渲染
    pub current_todos: Option<Vec<TodoItem>>,
    /// 任务面板是否处于展开态（仅在 is_todo_collapsible 为真时生效）
    pub todo_panel_expanded: bool,
    /// 每条任务的运行时统计（耗时/token），按 TodoItem.id 索引
    pub todo_stats: HashMap<String, TodoRuntimeStats>,
    /// 会话选择器（/sessions 命令触发时 Some）
    pub session_picker: Option<SessionPickerState>,
    /// 设置面板（/config 命令触发时 Some）
    pub settings_dialog: Option<SettingsDialog>,
    /// 分组管理面板（/model 无参命令触发时 Some）
    pub profile_dialog: Option<ProfileDialog>,
    /// CLAUDE.md 记忆面板（/memory 命令触发时 Some）
    pub memory_dialog: Option<MemoryDialog>,
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
    /// 上次渲染的滚动条区域（保留字段供将来用）
    pub scrollbar_area: Rect,
    /// 当前运行中 Agent 任务的补充信息注入通道（is_thinking 期间提交的消息走这里）
    pub injector: Option<mpsc::UnboundedSender<(Vec<ContentBlock>, InjectionKind)>>,
    /// 排队中尚未被 Agent 消费的补充消息（文本 + 附件），用于状态栏提示计数、
    /// 消费后回放到对话流、以及轮次已结束但仍有残留时的兜底重发
    pub pending_queue: Vec<(String, Vec<Attachment>)>,
    /// 当前生效的完整配置（/config 设置面板的数据来源与保存目标）
    pub config: Config,
    /// 子 Agent 实时状态（key = Hub 分配的 id，BTreeMap 保证面板按启动顺序排列）
    pub sub_agents: std::collections::BTreeMap<u64, SubAgentUiState>,
    /// 后台子 Agent 完成时主 Agent 空闲，暂存的 system-reminder，下轮起手注入
    pub pending_bg_reminders: Vec<String>,
    /// 子 Agent 累计 token 用量（与主 session 分开统计，/cost 单列）
    pub sub_input_tokens: u32,
    pub sub_output_tokens: u32,
    /// 子 Agent 生命周期管理中心（中断/退出清理用）
    pub hub: Arc<wyj_tools::SubAgentHub>,
}

impl AppState {
    fn new(
        cwd: PathBuf,
        model_name: String,
        context_window: u32,
        mode: AgentMode,
        config: Config,
        hub: Arc<wyj_tools::SubAgentHub>,
    ) -> Self {
        Self {
            messages: vec![],
            streaming_buf: String::new(),
            is_thinking: false,
            permission_dialog: None,
            ask_question_dialog: None,
            plan_dialog: None,
            exec_mode_confirm: None,
            scroll_offset: 0,
            chat_height: 20,
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
            session_allowed: HashSet::new(),
            current_todos: None,
            todo_panel_expanded: false,
            todo_stats: HashMap::new(),
            session_picker: None,
            settings_dialog: None,
            profile_dialog: None,
            memory_dialog: None,
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
            scrollbar_area: Rect::default(),
            injector: None,
            pending_queue: vec![],
            sub_agents: std::collections::BTreeMap::new(),
            pending_bg_reminders: vec![],
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            hub,
        }
    }

    /// 是否有仍在运行的子 Agent（驱动 spinner 与底部聚合面板）
    pub fn has_running_sub_agents(&self) -> bool {
        self.sub_agents
            .values()
            .any(|s| s.status == SubAgentStatus::Running)
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
        self.permission_dialog = None;
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
        self.messages.push(ChatMessage::user(text));
    }

    fn flush_streaming(&mut self) {
        if !self.streaming_buf.is_empty() {
            let text = std::mem::take(&mut self.streaming_buf);
            self.messages.push(ChatMessage::assistant(text));
        }
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(d) => self.streaming_buf.push_str(&d),

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
                self.messages.push(msg);
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
                // Agent 工具：把 ToolCall 上绑定的子 Agent id 带到 ToolResult，
                // 供展开时渲染内部工具调用明细
                let sub_id = if name == "Agent" {
                    self.messages
                        .iter()
                        .rev()
                        .find(|m| {
                            matches!(m.role, MessageRole::ToolCall) && m.sequence_no == Some(seq)
                        })
                        .and_then(|m| m.sub_agent_id)
                } else {
                    None
                };
                let mut msg =
                    ChatMessage::tool_result(output, is_error, elapsed_secs, seq, name, summary);
                msg.sub_agent_id = sub_id;
                self.messages.push(msg);
                if let Some(said) = sub_id {
                    if let Some(s) = self.sub_agents.get_mut(&said) {
                        s.has_result = true;
                    }
                }
            }

            AgentEvent::PermissionRequest {
                tool_name,
                input_preview,
                tx_id,
            } => {
                self.permission_dialog = Some(PermissionDialog {
                    tool_name,
                    input_preview,
                    tx_id,
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
                    self.messages
                        .push(ChatMessage::turn_summary(elapsed, d_in, d_out));
                }
            }

            AgentEvent::Error(e) => {
                self.flush_streaming();
                self.is_thinking = false;
                self.injector = None;
                self.messages
                    .push(ChatMessage::assistant_err(format!("[错误] {e}")));
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
                self.scroll_offset = 0; // 自动滚动到最新任务列表
            }

            AgentEvent::AskQuestions {
                questions,
                response_tx,
            } => {
                self.ask_question_dialog = Some(AskQuestionDialog::new(questions, response_tx));
            }

            AgentEvent::BashResult {
                output,
                exit_code,
                elapsed_secs,
            } => {
                self.messages
                    .push(ChatMessage::bash_output(output, exit_code, elapsed_secs));
                self.scroll_offset = 0;
            }

            AgentEvent::PlanApprovalRequest { plan, response_tx } => {
                self.is_thinking = false; // 暂停 spinner，等待用户操作
                self.plan_dialog = Some(PlanApprovalDialog {
                    plan,
                    scroll: 0,
                    response_tx,
                });
            }

            AgentEvent::SubAgent(ev) => self.apply_sub_agent_event(ev),

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
                        dialog.overlay = match result {
                            Ok(models) => ProfileOverlay::ModelsPicker {
                                entry_idx,
                                field_idx,
                                models,
                                selected: 0,
                            },
                            Err(e) => {
                                dialog.error =
                                    Some(wyj_i18n::tr_fmt("profile.fetch.failed", &[("err", &e)]));
                                ProfileOverlay::None
                            }
                        };
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
            } => {
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
                    },
                );
            }
            E::ToolStart {
                id,
                tool_name,
                arg_summary,
            } => {
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
            } => {
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
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.status = if is_error {
                        SubAgentStatus::Failed
                    } else {
                        SubAgentStatus::Done
                    };
                    s.final_elapsed = Some(elapsed_secs);
                    s.current_tool = None;
                }
                if background {
                    // 结果包成 system-reminder：主 Agent 忙则经注入通道在工具边界
                    // 送达；空闲则暂存，下一轮起手合并进 user 消息
                    let reminder = wyj_i18n::tr_fmt(
                        "subagent.bg_done_reminder",
                        &[
                            ("id", format!("a{id}").as_str()),
                            ("type", &agent_type),
                            ("desc", &description),
                            ("elapsed", &format!("{elapsed_secs:.0}")),
                            ("result", &result),
                        ],
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
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

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
    )
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
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
                        return s.contains("已批准计划");
                    }
                }
            }
        }
    }
    false
}

/// plan 模式下返回注入了 ExitPlanMode 工具和 system prompt 的 agent 副本，否则直接 Arc::clone
fn plan_turn_agent(base: &Arc<Agent>, mode: &AgentMode) -> Arc<Agent> {
    if !matches!(mode, AgentMode::Plan) {
        return Arc::clone(base);
    }
    let mut a = (**base).clone();
    a.register_tool(Arc::new(ExitPlanModeTool));
    let a = a.append_system(wyj_i18n::tr("system_prompt.plan_turn"));
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
    agent
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
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let path_buf = path.to_path_buf();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&editor).arg(&path_buf).status()
    })
    .await;

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
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

/// 根据 AgentMode 构建对应的 PermissionMode
fn mode_to_permission(mode: &AgentMode) -> PermissionMode {
    match mode {
        AgentMode::Plan => {
            let set: HashSet<String> = [
                "Read",
                "Glob",
                "Grep",
                "WebFetch",
                "AskQuestion",
                "Bash",         // 只读命令，由 system prompt 约束
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

#[allow(clippy::too_many_arguments)]
async fn tui_main<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
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
    let mut input = InputBox::new();
    let mut current_session_id = session_id;

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
    let cmd_registry = standard_registry_with_skills(&home_dir, &cwd);

    // 工具回调：ToolStart/ToolEnd/Usage → AgentEvent，同时拦截 TodoWrite 读取快照
    let agent = wire_tool_callback(agent, agent_tx.clone(), todo_store.clone());

    // 用 RwLock 包装 agent，支持 /model 热切换
    let shared_agent = Arc::new(std::sync::RwLock::new(Arc::new(agent)));

    // 初始化 Session：若有历史消息则恢复，并重建 TUI 显示
    let has_initial = !initial_messages.is_empty();
    let mut init_sess = Session::new();
    init_sess.messages = initial_messages;
    if has_initial {
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
        state.scroll_offset = 0;
    }
    let session = Arc::new(Mutex::new(init_sess));

    let mut last_spinner_advance = Instant::now();

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

        terminal.draw(|f| render::draw(f, &mut state, &input))?;

        // 清空 agent 事件队列
        while let Ok(ev) = agent_rx.try_recv() {
            state.apply_agent_event(ev);
        }

        // 清空子 Agent 事件队列（在 agent 事件之后排空，保证父 ToolStart 先于 Started 应用）
        while let Ok(ev) = sub_rx.try_recv() {
            state.apply_agent_event(AgentEvent::SubAgent(ev));
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
                    let sf = SessionFile {
                        session_id: current_session_id.clone(),
                        title: extract_title(&sess.messages),
                        last_preview: extract_preview(&sess.messages),
                        cwd: cwd.display().to_string(),
                        timestamp: now_iso(),
                        turns: state.turns,
                        input_tokens: sess.total_input_tokens,
                        output_tokens: sess.total_output_tokens,
                        messages: sess.messages.clone(),
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
                Err(_) => break,
            }
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            let ev = event::read()?;
            match ev {
                Event::Paste(pasted) => {
                    // 分组管理面板/设置面板正在编辑文本字段时，粘贴内容应进当前编辑的
                    // 字段（如 API Key、Base URL、重命名输入框），而不是主聊天输入框。
                    if let Some(dialog) = &mut state.profile_dialog {
                        if let Some(ib) = dialog.editing.as_mut() {
                            ib.insert_text(&pasted);
                            continue;
                        }
                        if let ProfileOverlay::Renaming { input, .. } = &mut dialog.overlay {
                            input.insert_text(&pasted);
                            continue;
                        }
                    }

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
                            state.pending_attachments.push(Attachment::File { path });
                        } else {
                            // 普通文字粘贴
                            input.insert_text(&pasted);
                            update_slash_completions(&mut state, &input, &cmd_registry);
                        }
                    }
                }
                Event::Mouse(MouseEvent { kind, .. }) => match kind {
                    MouseEventKind::ScrollUp => {
                        state.scroll_offset = state.scroll_offset.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        state.scroll_offset = state.scroll_offset.saturating_sub(3);
                    }
                    _ => {}
                },
                Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                                                let _ = store.save(&SessionFile {
                                                    session_id: current_session_id.clone(),
                                                    title: extract_title(&sess.messages),
                                                    last_preview: extract_preview(&sess.messages),
                                                    cwd: cwd.display().to_string(),
                                                    timestamp: now_iso(),
                                                    turns: state.turns,
                                                    input_tokens: sess.total_input_tokens,
                                                    output_tokens: sess.total_output_tokens,
                                                    messages: sess.messages.clone(),
                                                });
                                            }
                                        }
                                        let mut sess = session.lock().await;
                                        *sess = Session::new();
                                        drop(sess);
                                        current_session_id = new_session_id();
                                        state.messages.clear();
                                        state.total_input_tokens = 0;
                                        state.total_output_tokens = 0;
                                        state.context_tokens = 0;
                                        state.turns = 0;
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
                                                    let _ = store.save(&SessionFile {
                                                        session_id: current_session_id.clone(),
                                                        title: extract_title(&sess.messages),
                                                        last_preview: extract_preview(
                                                            &sess.messages,
                                                        ),
                                                        cwd: cwd.display().to_string(),
                                                        timestamp: now_iso(),
                                                        turns: state.turns,
                                                        input_tokens: sess.total_input_tokens,
                                                        output_tokens: sess.total_output_tokens,
                                                        messages: sess.messages.clone(),
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
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
                                                    state.context_tokens = context_tokens;
                                                    state.turns = file.turns;
                                                    state.scroll_offset = 0;
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

                                                let mut new_prompt =
                                                    wyj_i18n::tr("system_prompt.default");
                                                new_prompt.push_str(&system_prompt_extra);

                                                let model_for_mode = state
                                                    .config
                                                    .model_for_mode(&state.mode)
                                                    .to_string();
                                                match rebuild_fn(&state.config, &model_for_mode) {
                                                    Ok(new_agent) => {
                                                        let new_agent =
                                                            new_agent.with_system(new_prompt);
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
                                                        let updated_agent =
                                                            (*shared_agent.read().unwrap())
                                                                .as_ref()
                                                                .clone()
                                                                .with_system(new_prompt);
                                                        *shared_agent.write().unwrap() =
                                                            Arc::new(updated_agent);
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

                    // ⓪.6 分组管理面板拦截（/model 无参命令触发）
                    if state.profile_dialog.is_some() {
                        let is_editing = state
                            .profile_dialog
                            .as_ref()
                            .map(|d| d.editing.is_some())
                            .unwrap_or(false);

                        if is_editing {
                            match key.code {
                                KeyCode::Enter => {
                                    if let Some(dialog) = &mut state.profile_dialog {
                                        if let Some(mut ib) = dialog.editing.take() {
                                            let text = ib.take();
                                            if let (entry_idx, Some(field_idx)) =
                                                dialog.selected_row()
                                            {
                                                dialog.entries[entry_idx]
                                                    .set_text_value(field_idx, text);
                                            }
                                            dialog.error = None;
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    if let Some(dialog) = &mut state.profile_dialog {
                                        dialog.editing = None;
                                    }
                                }
                                KeyCode::Char(c) => {
                                    if let Some(ib) = state
                                        .profile_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.insert_char(c);
                                    }
                                }
                                KeyCode::Backspace => {
                                    if let Some(ib) = state
                                        .profile_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.backspace();
                                    }
                                }
                                KeyCode::Left => {
                                    if let Some(ib) = state
                                        .profile_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.move_left();
                                    }
                                }
                                KeyCode::Right => {
                                    if let Some(ib) = state
                                        .profile_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.move_right();
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }

                        let has_overlay = state
                            .profile_dialog
                            .as_ref()
                            .map(|d| !matches!(d.overlay, ProfileOverlay::None))
                            .unwrap_or(false);
                        if has_overlay {
                            let dialog = state.profile_dialog.as_mut().unwrap();
                            match &mut dialog.overlay {
                                ProfileOverlay::Renaming { entry_idx, input } => {
                                    let entry_idx = *entry_idx;
                                    match key.code {
                                        KeyCode::Enter => {
                                            let new_name = input.take();
                                            dialog.entries[entry_idx].name = new_name;
                                            dialog.overlay = ProfileOverlay::None;
                                        }
                                        KeyCode::Esc => {
                                            dialog.overlay = ProfileOverlay::None;
                                        }
                                        KeyCode::Char(c) => input.insert_char(c),
                                        KeyCode::Backspace => input.backspace(),
                                        KeyCode::Left => input.move_left(),
                                        KeyCode::Right => input.move_right(),
                                        _ => {}
                                    }
                                }
                                ProfileOverlay::TemplatePicker { selected } => match key.code {
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
                                        let existing_names: Vec<String> =
                                            dialog.entries.iter().map(|e| e.name.clone()).collect();
                                        let template = &wyj_api::PROFILE_TEMPLATES[idx];
                                        let new_entry = ProfileEntryDraft::from_template(
                                            template,
                                            &existing_names,
                                        );
                                        let suggested_name = new_entry.name.clone();
                                        dialog.entries.push(new_entry);
                                        let new_idx = dialog.entries.len() - 1;
                                        dialog.expanded = Some(new_idx);
                                        dialog.cursor = dialog
                                            .rows()
                                            .len()
                                            .saturating_sub(PROFILE_FIELD_COUNT + 1);
                                        // 新建后立即提示命名，而不是让用户自己发现 'r' 键
                                        let mut ib = InputBox::new();
                                        ib.insert_text(&suggested_name);
                                        dialog.overlay = ProfileOverlay::Renaming {
                                            entry_idx: new_idx,
                                            input: ib,
                                        };
                                    }
                                    KeyCode::Esc => {
                                        dialog.overlay = ProfileOverlay::None;
                                    }
                                    _ => {}
                                },
                                ProfileOverlay::ConfirmDelete { entry_idx } => {
                                    let entry_idx = *entry_idx;
                                    match key.code {
                                        KeyCode::Char('y')
                                        | KeyCode::Char('Y')
                                        | KeyCode::Enter => {
                                            dialog.entries.remove(entry_idx);
                                            if dialog.active_idx > entry_idx {
                                                dialog.active_idx -= 1;
                                            }
                                            match dialog.expanded {
                                                Some(e) if e == entry_idx => dialog.expanded = None,
                                                Some(e) if e > entry_idx => {
                                                    dialog.expanded = Some(e - 1)
                                                }
                                                _ => {}
                                            }
                                            dialog.overlay = ProfileOverlay::None;
                                            dialog.clamp_cursor();
                                        }
                                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                            dialog.overlay = ProfileOverlay::None;
                                        }
                                        _ => {}
                                    }
                                }
                                ProfileOverlay::FetchingModels { .. } => {
                                    if let KeyCode::Esc = key.code {
                                        dialog.overlay = ProfileOverlay::None;
                                    }
                                }
                                ProfileOverlay::ModelsPicker {
                                    entry_idx,
                                    field_idx,
                                    models,
                                    selected,
                                } => match key.code {
                                    KeyCode::Up => {
                                        if *selected > 0 {
                                            *selected -= 1;
                                        }
                                    }
                                    KeyCode::Down => {
                                        if *selected + 1 < models.len() {
                                            *selected += 1;
                                        }
                                    }
                                    KeyCode::Enter => {
                                        let entry_idx = *entry_idx;
                                        let field_idx = *field_idx;
                                        let chosen = models[*selected].clone();
                                        dialog.entries[entry_idx].set_text_value(field_idx, chosen);
                                        dialog.overlay = ProfileOverlay::None;
                                    }
                                    KeyCode::Esc => {
                                        dialog.overlay = ProfileOverlay::None;
                                    }
                                    _ => {}
                                },
                                ProfileOverlay::None => {}
                            }
                            continue;
                        }

                        match key.code {
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    if dialog.cursor > 0 {
                                        dialog.cursor -= 1;
                                    }
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
                                    let (entry_idx, field_idx) = dialog.selected_row();
                                    if let Some(f) = field_idx {
                                        if matches!(profile_field_kind(f), SettingsFieldKind::Enum)
                                        {
                                            dialog.entries[entry_idx].cycle_provider(false);
                                        }
                                    }
                                }
                            }
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let (entry_idx, field_idx) = dialog.selected_row();
                                    if let Some(f) = field_idx {
                                        if matches!(profile_field_kind(f), SettingsFieldKind::Enum)
                                        {
                                            dialog.entries[entry_idx].cycle_provider(true);
                                        }
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let (entry_idx, field_idx) = dialog.selected_row();
                                    match field_idx {
                                        None => {
                                            dialog.expanded = if dialog.expanded == Some(entry_idx)
                                            {
                                                None
                                            } else {
                                                Some(entry_idx)
                                            };
                                            dialog.clamp_cursor();
                                        }
                                        Some(f) => match profile_field_kind(f) {
                                            SettingsFieldKind::Enum => {
                                                dialog.entries[entry_idx].cycle_provider(true)
                                            }
                                            SettingsFieldKind::Text => {
                                                let mut ib = InputBox::new();
                                                ib.insert_text(
                                                    dialog.entries[entry_idx].text_value(f),
                                                );
                                                dialog.editing = Some(ib);
                                            }
                                        },
                                    }
                                }
                            }
                            KeyCode::Char('a') => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let (entry_idx, _) = dialog.selected_row();
                                    dialog.active_idx = entry_idx;
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Char('n') => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    dialog.overlay = ProfileOverlay::TemplatePicker { selected: 0 };
                                }
                            }
                            KeyCode::Char('d') => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let (entry_idx, _) = dialog.selected_row();
                                    if dialog.entries.len() <= 1 {
                                        dialog.error = Some(wyj_i18n::tr("profile.error.last_one"));
                                    } else if entry_idx == dialog.active_idx {
                                        dialog.error =
                                            Some(wyj_i18n::tr("profile.error.delete_active"));
                                    } else {
                                        dialog.overlay =
                                            ProfileOverlay::ConfirmDelete { entry_idx };
                                    }
                                }
                            }
                            KeyCode::Char('r') => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let (entry_idx, _) = dialog.selected_row();
                                    let mut ib = InputBox::new();
                                    ib.insert_text(&dialog.entries[entry_idx].name);
                                    dialog.overlay = ProfileOverlay::Renaming {
                                        entry_idx,
                                        input: ib,
                                    };
                                }
                            }
                            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if let Some(dialog) = &mut state.profile_dialog {
                                    let (entry_idx, field_idx) = dialog.selected_row();
                                    if let Some(f) = field_idx {
                                        if PROFILE_MODEL_FIELD_IDXS.contains(&f) {
                                            let entry = dialog.entries[entry_idx].clone();
                                            let api_key = entry.api_key.clone();
                                            if api_key.trim().is_empty() {
                                                dialog.error = Some(wyj_i18n::tr(
                                                    "profile.fetch.need_api_key",
                                                ));
                                            } else {
                                                let provider = entry.provider();
                                                let base_url = if entry.base_url.trim().is_empty() {
                                                    match provider {
                                                        wyj_config::Provider::Anthropic => {
                                                            "https://api.anthropic.com".to_string()
                                                        }
                                                        wyj_config::Provider::OpenAI => {
                                                            "https://api.openai.com/v1".to_string()
                                                        }
                                                    }
                                                } else {
                                                    entry.base_url.clone()
                                                };
                                                dialog.overlay = ProfileOverlay::FetchingModels {
                                                    entry_idx,
                                                    field_idx: f,
                                                };
                                                let tx = agent_tx.clone();
                                                tokio::spawn(async move {
                                                    let result = wyj_api::fetch_model_ids(
                                                        &provider, &base_url, &api_key,
                                                    )
                                                    .await
                                                    .map_err(|e| e.to_string());
                                                    let _ = tx
                                                        .send(AgentEvent::ModelsFetched {
                                                            entry_idx,
                                                            field_idx: f,
                                                            result,
                                                        })
                                                        .await;
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let mut should_close = false;
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
                                        new_cfg.profiles =
                                            dialog.entries.iter().map(|e| e.to_profile()).collect();
                                        new_cfg.active_profile =
                                            dialog.entries[dialog.active_idx].name.clone();
                                        match new_cfg.save() {
                                            Ok(()) => {
                                                should_close = true;
                                                state.config = new_cfg.clone();
                                                let model_for_mode = state
                                                    .config
                                                    .model_for_mode(&state.mode)
                                                    .to_string();
                                                match rebuild_fn(&state.config, &model_for_mode) {
                                                    Ok(new_agent) => {
                                                        let mut new_prompt =
                                                            wyj_i18n::tr("system_prompt.default");
                                                        new_prompt.push_str(&system_prompt_extra);
                                                        let new_agent =
                                                            new_agent.with_system(new_prompt);
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
                                                            wyj_i18n::tr("profile.saved"),
                                                        ));
                                                    }
                                                    Err(e) => {
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
                                    state.profile_dialog = None;
                                }
                            }
                            KeyCode::Esc => {
                                state.profile_dialog = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ① plan 批准对话框最高优先级
                    if state.plan_dialog.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if let Some(dlg) = state.plan_dialog.take() {
                                    let _ = dlg.response_tx.send(true);
                                    // 切换至执行模式；switch_mode 同步更新 shared_permission，
                                    // 对正在运行的这一轮（ExitPlanMode 调用所在的 turn）立即生效。
                                    let new_mode = AgentMode::Normal;
                                    switch_mode(&shared_mode, &shared_permission, new_mode.clone())
                                        .await;
                                    state.mode = new_mode;
                                    state.messages.push(ChatMessage::system(
                                        "已批准计划，切换至执行模式。".to_string(),
                                    ));
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                if let Some(dlg) = state.plan_dialog.take() {
                                    let _ = dlg.response_tx.send(false);
                                    state.messages.push(ChatMessage::system(
                                        "已取消，继续保持 plan 模式。".to_string(),
                                    ));
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dlg) = state.plan_dialog.as_mut() {
                                    dlg.scroll = dlg.scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dlg) = state.plan_dialog.as_mut() {
                                    dlg.scroll = dlg.scroll.saturating_add(1);
                                }
                            }
                            KeyCode::PageUp => {
                                if let Some(dlg) = state.plan_dialog.as_mut() {
                                    dlg.scroll = dlg.scroll.saturating_sub(10);
                                }
                            }
                            KeyCode::PageDown => {
                                if let Some(dlg) = state.plan_dialog.as_mut() {
                                    dlg.scroll = dlg.scroll.saturating_add(10);
                                }
                            }
                            _ => {}
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

                    // ③ 权限对话框拦截（分级授权）
                    if state.permission_dialog.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    false,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("allowed {}", dlg.tool_name),
                                ));
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.session_allowed.insert(dlg.tool_name.clone());
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    false,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("allowed {} (session)", dlg.tool_name),
                                ));
                            }
                            KeyCode::Char('p') | KeyCode::Char('P') => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    false,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("always allowed {}", dlg.tool_name),
                                ));
                            }
                            _ => {
                                let dlg = state.permission_dialog.take().unwrap();
                                state.messages.push(ChatMessage::tool_result(
                                    String::new(),
                                    true,
                                    0.0,
                                    0,
                                    dlg.tool_name.clone(),
                                    format!("denied {}", dlg.tool_name),
                                ));
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

                    // 其他按键重置 Ctrl+C 计数
                    state.ctrl_c_pressed = false;
                    state.last_ctrl_c = None;

                    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
                        input.insert_newline();
                        state.slash_completions.clear();
                    } else if key.code == KeyCode::Enter && !state.is_thinking {
                        if !input.is_empty() {
                            let text = input.take();
                            state.scroll_offset = 0;
                            state.slash_completions.clear();
                            state.file_completions.clear();
                            state.history_idx = None;

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
                            let estimated = {
                                let sess = session.lock().await;
                                wyj_core::estimate_tokens(&sess.messages)
                            };
                            let cmd_ctx = CommandContext {
                                cwd: cwd.clone(),
                                model: state.model_name.clone(),
                                input_tokens: state.total_input_tokens,
                                output_tokens: state.total_output_tokens,
                                context_window,
                                estimated_tokens: estimated,
                                home_dir: std::env::var("HOME")
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_default(),
                                sub_input_tokens: state.sub_input_tokens,
                                sub_output_tokens: state.sub_output_tokens,
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
                                            match store.list() {
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
                                                    let _ = store.save(&SessionFile {
                                                        session_id: current_session_id.clone(),
                                                        title: extract_title(&sess.messages),
                                                        last_preview: extract_preview(
                                                            &sess.messages,
                                                        ),
                                                        cwd: cwd.display().to_string(),
                                                        timestamp: now_iso(),
                                                        turns: state.turns,
                                                        input_tokens: sess.total_input_tokens,
                                                        output_tokens: sess.total_output_tokens,
                                                        messages: sess.messages.clone(),
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
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
                                                    state.context_tokens = context_tokens;
                                                    state.turns = file.turns;
                                                    state.scroll_offset = 0;
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
                    } else if key.code == KeyCode::PageUp {
                        let step = state.chat_height.max(3);
                        state.scroll_offset = state.scroll_offset.saturating_add(step);
                    } else if key.code == KeyCode::PageDown {
                        let step = state.chat_height.max(3);
                        state.scroll_offset = state.scroll_offset.saturating_sub(step);
                    } else if key.code == KeyCode::Home
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+Home：跳到最顶（render 内 clamp 到 max_scroll）
                        state.scroll_offset = u16::MAX;
                    } else if key.code == KeyCode::End
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+End：跳到最底（最新消息）
                        state.scroll_offset = 0;
                    } else if key.code == KeyCode::Up {
                        // 输入框空 → 滚动聊天区（含鼠标滚轮上滚转来的 Up）
                        // 输入框有内容 → 历史导航
                        if input.is_empty() && state.slash_completions.is_empty() {
                            state.scroll_offset = state.scroll_offset.saturating_add(3);
                        } else if !state.is_thinking && state.slash_completions.is_empty() {
                            let hist_len = state.input_history.len();
                            if hist_len > 0 {
                                let new_idx = match state.history_idx {
                                    None => hist_len - 1,
                                    Some(i) => i.saturating_sub(1),
                                };
                                state.history_idx = Some(new_idx);
                                let recalled = state.input_history[new_idx].clone();
                                input = InputBox::new();
                                for c in recalled.chars() {
                                    if c == '\n' {
                                        input.insert_newline();
                                    } else {
                                        input.insert_char(c);
                                    }
                                }
                            }
                        }
                    } else if key.code == KeyCode::Down {
                        // 输入框空 → 滚动聊天区（含鼠标滚轮下滚转来的 Down）
                        // 输入框有内容 → 历史导航
                        if input.is_empty() && state.slash_completions.is_empty() {
                            state.scroll_offset = state.scroll_offset.saturating_sub(3);
                        } else if !state.is_thinking && state.slash_completions.is_empty() {
                            if let Some(idx) = state.history_idx {
                                if idx + 1 < state.input_history.len() {
                                    let new_idx = idx + 1;
                                    state.history_idx = Some(new_idx);
                                    let recalled = state.input_history[new_idx].clone();
                                    input = InputBox::new();
                                    for c in recalled.chars() {
                                        if c == '\n' {
                                            input.insert_newline();
                                        } else {
                                            input.insert_char(c);
                                        }
                                    }
                                } else {
                                    // 超出历史末尾，清空退出历史模式
                                    state.history_idx = None;
                                    input = InputBox::new();
                                }
                            }
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
                        input.move_to_start_of_line();
                    } else if key.code == KeyCode::End {
                        input.move_to_end_of_line();
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
                                    state.scroll_offset = 0;
                                }
                                'o' => {
                                    // Ctrl+O — 展开/折叠最后一条工具结果（对齐 Claude Code）
                                    if let Some(last_tool) = state
                                        .messages
                                        .iter_mut()
                                        .rev()
                                        .find(|m| matches!(m.role, MessageRole::ToolResult))
                                    {
                                        last_tool.expanded = !last_tool.expanded;
                                    }
                                }
                                't' => {
                                    // Ctrl+T — 折叠/展开任务列表面板（仅在满足折叠条件时生效）
                                    if let Some(items) = &state.current_todos {
                                        if is_todo_collapsible(items) {
                                            state.todo_panel_expanded = !state.todo_panel_expanded;
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
                let _ = store.save(&SessionFile {
                    session_id: current_session_id.clone(),
                    title: extract_title(&sess.messages),
                    last_preview: extract_preview(&sess.messages),
                    cwd: cwd.display().to_string(),
                    timestamp: now_iso(),
                    turns: state.turns,
                    input_tokens: sess.total_input_tokens,
                    output_tokens: sess.total_output_tokens,
                    messages: sess.messages.clone(),
                });
                resumable_session_id = Some(current_session_id.clone());
            }
        }
    }

    Ok(resumable_session_id)
}

/// 将 API Message 列表重建为 TUI 显示用的 ChatMessage 列表
fn reconstruct_display(messages: &[Message]) -> Vec<ChatMessage> {
    let mut result = Vec::new();
    let mut tool_seq = 0usize;

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
                            content, is_error, ..
                        } => {
                            let text = match content {
                                ToolResultContent::Text(s) => s.clone(),
                                ToolResultContent::Blocks(v) => {
                                    serde_json::to_string_pretty(v).unwrap_or_default()
                                }
                            };
                            let summary = text
                                .lines()
                                .next()
                                .map(|l| l.trim().to_string())
                                .unwrap_or_default();
                            result.push(ChatMessage::tool_result(
                                text,
                                *is_error,
                                0.0,
                                tool_seq,
                                String::new(),
                                summary,
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
                        ContentBlock::ToolUse { name, .. } => {
                            if !text_buf.trim().is_empty() {
                                result.push(ChatMessage::assistant(std::mem::take(&mut text_buf)));
                            } else {
                                text_buf.clear();
                            }
                            tool_seq += 1;
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
        }));
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
        }));
        let s = state.sub_agents.get(&1).unwrap();
        assert!(s.current_tool.is_none());
        assert_eq!(s.tool_log.len(), 1);
        assert_eq!(s.tool_log[0].elapsed_secs, Some(0.3));
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
    }
}
