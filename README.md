# wyj-code

[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#快速开始)
[![Zero Telemetry](https://img.shields.io/badge/telemetry-none-green.svg)](#设计原则)

一个用 **Rust** 从零实现的终端 AI 编程助手——单二进制、原生 TUI、双协议 LLM 适配、
多 Agent 协作、可插拔 MCP 工具链和 OS 级安全执行。重点适配 GLM、MiniMax、Kimi、
DeepSeek、Qwen/百炼、豆包/火山等国内模型，也支持 Claude、OpenAI 与本地兼容端点。

> **定位说明**：这是个人工程作品集项目，用于展示 AI 系统的工程实现能力，非商业产品。
> 依据公开的 Anthropic Messages API、OpenAI Chat Completions API、MCP 公开规范
> clean-room 实现，不含任何第三方专有 prompt 或品牌资产，所有文案均为原创。

## 特性

**核心引擎**
- **Agent 推理循环** —— 流式 SSE 解析 → 累积工具调用 → 并发执行 → 结果回填，按 `stop_reason` 续接直到完成。
- **国内模型可信运行时** —— vendor 与 wire protocol 分离，能力值带来源/置信度/cache；静态目录覆盖 GLM、MiniMax、Kimi、DeepSeek、Qwen、豆包，并为 Ollama/vLLM/自定义兼容端点保留保守降级。
- **双协议 Provider** —— Anthropic Messages 与 OpenAI Chat Completions 双格式；工具 JSON 有限修复 + schema 校验，Provider 错误类型化，同角色 fallback 只在可恢复错误和完整消息边界发生。
- **多 Agent 协作** —— 内置 `general-purpose` / `Explore`(只读) / `Plan` 三类子 Agent，支持自定义定义文件；进程级 Hub 管理并发、前后台调度、follow-up、interrupt、retry-last 和落盘 trace。
- **ToolSearch / lazy schema** —— 工具数超过阈值后只发送核心与 sticky schema，按本地词法搜索加载其余工具；状态栏和 `WYJ_STATS_JSON` 展示 schema token 发送量与节省量。
- **上下文压缩** —— 估算 token 数接近窗口上限时自动摘要替换旧消息，保留最近若干条。
- **Checkpoint / Rewind / Branch** —— 在不改用户真实 Git index 的前提下保存对话与文件状态，支持 conversation/files/both 恢复和从 checkpoint 创建新 session。
- **Workflow + 隔离 Worktree** —— `workflow validate/run/status/control` 提供 DAG 校验、并行执行、暂停/恢复/重试/跳过/审批/取消与 token budget；拥有写权限的 Agent/Review 节点从当前脏工作区 checkpoint 自动创建独立 managed worktree，结果需显式 review/accept。
- **ACP / daemon 控制面** —— 支持 stdio ACP adapter 和本地 TCP daemon；daemon 使用进程级 session registry，客户端断线后 session 继续存在，新连接可 load、列出、提交、打断、rewind、branch、控制 workflow 或关闭 session。
- **本地 CodeIndex + Plugin LSP** —— 词法/符号索引失败时自动退化为 ignore-aware direct scan；已启用插件可提供真实 LSP `workspace/symbol`，结果与本地索引合并、去重和排序。
- **统一 Extensions 资源平台** —— `/extensions` 与 `wyj-code extensions` 统一管理 Skill、MCP、Plugin；支持 lockfile v2、原生 Claude MCP 迁移、stdio/Streamable HTTP，以及插件 hooks、output styles、themes、channels、LSP、monitors、settings schema 与 userConfig 的事务式运行时激活。
- **本地 Review 证据** —— `wyj-code review run` 对 commit/PR diff 生成可审计 JSON，覆盖 rename、空格路径、binary numstat 与 secret evidence 脱敏；GitHub Action 在 CI 中执行同一扫描器。
- **可插拔 MCP** —— 内置 Bash / Read / Write / Edit / Glob / Grep / WebFetch / TodoWrite，并桥接 stdio 或 Streamable HTTP MCP server。

**安全执行**
- **默认 fail-closed** —— headless、单次 `-p`、schedule 与 SubAgent 没有真实 UI 时不会隐式批准副作用工具；`bypass` 只跳过普通交互确认，不会静默关闭 OS sandbox。
- **Plan 文档写入** —— Plan 模式可直接维护 `doc/plan/**`、`docs/plan/**`、`.wyj-code/plans/**`；其他文档需逐路径授权，源码、脚本、配置与 shell 绕过始终拒绝。
- **统一 Bash sandbox** —— 前台、后台和 TUI `!command` 走同一 runner；macOS 使用 Seatbelt + 受控域名代理，Linux 使用 bubblewrap 并在域名桥接不可验证时失败关闭，常见凭证目录默认拒读。
- **一次性降级** —— 只有交互式 TUI 可明确批准一次 unsandboxed fallback，授权不能持久化；headless、schedule、SubAgent 始终拒绝。

**交互与配置**
- **原生 ratatui TUI** —— 流式 markdown、语法高亮、多行编辑、工具调用静态执行流、彩色 diff、Todo/SubAgent 列表与详情；终端可直接鼠标拖选复制，普通 `↑/↓` 始终留给输入框。
- **Profile 分组配置** —— provider / vendor / wire protocol / model / base_url / `api_key_env` 以具名 Profile 组织，多套供应商配置并存切换，`/model` 面板管理。
- **三模式 + 分模型** —— `normal` / `plan` / `bypass` 各自可绑定模型；权限策略与 sandbox 独立生效，模式切换不能扩大底层安全边界。
- **CLAUDE.md 记忆机制** —— 每轮重新读盘，全局 + 祖先链文件以 `<system-reminder>` 注入当轮 user 消息；工具触达新目录时动态加载；`@path` 递归导入。
- **Hooks 生命周期自动化** —— 复用 `.claude/settings.json`，在 PreToolUse / PostToolUse / UserPromptSubmit / Stop 四个节点挂任意 shell 脚本（拦截危险命令、保存即格式化、注入上下文、回合结束通知），`/hooks` 查看当前生效配置，`--no-hooks` 一键禁用。
- **自定义 slash 命令** —— 识别真实 Claude Code 的 `~/.claude/commands/*.md` 与 `.claude/commands/*.md`（与 wyj-code 自造的 `~/.wyj-code/skills`/`.wyj-code/skills` 并存，同作用域内真 CC 路径优先），frontmatter 支持 `description`/`argument-hint`/`allowed-tools`，`/help` 末尾动态列出全部自定义命令。
- **一键导入 Codex / Claude Code 配置** —— `/import` 面板扫描 `~/.codex/`（MCP server、prompts）与 Claude Code（`~/.claude.json`/`.mcp.json`、commands、agents）配置，勾选确认后物化为 wyj-code 自管配置；CLI 对应 `wyj-code extensions migrate --from codex|claude|all [--dry-run]`。
- **定时任务** —— `/schedule` 面板管理 cron 触发的任务及其 allowed tools、write roots、allowed domains、require sandbox 权限清单；旧任务升级后自动禁用并要求复核，复核后仍需再次显式启用。
- **会话中消息注入** —— Agent 忙碌时新消息进队列不打断，在工具调用往返边界排空合并。
- **i18n** —— 中 / 英双语，运行时切换，覆盖核心用户可见文案。
- **会话持久化** —— 自动写入 `~/.wyj-code/`，`-c` 续上次、`--resume <id>` 恢复指定会话。
- **自更新** —— `wyj-code update` 检查 GitHub Releases 并原地替换二进制。
- **零遥测** —— 仅在显式 LLM / WebFetch / MCP 调用时出网。

## 目录结构

```text
crates/
├── api/         # Provider、国内模型能力目录/doctor、SSE 流式解析
├── cli/         # 二进制入口、参数解析
├── commands/    # Slash 命令系统（/help /model /agents /compact …）
├── computer/    # Computer-use 截图、目标窗口和输入合成
├── config/      # Profile 分组配置加载
├── core/        # Agent、权限、ToolSearch、checkpoint、workspace/workflow/ACP 接口与 CodeIndex
├── i18n/        # 中英双语资源
├── mcp/         # MCP 客户端（stdio / http）
├── sandbox/     # macOS Seatbelt / Linux bubblewrap 隔离
├── store/       # Extension/Plugin runtime、schedule、安装与 lockfile
├── tools/       # 内置工具 + SubAgent + AgentHub
└── tui/         # ratatui 前端
```

## Demo

<!-- TODO: 用 asciinema 录一段真实 TUI 操作（问答 + 工具调用 + 权限确认弹窗的完整一轮），
     发布到 asciinema.org 后把下面这行换成实际的录屏链接/embed。 -->
> 📺 录屏演示占位——建议录一段完整的问答 + 工具调用 + 权限确认弹窗流程，替换本段为
> `[![asciinema](链接)](链接)` 形式的嵌入。

**60 秒上手**：

```bash
git clone <本仓库地址> && cd wyj-code
./build.sh install && wyj-code --config-status   # 装好后先看一眼配置状态
export WYJ_CODE_API_KEY=sk-...                    # 或写进 ~/.wyj-code/config.toml
wyj-code                                          # 启动 TUI，开始对话
```

## 安装

**方式零：一键安装脚本**（macOS / Linux / Windows）：脚本会自动识别平台架构、拉取
GitHub 最新 Release、校验 sha256 后装入用户目录并配置 PATH，全程无需 sudo/管理员权限：

```bash
# macOS / Linux
curl -fsSL https://wangyooujin.github.io/wyj-code/install.sh | sh

# Windows（PowerShell）
irm https://wangyooujin.github.io/wyj-code/install.ps1 | iex
```

之后可用 `wyj-code update` 检查并自动升级到最新版本，无需重新执行脚本。

**方式一：下载预编译产物**（macOS / Linux / Windows，见 [GitHub Releases](../../releases)）：
下载对应平台压缩包并解压，压缩包内自带一键安装脚本——macOS/Linux 执行 `./install.sh`，
Windows 双击（或在终端运行）`install.bat`。脚本会把 `wyj-code` 装进当前用户目录（`~/.local/bin`
或 `%USERPROFILE%\.wyj-code\bin`）并自动配置 PATH，全程无需 sudo/管理员权限。之后可用
`wyj-code update` 检查并自动升级到最新版本，无需重新下载。如果不想让脚本改动 shell 配置文件/
用户 PATH 注册表项，也可以跳过脚本，手动把 `wyj-code` 放进自己的 `PATH`。

**方式二：从源码构建**：

```bash
git clone <本仓库地址>
cd wyj-code
./build.sh install              # 构建 release 并安装到 ~/.local/bin/wyj-code
```

## 快速开始

```bash
cargo build --release           # → target/release/wyj-code
cargo run                       # TUI 模式
cargo run -- -p "你的问题"       # 单次问答
cargo run -- --headless         # REPL 模式
cargo run -- --config-status    # 查看配置状态
```

配置文件 `~/.wyj-code/config.toml`。推荐用 `api_key_env` 只保存环境变量名；兼容变量
`WYJ_CODE_API_KEY` 仍可覆盖当前 Profile，但运行时 secret 不会因打开设置面板后保存而写回文件：

```toml
active_profile = "minimax"
language = "zh"

[[profiles]]
name = "minimax"
provider = "openai"
vendor = "minimax"
wire_protocol = "open_ai_chat_completions"
model = "MiniMax-M2"
base_url = "https://api.minimaxi.com/v1"
api_key_env = "MINIMAX_API_KEY"
max_tokens = 8192
context_window = 200000

[model_runtime]
probe_mode = "explicit"
tool_argument_retries = 2
lazy_tools_threshold = 12
lazy_tools_top_k = 8
lazy_tools_sticky_turns = 3

[sandbox]
enabled = true
allow_unsandboxed_commands = true
fail_if_unavailable = false

[sandbox.network]
allowed_domains = []
```

TUI 内可用 `/model doctor`、`/sandbox`、`/checkpoint`、`/rewind`、`/branch`、
`/agent-control`；CLI 对应 `wyj-code model doctor`、`wyj-code sandbox` 和
`wyj-code session {checkpoint,checkpoints,rewind,branch}`。完整命令见 `/help` 和
`wyj-code --help`。

v1.5.0 的隔离执行、Workflow、ACP daemon 与本地 Review 都是 CLI 控制面：

```bash
wyj-code workspace create --base HEAD --purpose "isolated fix"
wyj-code workspace list
wyj-code workspace diff <workspace-id>
wyj-code workspace accept <workspace-id> path/to/file

wyj-code workflow validate workflow.json
wyj-code workflow run workflow.json
wyj-code workflow status <workflow-id>
wyj-code workflow control <workflow-id> pause

wyj-code acp                              # stdin/stdout ACP adapter
wyj-code daemon --listen 127.0.0.1:61337 # 跨连接共享 session 的本地 daemon
wyj-code review run --base HEAD^ --head HEAD --json
```

Workflow 仅对显式拥有 Write/Edit/Bash 且配置了 `write_roots` 的 Agent/Review 节点自动创建
worktree；成功后不会自动合并或删除，而是在结果中返回 `workspace diff` / `workspace accept`
命令。失败现场同样保留，便于复核。

模型诊断默认免费且不联网。只有显式 `--probe basic|full` 才发请求，并且只读取独立的
`WYJ_CODE_PROBE_API_KEY`；不会扫描或复用配置中的 Profile Key。没有真实 probe 证据时，
国内模型只显示 `static_only` / protocol-compatible，不能视为 live verified。

项目级资源会从 Git 仓库根目录的 `.wyj-code/` 自动发现；即使在 `src/`、
`crates/foo/` 等子目录启动，也会使用同一份项目配置：

```text
.wyj-code/
├── settings.toml          # 本项目禁用的 Skill / MCP 名称
├── mcp.toml               # 项目 MCP server；首次连接需确认信任
├── skills/
│   ├── review.md          # /review
│   └── release/SKILL.md   # /release（标准目录式 Skill）
├── agents/                # 项目 SubAgent 定义
└── installed.json         # 项目 Extensions lockfile
```

Skill 在启动及下一次命令边界自动重载，MCP 在安全回合边界连接/断开。
仓库内的 `settings.toml` 只接受安全的项目开关；模型 Profile、API Key 和全局网络
端点仍只从 `~/.wyj-code/config.toml`/环境变量读取，避免仓库文件注入凭证或悄悄
改写供应商配置。

资源管理也可以完全在 headless/CI 中执行：

```bash
wyj-code extensions list --json
wyj-code extensions doctor
wyj-code extensions migrate
wyj-code extensions enable mcp:postgres --scope project
```

Hooks 配置示例（`.claude/settings.json`，与真实 Claude Code 格式一致）：

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "./scripts/guard.sh" }] }
    ],
    "PostToolUse": [
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "cargo fmt" }] }
    ]
  }
}
```

## 设计决策

这里记录的不是"复刻了什么"，而是"为什么这样做"——区别于照搬规范的部分。

- **Profile 分组而非扁平字段**：真实场景下一套配置不够——主力 Claude、备援 OpenAI、本地 Ollama 往往并存。把 provider/model/base_url/api_key 收进具名 Profile，`/model` 面板增删改查，比官方工具的单一配置模型更贴近多供应商实际用法。
- **消息注入而非打断轮次**：用户在 Agent 思考中途补充信息是高频需求。直接打断会丢失中间状态；改成 `pending_queue` + `InjectionKind`，在工具调用往返边界排空合并进当前或续接的 user 回合，对用户透明、对 LLM 无感。
- **子 Agent 进程级 Hub**：多 agent 真正的难点是调度与回收，不是 spawn。用单例 Hub 统一分配 id、`Semaphore` 限并发、前台 oneshot / 后台经 system-reminder 通道回注、控制消息只在安全边界注入、ESC 只 abort 前台、退出 `abort_all`——把生命周期管理做成一等公民。
- **权限与 sandbox 分层**：权限策略回答“是否批准”，OS sandbox 回答“批准后最多能触达哪里”。二者分离后，`bypass`、项目配置、模型提示和持久化的 Allow 都不能覆盖 protected deny，也不能把 sandbox 失败变成自动直连。
- **静态兼容不冒充真实验证**：国内端点变化快，模型名推断只能提供保守默认。目录、用户 override 和显式 probe 按可信度合并；没有轮换 Key 和 live 证据就保持 `static_only`，发布材料不扩大结论。
- **Rust + async + workspace**：选 Rust 不是赶时髦，是这个场景天然契合——流式解析、并发工具执行、零运行时依赖的单二进制分发。12 个 crate 的切分让职责边界在编译期就强制清晰。

## 设计原则

- **本地优先** —— 源码、配置、历史只属于你，无任何隐式埋点或崩溃上报。
- **透明可控** —— Agent 循环、工具调用、权限确认全程实时展示，随时可打断。
- **协议中立** —— vendor 与 wire protocol 分离，同一兼容协议可服务不同厂商，不把模型名称猜测当成最终事实。

## 性能

v1.2 对"体积大不大、启动慢不慢"做了一次实测，而不是凭感觉判断：

| 指标 | 数值 |
|---|---|
| release 二进制体积（已 strip） | 12 MB |
| debug 二进制体积（`cargo run` 默认） | 64 MB（未 strip/未优化，编译换速度，属预期行为） |
| 稳态冷启动（`--config-status`，无 TUI 无网络） | ~10ms |
| 换新二进制后首次执行 | 数十 ms ~ 1s 量级（操作系统把可执行文件页面读入缓存的一次性开销，非程序问题） |

release 构建已开启 `opt-level = 3` + `lto = "thin"` + `codegen-units = 1` + `strip = true`，
`reqwest` 已选 `rustls-tls` 而非 native-tls/OpenSSL（规避最常见的"体积陡增"陷阱）。12MB 对一个
自带 TUI、异步运行时、HTTP 客户端、MCP 客户端的 Rust 单二进制来说属正常区间，未发现需要大动干戈
优化的问题。

**依赖去重排查**（`cargo tree --duplicates`）：发现 `bitflags`(1.3/2.13)、`getrandom`(0.2/0.3/0.4)、
`hashbrown`(0.15/0.17)、`itertools`(0.11/0.13)、`rustix`(0.38/1.1)、`schemars`(0.8/1.2)、
`unicode-width`(0.1/0.2) 多版本共存。逐条排查后如实记录：均不属于"改一下我们自己的版本号就能收敛"
的情况——`schemars` 0.8→1.x 需要跟着其 derive 宏的 breaking API 迁移 `crates/tools` 里的用法，是
一次独立的、有真实回归风险的升级，不适合塞进一次轻量收尾；`unicode-width` 我们自己已经在用更新的
0.2，0.1 版本来自 `ratatui`（经 `unicode-truncate`）的传递依赖锁定；其余几项均为不同上游库
（`ratatui`/`crossterm`/`arboard`/`rmcp`/`scraper` 等）各自独立锁定的版本，不在本项目控制范围内。

**`panic = "abort"` 评估后明确不采用**：能省一些体积（去掉 unwind 表），但项目大量使用
`tokio::spawn`（后台记忆提取、子 Agent、bash 后台会话）——目前某个 spawned task panic 只会让那个
task 失败，不影响主进程；切到 `panic = "abort"` 后任意线程 panic 会让整个进程（包括用户正在用的
TUI 会话）立即退出，这个行为回归对交互式工具不可接受，体积收益不足以抵消。

**已确认为必要成本而非可砍功能**：TUI 剪贴板粘贴图片依赖 `arboard` 的 `image-data` feature（间接
带入 `image`/`tiff`/`zune-jpeg` 等图像解码库），这是真实功能（粘贴图片喂给支持 vision 的模型）的
必需依赖，不建议为了体积砍掉。

## 已知限制

- MiniMax、GLM、Kimi、DeepSeek、Qwen、豆包在没有独立轮换 Key 的情况下只完成静态目录和协议 fixture，当前不宣称 live verified。
- macOS 已提供 Seatbelt 文件边界和域名级受控代理；Linux bubblewrap 已提供文件系统与网络 namespace 隔离，但域名 allowlist 在缺少可验证 namespace-to-proxy 桥接时会拒绝访问，不冒充“已允许”。
- 原生 Windows 暂无 Seatbelt/bubblewrap 同等级 OS sandbox；需要严格边界时建议在 WSL2 中运行并检查 `wyj-code sandbox` 输出。
- macOS 为兼容编译器/系统库读取，当前是全局只读 + 常见凭证路径 deny-read，并非“整个 home 完全不可遍历”；工作区和显式 write roots 才可写。
- Checkpoint 只能恢复会话和文件，不能撤销网络请求、数据库、已发送消息、外部应用或其他非文件副作用；自动 checkpoint 在大仓库中的 Git 扫描成本仍需继续优化。
- 自动 worktree 当前只覆盖 Workflow 中拥有写工具且配置 `write_roots` 的 Agent/Review 节点；普通 TUI 对话与独立 SubAgent 不会被静默迁入 worktree，接受和清理仍需显式操作。
- CodeIndex 当前是本地词法/符号索引 + plugin LSP `workspace/symbol`，并非 embedding/vector 语义检索；LSP 启动或协议失败时会保留本地索引和 direct-scan fallback。
- ACP daemon 默认监听本机回环地址且不提供公网鉴权层；如修改监听地址，调用方必须自行提供进程隔离、访问控制与传输保护。
- TUI 已恢复终端原生鼠标拖选；聊天历史仍由应用内 PageUp/PageDown 等键盘路径浏览，不把 alternate-screen 的终端 scrollback 宣称为已恢复。

## 贡献

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Fork → 特性分支 → PR，描述清楚动机与接口变化。详细约定见 [`CONTRIBUTING.md`](./CONTRIBUTING.md)。

## 许可证

MIT OR Apache-2.0，任选其一。见 [`LICENSE-MIT`](./LICENSE-MIT) / [`LICENSE-APACHE`](./LICENSE-APACHE)。
