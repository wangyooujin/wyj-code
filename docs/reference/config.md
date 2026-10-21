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
# aux_model = ""             # 辅助模型（留空则辅助调用也用 model）。见下方「辅助模型」节
base_url = ""                # 留空使用供应商默认端点
max_tokens = 32000
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
# interleaved_thinking = true # 工具调用轮之间允许交错思考。**Anthropic 线专属**（OpenAI 线不发
                              # interleaved-thinking beta，此处配了也不生效）。生效条件是「本轮真的发出了
                              # thinking 字段」，因此 MiniMax `/anthropic` 的 adaptive 形态（无 budget）同样成立。
# reasoning_effort = "medium" # thinking 投入档位：low/medium/high/xhigh/max。**协议无关**——
                              # OpenAI 线发顶层 `reasoning_effort` 字段，Anthropic 线发
                              # `output_config.effort`（官方 Anthropic 与 MiniMax `/anthropic`
                              # 端点同构），同一份 profile 换协议不必改配置。
                              # **官方 OpenAI（o 系列 / gpt-5 系）同样生效**，但档位只到 high
                              # （`minimal` 在 gpt-5.1+ 不被接受、`xhigh`/`max` 仅部分新型号），
                              # `o1-mini` / `o3-mini` 完全不支持——越界档位记成可见的 dropped。
                              # 只有 vendor 的 ThinkingSpec::effort_levels 收录该档位才会真正上送，
                              # 其余被丢弃并在日志里 warn（`wyj-code model doctor` 可查）。
                              # 与 thinking_budget 的关系按端点而定：多数端点两者互斥（Qwen 协议
                              # 明确禁止同传），官方 Anthropic 则可同时用——budget 限**思考**的
                              # token 数，effort 限**整个响应**的投入档位，两者控制不同维度。
                              # 官方 OpenAI 没有 budget 旋钮：配了 thinking_budget 会被记成 dropped。
# thinking_switch = "enabled" # OpenAI 线 thinking.type 显式覆盖：enabled/disabled/adaptive。
                              # 不写 = 由 adapter 按模型默认决定。MiniMax M3.1 发 disabled 会 400，
                              # adapter 会降级成"不发该字段"。
# max_tools_per_turn = 4     # 每次回复最多并行几个工具调用。不写 = 由模型目录/探测结果决定
                              # （静态目录对非 anthropic/openai vendor 一律判 1）。`/model` 面板
                              # 里是枚举「默认 / 1 / 2 / 4 / 8」，新建分组预填 4；清空即回到端点默认。
                              # env var `WYJ_CODE_MAX_TOOLS_PER_TURN` 优先（runtime override，不写盘）。
log_level = "warn"           # 调试时设为 "debug"
language = ""                # "en"/"zh"，留空（或空串）自动检测系统 locale；决定 UI 文案 **与模型输出语言**（含 thinking）

[search]                     # WebSearch 工具的后端与请求默认值（未配置 api_key 则工具不注册，模型看不到）
provider = "tavily"          # 后端：tavily / brave。写成未知值会显式报错，不会静默回退
api_key = ""                 # API Key，优先读环境变量 WYJ_CODE_SEARCH_API_KEY（env 覆盖不会写回本文件）
timeout_secs = 30            # 单次搜索请求超时（秒）
max_results = 5              # 模型未显式指定时返回的条数（硬上限 10）
# allowed_domains = []       # 默认白名单域名（模型可用 include_domains 覆盖）
# blocked_domains = []       # 默认黑名单域名（模型可用 exclude_domains 覆盖）
language = ""                # Brave 的 search_lang（ISO 639-1），留空用后端默认值；tavily 忽略
safe_search = true           # 安全搜索（Brave 映射为 safesearch=strict）

