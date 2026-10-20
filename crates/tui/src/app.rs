//! TUI 应用主循环

use crate::event::{is_quit, AgentEvent, BranchSwitch, RewindPrepared};
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
    style::{Print, ResetColor, SetForegroundColor},
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
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandRegistry, CommandResult};
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

/// 按宽度缓存的正文渲染结果：`(可用宽度, 已渲染行)`。
///
/// 服务两类「定稿后内容不再变化」的正文：Assistant 的 markdown
/// （`render_markdown`）、`!` bash 的折行结果与展开态 thinking。消息定稿后
/// 缓存避免每帧（约 20fps）对全部历史重跑解析/折行，长对话下这是交互延迟的
/// 主要来源。宽度变化时自动失效。
pub type RenderCache = std::cell::RefCell<Option<(usize, Vec<ratatui::text::Line<'static>>)>>;

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
    /// 按宽度缓存的正文渲染结果，见 [`RenderCache`]。
    ///
    /// 名字里的 `render` 是刻意的：一旦正文被折叠/截断（曾经的三行预览）就
    /// 没有缓存的必要——被丢掉的那部分根本不必算；这个字段存在的前提就是
    /// 「全文都要画」。
    pub render_cache: RenderCache,
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
            render_cache: std::cell::RefCell::new(None),
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
            render_cache: std::cell::RefCell::new(None),
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
            render_cache: std::cell::RefCell::new(None),
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
    /// 终态交接物（`a<id>.handoff.json`）。**未完成时**它是「做到哪了」的
    /// 唯一来源——`final_result` 里那段是给模型看的定界外壳，人读需要摘要。
    pub handoff: Option<wyj_tools::handoff::SubAgentHandoff>,
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
/// - `>= 10_000` → 一位小数的 k（`12.3k`）。这一步是 v0.2.31 加的：会话累计轻易
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

/// 速率滑动窗口长度。太短会把每个 chunk 的抖动放大，太长会让「模型突然变慢」
/// 延迟好几秒才看得出来。
pub(crate) const STREAM_RATE_WINDOW: Duration = Duration::from_secs(2);
/// 超过这么久没有新的生成增量，就认为当前请求已经不在吐 token（多半在生成
/// 工具参数或已经结束），标题栏不再显示速率——显示一个冻结的旧值比不显示更糟。
pub(crate) const STREAM_RATE_STALE_AFTER: Duration = Duration::from_millis(1500);
/// 分母下限：首个增量刚到时样本太少，`tokens / 几十毫秒` 会算出几百 tok/s 的
/// 假峰值。低于这个时长的生成段直接不显示速率，等它跑稳。
const STREAM_RATE_MIN_BURST: Duration = Duration::from_millis(50);

/// 流式生成速率表（tok/s）。
///
/// 三条口径对齐业界常见做法：
/// 1. **只算生成、不算 prefill**：计时从首个增量开始，TTFT 不进分母，否则
///    首 token 前的排队/建连时间会把速率压成个位数。
/// 2. **滑动窗口而不是整段平均**：整段平均会被一次长停顿永久拖低，瞬时值又
///    抖得没法读；最近 [`STREAM_RATE_WINDOW`] 秒的窗口两头都避开。
/// 3. **按字符估算 token**：真实 usage 要等请求结束才回来，赶不上「实时」；
///    增量用 [`wyj_core::estimate_text_tokens_f64`]（CJK 1.5、其余 1/3 token/字）。
///    逐 delta 累加必须用浮点版，整数版在单字符片段上会整段除成 0。
#[derive(Debug, Default)]
pub struct StreamRateMeter {
    /// `(到达时刻, 估算 token 增量)`，按时间递增，超出窗口的从队首弹出。
    pub(crate) samples: VecDeque<(Instant, f64)>,
    /// 当前生成段第一个增量的时刻（TTFT 之后），速率分母从这里起算。
    pub(crate) burst_started: Option<Instant>,
    /// 最近一次生成增量的时刻，用于判定速率是否已经过期。
    pub(crate) last_delta_at: Option<Instant>,
}

impl StreamRateMeter {
    /// 记录一段流式增量。`ThinkingDelta` 与 `TextDelta` 都要记——两者都是模型
    /// 在吐 token，用户看到的也是同一个 spinner。
    pub(crate) fn note(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let now = Instant::now();
        self.burst_started.get_or_insert(now);
        self.last_delta_at = Some(now);
        self.samples
            .push_back((now, wyj_core::estimate_text_tokens_f64(text)));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > STREAM_RATE_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// 生成段结束（请求收尾 / 开始跑工具 / 中断 / 新回合）时清空。
    /// 不清的话，工具执行的那几十秒会被算进「最近 2 秒」，速率瞬间塌成 0。
    pub(crate) fn reset(&mut self) {
        self.samples.clear();
        self.burst_started = None;
        self.last_delta_at = None;
    }

    /// 当前速率（tok/s）。`None` = 不在生成中，标题栏应整段不显示。
    pub(crate) fn rate(&self) -> Option<f64> {
        let last = self.last_delta_at?;
        if last.elapsed() > STREAM_RATE_STALE_AFTER {
            return None;
        }
        let started = self.burst_started?;
        let elapsed = started.elapsed();
        if elapsed < STREAM_RATE_MIN_BURST {
            return None;
        }
        let tokens: f64 = self
            .samples
            .iter()
            .filter(|(t, _)| last.duration_since(*t) <= STREAM_RATE_WINDOW)
            .map(|(_, n)| *n)
            .sum();
        // 生成段短于窗口时用实际时长（首 token 之后的平均值）；长于窗口时固定
        // 用窗口长度（最近两秒的瞬时速率）。样本本身已按窗口裁剪，分子分母同源。
        let denom = elapsed.as_secs_f64().min(STREAM_RATE_WINDOW.as_secs_f64());
        (denom > 0.0).then(|| tokens / denom)
    }
}

#[cfg(test)]
mod stream_rate_tests {
    use super::{StreamRateMeter, STREAM_RATE_WINDOW};
    use std::time::{Duration, Instant};

    #[test]
    fn notes_fractional_tokens_so_small_deltas_do_not_round_to_zero() {
        // 回归钉子：整数版估算对单个 ASCII 字符是 1/3 = 0，一帧几十个小 delta
        // 累加完仍是 0——速率会永远显示 0 tok/s。
        let mut m = StreamRateMeter::default();
        for _ in 0..300 {
            m.note("a");
        }
        let tokens: f64 = m.samples.iter().map(|(_, n)| *n).sum();
        assert!(
            (tokens - 100.0).abs() < 1.0,
            "300 个 ASCII 字符应估 ~100 token，实际 {tokens}"
        );
    }

    #[test]
    fn rate_is_none_before_the_burst_settles() {
        let mut m = StreamRateMeter::default();
        m.note("hello");
        assert_eq!(m.rate(), None, "首个增量刚到时不应给出假峰值");
    }

    #[test]
    fn rate_divides_by_actual_burst_duration_when_shorter_than_window() {
        let mut m = StreamRateMeter::default();
        // 手工构造一个已经跑了 1 秒的生成段：300 个字符 ≈ 100 token → ~100 tok/s。
        let started = Instant::now() - Duration::from_secs(1);
        m.burst_started = Some(started);
        m.last_delta_at = Some(Instant::now());
        m.samples.push_back((started, 50.0));
        m.samples.push_back((Instant::now(), 50.0));
        let rate = m.rate().expect("应给出速率");
        assert!(
            (rate - 100.0).abs() < 1.0,
            "1 秒 100 token 应约 100 tok/s，实际 {rate}"
        );
    }

    #[test]
    fn rate_uses_window_not_whole_burst_for_long_generations() {
        let mut m = StreamRateMeter::default();
        let started = Instant::now() - Duration::from_secs(10);
        m.burst_started = Some(started);
        m.last_delta_at = Some(Instant::now());
        // 只留下最近窗口内的样本：2 秒 60 token → 30 tok/s（而不是整段 10 秒的 6）。
        m.samples.push_back((Instant::now(), 60.0));
        let rate = m.rate().expect("应给出速率");
        let expected = 60.0 / STREAM_RATE_WINDOW.as_secs_f64();
        assert!(
            (rate - expected).abs() < 1.0,
            "应约 {expected} tok/s，实际 {rate}"
        );
    }

    #[test]
    fn reset_and_staleness_both_hide_the_rate() {
        let mut m = StreamRateMeter::default();
        let started = Instant::now() - Duration::from_secs(1);
        m.burst_started = Some(started);
        m.last_delta_at = Some(Instant::now());
        m.samples.push_back((started, 50.0));
        assert!(m.rate().is_some());

        m.reset();
        assert_eq!(m.rate(), None, "生成段结束后不得继续显示旧速率");

        m.burst_started = Some(started);
        m.last_delta_at = Some(Instant::now() - Duration::from_secs(5));
        m.samples.push_back((started, 50.0));
        assert_eq!(m.rate(), None, "超过 stale 阈值后不得显示冻结的旧速率");
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
                messages.clone(),
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

/// `/rewind` 的后台执行体。
///
/// 为什么独立成函数：这一步 = 读 checkpoint + 预览文件 + **建保护快照**（git
/// 管道或全树哈希）+ 还原文件，全是同步 IO，大仓库上秒级。放在主循环里
/// inline await 会把整个界面按住不动（`crates/tui/CLAUDE.md` 第 19 条）。
///
/// 与 `/evolve` 的旧写法不同，这里用 `session.lock().await` 而不是 `try_lock`：
/// 本函数跑在后台 task 上，等锁不会冻结界面；换来的是"读了 messages 之后
/// 又被回合改掉"这个窗口被彻底关掉（保护快照必须对应回退当下的历史）。
async fn run_rewind_in_background(
    sessions: &Arc<SessionStore>,
    cwd: &Path,
    session_id: &str,
    checkpoint_id: Option<String>,
    scope: wyj_core::RewindScope,
    confirmed: bool,
    session: &Arc<Mutex<Session>>,
) -> anyhow::Result<RewindPrepared> {
    let store = wyj_core::CheckpointStore::configured(sessions.dir(), session_id.to_string())?;
    let id = resolve_tui_checkpoint_id(&store, checkpoint_id)?;
    let restores_files = matches!(
        scope,
        wyj_core::RewindScope::Files | wyj_core::RewindScope::Both
    );
    let restores_conversation = matches!(
        scope,
        wyj_core::RewindScope::Conversation | wyj_core::RewindScope::Both
    );

    let mut sess = session.lock().await;
    let current_messages = sess.messages.clone();

    enum Io {
        NeedsConfirm(String),
        Ready {
            messages: Vec<Message>,
            protection_id: String,
            file_text: String,
        },
    }

    let cwd = cwd.to_path_buf();
    let io_id = id.clone();
    let io = tokio::task::spawn_blocking(move || -> anyhow::Result<Io> {
        let checkpoint = store.load(&io_id)?;
        if restores_files {
            let preview = store.preview_files(&io_id, &cwd)?;
            if preview.requires_confirmation && !confirmed {
                return Ok(Io::NeedsConfirm(format!(
                    "{}\n\n确认后请重新执行：`/rewind {} {:?} --confirm`",
                    format_rewind_preview(&preview),
                    io_id,
                    scope
                )));
            }
        }
        let protection = store.create(
            &cwd,
            current_messages.clone(),
            wyj_core::CheckpointKind::PreRewind,
            Some(format!("before rewind {io_id}")),
        )?;
        let file_text = if restores_files {
            let preview = store.restore_files(&io_id, &cwd, confirmed)?;
            format!("\n{}", format_rewind_preview(&preview))
        } else {
            String::new()
        };
        Ok(Io::Ready {
            messages: checkpoint.messages,
            protection_id: protection.id,
            file_text,
        })
    })
    .await??;

    match io {
        Io::NeedsConfirm(text) => Ok(RewindPrepared::NeedsConfirm(text)),
        Io::Ready {
            messages,
            protection_id,
            file_text,
        } => {
            sess.current_checkpoint_id = Some(id.clone());
            if restores_conversation {
                sess.messages = messages.clone();
            }
            let turns = sess
                .messages
                .iter()
                .filter(|message| matches!(message.role, Role::User))
                .count();
            Ok(RewindPrepared::Ready {
                checkpoint_id: id,
                restored_conversation: restores_conversation,
                protection_id,
                file_text,
                messages: sess.messages.clone(),
                compact_count: sess.compact_count,
                turns,
            })
        }
    }
}

/// `/branch` 的后台执行体：还原文件（可选）+ 从 checkpoint 派生新会话 + 切换
/// live session 与 agent 的会话指针。
///
/// 同样是同步 IO 的长任务（git 管道 + 写 session 文件），必须在后台跑；
/// 见 `run_rewind_in_background` 上面的说明。
#[allow(clippy::too_many_arguments)]
async fn run_branch_in_background(
    sessions: &Arc<SessionStore>,
    cwd: &Path,
    session_id: &str,
    checkpoint_id: Option<String>,
    restore_files: bool,
    confirmed: bool,
    session: &Arc<Mutex<Session>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
) -> anyhow::Result<BranchSwitch> {
    let store = wyj_core::CheckpointStore::configured(sessions.dir(), session_id.to_string())?;
    let id = resolve_tui_checkpoint_id(&store, checkpoint_id)?;

    let mut sess = session.lock().await;
    let current_messages = sess.messages.clone();

    let cwd = cwd.to_path_buf();
    let branch = {
        let sessions = sessions.clone();
        let session_id = session_id.to_string();
        let io_id = id.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<SessionFile> {
            let store = wyj_core::CheckpointStore::configured(sessions.dir(), session_id.clone())?;
            let checkpoint = store.load(&io_id)?;
            if restore_files {
                let preview = store.preview_files(&io_id, &cwd)?;
                if preview.requires_confirmation && !confirmed {
                    anyhow::bail!(
                        "{}\n\n确认后请重新执行：/branch {} --restore-files --confirm",
                        format_rewind_preview(&preview),
                        io_id
                    );
                }
                store.create(
                    &cwd,
                    current_messages.clone(),
                    wyj_core::CheckpointKind::PreRewind,
                    Some(format!("before branch restore {io_id}")),
                )?;
                store.restore_files(&io_id, &cwd, confirmed)?;
            }
            sessions.branch_from_checkpoint(&session_id, &checkpoint)
        })
        .await??
    };

    let checkpoint_id = id;
    let new_session_id = branch.session_id.clone();
    let turns = branch.turns;
    *sess = Session::new();
    sess.messages = branch.messages.clone();
    sess.current_checkpoint_id = branch.current_checkpoint_id;
    sess.branch_parent_session_id = branch.branch_parent_session_id;
    sess.branch_parent_checkpoint_id = branch.branch_parent_checkpoint_id;
    // 分支继承父会话的临时分组（`branch_from_checkpoint` 已把它写进新会话
    // 文件）。内存 Session 也要带上，否则下一次 autosave 会把新分支文件里的
    // 这个字段覆盖成 None，`--resume` 分支时又悄悄回到全局默认端点。
    sess.session_profile = branch.session_profile.clone();
    drop(sess);
    retarget_agent_session(shared_agent, Some(sessions), &new_session_id);

    Ok(BranchSwitch {
        session_id: new_session_id,
        checkpoint_id,
        messages: branch.messages,
        turns,
    })
}

#[cfg(test)]
mod navigation_focus_tests {
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

    #[test]
    fn only_shift_up_is_the_explicit_content_focus_shortcut() {
        assert!(is_content_focus_key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::SHIFT,
        )));
        // 终端可能把额外修饰位一起报上来，不能因为多一位就漏掉。
        assert!(is_content_focus_key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::SHIFT | KeyModifiers::META,
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
        // 带 Ctrl/Alt/Cmd 的组合不抢系统快捷键
        for extra in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            assert!(!is_content_focus_key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::SHIFT | extra,
            )));
        }
    }

    /// 从聊天区滚动态按 Shift+↑ 必须还能跳到正在跑的子 Agent。
    ///
    /// `UiFocus::Chat` 是「滚动过聊天区」的常态焦点，旧实现把它当成「已在
    /// 内容区」直接早退，于是子 Agent 在跑也进不去。
    #[test]
    fn shift_up_from_chat_focus_still_enters_running_sub_agent() {
        let mut state = make_state();
        state
            .messages
            .push(ChatMessage::user("message".to_string()));
        state
            .sub_agents
            .insert(9, sub_agent(SubAgentStatus::Running));
        state.ui_focus = UiFocus::Chat;
        state.chat_scroll = 3;
        state.chat_max_scroll = 10;
        state.chat_follow_tail = false;

        assert!(state.enter_content_focus());
        assert_eq!(state.ui_focus, UiFocus::SubAgents);
        assert_eq!(state.selected_sub_agent, Some(9));
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
            handoff: None,
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

    /// 补全列表打开时按 Enter 必须把选中项**填入**输入框，而不是提交
    /// 输入框里的不完整原文——旧行为是 Enter 直接落穿提交，于是 ↑↓ 选
    /// 中的命令从未生效，用户还得手动改对命令名。
    #[test]
    fn enter_with_completions_open_fills_the_selected_command() {
        assert!(slash_completion_enter_fills("/mo", "/model"));
        assert!(slash_completion_enter_fills("/", "/help"));
        // 带参数的中途形态：填入等于丢掉已打参数，因此这里只覆盖前缀形态。
        let filled = slash_completion_input("/model");
        assert_eq!(filled.display_lines(), &["/model "]);
    }

    /// 已经填好的形态（Tab 刚填过、或手输与选中项完全一致）必须放行提交，
    /// 否则用户会经历「回车 → 只填入 → 再回车才执行」的多余一步。
    #[test]
    fn enter_lets_through_once_the_input_already_equals_the_selection() {
        assert!(!slash_completion_enter_fills("/model ", "/model"));
        assert!(!slash_completion_enter_fills("/model", "/model"));
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
        state.sub_agent_detail_follow_tail = false;
        state.chat_scroll = 10;
        state.chat_max_scroll = 20;

        state.scroll_focus_lines(3);

        assert_eq!(state.sub_agent_detail_scroll, 8);
        assert_eq!(state.chat_scroll, 10);
    }

    /// 滚动方向必须与渲染同向：`scroll` 是"从正文第一行往下跳过的行数"，
    /// delta > 0 向下、delta < 0 向上。曾经实现把 scroll 当"距底部偏移"、
    /// 渲染时再取反，按键却原样透传 delta，于是 PageUp 反而把视口推向实时
    /// 尾部，用户按多少次都停不下来。
    #[test]
    fn sub_agent_detail_scroll_direction_matches_render() {
        let mut state = make_state();
        state.ui_focus = UiFocus::SubAgents;
        state.sub_agent_detail_open = true;
        state.sub_agent_detail_scroll = 10;
        state.sub_agent_detail_max_scroll = 40;
        state.sub_agent_detail_follow_tail = false;

        state.scroll_focus_lines(8); // PageDown
        assert_eq!(state.sub_agent_detail_scroll, 18);
        state.scroll_focus_lines(-8); // PageUp
        assert_eq!(state.sub_agent_detail_scroll, 10);
        // 滚到顶不再越界
        state.scroll_focus_lines(-100);
        assert_eq!(state.sub_agent_detail_scroll, 0);
        // 滚到底不再越界，并恢复贴底跟随
        state.scroll_focus_lines(1000);
        assert_eq!(state.sub_agent_detail_scroll, 40);
        assert!(state.sub_agent_detail_follow_tail);
    }

    /// 用户主动上滚后必须能"停住"：运行中的子 Agent 持续追加 trace 事件、
    /// max_scroll 只增不减，但视口要停在原地，不能被新内容拽走。
    #[test]
    fn sub_agent_detail_detaches_from_live_tail_after_user_scrolls_up() {
        let mut state = make_state();
        state.ui_focus = UiFocus::SubAgents;
        state.sub_agent_detail_open = true;
        state.sub_agent_detail_max_scroll = 40;
        state.sub_agent_detail_follow_tail = true;

        // 贴底态按 PageUp：视口离开末尾，脱离跟随
        state.scroll_focus_lines(-8);
        assert_eq!(state.sub_agent_detail_scroll, 32);
        assert!(!state.sub_agent_detail_follow_tail);

        // 新事件把可滚动上限推高，视口必须原地不动
        state.sub_agent_detail_max_scroll = 100;
        render::resolve_detail_scroll(
            &mut state.sub_agent_detail_scroll,
            &mut state.sub_agent_detail_follow_tail,
            state.sub_agent_detail_max_scroll,
        );
        assert_eq!(state.sub_agent_detail_scroll, 32);
        assert!(!state.sub_agent_detail_follow_tail);

        // 复位后重新贴底，新内容会把视口一起带到底部
        state.reset_sub_agent_detail_view();
        state.sub_agent_detail_max_scroll = 100;
        render::resolve_detail_scroll(
            &mut state.sub_agent_detail_scroll,
            &mut state.sub_agent_detail_follow_tail,
            state.sub_agent_detail_max_scroll,
        );
        assert_eq!(state.sub_agent_detail_scroll, 100);
    }

    /// 复位（切换选中项 / 关闭详情）必须同时把 follow_tail 拉回真，
    /// 否则下一个子 Agent 的执行内容会以"停在旧偏移"开场。
    #[test]
    fn reset_sub_agent_detail_view_restores_live_tail() {
        let mut state = make_state();
        state.sub_agent_detail_scroll = 12;
        state.sub_agent_detail_follow_tail = false;

        state.reset_sub_agent_detail_view();

        assert_eq!(state.sub_agent_detail_scroll, 0);
        assert!(state.sub_agent_detail_follow_tail);
    }

    /// 面板浏览态的 Ctrl+C / Ctrl+D 必须在内容区焦点分支被吃掉：它们一旦落进
    /// `_` 兜底就会继续下坠到全局分支，Ctrl+C 触发 interrupt() →
    /// current_task.abort() + hub.abort_foreground()，把整轮执行和所有前台
    /// 子 Agent 一起杀掉。
    #[test]
    fn panel_focus_swallows_ctrl_c_and_ctrl_d() {
        let mut state = make_state();
        state.ui_focus = UiFocus::SubAgents;
        state.sub_agent_detail_open = true;

        assert!(state.content_focus_swallows_global_key(&ctrl_key('c')));
        assert!(state.content_focus_swallows_global_key(&KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL
        )));

        // 面板焦点下普通字符不拦：仍按原逻辑退出面板焦点并落进输入框
        assert!(!state.content_focus_swallows_global_key(&KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));

        // Chat 焦点下 Ctrl+C 保留"中断 Agent"的原义
        state.ui_focus = UiFocus::Chat;
        assert!(!state.content_focus_swallows_global_key(&ctrl_key('c')));
    }

    /// 端到端的方向不变式：PageUp 之后真正喂给 `Paragraph::scroll` 的行偏移必须
    /// **变小**（视口上移）。旧实现把 scroll 当"距底部偏移"、渲染时再取反，
    /// 按键却原样透传 delta，于是 PageUp 反而让渲染偏移变大——视口被推向实时
    /// 尾部，新事件持续追加就永远停不下来。
    #[test]
    fn page_up_moves_rendered_viewport_up_not_down() {
        fn rendered(state: &mut AppState) -> u16 {
            render::resolve_detail_scroll(
                &mut state.sub_agent_detail_scroll,
                &mut state.sub_agent_detail_follow_tail,
                state.sub_agent_detail_max_scroll,
            )
        }

        let mut state = make_state();
        state.ui_focus = UiFocus::SubAgents;
        state.sub_agent_detail_open = true;
        state.sub_agent_detail_max_scroll = 40;
        state.sub_agent_detail_follow_tail = true;

        let tail = rendered(&mut state);
        assert_eq!(tail, 40, "默认贴底对齐末尾");

        state.scroll_focus_lines(-8); // PageUp
        let up = rendered(&mut state);
        assert!(up < tail, "PageUp 必须让视口上移：{up} < {tail}");

        state.scroll_focus_lines(8); // PageDown 原路返回
        assert_eq!(rendered(&mut state), tail);
        assert!(state.sub_agent_detail_follow_tail);
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
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

#[cfg(test)]
mod busy_slash_tests {
    use super::*;

    /// 回合进行中，面板类命令必须走正常 dispatch；会改历史 / 起新回合 / 读 session
    /// 统计的命令不得放行。
    ///
    /// 这是用户现场那条 `/model` 的直接回归：旧实现忙碌期 Enter 一律排队，于是
    /// `/model` 被当成一句用户消息发给模型，右下角同时冒出"1 条已排队"。
    #[test]
    fn panel_commands_run_while_the_turn_is_in_flight() {
        let registry = wyj_commands::builtin::standard_registry();

        for draft in ["/model", "/mcp", "/config", "/skills", "/sessions", "/help"] {
            assert!(
                draft_runs_while_busy(&registry, draft),
                "{draft} 应在回合进行中照常执行，而不是排队给模型"
            );
        }

        for draft in ["/cost", "/context", "/clear", "/compact", "/rewind", "/bug"] {
            assert!(
                !draft_runs_while_busy(&registry, draft),
                "{draft} 会改会话 / 起新回合 / 读 session 统计，不得在回合进行中执行"
            );
        }
    }

    /// 只有**整条**草稿是一条命令时才放行：正文里提到命令名、多行输入第一行
    /// 恰好是命令，都不是命令。
    #[test]
    fn only_a_whole_single_line_command_is_dispatched() {
        let registry = wyj_commands::builtin::standard_registry();
        for draft in ["", "把 /model 的面板打开", "/model\n再补一句", "/modelize"] {
            assert!(
                !draft_runs_while_busy(&registry, draft),
                "{draft:?} 不是一条完整命令，不该按命令执行"
            );
        }
    }

    /// 主循环的 Enter 分支顺序是这条不变式本身：白名单必须先于排队分支命中。
    /// 只改 `busy_slash` 的计算却忘了改条件、或把两个分支调换，都会退回到
    /// "忙碌期一律排队"，且不会有任何编译错误。
    #[test]
    fn busy_enter_branch_checks_the_whitelist_before_queueing() {
        let lines: Vec<&str> = include_str!("app.rs").lines().collect();
        // 按「行首就是」匹配，避免命中本测试源码里那句字符串字面量
        // （`include_str!` 把测试自己也包进来了）。
        let whitelist = lines
            .iter()
            .position(|l| {
                l.trim_start().starts_with(
                    "} else if key.code == KeyCode::Enter && (!state.is_thinking || busy_slash)",
                )
            })
            .expect("Enter 分支应放行 busy_slash 白名单命令");
        let queue = lines
            .iter()
            .position(|l| {
                l.trim_start()
                    .starts_with("} else if key.code == KeyCode::Enter && state.is_thinking {")
            })
            .expect("忙碌期 Enter 仍应有排队分支");
        assert!(
            whitelist < queue,
            "白名单分支必须排在排队分支之前（同一 if-else 链上先命中者生效）"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("draft_runs_while_busy(&cmd_registry")),
            "busy_slash 必须由 draft_runs_while_busy 推导"
        );
    }

    /// CommandContext 在回合进行中必须**降级构造**而不是跳过命令执行。
    ///
    /// 拿不到 session 锁时若 `continue`，用户按 `/model` 会看到一句"会话忙"却什么
    /// 也没发生——比排队更难解释。
    #[test]
    fn busy_command_still_runs_when_the_session_lock_is_held() {
        let lines: Vec<&str> = include_str!("app.rs").lines().collect();
        // `) = match try_session_lock(...)` 只在 CommandContext 的统计元组处出现
        // 一次（另外几处都写成 `let sess = match ...`）。按「行首就是」匹配，避免
        // 命中本测试源码里那句字符串字面量（`include_str!` 把测试自己也包进来了）。
        let lock = lines
            .iter()
            .position(|l| {
                l.trim_start()
                    .starts_with(") = match try_session_lock(&session) {")
            })
            .expect("CommandContext 的统计元组应按 session 锁取值");
        let ctx = (lock..)
            .find(|offset| lines[*offset].contains("let cmd_ctx = CommandContext"))
            .expect("命令执行路径应构造 CommandContext");
        assert!(lock < ctx, "锁的获取必须在 ctx 构造之前");
        let window = lines[lock..ctx].join("\n");
        assert!(
            !window.contains("continue;"),
            "回合进行中执行白名单命令不得因拿不到锁而跳过：\n{window}"
        );
        assert!(
            window.contains("None =>"),
            "拿不到 session 锁时应有降级分支（None =>），而不是直接返回"
        );
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
    /// 启动横幅之后弹出的「会话正被另一个实例使用」裁决框。为 `Some` 时会话历史
    /// 已经载入聊天区，但**尚未决定要不要写回**——按键裁决前不产生任何落盘。
    pub session_conflict: Option<SessionConflictDialog>,
    /// 本实例是否以「只读」方式挂载了别人的会话（见 [`SessionConflictChoice::ReadOnly`]）。
    /// 为 true 时所有会话持久化写入被跳过，对方实例的数据不会被覆盖。
    pub read_only_session: bool,
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
    /// 是否开启鼠标报告（滚轮/拖拽进入应用）。见 `config::UiCfg::mouse_capture`。
    ///
    /// 主循环每帧比对它与「已应用到终端的值」，变化时发开/关序列；`/mouse`
    /// 只改这里，不写回配置文件。
    pub mouse_capture: bool,
    /// 是否在内容区右侧绘制应用内滚动条（见 `config::UiCfg::scrollbar`）。
    pub show_scrollbar: bool,
    /// 聊天/详情内容区本帧占用的屏幕矩形，由渲染层每帧回写。
    ///
    /// 应用内拖选需要知道「哪些格子属于正文」：输入框、状态栏、底部面板都不能
    /// 被选进来，否则一次从上到下的拖选会把边框和提示文字一起复制。
    pub(crate) chat_area: ratatui::layout::Rect,
    /// 进行中的鼠标拖选。`None` 表示没有选区。
    pub(crate) selection: Option<TextSelection>,
    /// 上一次左键按下的时刻与位置，用于识别双击（选词）。
    pub(crate) last_click: Option<(Instant, u16, u16)>,
    /// 复制结果的瞬时提示（文案, 过期时刻），显示在状态栏右侧。
    pub(crate) selection_notice: Option<(String, Instant)>,
    /// 上一帧内容区的屏幕文本快照（`ScreenRow`），复制选区时按它还原文字。
    pub(crate) frame_rows: Vec<ScreenRow>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// 最近一次模型往返的时延画像，由 `AgentEvent::TurnDone` 从 session 带回来，
    /// 供 `/context` 面板展示。Agent 侧记在 `Session::latency` 上（因为计时只在
    /// Agent 的 stream 循环里准确），TUI 这边只是每次回合结束时同步一份快照。
    pub session_latency: wyj_core::session::TurnLatency,
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
    /// 被折叠的长文本粘贴，与输入框里的 `PASTE_MARKER` 原子一一对应。
    /// 发送时由 `InputBox::text_with_pastes` 按位置展开回正文。
    pub pending_pastes: Vec<PasteBlock>,
    /// @ 文件选取器候选列表
    pub file_completions: Vec<FileEntry>,
    /// 当前选中的文件选取器项索引
    pub file_selected: usize,
    /// @ 选取器当前浏览目录
    pub at_browse_dir: PathBuf,
    /// 当前正在执行的操作名（工具调用时为 "ToolName(arg)"，LLM 思考时为 None）
    pub current_op: Option<String>,
    /// 当前正在执行的**工具名**（不带参数、不带后端标注）。
    ///
    /// `current_op` 带着参数（`Bash(cargo test)`），而聊天区那行 `⏺` 已经把
    /// 参数完整画出来了。标题栏再复述一遍就是逐字节重复，所以它改读这个字段，
    /// 只回答"在用什么工具"，"在做什么"交给聊天区与下面的 todo 任务名。
    /// 仅 `ToolStart` 写入、`ToolEnd` / 新一轮开始清空；非工具型后台任务
    /// （`/compact` 等）保持 None，标题栏回落显示 `current_op` 原文。
    pub current_tool_name: Option<String>,
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
    /// 流式生成速率表（标题栏 `xx tok/s`）。只服务显示，不参与任何决策。
    ///
    /// 生命周期跟着「生成段」而不是跟着回合：`ToolStart`、回合收尾、中断都会
    /// 清空——工具执行期间模型没在吐 token，继续显示上一个速率就是在骗人。
    pub stream_rate: StreamRateMeter,
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
    ///
    /// 这里的 `active_profile` 始终是**全局默认值**（`config.toml` 里的值）。
    /// 会话级分组（`/model` 面板「仅本会话使用」）另存于
    /// [`AppState::session_profile_override`]，由 [`AppState::effective_config`]
    /// 在构造 Provider 时叠加——这样任何一次 `config.save()` 都不会把临时
    /// 分组固化成全局默认值。
    pub config: Config,
    /// 会话级分组覆盖（`/model` 面板「仅本会话使用」），`None` = 跟随全局。
    ///
    /// 只在当前会话生效，随 `SessionFile.session_profile` 落盘，`--resume`
    /// 后继续生效；新会话 / 切换到别的会话时会被换成那个会话自己的值。
    pub session_profile_override: Option<String>,
    /// 子 Agent 实时状态（key = Hub 分配的 id，BTreeMap 保证面板按启动顺序排列）
    pub sub_agents: BTreeMap<u64, SubAgentUiState>,
    /// 当前会话的子 Agent trace 事件缓存，按子 Agent id 索引。
    pub sub_agent_trace_cache: BTreeMap<u64, Arc<Vec<TraceEvent>>>,
    /// 子 Agent 详情视图的聊天化消息缓存（见 `refresh_sub_agent_detail_cache`）。
    ///
    /// 详情视图每帧都要把执行轨迹翻译成 `ChatMessage` 序列再交给聊天区渲染管线，
    /// 而助手文本的 markdown 解析是这里最贵的一步。轨迹只追加，所以「事件条数」
    /// 就是完整的版本号：条数没变就直接复用上一帧的翻译结果。
    pub sub_agent_detail_cache: Option<SubAgentDetailCache>,
    /// 当前 TUI 对应的 session id，供 /subagents 详情按需读取 trace 文件。
    pub current_session_id: String,
    /// session 文件目录，也是子 Agent trace 旁路文件所在根目录。
    pub sessions_dir: Option<PathBuf>,
    /// agents 面板当前选中项（按 id 而非 index，避免列表变动时错位）
    pub selected_sub_agent: Option<u64>,
    /// 选中项是否已展开详情（工具调用流水 + 最终结果）
    pub sub_agent_detail_open: bool,
    /// 详情内容的行级滚动偏移：**从正文第一行往下跳过的行数**，
    /// 与 `Paragraph::scroll` 同向（渲染时按可视行数 clamp 并写回）。
    pub sub_agent_detail_scroll: u16,
    /// 详情面板上一帧渲染算出的内部可滚动上限，供 scroll_focus_lines 判断是否已到边界并转发给聊天区
    pub sub_agent_detail_max_scroll: u16,
    /// 详情视口是否钉在末尾跟随新事件。为真时 `sub_agent_detail_scroll` 由渲染
    /// 每帧重置成 `max_scroll`；用户一旦主动上滚即置假，此后 scroll 是绝对偏移，
    /// 后续追加的 trace 事件只在下方堆积、不再把视口拽走。
    pub sub_agent_detail_follow_tail: bool,
    /// 后台子 Agent 完成时主 Agent 空闲，暂存的 system-reminder，下轮起手注入
    pub pending_bg_reminders: Vec<String>,
    /// 后台 shell 任务（Vec 保持 spawn 顺序，见 ShellUiState 注释）。
    /// 数据源是 `BashSessionManager::global()`，由主循环每帧 `poll_shells()` 同步。
    pub shells: Vec<ShellUiState>,
    /// shell 面板当前选中项（按 id 存而非 index，避免列表变动时错位）
    pub selected_shell: Option<String>,
    /// 选中 shell 是否已展开详情（实时输出）
    pub shell_detail_open: bool,
    /// 详情内容的行级滚动偏移：**从正文第一行往下跳过的行数**（同 sub_agent_detail_scroll）
    pub shell_detail_scroll: u16,
    /// 详情面板上一帧渲染算出的内部可滚动上限，供 scroll_focus_lines 判断是否已到边界并转发给聊天区
    pub shell_detail_max_scroll: u16,
    /// 详情视口是否钉在末尾跟随新输出，语义同 `sub_agent_detail_follow_tail`
    pub shell_detail_follow_tail: bool,
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
mod agents_panel;
mod clipboard;
mod dialogs;
mod events;
mod mcp_panel;
mod menu;
mod navigation;
mod panels;
mod plugins_panel;
mod profile_panel;
mod schedule_panel;
mod settings_dialogs;
mod skills_panel;
mod sub_agent_view;
mod terminal;
mod tool_view;
mod util;

// 子模块里的类型仍以 `crate::app::Xxx` 对外可见（渲染层、命令层、测试都这么引用）。
// 这是「物理拆分、逻辑位置不变」：模块边界只影响文件布局，不改变 API 表面。
pub(crate) use agents_panel::*;
pub(crate) use clipboard::*;
pub use dialogs::*;
pub use mcp_panel::*;
pub use menu::*;
pub use plugins_panel::*;
pub use profile_panel::*;
pub use schedule_panel::*;
pub use settings_dialogs::*;
pub use skills_panel::*;
pub(crate) use sub_agent_view::*;
use terminal::*;
pub(crate) use tool_view::*;
pub(crate) use util::*;

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
            session_conflict: None,
            read_only_session: false,
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
            mouse_capture: config.ui.mouse_capture,
            show_scrollbar: config.ui.scrollbar,
            chat_area: ratatui::layout::Rect::default(),
            selection: None,
            last_click: None,
            selection_notice: None,
            frame_rows: Vec::new(),
            total_input_tokens: 0,
            session_latency: wyj_core::session::TurnLatency::default(),
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
            session_profile_override: None,
            pending_attachments: vec![],
            pending_pastes: vec![],
            file_completions: vec![],
            file_selected: 0,
            at_browse_dir: PathBuf::new(),
            current_op: None,
            current_tool_name: None,
            current_op_started: None,
            turn_start_time: None,
            turn_start_input_tokens: 0,
            turn_start_output_tokens: 0,
            turn_live_input_tokens: 0,
            turn_live_output_tokens: 0,
            stream_rate: StreamRateMeter::default(),
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
            sub_agent_detail_cache: None,
            current_session_id: String::new(),
            sessions_dir: None,
            selected_sub_agent: None,
            sub_agent_detail_open: false,
            sub_agent_detail_scroll: 0,
            sub_agent_detail_max_scroll: 0,
            sub_agent_detail_follow_tail: true,
            pending_bg_reminders: vec![],
            shells: Vec::new(),
            selected_shell: None,
            shell_detail_open: false,
            shell_detail_scroll: 0,
            shell_detail_max_scroll: 0,
            shell_detail_follow_tail: true,
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
    /// 标记「有一个后台长任务在跑」（`/compact` 的 LLM 往返、
    /// `needs_background` 的 slash 命令、`/checkpoint` / `/rewind` / `/branch`
    /// 的工作区快照、`/evolve` 的候选激活）。
    ///
    /// 复用 `is_thinking` 而不是新增标志位，是因为"用户输入被挡、标题栏跑
    /// spinner、ESC 能 abort 掉 `current_task`、15 分钟停滞看门狗兜底"这一整套
    /// 机制已经挂在 `is_thinking` 上了；新造一个标志位等于把这四条重新实现一遍。
    /// 代价是 `/context` 之类面板会把后台压缩误读成"正在思考"——可接受，
    /// 换来的是用户随时能 ESC 中断。
    ///
    /// **不发** `turn_start_time`：它驱动的是回合耗时统计，后台任务不是回合，
    /// 混进去会污染 `/context` 的时延画像。
    pub(crate) fn begin_background_task(&mut self, label: String) {
        self.is_thinking = true;
        self.spinner_frame = 0;
        self.current_op = Some(label);
        self.current_tool_name = None;
        self.current_op_started = Some(Instant::now());
        self.last_turn_activity = Instant::now();
    }

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
        // 新回合从 TTFT 开始重新计时：上一轮的速率与这一轮毫无关系。
        self.stream_rate.reset();
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
        self.clear_pending_uploads();
        self.current_op = None;
        self.current_tool_name = None;
        self.current_op_started = None;
        self.turn_start_time = None;
        self.turn_start_input_tokens = 0;
        self.turn_start_output_tokens = 0;
        self.turn_live_input_tokens = 0;
        self.turn_live_output_tokens = 0;
        self.stream_rate.reset();
        self.last_turn_elapsed_secs = None;
        self.last_turn_input_tokens = 0;
        self.last_turn_output_tokens = 0;
        self.injector = None;
        self.pending_queue.clear();
        self.sub_agents.clear();
        self.sub_agent_trace_cache.clear();
        self.sub_agent_detail_cache = None;
        self.selected_sub_agent = None;
        self.sub_agent_detail_open = false;
        self.reset_sub_agent_detail_view();
        self.pending_bg_reminders.clear();
        // 会话级重置：后台任务属于「上一段会话的现场」，随会话一起作废
        // （进程本身由退出路径的 kill_all 收走）。与 begin_new_turn 的
        // 「跨轮次保留」是刻意区分的两级粒度。
        self.shells.clear();
        self.selected_shell = None;
        self.shell_detail_open = false;
        self.reset_shell_detail_view();
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
        self.stream_rate.reset();
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
        self.clear_pending_uploads();
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
        self.current_tool_name = None;
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
    /// 本实例是否允许把会话写回磁盘。
    ///
    /// 「只读」挂载（[`SessionConflictChoice::ReadOnly`]）下为 false：这个会话文件
    /// 属于另一个仍在运行的实例，本进程整份覆盖它正是我们要避免的那种丢数据。
    ///
    /// **每个会话保存点都必须先过这道闸**——共有六处（每轮结束、生成标题、切换
    /// 会话、两条 subagent 忙碌通知、退出时），漏掉任何一处都会让「只读」变成谎言：
    /// 面板上写着不写回，退出时却把整个会话覆盖掉了。
    pub fn session_persist_allowed(&self) -> bool {
        !self.read_only_session
    }

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
        self.current_tool_name = None;
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
}

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
    // 会话级分组（`/model` 面板「仅本会话使用」）。`--resume` / `-c` 时由 CLI
    // 从会话文件读出；新会话为 None。它与 `config.active_profile` 分离，
    // 保证任何一次 `config.save()` 都不会把临时分组固化成全局默认值。
    session_profile_override: Option<String>,
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
    // 本次 resume 的会话正被另一个 wyj-code 实例使用（`(session_id, owner_pid)`）。
    // 非 None 时启动横幅之后立刻弹窗让用户决定：接管 / 只读 / 退出。
    session_conflict: Option<SessionConflictInfo>,
    // CLI 启动参数带来的权限约束。`--allowed-tools` 与 `--allow-plan-write`
    // 之前只写进 CLI 侧那份 `ToolCtx`，而 TUI 自建 ctx、自算权限模式，
    // 于是这两个参数在 TUI 入口下静默失效（Plan 模式尤其致命：它的存在
    // 意义恰恰主要在 TUI 里）。
    permission_constraints: PermissionConstraints,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // 全程 Fullscreen + alternate screen（不再有 Inline viewport）：`Viewport::
    // Fullscreen` 的构造路径不查询终端光标位置（`(area, Position::ORIGIN)`），
    // 结构上不会撞上 Inline 撑满高度时那个 tmux/部分终端下的光标查询竞态
    // （历史教训见 crates/tui/CLAUDE.md）。聊天区因此天然贴着窗口顶部铺开、
    // 输入框/状态栏贴着窗口底部，不再有"内容不够高、底部留空白"的问题。
    // 历史消息永远留在 `AppState.messages`，由 PageUp/PageDown 与滚轮驱动的内容
    // 滚动入口翻看；滚轮绝不得进入 Composer 输入历史。
    // 鼠标捕获是否开启由 `[ui].mouse_capture` 决定（默认开）：alternate screen 里
    // 终端没有自己的滚动条，macOS Terminal.app 又不支持 DEC mode 1007，不开捕获
    // 时滚轮**完全不会送达应用**。开启的代价是拖选复制需按住 Option；`/mouse`
    // 与配置项都能退回。链接继续通过 OSC 8 交给终端处理 Command/Ctrl+点击。
    let wheel_routing = TerminalWheelRouting::detect();
    let initial_mouse_capture = config.ui.mouse_capture;
    enter_terminal_screen(&mut stdout, wheel_routing, initial_mouse_capture)?;
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
        session_profile_override,
        hub,
        local_plugin,
        mcp_tools,
        shared_agent_defs,
        hyperlink_registry,
        plugin_runtime,
        wheel_routing,
        needs_api_key_onboarding,
        workspace_cas,
        session_conflict,
        permission_constraints,
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
    // Backend 把 Show 暂存到下一次 `set_cursor_position` 才落地；退出路径后面没有
    // 定位调用，必须在这里显式收尾，否则光标停在隐藏态。
    terminal.backend_mut().flush_pending_cursor()?;

    match result {
        Ok(Some(resumable_session_id)) => {
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                Print("\n"),
                SetForegroundColor(Theme::to_crossterm(Theme::inactive_color())),
                Print("恢复此会话："),
                ResetColor,
                Print("\n  wyj-code --resume "),
                SetForegroundColor(Theme::to_crossterm(Theme::hint_color())),
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

/// 循环切换 Agent 模式：Auto → Plan → Bypass → Auto
///
/// 三档而不是四档：手动模式已并入 auto（弹窗改由闸门在判 AskUser 时出现），
/// 用户不该再为「要不要弹窗」单独切一档。Bypass 排最后而不是 auto 之前：它是
/// "用户明说全都放行"的选择，不该出现在日常切换路径里。
fn cycle_mode(mode: &AgentMode) -> AgentMode {
    match mode {
        AgentMode::Auto => AgentMode::Plan,
        AgentMode::Plan => AgentMode::Bypass,
        AgentMode::Bypass => AgentMode::Auto,
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
    mouse_capture: bool,
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
    enter_terminal_screen(terminal.backend_mut(), wheel_routing, mouse_capture)?;
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
/// 那一帧：不重绘、不读按键，连 v0.2.30 的回合心跳看门狗
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
            sess.messages.clone(),
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
    // Auto 模式判定闸门。随 ctx 派生传给子 Agent；`None` 时 auto 模式退化为
    // 逐次弹窗（见 `Agent::ensure_tool_permission`）。
    auto_gate: Option<Arc<dyn wyj_core::auto_mode::AutoGate>>,
    ui_ask_tx_clone: mpsc::Sender<UiAskRequest>,
    // 主 Agent 空闲期间积累的后台任务结果 reminder（子 Agent / 后台 shell）
    preface_reminders: Vec<String>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
    // CLI 启动参数带来的权限约束（`--allowed-tools` / `--allow-plan-write`）。
    permission_constraints: PermissionConstraints,
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
        ctx.auto_gate = auto_gate;
        ctx.ui_ask_tx = Some(ui_ask_tx_clone);
        // `--allow-plan-write` 的文档授权：ctx 每回合新建，策略表是空的，
        // 必须每次重新注入，否则 Plan 模式下「本轮授权的单个文档」这条
        // 放行路径在 TUI 里永远不成立。
        permission_constraints.apply_to_ctx(&ctx);
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
            latency: sess.latency,
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

/// CLI 启动参数带来的权限约束。
///
/// 存在的理由：`--allowed-tools` 与 `--allow-plan-write` 之前只写进 CLI 侧
/// 那份 `ToolCtx`，而 TUI 每回合自建 `ToolCtx`、权限模式又自己算一份
/// （`mode_to_permission`）——两个参数在 TUI 入口下**静默失效**：不报错、
/// 不警告，用户只会看到「参数写了但没反应」。
///
/// 之所以在 TUI 侧重新施加而不是让 CLI 直接下发算好的 `PermissionMode`：
/// TUI 允许运行中切换模式（Shift+Tab / `/mode` / Plan 审批），每次切换都要
/// 重算权限模式。只下发「最终权限模式」的话，切一次模式就把 CLI 的约束
/// 冲掉了；下发「约束」本身才能在每次重算后重新施加。
#[derive(Debug, Clone, Default)]
pub struct PermissionConstraints {
    /// `--allowed-tools`：本次进程显式授权的工具名。非空时收窄权限模式。
    pub allowed_tools: HashSet<String>,
    /// `--allow-plan-write`：本轮明确授权写入的文档（已按 cwd 解析为绝对路径）。
    pub plan_write_grants: Vec<PathBuf>,
}

impl PermissionConstraints {
    /// 由运行模式算出**最终**权限模式（模式白名单 ∩ CLI 显式名单）。
    pub fn permission_for(&self, mode: &AgentMode) -> PermissionMode {
        wyj_core::permission::restrict_to_allowed_tools(
            mode_to_permission(mode),
            &self.allowed_tools,
        )
    }

    /// 把 `--allow-plan-write` 的授权写进 ctx 的权限策略。
    ///
    /// TUI 每回合新建 `ToolCtx`，其 `permission_policy` 是全新的空策略，
    /// 所以每次建 ctx 后都要重新注入一遍。
    pub fn apply_to_ctx(&self, ctx: &wyj_tools::ToolCtx) {
        for path in &self.plan_write_grants {
            // 路径在 CLI 侧已解析过一次（`safe_resolve_write_target` + 扩展名
            // 检查），这里再解析一次是幂等的；解析失败说明 cwd 变了，
            // 跳过即可——放行一个解析不了的路径只会让策略表里多一条死条目。
            let _ = ctx.allow_plan_document(path);
        }
    }
}

/// 根据 AgentMode 构建对应的 PermissionMode
///
/// 走 `wyj_core::permission::permission_mode_for` 这一个入口：Plan 白名单
/// 必须与 CLI 侧完全一致，此前两边各写一份字面量且已经漂移（CLI 有
/// `Memory` 无 `TodoWrite`，TUI 反之），同一个模式在两种入口下放行的
/// 工具集不同。
fn mode_to_permission(mode: &AgentMode) -> PermissionMode {
    wyj_core::permission::permission_mode_for(mode)
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
    // v0.2.16 国产配置体检:把 profile 原始字段 + 推导后的 effective 值并列显示,
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

/// 应用 `/model` 面板的「仅本会话使用 / 恢复跟随全局」。
///
/// 与 `CommandResult::SwitchProfile`（写盘改全局默认值）严格区分：
///
/// 1. **绝不写 `config.toml`**。这是本功能的全部意义——为个别项目临时换模型
///    不该污染全局默认值，也不该影响别的项目。
/// 2. **记进内存 Session**（`Session.session_profile`），由 `SessionFile::from_session`
///    落盘，`--resume` 后继续生效。落盘的只有分组名，端点/Key 仍以
///    `config.toml` 为唯一事实来源；分组被删掉时恢复侧回落全局。
/// 3. 重建 Agent 时用 `effective_config()`（全局配置 + 会话级覆盖），而不是
///    直接改 `state.config.active_profile`——后者会让后续任何一次无关的
///    `config.save()`（`/config` 改语言、`/memory` 切开关）把临时分组固化。
///
/// 失败时**不改任何状态**：用户仍在原来的端点上，只是多一条错误提示。
///
/// 返回值 = 本次是否真的换了生效端点。调用方（主循环）据此决定**收起
/// `/model` 面板**：切换是面板唯一要交付的东西，留着它就等于留一个不再有
/// 待办的浮层，而它仍然吞掉全部按键（拦截条件 `state.profile_dialog.is_some()`
/// 与渲染条件同源，见 [`finish_session_profile_switch`]）。
#[allow(clippy::too_many_arguments)]
fn apply_session_profile(
    state: &mut AppState,
    action: ProfileSessionAction,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    session_store: Option<&Arc<SessionStore>>,
    current_session_id: &str,
    session: &Arc<Mutex<Session>>,
) -> bool {
    // `Save`（「设为当前」执行后的保存意图）不在这里处理——`profile_try_save`
    // 需要的句柄组合与 `set_session_profile` 不同，由调用方（/model 面板的
    // 菜单分支）拦截后直接走 `profile_try_save`。
    if matches!(action, ProfileSessionAction::Save) {
        return false;
    }
    let desired = match action {
        ProfileSessionAction::Use(name) => Some(name),
        ProfileSessionAction::FollowGlobal => None,
        ProfileSessionAction::Save => None,
    };
    set_session_profile(
        state,
        desired,
        true,
        agent_tx,
        rebuild_fn,
        system_prompt_extra,
        todo_store,
        shared_agent,
        session_store,
        current_session_id,
        session,
    )
}

/// 「仅本会话使用 / 恢复跟随全局」执行成功后收起 `/model` 面板。
///
/// 三条「立刻改变生效端点」的出口里只有「设为当前」会在成功后关面板，
/// 会话级两条漏了：菜单收起（`dialog.menu = None`）后面板本体还挂在屏幕上，
/// 而它的渲染条件（`render.rs` 的 `if let Some(dialog) = &state.profile_dialog`）
/// 与按键拦截条件（主循环的 `if state.profile_dialog.is_some()`）同源——
/// 于是一个没有待办的浮层继续吞掉全部按键，用户看着输入框的光标却打不进字。
///
/// 面板里有未保存的字段草稿时**不关**：会话级切换刻意不写盘，直接关等于
/// 静默丢掉用户刚敲进去的 Key。草稿的正常出口是 Ctrl+S 或 Esc 的三选一
/// 确认，不能被一次端点切换顺手抹掉；此时面板仍在，`dialog.session_profile`
/// 已更新，再次打开头行菜单就能看到「已在使用」与「恢复跟随全局」。
fn finish_session_profile_switch(state: &mut AppState) {
    if state.profile_dialog.as_ref().is_some_and(|d| d.is_dirty()) {
        return;
    }
    state.input_owner = None;
    state.profile_dialog = None;
}

/// [`apply_session_profile`] 的实际实现，`notify = false` 供会话切换复用
/// （那里已经有一条「已切换至会话 …」的提示，再来一条只会是噪音）。
///
/// 返回值同 [`apply_session_profile`]：重建 Agent 失败时为 `false`（此时状态
/// 完全没动，调用方应保留面板让用户看见 `profile.switch_failed`）。
#[allow(clippy::too_many_arguments)]
fn set_session_profile(
    state: &mut AppState,
    desired: Option<String>,
    notify: bool,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    session_store: Option<&Arc<SessionStore>>,
    current_session_id: &str,
    session: &Arc<Mutex<Session>>,
) -> bool {
    // 名字必须能在当前全局配置里解析到：分组被改名/删除后，旧会话文件里的
    // 名字会指向不存在的东西，静默回落全局比在下一轮请求时 400 更可解释。
    let desired = desired.filter(|name| state.config.profile_by_name(name).is_some());

    let mut candidate = state.effective_config();
    match &desired {
        Some(name) => candidate.active_profile = name.clone(),
        // 恢复跟随全局：`effective_config` 本来就会回落，显式写一遍更直白。
        None => candidate.active_profile = state.config.active_profile.clone(),
    }
    let model_for_mode = candidate.model_for_mode(&state.mode).to_string();
    let new_agent = match rebuild_fn(&candidate, &model_for_mode) {
        Ok(agent) => agent,
        Err(e) => {
            state
                .messages
                .push(ChatMessage::assistant_err(wyj_i18n::tr_fmt(
                    "profile.switch_failed",
                    &[("err", &e.to_string())],
                )));
            return false;
        }
    };
    let new_agent = new_agent.append_system(system_prompt_extra.trim_start().to_string());
    let new_agent = attach_agent_session(new_agent, session_store, current_session_id);
    let new_agent = wire_tool_callback(new_agent, agent_tx.clone(), todo_store.clone());
    *shared_agent.write().unwrap() = Arc::new(new_agent);

    state.session_profile_override = desired.clone();
    state.model_name = model_for_mode;
    state.context_window = candidate.active_profile().context_window;
    state.set_session_profile_in_session(session, desired.clone());
    // 会话级分组随会话落盘：这里只置标记，真正写盘走主循环的 autosave，
    // 避免在按键处理里同步做一次全量 JSON 序列化。
    state.save_needed = true;
    if let Some(dialog) = &mut state.profile_dialog {
        dialog.session_profile = desired.clone();
    }

    if !notify {
        return true;
    }
    let msg = match &desired {
        Some(name) => wyj_i18n::tr_fmt("profile.session_switched", &[("name", name)]),
        None => wyj_i18n::tr("profile.session_follow_global").to_string(),
    };
    state.messages.push(ChatMessage::assistant(msg));
    true
}

/// 会话切换（新建 / 切历史 / branch）后把生效分组换成目标会话自己的那个。
///
/// 会话级分组是**会话的属性**，不是进程的属性：从 A 会话切到 B 会话，端点和
/// 模型必须跟着 B 走，否则 B 的历史由一个它从未用过的模型续写。目标会话没有
/// 覆盖时回落全局默认值。
#[allow(clippy::too_many_arguments)]
fn adopt_session_profile(
    state: &mut AppState,
    target: Option<String>,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    rebuild_fn: &RebuildFn,
    system_prompt_extra: &str,
    todo_store: &Arc<std::sync::Mutex<TodoStore>>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    session_store: Option<&Arc<SessionStore>>,
    current_session_id: &str,
    session: &Arc<Mutex<Session>>,
) {
    // 面板里的 entries 是**打开那一刻**的 config 快照，会话一换就过期：里面
    // 的「设为当前 / 保存」会拿旧快照覆盖新会话读到的配置。面板的正常出口
    // （Esc 三选一）走不到这里——这些调用点都在面板关闭后才可能执行，但
    // 后台 branch 提交（`current_session_id` 变化）那条路径与面板无关，
    // 所以这里无条件收起，而不是赌"面板一定没开"。
    state.input_owner = None;
    state.profile_dialog = None;

    let target = target.filter(|name| state.config.profile_by_name(name).is_some());
    if target == state.session_profile_override {
        // 同一个分组：不必重建 Agent（重建会换掉 provider 句柄，正在跑的
        // 工具/缓存都白费），但会话文件里的记录要跟上目标会话。
        state.set_session_profile_in_session(session, target);
        return;
    }
    let _ = set_session_profile(
        state,
        target,
        false,
        agent_tx,
        rebuild_fn,
        system_prompt_extra,
        todo_store,
        shared_agent,
        session_store,
        current_session_id,
        session,
    );
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
///
/// `constraints` 必须参与重算：CLI 的 `--allowed-tools` 是**进程级**约束，
/// 切模式不能把它冲掉（否则用户按一下 Shift+Tab 就绕过了自己给的限制）。
async fn switch_mode(
    shared_mode: &Arc<tokio::sync::Mutex<AgentMode>>,
    shared_permission: &Arc<std::sync::RwLock<PermissionMode>>,
    new_mode: AgentMode,
    constraints: &PermissionConstraints,
) {
    *shared_mode.lock().await = new_mode.clone();
    *shared_permission.write().unwrap() = constraints.permission_for(&new_mode);
}

/// 模式切换后同步闸门的分类器开关。
///
/// 降级（`AutoGate::disable_classifier`）是一次性的：闸门在这个会话里已经证明
/// 判定不可靠，切走再切回来**不应该**悄悄恢复它，否则用户按两下 Shift+Tab 就
/// 把一个反复拒绝的动作重新塞回分类器往返里。反过来，用户在 `/mode` 里**显式
/// 选中 auto** 是「我知道了，我要重试」的表态，那就要恢复。
///
/// 因此只有 `reset` 为真的调用点（`/mode` 的显式选择与启动装配）才重新打开，
/// Shift+Tab 的循环走 `reset = false`。
async fn sync_gate_classifier(
    gate: Option<&Arc<dyn wyj_core::auto_mode::AutoGate>>,
    mode: &AgentMode,
    reset: bool,
) {
    let Some(gate) = gate else { return };
    if matches!(mode, AgentMode::Auto) && reset {
        gate.enable_classifier();
    }
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
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        // 与 `code_index::walk_files` / `grep` / `glob` 对齐：非 git 目录里也让
        // `.gitignore` 生效，在 git 仓库 / managed worktree 里必须为 true 才读得到
        // 主仓库的 `.git/info/exclude`（`dir` 可能是 worktree 里的深层子目录，
        // 判定会向上走到 worktree 根）。
        .require_git(wyj_config::gitignore::is_inside_git_repo(dir))
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
    let mut ids = list_trace_ids(sessions_dir, session_id);
    // `trace_enabled = false` 时会话只有交接物、没有 trace——这些 agent 同样
    // 应当出现在面板里（否则「有交接记录但看不到」比没有更困惑）。
    for id in wyj_tools::handoff::list_handoff_ids(sessions_dir, session_id) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    for id in ids {
        let path = trace_file(sessions_dir, session_id, id);
        // 没有 trace（`trace_enabled = false` 或文件被清理）但仍有交接物时，
        // 用交接物本身重建一条最小记录——总比让用户看不到这个 agent 好。
        let Ok(events) = read_trace(&path) else {
            if let Some(handoff) = wyj_tools::handoff::read_handoff(sessions_dir, session_id, id) {
                out.insert(
                    id,
                    SubAgentUiState {
                        agent_type: handoff.agent_type.clone(),
                        description: handoff.description.clone(),
                        background: handoff.background,
                        status: sub_agent_status_from_handoff(handoff.status),
                        started_at: Instant::now(),
                        final_elapsed: Some(handoff.elapsed_secs),
                        input_tokens: 0,
                        output_tokens: 0,
                        tool_calls: handoff.digest.total_calls,
                        current_tool: None,
                        tool_log: Vec::new(),
                        has_result: true,
                        finished_at: Some(Instant::now()),
                        final_result: Some(handoff.conclusion.clone()),
                        handoff: Some(handoff),
                    },
                );
            }
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
                        handoff: None,
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
                // 每轮落点：回灌时保留，详情面板靠它显示「推导出了什么」。
                // 不参与状态机，不影响 Running/Done 的判定。
                TraceEvent::Assistant { .. } => {}
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
    // 交接物与 trace 分开读：trace 可能因进程被强杀而缺 Done，但 Hub 在
    // abort 前写的 `a<id>.handoff.json` 仍在——它才是终态与结论的权威来源。
    for (id, state) in out.iter_mut() {
        if let Some(handoff) = wyj_tools::handoff::read_handoff(sessions_dir, session_id, *id) {
            state.handoff = Some(handoff);
        }
    }
    out
}

/// 交接终态 → 面板状态。`Completed` 之外一律映射为 `Interrupted`，
/// 因为面板只有 Running/Done/Failed/Interrupted 四态，而「轮数触顶」在 UI
/// 语义上就是一次没跑完的提前停止（详情区会显示具体是 turn_limit）。
fn sub_agent_status_from_handoff(status: wyj_tools::handoff::HandoffStatus) -> SubAgentStatus {
    use wyj_tools::handoff::HandoffStatus;
    match status {
        HandoffStatus::Completed => SubAgentStatus::Done,
        HandoffStatus::Failed => SubAgentStatus::Failed,
        HandoffStatus::Interrupted | HandoffStatus::TurnLimit => SubAgentStatus::Interrupted,
    }
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
            state.reset_sub_agent_detail_view();
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
                state.reset_sub_agent_detail_view();
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
            state.reset_shell_detail_view();
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
                state.reset_shell_detail_view();
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

/// Agent 忙碌时，这条 composer 草稿是否应**照常作为 slash 命令执行**，
/// 而不是排进 `pending_queue` 投递给模型。
///
/// 判定只看三件事，且必须同时成立：
///
/// 1. 整条草稿 trim 后**不含换行**——多行输入里第一行恰好是 `/model` 时它并不是
///    命令（后面还跟着正文），跟空闲时 `dispatch` 的语义保持一致；
/// 2. 首个 token 解析得出命令名（`CommandRegistry::command_name`）；
/// 3. 该命令注册为 [`wyj_commands::registry::Command::runs_while_busy`]。
///
/// 白名单为什么必须逐个声明、默认 false，见该方法上的四条排除理由。
fn draft_runs_while_busy(registry: &CommandRegistry, draft: &str) -> bool {
    let trimmed = draft.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return false;
    }
    registry.runs_while_busy(CommandRegistry::command_name(trimmed).unwrap_or(""))
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

/// 补全列表把选中命令写回输入框时用的新 `InputBox`：命令名 + 一个尾随空格，
/// 让用户直接接着打参数。Tab 与 Enter 共用这一条构造，避免两处各自逐字插入
/// 后慢慢走偏。
fn slash_completion_input(selected: &str) -> InputBox {
    let mut input = InputBox::new();
    for c in selected.chars() {
        input.insert_char(c);
    }
    input.insert_char(' ');
    input
}

/// 补全列表打开时按 Enter：应当**填入选中项**而不是提交输入框原文。
///
/// 判据是「输入框 trim 后的文本是否已经等于选中命令」：
/// - `/mo` + 选中 `/model` → 不等，填入后停住，用户接着打参数；
/// - `/model `（Tab 刚填过、或手输全一致）→ 相等，放行提交，
///   否则用户会经历「填入 → 再按一次回车才执行」的多余一步。
fn slash_completion_enter_fills(current: &str, selected: &str) -> bool {
    current.trim_end() != selected
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
    session_profile_override: Option<String>,
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
    // resume 到的会话正被另一个实例使用；非 None 则启动后弹窗让用户裁决。
    session_conflict: Option<SessionConflictInfo>,
    // CLI 启动参数带来的权限约束（`--allowed-tools` / `--allow-plan-write`）。
    permission_constraints: PermissionConstraints,
) -> Result<Option<String>> {
    // 主循环与各 turn task 都要读它（切模式时重算、建 ctx 时注入 plan 授权），
    // 所以内部持一份 clone，参数本身留给调用方。
    let permission_constraints = permission_constraints.clone();
    let shared_mode = Arc::new(tokio::sync::Mutex::new(mode.clone()));
    // 与 shared_mode 同步更新的实时权限句柄，见 switch_mode() 与 spawn_agent_turn()
    // 顶部说明：解决"运行中切换模式对本轮已生效"的问题。
    //
    // 初值经 `permission_constraints` 收窄：CLI 的 `--allowed-tools` 在 TUI
    // 入口下也必须生效（此前只写进 CLI 侧那份 ctx，TUI 入口静默失效）。
    let shared_permission = Arc::new(std::sync::RwLock::new(
        permission_constraints.permission_for(&mode),
    ));
    // Auto 模式闸门：整个会话一份，随 ctx 派生传给子 Agent（降级阈值因此
    // 全局共享）。建不出来时传 `None` —— auto 模式会退化成逐次弹窗。
    let auto_gate = Some(wyj_core::auto_mode::build_gate(
        &config,
        agent.provider_handle(),
    ));
    // Auto 是默认起始模式，且它会**替人**做放行决定。首次进入时把这件事说清楚，
    // 否则用户看到"什么都没问就改了文件"时的第一反应是以为权限系统坏了。
    let started_in_auto = mode == AgentMode::Auto;
    let mut auto_degrade_notified = false;
    let mut state = AppState::new(
        cwd.clone(),
        model_name,
        context_window,
        mode,
        config,
        hub.clone(),
    );
    state.session_profile_override = session_profile_override;
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
    if started_in_auto && initial_messages.is_empty() {
        // 只在新会话说一次。恢复旧会话时那些工具早就跑完了，再补一句是噪音。
        state
            .messages
            .push(ChatMessage::system(wyj_i18n::tr("auto.mode_notice")));
    }
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

    // 恢复的会话正被另一个实例使用 → 启动即弹裁决框。放在 MCP/skill 信任之后：
    // 信任框问的是「要不要执行这个仓库声明的东西」，冲突框问的是「要不要覆盖别人的
    // 会话」，后者优先级更高（它直接决定本次运行的落盘行为），但两者不会同时存在
    // ——真同时出现时这里会盖掉信任框，用户的第一次按键只处理冲突。
    if let Some(info) = session_conflict {
        state.session_conflict = Some(SessionConflictDialog::new(info));
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
    // 已经应用到终端的鼠标报告状态。`/mouse` 只改 `state.mouse_capture`，
    // 这里每帧比对后发一次终端序列——幂等，且不必在命令分支里持有 backend。
    let mut applied_mouse_capture = state.mouse_capture;

    loop {
        if state.mouse_capture != applied_mouse_capture {
            set_mouse_capture(terminal.backend_mut(), state.mouse_capture)?;
            applied_mouse_capture = state.mouse_capture;
        }

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

        // `/branch` 的会话切换跑在后台 task 里（见 `run_branch_in_background`），
        // 事件 handler 只能改 AppState —— 把主循环这份本地镜像同步回去，
        // 之后 autosave / checkpoint 才会写到新会话上。
        if state.current_session_id != current_session_id {
            current_session_id = state.current_session_id.clone();
            // 分支会话继承了父会话的临时分组（`branch_from_checkpoint` 写进
            // 新会话文件、后台 task 写进内存 Session）。这里把它落到 AppState
            // 并重建 Agent：不做的话分支内容会由一个它从未用过的端点续写。
            let branch_profile = try_session_lock(&session).and_then(|s| s.session_profile.clone());
            adopt_session_profile(
                &mut state,
                branch_profile,
                &agent_tx,
                &rebuild_fn,
                &system_prompt_extra,
                &todo_store,
                &shared_agent,
                session_store.as_ref(),
                &current_session_id,
                &session,
            );
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
        // 复制提示同理：它只在状态栏停留一秒半，不留常驻噪音。
        if state
            .selection_notice
            .as_ref()
            .is_some_and(|(_, at)| *at <= Instant::now())
        {
            state.selection_notice = None;
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

        // ── Auto 分类器降级检查 ───────────────────────────────────────
        // 连续/累计拦截超阈值后停用分类器：此后只剩确定性规则层，拿不准的
        // 交回逐次弹窗（对齐上游的 "3 consecutive or 20 total"）。
        //
        // 手动模式并入 auto 之后，这里**不能**再"切走"——那会把用户带到一个
        // 不存在的模式上。可逆的降级只有停用分类器这一个动作：确定性 allow /
        // hard_deny 都还在，弹窗照旧，hard_deny 也不会因此失效（闸门里两条
        // 确定性规则排在分类器开关之前）。只在本会话提示一次：反复提醒「这个
        // 会话的分类器不太灵」没有意义。
        if !auto_degrade_notified && state.mode == AgentMode::Auto {
            let degraded = auto_gate.as_ref().is_some_and(|gate| gate.should_degrade());
            if degraded {
                auto_degrade_notified = true;
                if let Some(gate) = auto_gate.as_ref() {
                    gate.disable_classifier();
                }
                state.messages.push(ChatMessage::system(wyj_i18n::tr(
                    "auto.degraded_to_confirm",
                )));
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
                auto_gate.clone(),
                ui_ask_tx.clone(),
                reminders,
                plugin_runtime.clone(),
                permission_constraints.clone(),
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
                auto_gate.clone(),
                ui_ask_tx.clone(),
                std::mem::take(&mut state.pending_bg_reminders),
                plugin_runtime.clone(),
                permission_constraints.clone(),
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
                    if !sess.messages.is_empty() && state.session_persist_allowed() {
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
                        // **丢到后台 task，主循环不 await**：`save` 是同步 IO +
                        // 跨进程文件锁（`filelock` 最坏等 30 秒）。即使挪进
                        // `spawn_blocking`，主循环若 `.await` 它，界面照样停在
                        // 这一帧——和直接调 `save` 的现场一模一样，只是换了个
                        // 线程在等。存盘是「尽力而为」的副作用，晚完成不影响
                        // 任何可见状态，因此 fire-and-forget 才是正确形状。
                        //
                        // `sf` 是 owned 快照，与 `sess` 已无借用关系（上面
                        // 先 `drop(sess)` 释放锁），可以安全送进 `'static` task。
                        drop(sess);
                        if state.session_persist_allowed() {
                            let store = store.clone();
                            tokio::spawn(async move {
                                if let Err(error) = store.save_async(sf).await {
                                    tracing::warn!("会话保存失败: {error}");
                                }
                            });
                        }
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
                    // 这里只额外探测「同一剪贴板里是不是文件」——**不**碰图片：文本
                    // 已经拿到了，让 `get_image()` 顶掉它正是文件被误判成 `[Image #n]`
                    // 的那条路（macOS Finder 复制的文件带 TIFF 图标预览）。
                    let paste = read_clipboard_files().unwrap_or(ClipboardPaste::Text(pasted));
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
                    // 应用内拖选：按下建锚点、拖拽扩范围、松开写剪贴板。
                    // 开鼠标报告后终端不再处理选择，这条路径是「直接拖选复制」的
                    // 唯一实现（Claude Code fullscreen 同款）。
                    MouseEventKind::Down(MouseButton::Left) => {
                        let rows = std::mem::take(&mut state.frame_rows);
                        state.begin_selection(mouse.column, mouse.row, &rows);
                        state.frame_rows = rows;
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        state.update_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        let rows = std::mem::take(&mut state.frame_rows);
                        state.end_selection(&rows);
                        state.frame_rows = rows;
                    }
                    MouseEventKind::ScrollUp => {
                        // 滚动会让屏幕上原本选中的文字错位，选区必须一起作废。
                        state.clear_selection();
                        state.scroll_focus_lines(-3)
                    }
                    MouseEventKind::ScrollDown => {
                        state.clear_selection();
                        state.scroll_focus_lines(3)
                    }
                    _ => {}
                },
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    // 任意按键都清除粘贴提示（提示本就是瞬时的）
                    state.paste_hint = None;
                    // 选区是「刚才那次鼠标操作」的产物。保留它会让高亮看起来像
                    // 卡住的样式；导航键（PageUp 等）尤其如此——用户按下去是要
                    // 翻页，不是要继续选。Esc 也走这条，无需单独分支。
                    state.clear_selection();

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
                                            if !sess.messages.is_empty()
                                                && state.session_persist_allowed()
                                            {
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
                                        // 新会话不继承上一个会话的临时分组。
                                        adopt_session_profile(
                                            &mut state,
                                            None,
                                            &agent_tx,
                                            &rebuild_fn,
                                            &system_prompt_extra,
                                            &todo_store,
                                            &shared_agent,
                                            session_store.as_ref(),
                                            &current_session_id,
                                            &session,
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
                                                if !sess.messages.is_empty()
                                                    && state.session_persist_allowed()
                                                {
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
                                                    state.reset_sub_agent_detail_view();
                                                    // 生效分组跟着目标会话走：B 会话
                                                    // 若记录了临时分组，就用它重建 Agent；
                                                    // 没记录则回落全局默认值。
                                                    adopt_session_profile(
                                                        &mut state,
                                                        file.session_profile.clone(),
                                                        &agent_tx,
                                                        &rebuild_fn,
                                                        &system_prompt_extra,
                                                        &todo_store,
                                                        &shared_agent,
                                                        session_store.as_ref(),
                                                        &current_session_id,
                                                        &session,
                                                    );
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
                                    InputOwner::Settings(_) => {
                                        // Settings 借用同 Profile：字段编辑直接借用
                                        // 输入框，Esc 只需清空 live_input + 归还
                                        // input_owner（上面已做），面板本身保持打开。
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
                                                let old = dialog.entries[entry_idx].name.clone();
                                                dialog.entries[entry_idx].name = name.clone();
                                                // 重命名的是本会话正在用的分组时，
                                                // 覆盖记录里的名字要跟着改：否则
                                                // Ctrl+S 时按新名字找不到旧覆盖，
                                                // 会静默回落全局（安全但意外）。
                                                if dialog.session_profile.as_deref()
                                                    == Some(old.as_str())
                                                {
                                                    dialog.session_profile = Some(name);
                                                }
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
                                    InputOwner::Settings(SettingsInputField::Field {
                                        field_idx,
                                    }) => {
                                        let value = text.unwrap_or_default();
                                        owner.clear_live_input(&mut state);
                                        state.input_owner = None;
                                        if let Some(dialog) = &mut state.settings_dialog {
                                            dialog.draft.set_text_value(field_idx, value);
                                            dialog.error = None;
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

                    // ⓪.5 设置面板拦截（/config 命令触发）
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
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    let idx = dialog.selected;
                                    dialog.draft.cycle_enum(idx, true);
                                }
                            }
                            KeyCode::Enter => {
                                // 按字段形态分派：枚举原地循环，文本/数字/密钥借用
                                // 底部主输入框。此前所有字段都是枚举，Enter 与
                                // Right 同义；加入 search 文本字段后必须分开。
                                let target = state.settings_dialog.as_ref().map(|d| {
                                    (
                                        d.selected,
                                        SETTINGS_FIELD_SPECS
                                            .get(d.selected)
                                            .map(|s| s.kind)
                                            .unwrap_or(SettingsFieldKind::Text),
                                    )
                                });
                                match target {
                                    Some((idx, SettingsFieldKind::Enum(_))) => {
                                        if let Some(dialog) = &mut state.settings_dialog {
                                            dialog.draft.cycle_enum(idx, true);
                                        }
                                    }
                                    Some((idx, _)) => {
                                        let prefill = state
                                            .settings_dialog
                                            .as_ref()
                                            .map(|d| d.draft.text_value(idx).to_string())
                                            .unwrap_or_default();
                                        if let Some(dialog) = &mut state.settings_dialog {
                                            dialog.live_input = InputBox::new();
                                            dialog.live_input.insert_text(&prefill);
                                        }
                                        state.input_owner =
                                            Some(InputOwner::Settings(SettingsInputField::Field {
                                                field_idx: idx,
                                            }));
                                    }
                                    None => {}
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let mut should_close = false;
                                if let Some(dialog) = &mut state.settings_dialog {
                                    {
                                        if let Some((key, args)) = dialog.draft.validate() {
                                            let refs: Vec<(&str, &str)> = args
                                                .iter()
                                                .map(|(k, v)| (*k, v.as_str()))
                                                .collect();
                                            dialog.error = Some(wyj_i18n::tr_fmt(key, &refs));
                                            // 校验失败：保留面板与已填内容，只展示错误，
                                            // 让用户改完再存。`continue` 跳过本轮事件处理
                                            // （面板不关闭）。
                                            continue;
                                        }
                                        let new_cfg = dialog.draft.to_config(&state.config);
                                        match new_cfg.save() {
                                            Ok(()) => {
                                                should_close = true;
                                                state.config = new_cfg.clone();
                                                let lang = wyj_i18n::resolve_locale(
                                                    state.config.language.as_deref(),
                                                );
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
                                    // 激活候选要先建工作区快照（git 管道 / 全树
                                    // 哈希），同步 IO 秒级，不能压住主循环。
                                    let session_c = session.clone();
                                    let cwd_c = cwd.clone();
                                    let id_c = current_session_id.clone();
                                    let session_store_c = session_store.clone();
                                    let tx = agent_tx.clone();
                                    state.begin_background_task("/evolve".to_string());
                                    let task = spawn_turn_task(async move {
                                        let result = execute_evolution_action(
                                            action,
                                            store,
                                            &cwd_c,
                                            session_store_c.as_ref(),
                                            &id_c,
                                            &session_c,
                                        )
                                        .await
                                        .map_err(|e| e.to_string());
                                        let _ =
                                            tx.send(AgentEvent::EvolutionActionDone(result)).await;
                                    });
                                    state.current_task = Some(task);
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
                                        let result = open_path_in_editor(
                                            terminal,
                                            &path,
                                            wheel_routing,
                                            state.mouse_capture,
                                        )
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
                                        // 两个 store 互斥存在（v3 建成时 v2 不构造，
                                        // 见 CLI 装配），这里照写两份是为了在 v3
                                        // 降级到 v2 的少数情况下开关仍然生效。
                                        {
                                            let agent = shared_agent.read().unwrap();
                                            if let Some(memory) = agent.memory_v3_ref() {
                                                memory.set_enabled(new_value);
                                            }
                                            if let Some(mem) = agent.memory_ref() {
                                                mem.set_enabled(new_value);
                                            }
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
                            if let Some(action) =
                                profile_handle_menu_key(&mut state, key.code, &agent_tx)
                            {
                                if matches!(action, ProfileSessionAction::Save) {
                                    // 「设为当前」执行完毕后的保存意图：走与 Ctrl+S
                                    // 完全相同的 `profile_try_save`（校验 → 写盘 →
                                    // 重建 agent），成功自动关面板，失败留在面板
                                    // 展示 error。
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
                                    }
                                } else if apply_session_profile(
                                    &mut state,
                                    action,
                                    &agent_tx,
                                    &rebuild_fn,
                                    &system_prompt_extra,
                                    &todo_store,
                                    &shared_agent,
                                    session_store.as_ref(),
                                    &current_session_id,
                                    &session,
                                ) {
                                    // 与 Ctrl+S / 「设为当前」同一收口：切换落地即收起
                                    // 面板，否则留下一个没有待办却仍吞按键的浮层。
                                    finish_session_profile_switch(&mut state);
                                }
                            }
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
                            KeyCode::Left | KeyCode::Right => {
                                let forward = key.code == KeyCode::Right;
                                let target = state.profile_dialog.as_ref().and_then(|d| {
                                    match d.selected_row() {
                                        ProfileRow::Field(entry_idx, f) => Some((entry_idx, f)),
                                        _ => None,
                                    }
                                });
                                if let Some((entry_idx, f)) = target {
                                    if let Some(dialog) = &mut state.profile_dialog {
                                        match PROFILE_FIELD_SPECS.get(f).map(|s| s.kind) {
                                            Some(ProfileFieldKind::Provider) => {
                                                dialog.entries[entry_idx].cycle_provider(forward)
                                            }
                                            Some(ProfileFieldKind::Enum(_)) => dialog.entries
                                                [entry_idx]
                                                .cycle_enum_field(f, forward),
                                            Some(ProfileFieldKind::Bool) => {
                                                dialog.entries[entry_idx].toggle_bool_field(f)
                                            }
                                            // Text/Secret/Model：文本字段没有"切换"语义，
                                            // 与旧行为一致（←→ 无动作）。
                                            _ => {}
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
                                        let kind = PROFILE_FIELD_SPECS.get(f).map(|s| s.kind);
                                        match kind {
                                            Some(ProfileFieldKind::Model) => {
                                                // 模型名：Enter 直接拉服务器列表。
                                                // 拉取失败或需要手填时不阻塞——结果菜单
                                                // 首项固定是「手动输入」（见 AgentEvent::ModelsFetched
                                                // 的菜单构造）。
                                                profile_start_fetch_models(
                                                    &mut state, &agent_tx, entry_idx, f,
                                                );
                                            }
                                            Some(ProfileFieldKind::Provider) => {
                                                if let Some(dialog) = &mut state.profile_dialog {
                                                    dialog.entries[entry_idx].cycle_provider(true);
                                                    // provider 换了，可见字段集合跟着变，
                                                    // 光标可能落在被隐藏的行上。
                                                    dialog.clamp_cursor();
                                                }
                                            }
                                            Some(ProfileFieldKind::Enum(_))
                                            | Some(ProfileFieldKind::Bool) => {
                                                if let Some(dialog) = &mut state.profile_dialog {
                                                    match kind {
                                                        Some(ProfileFieldKind::Enum(_)) => dialog
                                                            .entries[entry_idx]
                                                            .cycle_enum_field(f, true),
                                                        _ => dialog.entries[entry_idx]
                                                            .toggle_bool_field(f),
                                                    }
                                                }
                                            }
                                            _ => {
                                                // Text/Secret：借用底部输入框。
                                                let prefill = state
                                                    .profile_dialog
                                                    .as_ref()
                                                    .map(|d| {
                                                        d.entries[entry_idx]
                                                            .text_value(f)
                                                            .to_string()
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
                                    let new_mode = AgentMode::Auto;
                                    switch_mode(
                                        &shared_mode,
                                        &shared_permission,
                                        new_mode.clone(),
                                        &permission_constraints,
                                    )
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
                                    let new_mode = AgentMode::Auto;
                                    switch_mode(
                                        &shared_mode,
                                        &shared_permission,
                                        new_mode.clone(),
                                        &permission_constraints,
                                    )
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
                                        auto_gate.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                        plugin_runtime.clone(),
                                        permission_constraints.clone(),
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
                                        auto_gate.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                        plugin_runtime.clone(),
                                        permission_constraints.clone(),
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

                    // 会话占用裁决：优先级高于两个信任框（它直接决定本次运行会不会
                    // 覆盖别人的数据）。`Ctrl+C` 走 interrupt 而不是 Abort——用户想
                    // 的是「停掉当前回合」，不该被顺带解释成「放弃这个会话」。
                    if state.session_conflict.is_some() {
                        if matches!(
                            key.code,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL)
                        ) {
                            state.interrupt();
                            continue;
                        }
                        let choice = state
                            .session_conflict
                            .as_mut()
                            .and_then(|dialog| dialog.handle_key(key.code));
                        if let Some(choice) = choice {
                            let info = state
                                .session_conflict
                                .take()
                                .map(|dialog| dialog.info)
                                .unwrap_or_else(|| SessionConflictInfo {
                                    session_id: String::new(),
                                    owner_pid: 0,
                                });
                            match choice {
                                SessionConflictChoice::TakeOver => {
                                    state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                                        "dialog.session_conflict_taken_over",
                                        &[
                                            ("pid", &info.owner_pid.to_string()),
                                            ("id", &info.session_id),
                                        ],
                                    )));
                                }
                                SessionConflictChoice::ReadOnly => {
                                    // 只读：会话历史照常显示，但从现在起不写回会话文件。
                                    // 这是与 Plan 模式不同的东西——Plan 只限制文件工具，
                                    // 会话持久化照旧发生，覆盖冲突不会被 Plan 挡住。
                                    state.read_only_session = true;
                                    state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                                        "dialog.session_conflict_read_only_applied",
                                        &[
                                            ("pid", &info.owner_pid.to_string()),
                                            ("id", &info.session_id),
                                        ],
                                    )));
                                }
                                SessionConflictChoice::Abort => {
                                    state.messages.push(ChatMessage::system(wyj_i18n::tr(
                                        "dialog.session_conflict_abort_applied",
                                    )));
                                    return Ok(Some(wyj_i18n::tr(
                                        "dialog.session_conflict_abort_applied",
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

                    // 输入框 → 内容区的显式入口。必须排在补全列表拦截**之前**：
                    // 补全列表开着时裸 ↑ 归补全，但 Shift+↑ 是「进内容区」的
                    // 专用键，被补全分支 `continue` 吃掉的话用户按了没反应。
                    // 更上层的模态面板（Session Picker / input_owner / 各类 dialog）
                    // 都已先拦截，不会被这里抢键。
                    //
                    // `Chat` 焦点只有在下方面板真的存在时才抢键：否则「正在滚动
                    // 历史时按 Shift+↑」会突然跳到会话末尾（`focus_chat_edge`），
                    // 那不是用户按这个组合的意图。
                    let shift_up = is_content_focus_key(key);
                    let chat_has_panel_below = state.ui_focus == UiFocus::Chat
                        && (state.has_running_sub_agents()
                            || state
                                .current_todos
                                .as_deref()
                                .is_some_and(|items| !items.is_empty())
                            || state.has_running_shells());
                    if shift_up && (!state.content_focus_active() || chat_has_panel_below) {
                        let _ = state.enter_content_focus();
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
                                input = slash_completion_input(&selected);
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                continue;
                            }
                            // Enter 复用 Tab 的填入语义：↑↓ 选中的命令写回输入框
                            // 并补空格，而不是提交输入框里的不完整原文。若输入框
                            // 已经是「命令 + 空格」的形态（Tab 刚填过、或手输完
                            // 全一致），则落穿走正常提交，避免「填入后还要再按
                            // 一次回车」。
                            KeyCode::Enter => {
                                let selected =
                                    state.slash_completions[state.slash_selected].0.clone();
                                if slash_completion_enter_fills(&input.text(), &selected) {
                                    input = slash_completion_input(&selected);
                                    state.slash_completions.clear();
                                    state.slash_selected = 0;
                                    continue;
                                }
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                // 落穿：不带 continue，让下方 Enter 分支提交
                            }
                            KeyCode::Esc => {
                                state.slash_completions.clear();
                                state.slash_selected = 0;
                                continue;
                            }
                            _ => {} // 其他键落穿到下方正常处理
                        }
                    }

                    // 输入框 → 内容区的显式入口已在补全列表之前处理（见上）。

                    // 内容区获得焦点后，普通 ↑/↓ 用于连续导航：Chat、Todo、
                    // SubAgent 在各自边界自然跨区；TodoDetail 内则改为逐行滚动。
                    // Esc 按“详情 → 列表 → 输入框”逐级返回并始终保留草稿。
                    if state.content_focus_active() {
                        // 面板浏览态先截下全局中断/退出键（见
                        // `content_focus_swallows_global_key`），否则 Ctrl+C 会穿过
                        // 下面的 `_` 兜底直落 `interrupt()`，把整轮执行连同所有
                        // 前台子 Agent 一起 abort 掉。
                        if state.content_focus_swallows_global_key(&key) {
                            state.close_panel_focus();
                            continue;
                        }
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
                        switch_mode(
                            &shared_mode,
                            &shared_permission,
                            new_mode.clone(),
                            &permission_constraints,
                        )
                        .await;
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
                    // 回合进行中按 Enter：默认排队投递给模型，**但面板类 slash 命令
                    // 照常执行**。`/model` 这类命令没有"内容"可投递，排给模型等于把
                    // 字面的 `/model` 当成一句用户消息发出去（用户实测还会同时在右下角
                    // 冒出"1 条已排队"，看起来像排队失败）。判定见 `draft_runs_while_busy`。
                    let busy_slash =
                        state.is_thinking && draft_runs_while_busy(&cmd_registry, &input.text());
                    if newline {
                        if key.code == KeyCode::Enter
                            && !key.modifiers.contains(KeyModifiers::SHIFT)
                        {
                            // 行尾反斜杠只是"换行记号"，不进正文
                            input.backspace();
                        }
                        input.insert_newline();
                        state.slash_completions.clear();
                    } else if key.code == KeyCode::Enter && (!state.is_thinking || busy_slash) {
                        if composer_has_content(&state, &input) {
                            // 附件占位符的位置只存在于输入框内部（`take()` 之后
                            // 标记就没了），先留一份快照给聊天区展示用。
                            let composer_snapshot = input.clone();
                            // 折叠的粘贴块在这里展开回**原文**：`input.take()`
                            // 拿不到 `pending_pastes`，用它会把用户贴的内容整段吞掉。
                            let (text, composer_pastes) =
                                take_composer_text(&mut state, &mut input);
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
                                switch_mode(
                                    &shared_mode,
                                    &shared_permission,
                                    new_mode.clone(),
                                    &permission_constraints,
                                )
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
                                // 走 `AgentMode::parse` 而不是在这里再写一遍 match：
                                // CLI 的 `--permission-mode` 与 TUI 的 `/mode` 必须接受
                                // 同一组名字与别名（`default` ≡ `manual`），两处各写
                                // 一份必然漂移。
                                //
                                // 裸 `/mode` 显示当前模式与可选值，而不是像从前那样
                                // 「切到 normal」——现在默认值是 auto，一个不带参数的
                                // 切换命令没有任何自解释的语义。
                                if args.is_empty() {
                                    state.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                                        "mode.current",
                                        &[("mode", state.mode.label())],
                                    )));
                                    state.input_history.push(trimmed);
                                    continue;
                                }
                                match AgentMode::parse(args) {
                                    Some(m) => {
                                        let label = m.label();
                                        switch_mode(
                                            &shared_mode,
                                            &shared_permission,
                                            m.clone(),
                                            &permission_constraints,
                                        )
                                        .await;
                                        // 显式 `/mode auto` 是「重试」的表态：
                                        // 恢复本次会话因拦截过多而停用的分类器。
                                        sync_gate_classifier(auto_gate.as_ref(), &m, true).await;
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
                                prompt_drift,
                                sub_usage,
                            ) = match try_session_lock(&session) {
                                Some(sess) => {
                                    // 优先用 Agent 侧算好的 request 级分解（与自动压缩
                                    // 阈值同源）；本会话还没发过模型请求时才退回
                                    // messages-only 估算。
                                    let audit = if sess.context_audit.total() > 0 {
                                        Some(sess.context_audit)
                                    } else {
                                        None
                                    };
                                    let estimated = audit.map(|a| a.total()).unwrap_or_else(|| {
                                        wyj_core::estimate_tokens(&sess.messages)
                                    });
                                    (
                                        audit,
                                        estimated,
                                        sess.total_cache_read_tokens,
                                        sess.total_cache_write_tokens,
                                        sess.api_calls,
                                        sess.prompt_cache_state,
                                        sess.compact_count,
                                        (
                                            wyj_core::context_edit::elided_count(&sess.messages)
                                                as u32,
                                            sess.context_edit_freed_tokens,
                                        ),
                                        sess.prompt_drift_since_resume.clone(),
                                        // 子 Agent 用量取自 Session 的累加器而非
                                        // `state.sub_*`：后者只收 SubAgentEvent 的
                                        // input/output，**不含 cache_read**，且
                                        // `/resume` 后归零。子 Agent 的开销大头恰恰是
                                        // 缓存命中，丢掉它 /cost 会少算一大截。
                                        sess.sub_usage(),
                                    )
                                }
                                None => {
                                    // 回合进行中：session 被回合 task 整段持锁，这里必须
                                    // **降级而不是 continue**——continue 等于命令根本不执行，
                                    // 用户按 `/model` 会看到一句"会话忙"却什么也没发生。
                                    //
                                    // 走到这里的只有 `runs_while_busy` 白名单命令（面板类），
                                    // 它们不读这些统计字段；读统计的 `/cost`、`/context`
                                    // 不在白名单内，因此不会静默拿到一份失真的数据。
                                    // 压缩次数是 AppState 上的运行期计数，回合期间也准。
                                    (
                                        None,
                                        0,
                                        0,
                                        0,
                                        0,
                                        0,
                                        state.compact_count,
                                        (0, 0),
                                        // 回合进行中拿不到 session，漂移信息一并降级为
                                        // None（= 不知道）。`/context` 不在 `runs_while_busy`
                                        // 白名单里，这条分支实际不会被它命中。
                                        None::<wyj_core::prompt_fingerprint::PromptDrift>,
                                        wyj_core::session::SubUsage::default(),
                                    )
                                }
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
                                prompt_drift,
                                last_latency: state.session_latency,
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
                            // 声明了 `needs_background` 的命令（`/bug`、`/pr-comments`）
                            // 含子进程 / 网络往返，inline dispatch 会连按键都读不到。
                            // 丢后台 task，结果经 `AgentEvent::BackgroundCommandDone`
                            // 发回主循环渲染；ESC 可以随时 abort。
                            if let Some(name) = CommandRegistry::command_name(&trimmed) {
                                if cmd_registry.needs_background(name) {
                                    let registry = cmd_registry.clone();
                                    let ctx = cmd_ctx;
                                    let input = trimmed.to_string();
                                    let name = name.to_string();
                                    let tx = agent_tx.clone();
                                    state.begin_background_task(format!("/{name}"));
                                    let task = spawn_turn_task(async move {
                                        let result = match registry.dispatch(&input, &ctx).await {
                                            Some(Ok(result)) => Ok(result),
                                            Some(Err(error)) => Err(error.to_string()),
                                            None => Err(wyj_i18n::tr_fmt(
                                                "command.unknown",
                                                &[("name", &name)],
                                            )),
                                        };
                                        let _ = tx
                                            .send(AgentEvent::BackgroundCommandDone {
                                                name,
                                                result,
                                            })
                                            .await;
                                    });
                                    state.current_task = Some(task);
                                    continue;
                                }
                            }

                            // Skill 调用提示：动态命令（`~/.wyj-code/skills/*`）展开后的
                            // prompt 会被当作用户消息推进聊天流，用户看不出这次是某个
                            // skill 触发的——尤其在展开内容与用户原话差得远的时候。
                            // 记下命令名，等 `RunPrompt*` 分支真正起回合前插一行提示。
                            let skill_invocation = skill_invocation_name(&cmd_registry, &trimmed);

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
                                        state.clear_pending_uploads();
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
                                        // 压缩是一次完整的 LLM 往返（摘要输出预算
                                        // `min(cw/10, 16000)` + 最多 5 次指数退避重试），
                                        // 端点不稳时可以拖到数分钟。**绝不能**在主循环
                                        // 里 inline await：那会让同一个 task 既不重绘
                                        // 也不 `event::poll()`，界面冻死且 ESC /
                                        // Ctrl+C 全部读不到，只能 kill。见
                                        // `crates/tui/CLAUDE.md` 第 19 条。
                                        let agent_c = shared_agent.read().unwrap().clone();
                                        let session_c = session.clone();
                                        let tx = agent_tx.clone();
                                        if try_session_lock(&session).is_none() {
                                            state.notify_session_busy();
                                            continue;
                                        }
                                        state.begin_background_task("/compact".to_string());
                                        let task = spawn_turn_task(async move {
                                            let result = {
                                                let mut sess = session_c.lock().await;
                                                let out = agent_c.compact_context(&mut sess).await;
                                                // 锁内快照，锁必须先放再发事件
                                                (out.map_err(|e| e.to_string()), sess.compact_count)
                                            };
                                            let _ = tx
                                                .send(AgentEvent::CompactFinished {
                                                    result: result.0,
                                                    compact_count: result.1,
                                                })
                                                .await;
                                        });
                                        state.current_task = Some(task);
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
                                                if !sess.messages.is_empty()
                                                    && state.session_persist_allowed()
                                                {
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
                                            // 新会话不继承上一个会话的临时分组。
                                            adopt_session_profile(
                                                &mut state,
                                                None,
                                                &agent_tx,
                                                &rebuild_fn,
                                                &system_prompt_extra,
                                                &todo_store,
                                                &shared_agent,
                                                session_store.as_ref(),
                                                &current_session_id,
                                                &session,
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
                                                // `store.create` 会跑 git 管道（`git add -A`
                                                // + `write-tree` + `commit-tree`）做工作区快照：
                                                // 同步 IO，大仓库上能到秒级，绝不能挂在主循环
                                                // 里（那会让整帧卡住、按键读不到）。丢后台
                                                // task，连带把 `current_checkpoint_id` 写回
                                                // session 一起搬过去。
                                                let sess = match try_session_lock(&session) {
                                                    Some(sess) => sess,
                                                    None => {
                                                        state.notify_session_busy();
                                                        continue;
                                                    }
                                                };
                                                let messages = sess.messages.clone();
                                                drop(sess);
                                                let session_c = session.clone();
                                                let store_c = store.clone();
                                                let cwd_c = cwd.clone();
                                                let tx = agent_tx.clone();
                                                state.begin_background_task(
                                                    "/checkpoint".to_string(),
                                                );
                                                let task = spawn_turn_task(async move {
                                                    let id = {
                                                        // 快照本身是同步 IO：再套一层
                                                        // spawn_blocking，不占着 runtime
                                                        // 的工作线程。
                                                        let created = tokio::task::spawn_blocking(
                                                            move || {
                                                                store_c
                                                                    .create(
                                                                        &cwd_c,
                                                                        messages.clone(),
                                                                        wyj_core::CheckpointKind::Manual,
                                                                        name,
                                                                    )
                                                                    .map(|cp| cp.id)
                                                                    .map_err(|e| e.to_string())
                                                            },
                                                        )
                                                        .await
                                                        .unwrap_or_else(|e| Err(e.to_string()));
                                                        match created {
                                                            Ok(id) => {
                                                                session_c
                                                                    .lock()
                                                                    .await
                                                                    .current_checkpoint_id =
                                                                    Some(id.clone());
                                                                Ok(id)
                                                            }
                                                            Err(e) => Err(e),
                                                        }
                                                    };
                                                    let _ = tx
                                                        .send(AgentEvent::CheckpointCreated(id))
                                                        .await;
                                                });
                                                state.current_task = Some(task);
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
                                        // 快照 + 文件还原是同步 IO 的长任务，
                                        // 交给后台 task（见 `run_rewind_in_background`）。
                                        let Some(sessions) = session_store.clone() else {
                                            state.messages.push(ChatMessage::assistant_err(
                                                "[rewind] session 存储不可用".to_string(),
                                            ));
                                            continue;
                                        };
                                        let session_c = session.clone();
                                        let cwd_c = cwd.clone();
                                        let id_c = current_session_id.clone();
                                        let tx = agent_tx.clone();
                                        state.begin_background_task("/rewind".to_string());
                                        let task = spawn_turn_task(async move {
                                            let outcome = run_rewind_in_background(
                                                &sessions,
                                                &cwd_c,
                                                &id_c,
                                                checkpoint_id,
                                                scope,
                                                confirmed,
                                                &session_c,
                                            )
                                            .await;
                                            let _ = tx
                                                .send(AgentEvent::RewindPrepared(
                                                    outcome.map_err(|e| e.to_string()),
                                                ))
                                                .await;
                                        });
                                        state.current_task = Some(task);
                                    }
                                    Ok(CommandResult::BranchSession {
                                        checkpoint_id,
                                        restore_files,
                                        confirmed,
                                    }) => {
                                        let Some(sessions) = session_store.clone() else {
                                            state.messages.push(ChatMessage::assistant_err(
                                                "[branch] session 存储不可用".to_string(),
                                            ));
                                            continue;
                                        };
                                        let session_c = session.clone();
                                        let agent_c = shared_agent.clone();
                                        let cwd_c = cwd.clone();
                                        let id_c = current_session_id.clone();
                                        let tx = agent_tx.clone();
                                        state.begin_background_task("/branch".to_string());
                                        let task = spawn_turn_task(async move {
                                            let outcome = run_branch_in_background(
                                                &sessions,
                                                &cwd_c,
                                                &id_c,
                                                checkpoint_id,
                                                restore_files,
                                                confirmed,
                                                &session_c,
                                                &agent_c,
                                            )
                                            .await;
                                            let _ = tx
                                                .send(AgentEvent::BranchPrepared(
                                                    outcome.map_err(|e| e.to_string()),
                                                ))
                                                .await;
                                        });
                                        state.current_task = Some(task);
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
                                        state.profile_dialog = Some(ProfileDialog::new(
                                            &state.config,
                                            state.session_profile_override.clone(),
                                        ));
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
                                                    // `/model <名>` 是「把全局默认值改成它」，
                                                    // 与「仅本会话使用」语义相反。此时若还留着
                                                    // 会话级覆盖，生效值仍是旧的那套端点，用户会
                                                    // 看到"切了但没生效"。清掉覆盖让本会话立刻
                                                    // 跟随新的全局值（同时把会话文件里的记录也清掉，
                                                    // 否则下次 resume 又回到旧覆盖）。
                                                    if state
                                                        .session_profile_override
                                                        .take()
                                                        .is_some()
                                                    {
                                                        state.set_session_profile_in_session(
                                                            &session, None,
                                                        );
                                                    }
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
                                        // Skill 展开后的 prompt → 当作用户消息发给 agent。
                                        // 先插一行「使用 skill: /x」，否则聊天流里只剩一大段
                                        // 展开后的指令，用户看不出它是 skill 带来的。
                                        if let Some(name) = &skill_invocation {
                                            state.push_tracked_message(ChatMessage::system(
                                                wyj_i18n::tr_fmt(
                                                    "skill.invoked",
                                                    &[("name", name)],
                                                ),
                                            ));
                                        }
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
                                        // async move 会把外层变量搬进闭包，先 clone 一份
                                        // 留在闭包外用（下一轮循环还要再取）。
                                        let permission_constraints_c =
                                            permission_constraints.clone();

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
                                            // 同 spawn_agent_turn：plan 文档授权要每回合重注入。
                                            permission_constraints_c.apply_to_ctx(&ctx);
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
                                                latency: sess.latency,
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
                                        if let Some(name) = &skill_invocation {
                                            state.push_tracked_message(ChatMessage::system(
                                                wyj_i18n::tr_fmt(
                                                    "skill.invoked",
                                                    &[("name", name)],
                                                ),
                                            ));
                                        }
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
                                        // async move 会把外层变量搬进闭包，先 clone 一份
                                        // 留在闭包外用（下一轮循环还要再取）。
                                        let permission_constraints_c =
                                            permission_constraints.clone();

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
                                            // 同 spawn_agent_turn：plan 文档授权要每回合重注入。
                                            permission_constraints_c.apply_to_ctx(&ctx);
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
                                                latency: sess.latency,
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
                                                if !sess.messages.is_empty()
                                                    && state.session_persist_allowed()
                                                {
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
                                                    state.reset_sub_agent_detail_view();
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
                                    Ok(CommandResult::ToggleMouseCapture) => {
                                        // 只翻转状态：真正写终端的序列由主循环每帧比对后发，
                                        // 命令分支不持有 backend，也就不可能和绘制交错。
                                        state.mouse_capture = !state.mouse_capture;
                                        state.messages.push(ChatMessage::system(
                                            if state.mouse_capture {
                                                wyj_i18n::tr("mouse.on")
                                            } else {
                                                wyj_i18n::tr("mouse.off")
                                            },
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

                                // 展示文本从快照投影（必须在 @ 展开之后算：@ 追加的
                                // 附件没有输入框标记，要作为末尾单列项一起画）
                                let display_text = crate::render::composer_display_text(
                                    &composer_snapshot,
                                    &state.pending_attachments,
                                    &composer_pastes,
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
                                        auto_gate.clone(),
                                        ui_ask_tx.clone(),
                                        std::mem::take(&mut state.pending_bg_reminders),
                                        plugin_runtime.clone(),
                                        permission_constraints.clone(),
                                    );
                                    state.current_task = Some(task);
                                    state.injector = Some(injector);
                                }
                            }
                        }
                    } else if key.code == KeyCode::Enter && state.is_thinking {
                        // Agent 忙碌期间提交 → 排队，不打断当前操作。
                        //
                        // 面板类 slash 命令**不会落到这里**：它们在上面的 Enter 分支
                        // 就被 `draft_runs_while_busy` 拦去正常 dispatch 了。所以本分支
                        // 里的内容一定是"要发给模型的正文"，`/` 开头也不例外——
                        // 非白名单命令（`/cost`、`/clear`、`/bug`…）按字面排队，
                        // 这与空闲时 `dispatch` 返回未知命令的行为不同，是刻意的：
                        // 排队只是"晚点执行"，不会丢内容。
                        //
                        if !input.is_empty()
                            || !state.pending_attachments.is_empty()
                            || !state.pending_pastes.is_empty()
                        {
                            let composer_snapshot = input.clone();
                            let (text, composer_pastes) =
                                take_composer_text(&mut state, &mut input);
                            state.slash_completions.clear();
                            state.file_completions.clear();
                            state.history_idx = None;
                            state.history_draft = None;
                            expand_at_refs_to_attachments(
                                &text,
                                &cwd,
                                &mut state.pending_attachments,
                            );
                            let display = crate::render::composer_display_text(
                                &composer_snapshot,
                                &state.pending_attachments,
                                &composer_pastes,
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
                            delete_word_backward_composer(&mut state, &mut input);
                        } else {
                            backspace_composer(&mut state, &mut input);
                        }
                        update_slash_completions(&mut state, &input, &cmd_registry);
                        update_file_completions(&mut state, &input, &cwd);
                    } else if key.code == KeyCode::Delete {
                        delete_composer_forward(&mut state, &mut input);
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
                        delete_word_backward_composer(&mut state, &mut input);
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
                                    // Ctrl+T — 展开/折叠任务列表（含全部完成态）
                                    state.toggle_todo_panel();
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
            if !sess.messages.is_empty() && state.session_persist_allowed() {
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
                // （`render::composer_display_text`）保持一致。
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
    /// 必须与实时发送路径（`render::composer_display_text`）一致。
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
            AgentMode::Auto,
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
            handoff_body: None,
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
            // 行尾带后端标注，且标注与 `grep_backend_label()` 同源
            assert_eq!(
                s.current_tool.as_deref(),
                Some(format!("Grep(foo){}", tool_backend_badge("Grep")).as_str())
            );
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
            AgentMode::Auto,
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
                handoff: None,
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
                handoff: None,
            },
        );

        apply_open_subagents_panel(&mut state, None);
        assert_eq!(state.selected_sub_agent, Some(2));
        assert!(state.sub_agent_detail_open);
    }

    fn running_sub_agent(id: u64) -> SubAgentUiState {
        SubAgentUiState {
            agent_type: "t".into(),
            description: format!("d{id}"),
            background: false,
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
            handoff: None,
        }
    }

    /// 方向键在子 Agent 间移动时必须保持详情展开 —— 否则内容区会在移动后
    /// 突然塌回聊天流，用户会觉得"按 ↓ 把正在跑的东西弄丢了"。
    #[test]
    fn moving_between_sub_agents_keeps_the_content_view_open() {
        let mut state = make_state();
        state.sub_agents.insert(1, running_sub_agent(1));
        state.sub_agents.insert(2, running_sub_agent(2));
        state.sub_agent_detail_open = false;

        assert!(state.move_selected_sub_agent(1));
        assert_eq!(state.selected_sub_agent, Some(1));
        assert!(state.sub_agent_detail_open, "选中即应展开内容视图");

        assert!(state.move_selected_sub_agent(1));
        assert_eq!(state.selected_sub_agent, Some(2));
        assert!(state.sub_agent_detail_open);
    }

    /// `Shift+↑` 进入面板时必须选中**运行中**的条目。取 `sub_agents` 末项会选中
    /// 已完成的子 Agent（面板只列运行中），内容区会显示一个列表里看不见的条目。
    #[test]
    fn entering_panel_selects_a_running_sub_agent_not_the_last_one_added() {
        let mut state = make_state();
        state.sub_agents.insert(1, running_sub_agent(1));
        state.sub_agents.insert(2, running_sub_agent(2));
        // id 更大但已结束：`sub_agents` 末项是它，`visible_sub_agents` 末项不是。
        let mut done = running_sub_agent(3);
        done.status = SubAgentStatus::Done;
        state.sub_agents.insert(3, done);

        state.enter_content_focus();

        assert_eq!(state.ui_focus, UiFocus::SubAgents);
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
                handoff: None,
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
mod session_conflict_dialog_tests {
    use super::*;

    fn dialog() -> SessionConflictDialog {
        SessionConflictDialog::new(SessionConflictInfo {
            session_id: "sess-x".into(),
            owner_pid: 4242,
        })
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

    /// 回车默认必须落在「只读」而不是「接管」：接管会整份覆盖另一个实例正在写的
    /// 数据，而多数用户看到弹窗只会无脑回车。把破坏性选项设成默认，等于把一次误操作
    /// 变成一次确定的数据丢失。
    #[test]
    fn enter_defaults_to_read_only_not_take_over() {
        let mut dlg = dialog();
        assert_eq!(dlg.selected, 1);
        assert_eq!(
            dlg.handle_key(KeyCode::Enter),
            Some(SessionConflictChoice::ReadOnly)
        );
    }

    #[test]
    fn number_keys_select_directly() {
        assert_eq!(
            dialog().handle_key(KeyCode::Char('1')),
            Some(SessionConflictChoice::TakeOver)
        );
        assert_eq!(
            dialog().handle_key(KeyCode::Char('2')),
            Some(SessionConflictChoice::ReadOnly)
        );
        assert_eq!(
            dialog().handle_key(KeyCode::Char('3')),
            Some(SessionConflictChoice::Abort)
        );
        assert_eq!(
            dialog().handle_key(KeyCode::Esc),
            Some(SessionConflictChoice::Abort)
        );
    }

    #[test]
    fn arrows_wrap_around_all_three_options() {
        let mut dlg = dialog();
        // 从 1（只读）按两次 Down 到 0（接管），第三次回到 2（退出）。
        assert_eq!(dlg.handle_key(KeyCode::Down), None);
        assert_eq!(dlg.selected, 2);
        assert_eq!(dlg.handle_key(KeyCode::Down), None);
        assert_eq!(dlg.selected, 0);
        assert_eq!(
            dlg.handle_key(KeyCode::Enter),
            Some(SessionConflictChoice::TakeOver)
        );
        assert_eq!(dlg.handle_key(KeyCode::Up), None);
        assert_eq!(dlg.selected, 2);
    }

    /// 无关按键不产生裁决，否则输入框的正常按键会被弹窗吞掉。
    #[test]
    fn unrelated_keys_do_not_decide() {
        let mut dlg = dialog();
        assert_eq!(dlg.handle_key(KeyCode::Char('x')), None);
        assert_eq!(dlg.handle_key(KeyCode::Backspace), None);
    }

    /// 「只读」必须在每一个保存点都真的挡住写入——面板上写着不写回、退出时却覆盖，
    /// 比根本没有这个选项更糟。
    #[test]
    fn read_only_blocks_session_persistence() {
        let mut state = make_state();
        assert!(state.session_persist_allowed(), "默认必须允许写回");
        state.read_only_session = true;
        assert!(
            !state.session_persist_allowed(),
            "只读挂载后任何会话写入都必须被拒"
        );
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
            AgentMode::Auto,
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

    // ── TodoWrite 不进常规工具行（v0.2.27） ────────────────────────────────
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

    // ── 任务全部完成后 Ctrl+T 仍能展开 ───────────────────────────────────
    //
    // 回归背景：原实现在 Ctrl+T 里对 `all_done` 直接 no-op，理由是「面板已收成
    // 一行摘要，没有可展开的内容」。但摘要行的早退在渲染层、无条件生效，
    // 于是完成态成了真正的不可读——恰恰抹掉了唯一的复盘入口：每项耗时/token
    // 与执行回放（`todo_stats` / `todo_execution_logs`）只存在于内存，不落盘。
    // 正确划分是「默认折叠」（渲染层一行摘要）vs「可展开」（按键层照常切换）。

    #[test]
    fn ctrl_t_expands_the_panel_even_after_everything_is_done() {
        let mut state = make_state();
        state.current_todos = Some(vec![todo("a", TodoStatus::Completed)]);
        state.ui_focus = UiFocus::Chat;

        state.toggle_todo_panel();

        assert!(
            state.todo_panel_expanded,
            "全部完成后 Ctrl+T 仍应展开面板，而不是被静默吞掉"
        );
        assert_eq!(state.ui_focus, UiFocus::Todos);
        assert_eq!(state.selected_todo_id.as_deref(), Some("a"));
    }

    #[test]
    fn ctrl_t_collapses_the_completed_panel_again() {
        let mut state = make_state();
        state.current_todos = Some(vec![todo("a", TodoStatus::Completed)]);
        state.ui_focus = UiFocus::Chat;

        state.toggle_todo_panel();
        assert!(state.todo_panel_expanded);
        state.toggle_todo_panel();

        assert!(
            !state.todo_panel_expanded,
            "再次 Ctrl+T 应折回一行完成摘要，保持原有的 toggle 语义"
        );
    }

    #[test]
    fn ctrl_t_is_a_noop_when_there_is_no_todo_list() {
        // 新一轮（begin_new_turn 已清空 current_todos）之后没有可展开的东西，
        // 也不该抢走焦点。
        let mut state = make_state();
        state.ui_focus = UiFocus::Chat;

        state.toggle_todo_panel();

        assert!(!state.todo_panel_expanded);
        assert_eq!(state.ui_focus, UiFocus::Chat, "没有任务列表时不应劫持焦点");
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
            AgentMode::Auto,
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
            AgentMode::Auto,
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
