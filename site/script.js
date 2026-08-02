(function () {
  'use strict';

  var translations = {
    zh: {
      'nav.features': '特性',
      'nav.architecture': '架构',
      'nav.install': '安装',
      'nav.changelog': '更新日志',
      'nav.github': 'GitHub',

      'hero.kicker': '个人工程作品 · Clean-room 实现 · 开源',
      'hero.tagline': '用 Rust 从零实现的终端 AI 编程助手',
      'hero.desc': '单二进制、原生 TUI、国内模型可信运行时、OS 级安全执行与多 Agent 协作——重点适配 GLM、MiniMax、Kimi、DeepSeek、Qwen 和豆包，同时兼容 Claude、OpenAI 与本地端点。',
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
      'feature.computer.desc': '模型截图观察桌面、合成鼠标点击拖拽与键盘输入操控本机 GUI（macOS / Windows），官方端点与第三方 Anthropic 协议兼容端点均可用。',
      'feature.workflow.title': 'Workflow + 隔离 Worktree',
      'feature.workflow.desc': 'DAG 支持并行、预算、审批、暂停、重试与取消；有写权限的编码节点从脏工作区 checkpoint 自动创建独立 worktree，变更需显式 review/accept。',
      'feature.acp.title': 'ACP / daemon 控制面',
      'feature.acp.desc': 'stdio ACP 与本地 TCP daemon 共享前端无关事件协议；daemon session 跨连接存活，可重连、列出、提交、打断、rewind、branch 和关闭。',
      'feature.plugin.title': 'Plugin Runtime + LSP',
      'feature.plugin.desc': '插件可事务式贡献 hooks、styles、themes、channels、LSP、monitors 与 settings；真实 <code class="text-amber-400">workspace/symbol</code> 与本地索引合并。',
      'feature.review.title': '本地 Review 证据',
      'feature.review.desc': '<code class="text-amber-400">review run</code> 对 commit/PR diff 生成可审计 JSON，处理 rename、空格路径和 binary，并对 secret evidence 脱敏。',

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
      'changelog.latest': '最新',
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
      'nav.features': 'Features',
      'nav.architecture': 'Architecture',
      'nav.install': 'Install',
      'nav.changelog': 'Changelog',
      'nav.github': 'GitHub',

      'hero.kicker': 'Personal engineering project · Clean-room implementation · Open source',
      'hero.tagline': 'A terminal AI coding agent, built from scratch in Rust',
      'hero.desc': 'A single-binary terminal coding agent with a native TUI, capability-aware Chinese-model runtime, OS-level sandboxing, and controllable sub-agents — focused on GLM, MiniMax, Kimi, DeepSeek, Qwen, and Doubao while remaining compatible with Claude, OpenAI, and local endpoints.',
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
      'feature.computer.desc': 'The model views the desktop via screenshots and drives mouse clicks/drags and keyboard input to control the local GUI (macOS / Windows), on both the official endpoint and third-party Anthropic-protocol-compatible endpoints.',
      'feature.workflow.title': 'Workflow + isolated worktrees',
      'feature.workflow.desc': 'DAG execution supports parallelism, budgets, approvals, pause, retry, and cancellation. Write-capable coding nodes checkpoint the dirty checkout into isolated worktrees and require explicit review/accept.',
      'feature.acp.title': 'ACP / daemon control plane',
      'feature.acp.desc': 'A stdio ACP adapter and local TCP daemon share a frontend-neutral event protocol. Daemon sessions survive disconnects and can be reattached, listed, submitted, interrupted, rewound, branched, or closed.',
      'feature.plugin.title': 'Plugin runtime + LSP',
      'feature.plugin.desc': 'Plugins transactionally contribute hooks, styles, themes, channels, LSP, monitors, and settings. Real <code class="text-amber-400">workspace/symbol</code> results merge with the local index.',
      'feature.review.title': 'Local review evidence',
      'feature.review.desc': '<code class="text-amber-400">review run</code> emits auditable JSON for commit/PR diffs, including renames, spaced paths, and binaries, with secret evidence redacted.',

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
      'changelog.latest': 'Latest',
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
