//! wyj-code 配置模块
//! 管理 ~/.wyj-code/ 目录下的配置文件与 API Key 读取。

use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── MCP Server 配置 ───────────────────────────────────────────────────────────

/// MCP 服务器传输类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}

/// 单个 MCP server 配置（在 ~/.wyj-code/config.toml 的 [[mcp_servers]] 段声明）
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        }
    }
}

/// [subagent] 节 — 子 Agent 全局模型配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SubAgentCfg {
    /// 子 Agent 默认使用的 Profile 名（留空则沿用主 Agent 当前分组与模型）
    pub default_profile: Option<String>,
    /// 内置 Explore 类型专用 Profile 名（留空则回退 default_profile）
    pub explore_profile: Option<String>,
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
}

fn default_true() -> bool {
    true
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
            }],
            log_level: legacy.log_level,
            language: legacy.language,
            mcp_servers: legacy.mcp_servers,
            auto_memory_enabled: true,
            subagent: SubAgentCfg::default(),
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
    /// 加载配置：先读文件（含旧格式一次性迁移），再用环境变量覆盖激活分组的 api_key。
    pub fn load() -> Result<Self> {
        let config_path = config_file_path()?;
        let mut cfg: Config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
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
                migrated.save().context("迁移旧版配置文件失败")?;
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

        // 环境变量优先，覆盖到激活分组
        if let Ok(key) = std::env::var("WYJ_CODE_API_KEY") {
            if !key.is_empty() {
                cfg.active_profile_mut().api_key = Some(key);
            }
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
        let config_path = config_file_path()?;
        let content = toml::to_string_pretty(self).context("序列化配置失败")?;
        std::fs::write(&config_path, content)
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))
    }
}

/// 返回配置目录路径（~/.wyj-code），若不存在则创建。
pub fn config_dir() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().ok_or_else(|| anyhow::anyhow!("无法获取用户主目录"))?;
    let dir = user_dirs.home_dir().join(".wyj-code");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        tracing::info!("初始化配置目录: {}", dir.display());
    }
    Ok(dir)
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
