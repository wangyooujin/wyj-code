# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build --release            # 构建 release 版本 → target/release/wyj-code
cargo run                        # 启动 TUI 模式
cargo run -- --headless          # 启动 headless REPL 模式
cargo run -- -p "your prompt"    # 单次问答（不启动 TUI）
cargo run -- --config-status     # 查看当前配置和 API Key 状态
cargo run -- --cwd <dir>         # 指定工作目录（默认当前目录）
cargo run -- --plan              # 以 Plan 模式启动（仅只读工具）
cargo run -- --bypass-permissions # 以 Bypass 模式启动（跳过权限确认）
cargo run -- --no-hooks           # 禁用 Hooks 自动化系统
cargo run -- -c / --continue     # 恢复上一次会话
cargo run -- --resume <id>       # 恢复指定会话 ID
cargo run -- subagent-trace <session_id> [<sub_id>] [--json]  # 查看落盘的子 Agent 执行轨迹（无 sub_id 列出概览）

./build.sh                       # 等同 cargo build --release
./build.sh package               # 打包到 dist/<binary>-<version>-<platform>
./build.sh install               # 安装到 ~/.local/bin/wyj-code
./build.sh uninstall             # 卸载二进制；加 --purge 二次确认后彻底删除 ~/.wyj-code/
./build.sh cross linux-x86_64    # 交叉编译（支持 linux-x86_64, linux-aarch64, macos-*）
./build.sh release               # 交互确认版本号后自动 bump Cargo.toml + commit + tag + push（触发 GitHub Actions Release）
```

GitHub Release 压缩包（`.github/workflows/release.yml` 产出的 tar.gz/zip，不同于 `./build.sh package`
产出的裸二进制）额外内置 `install.sh`/`install.bat`（仓库根目录）两个一键安装脚本，解压后运行即可
装到当前用户目录并自动配置 PATH，无需 sudo/管理员权限。

## Test & Lint

```bash
cargo test                       # 全量测试
cargo test -p wyj-core           # 指定 crate 测试
cargo fmt                        # 格式化（max_width = 100，见 rustfmt.toml）
cargo clippy                     # lint
```

## Git 提交约定

- **不要**在 commit message 中添加 `Co-Authored-By: Claude <noreply@anthropic.com>` 或任何指向 Claude / Anthropic 的署名 trailer。仓库历史已清理过一次以移除这些 trailer——它们会让 GitHub 在 Contributors 列表中显示 "Claude"，本仓库希望仅展示真实人类作者。
- commit message 用简体中文，首行 `type: 简述`，空行后列要点。
- 用户说"提交代码并 push"时：`git add -A` → `git commit` → `git push`。仅当用户要求时才提交/推送；若在默认分支上，先建分支（本项目 master 为单作者主线，按用户指示可直接提交）。

## Slash 命令约定

- **每新增一条 `/xxx` slash 命令，必须同步在 `/help` 输出中注册该命令的说明**，包括：命令名、用途简介、用法/参数（若有）、示例（可选）。`/help` 是用户发现命令的唯一入口，未注册的命令等同于对用户不可见。
- 注册位置：`/help` 的输出由 `crates/commands/src/builtin.rs` 中 `HelpCmd` 读取的 i18n 模板 `help.body` 渲染（**不是**动态枚举注册表），因此新增命令时必须同步在 `crates/i18n/locales/{en,zh}.yml` 的 `help.body` 模板里追加该命令的说明行，保持与现有条目相同的格式与缩进。
- 同步要求：新增命令的 PR/提交里就应包含对应的 `/help` 条目，不要留到后续补；若临时不希望暴露（如调试命令），应在 `/help` 中显式标注「内部/调试」而非直接省略。
- 命令文案需走 i18n（`tr()` key），与 `/help` 其余条目一致，不可硬编码中文。
- **动态命令（Skill / `.claude/commands`）是上述约定的例外**：这类命令运行时从磁盘发现、因人而异，不可能写死进静态 `help.body` 模板。`Command` trait 的 `is_dynamic()` 默认方法（仅 `SkillCommand` 覆盖为 `true`）标记这类命令，`CommandContext.dynamic_commands` 由调用方在构造 ctx 前用 `cmd_registry.list()` 过滤好传入，`HelpCmd` 渲染完静态模板后在末尾追加一个「自定义命令」分组（i18n key: `help.custom_commands_header`）。两者互不冲突，新增静态内置命令仍必须遵守上面的硬性约定。
- **Skill / 自定义命令加载**（`commands::skill::load_skills`）：六层合并链，同作用域内真实 Claude Code 路径覆盖 wyj-code 自造路径——`内置 → 全局 ~/.wyj-code/skills → 全局真 CC ~/.claude/commands → 已启用插件贡献路径(先到先得) → 项目 .wyj/skills → 项目真 CC .claude/commands`（最高优先级）。frontmatter 支持 `description`/`argument-hint`/`allowed-tools`/`model` 四字段（复用 `core::frontmatter::parse`，与 `agent_def.rs` 共用同一套轻量 `key: value` 解析器）；`allowed-tools` 执行期通过 `CommandResult::RunPromptScoped` 让调用方把 `PermissionMode` 临时收紧为 `Allowlist`，跑完这一轮（含 ESC 中断，TUI 侧靠 `RestorePermissionOnDrop` 的 RAII 兜底）自动还原，不修改 `AgentMode` 本身；TUI scoped execution 会按 `model` 临时使用指定 Profile；Skill loader 递归支持 `/namespace:cmd`。

## Configuration

配置文件：`~/.wyj-code/config.toml`，API Key 优先读取环境变量 `WYJ_CODE_API_KEY`。

```toml
provider = "anthropic"       # 或 "openai"
model = "claude-opus-4-8"
plan_model = ""              # Plan 模式专用模型，留空则使用 model
exec_model = ""              # Exec/Bypass 模式专用模型，留空则使用 model
base_url = ""                # 留空使用供应商默认端点
max_tokens = 8192
context_window = 200000
vision = true                # 模型是否支持图片输入；false 时图片降级为占位文本（防非多模态端点 400）
# prompt_cache = false       # Anthropic-compatible 第三方端点可显式关闭 cache_control / beta header
# openai_stream_options = false # OpenAI-compatible 第三方端点可显式关闭 stream_options.include_usage
# thinking_budget = 8000     # extended thinking 预算 token；不写/0 = 关闭（思考计入 output 计费）
# interleaved_thinking = true # 工具调用轮之间允许交错思考（budget 开启时生效）
log_level = "warn"           # 调试时设为 "debug"
language = ""                # "en"/"zh"，留空自动检测系统 locale
search_provider = "tavily"   # WebSearch 搜索 provider（目前支持 tavily）
search_api_key = ""          # WebSearch API Key，优先读环境变量 WYJ_CODE_SEARCH_API_KEY；未配置则 WebSearch 工具不注册（模型看不到）

