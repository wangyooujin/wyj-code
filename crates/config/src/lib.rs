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
    /// Plan 模式：允许只读工具与受限规划文档写入；执行层仍按路径和命令复核。
    Plan,
    /// Bypass 模式：跳过普通交互确认，但不覆盖 protected deny 或 OS sandbox。
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

    /// Plan 模式下进入执行层复核的工具集；Write/Bash 不代表任意写入获批。
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

/// 模型端点使用的线协议。`Provider` 保留旧配置中的二分法并负责选择现有
/// 客户端实现；`WireProtocol` 则描述端点实际接受的请求格式，避免把模型厂商
/// 与兼容协议混为一谈。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    QwenNative,
    Gemini,
}

impl std::fmt::Display for WireProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiChatCompletions => "open_ai_chat_completions",
            Self::OpenAiResponses => "open_ai_responses",
            Self::QwenNative => "qwen_native",
            Self::Gemini => "gemini",
        };
        f.write_str(value)
    }
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
    /// 模型厂商或部署平台，例如 anthropic、minimax、zhipu、moonshot。
    /// 旧配置缺失时由模型目录和端点推导；不影响旧 provider 的客户端选择。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// 端点实际使用的线协议。缺失时与旧 provider 保持一致。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_protocol: Option<WireProtocol>,
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
    /// 推荐的 secret reference：运行时从该环境变量读取，不把真实值写回配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
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
            vendor: None,
            wire_protocol: None,
            model: "claude-opus-4-8".to_string(),
            plan_model: None,
            exec_model: None,
            base_url: String::new(),
            api_key: None,
            api_key_env: None,
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
    /// 在不改变旧配置行为的前提下得到有效线协议。
    pub fn effective_wire_protocol(&self) -> WireProtocol {
        self.wire_protocol.clone().unwrap_or(match self.provider {
            Provider::Anthropic => WireProtocol::AnthropicMessages,
            Provider::OpenAI => WireProtocol::OpenAiChatCompletions,
        })
    }

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

/// `[routing]` 节 — 按角色选择模型 Profile，并在可恢复供应商故障时按顺序切换。
///
/// Profile 名称而不是裸模型 id 是路由单元：这样每个候选都能携带自己的 vendor、
/// wire protocol、endpoint 与能力配置。默认禁止跨 vendor fallback，避免把同一份
/// 对话在用户未授权的情况下发送给另一家供应商。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RoutingCfg {
    pub cross_provider_fallback: bool,
    pub roles: RoutingRoles,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RoutingRoles {
    pub explore: Vec<String>,
    pub plan: Vec<String>,
    pub execute: Vec<String>,
    pub review: Vec<String>,
}