[subagent]
default_profile = ""         # 子 Agent 默认 Profile 名，留空沿用主 Agent 当前分组
explore_profile = ""         # 内置 Explore 类型专用 Profile 名（配便宜模型），留空回退 default_profile
trace_enabled = true         # 是否把子 Agent 完整执行轨迹落盘（供跨会话查看，见下方 SubAgent 节）
trace_max_bytes_per_agent = 262144  # 单个子 Agent trace 文件字节上限（默认 256KB），超限静默停写
                             # 交接物（a<id>.handoff.json）不受此上限约束：它是摘要，且必须完整
                             # 消息增量日志（a<id>.messages.jsonl）固定 1MB 上限，超限留 truncated 标记
                             # 两者都只在 trace_enabled = true 时生成；日志是进程被硬杀时唯一的恢复来源
max_depth = 3                # 子 Agent 最大嵌套层数（主 Agent 之下算第一层）；1 = 禁止嵌套。
                             # 触顶时 Agent 工具会从该层子 Agent 的工具集里消失，而不是留着报错
max_concurrent = 8           # 同时运行的子 Agent 上限；超限的 spawn 排队等待，不失败

[auto_mode]                     # Auto 权限模式（默认起始模式，详见下方 Auto 模式节）
enabled = true                # 模型分类器总开关。false = 只保留本地确定性规则层，
                                # 拿不准的操作一律逐次弹窗（保守模式）。
classifier_profile = ""       # 分类器用哪个调用分组。留空 = 复用当前激活分组；
                                # 填分组名 = 用它单独跑（适合配便宜的小模型降本）
timeout_secs = 30             # 单次分类器请求超时。超时后重试一次，仍失败则交回人工。
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
                                # **思考关不掉的端点同样压到 32 轮**（判据走 ThinkingSpec：
                                # MiniMax M2.x/M3.1、Qwen/豆包的 -thinking、Moonshot 的 -code）。
                                # 它们的每轮都是一次完整深度推理，单回合 200 轮可达半小时以上。

[tools]                          # 工具注册门控（v0.2.27+）
todo_enabled = true            # 是否注册 TodoWrite 任务列表工具。**默认开启**
                                # （v0.3.1 起由关闭改为开启，见架构节
                                # 「TodoWrite 默认开启」）。未注册 = 工具不产生
                                # schema，模型目录里看不到它。简单任务为主、
                                # 或用强模型跑短任务时设 false 更划算。

[ui]                            # TUI 展示区交互（v0.3.10+）
mouse_capture = true           # 是否开启鼠标报告（DEC 1000/1002/1006），让滚轮与
                                # 拖拽事件进入应用。**默认开启**。
                                # 为什么必须开：alternate screen 里终端没有自己的滚动条，
                                # 滚轮能否滚动取决于应用是否收到鼠标事件；而 macOS
                                # Terminal.app 不支持 DEC mode 1007（alternate screen
                                # 滚轮转方向键），不开捕获时滚轮**完全没反应**。
                                # 开启后应用自己实现拖选：拖拽选中、松开自动复制到系统
                                # 剪贴板，双击选词（路径/URL 整体）。终端原生拖选在捕获
                                # 期间不生效，这是 Terminal.app 上「滚轮 + 直接拖选」两全
                                # 的唯一办法。
                                # 运行时可用 /mouse 切换，不写回配置文件；关闭后恢复
                                # 终端原生拖选（Terminal.app 下滚轮随之失效）。
scrollbar = true               # 内容区右侧是否绘制应用内细滚动条（│ / ┃，less/vim 观感）。
                                # 只在内容超出视口时出现，宽度 < 3 列的终端不画。

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

wyj-code 只有**一个日常模式** `auto`（历史上的 `manual` 已并入它，作为兼容别名保留解析）。判定「这一步要不要问人」走一条固定顺序的流水线（`core::auto_mode::AutoGate`），拿不准时交回逐次弹窗：

