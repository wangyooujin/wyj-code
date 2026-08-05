# Changelog

本文件记录 wyj-code 各版本的主要变更，按版本从新到旧排列。

## [Unreleased]

- **Release CI 可恢复发布**：Release workflow 现在支持从默认分支手动选择并严格校验既有 annotated tag，checkout、tag dereference 与 `HEAD` 必须一致后才允许测试和打包；用于修复 CI 基础设施时无需移动已公开 tag，也不会把 tag 之后的产品代码混入旧版本资产。
- **Linux sandbox 发布门禁**：GitHub Ubuntu Runner 显式安装 bubblewrap；若 Ubuntu 24.04 AppArmor 启用了 `kernel.apparmor_restrict_unprivileged_userns`，只在临时 Runner 内解除限制并执行 bwrap 预检，确保 Linux 环境隔离测试验证真实 sandbox，而不是因 runner 缺少依赖或 namespace 权限误失败。

## [1.5.5] - 2026-08-04

- **证据化本地自进化 L0-L3**：每个用户目标落盘为独立 Episode，并按用户反馈、确定性测试、Review、工具结果和取消状态形成可审计 outcome；从成功/失败证据提炼带 scope、citation、TTL、冲突和当前分支验证的 Memory v2，按当前目标相关性和 8KB 预算选择性注入。Web/MCP/ToolSearch Episode 默认隔离，只有显式 include 后才允许形成仓库级候选。
- **Rule / Skill 候选与人工治理**：重复工作流和失败模式可生成 Rule/Skill 候选，Skill 自动构造直接、间接、信息不全、负向和安全边界共至少 8 个结构化 eval，并展示历史成功 Episode/Session 证据；当前不执行逐例 Agent replay、安装前后成功率对照或完整 benchmark。Rule 和 Skill 均不会自动激活。新增 `/evolve` 四视图治理中心与 `wyj-code evolve {status,list,review,feedback,skillize,approve,reject,rollback,forget,run,include,migrate,export,doctor}`，批准 Skill 前创建保护 checkpoint，项目/全局安装通过原子文件与 lockfile 写入，支持跨进程恢复旧内容。
- **本地、限额、可迁移**：Evolution 默认空闲 5 分钟、单 worker、每日 50,000 token / 30 分钟、每项目 100 MB；连续可恢复错误最多三次退避，三次失败后在 Health 暴露。旧 Markdown Memory 先 preview、再原子迁移并保留带时间戳备份；L4 核心代码自修改明确延后 v1.6.0，v1.5.5 不包含无人监督的自改代码。
- **TUI 多图片编辑修复**：输入框内图片占位符按顺序显示为 `[Image #1]` / `[Image #2]`；支持连续粘贴多张不同图片，并可在真实文本起点用 Backspace 从右向左逐个删除图片/文件附件，占位符仍只用于渲染，不会混入发送给模型的正文。
- **TUI 鼠标滚轮不再翻动输入历史**：根因是 Ghostty 在 alternate screen 且应用关闭 mouse capture 时，DEC mode 1007 会把滚轮转译为无修饰 `Up/Down`，随后被 Composer 的 `navigate_input_or_history` 误当真实键盘。Ghostty 直连路径现通过 Kitty 键盘 release/repeat 事件区分两者，滚轮只滚动内容区，真实 `↑/↓` 仍保留输入光标/历史语义；其它终端及 tmux/zellij 路径会成对保存、关闭、恢复 mode 1007，防止伪方向键污染输入。全程仍保持 `DisableMouseCapture`，不回退无需 Shift/Option 的终端原生拖选复制。
- **本地 Bash 环境与联网修复**：sandbox 新增 `network.allow_all` 以及 `environment.inherit/allow/deny` 配置，解决宿主网络正常时 Bash 仍表现为 DNS 解析失败、以及自定义环境变量被 `env_clear()` 无差别清空的问题；默认严格边界不变，启用继承时仍默认隔离 wyj-code 自身 provider/search/probe key。
- **computer-use 跨会话记忆污染修复**：当前请求显式注入真实工具清单并声明其高于历史记忆，`default/bypass` 仅影响审批而不移除 schema；自动记忆提取与解析同时拒绝保存“本轮/本会话缺少 Bash 或 computer-use”等瞬时运行状态。

## [1.5.4]

- **Computer-use 只读权限与工具语义对齐**：统一权限策略现在按 `action` 区分 `computer` / `app_computer` 的只读观察与变更操作。`screenshot`、`zoom`、`cursor_position`、`wait`、`list_windows`、`inspect_element` 可在 headless/daemon 的 Prompt 模式下执行，不再因为缺少交互审批通道被整类工具误拒。
- **变更操作继续失败关闭**：点击、输入、滚动及未知/缺失 action 在无 UI 表面仍拒绝；交互表面仍返回逐调用审批，AutoApprove、Allowlist、Plan、sandbox、foreground compatibility 与项目级授权边界均未扩大。
- **不可移动发布边界**：v1.5.3 已完成五平台 Release、11 个资产和 Pages 发布，因此保持原 tag 不变；该发布后发现的权限前置判断缺口以 v1.5.4 新补丁交付。