impl RoutingRoles {
    pub fn for_role(&self, role: RoutingRole) -> &[String] {
        match role {
            RoutingRole::Explore => &self.explore,
            RoutingRole::Plan => &self.plan,
            RoutingRole::Execute => &self.execute,
            RoutingRole::Review => &self.review,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingRole {
    Explore,
    Plan,
    Execute,
    Review,
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

/// `[model_runtime]`：国内模型工具协议与能力探测的保守默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRuntimeCfg {
    pub probe_mode: String,
    pub probe_ttl_hours: u64,
    pub tool_argument_retries: usize,
    pub lazy_tools_threshold: usize,
    pub lazy_tools_top_k: usize,
    pub lazy_tools_sticky_turns: u64,
}

impl Default for ModelRuntimeCfg {
    fn default() -> Self {
        Self {
            probe_mode: "explicit".to_string(),
            probe_ttl_hours: 168,
            tool_argument_retries: 2,
            lazy_tools_threshold: 12,
            lazy_tools_top_k: 8,
            lazy_tools_sticky_turns: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SandboxFilesystemCfg {
    pub allow_read: Vec<PathBuf>,
    pub allow_write: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    pub deny_write: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SandboxNetworkCfg {
    /// 允许 sandbox 内的命令访问任意公网地址；与 allowed_domains 互斥。
    pub allow_all: bool,
    pub allowed_domains: Vec<String>,
    pub allow_local_binding: bool,
    pub allow_unix_sockets: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxEnvironmentCfg {
    /// 继承启动 wyj-code 的宿主环境；默认关闭以避免模型直接读取任意 secret。
    pub inherit: bool,
    /// 最小环境模式下额外允许的变量名。
    pub allow: Vec<String>,
    /// 继承模式下仍强制移除的变量名。
    pub deny: Vec<String>,
}

impl Default for SandboxEnvironmentCfg {
    fn default() -> Self {
        Self {
            inherit: false,
            allow: Vec::new(),
            deny: vec![
                "WYJ_CODE_API_KEY".to_string(),
                "WYJ_CODE_SEARCH_API_KEY".to_string(),
                "WYJ_CODE_PROBE_API_KEY".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxCfg {
    pub enabled: bool,
    pub auto_allow_sandboxed: bool,
    pub allow_unsandboxed_commands: bool,
    pub fail_if_unavailable: bool,
    pub filesystem: SandboxFilesystemCfg,
    pub network: SandboxNetworkCfg,
    pub environment: SandboxEnvironmentCfg,
}

impl Default for SandboxCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_allow_sandboxed: false,
            allow_unsandboxed_commands: true,
            fail_if_unavailable: false,
            filesystem: SandboxFilesystemCfg::default(),
            network: SandboxNetworkCfg::default(),
            environment: SandboxEnvironmentCfg::default(),
        }
    }
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

/// 自进化保留与容量策略。不同证据类型的有效期不同，避免用一个统一 TTL
/// 同时误删明确用户偏好、又让仓库事实永久陈旧。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionRetentionCfg {
    pub repository_fact_stale_days: u32,
    pub workflow_stale_days: u32,
    pub user_preference_review_days: u32,
    pub failed_episode_days: u32,
    pub candidate_days: u32,
    pub audit_days: u32,
}

impl Default for EvolutionRetentionCfg {
    fn default() -> Self {
        Self {
            repository_fact_stale_days: 28,
            workflow_stale_days: 60,
            user_preference_review_days: 180,
            failed_episode_days: 30,
            candidate_days: 90,
            audit_days: 180,
        }
    }
}

/// 本地、自包含的 Agent 经验闭环。v1.5.5 收敛后只剩 Rule/Skill 治理；
/// 普通 Memory 数据层迁出 Evolution 后 `generate_experiences` /
/// `auto_activate_memories` 不再有业务含义，已删除（serde 默认忽略旧字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionCfg {
    pub enabled: bool,
    pub use_experiences: bool,
    pub suggest_rules: bool,
    pub suggest_skills: bool,
    pub auto_activate_rules: bool,
    pub auto_install_skills: bool,
    pub allow_self_code_experiments: bool,
    pub exclude_external_context: bool,
    pub infer_feedback: bool,
    pub skill_candidate_min_successes: u32,
    pub skill_candidate_min_sessions: u32,
    pub idle_delay_secs: u64,
    pub max_background_workers: u32,
    pub max_context_bytes: usize,
    pub max_daily_tokens: u32,
    pub max_daily_wall_secs: u64,
    pub max_project_store_bytes: u64,
    pub evolution_profile: String,
    pub retention: EvolutionRetentionCfg,
}

impl Default for EvolutionCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            use_experiences: true,
            suggest_rules: true,
            suggest_skills: true,
            auto_activate_rules: false,
            auto_install_skills: false,
            allow_self_code_experiments: false,
            exclude_external_context: true,
            infer_feedback: true,
            skill_candidate_min_successes: 3,
            skill_candidate_min_sessions: 2,
            idle_delay_secs: 300,
            max_background_workers: 1,
            max_context_bytes: 8_000,
            max_daily_tokens: 50_000,
            max_daily_wall_secs: 30 * 60,
            max_project_store_bytes: 100 * 1024 * 1024,
            evolution_profile: String::new(),
            retention: EvolutionRetentionCfg::default(),
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
    /// 基于真实 Episode、证据化 Memory 与人工批准候选的本地自进化配置。
    #[serde(default)]
    pub evolution: EvolutionCfg,
    /// 子 Agent 模型配置（[subagent] 节）
    #[serde(default)]
    pub subagent: SubAgentCfg,
    /// 同角色模型路由与可恢复错误 fallback。
    #[serde(default)]
    pub routing: RoutingCfg,
    /// 国内模型能力、工具参数恢复与 lazy schema 策略。
    #[serde(default)]
    pub model_runtime: ModelRuntimeCfg,
    /// OS sandbox 文件系统、网络和降级策略。
    #[serde(default)]
    pub sandbox: SandboxCfg,
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
    /// 仅运行期存在的 API Key；serde 永不读写，避免环境变量被设置面板落盘。
    #[serde(skip)]
    pub runtime_api_key: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_search_provider() -> String {
    "tavily".to_string()
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        })
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
            evolution: EvolutionCfg::default(),
            subagent: SubAgentCfg::default(),
            routing: RoutingCfg::default(),
            model_runtime: ModelRuntimeCfg::default(),
            sandbox: SandboxCfg::default(),
            computer_use: ComputerUseCfg::default(),
            search_provider: default_search_provider(),
            search_api_key: None,
            runtime_api_key: None,
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
                vendor: None,
                wire_protocol: None,
                model: legacy.model,
                plan_model: legacy.plan_model,
                exec_model: legacy.exec_model,
                base_url: legacy.base_url,
                api_key: legacy.api_key,
                api_key_env: None,
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
            evolution: EvolutionCfg::default(),
            subagent: SubAgentCfg::default(),
            routing: RoutingCfg::default(),
            model_runtime: ModelRuntimeCfg::default(),
            sandbox: SandboxCfg::default(),
            computer_use: ComputerUseCfg::default(),
            search_provider: default_search_provider(),
            search_api_key: None,
            runtime_api_key: None,
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

        // 环境变量只写入 serde 跳过的运行期槽位，绝不能因为打开设置面板并保存
        // 就把 secret 物化进 config.toml。全局兼容变量优先，其次 profile 的
        // `api_key_env` 引用，最后才由 `api_key()` 回退到显式 api_key 字段。
        cfg.runtime_api_key = std::env::var("WYJ_CODE_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
            .or_else(|| {
                cfg.active_profile()
                    .api_key_env
                    .as_deref()
                    .filter(|name| valid_env_name(name))
                    .and_then(|name| std::env::var(name).ok())
                    .filter(|key| !key.is_empty())
            });
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
        self.runtime_api_key
            .as_deref()
            .or_else(|| self.active_profile()
            .api_key
            .as_deref()
            .filter(|k| !k.is_empty()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "未找到 API Key。请设置 WYJ_CODE_API_KEY、profile.api_key_env，或兼容字段 api_key。"
                )
            })
    }

    pub fn redacted_api_key(&self) -> Option<String> {
        self.api_key().ok().map(|key| {
            let tail: String = key
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("••••{tail}")
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

/// 返回当前进程的项目根：显式 override 优先，否则从 `cwd` 向上查找项目清单或
/// Git 仓库根，最后回退到规范化后的 `cwd`。这里只检查文件系统标记，不执行
/// git 命令，避免项目配置发现进入启动性能关键路径。
static PROCESS_PROJECT_ROOT_OVERRIDE: std::sync::OnceLock<std::sync::RwLock<Option<PathBuf>>> =
    std::sync::OnceLock::new();

/// 为当前进程显式指定项目根。CLI 的 `--project-root` 在解析其他项目级资源前
/// 调用；显式值是本进程所有项目级资源的权威身份，允许从项目外目录管理非 Git
/// 项目，同时由独立的 `--cwd` 决定实际工具工作目录。
pub fn set_process_project_root_override(root: Option<PathBuf>) -> Result<()> {
    let normalized = match root {
        Some(path) => {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("项目根不存在或不可访问: {}", path.display()))?;
            if !canonical.is_dir() {
                anyhow::bail!("项目根不是目录: {}", canonical.display());
            }
            Some(canonical)
        }
        None => None,
    };
    *PROCESS_PROJECT_ROOT_OVERRIDE
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
        .unwrap() = normalized;
    Ok(())
}

/// 项目清单目前不承载任何全局可共享的 Memory 字段。如未来需要项目级声明，
/// 应先在 `ProjectManifest` 增字段并在本处提供对应的 reader；目前故意不读取
/// 任何 `[memory]` 段，以避免类似“跨项目共享 workspace”的语义再次悄然落地。
pub fn project_root(cwd: &Path) -> PathBuf {
    let normalized_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let override_root = PROCESS_PROJECT_ROOT_OVERRIDE
        .get_or_init(|| std::sync::RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    resolve_project_root(&normalized_cwd, override_root.as_deref())
}

fn resolve_project_root(normalized_cwd: &Path, override_root: Option<&Path>) -> PathBuf {
    if let Some(root) = override_root {
        return root.to_path_buf();
    }
    let mut dir = Some(normalized_cwd);
    while let Some(candidate) = dir {
        // 非 Git 项目可用一个显式、可提交或本地维护的清单稳定项目身份。
        if candidate.join(".wyj-code/project.toml").is_file() {
            return candidate.to_path_buf();
        }
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
        dir = candidate.parent();
    }
    normalized_cwd.to_path_buf()
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

        let canonical = repo.path().canonicalize().unwrap();
        assert_eq!(project_root(&nested), canonical);
        assert_eq!(project_config_dir(&nested), canonical.join(".wyj-code"));
    }

    #[test]
    fn project_config_dir_falls_back_to_non_git_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(project_root(dir.path()), canonical);
        assert_eq!(project_config_dir(dir.path()), canonical.join(".wyj-code"));
    }

    #[test]
    fn project_manifest_does_not_expose_legacy_workspaces_field() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("analysis/daily");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(dir.path().join(".wyj-code")).unwrap();
        // 即便遗留的旧 project.toml 里写了 [memory].workspaces，新解析路径
        // 也不应暴露 workspace 列表：两层作用域重构后只能拿到空集。
        std::fs::write(
            dir.path().join(".wyj-code/project.toml"),
            "[memory]\nworkspaces = [\"a-share\"]\n",
        )
        .unwrap();

        assert_eq!(project_root(&nested), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn explicit_project_root_is_authoritative_outside_the_working_directory() {
        let project = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let project = project.path().canonicalize().unwrap();
        let unrelated = unrelated.path().canonicalize().unwrap();

        assert_eq!(resolve_project_root(&unrelated, Some(&project)), project);
    }
}

#[cfg(test)]
mod subagent_cfg_tests {
    use super::{
        ComputerUseCfg, Config, EvolutionCfg, ForegroundFallback, Profile, Provider, RoutingRole,
        SubAgentCfg, WireProtocol,
    };

    #[test]
    fn routing_roles_parse_and_cross_provider_defaults_closed() {
        let cfg: Config = toml::from_str(
            r#"
active_profile = "main"

[routing.roles]
execute = ["main", "backup"]
plan = ["planner"]

[[profiles]]
name = "main"
provider = "openai"
model = "main-model"
base_url = "https://example.invalid/v1"
api_key = "placeholder"
max_tokens = 4096
context_window = 32000
"#,
        )
        .unwrap();

        assert!(!cfg.routing.cross_provider_fallback);
        assert_eq!(
            cfg.routing.roles.for_role(RoutingRole::Execute),
            &["main".to_string(), "backup".to_string()]
        );
        assert_eq!(
            cfg.routing.roles.for_role(RoutingRole::Plan),
            &["planner".to_string()]
        );
    }

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
    fn old_profile_without_vendor_or_wire_protocol_remains_compatible() {
        let p: Profile = toml::from_str(
            r#"
name = "legacy"
provider = "openai"
model = "deepseek-chat"
base_url = "https://api.deepseek.com"
max_tokens = 8192
context_window = 64000
"#,
        )
        .unwrap();

        assert_eq!(p.vendor, None);
        assert_eq!(p.wire_protocol, None);
        assert_eq!(
            p.effective_wire_protocol(),
            WireProtocol::OpenAiChatCompletions
        );
    }

    #[test]
    fn explicit_wire_protocol_takes_precedence_over_legacy_provider() {
        let p: Profile = toml::from_str(
            r#"
name = "dual-protocol"
provider = "openai"
vendor = "minimax"
wire_protocol = "anthropic_messages"
model = "MiniMax-M2"
base_url = "https://example.invalid/anthropic"
max_tokens = 8192
context_window = 200000
"#,
        )
        .unwrap();

        assert_eq!(p.vendor.as_deref(), Some("minimax"));
        assert_eq!(p.effective_wire_protocol(), WireProtocol::AnthropicMessages);
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

    #[test]
    fn runtime_and_referenced_secrets_are_not_materialized_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config {
            runtime_api_key: Some("runtime-secret-value".to_string()),
            ..Config::default()
        };
        config.active_profile_mut().api_key_env = Some("MINIMAX_API_KEY".to_string());
        config.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(path).unwrap();
        assert!(!saved.contains("runtime-secret-value"));
        assert!(saved.contains("api_key_env = \"MINIMAX_API_KEY\""));
    }

    #[test]
    fn partial_sandbox_environment_keeps_secret_denies_and_parses_allow_all() {
        let config: Config = toml::from_str(
            r#"
            [sandbox.network]
            allow_all = true

            [sandbox.environment]
            inherit = true
            "#,
        )
        .unwrap();

        assert!(config.sandbox.network.allow_all);
        assert!(config.sandbox.environment.inherit);
        assert!(config
            .sandbox
            .environment
            .deny
            .contains(&"WYJ_CODE_API_KEY".to_string()));
    }

    #[test]
    fn evolution_defaults_keep_high_risk_promotion_manual_and_bounded() {
        let evolution = EvolutionCfg::default();
        assert!(evolution.enabled);
        assert!(evolution.use_experiences);
        assert!(!evolution.auto_activate_rules);
        assert!(!evolution.auto_install_skills);
        assert!(!evolution.allow_self_code_experiments);
        assert!(evolution.exclude_external_context);
        assert_eq!(evolution.skill_candidate_min_successes, 3);
        assert_eq!(evolution.skill_candidate_min_sessions, 2);
        assert_eq!(evolution.idle_delay_secs, 300);
        assert_eq!(evolution.max_background_workers, 1);
        assert_eq!(evolution.max_context_bytes, 8_000);
        assert_eq!(evolution.max_daily_tokens, 50_000);
        assert_eq!(evolution.max_daily_wall_secs, 1_800);
        assert_eq!(evolution.max_project_store_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn partial_evolution_config_inherits_safe_defaults() {
        let config: Config = toml::from_str(
            r#"
            [evolution]
            enabled = false
            "#,
        )
        .unwrap();

        assert!(!config.evolution.enabled);
        assert!(!config.evolution.auto_activate_rules);
        assert!(!config.evolution.auto_install_skills);
        assert!(!config.evolution.allow_self_code_experiments);
        assert!(config.evolution.exclude_external_context);
        assert_eq!(config.evolution.max_background_workers, 1);
    }
}
