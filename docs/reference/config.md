# 配置参考

本文档是 `~/.wyj-code/config.toml` 的**带注释全量参考**。根目录 `CLAUDE.md` 只保留
日常最常改的几个键；其余全部搬到这里，按需查阅而不是每轮注入。

---

## 文件结构

配置文件：`~/.wyj-code/config.toml`，API Key 优先读取环境变量 `WYJ_CODE_API_KEY`。

### 数据根与落盘布局

整个状态目录默认是 `~/.wyj-code`，可用环境变量 **`WYJ_CODE_HOME`** 指向别处
（外置盘、独立测试环境）。它是进程级决定，对所有子系统一致生效：

```bash
WYJ_CODE_HOME=/Volumes/external/wyj-state wyj-code
```

路径模板（唯一来源是 `wyj_config::layout`，见 `crates/config/src/layout.rs`）：

```
<data_root>/
├── config.toml  settings.json  AGENTS.md  installed.json
├── skills/  agents/  plugins/  marketplaces/  model-capabilities/
├── sessions/<project_key>/       # 会话 + .meta.json + .checkpoints/ + .subagents/
├── history/<project_key>/history.jsonl
├── memory-v3/{global,projects/<key>}/
├── evolution/<project_key>/  indexes/<project_key>/
├── workspaces/<project_key>/  workflows/<project_key>/
├── projects/<project_key>/{allowed_tools,mcp_trust,skill_trust,hooks_trust}.json
│   ├── instances/  meta.json
│   └── skill-data/<skill>/       # 项目级 skill 的可写产物（不参与信任指纹）
└── cas/  spill/  logs/
```

`projects/<key>/skill-data/` 是给项目级 skill **写运行产物**的地方（下载的素材、
缓存、中间文件）。项目级 skill 的信任指纹覆盖仓库内 `.wyj-code/skills/` 整棵树，
写回那棵树的任何产物都会让刚批准的信任失效、下次启动重新追问；把产物写到这里
既不影响指纹，也不会被 `git pull` 覆盖。脚本里用 `$WYJ_CODE_SKILL_DATA` 拿到该路径
（前台与后台 Bash 都会注入；目录不预先创建，需要时自己 `mkdir -p`）。

`project_key` 形如 `v1-wyj-code-1f367bbfb19407b9`（目录名 + SHA-256 前 16 位），
按 **git 仓库根**归一：同一仓库的任意子目录、以及带 `--project-root` 指回主仓库的
worktree，都归到同一个 key。

**升级到按项目分目录的版本后**，旧数据仍在根目录，功能不会坏（读取有兜底），
但建议跑一次迁移：

```bash
wyj-code storage migrate          # 只报计划
wyj-code storage migrate --yes    # 实际搬迁（rename，同盘原子）
```

迁移搬三类：会话整组文件、`history.jsonl` 按行拆分、旧项目键目录改名。无法确定
归属的条目会报 `skipped` 并保持原样。

#### 落盘范围：`storage.scope`

```toml
[storage]
scope = "home"      # 默认。"project" = 会话与历史跟着工作副本走
```

`scope = "project"` 时，会话与历史改放 `<git-root>/.wyj-code/local/`，同时自动写
两道 gitignore（`local/.gitignore` 内容 `*`，仓库根 `.gitignore` 追加
`.wyj-code/local/`）。选它的理由是「换机器/换 clone 就带走对话历史」，代价是：

- transcript 里是工具输出原文，漏一次 `git add -A` 就是永久泄漏（gitignore 是
  自动写的，但**不要手工删**）；
- 多 worktree 时每个 worktree 一份独立会话，`-c` 只看到当前 worktree 的；
- 会话体积会进入 `git status` 的 stat 成本。

同一个开关还作用于**记忆 / 演化 / 代码索引的项目层**：

| 跟随 scope | 落到 `<git-root>/.wyj-code/local/` 下的 |
|---|---|
| 会话 | `sessions/<project_key>/` |
| 历史 | `history/<project_key>/history.jsonl` |
| 记忆（项目层） | `memory-v3/projects/<project_key>/` |
| 演化 | `evolution/<project_key>/` |
| 代码索引 | `indexes/<project_key>/lexical-v1.json` |

**不跟随的**（各有硬约束）：`memory-v3/global/`（跨项目共用单文件）、
`~/.wyj-code/projects/<key>/`（信任门控必须在仓库外，否则被信任的仓库能自己改掉
批准标记）、`cas/`（跨项目去重池，`gc` 是全局 LRU）、`workspaces/` 与
`workflows/`（本机运行态：worktree 路径、进程 pid）、`spill/`、`logs/`、
`model-capabilities/`。

**切换不会让状态凭空消失**：新位置还没有记录时，启动会把 home 下的旧项目目录
复制一份过来（不覆盖已有文件、不删源）。要真正收拢数据仍请手工搬目录——这不是
迁移工具。

定义类配置（`mcp.toml`、`settings.toml`、`skills/`、`agents/`）不受这个开关影响。

