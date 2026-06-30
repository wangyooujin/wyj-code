# wyj-code

[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#快速开始)
[![Zero Telemetry](https://img.shields.io/badge/telemetry-none-green.svg)](#设计原则)

一个用 **Rust** 编写的终端 AI 编程助手——单文件静态二进制、原生终端 UI、双协议
LLM 适配、可插拔 MCP 工具链。让你在 shell 里就能与 Claude / GPT 协作改代码，
而不是被困在某个 SaaS 网页里。

> **独立实现声明**：本项目是 clean-room 净室实现，仅依据公开的 Anthropic Messages API、
> OpenAI Chat Completions API、MCP/LSP 公开规范实现等价功能。不包含任何第三方的
> 专有系统提示词、工具描述文案或品牌资产。所有 prompt 与文案均为原创。

## 特性

- **单二进制分发** —— `cargo build --release` 产出无运行时依赖的可执行文件，丢到任何
  机器直接跑，无需 Node / Python / Docker。
- **双协议 LLM 适配** —— 同时支持 Anthropic Messages API 与 OpenAI Chat Completions
  API，混合配置（如"规划用 GPT、执行用 Claude"）开箱即用。
- **原生 ratatui TUI** —— 不被 Electron 绑架：流式 markdown 渲染、语法高亮、多行编辑、
  工具调用实时展示、全键盘流操作。
- **可插拔 MCP 工具链** —— 内置 Bash / Read / Write / Edit / Glob / Grep / WebFetch 七件
  工具，并通过 Model Context Protocol 接入任意外部 MCP server（数据库 / 浏览器 / GitHub …）。
- **三种 Agent 模式** —— `normal`（带权限确认）、`plan`（只读规划）、`bypass`（自动放行），
  不同模式可绑定不同模型。
- **会话历史持久化** —— 自动写入 `~/.wyj-code/history/`，下次启动可继续上一次对话。
- **零遥测、零网络上报** —— 所有数据留在你的机器上，仅在你显式调用 LLM / WebFetch
  / MCP 时才出网。

## 目录结构

```text
wyj-code/
├── crates/
│   ├── api/         # Anthropic / OpenAI 协议适配、流式解析
│   ├── cli/         # 命令行入口（wyj-code 二进制）
│   ├── commands/    # Slash 命令系统（/help、/model、/compact …）
│   ├── config/      # ~/.wyj-code/ 配置加载
│   ├── core/        # Agent 循环、Session、Tool trait
│   ├── mcp/         # MCP 客户端（stdio / SSE 传输）
│   ├── tools/       # 内置 7 件工具实现
│   └── tui/         # ratatui 前端
├── Cargo.toml       # workspace 根
├── CLAUDE.md        # 项目级 Claude 配置（给 AI 助手看的）
└── README.md
```

## 快速开始

### 构建

```bash
cargo build --release           # → target/release/wyj-code
cargo run -- --version          # 验证安装
cargo run -- --config-status    # 查看配置状态
```

### 配置

配置目录：`~/.wyj-code/`，主配置文件 `config.toml`。

### 基础配置

```toml
provider = "anthropic"   # 或 "openai"
model = "claude-opus-4-8"
base_url = ""            # 留空使用供应商默认端点
# api_key 优先从环境变量 WYJ_CODE_API_KEY 读取
```

### 不同模式下的模型

wyj-code 有三种 Agent 运行模式，可为不同模式分别指定模型，留空时回退到 `model` 字段。

| 模式      | 触发方式（示例）   | 行为                                     | 专用配置字段     |
| --------- | ------------------ | ---------------------------------------- | ---------------- |
| `normal`  | 默认               | 全部工具可用，TUI 下工具调用前弹确认     | `exec_model`     |
| `plan`    | Plan 分析          | 仅允许只读工具（read / glob / grep / web_fetch） | `plan_model`     |
| `bypass`  | 自动放行           | 自动允许所有工具调用，不弹确认对话框     | `exec_model`     |

> 说明：`normal` 与 `bypass` 共享 `exec_model`（执行型模型），`plan` 模式单独使用
> `plan_model`（规划型模型）。通过 `Config::model_for_mode(mode)` 在运行时解析。

#### 示例：规划用快模型、执行用强模型

```toml
provider    = "anthropic"
model       = "claude-opus-4-8"          # 全局兜底模型
plan_model  = "claude-haiku-4-5"         # Plan 模式：规划分析，便宜快速
exec_model  = "claude-opus-4-8"          # Normal/Bypass：实际改代码，用强力模型
base_url    = ""
```

#### 示例：Anthropic 主模型 + OpenAI 备援

```toml
provider    = "anthropic"
model       = "claude-opus-4-8"
plan_model  = "gpt-4o-mini"              # 用 OpenAI 兼容端点跑轻量规划
exec_model  = "claude-opus-4-8"
base_url    = ""                          # provider 决定协议与默认端点
```

如果 `plan_model` / `exec_model` 留空或省略，则使用 `model` 字段。

### 运行时切换模型

在 TUI 会话中可用 `/model` 命令查看或临时切换当前会话模型：

```text
/model                  # 显示当前模型
/model claude-haiku-4-5 # 切换当前会话模型（不写盘）
```

持久化的模式-模型映射请直接编辑 `~/.wyj-code/config.toml`。

## 设计原则

- **本地优先** —— 所有源码、配置、历史只属于你。不内置任何隐式埋点、崩溃上报、
  行为分析；联网只发生在你显式触发的 LLM 调用与工具执行。
- **透明可控** —— Agent 循环、工具调用、权限确认全部在 TUI 实时展示，每一步
  你都能看到、都能打断。
- **协议中立** —— 不被任何一家模型供应商绑定。Anthropic / OpenAI / 本地 vLLM /
  Ollama OpenAI 兼容端点…… 改一行 `provider` 即可切换。
- **小而清晰** —— 八个 crate 各司其职，无运行时依赖膨胀、无重型前端框架，
  clone 下来 `cargo build` 就能跑。

## 路线图

- ✅ 双协议 LLM 客户端（Anthropic / OpenAI）
- ✅ 七件核心工具（Bash / Read / Write / Edit / Glob / Grep / WebFetch）
- ✅ ratatui 原生终端 UI + 流式渲染
- ✅ Slash 命令系统 + 会话历史持久化
- ✅ TodoWrite、子 Agent、MCP 客户端
- 🔄 上下文压缩（/compact 自动摘要）
- 🔄 Skill / Memory / AskQuestion 增强
- 🔄 多平台 release（macOS / Linux / Windows 静态二进制）

## 贡献

欢迎 PR 与 Issue。建议流程：

1. Fork → 新建特性分支
2. `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
3. 提交前确保 `cargo build --release` 通过
4. 在 PR 中描述清楚动机、接口变化、测试覆盖

代码风格遵循仓库根目录的 `rustfmt.toml`。

## 许可证

MIT OR Apache-2.0
