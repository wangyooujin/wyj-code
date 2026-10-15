# 配置参考

本文档是 `~/.wyj-code/config.toml` 的**带注释全量参考**。根目录 `CLAUDE.md` 只保留
日常最常改的几个键；其余全部搬到这里，按需查阅而不是每轮注入。

---

## 文件结构

配置文件：`~/.wyj-code/config.toml`，API Key 优先读取环境变量 `WYJ_CODE_API_KEY`。

```toml
provider = "anthropic"       # 或 "openai"
model = "claude-opus-4-8"
plan_model = ""              # Plan 模式专用模型，留空则使用 model
exec_model = ""              # Exec/Bypass 模式专用模型，留空则使用 model
base_url = ""                # 留空使用供应商默认端点
max_tokens = 8192
context_window = 200000
vision = true                # 模型是否支持图片输入；false 时图片降级为占位文本（防非多模态端点 400）
# prompt_cache = true         # prompt caching。**默认跟随端点类型**：官方 Anthropic 端点开，
                              # 第三方 Anthropic 兼容端点（MiniMax/GLM/Kimi…）默认关——它们实现了
                              # cache_control（命中约 0.1x 输入价），但对未知 anthropic-beta 头常直接 400，
                              # 故第三方开启时只发 cache_control、不发 caching beta 头。
                              # 若端点连 cache_control 也不认，首次 400 会自动降级（不中断会话），
                              # 本进程内停用缓存并在 /cost 里显示降级原因。
# openai_stream_options = false # OpenAI-compatible 第三方端点可显式关闭 stream_options.include_usage
# thinking_budget = 8000     # extended thinking 预算 token；不写/0 = 关闭（思考计入 output 计费）
# interleaved_thinking = true # 工具调用轮之间允许交错思考（budget 开启时生效）
log_level = "warn"           # 调试时设为 "debug"
language = ""                # "en"/"zh"，留空（或空串）自动检测系统 locale；决定 UI 文案 **与模型输出语言**（含 thinking）
search_provider = "tavily"   # WebSearch 搜索 provider（目前支持 tavily）
search_api_key = ""          # WebSearch API Key，优先读环境变量 WYJ_CODE_SEARCH_API_KEY；未配置则 WebSearch 工具不注册（模型看不到）

[subagent]
default_profile = ""         # 子 Agent 默认 Profile 名，留空沿用主 Agent 当前分组
explore_profile = ""         # 内置 Explore 类型专用 Profile 名（配便宜模型），留空回退 default_profile
trace_enabled = true         # 是否把子 Agent 完整执行轨迹落盘（供跨会话查看，见下方 SubAgent 节）
trace_max_bytes_per_agent = 262144  # 单个子 Agent trace 文件字节上限（默认 256KB），超限静默停写

[auto_mode]                     # Auto 权限模式（默认起始模式，详见下方 Auto 模式节）
enabled = true                # 模型分类器总开关。false = 只保留本地确定性规则层，
                                # 拿不准的操作一律逐次弹窗（保守模式）。
classifier_profile = ""       # 分类器用哪个调用分组。留空 = 复用当前激活分组；
                                # 填分组名 = 用它单独跑（适合配便宜的小模型降本）
timeout_secs = 30             # 单次分类器请求超时。超时按 fail-closed 判拒绝。
                                # 默认 30：adaptive thinking 端点光思考就要好几秒
max_consecutive_blocks = 3    # 连续被拦达到该次数 → 自动降级为逐次人工确认
max_total_blocks = 20         # 整个会话累计被拦达到该次数 → 同样降级
classify_all_shell = false    # true 时所有 shell 命令一律送审，不走本地 allow 白名单
                                # （更安全，但每条命令都多一次模型往返）

# 以下四组是**喂给分类器的英文自然语言规则**（不是正则）。数组里出现字面量
# "$defaults" 就在该位置插入内置规则，因此自定义项可写在默认项之前或之后，
# 并继续继承后续版本的内置规则更新。**不写 "$defaults" 会整段替换内置规则**
# —— 对 allow / environment 通常正是你想要的，对 soft_deny / hard_deny 则会
# 实打实放宽拦截面，启动日志会就此告警。
#
# hard_deny = ["$defaults", "绝不允许把仓库内容发往 example.com"]
# soft_deny = ["$defaults", "禁止在 migrations CLI 之外跑数据库迁移"]
# allow      = ["$defaults", "允许部署到 staging：它与生产隔离且每日重置"]
# environment= ["$defaults", "Source control: github.example.com/acme-corp"]

[agent]                          # Agent 推理循环参数（v0.3.4+）
max_turns = 200                 # 单次用户提交内允许的最大推理轮数（一次 LLM 往返 = 一轮）。
                                # **默认 200**，本意是「防模型死循环」，不是给正常长任务设工期。
                                # 距上限还剩 4 轮时会先注入收尾指令，要求模型停止调用工具、
                                # 立即总结当前进度（见架构节「推理轮数触顶不再等于失败」），
                                # 本值只决定那道宽限从第几轮开始。超长任务（如跨几十个文件的
                                # 迁移）可调高；调低它限制的是**单次提交**的最长耗时。
                                # 死循环保护由 loop_guard 独立承担，不受本值影响；
                                # reasoning 模型另有 32 轮熔断，也不受本值影响。

[tools]                          # 工具注册门控（v0.2.27+）
todo_enabled = true            # 是否注册 TodoWrite 任务列表工具。**默认开启**
                                # （v0.3.1 起由关闭改为开启，见架构节
                                # 「TodoWrite 默认开启」）。未注册 = 工具不产生
                                # schema，模型目录里看不到它。简单任务为主、
                                # 或用强模型跑短任务时设 false 更划算。

[context_edit]                   # tool-result context editing（v0.2.26+）：
                                # 超阈值时先把过期工具输出外部化到 CAS，
                                # **清理优先于整段摘要**（对齐 Claude Code 的
                                # "clears older tool outputs first, then summarizes
                                # the conversation if needed"）。
                                # 详见架构节「上下文压缩」与
                                # doc/plan/v0.2.26-context-management-plan.md
enabled = true               # false = 完全退回 v0.2.25 行为（只压缩、不清理）
protect_recent = 3           # 最近 N 个工具结果保留全文（Anthropic keep=3）
batch_size = 10              # 单批清理条数（每批都会击穿 prompt cache 前缀）
min_result_bytes = 2000      # 小于此值不清理（省下的不够占位符本身）
max_batches = 3              # 单请求最多连续清理几批
soft_limit_ratio = 0.55       # **软阈值**：上下文占用超过窗口的这个比例就开始外部化，
                                # 而不是等撞上压缩硬阈值才动手。1M 窗口下硬阈值在 900K，
                                # 实际会话永远够不着 → context editing 等于从未运行。
                                # 0 = 关闭（完全退回旧行为）。软阈值只改「何时开始」，
                                # 不改「何时停手」：降到硬阈值以下才停。

[notify]                      # 统一通知通道（v0.2.23+）：TUI/CLI 回合完成 / 错误、
                              # 后台子 Agent 完成、cron schedule 失败统一派发到
                              # 终端响铃（stderr \x07）与桌面通知（macOS osascript /
                              # Linux notify-send / Windows PowerShell BurntToast）。
                              # 零新第三方依赖。详见 `wyj_core::notify` 模块文档。
enabled = true               # master 开关（opt-out 默认开）
rate_limit_seconds = 30      # 同类事件最小间隔去重；0 = 关闭

[notify.bell]
enabled = false              # 终端响铃 opt-in 默认关（TUI 共享场景避免扰民）

[notify.desktop]
enabled = true               # 桌面通知 opt-out 默认开（后台子 Agent / cron 失败场景必须通知）

[notify.events]
turn_finished = true         # CLI `-p` / REPL / TUI 回合正常完成
turn_error = true            # 主回合出错
subagent_done = true         # 后台子 Agent 完成（run_in_background: true）；前台调用不通知
schedule_failure = true      # cron schedule 任务失败

# include_session_id = false   # body 末尾追加 `[session:<id>]`，默认关闭（body 短）

# env override（init 阶段读取，绝不写回 cfg）：
#   WYJ_CODE_NOTIFY_OFF=1      全关（最高优先级）
#   WYJ_CODE_NOTIFY_BELL=0/1   覆盖 notify.bell.enabled
#   WYJ_CODE_NOTIFY_DESKTOP=0/1 覆盖 notify.desktop.enabled

[[mcp_servers]]
name = "my-server"
transport = "stdio"
command = "/path/to/server"
args = ["--flag"]
```


