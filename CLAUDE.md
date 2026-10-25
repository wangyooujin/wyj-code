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
cargo run -- run /explain <目标>  # 一次性执行一条斜杠命令 / Skill（不读 stdin，跑完即退出）
cargo run -- --config-status     # 查看当前配置和 API Key 状态（附降耗提示：超大 context_window / 低 max_tools_per_turn 告警）
cargo run -- --cwd <dir>         # 指定工作目录（默认当前目录）
cargo run -- --project-root <dir> # 显式指定项目根（无 Git 项目 / worktree 场景必带）
cargo run -- --plan              # Plan 模式：只读分析 + 受限规划文档写入
cargo run -- --bypass-permissions # 以 Bypass 模式启动（跳过权限确认）
cargo run -- --permission-mode bypass # 显式选模式：auto（默认）/ plan / bypass；manual/default/normal 是 auto 的兼容别名
cargo run -- --allowed-tools "Read,Grep" # 本次进程允许调用的工具名白名单（逗号分隔，仅工具名不授权写入范围）
cargo run -- --allow-plan-write docs/plan.md # Plan 模式额外放行的文档路径（可重复）
cargo run -- --profile <name>    # 一次性指定调用分组（不落盘、不改 active_profile；在子命令分发前生效）
cargo run -- --plugin-dir <dir>  # 加载本地插件目录用于开发调试（仅运行期入口有效，静态子命令会 exit(2)）
cargo run -- --no-hooks           # 禁用 Hooks 自动化系统
cargo run -- -c / --continue     # 恢复上一次会话
cargo run -- --resume <id>       # 恢复指定会话 ID
cargo run -- subagent-trace <session_id> [<sub_id>] [--json]  # 查看落盘的子 Agent 执行轨迹（无 sub_id 列出概览）
cargo run -- usage [--since YYYY-MM-DD] [--project PATH] [--json]  # 跨会话 token 消耗汇总（消耗率基线来源）
cargo run -- update [-y]         # 检查并自更新到 GitHub 最新 release（-y 跳过确认）
cargo run -- model doctor [profile] [--probe basic|full] # 模型身份/能力诊断；--probe 只认 WYJ_CODE_PROBE_API_KEY，不用 profile key
cargo run -- session checkpoint <id> [--name N] / checkpoints <id> # 会话 checkpoint 管理与列出
cargo run -- session rewind <id> <cp> [--scope conversation|files|both] [--force] # 回退（默认 both）
cargo run -- session branch <id> <cp> [--restore-files] [--force] # 从 checkpoint 开分支
cargo run -- extensions list|install|upgrade|enable|disable|remove|doctor # Skill/MCP/Plugin 资源管理（--scope project|global）
cargo run -- evolve status|list|review|feedback|approve|reject|... # 本地证据驱动的演化治理
cargo run -- memory status|search|search-sessions|write|forget|clear-all # Memory v3 claims 检查/纠正（跟随 storage.scope）
cargo run -- skillhub search <q> / info <slug> --namespace N / install <slug> --namespace N [--project] [--yes]
cargo run -- export backup.wyj       # 导出全部 profile + 全局 skill（含明文 api_key，落盘 0600）
cargo run -- import backup.wyj       # 恢复，默认 dry-run（--dry-run false 或 --yes 实际写入；profile 按 name 合并、skill 已存在则跳过）
cargo run -- storage status|doctor|prune|migrate # 本机 ~/.wyj-code 存储治理（prune/migrate 默认 dry-run）
cargo run -- trust / trust-mcp / trust-skills / trust-hooks # 项目级信任批准（trust = 三者全批）
cargo run -- workspace list      # 列出 managed Git worktree
cargo run -- workflow validate workflow.json # 校验 Workflow DAG
cargo run -- workflow run workflow.json      # 运行 DAG；写节点自动隔离
cargo run -- acp                 # stdin/stdout ACP adapter
cargo run -- daemon --listen 127.0.0.1:61337 # 全局 session daemon
cargo run -- review run --base HEAD^ --head HEAD [--json] [--fail-on <level>] # 本地 diff 审查证据

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
| **同一 checkout 起了两个实例** | `core::instance` 工作目录注册表（`~/.wyj-code/projects/<key>/instances/<pid>-<uuid>.json`，`InstanceGuard` drop 时删除）。启动时若同 cwd 还有存活实例，打印 `main.instances_share_cwd` 告警并给出 `workspace create` 的出路；`/rewind` 的 `RewindPreview.shared_peers` 非空时**强制确认**——`restore_files` 走 `git restore --worktree` 直接覆盖文件，对方没有 checkpoint 可退。判据是**同 cwd**，不是同项目：worktree 里文件物理隔离，不该报冲突 |
| 共享状态文件互相覆盖 | `config::filelock` 跨进程锁（锁住在最底层 crate，因为 `Config::save` 自己就是 load→改→save）+ `serialize::write_atomic` 原子替换（会话 / 授权名单 / memory-v3 全局层 / evolution / 代码索引） |
| 两个进程 resume 同一会话 | 启动时检出占用并告警（`SessionStore::active_owner`）。锁只能防写坏，防不了「整文件覆盖」语义上的丢更新，所以并行必须换会话 |