## [1.5.3]

- **Computer-use lazy schema 保底**：ToolSearch 在工具集很大时仍保留已注册的 `window_capture`、`app_computer` 与兼容 `computer` schema，避免国内模型把“当前未展示”误判成“本会话不支持 GUI”；工具是否注册、foreground compatibility 开关、权限与 sandbox 限制均不扩张。
- **跨平台 checksum 可移植性**：Windows 打包不再用产生 CRLF 的 `Out-File` 写 sidecar，而是显式写入 ASCII + LF；`SHA256SUMS` 聚合时同时防御性移除行尾 `\r`，因此 Unix `sha256sum/shasum` 不会把 Windows 文件名解析为带回车字符。
- **发布前实物校验**：Publish Release 在上传前对五个平台归档执行 `sha256sum --check SHA256SUMS`，任一 archive、sidecar 或聚合内容不一致都会阻断 Release，不再把 checksum 可下载误当成 checksum 可用。
- **不可移动边界**：v1.5.2 的 Test/Lint、五平台 Build 与 Publish Release 均成功，但发布后实物验收发现 Windows checksum CRLF 缺陷；v1.5.3 以新 tag 修复，所有历史 tag 保持不变。

## [1.5.2]

- **ToolSearch 核心执行面保底**：lazy schema 在大工具集下始终保留 Read/Glob/Grep/CodeSearch、Bash/Edit/Write、AskQuestion/TodoWrite、Agent/ExitPlanMode 等已注册核心工具，只延迟暴露可选集成；避免国内模型只看到搜索入口却看不到完成编码任务所需的执行工具。Plan/只读子 Agent 仍只暴露其实际注册和授权的子集。
- **Rust 1.96 严格门禁兼容**：修复 Rust/Clippy 1.96 新增的 `collapsible_match` 与 `unnecessary_sort_by` 告警，覆盖 Agent/Skill frontmatter、工具参数对象提取、MCP 工具稳定排序和 TUI import 确认路径；行为保持不变。
- **可复现 CI 工具链**：Release 的 Test/Lint 与五平台 Build、Review Action 都固定 Rust `1.96.0`，不再让可变的 `stable` 在 tag 推送后引入未本地复现的新 lint。发布前同时用 Linux Rust 1.96.1 容器执行全 workspace/all-targets 严格 Clippy。
- **不可移动发布边界**：`v1.5.1` 的 workspace tests 通过，但远端 `stable` 已升级到 Clippy 1.96，新增 lint 使其 Release Action 失败且未生成资产；`v1.5.2` 作为新的补丁版本接替发布，`v1.4.4`、`v1.5.0`、`v1.5.1` 均保持不可移动。

## [1.5.1]

- **国内模型多工具调用兼容修复**：保守能力目录仍可声明 `max_tools_per_turn = 1` 和禁止并行，但模型若已经在一个完整响应中返回多个合法 `tool_use`，执行器会按能力声明受控串行执行并逐个回填 `tool_result`，不再把额外调用伪装成参数错误，也不会因连续完整多调用响应误触“参数重试耗尽”。工具原始 schema、权限与 sandbox 校验继续逐项生效。
- **Release CI 跨平台修复**：修复 Linux `detect_backend` 的 `clippy::needless_return`，并按实际使用平台/测试条件编译 computer-use 窗口 generation helper；本机与 Linux/Rust 1.94 的 `cargo clippy --workspace --all-targets --locked -- -D warnings` 均作为发布门禁。
- **发布边界**：`v1.5.0` annotated tag 已公开且其首轮 Release Action 在 Test & Lint 阶段失败，因此保持不可移动；`v1.5.1` 随后也因 CI 的可变 `stable` 升级到 Clippy 1.96 而未生成 Release 资产，最终发布修复迁移到 `v1.5.2`。

## [1.5.0]

