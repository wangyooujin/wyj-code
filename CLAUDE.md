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
log_level = "warn"           # 调试时设为 "debug"
language = ""                # "en"/"zh"，留空自动检测系统 locale

[[mcp_servers]]
name = "my-server"
transport = "stdio"
command = "/path/to/server"
args = ["--flag"]
```

**CLAUDE.md 记忆机制**（对齐真实 Claude Code，`core::claude_md::ClaudeMdLoader`）：查找范围为全局 `~/.claude/CLAUDE.md`（复用真实 Claude Code 的路径） + 从 git 仓库根到 cwd 的祖先链（找不到 `.git` 则只用 cwd 本身）；每级目录内 `CLAUDE.md`/`CLAUDE.local.md` 都存在就都读（local 视作个人覆盖追加，不提交 git），两者都不存在则回退读 `AGENTS.md`；支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。**不焊死进 system prompt**，而是每轮对话开始时重新读盘，以 `<system-reminder>` 文本块前插进当轮 user 消息（`Session::prepend_to_last_user`）——保证运行期间编辑立即生效、压缩后依然完整。工具（Read/Edit/Write/Glob/Grep）触达新子目录时，若该目录有 CLAUDE.md 系文件且本会话未展示过，会在 `agent.rs` 的工具执行循环里追加一条独立 reminder（`ClaudeMdLoader::maybe_dir_reminder`，按目录去重）。`/init` 触发一次真正的 agent 回合（`CommandResult::RunPrompt`）去探索项目并生成/合并更新 CLAUDE.md，而非静态模板写文件；`/memory` 打开 TUI 面板列出当前会话适用的全部文件，选中后挂起 TUI 唤起 `$EDITOR` 编辑，同时暴露 auto-memory（跨会话记忆提取）开关与索引入口（`Config.auto_memory_enabled`）。不再兼容旧版 `WYJ.md`。

**`/config`**：TUI 内 `/config` 打开交互式设置面板（`OpenSettingsDialog`），可直接编辑 `base_url`/`api_key`/`model`/`plan_model`/`exec_model`/`language` 并写回 `config.toml`；`language` 留空则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文。当前 i18n 仅覆盖核心用户可见文案（TUI 对话框、slash 命令输出、CLI --help/--config-status、system prompt），工具内部错误消息等仍为中文，待后续阶段迁移。

## Architecture

这是一个 Rust workspace，单一 `wyj-code` 二进制，零遥测。各 crate 职责：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session、HistoryStore、MemoryStore、ClaudeMdLoader、上下文压缩 |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite/AskQuestion/ExitPlanMode/SubAgent）|
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`rust-i18n` 封装，`en`/`zh` 内嵌 YAML）与运行时语言切换（`tr()`/`set_locale()`）|
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

### 核心数据流

1. **Tool trait**（`core::tool`）：所有工具实现 `async fn run(input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>`，由 `tools::ToolRegistry` 统一管理。
2. **Agent 推理循环**（`core::agent::Agent::run_turn`）：流式接收 LLM 输出 → 累积工具调用 → 顺序执行（因 `ToolContext` 非 Send）→ 将结果追回 session → 继续直到 `stop_reason != tool_use`。
3. **上下文压缩**（`core::compact`）：估算 token 数（字符数/3 粗略），当 `estimated > context_window - 40_000` 时调用 LLM 生成摘要替换旧消息，保留最近 6 条。
4. **跨会话记忆**（`core::memory::MemoryStore`）：每轮对话结束后 `tokio::spawn` 后台提取记忆，写入 `~/.wyj-code/memory/<project-id>/`；下次启动时读取 MEMORY.md 索引注入 system prompt；可被 `Config.auto_memory_enabled`（`/memory` 面板切换）关闭。
5. **CLAUDE.md 注入**（`core::claude_md::ClaudeMdLoader`）：`Agent::run_turn_with_injection` 每轮开始时调用 `turn_reminder()` 重新读盘，把全局 + 祖先链的 CLAUDE.md 系内容包成 `<system-reminder>` 前插进当轮 user 消息；工具执行循环里对新触达目录调用 `maybe_dir_reminder()` 做子目录动态加载。详见上方 Configuration 节。
6. **MCP 桥接**（`mcp::bridge`）：连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。
7. **SubAgent**：`tools::SubAgentTool` 通过工厂函数创建拥有独立 provider 和工具集的子 Agent，顺序执行不嵌套并发。
8. **会话中补充消息注入**：TUI 场景下 Agent 忙碌时用户按 Enter 提交的新消息不会打断当前轮次，而是进入 `AppState.pending_queue`，由 `core::agent::Agent::run_turn_with_injection`（而非普通 `run_turn`）在每轮工具调用往返边界排空注入队列、合并进当前或续接的 user 回合。headless/`-p` 单次模式仍走普通 `run_turn`，不支持中途注入。

### 权限模型（TUI）

工具调用前通过 `mpsc` channel 发送 `AgentEvent::PermissionRequest`，TUI 渲染确认对话框，用户按 `y`/`n` 回复，Agent 协程阻塞等待。
