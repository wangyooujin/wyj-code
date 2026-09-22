(function () {
  'use strict';

  var translations = {
    zh: {
      'nav.delivery': '交付状态',
      'nav.features': '特性',
      'nav.architecture': '架构',
      'nav.install': '安装',
      'nav.changelog': '更新日志',
      'nav.github': 'GitHub',

      'hero.kicker': '个人工程作品 · Clean-room 实现 · 开源',
      'hero.tagline': '用 Rust 从零实现的终端 AI 编程助手',
      'hero.desc': 'v1.5.12 已公开发布：TUI 终端 panic 兜底还原（panic 时自动还原 raw mode + alternate screen）、CJK/emoji 字符串截断安全（`wyj_core::textutil::floor_char_boundary` 治本 memory_v3 撞 char boundary panic）、输入框标题栏/状态栏精简；安装脚本会获取最新公开 Release。',
      'hero.cta.github': '查看 GitHub 仓库',
      'hero.cta.install': '60 秒上手 →',
      'hero.badge.telemetry': '零遥测',
      'hero.oneliner.label': '一键安装（macOS / Linux）：',
      'hero.oneliner.windows': 'Windows：<a href="#install" class="underline hover:text-paper/60">PowerShell 命令见下方 →</a>',

      'hero.term.user': '把 CLI 参数解析里 <code class="text-paper">--resume</code> 的边界情况补一下测试',
      'hero.term.read': '正在读取 <span class="text-paper">crates/cli/src/main.rs</span>...',
      'hero.term.readres': 'Read main.rs (238 lines)',
      'hero.term.edit': '调用工具 <span class="text-paper">Edit</span> — crates/cli/src/main.rs',
      'hero.term.confirm.title': '允许此次编辑？',
      'hero.term.confirm.yes': '允许一次',
      'hero.term.confirm.always': '始终允许',
      'hero.term.confirm.deny': '拒绝',
      'hero.term.written': '已写入 <span class="text-phosphor">+18</span> <span class="text-[#ff8a8a]">-2</span>',
      'hero.term.run': '运行 <span class="text-paper">cargo test -p wyj-code</span>',
      'hero.term.testresult': 'test result: ok. 12 passed',

      'stats.size': 'release 二进制体积（已 strip）',
      'stats.boot': '稳态冷启动耗时',
      'stats.crates': 'workspace crate 数量',
      'stats.telemetry': '遥测 / 埋点上报',

      'delivery.kicker': 'Delivery Status',
      'delivery.title': 'v1.5.12 · TUI 终端 panic 还原 + char-boundary 安全工具',
      'delivery.desc': 'v1.5.12 公开交付三条 TUI 健壮性修复：(1) 新增 `crates/tui/src/panic_guard.rs` 进程级 `panic::set_hook`，`cli::main()` 入口最前 `install()`、`run_tui` 在 `enter/leave_terminal_screen` 配对 `mark_active()/mark_inactive()`，panic 触发时若 active 则 best-effort 还原终端（`DisableMouseCapture` → `LeaveAlternateScreen` → `disable_raw_mode` → `Show`），告别"TUI 进程 panic 后终端卡死 + alternate screen 残留 frame + panic 写 stderr 覆盖 ratatui cell 造成撕裂"。(2) 新增 `crates/core/src/textutil.rs::floor_char_boundary` 治本 `memory_v3.rs` 拼装 Active Memory 上下文超 `MAX_CONTEXT_BYTES` 时撞 CJK/emoji char boundary 触发的 `is_char_boundary` assertion panic，按字节切字符串前回退到最近 char boundary 再 `String::truncate`。(3) `thinking_status_label` 改回单层优先级静态字符串、标题栏仅 thinking 态保留 spinner、删除 `draw_input` 在 `is_thinking=true` 时跳过 `set_cursor_position` 致光标卡死的回归、状态栏默认不再放长串快捷键提示。历史 tag 保持不可移动，Release 与安装脚本同步指向最新公开版本。',
      'delivery.status': '已交付',
      'delivery.p0.title': 'P0 · 可信执行',
      'delivery.p0.desc': '国内模型能力目录与 doctor、工具参数修复/schema 校验、Provider 错误分类、权限 fail-closed、Plan 文档写策略与 OS sandbox。',
      'delivery.p1.title': 'P1 · 效率与恢复',
      'delivery.p1.desc': 'ToolSearch/lazy schema、同角色 fallback、checkpoint/rewind/branch、SubAgent 控制，以及 TUI/Markdown/终端交互整合。',
      'delivery.p2.title': 'P2 · 工程控制面',
      'delivery.p2.desc': 'Managed Worktree、Workflow DAG、ACP/daemon、跨连接 session、CodeIndex/LSP、Plugin runtime、本地 Review 与严格 Release CI。',
      'delivery.evolution.title': 'v1.5.13 · TypeSafe Jev 决策 API 接入',
      'delivery.evolution.desc': '新增 `crates/tools/src/jev.rs` 的 `JevTool`（POST `https://api.typesafe.ai/v1/systemone`，三种 primitive：choice / score / noul，每个 answer 自带 `confidence` + `probabilities`）和 `JevBudget` 进程级日预算计数器（micro-cent 精度）。`Config.tools.jev` 子块 + `Config::resolve_jev_api_key()` env/字段合并（env 优先 + 字段兜底，绝不物化进 config.toml）。三层硬封顶（`max_state_chars` / `max_questions` / `daily_budget_usd`）+ `core::prompts::JEV_HINT` 仅在 jev 已注册时拼到 system prompt。`/decision [ping|ask <问题>]` slash 命令做连通性诊断。Jev 与 chat 模型协议完全不同（无 stream / 无 multi-turn / 无 tool calling），不污染 `WireProtocol` / `Provider` 分桶；子 Agent 工厂不注册。13 项 jev 单测 + 7 项 config 单测覆盖 validation / happy path / 三 primitive / HTTP 错误归一（401/422/429/529）/ 重试 / state 截断 / budget 硬封顶。',
      'delivery.evolution.plan': '查看 Memory v3 实施计划 →',
      'delivery.domestic.title': '国内模型专项收口',
      'delivery.domestic.desc': '完整多工具响应会按保守能力受控串行执行；lazy schema 保留核心编码与 computer-use 工具；headless/daemon 可执行只读桌面观察，变更与未知动作仍失败关闭。没有独立 probe 证据时继续标记 static_only。',
      'delivery.tests.value': '881',
      'delivery.tests.label': 'workspace 通过 + 公网专项通过',
      'delivery.clippy.value': '0 warning',
      'delivery.clippy.label': 'workspace · all targets · Rust 1.96.0',
      'delivery.platforms.value': '5 / 5',
      'delivery.platforms.label': 'macOS · Linux musl · Windows',
      'delivery.assets.value': '11 assets',
      'delivery.assets.label': '5 归档 + 5 sidecar + SHA256SUMS',
      'delivery.release': '查看 v1.5.12 Release →',
      'delivery.plan': '查看完整实施计划 →',

      'features.kicker': 'Features',
      'features.title': '核心能力',
      'features.desc': '从推理循环到交互体验，每一层都是为终端场景重新设计的。',

      'feature.provider.title': '国内模型可信运行时',
      'feature.provider.desc': 'GLM、MiniMax、Kimi、DeepSeek、Qwen、豆包按能力来源和置信度适配；静态兼容与 live verified 明确分开。',
      'feature.agent.title': '可控的多 Agent 协作',
      'feature.agent.desc': '进程级 Hub 管理并发、前后台调度、follow-up、interrupt、retry-last 与落盘 trace，控制消息只在安全边界注入。',
      'feature.hooks.title': 'Hooks 生命周期自动化',
      'feature.hooks.desc': 'PreToolUse / PostToolUse / UserPromptSubmit / Stop 四个节点可挂任意 shell 脚本，对齐真实 Claude Code。',
      'feature.mcp.title': '可插拔 MCP',
      'feature.mcp.desc': '内置 Bash / Read / Write / Edit / Glob / Grep / WebFetch / TodoWrite，并可桥接任意外部 MCP server。',
      'feature.compact.title': '上下文自动压缩',
      'feature.compact.desc': 'token 数逼近窗口上限时自动生成摘要替换旧消息，长会话也不会突然断档。',
      'feature.memory.title': 'CLAUDE.md 记忆机制',
      'feature.memory.desc': '每轮重新读盘注入、子目录动态加载、@path 递归导入，跨会话记忆同样开箱即用。',
      'feature.evolution.title': '证据化自进化',
      'feature.evolution.desc': 'v1.5.5 用 Episode、可验证 Memory、人工审批 Rule/Skill 候选和原子回滚形成透明闭环；Web/MCP 外部上下文默认隔离。',
      'feature.tui.title': '原生 ratatui TUI',
      'feature.tui.desc': '流式 markdown、语法高亮、工具调用实时展示、子 Agent 聚合面板，交互体验为终端而生。',
      'feature.profile.title': '能力诊断与安全 Key 引用',
      'feature.profile.desc': '<code class="text-amber-400">model doctor</code> 展示 vendor/protocol/能力来源；<code class="text-amber-400">api_key_env</code> 避免运行时 secret 被写回配置。',
      'feature.session.title': 'Checkpoint / Rewind / Branch',
      'feature.session.desc': '保留真实 Git index，按 conversation/files/both 恢复，并从任意 checkpoint 创建不影响原会话的新分支。',
      'feature.slash.title': '自定义 Slash 命令',
      'feature.slash.desc': '兼容 <code class="text-amber-400">~/.claude/commands/*.md</code> 与项目级命令，六层路径合并加载。',
      'feature.i18n.title': '中 / 英双语',
      'feature.i18n.desc': '界面文案运行时切换语言，自动检测系统 locale，也可在配置中显式指定。',
      'feature.privacy.title': 'Fail-closed OS Sandbox',
      'feature.privacy.desc': 'macOS Seatbelt + 受控域名代理、Linux bubblewrap；headless、schedule 与 SubAgent 无 UI 时拒绝隐式放行。',
      'feature.computer.title': 'Computer-use 桌面控制',
      'feature.computer.desc': '模型可观察并操控本机 GUI（macOS / Windows）；v1.5.5 允许 headless/daemon 安全执行截图、窗口枚举和元素检查，点击、输入、滚动及未知动作仍失败关闭。',
      'feature.workflow.title': 'Workflow + 隔离 Worktree',
      'feature.workflow.desc': 'DAG 支持并行、预算、审批、暂停、重试与取消；有写权限的编码节点从脏工作区 checkpoint 自动创建独立 worktree，变更需显式 review/accept。',
      'feature.acp.title': 'ACP / daemon 控制面',
      'feature.acp.desc': 'stdio ACP 与本地 TCP daemon 共享前端无关事件协议；daemon session 跨连接存活，可重连、列出、提交、打断、rewind、branch 和关闭。',
      'feature.plugin.title': 'Plugin Runtime + LSP',
      'feature.plugin.desc': '插件可事务式贡献 hooks、styles、themes、channels、LSP、monitors 与 settings；真实 <code class="text-amber-400">workspace/symbol</code> 与本地索引合并。',
      'feature.review.title': '本地 Review 证据',
      'feature.review.desc': '<code class="text-amber-400">review run</code> 对 commit/PR diff 生成可审计 JSON，处理 rename、空格路径和 binary，并对 secret evidence 脱敏。',
      'feature.jev.title': 'Jev 决策 API（v1.5.13）',
      'feature.jev.desc': 'TypeSafe System One 决策模型作独立 tool：意图路由 / 分类 / guardrails / 置信度标注，输出结构化 answers + probabilities；<code class="text-amber-400">/decision</code> slash 命令做连通性诊断，三层硬封顶（state 大小 / question 数 / 日预算）防失控。',

      'arch.kicker': 'Architecture',
      'arch.title': '12 个 crate 的 Rust workspace',
      'arch.desc': '单一 wyj-code 二进制，职责分层清晰：从上到下依次是入口、服务、核心、基础四层。',
      'arch.layer.entry': '入口层 · Entry',
      'arch.cli': '二进制入口：组装全部 crate、解析 CLI 参数，启动 TUI / REPL / Workflow / ACP / daemon / Review',
      'arch.tui': 'ratatui 终端渲染：输入框、权限确认对话框、子 Agent 面板',
      'arch.layer.services': '服务层 · Services',
      'arch.tools': 'Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite 等工具实现',
      'arch.computer': 'Computer-use 系统层：截图 + 鼠标键盘输入合成，坐标缩放数学独立可测',
      'arch.commands': 'Slash 命令注册表与内置命令（/help、/compact 等）',
      'arch.mcp': 'MCP 客户端桥接（stdio / http 传输）',
      'arch.store': 'Extension 安装与 lockfile、Plugin runtime 事务激活、持久 LSP client、schedule',
      'arch.i18n': '多语言资源与运行时语言切换',
      'arch.layer.core': '核心层 · Core',
      'arch.core': 'Agent、Session runtime/events、权限、checkpoint、workspace/workflow 接口与本地 CodeIndex',
      'arch.layer.foundation': '基础层 · Foundation',
      'arch.api': 'LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析',
      'arch.config': '配置加载（~/.wyj-code/config.toml）、MCP 配置结构',
      'arch.sandbox': 'Seatbelt / bubblewrap 命令隔离、凭证 deny-read 与域名级网络边界',

      'install.kicker': 'Install',
      'install.title': '60 秒上手',
      'install.desc': '一条命令完成下载、安装、配置 PATH，无需 sudo / 管理员权限。',
      'install.oneliner.unix': 'macOS / Linux',
      'install.oneliner.win': 'Windows (PowerShell)',
      'install.oneliner.note': '脚本自动识别平台架构、拉取 GitHub 最新 Release、校验 sha256 后装入用户目录；之后可用 <code class="text-amber-400">wyj-code update</code> 升级。',
      'install.tab.prebuilt': '预编译安装包',
      'install.tab.source': '源码构建',
      'install.tab.dev': '开发者模式',
      'install.prebuilt.desc': '手动从 GitHub Releases 下载对应平台压缩包，解压后运行内置安装脚本——这正是上面一键脚本在背后做的事，适合不想执行 curl | sh 的场景。',
      'install.prebuilt.link': '前往 Releases 页面下载 →',
      'install.source.desc': '需要 Rust 1.80+ 工具链，构建 release 二进制并安装到 <code class="text-amber-400">~/.local/bin</code>。',
      'install.dev.desc': '直接用 cargo 跑起来，适合改代码调试。',
      'install.code.prebuilt': '<span class="text-paper/35"># macOS / Linux</span>\ntar xzf wyj-code-*.tar.gz &amp;&amp; cd wyj-code-*/ &amp;&amp; ./install.sh\n\n<span class="text-paper/35"># Windows（在解压目录里）</span>\ninstall.bat\n\n<span class="text-paper/35"># 之后升级</span>\nwyj-code update',
      'install.code.source': 'git clone https://github.com/wangyooujin/wyj-code.git\ncd wyj-code\n./build.sh install\n\n<span class="text-paper/35"># 卸载</span>\n./build.sh uninstall',
      'install.code.dev': '<span class="text-paper/35"># TUI 模式</span>\ncargo run\n\n<span class="text-paper/35"># 单次问答</span>\ncargo run -- -p "你的问题"\n\n<span class="text-paper/35"># headless REPL</span>\ncargo run -- --headless\n\n<span class="text-paper/35"># 查看配置状态</span>\ncargo run -- --config-status',

      'principles.kicker': 'Principles',
      'principles.title': '设计原则',
      'principle.local.title': '本地优先',
      'principle.local.desc': '没有隐式埋点、没有崩溃上报，配置与会话数据全部留在本机。',
      'principle.transparent.title': '透明可控',
      'principle.transparent.desc': '每一次工具调用全程实时展示，敏感操作前弹权限确认，随时可以 ESC 打断。',
      'principle.neutral.title': '协议中立',
      'principle.neutral.desc': 'vendor 与 wire protocol 分离，同一兼容协议可服务不同厂商，不把模型名称猜测当成最终事实。',
      'principle.zero.title': '零遥测',
      'principle.zero.desc': '只有显式的 LLM / WebFetch / MCP 调用才会出网，没有任何后台上报。',

      'changelog.kicker': 'Changelog',
      'changelog.title': '版本亮点',
      'changelog.latest': '最新公开版',
      'changelog.previous': '上一公开版',
      'changelog.v1513': '<strong class="text-amber-400">TypeSafe Jev 决策 API 接入</strong>：新增 `crates/tools/src/jev.rs` 的 `JevTool`（POST `https://api.typesafe.ai/v1/systemone`，三种 primitive：choice / score / noul，每个 answer 自带 `confidence` + `probabilities`），与 `JevBudget` 进程级日预算计数器（micro-cent 精度，输入 `$0.042/M`、输出免费）。`Config.tools.jev` 子块 + `Config::resolve_jev_api_key()` env/字段合并（与 `runtime_api_key` 同款"serde skip"语义）。客户端三层硬封顶：`max_state_chars`（按 char boundary 安全截断）/ `max_questions` / `daily_budget_usd`（`0` 关闭）。`cli::register_jev_tool_if_enabled` 仿 `register_computer_tool_if_enabled` 的门控模式（仅 enabled + API Key 可解析时注册），驱动 `core::prompts::JEV_HINT` 仅在已注册时拼到 system prompt。`/decision [ping|ask <问题>]` slash 命令做连通性诊断 + 快速问答（zh/en.yml `/help.body` 已同步注册）。Jev 与 chat 模型完全不同——无 stream / 无 multi-turn / 无 tool calling 协议——<strong>不</strong>新增 `WireProtocol` 变体、<strong>不</strong>改 `Provider` trait，避免污染 routing/capability_cache 分桶。子 Agent 工厂不注册（与 Computer / SubAgent / AskQuestion 同款白名单策略）。13 项 jev 单测覆盖 client-side validation / happy path / 三 primitive 端到端 / HTTP 错误归一（401/422/429/529）/ 429 重试 / state 截断 / budget 硬封顶；7 项 config 测试覆盖 jev 默认值 / partial 段解析 / base URL trim / api key 解析；workspace 全量回归 + clippy `-D warnings` 全绿。',
      'changelog.v1512': 'TUI 终端 panic 兜底还原 + char-boundary 安全工具 + 标题栏/状态栏精简：新增 `crates/tui/src/panic_guard.rs` 进程级 `panic::set_hook`（`cli::main()` 入口最前 `install()`，`run_tui` 在 `enter/leave_terminal_screen` 配对 `mark_active/mark_inactive`），panic 触发时若 active 则 best-effort 还原终端（`DisableMouseCapture` → `LeaveAlternateScreen` → `disable_raw_mode` → `Show`）。新增 `wyj_core::textutil::floor_char_boundary` 治本 `memory_v3.rs` 拼装 Active Memory 上下文超 `MAX_CONTEXT_BYTES` 时撞 CJK/emoji char boundary 的 `is_char_boundary` assertion panic。`thinking_status_label` 改回单层优先级静态字符串（去掉旧的"大象装进冰箱"4 秒旋转短语）；`draw_input` 标题栏仅 thinking 态保留 spinner，三模式 Plan/Bypass/Normal 非思考态只留最简洁 `[plan/bypass] Enter to send` / `Enter to send`；状态栏默认右侧不再放快捷键提示；删除 `draw_input` 在 `is_thinking=true` 时提前 `return` 致光标卡死的回归，配套新增两个 `TestBackend` 回归测试防再次复发。',
      'changelog.v1511': 'Session 存储重构（M1-M4）：新增 `crates/core/src/workspace_cas.rs` 提供 sha256 内容寻址 Blob Pool（`intern/get/release/gc/stats`），`FileEntry { hash, inline_bytes, size, sha256 }` 替代内联字节 + `#[serde(default, alias = "bytes")]` 兼容旧 v1.5.10 checkpoint；`WorkspaceSnapshot::Delta` 同 cwd 自动 fold 父链（20 层上限），跨 cwd 强制 baseline；`externalize_block_with` 把 >32KB image 与 >16KB thinking 外置到 CAS（`cas://<hash>` 引用，`materialize_block_with` 在 resume 时还原）；`gc()` 用 `last_ref_at` LRU 淘汰 0-ref blob，对齐 git pack-files 语义。实测：单 checkpoint 11MB → ~100KB，21 checkpoint 长会话从 ~230MB → ~3MB（~99% 压缩）。新增 `/new` slash 命令对齐 Claude Code 新会话语义：自动保存当前会话 + 分配新 session_id + 清空 TUI 状态 + 无二次确认弹窗。新增 `wyj-code storage {status,doctor,prune}` 占用治理子命令，`status --json` 可脚本消费。869 workspace 测试 + clippy clean。',
      'changelog.v1510': '50GB 级磁盘占用默认 cap：Evolution per-project 100MiB + 28–180 天 TTL；Session checkpoint 20/session、Memory v2/v3 records/Superseded 单独计数；Schedule 日志 50 文件 + run.log 10MiB×3 轮转；plugin .git 7 天 `git gc`；workspace worktree 30 天 prune；持久化前 content 截断（tool_result 20K+10K、thinking 8K、tool_use.input 64K）；顶层 `~/.wyj-code` 达 5GiB 启动一次性 warn；全部 opt-out by `0` in config。首次启动 `~/.wyj-code` 缺失时 TUI 自动打开 /model 引导用户填写 API Key（焦点预置 api_key 字段），写盘后 chmod 0600 自动重建 agent，无需重启；headless / -p / ACP / daemon 无 UI 时继续报错但给出指向 TUI 与 `WYJ_CODE_API_KEY` 的可复制 hint。MiniMax M3 thinking 按 `effort_levels` 拆出 `ReasoningEffort`，与 M2 协议并行走不同 vendor dispatch 与 capability 路径。',
      'changelog.v156': 'Memory v3 收敛为 Global/Project 两层（删除共享 Workspace scope），AI 自动管理项目记忆、Project 覆盖 Global 冲突；Global 背景提取走 Pending 候选，由 Memory 工具三步自然语言确认；新增 Task 类型（InProgress/Completed/Cancelled/Blocked）+ 动态 Project Brief，“继续/接着/resume/go on” 命中即恢复最近未完成任务；`/memory clear-all` 一键清空重建。Evolution 收敛到 GovernanceOnly，普通 Memory 数据层不再注入也不再自动生成。',
      'changelog.v155': '新增证据化 Evolution store、/evolve 四视图与完整 CLI、相关性 Memory 注入、外部上下文隔离、人工审批 Rule/Skill 候选、结构化 Skill eval、原子安装回滚；同时纳入多图片 Composer 与 Ghostty 滚轮/方向键隔离。',
      'changelog.v154': '按 action 对齐 computer-use 的统一权限与工具权限语义：headless/daemon 可安全执行截图、窗口枚举和元素检查，点击、输入、滚动及未知动作仍失败关闭。',
      'changelog.v153': 'ToolSearch 保留 computer-use 核心 schema，避免国内模型误判 GUI 不可用；同时修复 Windows checksum 的 CRLF 可移植性，并在上传前实测五平台 SHA256。',
      'changelog.v152': 'ToolSearch lazy schema 始终保留核心读写与 Agent 执行面，避免国内模型在大工具集下看不到必需工具；同时完成 Rust/Clippy 1.96 严格门禁兼容并固定 Release/Review 工具链。',
      'changelog.v151': '修复国内模型一次返回多个完整工具调用时的兼容路径：保守能力仍控制串行执行，但不再错误拒绝额外合法调用或误触参数重试熔断；同时修复 Linux 专属严格 Clippy 阻塞。',
      'changelog.v150': '完整交付 P2：Workflow 编码节点从当前脏工作区 checkpoint 自动进入 managed worktree；新增 workspace review/accept 生命周期、schema v2 ACP/daemon 全局 session、真实 Plugin LSP workspace/symbol、事务式 Plugin runtime、本地 Review/CI，并整合 Markdown 网格、终端原生拖选与 OSC 8 超链接。国内模型无独立 probe Key 时仍保持 static_only。',
      'changelog.v144': '国内模型可信运行时与安全执行：能力目录/doctor、工具参数修复校验、同角色 fallback、ToolSearch lazy schema、checkpoint/rewind/branch、SubAgent 控制协议，以及 macOS Seatbelt / Linux bubblewrap sandbox。未使用已暴露的 MiniMax Key，国内模型当前保持 static_only，不冒充 live verified。',
      'changelog.v142': 'TUI 对齐 Codex 静态执行流：Thinking、工具结果和 Bash 输出默认最多显示 3 个视觉行，Edit/Write 自动展示彩色 diff；鼠标可直接拖选复制文字，无需 Shift；并新增聊天逐行滚动、Todo 单任务详情，以及上滚阅读时不抢视口的新消息提示。',
      'changelog.v141': '修复 TUI 图片/文字/文件粘贴链路：Ctrl/Command+V 可直接读取纯图片剪贴板，附件以内联 <code class="text-amber-400">[Image]</code> / <code class="text-amber-400">[File]</code> 占位符显示并支持 attachment-only 发送；修复 foreground computer 在同 App 多窗口和“点击后立即输入”连续动作中误报 <code class="text-amber-400">target_changed</code>。',
      'changelog.v140': 'Computer-use 重构为人机互不干扰架构：默认改为稳定窗口目标 + macOS Accessibility 后台操作，不移动物理光标、不抢前台窗口，用户可在其它 App 正常输入的同时让 Agent 后台操作目标窗口，仅在真正冲突时才安全熔断；新增项目级 MCP server 信任确认（按内容指纹首次需人工批准）与 <code class="text-amber-400">.wyj-code/</code> 项目配置按 Git 仓库根自动发现，任意子目录启动均可用。',
      'changelog.v133': '新增 <code class="text-amber-400">/schedule</code> 定时任务面板：一句话 prompt 或固化当前对话即可生成到点自动执行的任务，保存后自动同步进系统 crontab（macOS / Linux），失败可选 macOS 系统通知；新增 <code class="text-amber-400">/import</code> 一键导入 Codex / Claude Code 的 MCP、自定义命令与 Agent 配置；全应用列表选中态背景色统一为深灰，告别忽蓝忽无色的不一致观感。',
      'changelog.v130': '新增 Computer-use 桌面 GUI 控制（macOS / Windows）：模型截图观察桌面、合成鼠标键盘操作，官方端点用原生工具、第三方 Anthropic 协议兼容端点自动退化为 custom 工具；TUI 主界面改为永久 Fullscreen，输入框/状态栏始终贴住窗口底部，不再有留白。',
      'changelog.v122': '统一 Extensions 中心管理 Skill / MCP / Plugin，支持热应用、诊断与兼容迁移；安装、锁定与回滚更可靠；上下文压缩更稳，MiniMax / GLM / DeepSeek 已完成请求采用供应商精确 usage 账务。',
      'changelog.v121': 'TUI 消息流重构（thinking / 工具块 / Ctrl+O 展开应用内滚动）；Profile 新增 prompt_cache / openai_stream_options 兼容开关，GLM / Kimi / DeepSeek 等国内模型官方开箱即用；/cost 与 stats JSON 补全 full input / 缓存命中率 / context 指标。',
      'changelog.v120': '自定义 Slash 命令对齐真实 Claude Code（识别 <code class="text-amber-400">~/.claude/commands/*.md</code>）；性能实测（12MB / ~10ms）与依赖排查；预编译包新增一键安装脚本 install.sh / install.bat。',
      'changelog.v110': '新增 Hooks 生命周期自动化系统（PreToolUse / PostToolUse / UserPromptSubmit / Stop）；开源产品化基线（LICENSE、CONTRIBUTING、CHANGELOG）。',
      'changelog.v102': '新增 <code class="text-amber-400">wyj-code update</code> 自更新；ExitPlanMode 计划面板重构；<code class="text-amber-400">build.sh release</code> 一键发版脚本。',
      'changelog.v100': '首发：Agent 推理循环、双协议 LLM 适配、内置工具集、ratatui TUI、子 Agent、上下文压缩、跨会话记忆、MCP / Skill / Plugin 市场。',
      'changelog.link': '查看完整更新日志 →',

      'cta.title': '在你的终端里试一下',
      'cta.desc': '开源、单二进制、零遥测——克隆下来跑起来只要 60 秒。',
      'cta.github': '前往 GitHub',
      'cta.install': '查看安装步骤',

      'footer.disclaimer': '个人技术作品集项目，基于公开的 Anthropic Messages API、OpenAI Chat Completions API 与 MCP 规范 clean-room 实现，不含任何第三方专有 prompt 或品牌资产，与 Anthropic / OpenAI 官方产品无关联。',

      'copy.copy': '复制',
      'copy.copied': '已复制',
    },
    en: {
      'nav.delivery': 'Delivery',
      'nav.features': 'Features',
      'nav.architecture': 'Architecture',
      'nav.install': 'Install',
      'nav.changelog': 'Changelog',
      'nav.github': 'GitHub',

      'hero.kicker': 'Personal engineering project · Clean-room implementation · Open source',
      'hero.tagline': 'A terminal AI coding agent, built from scratch in Rust',
      'hero.desc': 'v1.5.12 is now public: a process-level panic hook restores the TUI terminal on panic (raw mode + alternate screen), a `wyj_core::textutil::floor_char_boundary` helper safely truncates CJK / emoji strings at char boundaries, and the input title bar / status bar copy has been streamlined. Installers fetch the latest public Release.',
      'hero.cta.github': 'View on GitHub',
      'hero.cta.install': 'Get started in 60s →',
      'hero.oneliner.label': 'One-line install (macOS / Linux):',
      'hero.oneliner.windows': 'Windows: <a href="#install" class="underline hover:text-paper/60">see PowerShell command below →</a>',
      'hero.badge.telemetry': 'Zero telemetry',

      'hero.term.user': 'Add a test for the <code class="text-paper">--resume</code> edge cases in CLI arg parsing',
      'hero.term.read': 'Reading <span class="text-paper">crates/cli/src/main.rs</span>...',
      'hero.term.readres': 'Read main.rs (238 lines)',
      'hero.term.edit': 'Calling tool <span class="text-paper">Edit</span> — crates/cli/src/main.rs',
      'hero.term.confirm.title': 'Allow this edit?',
      'hero.term.confirm.yes': 'Allow once',
      'hero.term.confirm.always': 'Always allow',
      'hero.term.confirm.deny': 'Deny',
      'hero.term.written': 'Wrote <span class="text-phosphor">+18</span> <span class="text-[#ff8a8a]">-2</span>',
      'hero.term.run': 'Running <span class="text-paper">cargo test -p wyj-code</span>',
      'hero.term.testresult': 'test result: ok. 12 passed',

      'stats.size': 'release binary size (stripped)',
      'stats.boot': 'steady-state cold start',
      'stats.crates': 'crates in the workspace',
      'stats.telemetry': 'telemetry / analytics calls',

      'delivery.kicker': 'Delivery Status',
      'delivery.title': 'v1.5.12 · TUI panic-safe terminal + char-boundary safe truncation',
      'delivery.desc': 'v1.5.12 ships three TUI robustness fixes: (1) a new `crates/tui/src/panic_guard.rs` installs a process-level `panic::set_hook` at the top of `cli::main()`; `run_tui` pairs `mark_active()` / `mark_inactive()` around `enter/leave_terminal_screen`. When the hook fires while the alternate screen is active, it best-effort restores the terminal (`DisableMouseCapture` → `LeaveAlternateScreen` → `disable_raw_mode` → `Show`) before delegating to the previous hook to write the panic message — ending the "stuck raw mode + leftover alternate-screen frame + panic output overwriting ratatui cells" failure mode. (2) `crates/core/src/textutil.rs::floor_char_boundary` rewinds a byte index to the nearest char boundary before `String::truncate`, fixing the `is_char_boundary` assertion panic that `memory_v3.rs` triggered while assembling Active Memory context over `MAX_CONTEXT_BYTES` on CJK / emoji content. (3) `thinking_status_label` returns a single-priority static string, the title bar keeps the spinner only while thinking, the `is_thinking=true` early-return that skipped `set_cursor_position` (and froze the cursor) is removed, and the status bar drops the long shortcut hints. Historical tags remain immutable; Release links and installers point to the latest public version.',
      'delivery.status': 'Delivered',
      'delivery.p0.title': 'P0 · Trusted execution',
      'delivery.p0.desc': 'Chinese-model catalog and doctor, tool-argument repair/schema validation, provider error classes, fail-closed permissions, Plan-document writes, and OS sandboxing.',
      'delivery.p1.title': 'P1 · Efficiency and recovery',
      'delivery.p1.desc': 'ToolSearch/lazy schemas, same-role fallback, checkpoint/rewind/branch, SubAgent controls, and the integrated TUI/Markdown/terminal interaction model.',
      'delivery.p2.title': 'P2 · Engineering control plane',
      'delivery.p2.desc': 'Managed worktrees, Workflow DAGs, ACP/daemon, cross-connection sessions, CodeIndex/LSP, transactional plugin runtime, local Review, and strict Release CI.',
      'delivery.evolution.title': 'v1.5.13 · TypeSafe Jev decision API',
      'delivery.evolution.desc': 'New `JevTool` in `crates/tools/src/jev.rs` (POST `https://api.typesafe.ai/v1/systemone`; three primitives: choice / score / noul; every answer carries `confidence` + `probabilities`) alongside a process-level `JevBudget` with micro-cent precision. `Config.tools.jev` sub-block plus `Config::resolve_jev_api_key()` env-or-field merge (never materializes the env value into `config.toml`). Three hard caps on the client: `max_state_chars` (char-boundary safe truncation), `max_questions`, and `daily_budget_usd` (set to `0` to disable). `cli::register_jev_tool_if_enabled` mirrors `register_computer_tool_if_enabled` — the tool only joins the registry when both `enabled=true` and a key can be resolved — and that flag drives `core::prompts::JEV_HINT` so the hint only appears in the system prompt when the tool is actually live. The new `/decision [ping|ask <question>]` slash command provides a connectivity probe and quick Q&A. Jev is on a completely different protocol from chat models (no stream, no multi-turn, no tool-calling), so it does **not** extend `WireProtocol` or modify the `Provider` trait — routing / capability-cache buckets stay clean. Sub-agents do not inherit the tool (same allowlist policy as Computer / SubAgent / AskQuestion). 13 Jev unit tests cover client-side validation, the happy path, all three primitives end-to-end, HTTP error normalization (401/422/429/529), 429 retry, state truncation, and the budget hard-cap; 7 config tests cover Jev defaults, partial-section parsing, base-URL trimming, and API-key resolution. Full workspace regression + `clippy -D warnings` clean.',
      'delivery.evolution.plan': 'Read the Memory v3 implementation plan →',
      'delivery.domestic.title': 'Chinese-model compatibility closure',
      'delivery.domestic.desc': 'Complete multi-tool responses execute serially under conservative capabilities; lazy schemas retain core coding and computer-use tools; headless/daemon sessions may perform read-only desktop inspection while mutations and unknown actions still fail closed. Models without independent probe evidence remain static_only.',
      'delivery.tests.value': '869',
      'delivery.tests.label': 'workspace passes + public-network test pass',
      'delivery.clippy.value': '0 warnings',
      'delivery.clippy.label': 'workspace · all targets · Rust 1.96.0',
      'delivery.platforms.value': '5 / 5',
      'delivery.platforms.label': 'macOS · Linux musl · Windows',
      'delivery.assets.value': '11 assets',
      'delivery.assets.label': '5 archives + 5 sidecars + SHA256SUMS',
      'delivery.release': 'View the v1.5.12 Release →',
      'delivery.plan': 'Read the implementation plan →',

      'features.kicker': 'Features',
      'features.title': 'What it does',
      'features.desc': 'From the reasoning loop to the interaction model, every layer is designed for the terminal.',

      'feature.provider.title': 'Capability-aware Chinese models',
      'feature.provider.desc': 'GLM, MiniMax, Kimi, DeepSeek, Qwen, and Doubao are adapted through sourced capability data; static compatibility is kept distinct from live verification.',
      'feature.agent.title': 'Controllable multi-agent work',
      'feature.agent.desc': 'A process-wide hub manages concurrency, foreground/background scheduling, follow-up, interrupt, retry-last, and persisted traces at safe boundaries.',
      'feature.hooks.title': 'Hooks automation',
      'feature.hooks.desc': 'PreToolUse / PostToolUse / UserPromptSubmit / Stop — four lifecycle hooks that run arbitrary shell scripts, matching real Claude Code.',
      'feature.mcp.title': 'Pluggable MCP',
      'feature.mcp.desc': 'Bash / Read / Write / Edit / Glob / Grep / WebFetch / TodoWrite built in, plus a bridge to any external MCP server.',
      'feature.compact.title': 'Automatic context compaction',
      'feature.compact.desc': 'When the token count nears the context window limit, old messages are auto-summarized — long sessions never hit a hard wall.',
      'feature.memory.title': 'CLAUDE.md memory',
      'feature.memory.desc': 'Re-read from disk every turn, dynamic subdirectory loading, recursive @path imports — cross-session memory works out of the box.',
      'feature.evolution.title': 'Evidence-backed evolution',
      'feature.evolution.desc': 'v1.5.5 combines Episodes, validated Memories, human-approved Rule/Skill candidates, and atomic rollback in a transparent loop; Web/MCP external context is quarantined by default.',
      'feature.tui.title': 'Native ratatui TUI',
      'feature.tui.desc': 'Streaming markdown, syntax highlighting, live tool-call rendering, an aggregated sub-agent panel — an interaction model built for the terminal.',
      'feature.profile.title': 'Capability diagnostics and safe Key refs',
      'feature.profile.desc': '<code class="text-amber-400">model doctor</code> exposes vendor/protocol/capability sources, while <code class="text-amber-400">api_key_env</code> keeps runtime secrets out of saved config.',
      'feature.session.title': 'Checkpoint / Rewind / Branch',
      'feature.session.desc': 'Preserve the real Git index, rewind conversation/files/both, and branch a new session from a checkpoint without mutating the original.',
      'feature.slash.title': 'Custom slash commands',
      'feature.slash.desc': 'Compatible with <code class="text-amber-400">~/.claude/commands/*.md</code> and project-level commands, merged across a six-tier path chain.',
      'feature.i18n.title': 'Bilingual UI',
      'feature.i18n.desc': 'Runtime language switching with system-locale auto-detection, or set it explicitly in config.',
      'feature.privacy.title': 'Fail-closed OS sandbox',
      'feature.privacy.desc': 'macOS Seatbelt with a controlled domain proxy and Linux bubblewrap; headless, schedules, and sub-agents never treat a missing UI as approval.',
      'feature.computer.title': 'Computer-use desktop control',
      'feature.computer.desc': 'The model can inspect and control the local GUI on macOS and Windows. In v1.5.5, headless/daemon sessions may safely capture screenshots, enumerate windows, and inspect elements, while clicks, typing, scrolling, and unknown actions still fail closed.',
      'feature.workflow.title': 'Workflow + isolated worktrees',
      'feature.workflow.desc': 'DAG execution supports parallelism, budgets, approvals, pause, retry, and cancellation. Write-capable coding nodes checkpoint the dirty checkout into isolated worktrees and require explicit review/accept.',
      'feature.acp.title': 'ACP / daemon control plane',
      'feature.acp.desc': 'A stdio ACP adapter and local TCP daemon share a frontend-neutral event protocol. Daemon sessions survive disconnects and can be reattached, listed, submitted, interrupted, rewound, branched, or closed.',
      'feature.plugin.title': 'Plugin runtime + LSP',
      'feature.plugin.desc': 'Plugins transactionally contribute hooks, styles, themes, channels, LSP, monitors, and settings. Real <code class="text-amber-400">workspace/symbol</code> results merge with the local index.',
      'feature.review.title': 'Local review evidence',
      'feature.review.desc': '<code class="text-amber-400">review run</code> emits auditable JSON for commit/PR diffs, including renames, spaced paths, and binaries, with secret evidence redacted.',
      'feature.jev.title': 'Jev decision API (v1.5.13)',
      'feature.jev.desc': 'TypeSafe System One as a standalone tool: intent routing / classification / guardrails / confidence scoring with structured answers and probabilities. The <code class="text-amber-400">/decision</code> slash command probes connectivity; three client-side hard caps (state size / question count / daily budget) keep spend under control.',

      'arch.kicker': 'Architecture',
      'arch.title': 'A 12-crate Rust workspace',
      'arch.desc': 'One binary, cleanly layered responsibilities: entry, services, core, and foundation, top to bottom.',
      'arch.layer.entry': 'Entry layer',
      'arch.cli': 'Binary entry point: wires up every crate and launches TUI / REPL / Workflow / ACP / daemon / Review modes',
      'arch.tui': 'ratatui rendering: input box, permission dialogs, sub-agent panel',
      'arch.layer.services': 'Services layer',
      'arch.tools': 'Tool implementations: Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite and more',
      'arch.computer': 'Computer-use system layer: screenshot capture + mouse/keyboard input synthesis, coordinate-scaling math kept independently testable',
      'arch.commands': 'Slash command registry and built-ins (/help, /compact, etc.)',
      'arch.mcp': 'MCP client bridge (stdio / http transports)',
      'arch.store': 'Extension install/lockfile data, transactional plugin runtime, persistent LSP clients, and schedules',
      'arch.i18n': 'Localization resources and runtime language switching',
      'arch.layer.core': 'Core layer',
      'arch.core': 'Agent, Session runtime/events, permissions, checkpoints, workspace/workflow interfaces, and the local CodeIndex',
      'arch.layer.foundation': 'Foundation layer',
      'arch.api': 'LLM provider abstraction trait + Anthropic/OpenAI implementations, SSE stream parsing',
      'arch.config': 'Config loading (~/.wyj-code/config.toml), MCP config schema',
      'arch.sandbox': 'Seatbelt / bubblewrap command isolation, credential deny-read, and domain-scoped network boundaries',

      'install.kicker': 'Install',
      'install.title': 'Up and running in 60 seconds',
      'install.desc': 'One command downloads, installs, and configures PATH — no sudo/admin required.',
      'install.oneliner.unix': 'macOS / Linux',
      'install.oneliner.win': 'Windows (PowerShell)',
      'install.oneliner.note': 'The script detects your platform, fetches the latest GitHub Release, verifies its sha256, and installs into your user directory; upgrade later with <code class="text-amber-400">wyj-code update</code>.',
      'install.tab.prebuilt': 'Prebuilt binaries',
      'install.tab.source': 'Build from source',
      'install.tab.dev': 'Dev mode',
      'install.prebuilt.desc': 'Manually download the archive for your platform from GitHub Releases and run the bundled installer — that\'s exactly what the one-liner above does under the hood, handy if you\'d rather not pipe curl into sh.',
      'install.prebuilt.link': 'Go to Releases →',
      'install.source.desc': 'Requires the Rust 1.80+ toolchain; builds a release binary and installs it to <code class="text-amber-400">~/.local/bin</code>.',
      'install.dev.desc': 'Run it straight from cargo — handy while hacking on the code.',
      'install.code.prebuilt': '<span class="text-paper/35"># macOS / Linux</span>\ntar xzf wyj-code-*.tar.gz &amp;&amp; cd wyj-code-*/ &amp;&amp; ./install.sh\n\n<span class="text-paper/35"># Windows (inside the extracted folder)</span>\ninstall.bat\n\n<span class="text-paper/35"># upgrade later</span>\nwyj-code update',
      'install.code.source': 'git clone https://github.com/wangyooujin/wyj-code.git\ncd wyj-code\n./build.sh install\n\n<span class="text-paper/35"># uninstall</span>\n./build.sh uninstall',
      'install.code.dev': '<span class="text-paper/35"># TUI mode</span>\ncargo run\n\n<span class="text-paper/35"># one-shot prompt</span>\ncargo run -- -p "your question"\n\n<span class="text-paper/35"># headless REPL</span>\ncargo run -- --headless\n\n<span class="text-paper/35"># check config status</span>\ncargo run -- --config-status',

      'principles.kicker': 'Principles',
      'principles.title': 'Design principles',
      'principle.local.title': 'Local-first',
      'principle.local.desc': 'No implicit tracking, no crash reporting — config and session data stay on your machine.',
      'principle.transparent.title': 'Transparent & controllable',
      'principle.transparent.desc': 'Every tool call is shown live; sensitive actions require confirmation; you can interrupt with ESC at any time.',
      'principle.neutral.title': 'Protocol-neutral',
      'principle.neutral.desc': 'Vendor identity is separate from wire protocol, so compatible endpoints can vary without treating model-name guesses as verified facts.',
      'principle.zero.title': 'Zero telemetry',
      'principle.zero.desc': 'The only outbound calls are explicit LLM / WebFetch / MCP requests — nothing phones home.',

      'changelog.kicker': 'Changelog',
      'changelog.title': 'Release highlights',
      'changelog.latest': 'Latest public',
      'changelog.previous': 'Previous public release',
      'changelog.v1513': '<strong class="text-amber-400">TypeSafe Jev decision API integration</strong>: new `JevTool` in `crates/tools/src/jev.rs` (POST `https://api.typesafe.ai/v1/systemone`; three primitives: choice / score / noul; every answer carries `confidence` + `probabilities`) and a process-level `JevBudget` with micro-cent precision (input $0.042/M, output free). `Config.tools.jev` sub-block plus `Config::resolve_jev_api_key()` env-or-field merge that never materializes the env value into `config.toml` (same serde-skip pattern as `runtime_api_key`). Three hard client-side caps: `max_state_chars` (char-boundary safe truncation), `max_questions`, and `daily_budget_usd` (set `0` to disable). `cli::register_jev_tool_if_enabled` mirrors `register_computer_tool_if_enabled` — the tool joins the registry only when `enabled=true` AND a key resolves — and that flag drives `core::prompts::JEV_HINT` so the hint only appears in the system prompt when the tool is actually live. The new `/decision [ping|ask <question>]` slash command provides a connectivity probe and quick Q&A (registered in zh/en.yml `help.body`). Jev is on a fundamentally different protocol from chat models (no stream, no multi-turn, no tool-calling), so it does <strong>not</strong> extend `WireProtocol` or modify the `Provider` trait — routing / capability-cache buckets stay clean. Sub-agents do not inherit the tool (same allowlist policy as Computer / SubAgent / AskQuestion). 13 Jev unit tests cover client-side validation, the happy path, all three primitives end-to-end, HTTP error normalization (401/422/429/529), 429 retry, state truncation, and the budget hard-cap; 7 config tests cover defaults, partial-section parsing, base-URL trimming, and API-key resolution. Full workspace regression + `clippy -D warnings` clean.',
      'changelog.v1512': 'TUI panic-safe terminal + char-boundary safety + title-bar/status-bar cleanup: a new `crates/tui/src/panic_guard.rs` installs a process-level `panic::set_hook` (`wyj_tui::panic_guard::install()` at the top of `cli::main()`, `run_tui` pairs `mark_active` / `mark_inactive` around `enter/leave_terminal_screen`); when the hook fires while active, it best-effort restores the terminal (`DisableMouseCapture` → `LeaveAlternateScreen` → `disable_raw_mode` → `Show`). A new `wyj_core::textutil::floor_char_boundary` rewinds a byte index to the nearest char boundary before `String::truncate`, curing the `is_char_boundary` assertion panic that `memory_v3.rs` triggered on CJK / emoji content above `MAX_CONTEXT_BYTES`. `thinking_status_label` is now a single-priority static string (the old four-second rotating "elephant in the fridge" copy is gone); `draw_input` keeps the spinner only while thinking and the three mode labels fall back to `[plan/bypass] Enter to send` / `Enter to send`; the status bar no longer renders shortcut hints by default. Removes the `is_thinking=true` early-return that skipped `set_cursor_position` (the cursor froze), with two `TestBackend` regression tests guarding against the regression.',
      'changelog.v1511': 'Session storage refactor (M1–M4): a new `crates/core/src/workspace_cas.rs` provides a sha256 content-addressed Blob Pool (`intern/get/release/gc/stats`); `FileEntry { hash, inline_bytes, size, sha256 }` replaces the inlined bytes and `#[serde(default, alias = "bytes")]` keeps v1.5.10 checkpoints loadable. `WorkspaceSnapshot::Delta` auto-folds the parent chain when cwd matches (capped at 20 levels); cross-cwd checkpoints force a fresh baseline. `externalize_block_with` ships images >32KB and thinking blocks >16KB into CAS (`cas://<hash>` reference, `materialize_block_with` rehydrates on resume). `gc()` evicts zero-ref blobs by `last_ref_at` LRU, mirroring git pack-files. Real-world effect: a single checkpoint shrinks from ~11MB to ~100KB, a 21-checkpoint long session goes from ~230MB to ~3MB (~99% smaller). The new `/new` slash command matches Claude Code\'s new-session semantics (auto-save current, allocate new session_id, clear TUI state, no second confirmation). The new `wyj-code storage {status,doctor,prune}` subcommand covers occupancy governance; `status --json` is script-friendly. 869 workspace tests + clippy clean.',
      'changelog.v1510': '50GB-grade default disk-usage caps: Evolution per-project 100MiB with 28–180-day TTLs; Session checkpoint 20/session, Memory v2/v3 records/Superseded counted separately; Schedule logs capped at 50 files plus run.log 10MiB×3 rotations; plugin .git `git gc` every 7 days; workspace worktrees pruned at 30 days; pre-persistence content truncation (tool_result 20K+10K, thinking 8K, tool_use.input 64K); top-level `~/.wyj-code` emits a one-time startup warn at 5GiB. All caps opt out by setting `0` in config. A TUI launched without `~/.wyj-code` opens the /model dialog automatically (focus pre-set on the api_key field) and saves with chmod 0600 — no restart required. headless / -p / ACP / daemon surfaces the same hint and points users at `WYJ_CODE_API_KEY`. MiniMax M3 thinking now branches on `effort_levels` (`ReasoningEffort`), walking a different vendor dispatch and capability path from M2.',
      'changelog.v156': 'Memory v3 collapses to Global/Project two scopes (the shared Workspace scope is removed); AI auto-manages project memory and Project overrides Global on conflict. Global background extractions land as Pending candidates and require three natural-language tool steps to confirm. Adds the Task kind (InProgress/Completed/Cancelled/Blocked) and a dynamic Project Brief so “continue / resume / go on” resumes the most recent InProgress task; `/memory clear-all` rebuilds the store in one shot. Evolution collapses to GovernanceOnly, so the plain Memory layer is no longer injected or auto-generated.',
      'changelog.v155': 'Adds the evidence-backed Evolution store, four-view /evolve UI and full CLI, relevance-filtered Memory injection, external-context quarantine, human-approved Rule/Skill candidates, structured Skill evals, and atomic install rollback; also includes multi-image Composer support and Ghostty wheel/arrow isolation.',
      'changelog.v154': 'Aligns the unified computer-use policy with per-action tool permissions: headless and daemon sessions may safely inspect screenshots, windows, and elements, while clicks, typing, scrolling, and unknown actions still fail closed.',
      'changelog.v153': 'ToolSearch retains the core computer-use schemas so Chinese models do not misread lazy visibility as missing GUI support. Also fixed Windows checksum CRLF portability and verifies all five archives before upload.',
      'changelog.v152': 'ToolSearch lazy schemas now always retain the core read/write and Agent execution surface so Chinese models do not lose required tools in large catalogs. Also completed strict Rust/Clippy 1.96 compatibility and pinned the Release/Review toolchain.',
      'changelog.v151': 'Fixed compatibility when a Chinese model emits multiple complete tool calls in one response: conservative capabilities still serialize execution, but valid extra calls are no longer rejected or counted as argument-retry failures. Also fixed the initial Linux-only strict Clippy blockers.',
      'changelog.v150': 'Completed the P2 stack: Workflow coding nodes checkpoint the dirty checkout into managed worktrees; workspace review/accept lifecycle; schema-v2 ACP/daemon global sessions; real plugin LSP workspace/symbol; transactional plugin runtime; local Review/CI; plus the Markdown grid, native terminal selection, and OSC 8 links. Chinese models without an independent probe Key remain static_only.',
      'changelog.v144': 'A capability-aware Chinese-model runtime and fail-closed execution stack: model catalog/doctor, tool-argument repair and validation, same-role fallback, ToolSearch lazy schemas, checkpoint/rewind/branch, SubAgent controls, plus macOS Seatbelt and Linux bubblewrap. The exposed MiniMax Key was not used; unprobed Chinese models remain static_only rather than being presented as live verified.',
      'changelog.v142': 'Aligned the TUI with a Codex-style static execution stream: thinking, tool results, and Bash output show at most three visual rows by default, while Edit/Write render a colored diff automatically. Mouse drag now selects terminal text directly without Shift; line-by-line chat scrolling, per-task Todo detail, and non-disruptive new-message notices remain available.',
      'changelog.v141': 'Fixed the TUI paste path for images, text, and files: Ctrl/Command+V can now read image-only clipboards directly, attachments appear as compact inline <code class="text-amber-400">[Image]</code> / <code class="text-amber-400">[File]</code> placeholders, and attachment-only messages can be sent. Fixed false <code class="text-amber-400">target_changed</code> failures in foreground computer mode when one app has multiple windows or a click is immediately followed by typing.',
      'changelog.v140': 'Computer-use rebuilt as a non-interference architecture: background actions now default to stable window targets plus macOS Accessibility, no longer moving the physical cursor or stealing foreground focus, so the agent can drive its target window in the background while you keep typing in another app — it fails safe only on a genuine conflict. Added project-level MCP server trust confirmation (content-fingerprinted, approved once by hand) and Git-repo-root auto-discovery for <code class="text-amber-400">.wyj-code/</code> project config, which now works when launched from any subdirectory.',
      'changelog.v133': 'Added the <code class="text-amber-400">/schedule</code> panel for cron-triggered tasks: turn a one-line prompt — or the conversation you\'re already having — into a task that fires on schedule, auto-synced into the system crontab (macOS / Linux) on save, with an optional macOS notification on failure. Added <code class="text-amber-400">/import</code> for one-shot importing of Codex / Claude Code MCP servers, custom commands, and agent definitions. Unified the selected-row background across every list panel into a single dark gray, replacing the previous mix of saturated blue and no highlight at all.',
      'changelog.v130': 'Added computer-use desktop GUI control (macOS / Windows): the model views the desktop via screenshots and drives mouse/keyboard input; the official endpoint uses the native tool while third-party Anthropic-protocol-compatible endpoints automatically fall back to a custom tool. The TUI main view now runs permanently in Fullscreen, so the input box and status bar always sit at the bottom of the window with no leftover blank space.',
      'changelog.v122': 'A unified Extensions center manages Skills, MCP servers, and Plugins with hot apply, diagnostics, and compatibility migration; installation, locking, and rollback are more reliable; context compaction is safer, while completed MiniMax / GLM / DeepSeek requests use provider-reported usage for exact accounting.',
      'changelog.v121': 'TUI message stream refactor (thinking inline, tool blocks, Ctrl+O expand with in-app scrolling); Profile gained prompt_cache / openai_stream_options compatibility switches so GLM / Kimi / DeepSeek and other Chinese models work out of the box; /cost and stats JSON now expose full input / cache-hit ratio / context metrics.',
      'changelog.v120': 'Custom slash commands aligned with real Claude Code (discovers <code class="text-amber-400">~/.claude/commands/*.md</code>); measured performance (12MB / ~10ms) with a dependency audit; prebuilt archives now bundle one-shot install.sh / install.bat.',
      'changelog.v110': 'Added the Hooks lifecycle automation system (PreToolUse / PostToolUse / UserPromptSubmit / Stop); open-source productization baseline (LICENSE, CONTRIBUTING, CHANGELOG).',
      'changelog.v102': 'Added <code class="text-amber-400">wyj-code update</code> self-update; reworked the ExitPlanMode plan panel; <code class="text-amber-400">build.sh release</code> one-shot release script.',
      'changelog.v100': 'Initial release: agent reasoning loop, dual-protocol LLM support, built-in toolset, ratatui TUI, sub-agents, context compaction, cross-session memory, MCP / Skill / Plugin marketplaces.',
      'changelog.link': 'Read the full changelog →',

      'cta.title': 'Try it in your terminal',
      'cta.desc': 'Open source, single binary, zero telemetry — clone it and you’re running in 60 seconds.',
      'cta.github': 'Go to GitHub',
      'cta.install': 'See install steps',

      'footer.disclaimer': 'A personal engineering portfolio project, clean-room implemented against the public Anthropic Messages API, OpenAI Chat Completions API, and MCP specifications. Contains no third-party proprietary prompts or brand assets, and is not affiliated with Anthropic or OpenAI.',

      'copy.copy': 'Copy',
      'copy.copied': 'Copied',
    },
  };

  var STORAGE_KEY = 'wyj-lang';

  function detectDefaultLang() {
    var saved = null;
    try {
      saved = localStorage.getItem(STORAGE_KEY);
    } catch (e) {}
    if (saved === 'zh' || saved === 'en') return saved;
    var nav = (navigator.language || 'zh').toLowerCase();
    return nav.indexOf('zh') === 0 ? 'zh' : 'en';
  }

  function applyLang(lang) {
    var dict = translations[lang] || translations.zh;
    document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
    document.querySelectorAll('[data-i18n]').forEach(function (el) {
      var key = el.getAttribute('data-i18n');
      if (dict[key] !== undefined) el.innerHTML = dict[key];
    });
    document.querySelectorAll('.lang-btn').forEach(function (btn) {
      btn.classList.toggle('active', btn.getAttribute('data-lang') === lang);
    });
    try {
      localStorage.setItem(STORAGE_KEY, lang);
    } catch (e) {}
    window.__wyjLang = lang;
  }

  function initLang() {
    var lang = detectDefaultLang();
    applyLang(lang);
    document.querySelectorAll('.lang-btn').forEach(function (btn) {
      btn.addEventListener('click', function () {
        applyLang(btn.getAttribute('data-lang'));
      });
    });
  }

  function initTabs() {
    var buttons = document.querySelectorAll('.tab-btn');
    buttons.forEach(function (btn) {
      btn.addEventListener('click', function () {
        var target = btn.getAttribute('data-tab');
        buttons.forEach(function (b) { b.classList.toggle('active', b === btn); });
        document.querySelectorAll('[data-tab-panel]').forEach(function (panel) {
          panel.hidden = panel.getAttribute('data-tab-panel') !== target;
        });
      });
    });
  }

  function initCopyButtons() {
    document.querySelectorAll('.copy-btn').forEach(function (btn) {
      var targetId = btn.getAttribute('data-copy-target');
      var codeEl = document.getElementById(targetId);
      if (!codeEl) return;
      var originalIcon = btn.innerHTML;
      btn.addEventListener('click', function () {
        // Some snippets render a shell prompt for visual context. Copy an
        // explicit command when provided so the prompt is never executable
        // input accidentally.
        var explicitText = btn.getAttribute('data-copy-text');
        var text = explicitText !== null ? explicitText : codeEl.textContent;
        var done = function () {
          btn.innerHTML = '<svg viewBox="0 0 24 24" class="w-4 h-4"><use href="#i-check"/></svg>';
          setTimeout(function () { btn.innerHTML = originalIcon; }, 1600);
        };
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text).then(done).catch(function () { fallbackCopy(text); done(); });
        } else {
          fallbackCopy(text);
          done();
        }
      });
    });
  }

  function fallbackCopy(text) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); } catch (e) {}
    document.body.removeChild(ta);
  }

  function initReveal() {
    var items = document.querySelectorAll('.reveal');
    if (!('IntersectionObserver' in window)) {
      items.forEach(function (el) { el.classList.add('is-visible'); });
      return;
    }
    var observer = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            entry.target.classList.add('is-visible');
            observer.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.12, rootMargin: '0px 0px -40px 0px' }
    );
    items.forEach(function (el) { observer.observe(el); });
  }

  document.addEventListener('DOMContentLoaded', function () {
    initLang();
    initTabs();
    initCopyButtons();
    initReveal();
  });
})();
