//! wyj-code 配置模块
//! 管理 ~/.wyj-code/ 目录下的配置文件与 API Key 读取。

use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod codex;
pub mod project_mcp;
pub mod project_settings;
pub use codex::{codex_home_dir, load_codex_mcp};
pub use project_mcp::{
    load_native_mcp, load_project_mcp, merged_mcp_servers, native_mcp_names, project_mcp_path,
    save_project_mcp, ProjectMcpConfig,
};
pub use project_settings::{
    load_project_settings, project_settings_path, save_project_settings, ProjectSettings,
};

// ── MCP Server 配置 ───────────────────────────────────────────────────────────

/// MCP 服务器传输类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    /// Streamable HTTP transport. The legacy `http` spelling is accepted.
    #[serde(rename = "streamable_http", alias = "http")]
    StreamableHttp,
}

/// 单个 MCP server 配置（在 ~/.wyj-code/config.toml 的 [[mcp_servers]] 段声明）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// 服务名称（用于日志区分）
    pub name: String,
    /// 传输类型
    pub transport: McpTransport,
    /// stdio: 可执行命令
    #[serde(default)]
    pub command: Option<String>,
    /// stdio: 参数列表
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio: 附加环境变量
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// streamable_http: remote MCP endpoint
    #[serde(default)]
    pub url: Option<String>,
    /// streamable_http: additional headers. Values may be `${ENV_VAR}` references.
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

/// Agent 运行模式
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// 正常模式：全部工具可用，TUI 下工具调用前弹确认
    #[default]
    Normal,
    /// Plan 模式：仅允许只读工具（read / glob / grep / web_fetch），适合规划分析
    Plan,
    /// Bypass 模式：自动允许所有工具调用，不弹确认对话框
    Bypass,
}

impl AgentMode {
    pub fn label(&self) -> &'static str {
        match self {
            AgentMode::Normal => "normal",
            AgentMode::Plan => "plan",
            AgentMode::Bypass => "bypass",
        }
    }

    /// Plan 模式下允许的只读工具集
    pub fn allowed_tools(&self) -> Option<&'static [&'static str]> {
        match self {
            AgentMode::Plan => Some(&[
                "read",
                "glob",
                "grep",
                "web_fetch",
                "ask_question",
                "write",
                "bash",
                "exit_plan_mode",
            ]),
            _ => None,
        }
    }
}

/// 支持的 LLM 供应商格式
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Anthropic,
    OpenAI,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::Anthropic => write!(f, "anthropic"),
            Provider::OpenAI => write!(f, "openai"),
        }
    }
}

/// 一个具名的"调用分组"：一套完整的供应商调用参数，可与其他分组并存、按名切换。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// 分组名称（在 Config.profiles 中唯一）
    pub name: String,
    /// LLM 供应商格式（anthropic 或 openai）
    pub provider: Provider,
    /// 默认模型名称
    pub model: String,
    /// Plan 模式专用模型（留空则使用 model）
    #[serde(default)]
    pub plan_model: Option<String>,
    /// Exec/Bypass 模式专用模型（留空则使用 model）
    #[serde(default)]
    pub exec_model: Option<String>,
    /// API 端点（留空使用供应商默认值）
    #[serde(default)]
    pub base_url: String,
    /// API Key（优先从环境变量 WYJ_CODE_API_KEY 读取，覆盖到激活分组）
    #[serde(default)]
    pub api_key: Option<String>,
    /// 最大 token 预算（每轮）
    pub max_tokens: u32,
    /// 模型最大上下文窗口 token 数（用于自动压缩触发判断）
    pub context_window: u32,
    /// 模型是否支持图片输入（多模态）。false 时图片以占位文本发送，
    /// 避免非多模态端点收到 image 块返回 400。默认 true。
    #[serde(default = "default_vision")]
    pub vision: bool,
    /// Extended thinking 预算 token 数。None/0 = 关闭（默认）。
    /// 开启后请求携带 thinking 参数，思考内容计入 output token 计费。
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    /// 工具调用轮之间是否允许交错思考（interleaved thinking beta，
    /// 仅在 thinking_budget 开启时生效）。默认 true。
    #[serde(default = "default_vision")]
    pub interleaved_thinking: bool,
    /// Anthropic 协议 prompt caching 能力。None = 按 base_url/provider 自动判断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<bool>,
    /// OpenAI 协议 stream_options.include_usage 能力。None = 按 base_url/provider/model 自动判断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_stream_options: Option<bool>,
}

