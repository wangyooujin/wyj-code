# CLAUDE.md

> **本文件为什么这么短**：它会被**全文注入每一轮对话的 system prompt**（`core::claude_md::ClaudeMdLoader`），
> 体积直接乘在每趟请求的 prefill 上。原版 63KB / ~29k token，其中 43KB 是各子系统的实现细节，
> 而一个改 Rust bug 的任务根本用不到其中八成。现在根目录只留**跨 crate 的通则与索引**，
> 实现细节按目录惰性下沉——只有当工具真正触达 `crates/<x>/` 时，那份规约才会被拼进上下文。

## 规约文件索引

| 文件 | 何时加载 | 内容 |
|---|---|---|
| `crates/core/CLAUDE.md` | 触达 `crates/core/` | Agent 循环、上下文压缩与 context editing、跨会话记忆、规约文件注入、补充消息注入队列、worktree/workflow、ACP/daemon、Hooks、通知、system prompt 分段、TodoWrite、Skill 名单、Evolution、Storage caps、模型侧提示词 |
| `crates/tools/CLAUDE.md` | 触达 `crates/tools/` | SubAgent 工具与后台 shell、Computer-use 桌面 GUI 控制（含 v0.2.5 人机互不干扰架构、坐标缩放、权限边界） |
| `crates/store/CLAUDE.md` | 触达 `crates/store/` | Extensions/Plugin runtime、定时任务系统、项目级 `.wyj-code` 发现与信任门控、SkillHub |
| `crates/tui/CLAUDE.md` | 触达 `crates/tui/` | 聊天区渲染、输入框/状态栏、终端 panic 还原、后台任务面板 |
| `crates/cli/CLAUDE.md` | 触达 `crates/cli/` | CLI 装配、review/CI |
| `crates/api/CLAUDE.md` | 触达 `crates/api/` | System prompt stable/volatile 分段与 prompt cache 断点 |
| `crates/mcp/CLAUDE.md` | 触达 `crates/mcp/` | MCP 桥接 |
| `docs/reference/config.md` | 手动查阅 | `config.toml` 带注释全量参考、规约文件回退链、Hooks 事件 |
| `doc/plan/*.md` | 手动查阅 | 各项设计的历史决策与失败模式调研 |

## 代码导航约定

**读代码优先用 MCP `codegraph`**，它是本仓库唯一同时给得出调用关系、影响面和逐字源码的入口。

- **绝大多数探索走 `codegraph_explore`**：一次调用返回按文件分组的源码，等价于 Read，返回里已经给出的文件不要再 open。
- 只知道符号名要定位 → `codegraph_search`；explore 被截断或遇到同名重载 → `codegraph_node`（用 `file`/`line` 消歧）。重载方法尤其别急着 grep。
- 改签名、重构前 → `codegraph_callers` / `callees` / `impact`；看项目布局 → `codegraph_files`；索引可疑 → `codegraph_status`。
- `Grep` / `Glob` 留给 codegraph 命不中的场景：未索引的文件、字符串字面量、配置与文档。
- 内置 `CodeSearch`（`crates/core/src/code_index.rs`）是不依赖 MCP 的词法兜底，仅在 MCP 未连接或要跨项目查时选它。
- 索引可能滞后于工作区：codegraph 给的位置/内容与磁盘冲突时以 `Read` 为准，必要时提示用户重建索引。

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
cargo run -- --permission-mode bypass # 显式选模式：auto（默认）/ plan / bypass
cargo run -- --no-hooks           # 禁用 Hooks 自动化系统
cargo run -- -c / --continue     # 恢复上一次会话
cargo run -- --resume <id>       # 恢复指定会话 ID
cargo run -- subagent-trace <session_id> [<sub_id>] [--json]  # 查看落盘的子 Agent 执行轨迹（无 sub_id 列出概览）
cargo run -- usage [--since YYYY-MM-DD] [--project PATH] [--json]  # 跨会话 token 消耗汇总（消耗率基线来源）
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

## 同一项目跑多个实例

两个 wyj-code 进程共享同一批状态文件，**worktree 只隔离文件、不隔离状态**——`~/.wyj-code/`
下的授权名单 / 记忆 / 会话 / 扩展清单仍是共享单文件。两者正交，缺一不可：

