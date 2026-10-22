//! wyj-code 配置模块
//! 管理 ~/.wyj-code/ 目录下的配置文件与 API Key 读取。

use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod filelock;
pub mod gitignore;
pub mod layout;
pub mod project_mcp;
pub mod project_settings;
pub use project_mcp::{
    load_project_mcp, merged_mcp_servers, project_mcp_path, save_project_mcp, ProjectMcpConfig,
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
    /// Auto 模式（唯一需要判定的日常模式，也是默认）：不由人逐次点确认，
    /// 而是由 `wyj_core::auto_mode::AutoGate` 判定——确定性规则先行，拿不准的
    /// 送模型分类器，闸门自己也拿不准（分类器不可用 / 超阈值降级）时交回
    /// 逐次弹窗。Shift+Tab 在 Auto / Plan / Bypass 三档间循环。
    #[default]
    Auto,
    /// Plan 模式：允许只读工具与受限规划文档写入；执行层仍按路径和命令复核。
    Plan,
    /// Bypass 模式：跳过普通交互确认，但不覆盖 protected deny 或 OS sandbox。
    Bypass,
}

impl AgentMode {
    pub fn label(&self) -> &'static str {
        match self {
            AgentMode::Auto => "auto",
            AgentMode::Plan => "plan",
            AgentMode::Bypass => "bypass",
        }
    }

    /// 解析用户输入的模式名。
    ///
    /// `manual` / `default` / `normal` 都是 `auto` 的**向后兼容别名**，不再
    /// 指向独立模式：旧的配置文件与脚本里写着 `manual` 仍能加载，但语义已经
    /// 并入 auto（弹窗改为闸门判定后才出现），解析成 `None` 会让用户的旧
    /// 配置直接失效，那比留一个别名糟得多。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "manual" | "default" | "normal" => Some(AgentMode::Auto),
            "plan" => Some(AgentMode::Plan),
            "bypass" | "bypasspermissions" | "bypass_permissions" => Some(AgentMode::Bypass),
            _ => None,
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
    /// 默认模型名称
    pub model: String,
    /// Plan 模式专用模型（留空则使用 model）
    #[serde(default)]
    pub plan_model: Option<String>,
    /// Exec/Bypass 模式专用模型（留空则使用 model）
    #[serde(default)]
    pub exec_model: Option<String>,
    /// 辅助模型（留空则辅助调用也用 model）。
    ///
    /// 只作用于**短提示词、低风险**的辅助调用：会话标题生成、跨会话记忆提取。
    /// 这类请求不复用主对话的 system prompt 前缀，因此换模型不会让主会话的
    /// prompt cache 失效。
    ///
    /// **不作用于上下文压缩（compact）**：压缩请求逐字节复用主对话的 system +
    /// tools 前缀来命中缓存，换模型等于前缀不等 → 缓存归零 → 整段历史全价重发。
    ///
    /// 必须与本分组同 vendor、同端点（共用同一张 API Key）。辅助模型会收到
    /// 对话内容片段，跨供应商路由等于把这些内容发给另一家服务商。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux_model: Option<String>,
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
    /// `reasoning_effort` 档位（low/medium/high/max/xhigh）。
    ///
    /// **协议无关**：OpenAI 线发顶层 `reasoning_effort` 字段，Anthropic 线
    /// 发 `output_config.effort`（官方 Anthropic 与 MiniMax `/anthropic` 端点
    /// 同构）——同一份 profile 换协议不必改配置。
    ///
    /// 与 `thinking_budget` 的关系按端点而定：多数端点两者互斥（Qwen 协议
    /// 明确禁止同传），官方 Anthropic 则**可以同时用**——budget 限**思考**的
    /// token 数，effort 限**整个响应**的投入档位，两者控制不同维度。
    ///
    /// 档位越界（端点不认该档）会被记成可见的 `dropped_parameter`，不会发出去
    /// 换 400。env var `WYJ_CODE_THINKING_EFFORT` 优先（runtime override，不写盘）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// OpenAI-vendor `thinking.type` 字符串显式覆盖（enabled/disabled/auto/adaptive）。
    /// None = 让 adapter 按模型默认决定；Some 强制覆盖（含 MiniMax M2.x 等
    /// vendor 默认 adaptive 也允许显式关）。env var `WYJ_CODE_THINKING_SWITCH` 优先。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_switch: Option<String>,
    /// 每次回复最多并行几个工具调用。None = 由模型目录/探测结果决定；
    /// Some(n) 强制覆盖（含 Some(1) 显式退回串行）。env var
    /// `WYJ_CODE_MAX_TOOLS_PER_TURN` 优先（runtime override，不写盘）。
    ///
    /// 静态目录对所有非 anthropic/openai 端点一律判定"不支持并发工具"，
    /// 并往 system prompt 注入"每次回复最多调用一个工具"。该判定按 vendor
    /// 猜测而非实测，MiniMax/GLM/Kimi 等实际能并发的端点会被静默锁死成
    /// 每轮一次往返。此字段是它的逃生舱：`wyj-code model doctor --probe full`
    /// 探针通过后即可在此显式放开。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tools_per_turn: Option<usize>,
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
            aux_model: None,
            base_url: String::new(),
            api_key: None,
            api_key_env: None,
            max_tokens: 32_000,
            context_window: 200_000,
            vision: true,
            thinking_budget: None,
            interleaved_thinking: true,
            prompt_cache: None,
            openai_stream_options: None,
            reasoning_effort: None,
            thinking_switch: None,
            max_tools_per_turn: None,
        }
    }
}

impl Profile {
    /// 端点实际使用的线协议。**由 `provider` 唯一决定**。
    ///
    /// 此前这里还有一层 `Option<WireProtocol>` 覆盖（用户可显式写
    /// `wire_protocol = "..."`），现已随该字段一并移除——线协议是"用哪种
    /// 请求格式说话"，与 `provider` 是同一个决定，写两遍只会打架。
    pub fn effective_wire_protocol(&self) -> WireProtocol {
        match self.provider {
            Provider::Anthropic => WireProtocol::AnthropicMessages,
            Provider::OpenAI => WireProtocol::OpenAiChatCompletions,
        }
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

    /// 有效 `reasoning_effort`：env var `WYJ_CODE_THINKING_EFFORT` 优先，否则 profile。
    /// 返回 owned String 以避免静态借用泄漏；调用方（agent 构造 opts 时）按需 clone 一次。
    pub fn effective_reasoning_effort(&self) -> Option<String> {
        match std::env::var("WYJ_CODE_THINKING_EFFORT")
            .ok()
            .filter(|value| !value.is_empty())
        {
            Some(value) => Some(value),
            None => self.reasoning_effort.clone(),
        }
    }

    /// 有效 `thinking_switch`：env var `WYJ_CODE_THINKING_SWITCH` 优先，否则 profile。
    pub fn effective_thinking_switch(&self) -> Option<String> {
        match std::env::var("WYJ_CODE_THINKING_SWITCH")
            .ok()
            .filter(|value| !value.is_empty())
        {
            Some(value) => Some(value),
            None => self.thinking_switch.clone(),
        }
    }
    /// 有效 `max_tools_per_turn`：env var `WYJ_CODE_MAX_TOOLS_PER_TURN` 优先，否则 profile。
    /// 非法值（0 / 非数字）视为未设置并回落到 profile，不让一次手滑的 env 把
    /// 能力关掉——`0` 若被当成"关并行"会和 `Some(1)` 语义混淆，宁可什么都不做。
    pub fn effective_max_tools_per_turn(&self) -> Option<usize> {
        match std::env::var("WYJ_CODE_MAX_TOOLS_PER_TURN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
        {
            Some(value) => Some(value),
            None => self.max_tools_per_turn,
        }
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
    /// 子 Agent 的最大嵌套层数（主 Agent 之下算第一层）。1 = 禁止子 Agent 再派子
    /// Agent。默认 3。
    ///
    /// 触顶时 `Agent` 工具会从该层子 Agent 的工具集里**消失**（而不是留在原地
    /// 每次调用都报错）——模型看到工具没了才会自己把活干完，否则它会换个措辞
    /// 反复重试。
    pub max_depth: usize,
    /// 同时运行的子 Agent 上限（含本次会话内所有根级子 Agent）。默认 8。
    /// 超限的 spawn 在信号量上排队等待，不会失败。
    pub max_concurrent: usize,
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
            max_depth: 3,
            max_concurrent: 8,
        }
    }
}

/// 旧版全局 `computer` 工具需要占用前台鼠标/键盘时的回退策略。
///
/// v0.2.5 起后台 `app_computer` 是默认路径；前台接管必须显式配置，不能在
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
    /// 变更类动作成功后是否自动附带一张目标窗口截图。
    ///
    /// 这是「动作 → 观察」闭环的关键：`app_computer` 的动作只回一句文本，
    /// 模型必须自己再调一次 `screenshot` 才知道界面对不对，而它经常不会——
    /// 于是拿着过时的界面状态继续往下点。自动附图把这次往返省掉。
    ///
    /// 默认开启。代价是每次变更动作多约 1000~1600 input token（一张 960
    /// 长边的 PNG），长任务里累积可观，可用本开关关掉。
    pub auto_screenshot: bool,
    /// 自动附图的长边像素上限。刻意低于观察用的 `max_dim`（1280）：
    /// 附图只是「确认界面变了没有」，不需要看清小字，需要看清时模型会
    /// 显式调 `screenshot` 或 `zoom`。
    pub auto_screenshot_max_dim: u32,
}