1. **受保护路径写入**（`.git`、`.wyj-code`、`.env`、密钥文件）——任何模式都不放行，直接拒绝；
2. **确定性 allow**——工作目录内的文件写、只读 shell 命令、构建/测试命令、lockfile 依赖安装，本地判定，零延迟零成本；
3. **确定性 hard_deny**——`curl|bash`、`rm -rf ~`、`git push --force`、`git reset --hard`、`terraform destroy`、`sudo <program>`、凭据路径、启动无审批的 agent 循环等，本地判定，**不发模型请求**；
4. **模型分类器**——前两步都没结论时才付一次模型往返，让它按 `soft_deny` / `allow` / `environment` 规则判断「这一步是否在用户请求范围内」。优先级 `hard_deny → soft_deny → allow → 用户显式意图`。
5. **重试后交回人工**——分类器超时 / 返回非法内容 / 空响应（常见成因是 `stop_reason=MaxTokens` 把输出预算吃光）会先用「加格式提醒 + 输出预算翻倍」的输入重试一次；两次都拿不到判定就交回人工弹窗，**不消耗**降级计数——模型压根没表达过否决意图，计成拦截会让闸门白白停用分类器。安全侧没有松：无交互面（headless / 子 Agent）上人工确认本身就是 fail-closed，行为与直接拒绝一致。
6. **降级**——连续 3 次或累计 20 次**被判定拒绝**（不含「交回人工」），自动**停用模型分类器**：此后只剩 1~2 两条确定性规则，拿不准的操作改为逐次弹窗。`hard_deny` 不受影响（它排在分类器开关之前，无论配置怎么写都拦得住）。用 `/mode auto` 显式选一次可重新启用分类器。

> 第 1~3 步的顺序是硬要求：**两条确定性规则必须排在「分类器是否可用」之前**。否则在 `[auto_mode] enabled = false` 时 `curl|bash` 只会剩下一个弹窗，用户按一个 `y` 就过去了——降级降的应是语义判定，不是安全底线。

三种模式用 `--permission-mode auto|plan|bypass` 或 TUI 的 Shift+Tab 切换（`manual` / `default` / `normal` 仍可解析，全部落到 `auto`，是为了让旧配置与脚本不至于直接失效）；`--plan` / `--bypass-permissions` 是另两档的便捷写法。**auto 不要求交互面**，因此 `-p` / headless / 定时任务同样可用——这正是它相对「纯逐次弹窗」唯一而决定性的优势。

**`bypass` 免的是「询问」，不是「禁止」**：它跳过全部权限确认，但 `core::auto_rules` 里的 hard_deny 规则仍然生效——`rm -rf ~`、`git reset --hard`、`git push --force`、`curl <url> | bash`、`sudo`、`terraform destroy`、触碰 `~/.ssh` 等凭据路径，以及写入受保护路径（`.git/`、`.env`、`*.pem`、`~/.wyj-code/`）在任何模式下都会被拒。这些操作不可逆，用户选择 bypass 时表达的是「别问我」，而不是「允许毁掉家目录」。确实需要时请自己在终端执行。

`[auto_mode]` 的四个规则数组是**英文自然语言规则**（喂给分类器，不是正则），支持 `$defaults` 占位符原位插入内置规则。内置清单见 `core::auto_rules`。

两个容易踩的点：

- **分类器与主会话共用同一 Provider 时，它的 token 计在主会话的 `total_*` 里**。想彻底隔离，配一个独立的 `classifier_profile`（配便宜的小模型通常足够）。
- **「本轮已放行」的记忆粒度按工具分叉：Bash 按工具名，其余按输入指纹**。批准一次 Bash 后本轮所有 Bash 免问（命令全文不参与这把钥匙）——Bash 的风险几乎由程序+子命令决定，闸门在复用之前仍逐条走过 hard_deny 与只读白名单。写工具、`Agent`、computer-use 则一律按规范化输入指纹：`Edit` 批过 `notes.md` 不该顺带放行 `deploy.sh`，用户在为一个文件点头、模型顺手改了另一个是**收紧**而不是继承。key 打乱顺序的等价 JSON 仍算同一次调用。
- **子 Agent 继承同一个闸门实例**（含降级计数），所以子 Agent 的副作用工具也由分类器判定，而不是退回逐次弹窗。headless / 后端子 Agent 没有审批 UI，闸门判「交回人工」时会在 `confirm_tool` 处 fail-closed 拒掉该次调用。

### 项目规约文件回退链

