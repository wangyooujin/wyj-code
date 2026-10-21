//! 命令注册表与解析

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// 命令执行结果
#[derive(Debug)]
pub enum CommandResult {
    /// 输出文本给用户
    Output(String),
    /// 输出**自带排版**的文本：换行与列对齐必须原样保留。
    ///
    /// 为什么不复用 [`Output`]：那条路径在 TUI 里会走 `render_markdown`，
    /// 而 markdown 把「连续非空行」当一个段落重新折行（软换行语义）。任何
    /// 依赖列对齐的输出——`/cost` 的表格、`/usage` 的汇总——都会被揉成一团
    /// 糊（实测 50 行的成本表塌成 6 行）。代码块围栏也不行：那会给整块加上
    /// `╭─╮` 边框并再缩进 4 列。
    ///
    /// 消费方约定：逐行原样渲染，只在单行超出可用宽度时截断，**不折行**。
    OutputPlain(String),
    /// 清空对话历史
    ClearHistory,
    /// 打开分组（Profile）管理面板（/profile 无参触发）
    OpenProfileDialog,
    ModelDoctor(Option<String>),
    /// 按名切换激活分组（/profile <name> 触发）
    SwitchProfile(String),
    /// 打开模型选择面板（/model 无参触发）。
    ///
    /// 面板从当前分组的端点拉 `/models` 列表，失败时回退手填。
    OpenModelPicker,
    /// 切换当前分组下的模型（/model <model-name> 触发）。
    ///
    /// **只改内存**：不写 `config.toml`、不落会话文件。生效范围是当前
    /// 模式实际读的那个模型字段。
    SwitchModel(String),
    /// 打开思考强度档位面板（/effort 无参触发）。
    ///
    /// 面板只列当前端点**真正支持**的档位。
    OpenEffortPicker,
    /// 设置思考强度档位（/effort <level|off> 触发）。
    ///
    /// 档位随会话落盘，不写 `config.toml`。
    SetEffort(String),
    /// 手动触发上下文压缩
    CompactHistory,
    CreateCheckpoint {
        name: Option<String>,
        list: bool,
    },
    Rewind {
        checkpoint_id: Option<String>,
        scope: wyj_core::RewindScope,
        confirmed: bool,
    },
    BranchSession {
        checkpoint_id: Option<String>,
        restore_files: bool,
        confirmed: bool,
    },
    ControlSubAgent {
        id: u64,
        action: SubAgentControlAction,
    },
    /// 退出应用
    Quit,
    /// 无动作（静默成功）
    None,
    /// Skill 执行结果：包含展开后的 prompt，由调用方转发给 agent
    RunPrompt(String),
    /// 带执行期限定的 Skill/自定义命令结果：`allowed_tools` 非空时，调用方需在跑这一轮
    /// 之前临时把 `PermissionMode` 收紧为 `Allowlist(allowed_tools)`，跑完（无论成功失败）
    /// 都要还原为原值；不修改 `AgentMode`（Normal/Plan/Bypass）本身。`profile` 为
    /// `model:` frontmatter 字段的解析结果，本版本仅保留字段、暂不消费。
    RunPromptScoped {
        text: String,
        allowed_tools: Option<Vec<String>>,
        profile: Option<String>,
    },
    /// 打开会话选择器
    OpenSessionPicker,
    /// 开启全新会话：自动保存当前会话到磁盘、分配新 session_id、清空内存状态。
    /// 对齐 Claude Code `/new` 语义：无二次确认弹窗（不阻塞用户）。
    StartNewSession,
    /// 直接恢复指定 session（session-id）
    ResumeSession(String),
    /// 打开配置设置面板（/config 命令触发）
    OpenSettingsDialog,
    /// 打开 AGENTS.md 记忆面板（/memory 命令触发）
    OpenMemoryDialog,
    /// 打开 Memory v3 clear-all 二级确认对话框（/memory clear-all 触发）。
    /// 与 `OpenMemoryDialog` 区分：用户必须再按 y/Enter 才执行清空，避免误操作。
    OpenMemoryClearAllConfirm {
        active_count: usize,
        superseded_count: usize,
    },
    /// 打开证据化自进化治理中心（/evolve 命令触发）。
    OpenEvolutionDialog,
    /// 打开 MCP server 管理面板（/mcp 命令触发）
    OpenMcpDialog,
    /// 打开 Skill 管理面板（/skills 命令触发）
    OpenSkillsDialog,
    /// 打开插件管理面板（/plugins 命令触发）
    OpenPluginsDialog,
    /// 打开统一资源中心（/extensions 命令触发）
    OpenExtensionsDialog,
    /// 打开可用 Agent 类型交互面板（/agents 命令触发）。
    /// TUI 使用 `defs` 渲染列表与详情；headless 使用 `fallback_text` 保持纯文本输出。
    OpenAgentsDialog {
        defs: Vec<wyj_core::AgentDefinition>,
        fallback_text: String,
    },
    /// 打开/定位子 Agent 聚合面板（/subagents 命令触发）。
    /// `Some(id)` 直接选中并展开该 id 的详情；`None` 只是请求打开面板
    /// （TUI 侧据此决定默认选中项；headless 不支持，提示改用 `subagent-trace` 子命令）。
    OpenSubAgentsPanel(Option<u64>),
    /// 打开/定位后台 Shell 任务面板（/shells 命令触发）。
    /// `Some(id)` 直接选中并展开该任务的实时输出；`None` 定位到最近一个任务
    /// （TUI 侧实现；headless 不支持面板）。
    OpenShellsPanel(Option<String>),
    /// 打开定时任务面板（/schedule 命令触发）。headless 不支持，提示改用
    /// `wyj-code schedule` CLI 子命令。
    OpenScheduleDialog,
    /// 切换鼠标捕获开关（/mouse 命令触发）。
    ///
    /// 只改运行期状态，不写回 `config.toml`。开启后滚轮进入应用（代价是拖选
    /// 复制需按住 Option），关闭后完全交还终端。
    ToggleMouseCapture,
}