[subagent]
default_profile = ""         # 子 Agent 默认 Profile 名，留空沿用主 Agent 当前分组
explore_profile = ""         # 内置 Explore 类型专用 Profile 名（配便宜模型），留空回退 default_profile
trace_enabled = true         # 是否把子 Agent 完整执行轨迹落盘（供跨会话查看，见下方 SubAgent 节）
trace_max_bytes_per_agent = 262144  # 单个子 Agent trace 文件字节上限（默认 256KB），超限静默停写

[[mcp_servers]]
name = "my-server"
transport = "stdio"
command = "/path/to/server"
args = ["--flag"]
```

**CLAUDE.md 记忆机制**（对齐真实 Claude Code，`core::claude_md::ClaudeMdLoader`）：查找范围为全局 `~/.claude/CLAUDE.md`（复用真实 Claude Code 的路径） + 从 git 仓库根到 cwd 的祖先链（找不到 `.git` 则只用 cwd 本身）；每级目录内 `CLAUDE.md`/`CLAUDE.local.md` 都存在就都读（local 视作个人覆盖追加，不提交 git），两者都不存在则回退读 `AGENTS.md`；支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。每轮对话开始时重新读盘，以 `<system-reminder>` 包装拼进当轮 **system prompt 末尾**（不注入 user 消息、不进历史，配合 prompt caching 避免跨轮累积；文件不变时字节级稳定、缓存可命中）。工具（Read/Edit/Write/Glob/Grep）触达新子目录时，若该目录有 CLAUDE.md 系文件且本会话未展示过，会在 `agent.rs` 的工具执行循环里追加到 system 末尾（`ClaudeMdLoader::maybe_dir_reminder`，按目录去重）。`/init` 触发一次真正的 agent 回合（`CommandResult::RunPrompt`）去探索项目并生成/合并更新 CLAUDE.md，而非静态模板写文件；`/memory` 打开 TUI 面板列出当前会话适用的全部文件，选中后挂起 TUI 唤起 `$EDITOR` 编辑，同时暴露 auto-memory（跨会话记忆提取）开关与索引入口（`Config.auto_memory_enabled`）。不再兼容旧版 `WYJ.md`。

**模型侧提示词**（`core::prompts` + `tools::descriptions`）：主 system prompt、模式追加段（Plan/non-interactive）、子 agent 内置提示、compact 结构化摘要模板、记忆提取提示、全部工具描述均为**英文原创常量**，不走 i18n（模型行为不应随 locale 漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复）。system prompt 结构 = 英文主提示 + `<env>` 环境块（cwd/平台/日期/model 等会话内稳定字段，进 prompt 缓存）+ 跨会话记忆快照 + CLAUDE.md reminder；**git 状态快照**（分支/porcelain/近 5 commit）走会话首轮 user 消息注入（`Agent::with_git_snapshot`），因其每轮可能变、进 system 会击穿缓存。

**`/config`**：TUI 内 `/config` 打开设置面板（`OpenSettingsDialog`），现仅剩 `log_level`/`language` 两个字段（`SETTINGS_FIELD_COUNT = 2`，`crates/tui/src/app.rs`）；调用相关字段（`provider`/`model`/`base_url`/`api_key`/`plan_model`/`exec_model`）已迁移到 `/model` 的 Profile 分组管理器（`ProfileDialog`），按具名 Profile 管理，不再挂在 `/config` 下。`language` 留空则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文。当前 i18n 仅覆盖用户可见 UI 文案（TUI 对话框、slash 命令输出、CLI --help/--config-status）；模型侧提示词（system prompt、工具描述等）为英文常量不走 i18n（见上方"模型侧提示词"节），工具内部错误消息等仍为中文，待后续阶段迁移。

**Hooks 自动化系统**（对齐真实 Claude Code 的 `.claude/settings.json` hooks）：生命周期钩子配置来源与 CLAUDE.md 同一哲学，复用 `~/.claude/` 与项目 `.claude/` 路径。三源合并顺序：`~/.claude/settings.json` → `<git-root>/.claude/settings.json` → `<git-root>/.claude/settings.local.json`（后者追加不覆盖，local 文件供个人临时覆盖、不提交 git）。仅解析 settings.json 的 `hooks` 键，其它顶层键（真 CC 的 `permissions`/`env` 等）宽容忽略。支持 4 个事件：`PreToolUse`（工具执行前，可 block/approve）、`PostToolUse`（工具执行后，可追加反馈）、`UserPromptSubmit`（用户提交后进入推理前，可 block/追加上下文）、`Stop`（回合结束前，可继续下一轮）。每个 hook 是执行任意 shell 的 `command`，stdin 注入 JSON payload（含 session_id/cwd/event/tool_name/input/response），exit 2 表示 block（stderr 为原因），stdout JSON 可表达更丰富的 `decision`/`reason`/`additionalContext`/`continue`。默认超时 60s，可用 `--no-hooks` 完全禁用；首次检测到非空配置时打印一次性安全提示。实现：`core::hooks::HookRunner` 负责加载/合并/执行，`core::agent::Agent` 在 4 个精确插入点调用，`cli/main.rs` 构造并装配给主 Agent 与 TUI 重建路径，子 Agent 工厂不装配。

## Architecture

这是一个 Rust workspace，单一 `wyj-code` 二进制，零遥测。各 crate 职责：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session、HistoryStore、MemoryStore、ClaudeMdLoader、上下文压缩 |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/BashOutput/KillShell/Glob/Grep/WebFetch/WebSearch/TodoWrite/AskQuestion/ExitPlanMode/SubAgent/Computer；WebSearch 仅在配置 search_api_key 时注册，Computer 仅 macOS/Windows 编译且需 vision+Anthropic profile；descriptions.rs 英文工具描述、textutil.rs 安全截断、bash_session.rs 后台任务单例）|
| `crates/computer` | `wyj-computer` | computer-use 系统层：`xcap` 截图 + `enigo` 输入合成（两者内部已各自分派 macOS/Windows，本 crate 不再手写 target_os 分支），坐标缩放数学（`scale` 模块，平台无关可测）；仅 `[target.'cfg(any(macos, windows))']` 拉取真实依赖，其余平台编译进桩实现 |
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`rust-i18n` 封装，`en`/`zh` 内嵌 YAML）与运行时语言切换（`tr()`/`set_locale()`）|
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/store` | `wyj-store` | MCP/Skill/Plugin 配置管理数据层：安装元数据 lockfile、MCP registry HTTP client、skill/plugin git marketplace client、install/upgrade/uninstall/enable 编排；只管配置写入（config.toml/`.wyj/mcp.toml`/lockfile），绝不 shell out 执行依赖安装（如 `npm install`）|
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