#### 会话与锁的保留策略

```toml
[storage]
sessions_ttl_days = 0        # 0 = 不自动删（默认）。设 30/90 启用
sessions_keep_recent = 5     # 每个项目无条件保留最新 N 条
stale_lock_ttl_days = 7      # 删 cas/locks/ 与 sessions/*.lock 里无人持有且超期的
```

`sessions_ttl_days` **默认 0**：删历史不可逆，不该在升级后第一次启动里静默发生。
想立刻清一次用 `wyj-code storage prune`（dry-run）看计划，`--yes` 执行；未设置时
`prune` 按 30 天算，且 dry-run 与 `--yes` 用同一个值。

`sessions_keep_recent` 是 TTL 的必要配套：纯 TTL 会把「三个月没动的项目里唯一那批
会话」删光，而那恰恰是最想回看的。

删除一个会话会连同它的 `.meta.json`、`.json.lock`、`.json.owner`、
`.checkpoints/`、`.subagents/` 一起删——只删正文会留下孤儿 checkpoint 目录。

**项目级定义**（可提交、随 clone 落地）仍在 `<git-root>/.wyj-code/`：`mcp.toml`、
`settings.toml`、`settings.json`、`skills/`、`agents/`、`installed.json`。
`allowed_tools.json` 与 `*_trust.json` **不在**这里——它们记录「用户批准过什么」，
放进仓库等于让仓库自己改掉批准标记。

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
                              # **会话级覆盖**：`/effort <档位>` 临时改档，只随**会话文件**落盘
                              # （`--resume` 后继续生效），**绝不写本文件**。生效优先级是
                              # env（`WYJ_CODE_THINKING_EFFORT`）> 会话级 > 本字段。
                              # `/effort off` = 彻底关思考（effort 清空 + thinking_switch=disabled）。
# thinking_switch = "enabled" # OpenAI 线 thinking.type 显式覆盖：enabled/disabled/adaptive。
                              # 不写 = 由 adapter 按模型默认决定。MiniMax M3.1 发 disabled 会 400，
                              # adapter 会降级成"不发该字段"。
# max_tools_per_turn = 4     # 每次回复最多并行几个工具调用。不写 = 由模型目录/探测结果决定
                              # （静态目录对非 anthropic/openai vendor 一律判 1）。`/profile` 面板
                              # 里是枚举「默认 / 1 / 2 / 4 / 8」，新建分组预填 4；清空即回到端点默认。
                              # env var `WYJ_CODE_MAX_TOOLS_PER_TURN` 优先（runtime override，不写盘）。
log_level = "warn"           # 调试时设为 "debug"
language = ""                # "en"/"zh"，留空（或空串）自动检测系统 locale；决定 UI 文案 **与模型输出语言**（含 thinking）
auto_memory_enabled = true   # 跨会话记忆自动提取（/memory 面板可切换）。false = 不再每轮后台提取
active_profile = "default"   # 当前激活的分组名，必须在 [[profiles]] 里有同名项

# ── 调用分组：一套完整的端点 + 凭据 + 能力组合 ───────────────────────────────
# /profile 面板管理整套分组（写本文件）；/model 只切当前分组下的模型（仅内存）。
[[profiles]]
name = "default"             # 分组名，唯一。`--profile <name>` 一次性指定本次运行用哪套
provider = "anthropic"       # anthropic / openai —— 决定用哪个客户端实现（线协议由它唯一决定）
model = "claude-opus-4-8"
plan_model = ""              # Plan 模式专用模型，留空则用 model
exec_model = ""              # Bypass 模式专用模型，留空则用 model
aux_model = ""              # 辅助模型（标题生成 / 记忆提取），留空则用 model；见「辅助模型」节
base_url = ""                # 留空使用供应商默认端点
api_key = ""                 # 不推荐：明文落盘。优先用下一行的 api_key_env 或环境变量
api_key_env = ""             # 从该环境变量名读取 Key，运行时取值、不写回本文件
max_tokens = 32000           # 单轮输出预算
context_window = 200000      # **必须填真实值**：压缩/清理阈值都由它推算，填大了两套保护等于没开
vision = true                # 模型是否支持图片输入；false 时图片降级为占位文本（防非多模态端点 400）
thinking_budget = 0          # 扩展思考预算 token；0/不写 = 关闭（思考计入 output 计费）
interleaved_thinking = true  # 工具轮之间允许交错思考。Anthropic 线专属，OpenAI 线配了不生效
prompt_cache = true          # 留空 = 跟随端点类型（官方 Anthropic 开、第三方兼容端点关）
openai_stream_options = false # OpenAI 兼容端点可显式关闭 stream_options.include_usage
reasoning_effort = "medium"  # low/medium/high/xhigh/max。协议无关（OpenAI 发顶层字段，Anthropic 发 output_config.effort）
thinking_switch = "enabled"  # OpenAI 线 thinking.type 覆盖：enabled/disabled/adaptive。不写 = 按模型默认
max_tools_per_turn = 4       # 每次回复最多并行几个工具；不写 = 由模型目录决定（国内 vendor 常判 1）

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
                                # 死循环保护由 loop_guard 独立承担，不受本值影响。
