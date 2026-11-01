# CLAUDE.md

本文件**每轮**注入 system prompt 的 stable 段。只放跨 crate 的硬约定。
触达 `crates/<x>/` 时才加载该目录的 `CLAUDE.md`（进历史，每目录每会话一次）。
单文件超过 64KB 会被截断，尾部规则对模型不可见。长决策在 `doc/plan/`，
配置全量在 `docs/reference/config.md`，都不要写回这里。

| 文件 | 何时加载 |
|---|---|
| `crates/core/CLAUDE.md` | 触达 `crates/core/`：循环、压缩、记忆、存储、提示词、轮数与验收 |
| `crates/api/CLAUDE.md` | 触达 `crates/api/`：vendor 推断、缓存分段、thinking 回传 |
| `crates/tools/CLAUDE.md` | 触达 `crates/tools/`：搜索、子 Agent、桌面控制 |
| `crates/tui/CLAUDE.md` | 触达 `crates/tui/`：渲染、配色、主循环 |
| `crates/store/CLAUDE.md` | 触达 `crates/store/`：插件与项目级信任门控 |
| `crates/cli/CLAUDE.md` | 触达 `crates/cli/`：启动装配、review |
| `crates/mcp/CLAUDE.md` | 触达 `crates/mcp/`：MCP 桥 |

## 导航

读代码优先用 MCP `codegraph`：`codegraph_explore` 一次返回按文件分组的源码，返回过的文件不要再 Read。只知符号名用 `codegraph_search`；同名重载用 `codegraph_node` 的 `file`/`line` 消歧。改签名前用 `codegraph_callers` / `callees` / `impact`。`Grep`/`Glob` 只用于未索引文件、字符串字面量和文档。MCP 未连接时用内置 `CodeSearch`（`crates/core/src/code_index.rs`）。索引与磁盘冲突时以 Read 为准。

## 构建与提交

- `cargo test` / `cargo fmt`（`max_width = 100`）/ `cargo clippy`。指定 crate：`cargo test -p wyj-core`。
- **不要**把 `[profile.dev]` / `[profile.test]` 的 `incremental` 改回 `true`。增量编译留下永不回收的 `.o`，本仓库曾把 `target/` 撑到数百 GB，无变更 build 也变成分钟级。需要 lldb 看变量时临时把 `debug` 改回 `1`。
- 改了 prompt、工具描述、工具可见性、压缩或验收门禁，`cargo test` 覆盖不到效果。发布前用 `benchmarks/verify-tasks.sh`，再 `benchmarks/run.sh --repeat 3`。一次只改一个组件，每组至少 3 次，至少两个模型。`gate_blocks` 才是门禁真的跑过的证据。
- commit 用简体中文，首行 `type: 简述`。**不要**加 `Co-Authored-By: Claude` 或任何指向 Claude / Anthropic 的 trailer。只在用户要求时 `git add -A` → commit → push。
- 日常命令看 `cargo run -- --help`。worktree 启动必须带 `--project-root <主仓库>`：worktree 的 `.git` 是文件，不带这个参数会被当成另一个项目，授权、记忆、会话全部重开。

## 多实例

worktree 只隔离文件，不隔离 `~/.wyj-code/`。同一 checkout 两个进程会告警（判据是同 cwd，不是同项目）。共享 JSON 用 `config::filelock` + `serialize::write_atomic`。代码索引的增量写和 evolution 的写还要按磁盘合并，只加锁挡不住旧快照整份覆盖。两个进程不要 resume 同一会话。`git worktree add` 不带 ignored 文件；Rust 可用 `CARGO_TARGET_DIR=<主仓库>/target`。

## 斜杠命令