### 核心数据流

1. **Tool trait**（`core::tool`）：所有工具实现 `async fn run(input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>`，由 `tools::ToolRegistry` 统一管理。
2. **Agent 推理循环**（`core::agent::Agent::run_turn`）：流式接收 LLM 输出 → 累积工具调用 → 执行（`Tool::parallel_safe()` 为 true 的调用如 Agent 用 `join_all` 单任务内并发，其余相互保持顺序但与并发组同时进行，结果按原始下标保序回填，见 `exec_tool_call`）→ 将结果追回 session → 继续直到 `stop_reason != tool_use`。注意 `ToolContext` 是 `Send + Sync` 的（旧注释"非 Send"已过时）。
3. **上下文压缩**（`core::compact`）：估算 token 数按 CJK/非 CJK/图片分别启发式计算；触发缓冲为 `min(40000, max(4000, context_window / 5))`，当 `estimated > context_window - buffer` 时调用 LLM 生成摘要替换旧消息，保留最近 6 条。
4. **跨会话记忆**（`core::memory::MemoryStore`）：每轮对话结束后 `tokio::spawn` 后台提取记忆，写入 `~/.wyj-code/memory/<project-id>/`；下次启动时读取 MEMORY.md 索引注入 system prompt；可被 `Config.auto_memory_enabled`（`/memory` 面板切换）关闭。
5. **CLAUDE.md 注入**（`core::claude_md::ClaudeMdLoader`）：`Agent::run_turn_with_injection` 每轮开始时调用 `turn_reminder()` 重新读盘，把全局 + 祖先链的 CLAUDE.md 系内容包成 `<system-reminder>` 前插进当轮 user 消息；工具执行循环里对新触达目录调用 `maybe_dir_reminder()` 做子目录动态加载。详见上方 Configuration 节。
6. **MCP 桥接**（`mcp::bridge`）：连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。
7. **SubAgent 多 agent 编排**（对齐 Claude Code）：类型体系 = 内置 general-purpose/Explore(只读)/Plan（`core::agent_def::builtin_defs`）+ 自定义定义文件（`~/.claude/agents/*.md` 与项目 `.claude/agents/*.md`，frontmatter: name/description/tools/model，model 引用 **Profile 名**，同名后者覆盖，`load_agent_defs`）。`tools::SubAgentTool`（工具名 "Agent"，参数 subagent_type/description/prompt/run_in_background）把每个子 Agent 整体 `tokio::spawn` 并登记进 `tools::agent_hub::SubAgentHub`（进程级单例：id 分配、`Semaphore` 并发上限 8、`abort_foreground`/`abort_all`/`wait_background`）；子 Agent 挂 `with_tool_callback`/`with_usage_callback` 把内部工具事件与 token 用量以 `SubAgentEvent` 汇入 Hub 的 event_cb。前台调用 await oneshot 结果；`run_in_background: true` 立即返回，完成结果包成 system-reminder 经注入通道（主 Agent 忙）或 `AppState.pending_bg_reminders`（空闲，下轮起手 merge）送达。TUI 展示：每个 agent 的 ToolCall 行绑定 sub_agent_id（Started 事件 FIFO 配对）并在运行期间画动态 ⎿ 状态行（耗时/tokens/工具数/当前工具），ToolResult 展开（Ctrl+O）时先列内部工具调用明细；有运行中 agent 时显示底部聚合面板（`render::draw_sub_agents_panel`）。ESC 中断只 abort 前台子 Agent，后台不受影响；TUI 退出时 `abort_all`，headless `-p` 结束前 `wait_background`。子 Agent 模型解析优先级：def.model(Profile 名) → `[subagent].explore_profile`(仅 Explore) → `[subagent].default_profile` → 主 Agent 当前分组（`cli::make_sub_agent_factory`）。子 Agent 一律不注册 Agent（防嵌套）/AskQuestion/ExitPlanMode/TodoWrite，并继承 Plan 白名单交集。`/agents` 列出全部类型；`/cost` 单列子 Agent 用量。

    **子 Agent 执行轨迹持久化**（v1.2）：`SubAgentHub::emit()` 是 TUI/headless 唯一汇聚点，`with_trace(sessions_dir, session_id, max_bytes_per_agent)`（`SubAgentCfg::trace_enabled` 默认开启）在此接入一个专职后台写手（`tools::trace::TraceWriter`，内部 mpsc channel 串行 append，调用方零阻塞），把 `SubAgentEvent` 转成落盘用 `TraceEvent`（`ToolStart`/`ToolEnd` 补全完整 input JSON / output 全文，`textutil::truncate_str`/`truncate_head_tail` 截断，超过 `trace_max_bytes_per_agent`（默认 256KB）静默停写）写入 JSONL：`~/.wyj-code/sessions/<session_id>.subagents/a<id>.jsonl`（与 `SessionFile` 不共享结构，不污染 `api::types::Message`/`ContentBlock`）。`Started` 事件带 `parent_tool_use_id`（经 `Tool::run_with_meta`/`ToolCallMeta` 从 `exec_tool_call` 的 `tool_use_id` 透传，仅 `SubAgentTool` override，其余工具零改动）关联回会话消息里的 `ContentBlock::ToolUse.id`。TUI 启动/`-c`/`--resume` 统一路径下 `app::reload_persisted_sub_agents` 扫描该目录回灌 `AppState.sub_agents`（只填摘要级字段，全文仍留在磁盘，避免长会话常驻内存暴涨），天然复用现有面板与 `/subagents [id]` 命令（无参数定位最近一个，带 id 直接跳转，`headless_unsupported` 提示改用 CLI 子命令）。headless 侧 `wyj-code subagent-trace <session_id> [<sub_id>] [--json]`（`cli::run_subagent_trace_cmd`）纯读打印落盘内容，无 sub_id 列出该会话全部子 Agent 概览。
