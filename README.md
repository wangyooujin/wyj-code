# wyj-code

[![Release](https://img.shields.io/badge/release-v1.5.13-ffb454.svg)](https://github.com/wangyooujin/wyj-code/releases/tag/v1.5.13)
[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#安装)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#开源协议)
[![Pages](https://img.shields.io/badge/Pages-在线主页-22c55e.svg)](https://wangyooujin.github.io/wyj-code/)

**面向真实开发工作的 Rust 终端 AI 编程助手。**

wyj-code 提供原生 TUI、代码读写与命令执行、多 Agent 协作、MCP/Skill/Plugin 扩展、
会话与工作区管理，以及默认安全失败关闭的本地执行环境。同时支持 Anthropic Messages
和 OpenAI Chat Completions 双协议，可连接 Claude、OpenAI、GLM、MiniMax、Kimi、DeepSeek、
Qwen/百炼、豆包/火山及其他协议兼容端点。

[项目主页](https://wangyooujin.github.io/wyj-code/) ·
[安装页面](https://wangyooujin.github.io/wyj-code/#install) ·
[GitHub Releases](https://github.com/wangyooujin/wyj-code/releases) ·
[更新日志](./CHANGELOG.md) ·
[架构文档](./doc/architecture.md) ·
[贡献指南](./CONTRIBUTING.md)

> **版本状态**：最新公开版本
> [v1.5.13](https://github.com/wangyooujin/wyj-code/releases/tag/v1.5.13)。
> 历史 tag 保持不可移动，一键安装脚本始终下载 GitHub 最新公开 Release。
>
> **国产模型适配报告**：[DeepSeek / GLM / Kimi / Qwen / 豆包 / MiniMax 与 Claude Code / Codex 的能力对照](./doc/analysis/domestic-models-vs-claude-code.md)
> （基于公开网络资料与社区分享的最佳实践总结，未包含实测 benchmark）。

---

## 目录

- [项目介绍](#项目介绍)
- [安装](#安装)
- [快速开始](#快速开始)
- [核心特性](#核心特性)
- [配置](#配置)
- [项目资源](#项目资源)
- [架构](#架构)
- [文档与社区](#文档与社区)
- [安全与隐私](#安全与隐私)
- [开发与贡献](#开发与贡献)
- [开源协议](#开源协议)

---

## 项目介绍

wyj-code 把 AI coding 的核心能力放进一个**可审计、可扩展、可本地运行**的终端工具：

- **原生终端体验**：单二进制发布，流式 Markdown、代码高亮、多行输入、图片粘贴、
  diff 预览、Todo、SubAgent 与权限确认界面。
- **多模型 + 双协议**：供应商、线协议与模型能力分开建模，同一套工具可连接官方 API、
  国内模型与本地兼容端点。
- **Agent 与工具系统**：内置代码搜索、文件编辑、Bash、Web、Computer Use 与 SubAgent，
  并支持 MCP、Skill、Plugin、Hooks 与自定义 slash 命令。
- **工程化工作流**：checkpoint / rewind / branch、隔离 Git worktree、Workflow DAG、
  ACP adapter、本地 daemon session、机器可读 Review。
- **安全执行**：权限审批 + OS sandbox 分层；headless、定时任务与 SubAgent 在没有
  交互批准时默认拒绝副作用操作。
- **本地优先**：配置、会话、执行轨迹、记忆保存在本机；无隐式遥测、无崩溃上报。
- **证据化自进化**：按用户目标记录 Episode，生成带证据的 Memory、Rule、Skill 候选；
  Rule/Skill 必须人工批准才生效。

本项目基于公开的 Anthropic Messages API、OpenAI API 与 MCP 规范独立实现，
不包含第三方专有 prompt 或品牌资产。

---

## 安装

### 一键安装

安装脚本自动识别 OS 与 CPU 架构，下载最新公开 Release 并校验 SHA-256，
无需 `sudo` 或管理员权限。

macOS / Linux：

```bash
curl -fsSL https://wangyooujin.github.io/wyj-code/install.sh | sh
```

Windows PowerShell：

```powershell
irm https://wangyooujin.github.io/wyj-code/install.ps1 | iex
```

默认安装位置：

- macOS / Linux：`~/.local/bin/wyj-code`
- Windows：`%USERPROFILE%\.wyj-code\bin\wyj-code.exe`

如不希望执行远程脚本，使用下面的预编译包或源码安装方式。

### 下载预编译包

前往 [GitHub Releases](https://github.com/wangyooujin/wyj-code/releases)，下载对应平台的压缩包：

- macOS：Apple Silicon / Intel
- Linux：x86_64 / ARM64
- Windows：x86_64

每个 Release 同时提供独立 checksum 与 `SHA256SUMS`。解压后可运行包内安装脚本，
也可直接把二进制放入自己的 `PATH`。

### 从源码安装

需要 Git 与 Rust 1.80+：

```bash
git clone https://github.com/wangyooujin/wyj-code.git
cd wyj-code
./build.sh install
```

只构建、不安装：

```bash
cargo build --release      # 产物在 target/release/wyj-code
./build.sh package         # 打包到 dist/<binary>-<version>-<platform>
./build.sh cross linux-x86_64  # 交叉编译（macOS 主机 → Linux x86_64）
./build.sh release         # 交互确认版本号 → bump + commit + tag + push
```

### 验证安装

```bash
wyj-code --version
wyj-code --config-status   # 查看当前 Profile、模型与 API Key 状态
wyj-code --help
wyj-code update            # 检查更新
```

---

## 快速开始

### 1. 配置 API Key

最简单的方式是环境变量（推荐；不会落盘到任何文件）：

macOS / Linux：

```bash
export WYJ_CODE_API_KEY="<your-api-key>"
```

Windows PowerShell：

```powershell
$env:WYJ_CODE_API_KEY = "<your-api-key>"
```

随后运行 `wyj-code`，在 TUI 内用 `/model` 选择或新建 Profile（供应商 / 协议 / 模型 / API 地址）。
全局配置文件 `~/.wyj-code/config.toml` 可用 `api_key_env` 引用环境变量名，**不要把真实凭据
提交到仓库**。

### 2. 启动 wyj-code

```bash
wyj-code                       # 启动交互式 TUI
wyj-code --cwd /path/to/proj   # 在指定项目中启动
wyj-code -p "分析这个项目"       # 单次问答，不启动 TUI
wyj-code --headless            # Headless REPL
wyj-code -c                    # 恢复上次会话
wyj-code --plan                # 以 Plan 模式启动（仅只读工具）
wyj-code --bypass-permissions  # 以 Bypass 模式启动（跳过权限确认，不关闭 sandbox）
```

### 3. 常用命令

| 命令 | 用途 |
|---|---|
| `wyj-code --config-status` | 查看当前 Profile、模型与 API Key 状态 |
| `wyj-code model doctor` | 检查模型身份、协议与能力来源 |
| `wyj-code sandbox` | 查看当前 OS sandbox 与网络隔离状态 |
| `wyj-code extensions list` | 查看已安装的 Skill、MCP 与 Plugin |
| `wyj-code workspace list` | 查看隔离 Git worktree |
| `wyj-code workflow --help` | 多 Agent Workflow 命令族 |
| `wyj-code session --help` | checkpoint、rewind、branch 命令族 |
| `wyj-code storage {status,doctor,prune}` | CAS 存储占用诊断与 GC |
| `wyj-code evolve doctor` | Evolution 配置、预算与健康状态 |

TUI 内输入 `/help` 查看全部 slash 命令；常用入口：

| 命令 | 用途 |
|---|---|
| `/model` | 选择或新建 Profile |
| `/agents` | 列出全部 Agent 类型 |
| `/subagents` | 查看运行中的子 Agent |
| `/extensions` | 管理 Skill / MCP / Plugin |
| `/checkpoint` `/rewind` `/branch` | 会话快照与分支 |
| `/sandbox` | 查看 sandbox 与网络隔离状态 |
| `/evolve` | Memory / Rule / Skill 候选治理 |

---

## 核心特性

### 终端体验

- 单二进制 release，无运行时依赖；macOS / Linux / Windows 全平台发布。
- ratatui TUI：流式 Markdown、代码高亮、多行输入、图片粘贴、diff 预览。
- 权限确认、AskQuestion 多题面板、ExitPlanMode 计划批准、TUI panic 兜底还原。

### 多模型与双协议

- **Anthropic Messages** 完整支持：thinking、cache_control、原生 `computer_20251124`、
  tool 内嵌图片回传。
- **OpenAI Chat Completions** 兼容：reasoning_content、image_url、tool_result 降级；
  OpenAI 协议本身不支持 tool 内嵌图片，computer-use 在该路径上注册即占位（"名存实亡"）。
- 已验证或协议兼容：Claude（Opus / Sonnet / Haiku）、OpenAI（GPT-5 / o-series）、
  DeepSeek（V3 / V4-Pro / R1）、GLM-4.6、Kimi K2、Qwen3-Max、字节豆包、MiniMax 等。

### Agent 与工具

- 内置工具：Read / Write / Edit / Bash（带 sandbox）/ Glob / Grep / WebFetch / WebSearch /
  TodoWrite / SubAgent / Computer / AppComputer / Jev。
- WebSearch 仅在配置 `search_api_key` 时注册；Computer 仅 macOS/Windows 编译且需
  vision + Anthropic profile；Jev 仅在 `[tools.jev].enabled=true` 且 API Key 可解析时注册。
- SubAgent 类型：内置 general-purpose / Explore（只读）/ Plan + 用户自定义六层合并链。

### 工程化工作流

- **会话管理**：checkpoint、rewind、branch、TUI 面板与 CLI 子命令族。
- **工作区**：隔离 Git worktree（`workspace create/list/diff/accept/dispose`），防御
  symlink、父 HEAD 前进、用户并发修改与 binary 漏洞。
- **Workflow**：DAG 并行、token budget、human approval、pause/resume/retry/skip/cancel。
- **ACP / daemon**：stdio adapter 与 TCP 全局 session registry，schema version 2。
- **Review**：`wyj-code review run --base HEAD^ --head HEAD --json` 生成可审计 JSON。

### 安全执行

- 权限模型：Normal（每次确认）/ Bypass（全部放行）/ Plan（白名单限制）。
- `AllowAlways` 写入项目级 `~/.wyj-code/projects/<project_key>/allowed_tools.json`，跨会话生效。
- OS sandbox：macOS Seatbelt / Linux bubblewrap；headless、schedule、SubAgent 没有真实 UI 时
  默认拒绝写文件、执行命令、控制电脑等副作用操作。
- 项目级 MCP server 需经 `wyj-code trust-mcp` 手动批准；批准记录落在仓库控制不到的
  `~/.wyj-code/projects/<project_key>/mcp_trust.json`。

### 本地优先与证据化

- 配置 / 会话 / 记忆 / 执行轨迹全部在 `~/.wyj-code/`；无隐式遥测。
- Memory v3：Global / Project 两层作用域，AI 自动管理项目记忆，Global 候选走 Pending +
  自然语言确认；reject 后指纹写入 `rejected_history.json` 防止反复提议。
- Evolution：每个用户目标落盘为独立 Episode，生成带 scope、citation、TTL、冲突的 Memory；
  Rule/Skill 永不自动激活，必须经 `/evolve` 或 `wyj-code evolve approve` 显式批准。
- Storage caps：Evolution 100 MiB / project、checkpoints 20 / session、CAS Blob LRU、顶层
  5 GiB 启动一次性 warn；全部 opt-out by `0`。

---

## 配置

完整配置字段、环境变量、各 Profile 模板与 Storage caps 见 [`CLAUDE.md`](./CLAUDE.md) 的
Configuration 节；架构与数据面背景见 [`doc/architecture.md`](./doc/architecture.md)。

### 全局配置 `~/.wyj-code/config.toml`（节选）

```toml
provider = "anthropic"            # 或 "openai" / 自定义 Profile
model = "claude-opus-4-8"
plan_model = ""                   # Plan 模式专用模型，留空则用 model
exec_model = ""                   # Bypass 模式专用模型，留空则用 model
base_url = ""                     # 留空使用供应商默认端点
max_tokens = 8192
context_window = 200000
vision = true                     # 模型支持图片输入；false 时图片降级为占位文本

log_level = "warn"                # 调试时设为 "debug"
language = ""                     # "en"/"zh"，留空自动检测系统 locale

# API Key 优先从环境变量 WYJ_CODE_API_KEY 读取；配置文件不要写明文 key
# 多个 Profile 通过 TUI 内 /model 管理；详见 CLAUDE.md "Profile" 节

[tools.jev]                       # TypeSafe Jev 决策 API（v1.5.13+）
enabled = false                   # 默认禁用；付费 API，显式开启才注册
api_key = ""                      # 留空读环境变量 TYPESAFE_API_KEY
base_url = ""                     # 留空用 https://api.typesafe.ai
model = "jev-latest"
max_state_chars = 32000           # state 字符上限
max_questions = 32                # 单次最多 questions 数量
daily_budget_usd = 5.0            # 进程级日预算；0 = 关闭

[subagent]
default_profile = ""              # 子 Agent 默认 Profile
explore_profile = ""              # Explore 类型专用 Profile
trace_enabled = true              # 子 Agent 完整执行轨迹落盘
trace_max_bytes_per_agent = 262144 # 单 agent trace 文件字节上限（默认 256KB）

[[mcp_servers]]
name = "my-server"
transport = "stdio"
command = "/path/to/server"
args = ["--flag"]
```

### 项目级配置 `<git-root>/.wyj-code/`

| 文件 | 用途 |
|---|---|
| `settings.toml` | `disabled_skills` / `disabled_mcp_servers` 开关 |
| `mcp.toml` | 项目级 MCP server（需 `wyj-code trust-mcp` 手动批准） |
| `skills/` | 项目级 Skill 与自定义命令 |
| `agents/` | 项目级 SubAgent 定义 |

项目资源从 Git 仓库根目录自动发现；从子目录启动仍用同一份 `.wyj-code/`。
项目文件不能覆盖全局模型凭据或静默扩大 sandbox 权限。

### 模型侧提示词

主 system prompt、模式追加段（Plan / non-interactive）、子 agent 内置提示、compact 结构化
摘要模板、记忆提取提示、全部工具描述均为**英文原创常量**，不走 i18n（模型行为不应随 locale
漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复）。

---

## 项目资源

| 路径 | 内容 |
|---|---|
| `~/.wyj-code/config.toml` | 全局 Profile、模型、语言、sandbox 与运行时配置 |
| `~/.wyj-code/sessions/` | 本地会话、checkpoint 与 SubAgent trace |
| `~/.wyj-code/cas/` | CAS Blob Pool（v1.5.11+，内容寻址会话快照）|
| `~/.wyj-code/evolution/` | Episode、Memory v3、Rule/Skill 候选数据 |
| `~/.wyj-code/schedule/` | 定时任务清单 + crontab 标记区块 |
| `~/.wyj-code/projects/<project_key>/` | 项目级 `allowed_tools.json` 与 `mcp_trust.json` |
| `<repo>/.wyj-code/` | 项目级 settings / MCP / Skill / Agent 资源 |

CLI 治理入口：

- `wyj-code storage {status,doctor,prune}` —— CAS 占用诊断与 GC
- `wyj-code extensions {list,doctor,migrate,install,upgrade,enable,disable,remove}` —— Skill/MCP/Plugin 治理
- `wyj-code evolve {status,list,review,feedback,skillize,approve,reject,rollback,forget,run,include,migrate,export,doctor}` —— Evolution 治理

---

## 架构

整体架构、Workspace crate 布局、Agent 推理循环、Provider / WireProtocol / Capability
三层模型、上下文管理（compact、CLAUDE.md 注入、Memory v3）、CAS 会话存储、权限模型、
SubAgent 编排、MCP/Skill/Plugin/Hooks 扩展点、TUI/Headless/ACP/Daemon 入口、Computer-use 与
Schedule、Storage caps 完整表格，详见 [`doc/architecture.md`](./doc/architecture.md)。

更细的实现细节、版本演进中的"教训"注释与"为什么这样设计"的背景说明在
[`CLAUDE.md`](./CLAUDE.md)——那是同时喂给 Claude Code 的项目上下文，是最权威、更新最及时的
架构参考。

---

## 文档与社区

| 内容 | 路径 |
|---|---|
| 项目 Pages 主页 | <https://wangyooujin.github.io/wyj-code/> |
| 在线安装页面 | <https://wangyooujin.github.io/wyj-code/#install> |
| 在线功能介绍 | <https://wangyooujin.github.io/wyj-code/#features> |
| 在线架构介绍 | <https://wangyooujin.github.io/wyj-code/#architecture> |
| 在线版本记录 | <https://wangyooujin.github.io/wyj-code/#changelog> |
| GitHub Releases | <https://github.com/wangyooujin/wyj-code/releases> |
| 仓库更新日志 | [`CHANGELOG.md`](./CHANGELOG.md) |
| 架构详细文档 | [`doc/architecture.md`](./doc/architecture.md) |
| 国产模型适配报告 | [`doc/analysis/domestic-models-vs-claude-code.md`](./doc/analysis/domestic-models-vs-claude-code.md) |
| 版本规划 | [`doc/plan/`](./doc/plan/) |
| 开发约束与架构细节 | [`CLAUDE.md`](./CLAUDE.md) |
| 贡献指南 | [`CONTRIBUTING.md`](./CONTRIBUTING.md) |

GitHub Pages 源文件位于 [`site/`](./site/)；一键安装脚本入口为
[`site/install.sh`](./site/install.sh) 与 [`site/install.ps1`](./site/install.ps1)。

---

## 安全与隐私

- **无隐式遥测**：只有显式模型请求、Web 工具或 MCP 调用会访问网络。
- **API Key 安全**：优先从环境变量读取，配置诊断只显示掩码信息；TUI 写盘后自动 `chmod 0600`。
- **`--bypass-permissions`**：只跳过普通交互确认，**不**关闭 OS sandbox，**不**覆盖 protected deny。
- **无 UI 时的默认拒绝**：Headless、Schedule、SubAgent 没有真实 UI 时不会自动批准写文件、
  执行命令或控制电脑等副作用操作。
- **Checkpoint 范围**：只能恢复会话和文件，**不**能撤销网络请求、数据库写入或已经发送的
  外部消息。
- **项目级 MCP 信任**：未信任的项目级 server 静默排除，不阻塞 UI-less 场景（`-p` / `--headless`
  / `schedule run`）。

发现安全问题时，请**不要**在 Issue 里附带真实凭据、私有 endpoint 或敏感日志。可以先
在 [GitHub Issue](https://github.com/wangyooujin/wyj-code/issues) 提交经过脱敏的报告。

---

## 开发与贡献

欢迎提交 Issue 和 Pull Request。开始开发前请阅读 [`CONTRIBUTING.md`](./CONTRIBUTING.md)——
那份文档涵盖 crate 职责、提交约定、`/help` 注册规范、i18n 规则与 PR 流程。

本地基础门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release
```

更详细的架构、Agent 数据流、权限模型与开发约束见 [`CLAUDE.md`](./CLAUDE.md) 与
[`doc/architecture.md`](./doc/architecture.md)。

---

## 开源协议

本项目沿用 **MIT OR Apache-2.0** 双重开源许可，使用者可任选其一：

- [`LICENSE-MIT`](./LICENSE-MIT)
- [`LICENSE-APACHE`](./LICENSE-APACHE)
