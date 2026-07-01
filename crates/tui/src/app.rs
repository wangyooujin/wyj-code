//! TUI 应用主循环

use crate::event::{is_quit, AgentEvent};
use crate::input::InputBox;
use crate::render;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use wyj_api::types::{ContentBlock, Message, Role, ToolResultContent};
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::{
    extract_preview, extract_title, new_session_id, now_iso, Agent, HistoryEntry, HistoryStore,
    Session, SessionFile, SessionMeta, SessionStore, ToolEvent,
};
use wyj_tools::todo::TodoItem;
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

fn fmt_tokens(n: u32) -> String {
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

/// AskQuestion 对话框状态
pub struct AskQuestionDialog {
    pub question: String,
    pub options: Vec<String>,
    pub selected: usize,
    pub response_tx: tokio::sync::oneshot::Sender<Option<usize>>,
}

/// ExitPlanMode 工具触发的计划批准对话框状态
pub struct PlanApprovalDialog {
    pub plan_path: Option<String>,
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

/// 设置面板的可编辑字段索引（对应渲染顺序）
pub const SETTINGS_FIELD_COUNT: usize = 10;

/// 每个字段对应的 i18n label key，下标即字段索引
pub const SETTINGS_FIELD_LABEL_KEYS: [&str; SETTINGS_FIELD_COUNT] = [
    "settings.field.provider",
    "settings.field.model",
    "settings.field.plan_model",
    "settings.field.exec_model",
    "settings.field.base_url",
    "settings.field.api_key",
    "settings.field.max_tokens",
    "settings.field.context_window",
    "settings.field.log_level",
    "settings.field.language",
];

/// api_key 字段索引（渲染时需要打码）
pub const SETTINGS_API_KEY_FIELD_IDX: usize = 5;

/// 设置表单草稿（/config 命令触发时从 AppState.config 初始化，Ctrl+S 时写回）
pub struct SettingsDraft {
    /// 0 = Anthropic, 1 = OpenAI
    pub provider_idx: usize,
    pub model: String,
    /// 空串表示未覆盖（对应 Config.plan_model == None）
    pub plan_model: String,
    /// 空串表示未覆盖（对应 Config.exec_model == None）
    pub exec_model: String,
    /// 空串表示使用供应商默认端点
    pub base_url: String,
    pub api_key: String,
    /// 编辑态存文本，保存时校验解析为 u32
    pub max_tokens: String,
    pub context_window: String,
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
            provider_idx: match cfg.provider {
                wyj_config::Provider::Anthropic => 0,
                wyj_config::Provider::OpenAI => 1,
            },
            model: cfg.model.clone(),
            plan_model: cfg.plan_model.clone().unwrap_or_default(),
            exec_model: cfg.exec_model.clone().unwrap_or_default(),
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone().unwrap_or_default(),
            max_tokens: cfg.max_tokens.to_string(),
            context_window: cfg.context_window.to_string(),
            log_level_idx,
            language_idx,
        }
    }

    /// 用草稿覆盖出一份新 Config（基于 base 保留未编辑的字段，如 mcp_servers）
    fn to_config(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        cfg.provider = if self.provider_idx == 0 {
            wyj_config::Provider::Anthropic
        } else {
            wyj_config::Provider::OpenAI
        };
        cfg.model = self.model.clone();
        cfg.plan_model = if self.plan_model.trim().is_empty() {
            None
        } else {
            Some(self.plan_model.clone())
        };
        cfg.exec_model = if self.exec_model.trim().is_empty() {
            None
        } else {
            Some(self.exec_model.clone())
        };
        cfg.base_url = self.base_url.clone();
        cfg.api_key = if self.api_key.trim().is_empty() {
            None
        } else {
            Some(self.api_key.clone())
        };
        cfg.max_tokens = self.max_tokens.trim().parse().unwrap_or(base.max_tokens);
        cfg.context_window = self
            .context_window
            .trim()
            .parse()
            .unwrap_or(base.context_window);
        cfg.log_level = LOG_LEVELS[self.log_level_idx].to_string();
        cfg.language = Some(wyj_i18n::AVAILABLE_LOCALES[self.language_idx].to_string());
        cfg
    }

    /// 数字类文本字段校验（保存前调用）：任一非法则返回对应的 i18n key
    fn validate(&self) -> Option<&'static str> {
        if self.max_tokens.trim().parse::<u32>().is_err() {
            return Some("settings.error.invalid_max_tokens");
        }
        if self.context_window.trim().parse::<u32>().is_err() {
            return Some("settings.error.invalid_context_window");
        }
        None
    }

    /// 字段是否为文本类（Enter 进入行内编辑），与 SettingsFieldKind 保持一致
    fn text_value(&self, idx: usize) -> &str {
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

    /// 枚举字段循环切换（provider=0 / log_level=8 / language=9）
    fn cycle_enum(&mut self, idx: usize, forward: bool) {
        match idx {
            0 => self.provider_idx = cycle_index(self.provider_idx, 2, forward),
            8 => self.log_level_idx = cycle_index(self.log_level_idx, LOG_LEVELS.len(), forward),
            9 => {
                self.language_idx = cycle_index(
                    self.language_idx,
                    wyj_i18n::AVAILABLE_LOCALES.len(),
                    forward,
                )
            }
            _ => {}
        }
    }

    /// 供渲染用：返回某字段当前值的展示字符串（枚举字段已转成可读文本）
    pub fn display_value(&self, idx: usize) -> String {
        match idx {
            0 => {
                if self.provider_idx == 0 {
                    "Anthropic".to_string()
                } else {
                    "OpenAI".to_string()
                }
            }
            8 => LOG_LEVELS[self.log_level_idx].to_string(),
            9 => wyj_i18n::locale_display_name(wyj_i18n::AVAILABLE_LOCALES[self.language_idx])
                .to_string(),
            _ => self.text_value(idx).to_string(),
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

/// 设置面板状态（/config 命令触发）
pub struct SettingsDialog {
    pub draft: SettingsDraft,
    /// 0..SETTINGS_FIELD_COUNT，当前高亮字段
    pub selected: usize,
    /// Some 表示当前字段正在行内文本编辑
    pub editing: Option<InputBox>,
    /// 校验失败时的提示（如 max_tokens 非数字），保存成功后清空
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

/// 设置面板里可编辑字段的类型
pub enum SettingsFieldKind {
    /// 枚举字段：Enter/Left/Right 原地循环切换
    Enum,
    /// 文本字段：Enter 进入行内编辑
    Text,
}

pub fn settings_field_kind(idx: usize) -> SettingsFieldKind {
    match idx {
        0 | 8 | 9 => SettingsFieldKind::Enum, // provider / log_level / language
        _ => SettingsFieldKind::Text,
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
    /// 会话选择器（/sessions 命令触发时 Some）
    pub session_picker: Option<SessionPickerState>,
    /// 设置面板（/config 命令触发时 Some）
    pub settings_dialog: Option<SettingsDialog>,
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
    pub injector: Option<mpsc::UnboundedSender<Vec<ContentBlock>>>,
    /// 排队中尚未被 Agent 消费的补充消息（文本 + 附件），用于状态栏提示计数、
    /// 消费后回放到对话流、以及轮次已结束但仍有残留时的兜底重发
    pub pending_queue: Vec<(String, Vec<Attachment>)>,
    /// 当前生效的完整配置（/config 设置面板的数据来源与保存目标）
    pub config: Config,
}

impl AppState {
    fn new(
        cwd: PathBuf,
        model_name: String,
        context_window: u32,
        mode: AgentMode,
        config: Config,
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
            session_picker: None,
            settings_dialog: None,
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
        }
    }

    /// 中断当前正在运行的 Agent，保留已输出内容并标记 [已中断]
    fn interrupt(&mut self) {
        if let Some(h) = self.current_task.take() {
            h.abort();
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
                self.tool_info.insert(id, (name, seq));
                self.flush_streaming();
                self.messages.push(ChatMessage::tool_call(display, seq));
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
                self.messages.push(ChatMessage::tool_result(
                    output,
                    is_error,
                    elapsed_secs,
                    seq,
                    name,
                    summary,
                ));
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

            AgentEvent::Usage { input, output } => {
                self.total_input_tokens = input;
                self.total_output_tokens = output;
            }

            AgentEvent::TodoUpdate(items) => {
                self.current_todos = Some(items);
                self.scroll_offset = 0; // 自动滚动到最新任务列表
            }

            AgentEvent::AskQuestion {
                question,
                options,
                response_tx,
            } => {
                self.ask_question_dialog = Some(AskQuestionDialog {
                    question,
                    options,
                    selected: 0,
                    response_tx,
                });
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

            AgentEvent::PlanApprovalRequest {
                plan_path,
                response_tx,
            } => {
                self.is_thinking = false; // 暂停 spinner，等待用户操作
                self.plan_dialog = Some(PlanApprovalDialog {
                    plan_path,
                    response_tx,
                });
            }
        }
    }
}

/// 启动 TUI 主界面
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
    // 需要在新语言的默认提示词后原样拼回，避免冲掉 WYJ.md/Plan 模式说明。
    system_prompt_extra: String,
    // 当前生效的完整配置（供 /config 设置面板展示初始值、保存后重建 Agent 用）
    config: Config,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
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
    )
    .await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
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
                        return s.contains("已批准计划");
                    }
                }
            }
        }
    }
    false
}