#[derive(Debug, Clone)]
pub enum SubAgentControlAction {
    FollowUp(String),
    Interrupt,
    RetryLast,
}

/// 命令执行上下文
pub struct CommandContext {
    pub cwd: std::path::PathBuf,
    pub model: String,
    /// 聊天区**可用列数**，供 `/cost` 这类要排版的命令按终端宽度自适应。
    ///
    /// 0 = 未知（无 TTY、测试构造），命令走 `cost_view::DEFAULT_WIDTH`。
    /// 这里传的是内容宽度而非终端宽度——与 `tui::render::chat_content_width`
    /// 同源，否则表格算出的列数会比实际可渲染的宽，按终端宽排版就会被换行。
    pub terminal_width: usize,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 命中 prompt 缓存的输入 token（0.1x 计费，不含在 input_tokens 内）
    pub cache_read_tokens: u32,
    /// 写入 prompt 缓存的输入 token（1.25x 计费，不含在 input_tokens 内）
    pub cache_write_tokens: u32,
    /// 本会话累计模型推理次数。`/cost` 用它算「每次调用平均带多少上下文」——
    /// 调用次数才是 input 消耗的一阶乘数，只看单次 token 数会低估往返成本。
    pub api_calls: u32,
    /// 实际生效的 prompt cache 模式，取值见 `wyj_core::prompt_cache_state`。
    /// 2 = 曾开启但端点 400 已降级，`/cost` 据此解释「为什么缓存是 0」。
    pub prompt_cache_state: u8,
    pub context_window: u32,
    pub estimated_tokens: u32,
    /// 最近一次模型请求的上下文占用分解。与自动压缩的触发计算**同源**
    /// （`wyj_core::compact::ContextAudit::from_request`），`/context` 面板
    /// 直接展示它——旧实现里 `/cost` 的百分比用的是只算 messages 的
    /// `estimated_tokens`，与压缩阈值差一万多 token，对不上号。
    /// `None` = 本会话还没发起过模型请求。
    pub context_audit: Option<wyj_core::compact::ContextAudit>,
    /// 最近一次模型往返的时延画像（TTFT / 总耗时 / 重试开销）。
    /// 由 `Agent` 在每次 `MessageStop` 时写进 session，TUI 再透传到面板。
    /// `Default`（全 0）= 还没发起过模型请求，面板据此隐藏整行。
    pub last_latency: wyj_core::session::TurnLatency,
    /// 本会话自动压缩发生次数。
    pub compact_count: u32,
    /// 本会话 tool-result context editing 的 `(清理条数, 释放的估算 token)`。
    pub context_edit: (u32, u32),
    pub home_dir: std::path::PathBuf,
    /// 子 Agent 累计 token 用量（与主会话分开统计，/cost 单列；headless 传 0）
    ///
    /// 数据源是 `Session::sub_usage()`——由 `SubAgentTool` 在子 Agent 结束时
    /// 把**自己那层 Session** 的计数加进共享累加器、主 Agent 每批工具执行后
    /// 抽干入账。不用 TUI 的 `SubAgentEvent::Usage` 累加是因为那条路只带
    /// input/output，**不含 cache_read**，而子 Agent 的开销大头恰恰是缓存
    /// 命中；且它只活在内存里，`/resume` 后归零。
    pub sub_input_tokens: u32,
    pub sub_output_tokens: u32,
    /// 子 Agent 命中的缓存 token（0.1x 计费）。
    pub sub_cache_read_tokens: u32,
    /// 子 Agent 写入的缓存 token（1.25x 计费）。
    pub sub_cache_write_tokens: u32,
    /// 合并全局+项目配置、过滤禁用条目后实际会连接的 MCP server 数量
    /// （由调用方用 `wyj_store::mcp_install::effective_mcp_servers` 算好传入，
    /// `crates/commands` 本身不依赖 `wyj-store` 以保持 crate 边界干净）。
    pub effective_mcp_count: usize,
    /// 已启用插件贡献的 agent 定义文件/目录路径（同上，由调用方用
    /// `wyj_store::plugin_install::enabled_plugin_agent_paths` 算好传入），
    /// 供 `/agents` 展示时一并列出插件贡献的自定义 agent 类型。
    pub plugin_agent_paths: Vec<std::path::PathBuf>,
    /// 当前 Hooks 是否启用（来自 CLI `--no-hooks`）。
    pub hooks_enabled: bool,
    /// 运行时发现的动态命令（Skill / `.claude/commands`），供 `/help` 追加展示为
    /// "自定义命令"分组。`(name, description, usage)`，由调用方在构造本结构体前用
    /// `cmd_registry.list()` 过滤 `is_dynamic()` 算好传入。
    pub dynamic_commands: Vec<(String, String, String)>,
    /// resume 后第一次请求测得的 system prompt 漂移（见
    /// [`wyj_core::prompt_fingerprint`]）。
    ///
    /// `None` = 还没发起请求，或落盘没有指纹可比（升级前的老会话）。
    /// `Some(空)` = 比过了、完全一致——**与 `None` 是两种不同的结论**，
    /// `/context` 必须把它们分开显示：一个是"不知道"，一个是"确认没变"。
    pub prompt_drift: Option<wyj_core::prompt_fingerprint::PromptDrift>,
}

