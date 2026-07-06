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
cargo run -- -c / --continue     # 恢复上一次会话
cargo run -- --resume <id>       # 恢复指定会话 ID

./build.sh                       # 等同 cargo build --release
./build.sh package               # 打包到 dist/<binary>-<version>-<platform>
./build.sh install               # 安装到 ~/.local/bin/wyj-code
./build.sh uninstall             # 卸载二进制；加 --purge 二次确认后彻底删除 ~/.wyj-code/
./build.sh cross linux-x86_64    # 交叉编译（支持 linux-x86_64, linux-aarch64, macos-*）
```

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
# thinking_budget = 8000     # extended thinking 预算 token；不写/0 = 关闭（思考计入 output 计费）
# interleaved_thinking = true # 工具调用轮之间允许交错思考（budget 开启时生效）
log_level = "warn"           # 调试时设为 "debug"
language = ""                # "en"/"zh"，留空自动检测系统 locale

[subagent]
default_profile = ""         # 子 Agent 默认 Profile 名，留空沿用主 Agent 当前分组
explore_profile = ""         # 内置 Explore 类型专用 Profile 名（配便宜模型），留空回退 default_profile

[[mcp_servers]]
name = "my-server"
transport = "stdio"
command = "/path/to/server"
args = ["--flag"]
```

**CLAUDE.md 记忆机制**（对齐真实 Claude Code，`core::claude_md::ClaudeMdLoader`）：查找范围为全局 `~/.claude/CLAUDE.md`（复用真实 Claude Code 的路径） + 从 git 仓库根到 cwd 的祖先链（找不到 `.git` 则只用 cwd 本身）；每级目录内 `CLAUDE.md`/`CLAUDE.local.md` 都存在就都读（local 视作个人覆盖追加，不提交 git），两者都不存在则回退读 `AGENTS.md`；支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。每轮对话开始时重新读盘，以 `<system-reminder>` 包装拼进当轮 **system prompt 末尾**（不注入 user 消息、不进历史，配合 prompt caching 避免跨轮累积；文件不变时字节级稳定、缓存可命中）。工具（Read/Edit/Write/Glob/Grep）触达新子目录时，若该目录有 CLAUDE.md 系文件且本会话未展示过，会在 `agent.rs` 的工具执行循环里追加到 system 末尾（`ClaudeMdLoader::maybe_dir_reminder`，按目录去重）。`/init` 触发一次真正的 agent 回合（`CommandResult::RunPrompt`）去探索项目并生成/合并更新 CLAUDE.md，而非静态模板写文件；`/memory` 打开 TUI 面板列出当前会话适用的全部文件，选中后挂起 TUI 唤起 `$EDITOR` 编辑，同时暴露 auto-memory（跨会话记忆提取）开关与索引入口（`Config.auto_memory_enabled`）。不再兼容旧版 `WYJ.md`。

**模型侧提示词**（`core::prompts` + `tools::descriptions`）：主 system prompt、模式追加段（Plan/non-interactive）、子 agent 内置提示、compact 结构化摘要模板、记忆提取提示、全部工具描述均为**英文原创常量**，不走 i18n（模型行为不应随 locale 漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复）。system prompt 结构 = 英文主提示 + `<env>` 环境块（cwd/平台/日期/model 等会话内稳定字段，进 prompt 缓存）+ 跨会话记忆快照 + CLAUDE.md reminder；**git 状态快照**（分支/porcelain/近 5 commit）走会话首轮 user 消息注入（`Agent::with_git_snapshot`），因其每轮可能变、进 system 会击穿缓存。

**`/config`**：TUI 内 `/config` 打开设置面板（`OpenSettingsDialog`），现仅剩 `log_level`/`language` 两个字段（`SETTINGS_FIELD_COUNT = 2`，`crates/tui/src/app.rs`）；调用相关字段（`provider`/`model`/`base_url`/`api_key`/`plan_model`/`exec_model`）已迁移到 `/model` 的 Profile 分组管理器（`ProfileDialog`），按具名 Profile 管理，不再挂在 `/config` 下。`language` 留空则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文。当前 i18n 仅覆盖用户可见 UI 文案（TUI 对话框、slash 命令输出、CLI --help/--config-status）；模型侧提示词（system prompt、工具描述等）为英文常量不走 i18n（见上方"模型侧提示词"节），工具内部错误消息等仍为中文，待后续阶段迁移。

## Architecture