- **Workflow 自动隔离编码节点**：`wyj-code workflow validate/run/status/control` 已交付 DAG runtime、并发上限、token budget、human approval、pause/resume/retry/skip/cancel 和持久化状态。拥有 Write/Edit/Bash 且配置 `write_roots` 的 Agent/Review 节点会从当前脏工作区 checkpoint 自动创建独立 managed Git worktree；成功和失败现场都保留，结果返回 diff/review/accept 命令，不自动覆盖父 checkout。
- **Managed Worktree 完整生命周期**：`wyj-code workspace create/list/diff/accept/dispose` 支持 binary-capable diff、遗漏路径提示和选择性接受；接受前防御 symlink 逃逸、父 checkout HEAD 前进、用户并发修改与 binary 漏应用，强制清理必须显式 `--force`。
- **ACP 与全局 daemon session**：新增 stdio ACP adapter 和本地 TCP daemon。daemon 使用进程级 session registry，连接断开不再终止活动 session，新连接可 `session/load` attach，并通过 `_wyj/session/list` / `_wyj/session/control` 跨连接列出、提交、打断、rewind、branch、控制 workflow 或关闭 session；Rewind/Branch 文件恢复先 preview，确认后执行并创建保护 checkpoint。接口 schema 升级为 version 2。
- **前端无关事件流**：统一发出 text/thinking/tool/usage/error/turn finished，以及 PermissionRequested、DiffAvailable、CheckpointChanged、AgentStateChanged，供 ACP/IDE 客户端消费；stdio 连接仍在结束时清理自己的 session，daemon session 则全局存活。
- **CodeIndex 与真实 Plugin LSP 查询**：本地词法/符号索引带 ignore-aware direct-scan fallback；插件 LSP 完成 `Content-Length` framing、initialize/initialized 和 `workspace/symbol`，解析 file URI、symbol kind、container/path/line 后与本地结果合并、去重、排序。LSP 故障保持 fail-soft，不影响本地搜索。
- **Plugin Runtime 事务式激活**：已启用插件可贡献 hooks、output styles、themes、channels、LSP servers、monitors、settings schema 与 userConfig；任一 runtime contribution 无效时整插件回滚，不再留下半激活状态，名称冲突保持先到先得并记录 warning。
- **Review 与执行安全收口**：新增 `wyj-code review run` 和 GitHub Review Action，扫描 rename、空格路径、binary numstat，并对 secret evidence 脱敏；headless REPL 的 `!command` 统一走 SandboxRunner，不再通过 `sh -c` 绕过隔离。Release CI 强制执行 workspace 全量测试与 `clippy -D warnings`。
- **整合 TUI 交互改进**：纳入 Markdown 表格/timeline 物理行网格、终端原生鼠标拖选、OSC 8 文件/网页超链接、稠密渲染、welcome/theme 调整，并保持普通 `↑/↓` 留在 Composer，已退休的 `Shift+↑ content` 文案不再恢复。
- **国内模型边界不夸大**：未读取或使用此前暴露的 MiniMax Key；live probe 仍只接受独立 `WYJ_CODE_PROBE_API_KEY`。MiniMax 和其他无独立轮换 Key 的国内模型继续标记为 `static_only` / protocol-compatible。
- **版本**：工作区版本升级到 `1.5.0`；已发布的 annotated tag `v1.4.4` 保持不可移动。

## [1.4.4]

- **国内模型可信运行时**：新增 vendor / wire protocol 分离的 `ModelCapabilities`、能力来源与置信度、静态模型目录和 TTL cache；覆盖 GLM、MiniMax、Kimi、DeepSeek、Qwen/百炼、豆包/火山，以及 Ollama/vLLM/OpenAI-compatible 兼容端点。`wyj-code model doctor` 与 `/model doctor` 默认只做静态诊断，显式 live probe 只读取独立的 `WYJ_CODE_PROBE_API_KEY`。
- **工具调用不再带病执行**：原始工具参数先经过有限 JSON 语法修复，再按原始 schema 校验；缺少必填字段或语义不明时把精确错误回灌给模型定向重试，重试耗尽后停止，禁止退化为空对象或 `null` 执行。Provider 错误统一分类，安全参数可见降级，同厂商/同角色 fallback 只在完整消息边界和可恢复错误上发生。
- **权限默认失败关闭**：无 UI 的 headless、单次 `-p`、schedule 与 SubAgent 不再把“无法询问”当成批准；Plan 模式只允许在 `doc/plan/**`、`docs/plan/**`、`.wyj-code/plans/**` 写规划文档，额外文档必须逐路径授权，源码、脚本、配置和 Bash 写入绕过继续拒绝。
- **Claude Code 式 OS sandbox**：前台/后台 Bash 和 TUI `!command` 统一进入同一 runner。macOS 使用 Seatbelt，并通过 sandbox 外的 host/port 校验代理执行域名级网络授权；Linux 使用 bubblewrap 文件系统/网络 namespace，域名代理桥接尚不可验证时明确失败关闭。凭证目录默认拒读；只有交互式 TUI 可批准一次性、不可持久化的无隔离降级。
- **效率与恢复基础**：大工具集启用 `ToolSearch` + lazy schema，小工具集保持全量 schema；sticky 生命周期、top-K 与阈值可配置，状态栏和 `WYJ_STATS_JSON` 显示 schema sent/saved。新增 checkpoint、conversation/files/both rewind、session branch，并保留分支血缘和用户真实 Git index。
- **SubAgent 与 schedule 收口**：SubAgentHub 新增 follow-up、interrupt、retry-last、父子元数据和控制事件 trace；follow-up 只在完整模型/工具边界注入。旧 schedule 自动禁用并要求权限复核，TUI 可编辑 allowed tools、write roots、allowed domains 与 require sandbox，复核后仍需用户再次显式启用。
- **Secret 与后续接口**：Profile 支持 `api_key_env`，运行时 Key 不会因设置面板保存而物化进 `config.toml`，doctor/config-status 只显示末尾掩码；冻结 `ExecutionWorkspace`、workflow/DAG、前端无关 `SessionEvent`/ACP 和 `CodeIndex` 的 P2 接口，但不宣称已交付完整 worktree、daemon、workflow 或语义索引。
- **验证状态说明**：本版本没有使用用户此前暴露的 MiniMax Key；MiniMax 与其他未提供轮换 Key 的国内模型保持 `static_only` / protocol-compatible，不宣称 live verified。Linux 域名 allowlist、原生 Windows 同等级 sandbox 仍是明确边界。
- **版本**：工作区版本升级到 `1.4.4`。