/// `[auto_mode]` 节 — Auto 权限模式的判定策略（对齐 Claude Code auto mode）。
///
/// 字段全部是**可选**的：不写这一节时 auto 模式仍然可用，用的是内置默认值与
/// 内置规则集（`wyj_core::auto_rules`）。这个结构只承载"用户想改什么"。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoModeCfg {
    /// 模型分类器总开关。关掉后只剩确定性规则层，拿不准的操作一律弹窗——
    /// 这与「分类器请求失败」是两件事：这里是用户主动选的保守模式。
    pub enabled: bool,
    /// 分类器使用的 profile 名。留空 = 复用当前激活的 profile；填了就用
    /// `profiles` 里的同名分组单独跑（适合用小模型降本）。
    pub classifier_profile: String,
    /// 分类器单次请求超时（秒）。超时按 fail-closed 判拒绝。
    ///
    /// 默认 30 而非更小的值：分类器面对的端点可能是 adaptive thinking 模型，
    /// 思考本身就要好几秒（2026-10 对 MiniMax-M3.1-Flash-Preview 实测典型
    /// 3.4–5.5s，叠加网络抖动会逼近 15s）。超时与"判定解析失败"是同一个
    /// fail-closed 出口，用户看到的都是"命令被莫名拒绝"，排查时容易只盯着
    /// 预算而忽略这里。
    pub timeout_secs: u64,
    /// 连续被拒达到该次数后，auto 自动降级为逐次人工确认。
    pub max_consecutive_blocks: u32,
    /// 整个会话累计被拒达到该次数后同样降级。
    pub max_total_blocks: u32,
    /// true 时所有 shell 命令一律送审，不再走确定性 allow 白名单（更安全更慢）。
    pub classify_all_shell: bool,
    /// 分类器规则文本（英文自然语言规则，非正则）。数组中出现字面量
    /// `"$defaults"` 就在该位置插入内置规则；不写则整段替换内置规则。
    pub hard_deny: Vec<String>,
    pub soft_deny: Vec<String>,
    pub allow: Vec<String>,
    pub environment: Vec<String>,
}

impl Default for AutoModeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            classifier_profile: String::new(),
            timeout_secs: 30,
            max_consecutive_blocks: 3,
            max_total_blocks: 20,
            classify_all_shell: false,
            hard_deny: Vec::new(),
            soft_deny: Vec::new(),
            allow: Vec::new(),
            environment: Vec::new(),
        }
    }
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
    /// 工具搜索命中的**排序加权**窗口（回合数），不再决定工具是否可见。
    ///
    /// 语义在 v0.3.14 收窄过：旧实现用同一个计时器管「可见性」，到期就把
    /// 工具从请求里拿掉——而 Anthropic 的缓存前缀物理顺序是
    /// `tools → system → messages`，官方明确「会话中途增删 tools 会让 tools
    /// 缓存及其后的 system、messages 全部失效」。于是每 N 个回合就作废一次
    /// 整段 prompt cache，代价远大于省下的那点 schema token。现在可见性
    /// **单调不减**（`LazyToolState::exposed`），本字段只管「刚用过的工具在
    /// 搜索结果里排得更前」。
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

/// tool-result context editing：超阈值时先把过期工具输出外部化到 CAS，
/// 清理得动就**不**去跑整段摘要。对齐 Claude Code 的 "It clears older tool
/// outputs first, then summarizes the conversation if needed."
///
/// 全部字段 opt-out：`enabled = false` 完全退回 v0.2.25 的行为（只压缩、只统计
/// 可回收量、不真删）。详见 `doc/plan/v0.2.26-context-management-plan.md`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextEditCfg {
    pub enabled: bool,
    /// 最近 N 个工具结果保留全文（Anthropic `clear_tool_uses_20250919` 的 `keep`）。
    pub protect_recent: usize,
    /// 单批清理条数。每批都会改写 prompt cache 前缀，批太大浪费命中率。
    pub batch_size: usize,
    /// 低于此字节数的结果不清理：省下的 token 还不够一条占位符本身。
    pub min_result_bytes: usize,
    /// 单次请求最多连续清理几批（防"历史里全是可清理项"时一次做掉一大手术）。
    pub max_batches: usize,
    /// **软阈值**：上下文占用超过窗口的这个比例就开始外部化过期工具结果，
    /// 而不是等到撞上压缩硬阈值才动手。`0.0` = 关闭，完全退回「只在硬阈值清理」。
    ///
    /// 为什么需要它：context editing 原本只在 `estimate > compact_threshold`
    /// 时运行，而 `compact_threshold = window - compact_trigger_buffer`。1M 窗口
    /// 下这条线在 900K，于是「实际会话长到 300K 就该减负」这件事永远不发生
    /// （实测 129 个会话的 `context_edit_freed_tokens` 全为 0）。
    ///
    /// 提前动手的收益是乘法的：外部化越早，每趟 API 携带的历史越小、累计
    /// input 越低；而代价很小——外部化是无损的（原文进 CAS，`ContextRecall`
    /// 能召回），且便宜（无 output token、不幻觉），本就在压缩之前的优先路径上。
    ///
    /// 软阈值**不改变**最终收敛目标：清理到硬阈值以下才停；清理不动了仍然
    /// 交给 compact 摘要。
    pub soft_limit_ratio: f64,
}

impl Default for ContextEditCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            protect_recent: 3,
            batch_size: 10,
            min_result_bytes: 2_000,
            max_batches: 3,
            soft_limit_ratio: 0.55,
        }
    }
}

/// `[agent]` 节 —— Agent 推理循环本身的参数。
///
/// 与 `[tools]` 分开的原因：那一节管的是「哪些工具存在」（未注册 = 模型目录里
/// 根本看不到），而这里管的是「一次用户提交内最多能跑多少个工具轮」，两者被
/// 同一个开关混在一起时，「关掉某个工具」和「限制循环长度」会互相误伤。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCfg {
    /// 单次用户提交内允许的最大推理轮数（一次 LLM 往返 = 一轮）。
    ///
    /// **默认 200**。它的设计意图是「防模型死循环」，不是「给正常长任务设工期」，
    /// 所以真正触发它的场景极少——本地实测里唯一一次触顶（2026-10-15）翻出的
    /// 200 轮全是有效工作：143 次 Edit、85 次 Bash、81 次 Read，错误率 6%，
    /// 没有一轮空转。也就是说触顶时工作树里已经躺着大半成果，却因为拿不到
    /// 一次总结而被判成失败。
    ///
    /// 因此触顶前另有 `TURN_LIMIT_GRACE` 轮收尾宽限（见 `core::agent`），到点
    /// 会先要求模型停止调用工具、立即交付阶段成果；本值只决定那道宽限从第几轮
    /// 开始。调高它等于给「超长任务」更多预算，调低它限制的是**单次提交**的
    /// 最长耗时，两者都不影响死循环保护——那由 `loop_guard` 独立承担。
    ///
    /// 推理模型（DeepSeek `deepseek-reasoner` 等）另有更保守的 32 轮熔断，
    /// 不受本值影响。**思考关不掉的端点同样如此**——判据走
    /// `ThinkingSpec::for_vendor`，覆盖 MiniMax M2.x/M3.1、Qwen `-thinking`、
    /// 豆包 `-thinking`、Moonshot `-code`。理由：它们的每一轮都是一次完整
    /// 深度推理，实测一条 MiniMax 会话单回合跑了 163 轮、墙钟 30 分钟以上。
    pub max_turns: usize,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self { max_turns: 200 }
    }
}

/// `[tools]` 节 —— 工具注册的门控开关。
///
/// 未注册的工具不产生 schema，模型目录里根本看不到它，因此这里的开关等价于
/// "该工具是否存在"，而不是运行期的软禁用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsCfg {
    /// 是否注册 `TodoWrite` 任务列表工具。
    ///
    /// **默认开启**（v0.3.1 起由关闭改为开启）。这里保留一段反向实测，
    /// 因为它是"为什么曾经关闭"的唯一记录，也是本开关存在的理由：
    ///
    /// - 曾对齐 Claude Code v2.1.233 / Codex CLI v0.152.0 的默认关闭口径
    ///   （"新模型不需要书面任务列表也能自行跟踪多步工作"）。
    /// - 本地 102 个历史会话实测支持那个口径：27 个会话用到 TodoWrite、共 132
    ///   次调用，其中 **132 次（100%）独占一个完整 LLM 往返**，没有一次与真实
    ///   工作工具同轮发出；**59% 的会话结束时 todo 未收尾**，与 arXiv:2604.12147
    ///   的结论一致——"a subpar plan hurts performance even more than no plan at
    ///   all"。
    ///
    /// 那么为什么还是改成了默认开启：上面那份实测只覆盖了**强模型的短任务**，
    /// 它的结论形态是"对强模型是净成本"。而本项目要扛的复杂任务形态恰恰是
    /// 另一个区间——跨十几个文件、几十轮工具调用、中途还会被自动压缩打断的长
    /// 任务。那种任务里模型**没有外部状态锚点**：压缩一发生，它不知道自己走过
    /// 哪条路，实测退化路径是重新 grep 刚 grep 过的目录、重新 read 刚读过的
    /// 文件，把刚省下的上下文又吃回去。TodoWrite 是目前唯一一条能穿透压缩的
    /// 显式进度载体——它每轮被重放进上下文，压缩后仍在。
    ///
    /// 代价是确定的：**每个用了 TodoWrite 的会话多花约 1 个 LLM 往返**
    /// （132/27 ≈ 4.9 次调用，但按会话折算首次那一次不可省）。所以开关保留：
    /// 简单任务为主、或用强模型跑短任务的场景，`todo_enabled = false` 仍然
    /// 更划算。
    pub todo_enabled: bool,
}

impl Default for ToolsCfg {
    fn default() -> Self {
        Self { todo_enabled: true }
    }
}

/// `[search]` 节 —— WebSearch 工具的后端与请求默认值。
///
/// 为什么从顶层扁平字段收拢成子节：`search_provider` / `search_api_key` 曾是两个
/// 顶层字段，其中 `search_provider` **全仓没有任何读取点**——用户改了它毫无效果，
/// 是典型的"配置项幻象"（配置存在、UI 能编辑、代码从不消费）。改成子节后有
/// [`Config::search_provider_issue`] 这个单一判定入口，选到不认识的 provider 会
/// 显式报错而不是静默回退。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchCfg {
    /// 搜索后端：`"tavily"`（默认）或 `"brave"`。
    pub provider: String,
    /// 后端 API Key。优先读环境变量 `WYJ_CODE_SEARCH_API_KEY`（见
    /// [`Config::search_api_key`]）；未配置时 WebSearch 工具不注册，模型看不到它。
    pub api_key: Option<String>,
    /// 单次搜索请求超时（秒）。
    pub timeout_secs: u64,
    /// 模型未显式指定 `max_results` 时使用的条数。
    pub max_results: u32,
    /// 默认白名单域名（模型可用 `include_domains` 覆盖）。
    pub allowed_domains: Vec<String>,
    /// 默认黑名单域名（模型可用 `exclude_domains` 覆盖）。
    pub blocked_domains: Vec<String>,
    /// Brave 的 `search_lang`（ISO 639-1）。空串 = 不指定，用后端默认值。
    pub language: String,
    /// 是否开启安全搜索（Brave 映射为 `safesearch=strict`）。
    pub safe_search: bool,
}