**索引与 evolution 的写不能只加锁，还要合并**：两者都是「读整份 JSON → 改 → 整份写回」的单文件状态。只加跨进程锁只能保证不写坏，挡不住 A 拿着旧快照把 B 刚写进去的条目整份覆盖。`ProjectCodeIndex::persist` 的增量路径（`invalidate`）因此额外做**按 mtime/len 取新**的磁盘合并，全量重建则不合并（重建结果是权威快照）；`invalidate` 原来还是裸 `fs::write`，半截 JSON 会让另一个实例静默全量重建。`EvolutionStore::write_critical` 是进程内 Mutex + 跨进程锁双层，跨进程侧**分段重试 500ms 后降级**而非 `acquire` 阻塞 30 秒——它的写点有从 TUI 主流程直接调的。

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

**改了 harness 要跑评测基线，`cargo test` 覆盖不到。** prompt / 工具描述 / 工具可见性 /
压缩清理策略 / 验收门禁这些改动的效果，不在任何单测的断言里：

```bash
cargo build --release
benchmarks/verify-tasks.sh          # 任务集自检，零 API 费用
benchmarks/run.sh --repeat 3        # 20 个任务 × 3 次
benchmarks/compare.sh <baseline> <candidate>
```

纪律三条：**一次只改一个组件**、**每组 ≥3 次重复**、**至少 2 个模型对照**。详见
`benchmarks/README.md`。理由：外部受控实验里 harness 的方差可达模型方差的 7.8 倍，且
**很多 harness 改动是负收益**（verifier 在某些基准 −0.8pp、多候选搜索 −5.6pp）——没有基线
就只能靠感觉判断。`gate_blocks` 是门禁**实际拦截**次数，比「配置里写了 `[acceptance]`」
更能证明它真的跑了。

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
- **回合进行中（`is_thinking`）的 slash 命令**：忙碌时按 Enter 默认把内容排进 `pending_queue` 投递给模型，而 `/model` 这类命令**没有内容可投递**——排给模型等于把字面的 `/model` 当成一句用户消息发出去，右下角同时冒出"1 条已排队"（用户现场）。因此 TUI 在 Enter 分支先问 `Command::runs_while_busy`：为 true 的**面板类**命令（`/model` `/profile` `/effort` `/mcp` `/config` `/skills` `/plugins` `/agents` `/memory` `/sessions` `/help` `/mode` `/cwd` `/doctor` `/hooks` `/shells` `/subagents` `/computer` `/agent-control` `/extensions` `/quit`）照常 dispatch，其余维持排队。**该方法默认 false，且必须保持默认 false**，四条排除理由见其文档注释（起新回合 / 改会话历史 / 抢 `current_task` 句柄导致回合失去 ESC 中断 / 读 session 统计）。`/cost` `/context` 刻意不在白名单：忙碌时 session 被回合整段持锁，只能拿到降级 ctx（统计字段全 0），与其给一份静默失真的数字，不如让用户等回合结束。CommandContext 拿不到锁时**降级构造而不是跳过执行**——跳过等于用户按 `/model` 只看到一句"会话忙"。钉子：`app::busy_slash_tests`（白名单判定 + 主循环分支顺序）、`commands::registry::busy_whitelist_tests`（白名单**精确集合**断言，多一条少一条都红）。
- **三个模型相关命令的职责边界与作用域**（v0.3.16 拆分，此前 `/model` 一个命令背了三件事）：

  | 命令 | 职责 | 作用域 | 落盘 |
  |---|---|---|---|
  | `/profile` | 分组管理（= CLI `--profile <name>` 对应的那套端点凭据） | 全局默认 | 写 `config.toml` |
  | `/model` | 切换当前分组下的**模型** | **仅内存** | 不写 config、不落会话文件 |
  | `/effort` | 思考强度档位 | **随会话** | 落会话文件，不写 config |

  - `/model` 的覆盖**只改当前模式实际读的那个字段**（auto/bypass → `exec_model`，plan → `plan_model`，为空时回退 `model`），由 `Config::model_field_for_mode` 判定——判定规则与 `model_for_mode` **严格一致**。因此 plan 模式挑模型不会顺手改掉 auto 模式用的那个。
  - `/model` **不落盘**的理由：它表达的是「为手头这个任务临时换个模型」，写进全局配置会污染其它项目，写进会话文件则让「当前在跑哪个模型」在恢复后不可解释（聊天记录里明明是另一个模型的产出）。会话级**分组**反而是落盘的（`Session.session_profile`）——「这个会话跑在哪套端点上」是会话的属性。
  - `/effort` 档位**随会话落盘**（`Session.session_effort`）：思考档位是「这次对话要想多深」的工作状态，不该每次 `--resume` 都要重设。生效优先级是 **env > 会话级 > profile 字段**（`Config::effective_reasoning_effort`），env 最高是因为 `WYJ_CODE_THINKING_EFFORT` 本意就是「这次运行一律用这个档位」，任何面板操作都不该盖掉它。
  - `/effort off` 的语义是**彻底关思考**：`reasoning_effort` 清空 + `thinking_switch = "disabled"`。`thinking_budget` 不动——官方 Anthropic 上 effort 与 budget 控制不同维度，可以共存。
  - 档位列表按**当前模型的能力表**过滤（`wyj_api::supported_effort_levels` → `ThinkingSpec::effort_levels`）：Claude 五档、DeepSeek 四档、MiniMax M3 只三档、GLM 根本不认 effort。列出端点不认的档位只会被静默丢弃，用户还以为设上了。
  - `Profile.vendor` / `Profile.wire_protocol` **已移除**：vendor 由 `base_url` + 模型名**推断**（`wyj_api::models::infer_vendor`，全仓唯一一份），线协议由 `provider` 唯一决定。旧 config.toml 里那两行**静默忽略**（无 `deny_unknown_fields`），下次 `config.save()` 时随 `profiles` 数组整体替换而消失。
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
`[computer_use]` / `[tools]` / `[search]` 等块的完整字段与默认值见 `docs/reference/config.md`。