## [1.4.2]

- **恢复终端原生鼠标选中**：TUI 启动时显式关闭 mouse capture，聊天内容可直接用鼠标拖选复制，不再要求 Shift/Option；松开修饰键后应用也不会立即用鼠标事件冲掉选区。应用内历史继续通过 PageUp/PageDown 等键盘入口浏览，OSC 8 文件与网页链接仍由终端原生 Command/Ctrl+点击打开。
- **内容区连续键盘导航**：普通 `↑/↓` 始终保留给输入框和输入历史；Todo 与 SubAgent 支持键盘选择和单任务详情逐行阅读，`Esc` 按详情 → 列表 → 输入框逐级返回并保留草稿。
- **Codex 风格静态执行流**：移除聊天消息选中、高亮、`▶`、展开/折叠、详情滚动和 `Ctrl+O`；Thinking、ToolResult 与 BashOutput 标题下最多展示 3 个终端视觉行，超出以 ASCII `...` 收束，用户输入和 AI 最终回答保持完整展示并自然换行。
- **Edit/Write 自动 diff 预览**：编辑和写入结果直接展示红色删除、绿色新增、灰色上下文，不再需要手动展开；长 diff 同样遵守三视觉行上限，历史会话恢复后仍保留工具名并正确识别 diff。
- **稳定的尾部跟随语义**：默认视口跟随内容最后一行；用户主动上滚后保持阅读位置并显示新消息提示，流式 token、工具结果和新消息不会抢走视口，滚回底部后自动恢复跟随。
- 中英文 `/help` 与输入框快捷键提示同步新交互，并补齐三行预览、长单行换行、diff 配色、跨区域导航和上滚新消息等回归测试。
- **版本**：工作区版本升级到 `1.4.2`。

## [1.4.1]

- **TUI 剪贴板与附件体验修复**：Ctrl/Command+V 会主动读取系统剪贴板，纯图片剪贴板不再因为终端没有产生 bracketed-paste 事件而无法粘贴；文字、图片与文件路径统一走同一条处理链，配置类面板借用输入框时粘贴内容不会再穿透到聊天输入框。
- 图片与文件改为在输入框内持久显示 `[Image]` / `[File: name]` 占位符，不再额外占用一整条附件面板；占位符只参与渲染，不会混入发送给模型的正文。支持只发送附件，ESC/Ctrl+C 会一次清空文字与待发送附件，并避免重复附加同一图片或文件。
- **修复旧版前台 `computer` 连续动作误报 `target_changed`**：前台窗口识别改为保留系统 z-order，不再从展示排序后的同 App 多窗口中任取一个；截图观察在真实前台窗口未变化时可跨多次动作复用，因此“点击输入框 → 立即输入”不再被错误拦截，原有约 20 次动作上限与每次动作前的窗口身份复核仍保留。
- 强化第三方模型 computer-use 指引：精确文字、配置与诊断内容必须由工具结果或 zoom 裁剪证实；AXPress 不支持按固定控件能力处理，不盲目重试；切换到无目标绑定的旧 `computer` 前必须重新确认目标 App 位于最前台。
- **版本**：工作区版本升级到 `1.4.1`。

## [1.4.0]

- **v1.4 computer-use 人机互不干扰架构**：默认改为稳定窗口目标 + macOS Accessibility/目标 PID 后台动作，不移动物理光标、不主动切换前台 App；旧全局 `computer` 降级为默认关闭的 foreground compatibility 工具，禁止从后台失败静默回退。
- 新增 marker-based `InputArbiter`：精确排除自身合成事件，前台租约被人类输入立即撤销；后台动作按事件类型与目标窗口区域识别冲突，因此用户可持续在其它 App 输入/移动鼠标，只有碰到 Agent 目标窗口时才抢占。Event Tap、权限、锁屏或事件历史异常时失败关闭。
- 新增 `window_capture list/capture`、`app_computer`、结构化安全错误、前台 PID 前后校验与会话内不兼容动作熔断；删除动作级“检测到用户输入，是否继续”暂停弹窗，headless/cron/子 Agent 无条件禁止旧前台接管。
- `/computer` 扩展为完整只读诊断：AX/Input Monitoring、稳定窗口枚举、前台回退配置和本地路径/抢占/熔断计数，其中自动前台回退计数是恒零安全不变量。
- computer-use 权限确认改为**项目级首次批准即记住**：`computer`/`app_computer` 首次按 y、Enter 或 a 后分别写入当前项目的 `allowed_tools.json`，同项目后续动作及重新打开项目不再弹窗，不同项目仍需独立确认；拒绝不落盘，普通工具的“允许一次”语义不变。
- 项目 `.wyj-code/` 资源统一按 Git 仓库根自动发现：从任意子目录启动都能加载根目录的 `settings.toml`、`mcp.toml`、`skills/`、`agents/` 和 `installed.json`；Skill 同时支持 `name.md` 与标准 `name/SKILL.md`，目录式 Skill 内的 references/assets Markdown 不会误注册为额外命令。
- 新增**项目级 MCP server 信任确认**：`.wyj-code/mcp.toml`/`.mcp.json` 里的 server 会被当子进程执行，随仓库 clone 落地即可能静默跑任意命令，因此改为按内容指纹首次需人工批准；批准记录落在仓库控制不到的 `~/.wyj-code/projects/<key>/`，TUI 面板确认，`wyj-code trust-mcp` 提供无 UI 场景下的手动批准入口，`-p`/headless/`schedule run` 未批准时静默跳过并提示。
- `.wyj-code/settings.toml` 新增 `disabled_skills`/`disabled_mcp_servers`，按名字禁用 Skill/MCP（不限来源，覆盖六层合并链任意一层），与 lockfile 的 `enabled: false`（仅覆盖 `/extensions install` 装入的条目）互补。

