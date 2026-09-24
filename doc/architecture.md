# wyj-code 架构

> 本文档是 [`CLAUDE.md`](../CLAUDE.md) 的用户/开发者面向版：剥离了喂给 AI 的对话语气与版本历史注解，专注于讲清楚"系统怎么组成、各部分如何协作、关键约束是什么"。
>
> **维护原则**：架构变更（新增 crate、新增数据面、引入新的运行时门控）必须同步更新本文件；只改实现不动本文档的 PR 应在描述里说明原因。

---

## 目录

- [设计哲学与硬约束](#设计哲学与硬约束)
- [Workspace 布局](#workspace-布局)
- [核心数据流](#核心数据流)
- [Tool 与 Agent 推理循环](#tool-与-agent-推理循环)
- [Provider / WireProtocol / Capability 三层模型](#provider-wireprotocol-capability-三层模型)
- [上下文管理](#上下文管理)
  - [Compact 与持久化截断](#compact-与持久化截断)
  - [CLAUDE.md 注入](#claudemd-注入)
  - [Memory v3](#memory-v3)
- [会话与 Checkpoint](#会话与-checkpoint)
  - [存储 CAS + Delta 重构](#存储-cas-delta-重构)
- [权限模型](#权限模型)
- [SubAgent 编排](#subagent-编排)
- [扩展点](#扩展点)
  - [MCP](#mcp)
  - [Skill / 自定义命令](#skill-自定义命令)
  - [Agent 定义](#agent-定义)
  - [Hooks 生命周期自动化](#hooks-生命周期自动化)
- [TUI / Headless / ACP / Daemon 入口](#tui-headless-acp-daemon-入口)
- [Computer-use 与 Schedule](#computer-use-与-schedule)
- [Storage caps](#storage-caps)
- [进一步阅读](#进一步阅读)

---

## 设计哲学与硬约束

wyj-code 的架构决策围绕以下硬约束展开，违反任何一条都需要先有 plan 文档说明取舍：

1. **本地优先**——配置、会话、执行轨迹、记忆全部在 `~/.wyj-code/`；无隐式遥测，无崩溃上报，模型请求以外的出站网络必须由用户显式触发。
2. **供应商、协议、能力分离**——`Provider` 负责认证与端点，`WireProtocol` 决定序列化（Anthropic Messages / OpenAI Chat Completions），`ModelCapabilities` 描述模型行为面（thinking、vision、parallel_tool_calls 等）。三者解耦后同一套工具可在官方 API、国内厂商、协议兼容端点之间切换。
3. **证据化**——所有跨会话学习（Memory v3、Rule/Skill 候选）必须有可验证的 Episode 证据，且默认情况下 Rule/Skill 永不自动激活。
4. **失败关闭**——headless、schedule、SubAgent 在没有交互通道时默认拒绝写文件、执行命令、控制电脑等副作用操作；OS sandbox 是兜底而非主防线。
5. **数据面收敛**——同一类用户状态只有一份权威存储（如 Memory v3 收敛为 Global / Project 两层后，Evolution 不再维护平行 Memory 数据层）。
6. **增量可恢复**——跨进程的工作（SubAgent trace、定时任务日志、Skill 安装）必须用原子写（temp + rename）或事务式写入，进程中断后下一进程能从上一致状态继续。

---

## Workspace 布局

Cargo workspace 包含 11 个 crate，按"配置 → API → 核心 → 工具 → 命令 → TUI/CLI → 扩展"组织：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、Profile / ModelRuntime / MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析、Capability 探测与缓存 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session runtime/events、HistoryStore、Memory v3、ClaudeMdLoader、Hooks、Checkpoint、CAS 存储、Workspace/Workflow 接口 |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/Glob/Grep/WebFetch/WebSearch/TodoWrite/SubAgent/Computer/AppComputer/Jev 等）|
| `crates/computer` | `wyj-computer` | computer-use 系统层：`xcap` 截图 + `enigo` 输入合成 + 平台无关的坐标缩放数学（`scale` 模块）|
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact、/model、/agents、/evolve 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`rust-i18n` 封装，`en`/`zh` 内嵌 YAML）与运行时语言切换 |
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/store` | `wyj-store` | MCP / Skill / Plugin 配置与安装数据层、`extensions` 命令族、`schedule` 与 `cron_sync`、lockfile、marketplace |
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框、panic 兜底还原 |
| `crates/cli` | 二进制入口 | 解析 CLI 参数，组装所有 crate；启动 TUI / Headless REPL / 单次 `-p` 模式 / ACP adapter / daemon |

二进制名 `wyj-code`；workspace 版本 `1.5.13`，最低 Rust `1.80`，双协议许可（MIT OR Apache-2.0）。

---

## 核心数据流

```
┌─────────────────────────────────────────────────────────────────────┐
│                          CLI / TUI / Headless / ACP                  │
│              (crates/cli 解析参数 + crates/tui 渲染界面)             │
└─────────────────┬───────────────────────────────────┬───────────────┘
                  │ UserPrompt                         │ /command
                  ▼                                    ▼
        ┌──────────────────────┐              ┌────────────────────┐
        │  Agent::run_turn_    │              │  Command Registry  │
        │  with_injection      │              │  (wyj-commands)    │
        │  (wyj-core)          │              └────────────────────┘
        └──────────┬───────────┘
                   │ streaming request
                   ▼
        ┌──────────────────────────────────────────────────────────┐
        │              Provider  (wyj-api)                          │
        │   ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐    │
        │   │ Anthropic   │  │   OpenAI    │  │ 兼容端点         │    │
        │   │ Messages    │  │ Chat Compl. │  │ (Custom Profile)│    │
        │   └─────────────┘  └─────────────┘  └─────────────────┘    │
        └─────────────────────────┬────────────────────────────────────┘
                                  │ SSE StreamEvent
                                  ▼
        ┌──────────────────────────────────────────────────────────┐
        │   Tool Registry (wyj-tools)                              │
        │   Read Write Edit Bash Glob Grep WebFetch WebSearch       │
        │   TodoWrite SubAgent Computer AppComputer Jev ...        │
        │   + MCP 桥接工具 (wyj-mcp)                                │
        │   + 用户 Skill / 命令                                   │
        └─────────────────────────┬────────────────────────────────────┘
                                  │ ToolResult
                                  ▼
        ┌──────────────────────────────────────────────────────────┐
        │            SessionStore + HistoryStore                    │
        │   (wyj-core: 持久化到 ~/.wyj-code/sessions/)              │
        │   CAS Blob Pool + Delta Snapshot (v1.5.11)                │
        └──────────────────────────────────────────────────────────┘
```

**关键路径**：

1. 用户输入 → Agent 推理循环 → Provider 流式输出 → 累积 tool_use → 并发执行（`parallel_safe` 工具组内并发，其余串行但与并发组并行）→ ToolResult 回灌 → 续轮直到 `stop_reason != tool_use`。
2. 每轮开始时 `run_turn_with_injection` 从 `ClaudeMdLoader` 读 `CLAUDE.md`/`CLAUDE.local.md`/`AGENTS.md`，按目录去重拼到当轮 user 消息（不进历史，配合 prompt cache）。
3. 子 Agent 由 `tools::SubAgentTool` 整体 `tokio::spawn` 进 `SubAgentHub`（进程级单例，Semaphore 并发上限 8），事件汇入 hub 统一回调；后台完成结果经注入通道或 `pending_bg_reminders` 送达主 Agent。
4. TUI 与 headless 共享同一套 Agent / Provider 装配；差异只在渲染层与 `ToolCtx.permission_mode` 默认值。

---

## Tool 与 Agent 推理循环

所有工具实现统一 trait：

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;          // 发给模型的 schema
    fn needs_permission(&self) -> bool;              // 是否走 confirm_tool
    fn action_summary(&self, input: &Value) -> String;
    async fn run(&self, input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>;
}
```

`ToolContext` 是 `Send + Sync` 的，承担权限确认（`confirm_tool`）、UI 询问（`ui_ask_tx`）、状态读取、pending 消息入队等职责。

**Agent 循环（`wyj-core/src/agent.rs`）**：

1. 装配 `RequestOptions`（由 `RequestPlan` 把 `ModelCapabilities` 编译成下游可消费的 `ReasoningRequest` / `ToolRequestPolicy` / `CachePolicy`）。
2. 流式接收 `StreamEvent`，累积 `ContentBlock`；遇到 `tool_use` 闭合则入栈。
3. 一轮 `tool_use` 全部收齐后按 `parallel_safe` 分组：组内 `join_all` 并发，组间与串行工具保持原始下标，最终按原始顺序回填到 assistant message。
4. 每个 tool result 依次经过 `PreToolUse` hook → 权限检查 → 执行 → `PostToolUse` hook → 持久化截断（`persist_cap`）→ 回到 SessionStore。
5. 若用户在中途提交新消息，进入 `AppState.pending_queue`，由 `run_turn_with_injection` 在工具调用往返边界排空注入。
6. 一轮结束触发 `Stop` hook（仅 `!has_tool_calls && !got_injection` 分支）。

**ToolContext**：

- `permission_mode`：Prompt（Normal 模式）/ AutoApprove（Bypass）/ Allowlist（Plan，白名单限制）。
- `confirm_tool(name, summary)`：经 `ui_ask_tx` 发 `UiAskRequest::ToolPermission`，TUI 弹 `PermissionDialog`（y=AllowOnce / a=AllowAlways / d·Esc=Deny）。
- `ui_ask_tx` 同时承载 AskQuestion 多题面板与 ExitPlanMode 计划批准。
- 子 Agent ctx 不接 `ui_ask_tx`，`confirm_tool` 默认放行（不阻塞、不弹窗）。

---

## Provider / WireProtocol / Capability 三层模型

| 层 | 职责 | 关键类型 |
|---|---|---|
| **Provider** | 认证、端点、HTTP 客户端、流式封装 | `trait Provider`、`AnthropicProvider`、`OpenAIProvider`、`CustomProvider` |
| **WireProtocol** | 请求/响应序列化、SSE 事件、Tool 协议 | `AnthropicMessages`、`OpenAIChatCompletions` |
| **Capability** | 模型行为面：thinking、vision、parallel_tool_calls、max_tokens 等 | `ModelCapabilities`、`CapabilityCache`（7 天 TTL）、`ModelIdentity` |

**关键设计**：

- 同一 `Provider`（如 `AnthropicProvider`）可对接任意 Anthropic 兼容端点；官方端点判定由 `is_official_anthropic_endpoint()`（`provider == Anthropic` 且 `base_url` 为空或等于官方地址）单独控制某些能力（如原生 `computer_20251124`）。
- Capability 解析来源：① 内置 vendor 静态表（`api/src/model_catalog.rs`）② `CapabilityCache` 持久化探测结果 ③ 用户自定义 Profile 覆盖。
- Jev 决策 API（`tools/jev.rs`）不新增 `WireProtocol` 变体、不改 `Provider` trait——它走 ToolRegistry 直接调 HTTP，避免污染 routing/capability_cache 分桶。
- 模型侧提示词（`core/prompts` + `tools/descriptions`）为英文常量不走 i18n；末尾 "reply in the user's language" 保证中文用户得到中文回复。
- 双协议降级：Anthropic 路径完整（thinking、cache_control、native computer_20251124）；OpenAI Chat Completions 路径上 image_url 降级、thinking_* 直接忽略、tool 内嵌图片不可用（影响 computer-use 截图回传）。

---

## 上下文管理

### Compact 与持久化截断

**Token 估算**（`core/compact.rs::estimate_text_tokens`）：

- CJK：1.5 token/字（修订后 0.33 字节/token，避免低估）
- 非 CJK：0.25 token/字
- image ≈ b64 字节 × 3/4 / 750，封顶 1600

**触发**：`estimated > context_window - buffer`（`buffer = min(40000, max(4000, context_window/5))`），调 LLM 生成摘要替换旧消息，保留最近 6 条；连续 3 轮仍未收敛视为"上下文爆了"。

**持久化前截断**（`persist_cap`）：

| 内容 | 上限 |
|---|---|
| tool_result | 20K head + 10K tail |
| thinking | 8K |
| tool_use.input | 64K |

落盘前在 `serialize.rs` 走 `truncate_session_for_persistence`，resume/load 时再还原。

### CLAUDE.md 注入

`core::claude_md::ClaudeMdLoader`：

- 查找范围：全局 `~/.claude/CLAUDE.md` + 从 git 仓库根到 cwd 的祖先链。
- 每级目录内 `CLAUDE.md`/`CLAUDE.local.md` 都存在则都读（local 视作个人覆盖追加，不提交 git）；两者都不存在则回退读 `AGENTS.md`。
- 支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。
- 每轮对话开始重新读盘，以 `<system-reminder>` 包装拼进当轮 user 消息末尾（不注入 system、不进历史；文件不变时字节级稳定、缓存可命中）。
- 工具（Read/Edit/Write/Glob/Grep）触达新子目录时，若该目录有 CLAUDE.md 系文件且本会话未展示过，在 `agent.rs` 工具执行循环里追加到 system 末尾（按目录去重）。
- 不再兼容旧版 `WYJ.md`。

### Memory v3

收敛为 Global / Project 两层作用域：

- **Project scope**：AI 自动管理，记录项目级约定、当前任务、踩过的坑。
- **Global scope**：候选走 `PendingGlobalCandidate`，由模型用 Memory 工具三步自然语言确认（`list_pending_global_candidates / confirm_global_candidate / reject_global_candidate`）。
- reject 后的 `(scope, kind, title, content_fingerprint)` 写入 `rejected_history.json`，重复提议同一指纹会被立即拒绝。
- Task 类型 + Project Brief：动态生成"开放任务 + 下一步 + 阻塞原因 + 最近重要事件 + 相关 claims"；用户输入"继续/接着/continue/resume"时自动注入最近 InProgress Task 详情。
- `MemoryV3Store::clear_all` 一键清空（保留 `rejected_history.json`），重建 `active_records == 0`；AI 不允许静默触发。
- Evolution 普通 Memory 数据层仅保留兼容桩（v3 启用时旧 `MemoryStore` 不再注入、不再自动生成）。

---

## 会话与 Checkpoint

会话文件位于 `~/.wyj-code/sessions/`，结构：

```
~/.wyj-code/sessions/
├── <session_id>.json                  # SessionFile（消息流 + 元数据）
├── <session_id>.checkpoints/<n>.json  # checkpoint 快照
└── <session_id>.subagents/a<id>.jsonl # 子 Agent 完整执行轨迹
```

Checkpoint 支持：

- `create`：保存当前会话与 workspace 快照。
- `rewind`：恢复到指定 checkpoint。
- `branch`：从历史 checkpoint 创建新会话分支。
- TUI 通过 `/checkpoint` `/rewind` `/branch` 触发；CLI 通过 `wyj-code session` 子命令族触发；ACP / daemon 通过 `session/control` JSON-RPC 触发。

### 存储 CAS + Delta 重构

把每 checkpoint 内联的 workspace snapshot（~11MB/checkpoint，256 文件字节）改为 sha256 内容寻址 CAS Blob Pool：

```
~/.wyj-code/cas/sha256/aa/bb/<hash>.blob      # 内容
~/.wyj-code/cas/sha256/aa/bb/<hash>.meta.json # 元数据
```

`FileEntry { hash, inline_bytes, size, sha256 }` 替代内联字节。同 cwd 相邻 checkpoint 自动 fold 父链（最多 20 层），跨 cwd 强制 baseline。超大 base64 image 与长 thinking 自动外置到 CAS（`cas://<hash>` 引用，`materialize_block_with` 在 resume 时还原）。

实测：21 checkpoint 长会话从 ~230MB 降至 ~3MB（~99% 压缩）。CLI 提供 `wyj-code storage {status,doctor,prune}` 子命令做占用诊断与 GC。

---

## 权限模型

| 模式 | `permission_mode` | 行为 |
|---|---|---|
| **Normal**（TUI 默认）| `Prompt` | 写/命令/计算机控制类工具每次弹确认；`AllowAlways` 写入项目级 `~/.wyj-code/projects/<project_key>/allowed_tools.json`，跨会话生效 |
| **Bypass**（`--bypass-permissions`）| `AutoApprove` | 全部放行；**不**关闭 OS sandbox，不覆盖 protected deny |
| **Plan**（`--plan`）| `Allowlist` | 写类工具白名单限制，只能读/Plan |

`computer` 与 `app_computer` 是 `PROJECT_APPROVE_ONCE_TOOLS`：首次 `AllowOnce` 等价于当前工具的项目级 `AllowAlways`，用户只确认一次；但批准 `app_computer` 不会隐式放行风险更高的旧 `computer`。

**`project_key`** 按 git 仓库根派生（见 `core::project`），同一仓库不同 cwd 共享一份批准记录。

---

## SubAgent 编排

类型体系：

- **内置**：general-purpose / Explore（只读）/ Plan（`core/agent_def::builtin_defs`）
- **自定义**：定义文件六层合并链（同 skill 链）：
  内置 → `~/.wyj-code/agents` → `~/.claude/agents` → 插件贡献（先到先得）→ `<git-root>/.wyj-code/agents` → `<git-root>/.claude/agents`
- frontmatter：`name` / `description` / `tools` / `model`（引用 Profile 名），同名后者覆盖。

**`tools::SubAgentTool`**（工具名 `Agent`，参数 `subagent_type`/`description`/`prompt`/`run_in_background`）：

- 把每个子 Agent 整体 `tokio::spawn` 并登记进 `tools::agent_hub::SubAgentHub`（进程级单例：id 分配、`Semaphore` 并发上限 8、`abort_foreground`/`abort_all`/`wait_background`）。
- 子 Agent 挂 `with_tool_callback`/`with_usage_callback` 把内部工具事件与 token 用量以 `SubAgentEvent` 汇入 Hub 的 event_cb。
- 前台调用 `await` oneshot 结果；`run_in_background: true` 立即返回，完成结果包成 system-reminder 经注入通道（主 Agent 忙）或 `AppState.pending_bg_reminders`（空闲，下轮起手 merge）送达。
- TUI 展示：每个 agent 的 ToolCall 行绑定 `sub_agent_id`（Started 事件 FIFO 配对），运行期间画动态 ⎿ 状态行（耗时/tokens/工具数/当前工具）；完整内部工具调用与最终结果通过 SubAgent 独立详情面板查看。
- ESC 中断只 abort 前台子 Agent，后台不受影响；TUI 退出时 `abort_all`，headless `-p` 结束前 `wait_background`。

**模型解析优先级**：`def.model`（Profile 名）→ `[subagent].explore_profile`（仅 Explore）→ `[subagent].default_profile` → 主 Agent 当前分组（`cli::make_sub_agent_factory`）。

**白名单**：子 Agent 一律不注册 Agent / AskQuestion / ExitPlanMode / TodoWrite / Computer / AppComputer / Jev，并继承 Plan 白名单交集。

**轨迹持久化**（v1.2）：`SubAgentHub::emit()` 接入专职后台写手 `tools::trace::TraceWriter`（内部 mpsc 串行 append，调用方零阻塞），把 `SubAgentEvent` 转成落盘用 `TraceEvent`（`ToolStart`/`ToolEnd` 补全完整 input/output JSON，超 `trace_max_bytes_per_agent` 默认 256KB 静默停写）写入 JSONL：`~/.wyj-code/sessions/<session_id>.subagents/a<id>.jsonl`。CLI 子命令 `wyj-code subagent-trace <session_id> [<sub_id>] [--json]` 纯读打印落盘内容。

---

## 扩展点

### MCP

`mcp::bridge` 连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。配置来源：

- 全局 `~/.wyj-code/config.toml` 的 `[[mcp_servers]]`
- 项目级 `<git-root>/.wyj-code/mcp.toml` 与 `<cwd>/.mcp.json`

**项目级 MCP 信任确认**（v1.4，`store::project_trust`）：项目级 server 计算指纹（按 name 排序后规范序列化再 sha256），未信任时静默排除；批准记录必须落在仓库控制不到的位置 `~/.wyj-code/projects/<project_key>/mcp_trust.json`。CLI `wyj-code trust-mcp [--cwd <dir>]` 做交互式批准；TUI 启动检测到 Pending 时渲染 `BottomPanel::ProjectTrust`；headless/cron 无 UI 通道时一律跳过未信任 server 并打印一次提示（避免 stdin 阻塞）。

### Skill / 自定义命令

六层合并链：内置 → `~/.wyj-code/skills` → `~/.claude/commands` → 插件贡献 → `<git-root>/.wyj-code/skills` → `<git-root>/.claude/commands`。

- 支持单文件 `name.md` 与目录式 `name/SKILL.md`。
- frontmatter：`description` / `argument-hint` / `allowed-tools` / `model`（复用 `core::frontmatter::parse`）。
- `allowed-tools` 执行期通过 `CommandResult::RunPromptScoped` 把 `PermissionMode` 临时收紧为 `Allowlist`，跑完这一轮（含 ESC 中断）自动还原；TUI scoped execution 按 `model` 临时使用指定 Profile。

### Agent 定义

六层合并链同 Skill。frontmatter 字段同 Skill。

### Hooks 生命周期自动化

对齐 Claude Code 的 `.claude/settings.json` hooks。三源合并：`~/.claude/settings.json` → `<git-root>/.claude/settings.json` → `<git-root>/.claude/settings.local.json`（后者追加不覆盖）。

支持 4 个事件：

| 事件 | 时机 | 可表达 |
|---|---|---|
| `PreToolUse` | 工具执行前 | block / approve |
| `PostToolUse` | 工具执行后 | 追加反馈 |
| `UserPromptSubmit` | 用户提交后进入推理前 | block / 追加上下文 |
| `Stop` | 回合结束前 | 继续下一轮 |

每个 hook 是执行任意 shell 的 `command`，stdin 注入 JSON payload（含 `session_id`/`cwd`/`event`/`tool_name`/`input`/`response`），exit 2 表示 block（stderr 为原因），stdout JSON 可表达更丰富的 `decision`/`reason`/`additionalContext`/`continue`。默认超时 60s，可用 `--no-hooks` 完全禁用。

实现：`core::hooks::HookRunner` 加载/合并/执行；`core::agent::Agent` 在 4 个精确插入点调用；`cli/main.rs` 构造并装配给主 Agent 与 TUI 重建路径，子 Agent 工厂不装配。

---

## TUI / Headless / ACP / Daemon 入口

| 入口 | 启动方式 | 关键能力 |
|---|---|---|
| **TUI**（默认）| `wyj-code` | ratatui 渲染、流式 Markdown、权限对话框、SubAgent 面板、Profile/MCP/Skill/Settings 等设置面板 |
| **Headless REPL** | `wyj-code --headless` | stdin/stdout REPL，无 TUI 渲染，权限默认拒绝副作用操作 |
| **单次问答** | `wyj-code -p "..."` | 一次性 prompt 跑完即退，常被脚本/cron 调用 |
| **ACP adapter** | `wyj-code acp` | stdio adapter，连接结束时清理所属 session |
| **全局 daemon** | `wyj-code daemon --listen 127.0.0.1:61337` | TCP 连接之间共享进程级 session map，断线不终止 session；扩展 `_wyj/session/list` / `_wyj/session/control` 使用 schema version 2，覆盖 text/thinking/tool/usage/error/turn finished 以及 PermissionRequested、DiffAvailable、CheckpointChanged、AgentStateChanged 事件 |

TUI 永久运行在 `Viewport::Fullscreen`，输入框/状态栏贴住窗口底部、全部历史应用内滚动。

---

## Computer-use 与 Schedule

### Computer-use（v1.3.0+，仅 macOS/Windows 编译）

- 系统层 `wyj-computer`：`xcap` 截图 + `enigo` 输入合成 + 平台无关的 `scale` 模块。
- 工具层 `tools::computer::ComputerTool`：双模式由 `new(max_dim, native: bool)` 的 `native` 参数决定。
  - `native=true`：声明为 Anthropic 原生 `computer_20251124` 工具，需官方端点。
  - `native=false`：声明为普通 custom 工具，`description`/`input_schema` 动态嵌入实际下采样分辨率，用于第三方兼容端点。
- 注册门控：平台 `#[cfg]` + `Profile.vision` + `provider == Anthropic` + 端点官方判定，缺一不注册。
- 物理像素 vs 逻辑像素：macOS Retina 上 `xcap::Monitor` 返回"点"（逻辑），实际截到的是像素；裁剪 / 截图链路必须按"点→像素"换算（`scale::scale_region_to_pixels`），但点击坐标统一用"点"（`enigo` 坐标系）。
- v1.4 人机互不干扰架构：默认后台目标化执行（`tools::app_computer`），全局前台输入降级为显式兼容模式 `[computer_use].foreground_fallback = disabled|ask|idle_only`，默认 `disabled`；headless/cron/子 Agent 一律禁止前台接管。

### Schedule（v1.4）

- 数据层 `store::schedule`：标准 5 段 cron 表达式，原子写到 `~/.wyj-code/schedule/tasks.json`。
- 同步层 `store::cron_sync::sync_crontab`：只替换 `crontab -l` 输出里 `# BEGIN/END wyj-code schedule` 标记的区块，标记外内容原样保留；首次同步前自动备份原始 crontab。
- 触发层：复用 `wyj-code -p` 入口（`tokio::process::Command` 子进程调用自身二进制），stdout/stderr 重定向到 `schedule/logs/<task-id>/<ts>.log`。
- computer-use 安全边界由工具自身承担：headless/cron 没有交互通道，旧 `computer` 变更动作无条件失败关闭。

---

## Storage caps

> 全部 `0` opt-out by `~/.wyj-code/config.toml`；非零值启用对应清理策略。

| 子系统 | 默认 cap | 配置字段 |
|---|---|---|
| Evolution（per project）| 100 MiB + 28–180 天 TTL | `evolution.retention_*` / `evolution.max_project_store_bytes` |
| Session checkpoints | 20 / session | `storage.checkpoints_per_session` |
| Memory v2 `.md` | 200 / kind | `storage.memory_v2_md_per_kind` |
| Memory v3 `records.json` | 5000 条（Superseded 永保留）| `storage.memory_v3_records_max` |
| Memory v3 `jobs.json` | 32 pending | `storage.memory_v3_jobs_max` |
| Memory v3 `rejected_history.json` | 500 条 | `storage.memory_v3_rejected_history_max` |
| Schedule 日志 | 50 文件 / task | `storage.schedule_logs_per_task` |
| Schedule `run.log` | 10 MiB × 3 rotations | `storage.schedule_run_log_*` |
| 插件 `.git` | 7 天间隔 `git gc` | `storage.plugin_gc_interval_days` |
| Workspace worktrees | 30 天 prune | `storage.workspace_worktree_max_age_days` |
| Sub-agent trace | 256 KiB / agent | `subagent.trace_max_bytes_per_agent` |
| 持久化前 `ContentBlock` 字节截断 | tool_result 20K+10K head+tail、thinking 8K、tool_use.input 64K | `persist_cap.*` |
| 顶层 `~/.wyj-code` warn | 5 GiB 启动一次性 warn | `storage.disk_usage_warn_bytes` |

容量清理不得自动删除 Active candidate 或 Active/Pinned Memory。

**实现位置**：Phase 1 retention/cap 在 `crates/core/src/{checkpoint,memory,memory_v3}.rs` + `crates/store/src/{cron_sync,plugin_install}.rs` + `crates/core/src/workspace.rs`；Phase 2 截断在 `crates/core/src/serialize.rs`（`SessionStore::save` + `CheckpointStore::create` 落盘前调 `truncate_session_for_persistence`）；Phase 3 disk_usage 提示在 `crates/core/src/disk_usage.rs`，CLI 启动路径调一次，进程内 `OnceLock` 保证单进程只 warn 一次。

---

## 进一步阅读

- [`CLAUDE.md`](../CLAUDE.md) — 完整架构与开发约束（喂给 AI 的版本，含"教训"注释与版本历史细节）
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — 贡献流程、crate 职责、提交约定
- [`CHANGELOG.md`](../CHANGELOG.md) — 版本变更记录
- [`doc/plan/`](./plan/) — 各版本设计规划
- [`doc/analysis/domestic-models-vs-claude-code.md`](./analysis/domestic-models-vs-claude-code.md) — 国产模型适配对比
- [项目主页](https://wangyooujin.github.io/wyj-code/) — 在线架构图、功能演示、版本记录
