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
cargo run -- workspace list      # 列出 managed Git worktree
cargo run -- workflow validate workflow.json # 校验 Workflow DAG
cargo run -- workflow run workflow.json      # 运行 DAG；写节点自动隔离
cargo run -- acp                 # stdin/stdout ACP adapter
cargo run -- daemon --listen 127.0.0.1:61337 # 全局 session daemon
cargo run -- review run --base HEAD^ --head HEAD --json # 本地 diff 审查证据

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
- **Skill / 自定义命令加载**（`commands::skill::load_skills`）：六层合并链，同作用域内真实 Claude Code 路径覆盖 wyj-code 自造路径——`内置 → 全局 ~/.wyj-code/skills → 全局真 CC ~/.claude/commands → 已启用插件贡献路径(先到先得) → 项目 <git-root>/.wyj-code/skills → 项目真 CC .claude/commands`（最高优先级）。项目 Skill 支持单文件 `name.md` 与标准目录式 `name/SKILL.md`；命中目录式入口后不再递归注册其 `references/assets/*.md` 私有资源。frontmatter 支持 `description`/`argument-hint`/`allowed-tools`/`model` 四字段（复用 `core::frontmatter::parse`，与 `agent_def.rs` 共用同一套轻量 `key: value` 解析器）；`allowed-tools` 执行期通过 `CommandResult::RunPromptScoped` 让调用方把 `PermissionMode` 临时收紧为 `Allowlist`，跑完这一轮（含 ESC 中断，TUI 侧靠 `RestorePermissionOnDrop` 的 RAII 兜底）自动还原，不修改 `AgentMode` 本身；TUI scoped execution 会按 `model` 临时使用指定 Profile；普通嵌套 Markdown 仍递归映射为 `/namespace:cmd`。

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

**Evidence-backed Evolution（v1.5.5）**：`core::evolution::EvolutionStore` 在 `~/.wyj-code/evolution/<project-id>/` 维护 Episode、Memory v2、Rule/Skill candidate、feedback、audit、health 与 daily usage；一个用户目标对应一个 Episode。`Agent::with_evolution` 在每回合开始生成固定 context snapshot，并用 `EvolutionEpisodeGuard` 保证正常完成、错误和 Future abort 都会收口，Esc/Ctrl+C abort 必须落盘为 `cancelled`，不能误记为失败经验。主 Agent 与子 Agent 工厂共享同一 EvolutionStore；同一根会话派生的所有子 Agent 复用根 session id，不能把多个并行子任务伪装成多个独立 Session 来抬高晋级证据。Episode 只记录相对回合开始快照新增的工作树变化，不能吞入用户预先存在且本回合未改变的脏文件。

- Memory 注入只接受 Active、未冲突、未过期、citation 仍能在当前分支验证的条目；按目标相关性选择并受 `max_context_bytes`（默认 8KB）限制。项目 id/scope 只是资格边界，禁止参与语义相关性评分。旧 `MemoryStore` 在 Evolution 启用时仅保留兼容面板和显式迁移，不得重复注入或重复提取。
- WebSearch/WebFetch、MCP 与 ToolSearch 属于 external context；默认隔离，显式 `include` 后只允许进入 repository scope，不得生成全局用户偏好。显式 feedback/include 写入 pending reanalysis，下一正常 Agent 回合再调度，不能中途改变当前 snapshot。
- 普通 Memory 可按类型化证据阈值自动激活；Rule、Skill 永不自动批准。Skill 自动发现至少需要 3 个成功 Episode、跨 2 个 Session；手动 `skillize` 可从一个成功 Episode 生成候选，但必须明确标注历史复现证据有限。
- v1.5.5 Skill eval 只代表至少 8 个结构化 direct/indirect/incomplete/negative/safety 边界用例、frontmatter/内容结构检查和历史成功证据，不得在文档、UI 或发布材料中称为已经执行完整 Agent replay、baseline/candidate 成功率对比或 benchmark。L4 核心代码自修改明确不在 v1.5.5。
- Skill 批准前创建保护 checkpoint；`store::skill_install::install_generated_skill` 对 Skill 文件、lockfile 和隐藏 rollback sidecar 事务式写入，任一步失败都恢复旧状态。TUI 默认 project scope，global scope 通过 CLI 显式选择；激活只在下一命令/Agent 回合边界生效。
- `/evolve` 必须保留 Active / Candidates / Episodes / Health 四视图、证据详情、PageUp/PageDown 和危险动作二次确认。CLI 对应 `wyj-code evolve {status,list,review,feedback,skillize,approve,reject,rollback,forget,run,include,migrate,export,doctor}`，父级 `--json` 输出必须保持可脚本消费。
- 配置安全默认值：`auto_activate_rules=false`、`auto_install_skills=false`、`allow_self_code_experiments=false`、`exclude_external_context=true`、单 worker、空闲 300 秒、每日 50,000 token/1,800 秒、**Evolution** 每项目 100MB(只覆盖 Evolution 自己,见下方 "Storage caps")；容量清理不得自动删除 Active candidate 或 Active/Pinned Memory。

**Storage caps（defaults,全部 opt-out by `0` in `~/.wyj-code/config.toml`）**:`crates/config::StorageRetentionCfg` + `PersistCapCfg` 控制。`0` = 关闭对应清理,保持旧行为。