## 进阶主题

### Auto 模式（默认权限模式）

wyj-code 的**默认起始模式**是 `auto`，对齐 Claude Code v2.1.283+ 的内置起始模式。它与 `manual` 的差别只有一个位置：**谁来回答「这一步要不要问人」**。手动模式下是用户逐次点击；auto 模式下是一条固定顺序的流水线（`core::auto_mode::AutoGate`）：

1. **受保护路径写入**（`.git`、`.wyj-code`、`.env`、密钥文件）——任何模式都不放行，直接拒绝；
2. **确定性 allow**——工作目录内的文件写、只读 shell 命令、构建/测试命令、lockfile 依赖安装，本地判定，零延迟零成本；
3. **确定性 hard_deny**——`curl|bash`、`rm -rf ~`、`git push --force`、`git reset --hard`、`terraform destroy`、`sudo <program>`、凭据路径、启动无审批的 agent 循环等，本地判定，**不发模型请求**；
4. **模型分类器**——前两步都没结论时才付一次模型往返，让它按 `soft_deny` / `allow` / `environment` 规则判断「这一步是否在用户请求范围内」。优先级 `hard_deny → soft_deny → allow → 用户显式意图`。
5. **fail-closed**——分类器超时 / 返回非法内容 / 空响应，一律判拒绝。只有 `enabled = false`（用户主动选的保守模式）才交回人工弹窗。
6. **降级**——连续 3 次或累计 20 次被拦，自动退回逐次人工确认并提示；Shift+Tab 可随时切回。