fn default_vision() -> bool {
    true
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            provider: Provider::Anthropic,
            model: "claude-opus-4-8".to_string(),
            plan_model: None,
            exec_model: None,
            base_url: String::new(),
            api_key: None,
            max_tokens: 8192,
            context_window: 200_000,
            vision: true,
            thinking_budget: None,
            interleaved_thinking: true,
            prompt_cache: None,
            openai_stream_options: None,
        }
    }
}

impl Profile {
    /// 当前 profile 是否指向真正的 Anthropic 官方端点，而非仅仅"说 Anthropic
    /// 协议"的第三方兼容服务（MiniMax/GLM/Kimi 等常以 `provider = "anthropic"`
    /// 搭配自定义 `base_url` 接入）。只有官方端点才认得 Anthropic 专属扩展
    /// ——prompt caching beta、原生 computer-use 工具（`computer_20251124`）等；
    /// 第三方端点收到这些会直接 400，不是优雅降级，因此调用方必须显式检查
    /// 这个信号，不能只判断 `provider == Anthropic`。
    pub fn is_official_anthropic_endpoint(&self) -> bool {
        self.provider == Provider::Anthropic
            && (self.base_url.trim().is_empty()
                || self.base_url.trim_end_matches('/') == "https://api.anthropic.com")
    }

    pub fn effective_prompt_cache(&self) -> bool {
        self.prompt_cache
            .unwrap_or_else(|| self.is_official_anthropic_endpoint())
    }

    /// 指定模型是否必须请求供应商返回 usage，作为精确 token 账本来源。
    ///
    /// MiniMax、GLM、DeepSeek 的 tokenizer/聊天模板会随模型版本变化；不以
    /// `chars/token` 之类的本地猜测冒充精确值，而是使用供应商对实际请求（含
    /// system、tool schema 与消息包装）返回的 usage。显式配置
    /// `openai_stream_options = false` 仍优先，以兼容不支持该字段的私有代理。
    pub fn uses_provider_exact_token_usage_for_model(&self, model: &str) -> bool {
        let model = model.to_ascii_lowercase();
        let base_url = self.base_url.to_ascii_lowercase();
        model.contains("minimax")
            || model.contains("glm")
            || model.contains("deepseek")
            || base_url.contains("minimaxi.com")
            || base_url.contains("bigmodel.cn")
            || base_url.contains("z.ai")
            || base_url.contains("deepseek.com")
    }

    pub fn effective_openai_stream_options_for_model(&self, model: &str) -> bool {
        self.openai_stream_options.unwrap_or_else(|| {
            self.provider == Provider::OpenAI
                && (self.base_url.trim().is_empty()
                    || self.base_url.trim_end_matches('/') == "https://api.openai.com/v1"
                    || self.uses_provider_exact_token_usage_for_model(model))
        })
    }

    pub fn effective_openai_stream_options(&self) -> bool {
        self.effective_openai_stream_options_for_model(&self.model)
    }
}

/// [subagent] 节 — 子 Agent 全局模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SubAgentCfg {
    /// 子 Agent 默认使用的 Profile 名（留空则沿用主 Agent 当前分组与模型）
    pub default_profile: Option<String>,
    /// 内置 Explore 类型专用 Profile 名（留空则回退 default_profile）
    pub explore_profile: Option<String>,
    /// 是否把子 Agent 完整执行轨迹（工具调用序列、全文 input/output、usage）
    /// 落盘到 `~/.wyj-code/sessions/<session_id>.subagents/a<id>.jsonl`，供
    /// `/resume`、`/subagents`、`subagent-trace` 子命令跨会话查看。默认开启。
    pub trace_enabled: bool,
    /// 单个子 Agent trace 文件的字节上限，超限后该子 Agent 的后续事件静默停写
    /// （不影响其它子 Agent、不影响主流程）。默认 256KB。
    pub trace_max_bytes_per_agent: u64,
}