impl Default for SearchCfg {
    fn default() -> Self {
        Self {
            provider: "tavily".to_string(),
            api_key: None,
            timeout_secs: 30,
            max_results: 5,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            language: String::new(),
            safe_search: true,
        }
    }
}

impl SearchCfg {
    /// 已知后端列表，供错误信息与诊断输出使用。
    pub const KNOWN_PROVIDERS: &'static [&'static str] = &["tavily", "brave"];
}

/// 鼠标报告（DEC 1000/1002/1006）的开关策略。
///
/// alternate screen 里终端自己的滚动条不存在，滚轮能否滚动完全取决于应用是否
/// 收到鼠标事件；而 macOS Terminal.app 不支持 DEC mode 1007（alternate screen
/// 滚轮转方向键），所以**不开捕获时 Terminal.app 的滚轮事件根本不会送达应用**。
/// 应用因此把滚轮、拖选、双击选词、滚动条拖动全部自己实现（见 `crates/tui`），
/// 让「滚轮 + 直接拖选复制」同时成立——这是本枚举默认 `Auto` 的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseMode {
    /// 默认：按终端能力自动决定。只有 `TERM` 缺失、空串或 `dumb` 这类明确不支持
    /// 鼠标上报的环境才关闭，其余一律开启。
    #[default]
    Auto,
    /// 强制开启：滚轮与拖选都进应用。
    On,
    /// 强制关闭：完全交还终端。终端原生拖选恢复（无需按 Option），代价是
    /// Terminal.app 下滚轮不再滚动内容区（只有支持 DEC 1007 的终端仍可用）。
    Off,
}

impl MouseMode {
    /// 给定 `TERM` 值判定最终是否开启鼠标报告。纯函数，便于钉住自动检测规则。
    pub fn resolve(self, term: Option<&str>) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => !matches!(term.map(str::trim), None | Some("") | Some("dumb")),
        }
    }

    /// 按当前进程环境判定。
    pub fn detect(self) -> bool {
        self.resolve(std::env::var("TERM").ok().as_deref())
    }
}

/// `[ui]` 节 —— TUI 展示区的交互开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiCfg {
    /// 鼠标报告策略（`"auto"` / `"on"` / `"off"`），默认 `auto`。
    ///
    /// 应用自己实现滚轮滚动、拖选复制与滚动条拖动，所以开启捕获不再意味着
    /// 「失去拖选」——`off` 只留给「想用终端原生选区与自身滚动缓冲」的场景。
    pub mouse: MouseMode,
    /// 已废弃：v0.3.18 前的布尔开关，`true` 等价 `mouse = "on"`、`false` 等价
    /// `mouse = "off"`。仅为旧配置文件保留，不参与写回。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_capture: Option<bool>,
    /// 是否在内容区右侧绘制应用内细滚动条（`│`/`┃`，less/vim 观感）。
    ///
    /// 只在内容超出视口时出现，宽度 < 3 列的终端不画；出现后可用鼠标拖动。
    pub scrollbar: bool,
}

impl UiCfg {
    /// 本次运行实际生效的鼠标报告开关：旧字段显式写过就尊重它，否则按 `mouse`
    /// 三态判定（`auto` 走终端能力检测）。
    pub fn resolved_mouse(&self) -> bool {
        match (self.mouse, self.mouse_capture) {
            (MouseMode::Auto, Some(legacy)) => legacy,
            (mode, _) => mode.detect(),
        }
    }
}

impl Default for UiCfg {
    fn default() -> Self {
        Self {
            mouse: MouseMode::Auto,
            mouse_capture: None,
            scrollbar: true,
        }
    }
}

/// `crates/sandbox/` 已删除，下方所有 `Sandbox*Cfg` 字段仍保留只为兼容
/// `~/.wyj-code/config.toml` 历史配置文件（旧 block 不会被 serde 报错），
/// 运行时整块忽略，不再映射到任何 ToolCtx 状态。读取后会在 Config::load
/// 里检测到非默认值时打印一次 deprecation warning。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SandboxFilesystemCfg {
    pub allow_read: Vec<PathBuf>,
    pub allow_write: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    pub deny_write: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SandboxNetworkCfg {
    /// 已废弃：保留只为 `config.toml` 兼容。
    pub allow_all: bool,
    pub allowed_domains: Vec<String>,
    pub allow_local_binding: bool,
    pub allow_unix_sockets: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
            auto_screenshot: true,
            auto_screenshot_max_dim: 960,
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

/// 持久化前的 `ContentBlock` 字节上限。设为 0 关闭对应字段截断。
///
/// `SessionStore::save` / `CheckpointStore::create` 都会在落盘前调用
/// `wyj_core::serialize::truncate_session_for_persistence`,按本配置对
/// `ToolResult` 文本、`Thinking`、`ToolUse.input`、`Text` 等做 UTF-8 安全的
/// 截断。**同样作用于 `Agent::run_turn_with_injection_inner` 的运行时
/// LLM 发送路径与 `SessionStore::load` 的 resume 路径**,确保 disk /
/// runtime / resume 三条路径共用同一上限——避免长 `ContentBlock::Text`
/// (assistant 总结 / 用户粘贴日志) 在内存里无界累积、撞穿模型上下文窗口。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistCapCfg {
    /// `ToolResult` 文本部分(head)字节上限。0 = 不截断。
    pub tool_result_head_bytes: usize,
    /// `ToolResult` 文本部分(tail)字节上限。0 = 不截断。
    pub tool_result_tail_bytes: usize,
    /// **已废弃，不再生效**（保留字段仅为兼容既有 `config.toml`）。
    ///
    /// 原先按此上限截断 `Thinking.thinking`，但 Anthropic 协议与 MiniMax 都
    /// 要求工具调用续轮时把 thinking 块**原样**回传：截断文本却保留
    /// `signature` 会让签名校验失配，且该改写不可逆。思考体积改由
    /// context_edit / compact 控制。设 0 与设任意值行为一致。
    pub thinking_bytes: usize,
    /// **已废弃，不再生效**（保留字段仅为兼容既有 `config.toml`）。
    /// 理由同上；额外考虑是原实现会改 `reasoning_details[].text` 却保留
    /// `id` / `format`，产出结构不自洽的块。
    pub reasoning_details_bytes: usize,
    /// `ToolUse.input` JSON 字符串字节上限。0 = 不截断。
    pub tool_use_input_bytes: usize,
    /// `ContentBlock::Text` (user / assistant 纯文本) head 字节上限。
    /// 0 = 不截断。长 assistant 总结、用户粘贴日志在 `Text` 块里无界累积
    /// 会让单次请求撞穿模型上下文窗口;runtime / resume / save 三条路径
    /// 都按此上限收敛。
    pub text_head_bytes: usize,
    /// `ContentBlock::Text` tail 字节上限。0 = 不截断。
    pub text_tail_bytes: usize,
}

impl Default for PersistCapCfg {
    fn default() -> Self {
        Self {
            tool_result_head_bytes: 20 * 1024,
            tool_result_tail_bytes: 10 * 1024,
            // thinking / reasoning_details 不再截断（见字段注释）：截断会让
            // thinking 文本与 signature 失配，破坏工具调用续轮。默认值取 0
            // 以免让读者以为该项仍然生效。
            thinking_bytes: 0,
            reasoning_details_bytes: 0,
            tool_use_input_bytes: 64 * 1024,
            // Text 信息密度低于 tool_result (常含 markdown / 长 reasoning 解释),
            // 默认 head+tail 24K 比 tool_result 30K 更紧;长上下文先撞上限,
            // 触发 compact,而不是先撞死模型上下文窗口。reasoning 模型一轮
            // 可能回得更多,故保留 head 16K 比 thinking 8K 宽。
            text_head_bytes: 16 * 1024,
            text_tail_bytes: 8 * 1024,
        }
    }
}