| 冲突面 | 手段 |
|---|---|
| 两个 agent 同时改同一批源码 | `workspace create --branch <名字>` 开独立 worktree（唯一真隔离） |
| 共享状态文件互相覆盖 | `config::filelock` 跨进程锁（锁住在最底层 crate，因为 `Config::save` 自己就是 load→改→save）+ `serialize::write_atomic` 原子替换（会话 / 授权名单 / memory-v3 全局层） |
| 两个进程 resume 同一会话 | 启动时检出占用并告警（`SessionStore::active_owner`）。锁只能防写坏，防不了「整文件覆盖」语义上的丢更新，所以并行必须换会话 |

```bash
# 1) 开一个隔离实例（worktree 落在 ~/.wyj-code/workspaces/<project_id>/worktrees/）
cargo run -- workspace create --branch feat/login-refactor --session-id login

# 命令末尾会直接打印可粘贴的启动行 —— **别去掉 --project-root**：
wyj-code --cwd <worktree> --project-root /Volumes/WD_APFS/wyj-code

# 2) 汇合：分支可直接 push / merge；也能逐文件回灌
cargo run -- workspace diff <id>
cargo run -- workspace accept <id> <paths...>
cargo run -- workspace list [--reconcile]   # --reconcile 清掉失效 manifest 并 prune
cargo run -- workspace dispose <id>
```

**`--project-root` 为什么不能省**：worktree 里 `.git` 是个**文件**，而 `config::resolve_project_root`
用 `.exists()` 判仓库根（`crates/config/src/lib.rs`），于是不带这个参数时 worktree 会被当成
**另一个项目**——`project_key` 变了，`allowed_tools.json`、项目记忆、MCP/skill 信任、演化记录、
代码索引全部重开，`-c` / `/resume` 也看不到主仓库会话。带上它才是「共享身份 + 隔离文件」。

**已知限制**：`git worktree add` 只 checkout 已提交内容，ignored / untracked 文件（`target/`、
`node_modules/`、`.env`、未提交的 `AGENTS.local.md`）不会带过去。Rust 项目可用
`CARGO_TARGET_DIR=<主仓库>/target` 指回去绕开冷构建——cargo 对同一 target 目录是阻塞排队而非写坏。

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

**不要把 `[profile.dev]` / `[profile.test]` 的 `incremental` 改回 `true`**（2026-10-11 实测定位，详见 Cargo.toml 内注释）。Rust 增量编译把每个 codegen unit 的目标文件写进 `target/debug/deps/*.o`，文件名带 metadata hash；源码一改整个 crate 换新 hash → 写出一批全新文件名的 `.o`，**被取代的旧 `.o` 原地保留、cargo 从不回收**。本仓库一次完整构建产出 10,465 个 `.o`（0.70 GB，全来自 11 个工作区 crate），实测「touch core + cargo build」单轮新增 1,807 个 `.o` / 359 MB，折算 ≈ 7 GB/天——与实际观察到的 `target/` 膨胀到 **225 GB / 134 万文件**吻合。代价不只是占盘：cargo 每次调用都要 stat 全部产物判断新鲜度，实测真·无变更 build 的地板成本随文件数线性增长（4.2 GB / 1.07 万文件 = 0.66 s，134 万文件就是分钟级）。关掉后 `.o` 不再散落，产物按 metadata hash 稳定命名并原地覆盖，实测连续多轮编辑 **+0 文件 / +0 MB**，全量冷构建 2.2 GB。配套 `debug = "line-tables-only"`（保留行号供 panic backtrace，去掉变量值）把每个 `.o` 从 ~70 KB 压到量级更小；需要 lldb 看变量时临时改回 `debug = 1` 即可。

## Git 提交约定

- **不要**在 commit message 中添加 `Co-Authored-By: Claude <noreply@anthropic.com>` 或任何指向 Claude / Anthropic 的署名 trailer。仓库历史已清理过一次以移除这些 trailer——它们会让 GitHub 在 Contributors 列表中显示 "Claude"，本仓库希望仅展示真实人类作者。
- commit message 用简体中文，首行 `type: 简述`，空行后列要点。
- 用户说"提交代码并 push"时：`git add -A` → `git commit` → `git push`。仅当用户要求时才提交/推送；若在默认分支上，先建分支（本项目 master 为单作者主线，按用户指示可直接提交）。

## Slash 命令约定