四种模式用 `--permission-mode auto|manual|plan|bypass` 或 TUI 的 Shift+Tab 切换（`default` 是 `manual` 的别名）；`--plan` / `--bypass-permissions` 是前两者的便捷写法。**auto 不要求交互面**，因此 `-p` / headless / 定时任务同样可用——这正是它相对「逐次弹窗」唯一而决定性的优势。

`[auto_mode]` 的四个规则数组是**英文自然语言规则**（喂给分类器，不是正则），支持 `$defaults` 占位符原位插入内置规则。内置清单见 `core::auto_rules`。

两个容易踩的点：

- **分类器与主会话共用同一 Provider 时，它的 token 计在主会话的 `total_*` 里**。想彻底隔离，配一个独立的 `classifier_profile`（配便宜的小模型通常足够）。
- **「本轮已放行」的记忆粒度在两种模式下不同**：manual 按工具名（批准一次 Bash，本轮不再问），auto 按 `工具名 + 输入`（只免审**完全相同**的调用）。这是刻意的——否则审过一次 `npm test` 就会让本轮所有 `npm run deploy` 一起免审。

### 项目规约文件回退链

**项目规约文件记忆机制**（`core::claude_md::ClaudeMdLoader`）：查找范围为全局 `~/.wyj-code/` + 从项目根到 cwd 的祖先链（`config::project_root`，找不到 `.git`/`.wyj-code/project.toml` 则只用 cwd 本身）。**v0.2.31 起按槽位独立回退、命中第一个即停**：主槽位 `AGENTS.md` → `CLAUDE.md` → `GEMINI.md` → `AGENT.md`，个人覆盖槽 `AGENTS.local.md` → `CLAUDE.local.md`；两槽位都命中时 local 追加在后（视作个人覆盖增补，不提交 git）。**`AGENTS.md` 始终是首选**（复数，对齐 AGENTS.md 开放标准，Codex / Cursor / Gemini CLI 等均采用），回退只是为了让「根目录只有 CLAUDE.md」的既有仓库不至于对模型完全失明——这些文件都是项目作者写的规约，读不到等于整个项目的约定不可见。**两个槽位独立而非「整个家族二选一」**，是为了保住 personal override 语义：主文件走 `AGENTS.md` 而私有覆盖写在 `CLAUDE.local.md` 时仍然生效，不被静默丢弃；两槽位各只取一个文件，因此**永远不存在同一份规约被注入两遍**。判定用「有实际内容」而非单纯 `is_file()`：0 字节 / 全空白的主文件继续往下探。**历史沿革**：v0.2.27 曾硬切到只读 `AGENTS.md`（明确不保留回退，测试 `load_dir_files_ignores_claude_md_entirely` 锁死），v0.2.31 在保留「命中即停、绝不双份注入」这个核心不变式的前提下把回退链加了回来，该锁死测试已被替换为回退链系列测试。**全局路径仍只有 `~/.wyj-code/`，任何 `~/.claude/` 一律不读**（`config::claude_home_dir()` 已删除、替换为 `config::global_config_dir()`）；读的是**项目目录内**的 `CLAUDE.md` 文件，与读取外部工具的全局配置目录是两回事。支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。每轮对话开始时重新读盘，以 `<system-reminder>` 包装拼进当轮 **system prompt 末尾**（不注入 user 消息、不进历史，配合 prompt caching 避免跨轮累积；文件不变时字节级稳定、缓存可命中；注入文本里含具体路径与文件名，故增删/改名规约文件会自然失效该段缓存）。工具（Read/Edit/Write/Glob/Grep）触达新子目录时，若该目录有规约文件且本会话未展示过，会在 `agent.rs` 的工具执行循环里追加到 system 末尾（`ClaudeMdLoader::maybe_dir_reminder`，按目录去重）。`/init` 触发一次真正的 agent 回合（`CommandResult::RunPrompt`）去探索项目并生成/合并更新**项目规约文件**，而非静态模板写文件。**生成目标跟随仓库现状**：走 `claude_md::existing_memory_file_name`（与实际注入同一条 `resolve_slot` 解析线）取「当前真正会被读到的那一份」，只有当一个候选都不存在时才回退到规范名 `AGENTS.md` 创建新文件，并要求把其余不同名的规则文件一并整合进去。**这条不是洁癖**：本仓库自己就踩过——v0.2.31 的提交同时留下了 `AGENTS.md` 与内容逐字节相同的 `CLAUDE.md`，两份副本随后各自漂移；`git rm` 掉前者之后，若 `/init` 仍硬编码 `AGENTS.md`，下次跑一次就会把那份僵尸副本原样重建。「命中即停」只保证模型不双份注入，**管不住维护者写出两份不同步的文件**。回归测试 `init_target_follows_the_file_that_would_actually_be_read` 是这条不变式的钉子。`/memory` 打开 TUI 面板列出当前会话适用的全部文件（与注入共用同一个 `resolve_slot`，面板显示的就是实际会注入的那个文件，不存在「面板列了 A、实际读 B」的漂移；一个候选都不存在时才列出 `AGENTS.md` 作为 `exists=false` 的创建入口），选中后挂起 TUI 唤起 `$EDITOR` 编辑，同时暴露 auto-memory（跨会话记忆提取）开关与索引入口（`Config.auto_memory_enabled`）。不再兼容旧版 `WYJ.md`。**注意 i18n key 名仍沿用历史拼写 `claude_md_ok` / `claude_md_missing`（值已泛化为「项目规约 / Memory files」，不再硬编码某个文件名——回退生效时写死文件名会让 `doctor` 报出「未找到」这种与事实相反的结果），key 是代码契约，不要因为改名去动 i18n YAML 里的 key。**