/// `~/.wyj-code` 各子系统 retention / cap / rotation 配置。
///
/// 全部字段都是 opt-out:设为 0 即关闭对应清理逻辑,保持旧行为不变。
///
/// 默认值在 `Default for StorageRetentionCfg` 里集中声明,CLAUDE.md 的
/// "Storage caps (defaults)" 表格与之一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageRetentionCfg {
    /// 单个 session 的 checkpoint 上限(0 = 不限,旧行为)。
    pub checkpoints_per_session: usize,

    /// Memory v2 `.md` 文件每个 kind(feedback/tool/...)上限(0 = 不限)。
    pub memory_v2_md_per_kind: usize,

    /// Memory v3 `records.json` 单文件条目上限(0 = 不限)。Superseded
    /// 状态永远保留,只对 Active/Pending 做截断。
    pub memory_v3_records_max: usize,

    /// Memory v3 `jobs.json` pending 队列上限(0 = 不限)。
    pub memory_v3_jobs_max: usize,

    /// Memory v3 `rejected_history.json` 保留条数(0 = 不限)。
    pub memory_v3_rejected_history_max: usize,

    /// 调度任务日志每个 task 保留文件数(0 = 不限)。
    pub schedule_logs_per_task: usize,

    /// 共享 `run.log` rotate 触发阈值(0 = 不限)。
    pub schedule_run_log_size_bytes: u64,

    /// 共享 `run.log` rotate 副本数。
    pub schedule_run_log_rotations: usize,

    /// 插件 `.git` 做 `git gc --prune=now` 的间隔天数(0 = 关闭)。
    pub plugin_gc_interval_days: u32,

    /// workspace worktree 自动 prune 的过期天数(0 = 仅 dispose 时清理)。
    pub workspace_worktree_max_age_days: u32,

    /// 启动期 `disk_usage::warn_if_over_budget` 阈值(0 = 关闭)。
    pub disk_usage_warn_bytes: u64,

    /// CAS blob pool 单 blob 字节上限(0 = 关闭 CAS,沿用旧 inline 行为)。
    /// 超此值的文件不进 CAS,直接 inline 进 checkpoint JSON。
    pub cas_max_blob_bytes: u64,

    /// CAS blob pool 总量上限(字节,0 = 不做容量回收)。
    ///
    /// v0.2.26 重新加回：此前它因"无消费点"被删，但 `core::context_edit` 现在
    /// 会把过期工具输出外部化进 CAS —— `WorkspaceCas::gc` 的**生产调用点**
    /// 就是这里(进程启动时跑一次)。`gc` 只回收 `ref_count == 0` 的 blob，
    /// 所以依赖会话结束时逐个 `release`；两个机制缺一都会让 pool 无界增长。
    pub cas_total_bytes: u64,

    /// 是否在进程启动时跑一次 `WorkspaceCas::gc(cas_total_bytes)`。
    /// 关掉可省掉启动期一次全池目录遍历（blob 多时是毫秒级 IO）。
    pub cas_gc_on_start: bool,

    /// 会话文件保留天数（0 = 永不自动删除，旧行为）。
    ///
    /// 对应 Claude Code 的 `cleanupPeriodDays`。只删**早于**该天数的会话及其
    /// checkpoint / subagent 旁路目录；最近一条会话永远保留，避免把「唯一一次
    /// 会话」删掉。清理在进程启动时跑一次（与 CAS gc 同批），不阻塞主流程。
    pub sessions_ttl_days: u32,

    /// 会话清理时额外保留的最近条数（按项目计）。0 = 不额外保留。
    ///
    /// TTL 单独用会有一个陷阱：一个项目可能三个月没动过，但里面的会话正是
    /// 用户想回头看的。给每个项目留 N 条最新的，删掉的只是「又老又不新」的。
    pub sessions_keep_recent: usize,

    /// 陈旧锁文件保留天数（0 = 不清理）。
    ///
    /// 覆盖 `cas/locks/` 与 `sessions/*.json.lock` / `*.json.owner`。
    /// 这些文件在 unix 上由内核释放 flock，但**文件本身**不会被删除——实测
    /// `cas/locks/` 攒到 10641 个 / 42 MB，`sessions/` 下 238 个 `.lock` 与
    /// 65 个 `.owner` 残留。只删「mtime 超过 N 天且无人持有」的。
    pub stale_lock_ttl_days: u32,

    /// 运行状态的落盘范围：`"home"`（默认）或 `"project"`。
    ///
    /// `"project"` 把会话与历史改放到 `<git-root>/.wyj-code/local/`，让状态
    /// 跟着工作副本走；选它时会自动写 gitignore 防止 `git add -A` 泄漏
    /// transcript。代价见 [`layout::StorageScope`] 的文档。
    ///
    /// 用字符串而不是枚举：配置是 TOML，未知值应当**降级为默认并告警**，
    /// 而不是让整个 `config.toml` 解析失败。
    #[serde(default = "default_storage_scope")]
    pub scope: String,
    // 曾经存在的 2 个 Phase 4 字段(`checkpoint_bytes_per_session` /
    // `checkpoint_ttl_days`)仍无消费点,保持删除:留着只会让人误以为存在
    // 对应的清理逻辑。`#[serde(default)]` + 无 `deny_unknown_fields`,
    // 用户既有配置里写了这些键会被静默忽略,不影响加载。
}

fn default_storage_scope() -> String {
    "home".to_string()
}

impl Default for StorageRetentionCfg {
    fn default() -> Self {
        Self {
            checkpoints_per_session: 20,
            memory_v2_md_per_kind: 200,
            memory_v3_records_max: 5_000,
            memory_v3_jobs_max: 32,
            memory_v3_rejected_history_max: 500,
            schedule_logs_per_task: 50,
            schedule_run_log_size_bytes: 10 * 1024 * 1024,
            schedule_run_log_rotations: 3,
            plugin_gc_interval_days: 7,
            workspace_worktree_max_age_days: 30,
            disk_usage_warn_bytes: 5 * 1024 * 1024 * 1024,
            cas_max_blob_bytes: 16 * 1024 * 1024,
            // 1 GiB。context editing 每次清理写入的是"当时在上下文里"的内容，
            // 单个会话可控；跨会话累积靠 gc 在启动时按 LRU 收口。
            cas_total_bytes: 1024 * 1024 * 1024,
            cas_gc_on_start: true,
            // **默认关闭**。自动删历史会话是不可逆操作，不该在升级后的第一次
            // 启动里静默发生——用户会以为「历史丢了」。想开的话自己设成 30/90。
            // 想立刻清理一次走 `wyj-code storage prune --yes`（它读同一组配置）。
            sessions_ttl_days: 0,
            sessions_keep_recent: 5,
            // 锁文件是纯垃圾（内容只有 pid），默认清理没有数据风险。
            stale_lock_ttl_days: 7,
            scope: default_storage_scope(),
        }
    }
}

/// 本地、自包含的 Agent 经验闭环。v0.2.14 收敛后只剩 Rule/Skill 治理；
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
    /// tool-result context editing（外部化过期工具输出优先于整段摘要）。
    /// 设为 `enabled = false` 完全退回 v0.2.25 行为。
    pub context_edit: ContextEditCfg,
    /// OS sandbox 文件系统、网络和降级策略。
    #[serde(default)]
    pub sandbox: SandboxCfg,
    /// computer-use 后台优先与前台回退策略（[computer_use] 节）
    #[serde(default)]
    pub computer_use: ComputerUseCfg,
    /// Auto 权限模式的判定策略（[auto_mode] 节）
    #[serde(default)]
    pub auto_mode: AutoModeCfg,
    /// Agent 推理循环参数（[agent] 节）
    #[serde(default)]
    pub agent: AgentCfg,
    /// 工具注册门控（[tools] 节）
    #[serde(default)]
    pub tools: ToolsCfg,
    /// TUI 展示区交互开关（[ui] 节）
    #[serde(default)]
    pub ui: UiCfg,
    /// WebSearch 后端与请求默认值（[search] 节）
    #[serde(default)]
    pub search: SearchCfg,
    /// 仅运行期存在的搜索 API Key；serde 永不读写。与 `runtime_api_key` 同理：
    /// 环境变量覆盖必须落在 `#[serde(skip)]` 槽位上，否则用户打开设置面板保存时
    /// 会把 key 物化进 config.toml。
    #[serde(skip)]
    pub runtime_search_api_key: Option<String>,
    /// 仅运行期存在的 API Key；serde 永不读写，避免环境变量被设置面板落盘。
    #[serde(skip)]
    pub runtime_api_key: Option<String>,
    /// 仅运行期存在的**会话级思考档位**（`/effort` 设置）；serde 永不读写。
    ///
    /// 为什么不落进 `config.toml`：档位是「这次对话要想多深」的工作状态，
    /// 不是「这个分组是什么端点」的一部分。它随**会话文件**落盘
    /// （`Session.session_effort`），但绝不进全局配置。
    ///
    /// 之所以放在 `Config` 而不是单独传参：Agent 重建的入口是
    /// `RebuildFn(&Config, &str)`，而档位必须参与 `effective_reasoning_effort()`
    /// 的优先级裁决（env > 会话级 > profile）。放这里就不用改那条签名。
    #[serde(skip)]
    pub runtime_session_effort: Option<String>,
    /// 各子系统 retention / cap / rotation 配置(`[storage]` 节)。
    /// 全部字段 opt-out,0 = 关闭。
    #[serde(default)]
    pub storage: StorageRetentionCfg,
    /// 持久化前 `ContentBlock` 字节截断配置(`[persist_cap]` 节)。
    /// 全部字段 opt-out,0 = 不截断。
    #[serde(default)]
    pub persist_cap: PersistCapCfg,
    /// 统一通知通道（v0.2.23+）：覆盖 TUI/CLI 回合完成 / 错误、后台子 Agent
    /// 完成、定时任务失败四类事件，通过终端响铃（stderr `\x07`）与桌面通知
    /// （macOS `osascript` / Linux `notify-send` / Windows PowerShell BurntToast）
    /// 提醒用户。详见 `crates/cli/src/notify.rs`。
    #[serde(default)]
    pub notify: NotifyCfg,
    /// 本进程加载时读到的**原始磁盘文本**，供 `save_to` 做三方 diff 合并。
    ///
    /// 存原始文本而不是 `Config` 结构：合并要发生在 **toml key 粒度**上（见
    /// [`Config::save_to`]），用结构体比对就得为每个字段写一遍 PartialEq + 字段名，
    /// 每加一个配置项都会变成一处新的静默漏更新。
    ///
    /// `None` = 无基线（内存构造的 Config，如 onboarding 写入 API Key、或测试直接
    /// `Config::default()`），此时 `save_to` 退化为整份覆盖。
    #[serde(skip)]
    pub loaded_baseline: Option<String>,
}

/// `[notify]` 配置块（v0.2.23+）—— 统一通知通道的横切配置。
///
/// 覆盖 TUI/CLI/Schedule 三个子系统共用的 4 类事件（TUI 回合完成/错误、
/// 后台子 Agent 完成、cron 任务失败），通过 [`NotifyBellCfg`] 与
/// [`NotifyDesktopCfg`] 两个 sink 通道发到用户面前：
///
/// - `bell` —— stderr `\x07`，**opt-in 默认关**（共享场景易扰民）
/// - `desktop` —— 桌面通知，**opt-out 默认开**（cron / 后台场景必须）
/// - `events.*` —— 4 个细粒度事件 gate；`subagent_done` 用户选择
///   opt-out 默认开（后台 Agent 是用户离开终端最该被提醒的场景）
/// - `rate_limit_seconds` —— 同 kind 最小间隔，0 = 关闭
/// - `include_session_id` —— 是否在 body 末尾追加 `[session:xxx]`
///
/// env 紧急关：`WYJ_CODE_NOTIFY_OFF=1`；具体字段 override：`_BELL` / `_DESKTOP`。
/// env 在 `wyj_cli::notify::init` 阶段读取，**绝不写回 cfg**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyCfg {
    /// 总开关。默认 `true`（opt-out），仿 `evolution.enabled=true`。
    pub enabled: bool,
    /// 终端响铃 sink。
    #[serde(default)]
    pub bell: NotifyBellCfg,
    /// 桌面通知 sink。
    #[serde(default)]
    pub desktop: NotifyDesktopCfg,
    /// 4 类事件的细粒度 gate。
    #[serde(default)]
    pub events: NotifyEventsCfg,
    /// 同类事件最小发送间隔（秒）。`0` 关闭限流。默认 30。
    pub rate_limit_seconds: u64,
    /// 是否在通知 body 末尾追加 `[session:xxx]` 上下文。默认 `false`。
    pub include_session_id: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyBellCfg {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyDesktopCfg {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyEventsCfg {
    pub turn_finished: bool,
    pub turn_error: bool,
    pub subagent_done: bool,
    pub schedule_failure: bool,
}

impl Default for NotifyCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            bell: NotifyBellCfg::default(),       // enabled: false
            desktop: NotifyDesktopCfg::default(), // enabled: true
            events: NotifyEventsCfg::default(),   // 全 true
            rate_limit_seconds: 30,
            include_session_id: false,
        }
    }
}