**项目规约文件记忆机制**（`core::claude_md::ClaudeMdLoader`）：查找范围为全局 `~/.wyj-code/` + 从项目根到 cwd 的祖先链（`config::project_root`，找不到 `.git`/`.wyj-code/project.toml` 则只用 cwd 本身）。**v0.2.31 起按槽位独立回退、命中第一个即停**：主槽位 `AGENTS.md` → `CLAUDE.md` → `GEMINI.md` → `AGENT.md`，个人覆盖槽 `AGENTS.local.md` → `CLAUDE.local.md`；两槽位都命中时 local 追加在后（视作个人覆盖增补，不提交 git）。**`AGENTS.md` 始终是首选**（复数，对齐 AGENTS.md 开放标准，Codex / Cursor / Gemini CLI 等均采用），回退只是为了让「根目录只有 CLAUDE.md」的既有仓库不至于对模型完全失明——这些文件都是项目作者写的规约，读不到等于整个项目的约定不可见。**两个槽位独立而非「整个家族二选一」**，是为了保住 personal override 语义：主文件走 `AGENTS.md` 而私有覆盖写在 `CLAUDE.local.md` 时仍然生效，不被静默丢弃；两槽位各只取一个文件，因此**永远不存在同一份规约被注入两遍**。判定用「有实际内容」而非单纯 `is_file()`：0 字节 / 全空白的主文件继续往下探。**历史沿革**：v0.2.27 曾硬切到只读 `AGENTS.md`（明确不保留回退，测试 `load_dir_files_ignores_claude_md_entirely` 锁死），v0.2.31 在保留「命中即停、绝不双份注入」这个核心不变式的前提下把回退链加了回来，该锁死测试已被替换为回退链系列测试。**全局路径仍只有 `~/.wyj-code/`，任何 `~/.claude/` 一律不读**（`config::claude_home_dir()` 已删除、替换为 `config::global_config_dir()`）；读的是**项目目录内**的 `CLAUDE.md` 文件，与读取外部工具的全局配置目录是两回事。支持 `@path/to/file` 递归导入（深度上限 4，跳过 fenced code block）。每轮对话开始时重新读盘，以 `<system-reminder>` 包装拼进当轮 **system prompt 末尾**（不注入 user 消息、不进历史，配合 prompt caching 避免跨轮累积；文件不变时字节级稳定、缓存可命中；注入文本里含具体路径与文件名，故增删/改名规约文件会自然失效该段缓存）。工具（Read/Edit/Write/Glob/Grep）触达某个文件后，从其所在目录**逐级向上**查到项目根，沿途把每个「有规约文件且本会话未展示过」的目录追加到 system 末尾（`ClaudeMdLoader::maybe_dir_reminder`，按**归一化后**的目录去重）。**必须向上找而不是只看触达目录**：工具报上来的是文件的直接父目录（`crates/core/src/`），而规约习惯写在模块根（`crates/core/CLAUDE.md`），只看直接父目录会让有 `src/` 层的模块规约永远对模型不可见。项目根是硬边界，触达项目外的绝对路径只取其自身、不向上爬。单目录注入文本按 64KB 封顶，超限时保留头部并显式标注 `[truncated: ...]`（绝不静默丢尾，否则模型会以为自己读全了规则），防的是「把生成物误写成规约文件」这类事故而非压缩正常规约。`/init` 触发一次真正的 agent 回合（`CommandResult::RunPrompt`）去探索项目并生成/合并更新**项目规约文件**，而非静态模板写文件。**生成目标跟随仓库现状**：走 `claude_md::existing_memory_file_name`（与实际注入同一条 `resolve_slot` 解析线）取「当前真正会被读到的那一份」，只有当一个候选都不存在时才回退到规范名 `AGENTS.md` 创建新文件，并要求把其余不同名的规则文件一并整合进去。**这条不是洁癖**：本仓库自己就踩过——v0.2.31 的提交同时留下了 `AGENTS.md` 与内容逐字节相同的 `CLAUDE.md`，两份副本随后各自漂移；`git rm` 掉前者之后，若 `/init` 仍硬编码 `AGENTS.md`，下次跑一次就会把那份僵尸副本原样重建。「命中即停」只保证模型不双份注入，**管不住维护者写出两份不同步的文件**。回归测试 `init_target_follows_the_file_that_would_actually_be_read` 是这条不变式的钉子。`/memory` 打开 TUI 面板列出当前会话适用的全部文件（与注入共用同一个 `resolve_slot`，面板显示的就是实际会注入的那个文件，不存在「面板列了 A、实际读 B」的漂移；一个候选都不存在时才列出 `AGENTS.md` 作为 `exists=false` 的创建入口），选中后挂起 TUI 唤起 `$EDITOR` 编辑，同时暴露 auto-memory（跨会话记忆提取）开关与索引入口（`Config.auto_memory_enabled`）。不再兼容旧版 `WYJ.md`。**注意 i18n key 名仍沿用历史拼写 `claude_md_ok` / `claude_md_missing`（值已泛化为「项目规约 / Memory files」，不再硬编码某个文件名——回退生效时写死文件名会让 `doctor` 报出「未找到」这种与事实相反的结果），key 是代码契约，不要因为改名去动 i18n YAML 里的 key。**

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