## [1.3.3]

- **新增 `/schedule` 定时任务系统**（TUI 面板 + CLI `wyj-code schedule {list,add,remove,enable,disable,sync,run}`，详见 `doc/plan/v1.3.3-plan.md`）：
  - 一句话 prompt 或"把当前对话固化为模板"即可生成到点自动执行的任务；wyj-code 本身没有常驻后台进程，定时能力完全依赖系统级 `crontab`（v1 仅 macOS/Linux）唤起 headless 执行，`wyj-code schedule run <id>` 以子进程方式调用自身 `-p "<prompt>" --cwd <dir>` 入口，与手动配置 crontab 完全等价。
  - 面板保存后立即自动同步进系统 crontab，只替换 `# BEGIN/END wyj-code schedule` 标记的区块，不触碰用户其他 cron 条目，首次同步前自动备份原始 crontab。
  - 每个任务独立绑定工作目录；失败不重试，记录失败原因，可选 macOS 系统通知；跨天业务状态（如候选池）由任务 prompt 自行读写文件，框架不做业务状态管理。
- **新增 `/import` 一键导入 Codex / Claude Code 配置**（TUI 面板 + CLI `wyj-code extensions migrate --from codex|claude|all [--dry-run]`，底层共用 `wyj-store::import` 的 scan/apply）：
  - 扫描来源：Codex `~/.codex/config.toml` 的 `[mcp_servers.*]`（新增 TOML 解析器，未知字段宽容忽略）与 `~/.codex/prompts/*.md`；Claude Code `~/.claude.json`/`.mcp.json` 的 `mcpServers`、`~/.claude/commands`/`.claude/commands`（递归保留 namespace）、`~/.claude/agents`/`.claude/agents`。
  - TUI 交互：列表标注来源/scope/冲突/遮蔽，默认勾选全部无冲突项，Space 勾选、`a` 全选/清空、Enter 写入并展示结果报告；来源文件永远只读保留。
  - 幂等与冲突语义：与目标内容完全相同的候选不再列出（重复运行列表自然收敛）；同名不同内容标 conflict、默认不勾选、勾选即覆盖（CLI 非交互一律跳过冲突项并列出）。
  - 遮蔽提示：从 Claude commands/agents 目录导入的副本在原文件删除前被在线原文件遮蔽（合并链真 CC 路径优先），报告逐条提示。
- **BREAKING：项目级配置目录从 `.wyj/` 更名为 `./.wyj-code/`**（与全局 `~/.wyj-code/` 命名对称），承载 `skills/`、`agents/`、`mcp.toml`、`installed.json`。**不做旧目录兼容读取**——已有项目里的 `.wyj/` 配置会静默失效，手工执行 `mv .wyj .wyj-code` 即可迁移。路径拼接统一收敛到 `wyj_config::project_config_dir(cwd)` / `global_config_dir_in(home)` 两个辅助函数，消灭各 crate 内联硬编码。
- **子 Agent 定义新增 wyj 自有目录加载源**：`load_agent_defs` 扩成六层链 `内置 → ~/.wyj-code/agents → ~/.claude/agents → 插件 → ./.wyj-code/agents → .claude/agents`，与 skill 链哲学对齐，`/import` 导入的 agent 定义落在 wyj 目录。
- **UI：列表选中态背景色统一为深灰**：Todo 列表、子 Agent 面板、会话选择器、斜杠命令补全此前用饱和蓝色背景，Profile/Mcp/Skills/Plugins/Extensions/Import/Schedule/Agents 等管理面板此前完全没有背景色（只靠文字加粗+箭头），两套不一致的视觉语言统一收敛为同一个 `Theme::selected_row()`（深灰背景 + 品牌橙文字 + 加粗），更方便一眼辨识当前选中项。
- **修复 Todo / 子 Agent 详情面板滚动上限计算错误**：详情内容较短、完全在可视区域内时，PageUp/PageDown 此前会被面板滚动逻辑吞掉而不穿透到聊天区；根因是详情渲染函数未把实际内容行数回传给滚动上限状态，现在改为返回 `(scroll, max_scroll)` 二元组即时同步。
- **修复 `extensions migrate` 冲突检测 bug**：旧实现用 `Config::load()`（会合并 `~/.claude.json`）做去重，导致全局原生 server 全量误报 skipped、且合并结果被误物化写进 config.toml；新实现改用 `Config::load_file_only()` 裸读 config.toml 做冲突检测与写回。
- **修复 TUI 里 `/extensions` 不可用**：`ExtensionsCmd` 此前只注册在 `standard_registry()`，TUI 实际使用的 `standard_registry_with_skills()` 漏注册。

