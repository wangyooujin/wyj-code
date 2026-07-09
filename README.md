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
- **Hooks 生命周期自动化** —— 复用 `.claude/settings.json`，在 PreToolUse / PostToolUse / UserPromptSubmit / Stop 四个节点挂任意 shell 脚本（拦截危险命令、保存即格式化、注入上下文、回合结束通知），`/hooks` 查看当前生效配置，`--no-hooks` 一键禁用。
- **自定义 slash 命令** —— 识别真实 Claude Code 的 `~/.claude/commands/*.md` 与 `.claude/commands/*.md`（与 wyj-code 自造的 `~/.wyj-code/skills`/`.wyj/skills` 并存，同作用域内真 CC 路径优先），frontmatter 支持 `description`/`argument-hint`/`allowed-tools`，`/help` 末尾动态列出全部自定义命令。
- **会话中消息注入** —— Agent 忙碌时新消息进队列不打断，在工具调用往返边界排空合并。
- **i18n** —— 中 / 英双语，运行时切换，覆盖核心用户可见文案。
- **会话持久化** —— 自动写入 `~/.wyj-code/`，`-c` 续上次、`--resume <id>` 恢复指定会话。
- **自更新** —— `wyj-code update` 检查 GitHub Releases 并原地替换二进制。
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

TUI 内 `/model` 管理 Profile、`/mode` 切换模式、`/agents` 查看子 Agent、`/compact` 压缩、`/config` 设置面板、`/hooks` 查看生命周期钩子。完整命令见 `/help`。

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
- **子 Agent 进程级 Hub**：多 agent 真正的难点是调度与回收，不是 spawn。用单例 Hub 统一分配 id、`Semaphore` 限并发、前台 oneshot / 后台经 system-reminder 通道回注、ESC 只 abort 前台、退出 `abort_all`——把生命周期管理做成一等公民。
- **Rust + async + workspace**：选 Rust 不是赶时髦，是这个场景天然契合——流式解析、并发工具执行、零运行时依赖的单二进制分发。八个 crate 的切分让职责边界在编译期就强制清晰。

## 设计原则

- **本地优先** —— 源码、配置、历史只属于你，无任何隐式埋点或崩溃上报。
- **透明可控** —— Agent 循环、工具调用、权限确认全程实时展示，随时可打断。
- **协议中立** —— 改一行 `provider` 即可切换供应商，不绑定任何一家。

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

- **输入框不总是贴住终端最底部**：TUI 用 `ratatui::Viewport::Inline` 按内容实际高度动态定高，
  而非撑满整个终端高度。曾尝试让 Inline viewport 撑到接近终端整高以让输入框固定贴底，但实测
  在 tmux 下 ratatui + crossterm 的光标位置查询有相当概率与实际渲染错位（不仅贴不到底，严重时
  刚输入的字符会不可见），且在最小复现示例中同样能独立复现，判断为 ratatui/crossterm 层面的
  限制而非本项目代码可修的 bug，故保留当前动态定高方案。
- **已冻结内容 resize 不会重新换行**：已写入终端原生 scrollback 的历史消息（`insert_before`）
  脱离应用状态管辖，终端窗口 resize 后不会重排；历史回看长度由终端自身 scrollback 缓冲区大小
  决定，应用内不再提供 PageUp/PageDown 等翻页快捷键（交还给终端原生处理）。

## 贡献

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Fork → 特性分支 → PR，描述清楚动机与接口变化。详细约定见 [`CONTRIBUTING.md`](./CONTRIBUTING.md)。

## 许可证

MIT OR Apache-2.0，任选其一。见 [`LICENSE-MIT`](./LICENSE-MIT) / [`LICENSE-APACHE`](./LICENSE-APACHE)。