impl Default for NotifyDesktopCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for NotifyEventsCfg {
    fn default() -> Self {
        Self {
            turn_finished: true,
            turn_error: true,
            subagent_done: true,
            schedule_failure: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// API Key 环境变量名校验：首字符必须为字母或下划线，后续字符允许字母、数字、下划线。
/// 不强制 ASCII 大写——允许小写、混合大小写，因为用户经常在 `~/.bashrc` / `~/.zshrc`
/// 里写 `export model_minimax=...`、`export MiniMax_API_KEY=...` 之类。
/// 进程只读 `std::env::var(name)`，大小写敏感由 OS / shell 决定。
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
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
            context_edit: ContextEditCfg::default(),
            sandbox: SandboxCfg::default(),
            computer_use: ComputerUseCfg::default(),
            auto_mode: AutoModeCfg::default(),
            agent: AgentCfg::default(),
            tools: ToolsCfg::default(),
            ui: UiCfg::default(),
            search: SearchCfg::default(),
            runtime_search_api_key: None,
            runtime_api_key: None,
            runtime_session_effort: None,
            storage: StorageRetentionCfg::default(),
            persist_cap: PersistCapCfg::default(),
            notify: NotifyCfg::default(),
            loaded_baseline: None,
        }
    }
}

/// 把旧版顶层的 `search_provider` / `search_api_key` 搬进 `[search]` 节。
///
/// 返回是否发生了迁移（调用方据此打一次 warn）。规则：
///   * 目标是**已存在**的 `[search].provider` / `[search].api_key` 时保留之（新的显式
///     配置优先于旧字段）；
///   * 无论是否搬运成功，旧的顶层键都会**删除**——留着它们只会让用户以为还生效。
fn migrate_legacy_search_fields(value: &mut toml::Value) -> bool {
    let Some(table) = value.as_table_mut() else {
        return false;
    };
    let legacy_provider = table.remove("search_provider");
    let legacy_key = table.remove("search_api_key");
    if legacy_provider.is_none() && legacy_key.is_none() {
        return false;
    }
    let section = table
        .entry("search".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    if let Some(section) = section.as_table_mut() {
        if let Some(provider) = legacy_provider {
            section.entry("provider".to_string()).or_insert(provider);
        }
        if let Some(key) = legacy_key {
            section.entry("api_key".to_string()).or_insert(key);
        }
    }
    true
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
                aux_model: None,
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
                reasoning_effort: None,
                thinking_switch: None,
                max_tools_per_turn: None,
            }],
            log_level: legacy.log_level,
            language: legacy.language,
            mcp_servers: legacy.mcp_servers,
            auto_memory_enabled: true,
            evolution: EvolutionCfg::default(),
            subagent: SubAgentCfg::default(),
            routing: RoutingCfg::default(),
            model_runtime: ModelRuntimeCfg::default(),
            context_edit: ContextEditCfg::default(),
            sandbox: SandboxCfg::default(),
            computer_use: ComputerUseCfg::default(),
            auto_mode: AutoModeCfg::default(),
            agent: AgentCfg::default(),
            tools: ToolsCfg::default(),
            ui: UiCfg::default(),
            search: SearchCfg::default(),
            runtime_search_api_key: None,
            runtime_api_key: None,
            runtime_session_effort: None,
            storage: StorageRetentionCfg::default(),
            persist_cap: PersistCapCfg::default(),
            notify: NotifyCfg::default(),
            loaded_baseline: None,
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
            // Auto 与 Bypass 共用 exec 模型：两者对模型能力的要求相同，
            // 差别只在权限判定，不值得让用户为切换模式再配一个模型。
            AgentMode::Auto | AgentMode::Bypass => p.exec_model.as_deref().unwrap_or(&p.model),
        }
    }

    /// 当前模式下**实际生效**的模型字段名。
    ///
    /// `model_for_mode` 只返回模型名，调用方无法知道它来自哪个字段——
    /// 而「会话级模型覆盖」必须精确改写那个字段，否则会出现"改了
    /// `exec_model` 但当前是 plan 模式、看起来没生效"这类假象。
    ///
    /// 判定规则与 `model_for_mode` **严格一致**：
    /// - plan 模式 → `plan_model`（为空时回退 `model`）
    /// - auto / bypass → `exec_model`（为空时回退 `model`）
    pub fn model_field_for_mode(&self, mode: &AgentMode) -> &'static str {
        let p = self.active_profile();
        match mode {
            AgentMode::Plan => {
                if p.plan_model.is_some() {
                    "plan_model"
                } else {
                    "model"
                }
            }
            AgentMode::Auto | AgentMode::Bypass => {
                if p.exec_model.is_some() {
                    "exec_model"
                } else {
                    "model"
                }
            }
        }
    }

    /// 把当前模式实际生效的那个模型字段设为 `model`。
    ///
    /// 只改一个字段（见 [`Config::model_field_for_mode`]），其余模式专属模型
    /// 原样保留——用户在 auto 模式挑的模型不该顺手改掉 plan 模式用的那个。
    pub fn set_model_for_mode(&mut self, mode: &AgentMode, model: &str) {
        let field = self.model_field_for_mode(mode);
        let p = self.active_profile_mut();
        match field {
            "plan_model" => p.plan_model = Some(model.to_string()),
            "exec_model" => p.exec_model = Some(model.to_string()),
            _ => p.model = model.to_string(),
        }
    }

    /// 本次运行实际生效的 `reasoning_effort`：**env > 会话级 > profile**。
    ///
    /// 三级优先级的理由：
    /// 1. `WYJ_CODE_THINKING_EFFORT` 是用户在 shell 里显式设的运行期开关，
    ///    优先级最高——它本意就是「这次运行一律用这个档位」，任何面板操作
    ///    都不该盖掉它；
    /// 2. 其次是本会话的 `/effort` 覆盖（`runtime_session_effort`）；
    /// 3. 最后才是分组里配的 `reasoning_effort`。
    ///
    /// `off` 的特殊语义：**彻底关思考** —— `reasoning_effort` 返回 `None`，
    /// `thinking_switch` 返回 `Some("disabled")`。
    pub fn effective_reasoning_effort(&self) -> Option<String> {
        if let Some(value) = std::env::var("WYJ_CODE_THINKING_EFFORT")
            .ok()
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
        match self.runtime_session_effort.as_deref() {
            Some("off") => None,
            Some(level) if !level.trim().is_empty() => Some(level.to_string()),
            _ => self.active_profile().effective_reasoning_effort(),
        }
    }

    /// 本次运行实际生效的 `thinking_switch`：**env > 会话级 > profile**。
    ///
    /// 会话级档位是 `off` 时返回 `Some("disabled")`（彻底关思考）；
    /// 其它档位**不覆盖** profile 里的 `thinking_switch`——档位与开关是
    /// 两个独立旋钮：官方 Anthropic 上 effort 与 thinking budget 可以共存，
    /// 用户配了 `thinking_switch = "enabled"` 不该被 `/effort high` 抹掉。
    pub fn effective_thinking_switch(&self) -> Option<String> {
        if let Some(value) = std::env::var("WYJ_CODE_THINKING_SWITCH")
            .ok()
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
        match self.runtime_session_effort.as_deref() {
            Some("off") => Some("disabled".to_string()),
            _ => self.active_profile().effective_thinking_switch(),
        }
    }
}