forced_thinking_max_turns = 64  # 强制思考端点的单回合熔断轮数。**默认 64**（v0.3.19+）。
                                # 判据走 ThinkingSpec：模型名含 reasoner / -r1，或端点
                                # **思考根本关不掉**（MiniMax M2.x/M3.1、Qwen/豆包的
                                # -thinking、Moonshot 的 -code）。这类端点每轮都是一次完整
                                # 深度推理，实测单回合 163 轮 / 35.6 万 output token /
                                # 半小时以上，所以默认值比 max_turns 保守。
                                # v0.3.19 之前写死 32，代价是「一轮一个工具调用」的端点
                                # 在中型任务上必然撞顶（第 28 轮就关闸），任务被切成好几段、
                                # 用户每次都得手打「继续」。调高它买的是「单轮能推进多远」，
                                # 代价是单次提交的最长耗时与 token 上限。
                                # 死循环保护同样由 loop_guard 独立承担。

[tools]                          # 工具注册门控（v0.2.27+）
todo_enabled = true            # 是否注册 TodoWrite 任务列表工具。**默认开启**
                                # （v0.3.1 起由关闭改为开启，见架构节
                                # 「TodoWrite 默认开启」）。未注册 = 工具不产生
                                # schema，模型目录里看不到它。简单任务为主、
                                # 或用强模型跑短任务时设 false 更划算。

[ui]                            # TUI 展示区交互（v0.3.10+）
mouse = "auto"                 # 鼠标报告（DEC 1000/1002/1006）策略，三态：
                                #   "auto"（默认）按 TERM 自动判定——只有缺失、空串或
                                #          dumb 这类明确不支持鼠标上报的环境才关闭；
                                #   "on"   强制开启；
                                #   "off"  强制关闭，完全交还终端。
                                # 为什么默认要开：alternate screen 里终端没有自己的滚动条，
                                # 滚轮能否滚动取决于应用是否收到鼠标事件；而 macOS
                                # Terminal.app 不支持 DEC mode 1007（alternate screen
                                # 滚轮转方向键），不开捕获时滚轮**完全没反应**。
                                # 开启后滚轮、拖选、双击选词、滚动条拖动全部由应用实现：
                                # 内容区与输入框都能拖选，松开自动复制到系统剪贴板。
                                # `off` 只留给「想用终端原生选区与自身滚动缓冲」的场景，
                                # 代价是 Terminal.app 下滚轮不再滚动内容区。
                                # 旧字段 `mouse_capture = true/false` 仍被识别（等价于
                                # on/off），但不会写回，下次保存配置时消失。
scrollbar = true               # 内容区右侧是否绘制应用内细滚动条（│ / ┃，less/vim 观感）。
                                # 只在内容超出视口时出现，宽度 < 3 列的终端不画；
                                # 出现后可用鼠标拖动滑块或点击轨道跳转。

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
                              # 后台子 Agent 完成统一派发到
                              # 终端响铃（stderr \x07）与桌面通知（macOS osascript /
                              # Linux notify-send / Windows PowerShell BurntToast）。
                              # 零新第三方依赖。详见 `wyj_core::notify` 模块文档。
enabled = true               # master 开关（opt-out 默认开）
rate_limit_seconds = 30      # 同类事件最小间隔去重；0 = 关闭

[notify.bell]
enabled = false              # 终端响铃 opt-in 默认关（TUI 共享场景避免扰民）

[notify.desktop]
enabled = true               # 桌面通知 opt-out 默认开（后台子 Agent 场景必须通知）

[notify.events]
turn_finished = true         # CLI `-p` / REPL / TUI 回合正常完成
turn_error = true            # 主回合出错
subagent_done = true         # 后台子 Agent 完成（run_in_background: true）；前台调用不通知

# include_session_id = false   # body 末尾追加 `[session:<id>]`，默认关闭（body 短）

# env override（init 阶段读取，绝不写回 cfg）：
#   WYJ_CODE_NOTIFY_OFF=1      全关（最高优先级）
#   WYJ_CODE_NOTIFY_BELL=0/1   覆盖 notify.bell.enabled
#   WYJ_CODE_NOTIFY_DESKTOP=0/1 覆盖 notify.desktop.enabled

[routing]                        # 同角色多候选 Profile 与可恢复错误 fallback
cross_provider_fallback = false  # **默认 false**：故障切换只在同 vendor 内进行。打开等于允许把
                                 # 同一段对话在用户未授权时发给另一家供应商——与「辅助模型必须同端点」
                                 # 守的是同一条边界。确有多家同源端点时才开。
[routing.roles]                  # 每个角色是一个 **Profile 名**列表（按顺序尝试），不是裸模型 id：
                                 # 候选要能各自携带自己的 vendor / 协议 / endpoint / 能力配置。