- **每新增一条 `/xxx` slash 命令，必须同步在 `/help` 输出中注册该命令的说明**，包括：命令名、用途简介、用法/参数（若有）、示例（可选）。`/help` 是用户发现命令的唯一入口，未注册的命令等同于对用户不可见。
- 注册位置：`/help` 的输出由 `crates/commands/src/builtin.rs` 中 `HelpCmd` 读取的 i18n 模板 `help.body` 渲染（**不是**动态枚举注册表），因此新增命令时必须同步在 `crates/i18n/locales/{en,zh}.yml` 的 `help.body` 模板里追加该命令的说明行，保持与现有条目相同的格式与缩进。
- 同步要求：新增命令的 PR/提交里就应包含对应的 `/help` 条目，不要留到后续补；若临时不希望暴露（如调试命令），应在 `/help` 中显式标注「内部/调试」而非直接省略。
- 命令文案需走 i18n（`tr()` key），与 `/help` 其余条目一致，不可硬编码中文。
- **动态命令（Skill / 项目级自定义命令）是上述约定的例外**：这类命令运行时从磁盘发现、因人而异，不可能写死进静态 `help.body` 模板。`Command` trait 的 `is_dynamic()` 默认方法（仅 `SkillCommand` 覆盖为 `true`）标记这类命令，`CommandContext.dynamic_commands` 由调用方在构造 ctx 前用 `cmd_registry.list()` 过滤好传入，`HelpCmd` 渲染完静态模板后在末尾追加一个「自定义命令」分组（i18n key: `help.custom_commands_header`）。两者互不冲突，新增静态内置命令仍必须遵守上面的硬性约定。v0.2.24 起 Skill 来源收敛到 `~/.wyj-code/skills/` + `<git-root>/.wyj-code/skills/`，不再读取 `~/.claude/commands/` / `<cwd>/.claude/commands/`，动态命令扫描范围同步收敛。
- **回合进行中（`is_thinking`）的 slash 命令**：忙碌时按 Enter 默认把内容排进 `pending_queue` 投递给模型，而 `/model` 这类命令**没有内容可投递**——排给模型等于把字面的 `/model` 当成一句用户消息发出去，右下角同时冒出"1 条已排队"（用户现场）。因此 TUI 在 Enter 分支先问 `Command::runs_while_busy`：为 true 的**面板类**命令（`/model` `/mcp` `/config` `/skills` `/plugins` `/agents` `/memory` `/sessions` `/schedule` `/help` `/mode` `/cwd` `/doctor` `/hooks` `/shells` `/subagents` `/computer` `/agent-control` `/extensions` `/mouse` `/quit`）照常 dispatch，其余维持排队。**该方法默认 false，且必须保持默认 false**，四条排除理由见其文档注释（起新回合 / 改会话历史 / 抢 `current_task` 句柄导致回合失去 ESC 中断 / 读 session 统计）。`/cost` `/context` 刻意不在白名单：忙碌时 session 被回合整段持锁，只能拿到降级 ctx（统计字段全 0），与其给一份静默失真的数字，不如让用户等回合结束。CommandContext 拿不到锁时**降级构造而不是跳过执行**——跳过等于用户按 `/model` 只看到一句"会话忙"。钉子：`app::busy_slash_tests`（白名单判定 + 主循环分支顺序）、`commands::registry::busy_whitelist_tests`（白名单精确集合 + 四条排除）。
- **Skill / 自定义命令加载**（`commands::skill::load_skills`）：四层合并链（v0.2.24 起由 6 层简化）——`内置 → 全局 ~/.wyj-code/skills → 已启用插件贡献路径(先到先得) → 项目 <git-root>/.wyj-code/skills`（最高优先级）。wyj-code **不读取** `~/.claude/commands/` 或 `<cwd>/.claude/commands/` 等任何外部源。项目 Skill 支持单文件 `name.md` 与标准目录式 `name/SKILL.md`；命中目录式入口后不再递归注册其 `references/assets/*.md` 私有资源。frontmatter 支持 `description`/`argument-hint`/`allowed-tools`/`model` 四字段（复用 `core::frontmatter::parse`，与 `agent_def.rs` 共用同一套轻量 `key: value` 解析器）；`allowed-tools` 执行期通过 `CommandResult::RunPromptScoped` 让调用方把 `PermissionMode` 临时收紧为 `Allowlist`，跑完这一轮（含 ESC 中断，TUI 侧靠 `RestorePermissionOnDrop` 的 RAII 兜底）自动还原，不修改 `AgentMode` 本身；TUI scoped execution 会按 `model` 临时使用指定 Profile；普通嵌套 Markdown 仍递归映射为 `/namespace:cmd`。**注意 skill 仍不是模型可调用的工具**（无 `Skill` 工具），但可用 skill 的名单会经 `Agent::with_skills` 注入 system prompt 的 `# Skills` 段（v0.2.27+），模型据此能回答"该敲哪条斜杠命令"，详见架构节第 23 条。