8. **会话中补充消息注入**：TUI 场景下 Agent 忙碌时用户按 Enter 提交的新消息不会打断当前轮次，而是进入 `AppState.pending_queue`，由 `core::agent::Agent::run_turn_with_injection`（而非普通 `run_turn`）在每轮工具调用往返边界排空注入队列、合并进当前或续接的 user 回合。注入负载携带 `InjectionKind`（`UserMessage` 触发 UI 的 pending_queue 回放；`SystemReminder` 用于后台子 Agent 结果，对用户消息队列不可见）。headless/`-p` 单次模式仍走普通 `run_turn`，不支持中途注入。
9. **统一 Extensions 资源平台**（`wyj-store::extensions` + `/extensions` + `wyj-code extensions`）：统一读取 Skill/MCP/Plugin lockfile 投影，lockfile v2 新增跨类型 `extensions` 索引，同时兼容旧 v1 数组；支持 `list`/`doctor`/`migrate`/`install`/`upgrade`/`enable`/`disable`/`remove` 和 `--json`。项目原生路径优先于项目插件和旧 wyj 路径，全局同理；`.mcp.json`/`~/.claude.json` 的 `mcpServers` 可显式迁移到 wyj 配置，原文件保留。MCP 支持 stdio 与 Streamable HTTP（URL/环境变量引用 headers），工具名稳定映射为 `mcp__<server>__<tool>`。Skill loader 递归支持 `namespace:name` 命令。插件仍通过 `.claude-plugin` manifest 和 marketplace 安装；commands/skills、agents、MCP contributions 是 v1.2.2 支持边界，hooks/themes 仍未纳入。资源变更的持久化状态在下一次 Agent 回合边界生效，CLI 输出会明确提示这一点。
10. **TUI 聊天区渲染**：详见 `crates/tui/CLAUDE.md`（主循环永久运行在 `Viewport::Fullscreen`，输入框/状态栏贴住窗口底部、全部历史应用内滚动，仅在触达 `crates/tui/` 下文件时按需加载）。
11. **Hooks 生命周期自动化**（v1.1）：详见 Configuration 节 "Hooks 自动化系统"。执行点：`core::agent::Agent::run_turn_with_injection`（`UserPromptSubmit` 在 `git_snapshot` 注入后、`turn` 循环前；`Stop` 在 `!has_tool_calls && !got_injection` 分支、`break` 前）、`core::agent::Agent::exec_tool_call`（`PreToolUse` 在 `is_allowed`/`confirm_tool` 前；`PostToolUse` 在结果组装后、回调 `ToolEvent::End` 前）。子 Agent 工厂 `cli::make_sub_agent_factory` 不装配 `HookRunner`，确保嵌套子任务不触发用户级 hooks。
12. **Computer-use 桌面 GUI 控制**（v1.3.0，`tools::computer::ComputerTool`，`#[cfg(any(target_os = "macos", target_os = "windows"))]`）：`ApiTool` 走 `wyj_api::types::ToolDefinition.native: Option<NativeToolSpec>` 分支序列化，**双模式**由 `ComputerTool::new(max_dim, native: bool)` 的 `native` 参数决定，`run()` 的动作分派逻辑两种模式完全一致，只是对外声明方式不同：
    - **native=true**（原生模式）：声明为 Anthropic 原生 `computer_20251124` 工具——`{type, name, ...extra}`，无 description/input_schema，provider 层按 `native.beta` 追加 `anthropic-beta: computer-use-2025-11-24`（见 `api::anthropic::build_api_tool`/`collect_beta_header`）。仅官方 api.anthropic.com 认得这个空 schema 工具类型（Claude 训练时习得的调用约定）。
    - **native=false**（custom 模式）：声明为普通 custom 工具，`description`/`input_schema` 是真正发给模型的内容——`descriptions::computer_custom_description(target_width, target_height)` 动态嵌入实际下采样分辨率，完整列出全部 action 字段（无 Claude 那层内置训练兜底，必须自解释）。任何具备基本工具调用能力的模型都能按标准协议使用，用于第三方 Anthropic 协议兼容端点（MiniMax/GLM/Kimi 等）。
    - 注册门控（`cli::register_computer_tool_if_enabled`，main.rs 初始 registry 与 `/model` 重建 `rebuild_fn` 两处调用）：平台 `#[cfg]` + `Profile.vision` + `provider == Anthropic`（Messages API 协议本身才有 tool_result 内嵌图片的回传通路；OpenAI Chat Completions 的 `tool` 角色不支持图片，`openai.rs` 会把截图降级成占位文本，注册了也是名存实亡）缺一不注册；是否用原生声明另由 `Profile::is_official_anthropic_endpoint()` 决定（`provider == Anthropic` 且 `base_url` 为空或等于官方地址，与 `effective_prompt_cache()` 共用同一判定）——**教训**：早期版本把"是否注册"和"native == Anthropic"这两件事合并成一次判断，导致 MiniMax 这类走 `provider = "anthropic"` + 自定义 `base_url` 接入的第三方端点，要么被误判成官方端点收到无 schema 的原生工具直接 400，要么被整体拒绝注册、功能完全不可用；现在拆成"注册与否看协议、原生与否看端点"两层判断，两种情况都能正确工作（详见 `doc/plan/v1.3.0-plan.md`）。子 Agent 工厂不注册（与 Agent/AskQuestion 一致）。
    - 系统层 `wyj-computer`：仅截取主显示器，物理像素按 `scale::fit_within(max_dim)`（默认 `computer::DEFAULT_MAX_DIM = 1280`，与 Anthropic 官方博客推荐的默认分辨率一致）下采样后编码 PNG，`ToolResult::with_parts` 携带 `ToolResultPart::Image` 回传（复用 Read 工具已验证的图片回传链路，见下方 Read 相关说明）；模型给出的坐标基于下采样后的目标分辨率，执行前经 `scale::CoordScaler::to_physical` 换算回物理像素，`cursor_position` 动作反向经 `to_target` 换算回模型坐标系。
    - **`zoom` 动作**（提升识别准确率）：`wyj_computer::capture_region`/`scale::clamp_region` 截取主屏后裁剪到指定物理像素矩形，只在裁剪结果仍超过 `max_dim` 时才下采样——多数"放大看细节"场景裁剪区域远小于 `max_dim`，因此有效分辨率远高于全屏缩略图里的同一块内容，用于看清全屏截图下采样后会糊掉的密集数字表格/小字。`ComputerTool::do_zoom` 解析 `region: [x0,y0,x1,y1]`（目标坐标空间，四周自动加 `ZOOM_PADDING_RATIO=0.12` 余量防止切边），归入只读 action（`is_read_only_action`），不需权限确认，且和 `screenshot` 一样重置连续动作计数。原理依据 2025-2026 GUI agent 研究（RegionFocus/UI-Zoomer/GUI-Eyes 等，测试时动态裁剪放大可带来两位数百分点的 grounding/OCR 准确率提升）与 Anthropic 官方 computer-use 博客的"按需请求细节而非提高全屏分辨率"建议；`wyj_core::prompts::COMPUTER_USE_HINT` 与 `descriptions::computer_custom_description()` 都教模型"读数字前先 zoom，别猜"。
    - 权限：`screenshot`/`cursor_position`/`wait` 只读放行，其余变更类动作（click/drag/key/type/scroll/move）`needs_permission=true` 走既有 `confirm_tool` 弹窗；「始终允许」对 computer 走**会话内放行**而非跨会话持久化（`tools::ctx::SESSION_SCOPED_TOOLS`/`ToolCtx.session_allowed`，不写 `allowed_tools.json`，TUI 弹窗提示语走 `dialog.permission_hint_session_scoped`）。安全兜底两项均在 `ComputerTool::run` 内自持状态：失控角（鼠标物理坐标位于屏幕任一角落附近立即 abort）、距上次 `screenshot` 的连续变更动作数上限（超限返回错误提示模型截图核实或停下）。终端 TUI 无法渲染截图像素（纯文本终端），仅模型可见画面。
    - **系统提示追加**（`register_computer_tool_if_enabled` 返回是否实际注册，main.rs 据此 `agent.append_system(wyj_core::prompts::COMPUTER_USE_HINT)` 并写入 `system_prompt_extra`，`/model` 重建时随该变量原样拼回）：教模型"打开应用优先用 Bash 直接启动（如 macOS `open -a`），不要在 GUI 里瞎找"，以及"变更动作已有逐次确认弹窗，不必先在聊天里问用户'允许'"。**动机**：没有这条提示时，模型（尤其 custom 模式下无内置训练兜底的第三方模型）观察到一次空桌面截图就直接放弃，转而在聊天里等用户显式说"允许"或手动打开目标应用——这不是权限或协议问题，是模型不知道自己已经有 Bash 可以直接启动应用、也不知道逐动作确认这件事本来就会自动发生。custom 模式下 `descriptions::computer_custom_description()` 里也复述了一遍同样的要点（防止模型更看重工具自身描述、忽略系统提示）。**第二个用户报告的失败模式**：模型收到"打开某聊天软件看看某人发了什么消息"这类请求时直接拒绝，理由是"需要调用该软件的 API 才能读取内容"+"你自己的隐私信息我不该看"——两个前提都是错的：截图/`zoom` 读到的就是屏幕渲染出的任何内容，根本不需要该应用的 API；这是用户自己的设备、自己已登录的账号，且是用户本人直接发起的请求，不存在"第三方隐私"这回事。`COMPUTER_USE_HINT` 与 `descriptions::computer_custom_description()` 都补了一段话把这两点挑明，让模型把"帮我看看 X 应用里说了什么"当成用截图/zoom 就能完成的普通任务去执行，而不是拒绝后让用户自己去看。
    - **点击类动作的修饰键支持**：`left_click`/`right_click`/`middle_click`/`double_click` 复用通用 `text` 字段传入要按住的修饰键组合（如 `"shift"`、`"cmd"`，语法与 `key` 动作一致），对齐 Anthropic 官方 computer-use 工具的调用约定（shift-click 多选等场景）。`ComputerTool::do_click` 点击前 `wyj_computer::key_down`、点击后（无论点击成败）`key_up`，优先向模型报告点击本身的错误、释放失败作为次要错误兜底报告，避免修饰键状态残留污染后续操作。系统层 `wyj_computer::key_down`/`key_up`（`backend.rs::parse_key_combo` 共享解析）允许中间插入其他操作（如鼠标点击），因为按键按下/释放状态存在于操作系统层面，不依赖是哪个 `Enigo` 实例发出；`key()` 本身就是 `key_down` 紧接 `key_up`。`summarize_action` 权限确认弹窗摘要同步展示修饰键（如 `left_click at (10, 20) holding shift`）。
    - **`/computer` 诊断命令**（`commands::builtin::ComputerCmd`，i18n key 前缀 `computer.*`）：只读诊断，不依赖真的注册了 `ComputerTool`。门控判断逻辑（平台 → vision → provider）与 `register_computer_tool_if_enabled` 保持一致但独立复现（`crates/commands` 不依赖 `wyj-tools`，直接依赖 `wyj-computer`），再加是否为官方端点判断真实/custom 模式；平台受支持时额外做三项实时探测：`wyj_computer::primary_display_size()`+`scale::fit_within` 算目标分辨率、`capture_primary(64)` 真实截一次小图验证屏幕录制权限、`cursor_location()` 验证辅助功能权限链路可达。固定附带一条 macOS 辅助功能提醒：`cursor_location()` 读取成功不代表点击/按键一定生效——未授权时 `Enigo::new()` 往往仍成功、事件被系统静默丢弃且不报错，这种失败模式没有错误可捕获，只能靠固定文案兜底。