impl Default for SubAgentCfg {
    fn default() -> Self {
        Self {
            default_profile: None,
            explore_profile: None,
            trace_enabled: true,
            trace_max_bytes_per_agent: 256 * 1024,
        }
    }
}

/// 旧版全局 `computer` 工具需要占用前台鼠标/键盘时的回退策略。
///
/// v1.4 起后台 `app_computer` 是默认路径；前台接管必须显式配置，不能在
/// 后台动作不支持时悄悄降级，否则仍会和人类用户争夺焦点与输入设备。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundFallback {
    /// 完全禁用前台接管（默认）。
    #[default]
    Disabled,
    /// 每次接管仍走现有工具权限确认，并等待输入仲裁器确认安静窗口。
    Ask,
    /// 不额外询问，但只在用户持续空闲时执行；超过最大等待时间即放弃。
    IdleOnly,
}

/// `[computer_use]` 节 — computer-use 人机互不干扰策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerUseCfg {
    /// 旧版 `computer` 前台兼容工具的启用策略。
    pub foreground_fallback: ForegroundFallback,
    /// 获得前台输入租约前，必须连续没有外部输入的时长。
    pub quiet_period_ms: u64,
    /// 等待用户空闲的最长时间；到期后失败关闭，不强行执行。
    pub max_defer_secs: u64,
    /// 前台兼容动作结束后是否尝试恢复原观察上下文。
    pub restore_context: bool,
}

impl Default for ComputerUseCfg {
    fn default() -> Self {
        Self {
            foreground_fallback: ForegroundFallback::Disabled,
            quiet_period_ms: 2_000,
            max_defer_secs: 30,
            restore_context: true,
        }
    }
}

/// 主配置结构，对应 ~/.wyj-code/config.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 当前激活的分组名（对应 profiles 中某一项的 name）
    pub active_profile: String,
    /// 所有已配置的调用分组
    pub profiles: Vec<Profile>,
    /// 日志级别
    pub log_level: String,
    /// 界面/AI 回复语言（"en"/"zh"）。留空则自动检测系统 locale。
    #[serde(default)]
    pub language: Option<String>,
    /// MCP server 列表（空列表则不启动任何 MCP）
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// 是否启用跨会话记忆自动提取（/memory 面板可切换，默认开启）
    #[serde(default = "default_true")]
    pub auto_memory_enabled: bool,
    /// 子 Agent 模型配置（[subagent] 节）
    #[serde(default)]
    pub subagent: SubAgentCfg,
    /// computer-use 后台优先与前台回退策略（[computer_use] 节）
    #[serde(default)]
    pub computer_use: ComputerUseCfg,
    /// WebSearch 搜索 provider（目前支持 "tavily"）
    #[serde(default = "default_search_provider")]
    pub search_provider: String,
    /// WebSearch API Key（优先从环境变量 WYJ_CODE_SEARCH_API_KEY 读取）。
    /// 未配置时 WebSearch 工具不会注册，模型看不到该工具。
    #[serde(default)]
    pub search_api_key: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_search_provider() -> String {
    "tavily".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            active_profile: "default".to_string(),
            profiles: vec![Profile::default()],
            log_level: "warn".to_string(),
            language: None,
            mcp_servers: vec![],
            auto_memory_enabled: true,
            subagent: SubAgentCfg::default(),
            computer_use: ComputerUseCfg::default(),
            search_provider: default_search_provider(),
            search_api_key: None,
        }
    }
}

