# wyj-code

[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#快速开始)
[![Zero Telemetry](https://img.shields.io/badge/telemetry-none-green.svg)](#设计原则)

一个用 **Rust** 从零实现的终端 AI 编程助手——单二进制、原生 TUI、双协议 LLM 适配、
多 Agent 编排、可插拔 MCP 工具链。在 shell 里直接与 Claude / GPT 协作改代码。

> **定位说明**：这是个人工程作品集项目，用于展示 AI 系统的工程实现能力，非商业产品。
> 依据公开的 Anthropic Messages API、OpenAI Chat Completions API、MCP 公开规范
> clean-room 实现，不含任何第三方专有 prompt 或品牌资产，所有文案均为原创。

## 特性

**核心引擎**
- **Agent 推理循环** —— 流式 SSE 解析 → 累积工具调用 → 并发执行 → 结果回填，按 `stop_reason` 续接直到完成。
- **双协议 LLM 适配** —— Anthropic Messages 与 OpenAI Chat Completions 双格式，混合配置（规划用 GPT、执行用 Claude）开箱即用。
- **多 Agent 编排** —— 内置 `general-purpose` / `Explore`(只读) / `Plan` 三类子 Agent，支持自定义定义文件；`parallel_safe` 工具单回合内并发、其余保序回填；进程级 Hub 管理并发上限、前后台调度、ESC 中断。
- **上下文压缩** —— 估算 token 数接近窗口上限时自动摘要替换旧消息，保留最近若干条。
- **可插拔 MCP** —— 内置 Bash / Read / Write / Edit / Glob / Grep / WebFetch / TodoWrite，并桥接任意外部 MCP server。

**交互与配置**
- **原生 ratatui TUI** —— 流式 markdown、语法高亮、多行编辑、工具调用实时展示、子 Agent 聚合面板、Ctrl+O 展开明细。
- **Profile 分组配置** —— provider / model / base_url / api_key 以具名 Profile 组织，多套供应商配置并存切换，`/model` 面板管理。
- **三模式 + 分模型** —— `normal` / `plan`(只读) / `bypass`(自动放行)，各模式可绑定不同模型。
- **CLAUDE.md 记忆机制** —— 每轮重新读盘，全局 + 祖先链文件以 `<system-reminder>` 注入当轮 user 消息；工具触达新目录时动态加载；`@path` 递归导入。
- **会话中消息注入** —— Agent 忙碌时新消息进队列不打断，在工具调用往返边界排空合并。
- **i18n** —— 中 / 英双语，运行时切换，覆盖核心用户可见文案。
- **会话持久化** —— 自动写入 `~/.wyj-code/`，`-c` 续上次、`--resume <id>` 恢复指定会话。
- **零遥测** —— 仅在显式 LLM / WebFetch / MCP 调用时出网。

## 目录结构

```text
crates/
├── api/         # Anthropic / OpenAI 协议适配、SSE 流式解析
├── cli/         # 二进制入口、参数解析
├── commands/    # Slash 命令系统（/help /model /agents /compact …）
├── config/      # Profile 分组配置加载
├── core/        # Agent 循环、Session、Tool trait、压缩、CLAUDE.md、Memory
├── i18n/        # 中英双语资源
├── mcp/         # MCP 客户端（stdio / http）
├── tools/       # 内置工具 + SubAgent + AgentHub
└── tui/         # ratatui 前端
```

## 快速开始

```bash
cargo build --release           # → target/release/wyj-code
cargo run                       # TUI 模式
cargo run -- -p "你的问题"       # 单次问答
cargo run -- --headless         # REPL 模式
cargo run -- --config-status    # 查看配置状态
```

配置文件 `~/.wyj-code/config.toml`，API Key 优先读环境变量 `WYJ_CODE_API_KEY`：

```toml
[profiles.default]
provider = "anthropic"          # 或 "openai"
model    = "claude-opus-4-8"
# base_url / api_key 可选，留空用默认

plan_model = ""                 # Plan 模式专用，留空回退 model
exec_model = ""                 # Normal/Bypass 专用，留空回退 model
language   = ""                 # "en"/"zh"，留空自动检测系统 locale
```

TUI 内 `/model` 管理 Profile、`/mode` 切换模式、`/agents` 查看子 Agent、`/compact` 压缩、`/config` 设置面板。完整命令见 `/help`。

## 设计决策

这里记录的不是"复刻了什么"，而是"为什么这样做"——区别于照搬规范的部分。

- **Profile 分组而非扁平字段**：真实场景下一套配置不够——主力 Claude、备援 OpenAI、本地 Ollama 往往并存。把 provider/model/base_url/api_key 收进具名 Profile，`/model` 面板增删改查，比官方工具的单一配置模型更贴近多供应商实际用法。
- **消息注入而非打断轮次**：用户在 Agent 思考中途补充信息是高频需求。直接打断会丢失中间状态；改成 `pending_queue` + `InjectionKind`，在工具调用往返边界排空合并进当前或续接的 user 回合，对用户透明、对 LLM 无感。
- **子 Agent 进程级 Hub**：多 agent 真正的难点是调度与回收，不是 spawn。用单例 Hub 统一分配 id、`Semaphore` 限并发、前台 oneshot / 后台经 system-reminder 通道回注、ESC 只 abort 前台、退出 `abort_all`——把生命周期管理做成一等公民。
- **Rust + async + workspace**：选 Rust 不是赶时髦，是这个场景天然契合——流式解析、并发工具执行、零运行时依赖的单二进制分发。八个 crate 的切分让职责边界在编译期就强制清晰。

## 设计原则

- **本地优先** —— 源码、配置、历史只属于你，无任何隐式埋点或崩溃上报。
- **透明可控** —— Agent 循环、工具调用、权限确认全程实时展示，随时可打断。
- **协议中立** —— 改一行 `provider` 即可切换供应商，不绑定任何一家。

## 贡献

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Fork → 特性分支 → PR，描述清楚动机与接口变化。

## 许可证

MIT OR Apache-2.0
