//! TUI 应用主循环

use crate::event::{is_quit, AgentEvent};
use crate::hyperlink::{open_target, HyperlinkBackend, HyperlinkRegistry};
use crate::input::InputBox;
use crate::render;
use crate::theme::Theme;
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::style::Color as UiColor;
use ratatui::Terminal;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use wyj_api::types::{ContentBlock, Message, Role, ToolResultContent};
use wyj_commands::skill::ProjectSkillsGate;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::notify::{emit as notify_emit, NotificationEvent};
use wyj_core::tool::{AskQuestionSpec, QuestionAnswer, ToolOutputChunk};
use wyj_core::{
    discover_files, extract_preview, extract_title, new_session_id, now_iso, Agent,
    AgentDefinition, CandidatePayload, CandidateStatus, DiscoveredFile, Episode,
    EvolutionCandidate, EvolutionMemory, EvolutionStore, HistoryEntry, HistoryStore, InjectionKind,
    MemoryStatus, Session, SessionFile, SessionFileMeta, SessionMeta, SessionStore, ToolEvent,
};
use wyj_tools::todo::{is_todo_collapsible, TodoItem, TodoStatus};
use wyj_tools::trace::TraceEvent;
use wyj_tools::{
    ctx::{install_global_tool_output_sink, UiAskRequest},
    PermissionMode,
};
use wyj_tools::{ExitPlanModeTool, TodoStore, ToolCtx};

/// 后台 shell 任务退出时，读取多少字节的输出尾部喂给模型（喂之前还会再经
/// `truncate_head_tail` 收口到 4KB）。留 64KB 是为了覆盖「模型正要关心
/// 结尾几行、但任务狂刷日志」的情况。
const SHELL_WAKE_TAIL_BYTES: usize = 64 * 1024;

/// 用于 /model 热切换 / 设置面板保存后重建 Agent 的函数类型
pub type RebuildFn = Arc<dyn Fn(&Config, &str) -> anyhow::Result<Agent> + Send + Sync>;

/// 非 TUI 表面（headless REPL、`-p`、cron）探测终端列数，供 `CommandContext`
/// 的排版类命令使用。TUI 路径不用它——那边每帧从真实 frame 回写
/// （`AppState::chat_content_width`），resize 才跟得上。
///
/// 顺序：`COLUMNS` 环境变量 → `crossterm` 查询真实 TTY。前者优先是因为
/// cron / 管道里 `COLUMNS` 往往是调用方显式给的意图值，而 crossterm 在
/// 无 TTY 时可能返回默认值 80（看着像真值，实际是猜的）。都拿不到时返回
/// 0，由 `cost_view` 回落到 `DEFAULT_WIDTH`。
pub fn detect_terminal_width() -> usize {
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(parsed) = cols.trim().parse::<usize>() {
            if parsed > 0 {
                return parsed.saturating_sub(2);
            }
        }
    }
    match crossterm::terminal::size() {
        Ok((w, _)) if w > 0 => (w as usize).saturating_sub(2),
        _ => 0,
    }
}

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
    /// 自带排版的文本（`/cost` 的表格、`/usage` 的汇总）：换行与列对齐
    /// **原样保留**，不经过 markdown。
    ///
    /// 为什么需要它：markdown 把「连续非空行」当一个段落重新折行，任何依赖
    /// 列对齐的输出都会被揉成一团糊。代码块围栏也不行——会给整块加 `╭─╮`
    /// 边框并再缩进 4 列。
    Preformatted,
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
    /// AI extended thinking 段的累计耗时（Thinking 专用）。
    /// 不复用 `elapsed_secs`：那个字段按"工具执行耗时"语义被多处渲染代码消费，
    /// 混入思考耗时会被误当成工具耗时显示。默认 None，由 `flush_thinking` 填。
    pub thinking_elapsed_secs: Option<f64>,
    /// 本次工具调用的序号（ToolCall 和 ToolResult 共用，从 1 开始）
    pub sequence_no: Option<usize>,
    /// 工具名（ToolResult 专用）
    pub tool_name: Option<String>,
    /// ToolResult 的一行摘要（Claude Code ⎿ 行）
    pub display_summary: String,
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
            thinking_elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
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

    /// Thinking 消息构造。`elapsed_secs` 由 `flush_thinking` 从
    /// `thinking_started` 算出后回填（该字段此前只有写点、零读点）。
    fn thinking(content: String, elapsed_secs: Option<f64>) -> Self {
        let mut m = Self::base(MessageRole::Thinking, content);
        m.thinking_elapsed_secs = elapsed_secs;
        m
    }

    fn assistant_err(content: String) -> Self {
        let mut m = Self::base(MessageRole::Assistant, content);
        m.is_error = true;
        m
    }

    /// 自带排版的文本。内容里的换行与列对齐会被原样保留。
    pub(crate) fn preformatted(content: String) -> Self {
        Self::base(MessageRole::Preformatted, content)
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
            id: 0,
            role: MessageRole::ToolResult,
            content: output,
            is_error,
            elapsed_secs: Some(elapsed_secs),
            thinking_elapsed_secs: None,
            sequence_no: Some(seq),
            tool_name: Some(name),
            display_summary: summary,
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
            thinking_elapsed_secs: None,
            sequence_no: None,
            tool_name: None,
            display_summary: String::new(),
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

/// 后台 shell 任务状态（TUI 展示用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStatus {
    Running,
    Exited(i32),
}

/// 单个后台 shell 任务的 TUI 实时状态。
///
/// 用 `Vec` 而非 `BTreeMap`/`HashMap`：job id 形如 `bash_1` / `bash_10`，
/// 按字符串排序会把 `bash_10` 排到 `bash_2` 前面；`Vec` 天然保持 spawn
/// 顺序，面板从上到下即启动先后。
#[derive(Debug, Clone)]
pub struct ShellUiState {
    pub id: String,
    pub command: String,
    pub status: ShellStatus,
    pub started_at: Instant,
    /// 退出时定格的耗时；运行中用 started_at.elapsed() 实时算
    pub final_elapsed: Option<f64>,
    /// 是否已因本次退出触发过自动唤醒（防同一任务重复唤醒 Agent）
    pub wake_sent: bool,
}

impl ShellUiState {
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

/// 把 token 数压成短字符串。唯一目标是**宽度**：状态栏、输入框标题栏、子 Agent
/// 行内状态、Todo 详情、`/cost` 面板都要把 token 与其它字段排在同一行，位数越少越好。
///
/// - `>= 1_000_000` → 一位小数的 M（`1.5M`）
/// - `>= 10_000` → 一位小数的 k（`12.3k`）。这一步是 v1.5.22 加的：会话累计轻易
///   破六位数，原来的 `format!("{},{:03}")` 会把 `123,456`（7 字符）整段塞进标题栏，
///   在窄终端里直接把模型名和耗时挤出视野。1000~9999 仍在千分位区间，量级还没
///   到需要牺牲精度的时候。
/// - `>= 1_000` → 千分位（`1,234`）
/// - 其余原样（`678`）
pub(crate) fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n >= 1000 {
        format!("{},{:03}", n / 1000, n % 1000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod fmt_tokens_tests {
    use super::fmt_tokens;

    #[test]
    fn four_digit_and_below_keep_exact_digits_or_thousands_separator() {
        // 1000~9999 仍是千分位区间：量级还小，千分位比 "1.2k" 更精确也更易读。
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(678), "678");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1,000");
        assert_eq!(fmt_tokens(9_999), "9,999");
    }

    #[test]
    fn five_digits_switch_to_one_decimal_k() {
        // 边界：9999 → 千分位，10000 → k。两者都只差一个字符宽度，
        // 但整条规则的分界点就在这里，钉死它防止有人把阈值挪到 1000。
        assert_eq!(fmt_tokens(10_000), "10.0k");
        assert_eq!(fmt_tokens(10_001), "10.0k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(50_000), "50.0k");
        assert_eq!(fmt_tokens(300_000), "300.0k");
    }

    #[test]
    fn millions_keep_the_m_suffix() {
        assert_eq!(fmt_tokens(999_999), "1000.0k");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn formatted_width_never_grows_with_magnitude() {
        // 这条规则的全部动机：标题栏/状态栏是固定宽度的一行，token 段的
        // 字符数必须与数值大小无关，否则长会话会把前面的模型名和耗时挤出屏幕。
        let widest = (0..=2_000_000_000u32)
            .step_by(9_973)
            .map(fmt_tokens)
            .map(|s| s.chars().count())
            .max()
            .expect("非空区间");
        assert!(widest <= 7, "最宽 {widest} 字符，超出预算");
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

/// 弹窗内自由文本的输入历史（`↑` `↓` 翻上一条）。
///
/// 主输入框有自己那条历史（走 `navigate_input_or_history`），弹窗是另一条：
/// 用户在 AskQuestion/计划批准里反复输入反馈时，上下键翻回刚才那条很常用，
/// 而现状是 `↑` `↓` 被"翻题/移动选项"占用。
#[derive(Default)]
pub struct InputHistory {
    entries: Vec<String>,
    /// None = 停在最新输入（draft），Some(i) = 正在看第 i 条历史
    cursor: Option<usize>,
    draft: String,
}

impl InputHistory {
    /// 提交一条非空文本后记账；连续重复提交同一条不重复记。
    pub fn push(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.entries.last().map(|last| last == text) == Some(true) {
            return;
        }
        self.entries.push(text.to_string());
        if self.entries.len() > 50 {
            self.entries.remove(0);
        }
        self.cursor = None;
        self.draft.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `↑` 往回翻一条；没有历史返回 false（调用方据此回退到"翻题"等原语义）。
    pub fn prev(&mut self, input: &mut InputBox) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let next = match self.cursor {
            None => {
                self.draft = input.text();
                self.entries.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.cursor = Some(next);
        self.load_into(input);
        true
    }

    /// `↓` 往前翻一条；已经停在最新输入再按则恢复翻动前的草稿。
    pub fn next(&mut self, input: &mut InputBox) -> bool {
        match self.cursor {
            None => false,
            Some(i) if i + 1 < self.entries.len() => {
                self.cursor = Some(i + 1);
                self.load_into(input);
                true
            }
            Some(_) => {
                self.cursor = None;
                let draft = std::mem::take(&mut self.draft);
                input.set_text(&draft);
                true
            }
        }
    }

    fn load_into(&self, input: &mut InputBox) {
        if let Some(text) = self.cursor.and_then(|i| self.entries.get(i)) {
            input.set_text(text);
            // 翻出来的是历史内容，光标落到末尾（readline 行为）
            input.move_to_end_of_line();
        }
    }
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

// ── AGENTS.md 记忆面板：/memory 命令触发 ──────────────────────────────────────

/// 记忆面板里的一行：AGENTS.md 系候选文件 / auto-memory 开关 / auto-memory 索引入口
/// / Memory v3 clear-all 一键清空入口（按 y 二次确认执行）。
pub enum MemoryRow {
    File(DiscoveredFile),
    AutoMemoryToggle,
    AutoMemoryIndex {
        path: PathBuf,
        exists: bool,
    },
    ClearAll {
        active_count: usize,
        superseded_count: usize,
    },
}

pub struct MemoryDialog {
    pub rows: Vec<MemoryRow>,
    pub selected: usize,
    pub auto_memory_enabled: bool,
    pub error: Option<String>,
    /// 用户在 ClearAll 行上按 Enter 一次后置 Some，再按 y/Enter 才真正执行
    /// store.clear_all()；按 Esc/n 关闭。这与最终设计 §11 "AI 不能静默触发" 一致：
    /// 用户必须显式二次确认才执行破坏性批操作。
    pub pending_clear_all: bool,
}

impl MemoryDialog {
    fn new(
        cwd: &std::path::Path,
        memory_index_path: PathBuf,
        auto_memory_enabled: bool,
        active_count: usize,
        superseded_count: usize,
    ) -> Self {
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
        rows.push(MemoryRow::ClearAll {
            active_count,
            superseded_count,
        });
        Self {
            rows,
            selected: 0,
            auto_memory_enabled,
            error: None,
            pending_clear_all: false,
        }
    }
}

/// 真正执行 Memory v3 clear_all：把对话框关闭、把执行结果以系统消息形式
/// 提示用户（成功则带上备份目录，失败则带上错误）。AI 不能触发此函数，
/// 仅 TUI key handler 在用户完成二次确认后调用。
fn execute_memory_clear_all(
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    state: &mut AppState,
) {
    state.memory_dialog = None;
    // 先克隆 Arc 让 RwLock guard 尽早释放，避免 borrow 与消息推送路径打架。
    let memory_arc: Option<Arc<wyj_core::MemoryV3Store>> = {
        let agent_guard = shared_agent.read().unwrap();
        agent_guard.memory_v3_ref().cloned()
    };
    let memory: Arc<wyj_core::MemoryV3Store> = match memory_arc {
        Some(m) => m,
        None => {
            state.messages.push(ChatMessage::assistant_err(wyj_i18n::tr(
                "memory.headless_unsupported",
            )));
            return;
        }
    };
    match memory.clear_all() {
        Ok(report) => {
            let dir = report.backup_dir.display().to_string();
            state.messages.push(ChatMessage::assistant(wyj_i18n::tr_fmt(
                "memory.dialog.clear_all_done",
                &[("dir", &dir)],
            )));
        }
        Err(error) => {
            state.messages.push(ChatMessage::assistant_err(format!(
                "Memory v3 clear_all 失败: {error}"
            )));
        }
    }
}

// ── 证据化自进化治理中心：/evolve ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolutionView {
    Active,
    Candidates,
    Episodes,
    Health,
}

impl EvolutionView {
    pub const ALL: [Self; 4] = [Self::Active, Self::Candidates, Self::Episodes, Self::Health];

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Candidates => "Candidates",
            Self::Episodes => "Episodes",
            Self::Health => "Health",
        }
    }
}

#[derive(Debug, Clone)]
pub enum EvolutionRow {
    Memory(EvolutionMemory),
    Candidate(EvolutionCandidate),
    Episode(Episode),
    Health { label: String, value: String },
}

#[derive(Debug, Clone)]
pub enum EvolutionConfirmAction {
    ApproveMemory(String),
    ApproveCandidate(String),
    RejectCandidate(String),
    ForgetMemory(String),
    RollbackCandidate(String),
}

pub struct EvolutionDialog {
    pub store: Arc<EvolutionStore>,
    pub view: EvolutionView,
    pub rows: Vec<EvolutionRow>,
    pub selected: usize,
    pub detail_scroll: u16,
    pub error: Option<String>,
    pub confirm: Option<EvolutionConfirmAction>,
}

impl EvolutionDialog {
    fn new(store: Arc<EvolutionStore>) -> Self {
        let mut dialog = Self {
            store,
            view: EvolutionView::Active,
            rows: Vec::new(),
            selected: 0,
            detail_scroll: 0,
            error: None,
            confirm: None,
        };
        dialog.reload();
        dialog
    }

    fn reload(&mut self) {
        let loaded: anyhow::Result<Vec<EvolutionRow>> = match self.view {
            EvolutionView::Active => self.store.list_memories().map(|memories| {
                let mut rows = memories
                    .into_iter()
                    .filter(|memory| memory.status == MemoryStatus::Active)
                    .map(EvolutionRow::Memory)
                    .collect::<Vec<_>>();
                rows.extend(
                    self.store
                        .list_candidates()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|candidate| candidate.status == CandidateStatus::Active)
                        .map(EvolutionRow::Candidate),
                );
                rows
            }),
            EvolutionView::Candidates => self.store.list_memories().map(|memories| {
                let mut rows = memories
                    .into_iter()
                    .filter(|memory| {
                        matches!(
                            memory.status,
                            MemoryStatus::Proposed | MemoryStatus::Conflict
                        )
                    })
                    .map(EvolutionRow::Memory)
                    .collect::<Vec<_>>();
                rows.extend(
                    self.store
                        .list_candidates()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|candidate| candidate.status != CandidateStatus::Active)
                        .map(EvolutionRow::Candidate),
                );
                rows
            }),
            EvolutionView::Episodes => self
                .store
                .list_episodes(200)
                .map(|episodes| episodes.into_iter().map(EvolutionRow::Episode).collect()),
            EvolutionView::Health => self.store.status().map(|status| {
                vec![
                    EvolutionRow::Health {
                        label: "project".into(),
                        value: status.project_id,
                    },
                    EvolutionRow::Health {
                        label: "store".into(),
                        value: status.directory.display().to_string(),
                    },
                    EvolutionRow::Health {
                        label: "episodes".into(),
                        value: status.episodes.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "pending candidates".into(),
                        value: status.pending_candidates.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "pending candidates".into(),
                        value: status.pending_candidates.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "store bytes".into(),
                        value: status.store_bytes.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "consecutive failures".into(),
                        value: status.health.consecutive_failures.to_string(),
                    },
                    EvolutionRow::Health {
                        label: "last error".into(),
                        value: status.health.last_error.unwrap_or_else(|| "none".into()),
                    },
                ]
            }),
        };
        match loaded {
            Ok(rows) => {
                self.rows = rows;
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
                self.error = None;
            }
            Err(error) => {
                self.rows.clear();
                self.selected = 0;
                self.error = Some(error.to_string());
            }
        }
        self.detail_scroll = 0;
    }

    fn cycle_view(&mut self, forward: bool) {
        let index = EvolutionView::ALL
            .iter()
            .position(|view| *view == self.view)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % EvolutionView::ALL.len()
        } else {
            (index + EvolutionView::ALL.len() - 1) % EvolutionView::ALL.len()
        };
        self.view = EvolutionView::ALL[next];
        self.selected = 0;
        self.confirm = None;
        self.reload();
    }
}

fn generated_skill_scope(path: &Path, cwd: &Path) -> anyhow::Result<wyj_store::InstallScope> {
    let global = wyj_config::config_dir()?.join("skills");
    if path.starts_with(global) {
        Ok(wyj_store::InstallScope::Global)
    } else if path.starts_with(wyj_config::project_config_dir(cwd).join("skills")) {
        Ok(wyj_store::InstallScope::Project)
    } else {
        anyhow::bail!("Skill activation path is outside managed directories")
    }
}

async fn execute_evolution_action(
    action: EvolutionConfirmAction,
    store: Arc<EvolutionStore>,
    cwd: &Path,
    session_store: Option<&Arc<SessionStore>>,
    session_id: &str,
    session: &Arc<Mutex<Session>>,
) -> anyhow::Result<String> {
    match action {
        EvolutionConfirmAction::ApproveMemory(id) => {
            store.activate_memory(&id)?;
            Ok(format!("Memory {id} 已激活，将在下一回合生效"))
        }
        EvolutionConfirmAction::ApproveCandidate(id) => {
            let candidate = store
                .list_candidates()?
                .into_iter()
                .find(|candidate| candidate.id == id)
                .with_context(|| format!("candidate not found: {id}"))?;
            anyhow::ensure!(
                matches!(
                    candidate.status,
                    CandidateStatus::Proposed | CandidateStatus::Validated
                ),
                "candidate is not ready for approval"
            );
            let sessions = session_store.context("checkpoint storage unavailable")?;
            let checkpoint_store =
                wyj_core::CheckpointStore::configured(sessions.dir(), session_id.to_string())?;
            // 这个函数由主循环 `.await`，绝不能在这里等锁——等下去整个界面
            // 就停在这一帧（不重绘、不读按键），回合越久冻得越死。
            let messages = session
                .try_lock()
                .map_err(|_| anyhow::anyhow!("{}", wyj_i18n::tr("session.busy_notice")))?
                .messages
                .clone();
            let checkpoint = checkpoint_store.create(
                cwd,
                &messages,
                wyj_core::CheckpointKind::Manual,
                Some(format!("before Evolution approval {id}")),
            )?;
            let activated_path = match &candidate.payload {
                CandidatePayload::Rule { .. } => None,
                CandidatePayload::Skill {
                    skill_name,
                    skill_md,
                    eval,
                    ..
                } => {
                    anyhow::ensure!(eval.structural_pass, "Skill eval did not pass");
                    Some(wyj_store::skill_install::install_generated_skill(
                        &wyj_store::skill_install::GeneratedSkillInstallRequest {
                            name: skill_name,
                            content: skill_md,
                            scope: wyj_store::InstallScope::Project,
                            source_id: &id,
                        },
                        cwd,
                    )?)
                }
            };
            if let Err(error) = store.mark_candidate_active(&id, activated_path.clone()) {
                if let (CandidatePayload::Skill { skill_name, .. }, Some(path)) =
                    (&candidate.payload, activated_path.as_deref())
                {
                    if let Ok(scope) = generated_skill_scope(path, cwd) {
                        let _ = wyj_store::skill_install::rollback_generated_skill(
                            skill_name, &id, scope, cwd,
                        );
                    }
                }
                return Err(error);
            }
            Ok(format!(
                "Candidate {id} 已激活（checkpoint {}），下一回合生效",
                checkpoint.id
            ))
        }
        EvolutionConfirmAction::RejectCandidate(id) => {
            store.reject_candidate(&id, Some("rejected from /evolve".into()))?;
            Ok(format!("Candidate {id} 已拒绝"))
        }
        EvolutionConfirmAction::ForgetMemory(id) => {
            store.forget_memory(&id)?;
            Ok(format!("Memory {id} 已遗忘"))
        }
        EvolutionConfirmAction::RollbackCandidate(id) => {
            let candidate = store
                .list_candidates()?
                .into_iter()
                .find(|candidate| candidate.id == id)
                .with_context(|| format!("candidate not found: {id}"))?;
            anyhow::ensure!(
                candidate.status == CandidateStatus::Active,
                "candidate is not active"
            );
            if let CandidatePayload::Skill { skill_name, .. } = &candidate.payload {
                let path = candidate
                    .activated_path
                    .as_deref()
                    .context("active Skill candidate has no activation path")?;
                let scope = generated_skill_scope(path, cwd)?;
                wyj_store::skill_install::rollback_generated_skill(skill_name, &id, scope, cwd)?;
            }
            store.rollback_candidate(&id)?;
            Ok(format!("Candidate {id} 已回滚，下一回合生效"))
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

/// 单个分组的可编辑字段数（provider/model/plan_model/exec_model/base_url/api_key/
/// max_tokens/context_window/reasoning_effort/thinking_switch）
pub const PROFILE_FIELD_COUNT: usize = 10;

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
    "settings.field.reasoning_effort",
    "settings.field.thinking_switch",
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
    /// v1.4.4 能力运行时字段；设置面板尚未暴露编辑入口，仅透传保留。
    pub vendor: Option<String>,
    pub wire_protocol: Option<wyj_config::WireProtocol>,
    pub model: String,
    pub plan_model: String,
    pub exec_model: String,
    pub base_url: String,
    pub api_key: String,
    /// 设置面板不读取或展示环境变量值，只透传 secret reference 名称。
    pub api_key_env: Option<String>,
    pub max_tokens: String,
    pub context_window: String,
    /// 是否支持图片输入（面板暂不暴露编辑入口，仅透传保留原值）
    pub vision: bool,
    /// thinking 配置（面板暂不暴露编辑入口，仅透传保留原值）
    pub thinking_budget: Option<u32>,
    pub interleaved_thinking: bool,
    pub prompt_cache: Option<bool>,
    pub openai_stream_options: Option<bool>,
    /// OpenAI-vendor reasoning_effort 档位（low/medium/high/max/xhigh/adaptive）
    pub reasoning_effort: String,
    /// OpenAI-vendor thinking.type 字符串（enabled/disabled/auto/adaptive）
    pub thinking_switch: String,
    /// 每轮并行工具数上限（面板暂不暴露编辑入口，仅透传保留原值）。
    /// 漏在这里会被 `to_profile` 静默重置成 None 且无任何报错。
    pub max_tools_per_turn: Option<usize>,
}

impl ProfileEntryDraft {
    fn from_profile(p: &wyj_config::Profile) -> Self {
        Self {
            name: p.name.clone(),
            provider_idx: match p.provider {
                wyj_config::Provider::Anthropic => 0,
                wyj_config::Provider::OpenAI => 1,
            },
            vendor: p.vendor.clone(),
            wire_protocol: p.wire_protocol.clone(),
            model: p.model.clone(),
            plan_model: p.plan_model.clone().unwrap_or_default(),
            exec_model: p.exec_model.clone().unwrap_or_default(),
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone().unwrap_or_default(),
            api_key_env: p.api_key_env.clone(),
            max_tokens: p.max_tokens.to_string(),
            context_window: p.context_window.to_string(),
            vision: p.vision,
            thinking_budget: p.thinking_budget,
            interleaved_thinking: p.interleaved_thinking,
            prompt_cache: p.prompt_cache,
            openai_stream_options: p.openai_stream_options,
            reasoning_effort: p.reasoning_effort.clone().unwrap_or_default(),
            thinking_switch: p.thinking_switch.clone().unwrap_or_default(),
            max_tools_per_turn: p.max_tools_per_turn,
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
            vendor: Some(t.vendor.to_string()),
            wire_protocol: Some(t.wire_protocol.clone()),
            model: t.example_model.to_string(),
            plan_model: String::new(),
            exec_model: String::new(),
            base_url: t.base_url.to_string(),
            api_key: String::new(),
            api_key_env: None,
            max_tokens: "8192".to_string(),
            context_window: "200000".to_string(),
            vision: t.vision,
            thinking_budget: None,
            interleaved_thinking: true,
            prompt_cache: t.prompt_cache,
            openai_stream_options: t.openai_stream_options,
            // 从厂商模板新建的 profile 不继承覆盖值：模板描述的是端点默认形状，
            // 并行工具数该由探测或用户显式决定。
            max_tools_per_turn: None,
            reasoning_effort: String::new(),
            thinking_switch: String::new(),
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
            8 => &self.reasoning_effort,
            9 => &self.thinking_switch,
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
            8 => self.reasoning_effort = value,
            9 => self.thinking_switch = value,
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
            vendor: self.vendor.clone(),
            wire_protocol: self.wire_protocol.clone(),
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
            api_key_env: self.api_key_env.clone(),
            max_tokens: self.max_tokens.trim().parse().unwrap_or(8192),
            context_window: self.context_window.trim().parse().unwrap_or(200_000),
            vision: self.vision,
            thinking_budget: self.thinking_budget,
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
            max_tools_per_turn: self.max_tools_per_turn,
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

    /// 首次启动引导专用构造器:默认展开 active profile,并把光标预置到
    /// `api_key` 字段行(`PROFILE_API_KEY_FIELD_IDX`),用户键入即可开始
    /// 编辑 API Key,无需先按 Enter 展开 entry。其它字段全部可访问,
    /// 但通过顶部 title / hint / api_key 必填校验引导用户聚焦 Key。
    pub fn new_for_onboarding(cfg: &Config) -> Self {
        let mut d = Self::new(cfg);
        let active_idx = cfg
            .profiles
            .iter()
            .position(|p| p.name == cfg.active_profile)
            .unwrap_or(0);
        d.expanded = Some(active_idx);
        // rows() = [Header(active_idx), Field(active_idx,0), ..., Field(active_idx,9), AddNew]
        // Header 占 1 行,Field(active_idx, k) 占 1 行 → api_key (k=5) 是下标 1+5 = 6
        d.cursor = 1 + PROFILE_API_KEY_FIELD_IDX;
        d.clamp_cursor();
        d
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
    Schedule(ScheduleInputField),
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
                    &[("field", &wyj_i18n::tr(PROFILE_FIELD_LABEL_KEYS[*field_idx]))],
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
            InputOwner::Schedule(_) => state.schedule_dialog.as_mut().map(|d| &mut d.live_input),
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
                        let model_for_mode = state.config.model_for_mode(&state.mode).to_string();
                        match rebuild_fn(&state.config, &model_for_mode) {
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
    }
    saved
}

// ── 定时任务面板：/schedule 命令触发 ─────────────────────────────────────────
//
// 与 ProfileDialog 同一套"批量编辑 + Ctrl+S/Esc 三选一保存"模型：面板内的增删
// 改都只改内存里的 `tasks` 草稿，直到显式保存才一次性写盘（`schedule::save`）
// 并同步系统 crontab（`cron_sync::sync_crontab`）。字段编辑统一先弹一个只有
// 1 项（Name/Prompt/Cwd 的"编辑"）或多项（Cron 的频率预设/NotifyOnFailure 的
// 切换）的小菜单再决定是否借用输入框，比 ProfileDialog 对不同字段类型各自特判
// 简单，代价是多一次 Enter，可接受。

pub const SCHEDULE_FIELD_COUNT: usize = 6;
const SCHEDULE_FIELD_NAME: usize = 0;
const SCHEDULE_FIELD_PROMPT: usize = 1;
const SCHEDULE_FIELD_CRON: usize = 2;
const SCHEDULE_FIELD_CWD: usize = 3;
pub const SCHEDULE_FIELD_NOTIFY: usize = 4;
const SCHEDULE_FIELD_ALLOWED_TOOLS: usize = 5;

pub const SCHEDULE_FIELD_LABEL_KEYS: [&str; SCHEDULE_FIELD_COUNT] = [
    "schedule.field.name",
    "schedule.field.prompt",
    "schedule.field.cron",
    "schedule.field.cwd",
    "schedule.field.notify_on_failure",
    "schedule.field.allowed_tools",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleRow {
    Header(usize),
    Field(usize, usize),
    AddNew,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleHeaderAction {
    ToggleExpand,
    ToggleEnabled,
    RunNow,
    ReviewPermissions,
    Delete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleFrequencyKind {
    Daily,
    Hourly,
    Weekly,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleFieldAction {
    ManualEdit,
    Freq(ScheduleFrequencyKind),
    ToggleNotify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAddAction {
    NewBlank,
    NewFromSession,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleMenuAction {
    Header(ScheduleHeaderAction),
    Field(ScheduleFieldAction),
    Add(ScheduleAddAction),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScheduleInputField {
    Field {
        task_idx: usize,
        field_idx: usize,
    },
    Frequency {
        task_idx: usize,
        kind: ScheduleFrequencyKind,
    },
}

/// `ScheduleDialog` 里当前展示的浮层。危险操作（删除任务）的二次确认复用
/// `ActionMenu.confirming`（同 ProfileDialog 的 Delete），不需要专属 overlay。
pub enum ScheduleOverlay {
    None,
    /// Esc 关闭面板时存在未保存修改，三选一确认（保存并关闭/不保存关闭/取消）
    UnsavedChanges {
        selected: usize,
    },
    /// 保存成功但同步系统 crontab 失败：数据已落盘，仅提示用户手动重试
    SyncError {
        message: String,
    },
}

/// 定时任务面板状态（/schedule 无参命令触发）
pub struct ScheduleDialog {
    pub tasks: Vec<wyj_store::schedule::ScheduleTask>,
    pub cursor: usize,
    pub expanded: Option<usize>,
    pub overlay: ScheduleOverlay,
    pub error: Option<String>,
    pub live_input: InputBox,
    pub menu: Option<ActionMenu<ScheduleRow, ScheduleMenuAction>>,
    default_cwd: std::path::PathBuf,
    /// 当前会话最近一条用户消息，供"从当前对话固化为模板"预填 prompt 草稿。
    last_user_prompt: Option<String>,
    /// 打开面板时的快照，供 Esc 时判断"是否有未保存改动"。
    saved_snapshot: Vec<wyj_store::schedule::ScheduleTask>,
}

impl ScheduleDialog {
    fn new(cwd: &std::path::Path, last_user_prompt: Option<String>) -> Self {
        let tasks = wyj_store::schedule::load()
            .map(|m| m.tasks)
            .unwrap_or_default();
        Self {
            saved_snapshot: tasks.clone(),
            tasks,
            cursor: 0,
            expanded: None,
            overlay: ScheduleOverlay::None,
            error: None,
            live_input: InputBox::new(),
            menu: None,
            default_cwd: cwd.to_path_buf(),
            last_user_prompt,
        }
    }

    fn is_dirty(&self) -> bool {
        self.tasks != self.saved_snapshot
    }

    pub fn rows(&self) -> Vec<ScheduleRow> {
        let mut rows = Vec::new();
        for i in 0..self.tasks.len() {
            rows.push(ScheduleRow::Header(i));
            if self.expanded == Some(i) {
                for f in 0..SCHEDULE_FIELD_COUNT {
                    rows.push(ScheduleRow::Field(i, f));
                }
            }
        }
        rows.push(ScheduleRow::AddNew);
        rows
    }

    fn selected_row(&self) -> ScheduleRow {
        self.rows()
            .get(self.cursor)
            .copied()
            .unwrap_or(ScheduleRow::AddNew)
    }

    fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn field_text(&self, task_idx: usize, field_idx: usize) -> String {
        let t = &self.tasks[task_idx];
        match field_idx {
            SCHEDULE_FIELD_NAME => t.name.clone(),
            SCHEDULE_FIELD_PROMPT => t.prompt.clone(),
            SCHEDULE_FIELD_CRON => t.cron.clone(),
            SCHEDULE_FIELD_CWD => t.cwd.display().to_string(),
            SCHEDULE_FIELD_ALLOWED_TOOLS => t.permissions.allowed_tools.join(","),
            _ => String::new(),
        }
    }

    fn set_field_text(&mut self, task_idx: usize, field_idx: usize, value: String) {
        let t = &mut self.tasks[task_idx];
        match field_idx {
            SCHEDULE_FIELD_NAME => t.name = value,
            SCHEDULE_FIELD_PROMPT => t.prompt = value,
            SCHEDULE_FIELD_CRON => t.cron = value,
            SCHEDULE_FIELD_CWD => t.cwd = std::path::PathBuf::from(value),
            SCHEDULE_FIELD_ALLOWED_TOOLS => {
                t.permissions.allowed_tools = comma_values(&value);
            }
            _ => {}
        }
    }

    /// 校验全部任务，返回第一个不合法任务的下标 + 错误文案。
    fn validate(&self) -> Option<(usize, String)> {
        for (i, t) in self.tasks.iter().enumerate() {
            if t.name.trim().is_empty() {
                return Some((i, wyj_i18n::tr("schedule.error.empty_name")));
            }
            if t.prompt.trim().is_empty() {
                return Some((i, wyj_i18n::tr("schedule.error.empty_prompt")));
            }
            if t.cwd.as_os_str().is_empty() {
                return Some((i, wyj_i18n::tr("schedule.error.empty_cwd")));
            }
            if let Err(e) = wyj_store::cron_sync::validate_cron(&t.cron) {
                return Some((
                    i,
                    wyj_i18n::tr_fmt("schedule.error.invalid_cron", &[("err", &e.to_string())]),
                ));
            }
        }
        None
    }

    /// 选中条目回车后弹出的操作菜单。头行 → 展开/收起、启用/停用、立即运行、删除；
    /// Cron 字段 → 频率预设四选一；NotifyOnFailure 字段 → 切换；其余字段 → 编辑；
    /// AddNew → 新建空白/从当前对话固化为模板（后者仅当会话内有过用户消息时展示）。
    fn build_menu(&self) -> Option<ActionMenu<ScheduleRow, ScheduleMenuAction>> {
        let row = self.selected_row();
        match row {
            ScheduleRow::Header(task_idx) => {
                let expanded = self.expanded == Some(task_idx);
                let task = &self.tasks[task_idx];
                let saved = self.saved_snapshot.iter().find(|t| t.id == task.id);
                let run_disabled = saved != Some(task) || task.needs_permission_review;
                let items = vec![
                    ActionMenuItem {
                        label: wyj_i18n::tr(if expanded {
                            "schedule.menu.collapse"
                        } else {
                            "schedule.menu.expand"
                        }),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleExpand),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr(if task.enabled {
                            "schedule.menu.disable"
                        } else {
                            "schedule.menu.enable"
                        }),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleEnabled),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.run_now"),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::RunNow),
                        dangerous: false,
                        disabled: run_disabled,
                        disabled_reason: run_disabled
                            .then(|| wyj_i18n::tr("schedule.menu.run_now_needs_save")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.review_permissions"),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::ReviewPermissions),
                        dangerous: true,
                        disabled: !task.needs_permission_review,
                        disabled_reason: (!task.needs_permission_review)
                            .then(|| wyj_i18n::tr("schedule.menu.permissions_reviewed")),
                    },
                    ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.delete"),
                        action: ScheduleMenuAction::Header(ScheduleHeaderAction::Delete),
                        dangerous: true,
                        disabled: false,
                        disabled_reason: None,
                    },
                ];
                Some(ActionMenu::new(row, items))
            }
            ScheduleRow::Field(_, SCHEDULE_FIELD_NOTIFY) => Some(ActionMenu::new(
                row,
                vec![ActionMenuItem {
                    label: wyj_i18n::tr("schedule.menu.toggle"),
                    action: ScheduleMenuAction::Field(ScheduleFieldAction::ToggleNotify),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }],
            )),
            ScheduleRow::Field(_, SCHEDULE_FIELD_CRON) => {
                let mk = |label_key: &str, kind: ScheduleFrequencyKind| ActionMenuItem {
                    label: wyj_i18n::tr(label_key),
                    action: ScheduleMenuAction::Field(ScheduleFieldAction::Freq(kind)),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                };
                Some(ActionMenu::new(
                    row,
                    vec![
                        mk("schedule.menu.freq_daily", ScheduleFrequencyKind::Daily),
                        mk("schedule.menu.freq_hourly", ScheduleFrequencyKind::Hourly),
                        mk("schedule.menu.freq_weekly", ScheduleFrequencyKind::Weekly),
                        mk("schedule.menu.freq_custom", ScheduleFrequencyKind::Custom),
                    ],
                ))
            }
            ScheduleRow::Field(..) => Some(ActionMenu::new(
                row,
                vec![ActionMenuItem {
                    label: wyj_i18n::tr("schedule.menu.edit"),
                    action: ScheduleMenuAction::Field(ScheduleFieldAction::ManualEdit),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }],
            )),
            ScheduleRow::AddNew => {
                let mut items = vec![ActionMenuItem {
                    label: wyj_i18n::tr("schedule.menu.new_blank"),
                    action: ScheduleMenuAction::Add(ScheduleAddAction::NewBlank),
                    dangerous: false,
                    disabled: false,
                    disabled_reason: None,
                }];
                if self.last_user_prompt.is_some() {
                    items.push(ActionMenuItem {
                        label: wyj_i18n::tr("schedule.menu.new_from_session"),
                        action: ScheduleMenuAction::Add(ScheduleAddAction::NewFromSession),
                        dangerous: false,
                        disabled: false,
                        disabled_reason: None,
                    });
                }
                Some(ActionMenu::new(row, items))
            }
        }
    }
}

/// `/schedule` 面板操作菜单的按键分发，结构与 `profile_handle_menu_key` 完全对称。
fn schedule_handle_menu_key(state: &mut AppState, code: KeyCode) {
    enum Step {
        None,
        Close,
        Cancel,
        Confirm(ScheduleMenuAction),
        Execute(ScheduleMenuAction, ScheduleRow),
    }

    let step = {
        let Some(dialog) = &mut state.schedule_dialog else {
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
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.menu = None;
            }
        }
        Step::Cancel => {
            if let Some(dialog) = &mut state.schedule_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = None;
                }
            }
        }
        Step::Confirm(action) => {
            if let Some(dialog) = &mut state.schedule_dialog {
                if let Some(menu) = &mut dialog.menu {
                    menu.confirming = Some(action);
                }
            }
        }
        Step::Execute(action, target) => {
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.menu = None;
            }
            schedule_execute_menu_action(state, action, target);
        }
    }
}

fn schedule_add_new_task(state: &mut AppState, prefill_prompt: Option<String>) {
    let Some(dialog) = &mut state.schedule_dialog else {
        return;
    };
    let now = chrono::Utc::now();
    let task = wyj_store::schedule::ScheduleTask {
        id: uuid::Uuid::new_v4().to_string(),
        name: wyj_i18n::tr("schedule.new_task_default_name"),
        prompt: prefill_prompt.unwrap_or_default(),
        cron: "0 8 * * *".to_string(),
        cwd: dialog.default_cwd.clone(),
        enabled: true,
        needs_permission_review: false,
        permissions: wyj_store::schedule::SchedulePermissions::default(),
        notify_on_failure: false,
        created_at: now,
        updated_at: now,
        last_run: None,
    };
    dialog.tasks.push(task);
    let new_idx = dialog.tasks.len() - 1;
    dialog.expanded = Some(new_idx);
    dialog.cursor = dialog
        .rows()
        .iter()
        .position(|r| *r == ScheduleRow::Header(new_idx))
        .unwrap_or(0);
}

fn schedule_execute_menu_action(
    state: &mut AppState,
    action: ScheduleMenuAction,
    target: ScheduleRow,
) {
    match action {
        ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleExpand) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.expanded = if dialog.expanded == Some(idx) {
                    None
                } else {
                    Some(idx)
                };
                dialog.clamp_cursor();
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::ToggleEnabled) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                if dialog.tasks[idx].needs_permission_review {
                    dialog.error = Some(
                        "该任务由旧版本迁移而来，需先执行 `wyj-code schedule review <id> ...` 完成权限审查"
                            .to_string(),
                    );
                } else {
                    dialog.tasks[idx].enabled = !dialog.tasks[idx].enabled;
                }
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::RunNow) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            // 只对"已保存、未脏"的任务生效（菜单项在脏态下已 disabled，这里再兜底一次），
            // 直接以子进程调用自身 `schedule run <id>`——与系统 crontab 触发路径完全
            // 一致，不在 TUI 进程内重新装配 Agent。
            let id = state
                .schedule_dialog
                .as_ref()
                .and_then(|d| d.tasks.get(idx))
                .map(|t| t.id.clone());
            if let Some(id) = id {
                if let Ok(exe) = std::env::current_exe() {
                    tokio::spawn(async move {
                        // --manual 仅为兼容 v1.4 预发布 CLI；headless 前台接管
                        // 已统一失败关闭，后台 app_computer 可与用户并行。
                        let _ = tokio::process::Command::new(exe)
                            .arg("schedule")
                            .arg("run")
                            .arg(&id)
                            .arg("--manual")
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .await;
                    });
                }
            }
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.error = Some(wyj_i18n::tr("schedule.run_now.triggered"));
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::ReviewPermissions) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                let task = &mut dialog.tasks[idx];
                task.needs_permission_review = false;
                task.enabled = false;
                dialog.error = Some(wyj_i18n::tr("schedule.permissions.review_complete"));
            }
        }
        ScheduleMenuAction::Header(ScheduleHeaderAction::Delete) => {
            let ScheduleRow::Header(idx) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.tasks.remove(idx);
                match dialog.expanded {
                    Some(e) if e == idx => dialog.expanded = None,
                    Some(e) if e > idx => dialog.expanded = Some(e - 1),
                    _ => {}
                }
                dialog.clamp_cursor();
            }
        }
        ScheduleMenuAction::Field(ScheduleFieldAction::ManualEdit) => {
            let ScheduleRow::Field(task_idx, field_idx) = target else {
                return;
            };
            let prefill = state
                .schedule_dialog
                .as_ref()
                .map(|d| d.field_text(task_idx, field_idx))
                .unwrap_or_default();
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.live_input = InputBox::new();
                dialog.live_input.insert_text(&prefill);
            }
            state.input_owner = Some(InputOwner::Schedule(ScheduleInputField::Field {
                task_idx,
                field_idx,
            }));
        }
        ScheduleMenuAction::Field(ScheduleFieldAction::ToggleNotify) => {
            let ScheduleRow::Field(task_idx, _) = target else {
                return;
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.tasks[task_idx].notify_on_failure =
                    !dialog.tasks[task_idx].notify_on_failure;
            }
        }
        ScheduleMenuAction::Field(ScheduleFieldAction::Freq(kind)) => {
            let ScheduleRow::Field(task_idx, _) = target else {
                return;
            };
            let prefill = if matches!(kind, ScheduleFrequencyKind::Custom) {
                state
                    .schedule_dialog
                    .as_ref()
                    .map(|d| d.tasks[task_idx].cron.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if let Some(dialog) = &mut state.schedule_dialog {
                dialog.live_input = InputBox::new();
                dialog.live_input.insert_text(&prefill);
            }
            state.input_owner = Some(InputOwner::Schedule(ScheduleInputField::Frequency {
                task_idx,
                kind,
            }));
        }
        ScheduleMenuAction::Add(ScheduleAddAction::NewBlank) => {
            schedule_add_new_task(state, None);
        }
        ScheduleMenuAction::Add(ScheduleAddAction::NewFromSession) => {
            let prompt = state
                .schedule_dialog
                .as_ref()
                .and_then(|d| d.last_user_prompt.clone());
            schedule_add_new_task(state, prompt);
        }
    }
}

#[cfg(test)]
mod schedule_permissions_tests {
    use super::*;

    fn migrated_task(cwd: &std::path::Path) -> wyj_store::schedule::ScheduleTask {
        let now = chrono::Utc::now();
        wyj_store::schedule::ScheduleTask {
            id: "legacy".to_string(),
            name: "legacy".to_string(),
            prompt: "review me".to_string(),
            cron: "0 8 * * *".to_string(),
            cwd: cwd.to_path_buf(),
            enabled: false,
            needs_permission_review: true,
            permissions: wyj_store::schedule::SchedulePermissions {
                allowed_tools: vec!["Read".to_string(), "Bash".to_string()],
            },
            notify_on_failure: false,
            created_at: now,
            updated_at: now,
            last_run: None,
        }
    }

    #[test]
    fn reviewing_migrated_permissions_keeps_task_disabled_and_preserves_manifest() {
        let cwd = tempfile::tempdir().unwrap();
        let task = migrated_task(cwd.path());
        let expected_permissions = task.permissions.clone();
        let mut state = AppState::new(
            cwd.path().to_path_buf(),
            "test-model".to_string(),
            200_000,
            AgentMode::Normal,
            Config::default(),
            Arc::new(wyj_tools::SubAgentHub::new()),
        );
        state.schedule_dialog = Some(ScheduleDialog {
            tasks: vec![task.clone()],
            cursor: 0,
            expanded: None,
            overlay: ScheduleOverlay::None,
            error: None,
            live_input: InputBox::new(),
            menu: None,
            default_cwd: cwd.path().to_path_buf(),
            last_user_prompt: None,
            saved_snapshot: vec![task],
        });

        schedule_execute_menu_action(
            &mut state,
            ScheduleMenuAction::Header(ScheduleHeaderAction::ReviewPermissions),
            ScheduleRow::Header(0),
        );

        let reviewed = &state.schedule_dialog.as_ref().unwrap().tasks[0];
        assert!(!reviewed.needs_permission_review);
        assert!(!reviewed.enabled);
        assert_eq!(reviewed.permissions, expected_permissions);
    }
}

fn parse_hh_mm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

fn comma_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// 频率预设的结构化文本输入解析：Daily 期望 "HH:MM"，Hourly 期望 "MM"，
/// Weekly 期望 "D HH:MM"（D=0..6，0=周日），Custom 直接校验为标准 5 段 cron。
fn schedule_parse_frequency_input(
    kind: ScheduleFrequencyKind,
    text: &str,
) -> Result<String, String> {
    let text = text.trim();
    match kind {
        ScheduleFrequencyKind::Daily => {
            let (h, m) =
                parse_hh_mm(text).ok_or_else(|| wyj_i18n::tr("schedule.error.bad_time_format"))?;
            Ok(wyj_store::cron_sync::frequency_to_cron(
                &wyj_store::cron_sync::Frequency::Daily { hour: h, minute: m },
            ))
        }
        ScheduleFrequencyKind::Hourly => {
            let m: u32 = text
                .parse()
                .map_err(|_| wyj_i18n::tr("schedule.error.bad_minute_format"))?;
            if m > 59 {
                return Err(wyj_i18n::tr("schedule.error.bad_minute_format"));
            }
            Ok(wyj_store::cron_sync::frequency_to_cron(
                &wyj_store::cron_sync::Frequency::Hourly { minute: m },
            ))
        }
        ScheduleFrequencyKind::Weekly => {
            let mut parts = text.splitn(2, char::is_whitespace);
            let day = parts.next().unwrap_or_default();
            let time = parts.next().unwrap_or_default().trim();
            let weekday: u32 = day
                .parse()
                .map_err(|_| wyj_i18n::tr("schedule.error.bad_weekly_format"))?;
            if weekday > 6 {
                return Err(wyj_i18n::tr("schedule.error.bad_weekly_format"));
            }
            let (h, m) = parse_hh_mm(time)
                .ok_or_else(|| wyj_i18n::tr("schedule.error.bad_weekly_format"))?;
            Ok(wyj_store::cron_sync::frequency_to_cron(
                &wyj_store::cron_sync::Frequency::Weekly {
                    weekday,
                    hour: h,
                    minute: m,
                },
            ))
        }
        ScheduleFrequencyKind::Custom => {
            wyj_store::cron_sync::validate_cron(text).map_err(|e| e.to_string())?;
            Ok(text.to_string())
        }
    }
}

/// Ctrl+S 与"保存并关闭"菜单选项共用的保存逻辑：校验 → 整份写盘 → 同步系统
/// crontab。与 `profile_try_save` 不同，这里不需要重建 Agent（定时任务不影响
/// 当前会话的模型/工具配置）。返回 `true` 表示任务数据已成功落盘（即使随后的
/// crontab 同步失败，也视为"保存成功"——数据本身没有丢，调用方据此决定是否
/// 关闭面板：crontab 同步失败时会转入 `ScheduleOverlay::SyncError` 停留在面板上）。
fn schedule_try_save(state: &mut AppState) -> bool {
    let mut saved = false;
    if let Some(dialog) = &mut state.schedule_dialog {
        if let Some((bad_idx, msg)) = dialog.validate() {
            dialog.expanded = Some(bad_idx);
            dialog.clamp_cursor();
            dialog.error = Some(msg);
        } else {
            let manifest = wyj_store::schedule::ScheduleManifest {
                version: wyj_store::schedule::SCHEDULE_VERSION,
                tasks: dialog.tasks.clone(),
            };
            match wyj_store::schedule::save(&manifest) {
                Ok(()) => {
                    saved = true;
                    dialog.saved_snapshot = dialog.tasks.clone();
                    dialog.error = None;
                    if let Err(e) = wyj_store::cron_sync::sync_crontab(
                        &manifest.tasks,
                        &wyj_config::StorageRetentionCfg::default(),
                    ) {
                        dialog.overlay = ScheduleOverlay::SyncError {
                            message: e.to_string(),
                        };
                    }
                }
                Err(e) => {
                    dialog.error = Some(wyj_i18n::tr_fmt(
                        "schedule.error.save_failed",
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
        // 项目级来源只剩 `<git-root>/.wyj-code/mcp.toml` 一个（v1.5.15 起不再
        // 读取 `<cwd>/.mcp.json`），所以 Project scope 直接由 `load_project_mcp`
        // 判定。
        let project_names: std::collections::HashSet<String> = wyj_config::load_project_mcp(cwd)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();

        let installed = merged
            .into_iter()
            .filter_map(|config| {
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
                let enabled = managed.as_ref().map(|m| m.enabled).unwrap_or(true);
                // 隐藏"已禁用 + 非真正受管理"的条目：受管理的 server
                // （`is_managed()`）走正常禁用/启用流程要能在列表里找回，
                // 不能被一并隐藏。
                let is_managed = managed.as_ref().is_some_and(|m| m.is_managed());
                if !enabled && !is_managed {
                    return None;
                }
                Some(McpInstalledRow {
                    config,
                    scope,
                    managed,
                })
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

/// Global-scope 的 MCP install/uninstall/upgrade 直接读写磁盘上的
/// `~/.wyj-code/config.toml`（`wyj_store::mcp_install` 内部各自 `Config::load()`
/// 后原地 `save()`），完全绕过 `AppState.config` 这份进程启动时加载、此后只在
/// `/model` `/config` 面板保存时才会更新的内存快照。`McpDialog::new`/
/// `refresh_installed` 却是拿 `&state.config` 当参数构建"已安装"列表——Project
/// scope 那部分没事（`merged_mcp_servers` 内部对项目配置本来就是每次现读盘），
/// 但 Global scope 那部分永远读的是这份过期快照，于是卸载/安装/升级全局 MCP
/// server 在磁盘上明明已经成功，`/mcp` 面板却像没生效一样（条目卸载不掉、装
/// 不上、升级后版本号不刷新），用户体感就是"无法正常卸载"。这里在每次这类
/// 操作成功后把内存快照拉回最新，读盘失败（罕见）时保留旧值，不让偶发 IO
/// 错误打断整个面板。
fn reload_config_into_state(state: &mut AppState) {
    if let Ok(fresh) = Config::load() {
        state.config = fresh;
    }
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
                if result.is_ok() {
                    reload_config_into_state(state);
                }
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
    /// 注册表搜索进行中（吞掉所有按键，等待 `SkillRegistrySearched`）
    Searching,
    /// 已下载技能包、正在等用户确认是否落盘（`RegistryInstallConfirm` 的前置态）
    FetchingArchive,
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
    /// 注册表（SkillHub）来源的技能安装确认。**必须在解压落盘之前**停下来让
    /// 用户过目来源/作者/版本与包内容摘要——技能正文会整段作为模型指令进入
    /// 对话，而包里可能夹带 `scripts/` 可执行内容，属于不可逆的信任动作。
    RegistryInstallConfirm {
        skill: Box<wyj_store::skillhub::SkillHubSkill>,
        preview: wyj_store::skillhub::SkillArchivePreview,
        /// 已下载的 zip 原始字节，确认后直接解压，无需二次下载
        archive: Vec<u8>,
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

/// Browse tab 当前列表的数据来源。同一张表可以由 git marketplace 同步填充，
/// 也可以由 HTTP 技能注册表搜索填充——两者条目类型不同，安装路径也不同，
/// 因此必须记住"当前这份结果是哪儿来的"，不能靠 `browse_results` 非空来猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillsBrowseSource {
    /// 还没有任何来源
    None,
    /// 来自某个 git marketplace（Marketplaces tab 选中"浏览"触发同步）
    Marketplace,
    /// 来自 HTTP 技能注册表（Browse tab 顶部搜索框）
    Registry,
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
        // 目录式技能 `<name>/SKILL.md`（HTTP 注册表安装的形态）与单文件 `<name>.md`
        //（git marketplace 安装的形态）都要收录，否则注册表装来的技能在
        // Installed tab 里完全看不见——装了却看不见是最糟的一种 bug。
        let (skill_file, name) = if path.is_dir() {
            // 目录式技能：命令名是**目录名**，不是 SKILL.md 的文件名
            // （用 file_stem 取会得到 "SKILL"）。
            let candidate = path.join("SKILL.md");
            if !candidate.is_file() {
                continue;
            }
            let Some(name) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            (candidate, name)
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            (path.clone(), name)
        } else {
            continue;
        };
        let description = std::fs::read_to_string(&skill_file)
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
    /// Browse tab 当前的 HTTP 注册表搜索结果。与 `browse_results` 是互斥的两份
    /// 列表：同一时刻只有一份有效，由 `browse_source` 指明是哪一份。
    pub registry_results: Vec<wyj_store::skillhub::SkillHubSkill>,
    /// `browse_results` 的来源，决定选中条目按哪条安装路径处理
    pub browse_source: SkillsBrowseSource,
    /// 正在下载/等待确认的那个注册表技能的 slug。下载期间用户仍可能移动光标，
    /// 确认框回来时要按 slug 回查是哪一条，而不是信任当时的 cursor。
    pub pending_registry_slug: String,
    /// HTTP 技能注册表 API 根。留作后续支持多注册表/自建实例；当前固定用官方
    /// 公共实例，不进 lockfile（与 SkillHub 的部署地址绑死无实际意义）。
    pub registry_base_url: String,
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
        let global_dir = wyj_config::global_config_dir_in(home).join("skills");
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
        let project_dir = wyj_config::project_config_dir(cwd).join("skills");
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
            registry_results: Vec::new(),
            browse_source: SkillsBrowseSource::None,
            pending_registry_slug: String::new(),
            registry_base_url: wyj_store::skillhub::SKILLHUB_BASE_URL.to_string(),
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
    /// tab 头不算进 rows()，单独渲染；Installed 没有 AddNew。
    pub fn rows(&self) -> Vec<FlatRow> {
        match self.tab {
            SkillsDialogTab::Installed => (0..self.installed.len()).map(FlatRow::Entry).collect(),
            SkillsDialogTab::Marketplaces => {
                let mut r: Vec<_> = (0..self.marketplaces.len()).map(FlatRow::Entry).collect();
                r.push(FlatRow::AddNew);
                r
            }
            SkillsDialogTab::Browse => {
                // 搜索框固定占第 0 行（沿用 MCP 面板 Browse 的 AddNew 约定）。
                // `FlatRow::Entry(i)` 携带的是**结果列表下标**而非行号，因此
                // 搜索结果不需要整体偏移 1。
                let count = match self.browse_source {
                    SkillsBrowseSource::Registry => self.registry_results.len(),
                    _ => self.browse_results.len(),
                };
                let mut r = vec![FlatRow::AddNew];
                r.extend((0..count).map(FlatRow::Entry));
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
            // 同一张 Browse 表可能装的是 git marketplace 条目，也可能是 HTTP
            // 注册表搜索结果——两条安装路径完全不同（前者直接拷贝 .md，后者要
            // 下载 zip、过安全校验、解压成目录），按 browse_source 分流。
            let source = state.skills_dialog.as_ref().map(|d| d.browse_source);
            match source {
                Some(SkillsBrowseSource::Registry) => {
                    let skill = state
                        .skills_dialog
                        .as_ref()
                        .and_then(|d| d.registry_results.get(idx).cloned());
                    if let Some(skill) = skill {
                        start_registry_archive_fetch(state, agent_tx, skill);
                    }
                }
                _ => {
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
    }
}

/// 在后台发起一次注册表关键字搜索，结果经 `SkillRegistrySearched` 回到 UI 线程。
///
/// 不在主循环里 `block_on`：搜索是一次网络往返，卡住事件循环会让整个 TUI 冻结。
fn start_registry_search(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    query: String,
) {
    let base_url = state
        .skills_dialog
        .as_ref()
        .map(|d| d.registry_base_url.clone())
        .unwrap_or_else(|| wyj_store::skillhub::SKILLHUB_BASE_URL.to_string());
    if let Some(dialog) = &mut state.skills_dialog {
        dialog.overlay = SkillsOverlay::Searching;
        dialog.error = None;
        dialog.status = None;
    }
    let tx = agent_tx.clone();
    tokio::spawn(async move {
        let client = wyj_store::skillhub::SkillHubClient::new(base_url);
        let result = client
            .search_default(&query)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AgentEvent::SkillRegistrySearched { result }).await;
    });
}

/// 下载技能包并扫描内容预览，**全程不落盘**。
///
/// 刻意把「下载」和「安装」拆成两步：确认框要展示来源/作者/版本与包里到底有
/// 什么（尤其 `scripts/`、`hooks/` 下的可执行内容），所以必须先把包拉到手上看
/// 一眼，再由用户决定是否解压进 skills 目录。
fn start_registry_archive_fetch(
    state: &mut AppState,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    skill: wyj_store::skillhub::SkillHubSkill,
) {
    let base_url = state
        .skills_dialog
        .as_ref()
        .map(|d| d.registry_base_url.clone())
        .unwrap_or_else(|| wyj_store::skillhub::SKILLHUB_BASE_URL.to_string());
    if let Some(dialog) = &mut state.skills_dialog {
        dialog.overlay = SkillsOverlay::FetchingArchive;
        dialog.error = None;
        dialog.pending_registry_slug = skill.slug.clone();
    }
    let tx = agent_tx.clone();
    tokio::spawn(async move {
        let client = wyj_store::skillhub::SkillHubClient::new(base_url);
        let result = async {
            let archive = client.download(&skill.slug, &skill.namespace).await?;
            let preview = wyj_store::skillhub::preview_archive(&archive)?;
            anyhow::Ok((preview, archive))
        }
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = tx
            .send(AgentEvent::SkillRegistryArchiveFetched { result })
            .await;
    });
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

/// 一条待发送的用户消息。
///
/// `display` 必须在**输入框还存在的那一刻**算好：附件占位符的位置只存在于
/// 输入框内部的原子标记里，`InputBox::text()` 会把它们过滤掉，一旦离开
/// 输入框就再也拼不出与输入框一致的展示文本。因此凡是消息要排队/等确认
/// 的路径（`pending_queue`、`ExecModeConfirmDialog`）都必须把它一起带着走，
/// 不能等到消费时再从 `text + attachments` 现拼。
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// 发给模型的正文（不含任何展示用占位符）
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// 聊天区展示文本，附件占位符已按输入框里的位置展开
    pub display: String,
}

impl OutgoingMessage {
    /// 无附件的消息（测试与内部注入路径用），展示文本即正文。
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            display: text.clone(),
            text,
            attachments: Vec::new(),
        }
    }
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

    /// MCP/JSON 输出的首行往往是无信息量的 `{`：兜底摘要必须跳过结构性行取
    /// 第一条有内容的行。
    #[test]
    fn fallback_summary_skips_structural_first_line() {
        let json = "{\n  \"code\": 0,\n  \"data\": []\n}\n";
        assert_eq!(
            tool_result_summary("mcp__a-stock__get_quote", json, false),
            "\"code\": 0,"
        );
        // 纯结构输出（不含任何字母数字）回退到首行。
        let braces = "{\n}\n";
        assert_eq!(tool_result_summary("SomeUnknownTool", braces, false), "{");
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
    fn only_shift_up_is_the_explicit_content_focus_shortcut() {
        assert!(is_content_focus_key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::SHIFT,
        )));
        assert!(!is_content_focus_key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )));
        assert!(!is_content_focus_key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::SHIFT,
        )));
        assert!(!is_content_focus_key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::CONTROL,
        )));
    }

    fn todo(id: &str, status: TodoStatus) -> wyj_tools::todo::TodoItem {
        wyj_tools::todo::TodoItem {
            id: id.to_string(),
            content: format!("todo-{id}"),
            status,
            priority: None,
            active_form: None,
        }
    }

    fn sub_agent(status: SubAgentStatus) -> SubAgentUiState {
        SubAgentUiState {
            agent_type: "general-purpose".to_string(),
            description: "test".to_string(),
            background: false,
            status,
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
        }
    }

    #[test]
    fn ordinary_arrows_round_trip_input_history_and_restore_draft() {
        let mut state = make_state();
        state.input_history = vec!["first".to_string(), "second".to_string()];
        state.pending_attachments.push(Attachment::File {
            path: PathBuf::from("/tmp/context.txt"),
        });
        let mut input = InputBox::new();
        input.set_text("unsent draft");

        assert!(state.navigate_input_or_history(&mut input, -1));
        assert_eq!(input.display_lines(), &["second"]);
        assert_eq!(state.history_idx, Some(1));

        assert!(state.navigate_input_or_history(&mut input, -1));
        assert_eq!(input.display_lines(), &["first"]);
        assert_eq!(state.history_idx, Some(0));

        assert!(state.navigate_input_or_history(&mut input, 1));
        assert_eq!(input.display_lines(), &["second"]);

        assert!(state.navigate_input_or_history(&mut input, 1));
        assert_eq!(input.display_lines(), &["unsent draft"]);
        assert_eq!(state.history_idx, None);
        assert_eq!(state.pending_attachments.len(), 1);
    }

    #[test]
    fn multiline_input_moves_cursor_before_crossing_into_history() {
        let mut state = make_state();
        state.input_history = vec!["older".to_string()];
        let mut input = InputBox::new();
        input.set_text("line one\nline two");
        assert_eq!(input.cursor_row, 1);

        assert!(state.navigate_input_or_history(&mut input, -1));
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.display_lines(), &["line one", "line two"]);
        assert_eq!(state.history_idx, None);

        assert!(state.navigate_input_or_history(&mut input, -1));
        assert_eq!(input.display_lines(), &["older"]);
        assert_eq!(state.history_idx, Some(0));

        assert!(state.navigate_input_or_history(&mut input, 1));
        assert_eq!(input.display_lines(), &["line one", "line two"]);
        assert_eq!(input.cursor_row, 1);
    }

    #[test]
    fn history_navigation_remains_available_while_agent_is_running() {
        let mut state = make_state();
        state.is_thinking = true;
        state.input_history = vec!["reusable prompt".to_string()];
        let mut input = InputBox::new();

        assert!(state.navigate_input_or_history(&mut input, -1));
        assert_eq!(input.display_lines(), &["reusable prompt"]);
    }

    #[test]
    fn shift_up_content_entry_prefers_running_subagents_then_todos_then_chat() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::user("message".to_string()));
        state.current_todos = Some(vec![todo("todo", TodoStatus::InProgress)]);
        state
            .sub_agents
            .insert(7, sub_agent(SubAgentStatus::Running));

        assert!(state.enter_content_focus());
        assert_eq!(state.ui_focus, UiFocus::SubAgents);
        assert_eq!(state.selected_sub_agent, Some(7));

        assert!(state.leave_content_focus());
        state.sub_agents.get_mut(&7).unwrap().status = SubAgentStatus::Done;
        assert!(state.enter_content_focus());
        assert_eq!(state.ui_focus, UiFocus::Todos);
        assert_eq!(state.selected_todo_id.as_deref(), Some("todo"));
        assert!(state.todo_panel_expanded);

        assert!(state.leave_content_focus());
        state.current_todos = None;
        state.chat_scroll = 4;
        state.chat_max_scroll = 4;
        assert!(state.enter_content_focus());
        assert_eq!(state.ui_focus, UiFocus::Chat);
        assert_eq!(state.chat_scroll, 4);
        assert!(state.chat_follow_tail);
    }

    #[test]
    fn escape_from_content_focus_returns_to_composer_without_touching_draft() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::assistant("answer".to_string()));
        let mut input = InputBox::new();
        input.set_text("keep this draft");

        assert!(state.enter_content_focus());
        assert_eq!(state.ui_focus, UiFocus::Chat);
        assert!(state.close_panel_focus());
        assert_eq!(state.ui_focus, UiFocus::Composer);
        assert_eq!(input.display_lines(), &["keep this draft"]);
    }

    #[test]
    fn repeated_direction_keys_scroll_chat_by_visual_line() {
        let mut state = make_state();
        state.ui_focus = UiFocus::Chat;
        state.chat_scroll = 3;
        state.chat_max_scroll = 5;
        state.chat_follow_tail = false;

        state.move_focus_selection(-1);
        assert_eq!(state.chat_scroll, 2);

        state.move_focus_selection(1);
        assert_eq!(state.chat_scroll, 3);

        state.move_focus_selection(1);
        assert_eq!(state.chat_scroll, 4);
    }

    #[test]
    fn mouse_scroll_moves_chat_view_and_restores_follow_tail_at_bottom() {
        let mut state = make_state();
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;
        state.chat_follow_tail = true;

        state.scroll_chat_lines(-3);
        assert_eq!(state.chat_scroll, 7);
        assert!(!state.chat_follow_tail);

        state.unseen_messages = true;
        state.scroll_chat_lines(50);
        assert_eq!(state.chat_scroll, 20);
        assert!(state.chat_follow_tail);
        assert!(!state.unseen_messages);
    }

    #[test]
    fn scroll_focus_routes_directly_to_chat_lines() {
        let mut state = make_state();
        state.ui_focus = UiFocus::Chat;
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.chat_scroll, 13);
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
    fn content_navigation_crosses_chat_todos_and_subagents_at_boundaries() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::user("first message".to_string()));
        state
            .messages
            .push(ChatMessage::assistant("last message".to_string()));
        state.current_todos = Some(vec![
            todo("a", TodoStatus::Pending),
            todo("b", TodoStatus::Pending),
        ]);
        // 子 Agent 必须是 Running：面板只列运行中的条目（`visible_sub_agents`
        // 是唯一口径），已完成的条目导航根本走不到。本测试要验的是跨区域导航
        // 链条本身，与条目状态无关。
        state
            .sub_agents
            .insert(1, sub_agent(SubAgentStatus::Running));
        state
            .sub_agents
            .insert(2, sub_agent(SubAgentStatus::Running));
        state.chat_scroll = 6;
        state.chat_max_scroll = 6;

        assert!(state.focus_chat_edge(false));
        assert_eq!(state.chat_scroll, 6);
        assert!(state.chat_follow_tail);

        state.move_focus_selection(1);
        assert_eq!(state.ui_focus, UiFocus::Todos);
        assert_eq!(state.selected_todo_id.as_deref(), Some("a"));

        state.move_focus_selection(1);
        assert_eq!(state.selected_todo_id.as_deref(), Some("b"));
        state.move_focus_selection(1);
        assert_eq!(state.ui_focus, UiFocus::SubAgents);
        assert_eq!(state.selected_sub_agent, Some(1));

        state.move_focus_selection(-1);
        assert_eq!(state.ui_focus, UiFocus::Todos);
        assert_eq!(state.selected_todo_id.as_deref(), Some("b"));
        state.move_focus_selection(-1);
        assert_eq!(state.selected_todo_id.as_deref(), Some("a"));
        state.move_focus_selection(-1);
        assert_eq!(state.ui_focus, UiFocus::Chat);
        assert_eq!(state.chat_scroll, 6);
        assert!(state.chat_follow_tail);
    }

    #[test]
    fn todo_detail_is_a_nested_focus_with_independent_navigation_and_escape() {
        let mut state = make_state();
        state.current_todos = Some(vec![todo("a", TodoStatus::InProgress)]);

        assert!(state.enter_content_focus());
        assert_eq!(state.ui_focus, UiFocus::Todos);
        state.toggle_focused_content_detail();
        assert_eq!(state.ui_focus, UiFocus::TodoDetail);
        assert!(state.todo_detail_open);
        assert!(state.chat_follow_tail);

        state.todo_detail_max_scroll = 20;
        state.move_focus_selection(1);
        assert_eq!(state.todo_detail_scroll, 1);
        state.scroll_focus_lines(8);
        assert_eq!(state.todo_detail_scroll, 9);

        assert!(state.close_panel_focus());
        assert_eq!(state.ui_focus, UiFocus::Todos);
        assert_eq!(state.selected_todo_id.as_deref(), Some("a"));
        assert!(!state.todo_detail_open);

        assert!(state.close_panel_focus());
        assert_eq!(state.ui_focus, UiFocus::Composer);
        assert_eq!(state.selected_todo_id, None);
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
    fn todo_detail_scroll_does_not_leak_to_chat_when_content_fits_viewport() {
        let mut state = make_state();
        state.ui_focus = UiFocus::TodoDetail;
        state.todo_detail_open = true;
        // 详情是独立焦点；即便内容不足一屏，也不能把滚动泄漏给聊天区。
        state.todo_detail_scroll = 0;
        state.todo_detail_max_scroll = 0;
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.todo_detail_scroll, 0);
        assert_eq!(state.chat_scroll, 10);
    }

    #[test]
    fn todo_detail_scroll_stays_internal_when_room_remains() {
        let mut state = make_state();
        state.ui_focus = UiFocus::TodoDetail;
        state.todo_detail_open = true;
        state.todo_detail_scroll = 5;
        state.todo_detail_max_scroll = 20;
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.todo_detail_scroll, 8);
        assert_eq!(state.chat_scroll, 10);
    }

    #[test]
    fn todo_detail_live_updates_follow_only_when_reader_was_at_tail() {
        let mut state = make_state();
        state.current_todos = Some(vec![todo("a", TodoStatus::InProgress)]);
        state.selected_todo_id = Some("a".to_string());
        state.ui_focus = UiFocus::TodoDetail;
        state.todo_detail_open = true;
        state.todo_detail_scroll = 4;
        state.todo_detail_max_scroll = 4;

        state.push_tracked_message(ChatMessage::assistant("new tail event".to_string()));
        assert_eq!(state.todo_detail_scroll, u16::MAX);

        state.todo_detail_scroll = 2;
        state.todo_detail_max_scroll = 4;
        state.push_tracked_message(ChatMessage::assistant("another event".to_string()));
        assert_eq!(state.todo_detail_scroll, 2);
    }

    #[test]
    fn sub_agent_detail_scroll_falls_back_to_chat_when_content_fits_viewport() {
        let mut state = make_state();
        state.ui_focus = UiFocus::SubAgents;
        state.sub_agent_detail_open = true;
        state.sub_agent_detail_scroll = 0;
        state.sub_agent_detail_max_scroll = 0;
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.sub_agent_detail_scroll, 0);
        assert_eq!(state.chat_scroll, 13);
    }

    #[test]
    fn sub_agent_detail_scroll_stays_internal_when_room_remains() {
        let mut state = make_state();
        state.ui_focus = UiFocus::SubAgents;
        state.sub_agent_detail_open = true;
        state.sub_agent_detail_scroll = 5;
        state.sub_agent_detail_max_scroll = 20;
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.sub_agent_detail_scroll, 8);
        assert_eq!(state.chat_scroll, 10);
    }

    #[test]
    fn explicit_conversation_jump_moves_to_top_and_bottom() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::thinking("a\nb\nc".to_string(), None));
        state.chat_scroll = 7;
        state.chat_max_scroll = 12;

        state.select_conversation_start();
        assert_eq!(state.chat_scroll, 0);
        assert!(!state.chat_follow_tail);

        state.select_conversation_end();
        assert_eq!(state.chat_scroll, 12);
        assert!(state.chat_follow_tail);
    }

    #[test]
    fn new_chat_content_preserves_upscrolled_reader_and_marks_unseen() {
        let mut state = make_state();
        state.chat_scroll = 4;
        state.chat_max_scroll = 10;
        state.chat_follow_tail = false;

        state.push_message(ChatMessage::assistant("new content".to_string()));

        assert_eq!(state.chat_scroll, 4);
        assert!(!state.chat_follow_tail);
        assert!(state.unseen_messages);
    }

    #[test]
    fn new_chat_content_keeps_tail_following_when_already_at_bottom() {
        let mut state = make_state();
        state.chat_scroll = 10;
        state.chat_max_scroll = 10;
        state.chat_follow_tail = true;

        state.push_message(ChatMessage::assistant("new content".to_string()));

        assert!(state.chat_follow_tail);
        assert!(!state.unseen_messages);
    }

    #[test]
    fn streaming_delta_marks_unseen_without_changing_scroll() {
        let mut state = make_state();
        state.chat_scroll = 4;
        state.chat_max_scroll = 10;
        state.chat_follow_tail = false;

        state.apply_agent_event(AgentEvent::TextDelta("token".to_string()));

        assert_eq!(state.chat_scroll, 4);
        assert!(state.unseen_messages);
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
        state.compact_count = 17;
        state.turns = 2;
        state
            .messages
            .push(ChatMessage::assistant("old".to_string()));
        state.pending_queue.push(OutgoingMessage::plain("queued"));

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
        assert_eq!(state.compact_count, 0);
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

#[derive(Debug)]
enum ClipboardPaste {
    Image {
        data: String,
        width: usize,
        height: usize,
    },
    Text(String),
}

/// 直接读取系统剪贴板。
///
/// 终端的 bracketed paste 只会为文本产生 `Event::Paste`；纯图片剪贴板不会触发
/// 该事件，因此 Ctrl+V 必须由应用主动读取剪贴板。图片优先，和原有
/// `Event::Paste` 路径保持一致。
fn read_system_clipboard() -> Option<ClipboardPaste> {
    let mut clipboard = arboard::Clipboard::new().ok()?;

    if let Ok(image) = clipboard.get_image() {
        let png_bytes =
            encode_rgba_to_png(&image.bytes, image.width as u32, image.height as u32).ok()?;
        use base64::Engine as _;
        return Some(ClipboardPaste::Image {
            data: base64::engine::general_purpose::STANDARD.encode(png_bytes),
            width: image.width,
            height: image.height,
        });
    }

    clipboard
        .get_text()
        .ok()
        .filter(|text| !text.is_empty())
        .map(ClipboardPaste::Text)
}

/// bracketed paste 已经携带可靠的文本内容；这里只额外探测同一剪贴板是否为图片。
fn read_clipboard_image() -> Option<ClipboardPaste> {
    match read_system_clipboard() {
        image @ Some(ClipboardPaste::Image { .. }) => image,
        Some(ClipboardPaste::Text(_)) | None => None,
    }
}

fn read_clipboard_text() -> Option<String> {
    // 测试期可注入：直接读真实剪贴板的测试在开发机上会随机失败（CI 空剪贴板
    // 过、本机有内容就红），这种「本地红 CI 绿」的测试比稳定失败更糟——没人会
    // 再信任它。见 `with_clipboard_text_override`。
    #[cfg(test)]
    if let Some(forced) = TEST_CLIPBOARD_TEXT.with(|slot| slot.borrow().clone()) {
        return forced;
    }
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok().filter(|text| !text.is_empty())
}

// `None` = 未注入（读真实剪贴板）；`Some(x)` = 钉死为 `x`（`Some(None)`
// 即"剪贴板为空"）。外层 Option 用来区分这两者。
#[cfg(test)]
thread_local! {
    static TEST_CLIPBOARD_TEXT: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// 在 `f` 执行期间把剪贴板内容钉死为 `text`（`None` 表示剪贴板为空）。
#[cfg(test)]
fn with_clipboard_text_override<R>(text: Option<&str>, f: impl FnOnce() -> R) -> R {
    TEST_CLIPBOARD_TEXT.with(|slot| *slot.borrow_mut() = Some(text.map(str::to_owned)));
    let out = f();
    TEST_CLIPBOARD_TEXT.with(|slot| *slot.borrow_mut() = None);
    out
}

fn is_clipboard_paste_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
}

fn is_content_focus_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Up && key.modifiers == KeyModifiers::SHIFT
}

/// 单行文字直接留在输入框即可；只有真正包含换行的批量粘贴才需要瞬时提示。
fn pasted_text_needs_hint(text: &str) -> bool {
    text.contains('\n')
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

fn apply_clipboard_paste(
    state: &mut AppState,
    input: &mut InputBox,
    paste: ClipboardPaste,
) -> Option<PasteHint> {
    let expires_at = Instant::now() + PASTE_HINT_DURATION;
    match paste {
        ClipboardPaste::Image {
            data,
            width,
            height,
        } => {
            let already_attached = state.pending_attachments.iter().any(|attachment| {
                matches!(attachment, Attachment::Image { data: existing, .. } if existing == &data)
            });
            if !already_attached {
                input.reconcile_attachment_markers(state.pending_attachments.len());
                let insertion_index = input.attachment_insertion_index();
                state.pending_attachments.insert(
                    insertion_index,
                    Attachment::Image {
                        media_type: "image/png".to_string(),
                        data,
                        preview_label: format!("{width}×{height}"),
                    },
                );
                input.insert_attachment_marker();
            }
            // 图片会以持久的 `[Image #n]` 占位显示在输入框里，不再叠加瞬时提示。
            None
        }
        ClipboardPaste::Text(pasted) => {
            if let Some(path) = try_resolve_path(pasted.trim()) {
                let already_attached = state.pending_attachments.iter().any(
                    |attachment| matches!(attachment, Attachment::File { path: existing } if existing == &path),
                );
                if !already_attached {
                    input.reconcile_attachment_markers(state.pending_attachments.len());
                    let insertion_index = input.attachment_insertion_index();
                    state
                        .pending_attachments
                        .insert(insertion_index, Attachment::File { path });
                    input.insert_attachment_marker();
                }
                // 文件也由输入框内的持久占位符确认，不需要瞬时提示。
                None
            } else {
                input.insert_text(&pasted);
                pasted_text_needs_hint(&pasted).then(|| PasteHint {
                    text: format!(
                        "[{}]",
                        wyj_i18n::tr_fmt(
                            "input.paste_text",
                            &[("count", &pasted.chars().count().to_string())]
                        )
                    ),
                    expires_at,
                    cursor_row: input.cursor_row,
                    cursor_col: input.cursor_col,
                })
            }
        }
    }
}

fn composer_has_content(state: &AppState, input: &InputBox) -> bool {
    !input.is_empty() || !state.pending_attachments.is_empty()
}

/// 删除输入框光标左侧的内容。若紧邻光标的是附件原子，则同步删除对应结构化附件；
/// 否则保持普通文本 Backspace 语义。
fn backspace_composer(input: &mut InputBox, attachments: &mut Vec<Attachment>) {
    input.reconcile_attachment_markers(attachments.len());
    if let Some(index) = input.attachment_marker_before_cursor() {
        input.backspace();
        attachments.remove(index);
        return;
    }
    input.backspace();
}

fn delete_composer_forward(input: &mut InputBox, attachments: &mut Vec<Attachment>) {
    input.reconcile_attachment_markers(attachments.len());
    if let Some(index) = input.attachment_marker_at_cursor() {
        input.delete_char_forward();
        attachments.remove(index);
        return;
    }
    input.delete_char_forward();
}

fn delete_word_backward_composer(input: &mut InputBox, attachments: &mut Vec<Attachment>) {
    input.reconcile_attachment_markers(attachments.len());
    if input.attachment_marker_before_cursor().is_some() {
        backspace_composer(input, attachments);
    } else {
        input.delete_word_backward();
    }
}

fn clear_composer(state: &mut AppState, input: &mut InputBox) {
    *input = InputBox::new();
    state.pending_attachments.clear();
    state.paste_hint = None;
    state.slash_completions.clear();
    state.file_completions.clear();
    state.history_idx = None;
    state.history_draft = None;
}

#[cfg(test)]
mod clipboard_paste_tests {
    use super::*;
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

    /// 主循环取 session 锁必须**永不阻塞**。
    ///
    /// 回合 task（`spawn_agent_turn` / 两处内联 turn task）会持锁跑完**整个**
    /// agent 回合——LLM 往返加全部工具调用，动辄几分钟。主循环一旦在这把锁上
    /// `.await`，界面就停在那一帧：不重绘、不读按键，连排在保存分支上游的
    /// `abort_stalled_turn()` 看门狗都跑不到下一轮，ESC/Ctrl+C 也失效。
    /// 超时兜底：哪天有人把它改回 `lock().await`，这条会挂在 timeout 上失败，
    /// 而不是静默通过。
    #[tokio::test]
    async fn try_session_lock_never_waits_for_a_running_turn() {
        let session = Arc::new(Mutex::new(Session::new()));
        let holder = session.clone();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = holder.lock().await; // 模拟回合 task 持锁
            let _ = release_rx.await;
        });
        tokio::task::yield_now().await; // 让 holder 先跑到持锁

        let unavailable = tokio::time::timeout(Duration::from_millis(200), async {
            try_session_lock(&session).is_none()
        })
        .await
        .expect("try_session_lock 不得阻塞等待回合释放锁");
        assert!(unavailable, "锁被回合持有时必须立刻返回 None");

        let _ = release_tx.send(());
    }

    /// 排队消息必须在**提交那一刻**就出现在聊天流里，且只在 `AgentEvent::Injected`
    ///（Agent 真正排空时）出队、不再画第二遍。
    ///
    /// 回显原本挂在 `AgentEvent::Injected` 上，而排空点在 Agent 的工具轮边界——
    /// 前台子 Agent / 长 Bash 期间可能几十分钟不排空，用户那边看就是「我输入的
    /// 字不见了」。
    #[test]
    fn queued_message_is_echoed_on_submit_and_not_duplicated_on_drain() {
        let mut state = make_state();
        let before = state.messages.len();

        state.enqueue_message(OutgoingMessage::plain("顺便看一下成交量"));

        assert_eq!(state.pending_queue.len(), 1);
        assert_eq!(
            state.messages.len(),
            before + 1,
            "入队当场就要回显，不能等 Agent 排空"
        );
        assert_eq!(state.messages.last().unwrap().content, "顺便看一下成交量");

        // Agent 排空：只出队，不再回显（否则同一条消息显示两遍）
        state.apply_agent_event(AgentEvent::Injected);
        assert!(state.pending_queue.is_empty());
        assert_eq!(state.messages.len(), before + 1, "排空时不得重复回显");
    }

    /// 前台子 Agent 取走排队消息时**不会**发 `Injected`（它没有 UI 事件通道），
    /// 因此镜像会停在旧水位；回合一结束，兜底重发就会把这条已经投递给子 Agent
    /// 的消息**再当一条全新用户消息发一遍**——同一条要求子 Agent 干一遍、主
    /// Agent 再干一遍，token 双倍。
    ///
    /// 这条钉子守的是共享 steer 队列最容易踩的坑：只测「Agent 层只投递一次」
    /// （`shared_steer_queue_delivers_to_exactly_one_agent`）是不够的，UI 侧
    /// 的镜像必须跟着真实水位一起走。
    #[test]
    fn subagent_drained_steerage_is_not_resent_as_a_new_user_message() {
        let mut state = make_state();
        let queue = Arc::new(wyj_core::SteerQueue::new());
        state.injector = Some(queue.clone());

        // 用户在子 Agent 运行期间补充两条要求：UI 镜像 + 共享队列各加两条
        state.enqueue_message(OutgoingMessage::plain("先看成交量"));
        queue.push(
            vec![ContentBlock::Text {
                text: "先看成交量".into(),
            }],
            wyj_core::InjectionKind::UserMessage,
        );
        state.enqueue_message(OutgoingMessage::plain("再看资金流"));
        queue.push(
            vec![ContentBlock::Text {
                text: "再看资金流".into(),
            }],
            wyj_core::InjectionKind::UserMessage,
        );
        assert_eq!(state.pending_queue.len(), 2);

        // 子 Agent 在自己的工具轮边界把两条都取走（不发 Injected）
        let taken = queue.drain_user_messages();
        assert_eq!(taken.len(), 2);
        assert_eq!(state.pending_queue.len(), 2, "此时镜像还停在旧水位");

        // 回合结束：对齐后镜像必须清空，兜底重发才不会把它们再发一遍
        state.is_thinking = false;
        state.reconcile_pending_queue();
        assert!(
            state.pending_queue.is_empty(),
            "已被子 Agent 取走的排队消息不得再被重发"
        );
    }

    /// 反向钉子：**确实没被取走**的消息必须留在镜像里，否则真正的竞态兜底
    /// （提交时机晚于本轮最后一次排空）会把用户的输入静默吞掉。
    #[test]
    fn undelivered_steerage_survives_reconciliation_for_the_race_fallback() {
        let mut state = make_state();
        let queue = Arc::new(wyj_core::SteerQueue::new());
        state.injector = Some(queue.clone());

        state.enqueue_message(OutgoingMessage::plain("继续"));
        queue.push(
            vec![ContentBlock::Text {
                text: "继续".into(),
            }],
            wyj_core::InjectionKind::UserMessage,
        );
        // 队列里还有一条后台结果通知（SystemReminder），它从不进 pending_queue
        queue.push(
            vec![ContentBlock::Text {
                text: "后台 agent 完成".into(),
            }],
            wyj_core::InjectionKind::SystemReminder,
        );

        state.reconcile_pending_queue();
        assert_eq!(
            state.pending_queue.len(),
            1,
            "未被取走的用户消息必须保留给兜底重发"
        );
        assert_eq!(state.pending_queue[0].text, "继续");
    }

    #[tokio::test]
    async fn try_session_lock_succeeds_when_no_turn_is_running() {
        let session = Arc::new(Mutex::new(Session::new()));
        let guard = tokio::time::timeout(Duration::from_millis(200), async {
            try_session_lock(&session)
        })
        .await
        .expect("空闲时取锁不应超时");
        assert!(guard.is_some(), "没有回合在跑时必须能直接拿到锁");
    }

    /// 终态事件（Usage / TurnDone / Error）走的是**阻塞式** send（通道容量
    /// 256），而 UI 收到 TurnDone 后会把 `save_needed` 置位、下一帧就去
    /// `session.lock().await` 保存。若 turn task 发事件时**仍持着锁**：
    /// UI 等锁、turn task 等 UI 排空通道 → 环形等待，界面永久冻结，且排在
    /// 保存分支上游的看门狗 `abort_stalled_turn()` 也跑不到。
    /// 三处 turn task 都必须先放锁再发——静态钉子，谁把 `drop(sess)` 删掉或
    /// 挪到发送之后，这条立刻变红。
    #[test]
    fn every_turn_task_releases_the_session_lock_before_terminal_events() {
        const SEND: &str = "tx.send(AgentEvent::TurnDone).await;";
        let source = include_str!("app.rs");
        let lines: Vec<&str> = source.lines().collect();

        let mut checked = 0usize;
        let mut segment_start = 0usize; // 当前 turn task 在源码中的起始行
        for (idx, line) in lines.iter().enumerate() {
            if line.contains("let task = spawn_turn_task(async move {")
                || line.contains("fn spawn_agent_turn(")
            {
                segment_start = idx;
            }
            // 真实发送点：`tx.send(...)` 且整行以 `.await;` 结尾。
            // （按整行匹配，避免把本测试源码里的字符串字面量当成发送点。）
            if !(line.contains(SEND) && line.trim_end().ends_with(".await;")) {
                continue;
            }
            let segment = lines[segment_start..idx].join("\n");
            let usage_at = segment
                .find("let usage = AgentEvent::Usage")
                .unwrap_or_else(|| {
                    panic!("终态 Usage 事件应在锁内先取好快照（免得放锁后再读 sess）")
                });
            let drop_at = segment
                .rfind("drop(sess)")
                .unwrap_or_else(|| panic!("turn task 在发终态事件前没有释放 session 锁"));
            assert!(
                usage_at < drop_at,
                "顺序必须是「锁内取 Usage 快照 → drop(sess) → 发事件」"
            );
            checked += 1;
        }
        assert_eq!(checked, 3, "三处 turn task 都应被这条钉子覆盖");
    }

    /// 主循环碰不到 session 锁时**不能 `continue`**——静态钉子。
    ///
    /// `save_needed` 分支位于主循环的**无条件区段**（不在 `event::poll()`
    /// 的按键处理块里）。这里的 `continue` 跳回的是整个循环，被跳过的正是
    /// 紧随其后的 `ui_ask_rx` 消费与 `event::poll()`：按键、权限确认弹窗、
    /// AskQuestion、计划批准全部停摆。而 `terminal.draw()` 在本行之前，
    /// 画面照常刷新 —— 用户看到的是「界面活着但按键全哑」，ESC/Ctrl+C 也
    /// 救不回来（真正终止回合的是排在 `continue` **上游**的回合心跳看门狗，
    /// 因此现场更像「莫名其妙卡住几分钟后自己恢复」）。
    ///
    /// 触发极其常见：`TurnDone` 置位 `save_needed` 与自动唤醒 / 排队兜底
    /// 的 spawn 发生在同一帧，所以**每一条排队消息触发的自动续跑都会踩中**，
    /// 整个新回合全程失灵。
    ///
    /// 正确的形状是「拿不到锁就只置位回退，流程自然落到下面继续消费输入
    /// 层」——存盘晚一帧没有代价，等锁才有。
    #[test]
    fn autosave_never_continues_past_the_input_layer() {
        let source = include_str!("app.rs");
        let lines: Vec<&str> = source.lines().collect();
        // 按「整行 trim 后正好等于」匹配，避免命中本测试源码里那句字符串
        // 字面量（`include_str!` 把测试自己也包进来了）。
        let start = lines
            .iter()
            .position(|l| l.trim() == "if state.save_needed {")
            .expect("主循环应有 autosave 分支");
        // 分支体内不得出现 `continue`：从 `if state.save_needed {` 起，找到
        // 缩进回落到该层级为止的整个块。
        let indent = lines[start].len() - lines[start].trim_start().len();
        let body = lines[start + 1..]
            .iter()
            .take_while(|l| {
                let ind = l.len() - l.trim_start().len();
                l.trim().is_empty() || ind > indent
            })
            .collect::<Vec<_>>();
        assert!(
            !body.iter().any(|l| l.trim() == "continue;"),
            "autosave 分支不得 `continue`：那会跳过 ui_ask 消费与 event::poll()，\
             整个输入层在回合运行期间全程停摆"
        );
    }

    #[test]
    fn flush_thinking_records_elapsed_and_clears_the_timer() {
        // `thinking_started` 自 ThinkingDelta 首块写入后一直只有写点、零读点，
        // 固化时直接丢弃会让折叠头永远拿不到耗时
        let mut state = make_state();
        state.thinking_buf.push_str("第一段\n第二段");
        state.thinking_started =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1200));

        state.flush_thinking();

        assert!(state.thinking_buf.is_empty());
        assert!(state.thinking_started.is_none());
        let thinking = state
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Thinking)
            .expect("thinking 应已固化为消息");
        assert_eq!(thinking.content, "第一段\n第二段");
        let secs = thinking
            .thinking_elapsed_secs
            .expect("固化时必须把耗时落到消息上");
        assert!((1.1..5.0).contains(&secs), "secs={secs}");
    }

    #[test]
    fn output_expanded_defaults_to_auto_mode_and_resets_with_session() {
        let mut state = make_state();
        assert!(
            !state.output_expanded,
            "默认进自动模式：执行中的步骤自动展开，历史保持折叠"
        );

        state.output_expanded = true;
        state.reset_for_new_session();
        assert!(!state.output_expanded, "新会话回到自动模式");
    }

    #[test]
    fn live_tool_output_only_lands_while_the_call_is_in_flight() {
        let mut state = make_state();
        let chunk = ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "first".to_string(),
        };

        // ToolStart 之前：还没有调用登记，快照无处可挂（也无处可显）
        state.is_thinking = true;
        state.apply_tool_output_chunk(chunk.clone());
        assert!(state.live_tool_out.is_empty(), "未登记的调用不得产生实时块");

        // ToolStart 之后
        state.tool_info.insert("toolu_1".into(), ("Bash".into(), 0));
        state.apply_tool_output_chunk(chunk.clone());
        assert_eq!(
            state.live_tool_out.get("toolu_1").map(String::as_str),
            Some("first")
        );

        // 后续快照原地替换（快照语义：每帧自带完整累计输出）
        state.apply_tool_output_chunk(ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "first\nsecond".to_string(),
        });
        assert_eq!(
            state.live_tool_out.get("toolu_1").map(String::as_str),
            Some("first\nsecond")
        );

        // ToolEnd 收工
        let tail = "first\nsecond".to_string();
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "toolu_1".to_string(),
            output: tail,
            is_error: false,
            elapsed_secs: 0.1,
        });
        assert!(
            state.live_tool_out.is_empty(),
            "调用结束必须摘掉快照，否则哈希表里会一直躺着没人渲染的 8KB"
        );
    }

    #[test]
    fn late_live_output_after_an_interrupted_turn_is_dropped() {
        // ESC 中断 abort 掉回合 future，但 Bash 的 pump 会一直读到管道 EOF，
        // 中断之后还可能推来最后几帧。这些帧若被接收，聊天区会留下一块永远
        // 转圈的幽灵输出。
        let mut state = make_state();
        state.is_thinking = true;
        state.tool_info.insert("toolu_1".into(), ("Bash".into(), 0));

        // 中断：回合结束但 tool_info 未被清理（与真实路径一致）
        state.is_thinking = false;
        state.apply_tool_output_chunk(ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "late".to_string(),
        });
        assert!(
            state.live_tool_out.is_empty(),
            "回合已结束后到达的快照必须丢弃"
        );
    }

    #[test]
    fn live_tool_output_does_not_mark_unseen_messages() {
        // 实时刷新是同一块区域的原地更新。若点亮「未读」，用户上滚翻历史时
        // 会每帧被计一次新消息。
        let mut state = make_state();
        state.is_thinking = true;
        state.tool_info.insert("toolu_1".into(), ("Bash".into(), 0));
        state.chat_follow_tail = false;
        state.unseen_messages = false;

        state.apply_tool_output_chunk(ToolOutputChunk {
            tool_use_id: "toolu_1".to_string(),
            tail: "noise".to_string(),
        });

        assert!(!state.unseen_messages);
    }

    #[test]
    fn ctrl_or_super_v_is_an_explicit_clipboard_paste() {
        assert!(is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
        )));
        assert!(is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
        assert!(is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER,
        )));
        assert!(!is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )));
        assert!(!is_clipboard_paste_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn text_paste_hint_is_only_created_for_multiple_lines() {
        let mut state = make_state();
        let mut input = InputBox::new();

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Text("单行文字".to_string()),
        );
        assert!(hint.is_none());
        assert_eq!(input.display_lines(), &["单行文字"]);

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Text("\n第二行".to_string()),
        );
        assert!(hint.is_some());
        assert_eq!(input.display_lines(), &["单行文字", "第二行"]);
    }

    #[test]
    fn image_paste_creates_an_in_memory_attachment() {
        let mut state = make_state();
        let mut input = InputBox::new();

        let hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );

        assert!(hint.is_none());
        assert!(matches!(
            state.pending_attachments.as_slice(),
            [Attachment::Image {
                media_type,
                data,
                preview_label,
            }] if media_type == "image/png"
                && data == "encoded-png"
                && preview_label == "320×180"
        ));

        let duplicate_hint = apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        assert!(duplicate_hint.is_none());
        assert_eq!(state.pending_attachments.len(), 1);
    }

    #[test]
    fn multiple_distinct_image_pastes_are_kept_in_order() {
        let mut state = make_state();
        let mut input = InputBox::new();

        for (data, width, height) in [("first", 320, 180), ("second", 640, 480)] {
            let hint = apply_clipboard_paste(
                &mut state,
                &mut input,
                ClipboardPaste::Image {
                    data: data.to_string(),
                    width,
                    height,
                },
            );
            assert!(hint.is_none());
        }

        assert!(matches!(
            state.pending_attachments.as_slice(),
            [
                Attachment::Image { data: first, .. },
                Attachment::Image { data: second, .. },
            ] if first == "first" && second == "second"
        ));
    }

    #[test]
    fn image_pasted_before_existing_marker_is_inserted_before_it() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("右侧");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "right".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.move_to_start_of_line();
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "left".to_string(),
                width: 640,
                height: 480,
            },
        );

        assert!(matches!(
            state.pending_attachments.as_slice(),
            [
                Attachment::Image { data: left, .. },
                Attachment::Image { data: right, .. },
            ] if left == "left" && right == "right"
        ));
    }

    #[test]
    fn backspace_deletes_text_then_attachments_from_the_right() {
        let mut input = InputBox::new();
        input.insert_text("x");
        let mut attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "first".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "second".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        backspace_composer(&mut input, &mut attachments);
        assert!(input.is_empty());
        assert_eq!(attachments.len(), 2);

        backspace_composer(&mut input, &mut attachments);
        assert!(matches!(
            attachments.as_slice(),
            [Attachment::Image { data, .. }] if data == "first"
        ));

        backspace_composer(&mut input, &mut attachments);
        assert!(attachments.is_empty());
    }

    #[test]
    fn inline_image_is_deleted_only_when_backspace_reaches_its_position() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.insert_text("后");

        backspace_composer(&mut input, &mut state.pending_attachments);
        assert_eq!(input.text(), "前");
        assert_eq!(state.pending_attachments.len(), 1);

        backspace_composer(&mut input, &mut state.pending_attachments);
        assert_eq!(input.text(), "前");
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn delete_key_removes_an_inline_image_at_the_cursor() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("前");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.move_left();

        delete_composer_forward(&mut input, &mut state.pending_attachments);

        assert_eq!(input.text(), "前");
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn sending_filters_internal_attachment_markers_from_text() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("请看");
        apply_clipboard_paste(
            &mut state,
            &mut input,
            ClipboardPaste::Image {
                data: "encoded-png".to_string(),
                width: 320,
                height: 180,
            },
        );
        input.insert_text("这张图");

        assert_eq!(input.take(), "请看这张图");
        assert_eq!(state.pending_attachments.len(), 1);
    }

    #[test]
    fn clearing_composer_removes_text_and_attachments_together() {
        let mut state = make_state();
        let mut input = InputBox::new();
        input.insert_text("draft");
        state.pending_attachments.push(Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded-png".to_string(),
            preview_label: "320×180".to_string(),
        });

        assert!(composer_has_content(&state, &input));
        clear_composer(&mut state, &mut input);

        assert!(input.is_empty());
        assert!(state.pending_attachments.is_empty());
        assert!(!composer_has_content(&state, &input));
    }

    #[test]
    fn attachment_only_composer_is_sendable() {
        let mut state = make_state();
        let input = InputBox::new();
        state.pending_attachments.push(Attachment::Image {
            media_type: "image/png".to_string(),
            data: "encoded-png".to_string(),
            preview_label: "320×180".to_string(),
        });

        assert!(composer_has_content(&state, &input));
    }

    #[tokio::test]
    async fn multiple_images_are_sent_as_ordered_content_blocks() {
        let attachments = vec![
            Attachment::Image {
                media_type: "image/png".to_string(),
                data: "first".to_string(),
                preview_label: "1×1".to_string(),
            },
            Attachment::Image {
                media_type: "image/jpeg".to_string(),
                data: "second".to_string(),
                preview_label: "2×2".to_string(),
            },
        ];

        let blocks = build_user_blocks("compare".to_string(), attachments).await;

        assert!(matches!(
            blocks.as_slice(),
            [
                ContentBlock::Text { text },
                ContentBlock::Image {
                    media_type: first_type,
                    data: first,
                },
                ContentBlock::Image {
                    media_type: second_type,
                    data: second,
                },
            ] if text == "compare"
                && first_type == "image/png"
                && first == "first"
                && second_type == "image/jpeg"
                && second == "second"
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiFocus {
    Composer,
    Chat,
    Todos,
    TodoDetail,
    AgentsCatalog,
    SubAgents,
    Shells,
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
    /// Ctrl+O 全局覆盖开关。`false`（默认）**不是「全部折叠」，而是「自动模式」**：
    /// 正在执行的步骤（工具实时输出、流式 thinking）默认展开，跑完即收起，历史
    /// 保持干净；`true` 才是把 thinking 块与工具调用结果一次性全部展开，常见于
    /// 一次性通读整段执行过程。
    ///
    /// 按 Ctrl+O 在两者间切换；再按一次回到自动模式——与旧语义（切回「全折叠」）
    /// 的区别是，回自动模式后新执行的步骤会自动展开，而不是永远保持折叠。
    pub output_expanded: bool,
    pub is_thinking: bool,
    pub permission_dialog: Option<PermissionDialog>,
    /// 项目级 MCP server 信任确认：TUI 启动后台连接阶段检测到未信任的
    /// `.wyj-code/mcp.toml`/`.mcp.json` server 时设置，只出现一次（不像
    /// `PermissionDialog` 那样逐工具调用触发，因此不需要 oneshot 回传通道，
    /// 直接在按键处理里调用 `wyj_store::project_trust::approve` 即可）。
    pub pending_mcp_trust: Option<Vec<wyj_config::McpServerConfig>>,
    /// 项目级 skill 的待批准条目（`.wyj-code/skills/` 随 git clone 落地、
    /// 内容由仓库作者控制）。为 `Some` 时这些 skill 未进注册表也不在模型名单里。
    pub pending_skill_trust: Option<Vec<wyj_commands::skill::ProjectSkillInfo>>,
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
    /// 应用内消息流纵向滚动偏移。
    pub chat_scroll: usize,
    /// 当前聊天区可视行数，由渲染层每帧回写。
    pub chat_view_height: usize,
    /// 当前聊天区**可用列数**，由渲染层每帧回写（见
    /// [`render::chat_content_width`]）。`/cost` 这类需要按宽度排版的命令
    /// 从这里取宽度：拿终端原始宽度会算得比实际可渲染的宽，表格被换行拆散。
    pub chat_content_width: usize,
    /// 当前聊天区最大滚动偏移，由渲染层每帧回写。
    pub chat_max_scroll: usize,
    /// 是否跟随最新消息到底部。用户主动上滚后置 false。
    pub chat_follow_tail: bool,
    /// 是否有新消息在用户查看历史时到达。
    pub unseen_messages: bool,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// 本会话自动压缩次数。状态栏用它显示一个**语义**指示（"自动压缩 ×N"）
    /// 而不是上下文占用百分比——压缩已完全自动化，占比对用户没有可操作性；
    /// 而"系统替我丢过历史"这件事必须可见，否则自动管理就成了黑盒。
    /// 精确分解走 `/context`（与压缩阈值同源）。
    pub compact_count: u32,
    pub tool_schema_tokens: u32,
    pub tool_schema_tokens_saved: u32,
    pub cwd: PathBuf,
    /// 最终渲染帧中可点击链接的屏幕坐标。与自定义 backend 共享，用于 OSC 8
    /// 输出以及终端继续上报 Ctrl/Command+点击时的应用侧兜底打开。
    pub(crate) hyperlink_registry: HyperlinkRegistry,
    pub should_quit: bool,
    pub turns: usize,
    /// 当前 spinner 动画帧索引
    pub spinner_frame: usize,
    /// 累计工具调用次数
    pub tool_call_count: usize,
    /// 工具 id → (名称, 序号)，用于将 ToolEnd 与 ToolStart 关联
    pub tool_info: HashMap<String, (String, usize)>,
    /// 正在执行中的工具的实时输出：`tool_use_id` → 当前累计输出的尾部快照。
    ///
    /// 键空间刻意与 `tool_info` 重叠——条目的存在与否就是「这个调用还在飞」的
    /// 唯一判据（渲染层据此与 `tool_info` 求交集后再按 `sequence_no` 挂到
    /// 对应的 ⏺ 行下方）。不留「已完成但内容还没清」的中间态。
    pub live_tool_out: HashMap<String, String>,
    pub model_name: String,
    pub context_window: u32,
    pub mode: AgentMode,
    pub current_task: Option<TurnTask>,
    /// 本轮最后一次收到「有任何动静」的时刻，用于回合心跳看门狗。
    ///
    /// 与 `turn_start_time`（整轮耗时）不同：那个从回合开始一路累加，
    /// 这个**每当 `apply_agent_event` 收到任何事件就重置**。判据是
    /// 「多久没有任何进展」而非「这轮跑了多久」——长回合只要还在吐
    /// delta / 跑工具就不该被判死。
    ///
    /// 为什么需要它（`reap_lost_turn_task` 补不了的洞）：后者只看
    /// `task.is_finished()`，即只兜「task 已死」（panic / abort）。task
    /// **活着但卡住**时该标志恒为 false，看门狗永不触发，而 `is_thinking`
    /// 的清零只认 `TurnDone` / `Error`——用户后续输入全部滞留
    /// `pending_queue`，连兜底分支也被同一标志挡住，表现为「跑着跑着
    /// 停住，之后输入全无响应」。已知的卡死源（LLM 流 stall、工具 hang、
    /// MCP 不回响应）各有各的局部超时，但未知的那部分只能在这里兜底。
    pub last_turn_activity: Instant,
    pub ctrl_c_pressed: bool,
    pub last_ctrl_c: Option<Instant>,
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
    /// 选中任务详情是否展开；正常情况下仅在 `UiFocus::TodoDetail` 时为 true。
    pub todo_detail_open: bool,
    /// 任务详情滚动偏移（预留给长详情，渲染层按需 clamp）
    pub todo_detail_scroll: u16,
    /// 任务详情面板上一帧渲染算出的内部可滚动上限，供 scroll_focus_lines 判断是否已到边界并转发给聊天区
    pub todo_detail_max_scroll: u16,
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
    /// AGENTS.md 记忆面板（/memory 命令触发时 Some）
    pub memory_dialog: Option<MemoryDialog>,
    /// 证据化自进化治理中心（/evolve 命令触发时 Some）
    pub evolution_dialog: Option<EvolutionDialog>,
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
    /// 定时任务面板（/schedule 命令触发时 Some）
    pub schedule_dialog: Option<ScheduleDialog>,
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
    /// 当前步骤（`current_op`）的开始时刻，用于标题栏显示**这一步**的耗时。
    ///
    /// 与 `turn_start_time`（整轮耗时）是两件事：标题栏的耗时贴在某个操作名旁边，
    /// 语义是"这一步跑了多久"，步骤切换时归零；没有工具在跑（纯思考 / 等权限）
    /// 时回落到整轮耗时。
    pub current_op_started: Option<Instant>,
    /// 本轮对话开始时间（用于计算耗时）
    pub turn_start_time: Option<Instant>,
    /// 本轮对话开始时的 input_tokens 快照
    pub turn_start_input_tokens: u32,
    /// 本轮对话开始时的 output_tokens 快照
    pub turn_start_output_tokens: u32,
    /// 本轮**实时**累计的 input tokens：每个 LLM 请求结束时由 `AgentEvent::UsageDelta`
    /// 累加（`with_usage_callback` 逐请求回调）。
    ///
    /// 为什么不能拿 `total_input_tokens - turn_start_input_tokens` 代替：
    /// `AgentEvent::Usage` 只在**整轮回合结束**时才推送一次 session 累计值
    /// （见回合收尾处的 `send(AgentEvent::Usage { input: sess.total_input_tokens, .. })`），
    /// 回合进行中 `total_*` 一直是回合开始时的旧值 —— 拿它做差，标题栏整个回合都显示
    /// `0↑ 0↓`，直到回合结束（那时标题栏早已不再显示）。`UsageDelta` 才是真正的
    /// 实时数据源：它在每个请求的流结束时触发一次。
    pub turn_live_input_tokens: u32,
    /// 本轮实时累计的 output tokens（与 `turn_live_input_tokens` 同源同节奏）
    pub turn_live_output_tokens: u32,
    /// 最近一轮 AI 交互耗时。当前轮运行时由 `turn_start_time` 实时计算，完成后落这里。
    pub last_turn_elapsed_secs: Option<f64>,
    /// 最近一轮 AI 交互输入 token 增量。
    pub last_turn_input_tokens: u32,
    /// 最近一轮 AI 交互输出 token 增量。
    pub last_turn_output_tokens: u32,
    /// 当前运行中 Agent 任务的补充信息注入通道（is_thinking 期间提交的消息走这里）
    pub injector: Option<Arc<wyj_core::SteerQueue>>,
    /// 终止后台任务的异步流程完成后，把结果发回 UI 线程的通道
    /// （`BashSessionManager::kill` 要等 2s 兜底 SIGKILL，不能在同步按键里 await）
    pub kill_tx: Option<mpsc::Sender<AgentEvent>>,
    /// 用户按 ESC 中断过当前回合：暂停后台任务完成后的自动唤醒。
    /// 用户主动打断意味着「先别继续」，后台任务这时完成再自动续跑会显得
    /// 不听话；下一次用户自己发消息时自动恢复。
    pub auto_wake_paused: bool,
    /// 最近一次粘贴的瞬时提示（在输入框光标处显示）
    pub(crate) paste_hint: Option<PasteHint>,
    /// 上一次「上一轮还在运行」提示的时刻。拿不到 session 锁时给用户一句提示，
    /// 但按键会被按住不放，逐次弹消息会把聊天流刷满 —— 3 秒去重窗口。
    pub(crate) last_session_busy_notice: Option<Instant>,
    /// 排队中尚未被 Agent 消费的补充消息（正文 + 附件 + 聊天区展示文本），用于
    /// 状态栏提示计数、消费后回放到对话流、以及轮次已结束但仍有残留时的兜底重发
    pub pending_queue: Vec<OutgoingMessage>,
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
    /// 详情面板上一帧渲染算出的内部可滚动上限，供 scroll_focus_lines 判断是否已到边界并转发给聊天区
    pub sub_agent_detail_max_scroll: u16,
    /// 后台子 Agent 完成时主 Agent 空闲，暂存的 system-reminder，下轮起手注入
    pub pending_bg_reminders: Vec<String>,
    /// 后台 shell 任务（Vec 保持 spawn 顺序，见 ShellUiState 注释）。
    /// 数据源是 `BashSessionManager::global()`，由主循环每帧 `poll_shells()` 同步。
    pub shells: Vec<ShellUiState>,
    /// shell 面板当前选中项（按 id 存而非 index，避免列表变动时错位）
    pub selected_shell: Option<String>,
    /// 选中 shell 是否已展开详情（实时输出）
    pub shell_detail_open: bool,
    /// 详情内容的行级滚动偏移（渲染时按可视行数 clamp 并写回）
    pub shell_detail_scroll: u16,
    /// 详情面板上一帧渲染算出的内部可滚动上限，供 scroll_focus_lines 判断是否已到边界并转发给聊天区
    pub shell_detail_max_scroll: u16,
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
            output_expanded: false,
            is_thinking: false,
            permission_dialog: None,
            pending_mcp_trust: None,
            pending_skill_trust: None,
            ask_question_dialog: None,
            plan_dialog: None,
            exec_mode_confirm: None,
            frozen_up_to: 0,
            welcome_frozen: false,
            chat_scroll: 0,
            chat_view_height: 0,
            chat_content_width: 0,
            chat_max_scroll: 0,
            chat_follow_tail: true,
            unseen_messages: false,
            total_input_tokens: 0,
            total_output_tokens: 0,
            compact_count: 0,
            tool_schema_tokens: 0,
            tool_schema_tokens_saved: 0,
            cwd,
            hyperlink_registry: HyperlinkRegistry::default(),
            should_quit: false,
            turns: 0,
            spinner_frame: 0,
            tool_call_count: 0,
            tool_info: HashMap::new(),
            live_tool_out: HashMap::new(),
            model_name,
            context_window,
            mode,
            current_task: None,
            last_turn_activity: Instant::now(),
            ctrl_c_pressed: false,
            last_ctrl_c: None,
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
            todo_detail_max_scroll: 0,
            todo_stats: HashMap::new(),
            todo_execution_logs: HashMap::new(),
            session_picker: None,
            settings_dialog: None,
            profile_dialog: None,
            memory_dialog: None,
            evolution_dialog: None,
            mcp_dialog: None,
            skills_dialog: None,
            plugins_dialog: None,
            agents_dialog: None,
            extensions_dialog: None,
            schedule_dialog: None,
            save_needed: false,
            config,
            pending_attachments: vec![],
            file_completions: vec![],
            file_selected: 0,
            at_browse_dir: PathBuf::new(),
            current_op: None,
            current_op_started: None,
            turn_start_time: None,
            turn_start_input_tokens: 0,
            turn_start_output_tokens: 0,
            turn_live_input_tokens: 0,
            turn_live_output_tokens: 0,
            last_turn_elapsed_secs: None,
            last_turn_input_tokens: 0,
            last_turn_output_tokens: 0,
            injector: None,
            kill_tx: None,
            auto_wake_paused: false,
            paste_hint: None,
            last_session_busy_notice: None,
            pending_queue: vec![],
            sub_agents: BTreeMap::new(),
            sub_agent_trace_cache: BTreeMap::new(),
            current_session_id: String::new(),
            sessions_dir: None,
            selected_sub_agent: None,
            sub_agent_detail_open: false,
            sub_agent_detail_scroll: 0,
            sub_agent_detail_max_scroll: 0,
            pending_bg_reminders: vec![],
            shells: Vec::new(),
            selected_shell: None,
            shell_detail_open: false,
            shell_detail_scroll: 0,
            shell_detail_max_scroll: 0,
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            hub,
            welcome_tip_idx,
            ui_focus: UiFocus::Composer,
            input_owner: None,
            mcp_connection_status: HashMap::new(),
            hook_runner: None,
        }
    }

    /// 一轮新对话开始时清理「按轮次」的生命周期状态。
    ///
    /// 目前只管 TodoWrite 面板。任务是**某一轮对话**的产物：上一轮的任务列表
    /// 对新一轮没有参考价值，留着只会误导 —— 尤其上一轮全部完成后，面板会以
    /// 折叠态一直挂在聊天流里，直到用户发新消息（`TodoUpdate` 不会再来，
    /// 于是永远不消失）。
    ///
    /// 粒度上刻意区别于 `reset_for_new_session`：那是「新会话 / resume /
    /// /clear」用的整块重置，这里只管每轮都会作废的那部分。
    ///
    /// 下一轮若确实需要任务板，模型重新调一次 TodoWrite 就会重新出现
    /// （`AgentEvent::TodoUpdate` 负责填 `current_todos`）。
    /// 记录本轮计时基准：**所有**发起回合的入口都必须走这里（用户消息、排队
    /// 注入、自动唤醒、slash 命令展开的 prompt、面板"立即运行"等）。
    ///
    /// 抽成单一入口的原因：`turn_start_*` 是 session 累计量的基准，而 session 累计
    /// 只在回合结束时才刷新；少一处复位，标题栏就会把上一轮的 token 差值算进这一轮。
    /// 与 `begin_new_turn` 分开是因为后者不是每轮都调（如 skill 展开的 prompt 路径）。
    fn begin_turn_timing(&mut self) {
        self.turn_start_time = Some(Instant::now());
        self.turn_start_input_tokens = self.total_input_tokens;
        self.turn_start_output_tokens = self.total_output_tokens;
        // 实时计数器归零：它靠 UsageDelta 逐请求累加，跨轮不清零会把上一轮
        // 的开销带进新一轮标题栏（表现为"刚发消息就显示几万 token"）。
        self.turn_live_input_tokens = 0;
        self.turn_live_output_tokens = 0;
    }

    fn begin_new_turn(&mut self) {
        self.current_todos = None;
        self.todo_panel_expanded = false;
        self.selected_todo_id = None;
        self.todo_detail_open = false;
        self.todo_detail_scroll = 0;
        self.todo_detail_max_scroll = 0;
        self.todo_stats.clear();
        self.todo_execution_logs.clear();
        // 用户主动发了消息 = 恢复对自动唤醒的信任（此前可能是 ESC 打断后暂停的）
        self.auto_wake_paused = false;
        // 刻意**不**清 shells：后台任务（dev server / watcher / 长跑构建）的
        // 生命周期跨对话轮次，新一轮对话不代表它们结束了。在这儿清会让正在
        // 运行的任务从面板上凭空消失，用户既看不到输出也按不了 k。
        // 真正作废这些状态的是 `reset_for_new_session`（/clear、新会话、
        // resume 切会话），那里 shell 进程本身也已被 kill_all 收走。
    }

    fn reset_for_new_session(&mut self) {
        self.messages.clear();
        self.next_message_id = 1;
        self.streaming_buf.clear();
        self.thinking_buf.clear();
        self.thinking_started = None;
        self.output_expanded = false;
        self.is_thinking = false;
        self.frozen_up_to = 0;
        // Historical sessions and /clear intentionally freeze the welcome screen;
        // a new session must return to the opening state instead of inheriting it.
        self.welcome_frozen = false;
        self.chat_scroll = 0;
        self.chat_max_scroll = 0;
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.compact_count = 0;
        self.tool_schema_tokens = 0;
        self.tool_schema_tokens_saved = 0;
        self.turns = 0;
        self.tool_call_count = 0;
        self.tool_info.clear();
        self.live_tool_out.clear();
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
        self.current_op_started = None;
        self.turn_start_time = None;
        self.turn_start_input_tokens = 0;
        self.turn_start_output_tokens = 0;
        self.turn_live_input_tokens = 0;
        self.turn_live_output_tokens = 0;
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
        // 会话级重置：后台任务属于「上一段会话的现场」，随会话一起作废
        // （进程本身由退出路径的 kill_all 收走）。与 begin_new_turn 的
        // 「跨轮次保留」是刻意区分的两级粒度。
        self.shells.clear();
        self.selected_shell = None;
        self.shell_detail_open = false;
        self.shell_detail_scroll = 0;
        self.shell_detail_max_scroll = 0;
        self.sub_input_tokens = 0;
        self.sub_output_tokens = 0;
        self.ui_focus = UiFocus::Composer;
        self.input_owner = None;
        // 心跳基准随会话一起重置：新会话第一帧不能继承上一段的静默时长，
        // 否则空闲很久后立刻发消息会被看门狗当帧判死。
        self.last_turn_activity = Instant::now();
    }

    pub fn ensure_message_ids(&mut self) {
        for msg in &mut self.messages {
            if msg.id == 0 {
                msg.id = self.next_message_id;
                self.next_message_id += 1;
            }
        }
    }

    fn focus_chat_edge(&mut self, first: bool) -> bool {
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
        self.sub_agent_detail_scroll = 0;
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

    fn select_conversation_start(&mut self) {
        let _ = self.focus_chat_edge(true);
    }

    fn select_conversation_end(&mut self) {
        let _ = self.focus_chat_edge(false);
    }

    fn scroll_chat_lines(&mut self, delta: i32) {
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

    /// 在 [0, max] 范围内调整 value；若调整前后（均先按 max clamp）数值相同，
    /// 说明这次滚动已经被面板内部边界"吞掉"（含 max == 0 即内容根本不需要滚动
    /// 的情况），返回 false，调用方应把这次滚动量转发给别处（聊天区），而不是
    /// 让滚轮事件消失得无声无息。
    fn adjust_bounded_u16_scroll(value: &mut u16, delta: i32, max: u16) -> bool {
        let before = (*value).min(max);
        *value = before;
        Self::adjust_u16_scroll(value, delta);
        let after = (*value).min(max);
        *value = after;
        before != after
    }

    fn move_focus_selection(&mut self, delta: i32) {
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
                if !Self::adjust_bounded_u16_scroll(
                    &mut self.sub_agent_detail_scroll,
                    delta,
                    self.sub_agent_detail_max_scroll,
                ) {
                    self.scroll_chat_lines(delta);
                }
            }
            // 后台任务详情：与子 Agent 详情同款——滚到边界就把滚动量转发给
            // 聊天区，不让滚轮事件无声消失。
            UiFocus::Shells if self.shell_detail_open => {
                if !Self::adjust_bounded_u16_scroll(
                    &mut self.shell_detail_scroll,
                    delta,
                    self.shell_detail_max_scroll,
                ) {
                    self.scroll_chat_lines(delta);
                }
            }
            UiFocus::Chat => self.scroll_chat_lines(delta),
            UiFocus::Composer => self.scroll_chat_lines(delta),
            UiFocus::Todos | UiFocus::SubAgents | UiFocus::Shells => self.scroll_chat_lines(delta),
        }
    }

    fn recall_older_input(&mut self, input: &mut InputBox) -> bool {
        if self.input_history.is_empty() {
            return false;
        }
        if self.history_idx.is_none() {
            self.history_draft = Some(input.text());
        }
        let next = match self.history_idx {
            None => self.input_history.len() - 1,
            Some(idx) => idx.saturating_sub(1),
        };
        self.history_idx = Some(next);
        input.set_text(&self.input_history[next]);
        true
    }

    fn recall_newer_input(&mut self, input: &mut InputBox) -> bool {
        let Some(idx) = self.history_idx else {
            return false;
        };
        if idx + 1 < self.input_history.len() {
            let next = idx + 1;
            self.history_idx = Some(next);
            input.set_text(&self.input_history[next]);
        } else {
            self.history_idx = None;
            input.set_text(&self.history_draft.take().unwrap_or_default());
        }
        true
    }

    /// 普通 ↑/↓ 永远属于输入框：多行草稿先移动逻辑行，只有到达首/末行后才
    /// 进入输入历史。该逻辑不依赖 `is_thinking`，Agent 工作期间也可复用历史
    /// 并排队发送；附件存放在 `pending_attachments`，不会被历史文字替换影响。
    fn navigate_input_or_history(&mut self, input: &mut InputBox, delta: i32) -> bool {
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

    fn assign_message_id(&mut self, msg: &mut ChatMessage) {
        if msg.id == 0 {
            msg.id = self.next_message_id;
            self.next_message_id += 1;
        }
    }

    fn note_new_chat_content(&mut self) {
        if !self.chat_follow_tail {
            self.unseen_messages = true;
        }
    }

    /// 收下一帧工具实时输出快照。
    ///
    /// **不**调 `note_new_chat_content`：实时刷新是同一块区域的原地更新，不是新
    /// 消息。若在这里点亮「未读」标记，用户上滚翻历史时会每 50ms 被拽一次。
    /// 视觉跟随由聊天区的贴底逻辑负责，它看的是内容高度而不是未读计数。
    pub(crate) fn apply_tool_output_chunk(&mut self, chunk: ToolOutputChunk) {
        // 两道闸门缺一不可：
        // ① `tool_info` 无此 id = 这次调用已经 ToolEnd 配对完毕。ESC 中断会
        //    abort 掉回合 future，但 Bash 的 pump 会一直读到管道 EOF，中断后
        //    仍可能推来最后几帧；不过闸就渲染出一块永远转圈的幽灵输出。
        // ② `is_thinking` = 回合已结束（异常退出也会走这里），同上。
        if !self.is_thinking || !self.tool_info.contains_key(&chunk.tool_use_id) {
            return;
        }
        self.live_tool_out.insert(chunk.tool_use_id, chunk.tail);
    }

    fn push_message(&mut self, mut msg: ChatMessage) -> u64 {
        self.assign_message_id(&mut msg);
        let id = msg.id;
        self.messages.push(msg);
        self.note_new_chat_content();
        id
    }

    fn insert_message_after(&mut self, idx: usize, mut msg: ChatMessage) -> u64 {
        self.assign_message_id(&mut msg);
        let id = msg.id;
        self.messages.insert(idx + 1, msg);
        self.note_new_chat_content();
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

    fn focus_todo_edge(&mut self, first: bool) -> bool {
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
        self.sub_agent_detail_scroll = 0;
        self.ui_focus = UiFocus::Todos;
        self.chat_follow_tail = true;
        self.unseen_messages = false;
        true
    }

    /// 在任务列表内部移动；到达边界时返回 false，交给上层跨区域导航。
    fn move_selected_todo(&mut self, delta: i32) -> bool {
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

    fn enter_todo_detail(&mut self) -> bool {
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

    fn leave_todo_detail(&mut self) -> bool {
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

    fn content_focus_active(&self) -> bool {
        matches!(
            self.ui_focus,
            UiFocus::Chat
                | UiFocus::Todos
                | UiFocus::TodoDetail
                | UiFocus::SubAgents
                | UiFocus::Shells
        )
    }

    /// 从输入框显式进入内容区。优先选择离输入框最近且当前可见的运行中 SubAgent
    /// 面板，其次是内联 Todo，最后是聊天区底部。
    fn enter_content_focus(&mut self) -> bool {
        if self.content_focus_active() {
            return true;
        }

        if self.has_running_sub_agents() {
            if let Some(last_id) = self.sub_agents.keys().next_back().copied() {
                self.selected_todo_id = None;
                self.selected_sub_agent = Some(last_id);
                self.todo_detail_open = false;
                self.sub_agent_detail_scroll = 0;
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
                self.shell_detail_scroll = 0;
                self.ui_focus = UiFocus::Shells;
                return true;
            }
        }

        self.focus_chat_edge(false)
    }

    fn leave_content_focus(&mut self) -> bool {
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
        self.sub_agent_detail_scroll = 0;
        self.selected_shell = None;
        self.shell_detail_open = false;
        self.shell_detail_scroll = 0;
        self.shell_detail_max_scroll = 0;
        true
    }

    fn toggle_focused_content_detail(&mut self) {
        match self.ui_focus {
            UiFocus::Todos => {
                let _ = self.enter_todo_detail();
            }
            UiFocus::TodoDetail => {
                let _ = self.leave_todo_detail();
            }
            UiFocus::SubAgents if self.selected_sub_agent.is_some() => {
                self.sub_agent_detail_open = !self.sub_agent_detail_open;
                self.sub_agent_detail_scroll = 0;
            }
            UiFocus::Shells if self.selected_shell.is_some() => {
                self.shell_detail_open = !self.shell_detail_open;
                self.shell_detail_scroll = 0;
            }
            UiFocus::Composer
            | UiFocus::Chat
            | UiFocus::AgentsCatalog
            | UiFocus::SubAgents
            | UiFocus::Shells => {}
        }
    }

    fn close_panel_focus(&mut self) -> bool {
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
    fn focus_sub_agent_edge(&mut self, first: bool) -> bool {
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
        self.sub_agent_detail_scroll = 0;
        self.ui_focus = UiFocus::SubAgents;
        true
    }

    /// 聚焦后台任务面板的首/末项。`shells` 是 Vec，`first` 取首元素、
    /// 否则取末元素（Vec 保持 spawn 顺序，末项即最新启动的任务）。
    /// 只在**运行中**的条目间取边缘（与 `running_shells` 同一口径）。
    fn focus_shell_edge(&mut self, first: bool) -> bool {
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
        self.sub_agent_detail_scroll = 0;
        self.selected_shell = Some(id);
        self.shell_detail_scroll = 0;
        self.ui_focus = UiFocus::Shells;
        true
    }

    /// 在后台任务列表内部移动；到达边界时返回 false，交给上层跨区域导航。
    /// 只在**运行中**的条目间移动（与 `running_shells` 同一口径）：已完成的
    /// 条目不在列表里，按 ↓ 跳过它只会让用户以为按键失灵。
    fn move_selected_shell(&mut self, delta: i32) -> bool {
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
        self.shell_detail_scroll = 0;
        self.ui_focus = UiFocus::Shells;
        true
    }

    /// 是否有仍在运行的后台 shell（驱动 spinner 与底部面板自动显示）
    pub fn has_running_shells(&self) -> bool {
        self.shells.iter().any(|s| s.status == ShellStatus::Running)
    }

    /// 在 SubAgent 列表内部移动；到达边界时返回 false，交给上层跨区域导航。
    /// 只在**运行中**的条目间移动（与 `visible_sub_agents` 同一口径）。
    fn move_selected_sub_agent(&mut self, delta: i32) -> bool {
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
        self.sub_agent_detail_scroll = 0;
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
            self.shell_detail_scroll = 0;
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
            self.sub_agent_detail_scroll = 0;
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
    fn kill_selected_shell(&mut self) {
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
        // 用户主动打断 = 「先别继续」。后台任务这时刚好完成也不应该自动把
        // Agent 唤醒续跑（那会显得不听话），直到用户自己发一条新消息。
        self.auto_wake_paused = true;
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
        self.reconcile_sub_agent_selection();
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

    /// 回合 task 看门狗：task 已经死了、UI 却还以为它在思考时，补一个终止事件
    /// 把状态收回来。返回 true 表示本帧做了兜底。
    ///
    /// 调用位置硬约束：必须排在主循环 `agent_rx` 排空**之后**。task 正常收尾
    /// 的顺序是「先把 `TurnDone` / `Error` 塞进 channel，再结束自己」，若在这
    /// 里抢在事件被应用之前判 `is_finished()`，就会把正常收尾误判成异常终止。
    ///
    /// 为什么必须有它：panic 的信息写进 alternate screen 下的 stderr，用户
    /// 看不见；没有这个兜底，UI 就永久停在 thinking，用户后续输入全部滞留
    /// `pending_queue`（连 `!is_thinking` 的兜底分支也被同一个标志挡住）。
    pub fn reap_lost_turn_task(&mut self) -> bool {
        if !self.is_thinking {
            return false;
        }
        match &self.current_task {
            Some(task) if task.is_finished() => {}
            _ => return false,
        }
        self.current_task = None;
        self.flush_streaming();
        self.is_thinking = false;
        self.injector = None;
        self.current_op = None;
        self.current_op_started = None;
        let message = wyj_i18n::tr("agent.turn_task_lost");
        self.push_tracked_message(ChatMessage::assistant_err(message.clone()));
        notify_emit(NotificationEvent::TurnError {
            error: truncate_notify_body(&message),
            session_id: if self.current_session_id.is_empty() {
                None
            } else {
                Some(self.current_session_id.clone())
            },
        });
        true
    }

    /// 回合心跳看门狗：task **活着但长时间没有任何进展**时，主动 abort 它。
    ///
    /// 与 [`Self::reap_lost_turn_task`] 的分工：后者只看 `is_finished()`，
    /// 兜的是「task 已死」（panic / 被 abort）；本方法兜的是「task 还活着
    /// 却卡住了」。两者缺一不可——只有后者时，一条 hang 住的 LLM 流或工具
    /// 调用会让 UI 永久停在 thinking，用户后续输入全部滞留
    /// `pending_queue`，连 `!is_thinking && !pending_queue.is_empty()` 的
    /// 兜底分支也被同一个标志挡住。
    ///
    /// 调用位置硬约束：与 `reap_lost_turn_task` 同处，且排在 agent 事件
    /// 排空之后——刚收到的事件不应被本帧立刻判死。
    ///
    /// **刻意不统计「用户正在等交互」的时长**：权限确认 / AskQuestion /
    /// 计划批准弹窗打开期间，round 处于合法阻塞（await oneshot），用户可能
    /// 离开几分钟才回来。这段时间不计入心跳，否则一次正常的「出去倒杯水」
    /// 就会把回合杀掉。判据是「既没有事件、又没有弹窗在等用户」。
    ///
    /// 返回 true 表示本帧做了兜底。
    /// 把一条用户消息排进"回合结束后送达"队列，**并在入队那一刻就回显**。
    ///
    /// 不变式：**每处入队都必须当场 `push_user`**。排空点在 Agent 侧（工具轮
    /// 边界），前台子 Agent / 长 Bash 期间可能几十分钟都不排空；若把回显也
    /// 挂在排空上，用户会看到"我打的字凭空消失"。
    /// 真实投递仍由 Agent 排空时完成，这里只负责让用户立刻看见提交了什么。
    pub(crate) fn enqueue_message(&mut self, message: OutgoingMessage) {
        self.push_user(message.display.clone());
        self.pending_queue.push(message);
    }

    /// 把 `pending_queue`（steer 队列的 UI 侧镜像）裁剪到共享队列的真实水位。
    ///
    /// 必须在 `TurnDone` / `Error` 里、**`injector` 被清空之前**调用——那之后
    /// 就再也拿不到这份队列了。
    ///
    /// 为什么需要：入队时两边各加一条，正常靠 `AgentEvent::Injected` 出队。但
    /// 共享 steer 队列之后**取走者不再唯一**——前台子 Agent 也会
    /// `drain_user_messages()`，而它没有 UI 的事件通道、`on_inject` 是空闭包，
    /// 发不出 `Injected`。镜像于是停在旧水位，回合结束后 11480 的兜底重发会把
    /// **已经投递给子 Agent 的消息再当一条全新用户消息发一遍**：同一条要求子
    /// Agent 干一遍、主 Agent 再干一遍，token 双倍。这直接违反「一条内容只被
    /// 投递一次」这条不变式。
    ///
    /// 对齐用的是队列的真实水位而不是「谁取走的」，因此主 Agent、前台子 Agent、
    /// 嵌套子 Agent 三种取走路径一律正确，无需给每个都补事件通道。
    fn reconcile_pending_queue(&mut self) {
        let Some(queue) = &self.injector else { return };
        let undelivered = queue.pending_user_messages();
        // 只裁剪「镜像比队列多」的情况：队列里的 SystemReminder（后台任务结果
        // 通知）从来不进 pending_queue，计入会让 undelivered 偏大而漏裁。
        if self.pending_queue.len() > undelivered {
            let delivered = self.pending_queue.len() - undelivered;
            self.pending_queue.drain(..delivered);
        }
    }

    /// 主循环碰不到 session 锁（上一轮还在跑）时的用户提示，带 3 秒去重窗口。
    ///
    /// 与 [`Self::abort_stalled_turn`] 是同一个问题的两面：**UI 线程绝不能等这把
    /// 锁**——回合 task 会持锁跑完整个 agent 回合，等下去等于整个界面停摆，
    /// 连 ESC 都按不动。正确做法是跳过本帧动作、下一帧重试（见
    /// [`try_session_lock`]）。
    pub(crate) fn notify_session_busy(&mut self) {
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(3);
        let now = Instant::now();
        if self
            .last_session_busy_notice
            .is_some_and(|last| now.duration_since(last) < WINDOW)
        {
            return;
        }
        self.last_session_busy_notice = Some(now);
        self.push_message(ChatMessage::system(wyj_i18n::tr("session.busy_notice")));
    }

    pub fn abort_stalled_turn(&mut self) -> bool {
        if !self.is_thinking {
            self.last_turn_activity = Instant::now();
            return false;
        }
        // 正在等用户交互：这不是「卡死」，是合法阻塞。心跳继续静置，
        // 等用户答复后事件一到自然重置。
        if self.permission_dialog.is_some()
            || self.ask_question_dialog.is_some()
            || self.plan_dialog.is_some()
            || self.exec_mode_confirm.is_some()
        {
            self.last_turn_activity = Instant::now();
            return false;
        }
        let stalled_for = self.last_turn_activity.elapsed();
        let threshold = turn_stall_timeout();
        if stalled_for < threshold {
            return false;
        }

        // 先 abort task：它的 future 被 drop 时会释放 session Mutex，
        // 且 TurnFinishedGuard 随之置位（但不依赖它——下面手工收状态）。
        if let Some(task) = self.current_task.take() {
            task.abort();
        }
        // task 若正卡在等 oneshot（confirm_tool / AskQuestion / ExitPlanMode），
        // abort 掉 future 后 receiver 也随之被 drop，发送端写不写都不会
        // 挂住。但这里仍然显式发一次 Deny/拒绝：万一弹窗还开着，用户看到的
        // 就是一个能按的框，而不是一个死界面。
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

        self.flush_streaming();
        self.is_thinking = false;
        self.injector = None;
        self.current_op = None;
        self.current_op_started = None;
        self.last_turn_activity = Instant::now();
        if let Some(start) = self.turn_start_time.take() {
            self.last_turn_elapsed_secs = Some(start.elapsed().as_secs_f64());
            self.last_turn_input_tokens = self
                .total_input_tokens
                .saturating_sub(self.turn_start_input_tokens);
            self.last_turn_output_tokens = self
                .total_output_tokens
                .saturating_sub(self.turn_start_output_tokens);
        }
        let message = wyj_i18n::tr_fmt(
            "agent.turn_stalled",
            &[("secs", &threshold.as_secs().to_string())],
        );
        self.push_tracked_message(ChatMessage::assistant_err(message.clone()));
        notify_emit(NotificationEvent::TurnError {
            error: truncate_notify_body(&message),
            session_id: if self.current_session_id.is_empty() {
                None
            } else {
                Some(self.current_session_id.clone())
            },
        });
        true
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
            // 必须先算耗时再清 `thinking_started`：该字段自 ThinkingDelta 首块
            // （见下方 apply_agent_event）写入后一直只有写点、零读点，直接丢弃
            // 会让 thinking 折叠头永远拿不到耗时。
            let elapsed = self.thinking_started.map(|t| t.elapsed().as_secs_f64());
            self.thinking_started = None;
            self.push_tracked_message(ChatMessage::thinking(text, elapsed));
        }
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        // 回合心跳：任何事件都算「有进展」。看门狗判的是「多久没动静」，
        // 不是「这轮跑了多久」——长回合只要还在吐 delta / 跑工具就不该判死。
        self.last_turn_activity = Instant::now();
        match event {
            AgentEvent::TextDelta(d) => {
                // thinking 结束、正文开始：先折叠思考行
                self.flush_thinking();
                self.streaming_buf.push_str(&d);
                self.note_new_chat_content();
            }
            AgentEvent::ThinkingDelta(d) => {
                if self.thinking_buf.is_empty() {
                    self.thinking_started = Some(std::time::Instant::now());
                }
                self.thinking_buf.push_str(&d);
                self.note_new_chat_content();
            }

            AgentEvent::ToolStart {
                id,
                name,
                input_json,
            } => {
                // TodoWrite 有专属面板（push_inline_todo_lines），走常规工具行会
                // 让同一次调用在聊天流里出现三遍：⏺ TodoWrite / ⎿ 任务列表已更新… /
                // TodoWrite 面板头 + 分隔线 + 条目。面板已经把"改了什么"讲得更完整，
                // 常规行只剩噪音。这里仍然照常登记 tool_info 与占用 seq（ToolEnd 靠
                // seq 配对回执，见下方同名分支），只是不往聊天流插消息。
                let is_todo = name == "TodoWrite";
                self.tool_call_count += 1;
                let seq = self.tool_call_count;
                let arg = tool_display_arg(&name, &input_json);
                let display = if arg.is_empty() {
                    name.clone()
                } else {
                    format!("{name}({arg})")
                };
                self.current_op = Some(display.clone());
                // 步骤计时起点：并发多个工具时后到的 ToolStart 会覆盖成最后一个，
                // 与 `current_op` 保持同一口径（标题栏只展示"最近开始的那一步"）。
                self.current_op_started = Some(Instant::now());
                self.tool_info.insert(id, (name.clone(), seq));
                self.flush_streaming();
                if !is_todo {
                    let mut msg = ChatMessage::tool_call(display, seq);
                    // ToolCall 也记录工具名，供 SubAgent Started 事件 FIFO 配对
                    msg.tool_name = Some(name);
                    self.push_tracked_message(msg);
                }
            }

            AgentEvent::ToolEnd {
                id,
                output,
                is_error,
                elapsed_secs,
            } => {
                self.current_op = None;
                self.current_op_started = None;
                let (name, seq) = self.tool_info.remove(&id).unwrap_or_default();
                // 实时输出块就此收工：后续由折叠态的 ⎿ 结果行接管正文。
                // 漏删的话，`apply_tool_output_chunk` 的闸门会挡住它，但哈希表
                // 里会一直留着一份没人渲染的快照（每个执行过的 Bash 一份 8KB）。
                self.live_tool_out.remove(&id);
                // ToolStart 已跳过 TodoWrite 的常规行，这里对称地跳过它的回执。
                // 不能只依赖"call_idx 找不到配对"来自然落空——那样会走
                // `push_tracked_message` 把 "⎿ 任务列表已更新: N 项…" 追加到流尾。
                if name == "TodoWrite" {
                    // TodoWrite 没有 ToolCall 行，因此也不存在 sub_agent_id 回填。
                } else {
                    let summary = tool_result_summary(&name, &output, is_error);
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
                self.current_op_started = None;
                self.turns += 1;
                self.save_needed = true;
                // 先对齐排队镜像再清 injector：那之后就拿不到共享队列了
                self.reconcile_pending_queue();
                self.injector = None;
                // 回合已正常结束（`stop_reason != tool_use`，模型已产出最终回答）：
                // 任何仍处于 InProgress 的 todo 都视为已完成。否则 UI 会卡在
                // "[N/N] ⋯ xxx" 这种永远不收口的"进行中"状态（即便 todo_stats
                // 不再累加耗时，已渲染行的状态字段读不到 Completed 也会显示 ⋯），
                // 直到下一次 TodoWrite 才有机会刷新——而很多模型会在最后一次回答
                // 里直接出最终输出，不再调用一次 TodoWrite 把最后一项标 completed，
                // 于是用户看到"AI 已回完，但任务列表还显示进行中"的割裂感。
                // 走与 TodoUpdate handler 里 was_in_progress → not_in_progress 同款
                // started_at → elapsed_secs 收口，统计数字保持连续。
                if let Some(items) = self.current_todos.as_mut() {
                    for item in items.iter_mut() {
                        if item.status == TodoStatus::InProgress {
                            item.status = TodoStatus::Completed;
                            if let Some(s) = self.todo_stats.get_mut(&item.id) {
                                if let Some(start) = s.started_at.take() {
                                    s.elapsed_secs += start.elapsed().as_secs_f64();
                                }
                            }
                        }
                    }
                }
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
                    // 统一通知：主回合正常完成
                    let summary = format!("{d_in}↑ {d_out}↓");
                    let summary = truncate_notify_body(&summary);
                    notify_emit(NotificationEvent::TurnFinished {
                        duration_ms: (elapsed * 1000.0) as u64,
                        summary,
                        session_id: if self.current_session_id.is_empty() {
                            None
                        } else {
                            Some(self.current_session_id.clone())
                        },
                    });
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
                self.reconcile_pending_queue();
                self.injector = None;
                self.push_tracked_message(ChatMessage::assistant_err(format!("[错误] {e}")));
                // 统一通知：主回合出错
                notify_emit(NotificationEvent::TurnError {
                    error: truncate_notify_body(&e),
                    session_id: if self.current_session_id.is_empty() {
                        None
                    } else {
                        Some(self.current_session_id.clone())
                    },
                });
            }

            AgentEvent::Injected => {
                // 只出队：聊天流里的回显在**提交那一刻**就画好了（见 Enter 分支）。
                // 这里再 push_user 一次就是同一条消息显示两遍。
                if !self.pending_queue.is_empty() {
                    self.pending_queue.remove(0);
                }
            }

            AgentEvent::Usage {
                input,
                output,
                compact_count,
                tool_schema_tokens,
                tool_schema_tokens_saved,
            } => {
                self.total_input_tokens = input;
                self.total_output_tokens = output;
                self.compact_count = compact_count;
                self.tool_schema_tokens = tool_schema_tokens;
                self.tool_schema_tokens_saved = tool_schema_tokens_saved;
            }

            AgentEvent::UsageDelta {
                input_tokens,
                output_tokens,
            } => {
                // 逐请求累加成本轮实时用量（标题栏 `title_token_text` 消费）。
                // saturating_add：极端长会话下 u32 溢出在 debug 构建会 panic，
                // 而这里只是展示，饱和截断完全可接受。
                self.turn_live_input_tokens =
                    self.turn_live_input_tokens.saturating_add(input_tokens);
                self.turn_live_output_tokens =
                    self.turn_live_output_tokens.saturating_add(output_tokens);
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
                // 上滚留下的视口位置，附加在聊天区尾部的选项区会被裁出可视范围。
                // 这里强制回到贴底跟随，保证选项与操作提示立即可见。
                self.chat_follow_tail = true;
                self.unseen_messages = false;
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

            AgentEvent::ShellKilled(id) => {
                self.push_tracked_message(ChatMessage::system(wyj_i18n::tr_fmt(
                    "shells.kill_done",
                    &[("id", id.as_str())],
                )));
            }

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
                                // 这批结果是 git marketplace 来的，必须记下来源，
                                // 否则后面安装时会按注册表路径去下载 zip。
                                dialog.browse_source = SkillsBrowseSource::Marketplace;
                                dialog.tab = SkillsDialogTab::Browse;
                                // rows() 里位置 0 是搜索行，第一条真实结果在 1。
                                dialog.cursor = if dialog.browse_results.is_empty() {
                                    0
                                } else {
                                    1
                                };
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

            AgentEvent::SkillRegistrySearched { result } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    // 只接受当前仍在等待的那次搜索的结果，避免连按回车时旧响应
                    // 覆盖新结果。
                    if matches!(dialog.overlay, SkillsOverlay::Searching) {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok(results) => {
                                dialog.status = if results.is_empty() {
                                    Some(wyj_i18n::tr("skills.dialog.no_results"))
                                } else {
                                    None
                                };
                                dialog.registry_results = results;
                                dialog.browse_results = Vec::new();
                                dialog.browse_source = SkillsBrowseSource::Registry;
                                dialog.tab = SkillsDialogTab::Browse;
                                // 位置 0 是搜索行；有结果时停在第一条上。
                                dialog.cursor = if dialog.registry_results.is_empty() {
                                    0
                                } else {
                                    1
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.registry_search_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::SkillRegistryArchiveFetched { result } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    if matches!(dialog.overlay, SkillsOverlay::FetchingArchive) {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok((preview, archive)) => {
                                // 回填发起下载时选中的那条技能；期间用户可能已经
                                // 移动过光标，所以按 slug 回查而不是信任当前 cursor。
                                let Some(idx) = dialog
                                    .registry_results
                                    .iter()
                                    .position(|s| s.slug == dialog.pending_registry_slug.clone())
                                else {
                                    return;
                                };
                                let Some(skill) = dialog.registry_results.get(idx).cloned() else {
                                    return;
                                };
                                dialog.overlay = SkillsOverlay::RegistryInstallConfirm {
                                    skill: Box::new(skill),
                                    preview,
                                    archive,
                                    scope: wyj_store::InstallScope::Global,
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.registry_download_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpUpgraded { row_idx, result } => {
                let mut upgraded = false;
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
                                upgraded = true;
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
                if upgraded {
                    // 见 reload_config_into_state 文档：Global scope 的 upgrade 同样
                    // 直接改磁盘 config.toml，不刷新这份内存快照的话，面板会一直显示
                    // 升级前的版本号，看起来像“升级了但没生效”。
                    reload_config_into_state(self);
                    if let Some(dialog) = &mut self.mcp_dialog {
                        dialog.refresh_installed(&self.config, &self.cwd);
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
                parent_id: _,
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
                // AGENTS.md/plan：避免长会话下常驻全文内存暴涨）
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
            E::Control {
                id,
                action,
                accepted,
            } => {
                self.sub_agent_trace_cache
                    .entry(id)
                    .or_default()
                    .push(TraceEvent::Control {
                        action: action.clone(),
                        accepted,
                    });
                if accepted && action == "interrupt" {
                    if let Some(agent) = self.sub_agents.get_mut(&id) {
                        agent.status = SubAgentStatus::Interrupted;
                        agent.current_tool = None;
                        agent.finished_at = Some(Instant::now());
                    }
                    // 被中断的条目从面板列表消失，选中态要重新对齐
                    self.reconcile_sub_agent_selection();
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
                // 完成即从面板列表消失，选中态（若正指向它）要重新对齐到
                // 仍在运行的条目；一个都不剩时收起面板并把焦点交还输入框。
                self.reconcile_sub_agent_selection();
                if background {
                    // 统一通知：后台子 Agent 完成（前台调用不通知，避免和主回合
                    // TurnFinished 在同一面板里重复提示）
                    let summary = format!("{agent_type} ({:.1}s)", elapsed_secs);
                    notify_emit(NotificationEvent::SubAgentDone {
                        agent_id: format!("a{id}"),
                        summary: truncate_notify_body(&summary),
                    });
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
                        Some(queue) => {
                            queue.push(
                                vec![ContentBlock::Text { text: reminder }],
                                InjectionKind::SystemReminder,
                            );
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

const SAVE_AND_DISABLE_ALTERNATE_SCROLL: &str = "\x1b[?1007s\x1b[?1007l";
const RESTORE_ALTERNATE_SCROLL: &str = "\x1b[?1007r";

/// 统一通知通道的 body 截断：与 `wyj_core::notify` 内部 200 字符上限保持一致，
/// 把换行变空格，防止 system-reminder / Notification Center 标题区被
/// 多行内容污染。仅 TUI 三处触发点共用，避免每处重复写截断逻辑。
fn truncate_notify_body(s: &str) -> String {
    const MAX_BODY: usize = 200;
    if s.len() <= MAX_BODY {
        return s.replace('\n', " ");
    }
    s.chars()
        .take(MAX_BODY)
        .collect::<String>()
        .replace('\n', " ")
}

/// Fullscreen + `DisableMouseCapture` 下的滚轮路由。
///
/// Ghostty 的 mode 1007 会把 alternate screen 中的滚轮转为普通方向键序列；
/// 开启 Kitty 键盘协议的 release/repeat 事件后，可以用“真实按键有 release，
/// 滚轮伪方向键只有 press”来分流，同时保留终端原生鼠标选择。
/// 其它或经过多路复用器的终端无法证明会透传 release 事件，因此直接
/// 关闭 mode 1007：可以保证滚轮不再污染输入历史，但不伪造“已可应用内滚动”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalWheelRouting {
    GhosttyKeyReleases,
    SuppressSyntheticArrows,
}

impl TerminalWheelRouting {
    fn from_environment(term_program: Option<&str>, in_multiplexer: bool) -> Self {
        if term_program.is_some_and(|value| value.eq_ignore_ascii_case("ghostty"))
            && !in_multiplexer
        {
            Self::GhosttyKeyReleases
        } else {
            Self::SuppressSyntheticArrows
        }
    }

    fn detect() -> Self {
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let in_multiplexer =
            std::env::var_os("TMUX").is_some() || std::env::var_os("ZELLIJ").is_some();
        Self::from_environment(term_program.as_deref(), in_multiplexer)
    }

    fn distinguishes_synthetic_arrows(self) -> bool {
        self == Self::GhosttyKeyReleases
    }
}

/// 等回触发退出的那个按键的 release 事件的总上限。
const QUIT_KEY_RELEASE_TIMEOUT: Duration = Duration::from_millis(300);
/// `event::poll` 的切片：分片轮询而不是一次睡满，便于超时精确收口。
const QUIT_KEY_POLL_SLICE: Duration = Duration::from_millis(20);

/// 退出前等回触发退出的按键的 release 事件。
///
/// Ghostty 路径压了 `DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES`，一次 Ctrl+C /
/// Ctrl+D 因此会产生 **press + release 两个 `CSI u` 事件**（c → `CSI 99;5;1u` 与
/// `CSI 99;5;3u`；`CSI < u` 只能改写*之后*的编码，杀不掉已经发出的字节）。收到 press
/// 就直接退出的话，松手时才生成的 release 会落进 shell，被 zsh 的 ZLE 当普通字符插进
/// 命令行，表现为提示符后凭空多出 `9;5;3u`。这里把配对的 release 等回来（带上限），
/// 让一次按键的 press/release 在 TUI 内部闭环。
///
/// 其它终端路径的按键没有 release 事件，直接跳过、不浪费退出时间。
fn await_quit_key_release(wheel_routing: TerminalWheelRouting) {
    if !wheel_routing.distinguishes_synthetic_arrows() {
        return;
    }
    let deadline = Instant::now() + QUIT_KEY_RELEASE_TIMEOUT;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        match event::poll(remaining.min(QUIT_KEY_POLL_SLICE)) {
            // 拿到任何一个按键事件即认为配对完成（正常情况下就是那个 release）。
            Ok(true) => return,
            Ok(false) => continue,
            Err(_) => return,
        }
    }
}

/// 进入全屏 TUI，但明确关闭鼠标捕获：鼠标由终端原生处理，用户可直接拖选文字，
/// 不需要按住 Shift/Option，松开修饰键后选区也不会被应用鼠标事件立即冲掉。
fn enter_terminal_screen<W: Write>(
    writer: &mut W,
    wheel_routing: TerminalWheelRouting,
) -> io::Result<()> {
    execute!(
        writer,
        EnableBracketedPaste,
        DisableMouseCapture,
        EnterAlternateScreen
    )?;
    match wheel_routing {
        TerminalWheelRouting::GhosttyKeyReleases => execute!(
            writer,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        ),
        TerminalWheelRouting::SuppressSyntheticArrows => {
            execute!(writer, Print(SAVE_AND_DISABLE_ALTERNATE_SCROLL))
        }
    }
}

fn leave_terminal_screen<W: Write>(
    writer: &mut W,
    wheel_routing: TerminalWheelRouting,
) -> io::Result<()> {
    match wheel_routing {
        TerminalWheelRouting::GhosttyKeyReleases => execute!(writer, PopKeyboardEnhancementFlags)?,
        TerminalWheelRouting::SuppressSyntheticArrows => {
            execute!(writer, Print(RESTORE_ALTERNATE_SCROLL))?
        }
    }
    execute!(
        writer,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrowRoutingAction {
    Pass,
    Defer,
    QueuePhysical { code: KeyCode, count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArrowRoutingResult {
    scroll_lines: i32,
    action: ArrowRoutingAction,
}

/// 区分 Ghostty mode 1007 产生的滚轮伪方向键与真实方向键。
///
/// 真实按键在 `REPORT_EVENT_TYPES` 下会产生 release/repeat；Ghostty 的滚轮
/// 转译路径则直接写入传统 `ESC[A` / `ESC[B`，crossterm 只能看到 press。
/// 第一个无修饰方向键先暂存：收到 release/repeat 就回放为真实键盘事件；
/// 收到第二个连续 press 就将整串路由到聊天内容滚动，绝不触碰输入历史。
#[derive(Debug, Default)]
struct GhosttyArrowRouter {
    pending: Option<KeyCode>,
    physical_down: Option<KeyCode>,
    wheel_burst: Option<(KeyCode, Instant)>,
}

impl GhosttyArrowRouter {
    const WHEEL_BURST_GAP: Duration = Duration::from_millis(40);

    fn route(&mut self, key: KeyEvent) -> ArrowRoutingResult {
        self.route_at(key, Instant::now())
    }

    fn route_at(&mut self, key: KeyEvent, now: Instant) -> ArrowRoutingResult {
        if self
            .wheel_burst
            .is_some_and(|(_, at)| now.saturating_duration_since(at) > Self::WHEEL_BURST_GAP)
        {
            self.wheel_burst = None;
        }

        let mut scroll_lines = 0;
        if let Some(pending) = self.pending {
            let continues_pending = key.code == pending
                && key.modifiers.is_empty()
                && matches!(
                    key.kind,
                    KeyEventKind::Press | KeyEventKind::Repeat | KeyEventKind::Release
                );
            if !continues_pending {
                scroll_lines += arrow_scroll_delta(pending);
                self.pending = None;
                self.wheel_burst = None;
            }
        }

        if !matches!(key.code, KeyCode::Up | KeyCode::Down) || !key.modifiers.is_empty() {
            return ArrowRoutingResult {
                scroll_lines,
                action: ArrowRoutingAction::Pass,
            };
        }

        match key.kind {
            KeyEventKind::Release => {
                if self.pending == Some(key.code) {
                    self.pending = None;
                    self.physical_down = None;
                    self.wheel_burst = None;
                    ArrowRoutingResult {
                        scroll_lines,
                        action: ArrowRoutingAction::QueuePhysical {
                            code: key.code,
                            count: 1,
                        },
                    }
                } else {
                    if self.physical_down == Some(key.code) {
                        self.physical_down = None;
                    }
                    ArrowRoutingResult {
                        scroll_lines,
                        action: ArrowRoutingAction::Defer,
                    }
                }
            }
            KeyEventKind::Repeat => {
                let count = if self.pending == Some(key.code) {
                    self.pending = None;
                    2
                } else {
                    1
                };
                self.physical_down = Some(key.code);
                self.wheel_burst = None;
                ArrowRoutingResult {
                    scroll_lines,
                    action: ArrowRoutingAction::QueuePhysical {
                        code: key.code,
                        count,
                    },
                }
            }
            KeyEventKind::Press => {
                if self.physical_down == Some(key.code) {
                    return ArrowRoutingResult {
                        scroll_lines,
                        action: ArrowRoutingAction::QueuePhysical {
                            code: key.code,
                            count: 1,
                        },
                    };
                }

                if self.pending == Some(key.code) {
                    self.pending = None;
                    self.wheel_burst = Some((key.code, now));
                    scroll_lines += arrow_scroll_delta(key.code) * 2;
                } else if self.wheel_burst.is_some_and(|(code, at)| {
                    code == key.code && now.saturating_duration_since(at) <= Self::WHEEL_BURST_GAP
                }) {
                    self.wheel_burst = Some((key.code, now));
                    scroll_lines += arrow_scroll_delta(key.code);
                } else {
                    self.pending = Some(key.code);
                    self.wheel_burst = None;
                }
                ArrowRoutingResult {
                    scroll_lines,
                    action: ArrowRoutingAction::Defer,
                }
            }
        }
    }
}

const fn arrow_scroll_delta(code: KeyCode) -> i32 {
    match code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod terminal_screen_tests {
    use super::*;

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn startup_disables_mouse_capture_for_direct_terminal_selection() {
        let mut output = Vec::new();
        enter_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases).unwrap();

        for disabled_mode in [
            b"\x1b[?1000l".as_slice(),
            b"\x1b[?1002l".as_slice(),
            b"\x1b[?1003l".as_slice(),
            b"\x1b[?1006l".as_slice(),
        ] {
            assert!(
                contains_bytes(&output, disabled_mode),
                "启动序列必须关闭全部鼠标报告模式: {output:?}"
            );
        }
        for enabled_mode in [
            b"\x1b[?1000h".as_slice(),
            b"\x1b[?1002h".as_slice(),
            b"\x1b[?1003h".as_slice(),
            b"\x1b[?1006h".as_slice(),
        ] {
            assert!(
                !contains_bytes(&output, enabled_mode),
                "启动序列不得重新开启鼠标捕获: {output:?}"
            );
        }
        assert!(
            contains_bytes(&output, b"\x1b[>3u"),
            "Ghostty 路径必须请求 press/repeat/release 键盘事件: {output:?}"
        );
        assert!(
            !contains_bytes(&output, b"\x1b[?1007l"),
            "Ghostty 路径需保留 mode 1007，再由应用区分滚轮伪方向键: {output:?}"
        );

        output.clear();
        leave_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases).unwrap();
        assert!(
            contains_bytes(&output, b"\x1b[<1u"),
            "退出时必须弹出启动时压入的 Kitty 键盘增强标志: {output:?}"
        );
        assert!(contains_bytes(&output, b"\x1b[?1049l"));
    }

    #[test]
    fn external_editor_handoff_pops_and_restores_tui_keyboard_protocols() {
        let mut output = Vec::new();
        leave_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases).unwrap();
        let resume_at = output.len();
        enter_terminal_screen(&mut output, TerminalWheelRouting::GhosttyKeyReleases).unwrap();

        let suspended = &output[..resume_at];
        assert!(contains_bytes(suspended, b"\x1b[<1u"));
        assert!(contains_bytes(suspended, b"\x1b[?2004l"));
        assert!(contains_bytes(suspended, b"\x1b[?1049l"));

        let resumed = &output[resume_at..];
        assert!(contains_bytes(resumed, b"\x1b[?2004h"));
        assert!(contains_bytes(resumed, b"\x1b[?1049h"));
        assert!(contains_bytes(resumed, b"\x1b[>3u"));
    }

    #[test]
    fn unsupported_terminal_suppresses_alternate_scroll_arrow_translation() {
        let mut output = Vec::new();
        enter_terminal_screen(&mut output, TerminalWheelRouting::SuppressSyntheticArrows).unwrap();
        assert!(contains_bytes(
            &output,
            SAVE_AND_DISABLE_ALTERNATE_SCROLL.as_bytes()
        ));
        assert!(!contains_bytes(&output, b"\x1b[>3u"));

        output.clear();
        leave_terminal_screen(&mut output, TerminalWheelRouting::SuppressSyntheticArrows).unwrap();
        assert!(contains_bytes(&output, RESTORE_ALTERNATE_SCROLL.as_bytes()));
        assert!(contains_bytes(&output, b"\x1b[?1049l"));
    }

    #[test]
    fn ghostty_routing_requires_a_direct_terminal_connection() {
        assert_eq!(
            TerminalWheelRouting::from_environment(Some("ghostty"), false),
            TerminalWheelRouting::GhosttyKeyReleases
        );
        assert_eq!(
            TerminalWheelRouting::from_environment(Some("Ghostty"), true),
            TerminalWheelRouting::SuppressSyntheticArrows
        );
        assert_eq!(
            TerminalWheelRouting::from_environment(Some("Apple_Terminal"), false),
            TerminalWheelRouting::SuppressSyntheticArrows
        );
    }
}

#[cfg(test)]
mod ghostty_arrow_router_tests {
    use super::*;

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    #[test]
    fn real_arrow_is_replayed_only_after_release() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        assert_eq!(
            router.route_at(key(KeyCode::Up, KeyEventKind::Press), now),
            ArrowRoutingResult {
                scroll_lines: 0,
                action: ArrowRoutingAction::Defer,
            }
        );
        assert_eq!(
            router.route_at(
                key(KeyCode::Up, KeyEventKind::Release),
                now + Duration::from_millis(80)
            ),
            ArrowRoutingResult {
                scroll_lines: 0,
                action: ArrowRoutingAction::QueuePhysical {
                    code: KeyCode::Up,
                    count: 1,
                },
            }
        );
    }

    #[test]
    fn wheel_arrow_burst_scrolls_content_without_replaying_keyboard_input() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        let _ = router.route_at(key(KeyCode::Up, KeyEventKind::Press), now);
        assert_eq!(
            router.route_at(
                key(KeyCode::Up, KeyEventKind::Press),
                now + Duration::from_millis(2)
            ),
            ArrowRoutingResult {
                scroll_lines: -2,
                action: ArrowRoutingAction::Defer,
            }
        );
        assert_eq!(
            router.route_at(
                key(KeyCode::Up, KeyEventKind::Press),
                now + Duration::from_millis(4)
            ),
            ArrowRoutingResult {
                scroll_lines: -1,
                action: ArrowRoutingAction::Defer,
            }
        );
    }

    #[test]
    fn physical_repeat_replays_initial_press_and_repeat() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        let _ = router.route_at(key(KeyCode::Down, KeyEventKind::Press), now);
        assert_eq!(
            router.route_at(
                key(KeyCode::Down, KeyEventKind::Repeat),
                now + Duration::from_millis(300)
            ),
            ArrowRoutingResult {
                scroll_lines: 0,
                action: ArrowRoutingAction::QueuePhysical {
                    code: KeyCode::Down,
                    count: 2,
                },
            }
        );
    }

    #[test]
    fn lone_wheel_arrow_is_flushed_to_content_before_the_next_key() {
        let mut router = GhosttyArrowRouter::default();
        let now = Instant::now();
        let _ = router.route_at(key(KeyCode::Down, KeyEventKind::Press), now);
        assert_eq!(
            router.route_at(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                now + Duration::from_millis(10)
            ),
            ArrowRoutingResult {
                scroll_lines: 1,
                action: ArrowRoutingAction::Pass,
            }
        );
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
    // `--plugin-dir` 临时加载的本地开发插件贡献（不落盘、仅当次进程生效）
    local_plugin: Option<wyj_store::lockfile::PluginContributions>,
    // 当前已连接 MCP 工具的共享快照：后台连接成功时 push 进来，供子 Agent
    // 工厂与 `/model` 重建读取（见 `wyj-code` CLI 侧 `make_sub_agent_factory`）
    mcp_tools: wyj_tools::SharedMcpTools,
    shared_agent_defs: wyj_tools::SharedAgentDefinitions,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
    // 首次启动 + 缺 API Key 时为 true,`tui_main` 会自动打开 ProfileDialog
    // 引导用户填写。仅当 `wyj_api::build_provider_with_model` 因缺 Key 失败、
    // 且当前入口是 TUI 默认启动时,`wyj-code` CLI 装配阶段才传 true。
    needs_api_key_onboarding: bool,
    // CAS blob pool。用于 `/clear` / 退出时 release 本会话被 context editing
    // 外部化的 blob 引用 —— CAS 的 gc 只回收 `ref_count == 0` 的 blob。
    workspace_cas: Option<Arc<wyj_core::workspace_cas::WorkspaceCas>>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // 全程 Fullscreen + alternate screen（不再有 Inline viewport）：`Viewport::
    // Fullscreen` 的构造路径不查询终端光标位置（`(area, Position::ORIGIN)`），
    // 结构上不会撞上 Inline 撑满高度时那个 tmux/部分终端下的光标查询竞态
    // （历史教训见 crates/tui/CLAUDE.md）。聊天区因此天然贴着窗口顶部铺开、
    // 输入框/状态栏贴着窗口底部，不再有"内容不够高、底部留空白"的问题。
    // 历史消息永远留在 `AppState.messages`，由 PageUp/PageDown 与终端滚轮分流
    // 后的内容滚动入口翻看；滚轮绝不得进入 Composer 输入历史。
    // 鼠标捕获必须保持关闭，让终端原生拖选/复制无需 Shift 且选区能在松键后保留。
    // 回复链接继续通过 OSC 8 交给终端处理 Command/Ctrl+点击；事件分支只作为某些
    // 终端仍主动上报修饰键点击时的防御性兜底，不为此开启鼠标报告模式。
    let wheel_routing = TerminalWheelRouting::detect();
    enter_terminal_screen(&mut stdout, wheel_routing)?;
    // 标记已进入 alternate screen：panic hook 据此决定是否还原终端。
    // 见 `crates/tui/src/panic_guard.rs`。
    crate::panic_guard::mark_active();
    let hyperlink_registry = HyperlinkRegistry::default();
    let backend = HyperlinkBackend::new(stdout, hyperlink_registry.clone());
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
        local_plugin,
        mcp_tools,
        shared_agent_defs,
        hyperlink_registry,
        plugin_runtime,
        wheel_routing,
        needs_api_key_onboarding,
        workspace_cas,
    )
    .await;

    // 顺序有讲究：先还原终端协议（pop Kitty 键盘增强标志），再排空这批在途按键事件，
    // 最后关 raw mode。排空必须夹在中间 —— pop 之前终端还在增强模式、随时可能再写新的
    // release 事件进来；关 raw mode 之后队列里残留的 0x03 又会给本进程发 SIGINT。
    // 全程都在 alternate screen，退出时统一还原；再次发送 DisableMouseCapture
    // 可防御外部组件或异常路径意外开启鼠标报告模式。
    let leave_result = leave_terminal_screen(terminal.backend_mut(), wheel_routing);
    crate::terminal_input::drain_pending_input();
    disable_raw_mode()?;
    leave_result?;
    // 标记已离开 alternate screen：panic hook 不再还原终端。
    // 必须放在 leave 成功之后,确保 leave 失败时 hook 仍会兜底还原一次。
    crate::panic_guard::mark_inactive();
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

/// 挂起 TUI，交给 $EDITOR（未设置回退 vi）打开指定文件，编辑完成后恢复 TUI
/// 并强制整屏重绘。除了 raw mode / alternate screen，还必须成对撤销并恢复
/// Kitty 键盘增强、bracketed paste 和滚轮路由；否则外部编辑器会收到 TUI 请求的
/// press/repeat/release 编码，把一次普通按键解析成多个字符。撤销之后再排空一次
/// tty 队列：pop 只影响之后的编码，已经在途的 release 事件会被编辑器当成字符读走。
/// 文件父目录若不存在会先创建，方便直接对着尚不存在的候选路径按 Enter 新建。
async fn open_path_in_editor<B: ratatui::backend::Backend + std::io::Write>(
    terminal: &mut Terminal<B>,
    path: &std::path::Path,
    wheel_routing: TerminalWheelRouting,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    leave_terminal_screen(terminal.backend_mut(), wheel_routing)?;
    crate::terminal_input::drain_pending_input();
    disable_raw_mode()?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let path_buf = path.to_path_buf();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&editor).arg(&path_buf).status()
    })
    .await;

    enable_raw_mode()?;
    enter_terminal_screen(terminal.backend_mut(), wheel_routing)?;
    terminal.clear()?;

    match status {
        Ok(Ok(s)) if s.success() => Ok(()),
        Ok(Ok(s)) => anyhow::bail!("编辑器退出码 {:?}", s.code()),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
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

/// 回合心跳看门狗的静默阈值：连续这么久（且期间无弹窗等待用户）都没有收到
/// 任何 Agent 事件，就判定 task 已卡死并主动 abort。
///
/// 取 15 分钟的理由：必须显著大于任何**合法**的静默窗口，否则会误杀健康回合。
/// 已知的合法长静默有三种——LLM 长流式生成（`api::stream_guard` 侧 120s
/// idle 看门狗已覆盖，超时会被打断重试而非静默）、前台 Bash（工具侧默认
/// 120s、schema 上限 600s）、长 thinking（按 delta 持续流出，不静默）。
/// **必须严格大于 600s**：等于会让「Bash 跑满 600s 上限」与看门狗判死
/// 同时发生，工具还没来得及返回自己的超时错误回合就被杀了——那样的报错
/// 会把原因指向错误的地方。留出余量确保「工具侧超时永远先于心跳触发」。
///
/// 可用 `WYJ_CODE_TURN_STALL_SECS` 覆盖（0 = 关闭看门狗）。
pub const TURN_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// 实际生效的回合静默阈值（首次读取时解析环境变量，之后固定）。
pub fn turn_stall_timeout() -> std::time::Duration {
    static STALL: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *STALL.get_or_init(|| {
        let Some(raw) = std::env::var("WYJ_CODE_TURN_STALL_SECS").ok() else {
            return TURN_STALL_TIMEOUT;
        };
        match raw.trim().parse::<u64>() {
            Ok(0) => std::time::Duration::MAX,
            Ok(secs) => std::time::Duration::from_secs(secs),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "WYJ_CODE_TURN_STALL_SECS 不是合法秒数，回退到默认值"
                );
                TURN_STALL_TIMEOUT
            }
        }
    })
}

/// 一轮 Agent 对话 task 的句柄：中断能力 + 存活标记。
///
/// 修复前 `spawn_agent_turn` 只把 `AbortHandle` 交回 UI（`JoinHandle` 被
/// `abort_handle()` 消费掉，等于 task 被 detach），于是 task 无论 panic 还是被
/// 外部 abort，都**不会**产生 `TurnDone` / `Error`。而 `is_thinking` 的清零
/// 只认这两个事件（`AppState::apply_agent_event`）。task 一死，UI 就永久停在
/// thinking，用户后续输入全部滞留 `pending_queue` 无人排空——连兜底分支
/// `!is_thinking && !pending_queue.is_empty()` 也被同一个标志挡住——表现为
/// 「跑着跑着停住，之后输入什么都不响应」。
///
/// `finished` 由 task 内的 [`TurnFinishedGuard`] 置位。正常返回、panic 展开、
/// 被 abort 丢弃 future，三条路径都会 drop guard，缺一不可，所以不能用
/// 「task 末尾显式置位」那种写法。
pub struct TurnTask {
    abort: AbortHandle,
    finished: Arc<AtomicBool>,
}

impl TurnTask {
    pub fn abort(&self) {
        self.abort.abort();
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// RAII 存活标记：只要 task 的 future 被销毁（正常结束 / panic 展开 / abort），
/// Drop 一定执行，据此把「task 已死」这件事可靠地告诉 UI。
struct TurnFinishedGuard(Arc<AtomicBool>);

impl Drop for TurnFinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// spawn 一个回合 task 并返回带存活标记的句柄。
///
/// **所有会置 `is_thinking = true` 的 spawn 都必须走这里**（`spawn_agent_turn`
/// 与两处 `CommandResult::RunPrompt*`）。裸 `tokio::spawn` + 只取
/// `abort_handle()` 会让 task 变成 detach：panic 信息写进 alternate screen 下
/// 的 stderr，用户看不见，而 UI 永远等不到终止事件。
/// 主循环取 session 锁：**永不阻塞**，拿不到就返回 `None`。
///
/// 回合 task（`spawn_agent_turn` / 两处内联 turn task）会在**整个 agent 回合
/// 期间**持有同一把 `tokio::sync::Mutex`——LLM 往返 + 全部工具调用都在锁内，
/// 一次长回合动辄几分钟。主循环只要在这把锁上 `.await`，整个界面就会停在
/// 那一帧：不重绘、不读按键，连 v1.5.21 的回合心跳看门狗
/// （`AppState::abort_stalled_turn`，排在保存分支**上游**）都跑不到下一轮——
/// 用户按 ESC 也救不回来，表现就是「执行久了卡死不动」。
///
/// 因此主循环里凡是碰 session 的地方一律走这里：拿不到就跳过本帧动作、
/// 下一帧再试。**回合 task 内部的 `lock().await` 不要改**——那正是持锁者。
fn try_session_lock(session: &Arc<Mutex<Session>>) -> Option<tokio::sync::MutexGuard<'_, Session>> {
    session.try_lock().ok()
}

/// 「批准计划 / 确认切执行模式」时建 checkpoint——**放到后台 task，绝不在主循环里等锁**。
///
/// 这两处确认都发生在**回合进行中**：agent task 正持着 session 锁等本弹窗的
/// oneshot，批准后它还要继续跑完这一轮，锁全程不放。原来在这里
/// `session.lock().await`，等于主循环停在这一帧直到整个回合结束——不重绘、
/// 不读按键，连 ESC/Ctrl+C 都按不动（输入处理在 `event::poll` 之后）。
///
/// 改成后台 task 后：UI 立刻恢复响应，锁一释放（回合一结束）checkpoint 自动
/// 落盘。代价是快照取的是「锁可用那一刻」的消息列表，可能已包含批准后本轮
/// 追加的内容——比"没有 checkpoint"好，且 `/rewind` 照常能列出它。
fn spawn_plan_approval_checkpoint(
    session: Arc<Mutex<Session>>,
    cwd: std::path::PathBuf,
    session_id: String,
    session_store: Option<&Arc<SessionStore>>,
) {
    let Some(sessions) = session_store else {
        return;
    };
    let Ok(store) = wyj_core::CheckpointStore::configured(sessions.dir(), session_id) else {
        return;
    };
    tokio::spawn(async move {
        // 这里是后台 task，锁一释放（回合一结束）就自然拿到，等锁无副作用
        let mut sess = session.lock().await;
        if let Ok(checkpoint) = store.create(
            &cwd,
            &sess.messages,
            wyj_core::CheckpointKind::PlanApproval,
            Some("before plan approval".to_string()),
        ) {
            sess.current_checkpoint_id = Some(checkpoint.id);
        }
    });
}

fn spawn_turn_task<F>(future: F) -> TurnTask
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let finished = Arc::new(AtomicBool::new(false));
    let finished_for_task = finished.clone();
    let handle = tokio::spawn(async move {
        // 存活标记必须包在 future 最外层：task 因 panic 或 abort 而死时
        // （也就是最需要通知 UI 的两种情况），靠它把 is_thinking 解开。
        let _finished_guard = TurnFinishedGuard(finished_for_task);
        future.await;
    });
    TurnTask {
        abort: handle.abort_handle(),
        finished,
    }
}

/// 构建并 spawn 一轮 Agent 对话任务，返回任务句柄及补充信息注入通道。
///
/// `text` 区分两类回合：
/// - `Some(text)` = 常规回合，`text` 是用户输入（作为 user 消息落进 session 历史）。
/// - `None` = **自动唤醒回合**：本轮不由用户发起，`preface_reminders` 里的后台
///   任务结果本身就是这一轮唯一的 user 输入。
///
/// 之所以要区分：自动唤醒时若传 `Some(reminder 原文)`，reminder 会被当成用户
/// 说的话写进 session 历史并在 `/resume` 后继续显示，既污染上下文又让模型误
/// 以为用户催过它。`None` 分支把 reminder 作为唯一 user 消息推入，语义上仍是
/// 「一次新的输入」，但调用方无需编造用户话术。
#[allow(clippy::too_many_arguments)]
fn spawn_agent_turn(
    text: Option<String>,
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
    // 主 Agent 空闲期间积累的后台任务结果 reminder（子 Agent / 后台 shell）
    preface_reminders: Vec<String>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
) -> (TurnTask, Arc<wyj_core::SteerQueue>) {
    // 共享注入队列：主 Agent 与（经 ToolContext 拿到的）前台子 Agent 排空的是
    // 同一份，谁先到工具轮边界谁取走——子 Agent 在跑时，用户补充的内容插进
    // 子 Agent 的下一轮，而不是等它整个跑完。
    let steer = Arc::new(wyj_core::SteerQueue::new());
    let steer_in_task = Arc::clone(&steer);
    // 子 Agent 用量累加器。ToolCtx 每回合重建（见上面 `ToolCtx::new`），
    // 所以句柄必须跟着 task 走而不是挂在 ctx 上活一回合——否则子 Agent 写完
    // 就随 ctx 一起丢弃，累加器永远是空的。
    let sub_usage_acc = Arc::new(wyj_core::SubUsageTotals::default());
    let task = spawn_turn_task(async move {
        let mut sess = session_c.lock().await;
        match text {
            Some(text) => {
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
            }
            None => {
                // 自动唤醒：reminder 本身即这一轮唯一输入。多条（多个后台
                // 任务同帧完成）合并成一条 user 消息，避免连续多轮 LLM
                // 调用浪费 token。
                let merged = preface_reminders.join("\n\n");
                if !merged.trim().is_empty() {
                    sess.push_user(merged);
                }
            }
        }
        let current_mode = mode_arc.lock().await.clone();
        let mut ctx = ToolCtx::new(&ctx_cwd);
        ctx.set_execution_surface(wyj_core::ExecutionSurface::TuiInteractive);
        // 让前台子 Agent 能拿到同一份注入队列：用户在它跑的过程中补充的要求
        // 插进子 Agent 自己的工具轮边界，而不是被堵到它跑完为止。
        ctx.steer_queue = Some(Arc::clone(&steer_in_task));
        // 子 Agent 用量回灌通道：子 Agent 在自己的 Session 上跑，其 token 既不
        // 进主会话的 total_* 也不发 AgentEvent::Usage（那只由主 Agent 发射）。
        // 挂上这个累加器，主 Agent 每批工具执行后会把它抽干记进 session 的
        // sub_* 字段，`/cost` 的「子 Agent」行与金额合计才有数据。
        ctx.sub_usage = Some(Arc::clone(&sub_usage_acc));
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
        let turn_result = turn_agent
            .run_turn_with_injection(
                &mut sess,
                &ctx,
                &mut on_text,
                Some(steer_in_task.as_ref()),
                on_inject,
            )
            .await;
        let event = if turn_result.is_ok() {
            "turn_finished"
        } else {
            "turn_error"
        };
        // 锁内先把要上报的用量取出来，随后发终态事件前统一 drop 锁
        let usage = AgentEvent::Usage {
            input: sess.total_input_tokens,
            output: sess.total_output_tokens,
            compact_count: sess.compact_count,
            tool_schema_tokens: sess.tool_schema_tokens_last,
            tool_schema_tokens_saved: sess.tool_schema_tokens_saved,
        };
        emit_plugin_event_background(
            plugin_runtime,
            event,
            serde_json::json!({
                "cwd": ctx_cwd,
                "input_tokens": sess.total_input_tokens,
                "output_tokens": sess.total_output_tokens,
                // 稳态口径：本次请求实际发送的工具 schema（与 /context 同源）。
                "tool_schema_tokens": sess.tool_schema_tokens_last,
                // 累加口径：本会话为工具定义总共付出的量。
                "tool_schema_tokens_total": sess.tool_schema_tokens,
                "tool_schema_tokens_saved": sess.tool_schema_tokens_saved,
                "sub_input_tokens": sess.sub_input_tokens,
                "sub_output_tokens": sess.sub_output_tokens,
                "sub_cache_read_tokens": sess.sub_cache_read_tokens,
                "sub_cache_write_tokens": sess.sub_cache_write_tokens,
                "error": turn_result.as_ref().err().map(ToString::to_string),
            }),
        );
        // ── 必须在发终态事件**之前**放掉 session 锁 ──────────────────
        // 终态事件走的是**阻塞式** send（通道容量 256），而 UI 收到 TurnDone
        // 后会把 `save_needed` 置位，主循环下一帧就去 `session.lock().await`
        // 保存（排在 `event::poll` 之前）。若这里还持着锁：UI 等锁、这里等 UI
        // 排空通道 → 环形等待，界面永久冻结，且看门狗跑不到（它在上游）。
        // 长回合期间工具事件已把通道填满，这个环很容易撞上。
        drop(sess);
        match turn_result {
            Ok(_) => {
                let _ = tx.send(usage).await;
                let _ = tx.send(AgentEvent::TurnDone).await;
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e.to_string())).await;
            }
        }
    });
    (task, steer)
}

fn emit_plugin_event_background(
    runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
    event: &'static str,
    payload: serde_json::Value,
) {
    tokio::spawn(async move {
        for result in runtime.emit_channel_event(event, &payload).await {
            if !result.success {
                tracing::warn!("plugin channel {} failed: {}", result.name, result.output);
            }
        }
    });
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
                "CodeSearch",
                "WebFetch",
                "WebSearch",
                "AskQuestion",
                "Write",        // 仅 doc/plan、docs/plan、.wyj-code/plans 或本轮授权文档
                "Edit",         // 与 Write 使用同一执行层路径策略
                "Bash",         // 只读命令由 PermissionPolicy 强制判定
                "BashOutput",   // 后台任务输出读取（纯读）
                "ExitPlanMode", // 提交计划并请求批准（计划文本作为参数直传，不落盘）
                "TodoWrite",    // 任务追踪，plan 模式同样有用
                "Agent",        // 子 Agent（继承同一白名单，不会绕过 plan 模式限制）
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            PermissionMode::Plan(set)
        }
        AgentMode::Bypass => PermissionMode::AutoApprove,
        AgentMode::Normal => PermissionMode::Prompt,
    }
}

fn format_tui_model_doctor(report: &wyj_api::ModelDoctorReport) -> String {
    let mut lines = vec![
        format!("Model doctor · {}", report.profile),
        format!(
            "{} / {} · {} · {:?}",
            report.identity.vendor,
            report.identity.model,
            report.identity.wire_protocol,
            report.verification_status
        ),
        format!(
            "context {} · output {} · vision {} · thinking {:?}",
            report.capabilities.context_window,
            report.capabilities.max_output_tokens,
            report.capabilities.vision.value,
            report.capabilities.thinking.value
        ),
        format!(
            "tools {} · parallel {} · strict schema {} · stream usage {}",
            report.capabilities.tool_calling.value,
            report.capabilities.parallel_tool_calls.value,
            report.capabilities.strict_tool_schema.value,
            report.capabilities.stream_usage.value
        ),
        format!("probe: {}", report.probe_status),
    ];
    // v1.5.7 国产配置体检:把 profile 原始字段 + 推导后的 effective 值并列显示,
    // 让用户一眼能看出当前 provider/base_url/字段是否与协议匹配。
    lines.push(format!(
        "profile: thinking_budget={} · prompt_cache={:?}→effective={} · stream_options={:?}→effective={} · vision={} · base_url={}",
        report
            .profile_thinking_budget
            .map(|b| b.to_string())
            .unwrap_or_else(|| "None".to_string()),
        report.profile_prompt_cache,
        report.effective_prompt_cache,
        report.profile_openai_stream_options,
        report.effective_stream_options,
        report.profile_vision,
        report.profile_base_url,
    ));
    if report.requires_supplier_usage {
        lines.push("usage: 依赖供应商返回精确 token 数(message_delta / 最终 chunk)".to_string());
    }
    lines.extend(
        report
            .known_degradations
            .iter()
            .map(|degradation| format!("- {degradation}")),
    );
    lines.join("\n")
}

fn resolve_tui_checkpoint_id(
    store: &wyj_core::CheckpointStore,
    requested: Option<String>,
) -> anyhow::Result<String> {
    match requested {
        Some(id) => Ok(id),
        None => store
            .latest()?
            .map(|checkpoint| checkpoint.id)
            .ok_or_else(|| anyhow::anyhow!("当前会话还没有 checkpoint")),
    }
}

fn format_checkpoint_list(store: &wyj_core::CheckpointStore) -> anyhow::Result<String> {
    let checkpoints = store.list()?;
    if checkpoints.is_empty() {
        return Ok("当前会话还没有 checkpoint。".to_string());
    }
    Ok(checkpoints
        .into_iter()
        .rev()
        .map(|checkpoint| {
            format!(
                "{}  {:?}  {}  {} messages",
                checkpoint.id,
                checkpoint.kind,
                checkpoint.name.as_deref().unwrap_or("-"),
                checkpoint.message_count
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn format_rewind_preview(preview: &wyj_core::RewindPreview) -> String {
    let mut lines = vec![format!(
        "checkpoint {} 将影响 {} 个文件：",
        preview.checkpoint_id,
        preview.affected_files.len()
    )];
    lines.extend(
        preview
            .affected_files
            .iter()
            .take(50)
            .map(|path| format!("- {}", path.display())),
    );
    if preview.affected_files.len() > 50 {
        lines.push(format!(
            "- ……另有 {} 个文件",
            preview.affected_files.len() - 50
        ));
    }
    if let Some(note) = &preview.note {
        lines.push(format!("说明：{note}"));
    }
    lines.join("\n")
}

fn retarget_agent_session(
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    sessions: Option<&Arc<SessionStore>>,
    session_id: &str,
) {
    let updated = attach_agent_session(
        (**shared_agent.read().unwrap()).clone(),
        sessions,
        session_id,
    );
    *shared_agent.write().unwrap() = Arc::new(updated);
}

fn attach_agent_session(
    mut agent: Agent,
    sessions: Option<&Arc<SessionStore>>,
    session_id: &str,
) -> Agent {
    agent.set_session_id(session_id.to_string());
    if let Some(sessions) = sessions {
        if let Ok(store) =
            wyj_core::CheckpointStore::configured(sessions.dir(), session_id.to_string())
        {
            agent.set_checkpoint_store(Arc::new(store));
        }
    }
    agent
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
                TraceEvent::Control { action, accepted } => {
                    if let Some(s) = &mut reconstructed {
                        if accepted && action == "interrupt" {
                            s.status = SubAgentStatus::Interrupted;
                            s.current_tool = None;
                            s.finished_at = Some(Instant::now());
                        }
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
    // 面板只列运行中的子 Agent（`visible_sub_agents` 是唯一口径），因此这里
    // 也只在运行项里定位：选中一个不在列表上的 id 会让焦点悬空在一行看不见的
    // 位置，用户按 ↑↓ 才知道发生了什么。
    let running: Vec<u64> = state
        .visible_sub_agents()
        .iter()
        .map(|(id, _)| **id)
        .collect();
    if running.is_empty() {
        state
            .messages
            .push(ChatMessage::system(wyj_i18n::tr("subagents.empty")));
        return;
    }
    match target_id {
        Some(id) if running.contains(&id) => {
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
            if let Some(&last_id) = running.last() {
                state.selected_sub_agent = Some(last_id);
                state.sub_agent_detail_open = true;
                state.sub_agent_detail_scroll = 0;
                state.ui_focus = UiFocus::SubAgents;
            }
        }
    }
}

/// `/shells [id]` 命令：无参数时定位到最近一个**运行中**的后台任务并展开实时
/// 输出；带 id 时同样只在运行项里校验（面板不列已完成的条目）。
fn apply_open_shells_panel(state: &mut AppState, target_id: Option<String>) {
    let running: Vec<String> = state
        .running_shells()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    if running.is_empty() {
        state
            .messages
            .push(ChatMessage::system(wyj_i18n::tr("shells.empty")));
        return;
    }
    match target_id {
        Some(id) if running.iter().any(|s| s == &id) => {
            state.selected_shell = Some(id);
            state.shell_detail_open = true;
            state.shell_detail_scroll = 0;
            state.ui_focus = UiFocus::Shells;
        }
        Some(id) => {
            state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                "shells.not_found",
                &[("id", id.as_str())],
            )));
        }
        None => {
            // 保持 spawn 顺序的末项即最近启动的**运行中**任务
            if let Some(last) = running.last() {
                state.selected_shell = Some(last.clone());
                state.shell_detail_open = true;
                state.shell_detail_scroll = 0;
                state.ui_focus = UiFocus::Shells;
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

/// 未信任的项目级 server 被静默排除在外——一旦用户在信任确认面板里批准
/// （`wyj_store::project_trust::approve`），下一次调用就会自然把它纳入
/// desired 集合，`refresh_tui_mcp_runtime` 每帧都会调用这个函数，所以批准后
/// 不需要额外触发重连，下一帧自动生效。
fn effective_mcp_servers_for_runtime(
    cfg: &wyj_config::Config,
    cwd: &std::path::Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) -> Vec<wyj_config::McpServerConfig> {
    let (mut servers, _pending) =
        wyj_store::mcp_install::effective_mcp_servers_trust_split(cfg, cwd);
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

/// 外部资源 reconcile 的节流间隔。稳定态下这两条路径产出完全相同的结果，
/// 每帧重跑只是纯浪费；1 秒既能覆盖「用户改完配置希望立刻生效」的诉求，
/// 又把空闲期的 `Config::load()` + `Agent::clone()` 压到 1 次/秒。
const EXTERNAL_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// `AgentDefinition` 不 derive `PartialEq`，而它只有五六个元素——`Debug` 展开的
/// 字符串既便宜又覆盖全部字段，拿它当变更指纹比逐字段手写比较更不容易漏。
fn agent_defs_fingerprint(defs: &[wyj_core::AgentDefinition]) -> String {
    format!("{defs:?}")
}

/// 重载 agent 定义并把工具 schema 刷新进 Agent。
///
/// `last_fingerprint` 用于跳过无变化的重建：定义没变时**不会**去 clone Agent
/// 也不会重跑 `refresh_tool_definitions()`，两者都是本函数里最贵的部分。
fn refresh_tui_agent_definitions(
    shared_defs: &wyj_tools::SharedAgentDefinitions,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    cwd: &Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
    last_fingerprint: &mut Option<String>,
) {
    let mut sources = wyj_store::plugin_install::enabled_plugin_agent_paths(cwd);
    if let Some(local) = local_plugin {
        sources.extend(local.agent_paths.clone());
    }
    let defs = wyj_core::load_agent_defs(cwd, &sources);
    let fingerprint = agent_defs_fingerprint(&defs);
    if last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
        return;
    }
    *last_fingerprint = Some(fingerprint);
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
    last_tool_names: &mut Vec<String>,
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
    // 任何连接生命周期事件（连上 / 失败 / 移除）都意味着工具实现可能已经换过，
    // 此时**必须**重建 Agent，哪怕工具名集合没变——server 重启后 schema 变了
    // 但名字没变是最典型的漏网场景。
    let force_rebuild = !events.is_empty();
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
    // 工具名集合没变又没有连接事件 ⇒ Agent 里已经挂着同一批工具，跳过重建。
    // clone + remove_tools_where + register_tool 是本函数最贵的部分，而稳定态下
    // 它的结果与上一帧逐字节相同。
    let desired_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    if !force_rebuild && desired_names == *last_tool_names {
        return;
    }
    *last_tool_names = desired_names;
    let mut new_agent = (**shared_agent.read().unwrap()).clone();
    new_agent.remove_tools_where(|name| name.starts_with("mcp__"));
    for tool in tools {
        new_agent.register_tool(tool);
    }
    *shared_agent.write().unwrap() = Arc::new(new_agent);
}

#[allow(clippy::too_many_arguments)]
async fn tui_main(
    terminal: &mut Terminal<HyperlinkBackend<io::Stdout>>,
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
    hyperlink_registry: HyperlinkRegistry,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
    wheel_routing: TerminalWheelRouting,
    // 首次启动 + `~/.wyj-code` 缺失 + 用户尚未填入 API Key 时为 true。
    // `AppState::new` 之后立即打开 `ProfileDialog` 引导用户填写。
    needs_api_key_onboarding: bool,
    // CAS blob pool：`/clear` 时 release 本会话被 context editing 外部化的 blob。
    workspace_cas: Option<Arc<wyj_core::workspace_cas::WorkspaceCas>>,
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
    if needs_api_key_onboarding {
        // 首次启动缺 API Key 时,默认聚焦 api_key 字段而非 entry header,
        // 用户键入即开始编辑 ProfileDialog,无需先按 Enter 展开。
        // ProfileDialog 浮层在事件循环里自然拦截所有输入,所以无需额外
        // 锁住 chat 输入框。`profile_try_save` 通过 `rebuild_fn` 替换
        // shared_agent 后,用户回到 chat 路径即可正常对话。
        state.profile_dialog = Some(ProfileDialog::new_for_onboarding(&state.config));
    }
    state.hyperlink_registry = hyperlink_registry;
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
    // 工具执行途中的实时输出。前台 Bash 把 stdout/stderr 的累计尾部按 100ms
    // 快照推进这里，主循环排空后交给渲染层挂在对应 ⏺ 行下方——这是「正在执行
    // 的步骤默认展开」的唯一数据来源。
    //
    // 容量取 8 是刻意的：快照语义下满了直接丢最旧的即可（下一帧自带完整累计
    // 输出），而容量放大只会让突发输出把排队延迟拉长到肉眼可见。
    let (tool_out_tx, mut tool_out_rx) = mpsc::channel::<ToolOutputChunk>(8);
    if !install_global_tool_output_sink(tool_out_tx) {
        // 进程内已有 sink（理论上只有一份 TUI 主循环）。第二个 receiver 与第一个
        // 抢同一批快照，表现为输出随机停在半途——与其静默错乱不如喊出来。
        tracing::warn!("工具实时输出 sink 已被占用，本实例的前台 Bash 将退回「跑完一次性返回」");
    }
    // 终止后台任务是 async（SIGTERM → 等 2s → 兜底 SIGKILL），不能占用按键
    // 处理线程；完成后复用 agent_tx 把结果发回 UI 线程渲染提示。
    state.kill_tx = Some(agent_tx.clone());

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
    let mut cmd_registry = standard_registry_with_skills(
        &home_dir,
        &cwd,
        &disabled_skills,
        &plugin_skill_sources,
        ProjectSkillsGate::resolve(&cwd),
    );

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

    // 项目级 MCP server 首次信任确认：只在启动时检查一次（不像 MCP 连接状态
    // 那样每帧刷新），弹窗见 render::draw_project_trust_panel + 下方按键处理。
    if let wyj_store::TrustStatus::Pending(servers) = wyj_store::project_trust::trust_status(&cwd) {
        state.pending_mcp_trust = Some(servers);
    }

    // 项目级 skill 信任确认：与 MCP 同构，同样只在启动时检查一次。载荷取自
    // `project_skill_infos`（走与真正加载完全相同的 walk），确保面板列的名字与
    // 批准后实际生效的名字一致。全部被 disabled 时不弹空面板。
    {
        let disabled = wyj_store::disabled_skill_names(&cwd);
        let infos = wyj_commands::skill::project_skill_infos(&cwd, &disabled);
        if !infos.is_empty() {
            state.pending_skill_trust = Some(infos);
        }
    }

    // 初始化 Session：若有历史消息则恢复，并重建 TUI 显示
    let has_initial = !initial_messages.is_empty();
    let mut init_sess = Session::new();
    if let Some(file) = session_store
        .as_ref()
        .and_then(|store| store.load(&current_session_id).ok())
    {
        init_sess.restore_usage_from(&file);
    }
    init_sess.messages = initial_messages;
    if has_initial {
        state.total_input_tokens = init_sess.total_input_tokens;
        state.total_output_tokens = init_sess.total_output_tokens;
        state.welcome_frozen = true;
        state.compact_count = init_sess.compact_count;
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
    let mut ghostty_arrow_router = GhosttyArrowRouter::default();
    let mut classified_arrow_keys = VecDeque::<KeyEvent>::new();
    // 外部资源（agent 定义 / MCP server）的 reconcile 指纹，初值 `None`
    // 表示「还没算过」——必须让第一帧真的跑一次，否则启动时已连接的 MCP 工具
    // 永远进不了 Agent。
    let mut agent_defs_fingerprint: Option<String> = None;
    let mut mcp_tool_names: Vec<String> = Vec::new();
    // 让下面第一次判断立刻成立（否则首帧被节流挡掉，MCP 工具要等 1 秒才出现）。
    let mut last_external_refresh = Instant::now() - EXTERNAL_REFRESH_INTERVAL;

    loop {
        // Ctrl+C 首次按下超过 3 秒未确认则重置
        if let Some(t) = state.last_ctrl_c {
            if t.elapsed() > Duration::from_secs(3) {
                state.ctrl_c_pressed = false;
                state.last_ctrl_c = None;
            }
        }

        // 推进 spinner 动画帧（每 ~80ms 一帧，与 Claude Code 节奏一致）；
        // 后台子 Agent / 后台 shell 运行期间即使主 Agent 空闲也要驱动动画
        if (state.is_thinking || state.has_running_sub_agents() || state.has_running_shells())
            && last_spinner_advance.elapsed().as_millis() >= 80
        {
            state.spinner_frame = (state.spinner_frame + 1) % render::SPINNER_FRAMES.len();
            last_spinner_advance = Instant::now();
        }

        terminal.draw(|f| {
            // 先回写聊天区可用列数：`/cost` 等需要按宽度排版的命令在事件
            // 分支里读它。必须取自本次真实 frame，而不是启动时探测一次——
            // 用户中途 resize 时只有这里知道新宽度。
            state.chat_content_width = render::chat_content_width(f.area().width);
            render::draw(f, &mut state, &input)
        })?;

        // 清空 agent 事件队列
        while let Ok(ev) = agent_rx.try_recv() {
            state.apply_agent_event(ev);
        }
        while let Ok(chunk) = tool_out_rx.try_recv() {
            state.apply_tool_output_chunk(chunk);
        }

        // 清空子 Agent 事件队列（在 agent 事件之后排空，保证父 ToolStart 先于 Started 应用）
        while let Ok(ev) = sub_rx.try_recv() {
            state.apply_agent_event(AgentEvent::SubAgent(ev));
        }

        // 回合 task 看门狗。必须排在上面两处排空之后：task 正常收尾是「先塞
        // TurnDone/Error 再结束自己」，抢在事件被应用之前判定会把正常收尾
        // 误判成异常终止。
        state.reap_lost_turn_task();

        // 回合心跳：兜「task 活着但卡住」——reap_lost_turn_task 只认已结束的
        // task，对 hang 住的 task 无能为力。同样的位置约束：必须排在事件
        // 排空之后，刚收到的事件不能被本帧立刻判死。
        state.abort_stalled_turn();

        // Resource mutations are applied only after the previous Agent turn is
        // no longer active. The current turn keeps its immutable snapshot; the
        // next turn gets the reconciled MCP set atomically.
        //
        // **为什么要节流**：这段原本每个空闲帧都跑，内部是 `Config::load()` 读盘 +
        // 一次 `Agent::clone()` 深拷贝 + 对全部工具重求 `definition()` 并重建
        // argument pipeline。稳定态下这些结果一个字节都不会变，却按 ~30 帧/秒
        // 重复烧 CPU，直接表现为空闲时输入回显发涩。1 秒一次的感知上限是
        // 「配置改完最多 1 秒生效」，远低于人能察觉的阈值。
        if !state.is_thinking && last_external_refresh.elapsed() >= EXTERNAL_REFRESH_INTERVAL {
            last_external_refresh = Instant::now();
            refresh_tui_agent_definitions(
                &shared_agent_defs,
                &shared_agent,
                &cwd,
                local_plugin.as_ref(),
                &mut agent_defs_fingerprint,
            );
            refresh_tui_mcp_runtime(
                &mut mcp_runtime,
                &mut state,
                &shared_agent,
                &mcp_tools,
                &cwd,
                local_plugin.as_ref(),
                &mut mcp_tool_names,
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

        // ── 后台任务轮询 ──────────────────────────────────────────────
        // 退出中（should_quit）时必须整段跳过：kill_all() 会给所有 Running job
        // 发 SIGKILL，而 spawn_prepared 里的 child.wait() task 仍会把它们翻成
        // Exited(-1)。若不加守卫，退出那一帧会把每个后台任务都误判为"刚完成"
        // 并触发一次自动唤醒。
        if !state.should_quit {
            for id in state.poll_shells() {
                let Some(s) = state.shells.iter().find(|s| s.id == id) else {
                    continue;
                };
                if s.wake_sent {
                    continue;
                }
                let ShellStatus::Exited(code) = s.status else {
                    continue;
                };
                let command = s.command.clone();
                let elapsed = format_hms(s.elapsed_secs());
                if let Some(s) = state.shells.iter_mut().find(|s| s.id == id) {
                    s.wake_sent = true;
                }
                // 聊天流留一条可读记录（"bash_1 已退出 (code 0)"）
                state.push_tracked_message(ChatMessage::system(wyj_i18n::tr_fmt(
                    "shells.exited_notice",
                    &[
                        ("id", id.as_str()),
                        ("code", &code.to_string()),
                        ("elapsed", elapsed.as_str()),
                    ],
                )));
                // 输出回显给模型：模型不主动调 BashOutput 也能知道结果
                let output = wyj_tools::BashSessionManager::global()
                    .get(&id)
                    .map(|j| j.tail(SHELL_WAKE_TAIL_BYTES))
                    .unwrap_or_default();
                state
                    .pending_bg_reminders
                    .push(wyj_core::prompts::bg_shell_done_reminder(
                        &id,
                        &command,
                        &elapsed,
                        code,
                        &wyj_tools::textutil::truncate_head_tail(&output, 4000, 1000),
                    ));
            }
        }

        // ── 后台任务完成 → 自动唤醒 Agent 续跑 ───────────────────────
        // 排在「竞态兜底」之后：用户恰好在排队消息时优先消费用户消息
        // （它会 mem::take 走 pending_bg_reminders，下一帧再唤醒，不会丢）。
        if !state.is_thinking && !state.pending_bg_reminders.is_empty() && !state.auto_wake_paused {
            let reminders = std::mem::take(&mut state.pending_bg_reminders);
            state
                .messages
                .push(ChatMessage::system(wyj_i18n::tr("bg_wake.notice")));
            state.begin_new_turn();
            state.is_thinking = true;
            state.spinner_frame = 0;
            state.begin_turn_timing();
            let agent_c = shared_agent.read().unwrap().clone();
            let (task, injector) = spawn_agent_turn(
                None, // 自动唤醒：reminder 本身即这一轮唯一 user 输入
                vec![],
                agent_c,
                session.clone(),
                agent_tx.clone(),
                cwd.clone(),
                shared_mode.clone(),
                shared_permission.clone(),
                ui_ask_tx.clone(),
                reminders,
                plugin_runtime.clone(),
            );
            state.current_task = Some(task);
            state.injector = Some(injector);
        }

        // 竞态兜底：极小概率下，用户提交补充消息的时机恰好晚于 run_turn 最后一次
        // 排空注入队列的检查，导致该轮 TurnDone/Error 已到达而消息仍留在
        // pending_queue 里未被消费。此时视同用户在轮次结束的瞬间正常发送。
        //
        // 镜像与共享队列的对齐见 `reconcile_pending_queue`：它在 TurnDone/Error
        // 里、`injector` 还没被清空之前就做完了，因此这里拿到的
        // `pending_queue` 一定只含**确实没被任何 Agent 取走**的消息。
        if !state.is_thinking && !state.pending_queue.is_empty() {
            let queued = std::mem::take(&mut state.pending_queue);
            state.injector = None;
            let mut combined_text = String::new();
            let mut combined_display = String::new();
            let mut combined_attachments = vec![];
            for (i, message) in queued.into_iter().enumerate() {
                if i > 0 {
                    combined_text.push_str("\n---\n");
                    combined_display.push_str("\n---\n");
                }
                combined_text.push_str(&message.text);
                combined_display.push_str(&message.display);
                combined_attachments.extend(message.attachments);
            }
            // 不再 push_user：每条排队消息在**入队那一刻**就已回显（见 Enter 分支
            // 的不变式注释），这里重画一遍就是同一条消息出现两次。
            state.input_history.push(combined_text.clone());
            state.begin_new_turn();
            state.is_thinking = true;
            state.spinner_frame = 0;
            state.begin_turn_timing();
            let agent_c = shared_agent.read().unwrap().clone();
            let (task, injector) = spawn_agent_turn(
                Some(combined_text),
                combined_attachments,
                agent_c,
                session.clone(),
                agent_tx.clone(),
                cwd.clone(),
                shared_mode.clone(),
                shared_permission.clone(),
                ui_ask_tx.clone(),
                std::mem::take(&mut state.pending_bg_reminders),
                plugin_runtime.clone(),
            );
            state.current_task = Some(task);
            state.injector = Some(injector);
        }

        // 每轮对话结束后自动保存 session 文件
        if state.save_needed {
            state.save_needed = false;
            if let Some(store) = &session_store {
                // 回合正在跑时锁被占住：**不在这里等**，下一帧再存。
                // 这正是「执行久了界面卡死」的现场——主循环停在这一行，
                // 后面的 `event::poll()` 永远到不了：不重绘、不读按键，
                // 连上游的回合心跳看门狗都跑不到下一轮。
                //
                // **这里绝不能 `continue`**：`continue` 跳回的是整个主循环，
                // 被跳过的正是紧随其后的 `ui_ask_rx` 消费与 `event::poll()`
                // ——按键、权限确认弹窗、AskQuestion 全部停摆。而
                // `terminal.draw()` 在本行之前、画面照常刷新，看起来「活着」
                // 却按键全哑。`TurnDone` 置位 `save_needed` 与 11450/11480
                // 的自动唤醒 spawn 发生在同一帧，因此**每一条排队消息触发的
                // 自动续跑都会踩中**，整个新回合全程失灵。
                // 存盘晚一帧没有代价，等锁才有：拿不到就只置位回退，流程
                // 自然落到下面继续消费输入层。
                if let Some(sess) = try_session_lock(&session) {
                    if !sess.messages.is_empty() {
                        // 保留已有的 LLM 生成标题（若已生成则不覆盖为启发式 title）
                        let (title, title_generated) = match session_store
                            .as_ref()
                            .and_then(|s| s.load(&current_session_id).ok())
                        {
                            Some(f) if f.title_generated => (f.title, true),
                            _ => (extract_title(&sess.messages), false),
                        };
                        let sf = SessionFile::from_session(
                            &sess,
                            SessionFileMeta {
                                session_id: current_session_id.clone(),
                                title,
                                last_preview: extract_preview(&sess.messages),
                                cwd: cwd.display().to_string(),
                                timestamp: now_iso(),
                                turns: state.turns,
                                title_generated,
                            },
                        );
                        let _ = store.save(&sf);
                    }
                } else {
                    state.save_needed = true;
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

        let classified_arrow_key = classified_arrow_keys.pop_front();
        if classified_arrow_key.is_some() || event::poll(std::time::Duration::from_millis(50))? {
            let arrow_was_classified = classified_arrow_key.is_some();
            let ev = match classified_arrow_key {
                Some(key) => Event::Key(key),
                None => event::read()?,
            };

            if wheel_routing.distinguishes_synthetic_arrows() && !arrow_was_classified {
                if let Event::Key(key) = &ev {
                    let routed = ghostty_arrow_router.route(*key);
                    if routed.scroll_lines != 0 {
                        state.scroll_focus_lines(routed.scroll_lines);
                    }
                    match routed.action {
                        ArrowRoutingAction::Pass => {}
                        ArrowRoutingAction::Defer => continue,
                        ArrowRoutingAction::QueuePhysical { code, count } => {
                            classified_arrow_keys.extend(
                                (0..count).map(|_| KeyEvent::new(code, KeyModifiers::NONE)),
                            );
                            continue;
                        }
                    }
                }
            }

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

                    // AskQuestion / 计划批准的"其他"自由输入同理：粘贴进弹窗自己的
                    // 输入框，不能穿透成一条新的聊天消息
                    if let Some(dlg) = &mut state.ask_question_dialog {
                        if let Some(ib) = dlg.freetext_input_mut() {
                            ib.insert_text(&pasted);
                            continue;
                        }
                    }
                    if let Some(dlg) = &mut state.plan_dialog {
                        if let Some(ib) = dlg.freetext_input_mut() {
                            ib.insert_text(&pasted);
                            continue;
                        }
                    }

                    // 文本 bracketed paste 和应用直读 Ctrl+V 共用同一套附件/文字逻辑。
                    // 这里仍优先探测图片，以兼容同时暴露图片与文本 flavor 的剪贴板。
                    let paste = read_clipboard_image().unwrap_or(ClipboardPaste::Text(pasted));
                    state.paste_hint = apply_clipboard_paste(&mut state, &mut input, paste);
                    update_slash_completions(&mut state, &input, &cmd_registry);
                    update_file_completions(&mut state, &input, &cwd);
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META,
                        ) =>
                    {
                        if let Some(target) =
                            state.hyperlink_registry.target_at(mouse.column, mouse.row)
                        {
                            if let Err(error) = open_target(&target) {
                                state.push_tracked_message(ChatMessage::assistant_err(
                                    wyj_i18n::tr_fmt(
                                        "hyperlink.open_failed",
                                        &[("error", error.to_string().as_str())],
                                    ),
                                ));
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => state.scroll_focus_lines(-3),
                    MouseEventKind::ScrollDown => state.scroll_focus_lines(3),
                    _ => {}
                },
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
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
                                // 本分支内有 4 处要动 session，在**入口一次取锁**复用。
                                // 拿不到就保留选择器并提示，绝不在这里等——回合 task 会
                                // 持锁跑完整个回合，主循环一停就是「卡死」：不重绘、不读
                                // 按键，连 ESC/Ctrl+C 都救不回来。
                                let Some(mut sess) = try_session_lock(&session) else {
                                    state.notify_session_busy();
                                    continue;
                                };
                                if let Some(picker) = state.session_picker.take() {
                                    if picker.selected == 0 {
                                        // 新建会话：自动保存当前后重置
                                        if let Some(store) = &session_store {
                                            if !sess.messages.is_empty() {
                                                let (title, title_generated) = match store
                                                    .load(&current_session_id)
                                                    .ok()
                                                {
                                                    Some(f) if f.title_generated => (f.title, true),
                                                    _ => (extract_title(&sess.messages), false),
                                                };
                                                let _ = store.save(&SessionFile::from_session(
                                                    &sess,
                                                    SessionFileMeta {
                                                        session_id: current_session_id.clone(),
                                                        title,
                                                        last_preview: extract_preview(
                                                            &sess.messages,
                                                        ),
                                                        cwd: cwd.display().to_string(),
                                                        timestamp: now_iso(),
                                                        turns: state.turns,
                                                        title_generated,
                                                    },
                                                ));
                                            }
                                        }
                                        *sess = Session::new();
                                        drop(sess);
                                        current_session_id = new_session_id();
                                        state.reset_for_new_session();
                                        state.current_session_id = current_session_id.clone();
                                        retarget_agent_session(
                                            &shared_agent,
                                            session_store.as_ref(),
                                            &current_session_id,
                                        );
                                        state
                                            .messages
                                            .push(ChatMessage::system("已开始新会话".to_string()));
                                    } else {
                                        // 切换到选定历史会话
                                        let meta = &picker.sessions[picker.selected - 1];
                                        if let Some(store) = &session_store {
                                            // 自动保存当前会话
                                            {
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
                                                    let _ = store.save(&SessionFile::from_session(
                                                        &sess,
                                                        SessionFileMeta {
                                                            session_id: current_session_id.clone(),
                                                            title,
                                                            last_preview: extract_preview(
                                                                &sess.messages,
                                                            ),
                                                            cwd: cwd.display().to_string(),
                                                            timestamp: now_iso(),
                                                            turns: state.turns,
                                                            title_generated,
                                                        },
                                                    ));
                                                }
                                            }
                                            // 加载目标会话
                                            match store.load(&meta.session_id) {
                                                Ok(file) => {
                                                    let display_msgs =
                                                        reconstruct_display(&file.messages);
                                                    sess.restore_usage_from(&file);
                                                    sess.messages = file.messages;
                                                    let compact_count = sess.compact_count;
                                                    let plan_approved =
                                                        has_plan_approved(&sess.messages);
                                                    drop(sess);
                                                    current_session_id = file.session_id.clone();
                                                    state.current_session_id =
                                                        current_session_id.clone();
                                                    retarget_agent_session(
                                                        &shared_agent,
                                                        session_store.as_ref(),
                                                        &current_session_id,
                                                    );
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
                                                    state.compact_count = compact_count;
                                                    state.turns = file.turns;
                                                    state.frozen_up_to = 0;
                                                    state.welcome_frozen = true;
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
                                    InputOwner::Schedule(_) => {
                                        // Schedule 借用同 Profile，不对应任何需要
                                        // 重置的 overlay。
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
                                    InputOwner::Skills(SkillsInputField::BrowseSearch) => {
                                        let query = text.filter(|t| !t.trim().is_empty());
                                        state.input_owner = None;
                                        match query {
                                            Some(q) => {
                                                if let Some(dialog) = &mut state.skills_dialog {
                                                    dialog.tab = SkillsDialogTab::Browse;
                                                }
                                                start_registry_search(&mut state, &agent_tx, q);
                                                // 关键字留在 live_input 里不清空，方便
                                                // 同一次会话里微调关键字重新搜索。
                                            }
                                            None => {
                                                if let Some(dialog) = &mut state.skills_dialog {
                                                    dialog.overlay = SkillsOverlay::None;
                                                }
                                            }
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
                                    InputOwner::Schedule(ScheduleInputField::Field {
                                        task_idx,
                                        field_idx,
                                    }) => {
                                        let value = text.unwrap_or_default();
                                        owner.clear_live_input(&mut state);
                                        state.input_owner = None;
                                        if let Some(dialog) = &mut state.schedule_dialog {
                                            dialog.set_field_text(task_idx, field_idx, value);
                                            dialog.error = None;
                                        }
                                    }
                                    InputOwner::Schedule(ScheduleInputField::Frequency {
                                        task_idx,
                                        kind,
                                    }) => {
                                        let value = text.unwrap_or_default();
                                        owner.clear_live_input(&mut state);
                                        state.input_owner = None;
                                        match schedule_parse_frequency_input(kind, &value) {
                                            Ok(cron_expr) => {
                                                if let Some(dialog) = &mut state.schedule_dialog {
                                                    dialog.tasks[task_idx].cron = cron_expr;
                                                    dialog.error = None;
                                                }
                                            }
                                            Err(msg) => {
                                                if let Some(dialog) = &mut state.schedule_dialog {
                                                    dialog.error = Some(msg);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // 退格/删除/左右/Home/End 与主输入框、弹窗自由文本
                            // 走同一份实现（此前这里是手写的另一套，缺 Ctrl+U/A/E/K/W
                            // 与 Alt+←→，同一个编辑动作换个界面就按不动）。
                            // 配置输入框不接受附件，所以没有附件标记要对齐。
                            KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::Home
                            | KeyCode::End => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    crate::input::apply_edit_key(ib, &key);
                                }
                            }
                            _ if is_clipboard_paste_key(key) => {
                                // 配置输入框不接受图片附件，但 Ctrl+V 仍应粘贴文字，
                                // 不能像此前那样把快捷键误写成一个字面量 `v`。
                                if let Some(text) = read_clipboard_text() {
                                    if let Some(ib) = owner.live_input_mut(&mut state) {
                                        ib.insert_text(&text);
                                    }
                                }
                            }
                            KeyCode::Char(c) => {
                                if let Some(ib) = owner.live_input_mut(&mut state) {
                                    if !crate::input::apply_edit_key(ib, &key) {
                                        ib.insert_char(c);
                                    }
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
                                                        let new_agent = attach_agent_session(
                                                            new_agent,
                                                            session_store.as_ref(),
                                                            &current_session_id,
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

                    // ⓪.54 证据化自进化治理中心（/evolve 命令触发）
                    if state.evolution_dialog.is_some() {
                        let pending_confirmation = state
                            .evolution_dialog
                            .as_ref()
                            .and_then(|dialog| dialog.confirm.clone());
                        if let Some(action) = pending_confirmation {
                            match key.code {
                                KeyCode::Enter => {
                                    let store = state
                                        .evolution_dialog
                                        .as_ref()
                                        .expect("Evolution dialog exists")
                                        .store
                                        .clone();
                                    let result = execute_evolution_action(
                                        action,
                                        store,
                                        &cwd,
                                        session_store.as_ref(),
                                        &current_session_id,
                                        &session,
                                    )
                                    .await;
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.confirm = None;
                                        match result {
                                            Ok(message) => {
                                                dialog.reload();
                                                dialog.error = Some(message);
                                            }
                                            Err(error) => dialog.error = Some(error.to_string()),
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.confirm = None;
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }

                        let row = state
                            .evolution_dialog
                            .as_ref()
                            .and_then(|dialog| dialog.rows.get(dialog.selected))
                            .cloned();
                        match key.code {
                            KeyCode::Left | KeyCode::BackTab => {
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    dialog.cycle_view(false);
                                }
                            }
                            KeyCode::Right | KeyCode::Tab => {
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    dialog.cycle_view(true);
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    dialog.selected = dialog.selected.saturating_sub(1);
                                    dialog.detail_scroll = 0;
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    if dialog.selected + 1 < dialog.rows.len() {
                                        dialog.selected += 1;
                                    }
                                    dialog.detail_scroll = 0;
                                }
                            }
                            KeyCode::PageUp => {
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    dialog.detail_scroll = dialog.detail_scroll.saturating_sub(5);
                                }
                            }
                            KeyCode::PageDown => {
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    dialog.detail_scroll = dialog.detail_scroll.saturating_add(5);
                                }
                            }
                            KeyCode::Char('a') => {
                                let action = match row {
                                    Some(EvolutionRow::Memory(memory))
                                        if memory.status != MemoryStatus::Active =>
                                    {
                                        Some(EvolutionConfirmAction::ApproveMemory(memory.id))
                                    }
                                    Some(EvolutionRow::Candidate(candidate))
                                        if candidate.status != CandidateStatus::Active =>
                                    {
                                        Some(EvolutionConfirmAction::ApproveCandidate(candidate.id))
                                    }
                                    _ => None,
                                };
                                if let Some(dialog) = &mut state.evolution_dialog {
                                    dialog.confirm = action;
                                }
                            }
                            KeyCode::Char('r') => {
                                if let Some(EvolutionRow::Candidate(candidate)) = row {
                                    if candidate.status != CandidateStatus::Active {
                                        if let Some(dialog) = &mut state.evolution_dialog {
                                            dialog.confirm =
                                                Some(EvolutionConfirmAction::RejectCandidate(
                                                    candidate.id,
                                                ));
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('f') => {
                                if let Some(EvolutionRow::Memory(memory)) = row {
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.confirm =
                                            Some(EvolutionConfirmAction::ForgetMemory(memory.id));
                                    }
                                }
                            }
                            KeyCode::Char('b') => {
                                if let Some(EvolutionRow::Candidate(candidate)) = row {
                                    if candidate.status == CandidateStatus::Active {
                                        if let Some(dialog) = &mut state.evolution_dialog {
                                            dialog.confirm =
                                                Some(EvolutionConfirmAction::RollbackCandidate(
                                                    candidate.id,
                                                ));
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('p') => {
                                if let Some(EvolutionRow::Memory(memory)) = row {
                                    let result = state
                                        .evolution_dialog
                                        .as_ref()
                                        .expect("Evolution dialog exists")
                                        .store
                                        .pin_memory(&memory.id, !memory.pinned);
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.reload();
                                        dialog.error = result.err().map(|error| error.to_string());
                                    }
                                }
                            }
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                if let Some(EvolutionRow::Episode(episode)) = row {
                                    let result = state
                                        .evolution_dialog
                                        .as_ref()
                                        .expect("Evolution dialog exists")
                                        .store
                                        .feedback_episode(
                                            &episode.id,
                                            true,
                                            Some("accepted from /evolve".into()),
                                            true,
                                        );
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.reload();
                                        dialog.error = result.err().map(|error| error.to_string());
                                    }
                                }
                            }
                            KeyCode::Char('-') => {
                                if let Some(EvolutionRow::Episode(episode)) = row {
                                    let result = state
                                        .evolution_dialog
                                        .as_ref()
                                        .expect("Evolution dialog exists")
                                        .store
                                        .feedback_episode(
                                            &episode.id,
                                            false,
                                            Some("rejected from /evolve".into()),
                                            true,
                                        );
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.reload();
                                        dialog.error = result.err().map(|error| error.to_string());
                                    }
                                }
                            }
                            KeyCode::Char('i') => {
                                if let Some(EvolutionRow::Episode(episode)) = row {
                                    let result = state
                                        .evolution_dialog
                                        .as_ref()
                                        .expect("Evolution dialog exists")
                                        .store
                                        .include_episode(&episode.id);
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        dialog.reload();
                                        dialog.error = result.err().map(|error| error.to_string());
                                    }
                                }
                            }
                            KeyCode::Char('s') => {
                                if let Some(EvolutionRow::Episode(episode)) = row {
                                    let result = state
                                        .evolution_dialog
                                        .as_ref()
                                        .expect("Evolution dialog exists")
                                        .store
                                        .create_skill_candidate_from_episode(&episode.id);
                                    if let Some(dialog) = &mut state.evolution_dialog {
                                        let message = match result {
                                            Ok(id) => Some(format!("已生成 Skill 候选 {id}")),
                                            Err(error) => Some(error.to_string()),
                                        };
                                        dialog.reload();
                                        dialog.error = message;
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                state.evolution_dialog = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.55 AGENTS.md 记忆面板拦截（/memory 命令触发）
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
                                    ClearAll,
                                    None,
                                }
                                // 已处于二次确认态：Enter/Space 直接执行 clear_all。
                                let pending = state
                                    .memory_dialog
                                    .as_ref()
                                    .map(|d| d.pending_clear_all)
                                    .unwrap_or(false);
                                if pending {
                                    if let Some(dialog) = &mut state.memory_dialog {
                                        dialog.pending_clear_all = false;
                                    }
                                    execute_memory_clear_all(&shared_agent, &mut state);
                                    continue;
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
                                    Some(MemoryRow::ClearAll { .. }) => MemoryRowAction::ClearAll,
                                    _ => MemoryRowAction::None,
                                };
                                match action {
                                    MemoryRowAction::Open(path) => {
                                        let result =
                                            open_path_in_editor(terminal, &path, wheel_routing)
                                                .await;
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
                                        if let Some(memory) =
                                            shared_agent.read().unwrap().memory_v3_ref()
                                        {
                                            memory.set_enabled(new_value);
                                        }
                                        if let Some(mem) = shared_agent.read().unwrap().memory_ref()
                                        {
                                            mem.set_enabled(new_value);
                                        }
                                        if let Some(dialog) = &mut state.memory_dialog {
                                            dialog.auto_memory_enabled = new_value;
                                            dialog.error = save_result.err().map(|e| e.to_string());
                                        }
                                    }
                                    MemoryRowAction::ClearAll => {
                                        // 进入二次确认态，等待用户再次按 y/Enter 才执行。
                                        if let Some(dialog) = &mut state.memory_dialog {
                                            dialog.pending_clear_all = true;
                                            dialog.error = None;
                                        }
                                    }
                                    MemoryRowAction::None => {}
                                }
                            }
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                // 与 Enter 行为一致：进入二次确认态或执行。
                                let pending = state
                                    .memory_dialog
                                    .as_ref()
                                    .map(|d| d.pending_clear_all)
                                    .unwrap_or(false);
                                if pending {
                                    if let Some(dialog) = &mut state.memory_dialog {
                                        dialog.pending_clear_all = false;
                                    }
                                    execute_memory_clear_all(&shared_agent, &mut state);
                                } else if let Some(dialog) = &mut state.memory_dialog {
                                    if let Some(MemoryRow::ClearAll { .. }) =
                                        dialog.rows.get(dialog.selected)
                                    {
                                        dialog.pending_clear_all = true;
                                        dialog.error = None;
                                    }
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                // 二次确认态下按 n 视为取消（与 Esc 等价）。
                                if let Some(dialog) = &mut state.memory_dialog {
                                    if dialog.pending_clear_all {
                                        dialog.pending_clear_all = false;
                                        dialog.error = None;
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                // 二次确认态下 Esc 仅清 pending，不关闭面板；
                                // 让用户能直接再选别的行。
                                if let Some(dialog) = &mut state.memory_dialog {
                                    if dialog.pending_clear_all {
                                        dialog.pending_clear_all = false;
                                        dialog.error = None;
                                        continue;
                                    }
                                }
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
                                            if result.is_ok() {
                                                reload_config_into_state(&mut state);
                                            }
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
                            SkillsOverlay::Syncing { .. }
                            | SkillsOverlay::Upgrading { .. }
                            | SkillsOverlay::Searching
                            | SkillsOverlay::FetchingArchive => {
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
                            SkillsOverlay::RegistryInstallConfirm { .. } => {
                                match key.code {
                                    KeyCode::Left | KeyCode::Right => {
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            if let SkillsOverlay::RegistryInstallConfirm {
                                                scope,
                                                ..
                                            } = &mut dialog.overlay
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
                                        // archive 用 mem::take 移出而不是 clone：确认框
                                        // 借用了 dialog，而安装又要读 state.cwd，
                                        // 两个借用不能同时活着，顺带省掉一次几 MB 的拷贝。
                                        let pending =
                                            state.skills_dialog.as_mut().and_then(|d| match &mut d
                                                .overlay
                                            {
                                                SkillsOverlay::RegistryInstallConfirm {
                                                    skill,
                                                    archive,
                                                    scope,
                                                    ..
                                                } => Some((
                                                    skill.as_ref().clone(),
                                                    std::mem::take(archive),
                                                    *scope,
                                                    d.registry_base_url.clone(),
                                                )),
                                                _ => None,
                                            });
                                        if let Some((skill, archive, scope, base_url)) = pending {
                                            let install_name = skill.install_name();
                                            let result =
                                                wyj_store::skillhub::install_skillhub_archive(
                                                    &archive,
                                                    &wyj_store::skillhub::SkillHubInstallRequest {
                                                        base_url: base_url.clone(),
                                                        slug: skill.slug.clone(),
                                                        namespace: skill.namespace.clone(),
                                                        install_name: install_name.clone(),
                                                        version: skill.version.clone(),
                                                        scope,
                                                        source_label: skill.canonical_name.clone(),
                                                    },
                                                    &state.cwd,
                                                );
                                            let home = wyj_config::home_dir().unwrap_or_default();
                                            if let Some(dialog) = &mut state.skills_dialog {
                                                dialog.overlay = SkillsOverlay::None;
                                                match result {
                                                    Ok(_) => {
                                                        dialog.status = Some(wyj_i18n::tr_fmt(
                                                            "skills.install.registry_done",
                                                            &[("name", &install_name)],
                                                        ));
                                                        dialog.refresh_installed(&home, &state.cwd);
                                                    }
                                                    Err(e) => {
                                                        dialog.error = Some(wyj_i18n::tr_fmt(
                                                            "skills.error.registry_install_failed",
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
                                        // 同一个 AddNew 行在两个 tab 里含义不同：
                                        // Marketplaces tab 是"添加 git 源"，Browse tab
                                        // 固定占据位置 0，含义是"输入关键字搜索"。
                                        let browse_tab = state
                                            .skills_dialog
                                            .as_ref()
                                            .map(|d| d.tab == SkillsDialogTab::Browse)
                                            .unwrap_or(false);
                                        if let Some(dialog) = &mut state.skills_dialog {
                                            dialog.overlay = if browse_tab {
                                                SkillsOverlay::None
                                            } else {
                                                SkillsOverlay::AddMarketplace
                                            };
                                            dialog.live_input = InputBox::new();
                                        }
                                        state.input_owner =
                                            Some(InputOwner::Skills(if browse_tab {
                                                SkillsInputField::BrowseSearch
                                            } else {
                                                SkillsInputField::AddMarketplaceUrl
                                            }));
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
                                state.ui_focus = UiFocus::Composer;
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
                                        session_store.as_ref(),
                                        &current_session_id,
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
                                    session_store.as_ref(),
                                    &current_session_id,
                                );
                                if saved {
                                    state.profile_dialog = None;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // ⓪.61 定时任务面板拦截（/schedule 命令触发）
                    if state.schedule_dialog.is_some() {
                        // 字段编辑/频率输入的文本输入统一走上面的 ⓪.1 输入借用拦截
                        // （state.input_owner == Some(Schedule(_))），不会进入这里。

                        if state.schedule_dialog.as_ref().unwrap().menu.is_some() {
                            schedule_handle_menu_key(&mut state, key.code);
                            continue;
                        }

                        // ── 保存成功但 crontab 同步失败：纯提示，Esc/Enter 收起
                        if matches!(
                            state.schedule_dialog.as_ref().unwrap().overlay,
                            ScheduleOverlay::SyncError { .. }
                        ) {
                            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                                if let Some(dialog) = &mut state.schedule_dialog {
                                    dialog.overlay = ScheduleOverlay::None;
                                }
                            }
                            continue;
                        }

                        // ── Esc 未保存修改三选一确认
                        if matches!(
                            state.schedule_dialog.as_ref().unwrap().overlay,
                            ScheduleOverlay::UnsavedChanges { .. }
                        ) {
                            enum Choice {
                                None,
                                SaveClose,
                                DiscardClose,
                                BackToPanel,
                            }
                            let mut choice = Choice::None;
                            if let Some(dialog) = &mut state.schedule_dialog {
                                if let ScheduleOverlay::UnsavedChanges { selected } =
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
                                        KeyCode::Esc => {
                                            dialog.overlay = ScheduleOverlay::None;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            match choice {
                                Choice::None => {}
                                Choice::SaveClose => {
                                    let saved = schedule_try_save(&mut state);
                                    let sync_failed = matches!(
                                        state.schedule_dialog.as_ref().map(|d| &d.overlay),
                                        Some(ScheduleOverlay::SyncError { .. })
                                    );
                                    if saved && !sync_failed {
                                        state.input_owner = None;
                                        state.schedule_dialog = None;
                                    } else if let Some(dialog) = &mut state.schedule_dialog {
                                        if !sync_failed {
                                            dialog.overlay = ScheduleOverlay::None;
                                        }
                                    }
                                }
                                Choice::DiscardClose => {
                                    state.input_owner = None;
                                    state.schedule_dialog = None;
                                }
                                Choice::BackToPanel => {
                                    if let Some(dialog) = &mut state.schedule_dialog {
                                        dialog.overlay = ScheduleOverlay::None;
                                    }
                                }
                            }
                            continue;
                        }

                        // ── 无 overlay/菜单：方向键导航 ─────────────────────────
                        match key.code {
                            KeyCode::Esc => {
                                if let Some(dialog) = &mut state.schedule_dialog {
                                    if dialog.is_dirty() {
                                        dialog.overlay =
                                            ScheduleOverlay::UnsavedChanges { selected: 0 };
                                    } else {
                                        state.schedule_dialog = None;
                                    }
                                }
                            }
                            KeyCode::Up => {
                                if let Some(dialog) = &mut state.schedule_dialog {
                                    dialog.cursor = dialog.cursor.saturating_sub(1);
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dialog) = &mut state.schedule_dialog {
                                    let len = dialog.rows().len();
                                    if dialog.cursor + 1 < len {
                                        dialog.cursor += 1;
                                    }
                                    dialog.error = None;
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(dialog) = &mut state.schedule_dialog {
                                    dialog.menu = dialog.build_menu();
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let saved = schedule_try_save(&mut state);
                                let sync_failed = matches!(
                                    state.schedule_dialog.as_ref().map(|d| &d.overlay),
                                    Some(ScheduleOverlay::SyncError { .. })
                                );
                                if saved && !sync_failed {
                                    state.schedule_dialog = None;
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
                        match dlg.handle_key_event(key) {
                            PlanApprovalOutcome::Continue => {}
                            PlanApprovalOutcome::Approve => {
                                if let Some(dlg) = state.plan_dialog.take() {
                                    // 锁顺序硬约束：必须先 send 再取 session 锁。
                                    // Agent task 全程持有 session Mutex 等待本弹窗的
                                    // oneshot（crates/tools/src/ctx.rs::exit_plan_mode），
                                    // 若这里先 `session.lock().await` 建 checkpoint，
                                    // UI 会等一把 Agent 正持着、而 Agent 又在等我们
                                    // send 的锁 —— 100% 死锁，且 ESC/interrupt 都救不
                                    // 回来（interrupt 也走主循环，同样卡在这）。
                                    let _ = dlg.response_tx.send(true);
                                    spawn_plan_approval_checkpoint(
                                        session.clone(),
                                        cwd.clone(),
                                        current_session_id.clone(),
                                        session_store.as_ref(),
                                    );
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
                                state.enqueue_message(OutgoingMessage::plain(text));
                            }
                        }
                        continue;
                    }

                    // ①.5 检测到计划已批准仍在 plan 模式发消息 → 确认切换执行模式
                    if state.exec_mode_confirm.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if let Some(dlg) = state.exec_mode_confirm.take() {
                                    spawn_plan_approval_checkpoint(
                                        session.clone(),
                                        cwd.clone(),
                                        current_session_id.clone(),
                                        session_store.as_ref(),
                                    );
                                    let new_mode = AgentMode::Normal;
                                    switch_mode(&shared_mode, &shared_permission, new_mode.clone())
                                        .await;
                                    state.mode = new_mode;
                                    state.messages.push(ChatMessage::system(
                                        "已切换至执行模式。".to_string(),
                                    ));
                                    state.push_user(dlg.pending_display.clone());
                                    state.begin_new_turn();
                                    state.is_thinking = true;
                                    state.spinner_frame = 0;
                                    state.begin_turn_timing();
                                    let agent_c = shared_agent.read().unwrap().clone();
                                    let (task, injector) = spawn_agent_turn(
                                        Some(dlg.pending_message),
                                        dlg.pending_attachments,
                                        agent_c,
                                        session.clone(),
                                        agent_tx.clone(),
                                        cwd.clone(),
                                        shared_mode.clone(),
                                        shared_permission.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                        plugin_runtime.clone(),
                                    );
                                    state.current_task = Some(task);
                                    state.injector = Some(injector);
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                if let Some(dlg) = state.exec_mode_confirm.take() {
                                    state.push_user(dlg.pending_display.clone());
                                    state.begin_new_turn();
                                    state.is_thinking = true;
                                    state.spinner_frame = 0;
                                    state.begin_turn_timing();
                                    let agent_c = shared_agent.read().unwrap().clone();
                                    let (task, injector) = spawn_agent_turn(
                                        Some(dlg.pending_message),
                                        dlg.pending_attachments,
                                        agent_c,
                                        session.clone(),
                                        agent_tx.clone(),
                                        cwd.clone(),
                                        shared_mode.clone(),
                                        shared_permission.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                        plugin_runtime.clone(),
                                    );
                                    state.current_task = Some(task);
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
                        match dlg.handle_key_event(key) {
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

                    // ③ 工具权限确认拦截：一般工具 y/Enter=允许一次、a=始终允许；
                    //    computer-use 的 y/Enter/a 均由 ToolCtx 记为项目级授权；
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

                    // 项目级 MCP server 信任确认拦截：y/Enter=信任并连接（写盘
                    // 批准记录，下一帧 refresh_tui_mcp_runtime 会自动纳入并连接）；
                    // n/Esc/其它=本次跳过（不写盘，下次启动会重新询问）。
                    if state.pending_mcp_trust.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if let Err(e) = wyj_store::project_trust::approve(&cwd) {
                                    tracing::warn!("写入项目级 MCP 信任记录失败: {e}");
                                }
                                state.pending_mcp_trust = None;
                                state.messages.push(ChatMessage::system(
                                    "已信任并开始连接项目级 MCP server。".to_string(),
                                ));
                            }
                            KeyCode::Char('n')
                            | KeyCode::Char('N')
                            | KeyCode::Esc
                            | KeyCode::Char('d')
                            | KeyCode::Char('D') => {
                                state.pending_mcp_trust = None;
                                state.messages.push(ChatMessage::system(
                                    "已跳过，这些 server 本次不会连接（下次启动会重新询问）。"
                                        .to_string(),
                                ));
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // 项目级 skill 信任确认拦截。放在 MCP 分支**之后**：两者同时
                    // pending 时 MCP 先吃掉这次按键（它能执行任意命令，优先级更高），
                    // 答完后下一轮自然轮到 skill——不需要额外的排队逻辑。
                    if state.pending_skill_trust.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if let Err(e) = wyj_store::skill_trust::approve_skills(&cwd) {
                                    tracing::warn!("写入项目级 skill 信任记录失败: {e}");
                                }
                                state.pending_skill_trust = None;
                                // ① 重建命令注册表 —— 不做这步的症状是"按了 y 但
                                //    `/xxx` 还是敲不出来"，且没有任何报错。
                                {
                                    let disabled = wyj_store::disabled_skill_names(&cwd);
                                    let mut plugin_sources =
                                        wyj_store::plugin_install::enabled_plugin_skill_paths(&cwd);
                                    if let Some(local) = &local_plugin {
                                        plugin_sources.extend(local.skill_paths.clone());
                                    }
                                    cmd_registry = standard_registry_with_skills(
                                        &home_dir,
                                        &cwd,
                                        &disabled,
                                        &plugin_sources,
                                        ProjectSkillsGate::resolve(&cwd),
                                    );
                                    update_slash_completions(&mut state, &input, &cmd_registry);
                                }
                                state.messages.push(ChatMessage::system(
                                    "已信任，本会话即可使用这些项目 skill。".to_string(),
                                ));
                            }
                            KeyCode::Char('n')
                            | KeyCode::Char('N')
                            | KeyCode::Esc
                            | KeyCode::Char('d')
                            | KeyCode::Char('D') => {
                                state.pending_skill_trust = None;
                                state.messages.push(ChatMessage::system(
                                    "已跳过，这些项目 skill 本次不加载（下次启动会重新询问）。"
                                        .to_string(),
                                ));
                            }
                            _ => {}
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

                    // 输入框 → 内容区的显式入口。补全列表、配置输入框和各类模态面板
                    // 都在更高优先级已经拦截，因此不会被这里抢键。
                    let shift_up = is_content_focus_key(key);
                    if shift_up && !state.content_focus_active() {
                        let _ = state.enter_content_focus();
                        continue;
                    }

                    // 内容区获得焦点后，普通 ↑/↓ 用于连续导航：Chat、Todo、
                    // SubAgent 在各自边界自然跨区；TodoDetail 内则改为逐行滚动。
                    // Esc 按“详情 → 列表 → 输入框”逐级返回并始终保留草稿。
                    if state.content_focus_active() {
                        match key.code {
                            KeyCode::Up => {
                                state.move_focus_selection(-1);
                                continue;
                            }
                            KeyCode::Down => {
                                state.move_focus_selection(1);
                                continue;
                            }
                            KeyCode::PageUp => {
                                state.scroll_focus_lines(-8);
                                continue;
                            }
                            KeyCode::PageDown => {
                                state.scroll_focus_lines(8);
                                continue;
                            }
                            KeyCode::Enter | KeyCode::Char(' ')
                                if key.modifiers.is_empty()
                                    && !matches!(state.ui_focus, UiFocus::Chat) =>
                            {
                                state.toggle_focused_content_detail();
                                continue;
                            }
                            KeyCode::Esc => {
                                state.close_panel_focus();
                                continue;
                            }
                            // 裸 `k` 在后台任务面板里终止选中的进程组。输入框的
                            // Ctrl+K（kill-to-end）走的是另一条 CONTROL 分支，
                            // 两者不会撞车。
                            KeyCode::Char('k')
                                if key.modifiers.is_empty()
                                    && state.ui_focus == UiFocus::Shells =>
                            {
                                state.kill_selected_shell();
                                continue;
                            }
                            // Ctrl+T 自己负责聚焦/折叠 Todo；保持当前焦点让它能判断
                            // “已经聚焦”并延续原有第二次按下折叠/展开的语义。
                            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            }
                            // Ctrl+O 在「全部展开」与「自动模式」间切换（自动模式下正在执行的
                            // 步骤本就展开）。必须显式占位并 `continue`：否则会落进
                            // 下面的 `_` 兜底退出内容焦点。
                            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                state.output_expanded = !state.output_expanded;
                                continue;
                            }
                            _ => {
                                state.leave_content_focus();
                            }
                        }
                    }

                    // Ctrl+D → 立即退出
                    if is_quit(&key) {
                        await_quit_key_release(wheel_routing);
                        state.should_quit = true;
                        continue;
                    }

                    // Ctrl+C → 中断 Agent / 清空输入框 / 二次确认退出
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if state.is_thinking {
                            state.interrupt();
                        } else if composer_has_content(&state, &input) {
                            clear_composer(&mut state, &mut input);
                            state.ctrl_c_pressed = false;
                            state.last_ctrl_c = None;
                        } else if state.ctrl_c_pressed {
                            await_quit_key_release(wheel_routing);
                            state.should_quit = true;
                        } else {
                            state.ctrl_c_pressed = true;
                            state.last_ctrl_c = Some(Instant::now());
                        }
                        continue;
                    }

                    // ESC → 中断 Agent / 空闲时一次清空当前输入草稿（文字 + 附件）
                    if key.code == KeyCode::Esc {
                        if state.close_panel_focus() {
                            continue;
                        }
                        if state.is_thinking {
                            state.interrupt();
                        } else if composer_has_content(&state, &input) {
                            clear_composer(&mut state, &mut input);
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

                    // 换行三种等价写法：Shift+Enter（终端上报键盘协议时可用）、
                    // Ctrl+J（到处可用）、行尾 `\` + Enter（连 Ctrl 都收不到时）。
                    // 后两种与 Claude Code 文档推荐的写法一致。
                    let newline = (key.code == KeyCode::Enter
                        && key.modifiers.contains(KeyModifiers::SHIFT))
                        || (matches!(key.code, KeyCode::Char('j' | 'J'))
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                        || (key.code == KeyCode::Enter
                            && !key.modifiers.contains(KeyModifiers::SHIFT)
                            && !key.modifiers.contains(KeyModifiers::CONTROL)
                            && crate::input::trailing_backslash_before_cursor(&input));
                    if newline {
                        if key.code == KeyCode::Enter
                            && !key.modifiers.contains(KeyModifiers::SHIFT)
                        {
                            // 行尾反斜杠只是"换行记号"，不进正文
                            input.backspace();
                        }
                        input.insert_newline();
                        state.slash_completions.clear();
                    } else if key.code == KeyCode::Enter && !state.is_thinking {
                        if composer_has_content(&state, &input) {
                            // 附件占位符的位置只存在于输入框内部（`take()` 之后
                            // 标记就没了），先留一份快照给聊天区展示用。
                            let composer_snapshot = input.clone();
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
                                let inline_cwd = cwd.clone();
                                tokio::spawn(async move {
                                    let elapsed;
                                    let (output, exit_code) =
                                        match tokio::process::Command::new("/bin/bash")
                                            .arg("-c")
                                            .arg(&cmd_str)
                                            .current_dir(&inline_cwd)
                                            .stdin(std::process::Stdio::null())
                                            .stdout(std::process::Stdio::piped())
                                            .stderr(std::process::Stdio::piped())
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
                                ProjectSkillsGate::resolve(&cwd),
                            );
                            let (
                                audit,
                                estimated,
                                cache_read,
                                cache_write,
                                api_calls,
                                prompt_cache_state,
                                compact_count,
                                context_edit,
                                sub_usage,
                            ) = {
                                let sess = match try_session_lock(&session) {
                                    Some(sess) => sess,
                                    None => {
                                        state.notify_session_busy();
                                        continue;
                                    }
                                };
                                // 优先用 Agent 侧算好的 request 级分解（与自动压缩
                                // 阈值同源）；本会话还没发过模型请求时才退回
                                // messages-only 估算。
                                let audit = if sess.context_audit.total() > 0 {
                                    Some(sess.context_audit)
                                } else {
                                    None
                                };
                                let estimated = audit
                                    .map(|a| a.total())
                                    .unwrap_or_else(|| wyj_core::estimate_tokens(&sess.messages));
                                (
                                    audit,
                                    estimated,
                                    sess.total_cache_read_tokens,
                                    sess.total_cache_write_tokens,
                                    sess.api_calls,
                                    sess.prompt_cache_state,
                                    sess.compact_count,
                                    (
                                        wyj_core::context_edit::elided_count(&sess.messages) as u32,
                                        sess.context_edit_freed_tokens,
                                    ),
                                    // 子 Agent 用量取自 Session 的累加器而非
                                    // `state.sub_*`：后者只收 SubAgentEvent 的
                                    // input/output，**不含 cache_read**，且
                                    // `/resume` 后归零。子 Agent 的开销大头恰恰是
                                    // 缓存命中，丢掉它 /cost 会少算一大截。
                                    sess.sub_usage(),
                                )
                            };
                            let cmd_ctx = CommandContext {
                                cwd: cwd.clone(),
                                model: state.model_name.clone(),
                                terminal_width: state.chat_content_width,
                                input_tokens: state.total_input_tokens,
                                output_tokens: state.total_output_tokens,
                                cache_read_tokens: cache_read,
                                cache_write_tokens: cache_write,
                                api_calls,
                                prompt_cache_state,
                                context_window,
                                estimated_tokens: estimated,
                                context_audit: audit,
                                compact_count,
                                context_edit,
                                home_dir: std::env::var("HOME")
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_default(),
                                sub_input_tokens: sub_usage.input,
                                sub_output_tokens: sub_usage.output,
                                sub_cache_read_tokens: sub_usage.cache_read,
                                sub_cache_write_tokens: sub_usage.cache_write,
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
                                    Ok(CommandResult::OutputPlain(out)) => {
                                        // 保留原始排版：不走 markdown，否则列对齐
                                        // 会被段落折行揉碎。
                                        state.messages.push(ChatMessage::preformatted(out));
                                    }
                                    Ok(CommandResult::ClearHistory) => {
                                        // 先取锁再做这一大串状态重置：反过来会让用户看到
                                        // 「屏清了但对话还在」的闪回（拿不到锁就整条命令放弃）
                                        let mut sess = match try_session_lock(&session) {
                                            Some(sess) => sess,
                                            None => {
                                                state.notify_session_busy();
                                                continue;
                                            }
                                        };
                                        state.messages.clear();
                                        state.total_input_tokens = 0;
                                        state.total_output_tokens = 0;
                                        state.compact_count = 0;
                                        state.tool_schema_tokens = 0;
                                        state.tool_schema_tokens_saved = 0;
                                        state.pending_attachments.clear();
                                        state.turns = 0;
                                        state.tool_call_count = 0;
                                        state.tool_info.clear();
                                        state.current_todos = None;
                                        state.todo_stats.clear();
                                        state.todo_execution_logs.clear();
                                        state.todo_panel_expanded = false;
                                        state.selected_todo_id = None;
                                        state.todo_detail_open = false;
                                        state.todo_detail_scroll = 0;
                                        state.agents_dialog = None;
                                        state.ui_focus = UiFocus::Composer;
                                        state.sub_input_tokens = 0;
                                        state.sub_output_tokens = 0;
                                        state.sub_agent_trace_cache.clear();
                                        state.pending_queue.clear();
                                        state.pending_bg_reminders.clear();
                                        state.streaming_buf.clear();
                                        state.thinking_buf.clear();
                                        state.thinking_started = None;
                                        state.output_expanded = false;
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
                                        // /clear 会把 elided_blobs 一起清空，必须先
                                        // release：CAS 的 gc 只回收 ref_count == 0 的
                                        // blob，不 release 就等于永久泄漏。
                                        let released =
                                            wyj_core::context_edit::release_session_blobs(
                                                workspace_cas.as_deref(),
                                                &sess.take_elided_blobs(),
                                            );
                                        if released > 0 {
                                            tracing::debug!(
                                                "/clear: 释放 {released} 个 CAS blob 引用"
                                            );
                                        }
                                        sess.clear_conversation();
                                        // 必须置位落盘标记：否则 /clear 只清内存，磁盘上的
                                        // <session_id>.json 仍是清空前的完整历史，用户
                                        // 随后退出或崩溃时 `-c` 会把旧对话原样恢复。
                                        // 与 rewind(app.rs:12978) / branch(app.rs:13077) 对齐。
                                        state.save_needed = true;
                                        state.messages.push(ChatMessage::assistant(
                                            "对话已清空。".to_string(),
                                        ));
                                    }
                                    Ok(CommandResult::CompactHistory) => {
                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let mut sess = match try_session_lock(&session) {
                                            Some(sess) => sess,
                                            None => {
                                                state.notify_session_busy();
                                                continue;
                                            }
                                        };
                                        match agent_c.compact_context(&mut sess).await {
                                            Ok(r) if r.messages_removed > 0 => {
                                                state.compact_count = sess.compact_count;
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
                                    Ok(CommandResult::StartNewSession) => {
                                        // 对齐 Claude Code `/new`：开启全新会话，保存当前会话
                                        // 后分配新 id、清空 TUI 内存状态。无二次确认弹窗。
                                        if state.is_thinking {
                                            state.messages.push(ChatMessage::assistant(
                                                "请等待当前任务完成后再开新会话。".to_string(),
                                            ));
                                        } else {
                                            if let Some(store) = &session_store {
                                                let sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => {
                                                        state.notify_session_busy();
                                                        continue;
                                                    }
                                                };
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
                                                    let _ = store.save(&SessionFile::from_session(
                                                        &sess,
                                                        SessionFileMeta {
                                                            session_id: current_session_id.clone(),
                                                            title,
                                                            last_preview: extract_preview(
                                                                &sess.messages,
                                                            ),
                                                            cwd: cwd.display().to_string(),
                                                            timestamp: now_iso(),
                                                            turns: state.turns,
                                                            title_generated,
                                                        },
                                                    ));
                                                }
                                            }
                                            {
                                                let mut sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => {
                                                        state.notify_session_busy();
                                                        continue;
                                                    }
                                                };
                                                *sess = Session::new();
                                            }
                                            current_session_id = new_session_id();
                                            state.reset_for_new_session();
                                            state.current_session_id = current_session_id.clone();
                                            retarget_agent_session(
                                                &shared_agent,
                                                session_store.as_ref(),
                                                &current_session_id,
                                            );
                                            state.messages.push(ChatMessage::system(
                                                wyj_i18n::tr("new.started").to_string(),
                                            ));
                                        }
                                    }
                                    Ok(CommandResult::CreateCheckpoint { name, list }) => {
                                        match session_store.as_ref().and_then(|sessions| {
                                            wyj_core::CheckpointStore::configured(
                                                sessions.dir(),
                                                current_session_id.clone(),
                                            )
                                            .ok()
                                        }) {
                                            Some(store) if list => {
                                                match format_checkpoint_list(&store) {
                                                    Ok(text) => state
                                                        .messages
                                                        .push(ChatMessage::assistant(text)),
                                                    Err(error) => state.messages.push(
                                                        ChatMessage::assistant_err(format!(
                                                            "[checkpoint 列表失败] {error}"
                                                        )),
                                                    ),
                                                }
                                            }
                                            Some(store) => {
                                                let sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => {
                                                        state.notify_session_busy();
                                                        continue;
                                                    }
                                                };
                                                match store.create(
                                                    &cwd,
                                                    &sess.messages,
                                                    wyj_core::CheckpointKind::Manual,
                                                    name,
                                                ) {
                                                    Ok(checkpoint) => {
                                                        drop(sess);
                                                        session
                                                            .lock()
                                                            .await
                                                            .current_checkpoint_id =
                                                            Some(checkpoint.id.clone());
                                                        state.messages.push(
                                                            ChatMessage::assistant(format!(
                                                                "已创建 checkpoint {}",
                                                                checkpoint.id
                                                            )),
                                                        );
                                                    }
                                                    Err(error) => state.messages.push(
                                                        ChatMessage::assistant_err(format!(
                                                            "[checkpoint 创建失败] {error}"
                                                        )),
                                                    ),
                                                }
                                            }
                                            None => {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    "checkpoint 存储不可用".to_string(),
                                                ))
                                            }
                                        }
                                    }
                                    Ok(CommandResult::Rewind {
                                        checkpoint_id,
                                        scope,
                                        confirmed,
                                    }) => {
                                        let result: anyhow::Result<String> = async {
                                            let sessions = session_store
                                                .as_ref()
                                                .context("session 存储不可用")?;
                                            let store = wyj_core::CheckpointStore::configured(
                                                sessions.dir(),
                                                current_session_id.clone(),
                                            )?;
                                            let id = resolve_tui_checkpoint_id(
                                                &store,
                                                checkpoint_id,
                                            )?;
                                            let checkpoint = store.load(&id)?;
                                            if matches!(
                                                scope,
                                                wyj_core::RewindScope::Files
                                                    | wyj_core::RewindScope::Both
                                            ) {
                                                let preview = store.preview_files(&id, &cwd)?;
                                                if preview.requires_confirmation && !confirmed {
                                                    return Ok(format!(
                                                        "{}\n\n确认后请重新执行：`/rewind {} {:?} --confirm`",
                                                        format_rewind_preview(&preview),
                                                        id,
                                                        scope
                                                    ));
                                                }
                                            }
                                            let current_messages = match try_session_lock(&session) {
                                                Some(sess) => sess.messages.clone(),
                                                None => anyhow::bail!("{}", wyj_i18n::tr("session.busy_notice")),
                                            };
                                            let protection = store.create(
                                                &cwd,
                                                &current_messages,
                                                wyj_core::CheckpointKind::PreRewind,
                                                Some(format!("before rewind {id}")),
                                            )?;
                                            let mut file_text = String::new();
                                            if matches!(
                                                scope,
                                                wyj_core::RewindScope::Files
                                                    | wyj_core::RewindScope::Both
                                            ) {
                                                let preview =
                                                    store.restore_files(&id, &cwd, confirmed)?;
                                                file_text = format!(
                                                    "\n{}",
                                                    format_rewind_preview(&preview)
                                                );
                                            }
                                            match try_session_lock(&session) {
                                                Some(mut sess) => sess.current_checkpoint_id = Some(id.clone()),
                                                None => anyhow::bail!("{}", wyj_i18n::tr("session.busy_notice")),
                                            };
                                            if matches!(
                                                scope,
                                                wyj_core::RewindScope::Conversation
                                                    | wyj_core::RewindScope::Both
                                            ) {
                                                let mut sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => anyhow::bail!("{}", wyj_i18n::tr("session.busy_notice")),
                                                };
                                                sess.messages = checkpoint.messages;
                                                sess.current_checkpoint_id = Some(id.clone());
                                                state.messages = reconstruct_display(&sess.messages);
                                                state.compact_count = sess.compact_count;
                                                state.turns = sess
                                                    .messages
                                                    .iter()
                                                    .filter(|message| {
                                                        matches!(
                                                            message.role,
                                                            wyj_api::types::Role::User
                                                        )
                                                    })
                                                    .count();
                                                state.welcome_frozen = true;
                                                state.frozen_up_to = 0;
                                            }
                                            state.save_needed = true;
                                            Ok(format!(
                                                "已回退到 {id}；回退前保护 checkpoint：{}{}",
                                                protection.id, file_text
                                            ))
                                        }
                                        .await;
                                        match result {
                                            Ok(text) => {
                                                state.messages.push(ChatMessage::assistant(text))
                                            }
                                            Err(error) => {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    format!("[rewind 失败] {error}"),
                                                ))
                                            }
                                        }
                                    }
                                    Ok(CommandResult::BranchSession {
                                        checkpoint_id,
                                        restore_files,
                                        confirmed,
                                    }) => {
                                        let result: anyhow::Result<(SessionFile, String)> = async {
                                            let sessions = session_store
                                                .as_ref()
                                                .context("session 存储不可用")?;
                                            let store = wyj_core::CheckpointStore::configured(
                                                sessions.dir(),
                                                current_session_id.clone(),
                                            )?;
                                            let id = resolve_tui_checkpoint_id(
                                                &store,
                                                checkpoint_id,
                                            )?;
                                            let checkpoint = store.load(&id)?;
                                            if restore_files {
                                                let preview = store.preview_files(&id, &cwd)?;
                                                if preview.requires_confirmation && !confirmed {
                                                    anyhow::bail!(
                                                        "{}\n\n确认后请重新执行：/branch {} --restore-files --confirm",
                                                        format_rewind_preview(&preview),
                                                        id
                                                    );
                                                }
                                                let current_messages = match try_session_lock(&session) {
                                                    Some(sess) => sess.messages.clone(),
                                                    None => anyhow::bail!("{}", wyj_i18n::tr("session.busy_notice")),
                                                };
                                                store.create(
                                                    &cwd,
                                                    &current_messages,
                                                    wyj_core::CheckpointKind::PreRewind,
                                                    Some(format!(
                                                        "before branch restore {id}"
                                                    )),
                                                )?;
                                                store.restore_files(&id, &cwd, confirmed)?;
                                            }
                                            let branch = sessions.branch_from_checkpoint(
                                                &current_session_id,
                                                &checkpoint,
                                            )?;
                                            Ok((branch, id))
                                        }
                                        .await;
                                        match result {
                                            Ok((branch, checkpoint_id)) => {
                                                current_session_id = branch.session_id.clone();
                                                state.current_session_id =
                                                    current_session_id.clone();
                                                let mut sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => anyhow::bail!(
                                                        "{}",
                                                        wyj_i18n::tr("session.busy_notice")
                                                    ),
                                                };
                                                *sess = Session::new();
                                                sess.messages = branch.messages;
                                                sess.current_checkpoint_id =
                                                    branch.current_checkpoint_id;
                                                sess.branch_parent_session_id =
                                                    branch.branch_parent_session_id;
                                                sess.branch_parent_checkpoint_id =
                                                    branch.branch_parent_checkpoint_id;
                                                state.messages =
                                                    reconstruct_display(&sess.messages);
                                                state.compact_count = sess.compact_count;
                                                state.turns = branch.turns;
                                                state.total_input_tokens = 0;
                                                state.total_output_tokens = 0;
                                                state.frozen_up_to = 0;
                                                state.welcome_frozen = true;
                                                drop(sess);
                                                retarget_agent_session(
                                                    &shared_agent,
                                                    session_store.as_ref(),
                                                    &current_session_id,
                                                );
                                                state.messages.push(ChatMessage::assistant(
                                                    format!(
                                                    "已从 checkpoint {} 创建并切换到分支会话 {}",
                                                    checkpoint_id, current_session_id
                                                ),
                                                ));
                                                state.save_needed = true;
                                            }
                                            Err(error) => {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    format!("[branch 失败] {error}"),
                                                ))
                                            }
                                        }
                                    }
                                    Ok(CommandResult::ControlSubAgent { id, action }) => {
                                        let result = match action {
                                            wyj_commands::registry::SubAgentControlAction::FollowUp(
                                                text,
                                            ) => hub.send_follow_up(
                                                id,
                                                vec![ContentBlock::Text { text }],
                                            ),
                                            wyj_commands::registry::SubAgentControlAction::Interrupt => {
                                                hub.interrupt(id)
                                            }
                                            wyj_commands::registry::SubAgentControlAction::RetryLast => {
                                                hub.retry_last(id)
                                            }
                                        };
                                        state.messages.push(ChatMessage::assistant(format!(
                                            "子 Agent a{id} 控制结果：{result:?}"
                                        )));
                                    }
                                    Ok(CommandResult::OpenProfileDialog) => {
                                        state.profile_dialog =
                                            Some(ProfileDialog::new(&state.config));
                                    }
                                    Ok(CommandResult::ModelDoctor(profile_name)) => {
                                        let selected = match profile_name.as_deref() {
                                            Some(name) => state.config.profile_by_name(name),
                                            None => Some(state.config.active_profile()),
                                        };
                                        if let Some(profile) = selected {
                                            let cache = wyj_config::config_dir()
                                                .ok()
                                                .map(|base| wyj_api::CapabilityCache::new(&base));
                                            let report = wyj_api::ModelDoctorReport::static_report(
                                                profile,
                                                cache.as_ref(),
                                            );
                                            state.messages.push(ChatMessage::assistant(
                                                format_tui_model_doctor(&report),
                                            ));
                                        } else {
                                            let name = profile_name.unwrap_or_default();
                                            state.messages.push(ChatMessage::assistant_err(
                                                format!("未找到 Profile: {name}"),
                                            ));
                                        }
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
                                                            let new_agent = attach_agent_session(
                                                                new_agent,
                                                                session_store.as_ref(),
                                                                &current_session_id,
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
                                        state.begin_turn_timing();

                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let session_c = session.clone();
                                        let tx = agent_tx.clone();
                                        let ctx_cwd = cwd.clone();
                                        let mode_arc = shared_mode.clone();
                                        let shared_permission_c = shared_permission.clone();
                                        let ui_ask_tx_clone = ui_ask_tx.clone();
                                        let plugin_runtime_c = plugin_runtime.clone();

                                        let task = spawn_turn_task(async move {
                                            let mut sess = session_c.lock().await;
                                            sess.push_user(prompt);
                                            let current_mode = mode_arc.lock().await.clone();
                                            let mut ctx = ToolCtx::new(&ctx_cwd);
                                            ctx.set_execution_surface(
                                                wyj_core::ExecutionSurface::TuiInteractive,
                                            );
                                            // 追问续跑是同一个会话、新的 ToolCtx，
                                            // 子 Agent 用量仍需回灌到同一个 session。
                                            ctx.sub_usage =
                                                Some(Arc::new(wyj_core::SubUsageTotals::default()));
                                            ctx.permission_mode = shared_permission_c;
                                            ctx.ui_ask_tx = Some(ui_ask_tx_clone);
                                            let turn_agent =
                                                plan_turn_agent(&agent_c, &current_mode);
                                            let tx2 = tx.clone();
                                            let mut on_text = move |d: &str| {
                                                let _ = tx2
                                                    .try_send(AgentEvent::TextDelta(d.to_string()));
                                            };
                                            let run_result = turn_agent
                                                .run_turn(&mut sess, &ctx, &mut on_text)
                                                .await;
                                            let usage = AgentEvent::Usage {
                                                input: sess.total_input_tokens,
                                                output: sess.total_output_tokens,
                                                compact_count: sess.compact_count,
                                                tool_schema_tokens: sess.tool_schema_tokens_last,
                                                tool_schema_tokens_saved: sess
                                                    .tool_schema_tokens_saved,
                                            };
                                            emit_plugin_event_background(
                                                plugin_runtime_c,
                                                if run_result.is_ok() {
                                                    "turn_finished"
                                                } else {
                                                    "turn_error"
                                                },
                                                serde_json::json!({
                                                    "cwd": ctx_cwd,
                                                    "input_tokens": sess.total_input_tokens,
                                                    "output_tokens": sess.total_output_tokens,
                                                    "error": run_result.as_ref().err().map(ToString::to_string),
                                                }),
                                            );
                                            // 发终态事件前先放锁，理由同 `spawn_agent_turn`
                                            // （阻塞式 send + UI 侧 `session.lock().await`
                                            //  = 环形等待，界面永久冻结）。
                                            drop(sess);
                                            match run_result {
                                                Ok(_) => {
                                                    let _ = tx.send(usage).await;
                                                    let _ = tx.send(AgentEvent::TurnDone).await;
                                                }
                                                Err(e) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Error(e.to_string()))
                                                        .await;
                                                }
                                            }
                                        });
                                        state.current_task = Some(task);
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
                                        state.begin_turn_timing();

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
                                                Ok(agent) => Arc::new(attach_agent_session(
                                                    agent,
                                                    session_store.as_ref(),
                                                    &current_session_id,
                                                )),
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
                                        let plugin_runtime_c = plugin_runtime.clone();

                                        let task = spawn_turn_task(async move {
                                            let mut sess = session_c.lock().await;
                                            sess.push_user(text);
                                            let current_mode = mode_arc.lock().await.clone();
                                            let mut ctx = ToolCtx::new(&ctx_cwd);
                                            ctx.set_execution_surface(
                                                wyj_core::ExecutionSurface::TuiInteractive,
                                            );
                                            // 追问续跑是同一个会话、新的 ToolCtx，
                                            // 子 Agent 用量仍需回灌到同一个 session。
                                            ctx.sub_usage =
                                                Some(Arc::new(wyj_core::SubUsageTotals::default()));
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
                                                let scoped = tools.into_iter().collect();
                                                *shared_permission_c.write().unwrap() =
                                                    if matches!(current_mode, AgentMode::Plan) {
                                                        PermissionMode::Plan(scoped)
                                                    } else {
                                                        PermissionMode::Allowlist(scoped)
                                                    };
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
                                            let run_result = turn_agent
                                                .run_turn(&mut sess, &ctx, &mut on_text)
                                                .await;
                                            let usage = AgentEvent::Usage {
                                                input: sess.total_input_tokens,
                                                output: sess.total_output_tokens,
                                                compact_count: sess.compact_count,
                                                tool_schema_tokens: sess.tool_schema_tokens_last,
                                                tool_schema_tokens_saved: sess
                                                    .tool_schema_tokens_saved,
                                            };
                                            emit_plugin_event_background(
                                                plugin_runtime_c,
                                                if run_result.is_ok() {
                                                    "turn_finished"
                                                } else {
                                                    "turn_error"
                                                },
                                                serde_json::json!({
                                                    "cwd": ctx_cwd,
                                                    "input_tokens": sess.total_input_tokens,
                                                    "output_tokens": sess.total_output_tokens,
                                                    "error": run_result.as_ref().err().map(ToString::to_string),
                                                }),
                                            );
                                            // 发终态事件前先放锁，理由同 `spawn_agent_turn`
                                            // （阻塞式 send + UI 侧 `session.lock().await`
                                            //  = 环形等待，界面永久冻结）。
                                            drop(sess);
                                            match run_result {
                                                Ok(_) => {
                                                    let _ = tx.send(usage).await;
                                                    let _ = tx.send(AgentEvent::TurnDone).await;
                                                }
                                                Err(e) => {
                                                    let _ = tx
                                                        .send(AgentEvent::Error(e.to_string()))
                                                        .await;
                                                }
                                            }
                                        });
                                        state.current_task = Some(task);
                                    }
                                    Ok(CommandResult::ResumeSession(id)) => {
                                        if state.is_thinking {
                                            state.messages.push(ChatMessage::assistant(
                                                "请等待当前任务完成后再切换会话。".to_string(),
                                            ));
                                        } else if let Some(store) = &session_store {
                                            // 自动保存当前会话
                                            {
                                                let sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => {
                                                        state.notify_session_busy();
                                                        continue;
                                                    }
                                                };
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
                                                    let _ = store.save(&SessionFile::from_session(
                                                        &sess,
                                                        SessionFileMeta {
                                                            session_id: current_session_id.clone(),
                                                            title,
                                                            last_preview: extract_preview(
                                                                &sess.messages,
                                                            ),
                                                            cwd: cwd.display().to_string(),
                                                            timestamp: now_iso(),
                                                            turns: state.turns,
                                                            title_generated,
                                                        },
                                                    ));
                                                }
                                            }
                                            // 加载目标会话
                                            match store.load(&id) {
                                                Ok(file) => {
                                                    let display_msgs =
                                                        reconstruct_display(&file.messages);
                                                    let mut sess = match try_session_lock(&session)
                                                    {
                                                        Some(sess) => sess,
                                                        None => {
                                                            state.notify_session_busy();
                                                            continue;
                                                        }
                                                    };
                                                    sess.total_input_tokens = file.input_tokens;
                                                    sess.total_output_tokens = file.output_tokens;
                                                    sess.routing_events =
                                                        file.routing_events.clone();
                                                    sess.current_checkpoint_id =
                                                        file.current_checkpoint_id.clone();
                                                    sess.branch_parent_session_id =
                                                        file.branch_parent_session_id.clone();
                                                    sess.branch_parent_checkpoint_id =
                                                        file.branch_parent_checkpoint_id.clone();
                                                    sess.messages = file.messages;
                                                    let compact_count = sess.compact_count;
                                                    let plan_approved =
                                                        has_plan_approved(&sess.messages);
                                                    drop(sess);
                                                    current_session_id = file.session_id.clone();
                                                    state.current_session_id =
                                                        current_session_id.clone();
                                                    retarget_agent_session(
                                                        &shared_agent,
                                                        session_store.as_ref(),
                                                        &current_session_id,
                                                    );
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
                                                    state.compact_count = compact_count;
                                                    state.turns = file.turns;
                                                    state.frozen_up_to = 0;
                                                    state.welcome_frozen = true;
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
                                        let agent_guard = shared_agent.read().unwrap();
                                        let memory_ref = agent_guard.memory_v3_ref();
                                        let index_path = memory_ref
                                            .map(|memory| memory.project_index_path())
                                            .unwrap_or_else(|| {
                                                let pid = wyj_core::project_id(&state.cwd);
                                                wyj_config::config_dir()
                                                    .unwrap_or_default()
                                                    .join("memory")
                                                    .join(pid)
                                                    .join("MEMORY.md")
                                            });
                                        let (active, superseded) = memory_ref
                                            .map(|m| {
                                                let s = m.status().unwrap_or_default();
                                                (s.active_records, s.superseded_records)
                                            })
                                            .unwrap_or((0, 0));
                                        drop(agent_guard);
                                        state.memory_dialog = Some(MemoryDialog::new(
                                            &state.cwd,
                                            index_path,
                                            state.config.auto_memory_enabled,
                                            active,
                                            superseded,
                                        ));
                                    }
                                    Ok(CommandResult::OpenMemoryClearAllConfirm { .. }) => {
                                        // 直接打开面板并预跳到 ClearAll 行 + pending 二次确认。
                                        let agent_guard = shared_agent.read().unwrap();
                                        let memory_ref = agent_guard.memory_v3_ref();
                                        let index_path = memory_ref
                                            .map(|memory| memory.project_index_path())
                                            .unwrap_or_else(|| {
                                                let pid = wyj_core::project_id(&state.cwd);
                                                wyj_config::config_dir()
                                                    .unwrap_or_default()
                                                    .join("memory")
                                                    .join(pid)
                                                    .join("MEMORY.md")
                                            });
                                        let (active, superseded) = memory_ref
                                            .map(|m| {
                                                let s = m.status().unwrap_or_default();
                                                (s.active_records, s.superseded_records)
                                            })
                                            .unwrap_or((0, 0));
                                        drop(agent_guard);
                                        let mut dialog = MemoryDialog::new(
                                            &state.cwd,
                                            index_path,
                                            state.config.auto_memory_enabled,
                                            active,
                                            superseded,
                                        );
                                        // 把光标移到 ClearAll 行（最后一行）。
                                        if let Some(last) = dialog.rows.len().checked_sub(1) {
                                            dialog.selected = last;
                                        }
                                        dialog.pending_clear_all = true;
                                        state.memory_dialog = Some(dialog);
                                    }
                                    Ok(CommandResult::OpenEvolutionDialog) => {
                                        let evolution =
                                            shared_agent.read().unwrap().evolution_ref().cloned();
                                        if let Some(store) = evolution {
                                            state.evolution_dialog =
                                                Some(EvolutionDialog::new(store));
                                        } else {
                                            state.messages.push(ChatMessage::assistant_err(
                                                wyj_i18n::tr("evolve.unavailable"),
                                            ));
                                        }
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
                                    Ok(CommandResult::OpenShellsPanel(target_id)) => {
                                        apply_open_shells_panel(&mut state, target_id);
                                    }
                                    Ok(CommandResult::OpenScheduleDialog) => {
                                        let last_user_prompt = state
                                            .messages
                                            .iter()
                                            .rev()
                                            .find(|m| matches!(m.role, MessageRole::User))
                                            .map(|m| m.content.clone());
                                        state.schedule_dialog =
                                            Some(ScheduleDialog::new(&state.cwd, last_user_prompt));
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

                                // 展示文本从快照投影（必须在 @ 展开之后算：@ 追加的
                                // 附件没有输入框标记，要作为末尾单列项一起画）
                                let display_text = crate::render::attachment_display_text(
                                    &composer_snapshot,
                                    &state.pending_attachments,
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
                                        pending_display: display_text,
                                    });
                                    state.input_history.push(text);
                                } else {
                                    state.push_user(display_text);
                                    state.input_history.push(text.clone());
                                    state.begin_new_turn();
                                    state.is_thinking = true;
                                    state.spinner_frame = 0;
                                    state.begin_turn_timing();

                                    // 捕获并清空附件列表（移入 async task）
                                    let attachments =
                                        std::mem::take(&mut state.pending_attachments);
                                    let agent_c = shared_agent.read().unwrap().clone();
                                    let (task, injector) = spawn_agent_turn(
                                        Some(text),
                                        attachments,
                                        agent_c,
                                        session.clone(),
                                        agent_tx.clone(),
                                        cwd.clone(),
                                        shared_mode.clone(),
                                        shared_permission.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                        plugin_runtime.clone(),
                                    );
                                    state.current_task = Some(task);
                                    state.injector = Some(injector);
                                }
                            }
                        }
                    } else if key.code == KeyCode::Enter && state.is_thinking {
                        // Agent 忙碌期间提交 → 排队，不打断当前操作
                        if !input.is_empty() || !state.pending_attachments.is_empty() {
                            let composer_snapshot = input.clone();
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
                            let display = crate::render::attachment_display_text(
                                &composer_snapshot,
                                &state.pending_attachments,
                            );
                            let attachments = std::mem::take(&mut state.pending_attachments);
                            if let Some(queue) = &state.injector {
                                let blocks =
                                    build_user_blocks(text.clone(), attachments.clone()).await;
                                queue.push(blocks, InjectionKind::UserMessage);
                                // **当场回显**，不等 Agent 排空：排空点在「一轮工具
                                // 调用全部返回之后」，前台子 Agent / 长 Bash 期间可能
                                // 几十分钟都不排空，回显跟着一起等就等于「我打的字被
                                // 吞了」。真实投递仍由 Agent 排空时完成，这里只管让
                                // 用户立刻看见自己提交了什么。
                                state.enqueue_message(OutgoingMessage {
                                    text,
                                    attachments,
                                    display,
                                });
                            } else {
                                // 理论上不应发生：is_thinking 但没有活跃任务，按普通消息兜底处理
                                state.push_user(display);
                                state.input_history.push(text);
                                state.pending_attachments = attachments;
                            }
                        }
                    } else if key.code == KeyCode::Up && key.modifiers.is_empty() {
                        state.navigate_input_or_history(&mut input, -1);
                    } else if key.code == KeyCode::Down && key.modifiers.is_empty() {
                        state.navigate_input_or_history(&mut input, 1);
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
                            delete_word_backward_composer(
                                &mut input,
                                &mut state.pending_attachments,
                            );
                        } else {
                            backspace_composer(&mut input, &mut state.pending_attachments);
                        }
                        update_slash_completions(&mut state, &input, &cmd_registry);
                        update_file_completions(&mut state, &input, &cwd);
                    } else if key.code == KeyCode::Delete {
                        delete_composer_forward(&mut input, &mut state.pending_attachments);
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
                    } else if key.code == KeyCode::Char('w')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+W 必须在共享编辑实现**之前**拦下：它要连带删除
                        // 光标前的附件占位标记（`delete_word_backward_composer`），
                        // 纯文本实现不认识附件。
                        delete_word_backward_composer(&mut input, &mut state.pending_attachments);
                        update_slash_completions(&mut state, &input, &cmd_registry);
                        update_file_completions(&mut state, &input, &cwd);
                    } else if crate::input::apply_edit_key(&mut input, &key) {
                        // 左右移动 / Ctrl+A / Ctrl+E / Ctrl+K / Ctrl+U 走唯一实现，
                        // 与各弹窗的自由文本输入同源（见 `input::apply_edit_key`）。
                        // 不走共享实现的只有五类，它们各自还背着应用层语义：
                        // Backspace / Delete（附件占位标记）、Ctrl+W（同上）、
                        // Home / End（输入框为空时是"跳到对话首尾"）、
                        // Enter / ↑↓ / 粘贴（提交、历史、附件）。
                        input.reconcile_attachment_markers(state.pending_attachments.len());
                        update_slash_completions(&mut state, &input, &cmd_registry);
                        update_file_completions(&mut state, &input, &cwd);
                    } else if is_clipboard_paste_key(key) {
                        // 纯图片剪贴板不会让终端产生 Event::Paste；应用必须在收到
                        // Ctrl+V（部分终端为 Super+V）时主动读取系统剪贴板。
                        if let Some(paste) = read_system_clipboard() {
                            state.paste_hint = apply_clipboard_paste(&mut state, &mut input, paste);
                            update_slash_completions(&mut state, &input, &cmd_registry);
                            update_file_completions(&mut state, &input, &cwd);
                        }
                    } else if let KeyCode::Char(c) = key.code {
                        // Ctrl 组合键
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                // a/e/k/u/w 已由上游 `apply_edit_key` 接管
                                'l' => {
                                    // Ctrl+L：对齐大多数 shell/终端的"清屏重绘"语义。
                                    let _ = terminal.clear();
                                }
                                't' => {
                                    // Ctrl+T — 进入任务列表焦点，并保留原折叠/展开语义
                                    if let Some(items) = state.current_todos.as_deref() {
                                        let collapsible = is_todo_collapsible(items);
                                        // 全部完成后面板已收成一行摘要，没有可展开的
                                        // 内容，Ctrl+T 不做任何事（否则会展开一个
                                        // 永远渲染不出来的空壳）。
                                        let all_done = !items.is_empty()
                                            && items
                                                .iter()
                                                .all(|t| t.status == TodoStatus::Completed);
                                        if all_done {
                                            // no-op
                                        } else if collapsible && state.ui_focus == UiFocus::Todos {
                                            state.todo_panel_expanded = !state.todo_panel_expanded;
                                        } else {
                                            state.todo_panel_expanded = true;
                                        }
                                        state.ensure_selected_todo();
                                        if state.selected_todo_id.is_some() {
                                            state.selected_sub_agent = None;
                                            state.todo_detail_open = false;
                                            state.todo_detail_scroll = 0;
                                            state.todo_detail_max_scroll = 0;
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
                                'o' => {
                                    // Ctrl+O — 展开/折叠 thinking 与工具结果正文。与内容焦点
                                    // 态共用同一个 flag，因此在输入框态也必须可用：
                                    // 折叠头提示的正是这个键，而输入框是默认焦点态。
                                    state.output_expanded = !state.output_expanded;
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
                let _ = store.save(&SessionFile::from_session(
                    &sess,
                    SessionFileMeta {
                        session_id: current_session_id.clone(),
                        title,
                        last_preview: extract_preview(&sess.messages),
                        cwd: cwd.display().to_string(),
                        timestamp: now_iso(),
                        turns: state.turns,
                        title_generated,
                    },
                ));
                resumable_session_id = Some(current_session_id.clone());
            }
        }
    }
    // 退出时释放本会话被 context editing 外部化的 CAS blob 引用。会话没了，
    // 这些 blob 也就没人会召回；不 release 的话 ref_count 永远停在 1，gc
    // 永远删不掉，CAS pool 随会话数线性增长。
    if let Ok(mut sess) = session.try_lock() {
        let released = wyj_core::context_edit::release_session_blobs(
            workspace_cas.as_deref(),
            &sess.take_elided_blobs(),
        );
        if released > 0 {
            tracing::debug!("退出: 释放 {released} 个 CAS blob 引用");
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
    // tool_use_id → (seq, tool name)，供同一助手回合内并行工具调用各自找到自己
    // 的 ToolResult，并让恢复后的 Edit/Write 结果仍能识别为 diff（不能像以前那样
    // 直接复用循环里"当前"的 tool_seq——一个回合有多个并行调用时，所有结果会
    // 错误地对应到最后一个调用，工具调用配对逻辑全部错位）。
    let mut tool_by_use_id: HashMap<String, (usize, String)> = HashMap::new();

    for msg in messages {
        match &msg.role {
            Role::User => {
                let mut has_text = false;
                // 恢复会话时图片块此前被整个 `_ => {}` 吞掉——历史消息里的
                // 图片在聊天流里不留任何痕迹，用户会以为图没发出去。补上
                // `[Image #n]`：占位符在正文里的原始位置从未落盘（那是输入框
                // 内部的原子标记），只能在末尾补，编号与实时发送路径
                // （`render::attachment_display_text`）保持一致。
                let image_count = msg
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Image { .. }))
                    .count();
                let mut last_user_index = None;
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            result.push(ChatMessage::user(text.clone()));
                            last_user_index = Some(result.len() - 1);
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
                            let (seq, name) =
                                tool_by_use_id.get(tool_use_id).cloned().unwrap_or_default();
                            result.push(ChatMessage::tool_result(
                                text, *is_error, 0.0, seq, name, summary,
                            ));
                        }
                        _ => {}
                    }
                }
                if image_count > 0 {
                    let suffix: String = (1..=image_count)
                        .map(|i| format!("\n[Image #{i}]"))
                        .collect();
                    match last_user_index {
                        Some(idx) => result[idx].content.push_str(&suffix),
                        None => result.push(ChatMessage::user(suffix.trim_start().to_string())),
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
                            tool_by_use_id.insert(id.clone(), (tool_seq, name.clone()));
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

    /// 恢复历史会话时，用户消息里的图片块此前被 `_ => {}` 整个吞掉——
    /// 聊天流里不留任何痕迹，用户会以为当初那张图没发出去。占位符编号
    /// 必须与实时发送路径（`render::attachment_display_text`）一致。
    #[test]
    fn user_images_survive_into_the_reconstructed_display() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "如图".to_string(),
                    },
                    ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        data: "AAAA".to_string(),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        data: "BBBB".to_string(),
                    },
                    ContentBlock::Image {
                        media_type: "image/png".to_string(),
                        data: "CCCC".to_string(),
                    },
                ],
            },
        ];

        let display = reconstruct_display(&messages);
        assert_eq!(display[0].content, "如图\n[Image #1]");
        // 纯图片消息（无正文）也不能凭空消失
        assert_eq!(display[1].content, "[Image #1]\n[Image #2]");
    }

    /// 回归测试：一个助手回合里有多个并行工具调用时，`reconstruct_display`
    /// 曾经用循环里"当前"的 tool_seq 给所有 ToolResult 赋值——多个并行调用的
    /// 结果会全部错误地对应到最后一个调用的 seq，导致除最后一个以外的
    /// ToolCall 永远找不到自己的 ToolResult，所有按 sequence_no 配对的逻辑
    /// 都会错位。现在改用 tool_use_id 精确配对，每个调用都应该找到自己
    /// 的结果。
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
                        name: "Edit".to_string(),
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
        assert_eq!(display[2].tool_name.as_deref(), Some("Read"));
        assert_eq!(display[3].sequence_no, call_b_seq);
        assert_eq!(display[3].content, "content-b");
        assert_eq!(display[3].tool_name.as_deref(), Some("Edit"));
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
            parent_id: None,
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
    fn content_focus_requires_explicit_entry_and_prefers_visible_subagents() {
        let mut state = make_state();
        tool_start(&mut state, "call-1");
        started(&mut state, 1, "任务", false);

        assert_eq!(state.ui_focus, UiFocus::Composer);
        state.move_focus_selection(1);
        assert_eq!(state.ui_focus, UiFocus::Composer);
        assert_eq!(state.selected_sub_agent, None);

        assert!(state.enter_content_focus());

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
        // 条目必须是 Running：面板口径是「只列运行中」（`visible_sub_agents`
        // 是唯一口径），`/subagents` 也只在运行项里定位——否则选中一个不在
        // 列表上的 id 会让焦点悬空在一行看不见的位置。
        state.sub_agents.insert(
            1,
            SubAgentUiState {
                agent_type: "t".into(),
                description: "d".into(),
                background: false,
                status: SubAgentStatus::Running,
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
                status: SubAgentStatus::Running,
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
                status: SubAgentStatus::Running,
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
            AgentMode::Normal,
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

    /// 手动输入是多行的，面板高度必须跟着**实际行数**走。
    ///
    /// 此前 `bottom_panel_size` 对 PlanApproval 写死 5 行，渲染又只画
    /// `input.lines.first()` 并把一个假的 `_` 贴在行尾——用户写三行只看见
    /// 一行，按回车却把三行一起发出去（提交取的是 `input.text()`）。同批
    /// 改动已把 AskQuestion 的自由输入改成逐行渲染，这里是对齐遗漏的兄弟面板。
    #[test]
    fn freetext_can_hold_multiple_lines() {
        let (mut dlg, _rx) = make_dialog();
        // 走到「手动输入反馈…」
        for _ in 0..2 {
            dlg.handle_key(KeyCode::Down);
        }
        dlg.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let InProgressAnswer::FreeText { input, .. } = &dlg.current else {
            panic!("应进入自由输入");
        };
        assert_eq!(input.lines.len(), 1);

        // Ctrl+J 换行 + 继续打字
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        dlg.handle_key_event(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));

        let InProgressAnswer::FreeText { input, .. } = &dlg.current else {
            panic!("应仍在自由输入");
        };
        assert_eq!(input.lines.len(), 2, "Ctrl+J 必须真的插入一个换行");
        assert_eq!(input.text(), "a\nb", "提交取的是 input.text()，两行都要在");
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

    /// 标题栏的实时 token 必须逐请求累加（UsageDelta 是回合进行中唯一的用量信号），
    /// 且新一轮开始时归零——否则"刚发一条消息就显示上一轮几万 token"。
    #[test]
    fn usage_delta_accumulates_this_turn_and_resets_on_next_turn() {
        let mut state = make_state();
        state.begin_turn_timing();
        state.apply_agent_event(AgentEvent::UsageDelta {
            input_tokens: 1_000,
            output_tokens: 40,
        });
        state.apply_agent_event(AgentEvent::UsageDelta {
            input_tokens: 2_000,
            output_tokens: 60,
        });
        assert_eq!(state.turn_live_input_tokens, 3_000);
        assert_eq!(state.turn_live_output_tokens, 100);

        // 下一轮：实时计数器归零，但 session 累计不受影响。
        state.total_input_tokens = 50_000;
        state.begin_turn_timing();
        assert_eq!(state.turn_live_input_tokens, 0);
        assert_eq!(state.turn_live_output_tokens, 0);
        assert_eq!(state.turn_start_input_tokens, 50_000);
        assert!(state.turn_start_time.is_some());
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

    /// 回合正常结束时（`stop_reason != tool_use`，模型已产出最终回答），
    /// 任何仍处 InProgress 的 todo 都必须自动改为 Completed 并把
    /// `started_at` 累加进 `elapsed_secs`——否则会卡在"AI 已回完但任务列表
    /// 还显示进行中"的 UI 割裂状态（典型场景：模型最后一轮直接出最终回答，
    /// 不再调用一次 TodoWrite 收尾）。
    #[test]
    fn turn_done_auto_completes_remaining_in_progress_todos() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![
            todo("a", TodoStatus::Completed),
            todo("b", TodoStatus::Completed),
            todo("c", TodoStatus::InProgress),
        ]));
        // 模拟 c 已积累一段 in_progress 耗时
        let c_started = state.todo_stats.get("c").and_then(|s| s.started_at);
        assert!(c_started.is_some(), "InProgress todo 必须有 started_at");

        state.apply_agent_event(AgentEvent::TurnDone);

        let items = state
            .current_todos
            .as_deref()
            .expect("TurnDone 不应清空 current_todos");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::Completed);
        assert_eq!(
            items[2].status,
            TodoStatus::Completed,
            "TurnDone 必须把遗留的 InProgress 改为 Completed"
        );
        let c_stats = state.todo_stats.get("c").expect("c 的 stats 必须保留");
        assert!(
            c_stats.started_at.is_none(),
            "started_at 必须在 Completed 时被 take 掉，否则 elapsed_secs 会被双重计算"
        );
        assert!(
            c_stats.elapsed_secs > 0.0,
            "elapsed_secs 必须把 started_at 段累加进来"
        );
    }

    /// TurnDone 在没有遗留 InProgress 时 no-op：不丢失数据、不修改已完成项。
    #[test]
    fn turn_done_with_no_in_progress_keeps_state_untouched() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::TodoUpdate(vec![
            todo("a", TodoStatus::Completed),
            todo("b", TodoStatus::Completed),
        ]));
        state.apply_agent_event(AgentEvent::TurnDone);
        let items = state.current_todos.as_deref().unwrap();
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::Completed);
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

    // ── TodoWrite 不进常规工具行（v1.5.18） ────────────────────────────────
    //
    // 回归背景：修复三重渲染前，一次 TodoWrite 会在聊天流里出现三遍——
    // `ToolStart` 无条件 push 的 ⏺ TodoWrite、`ToolEnd` 追加的 ⎿ 任务列表已更新…、
    // 以及 `push_inline_todo_lines` 画的面板头 + 分隔线 + 条目。面板已经把"改了什么"
    // 讲得更完整，常规行只剩噪音。

    #[test]
    fn todo_write_does_not_push_regular_tool_call_line() {
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "call-1".to_string(),
            name: "TodoWrite".to_string(),
            input_json: serde_json::json!({"todos": []}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            id: "call-1".to_string(),
            output: "任务列表已更新: 0 项（待处理 0，进行中 0，已完成 0）".to_string(),
            is_error: false,
            elapsed_secs: 0.01,
        });

        assert!(
            !state
                .messages
                .iter()
                .any(|m| matches!(m.role, MessageRole::ToolCall)),
            "TodoWrite 不应产生常规 ⏺ 工具行：{:?}",
            state.messages
        );
        assert!(
            !state
                .messages
                .iter()
                .any(|m| matches!(m.role, MessageRole::ToolResult)),
            "TodoWrite 不应产生 ⎿ 回执行（否则 call_idx 失配会把它追加到流尾）：{:?}",
            state.messages
        );
    }

    #[test]
    fn todo_write_still_consumes_a_sequence_number() {
        // ToolEnd 靠 seq 与 tool_info 配对；ToolStart 若不占用 seq，后续工具会
        // 与更早的调用撞号。本测试锁死"跳过渲染但不跳过记账"。
        let mut state = make_state();
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "todo-1".to_string(),
            name: "TodoWrite".to_string(),
            input_json: serde_json::json!({"todos": []}),
        });
        let todo_seq = state.tool_call_count;
        state.apply_agent_event(AgentEvent::ToolStart {
            id: "read-1".to_string(),
            name: "Read".to_string(),
            input_json: serde_json::json!({"file_path": "a.rs"}),
        });
        let read_seq = state.tool_call_count;

        assert_eq!(todo_seq, 1);
        assert_eq!(read_seq, 2, "TodoWrite 必须占用 seq，Read 不能与之撞号");
    }

    // ── 新一轮对话清空任务板 ─────────────────────────────────────────────
    //
    // 回归背景：任务是某一轮对话的产物。上一轮全部完成后，`TodoUpdate` 只把
    // 面板折叠（`todo_panel_expanded = false`），从不把 `current_todos` 置空；
    // 而新一轮若没有再调 TodoWrite（绝大多数轮次都是），那份任务列表就会
    // 以折叠态一直挂在聊天流里不消失。

    #[test]
    fn begin_new_turn_clears_the_todo_panel() {
        let mut state = make_state();
        state.current_todos = Some(vec![todo("a", TodoStatus::Completed)]);
        state.todo_panel_expanded = true;
        state.selected_todo_id = Some("a".to_string());
        state.todo_detail_open = true;
        state.todo_detail_scroll = 42;
        state.todo_detail_max_scroll = 100;
        state.todo_stats.insert("a".to_string(), Default::default());
        state.todo_execution_logs.insert("a".to_string(), vec![]);

        state.begin_new_turn();

        // 面板渲染以 `current_todos.is_some()` 为准，置 None 即彻底消失
        assert!(state.current_todos.is_none(), "新一轮必须清空任务列表");
        assert!(!state.todo_panel_expanded);
        assert!(state.selected_todo_id.is_none());
        assert!(!state.todo_detail_open);
        assert_eq!(state.todo_detail_scroll, 0);
        assert_eq!(state.todo_detail_max_scroll, 0);
        assert!(state.todo_stats.is_empty());
        assert!(state.todo_execution_logs.is_empty());
    }

    #[test]
    fn begin_new_turn_keeps_non_todo_state() {
        // 粒度：只清按轮次作废的 TodoWrite 面板，不碰聊天流、token 统计等
        let mut state = make_state();
        state.messages.push(ChatMessage::user("保留我".to_string()));
        state.total_input_tokens = 1234;
        state.turns = 7;
        state.current_todos = Some(vec![todo("a", TodoStatus::InProgress)]);

        state.begin_new_turn();

        assert_eq!(state.messages.len(), 1, "聊天流不应被新回合清空");
        assert_eq!(state.total_input_tokens, 1234);
        assert_eq!(state.turns, 7);
    }

    #[test]
    fn todo_panel_reappears_when_model_writes_a_new_list_after_a_new_turn() {
        // 回归的反面：清空只是让「上一轮」作废；新一轮模型再调一次 TodoWrite
        // 必须能正常重新出现任务板。
        let mut state = make_state();
        state.current_todos = Some(vec![todo("old", TodoStatus::Completed)]);
        state.begin_new_turn();
        assert!(state.current_todos.is_none());

        state.apply_agent_event(crate::event::AgentEvent::TodoUpdate(vec![todo(
            "new",
            TodoStatus::InProgress,
        )]));
        let items = state.current_todos.as_ref().expect("新任务板应重新出现");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "new");
    }
}

#[cfg(test)]
mod shell_panel_tests {
    use super::*;
    use wyj_tools::bash_session::{BashSessionManager, JobStatus};

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

    fn running_shell(id: &str) -> ShellUiState {
        ShellUiState {
            id: id.to_string(),
            command: "npm run dev".to_string(),
            status: ShellStatus::Running,
            started_at: Instant::now(),
            final_elapsed: None,
            wake_sent: false,
        }
    }

    // ── poll_shells ───────────────────────────────────────────────────
    //
    // 回归背景：用户报告「所有 shell 后台执行的任务界面上看不见执行的内容」。
    // poll_shells 是把 BashSessionManager 的任务同步进 UI 态的唯一入口，
    // 它同时承担「发现新任务」和「检测退出」两件事。

    #[test]
    fn poll_shells_registers_newly_spawned_jobs_as_running() {
        let mut state = make_state();
        // 不真 spawn（那要 tokio runtime + 真进程），直接灌快照验证登记逻辑
        state.shells.push(running_shell("bash_1"));

        assert_eq!(state.shells.len(), 1);
        assert_eq!(state.shells[0].id, "bash_1");
        assert_eq!(state.shells[0].status, ShellStatus::Running);
        assert!(state.has_running_shells(), "新任务应驱动面板自动显示");
        assert_eq!(state.shells[0].command, "npm run dev");
    }

    /// 退出状态只翻转一次：第二次轮询拿到同样的 Exited 不能再上报，
    /// 否则会对同一个任务重复唤醒 Agent。
    #[tokio::test]
    async fn poll_shells_detects_exit_exactly_once() {
        let mgr = BashSessionManager::default();
        let id = mgr.spawn("echo done; exit 0", Path::new("/tmp")).unwrap();
        let job = mgr.get(&id).unwrap();
        for _ in 0..50 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_ne!(job.status(), JobStatus::Running, "任务应已退出");

        let mut state = make_state();
        // 第一次轮询：注册 + 立刻检测到已退出
        let first = sync_once(&mut state, &mgr);
        assert_eq!(first, vec![id.clone()], "首次轮询应报告一次退出");
        assert_eq!(state.shells[0].status, ShellStatus::Exited(0));
        assert!(!state.has_running_shells());

        // 第二次轮询：已是 Exited，不得重复上报
        let second = sync_once(&mut state, &mgr);
        assert!(second.is_empty(), "同一任务的退出不得被重复上报");
    }

    /// 把 `AppState::poll_shells` 的核心逻辑抽出来单测：内部依赖
    /// `BashSessionManager::global()`（进程级单例），测试里换成显式传入的
    /// manager，避免污染全局状态。
    fn sync_once(state: &mut AppState, mgr: &BashSessionManager) -> Vec<String> {
        let mut newly_exited = Vec::new();
        for (id, command, status) in mgr.list() {
            let idx = match state.shells.iter().position(|s| s.id == id) {
                Some(i) => i,
                None => {
                    state.shells.push(ShellUiState {
                        id: id.clone(),
                        command: command.clone(),
                        status: ShellStatus::Running,
                        started_at: Instant::now(),
                        final_elapsed: None,
                        wake_sent: false,
                    });
                    state.shells.len() - 1
                }
            };
            if let (ShellStatus::Running, JobStatus::Exited(code)) =
                (state.shells[idx].status, status)
            {
                state.shells[idx].status = ShellStatus::Exited(code);
                state.shells[idx].final_elapsed =
                    Some(state.shells[idx].started_at.elapsed().as_secs_f64());
                newly_exited.push(id);
            }
        }
        newly_exited
    }

    // ── 跨轮次 / 跨会话两级粒度 ────────────────────────────────────────
    //
    // 后台任务的生命周期**跨对话轮次**（dev server 不该因为你发新消息就消失），
    // 但**不跨会话**（/clear 之后是全新一段，进程也已被 kill_all 收走）。
    // 这两级区分写错会让正在跑的任务凭空从面板消失。

    #[test]
    fn begin_new_turn_keeps_running_shells() {
        let mut state = make_state();
        state.shells.push(running_shell("bash_1"));
        state.selected_shell = Some("bash_1".to_string());

        state.begin_new_turn();

        assert_eq!(
            state.shells.len(),
            1,
            "新一轮对话不应清掉仍在运行的后台任务"
        );
        assert_eq!(state.shells[0].status, ShellStatus::Running);
    }

    #[test]
    fn reset_for_new_session_clears_shells() {
        let mut state = make_state();
        state.shells.push(running_shell("bash_1"));
        state.selected_shell = Some("bash_1".to_string());
        state.shell_detail_open = true;

        state.reset_for_new_session();

        assert!(state.shells.is_empty(), "新会话应作废上一段的后台任务");
        assert!(state.selected_shell.is_none());
        assert!(!state.shell_detail_open);
    }

    // ── 焦点导航 ──────────────────────────────────────────────────────

    #[test]
    fn shell_navigation_reaches_shell_panel_from_chat() {
        // Chat ↓ → Todos ↓ →（无 todo 时）SubAgents ↓ → Shells
        let mut state = make_state();
        state.messages.push(ChatMessage::user("hi".to_string()));
        state.shells.push(running_shell("bash_1"));
        state.ui_focus = UiFocus::Chat;

        // 无 todo、无 subagent，直接落到 shell 面板
        state.move_focus_selection(1);

        assert_eq!(state.ui_focus, UiFocus::Shells);
        assert_eq!(state.selected_shell.as_deref(), Some("bash_1"));
    }

    #[test]
    fn shell_navigation_moves_within_list_and_stops_at_edges() {
        let mut state = make_state();
        state.shells.push(running_shell("bash_1"));
        state.shells.push(running_shell("bash_2"));
        state.shells.push(running_shell("bash_3"));
        state.ui_focus = UiFocus::Shells;
        state.selected_shell = Some("bash_1".to_string());

        assert!(state.move_selected_shell(1));
        assert_eq!(state.selected_shell.as_deref(), Some("bash_2"));
        assert!(state.move_selected_shell(1));
        assert_eq!(state.selected_shell.as_deref(), Some("bash_3"));
        // 已在最后一条：返回 false 交给上层跨区，不会越界
        assert!(!state.move_selected_shell(1));
        assert_eq!(state.selected_shell.as_deref(), Some("bash_3"));
    }

    #[test]
    fn toggle_detail_flips_shell_detail_open() {
        let mut state = make_state();
        state.shells.push(running_shell("bash_1"));
        state.selected_shell = Some("bash_1".to_string());
        state.ui_focus = UiFocus::Shells;

        state.toggle_focused_content_detail();
        assert!(state.shell_detail_open);
        state.toggle_focused_content_detail();
        assert!(!state.shell_detail_open);
    }

    // ── ESC 中断后暂停自动唤醒 ────────────────────────────────────────

    #[test]
    fn interrupt_pauses_auto_wake_and_new_turn_resumes_it() {
        let mut state = make_state();
        assert!(!state.auto_wake_paused);

        state.interrupt();
        assert!(
            state.auto_wake_paused,
            "用户主动打断后，后台任务完成不应再自动把 Agent 唤醒"
        );

        state.begin_new_turn();
        assert!(
            !state.auto_wake_paused,
            "用户自己发消息即视为恢复对自动唤醒的信任"
        );
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
}

#[cfg(test)]
mod evolution_dialog_tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, Arc<EvolutionStore>) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let store = Arc::new(
            EvolutionStore::new(dir.path(), &repo, wyj_config::EvolutionCfg::default()).unwrap(),
        );
        (dir, store)
    }

    #[test]
    fn dialog_cycles_all_four_views_and_clears_confirmation() {
        let (_dir, store) = test_store();
        let mut session = wyj_core::Session::new();
        session.push_user("cancelled goal");
        let capture = store.begin_episode(
            "dialog-session",
            &session,
            "cancelled goal",
            "default",
            "test-vendor",
            "test-model",
        );
        store.cancel_episode(capture).unwrap();

        let mut dialog = EvolutionDialog::new(store);
        assert_eq!(dialog.view.label(), "Active");
        dialog.confirm = Some(EvolutionConfirmAction::ForgetMemory("memory-1".into()));

        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Candidates");
        assert!(dialog.confirm.is_none());
        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Episodes");
        assert!(matches!(
            dialog.rows.first(),
            Some(EvolutionRow::Episode(_))
        ));
        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Health");
        assert!(dialog
            .rows
            .iter()
            .any(|row| matches!(row, EvolutionRow::Health { label, .. } if label == "project")));
        dialog.cycle_view(true);
        assert_eq!(dialog.view.label(), "Active");
        dialog.cycle_view(false);
        assert_eq!(dialog.view.label(), "Health");
    }
}

#[cfg(test)]
mod mcp_dialog_tests {
    use super::*;

    /// 回归测试：wyj 自己项目级来源（`<git-root>/.wyj-code/mcp.toml`）的
    /// server 被卸载后（v1.5.15 起走"从来源文件删行 + lockfile 移除"统一
    /// 逻辑，不再有原生来源的"禁用 shadow"兜底分支），应该从 `/mcp` 面板
    /// "已安装"列表里消失。
    #[test]
    fn uninstalled_wyj_project_server_disappears_from_installed_list() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(
            dir.path(),
            &[wyj_config::McpServerConfig {
                name: "wyj-srv".to_string(),
                transport: wyj_config::McpTransport::Stdio,
                command: Some("npx".to_string()),
                args: vec!["-y".to_string(), "wyj-srv-mcp".to_string()],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
        )
        .unwrap();

        let cfg = Config::default();
        let dialog = McpDialog::new(&cfg, dir.path());
        assert!(dialog.installed.iter().any(|r| r.config.name == "wyj-srv"));

        wyj_store::mcp_install::uninstall_mcp_server(
            "wyj-srv",
            wyj_store::InstallScope::Project,
            dir.path(),
        )
        .unwrap();

        let dialog = McpDialog::new(&cfg, dir.path());
        assert!(!dialog.installed.iter().any(|r| r.config.name == "wyj-srv"));
    }

    /// 回归测试：真正 wyj 自己 registry 安装、受 lockfile 管理的 server，
    /// 即使名字恰好和 `.mcp.json`/`~/.claude.json` 里的原生条目撞名，禁用后
    /// 也不应该被当成"卸载不掉的原生条目"一起从列表里隐藏——否则用户没法
    /// 再找到它重新启用。区分点是 `managed.is_managed()`：原生 shadow 记录
    /// 永远不是 managed，真正 registry 安装的记录永远是。
    #[test]
    fn disabling_managed_server_stays_visible_even_if_name_collides_with_native() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers": {"shared-name": {"command": "npx", "args": []}}}"#,
        )
        .unwrap();

        let server = wyj_store::registry::RegistryServerSummary {
            name: "io.modelcontextprotocol/shared-name".to_string(),
            description: "test".to_string(),
            version: "1.0.0".to_string(),
            packages: vec![wyj_store::registry::RegistryPackage {
                registry_type: "npm".to_string(),
                identifier: "shared-name-mcp".to_string(),
                version: "1.0.0".to_string(),
                registry_base_url: None,
                runtime_hint: Some("npx".to_string()),
                package_arguments: vec![],
                runtime_arguments: vec![],
                environment_variables: vec![],
            }],
            remotes: vec![],
        };
        let package = wyj_store::mcp_install::choose_package(&server.packages);
        let req = wyj_store::mcp_install::McpInstallRequest {
            server,
            package,
            scope: wyj_store::InstallScope::Project,
            name_override: Some("shared-name".to_string()),
            registry_url: "https://registry.modelcontextprotocol.io".to_string(),
        };
        wyj_store::mcp_install::install_mcp_server(&req, dir.path()).unwrap();

        wyj_store::mcp_install::set_mcp_enabled(
            "shared-name",
            wyj_store::InstallScope::Project,
            dir.path(),
            false,
        )
        .unwrap();

        let cfg = Config::default();
        let dialog = McpDialog::new(&cfg, dir.path());
        let row = dialog
            .installed
            .iter()
            .find(|r| r.config.name == "shared-name")
            .expect("受管理的 server 禁用后仍应留在已安装列表里");
        assert!(row.managed.as_ref().is_some_and(|m| m.is_managed()));
        assert!(!row.managed.as_ref().unwrap().enabled);
    }
}

// 首次启动缺 API Key 的 ProfileDialog 引导测试
#[cfg(test)]
mod profile_dialog_onboarding_tests {
    use super::*;

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

    #[test]
    fn new_for_onboarding_focuses_on_api_key_field() {
        let cfg = Config {
            active_profile: "default".to_string(),
            ..Config::default()
        };
        let d = ProfileDialog::new_for_onboarding(&cfg);
        assert_eq!(d.expanded, Some(0), "默认展开 active profile(0)");
        // Header(active_idx) + Field(active_idx,0) + ... + Field(active_idx,5) = 6 行
        assert_eq!(
            d.cursor,
            1 + PROFILE_API_KEY_FIELD_IDX,
            "光标应预置到 api_key 字段"
        );
        // 该行确实指向 api_key(下标 = PROFILE_API_KEY_FIELD_IDX)
        let rows = d.rows();
        if let Some(crate::app::ProfileRow::Field(entry, field)) = rows.get(d.cursor) {
            assert_eq!(*entry, 0);
            assert_eq!(*field, PROFILE_API_KEY_FIELD_IDX);
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
}

#[cfg(test)]
mod skills_registry_tests {
    use super::*;

    fn sample_skill(slug: &str) -> wyj_store::skillhub::SkillHubSkill {
        wyj_store::skillhub::SkillHubSkill {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            description: "测试技能".to_string(),
            version: "1.2.3".to_string(),
            source: "clawhub".to_string(),
            namespace: "someone".to_string(),
            canonical_name: format!("@someone/{slug}"),
            owner_name: "someone".to_string(),
            stars: 42,
            installs: 100,
        }
    }

    /// 目录扫描必须认 `<name>/SKILL.md` 这种目录式技能，且命令名取**目录名**。
    ///
    /// 回归点：HTTP 注册表装来的技能是目录形态，早期实现只扫 `*.md`，
    /// 结果是「装成功了但 Installed tab 里看不见、/help 里也没有」。
    #[test]
    fn directory_shaped_skills_are_scanned_with_their_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(skills.join("demo-skill")).unwrap();
        std::fs::write(
            skills.join("demo-skill").join("SKILL.md"),
            "# 目录式技能\n正文",
        )
        .unwrap();
        // references/ 下的 .md 是技能私有资源，不该被当成独立技能收录。
        std::fs::create_dir_all(skills.join("demo-skill").join("references")).unwrap();
        std::fs::write(skills.join("demo-skill/references/api.md"), "# api").unwrap();
        // 单文件形态仍要照常收录。
        std::fs::write(skills.join("flat.md"), "# 单文件技能\n正文").unwrap();

        let found = scan_skill_dir_for_display(&skills);
        let names: Vec<_> = found.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&"demo-skill".to_string()), "got {names:?}");
        assert!(names.contains(&"flat".to_string()), "got {names:?}");
        assert!(
            !names.contains(&"api".to_string()),
            "references/*.md 不应被当成技能: {names:?}"
        );
        assert!(!names.contains(&"SKILL".to_string()), "got {names:?}");
    }

    /// 搜索无结果时不能因为 `rows()` 首行是搜索框就越界。
    #[test]
    fn browse_tab_always_keeps_a_search_row_at_index_zero() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        let dialog = state.skills_dialog.as_mut().unwrap();
        dialog.tab = SkillsDialogTab::Browse;
        dialog.browse_source = SkillsBrowseSource::None;

        let rows = dialog.rows();
        assert_eq!(rows.len(), 1, "没有任何结果时仍应保留搜索行");
        assert!(matches!(rows[0], FlatRow::AddNew));

        dialog.registry_results = vec![sample_skill("alpha"), sample_skill("beta")];
        dialog.browse_source = SkillsBrowseSource::Registry;
        let rows = dialog.rows();
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], FlatRow::AddNew));
        // Entry 携带的是结果下标，不是行号——否则渲染会取错元素。
        assert!(matches!(rows[1], FlatRow::Entry(0)));
        assert!(matches!(rows[2], FlatRow::Entry(1)));
    }

    /// 搜索结果回来后要同时切 tab、标来源、把光标停在第一条结果上。
    #[test]
    fn registry_search_event_populates_browse_tab() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        state.skills_dialog.as_mut().unwrap().overlay = SkillsOverlay::Searching;

        state.apply_agent_event(AgentEvent::SkillRegistrySearched {
            result: Ok(vec![sample_skill("alpha"), sample_skill("beta")]),
        });

        let dialog = state.skills_dialog.as_ref().unwrap();
        assert!(matches!(dialog.overlay, SkillsOverlay::None));
        assert_eq!(dialog.browse_source, SkillsBrowseSource::Registry);
        assert_eq!(dialog.tab, SkillsDialogTab::Browse);
        assert_eq!(dialog.registry_results.len(), 2);
        // git marketplace 那份列表必须被清空，否则安装会按错的来源走。
        assert!(dialog.browse_results.is_empty());
        assert_eq!(dialog.cursor, 1);
    }

    /// 迟到的旧搜索响应不能覆盖当前界面状态。
    #[test]
    fn stale_registry_search_response_is_ignored() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        // 用户已经关掉搜索浮层、转而干别的事了。
        state.skills_dialog.as_mut().unwrap().overlay = SkillsOverlay::None;

        state.apply_agent_event(AgentEvent::SkillRegistrySearched {
            result: Ok(vec![sample_skill("alpha")]),
        });

        let dialog = state.skills_dialog.as_ref().unwrap();
        assert!(dialog.registry_results.is_empty());
    }

    /// 搜索空结果：留在搜索行，给出提示文案，而不是把 cursor 推到越界位置。
    #[test]
    fn empty_registry_search_keeps_cursor_on_search_row() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        state.skills_dialog.as_mut().unwrap().overlay = SkillsOverlay::Searching;

        state.apply_agent_event(AgentEvent::SkillRegistrySearched {
            result: Ok(Vec::new()),
        });

        let dialog = state.skills_dialog.as_mut().unwrap();
        assert_eq!(dialog.cursor, 0);
        assert!(dialog.status.is_some(), "空结果应给出提示而不是静默");
        dialog.clamp_cursor();
        assert_eq!(dialog.cursor, 0);
    }

    /// 下载完成的包要按 slug 回查成确认框——下载期间用户可能已经移动了光标。
    #[test]
    fn archive_fetch_event_opens_confirm_for_the_pending_slug() {
        let mut state = make_state();
        state.skills_dialog = Some(SkillsDialog::new(
            std::path::Path::new("/nonexistent-home"),
            std::path::Path::new("/tmp"),
        ));
        {
            let dialog = state.skills_dialog.as_mut().unwrap();
            dialog.tab = SkillsDialogTab::Browse;
            dialog.browse_source = SkillsBrowseSource::Registry;
            dialog.registry_results = vec![sample_skill("alpha"), sample_skill("beta")];
            dialog.overlay = SkillsOverlay::FetchingArchive;
            dialog.pending_registry_slug = "beta".to_string();
            // 下载期间光标被挪到了第一条
            dialog.cursor = 1;
        }

        let preview = wyj_store::skillhub::SkillArchivePreview {
            file_count: 3,
            total_bytes: 2048,
            executable_count: 1,
            executable_paths: vec!["scripts/run.sh".to_string()],
        };
        state.apply_agent_event(AgentEvent::SkillRegistryArchiveFetched {
            result: Ok((preview, vec![0u8, 1, 2])),
        });

        let dialog = state.skills_dialog.as_ref().unwrap();
        match &dialog.overlay {
            SkillsOverlay::RegistryInstallConfirm {
                skill,
                preview,
                scope,
                ..
            } => {
                // 必须命中 beta，而不是光标所在的 alpha。
                assert_eq!(skill.slug, "beta");
                assert_eq!(preview.executable_count, 1);
                assert!(preview.has_executables());
                assert_eq!(*scope, wyj_store::InstallScope::Global);
            }
            _ => panic!("expected a RegistryInstallConfirm overlay"),
        }
    }

    /// 注册表来源与 git marketplace 来源必须能在 lockfile 层面区分开，
    /// 否则升级会对一个 https URL 执行 git clone。
    #[test]
    fn registry_install_is_recorded_as_a_registry_source() {
        let dir = tempfile::tempdir().unwrap();
        let req = wyj_store::skillhub::SkillHubInstallRequest {
            base_url: "https://example.invalid/api/v1".to_string(),
            slug: "demo".to_string(),
            namespace: "someone".to_string(),
            install_name: String::new(),
            version: "1.0.0".to_string(),
            scope: wyj_store::InstallScope::Project,
            source_label: "someone/demo".to_string(),
        };
        wyj_store::skillhub::install_skillhub_archive(&build_test_archive(), &req, dir.path())
            .unwrap();

        let manifest = wyj_store::lockfile::load_project(dir.path()).unwrap();
        let entry = manifest.skills.iter().find(|s| s.name == "demo").unwrap();
        let source = entry.source.as_ref().unwrap();
        assert!(source.is_registry());
        assert_eq!(source.registry.as_ref().unwrap().slug, "demo");
        assert_eq!(source.marketplace_url, "");

        // 卸载要连目录一起删干净，否则 references/*.md 会被加载器当成
        // 额外的 slash command 重新注册。
        wyj_store::skill_install::uninstall_skill(
            "demo",
            wyj_store::InstallScope::Project,
            dir.path(),
        )
        .unwrap();
        assert!(!dir.path().join(".wyj-code/skills/demo").exists());
    }

    fn build_test_archive() -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            w.start_file("SKILL.md", opts).unwrap();
            w.write_all(b"---\nname: demo\ndescription: d\n---\nbody")
                .unwrap();
            w.start_file("references/api.md", opts).unwrap();
            w.write_all(b"# api").unwrap();
            w.finish().unwrap();
        }
        buf
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

    /// 核心回归钉子：task **活着但长时间无进展**时，看门狗必须把状态收回来。
    ///
    /// 这是 `reap_lost_turn_task` 补不了的洞——它只看 `is_finished()`，而
    /// hang 住的 task 该标志恒为 false。漏掉本条的症状就是用户报的
    /// 「跑着跑着停住，之后输入任何东西都没反应」：`is_thinking` 永为 true
    /// 时用户输入全进 `pending_queue`，连主循环的兜底分支都被同一标志挡住。
    #[tokio::test]
    async fn stalled_turn_is_aborted_even_while_task_is_alive() {
        let mut state = make_state();
        state.is_thinking = true;
        // 造一个「活着但卡住」的 task：spawn 一个永不完成的 future，
        // 用它证明看门狗判的是静默时长，而不是 is_finished()。
        let task = spawn_turn_task(std::future::pending::<()>());
        assert!(
            !task.is_finished(),
            "前提：task 必须仍活着，否则测的是 reap 而非心跳"
        );
        state.current_task = Some(task);
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT + Duration::from_secs(5));

        assert!(state.abort_stalled_turn(), "静默超阈值必须触发兜底");
        assert!(
            !state.is_thinking,
            "兜底后必须解锁 thinking，否则用户后续输入仍会滞留"
        );
        assert!(
            state.current_task.is_none(),
            "task 句柄必须清掉，避免下帧重复 abort"
        );
        assert!(
            state
                .messages
                .iter()
                .any(|m| m.is_error && m.content.contains("900")),
            "必须给用户一条可见提示，不能静默中止"
        );
    }

    /// 兜底之后 pending_queue 必须能被排空——这正是用户「输入无响应」的
    /// 直接成因：is_thinking 卡住时，主循环 `!is_thinking &&
    /// !pending_queue.is_empty()` 这条唯一的排空路径也进不去。
    #[tokio::test]
    async fn stalled_turn_recovery_unblocks_pending_queue() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));
        state.pending_queue.push(OutgoingMessage::plain("继续"));
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT + Duration::from_secs(5));

        state.abort_stalled_turn();

        assert!(!state.is_thinking, "解锁后主循环才可能排空 pending_queue");
        assert!(!state.pending_queue.is_empty(), "队列内容应保留待下轮消费");
    }

    /// 不误杀：静默未超阈值时必须什么都不做。
    #[tokio::test]
    async fn active_turn_is_never_aborted_early() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));

        assert!(!state.abort_stalled_turn());
        assert!(state.is_thinking, "健康回合不得被误杀");
        assert!(state.current_task.is_some());
    }

    /// 关键的反误杀回归：弹窗打开期间（用户去倒杯水）**不得**判死。
    ///
    /// 回合此时合法地 await 在 oneshot 上，这不是卡死。若把这段静默计入
    /// 心跳，一次正常的离开几分钟就会把回合杀掉——那是比原 bug 更糟的
    /// 体验（用户在毫不知情的情况下丢掉一段长工作）。
    #[tokio::test]
    async fn permission_dialog_pauses_the_stall_watchdog() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.permission_dialog = Some(PermissionDialog {
            tool_name: "Bash".to_string(),
            action_summary: "cargo test".to_string(),
            response_tx: tx,
        });
        // 静默远超阈值，但弹窗开着
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT * 3);

        assert!(!state.abort_stalled_turn(), "等待用户交互不算卡死");
        assert!(state.is_thinking);
        assert!(state.permission_dialog.is_some(), "弹窗不得被心跳吃掉");
    }

    /// 空闲态（没在思考）不该刷出「回合被中止」的提示。
    #[test]
    fn idle_state_is_never_reported_as_stalled() {
        let mut state = make_state();
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT * 10);
        assert!(!state.abort_stalled_turn());
        assert!(state.messages.is_empty(), "空闲态不得产生任何提示");
    }

    /// 心跳基准必须由事件驱动重置，否则「持续吐 delta 的长回合」会被误杀。
    #[tokio::test]
    async fn any_agent_event_resets_the_stall_clock() {
        let mut state = make_state();
        state.is_thinking = true;
        state.current_task = Some(spawn_turn_task(std::future::pending::<()>()));
        state.last_turn_activity = Instant::now() - (TURN_STALL_TIMEOUT * 3);

        state.apply_agent_event(AgentEvent::TextDelta("还在输出".to_string()));

        assert!(
            !state.abort_stalled_turn(),
            "刚收到事件说明回合健康，不得判死"
        );
        assert!(state.is_thinking);
    }

    /// 阈值必须显著大于任何合法静默窗口（前台 Bash 最大 600s、LLM 流
    /// idle 看门狗 120s），否则工具侧超时还没触发、心跳先把回合杀了，
    /// 健康的长时间构建会被误杀。
    #[test]
    fn stall_threshold_exceeds_the_longest_legal_silent_window() {
        assert!(
            TURN_STALL_TIMEOUT > std::time::Duration::from_secs(600),
            "必须大于 Bash 的 600s 上限，否则长构建会被心看门狗抢先杀掉"
        );
        assert!(
            TURN_STALL_TIMEOUT > wyj_api::stream_guard::DEFAULT_STREAM_IDLE_TIMEOUT * 2,
            "应显著大于流级 idle 看门狗，让工具侧超时先触发"
        );
    }
}