## [1.3.0]

- **Computer-use（桌面 GUI 控制，macOS/Windows）**：对接 Anthropic 原生 `computer_20251124` 工具——模型截图观察桌面、合成鼠标点击/拖拽/滚动与键盘输入操控本机 GUI。
  - 新增 `crates/computer`（`wyj-computer`）：`xcap` 截图 + `enigo` 输入合成（两者内部已各自处理 macOS/Windows 差异，无需再手写一套平台分支），坐标缩放数学独立成模块、平台无关可测；仅 macOS/Windows 拉取真实依赖，其余平台编译进桩实现，Linux 首版不支持。
  - `wyj_api::types::ToolDefinition` 新增 `native: Option<NativeToolSpec>`：为 `Some` 时 provider 层按 Anthropic 原生工具格式（`{type, name, ...extra}`，无 description/input_schema）序列化并自动追加所需 `anthropic-beta` header；OpenAI 供应商防御性跳过原生工具。
  - **双模式，兼容 MiniMax/GLM/Kimi 等国内 Anthropic 协议兼容端点**：官方 api.anthropic.com 用原生 `computer_20251124` 工具（体验最优，依赖 Claude 内置训练的调用约定）；第三方 Anthropic 协议兼容端点（`provider = "anthropic"` + 自定义 `base_url`，如 MiniMax）自动退化为普通 custom 工具（带完整 description + input_schema，动态嵌入实际截图分辨率），任何具备基本工具调用能力的模型都能正常使用，不再因为发了无 schema 的原生工具类型而 400 或不可用。
  - 权限模型：截图/查光标/等待只读放行；点击/拖拽/按键/输入/滚动等变更类动作逐个走既有确认弹窗。「始终允许」对 computer 只在**当前会话内存**放行，不写入跨会话的 `allowed_tools.json`（整机控制权限风险面与 Bash/Edit 不对等）。
  - 安全兜底：鼠标物理坐标落入屏幕任一角落附近时视为「失控角」信号，立即中止变更动作；连续变更动作数超过阈值仍未截图核实进度时自动暂停并提示模型截图或停下确认。
  - 注册门控：仅当平台支持 + 当前 Profile `vision=true` + provider 为 Anthropic（Messages API 协议本身才支持 tool_result 内嵌图片回传，OpenAI Chat Completions 的 tool 消息不支持图片）时注册；子 Agent 不注册（与 `Agent`/`AskQuestion` 一致）。
  - **新增系统提示**：computer-use 注册成功时自动追加一段使用说明，教模型"打开应用优先用 Bash 直接启动（如 macOS `open -a`），不要在 GUI 里瞎找"，以及"变更动作已有逐次确认弹窗、不必先在聊天里问用户'允许'"——此前模型（尤其第三方模型）截一张空桌面的图就直接放弃，转而等用户手动打开软件或在聊天里明确说"允许"。
  - **新增 `zoom` 动作，提升识别准确率**：全屏截图会下采样，密集数字表格/小字容易被模型看错或看不清。`zoom` 裁剪一块区域后尽量不下采样地重新编码，有效分辨率远高于同一块内容在全屏缩略图里的样子。参考 2025-2026 GUI agent 研究（动态裁剪放大可带来两位数百分点的识别准确率提升）与 Anthropic 官方指导（按需请求细节而非一味提高全屏分辨率），系统提示与 custom 工具描述都会提醒模型"读数字前先 zoom，别猜"。只读动作，无需权限确认，和 `screenshot` 一样会重置连续动作计数。
  - **新增点击类动作的修饰键支持**：`left_click`/`right_click`/`middle_click`/`double_click` 现支持通过 `text` 字段传入要按住的修饰键组合（如 `"shift"`、`"cmd"`，语法与 `key` 动作一致），对齐官方调用约定，支持 shift-click 多选、cmd-click 等场景；`wyj-computer` 新增 `key_down`/`key_up` 系统层原语，点击失败时仍保证修饰键被释放，不残留状态。
  - **新增 `/computer` 诊断命令**：只读展示 computer-use 是否受当前平台/Profile 支持、原生还是 custom 模式、主屏物理/目标分辨率，并做一次真实截图 + 光标读取自检，附带 macOS「辅助功能」权限的静默失效提醒（未授权时点击/按键常被系统静默丢弃且不报错，读光标位置成功不代表输入真的生效）。
  - **修复模型误拒"帮我看看某 App 里的消息"类请求**：用户反馈让模型打开聊天软件看某联系人发的消息时，模型以"需要调用该软件 API"+"不该看你的隐私"为由拒绝并让用户自己去看——两点都站不住脚：截图/`zoom` 读的是屏幕渲染内容，不需要任何 API；这是用户自己的设备和已登录账号，用户本人直接发起的请求，没有第三方隐私可言。系统提示与 custom 工具描述都补充了这段说明，模型现在会把这类请求当成普通任务直接执行。
  - 已知限制：`provider = "openai"` 的 MiniMax/DeepSeek 等配置暂不支持 computer-use（截图无法以图片形式回传，见上）；终端 TUI 无法渲染截图像素（纯文本终端），仅模型可见画面；scroll 的像素步长部分按键组合语义、以及 TCC/DPI 主动引导授权（区别于 `/computer` 的按需诊断）待真机手测校准。