explore = ["default"]            # Explore 子 Agent 的角色链
plan = []                        # 规划角色链；空 = 用当前激活分组
execute = []                     # 执行角色链；空 = 用当前激活分组
review = []                      # 评审角色链；空 = 用当前激活分组

[model_runtime]                  # 国内模型工具协议与能力探测的保守默认值
probe_mode = "explicit"          # 能力探测模式：explicit（只在显式请求时探；默认）/ 其它由
                                 # `wyj-code model doctor` 与 startup 复用。默认值刻意保守：
                                 # 未经探测的能力一律标 static_only，不假装已验证。
probe_ttl_hours = 168            # 探测结果缓存有效期（小时）。默认 7 天，过期后 `doctor --refresh` 才重探
tool_argument_retries = 2        # 工具参数解析失败后的重试次数（弱模型常产出半截 JSON）；
                                 # 0 = 不重试，直接把错误回灌给模型
lazy_tools_threshold = 12        # 工具总数超过该值时启用 lazy schema（只给核心工具完整 schema，
                                 # 其余走 ToolSearch 按需展开）。工具少时全量给更省一次往返
lazy_tools_top_k = 8             # lazy 模式下一轮最多展开几个候选工具
lazy_tools_sticky_turns = 3      # 命中的工具在搜索结果里**排序加权**的回合窗口。
                                 # **v0.3.14 起不再控制可见性**：可见性单调不减，因为工具数组
                                 # 物理排在 prompt cache 前缀最前面，中途摘工具会让整段缓存失效

[agent]                          # Agent 推理循环参数
max_turns = 200                  # 单次用户提交内允许的最大推理轮数（一次 LLM 往返 = 一轮）。
                                 # **默认 200**，本意是「防模型死循环」，不是给正常长任务设工期。
                                 # 距上限还剩 4 轮时会先注入收尾指令，要求模型停止调用工具、
                                 # 立即总结当前进度。调高它买「单轮能推进多远」，代价是单次提交
                                 # 的最长耗时与 token 上限；死循环保护由 loop_guard 独立承担
forced_thinking_max_turns = 64   # **思考关不掉的端点**（reasoner / -r1 / MiniMax M2.x-M3.1 /
                                 # Qwen·豆包 -thinking / Moonshot -code）的单回合熔断轮数。
                                 # 默认 64 而非与 max_turns 齐平：这类端点每轮都是一次完整深度
                                 # 推理，实测单回合可达 163 轮 / 35.6 万 output token

[tools]
todo_enabled = true              # 是否注册 TodoWrite 任务列表工具（**默认开启**）。
                                 # 未注册 = 工具不产生 schema，模型目录里看不到它。
                                 # 简单任务为主、或用强模型跑短任务时设 false 更划算

[ui]                             # TUI 展示区交互
mouse = "auto"                   # 鼠标报告策略三态：auto（按 TERM 判定，默认）/ on / off。
                                 # 默认开的原因：alternate screen 里终端没有自己的滚动条，而
                                 # macOS Terminal.app 不支持 DEC 1007，不开捕获时滚轮完全没反应。
                                 # 开启后滚轮、拖选、双击选词、滚动条拖动全部由应用实现
# mouse_capture = true           # **已废弃**：v0.3.18 前的布尔字段，等价于 on/off，不再写回
scrollbar = true                 # 内容区右侧是否绘制应用内细滚动条（│/┃）。
                                 # 只在内容超出视口时出现，宽度 < 3 列的终端不画

[evolution]                      # 基于真实 Episode 的本地自进化
enabled = true                   # 总开关。关掉后完全不记录 Episode、不生成候选
use_experiences = true           # 是否把历史经验纳入提取上下文
suggest_rules = true             # 是否产出 Rule 候选
suggest_skills = true            # 是否产出 Skill 候选
auto_activate_rules = false      # **默认 false，且不建议打开**：Rule 会进 system prompt，
                                 # 自动激活等于让模型自己改自己的行为约束
auto_install_skills = false      # 同上，Skill 自动安装默认关闭
allow_self_code_experiments = false  # 是否允许对自身代码做实验性修改，默认关闭
exclude_external_context = true  # 是否把 Web/MCP 等外部上下文排除在演化证据外（默认排除：
                                 # 外部内容不是用户意图的证据，混进来会污染偏好学习）