/// Slash 命令 trait
#[async_trait]
pub trait Command: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn usage(&self) -> String;
    /// 是否为运行时动态发现的命令（Skill / `.claude/commands`），而非内置静态命令。
    /// 默认 false；仅动态命令实现（如 `SkillCommand`）覆盖为 true。
    fn is_dynamic(&self) -> bool {
        false
    }
    /// 该命令的执行是否必须离开 UI 主循环。
    ///
    /// 标 true 的命令会 spawn 子进程或走网络（`gh`、`open`），**完成时间不可预期**：
    /// 认证挂起、网络重试、大仓库的 `gh pr diff` 都可能拖到几十秒。TUI 主循环是
    /// 一个同时负责重绘和读按键的 task，inline `await` 它等于让界面停在那一帧——
    /// 不重绘、不 `event::poll()`，ESC / Ctrl+C 全部失效（见 `crates/tui/CLAUDE.md`
    /// 第 19 条）。因此 TUI 见到 `true` 的命令会丢进后台 task，跑完用
    /// `AgentEvent::BackgroundCommandDone` 把结果发回主循环渲染。
    ///
    /// 约定：后台执行的命令只能返回 `Output` / `OutputPlain` / `None`。
    /// 返回 `RunPrompt`（要起新回合）的命令不能标 true——回合的 spawn 逻辑
    /// 只存在于 TUI 主循环里，把结果发回事件通道无法起回合。
    fn needs_background(&self) -> bool {
        false
    }
    /// 该命令能否在 Agent 回合进行中（`is_thinking`）**立即执行**。
    ///
    /// 背景：Agent 忙碌时按 Enter 提交的内容默认进 `pending_queue` 排队投递给模型
    /// （`tui::app` 的 Enter 分支）。而 `/model` 这类命令根本没有"内容"可投递——
    /// 排给模型等于把字面的 `/model` 当成一句用户消息发出去，右下角还会冒出
    /// "1 条已排队"。TUI 因此在 `is_thinking` 时先查本方法：为 true 的命令照常
    /// dispatch，其余维持排队。
    ///
    /// **默认 false，且必须保持默认 false**——误标一条的代价是让它在回合中途起新
    /// 回合 / 改写正在被回合持有的 session：
    ///
    /// - 返回 `RunPrompt` 的命令（`/bug`、`/review`、`/pr-comments`）会起新回合；
    /// - 返回 `ClearHistory` / `StartNewSession` / `Rewind` / `BranchSession` /
    ///   `ResumeSession` / `CompactHistory` 的命令会改会话历史或当前会话身份；
    /// - 需要 `begin_background_task` 的命令（`/checkpoint`、`/evolve` 的确认动作）
    ///   会把 `AppState::current_task` 覆盖成自己的 handle，正在跑的回合从此
    ///   **失去 ESC 中断能力**（task 仍在跑，但句柄已被丢弃）；
    /// - 返回 `Output` 却读 `ctx.estimated_tokens` / `context_audit` 的命令
    ///   （`/cost`、`/context`）在忙碌时拿不到 session 锁，只能拿到降级 ctx，
    ///   数据会静默失真。
    ///
    /// 只有"打开面板 / 切纯运行期开关"这类既不碰 session 也不起回合的命令才覆写为
    /// true。新增命令时按上面四条逐一排除，不要靠"看起来像面板"来判断。
    fn runs_while_busy(&self) -> bool {
        false
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult>;
}