### 辅助模型（aux_model）

**`aux_model`**：把**低风险、短提示词**的辅助调用切到更便宜/更快的模型。留空 = 辅助调用也用 `model`（默认行为，不配就没有任何代价）。

```toml
[[profiles]]
name = "default"
model = "MiniMax-M3"
aux_model = "MiniMax-M3.1-Flash-Preview"   # 留空 = 辅助调用也用 MiniMax-M3
```

生效的辅助调用点（**只有这两个**）：

| 调用点 | 位置 | 提示词规模 |
|---|---|---|
| 会话标题生成 | `core::summary` | 短常量，上限 256 output token |
| 跨会话记忆提取 | `core::memory` / `core::memory_v3` | 最近 20 条消息文本（有界） |

**四条不可回退的约束**：

1. **上下文压缩（compact）绝不用辅助模型**。压缩请求逐字节复用主对话的 system + tools 前缀来命中 prompt cache（`core::agent` 的 `CompactPrefix`），换模型等于前缀不等 → 缓存归零 → 整段历史全价重发。长会话里这一次重发比所有辅助调用的总开销大一个数量级。测试 `compaction_never_uses_the_auxiliary_provider` 钉住这条。
2. **换模型不打穿主会话缓存**。两个辅助调用各自用**独立的短 system prompt**（`prompts::MEMORY_SYSTEM` 等），不复用主对话前缀，因此主会话 87% 的缓存命中率不受影响。
3. **必须同分组、同端点、同 Key**。辅助模型会收到对话内容片段，跨供应商路由等于把这些内容发给另一家服务商——这与 `[routing].cross_provider_fallback = false` 守的是同一条边界。
4. **辅助调用失败不牵连主流程**。标题生成失败 → 静默跳过；记忆提取失败 → `tracing::debug!` 吞掉；provider 构建失败 → 告警并退回主模型。三条路径都不会中断主对话。

**thinking 不是「更便宜」的保证**：辅助调用用 `RequestOptions::text_only(N)`，本身不发 thinking 参数，但端点默认行为仍在——MiniMax M3.1 在 Anthropic 端点**关不掉** thinking（`thinking.rs` 的 `user_can_disable: false`），M3 则可以。选 M3.1-Flash 意味着 thinking token 照计，最终费用以 `/cost` 实测为准，不要只看标价。

**单价参考**（MiniMax 官方，人民币/百万 tokens；`/cost` 表按 7.0 折算美元）：

| 模型 | 输入 | 输出 | 缓存读取 |
|---|---|---|---|
| MiniMax-M3.1-Flash | 1.00 | 4.00 | 0.10（0.1x） |
| MiniMax-M3 | 2.10 | 8.40 | 0.42（0.2x） |

### 网页搜索（WebSearch）

**`[search]` 节**配置内置网页搜索工具（`WebSearch`）的后端与请求默认值。它是 wyj-code 的**内置工具**——不是 MCP server、不是让模型自己拼 `curl` 的 shell 命令，而是与 Read / Grep / WebFetch 并列的一个正式工具。

```toml
[search]
provider = "tavily"          # 后端：tavily / brave
api_key = ""                 # 优先读环境变量 WYJ_CODE_SEARCH_API_KEY
timeout_secs = 30
max_results = 5              # 模型未显式指定时返回的条数（硬上限 10）
```