infer_feedback = true            # 是否从后续对话推断隐式反馈（用户重述/修正即负反馈）
skill_candidate_min_successes = 3    # Skill 候选所需的最少成功次数
skill_candidate_min_sessions = 2     # Skill 候选所需的最少独立会话数
idle_delay_secs = 300            # 空闲多久后才跑后台演化（秒）；避免和前台回合抢模型配额
max_background_workers = 1       # 后台演化并发 worker 数
max_context_bytes = 8000         # 单次演化提取喂给模型的上下文字节上限
max_daily_tokens = 50000         # 演化每日 token 预算（硬封顶，超了当天停跑）
max_daily_wall_secs = 1800       # 演化每日墙钟预算（秒），默认 30 分钟
max_project_store_bytes = 104857600  # 单项目演化数据目录上限（字节），默认 100 MiB，超了走 retention
evolution_profile = ""           # 演化调用用哪个 Profile，留空 = 复用当前激活分组
[evolution.retention]            # 不同证据类型有效期不同，避免一个统一 TTL 同时误删偏好、放着事实陈旧
repository_fact_stale_days = 28  # 仓库事实（模块职责、构建命令）多久后算陈旧
workflow_stale_days = 60         # 工作流记忆的陈旧期
user_preference_review_days = 180    # 用户偏好多久后进入复审（不直接删）
failed_episode_days = 30         # 失败 Episode 保留期
candidate_days = 90              # 未处理的 Rule/Skill 候选保留期
audit_days = 180                 # 审计记录保留期

[persist_cap]                    # 落盘 / 运行时 / resume 三条路径共用的 ContentBlock 字节上限
tool_result_head_bytes = 20480   # 工具结果 head 上限，0 = 不截断
tool_result_tail_bytes = 10240   # 工具结果 tail 上限，0 = 不截断
thinking_bytes = 0               # **已废弃，不再生效**（保留仅为兼容旧配置）。
                                 # 原按此截断 thinking，但协议要求续轮时原样回传 thinking 块，
                                 # 截断文本保留 signature 会让签名校验失配且不可逆
reasoning_details_bytes = 0      # **已废弃，不再生效**，理由同上
tool_use_input_bytes = 65536     # ToolUse.input JSON 字符串上限，0 = 不截断
text_head_bytes = 16384          # 纯文本 ContentBlock 的 head 上限，0 = 不截断
text_tail_bytes = 8192           # 纯文本 ContentBlock 的 tail 上限，0 = 不截断

[storage]                        # ~/.wyj-code 各子系统 retention / cap / rotation（0 = 关闭该项清理）
checkpoints_per_session = 20     # 单会话 checkpoint 上限。超限按时间淘汰最旧的
memory_v2_md_per_kind = 200      # Memory v2 每类 .md 文件条数上限
memory_v3_records_max = 5000     # Memory v3 records.json 单文件条目上限。Superseded 永远保留，
                                 # 只对 Active/Pending 按「写入新鲜度 0.30 + 使用强度 0.70」淘汰
memory_v3_jobs_max = 32          # Memory v3 pending 任务队列上限
memory_v3_rejected_history_max = 500  # rejected_history.json 条数上限（防止反复提议已拒绝项的那份指纹表）
plugin_gc_interval_days = 7      # 插件 .git 做 `git gc --prune=now` 的间隔天数
workspace_worktree_max_age_days = 30  # workspace worktree 自动 prune 的过期天数
disk_usage_warn_bytes = 5368709120    # 启动期占用告警阈值（默认 5 GiB）
cas_max_blob_bytes = 16777216    # CAS 单 blob 上限（默认 16 MiB），超此值不进 CAS 直接 inline
cas_total_bytes = 1073741824     # CAS 总量上限（默认 1 GiB），启动时按 LRU 回收 ref_count == 0 的 blob
cas_gc_on_start = true           # 启动时是否跑一次 CAS gc。blob 很多时是毫秒级全池遍历，可关
sessions_ttl_days = 0            # **默认 0 = 不自动删**。删历史不可逆，不该在升级后第一次启动里
                                 # 静默发生。想启用设 30/90，或手动跑 `wyj-code storage prune --yes`
                                 # （它读同一组配置；dry-run 与 --yes 用同一个值）
sessions_keep_recent = 5         # 每个项目无条件保留最新 N 条。TTL 的必要配套：纯 TTL 会把
                                 # 「三个月没动的项目里唯一那批会话」删光，而那恰恰最想回看
stale_lock_ttl_days = 7          # 删 cas/locks/ 与 sessions/*.lock 里无人持有且超期的
                                 # （内核会释放 flock，但文件本身不会被删——实测能攒到上万个小文件）
scope = "home"                   # 运行状态落盘范围：home（默认，~/.wyj-code/）/ project
                                 # （会话与历史跟着工作副本走，见上方「落盘范围」节）

# [sandbox]                      # **已整体移除**（crates/sandbox 已删除）。整块运行时被忽略，
                                 # 读取后会在 Config::load 里打印一次 deprecation 警告。
                                 # 旧文件里的 enabled / filesystem / network / environment 等字段
                                 # 不会导致解析失败，但改它们没有任何效果。Bash 直接继承宿主环境。
                                 # 删掉这一块即可消除警告。

[[mcp_servers]]
name = "my-server"               # 服务名（日志区分用；项目级同名覆盖全局）
transport = "stdio"              # stdio / streamable_http（旧写法 "http" 仍被接受）
command = "/path/to/server"      # stdio：可执行命令
args = ["--flag"]                # stdio：参数列表
# env = { KEY = "VALUE" }        # stdio：附加环境变量
# url = "https://..."            # streamable_http：远端端点
# headers = { Authorization = "Bearer ${MY_TOKEN}" }  # streamable_http：请求头，
                                                   # 值支持 ${ENV_VAR} 引用（Key 不落盘）