- **TUI 主界面改为永久 Fullscreen，输入框永远贴住窗口底部**：此前聊天区按内容动态定高（`Viewport::Inline`），内容较短时输入框下方会留一段正常但显眼的终端空白。现在主循环全程运行在 `Viewport::Fullscreen` + alternate screen，聊天区自动撑满可用空间、输入框/状态栏贴着窗口最底部，不再有这块空白。
  - **有意的取舍**：为了让鼠标滚轮驱动应用内翻页，同时开启了 `EnableMouseCapture`，代价是终端原生鼠标选中/拖拽复制聊天记录、终端原生 scrollback 缓冲区不再可用（多数终端可用 Option/Shift+拖拽 强制原生选中作为退路）；改为应用内滚动——PageUp/PageDown、鼠标滚轮、Ctrl+O 展开单条消息，历史消息永远留在应用状态里，理论上不会丢。复制最后一条 AI 回复用 Ctrl+Y（不受影响）。
  - **技术依据**：此前两次尝试在 `Viewport::Inline` 模式下"撑满终端高度"实现贴底（`b5729c5` 与本版本内一次收窄重试）都在真实终端上复现了画面撕裂/输入不可见，根因是 Inline 构造/resize 依赖的终端光标位置查询在部分终端下存在竞态。`Viewport::Fullscreen` 的构造路径不查询光标位置，结构上避开了这个问题。

## [1.2.2]

- **统一 Extensions 资源平台**：新增 `wyj-code extensions` CLI 和 `/extensions` 入口，统一查看、诊断、迁移、安装、升级、启用、禁用和卸载 Skill / MCP / Plugin。
- **运行时热应用**：MCP 连接、插件 Agent 定义和工具快照在安全 Agent 回合边界原子更新；禁用/卸载后旧工具不会继续暴露给下一回合。
- **统一 Extensions TUI**：`/extensions` 提供列表、详情、启用、禁用、卸载和刷新操作，headless/CLI 继续提供稳定 JSON 输出。
- **安装可靠性与锁定**：lockfile/config 原子替换，Skill/MCP 写入失败回滚；插件依赖支持递归安装、循环检测和 semver 约束，记录 Git commit/MCP 包描述 digest。
- **lockfile v2**：保留旧字段兼容，同时新增跨类型 `extensions` 索引；安装流程会记录统一资源条目。
- **Claude MCP 兼容**：运行时读取项目 `.mcp.json` 和全局 `~/.claude.json` 的 `mcpServers`；支持显式迁移到 wyj-code 配置，原始文件保留。
- **MCP 传输扩展**：支持 stdio 与 Streamable HTTP，远程配置支持 URL、环境变量引用 header，工具名稳定映射为 `mcp__server__tool`。
- **Skill 命名空间与热发现**：递归加载 `.claude/commands/<namespace>/<name>.md`，映射为 `/namespace:name`；每个 slash 命令边界重新发现 Skill/Plugin 命令。
- **修复长内容被视口遮挡（可见性优先冻结）**：长 markdown 正文/工具流此前会被「最后可折叠 ToolResult」的冻结封顶困在 Inline viewport 待定尾部，超出可视高度（终端高 70%）的部分既不在屏幕也不在终端 scrollback、彻底无法查看。现在待定尾部一旦超过可视上限即豁免封顶，内容冻结进 scrollback 用鼠标滚轮完整回看；AskQuestion 面板打开期间同样豁免，且打开时强制视口贴底并清掉选中锚点，保证选项区立即可见。MCP/JSON 工具结果的 `⎿` 摘要行不再显示无信息量的 `{`，改取第一条有内容的行。
- **上下文压缩可靠性**：工具调用密集的单回合不会再产生空消息压缩；完整请求预算纳入系统提示、工具 schema、消息开销与输出预留，UTF-8 截断安全，记忆按反馈、用户、项目、参考资料的优先级注入。
- **精确 Token 账务**：MiniMax、GLM 与 DeepSeek 的已完成请求优先采用供应商响应中的 `usage` 精确计数；OpenAI 兼容流自动请求并解析流式 usage，Anthropic 兼容流兼容 `message_start` / `message_delta`。发送前的上下文保护仍使用保守估算。
- **版本**：工作区版本升级到 `1.2.2`。