**环境变量覆盖族**（都不出现在 `-h`，只覆盖本次运行、不落盘）：

| 变量 | 作用 |
|---|---|
| `WYJ_CODE_API_KEY` | 覆盖激活分组 API Key（优先级高于 config.toml） |
| `WYJ_CODE_THINKING_EFFORT` / `WYJ_CODE_THINKING_SWITCH` | 覆盖思考档位/开关（优先级最高，面板操作不盖掉它） |
| `WYJ_CODE_MAX_TOOLS_PER_TURN` | 覆盖每轮工具数上限 |
| `WYJ_CODE_SEARCH_API_KEY` | WebSearch 后端（Tavily/Brave）密钥 |
| `WYJ_CODE_PROBE_API_KEY` | `model doctor --probe` 专用轮换密钥（刻意不用 profile key） |
| `WYJ_CODE_MCP_CALL_SECS` / `WYJ_CODE_STREAM_IDLE_SECS` / `WYJ_CODE_TURN_STALL_SECS` | MCP 调用 / 流空闲 / 回合停滞超时 |
| `WYJ_CODE_NOTIFY_OFF` / `_BELL` / `_DESKTOP` | 通知通道开关 |

## Architecture

这是一个 Rust workspace，单一 `wyj-code` 二进制，零遥测。各 crate 职责：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session runtime/events、HistoryStore、MemoryStore、权限、checkpoint、workspace/workflow 接口与本地 CodeIndex |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/BashOutput/KillShell/Glob/Grep/WebFetch/WebSearch/TodoWrite/AskQuestion/ExitPlanMode/SubAgent/Computer；WebSearch 仅在 `[search]` 配置可用（api_key 存在且 provider 已知）时注册，后端可选 Tavily / Brave，Computer 仅 macOS/Windows 编译且需 vision+Anthropic profile；descriptions.rs 英文工具描述、textutil.rs 安全截断、bash_session.rs 后台任务单例）|
| `crates/computer` | `wyj-computer` | computer-use 系统层：`xcap` 截图 + `enigo` 输入合成（两者内部已各自分派 macOS/Windows，本 crate 不再手写 target_os 分支），坐标缩放数学（`scale` 模块，平台无关可测）；仅 `[target.'cfg(any(macos, windows))']` 拉取真实依赖，其余平台编译进桩实现 |
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`rust-i18n` 封装，`en`/`zh` 内嵌 YAML）与运行时语言切换（`tr()`/`set_locale()`）|
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/store` | `wyj-store` | MCP/Skill/Plugin 配置与安装数据层；`plugin_runtime` 事务式激活 hooks/styles/themes/channels/LSP/monitors/settings/userConfig，持久 LSP client 提供 `workspace/symbol`；lockfile 与 marketplace 同样在此。v0.2.24 起移除 `import` 子模块（/import 命令下线）与 `wyj-config::codex` 兼容层（Codex 导入能力下线） |
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
| 32 | 验收门禁：模型说「完成」不算完成，框架判完才算 | `core::acceptance` + `core::agent` 收尾门禁 | `crates/core` + `crates/cli` |

**跨 crate 改动的两条硬约定**：

- **模型侧提示词是英文常量，不走 i18n**（`core::prompts` + `tools::descriptions`）。模型行为不应随
  locale 漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复。i18n 只覆盖用户可见
  的 UI 文案。
- **改提示词就要重测缓存相关的不变式**：system prompt 落在 stable 段且断点在段末，
  任何每轮变动的内容混进 stable 都会让整段前缀缓存失效。

### 权限模型（TUI）

**三种模式（`AgentMode`：Auto / Plan / Bypass，`--permission-mode` 或 Shift+Tab 在这三档间循环）**。`manual` / `default` / `normal` 仍能解析但一律落到 `Auto`——手动模式已并入 auto，作为**向后兼容别名**保留，让旧配置与脚本不至于静默失效（`AgentMode::parse` 的测试钉死「必须仍返回 `Some`」）。`ToolCtx.permission_mode`（`PermissionMode::{Auto, AutoApprove, Allowlist, Plan}`）控制 `is_allowed`，而「要不要问人」在 `agent.rs::ensure_tool_permission` 里决定，它返回 `Result<(), String>`——错误里带**具体**原因，因为「用户点了 d」与「auto 分类器判定越界」对模型而言是两种不同的后续行为。

**Auto 是唯一的日常判定模式**：判定交给 `core::auto_mode::AutoGate`，固定顺序 = 受保护路径 deny（任何模式都不放行）→ 确定性 allow（工作目录内文件写、只读/构建/lockfile 安装的 shell，零延迟）→ 确定性 hard_deny（`curl|bash`、`rm -rf ~`、force push、`git reset --hard`、`terraform destroy`、`sudo <program>`、凭据路径、无审批 agent 循环，不发模型请求）→ 模型分类器（其余才付一次模型往返，system prompt 见 `prompts::AUTO_CLASSIFIER`，规则文本见 `core::auto_rules`）→ 交回逐次弹窗 → **降级**（连续 3 次或累计 20 次被拦自动 `disable_classifier()`，只剩确定性规则 + 弹窗；`/mode auto` 显式选一次可恢复）。**两条确定性规则必须排在「分类器是否可用」之前**——否则 `[auto_mode] enabled = false` 时 `curl|bash` 只剩一个弹窗，按个 `y` 就过去了（测试 `disabling_the_classifier_never_drops_the_hard_deny_layer` 钉死）。分类器超时/非法输出/空响应先加格式提醒重试一次，两次都失败才交回人工，且不计入降级预算。闸门挂在 `ToolContext`（`auto_gate()`）而非 `Agent` 上：子 Agent 的 ctx 由父 ctx 派生，闸门自动传递、计数全局共享；`PermissionPolicy::evaluate` 保持同步，只管「任何模式都不放行」那部分（真实判定需要 await 模型）。**Auto 不要求交互面**，因此 `-p`/headless/定时任务同样可用；无交互面时闸门判「交回人工」会落到 `ToolCtx::confirm_tool` 的 fail-closed 分支（删掉 `PermissionMode::Prompt` 之后，这是唯一保证「无审批通道就不放行」的关卡）。**「本轮已放行」的记忆粒度按工具分叉：Bash 按工具名，其余按输入指纹**——Bash 批准一次本轮不再问（长任务里几十条同类命令不该各付一次往返），而写工具/`Agent`/computer-use 必须逐次记（`Edit` 批过 `notes.md` 不该顺带放行 `deploy.sh`；用户在为一个文件点头不等于同意改另一个）。

**⚠️ `hard_deny` 的挂载点是 `PermissionPolicy::evaluate`，不是 AutoGate**（v0.3.13 修 P0 回归）。此前它只在 `AutoGate::review` 里，而 `Agent::ensure_tool_permission` 的 `turn_granted` 短路会整段跳过 review——Bash 的本轮授权键是**裸工具名**，于是同一回合里跑过任意一条 `git log` 之后，`git reset --hard` / `curl x | bash` / `git push --force` 全部零拦截执行。现在 `evaluate` 在模式分派**之前**先跑 `hard_deny_denial()`，由 `ToolCtx::is_allowed` 在 `ensure_tool_permission` 上游调用，**不读任何授权状态**：授权只能免除「问人」，不能免除「禁止」。唯一例外是 `AutoApprove`（Bypass，用户显式选择跳过全部询问）；受保护路径写仍然照拦。拒绝理由经 `ToolContext::denial_reason()` 回灌模型，措辞必须区分「换模式就行」与「这条命令永远不行」。钉子：`permission::tests::hard_deny_is_enforced_regardless_of_grants_and_mode`、`ctx::tests::hard_deny_survives_turn_grants_and_returns_a_specific_reason`。**新增 shell 执行类工具（含 MCP 提供的 shell 桥）必须在 `hard_deny_denial()` 里接上**，否则它就是绕过这一层的新口子。

**凭据保护必须覆盖「批量读取」，不只是 Read/Write**（v0.3.13 修 P0）。`check_read_target` 只挂在 `Read` 上，而 `Grep`/`Glob`/`CodeSearch` 遍历整棵树、显式 `--hidden`、`needs_permission` 走默认 `false`——一条 `Grep(pattern: "PRIVATE KEY", path: "~")` 就能把 `~/.ssh/id_rsa` 读进上下文并落盘。现在保护分两层，缺一不可：**根校验** `check_search_root()`（根本身落在受保护子树里直接拒绝，避免白跑一趟后把「未找到」当结论）+ **遍历过滤**（`WalkBuilder::filter_entry` / rg 的 `-g '!**/.ssh/**'`，挡住「根是家目录、凭据在下面」这一形态）。四条路径共用 `permission::PROTECTED_DIR_NAMES` 同一张表，`is_protected_dir_component()` 是唯一判定入口；`CodeSearch` 额外在 `index_file()` 里再挡一次，因为 `invalidate()` 走单文件增量重建、不经过遍历器，而索引是**持久化**的。Bash 侧的对应缺口由 `auto_rules::references_protected_dir()` 补上：`credential_paths` 原来的 needle 带尾斜杠（`.ssh/`），`cat ~/.ssh` / `ls ~/.ssh` 整片漏过，现改为按**路径组件**判定（`project/.awsome/x` 不误伤）。钉子：`grep::tests::protected_dirs_are_never_searched_by_either_backend`（rg 与 native 两条后端都要过）、`auto_rules::tests::credential_dirs_are_blocked_even_without_trailing_slash`。

**逐调用工具权限确认（v0.1.2 起，Auto 判 AskUser 时的落点）**：`ToolCtx::confirm_tool(name, summary)` 经 `ui_ask_tx` 发 `UiAskRequest::ToolPermission` 并 await `oneshot<PermissionDecision>`，TUI 弹 `PermissionDialog`（`draw_permission_dialog`），按键 y=AllowOnce / a=AllowAlways / d·Esc=Deny。`Deny` 把拒绝信息作为 `is_error` 工具结果回灌给模型；`AllowAlways` 把工具名写入项目级 `~/.wyj-code/projects/<project_key>/allowed_tools.json`（`project_key` 按 git 仓库根派生，见 `core::project`）并跨会话生效——`ToolCtx::load_allowed_tools` 在每轮 ctx 装配时载入。computer-use 是刻意收窄的例外语义：`PROJECT_APPROVE_ONCE_TOOLS = ["computer", "app_computer"]`，这两个工具的首次 `AllowOnce` 也等价于当前工具的项目级 `AllowAlways`，因此用户只确认一次，但批准 `app_computer` 不会隐式放行风险更高的旧 `computer`。`summary` 由 `Tool::action_summary()` 提供（Bash=命令、Edit/Write=文件路径）。需要确认的工具均非 `parallel_safe`，串行执行，同一时刻至多一个对话框；`ToolCtx::needs_dialog` 的判据必须与 `confirm_tool` 的两条「直接放行」分支严格对齐（Auto 也在其列——闸门判定要 await、AskUser 还要弹窗，同样会阻塞），否则 Agent 会把多调用排进并发组同时弹 N 个框。Bypass=`AutoApprove` 免询问，但**不**免 hard_deny 与受保护路径（`rm -rf ~`、force push、`curl|bash`、凭据路径仍拦）；Plan=`Plan(白名单)` + 只读 Bash 校验 + 规划文档路径策略。`ui_ask_tx` 同时承载 AskQuestion 多题面板与 ExitPlanMode 计划批准；子 Agent 的 ctx 不接 `ui_ask_tx`，`confirm_tool` 默认放行（不阻塞、不弹窗）。配置见 `docs/reference/config.md` 的 `[auto_mode]` 节。