## Configuration

配置文件 `~/.wyj-code/config.toml`（API Key 优先读 `WYJ_CODE_API_KEY`）。**带注释全量参考见
`docs/reference/config.md`**，下面是日常最常改的：

```toml
provider = "anthropic"       # 或 "openai"
model = "claude-opus-4-8"
base_url = ""                # 留空用供应商默认端点
max_tokens = 32000
context_window = 200000      # 决定压缩触发线：window - max(max_tokens+8192, window/10)
vision = true
log_level = "warn"           # 调试时 "debug"
language = ""                # "en"/"zh"，留空自动检测系统 locale
```

**`context_window` 必须写真实值**，它是裸 `u32`、没有 live probe 兜底。填一个未经验证的
超大值（如把 204,800 填成 1,000,000）会让压缩硬阈值落在 900k、context editing 软阈值落在
550k，而真实会话根本摸不到——两套保护等于从未运行，工具输出全量累积、每趟重发整段历史。
`--config-status` 会对 >500k 的值给出告警。

**`prompt_cache` 默认跟随端点类型**：官方 Anthropic 端点开；第三方 Anthropic 兼容端点
（MiniMax/GLM/Kimi…）默认关——它们实现了 `cache_control`，但对未知的 `anthropic-beta` 头常直接 400，
故第三方开启时只发 `cache_control`、**不发** caching beta 头。若端点连 `cache_control` 也不认，
首次 400 会自动降级（不中断会话），本进程内停用缓存并在 `/cost` 显示降级原因。

`[notify]` / `[storage]` / `[persist_cap]` / `[evolution]` / `[subagent]` / `[context_edit]` /
`[computer_use]` / `[tools]` 等块的完整字段与默认值见 `docs/reference/config.md`。

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
| `crates/store` | `wyj-store` | MCP/Skill/Plugin 配置与安装数据层；`plugin_runtime` 事务式激活 hooks/styles/themes/channels/LSP/monitors/settings/userConfig，持久 LSP client 提供 `workspace/symbol`；schedule/cron_sync、lockfile 与 marketplace 同样在此。v0.2.24 起移除 `import` 子模块（/import 命令下线）与 `wyj-config::codex` 兼容层（Codex 导入能力下线） |
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

### 核心数据流索引

实现细节已下沉到上表的 crate 规约文件，这里只留一句话定位，避免"要改 X 该去哪"的往返：