## [1.2.0]

- **自定义 slash 命令对齐真实 Claude Code**：Skill 系统扩展为同时识别真 CC 的
  `~/.claude/commands/*.md`（全局）与 `.claude/commands/*.md`（项目），六层合并链下同作用域内
  真 CC 路径覆盖 wyj-code 自造的 `~/.wyj-code/skills`/`.wyj/skills`。
  - frontmatter 新增结构化字段：`description`（覆盖默认取正文标题的行为）、`argument-hint`
    （影响 `/help` 里展示的用法提示）、`allowed-tools`（该命令执行期间临时把工具白名单收紧为
    `Allowlist`，跑完自动还原，ESC 中断也能正确还原）、`model`（本版本仅解析存储，运行期切换
    Profile 暂不生效，留待后续版本）。
  - `/help` 输出末尾新增「自定义命令」动态分组，展示当前发现的全部 Skill / 自定义命令。
  - frontmatter 解析器抽取为 `core::frontmatter`，与 `~/.claude/agents/*.md` 的解析逻辑共用。
- **性能实测与依赖排查**：release 二进制体积 12MB、稳态冷启动 ~10ms，实测数据证实现有构建配置
  （`opt-level=3`/`lto=thin`/`codegen-units=1`/`strip=true` + rustls）已经足够精简；`cargo tree
  --duplicates` 排查出的多版本依赖逐条记录了可否收敛的结论（详见 README「性能」章节）。
- **稳定性补强**：`crates/cli`/`crates/i18n`/`crates/mcp` 补充基础冒烟测试（此前零覆盖）。
- **预编译压缩包新增一键安装脚本**：`install.sh`（macOS/Linux）与 `install.bat`（Windows）随
  GitHub Release 压缩包分发，解压后运行即可把二进制装到当前用户目录（`~/.local/bin` /
  `%USERPROFILE%\.wyj-code\bin`）并自动配置 PATH，全程无需 sudo/管理员权限；重复运行幂等，
  不会重复追加 PATH 配置。

## [1.1.0]

- **新增 Hooks 生命周期自动化系统**：支持在 `.claude/settings.json`（用户级 → 项目级 →
  `settings.local.json` 三源合并）声明 shell hook，在 `PreToolUse` / `PostToolUse` /
  `UserPromptSubmit` / `Stop` 四个生命周期节点触发，可用于拦截危险命令、保存即格式化、
  注入上下文、回合结束通知等自动化场景，行为对齐真实 Claude Code。
  - 新增 `/hooks` 命令列出当前生效的 Hooks 配置。
  - 新增 `--no-hooks` CLI 开关全局禁用。
  - 首次检测到非空 Hooks 配置时打印一次性安全提示。
  - 子 Agent 不装配 Hooks，避免嵌套子任务触发用户级自动化。
- **开源产品化基线**：新增 `LICENSE-MIT` / `LICENSE-APACHE`、`CONTRIBUTING.md`、
  `CHANGELOG.md`；README 补充 Hooks 特性说明、安装方式（GitHub Releases 下载 /
  `wyj-code update` 自更新 / 源码构建）与已知限制章节。
- 明确记录 TUI Inline viewport 输入框贴底问题的调查结论：确认为 ratatui/crossterm 层面
  限制而非本项目代码可修的 bug，本版本不做改动，保留动态定高方案（见 README「已知限制」）。

## [1.0.2]

- 修复工具结果展开正文与摘要首行重复的问题。
- ExitPlanMode 计划面板重构为消息流内的 `PlanProposal`，支持原生鼠标滚轮滚动。
- 新增 `wyj-code update` 自更新命令（检查 GitHub Release、下载校验、原地替换二进制）。
- 新增 `build.sh release` 一键发版脚本。

## [1.0.1]

- 逐调用工具权限确认（Edit/Write/Bash 前弹出权限对话框，支持 AllowOnce/AllowAlways/Deny）。
- 会话按项目（git 仓库根）隔离存储与恢复。
- 新增 WebSearch（Tavily）工具。
- 新增 GitHub 相关 slash 命令（`/bug` `/review` `/pr-comments` 等）。
- TUI 聊天区改用终端原生 scrollback（`ratatui::Viewport::Inline` + `insert_before`），
  对齐 Claude Code 的鼠标滚轮/原生选中复制体验。

## [1.0.0]

- 首个正式版本：Agent 推理循环、双协议 LLM 适配（Anthropic/OpenAI）、内置工具集
  （Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite）、ratatui TUI、Profile 分组配置、
  子 Agent 编排、上下文压缩、跨会话记忆、CLAUDE.md 记忆机制、i18n（中/英）、
  MCP/Skill/Plugin 三市场、多平台 GitHub Actions Release CI。