pub struct CommandRegistry {
    commands: HashMap<String, Arc<dyn Command>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    pub fn register(&mut self, cmd: Arc<dyn Command>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        self.commands.get(name)
    }

    /// 该命令是否声明了 [`Command::needs_background`]（TUI 据此丢后台 task）。
    pub fn needs_background(&self, name: &str) -> bool {
        self.commands
            .get(name)
            .is_some_and(|cmd| cmd.needs_background())
    }

    /// 该命令是否声明了 [`Command::runs_while_busy`]（TUI 据此在回合进行中照常执行，
    /// 而不是把 `/xxx` 当成一条消息排队投递给模型）。
    pub fn runs_while_busy(&self, name: &str) -> bool {
        self.commands
            .get(name)
            .is_some_and(|cmd| cmd.runs_while_busy())
    }

    /// 从 "/foo bar" 里取出命令名（不含 `/`、不含参数）。非 slash 输入返回 `None`。
    pub fn command_name(input: &str) -> Option<&str> {
        let rest = input.trim().strip_prefix('/')?;
        let name = rest.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    pub fn list(&self) -> Vec<&Arc<dyn Command>> {
        let mut v: Vec<_> = self.commands.values().collect();
        v.sort_by_key(|c| c.name());
        v
    }

    /// 解析并执行 "/command args"，返回 None 表示不是 slash 命令
    pub async fn dispatch(
        &self,
        input: &str,
        ctx: &CommandContext,
    ) -> Option<Result<CommandResult>> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let rest = &trimmed[1..];
        let (name, args) = match rest.find(char::is_whitespace) {
            Some(pos) => (&rest[..pos], rest[pos + 1..].trim()),
            None => (rest, ""),
        };
        if name.is_empty() {
            return None;
        }
        match self.commands.get(name) {
            Some(cmd) => Some(cmd.run(args, ctx).await),
            None => Some(Err(anyhow::anyhow!(
                "{}",
                wyj_i18n::tr_fmt("command.unknown", &[("name", name)])
            ))),
        }
    }