**两种「不注册」的语义必须分开**（`Config::search_provider_issue` 是唯一判定入口，`/doctor` 与 `--config-status` 都基于它）：

| 情况 | 行为 | 原因 |
|---|---|---|
| 没有 API Key | **工具不注册**，模型目录里看不到它 | 预期行为——模型不该看到一个必然失败的工具，不浪费一次 LLM 往返 |
| provider 名字写错 | **显式报错**，绝不静默回退到默认后端 | 配置错误必须被看见；静默回退会让用户以为用的是 Brave |

**Key 只进请求头**（Tavily 走 `Authorization`，Brave 走 `X-Subscription-Token`），绝不放进请求体——body 会被代理日志与错误上报完整打印，头不会。`WYJ_CODE_SEARCH_API_KEY` 的值**不会**被写回 `config.toml`。

**后端差异是显式的**：`BraveBackend::supports_topic()` 返回 `false`，模型请求 `topic` 时工具层**直接报错**而不是静默忽略——静默忽略会让模型以为「新闻检索」生效了，实际拿到的是通用结果。归一化模型（`SearchRequest` / `SearchHit` / `SearchResponse`）是两家的并集，新增后端只需实现 `SearchBackend`，不必碰工具定义、权限名单、i18n 与 TUI 渲染。

**模型可用的六个参数**：`query`（必填）、`max_results`、`topic`（general / news / finance）、`time_range`（day / week / month / year）、`include_domains` / `exclude_domains`。模型显式给域名过滤时**以模型为准（覆盖，不是并集）**：并集会让模型既无法临时放开配置里的黑名单，也无法把白名单收窄成自己需要的几个域——两种用法都是排障时的常见需求。

**外部文本按不可信输入处理**：结果统一过 `escape_harness_markers` 才能回填给模型（否则网页正文里的 `</system-reminder>` 或行首 `Human:` 能伪造 harness 结构与对话轮次）；超长输出走 spill 落盘而非硬截断，模型可用 Read 取回被省略的中后段。瞬时错误（`Transient`）退避重试一次，限流 / 认证 / 参数错误**不重试**——重试只会更快烧掉配额或拿到同样的错误。

### Computer-use（桌面 GUI 控制）

**`[computer_use]` 节**配置桌面 GUI 控制工具（`computer` / `app_computer` / `window_capture`）。三个工具默认在 macOS + `vision = true` + `provider = "anthropic"` 时注册；`app_computer` 是否用原生 `computer_20251124` 声明另由「是否官方端点」决定，与 `effective_prompt_cache()` 共用同一判定。

```toml
[computer_use]
foreground_fallback = "disabled"   # 旧 computer 工具的前台接管策略：disabled / ask / idle_only
quiet_period_ms = 2000             # 获得输入租约前必须连续无外部输入的时长
max_defer_secs = 30                # 等待用户空闲的最长时间，到期失败关闭
restore_context = true             # 前台兼容动作结束后恢复原观察上下文
auto_screenshot = true             # 变更动作成功后自动附带一张目标窗口截图
auto_screenshot_max_dim = 960      # 附图的长边像素上限（低于观察用的 1280）
```

**`auto_screenshot` 是「动作 → 观察」闭环的核心**。`app_computer` 的变更动作（`click` / `set_text` / `key` / `scroll` / `press_menu`）成功后会附一张**动作之后**的窗口截图，模型不必自己再调一次 `screenshot` 才知道界面变了没有。关掉它并不会关掉整条验证链：连续 20 次变更动作没有截图时（`MAX_ACTIONS_WITHOUT_SCREENSHOT`）仍会强制附一张。三条不变式——只在成功路径附图（失败时错误信息更重要）、附图失败绝不改变工具结果语义、附图分辨率刻意低于观察用的 `max_dim`（它只是「确认界面变了没有」，要看清小字时模型会显式 `screenshot` 或 `zoom`）。

**`auto_screenshot = true` 的 token 代价**：每张 960 长边的 PNG 约 1000~1600 input token。长 GUI 任务里累积可观，需要控制成本时关掉，改由提示词驱动的按需截图。