| # | 机制 | 落在 | 详规 |
|---|---|---|---|
| 1 | Tool trait | `core::tool` | `crates/core` |
| 2 | Agent 推理循环（流式 → 工具 → 回填 → 再请求） | `core::agent::Agent::run_turn` | `crates/core` |
| 3 | 上下文压缩 + context editing（清理优先于摘要） | `core::compact` / `core::context_edit` | `crates/core` |
| 4 | 跨会话记忆（每轮后台提取） | `core::memory::MemoryStore` | `crates/core` |
| 5 | 规约文件注入（按槽位回退 + 子目录惰性加载） | `core::claude_md::ClaudeMdLoader` | `crates/core` |
| 6 | MCP 桥接 | `mcp::bridge` | `crates/mcp` |
| 7 | SubAgent 编排（类型体系 / Hub / 轨迹落盘） | `tools::sub_agent` + `tools::agent_hub` | `crates/tools` |
| 8 | 会话中补充消息注入（SteerQueue） | `core::SteerQueue` | `crates/core` |
| 9 | Extensions 与 Plugin runtime | `store::extensions` / `plugin_runtime` | `crates/store` |
| 10 | Managed Worktree + Workflow | `core::workspace` / `core::workflow` | `crates/core` |
| 11 | ACP / daemon 全局 Session Registry | `cli::acp` + `core::session_runtime` | `crates/core` |
| 12 | 本地 Review 与 CI | `cli::review_cmd` | `crates/cli` |
| 13 | TUI 聊天区渲染 | `crates/tui` | `crates/tui` |
| 14 | Hooks 生命周期自动化（4 个事件，同步 shell out） | `core::hooks::HookRunner` | `crates/core` |
| 15 | Computer-use 桌面 GUI 控制 | `tools::computer` / `tools::app_computer` | `crates/tools` |
| 16 | 定时任务系统（crontab 唤起 headless 子进程） | `store::schedule` / `store::cron_sync` | `crates/store` |
| 17 | 项目级 `.wyj-code` 发现 + MCP/skill 信任门控 | `config::project_root` + `store::project_trust` | `crates/store` |
| 18 | TUI 终端 panic 还原 + char-boundary 安全 | `tui::panic_guard` / `core::textutil` | `crates/tui` |
| 19 | 统一通知通道（bell + 桌面通知） | `core::notify` | `crates/core` |
| 20 | 后台任务可见性与自动续跑 | `tui::AppState::poll_shells` | `crates/tui` |
| 21 | System prompt 拆 stable / volatile 两段 | `api::SystemPrompt` | `crates/api` |
| 22 | TodoWrite 默认开启（`[tools].todo_enabled` 可关） | `[tools].todo_enabled` | `crates/core` |
| 23 | Skill 名单注入模型上下文（不是工具） | `core::prompts::MAIN` 的 `# Skills` 段 | `crates/core` |
| 24 | HTTP 技能注册表 SkillHub | `store::skillhub` | `crates/store` |
| 25 | 前台 Bash 输出实时推进（快照节流，非增量） | `tools::BashTool` + `ToolContext::tool_output()` | `crates/tools` + `crates/tui` |
| 26 | 面板类命令的排版引擎（按显示宽度对齐，零 i18n 调用） | `commands::cost_view` + `CommandContext::terminal_width` | `crates/commands` |
| 27 | 预排版消息通道（逐行原样渲染，不折行） | `CommandResult::OutputPlain` + `MessageRole::Preformatted` | `crates/commands` + `crates/tui` |
| 28 | 子 Agent token 用量计量（原子累加器，逐层透传） | `core::session::SubUsage` + `ToolContext::sub_usage_sink()` | `crates/core` + `crates/tools` |
| 29 | 编辑后自动验证闭环（项目声明 `verify.command` 才生效） | `core::verify` | `crates/core` |
| 30 | 每轮时延画像（TTFT / 生成 / 重试开销） | `core::session::TurnLatency` + `/context` | `crates/core` + `crates/commands` |
| 31 | 推理轮数触顶：先收尾交付，再硬中止存档 | `CheckpointKind::TurnLimit` + `TURN_LIMIT_GRACE` | `crates/core` |

**跨 crate 改动的两条硬约定**：

- **模型侧提示词是英文常量，不走 i18n**（`core::prompts` + `tools::descriptions`）。模型行为不应随
  locale 漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复。i18n 只覆盖用户可见
  的 UI 文案。
- **改提示词就要重测缓存相关的不变式**：system prompt 落在 stable 段且断点在段末，
  任何每轮变动的内容混进 stable 都会让整段前缀缓存失效。

### 权限模型（TUI）

**三种模式（`AgentMode`：Auto / Plan / Bypass，`--permission-mode` 或 Shift+Tab 在这三档间循环）**。`manual` / `default` / `normal` 仍能解析但一律落到 `Auto`——手动模式已并入 auto，作为**向后兼容别名**保留，让旧配置与脚本不至于静默失效（`AgentMode::parse` 的测试钉死「必须仍返回 `Some`」）。`ToolCtx.permission_mode`（`PermissionMode::{Auto, AutoApprove, Allowlist, Plan}`）控制 `is_allowed`，而「要不要问人」在 `agent.rs::ensure_tool_permission` 里决定，它返回 `Result<(), String>`——错误里带**具体**原因，因为「用户点了 d」与「auto 分类器判定越界」对模型而言是两种不同的后续行为。