/// plan 模式下返回注入了 ExitPlanMode 工具和 system prompt 的 agent 副本，否则直接 Arc::clone
fn plan_turn_agent(base: &Arc<Agent>, mode: &AgentMode, cwd: &std::path::Path) -> Arc<Agent> {
    if !matches!(mode, AgentMode::Plan) {
        return Arc::clone(base);
    }
    let plan_path = cwd.join(".wyj").join("plan.md");
    let mut a = (**base).clone();
    a.register_tool(Arc::new(ExitPlanModeTool));
    let a = a.append_system(wyj_i18n::tr_fmt(
        "system_prompt.plan_turn",
        &[("path", &plan_path.display().to_string())],
    ));
    Arc::new(a)
}

/// 给重建出的 Agent 挂上工具事件回调（ToolStart/ToolEnd → AgentEvent，
/// TodoWrite 额外广播快照），/model 热切换与设置面板保存后重建 Agent 共用。
fn wire_tool_callback(
    agent: Agent,
    tool_tx: mpsc::Sender<AgentEvent>,
    todo_store: Arc<std::sync::Mutex<TodoStore>>,
) -> Agent {
    agent.with_tool_callback(move |event: ToolEvent| match event {
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
    ui_ask_tx_clone: mpsc::Sender<UiAskRequest>,
) -> (AbortHandle, mpsc::UnboundedSender<Vec<ContentBlock>>) {
    let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Vec<ContentBlock>>();
    let handle = tokio::spawn(async move {
        let mut sess = session_c.lock().await;
        if attachments.is_empty() {
            sess.push_user(text);
        } else {
            let blocks = build_user_blocks(text, attachments).await;
            sess.push_user_with_blocks(blocks);
        }
        let current_mode = mode_arc.lock().await.clone();
        let mut ctx = ToolCtx::new(&ctx_cwd);
        ctx.permission_mode = mode_to_permission(&current_mode);
        ctx.ui_ask_tx = Some(ui_ask_tx_clone);
        let turn_agent = plan_turn_agent(&agent_c, &current_mode, &ctx_cwd);
        let tx2 = tx.clone();
        let mut on_text = move |d: &str| {
            let _ = tx2.try_send(AgentEvent::TextDelta(d.to_string()));
        };
        let tx3 = tx.clone();
        let on_inject = move || {
            let _ = tx3.try_send(AgentEvent::Injected);
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
                "Write",        // 写计划文件
                "Bash",         // 只读命令，由 system prompt 约束
                "ExitPlanMode", // 提交计划并请求批准
                "TodoWrite",    // 任务追踪，plan 模式同样有用
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
) -> Result<Option<String>> {
    let shared_mode = Arc::new(tokio::sync::Mutex::new(mode.clone()));
    let mut state = AppState::new(cwd.clone(), model_name, context_window, mode, config);
    let mut input = InputBox::new();
    let mut current_session_id = session_id;

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (ui_ask_tx, mut ui_ask_rx) = mpsc::channel::<UiAskRequest>(8);
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let cmd_registry = standard_registry_with_skills(&home_dir, &cwd);

    // 工具回调：ToolStart/ToolEnd → AgentEvent，同时拦截 TodoWrite 读取快照
    let todo_store_cb = todo_store.clone();
    let tool_tx = agent_tx.clone();
    let agent = agent.with_tool_callback(move |event| match event {
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
                if let Ok(store) = todo_store_cb.lock() {
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
    });

    // 用 RwLock 包装 agent，支持 /model 热切换
    let shared_agent = Arc::new(std::sync::RwLock::new(Arc::new(agent)));

    // 初始化 Session：若有历史消息则恢复，并重建 TUI 显示
    let has_initial = !initial_messages.is_empty();
    let mut init_sess = Session::new();
    init_sess.messages = initial_messages;
    if has_initial {
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

        // 推进 spinner 动画帧（每 ~80ms 一帧，与 Claude Code 节奏一致）
        if state.is_thinking && last_spinner_advance.elapsed().as_millis() >= 80 {
            state.spinner_frame = (state.spinner_frame + 1) % render::SPINNER_FRAMES.len();
            last_spinner_advance = Instant::now();
        }

        terminal.draw(|f| render::draw(f, &mut state, &input))?;

        // 清空 agent 事件队列
        loop {
            match agent_rx.try_recv() {
                Ok(ev) => state.apply_agent_event(ev),
                Err(_) => break,
            }
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
                ui_ask_tx.clone(),
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
                Ok(UiAskRequest::Question {
                    question,
                    options,
                    response_tx,
                }) => {
                    state.apply_agent_event(AgentEvent::AskQuestion {
                        question,
                        options,
                        response_tx,
                    });
                }
                Ok(UiAskRequest::ExitPlanMode {
                    plan_path,
                    response_tx,
                }) => {
                    state.apply_agent_event(AgentEvent::PlanApprovalRequest {
                        plan_path,
                        response_tx,
                    });
                }
                Err(_) => break,
            }
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            let ev = event::read()?;
            match ev {
                Event::Paste(pasted) if !state.is_thinking => {
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
                                                    let plan_approved =
                                                        has_plan_approved(&sess.messages);
                                                    drop(sess);
                                                    current_session_id = file.session_id.clone();
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
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

                    // ⓪.5 设置面板拦截（/config 命令触发）
                    if state.settings_dialog.is_some() {
                        let is_editing = state
                            .settings_dialog
                            .as_ref()
                            .map(|d| d.editing.is_some())
                            .unwrap_or(false);

                        if is_editing {
                            match key.code {
                                KeyCode::Enter => {
                                    if let Some(dialog) = &mut state.settings_dialog {
                                        if let Some(mut ib) = dialog.editing.take() {
                                            let text = ib.take();
                                            let idx = dialog.selected;
                                            dialog.draft.set_text_value(idx, text);
                                            dialog.error = None;
                                        }
                                    }
                                }
                                KeyCode::Esc => {
                                    if let Some(dialog) = &mut state.settings_dialog {
                                        dialog.editing = None;
                                    }
                                }
                                KeyCode::Char(c) => {
                                    if let Some(ib) = state
                                        .settings_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.insert_char(c);
                                    }
                                }
                                KeyCode::Backspace => {
                                    if let Some(ib) = state
                                        .settings_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.backspace();
                                    }
                                }
                                KeyCode::Left => {
                                    if let Some(ib) = state
                                        .settings_dialog
                                        .as_mut()
                                        .and_then(|d| d.editing.as_mut())
                                    {
                                        ib.move_left();
                                    }
                                }
                                KeyCode::Right => {
                                    if let Some(ib) = state
                                        .settings_dialog
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
                                    if matches!(settings_field_kind(idx), SettingsFieldKind::Enum) {
                                        dialog.draft.cycle_enum(idx, false);
                                    }
                                }
                            }
                            KeyCode::Right => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    let idx = dialog.selected;
                                    if matches!(settings_field_kind(idx), SettingsFieldKind::Enum) {
                                        dialog.draft.cycle_enum(idx, true);
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(dialog) = &mut state.settings_dialog {
                                    let idx = dialog.selected;
                                    match settings_field_kind(idx) {
                                        SettingsFieldKind::Enum => {
                                            dialog.draft.cycle_enum(idx, true)
                                        }
                                        SettingsFieldKind::Text => {
                                            let mut ib = InputBox::new();
                                            ib.insert_text(dialog.draft.text_value(idx));
                                            dialog.editing = Some(ib);
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let mut should_close = false;
                                if let Some(dialog) = &mut state.settings_dialog {
                                    if let Some(err_key) = dialog.draft.validate() {
                                        dialog.error = Some(wyj_i18n::tr(err_key));
                                    } else {
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
                                                        state.context_window =
                                                            state.config.context_window;
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

                    // ① plan 批准对话框最高优先级
                    if state.plan_dialog.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if let Some(dlg) = state.plan_dialog.take() {
                                    let _ = dlg.response_tx.send(true);
                                    // 切换至执行模式
                                    let new_mode = AgentMode::Normal;
                                    *shared_mode.lock().await = new_mode.clone();
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
                                    *shared_mode.lock().await = new_mode.clone();
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
                                        ui_ask_tx.clone(),
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
                                        ui_ask_tx.clone(),
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
                    if state.ask_question_dialog.is_some() {
                        match key.code {
                            KeyCode::Up => {
                                if let Some(dlg) = &mut state.ask_question_dialog {
                                    if dlg.selected > 0 {
                                        dlg.selected -= 1;
                                    }
                                }
                            }
                            KeyCode::Down => {
                                if let Some(dlg) = &mut state.ask_question_dialog {
                                    if dlg.selected + 1 < dlg.options.len() {
                                        dlg.selected += 1;
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(dlg) = state.ask_question_dialog.take() {
                                    let _ = dlg.response_tx.send(Some(dlg.selected));
                                }
                            }
                            KeyCode::Esc => {
                                if let Some(dlg) = state.ask_question_dialog.take() {
                                    let _ = dlg.response_tx.send(None);
                                }
                            }
                            _ => {}
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
                        *shared_mode.lock().await = new_mode.clone();
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
                                *shared_mode.lock().await = new_mode.clone();
                                state.mode = new_mode;
                                state.messages.push(ChatMessage::system(wyj_i18n::tr(
                                    "mode.switched_to_plan",
                                )));
                                state.input_history.push(trimmed);
                                continue;
                            }

                            // ── /mode 命令：运行时切换模式 ───────────────────
                            if let Some(args) = trimmed.strip_prefix("/mode") {
                                let args = args.trim();
                                let new_mode = match args {
                                    "plan" => Some(AgentMode::Plan),
                                    "bypass" => Some(AgentMode::Bypass),
                                    "normal" | "" => Some(AgentMode::Normal),
                                    _ => None,
                                };
                                match new_mode {
                                    Some(m) => {
                                        let label = m.label();
                                        *shared_mode.lock().await = m.clone();
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
                                    Ok(CommandResult::SetModel(m)) => {
                                        match rebuild_fn(&state.config, &m) {
                                            Ok(new_agent) => {
                                                let new_agent = wire_tool_callback(
                                                    new_agent,
                                                    agent_tx.clone(),
                                                    todo_store.clone(),
                                                );
                                                *shared_agent.write().unwrap() =
                                                    Arc::new(new_agent);
                                                state.model_name = m.clone();
                                                state.messages.push(ChatMessage::assistant(
                                                    wyj_i18n::tr_fmt(
                                                        "model.switched",
                                                        &[("model", &m)],
                                                    ),
                                                ));
                                            }
                                            Err(e) => {
                                                state.messages.push(ChatMessage::assistant_err(
                                                    wyj_i18n::tr_fmt(
                                                        "model.switch_failed",
                                                        &[("err", &e.to_string())],
                                                    ),
                                                ));
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
                                        let ui_ask_tx_clone = ui_ask_tx.clone();

                                        let handle = tokio::spawn(async move {
                                            let mut sess = session_c.lock().await;
                                            sess.push_user(prompt);
                                            let current_mode = mode_arc.lock().await.clone();
                                            let mut ctx = ToolCtx::new(&ctx_cwd);
                                            ctx.permission_mode = mode_to_permission(&current_mode);
                                            ctx.ui_ask_tx = Some(ui_ask_tx_clone);
                                            let turn_agent =
                                                plan_turn_agent(&agent_c, &current_mode, &ctx_cwd);
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
                                                    let plan_approved =
                                                        has_plan_approved(&sess.messages);
                                                    drop(sess);
                                                    current_session_id = file.session_id.clone();
                                                    state.messages = display_msgs;
                                                    state.total_input_tokens = file.input_tokens;
                                                    state.total_output_tokens = file.output_tokens;
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
                                        ui_ask_tx.clone(),
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
                                let _ = tx.send(blocks);
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