这是一个 Rust workspace，单一 `wyj-code` 二进制，零遥测。各 crate 职责：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session、HistoryStore、MemoryStore、ClaudeMdLoader、上下文压缩 |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/BashOutput/KillShell/Glob/Grep/WebFetch/TodoWrite/AskQuestion/ExitPlanMode/SubAgent；descriptions.rs 英文工具描述、textutil.rs 安全截断、bash_session.rs 后台任务单例）|
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`rust-i18n` 封装，`en`/`zh` 内嵌 YAML）与运行时语言切换（`tr()`/`set_locale()`）|
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/store` | `wyj-store` | MCP/Skill/Plugin 配置管理数据层：安装元数据 lockfile、MCP registry HTTP client、skill/plugin git marketplace client、install/upgrade/uninstall/enable 编排；只管配置写入（config.toml/`.wyj/mcp.toml`/lockfile），绝不 shell out 执行依赖安装（如 `npm install`）|
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

### 核心数据流

1. **Tool trait**（`core::tool`）：所有工具实现 `async fn run(input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>`，由 `tools::ToolRegistry` 统一管理。
2. **Agent 推理循环**（`core::agent::Agent::run_turn`）：流式接收 LLM 输出 → 累积工具调用 → 执行（`Tool::parallel_safe()` 为 true 的调用如 Agent 用 `join_all` 单任务内并发，其余相互保持顺序但与并发组同时进行，结果按原始下标保序回填，见 `exec_tool_call`）→ 将结果追回 session → 继续直到 `stop_reason != tool_use`。注意 `ToolContext` 是 `Send + Sync` 的（旧注释"非 Send"已过时）。
3. **上下文压缩**（`core::compact`）：估算 token 数（字符数/3 粗略），当 `estimated > context_window - 40_000` 时调用 LLM 生成摘要替换旧消息，保留最近 6 条。
4. **跨会话记忆**（`core::memory::MemoryStore`）：每轮对话结束后 `tokio::spawn` 后台提取记忆，写入 `~/.wyj-code/memory/<project-id>/`；下次启动时读取 MEMORY.md 索引注入 system prompt；可被 `Config.auto_memory_enabled`（`/memory` 面板切换）关闭。
5. **CLAUDE.md 注入**（`core::claude_md::ClaudeMdLoader`）：`Agent::run_turn_with_injection` 每轮开始时调用 `turn_reminder()` 重新读盘，把全局 + 祖先链的 CLAUDE.md 系内容包成 `<system-reminder>` 前插进当轮 user 消息；工具执行循环里对新触达目录调用 `maybe_dir_reminder()` 做子目录动态加载。详见上方 Configuration 节。
6. **MCP 桥接**（`mcp::bridge`）：连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。
7. **SubAgent 多 agent 编排**（对齐 Claude Code）：类型体系 = 内置 general-purpose/Explore(只读)/Plan（`core::agent_def::builtin_defs`）+ 自定义定义文件（`~/.claude/agents/*.md` 与项目 `.claude/agents/*.md`，frontmatter: name/description/tools/model，model 引用 **Profile 名**，同名后者覆盖，`load_agent_defs`）。`tools::SubAgentTool`（工具名 "Agent"，参数 subagent_type/description/prompt/run_in_background）把每个子 Agent 整体 `tokio::spawn` 并登记进 `tools::agent_hub::SubAgentHub`（进程级单例：id 分配、`Semaphore` 并发上限 8、`abort_foreground`/`abort_all`/`wait_background`）；子 Agent 挂 `with_tool_callback`/`with_usage_callback` 把内部工具事件与 token 用量以 `SubAgentEvent` 汇入 Hub 的 event_cb。前台调用 await oneshot 结果；`run_in_background: true` 立即返回，完成结果包成 system-reminder 经注入通道（主 Agent 忙）或 `AppState.pending_bg_reminders`（空闲，下轮起手 merge）送达。TUI 展示：每个 agent 的 ToolCall 行绑定 sub_agent_id（Started 事件 FIFO 配对）并在运行期间画动态 ⎿ 状态行（耗时/tokens/工具数/当前工具），ToolResult 展开（Ctrl+O）时先列内部工具调用明细；有运行中 agent 时显示底部聚合面板（`render::draw_sub_agents_panel`）。ESC 中断只 abort 前台子 Agent，后台不受影响；TUI 退出时 `abort_all`，headless `-p` 结束前 `wait_background`。子 Agent 模型解析优先级：def.model(Profile 名) → `[subagent].explore_profile`(仅 Explore) → `[subagent].default_profile` → 主 Agent 当前分组（`cli::make_sub_agent_factory`）。子 Agent 一律不注册 Agent（防嵌套）/AskQuestion/ExitPlanMode/TodoWrite，并继承 Plan 白名单交集。`/agents` 列出全部类型；`/cost` 单列子 Agent 用量。
8. **会话中补充消息注入**：TUI 场景下 Agent 忙碌时用户按 Enter 提交的新消息不会打断当前轮次，而是进入 `AppState.pending_queue`，由 `core::agent::Agent::run_turn_with_injection`（而非普通 `run_turn`）在每轮工具调用往返边界排空注入队列、合并进当前或续接的 user 回合。注入负载携带 `InjectionKind`（`UserMessage` 触发 UI 的 pending_queue 回放；`SystemReminder` 用于后台子 Agent 结果，对用户消息队列不可见）。headless/`-p` 单次模式仍走普通 `run_turn`，不支持中途注入。
9. **插件市场 / MCP / Skill 安装管理**（`wyj-store` + TUI `/plugins` `/mcp` `/skills` 面板）：`.claude-plugin` 清单解析（`store::plugin_manifest`）+ marketplace 安装编排（`store::plugin_install`）；`store::lockfile` 记录已安装 MCP/Skill/Plugin 及其启用状态（`InstalledPluginEntry` 等），`store::registry`/`store::marketplace` 分别对接 MCP registry HTTP 与 skill/plugin 的 git marketplace 拉取；三个面板均支持浏览/安装/升级/卸载/启用/禁用，变更需重启生效。`--plugin-dir <dir>` 可临时加载本地开发中的插件（不落盘、不经 lockfile，仅当次运行有效，不出现在 `/plugins` 已安装列表）。

### 权限模型（TUI）

`ToolCtx.permission_mode`（Prompt/AutoApprove/Allowlist）控制 `is_allowed`；目前 `Prompt` 分支直接放行（`AgentEvent::PermissionRequest` 是无生产者的 stub，真正的逐调用确认对话框尚未实现）。工具与 TUI 的真实交互通道是 `ToolCtx.ui_ask_tx: mpsc::Sender<UiAskRequest>`（AskQuestion 多题面板、ExitPlanMode 计划批准走它）。Plan 模式通过 `Allowlist` 白名单限制工具，子 Agent 继承该白名单（与类型定义的工具集取交集），且子 Agent 的 ctx 不接 ui_ask_tx（也未注册需要 UI 交互的工具）。