/// 旧版（v0）扁平配置结构，仅用于 `Config::load()` 里一次性迁移旧 config.toml。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct LegacyConfigV0 {
    provider: Provider,
    model: String,
    #[serde(default)]
    plan_model: Option<String>,
    #[serde(default)]
    exec_model: Option<String>,
    base_url: String,
    api_key: Option<String>,
    max_tokens: u32,
    context_window: u32,
    log_level: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,
}

impl Default for LegacyConfigV0 {
    fn default() -> Self {
        let p = Profile::default();
        Self {
            provider: p.provider,
            model: p.model,
            plan_model: p.plan_model,
            exec_model: p.exec_model,
            base_url: p.base_url,
            api_key: p.api_key,
            max_tokens: p.max_tokens,
            context_window: p.context_window,
            log_level: "warn".to_string(),
            language: None,
            mcp_servers: vec![],
        }
    }
}

impl From<LegacyConfigV0> for Config {
    fn from(legacy: LegacyConfigV0) -> Self {
        Config {
            active_profile: "default".to_string(),
            profiles: vec![Profile {
                name: "default".to_string(),
                provider: legacy.provider,
                model: legacy.model,
                plan_model: legacy.plan_model,
                exec_model: legacy.exec_model,
                base_url: legacy.base_url,
                api_key: legacy.api_key,
                max_tokens: legacy.max_tokens,
                context_window: legacy.context_window,
                vision: true,
                thinking_budget: None,
                interleaved_thinking: true,
                prompt_cache: None,
                openai_stream_options: None,
            }],
            log_level: legacy.log_level,
            language: legacy.language,
            mcp_servers: legacy.mcp_servers,
            auto_memory_enabled: true,
            subagent: SubAgentCfg::default(),
            computer_use: ComputerUseCfg::default(),
            search_provider: default_search_provider(),
            search_api_key: None,
        }
    }
}

impl Config {
    /// 按名查找分组。
    pub fn profile_by_name(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// 返回当前激活分组（按名查找，找不到则回退到第一个；profiles 非空是不变量）。
    pub fn active_profile(&self) -> &Profile {
        self.profiles
            .iter()
            .find(|p| p.name == self.active_profile)
            .unwrap_or(&self.profiles[0])
    }

    /// 返回当前激活分组的可变引用。
    pub fn active_profile_mut(&mut self) -> &mut Profile {
        let name = self.active_profile.clone();
        if let Some(idx) = self.profiles.iter().position(|p| p.name == name) {
            &mut self.profiles[idx]
        } else {
            &mut self.profiles[0]
        }
    }

    /// 当前激活分组的供应商格式。
    pub fn provider(&self) -> &Provider {
        &self.active_profile().provider
    }

    /// 根据 AgentMode 返回对应模型名（未配置则回退到激活分组的 model）
    pub fn model_for_mode(&self, mode: &AgentMode) -> &str {
        let p = self.active_profile();
        match mode {
            AgentMode::Plan => p.plan_model.as_deref().unwrap_or(&p.model),
            AgentMode::Normal | AgentMode::Bypass => p.exec_model.as_deref().unwrap_or(&p.model),
        }
    }
}

impl Config {
    /// 加载配置：先读文件（含旧格式一次性迁移），再合并 `~/.claude.json` 的原生
    /// MCP 配置，最后用环境变量覆盖激活分组的 api_key。
    pub fn load() -> Result<Self> {
        let mut cfg = Self::load_file_only()?;

        // Claude Code's global native MCP file has higher precedence than the
        // legacy wyj global TOML, while remaining read-only until explicit migrate.
        if let Ok(home) = home_dir() {
            let native_path = home.join(".claude.json");
            if let Ok(native_servers) = load_native_mcp(&native_path) {
                for server in native_servers {
                    if let Some(existing) =
                        cfg.mcp_servers.iter_mut().find(|s| s.name == server.name)
                    {
                        *existing = server;
                    } else {
                        cfg.mcp_servers.push(server);
                    }
                }
            }
        }

        // 环境变量优先，覆盖到激活分组
        if let Ok(key) = std::env::var("WYJ_CODE_API_KEY") {
            if !key.is_empty() {
                cfg.active_profile_mut().api_key = Some(key);
            }
        }
        // WebSearch key：环境变量优先
        if let Ok(key) = std::env::var("WYJ_CODE_SEARCH_API_KEY") {
            if !key.is_empty() {
                cfg.search_api_key = Some(key);
            }
        }

        Ok(cfg)
    }