### 权限模型（TUI）

`ToolCtx.permission_mode`（Prompt/AutoApprove/Allowlist）控制 `is_allowed`。**逐调用工具权限确认（v1.0.1 起）**：Normal 模式映射为 `Prompt`，`agent.rs::exec_tool_call` 在执行任一 `Tool::needs_permission()` 为 true 的工具（Edit/Write/Bash）前调用 `ToolContext::confirm_tool(name, summary)`；`ToolCtx` 的实现经 `ui_ask_tx` 发 `UiAskRequest::ToolPermission` 并 await `oneshot<PermissionDecision>`，TUI 弹 `PermissionDialog`（`draw_permission_dialog`），按键 y=AllowOnce / a=AllowAlways / d·Esc=Deny。`Deny` 把拒绝信息作为 `is_error` 工具结果回灌给模型；`AllowAlways` 把工具名写入项目级 `~/.wyj-code/projects/<project_key>/allowed_tools.json`（`project_key` 按 git 仓库根派生，见 `core::project`）并跨会话生效——`ToolCtx::load_allowed_tools` 在每轮 ctx 装配时载入。`summary` 由 `Tool::action_summary()` 提供（Bash=命令、Edit/Write=文件路径）。需要确认的工具均非 `parallel_safe`，串行执行，同一时刻至多一个对话框。Bypass=`AutoApprove` 全放行；Plan=`Allowlist` 白名单限制。`ui_ask_tx` 同时承载 AskQuestion 多题面板与 ExitPlanMode 计划批准；子 Agent 的 ctx 不接 `ui_ask_tx`，`confirm_tool` 默认放行（不阻塞、不弹窗）。
