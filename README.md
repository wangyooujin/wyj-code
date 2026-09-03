# wyj-code

[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)
[![Release](https://img.shields.io/badge/release-v1.5.11-ffb454.svg)](https://github.com/wangyooujin/wyj-code/releases/tag/v1.5.11)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#安装)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#开源协议)
[![Pages](https://img.shields.io/badge/Pages-在线主页-22c55e.svg)](https://wangyooujin.github.io/wyj-code/)

**面向真实开发工作的 Rust 终端 AI 编程助手。**

wyj-code 提供原生 TUI、代码读写与命令执行、多 Agent 协作、MCP/Skill/Plugin 扩展、
会话与工作区管理，以及默认安全失败关闭的本地执行环境。它同时支持 Anthropic Messages
和 OpenAI Chat Completions 协议，可连接 Claude、OpenAI、GLM、MiniMax、Kimi、DeepSeek、
Qwen/百炼、豆包/火山以及其他协议兼容端点。

[项目主页](https://wangyooujin.github.io/wyj-code/) ·
[安装页面](https://wangyooujin.github.io/wyj-code/#install) ·
[GitHub Releases](https://github.com/wangyooujin/wyj-code/releases) ·
[更新日志](./CHANGELOG.md) ·
[贡献指南](./CONTRIBUTING.md)

[国产模型适配体验对比报告](./doc/analysis/domestic-models-vs-claude-code.md) — DeepSeek / GLM / Kimi / Qwen / 豆包 / MiniMax 与 Claude Code / Codex 的能力对照、踩坑与最佳实践。

> **版本状态**：最新公开版本是
> [v1.5.11](https://github.com/wangyooujin/wyj-code/releases/tag/v1.5.11)。历史 tag 保持不可移动，
> 一键安装脚本始终下载 GitHub 最新公开 Release。

## 项目介绍

wyj-code 希望把 AI coding 的核心能力放进一个可审计、可扩展、可本地运行的终端工具中：

- **原生终端体验**：单二进制发布，提供流式 Markdown、代码高亮、多行输入、图片粘贴、
  diff 预览、Todo、SubAgent 和权限确认界面。
- **多模型与双协议**：供应商、线协议和模型能力分开建模，同一套工具可以连接官方 API、
  国内模型和本地兼容端点。
- **Agent 与工具系统**：内置代码搜索、文件编辑、Bash、Web、Computer Use 和 SubAgent，
  并支持 MCP、Skill、Plugin、Hooks 与自定义 slash 命令。
- **工程化工作流**：支持 checkpoint、rewind、branch、隔离 Git worktree、Workflow DAG、
  ACP adapter、本地 daemon session 和机器可读 Review。
- **安全执行**：权限审批与 OS sandbox 分层；macOS 使用 Seatbelt，Linux 使用 bubblewrap；
  headless、定时任务和 SubAgent 在没有交互批准时默认拒绝副作用操作。
- **本地优先**：配置、会话、执行轨迹和记忆保存在本机，不包含隐式遥测或崩溃上报。
- **证据化自进化**：v1.5.5 可以按用户目标记录 Episode，生成带证据的 Memory、
  Rule 和 Skill 候选；Rule/Skill 必须人工批准，完整边界见
  [v1.5.5 计划](./doc/plan/v1.5.5-plan.md)。
- **Memory v3 单一数据面**：v1.5.6 收敛为 Global / Project 两层作用域，
  AI 自动管理项目记忆，Global 候选走 Pending + 自然语言确认；
  新增 Task 类型 + 动态 Project Brief，裸"继续"自动恢复最近未完成任务；
  `/memory clear-all` 一键清空重建，保留用户曾拒绝的指纹。
- **Session 存储 CAS + Delta 重构**：v1.5.11 把 `~/.wyj-code/sessions/` 的
  workspace snapshot 从每 checkpoint 内联 256 文件字节（~11MB/checkpoint）
  改为 sha256 内容寻址 CAS Blob Pool + 同 cwd 相邻 checkpoint Delta 链，
  实测长会话占用从 1.15GB 降至 ~3MB（~99% 压缩）；超大 base64 image 与
  长 thinking 自动外置到 CAS（`cas://<hash>` 引用，`materialize_block_with`
  在 resume 时还原）；新增 `wyj-code storage {status,doctor,prune}` 子命令
  做占用诊断与 GC。
- **`/new` slash 命令**：v1.5.11 新增 `/new`，对齐 Claude Code 开启新会话
  语义——自动保存当前会话历史后分配新 session_id、清空 TUI 状态，
  无二次确认弹窗；与 `/clear` 区分（清空 ≠ 全新会话）。

国内模型在没有独立 live probe 证据时只标记为 `static_only` 或 protocol-compatible；
协议兼容不等于每个模型、端点和工具组合都已经在线验证。可通过 `wyj-code model doctor`
查看当前模型身份、能力来源和兼容状态。

> 本项目是个人工程作品集项目，依据公开的 Anthropic Messages API、OpenAI API 和 MCP
> 规范独立 clean-room 实现，不包含第三方专有 prompt 或品牌资产，也不代表相关厂商官方产品。

## 安装

### 一键安装

安装脚本会识别操作系统和 CPU 架构，下载最新公开 Release，校验 SHA-256 后安装到用户目录，
不需要 `sudo` 或管理员权限。

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

如果不希望执行远程脚本，请使用下面的预编译包或源码安装方式。

### 下载预编译包

前往 [GitHub Releases](https://github.com/wangyooujin/wyj-code/releases)，下载对应平台的压缩包：

- macOS：Apple Silicon / Intel
- Linux：x86_64 / ARM64
- Windows：x86_64

每个 Release 同时提供独立 checksum 和 `SHA256SUMS`。解压后可以运行包内安装脚本，
也可以直接把二进制放入自己的 `PATH`。

### 从源码安装

需要 Git 和 Rust 1.80 或更高版本：

```bash
git clone https://github.com/wangyooujin/wyj-code.git
cd wyj-code
./build.sh install
```

`./build.sh install` 会构建 release 二进制，并原子安装到 `~/.local/bin/wyj-code`。
只构建、不安装时可以运行：

```bash
cargo build --release
```

### 验证安装

```bash
wyj-code --version
wyj-code --config-status
wyj-code --help
```

检查更新：

```bash
wyj-code update
```

## 快速开始

### 1. 配置 API Key

最简单的方式是通过环境变量提供当前 Profile 的 API Key：

macOS / Linux：

```bash
export WYJ_CODE_API_KEY="<your-api-key>"
```

Windows PowerShell：

```powershell
$env:WYJ_CODE_API_KEY = "<your-api-key>"
```

随后运行 `wyj-code`，在 TUI 中使用 `/model` 选择或配置供应商、协议、模型和 API 地址。
全局配置文件位于 `~/.wyj-code/config.toml`。推荐使用 `api_key_env` 引用环境变量名，
不要把真实凭据提交到仓库。

### 2. 启动 wyj-code

```bash
# 启动交互式 TUI
wyj-code

# 在指定项目中启动
wyj-code --cwd /path/to/project

# 单次问答，不启动 TUI
wyj-code -p "分析这个项目并说明主要模块"

# Headless REPL
wyj-code --headless

# 恢复上次会话
wyj-code -c
```

### 3. 常用命令

| 命令 | 用途 |
|---|---|
| `wyj-code --config-status` | 查看当前 Profile、模型和 API Key 状态 |
| `wyj-code model doctor` | 检查模型身份、协议和能力来源 |
| `wyj-code sandbox` | 查看当前 OS sandbox 与网络隔离状态 |
| `wyj-code extensions list` | 查看已安装的 Skill、MCP 和 Plugin |
| `wyj-code workspace list` | 查看隔离 Git worktree |
| `wyj-code workflow --help` | 查看多 Agent Workflow 命令 |
| `wyj-code session --help` | 查看 checkpoint、rewind 和 branch 命令 |
| `wyj-code evolve doctor` | 检查本地 Evolution 配置、预算和健康状态 |

TUI 内输入 `/help` 可以查看全部 slash 命令。常用入口包括 `/model`、`/extensions`、
`/agents`、`/subagents`、`/sandbox`、`/checkpoint`、`/rewind`、`/branch` 和 `/evolve`。

## 配置与项目资源

| 路径 | 内容 |
|---|---|
| `~/.wyj-code/config.toml` | 全局 Profile、模型、语言、sandbox 与运行时配置 |
| `~/.wyj-code/sessions/` | 本地会话、checkpoint 和 SubAgent trace |
| `~/.wyj-code/evolution/` | 本地 Episode、Memory 和 Rule/Skill 候选数据 |
| `<repo>/.wyj-code/settings.toml` | 项目级资源开关 |
| `<repo>/.wyj-code/mcp.toml` | 项目级 MCP server |
| `<repo>/.wyj-code/skills/` | 项目级 Skill 与 slash 命令 |
| `<repo>/.wyj-code/agents/` | 项目级 SubAgent 定义 |

项目资源会从 Git 仓库根目录自动发现。从仓库子目录启动时，仍会使用同一份 `.wyj-code/`
配置。项目文件不能覆盖全局模型凭据或静默扩大 sandbox 权限。

## Pages 与文档路径

| 内容 | 路径 |
|---|---|
| 项目 Pages 主页 | <https://wangyooujin.github.io/wyj-code/> |
| 在线安装页面 | <https://wangyooujin.github.io/wyj-code/#install> |
| 在线功能介绍 | <https://wangyooujin.github.io/wyj-code/#features> |
| 在线架构介绍 | <https://wangyooujin.github.io/wyj-code/#architecture> |
| 在线版本记录 | <https://wangyooujin.github.io/wyj-code/#changelog> |
| GitHub Releases | <https://github.com/wangyooujin/wyj-code/releases> |
| 仓库更新日志 | [`CHANGELOG.md`](./CHANGELOG.md) |
| 版本规划 | [`doc/plan/`](./doc/plan/) |
| 项目架构与开发约束 | [`CLAUDE.md`](./CLAUDE.md) |
| 贡献指南 | [`CONTRIBUTING.md`](./CONTRIBUTING.md) |

GitHub Pages 的仓库源文件位于 [`site/`](./site/)，安装脚本入口分别是
[`site/install.sh`](./site/install.sh) 和 [`site/install.ps1`](./site/install.ps1)。

## 安全与隐私

- wyj-code 没有隐式遥测；只有显式模型请求、Web 工具或 MCP 调用会访问网络。
- API Key 优先从环境变量读取，配置诊断只显示掩码信息。
- `--bypass-permissions` 只跳过普通交互确认，不会关闭 OS sandbox 或覆盖 protected deny。
- Headless、schedule 和 SubAgent 没有真实 UI 时，不会自动批准写文件、执行命令或控制电脑等副作用操作。
- Checkpoint 只能恢复会话和文件，不能撤销网络请求、数据库写入或已经发送的外部消息。

发现安全问题时，请不要公开附带真实凭据、私有 endpoint 或敏感日志。可以先提交经过脱敏的
[GitHub Issue](https://github.com/wangyooujin/wyj-code/issues)。

## 开发与贡献

欢迎提交 Issue 和 Pull Request。开始开发前请阅读 [`CONTRIBUTING.md`](./CONTRIBUTING.md)。

本地基础门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

更详细的 crate 职责、Agent 数据流、权限模型和开发约束见 [`CLAUDE.md`](./CLAUDE.md)。

## 开源协议

本项目沿用 **MIT OR Apache-2.0** 双重开源许可，使用者可以任选其一：

- [`LICENSE-MIT`](./LICENSE-MIT)
- [`LICENSE-APACHE`](./LICENSE-APACHE)