    /// 只读 `config.toml` 文件本体（含旧格式一次性迁移）：不合并 `~/.claude.json`
    /// 的原生 MCP、不吃环境变量。`/import` 等"要把结果写回 config.toml"的路径必须
    /// 用这个入口做冲突检测与写回，否则会把只读的原生 server 误物化进文件。
    pub fn load_file_only() -> Result<Self> {
        Self::load_file_only_at(&config_file_path()?)
    }

    /// `load_file_only` 的路径注入版（测试、`/import` 的 `ImportTargets` 使用）。
    pub fn load_file_only_at(config_path: &Path) -> Result<Self> {
        let mut cfg: Config = if config_path.exists() {
            let content = std::fs::read_to_string(config_path)
                .with_context(|| format!("读取配置文件失败: {}", config_path.display()))?;
            let value: toml::Value = toml::from_str(&content)
                .with_context(|| format!("解析配置文件失败: {}", config_path.display()))?;
            if value.get("profiles").is_some() {
                toml::from_str(&content)
                    .with_context(|| format!("解析配置文件失败: {}", config_path.display()))?
            } else {
                let legacy: LegacyConfigV0 = toml::from_str(&content)
                    .with_context(|| format!("解析旧版配置文件失败: {}", config_path.display()))?;
                let migrated: Config = legacy.into();
                migrated
                    .save_to(config_path)
                    .context("迁移旧版配置文件失败")?;
                tracing::info!(
                    "已将旧版配置迁移为分组结构，默认分组名为 default: {}",
                    config_path.display()
                );
                migrated
            }
        } else {
            Config::default()
        };

        if cfg.profiles.is_empty() {
            cfg.profiles.push(Profile::default());
        }
        if !cfg.profiles.iter().any(|p| p.name == cfg.active_profile) {
            cfg.active_profile = cfg.profiles[0].name.clone();
        }

        Ok(cfg)
    }

    /// 返回激活分组的有效 API Key，若无则报错。
    pub fn api_key(&self) -> Result<&str> {
        self.active_profile()
            .api_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "未找到 API Key。请设置环境变量 WYJ_CODE_API_KEY 或在配置文件中设置 api_key。"
                )
            })
    }

    /// 返回激活分组的 base_url（若配置为空则用供应商默认值）。
    pub fn resolved_base_url(&self) -> &str {
        let p = self.active_profile();
        if !p.base_url.is_empty() {
            &p.base_url
        } else {
            match p.provider {
                Provider::Anthropic => "https://api.anthropic.com",
                Provider::OpenAI => "https://api.openai.com/v1",
            }
        }
    }

    /// 将当前配置写入文件。
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_file_path()?)
    }

    /// 将当前配置写入指定路径（供 `/import` 等需要注入目标路径的调用方与测试使用）。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self).context("序列化配置失败")?;
        write_atomic(path, &content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))
    }
}

pub(crate) fn write_atomic(path: &std::path::Path, content: &str) -> Result<()> {
    let nonce = format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let tmp = path.with_file_name(format!(
        "{}.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config"),
        nonce
    ));
    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// 返回配置目录路径（~/.wyj-code），若不存在则创建。
pub fn config_dir() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().ok_or_else(|| anyhow::anyhow!("无法获取用户主目录"))?;
    let dir = global_config_dir_in(user_dirs.home_dir());
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        tracing::info!("初始化配置目录: {}", dir.display());
    }
    Ok(dir)
}