**`foreground_fallback` 的三档语义**：`disabled`（默认）下旧 `computer` 的变更动作直接返回 `requires_foreground_takeover`；`ask` 每次变更弹一次确认（要求真实 TUI 交互通道，headless / cron / 子 Agent 一律拒绝）；`idle_only` 只按安静期执行。三者都仍受 `InputArbiter` 与前台 PID 校验约束，且都不会让 `app_computer` 的失败静默回退到全局输入。

**权限**：`computer` / `app_computer` 属 `PROJECT_APPROVE_ONCE_TOOLS`——首次按 y/Enter/a 即写入当前项目的 `allowed_tools.json`，同项目后续动作与重开项目都不再确认，不同项目仍隔离，拒绝不落盘。两者**互不隐式授权**：批准后台工具不等于批准前台全局输入。

**诊断**：`/computer` 报告平台/vision/provider 门控结果、AX 授权、InputArbiter 状态、前台回退配置、稳定窗口数量、截图与全局输入探测，以及只存在当前进程内的路径计数与熔断计数（`automatic_foreground_fallbacks` 是显式恒零不变量）。它不依赖工具是否真的注册成功。

### Hooks 自动化系统

**Hooks 自动化系统**（格式对齐真实 Claude Code 的 hooks，路径完全自有）：生命周期钩子配置来源与 AGENTS.md 同一哲学。**v0.2.27 起三源全部迁入 `.wyj-code/` 体系**：`~/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.local.json`（后者追加不覆盖，local 文件供个人临时覆盖、不提交 git）；原三源 `~/.claude/settings.json` + `<git-root>/.claude/settings.json` + `<git-root>/.claude/settings.local.json` **不再被读取**。仅解析 settings.json 的 `hooks` 键，其它顶层键（真 CC 的 `permissions`/`env` 等）宽容忽略。支持 4 个事件：`PreToolUse`（工具执行前，可 block/approve）、`PostToolUse`（工具执行后，可追加反馈）、`UserPromptSubmit`（用户提交后进入推理前，可 block/追加上下文）、`Stop`（回合结束前，可继续下一轮）。每个 hook 是执行任意 shell 的 `command`，stdin 注入 JSON payload（含 session_id/cwd/event/tool_name/input/response），exit 2 表示 block（stderr 为原因），stdout JSON 可表达更丰富的 `decision`/`reason`/`additionalContext`/`continue`。默认超时 60s，可用 `--no-hooks` 完全禁用；首次检测到非空配置时打印一次性安全提示。实现：`core::hooks::HookRunner` 负责加载/合并/执行，`core::agent::Agent` 在 4 个精确插入点调用，`cli/main.rs` 构造并装配给主 Agent 与 TUI 重建路径，子 Agent 工厂不装配。

### /config 面板

**`/config`**：TUI 内 `/config` 打开设置面板（`OpenSettingsDialog`），现仅剩 `log_level`/`language` 两个字段（`SETTINGS_FIELD_COUNT = 2`，`crates/tui/src/app.rs`）；调用相关字段（`provider`/`model`/`base_url`/`api_key`/`plan_model`/`exec_model`/`aux_model`）已迁移到 `/model` 的 Profile 分组管理器（`ProfileDialog`），按具名 Profile 管理，不再挂在 `/config` 下。`language` 留空（或空串）则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文；解析统一走 `wyj_i18n::resolve_locale`。

`language` 同时决定**模型输出语言**，不只是 UI 文案：解析结果经 `prompts::EnvInfo::collect` 进入 system prompt 的 `<env>` 块（`Response language: Chinese (zh)`），并由 `prompts::sub_agent_language_section` 追加给子 Agent（子 Agent 的 system prompt 是整体替换，拿不到 `<env>` 块）。i18n 本身仍只覆盖用户可见 UI 文案（TUI 对话框、slash 命令输出、CLI --help/--config-status），模型侧提示词（system prompt、工具描述等）为英文常量不走 i18n（见上方"模型侧提示词"节），工具内部错误消息等仍为中文，待后续阶段迁移。