```

### 环境变量总表

配置项优先从文件读；下面这些是**只在运行时生效、绝不写回 `config.toml`** 的进程级开关（`runtime_*`
槽位带 `#[serde(skip)]`，正是为了「打开设置面板保存」不会把 secret 物化进文件）。

| 变量 | 作用 | 默认 / 备注 |
|---|---|---|
| `WYJ_CODE_API_KEY` | 模型 API Key，**最推荐**的填法 | 覆盖激活分组的 key；不落盘 |
| `WYJ_CODE_SEARCH_API_KEY` | WebSearch 后端 Key | 未配置则**不注册** WebSearch 工具 |
| `WYJ_CODE_PROBE_API_KEY` | `model doctor --probe` 专用 Key | 刻意不复用配置里的 Key：探针要多发请求，用户需自行轮换 |
| `WYJ_CODE_HOME` | 数据根目录 | 默认 `~/.wyj-code`；外置盘 / 独立测试环境用 |
| `WYJ_CODE_THINKING_EFFORT` | 思考档位（low/medium/high/xhigh/max/off） | 优先级最高：覆盖会话级 `/effort` 与 profile 字段 |
| `WYJ_CODE_THINKING_SWITCH` | OpenAI 线 `thinking.type` | enabled/disabled/adaptive；覆盖 profile |
| `WYJ_CODE_MAX_TOOLS_PER_TURN` | 每轮最多并行工具数 | 覆盖 profile；用于放开被静态目录误判成 1 的端点 |
| `WYJ_CODE_NOTIFY_OFF` | 通知总开关 | `1` = 全关（最高优先级） |
| `WYJ_CODE_NOTIFY_BELL` | 覆盖 `[notify.bell].enabled` | `0`/`1` |
| `WYJ_CODE_NOTIFY_DESKTOP` | 覆盖 `[notify.desktop].enabled` | `0`/`1` |
| `WYJ_CODE_SKILL_DATA` | 项目级 skill 的**可写产物根** | 前台 / 后台 Bash 都会注入；目录不预创建，需要时自己 `mkdir -p` |
| `WYJ_CODE_STREAM_IDLE_SECS` | SSE 流空闲多久判定卡死 | 默认值见 `api::stream_guard`；`0` = 关闭看门狗 |
| `WYJ_CODE_MCP_CALL_SECS` | 单次 MCP 调用超时 | `0` = 关闭 |
| `WYJ_CODE_TURN_STALL_SECS` | TUI 回合停滞看门狗 | `0` = 关闭 |
| `WYJ_CODE_UPDATE_GOLDEN` | 重新生成 TUI 快照基线 | 仅开发用（`=1 cargo test -p wyj-tui golden`） |