- 每新增一条静态 `/xxx`，同步改 `crates/i18n/locales/{en,zh}.yml` 的 `help.body`（`HelpCmd` 读的是模板，不是注册表）。文案走 `tr()`。调试命令标「内部/调试」，不要直接省略。
- Skill 是运行时发现的动态命令（`is_dynamic()`），不写进 `help.body`。加载链：内置 → `~/.wyj-code/skills` → 已启用插件 → `<git-root>/.wyj-code/skills`。不读 `~/.claude/`。Skill **不是**模型可调用的工具；名单经 `Agent::with_skills` 注入 `# Skills`。
- `Command::runs_while_busy` **默认 false，必须保持 false**。忙碌时 Enter 会把字面命令排进模型。只有面板类命令返回 true。`/cost` `/context` 不在白名单：忙碌时拿不到真实统计。拿不到锁时降级构造，不要跳过执行。钉子：`app::busy_slash_tests`、`commands::registry::busy_whitelist_tests`。
- `/rules` 预览项目规约的结构裁剪，`/rules apply` 才写盘。CLI 是 `wyj-code rules`（`--yes` 写盘）。不要用模型改写规约正文。
- `/profile` 改分组并写 `config.toml`。`/model` 只改当前模式实际用的模型字段，**仅内存**。`/effort` 随会话落盘，不写 config。优先级 env > 会话 > profile。`/effort off` = 清空 effort 且 `thinking_switch = "disabled"`，不动 `thinking_budget`。档位必须落在 `ThinkingSpec::effort_levels` 里。`vendor` / `wire_protocol` 已从 profile 删除，由 `infer_vendor` 与 `provider` 决定。

## 配置

`~/.wyj-code/config.toml`，Key 优先 `WYJ_CODE_API_KEY`。字段见 `docs/reference/config.md`。

- **`context_window` 必须是端点真实值。** 它是裸 `u32`，运行时不按模型名改小。填大了，清理和压缩都够不着，工具输出会堆到撞 400。唯一收紧来源是端点报超限后记下的上界。
- 第三方 Anthropic 兼容端点默认关 prompt cache。打开时只发 `cache_control`，不发 caching beta 头。
- 不在 `-h` 里、只覆盖本次运行：`WYJ_CODE_API_KEY`、`WYJ_CODE_THINKING_EFFORT` / `_SWITCH`、`WYJ_CODE_MAX_TOOLS_PER_TURN`、`WYJ_CODE_SEARCH_API_KEY`、`WYJ_CODE_PROBE_API_KEY`、`WYJ_CODE_MCP_CALL_SECS` / `_STREAM_IDLE_SECS` / `_TURN_STALL_SECS`、`WYJ_CODE_NOTIFY_OFF` / `_BELL` / `_DESKTOP`。

## 跨 crate 硬约定

- 模型侧提示词是英文常量（`core::prompts`、`tools::descriptions`），不走 i18n。用户可见 UI 才走 `tr()`。
- 缓存前缀顺序是 `tools → system → messages`。tools 数组会话内只增不减；system 里任何字节变化都会让整段历史缓存失效。每轮会变的内容进历史，不要进 system。改提示词后核对这条。
- 权限三档：`Auto` / `Plan` / `Bypass`（`AutoApprove`）。`manual`/`default`/`normal` 仍解析为 Auto。`hard_deny` 挂在 `PermissionPolicy::evaluate`，在本轮授权短路**之前**，不读授权状态。授权只能免去询问，不能免去禁止。Bypass 仍拦 hard_deny 和受保护路径。**新的 shell 执行工具（含 MCP shell 桥）必须接上 `hard_deny_denial()`。**
- 确定性 hard_deny（`curl|bash`、`rm -rf ~`、force push、`git reset --hard`、`sudo`、凭据路径等）排在「分类器是否可用」之前。关掉分类器不能让这些命令只剩一个弹窗。
- 凭据目录（`.ssh` `.aws` `.gnupg` `.kube` `.docker`）的保护要覆盖搜索，不只 Read：`check_search_root()` 加遍历过滤。判定入口只有 `is_protected_dir_component()`。`CodeSearch` 的单文件增量索引再挡一次。Bash 按路径组件判定，不要要求尾斜杠。
- 需要弹窗的工具不能 `parallel_safe`。`needs_dialog` 必须和 `confirm_tool` 的放行分支对齐，否则会同时弹出多个框。子 Agent 没有 `ui_ask_tx`，拒绝理由不能写成「用户拒绝了」。`computer` / `app_computer` 的首次 AllowOnce 等于该工具的项目级 AllowAlways，批准一个不放行另一个。
- 本轮已放行：Bash 按工具名，其余按输入指纹。