/// 给定 home 下的全局配置目录（`<home>/.wyj-code`）。纯路径拼接、不创建目录、
/// 不查真实主目录，供 `load_skills` 等以 home 为注入参数的 API 使用。
pub fn global_config_dir_in(home: &Path) -> PathBuf {
    home.join(".wyj-code")
}

/// 返回 `cwd` 所属项目的根目录：优先向上查找 Git 仓库根；找不到 `.git` 时
/// 回退到规范化后的 `cwd` 本身。这里只检查文件系统标记，不执行 git 命令，
/// 避免项目配置发现进入启动性能关键路径。
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = Some(cwd);
    while let Some(candidate) = dir {
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
        dir = candidate.parent();
    }
    cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())
}

/// 项目级配置目录（`<git-root>/.wyj-code`），承载 `skills/`、`agents/`、
/// `mcp.toml`、`settings.toml`、`installed.json`。只解析路径、不创建（只读操作
/// 不应污染用户项目目录），由各写入方在落盘前自行 `create_dir_all`。
///
/// 从仓库任意子目录启动时都会指向同一个目录，保证 Skill/MCP/settings/agent
/// 的读取、安装和禁用状态不会随当前子目录漂移。
pub fn project_config_dir(cwd: &Path) -> PathBuf {
    project_root(cwd).join(".wyj-code")
}

/// 返回主配置文件路径（~/.wyj-code/config.toml）。
pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// 返回真实 Claude Code 的全局配置目录路径（~/.claude），仅解析路径、不创建。
/// 复用该路径是为了让 wyj-code 直接吃到用户已有的真实 Claude Code 全局
/// CLAUDE.md 记忆，与其使用习惯保持一致。
pub fn claude_home_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

/// 返回用户主目录路径。
pub fn home_dir() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().ok_or_else(|| anyhow::anyhow!("无法获取用户主目录"))?;
    Ok(user_dirs.home_dir().to_path_buf())
}

#[cfg(test)]
mod project_path_tests {
    use super::*;

    #[test]
    fn project_config_dir_walks_to_git_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("crates").join("demo").join("src");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(project_root(&nested), repo.path());
        assert_eq!(project_config_dir(&nested), repo.path().join(".wyj-code"));
    }

    #[test]
    fn project_config_dir_falls_back_to_non_git_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(project_root(dir.path()), canonical);
        assert_eq!(project_config_dir(dir.path()), canonical.join(".wyj-code"));
    }
}

#[cfg(test)]
mod subagent_cfg_tests {
    use super::{ComputerUseCfg, ForegroundFallback, Profile, Provider, SubAgentCfg};

    #[test]
    fn computer_use_defaults_fail_closed_for_foreground_takeover() {
        let cfg = ComputerUseCfg::default();
        assert_eq!(cfg.foreground_fallback, ForegroundFallback::Disabled);
        assert_eq!(cfg.quiet_period_ms, 2_000);
        assert_eq!(cfg.max_defer_secs, 30);
        assert!(cfg.restore_context);
    }

    #[test]
    fn partial_computer_use_section_keeps_safe_defaults() {
        let cfg: ComputerUseCfg = toml::from_str("foreground_fallback = \"idle_only\"").unwrap();
        assert_eq!(cfg.foreground_fallback, ForegroundFallback::IdleOnly);
        assert_eq!(cfg.quiet_period_ms, 2_000);
        assert_eq!(cfg.max_defer_secs, 30);
    }

    #[test]
    fn defaults_enable_trace_with_256kb_cap() {
        let cfg = SubAgentCfg::default();
        assert!(cfg.trace_enabled);
        assert_eq!(cfg.trace_max_bytes_per_agent, 256 * 1024);
    }

    #[test]
    fn empty_toml_section_falls_back_to_defaults() {
        let cfg: SubAgentCfg = toml::from_str("").unwrap();
        assert!(cfg.trace_enabled);
        assert_eq!(cfg.trace_max_bytes_per_agent, 256 * 1024);
    }