### 编辑后自动验证（verify.command）

**项目级**、写在同一套 `.wyj-code/settings.json` 体系里的键（与 `hooks` 同源同合并顺序：全局 → 项目 → `settings.local.json`）：

```json
{
  "verify": {
    "command": "cargo test -p wyj-core",
    "timeout_secs": 120
  }
}
```

`verify.command` 让「编辑完就验证」成为闭环：每次 Edit / Write / MultiEdit 成功返回后，Agent 立即跑这条命令，把退出码和截断后的输出作为一条 tool result 回灌给模型，模型据此自己看到编译/测试错误并继续修——而不是把「我改完了，跑一下看看对不对」这件事推给用户。

三条设计约束：
- **零配置成本**：没有声明 `verify.command` 时，`verify.rs` 直接 early-return，**不探测 `npm`/`cargo` 是否存在、不启动任何 shell、不产生任何输出**。绝大多数项目不会配这个键，不配就没有任何代价。
- **执行顺序**：编辑工具成功 → `PostToolUse` hook → 项目 verify。hook 的反馈（格式化、lint 提示）先到，verify 看到的是 hook 已经处理过的工作树。编辑工具**自身失败**时不跑 verify——失败原因已经在工具结果里了，再跑一遍命令只是浪费。
- **有界**：默认超时 120s（`timeout_secs` 可覆盖），输出按文本预算截断，`sh -c`（Unix）/ `cmd /C`（Windows）执行。实现见 `core::verify`（`load_verify_cfg` / `run_verify`），装配点 `cli/main.rs` 的初次构建与 `/model` 热重建。