**Auto 是唯一的日常判定模式**：判定交给 `core::auto_mode::AutoGate`，固定顺序 = 受保护路径 deny（任何模式都不放行）→ 确定性 allow（工作目录内文件写、只读/构建/lockfile 安装的 shell，零延迟）→ 确定性 hard_deny（`curl|bash`、`rm -rf ~`、force push、`git reset --hard`、`terraform destroy`、`sudo <program>`、凭据路径、无审批 agent 循环，不发模型请求）→ 模型分类器（其余才付一次模型往返，system prompt 见 `prompts::AUTO_CLASSIFIER`，规则文本见 `core::auto_rules`）→ 交回逐次弹窗 → **降级**（连续 3 次或累计 20 次被拦自动 `disable_classifier()`，只剩确定性规则 + 弹窗；`/mode auto` 显式选一次可恢复）。**两条确定性规则必须排在「分类器是否可用」之前**——否则 `[auto_mode] enabled = false` 时 `curl|bash` 只剩一个弹窗，按个 `y` 就过去了（测试 `disabling_the_classifier_never_drops_the_hard_deny_layer` 钉死）。分类器超时/非法输出/空响应先加格式提醒重试一次，两次都失败才交回人工，且不计入降级预算。闸门挂在 `ToolContext`（`auto_gate()`）而非 `Agent` 上：子 Agent 的 ctx 由父 ctx 派生，闸门自动传递、计数全局共享；`PermissionPolicy::evaluate` 保持同步，只管「任何模式都不放行」那部分（真实判定需要 await 模型）。**Auto 不要求交互面**，因此 `-p`/headless/定时任务同样可用；无交互面时闸门判「交回人工」会落到 `ToolCtx::confirm_tool` 的 fail-closed 分支（删掉 `PermissionMode::Prompt` 之后，这是唯一保证「无审批通道就不放行」的关卡）。**「本轮已放行」的记忆粒度按工具分叉：Bash 按工具名，其余按输入指纹**——Bash 批准一次本轮不再问（长任务里几十条同类命令不该各付一次往返，闸门在复用之前仍逐条走过 hard_deny 与只读白名单），而写工具/`Agent`/computer-use 必须逐次记（`Edit` 批过 `notes.md` 不该顺带放行 `deploy.sh`；用户在为一个文件点头不等于同意改另一个）。

**逐调用工具权限确认（v0.1.2 起，Auto 判 AskUser 时的落点）**：`ToolCtx::confirm_tool(name, summary)` 经 `ui_ask_tx` 发 `UiAskRequest::ToolPermission` 并 await `oneshot<PermissionDecision>`，TUI 弹 `PermissionDialog`（`draw_permission_dialog`），按键 y=AllowOnce / a=AllowAlways / d·Esc=Deny。`Deny` 把拒绝信息作为 `is_error` 工具结果回灌给模型；`AllowAlways` 把工具名写入项目级 `~/.wyj-code/projects/<project_key>/allowed_tools.json`（`project_key` 按 git 仓库根派生，见 `core::project`）并跨会话生效——`ToolCtx::load_allowed_tools` 在每轮 ctx 装配时载入。computer-use 是刻意收窄的例外语义：`PROJECT_APPROVE_ONCE_TOOLS = ["computer", "app_computer"]`，这两个工具的首次 `AllowOnce` 也等价于当前工具的项目级 `AllowAlways`，因此用户只确认一次，但批准 `app_computer` 不会隐式放行风险更高的旧 `computer`。`summary` 由 `Tool::action_summary()` 提供（Bash=命令、Edit/Write=文件路径）。需要确认的工具均非 `parallel_safe`，串行执行，同一时刻至多一个对话框；`ToolCtx::needs_dialog` 的判据必须与 `confirm_tool` 的两条「直接放行」分支严格对齐（Auto 也在其列——闸门判定要 await、AskUser 还要弹窗，同样会阻塞），否则 Agent 会把多调用排进并发组同时弹 N 个框。Bypass=`AutoApprove` 全放行；Plan=`Plan(白名单)` + 只读 Bash 校验 + 规划文档路径策略。`ui_ask_tx` 同时承载 AskQuestion 多题面板与 ExitPlanMode 计划批准；子 Agent 的 ctx 不接 `ui_ask_tx`，`confirm_tool` 默认放行（不阻塞、不弹窗）。配置见 `docs/reference/config.md` 的 `[auto_mode]` 节。