    #[test]
    fn trace_enabled_can_be_turned_off_without_specifying_other_fields() {
        let cfg: SubAgentCfg = toml::from_str("trace_enabled = false").unwrap();
        assert!(!cfg.trace_enabled);
        // 未显式指定的字段仍走默认值
        assert_eq!(cfg.trace_max_bytes_per_agent, 256 * 1024);
    }

    #[test]
    fn official_endpoints_enable_native_optimizations_by_default() {
        let mut p = Profile {
            provider: Provider::Anthropic,
            base_url: String::new(),
            ..Profile::default()
        };
        assert!(p.effective_prompt_cache());

        p.provider = Provider::OpenAI;
        p.base_url.clear();
        assert!(p.effective_openai_stream_options());
    }

    #[test]
    fn is_official_anthropic_endpoint_distinguishes_real_api_from_compatible_proxies() {
        // 真官方端点：留空 base_url 或显式填官方地址
        let official_blank = Profile {
            provider: Provider::Anthropic,
            base_url: String::new(),
            ..Profile::default()
        };
        assert!(official_blank.is_official_anthropic_endpoint());
        let official_explicit = Profile {
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".to_string(),
            ..Profile::default()
        };
        assert!(official_explicit.is_official_anthropic_endpoint());

        // 第三方 Anthropic 协议兼容端点（如 MiniMax/GLM 走 provider="anthropic"
        // 但 base_url 指向自己的域名）：不是官方端点，不能发原生扩展
        let minimax_via_anthropic_protocol = Profile {
            provider: Provider::Anthropic,
            base_url: "https://api.minimaxi.com/anthropic".to_string(),
            ..Profile::default()
        };
        assert!(!minimax_via_anthropic_protocol.is_official_anthropic_endpoint());

        // provider=OpenAI 一律不是官方 Anthropic 端点，即便 base_url 恰好留空
        let openai = Profile {
            provider: Provider::OpenAI,
            base_url: String::new(),
            ..Profile::default()
        };
        assert!(!openai.is_official_anthropic_endpoint());
    }

    #[test]
    fn compatible_third_party_endpoints_keep_unrelated_optimizations_disabled() {
        let p = Profile {
            provider: Provider::Anthropic,
            base_url: "https://open.bigmodel.cn/api/anthropic".to_string(),
            ..Profile::default()
        };
        assert!(!p.effective_prompt_cache());

        let p = Profile {
            provider: Provider::OpenAI,
            base_url: "https://compatible.example/v1".to_string(),
            ..Profile::default()
        };
        assert!(!p.effective_openai_stream_options());
    }

    #[test]
    fn domestic_models_enable_usage_streams_for_exact_token_accounting() {
        for (base_url, model) in [
            ("https://api.minimaxi.com/v1", "MiniMax-M2"),
            ("https://api.deepseek.com", "deepseek-chat"),
            ("https://ark.cn-beijing.volces.com/api/v3", "glm-5.2"),
        ] {
            let p = Profile {
                provider: Provider::OpenAI,
                base_url: base_url.to_string(),
                model: model.to_string(),
                ..Profile::default()
            };
            assert!(p.uses_provider_exact_token_usage_for_model(model));
            assert!(p.effective_openai_stream_options_for_model(model));
        }
    }

    #[test]
    fn explicit_compatibility_switches_win_over_endpoint_defaults() {
        let p = Profile {
            provider: Provider::Anthropic,
            base_url: String::new(),
            prompt_cache: Some(false),
            ..Profile::default()
        };
        assert!(!p.effective_prompt_cache());

        let p = Profile {
            provider: Provider::OpenAI,
            base_url: "https://api.minimaxi.com/v1".to_string(),
            openai_stream_options: Some(true),
            ..Profile::default()
        };
        assert!(p.effective_openai_stream_options());

        let p = Profile {
            provider: Provider::OpenAI,
            model: "deepseek-chat".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            openai_stream_options: Some(false),
            ..Profile::default()
        };
        assert!(!p.effective_openai_stream_options());
    }
}