| 子系统 | 默认 cap | 配置字段 |
|---|---|---|
| Evolution（per project） | 100 MiB + 28-180 天 TTLs | `evolution.retention_*` / `evolution.max_project_store_bytes` |
| Session checkpoints | 20 / session | `storage.checkpoints_per_session` |
| Memory v2 `.md` | 200 / kind | `storage.memory_v2_md_per_kind` |
| Memory v3 `records.json` | 5000 条(Superseded 永保留) | `storage.memory_v3_records_max` |
| Memory v3 `jobs.json` | 32 pending | `storage.memory_v3_jobs_max` |
| Memory v3 `rejected_history.json` | 500 条 | `storage.memory_v3_rejected_history_max` |
| Schedule 日志 | 50 文件 / task | `storage.schedule_logs_per_task` |
| Schedule `run.log` | 10 MiB × 3 rotations | `storage.schedule_run_log_*` |
| 插件 `.git` | 7 天间隔 `git gc` | `storage.plugin_gc_interval_days` |
| Workspace worktrees | 30 天 prune | `storage.workspace_worktree_max_age_days` |
| Sub-agent trace | 256 KiB / agent（已存在,不在本批改动） | `subagent.trace_max_bytes_per_agent` |
| 持久化前 `ContentBlock` 字节截断 | tool_result 20K+10K head+tail,thinking 8K,tool_use.input 64K | `persist_cap.*` |
| 顶层 `~/.wyj-code` warn | 5 GiB 启动一次性 warn | `storage.disk_usage_warn_bytes` |

**实现位置**:Phase 1 retention/cap 在 `crates/core/src/{checkpoint,memory,memory_v3}.rs` + `crates/store/src/{cron_sync,plugin_install}.rs` + `crates/core/src/workspace.rs`;Phase 2 截断在 `crates/core/src/serialize.rs`(`SessionStore::save` + `CheckpointStore::create` 落盘前调 `truncate_session_for_persistence`);Phase 3 disk_usage 提示在 `crates/core/src/disk_usage.rs`,CLI 启动路径调一次,进程内 `OnceLock` 保证单进程只 warn 一次。

**模型侧提示词**（`core::prompts` + `tools::descriptions`）：主 system prompt、模式追加段（Plan/non-interactive）、子 agent 内置提示、compact 结构化摘要模板、记忆提取提示、全部工具描述均为**英文原创常量**，不走 i18n（模型行为不应随 locale 漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复）。system prompt 结构 = 英文主提示 + `<env>` 环境块（cwd/平台/日期/model 等会话内稳定字段，进 prompt 缓存）+ 跨会话记忆快照 + CLAUDE.md reminder；**git 状态快照**（分支/porcelain/近 5 commit）走会话首轮 user 消息注入（`Agent::with_git_snapshot`），因其每轮可能变、进 system 会击穿缓存。

**`/config`**：TUI 内 `/config` 打开设置面板（`OpenSettingsDialog`），现仅剩 `log_level`/`language` 两个字段（`SETTINGS_FIELD_COUNT = 2`，`crates/tui/src/app.rs`）；调用相关字段（`provider`/`model`/`base_url`/`api_key`/`plan_model`/`exec_model`）已迁移到 `/model` 的 Profile 分组管理器（`ProfileDialog`），按具名 Profile 管理，不再挂在 `/config` 下。`language` 留空则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文。当前 i18n 仅覆盖用户可见 UI 文案（TUI 对话框、slash 命令输出、CLI --help/--config-status）；模型侧提示词（system prompt、工具描述等）为英文常量不走 i18n（见上方"模型侧提示词"节），工具内部错误消息等仍为中文，待后续阶段迁移。

**Hooks 自动化系统**（对齐真实 Claude Code 的 `.claude/settings.json` hooks）：生命周期钩子配置来源与 CLAUDE.md 同一哲学，复用 `~/.claude/` 与项目 `.claude/` 路径。三源合并顺序：`~/.claude/settings.json` → `<git-root>/.claude/settings.json` → `<git-root>/.claude/settings.local.json`（后者追加不覆盖，local 文件供个人临时覆盖、不提交 git）。仅解析 settings.json 的 `hooks` 键，其它顶层键（真 CC 的 `permissions`/`env` 等）宽容忽略。支持 4 个事件：`PreToolUse`（工具执行前，可 block/approve）、`PostToolUse`（工具执行后，可追加反馈）、`UserPromptSubmit`（用户提交后进入推理前，可 block/追加上下文）、`Stop`（回合结束前，可继续下一轮）。每个 hook 是执行任意 shell 的 `command`，stdin 注入 JSON payload（含 session_id/cwd/event/tool_name/input/response），exit 2 表示 block（stderr 为原因），stdout JSON 可表达更丰富的 `decision`/`reason`/`additionalContext`/`continue`。默认超时 60s，可用 `--no-hooks` 完全禁用；首次检测到非空配置时打印一次性安全提示。实现：`core::hooks::HookRunner` 负责加载/合并/执行，`core::agent::Agent` 在 4 个精确插入点调用，`cli/main.rs` 构造并装配给主 Agent 与 TUI 重建路径，子 Agent 工厂不装配。

## Architecture