impl Config {
    /// 加载配置：先读 `~/.wyj-code/config.toml` 文件本体（含旧格式一次性迁移），
    /// 再用环境变量覆盖激活分组的 api_key。
    ///
    /// 自 v0.2.24 起不再合并 `~/.claude.json` 等外部"原生"MCP 配置——wyj-code
    /// 只信任用户写在 `~/.wyj-code/` 与 `<git-root>/.wyj-code/` 下的条目。
    pub fn load() -> Result<Self> {
        let mut cfg = Self::load_file_only()?;

        // 一次性 deprecation：用户保留 [sandbox] 块时打印一次警告。
        // `crates/sandbox/` 已删除，所有 sandbox 字段已不再生效。
        if cfg.sandbox != SandboxCfg::default() {
            tracing::warn!(
                "[sandbox] 配置块已废弃（crates/sandbox crate 已移除）；Bash 命令直接继承宿主 PATH/USER/SHELL。\
                 请从 ~/.wyj-code/config.toml 删除 [sandbox] 块以消除本警告。"
            );
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
        // WebSearch key：**写进 serde 跳过的运行期槽位**，与 api_key 同理。
        // 早先的实现直接覆盖 `search_api_key` 普通字段，于是用户只要打开一次设置
        // 面板并保存，环境变量里的 key 就被物化进了 config.toml。
        cfg.runtime_search_api_key = std::env::var("WYJ_CODE_SEARCH_API_KEY")
            .ok()
            .filter(|key| !key.is_empty());
        Ok(cfg)
    }

    /// 只读 `config.toml` 文件本体（含旧格式一次性迁移），不吃环境变量。当前与
    /// `Config::load` 行为完全一致（合并 `~/.claude.json` 的逻辑自 v0.2.24 起
    /// 删除），保留该入口仅为兼容历史调用方——新代码直接用 `Config::load`。
    pub fn load_file_only() -> Result<Self> {
        Self::load_file_only_at(&config_file_path()?)
    }

    /// `load_file_only` 的路径注入版（测试、`/import` 的 `ImportTargets` 使用）。
    pub fn load_file_only_at(config_path: &Path) -> Result<Self> {
        let mut cfg: Config = if config_path.exists() {
            let content = std::fs::read_to_string(config_path)
                .with_context(|| format!("读取配置文件失败: {}", config_path.display()))?;
            let mut value: toml::Value = toml::from_str(&content)
                .with_context(|| format!("解析配置文件失败: {}", config_path.display()))?;
            if value.get("profiles").is_some() {
                // 旧顶层 `search_provider` / `search_api_key` 一次性迁到 `[search]` 节。
                // 必须在反序列化**之前**改写 toml::Value：`Config` 里已没有这两个字段，
                // 残留的旧键会被 serde 静默忽略——用户会看到「key 还在文件里，工具却不灵」。
                if migrate_legacy_search_fields(&mut value) {
                    tracing::warn!(
                        "配置里的 search_provider / search_api_key 已迁移到 [search] 节 \
                         （provider / api_key）；下次保存配置即会写回新格式: {}",
                        config_path.display()
                    );
                }
                value
                    .try_into()
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

        // 记下「本进程读到的原始磁盘内容」，供 `save_to` 做三方 diff 合并。
        // 必须在上面的规范化（补默认 profile、纠正 active_profile）**之前**采样，
        // 否则基线里会混进「我们替用户补的值」，保存时会被误判成本进程改动。
        cfg.loaded_baseline = config_path
            .exists()
            .then(|| std::fs::read_to_string(config_path).ok())
            .flatten();

        Ok(cfg)
    }

    /// 返回 WebSearch 后端的有效 API Key。
    ///
    /// 优先环境变量（已落进 `runtime_search_api_key`），其次 `[search].api_key`。
    /// 返回 `None` 表示未配置——调用方据此**不注册** WebSearch 工具（而不是注册一个
    /// 必然失败的工具让模型白跑一趟）。
    pub fn search_api_key(&self) -> Option<&str> {
        self.runtime_search_api_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .or_else(|| self.search.api_key.as_deref().filter(|k| !k.is_empty()))
    }

    /// 生效的搜索后端名。空串按默认（`tavily`）处理——诊断输出需要一个具体的
    /// 名字，直接把空串渲染成「WebSearch: （已配置）」会让用户以为配置掉了。
    pub fn effective_search_provider(&self) -> &str {
        let provider = self.search.provider.trim();
        if provider.is_empty() {
            "tavily"
        } else {
            provider
        }
    }

    /// 搜索后端的配置问题，`None` 表示配置可用。三类返回值：
    ///   * provider 名字不在已知列表 → 显式报错（**绝不静默回退**到默认后端）；
    ///   * provider 非空但未配置 API Key → 提示去哪配；
    ///   * 其它情况（含"有 key 但 provider 为空，按默认 tavily 处理"）→ `None`。
    ///
    /// 这个方法是 "provider 是否被消费" 的单一判定入口：`build_backend`、
    /// `/doctor`、`--config-status` 都调它，避免三处各写一套判断后互相漂移。
    pub fn search_provider_issue(&self) -> Option<String> {
        let provider = self.search.provider.trim();
        let known = SearchCfg::KNOWN_PROVIDERS
            .iter()
            .any(|p| p.eq_ignore_ascii_case(provider));
        // 空串按默认后端（tavily）处理，兼容手写配置时留空。
        if !provider.is_empty() && !known {
            return Some(format!(
                "[search].provider = \"{provider}\" 不是已知后端；可用值：{}",
                SearchCfg::KNOWN_PROVIDERS.join(" / ")
            ));
        }
        if self.search_api_key().is_none() {
            return Some(
                "未配置 [search].api_key（或环境变量 WYJ_CODE_SEARCH_API_KEY）".to_string(),
            );
        }
        None
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
    ///
    /// 跨进程安全靠两层：
    /// 1. **锁内重读 + 三方合并**——两个 wyj-code 同时改 `config.toml` 的**不同**字段
    ///    （A 在 `/model` 换模型、B 在 `/config` 改 log_level）时，两者都必须生效。
    ///    合并规则是标准三方 diff：只把「本进程相对基线改过的 key」覆盖到磁盘当前值上，
    ///    其余保留磁盘版本。若没有基线（内存构造的 Config，如 onboarding 写入 API key），
    ///    退化成整份覆盖——那种场景本进程掌握全部事实，无从也无须合并。
    /// 2. **原子写**（tmp + rename），保证读侧不会看到截断的 toml。
    ///
    /// 刻意用 **toml key 粒度**而不是给每个字段写合并代码：配置项会一直增加，
    /// 逐字段实现意味着每加一个字段都要记得更新合并逻辑，漏一个就是新的静默丢更新。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let _guard = filelock::acquire(&path.with_extension("toml.lock"), "config.toml")?;
        let merged = self.merged_toml(path)?;
        let content = toml::to_string_pretty(&merged).context("序列化配置失败")?;
        write_atomic(path, &content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))
    }

    /// 锁内求「应该写回磁盘的最终内容」：磁盘当前值 ⊕ 本进程相对基线的改动。
    fn merged_toml(&self, path: &Path) -> Result<toml::Value> {
        let mine = toml::Value::try_from(self).context("序列化配置失败")?;
        let Some(baseline) = self.loaded_baseline.as_deref() else {
            return Ok(mine);
        };
        let Ok(base_value) = toml::from_str::<toml::Value>(baseline) else {
            // 基线不可解析（文件是别的进程写的、或被人手改坏了）：无基线可 diff，
            // 整份覆盖是此刻唯一能确定语义的选择。
            return Ok(mine);
        };
        let disk = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<toml::Value>(&text).ok(),
            // 磁盘文件没了（用户删了）：以基线为底，把本进程的改动全部盖上去。
            Err(_) => Some(base_value.clone()),
        };
        let Some(mut merged) = disk else {
            return Ok(mine);
        };
        let (Some(base_table), Some(merged_table)) = (base_value.as_table(), merged.as_table_mut())
        else {
            // 磁盘上不是 table 结构：无基线结构可比，整份覆盖。
            return Ok(mine);
        };
        let empty = toml::Table::new();
        apply_changes(merged_table, base_table, mine.as_table().unwrap_or(&empty));
        Ok(merged)
    }
}

/// 三方合并：把 `mine` 相对 `base` 变化的 key 覆盖到 `target` 上，未变化的保留 target。
///
/// 递归到 table 层，因此 `[sandbox.network]` 这种嵌套块里改一个子字段也不会把
/// 同一块里的其他子字段一起覆盖掉。数组（含 `profiles`）按整体比较。
fn apply_changes(target: &mut toml::Table, base: &toml::Table, mine: &toml::Table) {
    for (key, mine_value) in mine {
        let base_value = base.get(key);
        if Some(mine_value) == base_value {
            // 本进程没碰这个 key —— 保留磁盘上（可能被别人改过的）值。
            continue;
        }
        let Some(base_table) = base_value.and_then(toml::Value::as_table) else {
            target.insert(key.clone(), mine_value.clone());
            continue;
        };
        match target.get_mut(key).and_then(toml::Value::as_table_mut) {
            Some(target_table) => {
                apply_changes(
                    target_table,
                    base_table,
                    mine_value.as_table().unwrap_or(&toml::Table::new()),
                );
            }
            // 磁盘上这个 key 不是 table（被别人换了类型）：本进程的改动优先。
            None => {
                target.insert(key.clone(), mine_value.clone());
            }
        }
    }
    // 只在基线里有、mine 里删掉的 key → 也算本进程的改动，要从 target 删掉。
    for key in base.keys() {
        if !mine.contains_key(key) && !matches!(target.get(key), Some(toml::Value::Table(_))) {
            target.remove(key);
        }
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
    // 配置文件通常包含 API Key 等敏感字段,落盘后默认收紧到仅当前用户
    // 可读写。Windows 上没有等价模式,跳过。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 返回配置目录路径（~/.wyj-code），若不存在则创建。
///
/// 数据根可由 `WYJ_CODE_HOME` 覆盖，见 [`layout::data_root_in`]。
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
///
/// 只有当 `home` 就是本进程真实主目录时才接受 `WYJ_CODE_HOME` 覆盖——传
/// tempdir 的测试路径必须保持隔离，理由见 [`layout::data_root_for_home`]。
pub fn global_config_dir_in(home: &Path) -> PathBuf {
    layout::data_root_for_home(home)
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

/// 返回全局配置目录（`~/.wyj-code`），仅解析路径、不创建。
///
/// 记忆文件 `~/.wyj-code/AGENTS.md` 与全局 hooks `~/.wyj-code/settings.json`
/// 都落在这里。**v0.2.27 起不再有 `claude_home_dir()`**：这两者分别从
/// `~/.claude/CLAUDE.md` 与 `~/.claude/settings.json` 迁入，wyj-code 自此
/// 完全不读任何 `.claude/` 路径。项目级对应物由 [`project_config_dir`] 定位。
pub fn global_config_dir() -> Result<PathBuf> {
    Ok(global_config_dir_in(&home_dir()?))
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
mod valid_env_name_tests {
    /// `valid_env_name` 是私有函数——只能从同 crate 的测试访问。
    /// 通过 `Config::load` 走真实路径比较稳，但环境变量是进程级副作用，
    /// 单测里直接用 `valid_env_name` 测纯函数更轻、可重复。
    use super::{valid_env_name, Config, Profile, Provider};

    #[test]
    fn accepts_classic_uppercase_names() {
        // 旧实现下能过的名字——回归保护。
        for name in [
            "WYJ_CODE_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "MINIMAX_API_KEY",
            "_LEADING_UNDERSCORE",
        ] {
            assert!(valid_env_name(name), "应当接受 {name}");
        }
    }

    #[test]
    fn accepts_lowercase_and_mixed_case_names() {
        // ~/.bashrc / ~/.zshrc 里经常写 `export model_minimax=...` 这种小写或
        // 混合大小写（MiniMax_API_KEY / minImAx_KEY）。进程读 std::env::var 时
        // 由 OS 决定大小写敏感，这里只放宽结构校验，不强行改大小写。
        for name in ["model_minimax", "MiniMax_API_KEY", "minImAx_KEY", "a"] {
            assert!(valid_env_name(name), "应当接受 {name}");
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "", "1FOO", "FOO-BAR", "FOO BAR", "FOO$BAR", "FOO=BAR", " FOO",
        ] {
            assert!(!valid_env_name(name), "应当拒绝 {name:?}");
        }
    }

    /// Profile → Config::load 的端到端链路：api_key_env 为小写时也能从 env 读到 key。
    /// 用临时 env（避免污染进程）；serial 标注确保不与其他 env 测试并发。
    #[test]
    fn profile_with_lowercase_api_key_env_is_loaded_from_environment() {
        let cfg = Config {
            active_profile: "minimax".to_string(),
            profiles: vec![Profile {
                name: "minimax".to_string(),
                provider: Provider::OpenAI,
                model: "MiniMax-M3".to_string(),
                plan_model: None,
                exec_model: None,
                aux_model: None,
                base_url: "https://api.minimax.chat/v1".to_string(),
                api_key: None,
                api_key_env: Some("model_minimax".to_string()),
                max_tokens: 8192,
                context_window: 200_000,
                vision: true,
                thinking_budget: None,
                interleaved_thinking: true,
                prompt_cache: None,
                openai_stream_options: None,
                reasoning_effort: None,
                thinking_switch: None,
                max_tools_per_turn: None,
            }],
            ..Config::default()
        };

        // 单元测试不通过 Config::load 整条路径（那会读盘 + 触真实 ~/.claude.json）。
        // 这里直接复用 Config::load 里那一段相同优先级的 env 解析逻辑：
        // 1. WYJ_CODE_API_KEY 优先；2. profile.api_key_env；3. profile.api_key。
        // 我们临时注入 model_minimax，断言 resolve_api_key 拿到它。
        std::env::set_var("WYJ_CODE_API_KEY", "");
        std::env::set_var("model_minimax", "lowercase-key-from-bashrc");
        let resolved: Option<String> = cfg
            .active_profile()
            .api_key_env
            .as_deref()
            .filter(|n| valid_env_name(n))
            .and_then(|n| std::env::var(n).ok())
            .filter(|k| !k.is_empty());
        std::env::remove_var("model_minimax");
        std::env::remove_var("WYJ_CODE_API_KEY");

        assert_eq!(resolved.as_deref(), Some("lowercase-key-from-bashrc"));
    }
}

#[cfg(test)]
mod aux_model_cfg_tests {
    use super::{Config, Profile};

    /// 旧配置里没有 `aux_model` 这项 → 必须读成 `None`（辅助调用走主模型），
    /// 而不是解析失败或被静默填成主模型名。
    #[test]
    fn a_profile_without_aux_model_means_use_the_main_model() {
        let cfg: Config = toml::from_str(
            r#"
active_profile = "main"

[[profiles]]
name = "main"
provider = "anthropic"
model = "claude-opus-4-8"
base_url = ""
api_key = "placeholder"
max_tokens = 32000
context_window = 200000
"#,
        )
        .unwrap();

        assert_eq!(cfg.active_profile().aux_model, None);
    }

    /// `aux_model` 写进 TOML 后必须原样读回来（写盘 → 读盘往返）。
    #[test]
    fn aux_model_survives_a_toml_round_trip() {
        let cfg: Config = toml::from_str(
            r#"
active_profile = "main"

[[profiles]]
name = "main"
provider = "anthropic"
model = "MiniMax-M3"
aux_model = "MiniMax-M3.1-Flash-Preview"
base_url = "https://api.minimaxi.com/anthropic"
api_key = "placeholder"
max_tokens = 32000
context_window = 200000
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.active_profile().aux_model.as_deref(),
            Some("MiniMax-M3.1-Flash-Preview")
        );

        // 序列化回去再读一次，值不能丢。
        let text = toml::to_string(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            reparsed.active_profile().aux_model.as_deref(),
            Some("MiniMax-M3.1-Flash-Preview")
        );
    }

    /// 默认值是 `None`（不配置辅助模型 = 全部走主模型）。
    #[test]
    fn the_default_profile_has_no_aux_model() {
        assert_eq!(Profile::default().aux_model, None);
    }
}

#[cfg(test)]
mod subagent_cfg_tests {
    use super::{
        ComputerUseCfg, Config, EvolutionCfg, ForegroundFallback, MouseMode, Profile, Provider,
        RoutingRole, SubAgentCfg, UiCfg, WireProtocol,
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

    /// 旧配置里写了 `vendor` / `wire_protocol` 两项（它们已从 `Profile`
    /// 移除）→ **加载必须不报错**，且这两行被静默忽略。
    ///
    /// `Config` / `Profile` 都没有 `deny_unknown_fields`，所以未知键直接丢弃。
    /// 用户看到的是"文件里还写着但代码不读"——下一次 `config.save()` 时
    /// `profiles` 数组被整体替换，这两行会从文件里消失。
    ///
    /// 这与仓库里删字段的既有惯例一致（如 `StorageRetentionCfg`）：
    /// 静默忽略 + 无需迁移代码，代价是配置文件里会短暂留下一行无效配置。
    #[test]
    fn legacy_vendor_and_wire_protocol_keys_are_silently_ignored() {
        let p: Profile = toml::from_str(
            r#"
name = "legacy"
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

        // 线协议现在**只**由 provider 决定 —— 旧配置里显式写的
        // `anthropic_messages` 不再有任何效力。
        assert_eq!(
            p.effective_wire_protocol(),
            WireProtocol::OpenAiChatCompletions
        );
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

    #[test]
    fn notify_defaults_match_recommendation() {
        let cfg = super::NotifyCfg::default();
        assert!(cfg.enabled, "master opt-out 默认开");
        assert!(!cfg.bell.enabled, "bell opt-in 默认关（共享场景防扰民）");
        assert!(cfg.desktop.enabled, "desktop opt-out 默认开");
        assert!(cfg.events.turn_finished);
        assert!(cfg.events.turn_error);
        assert!(
            cfg.events.subagent_done,
            "subagent_done 用户选择 opt-out 默认开（后台离开终端最该被提醒）"
        );
        assert!(cfg.events.schedule_failure);
        assert_eq!(cfg.rate_limit_seconds, 30);
        assert!(!cfg.include_session_id);
    }

    #[test]
    fn empty_notify_section_keeps_safe_defaults() {
        // 完全空的 `[notify]` 块应全部走 default impl，不允许用户写空块崩解析。
        let toml = r#"
            [notify]
            "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.notify.enabled);
        assert!(!cfg.notify.bell.enabled);
        assert!(cfg.notify.desktop.enabled);
        assert_eq!(cfg.notify.rate_limit_seconds, 30);
    }

    #[test]
    fn notify_partial_override_keeps_untouched_defaults() {
        // 只写一个字段，其它字段继承 default
        let toml = r#"
            [notify.events]
            turn_finished = false
            "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.notify.events.turn_finished);
        assert!(cfg.notify.events.turn_error);
        assert!(cfg.notify.events.subagent_done);
        assert!(cfg.notify.events.schedule_failure);
        assert_eq!(cfg.notify.rate_limit_seconds, 30);
    }

    #[test]
    fn todo_is_enabled_by_default_and_opt_out_works() {
        // 默认开启（v0.3.1）：复杂长任务里 TodoWrite 是唯一能穿透自动压缩的
        // 显式进度锚点。代价是每个用到的会话多约 1 个 LLM 往返，所以开关保留。
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.tools.todo_enabled);

        // 用户显式关闭时（简单任务 / 强模型短任务，对应 CLAUDE_CODE_ENABLE_TODO_TOOLS=0）
        let cfg: Config = toml::from_str("[tools]\ntodo_enabled = false\n").unwrap();
        assert!(!cfg.tools.todo_enabled);
    }

    #[test]
    fn todo_opt_out_survives_a_partial_tools_block() {
        // 老配置里已有 [tools] 但没写 todo_enabled：走结构体级 serde(default)，
        // 取 ToolsCfg::default() 的 true。显式写 false 的必须仍是 false——
        // 否则用户关不掉，只能去删整个 [tools] 块。
        let cfg: Config = toml::from_str("[tools]\n").unwrap();
        assert!(cfg.tools.todo_enabled);
        let cfg: Config = toml::from_str("[tools]\ntodo_enabled = false\n").unwrap();
        assert!(!cfg.tools.todo_enabled);
    }

    #[test]
    fn max_turns_defaults_to_200_and_is_overridable() {
        // 老配置里根本没有 [agent] 这一节：走结构体级 serde(default)，拿到 200。
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.agent.max_turns, 200);

        // 超长任务的用户需要更多预算；显式写了必须生效。
        let cfg: Config = toml::from_str("[agent]\nmax_turns = 500\n").unwrap();
        assert_eq!(cfg.agent.max_turns, 500);
    }

    /// `[agent]` 块存在但没写 `max_turns` 时必须仍是 200——否则用户为了调
    /// 别的参数而新建这个块，会顺手把上限静默改成 0，整个 Agent 立刻不可用。
    #[test]
    fn a_partial_agent_block_keeps_the_default_budget() {
        let cfg: Config = toml::from_str("[agent]\n").unwrap();
        assert_eq!(cfg.agent.max_turns, 200);
    }

    #[test]
    fn save_to_sets_0o600_permissions_on_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let cfg = Config::default();
        cfg.save_to(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "config.toml 应仅当前用户可读写"
        );
    }

    use super::AgentMode;

    /// 两个 wyj-code 同时改 `config.toml` 的**不同**字段时，两边都必须生效。
    ///
    /// 旧行为是整份覆盖：A 在 `/model` 面板改 model，B 在 `/config` 改 log_level，
    /// B 后保存就把 A 的 model 改回旧值——用户会看到「刚换的模型自己变回去了」，
    /// 且没有任何提示。三方合并（base=各自的加载快照，mine=各自内存态，
    /// target=锁内重读的磁盘当前值）保证未改动的字段一律以磁盘为准。
    #[test]
    fn save_to_merges_concurrent_edits_to_different_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        Config::default().save_to(&path).unwrap();

        // A 与 B 都从**同一份**磁盘状态加载，各自看到相同的基线。
        let mut a = Config::load_file_only_at(&path).unwrap();
        let mut b = Config::load_file_only_at(&path).unwrap();

        // A 改模型，B 改日志级别——两个互不相干的字段。
        a.profiles[0].model = "model-from-A".to_string();
        b.log_level = "debug".to_string();

        // B 先落盘，A 后落盘：A 是「陈旧快照」，必须合而不是盖。
        b.save_to(&path).unwrap();
        a.save_to(&path).unwrap();

        let final_cfg = Config::load_file_only_at(&path).unwrap();
        assert_eq!(
            final_cfg.profiles[0].model, "model-from-A",
            "A 改的 model 必须保住"
        );
        assert_eq!(
            final_cfg.log_level, "debug",
            "B 改的 log_level 不能被 A 的陈旧快照盖回去"
        );
    }

    /// 同一个字段被两个进程改：后保存者赢（无法也不该调和，只能有确定行为）。
    #[test]
    fn save_to_last_writer_wins_on_the_same_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        Config::default().save_to(&path).unwrap();

        let mut a = Config::load_file_only_at(&path).unwrap();
        let mut b = Config::load_file_only_at(&path).unwrap();
        a.profiles[0].model = "from-A".to_string();
        b.profiles[0].model = "from-B".to_string();

        a.save_to(&path).unwrap();
        b.save_to(&path).unwrap();
        assert_eq!(
            Config::load_file_only_at(&path).unwrap().profiles[0].model,
            "from-B"
        );
    }

    /// 无基线（内存构造，如 onboarding 写 API Key）时退化为整份覆盖，不能静默不写。
    #[test]
    fn save_to_without_baseline_still_writes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.profiles[0].model = "onboarded-model".to_string();
        cfg.save_to(&path).unwrap();
        assert_eq!(
            Config::load_file_only_at(&path).unwrap().profiles[0].model,
            "onboarded-model"
        );
    }

    /// 默认起始模式必须是 Auto —— 这是与上游对齐的第一条，回归测试钉死它。
    #[test]
    fn default_agent_mode_is_auto() {
        assert_eq!(AgentMode::default(), AgentMode::Auto);
        assert_eq!(AgentMode::Auto.label(), "auto");
    }

    /// `[ui]` 缺省时：鼠标走自动检测（常见终端都开）+ 滚动条可见。
    ///
    /// 老配置文件里没有 `[ui]` 节，必须靠 `#[serde(default)]` 落到这里的默认值。
    #[test]
    fn ui_cfg_defaults_to_auto_mouse_and_scrollbar_on() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.ui.mouse, MouseMode::Auto);
        assert!(cfg.ui.scrollbar);
        // 常见终端（xterm-256color / screen / tmux）都判定为开启。
        assert!(cfg.ui.mouse.resolve(Some("xterm-256color")));
        assert!(cfg.ui.mouse.resolve(Some("screen-256color")));
        // 明确不支持鼠标上报的环境关闭。
        assert!(!cfg.ui.mouse.resolve(Some("dumb")));
        assert!(!cfg.ui.mouse.resolve(None));
    }