> **哪些不该写进环境变量**：`context_window`、`max_turns` 这类「结构性参数」没有 env 覆盖——它们是
> 配置的一部分，改它应该改文件并留痕；env 只留给「这次运行换个开关」的场景。



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
    "timeout_ms": 120000
  }
}
```

`verify.command` 让「编辑完就验证」成为闭环：每次 Edit / Write / MultiEdit 成功返回后，Agent 立即跑这条命令，把退出码和截断后的输出作为一条 tool result 回灌给模型，模型据此自己看到编译/测试错误并继续修——而不是把「我改完了，跑一下看看对不对」这件事推给用户。

三条设计约束：
- **零配置成本**：没有声明 `verify.command` 时，`verify.rs` 直接 early-return，**不探测 `npm`/`cargo` 是否存在、不启动任何 shell、不产生任何输出**。绝大多数项目不会配这个键，不配就没有任何代价。
- **执行顺序**：编辑工具成功 → `PostToolUse` hook → 项目 verify。hook 的反馈（格式化、lint 提示）先到，verify 看到的是 hook 已经处理过的工作树。编辑工具**自身失败**时不跑 verify——失败原因已经在工具结果里了，再跑一遍命令只是浪费。
- **有界**：默认超时 120s（`timeout_ms` 可覆盖，单位毫秒），输出按文本预算截断，`sh -c`（Unix）/ `cmd /C`（Windows）执行。实现见 `core::verify`（`load_verify_cfg` / `run_verify`），装配点 `cli/main.rs` 的初次构建与 `/model`·`/profile` 热重建。

### 验收门禁（acceptance）

**项目级**、与 `verify` 同一套 `.wyj-code/settings.json` 体系与合并顺序（全局 → 项目 → `settings.local.json`）：

```json
{
  "acceptance": {
    "max_retries": 2,
    "criteria": [
      { "id": "tests-pass",  "kind": "command",       "command": "cargo test -p wyj-core" },
      { "id": "no-debug",   "kind": "grep_absent",   "pattern": "dbg!\\(", "paths": ["src"] },
      { "id": "changelog",  "kind": "file_contains", "path": "CHANGELOG.md", "needle": "0.3.16" },
      { "id": "new-tests",  "kind": "grep_present",  "pattern": "#\\[test\\]", "paths": ["tests"] }
    ]
  }
}
```

**它改的是什么**：在 `verify` 之前，「任务完成」的定义是**模型说了完成**——模型输出最终文本，回合就结束。这条等式是整条链上最贵的漏洞：模型意识不到功能没有端到端跑通，让它自己评自己的活，它会「自信地夸赞明显平庸的作品」。`acceptance` 把这个定义换成**框架判定**：模型要结束回合时，框架先跑一遍判据；不通过就不让回合结束，把**证据**（不是评语）回灌给模型让它继续修。

四条设计约束：
- **零假设**：没声明 `[acceptance]` 时，`core::acceptance` 完全不参与——不 spawn 子进程、不读文件、不改变任何行为。`enabled: false` 或判据列表为空同样等于没有。`max_retries` 默认 2，即「发现漏改 → 修 → 再验」给两次机会。
- **判据必须是框架能判的**：只收录不需要模型参与的判定（命令退出码、文件存在、文件包含字面量、目录下的正则匹配）。需要语义判定的验收（例如「这个 UI 真的能用吗」）**不在本机制内**——弱验证下加宽候选几乎无用，验收对象与最终验收对象不对齐时 verifier 甚至是负收益。先把能自动判的判准，再考虑让模型去判。
- **信任门控**：`command` 判据会被 `sh -c` 直接执行，与 `verify.command` **同级**的任意命令执行面。项目级的 `[acceptance]` 块因此与 `hooks`/`verify` 一起过 `core::hooks_trust`（`wyj-code trust`）：未批准前整块跳过。指纹覆盖 `hooks`/`verify`/`acceptance` 三个键——既有项目没有 `[acceptance]` 块时指纹逐字节不变（批准记录不受影响），新增该块则需重新批准。
- **有界**：路径一律相对工作目录解析且必须落在工作目录内（逃出去判为无效）；失败详情只回报**文件路径与行号**，不回显匹配到的内容——报告要回灌进模型上下文，不应成为绕过凭据保护的读文件通道。判据本身配错时**恒判不通过**（fail-closed）：一条被静默跳过的判据会让门禁强度打折，比明确失败更危险。

**三条边界**：① **Plan 模式跳过**——计划模式本就不改文件，判据必然失败；② **重试预算耗尽 → 按「未完成」交付**，既不静默放行也不无限重试；③ **声明了但解析失败 → 在收尾时明确报给用户**，绝不静默把门禁关掉。实现见 `core::acceptance`（`load_acceptance_cfg` / `run_acceptance`），装配点 `cli::assembly::SharedAgentConfig::apply`。

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

**`foreground_fallback` 的三档语义**：`disabled`（默认）下旧 `computer` 的变更动作直接返回 `requires_foreground_takeover`；`ask` 每次变更弹一次确认（要求真实 TUI 交互通道，headless / 子 Agent 一律拒绝）；`idle_only` 只按安静期执行。三者都仍受 `InputArbiter` 与前台 PID 校验约束，且都不会让 `app_computer` 的失败静默回退到全局输入。

**权限**：`computer` / `app_computer` 属 `PROJECT_APPROVE_ONCE_TOOLS`——首次按 y/Enter/a 即写入当前项目的 `allowed_tools.json`，同项目后续动作与重开项目都不再确认，不同项目仍隔离，拒绝不落盘。两者**互不隐式授权**：批准后台工具不等于批准前台全局输入。

**诊断**：`/computer` 报告平台/vision/provider 门控结果、AX 授权、InputArbiter 状态、前台回退配置、稳定窗口数量、截图与全局输入探测，以及只存在当前进程内的路径计数与熔断计数（`automatic_foreground_fallbacks` 是显式恒零不变量）。它不依赖工具是否真的注册成功。

### Hooks 自动化系统

**Hooks 自动化系统**（格式对齐真实 Claude Code 的 hooks，路径完全自有）：生命周期钩子配置来源与 AGENTS.md 同一哲学。**v0.2.27 起三源全部迁入 `.wyj-code/` 体系**：`~/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.local.json`（后者追加不覆盖，local 文件供个人临时覆盖、不提交 git）；原三源 `~/.claude/settings.json` + `<git-root>/.claude/settings.json` + `<git-root>/.claude/settings.local.json` **不再被读取**。仅解析 settings.json 的 `hooks` 键，其它顶层键（真 CC 的 `permissions`/`env` 等）宽容忽略。支持 4 个事件：`PreToolUse`（工具执行前，可 block/approve）、`PostToolUse`（工具执行后，可追加反馈）、`UserPromptSubmit`（用户提交后进入推理前，可 block/追加上下文）、`Stop`（回合结束前，可继续下一轮）。每个 hook 是执行任意 shell 的 `command`，stdin 注入 JSON payload（含 session_id/cwd/event/tool_name/input/response），exit 2 表示 block（stderr 为原因），stdout JSON 可表达更丰富的 `decision`/`reason`/`additionalContext`/`continue`。默认超时 60s，可用 `--no-hooks` 完全禁用；首次检测到非空配置时打印一次性安全提示。实现：`core::hooks::HookRunner` 负责加载/合并/执行，`core::agent::Agent` 在 4 个精确插入点调用，`cli/main.rs` 构造并装配给主 Agent 与 TUI 重建路径，子 Agent 工厂不装配。

### 配置备份：`wyj-code export` / `import`

换机器时不必手抄 `[[profiles]]`，也不必逐个重装全局技能：

```bash
wyj-code export backup.wyj          # 已存在则拒绝，加 --force 覆盖
wyj-code import backup.wyj          # 默认 dry-run，只报计划
wyj-code import backup.wyj --yes    # 实际写入
```

`.wyj` 是一个 zip，改名成 `.zip` 就能直接查看。包内结构固定：

```
manifest.json            # 格式版本 / 导出时间 / 主机名 / 是否含明文密钥 / 条目清单
config.profiles.toml     # 只有 active_profile 与 [[profiles]] 两个字段
installed.skills.json    # installed.json 里属于 skill 的那部分（可选）
skills/<name>/...        # 全局 skills/ 整棵树（仅普通文件，符号链接跳过）
```

**装什么、不装什么**：

| 进包 | 不进包 |
|---|---|
| `[[profiles]]` 全部字段（含 `api_key`、`api_key_env`） | `mcp_servers` |
| `active_profile` | `[auto_mode]` / `[evolution]` / `[storage]` / `[tools]` 等全局开关 |
| `~/.wyj-code/skills/` 整棵树 | `sessions/`、`history/`、`memory-v3/`、`evolution/`、`workspaces/`、`cas/` |
| `installed.json` 的 skill 条目（版本/来源/启用状态/`extensions` 索引） | `installed.json` 的 MCP / 插件 / marketplace 源 |

不装全局开关与 MCP 的理由是它们**属于这台机器**：一次「恢复备份」若顺手把
`[auto_mode]` 或 `[storage]` 盖掉，用户的权限档位与自动批准规则会在没有任何提示
的情况下改变，代价远大于收益。

**合并语义**（导入）：

- profile 按 `name` 合并，同名以归档为准；本机独有的分组原样保留。
- `active_profile` 只在两种情况跟随归档：该分组本机原本就有，或本机仍是空白配置
  （只有一个自动补的空壳 `default`）。否则保持本机现值——不让一次导入静默切换
  当前会话的端点。
- skill 目录**已存在就整个跳过并报告**，绝不覆盖。用户手改过的 `SKILL.md` 是
  用户资产，需要更新就自己删掉再导。
- `installed.json` 的 skill 条目按 `name` union，同名以归档为准；其它数组不动。

**明文密钥**：归档默认含 `api_key`（换机后开箱即用靠它），写盘后强制 `0600`，
导出成功时会单独打一段警告。导入前会先读 `manifest.json` 校验格式版本，版本
不认识就拒绝，而不是按老规则硬解析导致新字段被静默丢弃。

实现：数据层 `wyj_store::bundle`（合并规则、归档格式）、zip 安全校验
`wyj_store::archive`（与 `skillhub` 共用，拒绝路径穿越/绝对路径/符号链接）、
profile 片段 `wyj_config::backup::ProfilesFragment`、CLI 出口
`crates/cli/src/backup_cmd.rs`。

### /config 面板

**`/config`**：TUI 内 `/config` 打开设置面板（`OpenSettingsDialog`），现仅剩 `log_level`/`language` 两个字段（`SETTINGS_FIELD_COUNT = 2`，`crates/tui/src/app.rs`）；调用相关字段（`provider`/`model`/`base_url`/`api_key`/`plan_model`/`exec_model`/`aux_model`）已迁移到 `/profile` 的 Profile 分组管理器（`ProfileDialog`），按具名 Profile 管理，不再挂在 `/config` 下。`language` 留空（或空串）则回退到自动检测系统 locale（`LANG`/`LC_ALL`），检测不到则用英文；解析统一走 `wyj_i18n::resolve_locale`。

`language` 同时决定**模型输出语言**，不只是 UI 文案：解析结果经 `prompts::EnvInfo::collect` 进入 system prompt 的 `<env>` 块（`Response language: Chinese (zh)`），并由 `prompts::sub_agent_language_section` 追加给子 Agent（子 Agent 的 system prompt 是整体替换，拿不到 `<env>` 块）。i18n 本身仍只覆盖用户可见 UI 文案（TUI 对话框、slash 命令输出、CLI --help/--config-status），模型侧提示词（system prompt、工具描述等）为英文常量不走 i18n（见上方"模型侧提示词"节），工具内部错误消息等仍为中文，待后续阶段迁移。