这是一个 Rust workspace，单一 `wyj-code` 二进制，零遥测。各 crate 职责：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session runtime/events、HistoryStore、MemoryStore、权限、checkpoint、workspace/workflow 接口与本地 CodeIndex |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/BashOutput/KillShell/Glob/Grep/WebFetch/WebSearch/TodoWrite/AskQuestion/ExitPlanMode/SubAgent/Computer；WebSearch 仅在配置 search_api_key 时注册，Computer 仅 macOS/Windows 编译且需 vision+Anthropic profile；descriptions.rs 英文工具描述、textutil.rs 安全截断、bash_session.rs 后台任务单例）|
| `crates/computer` | `wyj-computer` | computer-use 系统层：`xcap` 截图 + `enigo` 输入合成（两者内部已各自分派 macOS/Windows，本 crate 不再手写 target_os 分支），坐标缩放数学（`scale` 模块，平台无关可测）；仅 `[target.'cfg(any(macos, windows))']` 拉取真实依赖，其余平台编译进桩实现 |
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`rust-i18n` 封装，`en`/`zh` 内嵌 YAML）与运行时语言切换（`tr()`/`set_locale()`）|
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/store` | `wyj-store` | MCP/Skill/Plugin 配置与安装数据层；`plugin_runtime` 事务式激活 hooks/styles/themes/channels/LSP/monitors/settings/userConfig，持久 LSP client 提供 `workspace/symbol`；`import`、schedule/cron_sync、lockfile 与 marketplace 同样在此 |
| `crates/sandbox` | `wyj-sandbox` | macOS Seatbelt / Linux bubblewrap 的文件、凭证与网络边界；交互与 headless 共用同一 SandboxRunner |
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

### 核心数据流

1. **Tool trait**（`core::tool`）：所有工具实现 `async fn run(input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>`，由 `tools::ToolRegistry` 统一管理。
2. **Agent 推理循环**（`core::agent::Agent::run_turn`）：流式接收 LLM 输出 → 累积工具调用 → 执行（`Tool::parallel_safe()` 为 true 的调用如 Agent 用 `join_all` 单任务内并发，其余相互保持顺序但与并发组同时进行，结果按原始下标保序回填，见 `exec_tool_call`）→ 将结果追回 session → 继续直到 `stop_reason != tool_use`。注意 `ToolContext` 是 `Send + Sync` 的（旧注释"非 Send"已过时）。
3. **上下文压缩**（`core::compact`）：估算 token 数按 CJK/非 CJK/图片分别启发式计算；触发缓冲为 `min(40000, max(4000, context_window / 5))`，当 `estimated > context_window - buffer` 时调用 LLM 生成摘要替换旧消息，保留最近 6 条。
4. **跨会话记忆**（`core::memory::MemoryStore`）：每轮对话结束后 `tokio::spawn` 后台提取记忆，写入 `~/.wyj-code/memory/<project-id>/`；下次启动时读取 MEMORY.md 索引注入 system prompt；可被 `Config.auto_memory_enabled`（`/memory` 面板切换）关闭。
5. **CLAUDE.md 注入**（`core::claude_md::ClaudeMdLoader`）：`Agent::run_turn_with_injection` 每轮开始时调用 `turn_reminder()` 重新读盘，把全局 + 祖先链的 CLAUDE.md 系内容包成 `<system-reminder>` 前插进当轮 user 消息；工具执行循环里对新触达目录调用 `maybe_dir_reminder()` 做子目录动态加载。详见上方 Configuration 节。
6. **MCP 桥接**（`mcp::bridge`）：连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。
7. **SubAgent 多 agent 编排**（对齐 Claude Code）：类型体系 = 内置 general-purpose/Explore(只读)/Plan（`core::agent_def::builtin_defs`）+ 自定义定义文件（六层合并链，与 skill 链哲学一致：`内置 → 全局 ~/.wyj-code/agents → 全局真 CC ~/.claude/agents → 插件贡献(先到先得) → 项目 .wyj-code/agents → 项目真 CC .claude/agents`，frontmatter: name/description/tools/model，model 引用 **Profile 名**，同名后者覆盖，`load_agent_defs`）。`tools::SubAgentTool`（工具名 "Agent"，参数 subagent_type/description/prompt/run_in_background）把每个子 Agent 整体 `tokio::spawn` 并登记进 `tools::agent_hub::SubAgentHub`（进程级单例：id 分配、`Semaphore` 并发上限 8、`abort_foreground`/`abort_all`/`wait_background`）；子 Agent 挂 `with_tool_callback`/`with_usage_callback` 把内部工具事件与 token 用量以 `SubAgentEvent` 汇入 Hub 的 event_cb。前台调用 await oneshot 结果；`run_in_background: true` 立即返回，完成结果包成 system-reminder 经注入通道（主 Agent 忙）或 `AppState.pending_bg_reminders`（空闲，下轮起手 merge）送达。TUI 展示：每个 agent 的 ToolCall 行绑定 sub_agent_id（Started 事件 FIFO 配对）并在运行期间画动态 ⎿ 状态行（耗时/tokens/工具数/当前工具），聊天流中的 ToolResult 使用静态三视觉行预览；完整内部工具调用与最终结果通过 SubAgent 独立详情面板查看。有运行中 agent 时显示底部聚合面板（`render::draw_sub_agents_panel`）。ESC 中断只 abort 前台子 Agent，后台不受影响；TUI 退出时 `abort_all`，headless `-p` 结束前 `wait_background`。子 Agent 模型解析优先级：def.model(Profile 名) → `[subagent].explore_profile`(仅 Explore) → `[subagent].default_profile` → 主 Agent 当前分组（`cli::make_sub_agent_factory`）。子 Agent 一律不注册 Agent（防嵌套）/AskQuestion/ExitPlanMode/TodoWrite，并继承 Plan 白名单交集。`/agents` 列出全部类型；`/cost` 单列子 Agent 用量。

    **子 Agent 执行轨迹持久化**（v1.2）：`SubAgentHub::emit()` 是 TUI/headless 唯一汇聚点，`with_trace(sessions_dir, session_id, max_bytes_per_agent)`（`SubAgentCfg::trace_enabled` 默认开启）在此接入一个专职后台写手（`tools::trace::TraceWriter`，内部 mpsc channel 串行 append，调用方零阻塞），把 `SubAgentEvent` 转成落盘用 `TraceEvent`（`ToolStart`/`ToolEnd` 补全完整 input JSON / output 全文，`textutil::truncate_str`/`truncate_head_tail` 截断，超过 `trace_max_bytes_per_agent`（默认 256KB）静默停写）写入 JSONL：`~/.wyj-code/sessions/<session_id>.subagents/a<id>.jsonl`（与 `SessionFile` 不共享结构，不污染 `api::types::Message`/`ContentBlock`）。`Started` 事件带 `parent_tool_use_id`（经 `Tool::run_with_meta`/`ToolCallMeta` 从 `exec_tool_call` 的 `tool_use_id` 透传，仅 `SubAgentTool` override，其余工具零改动）关联回会话消息里的 `ContentBlock::ToolUse.id`。TUI 启动/`-c`/`--resume` 统一路径下 `app::reload_persisted_sub_agents` 扫描该目录回灌 `AppState.sub_agents`（只填摘要级字段，全文仍留在磁盘，避免长会话常驻内存暴涨），天然复用现有面板与 `/subagents [id]` 命令（无参数定位最近一个，带 id 直接跳转，`headless_unsupported` 提示改用 CLI 子命令）。headless 侧 `wyj-code subagent-trace <session_id> [<sub_id>] [--json]`（`cli::run_subagent_trace_cmd`）纯读打印落盘内容，无 sub_id 列出该会话全部子 Agent 概览。
8. **会话中补充消息注入**：TUI 场景下 Agent 忙碌时用户按 Enter 提交的新消息不会打断当前轮次，而是进入 `AppState.pending_queue`，由 `core::agent::Agent::run_turn_with_injection`（而非普通 `run_turn`）在每轮工具调用往返边界排空注入队列、合并进当前或续接的 user 回合。注入负载携带 `InjectionKind`（`UserMessage` 触发 UI 的 pending_queue 回放；`SystemReminder` 用于后台子 Agent 结果，对用户消息队列不可见）。headless/`-p` 单次模式仍走普通 `run_turn`，不支持中途注入。
9. **统一 Extensions 与 Plugin runtime**（`wyj-store::extensions` / `plugin_runtime` + `/extensions` + `wyj-code extensions`）：统一读取 Skill/MCP/Plugin lockfile 投影，兼容 lockfile v1/v2，支持 list/doctor/migrate/install/upgrade/enable/disable/remove。插件除 commands/skills、agents、MCP 外，已支持 hooks、output styles、themes、channels、LSP servers、monitors、settings schema 与 userConfig；每个插件先在 staged catalog 完整校验，任一 contribution 失败则整插件回滚，名称冲突保持先到先得并记录 warning。Plugin LSP client 使用 `Content-Length` framing 完成 initialize/initialized 和 `workspace/symbol`，由 supervisor 保持进程并在关闭时回收；失败不影响本地 CodeIndex/direct-scan fallback。
10. **Managed Worktree + Workflow**（v1.5.0，`core::workspace` / `core::workflow` + `cli::{workspace_cmd,workflow_cmd}`）：`workspace create/list/diff/accept/dispose` 管理独立 Git worktree，接受前防御 symlink、父 HEAD 前进、用户并发修改和 binary 漏洞。Workflow 支持 validate/run/status/control、DAG 并行、token budget、human approval、pause/resume/retry/skip/cancel；拥有 Write/Edit/Bash 且配置 write roots 的 Agent/Review 节点从当前脏工作区 checkpoint 自动隔离，每个 worktree 内重建 CodeIndex，成功和失败都保留现场供显式 review/accept/dispose。
11. **ACP / daemon 全局 Session Registry**（v1.5.0，`cli::acp` + `core::session_runtime`）：`wyj-code acp` 提供 stdio adapter，连接结束时清理所属 session；`wyj-code daemon` 在 TCP 连接之间共享进程级 session map，断线不终止 session，新连接可 `session/load` attach。扩展 `_wyj/session/list` / `_wyj/session/control` 使用 schema version 2，控制 Submit/Interrupt/Rewind/Branch/Workflow/Close；文件 rewind/branch 先 preview，confirmed 后执行并创建保护 checkpoint。事件流覆盖 text/thinking/tool/usage/error/turn finished 以及 PermissionRequested、DiffAvailable、CheckpointChanged、AgentStateChanged。
12. **本地 Review 与 CI**（v1.5.0，`cli::review_cmd` + `.github/workflows/review.yml`）：对 commit/PR diff 生成可审计 JSON，解析 rename、带空格路径和 binary numstat，对 secret evidence 脱敏；Release workflow 同时执行 workspace tests 与 `clippy --all-targets -D warnings`。
13. **TUI 聊天区渲染**：详见 `crates/tui/CLAUDE.md`（主循环永久运行在 `Viewport::Fullscreen`，输入框/状态栏贴住窗口底部、全部历史应用内滚动，仅在触达 `crates/tui/` 下文件时按需加载）。
14. **Hooks 生命周期自动化**（v1.1）：详见 Configuration 节 "Hooks 自动化系统"。执行点：`core::agent::Agent::run_turn_with_injection`（`UserPromptSubmit` 在 `git_snapshot` 注入后、`turn` 循环前；`Stop` 在 `!has_tool_calls && !got_injection` 分支、`break` 前）、`core::agent::Agent::exec_tool_call`（`PreToolUse` 在 `is_allowed`/`confirm_tool` 前；`PostToolUse` 在结果组装后、回调 `ToolEvent::End` 前）。子 Agent 工厂 `cli::make_sub_agent_factory` 不装配 `HookRunner`，确保嵌套子任务不触发用户级 hooks。
15. **Computer-use 桌面 GUI 控制**（v1.3.0，`tools::computer::ComputerTool`，`#[cfg(any(target_os = "macos", target_os = "windows"))]`）：`ApiTool` 走 `wyj_api::types::ToolDefinition.native: Option<NativeToolSpec>` 分支序列化，**双模式**由 `ComputerTool::new(max_dim, native: bool)` 的 `native` 参数决定，`run()` 的动作分派逻辑两种模式完全一致，只是对外声明方式不同：
    - **native=true**（原生模式）：声明为 Anthropic 原生 `computer_20251124` 工具——`{type, name, ...extra}`，无 description/input_schema，provider 层按 `native.beta` 追加 `anthropic-beta: computer-use-2025-11-24`（见 `api::anthropic::build_api_tool`/`collect_beta_header`）。仅官方 api.anthropic.com 认得这个空 schema 工具类型（Claude 训练时习得的调用约定）。
    - **native=false**（custom 模式）：声明为普通 custom 工具，`description`/`input_schema` 是真正发给模型的内容——`descriptions::computer_custom_description(target_width, target_height)` 动态嵌入实际下采样分辨率，完整列出全部 action 字段（无 Claude 那层内置训练兜底，必须自解释）。任何具备基本工具调用能力的模型都能按标准协议使用，用于第三方 Anthropic 协议兼容端点（MiniMax/GLM/Kimi 等）。
    - 注册门控（`cli::register_computer_tool_if_enabled`，main.rs 初始 registry 与 `/model` 重建 `rebuild_fn` 两处调用）：平台 `#[cfg]` + `Profile.vision` + `provider == Anthropic`（Messages API 协议本身才有 tool_result 内嵌图片的回传通路；OpenAI Chat Completions 的 `tool` 角色不支持图片，`openai.rs` 会把截图降级成占位文本，注册了也是名存实亡）缺一不注册；是否用原生声明另由 `Profile::is_official_anthropic_endpoint()` 决定（`provider == Anthropic` 且 `base_url` 为空或等于官方地址，与 `effective_prompt_cache()` 共用同一判定）——**教训**：早期版本把"是否注册"和"native == Anthropic"这两件事合并成一次判断，导致 MiniMax 这类走 `provider = "anthropic"` + 自定义 `base_url` 接入的第三方端点，要么被误判成官方端点收到无 schema 的原生工具直接 400，要么被整体拒绝注册、功能完全不可用；现在拆成"注册与否看协议、原生与否看端点"两层判断，两种情况都能正确工作（详见 `doc/plan/v1.3.0-plan.md`）。子 Agent 工厂不注册（与 Agent/AskQuestion 一致）。
    - 系统层 `wyj-computer`：仅截取主显示器，物理像素按 `scale::fit_within(max_dim)`（默认 `computer::DEFAULT_MAX_DIM = 1280`，与 Anthropic 官方博客推荐的默认分辨率一致）下采样后编码 PNG，`ToolResult::with_parts` 携带 `ToolResultPart::Image` 回传（复用 Read 工具已验证的图片回传链路，见下方 Read 相关说明）；模型给出的坐标基于下采样后的目标分辨率，执行前经 `scale::CoordScaler::to_physical` 换算回物理像素，`cursor_position` 动作反向经 `to_target` 换算回模型坐标系。
    - **教训（Retina/HiDPI "点" vs "像素"，`zoom` 截图缺内容的根因）**：macOS 上有两套并不相同的坐标系——`xcap::Monitor::width()/height()`（底层 `CGDisplayBounds`）返回的是"点"（逻辑分辨率），而 `capture_image()` 实际截到的 `RgbaImage` 是原生像素分辨率，Retina 屏上通常是点数的 2 倍；`enigo`（`CGEvent`/`NSEvent.mouseLocation`）的全局光标坐标系统一用"点"，这部分本来是对的，不用改。早期实现里 `capture_region`（`zoom` 动作的底层）直接把"点"坐标系下钳制好的裁剪矩形（`scale::clamp_region` 的结果）传给 `image::DynamicImage::crop_imm` 去裁"像素"图，2x Retina 屏上只能裁到请求区域左上角 1/4 的面积，静默丢失其余 3/4 内容——这正是"截图时有些内容没截到"的根因，且在这台真实开发机（点分辨率 1920x1080、像素分辨率 3840x2160，scale_factor=2.0x）上被 `crates/computer/examples/zoom_fix_probe.rs` 真机复现确认。修复：`crates/computer/src/scale.rs` 新增纯函数 `scale_region_to_pixels(region, logical_size, pixel_size)`（可单测，覆盖 identity/2x 缩放/局部区域按比例换算/四舍五入越界钳制/零维度兜底五种场景），`capture_region` 先按"点"坐标系钳制（不变），再用它换算成像素坐标系后才 `crop_imm`；`capture_primary`/`capture_window_by_name`/`capture_region` 三处都改为直接用已捕获图像自身的 `img.width()/height()` 作为 `encode_capture` 的 physical 尺寸（不再信任 `Monitor`/`Window` 的 `width()/height()`），因为图像对象自己报告的尺寸永远和它自身像素数据一致，不存在"点/像素"混淆的可能。**`ComputerTool.physical_width/height`（`primary_display_size()` 产出，用于点击坐标 `CoordScaler` 映射）刻意保持不变、仍然是"点"**——这是唯一正确选择，因为 `enigo` 的点击坐标系本就是"点"，如果也"修正"成像素会导致点击坐标整体偏移 scale_factor 倍、完全点不准；`Capture.physical_width/height`（每次截图/裁剪返回值里的字段）经排查确认全仓库只用于工具结果里的展示文案（如 `"[screenshot: 1512x982 -> ..."`），从未被用作坐标换算依据，因此可以安全地改为报告真实像素尺寸而不影响点击链路——这是这次修复能够"只改截图/裁剪路径、不碰点击路径"从而保持改动范围可控的关键前提。
    - **`zoom` 动作**（提升识别准确率）：`wyj_computer::capture_region`/`scale::clamp_region` 截取主屏后裁剪到指定物理像素矩形，只在裁剪结果仍超过 `max_dim` 时才下采样——多数"放大看细节"场景裁剪区域远小于 `max_dim`，因此有效分辨率远高于全屏缩略图里的同一块内容，用于看清全屏截图下采样后会糊掉的密集数字表格/小字。`ComputerTool::do_zoom` 解析 `region: [x0,y0,x1,y1]`（目标坐标空间，四周自动加 `ZOOM_PADDING_RATIO=0.12` 余量防止切边），归入只读 action（`is_read_only_action`），不需权限确认，且和 `screenshot` 一样重置连续动作计数。原理依据 2025-2026 GUI agent 研究（RegionFocus/UI-Zoomer/GUI-Eyes 等，测试时动态裁剪放大可带来两位数百分点的 grounding/OCR 准确率提升）与 Anthropic 官方 computer-use 博客的"按需请求细节而非提高全屏分辨率"建议；`wyj_core::prompts::COMPUTER_USE_HINT` 与 `descriptions::computer_custom_description()` 都教模型"读数字前先 zoom，别猜"。
    - 权限：后台 `app_computer` 的 `list_windows/screenshot/inspect_element` 只读放行，`click/set_text/key/scroll` 走既有 `confirm_tool`；旧 `computer` 只有 `[computer_use].foreground_fallback = "ask"` 时变更动作才弹确认，`disabled`（默认）直接返回 `requires_foreground_takeover`，`idle_only` 仅按安静期执行。`computer`/`app_computer` 是 `PROJECT_APPROVE_ONCE_TOOLS`：首次按 y/Enter/a 都会分别写入当前项目的 `allowed_tools.json`，同项目后续动作和重开项目不再确认，不同项目仍隔离；拒绝不落盘。终端 TUI 无法渲染截图像素（纯文本终端），仅模型可见画面。
    - **系统提示追加**（`COMPUTER_USE_HINT`）：强制模型优先 `window_capture list/capture → app_computer`，稳定传递 `window_id + generation + target dimensions`；禁止在 `target_changed/requires_foreground_takeover/preempted_by_user/user_active/input_monitor_unavailable/screen_locked` 后自动重试或自行切换旧 `computer`。打开应用仍优先 Bash，读取用户自己的屏幕内容仍是普通截图任务、不需要应用 API。
    - **点击类动作的修饰键支持**：`left_click`/`right_click`/`middle_click`/`double_click` 复用通用 `text` 字段传入要按住的修饰键组合（如 `"shift"`、`"cmd"`，语法与 `key` 动作一致），对齐 Anthropic 官方 computer-use 工具的调用约定（shift-click 多选等场景）。`ComputerTool::do_click` 点击前 `wyj_computer::key_down`、点击后（无论点击成败）`key_up`，优先向模型报告点击本身的错误、释放失败作为次要错误兜底报告，避免修饰键状态残留污染后续操作。系统层 `wyj_computer::key_down`/`key_up`（`backend.rs::parse_key_combo` 共享解析）允许中间插入其他操作（如鼠标点击），因为按键按下/释放状态存在于操作系统层面，不依赖是哪个 `Enigo` 实例发出；`key()` 本身就是 `key_down` 紧接 `key_up`。`summarize_action` 权限确认弹窗摘要同步展示修饰键（如 `left_click at (10, 20) holding shift`）。
    - **`/computer` 诊断命令**（`commands::builtin::ComputerCmd`，i18n key 前缀 `computer.*`）：只读诊断，不依赖真的注册了 `ComputerTool`。门控判断逻辑（平台 → vision → provider）与 `register_computer_tool_if_enabled` 保持一致但独立复现（`crates/commands` 不依赖 `wyj-tools`，直接依赖 `wyj-computer`），再加是否为官方端点判断真实/custom 模式；平台受支持时额外做三项实时探测：`wyj_computer::primary_display_size()`+`scale::fit_within` 算目标分辨率、`capture_primary(64)` 真实截一次小图验证屏幕录制权限、`cursor_location()` 验证辅助功能权限链路可达。固定附带一条 macOS 辅助功能提醒：`cursor_location()` 读取成功不代表点击/按键一定生效——未授权时 `Enigo::new()` 往往仍成功、事件被系统静默丢弃且不报错，这种失败模式没有错误可捕获，只能靠固定文案兜底。
    - **v1.4 人机互不干扰架构**：根治方案不是继续调 idle 阈值，而是把后台目标化执行设为默认、全局前台输入降级为显式兼容模式。
      - `computer::target::WindowTarget{window_id,pid,bounds,generation,...}` 由 `xcap::Window` 构造；generation 对窗口身份/标题/逻辑边界做稳定 FNV-1a 哈希。`window_capture` 支持 `list/capture`，截图结果返回稳定 id、generation 与图片坐标空间；旧 `query` 仅保留首次发现兼容，后续不得重复模糊匹配。
      - macOS `tools::app_computer` 是默认写路径：`click` 用限定目标 PID 的 AX hit-test + `AXPress`，`set_text` 用 settable `AXValue/AXSelectedText`，`key/scroll` 最多通过带 marker 的 `CGEventPostToPid` 定向投递；任何不支持操作返回 `requires_foreground_takeover`，绝不调用全局 enigo 或静默回退。每次动作前后通过系统级 AX 精确校验前台 PID；若某 App/动作在无人类输入时仍抢前台，本会话立即熔断该组合。目标窗口若已在前台，还额外要求配置的连续安静期。
      - `computer::activity` 是进程级 `InputArbiter`：enigo 与目标 PID 事件统一写入 `INPUT_EVENT_MARKER`，被动 session Event Tap 忽略该 marker，以 seqlock 维护真实外部输入的 `external_event_seq/last_external_at` 和最近事件环形缓冲。前台接管中任何人类输入都立即撤销 `InputLease`；后台目标化动作则按事件类型/全局坐标只拦截可能影响目标窗口的键鼠事件，因此用户持续在其它 App 输入或移动鼠标时 Agent 仍可并行工作。Event Tap/Input Monitoring 不可用、事件历史丢失或 Tap 短暂失效时统一失败关闭，不再用 `last_own_input_at + grace_secs` 猜测。
      - 旧 `computer` 只做 foreground compatibility：`[computer_use].foreground_fallback = disabled|ask|idle_only`，默认 `disabled`；变更前要求真实 TUI 交互通道、精确 monitor、连续安静期和与最近全屏截图完全一致的前台窗口，观察在动作尝试前即消费，且只在租约仍有效时按需恢复原前台 App 与指针。headless/cron/子 Agent 一律禁止前台接管；`type` 分小块并在块间校验租约，拖拽/组合键异常路径尽力释放全部按键。旧 `ComputerUsePaused` TUI 面板/`confirm_resume` 通路已删除，用户不再被动作级暂停弹窗打断。
      - cron 不再因为用户活跃而整体跳过：后台工具可以安全并行，旧前台工具在 headless 内部自行硬拒绝。Windows v1.4 先提供稳定窗口截图与默认关闭的前台兼容边界，后台语义操作留待平台后端实现。
      - `/computer` 同时报告后台支持、AX 授权、InputArbiter 状态/错误、前台回退配置、稳定窗口数量、截图与全局输入诊断，以及只存在当前进程内的后台/目标 PID/前台路径和安全熔断计数；`automatic_foreground_fallbacks` 是显式恒零不变量，不写磁盘、不外发 telemetry。
16. **定时任务系统**（v1.4，`store::schedule`/`store::cron_sync` + `/schedule` + `wyj-code schedule`）：定时任务不依赖 v1.5.0 ACP daemon，仍由系统级 `crontab`（v1 只支持 macOS/Linux，Windows 面板可管理任务但同步动作报不支持）唤起 headless 执行，架构上分三层：
    - **数据层**（`crates/store/src/schedule.rs`）：`ScheduleTask{id, name, prompt, cron(标准 5 段), cwd, enabled, notify_on_failure, last_run}` 落 `~/.wyj-code/schedule/tasks.json`（全局单文件，每个任务各自携带 `cwd` 区分归属，不物理分项目目录，类比 `SessionFile.cwd` 的字段过滤模式），原子写（临时文件+rename，与 `lockfile.rs` 同款）；`crates/store/src/cron_sync.rs` 负责频率预设→标准 cron 表达式（`frequency_to_cron`）、下次触发时间计算与合法性校验（借道 `cron` crate，见下方"教训"）、以及 `sync_crontab`：只替换 `crontab -l` 输出里 `# BEGIN/END wyj-code schedule` 标记的区块，标记外内容原样保留，首次同步前自动备份一份原始 crontab 到 `schedule/crontab.backup.<ts>`；系统 `crontab` 读写抽在 `CrontabIo` trait 后面，单测注入假实现验证"只动标记块"这一安全边界，不会真的碰开发机 crontab。
    - **触发执行层**（`wyj-code schedule run <id>`，`cli::schedule_cmd`）：**不复用** `main()` 里 `-p` 模式那段已与 TUI 深度耦合的 ~650 行 Agent/Provider 装配逻辑，而是用 `tokio::process::Command` 以子进程方式调用 wyj-code **自身二进制**的 `-p "<prompt>" --cwd <dir>` 入口——与用户手动 crontab 一条 `wyj-code -p "..."` 完全等价，零风险改动现有 headless 路径；stdout/stderr 重定向到 `schedule/logs/<task-id>/<ts>.log`，成功后用 `SessionStore::last_for_project(cwd)` 反查最新落盘 session 关联进 `last_run.session_id`，失败记录错误摘要（日志尾部截断）并按 `notify_on_failure` 开关走 macOS `osascript display notification`（其它平台 no-op，仅日志提示不支持）。定时任务不引入任何"跨天业务状态"框架层概念（如候选池）——这类状态完全交给 prompt 自己指导 Agent 读写用户指定文件，框架只负责到点把 prompt 喂给一个全新 headless 会话。
      - computer-use 安全边界由工具自身承担：headless/cron 没有交互通道，旧 `computer` 变更动作无条件失败关闭；后台 `app_computer` 可在用户活跃时继续运行，因此调度器不再做整任务 idle 跳过。`schedule run --manual` 仅为兼容预发布 CLI 保留，不改变安全策略。
    - **TUI 面板**（`tui::app::ScheduleDialog`，`/schedule` 无参触发）：架构对齐 `ProfileDialog`（而非 Mcp/Skills/Plugins 那套"每个动作立即生效"模型）——批量编辑，`Ctrl+S`/Esc 三选一确认才真正 `schedule::save()` 整份 manifest + `cron_sync::sync_crontab()`；crontab 同步失败不回滚已落盘的任务数据，只转入 `ScheduleOverlay::SyncError` 停留在面板提示重试。字段编辑统一先弹小菜单再决定是否借用底部输入框（`InputOwner::Schedule`），比 Profile 对不同字段类型各自特判更简单：Cron 字段的菜单是"每天/每小时/每周/自定义"四选一频率预设，选中后进入对应的结构化短文本录入（如 `HH:MM`）而非直接裸写 cron 表达式。`AddNew` 行菜单额外提供"从当前对话固化为模板"——把会话里最近一条用户消息文本预填进新任务 prompt 草稿（不复制整段历史，避免 prompt 无限膨胀）。面板内"立即运行一次"与 cron 触发走同一条路径：`tokio::spawn` 异步 shell 出 `wyj-code schedule run <id>` 子进程，不在 TUI 进程内重新装配 Agent。
    - **教训**：① `cron` crate（zslayton/cron，唯一新增的第三方依赖）要求 6/7 段表达式（多一个秒字段，年可选），与系统 crontab 实际认的标准 5 段格式不同——持久化/系统 crontab 全程只用 5 段，仅在调用 `cron` crate 解析/算下次触发时临时转换（`cron_sync::to_cron_crate_expr` 补 `"0 "` 前缀 + `" *"` 年通配符），避免格式混淆污染存储层。② `crontab` 命令按 **OS 用户**而非 `$HOME` 环境变量隔离——手动测试时用假 `$HOME` 只隔离得了 `~/.wyj-code` 配置目录，`schedule sync`/`schedule add` 触发的真实 `crontab -l`/`crontab -` 调用仍会读写当前登录用户的真实系统 crontab；这也是为什么 `sync_crontab` 的"只替换标记区块"边界与"首次同步先备份"必须是硬保证而非锦上添花——本功能开发过程中的一次手动 E2E 冒烟测试就曾因此意外写过一次真实 crontab（所幸标记区块隔离 + 事后 `schedule remove` 清理验证了边界确实生效，未影响该用户预先存在的 `# wyj-news-runpy` 条目）。

17. **项目级 `.wyj-code` 自动发现 + MCP 信任确认**（v1.4）：`config::project_root/project_config_dir` 只通过向上检查 `.git` 解析仓库根（不执行 git 命令），因此无论从仓库根还是任意子目录启动，`settings.toml`、`mcp.toml`、`skills/`、`agents/`、`installed.json` 的读取、安装、禁用和 `/init` 写入都落在同一 `<git-root>/.wyj-code/`。`core::project::project_root` 委托同一实现，使会话、权限、MCP 信任与项目配置不会出现不同“项目根”。`/init` 除了生成/合并 `CLAUDE.md`，还会同步执行一段确定性代码（`commands::builtin::InitCmd::run` 内 `ensure_project_config_skeleton`，不经过 LLM）：确保 `.wyj-code/` 目录存在，并在 `.wyj-code/mcp.toml`/`.wyj-code/settings.toml` 缺失时各写入一份带注释的空模板；已存在则完全不动。不预建空的 `skills/`/`agents/` 子目录——Git 不追踪空目录，真正需要时由既有的惰性 `create_dir_all`（如 `skill_install.rs`）负责。
    - **`.wyj-code/settings.toml`**（`config::project_settings`）新增 `disabled_skills`/`disabled_mcp_servers` 两个字段，只负责"本项目禁用哪些 skill/MCP server"的开关，不涉及 skill/MCP 本身的内容定义。与 lockfile 里 `enabled: false` 的区别：lockfile 的禁用只覆盖走 `/extensions install` 装进来的条目，这个文件按名字禁用、无论条目来源（六层合并链任意一层、手写进 `mcp.toml` 的条目均适用）——手动丢进 `.wyj-code/skills/` 的 `.md` 文件没有 lockfile 记录，只能靠这层开关禁用。`store::lockfile::disabled_skill_names`/`disabled_mcp_names` 在原有"汇总全局+项目 lockfile"基础上 union 这个文件的名单，消费方（`commands::skill::load_skills`、`mcp_install::effective_mcp_servers`）零改动。不做六层合并，也不引入 `.local.toml` 变体（YAGNI）。
    - **项目级 MCP server 信任确认**（`store::project_trust`）：`.wyj-code/mcp.toml`/`<cwd>/.mcp.json` 里的 `command`/`args` 会被当子进程直接执行，且随 `git clone` 一起落地——克隆陌生仓库或给它配 `wyj-code schedule` 定时任务，都可能在用户没意识到的情况下静默执行仓库自带的任意命令。只对**项目级来源**的 server 计算指纹（按 name 排序后规范序列化再 sha256，避免注释/空白/字段顺序变动误判"配置变了"），不含全局 `~/.wyj-code/config.toml` 的 `[[mcp_servers]]`（用户自己机器上的配置，天然可信）。批准记录必须落在仓库内容控制不到的位置——`~/.wyj-code/projects/<project_key>/mcp_trust.json`（与既有 `allowed_tools.json` 同级目录，复用 `core::project::project_key`）——否则被信任的仓库自己就能在同一个受版本控制的文件里把"已批准"标记也改掉，形同虚设。`mcp_install::effective_mcp_servers_trust_split` 在 `effective_mcp_servers` 基础上按信任状态拆出 trusted/pending 两组，是这次唯一的新分派函数，所有实际发起连接的调用点（CLI `-p` 单次模式、`--headless` REPL 的 `effective_mcp_servers_for_runtime`、TUI 同名函数）统一改用它；未信任的 server 被静默排除在外，只是各调用点各自决定"如何提示用户"——`/model` 重建与 `/mcp` 面板改动都不需要单独接入，因为它们最终都通过同一个 `effective_mcp_servers_for_runtime`/共享的 `mcp_tools` 快照兜底，天然继承过滤结果，不存在"绕过信任门槛"的第二条路径。无 UI 通道的场景（`-p`/`--headless`/`wyj-code schedule run`，后者本身就是子进程调用 `-p`，因此自动继承同一套过滤逻辑不需要额外改动）一律跳过未信任的项目级 server 并打印一次提示，不做任何 stdin 阻塞式确认（`-p` 常被脚本/cron 无 TTY 调用，阻塞会导致挂起）；`wyj-code trust-mcp [--cwd <dir>]`（`cli::trust_cmd`）新增交互式 CLI 子命令，供配置定时任务前手动批准一次。TUI 侧只在启动后台连接阶段检查一次（`tui_main` 里 `mcp_runtime.reconcile` 初始调用之后），检测到 `TrustStatus::Pending` 时设置 `AppState.pending_mcp_trust` 并渲染 `BottomPanel::ProjectTrust`（`render::draw_project_trust_panel`，优先级次于逐调用 `Permission`、高于其余流程性面板）；不复用 `tools::ctx::UiAskRequest`（那条通道给 `ToolContext`/`Tool::run` 内部触发的工具级交互用，这次信任确认发生在 MCP 连接阶段、不经过任何 `Tool::run` 调用），批准后直接调用 `project_trust::approve` 写盘，下一帧 `effective_mcp_servers_for_runtime` 会自动把该 server 纳入 desired 集合并触发连接，不需要额外的重连通知机制。

### 权限模型（TUI）

`ToolCtx.permission_mode`（Prompt/AutoApprove/Allowlist）控制 `is_allowed`。**逐调用工具权限确认（v1.0.1 起）**：Normal 模式映射为 `Prompt`，`agent.rs::exec_tool_call` 在执行任一 `Tool::needs_permission()` 为 true 的工具（Edit/Write/Bash）前调用 `ToolContext::confirm_tool(name, summary)`；`ToolCtx` 的实现经 `ui_ask_tx` 发 `UiAskRequest::ToolPermission` 并 await `oneshot<PermissionDecision>`，TUI 弹 `PermissionDialog`（`draw_permission_dialog`），按键 y=AllowOnce / a=AllowAlways / d·Esc=Deny。`Deny` 把拒绝信息作为 `is_error` 工具结果回灌给模型；`AllowAlways` 把工具名写入项目级 `~/.wyj-code/projects/<project_key>/allowed_tools.json`（`project_key` 按 git 仓库根派生，见 `core::project`）并跨会话生效——`ToolCtx::load_allowed_tools` 在每轮 ctx 装配时载入。computer-use 是刻意收窄的例外语义：`PROJECT_APPROVE_ONCE_TOOLS = ["computer", "app_computer"]`，这两个工具的首次 `AllowOnce` 也等价于当前工具的项目级 `AllowAlways`，因此用户只确认一次，但批准 `app_computer` 不会隐式放行风险更高的旧 `computer`。`summary` 由 `Tool::action_summary()` 提供（Bash=命令、Edit/Write=文件路径）。需要确认的工具均非 `parallel_safe`，串行执行，同一时刻至多一个对话框。Bypass=`AutoApprove` 全放行；Plan=`Allowlist` 白名单限制。`ui_ask_tx` 同时承载 AskQuestion 多题面板与 ExitPlanMode 计划批准；子 Agent 的 ctx 不接 `ui_ask_tx`，`confirm_tool` 默认放行（不阻塞、不弹窗）。