    /// 三态显式取值要能生效，且优先级高于自动检测。
    #[test]
    fn ui_cfg_mouse_mode_parses_and_overrides_detection() {
        let cfg: Config = toml::from_str("[ui]\nmouse = \"off\"\n").unwrap();
        assert_eq!(cfg.ui.mouse, MouseMode::Off);
        assert!(!cfg.ui.mouse.resolve(Some("xterm-256color")));

        let cfg: Config = toml::from_str("[ui]\nmouse = \"on\"\nscrollbar = false\n").unwrap();
        assert_eq!(cfg.ui.mouse, MouseMode::On);
        assert!(cfg.ui.mouse.resolve(Some("dumb")));
        assert!(!cfg.ui.scrollbar);
    }

    /// 旧字段 `mouse_capture` 仍要生效（用户可能还没改配置），但不会被写回。
    #[test]
    fn legacy_mouse_capture_field_still_applies_and_is_not_serialized() {
        let cfg: Config = toml::from_str("[ui]\nmouse_capture = false\n").unwrap();
        assert_eq!(cfg.ui.mouse, MouseMode::Auto);
        assert!(!cfg.ui.resolved_mouse());

        let cfg: Config = toml::from_str("[ui]\nmouse_capture = true\n").unwrap();
        assert!(cfg.ui.resolved_mouse());

        // 写回时不再出现旧键，避免「删不掉的配置项」。
        let text = toml::to_string(&UiCfg::default()).unwrap();
        assert!(!text.contains("mouse_capture"), "{text}");
    }