### Hooks 自动化系统

**Hooks 自动化系统**（格式对齐真实 Claude Code 的 hooks，路径完全自有）：生命周期钩子配置来源与 AGENTS.md 同一哲学。**v0.2.27 起三源全部迁入 `.wyj-code/` 体系**：`~/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.local.json`（后者追加不覆盖，local 文件供个人临时覆盖、不提交 git）；原三源 `~/.claude/settings.json` + `<git-root>/.claude/settings.json` + `<git-root>/.claude/settings.local.json` **不再被读取**。仅解析 settings.json 的 `hooks` 键，其它顶层键（真 CC 的 `permissions`/`env` 等）宽容忽略。支持 4 个事件：`PreToolUse`（工具执行前，可 block/approve）、`PostToolUse`（工具执行后，可追加反馈）、`UserPromptSubmit`（用户提交后进入推理前，可 block/追加上下文）、`Stop`（回合结束前，可继续下一轮）。每个 hook 是执行任意 shell 的 `command`，stdin 注入 JSON payload（含 session_id/cwd/event/tool_name/input/response），exit 2 表示 block（stderr 为原因），stdout JSON 可表达更丰富的 `decision`/`reason`/`additionalContext`/`continue`。默认超时 60s，可用 `--no-hooks` 完全禁用；首次检测到非空配置时打印一次性安全提示。实现：`core::hooks::HookRunner` 负责加载/合并/执行，`core::agent::Agent` 在 4 个精确插入点调用，`cli/main.rs` 构造并装配给主 Agent 与 TUI 重建路径，子 Agent 工厂不装配。

### /config 面板

**`/config`**：TUI 内 `/config` 打开设置面板（`OpenSettingsDialog`），现仅剩 `log_level`/`language` 两个字段（`SETTINGS_FIELD_COUNT = 2`，`crates/tui/src/app.rs`）；调用相关字段（`provider`/`model`/`base_url`/`api_key`/`plan_model`/`exec_model`）已迁移到 `/model` 的 Profile 分组管理器（`ProfileDialog`），按具名 Profile 管理，不再挂在 `/config` 下。`language` 留空（或空串）则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文；解析统一走 `wyj_i18n::resolve_locale`。

`language` 同时决定**模型输出语言**，不只是 UI 文案：解析结果经 `prompts::EnvInfo::collect` 进入 system prompt 的 `<env>` 块（`Response language: Chinese (zh)`），并由 `prompts::sub_agent_language_section` 追加给子 Agent（子 Agent 的 system prompt 是整体替换，拿不到 `<env>` 块）。i18n 本身仍只覆盖用户可见 UI 文案（TUI 对话框、slash 命令输出、CLI --help/--config-status），模型侧提示词（system prompt、工具描述等）为英文常量不走 i18n（见上方"模型侧提示词"节），工具内部错误消息等仍为中文，待后续阶段迁移。