    /// 返回以 prefix 开头的 (命令名, 描述) 对（用于补全）
    pub fn complete(&self, prefix: &str) -> Vec<(String, String)> {
        let p = prefix.trim_start_matches('/');
        self.commands
            .iter()
            .filter(|(k, _)| k.starts_with(p))
            .map(|(k, v)| (format!("/{k}"), v.description()))
            .collect()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod busy_whitelist_tests {
    use crate::builtin::standard_registry;

    /// 回合进行中可执行的白名单是**精确集合**，不多不少。
    ///
    /// 多一条的危险见 [`Command::runs_while_busy`] 的四条排除理由；少一条则是
    /// 用户现场那条 `/model`：命令被当成消息排队给模型。这条断言把"少"和"多"
    /// 一起钉死，新增内置命令时会被迫在这里表态。
    #[test]
    fn busy_whitelist_is_exactly_the_panel_commands() {
        let reg = standard_registry();
        let mut actual: Vec<&str> = reg
            .list()
            .iter()
            .filter(|c| c.runs_while_busy())
            .map(|c| c.name())
            .collect();
        actual.sort_unstable();
        // evolve / resume 刻意**不在**白名单：evolve 的确认动作要建工作区快照并
        // spawn 后台 task，会把 AppState::current_task 覆盖成自己的 handle，正在跑
        // 的回合从此失去 ESC 中断能力；resume 会切走当前会话身份。
        //
        // computer 也不在：它跑一串系统调用（截图 / 窗口枚举 / Event Tap 快照），
        // 必须走 `needs_background` 的后台路径，而两者互斥（见下一条测试）。
        let mut expected = vec![
            "agent-control",
            "agents",
            "config",
            "cwd",
            "doctor",
            "effort",
            "extensions",
            "help",
            "hooks",
            "mcp",
            "memory",
            "mode",
            "model",
            "mouse",
            "plugins",
            "profile",
            "quit",
            "schedule",
            "sessions",
            "shells",
            "skills",
            "subagents",
        ];
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    /// 四条排除理由逐条钉死：不许把会起新回合 / 改会话 / 读 session 统计 /
    /// 抢 `current_task` 的命令挪进白名单。
    #[test]
    fn no_turn_spawning_or_history_mutating_command_is_whitelisted() {
        let reg = standard_registry();
        for name in [
            // 起新回合
            "bug",
            "review",
            "pr-comments",
            // 改会话历史或当前会话身份
            "clear",
            "compact",
            "new",
            "checkpoint",
            "rewind",
            "branch",
            "resume",
            // 读 session 统计，忙碌时只能拿到降级 ctx
            "cost",
            "context",
            // 确认动作要 begin_background_task，会覆盖 current_task 句柄
            "evolve",
            // 写文件，与回合并行可能撞
            "init",
        ] {
            assert!(
                !reg.runs_while_busy(name),
                "/{name} 不得在回合进行中执行，理由见 Command::runs_while_busy"
            );
        }
    }

    /// 白名单里的命令都不能同时是后台命令：`needs_background` 的执行路径会
    /// `begin_background_task` + 覆盖 `current_task`，与"回合继续跑"直接冲突。
    #[test]
    fn busy_whitelist_never_overlaps_needs_background() {
        let reg = standard_registry();
        for cmd in reg.list() {
            if cmd.runs_while_busy() {
                assert!(
                    !cmd.needs_background(),
                    "/{} 同时标了 runs_while_busy 与 needs_background",
                    cmd.name()
                );
            }
        }
    }
}