    /// CLI 的 `--permission-mode` 与 TUI 的 `/mode` 共用这一个解析器。
    ///
    /// `manual` / `default` / `normal` 是历史遗留拼法，手动模式合并进 auto 后
    /// 它们全部落到 `Auto`——**必须仍然返回 `Some`**：旧配置与脚本里写着
    /// `manual` 是常态，让它解析失败等于用户的配置被静默作废。
    #[test]
    fn agent_mode_parse_accepts_upstream_spellings() {
        assert_eq!(AgentMode::parse("auto"), Some(AgentMode::Auto));
        assert_eq!(AgentMode::parse("AUTO"), Some(AgentMode::Auto));
        assert_eq!(AgentMode::parse("manual"), Some(AgentMode::Auto));
        assert_eq!(AgentMode::parse("default"), Some(AgentMode::Auto));
        assert_eq!(AgentMode::parse("normal"), Some(AgentMode::Auto));
        assert_eq!(AgentMode::parse("plan"), Some(AgentMode::Plan));
        assert_eq!(AgentMode::parse("bypass"), Some(AgentMode::Bypass));
        assert_eq!(AgentMode::parse("autoo"), None);
        assert_eq!(AgentMode::parse(""), None);
    }

    /// `[auto_mode]` 缺省时全部走内置默认值；显式声明时逐字段解析。
    #[test]
    fn auto_mode_cfg_defaults_and_overrides() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.auto_mode.enabled);
        assert_eq!(cfg.auto_mode.timeout_secs, 30);
        assert_eq!(cfg.auto_mode.max_consecutive_blocks, 3);
        assert_eq!(cfg.auto_mode.max_total_blocks, 20);
        assert!(!cfg.auto_mode.classify_all_shell);
        assert!(cfg.auto_mode.allow.is_empty(), "未配置时展开由 core 负责");

        let cfg: Config = toml::from_str(
            r#"
[auto_mode]
enabled = false
classifier_profile = "cheap"
classify_all_shell = true
max_consecutive_blocks = 1
allow = ["$defaults", "允许推送到 staging"]
"#,
        )
        .unwrap();
        assert!(!cfg.auto_mode.enabled);
        assert_eq!(cfg.auto_mode.classifier_profile, "cheap");
        assert!(cfg.auto_mode.classify_all_shell);
        assert_eq!(cfg.auto_mode.max_consecutive_blocks, 1);
        assert_eq!(cfg.auto_mode.allow, vec!["$defaults", "允许推送到 staging"]);
    }
}

#[cfg(test)]
mod search_cfg_tests {
    use super::{Config, SearchCfg};

    #[test]
    fn legacy_flat_search_fields_migrate_into_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
active_profile = "default"
search_provider = "brave"
search_api_key = "tvly-legacy"

[[profiles]]
name = "default"
provider = "anthropic"
model = "claude-opus-4-8"
base_url = ""
api_key = "placeholder"
max_tokens = 32000
context_window = 200000
"#,
        )
        .unwrap();

        let cfg = Config::load_file_only_at(&path).unwrap();
        assert_eq!(cfg.search.provider, "brave");
        assert_eq!(cfg.search_api_key(), Some("tvly-legacy"));

        // 保存后旧顶层键必须消失，否则用户会以为它们还是生效的。
        cfg.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("search_provider"), "{saved}");
        assert!(!saved.contains("search_api_key"), "{saved}");
        assert!(saved.contains("[search]"), "{saved}");
        // 迁移后的值仍然可读。
        let reloaded = Config::load_file_only_at(&path).unwrap();
        assert_eq!(reloaded.search.provider, "brave");
        assert_eq!(reloaded.search_api_key(), Some("tvly-legacy"));
    }

    #[test]
    fn explicit_search_section_wins_over_legacy_flat_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
active_profile = "default"
search_provider = "brave"
search_api_key = "legacy-key"

[search]
provider = "tavily"
api_key = "new-key"

[[profiles]]
name = "default"
provider = "anthropic"
model = "claude-opus-4-8"
base_url = ""
api_key = "placeholder"
max_tokens = 32000
context_window = 200000
"#,
        )
        .unwrap();
        let cfg = Config::load_file_only_at(&path).unwrap();
        assert_eq!(cfg.search.provider, "tavily");
        assert_eq!(cfg.search_api_key(), Some("new-key"));
    }

    /// 早先的实现把 env 直接覆盖到普通字段 `search_api_key` 上，于是「打开设置面板
    /// 保存一次」就会把环境变量里的 key 物化进 config.toml。落在 `#[serde(skip)]`
    /// 槽位后，落盘与读取两端都必须正确。
    #[test]
    fn search_runtime_key_is_not_materialized_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            runtime_search_api_key: Some("tvly-from-env".to_string()),
            ..Config::default()
        };
        config.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("tvly-from-env"), "{saved}");
        assert_eq!(config.search_api_key(), Some("tvly-from-env"));
    }

    #[test]
    fn search_provider_issue_flags_unknown_backends_and_missing_keys() {
        // 未知 provider：显式报错，绝不静默回退。
        let cfg = Config {
            search: SearchCfg {
                provider: "duckduckgo".to_string(),
                api_key: Some("k".to_string()),
                ..SearchCfg::default()
            },
            ..Config::default()
        };
        let issue = cfg.search_provider_issue().unwrap();
        assert!(issue.contains("duckduckgo"), "{issue}");
        assert!(issue.contains("tavily"), "{issue}");

        // provider 合法但缺 key。
        let cfg = Config {
            search: SearchCfg {
                provider: "tavily".to_string(),
                ..SearchCfg::default()
            },
            ..Config::default()
        };
        assert!(cfg.search_provider_issue().unwrap().contains("api_key"));

        // 空白 provider 也按缺 key 报（provider 本身有默认值兜底）。
        let cfg = Config {
            search: SearchCfg {
                provider: String::new(),
                ..SearchCfg::default()
            },
            ..Config::default()
        };
        assert!(cfg.search_provider_issue().unwrap().contains("api_key"));

        // 空 provider + 有 key → 视为可用，并按默认后端展示名。
        let cfg = Config {
            search: SearchCfg {
                provider: String::new(),
                api_key: Some("k".to_string()),
                ..SearchCfg::default()
            },
            ..Config::default()
        };
        assert!(cfg.search_provider_issue().is_none());
        assert_eq!(cfg.effective_search_provider(), "tavily");
    }

    /// 默认配置下没有 key → 工具不注册。这是「模型看不到必然失败的工具」的前提。
    #[test]
    fn default_config_has_no_search_key() {
        let cfg = Config::default();
        assert!(cfg.search_api_key().is_none());
        assert_eq!(cfg.effective_search_provider(), "tavily");
        assert!(cfg.search_provider_issue().is_some());
    }
}
