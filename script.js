(function () {
  'use strict';

  var translations = {
    zh: {
      'nav.features': '特性',
      'nav.architecture': '架构',
      'nav.install': '安装',
      'nav.changelog': '更新日志',
      'nav.github': 'GitHub',
      'nav.demo': '演示',
      'nav.compat': '模型兼容',
      'nav.audience': '适合谁',
      'nav.docs': '文档',
      'nav.security': '安全模型',
      'nav.security': '安全模型',

      'hero.kicker': '单二进制 · 零遥测 · MIT 开源',
      'hero.tagline': '跑在你自己终端里的 AI 编码助手',
      'hero.desc': '每一次工具调用都在你眼前展开，写文件之前先问你一句，任何时候都能打断。它不把判断藏起来——因为这是你自己的代码库。',
      'hero.cta.install': '60 秒安装',
      'hero.cta.demo': '先看它怎么工作 ↓',
      'hero.oneliner.label': '一键安装（macOS / Linux）：',
      'hero.oneliner.windows': 'Windows：<a href="#install" class="underline hover:text-paper/60">PowerShell 命令见下方 →</a>',


      'stats.size': 'release 二进制体积（已 strip）',
      'stats.boot': '稳态冷启动耗时',
      'stats.telemetry': '遥测 / 埋点上报',
      'stats.principles': '条设计原则，无一例外',
      'demo.foldnote': '上面是折叠态：每个工具调用只留标题行，长输出不刷屏。按 <kbd class="text-paper/60">Ctrl+O</kbd> 随时展开全文。',
      'demo.term.user': '给所有 HTTP handler 加上 tracing span',
      'demo.term.search': '搜索路由处理器',
      'demo.term.searchres': '找到 31 处，分布在 31 个文件',
      'demo.term.edit': '调用工具 <span class="text-paper">Edit</span> × 31',
      'demo.term.confirm.title': '允许此次批量编辑？',
      'demo.term.confirm.yes': '允许一次',
      'demo.term.confirm.always': '始终允许',
      'demo.term.confirm.deny': '拒绝',
      'demo.term.written': '已写入 <span class="text-phosphor">+214</span> <span class="text-[#ff8a8a]">-37</span>',
      'demo.term.run': '运行 <span class="text-paper">cargo test</span>',
      'demo.term.testresult': 'test result: ok. 148 passed',
      'quick.kicker': 'First Five Minutes',
      'quick.title': '装好之后，这四件事都做得到',
      'quick.1.title': '配好一个模型',
      'quick.1.desc': '首次启动自动打开配置面板，填入 API Key 即可。Key 只存本机，也可走环境变量，不落盘明文。',
      'quick.2.title': '问一个真问题',
      'quick.2.desc': '在你自己的仓库里问，它会先读文件再回答，读取过程逐行展开给你看，不是凭空作答。',
      'quick.3.title': '改一处早想改的地方',
      'quick.3.desc': '权限弹窗按 <kbd class="text-amber-400">y</kbd> 放行、<kbd class="text-amber-400">d</kbd> 拒绝——拒绝的信息也会回灌给它，让它换个思路。',
      'quick.4.title': '挂上自己的 MCP server',
      'quick.4.desc': '在 <code class="text-paper/70">~/.wyj-code/config.toml</code> 加一段配置，它的工具就出现在模型可用的工具列表里。',
      'compat.kicker': 'Model Compatibility',
      'compat.title': '我的模型能不能用',
      'compat.desc': '这张表按<strong>证据等级</strong>如实标注，而不是一律写「支持」。<strong>StaticOnly 意味着「我们从未在你的模型上实测过」</strong>——下面会告诉你怎么自己验证。',
      'compat.th.provider': 'Provider',
      'compat.th.protocol': '接入协议',
      'compat.th.level': '验证等级',
      'compat.th.parallel': '并行工具调用',
      'compat.th.thinking': '思考预算',
      'compat.level.reference': 'Reference',
      'compat.level.static': 'StaticOnly',
      'compat.level.experimental': 'Experimental',
      'compat.yes': '支持',
      'compat.no': '不支持',
      'compat.budget': 'token 预算',
      'compat.effort': 'effort 档',
      'compat.parallel.off': '关闭（未实测前）',
      'compat.parallel.dep': '取决于模型与启动参数',
      'compat.probe.title': '别信这张表，自己验一遍',
      'compat.probe.desc': '装好之后，静态诊断是免费的；实测会发极少量最小请求，<strong>结果写入本机缓存，之后该模型在你的机器上就标记为「已实测验证」</strong>。',
      'compat.probe.keytitle': '实测必须用临时 key',
      'compat.probe.keydesc': '实测<strong>故意不读取</strong>配置文件里已保存的凭据，只认 <code class="text-amber-400">WYJ_CODE_PROBE_API_KEY</code>。需要你自己临时轮换一个——理由是探测请求要能证明「这个 key 能用」，而不是「配置文件里那串字符还在」。',
      'audience.kicker': 'Who Is This For',
      'audience.title': '它适合谁，不适合谁',
      'audience.desc': '右边那一列同样重要——如果你正好属于它，现在就可以关掉这个页面。',
      'audience.for.title': '你可能更适合它，如果',
      'audience.for.1': '你本来就在终端里工作，希望 AI 的每一步都看得见，而不是收到一个成品 diff',
      'audience.for.2': '你在用国内模型，需要一套统一工具链，而不是为每个厂商单独适配一遍',
      'audience.for.3': '你在意工具链本身——愿意读源码、自己改工具链，甚至自己加一个模型适配',
      'audience.for.4': '你需要定时任务、后台任务与多 Agent 协作，而不是只有一问一答',
      'audience.against.title': '你可能不用换，如果',
      'audience.against.1': '你需要开箱即用的图形界面，不想面对终端',
      'audience.against.2': '你不想自己准备 API Key 并管理多模型配置',
      'audience.against.3': '你依赖某个闭源产品特有的生态、托管能力和商业支持',
      'audience.against.4': '你更在意「什么都不用管」，而不是「每一步都在你控制下」',
      'cta.install': '60 秒安装',
      'cta.docs': '想先了解实现？看文档',

      'delivery.kicker': 'Delivery Status',
      'delivery.title': 'v1.5.20 · token 账本落盘 + 上下文清理真正开始运行',
      'delivery.desc': '本次是一个<strong>先修尺子、再谈优化</strong>的版本：token 消耗此前<strong>不可测量</strong>，上下文清理此前<strong>从未运行</strong>。<br><br><strong class="text-amber-400">账本本身是坏的</strong>。内存里的 <code>Session</code> 维护了 <code>cache_read / cache_write / api_calls / tool_schema_tokens</code> 几个计数器，落盘的 <code>SessionFile</code> <strong>一个都没写</strong>——进程退出或 <code>--resume</code> 之后 <code>/cost</code> 只剩 input / output 两列，缓存命中率与「每回合多少趟 API」永久丢失，18M token 无法归因。根因不在字段本身，而在<strong>12 处构造点各自手写全量字段</strong>：新增字段不可能不漏。现收敛为 <code>from_session</code> / <code>restore_usage_from</code> 一对，漏改从此是<strong>编译期错误</strong>。跨会话汇总新增 <code>wyj-code usage [--since] [--project] [--json]</code>，并<strong>区分「没记录」与「真的是 0」</strong>——旧会话没有调用次数时显示 <code>-</code> 而不是 0，否则历史数据会被读成零消耗。<br><br><strong class="text-amber-400">上下文清理等于从未运行</strong>。129 个会话的 <code>compact_count</code> 与 <code>context_edit_freed_tokens</code> <strong>全为 0</strong>。这不是 bug 是阈值：<code>context_window = 1,000,000</code> 把压缩触发线推到 <strong>900K</strong>，而实际会话历史最大约 292K，于是机制一次都没触发，工具输出一直全量累积。现拆成两级：<strong>软阈值 <code>soft_limit_ratio = 0.55</code> 决定何时动手</strong>，硬阈值仍决定何时停手，收敛目标不变（清不动照样交给摘要）；<code>= 0</code> 完全退回旧行为并有回归测试钉住。收益是乘法的——外部化越早，每趟 API 携带的历史越小。<br><br><strong class="text-amber-400">缓存从未启用，且不能只靠开关解决</strong>。第三方 Anthropic 兼容端点此前一律判 <code>prompt_cache</code> 不支持，<code>cache_control</code> 一次都没发过，能力探测文件里记的还是 <code>protocol_default</code> 猜测、<strong>从未实测</strong>。现改为显式 <code>prompt_cache = true</code> 即可开启，并把 <code>cache_control</code> 与 <code>anthropic-beta</code> 头<strong>解耦</strong>：第三方只发前者（它们实现了前者，却会对未知 beta 头直接 400）。真被拒绝时首次 <strong>400 自动降级并重试一次</strong>；关键是<strong>上下文超限类的 400 被显式排除</strong>，否则「提示词太长」会被误判成「缓存不支持」，让用户遇到「上下文一满就永久失去缓存」。降级事实回写会话并落盘，<code>/cost</code> 据此把「没配缓存」和「配了但端点用不了」分成两种<strong>方向相反</strong>的提示。<br><br>另有<strong>并行工具能力收敛到单一写入点</strong>：<code>parallel_tool_calls</code> / <code>max_tools_per_turn</code> / <code>RequiresSingleTool</code> 表达的是同一个事实，此前分散在两处各算一半，只改其一会静默失效；<code>model doctor</code> 也改走与运行时同一份能力解析。<strong>1064 个 workspace 测试</strong> + clippy <code>-D warnings</code> 全绿。',
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
      'delivery.release': '查看 v1.5.19 Release →',
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
            'changelog.v1520': '<strong class="text-amber-400">先修一把尺子</strong>：本地 129 个会话实测 <strong>18,089,273 input / 1,767,936 output token</strong>、240 回合、平均每回合 <strong>16.7 次模型调用</strong>——但这些数字此前<strong>无法归因</strong>。<code class="text-amber-400">Session</code> 在内存里维护了 <code class="text-amber-400">cache_read / cache_write / api_calls / tool_schema_tokens</code> 几个计数器，落盘的 <code class="text-amber-400">SessionFile</code> <strong>一个都没写</strong>：进程退出或 <code class="text-amber-400">--resume</code> 之后 <code class="text-amber-400">/cost</code> 只剩 input / output 两个数，缓存命中率与「每回合多少趟 API」永久丢失。现补齐这 6 个字段（含缓存降级状态），并把 <strong>12 处各自手写全量字段的构造点收敛为 <code class="text-amber-400">from_session</code> / <code class="text-amber-400">restore_usage_from</code> 一对</strong>——新增字段漏改一处就是编译期报错，而不是像这次一样悄悄漏掉一半。跨会话汇总新增 <code class="text-amber-400">wyj-code usage [--since] [--project] [--json]</code>：按月 / 按项目聚合，并<strong>区分「没记录」与「真的是 0」</strong>（旧会话没有 api_calls 时 per-call 显示 <code class="text-amber-400">-</code> 而不是 0，否则会被读成零消耗）。',
            'changelog.v1520b': '<strong class="text-amber-400">上下文清理此前从未运行过</strong>：实测 129 个会话的 <code class="text-amber-400">compact_count</code> 与 <code class="text-amber-400">context_edit_freed_tokens</code> <strong>全为 0</strong>。根因不是 bug 而是阈值：<code class="text-amber-400">context_window = 1,000,000</code> 下压缩触发线落在 <strong>900K</strong>，而实际会话历史最大约 292K——context editing 只在越过硬阈值时才跑，于是<strong>等于从未运行</strong>，工具输出一直全量累积。现拆成两级：<strong>软阈值 <code class="text-amber-400">soft_limit_ratio = 0.55</code> 决定何时动手</strong>，硬阈值仍决定何时停手，收敛目标不变（清不动照样交给摘要），<code class="text-amber-400">= 0</code> 完全退回旧行为并有回归测试钉住。收益是乘法的——外部化越早，每趟 API 携带的历史越小。',
            'changelog.v1520c': '<strong class="text-amber-400">缓存从未启用，且不能只靠开关解决</strong>：第三方 Anthropic 兼容端点（MiniMax / GLM / Kimi）此前一律判 <code class="text-amber-400">prompt_cache</code> 不支持，<code class="text-amber-400">cache_control</code> 一次都没发过；能力探测文件里记的还是 <code class="text-amber-400">unsupported (source: protocol_default)</code>——<strong>从未实测</strong>。现改为显式 <code class="text-amber-400">prompt_cache = true</code> 即可开启，且<strong>把 <code class="text-amber-400">cache_control</code> 与 <code class="text-amber-400">anthropic-beta</code> 头解耦</strong>：第三方只发前者（它们实现了前者，却会对未知 beta 头直接 400）。真被拒绝时首次 <strong>400 自动降级并重试一次</strong>，本进程内记住该端点不支持——关键是<strong>上下文超限类的 400 被显式排除</strong>，否则「提示词太长」会被误判成「缓存不支持」，让用户遇到「上下文一满就永久失去缓存」。降级事实回写会话并落盘，<code class="text-amber-400">/cost</code> 据此把「没配缓存」和「配了但端点用不了」<strong>分成两种不同的提示</strong>——这两种情况用户该做的事完全相反。<code class="text-amber-400">--config-status</code> 另会对「未探测的超大 <code class="text-amber-400">context_window</code>」给出警告。',
            'changelog.v1520d': '<strong class="text-amber-400">并行工具能力收敛到单一写入点</strong>：<code class="text-amber-400">parallel_tool_calls</code> / <code class="text-amber-400">max_tools_per_turn</code> / <code class="text-amber-400">RequiresSingleTool</code> 三者表达的是<strong>同一个事实</strong>，此前却分散在静态目录与 live probe 两处各算一半，导致两种静默失效：只翻前者则 system prompt 仍注入「每次回复最多调用一个工具」，模型照样不批量；只翻后者则即使模型发了多个也退化成串行执行。现全部经 <code class="text-amber-400">apply_parallel_policy</code>，并配<strong>三字段同步不变式</strong>的回归钉子。<code class="text-amber-400">model doctor</code> 改走 <code class="text-amber-400">resolve_with_cache</code>（此前会把 probe 快照整体覆盖回来，连用户 profile 里的显式覆盖一起冲掉），<strong>报告的能力从此与运行时实际用的同一份</strong>；<code class="text-amber-400">max_tools_per_turn</code> 进入 <code class="text-amber-400">/model</code> 面板并有往返测试。<strong>1064 个 workspace 测试</strong> + clippy <code class="text-amber-400">-D warnings</code> 全绿。',
            'changelog.v1519': '<strong class="text-amber-400">BREAKING：项目级 skill 信任确认</strong>：<code class="text-amber-400">.wyj-code/skills/</code> 随 <code class="text-amber-400">git clone</code> 落地、正文由<strong>仓库作者</strong>控制，此前与你自己写的 skill 走同一条加载路径。本版门控强度与 <code class="text-amber-400">mcp.toml</code> 完全对齐：首次启动弹面板，<strong>不批准则这些 skill 不加载</strong>（斜杠补全不出现、模型看不见也不会推荐）；批准记录落 <code class="text-amber-400">~/.wyj-code/projects/&lt;key&gt;/skill_trust.json</code>，在仓库内容控制不到的位置；内容变化后指纹失配、重新待批准（含被 <code class="text-amber-400">git pull</code> 悄悄替换的场景）。CLI 侧 <code class="text-amber-400">trust-mcp</code> 一字不改，另设 <code class="text-amber-400">trust-skills</code> 与总入口 <code class="text-amber-400">trust</code>——信任决策必须逐类可见。',
            'changelog.v1519b': '<strong class="text-amber-400">工具结果折叠（对齐 Claude Code）</strong>：此前工具结果固定显示<strong>最多 3 个视觉行</strong>，Bash 的 JSON 输出永远只有头三行，看不到尾部的 <code class="text-amber-400">request_id</code> / <code class="text-amber-400">reason</code> / traceback 末行。现套用 v1.5.18 的 thinking 折叠模式：<strong>默认只留 <code class="text-amber-400">⏺ Bash(…)</code> 标题行、下面一行正文都不出</strong>，<code class="text-amber-400">Ctrl+O</code> 出全文、不再有任何行数上限；Edit/Write 彩色 diff、失败结果的红色 <code class="text-amber-400">· failed</code> 同理。内容加工按「是否预算启发式」分化——三行截断 <code class="text-amber-400">...</code>、错误 head+tail 的 <code class="text-amber-400">⋮</code>、空行过滤全部删除；剔除「退出码 N」横幅、Read 剥行号、Edit 只显示 diff 段、错误首行红其余淡全部保留。输入框敲 <code class="text-amber-400">!</code> 的命令输出是唯一例外，仍保持三行预览。',
            'changelog.v1519c': '<strong class="text-amber-400">修模型凭空编造 skill 路径</strong>：用户输入「请用 skills 分析股票」，模型答「技能目录 <code class="text-amber-400">/Users/dev/.wyj-code/skills</code> 在这台机器上不存在」——该路径全仓库和 skill 目录里都不存在，是模型<strong>编的</strong>（真实用户名并非 <code class="text-amber-400">dev</code>）。session 记录里抓到它的 thinking 原文：「Read the file directly at /Users/dev/… <strong>But I don\'t know the home dir</strong>」。skill 名单注入本身正常（模型能准确说出 <code class="text-amber-400">/hithink-finance</code>），暴露的是两个真缺陷：① 禁令是<strong>路径枚举</strong>、模型换个目录就绕过去了（它转而去读<strong>正确</strong>的 <code class="text-amber-400">~/.wyj-code/skills/</code>，只是把 home 编错），现改成<strong>行为禁令</strong>「禁止一切自发检索 skill 目录」；② <code class="text-amber-400">&lt;env&gt;</code> 块没有 home 目录、模型无从得知真实绝对路径，现新增 <code class="text-amber-400">Home directory</code> 字段（与会话内恒定的 cwd 同性质，进 prompt cache 的 stable 段）。',
            'changelog.v1518': '<strong class="text-amber-400">BREAKING：彻底切断 .claude/ 依赖，CLAUDE.md → AGENTS.md</strong>：wyj-code 自本版起<strong>不再读取任何 .claude/ 路径</strong>——不是「优先读 A、回退读 B」，是硬切换、不保留回退。起因是一个被实测出来的真实故障：模型凭 Claude Code 的训练先验去找 <code class="text-amber-400">~/.claude/skills/</code>（那里有 20 个 wyj-code 根本不读的 skill），而 wyj-code 自己的 <code class="text-amber-400">~/.wyj-code/skills/</code> 就躺在旁边看不见。<strong>升级必读</strong>：仓库根的 <code class="text-amber-400">CLAUDE.md</code> 不再被读取，请改名为 <code class="text-amber-400">AGENTS.md</code>（AGENTS.md 开放标准，Codex / Cursor / Gemini CLI 均采用）。查找范围为全局 <code class="text-amber-400">~/.wyj-code/AGENTS.md</code> + 从项目根到 cwd 的祖先链，每级目录内 <code class="text-amber-400">AGENTS.local.md</code> 作个人覆盖追加（不提交 git）；原「CLAUDE.md → 祖先链 → 两者皆无才读 AGENTS.md」的三级兜底<strong>塌缩为两级</strong>（回退机制本身也是模型行为漂移的来源）。hooks 三源同步迁入 <code class="text-amber-400">.wyj-code/settings.json</code> → <code class="text-amber-400">settings.local.json</code>，原 <code class="text-amber-400">.claude/settings*.json</code> 不再被读取（4 个事件语义、exit 2 = block、stdout JSON 的 decision/reason 表达全部不变）。<code class="text-amber-400">/init</code> 的生成目标同步改为 <code class="text-amber-400">AGENTS.md</code>，否则它会生成一个永远不会被读取的文件。',
      'changelog.v1518b': '<strong class="text-amber-400">skill 名单注入模型上下文</strong>：此前用户说「请使用 skill 分析股票」，模型回答「我不能自己调用 skill——skill 必须由你用斜杠命令触发」，用户依然<strong>不知道该敲哪条命令</strong>。根因是提示词同时要求了两件冲突的事——<strong>禁止</strong>模型自行检索 skill 目录、却要求它输出一个具体命令名；而 skill 名单此前只存在于 TUI 的 <code class="text-amber-400">CommandRegistry</code>（只服务 <code class="text-amber-400">/xxx</code> 补全），<strong>从不进入模型上下文</strong>。这是提示词的<strong>禁令 / 数据不匹配</strong>：拿走了获取名单的途径，却要求基于该信息输出。修法是仿子 Agent 类型那条现成通路（<code class="text-amber-400">SUB_AGENT_TEMPLATE.replace("{types}", …)</code>）给 skill 补上同等出口——<code class="text-amber-400"># Skills</code> 段留 <code class="text-amber-400">{skills}</code> 占位，<code class="text-amber-400">Agent::with_skills()</code> 在构造期替换为可用 skill 名单。名单<strong>按 name 排序</strong>后注入（<code class="text-amber-400">load_skills</code> 内部是 HashMap，不排序则每次进程启动字节不同、打穿 prompt cache），落在 system prompt 的 stable 段、会话内冻结，<strong>prompt cache 照常命中</strong>。skill 仍是用户侧斜杠命令、<strong>不是模型可调用的工具</strong>（仍无 <code class="text-amber-400">Skill</code> 工具），但模型现在能准确回答「请运行 /xxx」。',
      'changelog.v1518c': '<strong class="text-amber-400">thinking 块默认折叠</strong>：此前固定显示前 3 行正文 + 一个语义不明的 <code class="text-amber-400">· 41 lines</code> 标记，头部字符 <code class="text-amber-400">✻</code>（U+273B, Dingbats）在终端字体里缺字形画成豆腐块。现在流式期间只画一行 <code class="text-amber-400">⎿ ⠋ 思考中…</code>，<strong>完全不碰 content</strong>——旧实现每帧把整个 thinking buffer 重新折行一遍、再只取前 3 行扔掉（500 行 thinking 在 20fps 下约每秒一万次无用切分与分配）；固化后折叠成一行元信息 <code class="text-amber-400">⎿ 已思考 12s（41 行） · Ctrl+O 展开</code>，<code class="text-amber-400">Ctrl+O</code> 全局切换后出<strong>全文</strong>（旧实现取头 3 行意味着几十行思考的<strong>结论永远看不到</strong>，而 thinking 是流式追加的、结论在尾部）。行数口径一并修正：<code class="text-amber-400">N lines</code> 数的是去空行后的原始行数，与屏幕上经折行再截断的视觉行数并<strong>无对应关系</strong>，因此只能当元信息。字符换成 <code class="text-amber-400">⎿</code>（U+23BF）——与 <code class="text-amber-400">⏺</code> 同属 Miscellaneous Technical 区，字体覆盖率不是一个量级。顺带接上两个<strong>一直存在却从未被消费</strong>的东西：<code class="text-amber-400">AppState.thinking_started</code>（自 ThinkingDelta 首块写入后零读点、固化时被直接丢弃）与 i18n 里的 <code class="text-amber-400">thinking.done</code>（零引用死 key，其「只报元信息不给正文」的形态恰恰是原始设计意图，实现漂移成了「显示前 3 行」）。',
      'changelog.v1518d': '<strong class="text-amber-400">TodoWrite 任务列表默认关闭</strong>：对齐 Claude Code v2.1.233 与 OpenAI Codex CLI v0.152.0——两家头部厂商在 2026 年都已把 todo 脚手架改为默认关闭。本地实测依据是 102 个历史会话里的 <strong>132 次调用 100% 独占一个完整 LLM 往返</strong>（0 次与真实工作工具同轮发出），回给模型的「任务列表已更新」信息量为零（模型自己刚写的列表它自己知道），最密会话累计重复 prefill 5.2 MB 上下文，<strong>59% 的会话结束时 todo 未收尾</strong>。新增 <code class="text-amber-400">[tools].todo_enabled</code> 开关，弱模型 / 自托管场景仍可按需开启（外部实证：对强模型 planning 是「省 ~30% 成本、准确率不升反微降」，对弱模型则是 +11.6pp 的防过早放弃脚手架）。同时修掉三处实现缺陷：同一次调用在聊天流里<strong>渲染三遍</strong>、每次调用<strong>独占一轮</strong>往返、描述冗长诱导性措辞。<br><br><strong class="text-amber-400">工具结果预览</strong>：<code class="text-amber-400">⎿</code> 行直接展示真实错误首行，不再被「退出码 N」横幅占掉——第三方 API 的 <code class="text-amber-400">request id</code> / <code class="text-amber-400">reason</code> / traceback 末行几乎总在<strong>尾部</strong>，取头部必然丢失它们。<strong>1000 个 workspace 测试</strong> + clippy <code class="text-amber-400">-D warnings</code> 全绿。',
'changelog.v1517': '<strong class="text-amber-400">上下文管理重整：清理优先于摘要</strong>：超阈值时先把<strong>过期的工具输出外部化到 CAS</strong>，留一条带工具名 / 大小 / 首行 / <code class="text-amber-400">cas://&lt;hash&gt;</code> 的占位符，清理得动就<strong>不</strong>去跑整段摘要——顺序对齐 Claude Code 官方文档的 “It clears older tool outputs first, then summarizes the conversation if needed”。清理比摘要便宜（无 output token、不产生幻觉）也更保真：原文只是移出模型视野<strong>没被转述</strong>，而摘要最先丢的恰好是负知识（被否决的假设）、精确路径行号、顺序因果。新增 <code class="text-amber-400">ContextRecall</code> 工具按 hash 取回全文并支持分页（<code class="text-amber-400">Bash</code> / <code class="text-amber-400">WebFetch</code> 这类一次性命令重跑也拿不回来）；白名单 <code class="text-amber-400">Read / Grep / Glob / Bash / WebFetch</code>，<code class="text-amber-400">Edit</code> / <code class="text-amber-400">Write</code> / <code class="text-amber-400">NotebookEdit</code> 的回执永不清理（丢了模型会以为自己没改过从而重做），最近 3 个工具结果保留全文。<strong>模型可见上下文 ≠ 持久 transcript</strong>：落盘 session 存占位符、原文在 CAS，<code class="text-amber-400">--resume</code> 时还原成完整上下文（新会话从头开始），之后随增长再逐步重新清理；blob 已被回收时保留占位符而不是变成空洞。<code class="text-amber-400">cas_total_bytes</code> / <code class="text-amber-400">cas_gc_on_start</code> 这两个此前因「无消费点」被删的配置项恢复并接上真实生产调用点（启动时 gc + 会话丢弃时逐个 release，让 CAS 的 <code class="text-amber-400">ref_count</code> 归零）。<code class="text-amber-400">[context_edit]</code> 配置块可整体关闭退回 v1.5.16 行为；CAS 不可写时机制整体关闭而不降级成「就地删掉」。',
      'changelog.v1517b': '<strong class="text-amber-400">口径统一 + 状态栏去掉上下文占比</strong>：此前压缩决策算 request 级（system + 工具 schema + 输出预留）、状态栏只算 messages、记忆注入另有 8KB 字节硬截断——<strong>三套口径互不一致</strong>。主 system prompt 约 2,245 token + 十余个常驻工具 schema + <code class="text-amber-400">max_tokens</code> 8,192 全都不在进度条里，200K 窗口下就是 ≥6.5 个百分点的系统性低报，条的颜色和实际压缩时机对不上号（Anthropic 官方文档也明确警告过 statusline 的 <code class="text-amber-400">used_percentage</code> 不指示压缩时机；Claude Code 内置 UI 也已不再默认显示该百分比）。现在新增 <code class="text-amber-400">ContextAudit::from_request</code> 作为<strong>唯一口径</strong>，压缩决策 / <code class="text-amber-400">/cost</code> / 新增的 <code class="text-amber-400">/context</code> 面板全部消费它。状态栏<strong>彻底移除</strong> <code class="text-amber-400">[████░░░░] 78%</code>，改为 <code class="text-amber-400">已自动压缩 ×N</code> 的语义指示（计数随 <code class="text-amber-400">SessionFile.compact_count</code> 落盘，<code class="text-amber-400">/resume</code> 后不丢）——去掉占比不等于去掉可见性，「系统替我丢过历史」必须让用户知道，否则自动化就是黑盒。压缩提示词也改写成结构化系统消息（第 N 次 / 丢弃几条 / 保留几条原文 / 释放多少 token）。',
      'changelog.v1517c': '<strong class="text-amber-400">两个会导致真 400 的硬缺陷</strong>：<strong>压缩 buffer 必须 ≥ <code class="text-amber-400">max_tokens</code></strong>——旧公式 <code class="text-amber-400">min(40K, cw/5)</code> 绝对封顶 40K，而 <code class="text-amber-400">max_tokens</code> 在配置层没有任何上限钳制，<code class="text-amber-400">max_tokens = 64000</code> + <code class="text-amber-400">context_window = 200000</code> 时算出阈值 160K，加 64K 输出 = <strong>224K &gt; 200K，结构性必 400</strong>；且 1M 窗口下只留 4% 余量（阈值 96%），窗口越大反而触发越晚。改为 <code class="text-amber-400">max(max_output_tokens + 8192, cw / 10)</code>，200K / 1M 都是 90%，与 Codex CLI 的 <code class="text-amber-400">window × 0.9</code> 对齐。<strong>没有 context 超限恢复路径</strong>——<code class="text-amber-400">ProviderErrorKind::ContextLengthExceeded</code> 分类早在 <code class="text-amber-400">error.rs</code> 就有，但 <code class="text-amber-400">agent.rs</code> 的 stream 错误分支只处理 <code class="text-amber-400">UnsupportedParameter</code>（thinking 降级）和同角色模型 fallback，<strong>没有任何一处消费它</strong>：启发式一旦低估，400 就直接抛给用户；更糟的是 Anthropic 官方的 <code class="text-amber-400">prompt is too long: N tokens &gt; M maximum</code> 一句都不含旧匹配表的三个关键词，会掉进 <code class="text-amber-400">is_client_error</code> 被误判成普通 <code class="text-amber-400">InvalidRequest</code>。现在超限时无视阈值强制压缩一次并重试（只重试一次，对标 Claude Code 的 “Autocompact is thrashing” 熔断），压缩本身压不动也照常重入循环让 <code class="text-amber-400">truncate_messages</code> 兜底。压缩保留量也从固定 6 条改为随窗口伸缩的 token 预算 <code class="text-amber-400">clamp(cw/8, 4K, 32K)</code>（工具密集回合里 6 条可能就是一整个巨型 tool_result，纯对话回合里 6 条可能只有 1K token——两头都错）。',
      'changelog.v1517d': '<strong class="text-amber-400">全量移除 Jev 决策工具</strong>：<code class="text-amber-400">crates/tools/src/jev.rs</code>（约 1,240 行 / 8 文件）审计发现「实现了但没接通」——97 个历史 session 中模型 <strong>0 次</strong>调用过它，两处接线缺陷（<code class="text-amber-400">rebuild_fn</code> 漏注册、被 lazy 工具折叠），每轮固定烧约 1,150 token。已从 <code class="text-amber-400">lib.rs</code> / <code class="text-amber-400">descriptions.rs</code> / <code class="text-amber-400">prompts.rs</code> / <code class="text-amber-400">config</code> / <code class="text-amber-400">main.rs</code> / <code class="text-amber-400">builtin.rs</code>（<code class="text-amber-400">/decision</code>）/ i18n / README / CLAUDE.md / CHANGELOG 移除全部接线与文案，并从用户 <code class="text-amber-400">~/.wyj-code/config.toml</code> 删掉 <code class="text-amber-400">[tools.jev]</code> 块（原文件备份为 <code class="text-amber-400">config.toml.bak-before-jev-removal</code>）。<strong>982 个 workspace 测试</strong> + clippy <code class="text-amber-400">-D warnings</code> 全绿。',
      'changelog.v1516': '<strong class="text-amber-400">后台任务可见性与自动续跑</strong>：此前后台 subagent 的结果只暂存到下一条用户消息才注入、后台 shell 更只返回一个 <code class="text-amber-400">bash_N</code> id 靠模型自己轮询 <code class="text-amber-400">BashOutput</code>，两条路都会停在等用户输入——"派完活就不动了"。现在主循环 idle 钩子检测到后台任务完成即自动起新 turn 把结果喂回主 Agent，多任务同帧完成合并成<strong>一轮</strong>避免连续多次 LLM 往返；<code class="text-amber-400">spawn_agent_turn</code> 新增自动唤醒模式（<code class="text-amber-400">text: Option&lt;String&gt;</code>），reminder 作为这一轮唯一 user 输入而非伪装成用户发言，聊天流只显示可读的 <code class="text-amber-400">⚙</code> 系统提示。ESC 中断后<strong>暂停自动唤醒</strong>（"我明明打断了怎么又跑起来"），直到你发下一条消息才恢复。新增底部<strong class="text-amber-400">后台任务面板</strong>：有 run_in_background 任务时自动出现，<code class="text-amber-400">BashSessionManager</code> 有完整输出缓冲但 TUI 侧此前一行代码都没接，所有后台任务在界面上完全不可见——现在列表实时显示 id / 命令 / 状态 / 耗时 / 退出码，<code class="text-amber-400">Enter</code> 展开实时输出、<code class="text-amber-400">PageUp/PageDown</code> 翻页看历史（默认贴底跟随）、按 <code class="text-amber-400">k</code> 终止进程组；任务退出时输出尾部自动回显给模型。新增 <code class="text-amber-400">/shells [id]</code> 命令手动查看已结束任务。两个面板互斥时按<strong>用户焦点优先</strong>（子 Agent 与后台 shell 常同时跑，固定先后会让其中一方永远看不见也按不了 k），跨区链条 <code class="text-amber-400">Chat ↓ → Todos ↓ → SubAgents ↓ → Shells</code>。新增 <code class="text-amber-400">BackgroundJob::tail()</code> 只读快照 API——面板每帧读输出不能推进 <code class="text-amber-400">BashOutput</code> 的增量游标，否则两者互相抢对方的内容。三个坑都做了防护：退出时 <code class="text-amber-400">kill_all()</code> 会把全部 job 翻成 <code class="text-amber-400">Exited(-1)</code>，<code class="text-amber-400">should_quit</code> 守卫拦住误唤醒；后台任务<strong>跨轮次保留</strong>只在会话级重置才清（否则 dev server 会在你发新消息时从面板凭空消失）；任务列表全部完成后<strong>收成一行摘要</strong>，不再以 2 行折叠面板一直挂到下一条消息。',
      'changelog.v1516b': '<strong class="text-amber-400">Prompt cache 自我击穿修复</strong>：<code class="text-amber-400">Provider</code> 的 <code class="text-amber-400">system</code> 参数从 <code class="text-amber-400">&amp;str</code> 改为 <code class="text-amber-400">&amp;SystemPrompt&lt;_\'&gt;</code>（新增 <code class="text-amber-400">stable</code> / <code class="text-amber-400">volatile</code> 两段）。此前整个 system 压成一个字符串、Anthropic 侧只能发单个 text 块且断点打在块尾，而 system 里混了大量每轮会变的内容（工具可用性、模型兼容 suffix、子目录 CLAUDE.md reminder、Project Brief），<strong>任何一项变化都让整段 1.6k~5k token 全价重算</strong>——旧注释"reminder 只增不减、前缀仍可缓存"是错的，追加在断点之后同样改写前缀哈希。现在 stable 段末尾打 <code class="text-amber-400">cache_control: EPHEMERAL</code>、volatile 段不打（断点预算 3，仍在上限 4 内）；Project Brief 改按 <code class="text-amber-400">MEMORY_SNAPSHOT_REFRESH_TURNS = 10</code> 分桶缓存，缓存抖动降到 1/10。<strong>【源码级破坏】</strong>自定义 <code class="text-amber-400">Provider</code> 实现需改签名（辅助 LLM 调用用 <code class="text-amber-400">SystemPrompt::stable_only(...)</code> 一行迁移）。<strong>【API 移除】</strong><code class="text-amber-400">compact::COMPACT_TRIGGER_BUFFER</code> 常量改为函数。',
      'changelog.v1516c': '<strong class="text-amber-400">一批"看起来在工作、实际静默失效"的缺陷修复</strong>：<code class="text-amber-400">persist_cap</code> 的两条 JSON 上限<strong>从上线起从未生效</strong>——<code class="text-amber-400">ToolUse.input</code> 与 tool_result 的 <code class="text-amber-400">Blocks</code> 被 <code class="text-amber-400">to_string</code> 后做字符串级截断，插入的 <code class="text-amber-400">[truncated N bytes]</code> 标记使 JSON <strong>必然</strong>解析失败，而失败分支是 <code class="text-amber-400">if let Ok(...)</code> 静默吞掉，超限的 Edit/Write 参数与 Bash 输出原样进请求体并落盘；改为 <code class="text-amber-400">shrink_json_to_budget()</code> 按"最长 string 叶子"裁剪并保持合法 JSON。<strong>子 Agent 权限比主 Agent 更严</strong>：父级处于 Bypass(AutoApprove) 时子 Agent 落回默认 Prompt，而它没有审批 UI，<code class="text-amber-400">Prompt</code> + 无 UI 通道在 <code class="text-amber-400">PermissionPolicy::evaluate</code> 与 <code class="text-amber-400">confirm_tool</code> <strong>两道</strong>关卡上 fail-closed，Bash/Edit/Write 被全量拒掉；改为委派本身已在父级审批过、子 Agent 继承 AutoApprove，同时<strong>加护栏</strong>：只有被委派类型<em>可能</em>拿到副作用工具时才弹一次审批（Explore/Plan 零打扰）。<strong>孤儿 tool_use 兜底配对</strong>：模型写参数被 <code class="text-amber-400">max_tokens</code> 截断时留下无配对 <code class="text-amber-400">tool_result</code>，该历史一旦落盘会让每次 <code class="text-amber-400">--resume</code> 永久 400 且用户无法自愈；现在合成 <code class="text-amber-400">is_error: true</code> 的兜底结果。<strong>Checkpoint 配置被 TUI 侧覆盖</strong>：7 处裸 <code class="text-amber-400">new()</code> + <code class="text-amber-400">attach_agent_session</code> 覆盖装配好的 store，切换一次模型后 checkpoint 不再封顶、快照不走 CAS 去重（同 session 从 &lt;5 MB 退回 200 MB 量级）；改为进程级 <code class="text-amber-400">set_checkpoint_config()</code> + <code class="text-amber-400">configured()</code>。<strong>压缩用量漏记</strong>：压缩本身的 LLM 往返被丢弃，<code class="text-amber-400">/cost</code> 系统性低估真实花费；<strong>悬空 cwd panic</strong>：工作目录被删时 <code class="text-amber-400">current_dir()</code> 返回 Err 而调用点 <code class="text-amber-400">.unwrap()</code> panic，改为明确中文错误并<strong>刻意不做 <code class="text-amber-400">$HOME</code> 兜底</strong>（cwd 决定 project root 与会话归属，静默换目录比报错危险）；<code class="text-amber-400">/context</code> 占用率不再写死 200K；<code class="text-amber-400">agent.compacted</code> 文案从硬编码中文改走 i18n。删除 4 个<strong>从未有任何消费点的幻象配置</strong>（<code class="text-amber-400">checkpoint_bytes_per_session</code> / <code class="text-amber-400">cas_total_bytes</code> / <code class="text-amber-400">cas_gc_on_start</code> / <code class="text-amber-400">checkpoint_ttl_days</code>，迁移成本为零）——但需注意 <code class="text-amber-400">cas/</code> 与 <code class="text-amber-400">*.checkpoints/</code> 至今仍无任何回收路径。',
      'changelog.v1515': '<strong class="text-amber-400">配置加载收敛到 .wyj-code/（BREAKING）</strong>：wyj-code 自 v1.5.15 起<strong>只</strong>读写 `~/.wyj-code/`（全局）与 `<git-root>/.wyj-code/`（项目级），不再读取 `~/.claude/commands/`、`~/.claude/agents/`、`~/.claude.json`、`<cwd>/.mcp.json`、`~/.codex/` 等任何外部源。Skill 加载链由 6 层裁为 4 层（内置 → `~/.wyj-code/skills` → 插件贡献 → 项目 `~/.wyj-code/skills`），SubAgent 类型链同方向 6 层裁为 4 层；MCP 全局 `Config::load()` 不再合并 `~/.claude.json`、项目级 `merged_mcp_servers` 不再读 `<cwd>/.mcp.json`、`uninstall_mcp_server` 删"原生来源 → 落禁用记录"分支统一走"从来源文件删行 + lockfile 移除"。`/import` slash 命令 + TUI `ImportDialog` + `ExtensionCommand::Migrate` + `crates/store/src/import.rs`（700+ 行）+ `crates/config/src/codex.rs`（Codex 兼容层 150+ 行 + 5 个测试）整链路下线——用户跨来源导入需求改为手工复制粘贴到 `.wyj-code/`。同步修复 master 上 pre-existing 的 `resolve_jev_api_key_prefers_field_over_env_when_both_set` 测试失败（拆 3 个测试 + 进程级 `JEV_ENV_LOCK: Mutex<()>` 串行化）；TUI `AgentEvent::TurnDone` 分支增加清扫，把任何遗留 InProgress 的 todo 自动收口为 Completed，根治"AI 已回复但任务列表还显示进行中"的 UI 割裂。CLAUDE.md 同步更新 Skill 链 / Agent 链 / MCP 加载边界。1099 个 workspace 测试 + clippy `-D warnings` 全绿。',
      'changelog.v1514': '<strong class="text-amber-400">统一通知通道</strong>：新增 `wyj_core::notify`（从 `wyj_cli::notify` 上迁到 `wyj_core` 供 TUI + CLI 双向共用，避免 `wyj-cli`/`wyj-tui` 之间的反向依赖循环），覆盖 4 类事件（`TurnFinished`/`TurnError`/`SubAgentDone`/`ScheduleFailed`）× 2 类 sink（`BellSink` stderr `\x07` 跨平台通用 / `DesktopSink` macOS `osascript` + Linux `notify-send` + Windows PowerShell BurntToast），6/7 触发点已接入（TUI TurnDone / TUI Error / TUI 后台 SubAgentDone / CLI `-p` / CLI `--headless` REPL / cron schedule 失败；ACP/daemon 长跑后端无人类会话上下文故故意不接）。Config 顶层 `[notify]` block（`enabled`/`bell.enabled`/`desktop.enabled`/`events.*`/`rate_limit_seconds`/`include_session_id`，`bell.enabled` opt-in 默认关避扰民其余全部 opt-out 默认开，`rate_limit_seconds=30` 同类事件最小间隔防滥用）。env override 最小集 `WYJ_CODE_NOTIFY_OFF/BELL/DESKTOP`——env 在 init 阶段读取，<strong>绝不</strong>写回 cfg（沿用 `Config::resolve_jev_api_key` 模式）。所有 sink 失败 swallow + 首次失败 `tracing::debug!` 一次（`OnceLock` 防洪水）。`include_session_id=true` 时 body 末尾追加 `[session:<id>]`，总长被 200 字符上限收口。零新第三方依赖。9 项 notify 单测 + 3 项 config 单测全绿。',
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

      'cta.title': '在你的终端里试一次',
      'cta.desc': '单二进制、零遥测，克隆下来跑起来只要 60 秒。',
      'cta.github': '前往 GitHub',
      'docs.meta.title': 'wyj-code 文档 — 架构、能力与安全模型',
      'docs.back.home': '← 返回首页',
      'docs.kicker': '技术文档',
      'docs.title': '架构、能力与安全模型',
      'docs.desc': '首页只回答「要不要试」，这里回答「它到底怎么做的」。',
      'security.kicker': 'Security Model',
      'security.title': '仓库能往你上下文里塞什么，边界在哪',
      'security.desc': '这一章记录 wyj-code 的信任边界设计决策。<strong>核心问题很具体</strong>：你 <code>git clone</code> 一个陌生仓库，对方就能往你的目录里放文件——而 AI 助手的部分文件是<strong>纯文本指令</strong>，内容会作为模型指令进入对话。哪些文件可以自动生效，哪些必须先问你，是这一章的全部内容。',
      'security.desc2': '涉及信任门控的扩展点有三处：<code class="text-amber-400">mcp.toml</code>（会执行任意命令）、<code class="text-amber-400">.wyj-code/skills/</code>（正文是指语）、<code class="text-amber-400">settings.json</code> 的 hooks（会在四个时机 shell out）。前两者有显式批准流程，<strong>hooks 目前尚无门控</strong>——这是已知缺口，不是遗漏。',
      'cta.install': '60 秒安装',

      'footer.disclaimer': '个人技术作品集项目，基于公开的 Anthropic Messages API、OpenAI Chat Completions API 与 MCP 规范 clean-room 实现，不含任何第三方专有 prompt 或品牌资产，与 Anthropic / OpenAI 官方产品无关联。',

    },
    en: {
      'nav.features': 'Features',
      'nav.architecture': 'Architecture',
      'nav.install': 'Install',
      'nav.changelog': 'Changelog',
      'nav.github': 'GitHub',
      'nav.demo': 'Demo',
      'nav.compat': 'Compatibility',
      'nav.audience': 'Who it\'s for',
      'nav.docs': 'Docs',
      'nav.security': 'Security model',

      'hero.kicker': 'Single binary · Zero telemetry · MIT licensed',
      'hero.tagline': 'An AI coding assistant that lives in your terminal',
      'hero.desc': 'Every tool call unfolds in front of you, it asks before writing a file, and you can interrupt at any point. It does not hide the decisions — this is your codebase.',
      'hero.cta.install': 'Install in 60s',
      'hero.cta.demo': 'See how it works first ↓',
      'hero.oneliner.label': 'One-line install (macOS / Linux):',
      'hero.oneliner.windows': 'Windows: <a href="#install" class="underline hover:text-paper/60">see PowerShell command below →</a>',


      'stats.size': 'release binary size (stripped)',
      'stats.boot': 'steady-state cold start',
      'stats.telemetry': 'telemetry / analytics calls',
      'stats.principles': 'design principles, no exceptions',
      'demo.foldnote': 'That is the collapsed state: every tool call keeps only its title line, so long output never floods the screen. Press <kbd class="text-paper/60">Ctrl+O</kbd> for the full text at any time.',
      'demo.term.user': 'Add a tracing span to every HTTP handler',
      'demo.term.search': 'Searching route handlers',
      'demo.term.searchres': '31 matches across 31 files',
      'demo.term.edit': 'Calling tool <span class="text-paper">Edit</span> × 31',
      'demo.term.confirm.title': 'Allow this batch edit?',
      'demo.term.confirm.yes': 'Allow once',
      'demo.term.confirm.always': 'Always allow',
      'demo.term.confirm.deny': 'Deny',
      'demo.term.written': 'Wrote <span class="text-phosphor">+214</span> <span class="text-[#ff8a8a]">-37</span>',
      'demo.term.run': 'Running <span class="text-paper">cargo test</span>',
      'demo.term.testresult': 'test result: ok. 148 passed',
      'quick.kicker': 'First Five Minutes',
      'quick.title': 'Four things you can do right after installing',
      'quick.1.title': 'Configure a model',
      'quick.1.desc': 'The setup panel opens on first launch; paste an API key and you are done. Keys stay on your machine, or come from an environment variable.',
      'quick.2.title': 'Ask a real question',
      'quick.2.desc': 'Ask inside your own repository. It reads files before answering, and the reads unfold line by line in front of you.',
      'quick.3.title': 'Change something you meant to change',
      'quick.3.desc': 'Press <kbd class="text-amber-400">y</kbd> to allow, <kbd class="text-amber-400">d</kbd> to deny — a denial is fed back to the model so it tries a different approach.',
      'quick.4.title': 'Plug in your own MCP server',
      'quick.4.desc': 'Add a stanza to <code class="text-paper/70">~/.wyj-code/config.toml</code> and its tools join the list the model can call.',
      'compat.kicker': 'Model Compatibility',
      'compat.title': 'Will my model work?',
      'compat.desc': 'This table is graded by <strong>evidence level</strong>, not a blanket "supported". <strong>StaticOnly means "we have never tested this on your model"</strong> — below is how to verify it yourself.',
      'compat.th.provider': 'Provider',
      'compat.th.protocol': 'Protocol',
      'compat.th.level': 'Evidence',
      'compat.th.parallel': 'Parallel tools',
      'compat.th.thinking': 'Thinking budget',
      'compat.level.reference': 'Reference',
      'compat.level.static': 'StaticOnly',
      'compat.level.experimental': 'Experimental',
      'compat.yes': 'Supported',
      'compat.no': 'Not supported',
      'compat.budget': 'token budget',
      'compat.effort': 'effort levels',
      'compat.parallel.off': 'Off until probed',
      'compat.parallel.dep': 'Varies by model and launch flags',
      'compat.probe.title': 'Don\'t trust this table — verify it',
      'compat.probe.desc': 'After installing, static diagnosis is free. A live probe sends a handful of minimal requests; <strong>the result is cached locally, after which that model is marked verified on your machine</strong>.',
      'compat.probe.keytitle': 'A probe requires a fresh key',
      'compat.probe.keydesc': 'A probe <strong>deliberately refuses</strong> to read credentials saved in your config; it only accepts <code class="text-amber-400">WYJ_CODE_PROBE_API_KEY</code>. Rotate one yourself — the point is to prove <em>this key works</em>, not that a string is still sitting in a file.',
      'audience.kicker': 'Who Is This For',
      'audience.title': 'Who it suits, and who it does not',
      'audience.desc': 'The right-hand column matters just as much — if that is you, you can close this page now.',
      'audience.for.title': 'You will likely like it if',
      'audience.for.1': 'You already live in a terminal and want to see each AI step rather than receive a finished diff',
      'audience.for.2': 'You use Chinese-vendor models and want one toolchain instead of adapting per vendor',
      'audience.for.3': 'You care about the toolchain itself — willing to read the source, change it, even add a model adapter',
      'audience.for.4': 'You need scheduled tasks, background jobs and multi-agent work, not just question and answer',
      'audience.against.title': 'You probably should not switch if',
      'audience.against.1': 'You need a polished graphical interface and would rather not touch a terminal',
      'audience.against.2': 'You would rather not obtain an API key and manage multiple model profiles',
      'audience.against.3': 'You depend on a closed-source product\'s ecosystem, hosted features or commercial support',
      'audience.against.4': 'You care more about having nothing to manage than about every step being under your control',
      'cta.install': 'Install in 60s',
      'cta.docs': 'Curious how it works? Read the docs',

      'delivery.kicker': 'Delivery Status',
      'delivery.title': 'v1.5.20 · the token ledger persists, context trimming finally runs',
      'delivery.desc': 'This release is about <strong>fixing the ruler before optimising against it</strong>: token consumption was <strong>not measurable</strong>, and context trimming had <strong>never run once</strong>.<br><br><strong class="text-amber-400">The ledger itself was broken</strong>. The in-memory <code>Session</code> tracked <code>cache_read / cache_write / api_calls / tool_schema_tokens</code>, but the persisted <code>SessionFile</code> wrote <strong>none of them</strong> — after the process exits or on <code>--resume</code>, <code>/cost</code> kept only the input and output columns, so cache hit rate and how many API round-trips per turn were lost permanently and 18M tokens could not be attributed. The cause is not the fields but <strong>12 construction sites each writing the full field set by hand</strong>: a new field cannot but go missing. Collapsing them into a <code>from_session</code> / <code>restore_usage_from</code> pair turns that omission into a <strong>compile error</strong>. Cross-session aggregation is new: <code>wyj-code usage [--since] [--project] [--json]</code>, and it <strong>separates not-recorded from genuinely-zero</strong> — sessions predating call counting show <code>-</code>, not 0, or historical data reads as no consumption at all.<br><br><strong class="text-amber-400">Context trimming was equivalent to never running</strong>. Across 129 sessions, both <code>compact_count</code> and <code>context_edit_freed_tokens</code> were <strong>zero</strong>. That is not a bug but a threshold: <code>context_window = 1,000,000</code> pushes the compaction trigger to <strong>900K</strong>, while the largest real session history was about 292K, so the mechanism never fired once and tool output accumulated in full. It is now split in two: a <strong>soft threshold <code>soft_limit_ratio = 0.55</code> decides when to start</strong>, the hard threshold still decides when to stop, and the convergence target is unchanged (if trimming cannot help, summarising still takes over); <code>= 0</code> restores the old behaviour, pinned by a regression test. The payoff is multiplicative — the earlier stale tool output is externalised, the smaller the context each API call carries.<br><br><strong class="text-amber-400">Caching was never enabled, and a switch alone would not fix it</strong>. Third-party Anthropic-compatible endpoints were all judged <code>prompt_cache</code>-unsupported, so <code>cache_control</code> was never sent at all, and the capability cache still held a <code>protocol_default</code> guess that was <strong>never probed</strong>. It now takes an explicit <code>prompt_cache = true</code>, and <strong>decouples <code>cache_control</code> from the <code>anthropic-beta</code> header</strong>: third parties get only the former (they implement it, but reject unknown beta headers with a 400). If it is genuinely rejected, the first 400 <strong>downgrades and retries once</strong> — and crucially a <strong>context-overflow 400 is explicitly excluded</strong>, otherwise prompt-too-long would be misread as caching-unsupported and a full context would permanently lose caching. The downgrade is written back to the session and persisted, so <code>/cost</code> can tell you-did-not-configure-caching apart from you-did-and-the-endpoint-cannot — two situations that call for opposite actions.<br><br>Also, <strong>parallel tool capability now has a single write path</strong>: <code>parallel_tool_calls</code> / <code>max_tools_per_turn</code> / <code>RequiresSingleTool</code> state the same fact but were previously computed in halves across two places, so changing just one fails silently; <code>model doctor</code> now resolves capabilities through the same path the runtime uses. <strong>1064 workspace tests</strong> + clippy <code>-D warnings</code> clean.',
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
      'delivery.release': 'View the v1.5.19 Release →',
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
            'changelog.v1520': '<strong class="text-amber-400">Fix the ruler first</strong>: across 129 local sessions, <strong>18,089,273 input / 1,767,936 output tokens</strong> over 240 turns, averaging <strong>16.7 model calls per turn</strong> — and none of it could be attributed. The in-memory <code class="text-amber-400">Session</code> maintained <code class="text-amber-400">cache_read / cache_write / api_calls / tool_schema_tokens</code>, but the persisted <code class="text-amber-400">SessionFile</code> wrote <strong>none of them</strong>: after the process exits or a <code class="text-amber-400">--resume</code>, <code class="text-amber-400">/cost</code> kept only the input and output columns, so cache hit rate and round-trips per turn were lost permanently. The cause is not the fields but <strong>12 construction sites each writing the full set by hand</strong> — a new field cannot but go missing. They now collapse into a <code class="text-amber-400">from_session</code> / <code class="text-amber-400">restore_usage_from</code> pair, turning an omission into a compile error. Cross-session aggregation arrives as <code class="text-amber-400">wyj-code usage [--since] [--project] [--json]</code>, and it <strong>separates not-recorded from genuinely-zero</strong>: sessions predating call counting show <code class="text-amber-400">-</code>, not 0, or history reads as no consumption at all.',
            'changelog.v1520b': '<strong class="text-amber-400">Context trimming had never run once</strong>: across all 129 sessions, <code class="text-amber-400">compact_count</code> and <code class="text-amber-400">context_edit_freed_tokens</code> were <strong>zero</strong>. Not a bug — a threshold: <code class="text-amber-400">context_window = 1,000,000</code> pushes the compaction trigger to <strong>900K</strong>, while the largest real session history was about 292K, so the mechanism never fired and tool output accumulated in full. It is now two thresholds: a <strong>soft threshold <code class="text-amber-400">soft_limit_ratio = 0.55</code> decides when to start</strong>, the hard threshold still decides when to stop, and the convergence target is unchanged (if trimming cannot help, summarising still takes over); <code class="text-amber-400">= 0</code> restores the old behaviour, pinned by a regression test. The payoff is multiplicative — the earlier stale tool output is externalised, the smaller the context each API call carries.',
            'changelog.v1520c': '<strong class="text-amber-400">Caching was never enabled, and a switch alone would not fix it</strong>: third-party Anthropic-compatible endpoints (MiniMax / GLM / Kimi) were all judged <code class="text-amber-400">prompt_cache</code>-unsupported, so <code class="text-amber-400">cache_control</code> was never sent at all, and the capability cache still held <code class="text-amber-400">unsupported (source: protocol_default)</code> — <strong>never probed</strong>. It now takes an explicit <code class="text-amber-400">prompt_cache = true</code>, and <strong>decouples <code class="text-amber-400">cache_control</code> from the <code class="text-amber-400">anthropic-beta</code> header</strong>: third parties get only the former (they implement it, but reject unknown beta headers with a 400). If genuinely rejected, the first 400 <strong>downgrades and retries once</strong> — and crucially a <strong>context-overflow 400 is explicitly excluded</strong>, otherwise prompt-too-long would be misread as caching-unsupported and a full context would permanently lose caching. The downgrade is persisted, so <code class="text-amber-400">/cost</code> tells you-did-not-configure-caching apart from you-did-and-the-endpoint-cannot — <strong>opposite</strong> actions. <code class="text-amber-400">--config-status</code> additionally warns about an unprobed, oversized <code class="text-amber-400">context_window</code>.',
            'changelog.v1520d': '<strong class="text-amber-400">Parallel tool capability collapsed to a single write path</strong>: <code class="text-amber-400">parallel_tool_calls</code> / <code class="text-amber-400">max_tools_per_turn</code> / <code class="text-amber-400">RequiresSingleTool</code> state the same fact, yet were computed in halves across the static catalog and the live probe, producing two silent failures: flipping only the first still injects "call one tool per reply" into the system prompt and the model still batches nothing; flipping only the second still serialises even when the model emits several. All three now go through <code class="text-amber-400">apply_parallel_policy</code>, with a regression nail on the three-field invariant. <code class="text-amber-400">model doctor</code> switches to <code class="text-amber-400">resolve_with_cache</code> (it previously overwrote the resolved capabilities with the probe snapshot, wiping explicit profile overrides too), so <strong>reported capabilities are now the same document the runtime uses</strong>; <code class="text-amber-400">max_tools_per_turn</code> enters the <code class="text-amber-400">/model</code> panel with a round-trip test. <strong>1064 workspace tests</strong> + clippy <code class="text-amber-400">-D warnings</code> clean.',
            'changelog.v1519': '<strong class="text-amber-400">BREAKING: project-skill trust gate</strong>: <code class="text-amber-400">.wyj-code/skills/</code> lands with <code class="text-amber-400">git clone</code> and its body is written by the <strong>repo author</strong>; it previously loaded through exactly the same path as skills you wrote yourself. The gate now matches <code class="text-amber-400">mcp.toml</code>: a panel on first launch, and <strong>without approval these skills do not load</strong> (absent from completions, unknown to the model, never recommended). Approval lands at <code class="text-amber-400">~/.wyj-code/projects/&lt;key&gt;/skill_trust.json</code> — outside the repo\'s control — and any content change invalidates the fingerprint, including a silent <code class="text-amber-400">git pull</code> swap. <code class="text-amber-400">trust-mcp</code> is left byte-for-byte unchanged; <code class="text-amber-400">trust-skills</code> and an all-in-one <code class="text-amber-400">trust</code> are added — trust decisions must stay per-category and visible.',
            'changelog.v1519b': '<strong class="text-amber-400">Folded tool results (matching Claude Code)</strong>: tool results previously showed at most <strong>3 visual rows</strong>, so a Bash JSON output was always cut to its first lines and the trailing <code class="text-amber-400">request_id</code> / <code class="text-amber-400">reason</code> / traceback tail was invisible. They now use the thinking fold model introduced in v1.5.18: <strong>by default only the <code class="text-amber-400">⏺ Bash(…)</code> title line renders, not a single body line</strong>, and <code class="text-amber-400">Ctrl+O</code> reveals the full body with no line cap; Edit/Write colour diffs and the red <code class="text-amber-400">· failed</code> marker behave the same way. Content processing is now split by "is this a budget heuristic?": the 3-row <code class="text-amber-400">...</code> cap, the error head+tail <code class="text-amber-400">⋮</code>, and blank-line filtering are <strong>all deleted</strong>; skipping the "exit code N" banner, stripping Read line numbers, showing only the diff for Edit/Write, and red-first-line-then-dim for errors all stay. Output from <code class="text-amber-400">!</code> shell commands is the one exception and keeps its three-line preview.',
            'changelog.v1519c': '<strong class="text-amber-400">Fixed the model inventing skill paths</strong>: asking "please use skills to analyse a stock" produced "the skills directory <code class="text-amber-400">/Users/dev/.wyj-code/skills</code> does not exist on this machine" — a path that exists nowhere in the repo or the skills directory; the model <strong>made it up</strong> (the real username is not <code class="text-amber-400">dev</code>). Its own thinking is in the session log: "Read the file directly at /Users/dev/… <strong>But I don\'t know the home dir</strong>". The skill list injection itself was fine (the model named <code class="text-amber-400">/hithink-finance</code> correctly); two real defects surfaced: ① the ban was a <strong>path enumeration</strong> the model simply routed around (it went for the <strong>correct</strong> <code class="text-amber-400">~/.wyj-code/skills/</code>, just with a fabricated home) — it is now a <strong>behavioural ban</strong> on retrieving skill files at all; ② the <code class="text-amber-400">&lt;env&gt;</code> block carried no home directory, so the model had no way to learn the real absolute path — a <code class="text-amber-400">Home directory</code> field was added (session-constant like cwd, so it sits in the prompt-cache stable segment).',
            'changelog.v1518': '<strong class="text-amber-400">BREAKING: the .claude/ dependency is gone — CLAUDE.md → AGENTS.md</strong>: wyj-code now reads <strong>nothing under any .claude/ path</strong> — a hard switch, not "prefer A, fall back to B". It started with a measured failure: the model, following its Claude Code priors, went looking in <code class="text-amber-400">~/.claude/skills/</code> (20 skills wyj-code never loads) while wyj-code\'s own <code class="text-amber-400">~/.wyj-code/skills/</code> sat right next to it, unseen. <strong>Read before upgrading: a repo-root <code class="text-amber-400">CLAUDE.md</code> is no longer loaded</strong> — rename it to <code class="text-amber-400">AGENTS.md</code> (the open standard used by Codex / Cursor / Gemini CLI). Lookup covers the global <code class="text-amber-400">~/.wyj-code/AGENTS.md</code> plus the ancestor chain from repo root to cwd, with <code class="text-amber-400">AGENTS.local.md</code> as a personal, uncommitted overlay; the old three-tier fallback collapses to two. Hooks sources move to <code class="text-amber-400">.wyj-code/settings.json</code> → <code class="text-amber-400">settings.local.json</code> (all 4 events, exit 2 = block, and the stdout JSON decision/reason shape are unchanged), and <code class="text-amber-400">/init</code> now generates <code class="text-amber-400">AGENTS.md</code> instead of a file that would never be read.',
      'changelog.v1518b': '<strong class="text-amber-400">The skill list is injected into the model\'s context</strong>: previously, asking "use a skill to analyze a stock" got back "I can\'t invoke skills — you must trigger it with a slash command", leaving the user no idea <em>which</em> command to type. Root cause: the prompt forbade the model from searching for skill directories while simultaneously requiring it to name a specific command — and the skill list lived only in the TUI\'s <code class="text-amber-400">CommandRegistry</code> (for <code class="text-amber-400">/xxx</code> completion), <strong>never in the model\'s context</strong>. A textbook instruction/data mismatch. The fix mirrors the path sub-agent types already had (<code class="text-amber-400">SUB_AGENT_TEMPLATE.replace("{types}", …)</code>): the <code class="text-amber-400"># Skills</code> section keeps a <code class="text-amber-400">{skills}</code> placeholder that <code class="text-amber-400">Agent::with_skills()</code> replaces at construction time. The list is injected <strong>sorted by name</strong> (<code class="text-amber-400">load_skills</code> returns a HashMap, so unsorted it would differ on every process start and blow out the prompt cache) and lands in the stable segment of the system prompt, frozen per session, so <strong>the prompt cache still hits</strong>. Skills remain user-side slash commands, <strong>not something the model can invoke</strong> (there is still no <code class="text-amber-400">Skill</code> tool) — but the model can now answer precisely: "run /xxx".',
      'changelog.v1518c': '<strong class="text-amber-400">Thinking blocks fold by default</strong>: the old renderer always showed the first 3 lines plus a meaningless <code class="text-amber-400">· 41 lines</code> marker, and its leading <code class="text-amber-400">✻</code> (U+273B, Dingbats) rendered as a tofu box in many terminal fonts. While streaming it now draws one <code class="text-amber-400">⎿ ⠋ 思考中…</code> line and <strong>never touches the content</strong> — the old path re-wrapped the entire buffer every frame, then kept 3 lines (a 500-line thought meant ~10k pointless splits per second at 20fps). Once settled it folds to <code class="text-amber-400">⎿ 已思考 12s（41 行） · Ctrl+O 展开</code>, and <code class="text-amber-400">Ctrl+O</code> expands the <strong>full body</strong>: taking only the first 3 lines meant the <strong>conclusion</strong> of a multi-dozen-line chain of thought was never visible, even though thinking is appended as it streams and the conclusion lands at the end. The line count is re-based too — <code class="text-amber-400">N lines</code> counted non-empty source lines, which have <strong>no correspondence</strong> to the visual lines actually shown after wrapping and truncation, so it can only be metadata. The marker moves to <code class="text-amber-400">⎿</code> (U+23BF), same coverage tier as <code class="text-amber-400">⏺</code>. Two long-dormant things get wired up along the way: <code class="text-amber-400">AppState.thinking_started</code> (written on the first ThinkingDelta, then discarded at flush with zero readers) and the i18n key <code class="text-amber-400">thinking.done</code> (a dead key whose "metadata only, no body" shape was the original design intent before the implementation drifted to "show the first 3 lines").',
      'changelog.v1518d': '<strong class="text-amber-400">TodoWrite is off by default</strong>, matching Claude Code v2.1.233 and OpenAI Codex CLI v0.152.0 — both vendors flipped the todo scaffolding to off during 2026. The local evidence: across 102 historical sessions, <strong>all 132 calls consumed a full LLM round-trip exclusively</strong> (never batched with real work tools) and the "task list updated" tool result carried zero information (the model just wrote the list, it knows); the densest session repeated 5.2 MB of prefilled context, and <strong>59% of sessions ended with todos unfinished</strong>. A <code class="text-amber-400">[tools].todo_enabled</code> switch keeps it available for weaker / self-hosted models (external evidence: planning saves ~30% cost on strong models while giving weak models +11.6pp against premature abandonment). Three implementation defects fixed along the way: the same call rendered <strong>three times</strong> in the chat stream, every call <strong>occupied its own round-trip</strong>, and the description carried verbose leading language.<br><br><strong class="text-amber-400">Tool-result previews</strong>: the <code class="text-amber-400">⎿</code> line now shows the real first line of the error instead of an "exit code N" banner — third-party API <code class="text-amber-400">request id</code> / <code class="text-amber-400">reason</code> and traceback tails are almost always at the <strong>end</strong>, so taking the head necessarily lost them. <strong>1000 workspace tests</strong> + clippy <code class="text-amber-400">-D warnings</code> clean.',
'changelog.v1517': '<strong class="text-amber-400">Context management rebuilt: clearing before summarizing</strong>: once the context fills up, <strong>stale tool outputs are externalized to CAS</strong> and replaced by a placeholder carrying the tool name, size, first line and a <code class="text-amber-400">cas://&lt;hash&gt;</code> reference; if that alone gets under the limit, <strong>no</strong> summarization round-trip is spent — the same order Claude Code\'s documentation describes (“It clears older tool outputs first, then summarizes the conversation if needed”). Clearing is cheaper (no output tokens, no hallucination) and far more faithful: the original text only leaves the model\'s view <strong>instead of being paraphrased</strong>, and paraphrase is exactly what loses negative knowledge (hypotheses already ruled out), exact paths/line numbers and causal ordering. The new <code class="text-amber-400">ContextRecall</code> tool fetches the full text by hash with pagination — essential for one-shot <code class="text-amber-400">Bash</code> / <code class="text-amber-400">WebFetch</code> output, which re-running cannot reproduce. Allow-list: <code class="text-amber-400">Read / Grep / Glob / Bash / WebFetch</code>; <code class="text-amber-400">Edit</code> / <code class="text-amber-400">Write</code> / <code class="text-amber-400">NotebookEdit</code> receipts are never cleared (losing those makes the model think its edit never happened and redo the work), and the last 3 tool results keep their full text. <strong>Model-visible context ≠ durable transcript</strong>: the persisted session stores placeholders while the originals live in CAS, and <code class="text-amber-400">--resume</code> restores the full text (a new session starts from complete context) which then gets re-cleared as it grows; if a blob was already reclaimed the placeholder is kept rather than turning into a hole. <code class="text-amber-400">cas_total_bytes</code> / <code class="text-amber-400">cas_gc_on_start</code> — previously deleted for having “no consumer” — are back with a real production call site (gc at startup plus per-blob release when a session is discarded, driving CAS <code class="text-amber-400">ref_count</code> back to zero). The <code class="text-amber-400">[context_edit]</code> config block can disable the whole mechanism to revert to v1.5.16 behaviour; if CAS is unwritable the mechanism turns off entirely rather than degrading to “delete in place”.',
      'changelog.v1517b': '<strong class="text-amber-400">One accounting basis + no more context percentage in the status bar</strong>: previously the compaction decision used a request-level estimate (system + tool schemas + output reserve), the status bar counted messages only, and memory injection had its own 8 KB byte cap — <strong>three mutually inconsistent measures</strong>. The ~2,245-token main system prompt, a dozen+ always-attached tool schemas and the 8,192-token <code class="text-amber-400">max_tokens</code> reserve were all invisible to the bar, which on a 200K window is a systematic understatement of 6+ percentage points — the bar\'s colour never matched when compaction actually fired (Anthropic\'s own docs warn that a statusline\'s <code class="text-amber-400">used_percentage</code> does not indicate when compaction runs, and Claude Code\'s built-in UI no longer shows a percentage by default). The new <code class="text-amber-400">ContextAudit::from_request</code> is now the <strong>single basis</strong> consumed by compaction, <code class="text-amber-400">/cost</code> and the new <code class="text-amber-400">/context</code> command. The status bar <strong>drops <code class="text-amber-400">[████░░░░] 78%</code> entirely</strong> and shows a semantic <code class="text-amber-400">auto-compacted ×N</code> indicator instead (persisted in <code class="text-amber-400">SessionFile.compact_count</code>, so it survives <code class="text-amber-400">/resume</code>) — removing the number is not the same as removing visibility: “the system dropped history on my behalf” still has to be visible, otherwise automated context management is a black box. The compaction notice is now a structured system message (Nth time / messages dropped / messages kept verbatim / tokens freed).',
      'changelog.v1517c': '<strong class="text-amber-400">Two latent bugs that could really 400</strong>: <strong>the compaction buffer must be ≥ <code class="text-amber-400">max_tokens</code></strong> — the old <code class="text-amber-400">min(40K, cw/5)</code> capped absolutely at 40K while <code class="text-amber-400">max_tokens</code> has no upper clamp in the config layer, so <code class="text-amber-400">max_tokens = 64000</code> on a 200K window produced a 160K threshold + 64K output = <strong>224K &gt; 200K, structurally over budget</strong>; on a 1M window it also left only 4% headroom (fired at 96%) — the bigger the window, the later it triggered. Now <code class="text-amber-400">max(max_output_tokens + 8192, cw / 10)</code>: 90% at both 200K and 1M, matching Codex CLI\'s <code class="text-amber-400">window × 0.9</code>. <strong>No recovery path for a genuine context overflow</strong> — <code class="text-amber-400">ProviderErrorKind::ContextLengthExceeded</code> existed in <code class="text-amber-400">error.rs</code> all along, but <code class="text-amber-400">agent.rs</code> only handled <code class="text-amber-400">UnsupportedParameter</code> (thinking downgrade) and same-role model failover, so <strong>nothing consumed it</strong>: an under-estimating heuristic just surfaced the 400. Worse, Anthropic\'s own <code class="text-amber-400">prompt is too long: N tokens &gt; M maximum</code> wording contains none of the old matcher\'s keywords and fell through to plain <code class="text-amber-400">InvalidRequest</code>. Compaction now runs once ignoring the threshold and retries (once only — the same guard as Claude Code\'s “Autocompact is thrashing” circuit breaker), and even if compaction cannot shrink anything it still re-enters the loop so <code class="text-amber-400">truncate_messages</code> can handle “too few messages but one huge block”. The retained tail also moved from a fixed 6 messages to a window-scaled token budget <code class="text-amber-400">clamp(cw/8, 4K, 32K)</code> — 6 messages can be one giant tool result in a tool-dense turn and barely 1K tokens in a chatty one, so the old rule was wrong at both ends.',
      'changelog.v1517d': '<strong class="text-amber-400">Removed the Jev decision tool entirely</strong>: auditing <code class="text-amber-400">crates/tools/src/jev.rs</code> (~1,240 lines across 8 files) found it was “implemented but never wired” — across 97 historical sessions the model invoked it <strong>0 times</strong>, and two wiring defects kept it out (the <code class="text-amber-400">rebuild_fn</code> path never registered it, and the lazy-tool fold hid it past a threshold of 12), while burning ~1,150 tokens per turn. All wiring and copy removed from <code class="text-amber-400">lib.rs</code> / <code class="text-amber-400">descriptions.rs</code> / <code class="text-amber-400">prompts.rs</code> / <code class="text-amber-400">config</code> / <code class="text-amber-400">main.rs</code> / <code class="text-amber-400">builtin.rs</code> (<code class="text-amber-400">/decision</code>) / i18n / README / CLAUDE.md / CHANGELOG, and the <code class="text-amber-400">[tools.jev]</code> block was removed from the user\'s <code class="text-amber-400">~/.wyj-code/config.toml</code> (original backed up as <code class="text-amber-400">config.toml.bak-before-jev-removal</code>). <strong>982 workspace tests</strong> + clippy <code class="text-amber-400">-D warnings</code> all green.',
      'changelog.v1516': '<strong class="text-amber-400">Background task visibility and auto-resume</strong>: previously a background subagent\'s result was only injected when you sent the next message, and a background shell merely returned a <code class="text-amber-400">bash_N</code> id for the model to poll via <code class="text-amber-400">BashOutput</code> — both paths stalled waiting for human input ("it dispatches the work and then sits there"). The main loop\'s idle hook now starts a new turn and feeds the result back to the main agent as soon as a background task finishes; tasks completing in the same frame are merged into <strong>one</strong> turn to avoid repeated LLM round-trips. <code class="text-amber-400">spawn_agent_turn</code> gains an auto-resume mode (<code class="text-amber-400">text: Option&lt;String&gt;</code>) so the reminder becomes that turn\'s only user input rather than masquerading as something the user said; the chat stream shows only a readable <code class="text-amber-400">⚙</code> system notice. After an ESC interrupt auto-resume <strong>pauses</strong> ("I clearly stopped it, why is it running again?") until you send the next message. New bottom <strong class="text-amber-400">background shell panel</strong>: appears automatically whenever a run_in_background task exists — <code class="text-amber-400">BashSessionManager</code> already had a full output buffer but the TUI side had zero lines wired up, so every background task was completely invisible in the UI. The list now shows id / command / status / elapsed / exit code live; <code class="text-amber-400">Enter</code> expands the live output, <code class="text-amber-400">PageUp/PageDown</code> pages through history (following the tail by default), and <code class="text-amber-400">k</code> terminates the process group; the output tail is echoed back to the model on exit. A new <code class="text-amber-400">/shells [id]</code> command opens finished tasks. When the two panels compete for the bottom slot, <strong>user focus wins</strong> (background subagents and background shells very often run together, and a fixed order would leave one of them permanently invisible and unkillable); the cross-region chain is <code class="text-amber-400">Chat ↓ → Todos ↓ → SubAgents ↓ → Shells</code>. A new read-only snapshot API <code class="text-amber-400">BackgroundJob::tail()</code> — the panel cannot advance <code class="text-amber-400">BashOutput</code>\'s incremental cursor, or the two would steal each other\'s content. Three traps are guarded: on exit <code class="text-amber-400">kill_all()</code> flips every job to <code class="text-amber-400">Exited(-1)</code>, so a <code class="text-amber-400">should_quit</code> guard blocks spurious wake-ups; background tasks are <strong>kept across turns</strong> and only cleared on a session-level reset (otherwise a dev server would vanish from the panel the moment you sent a new message); and a fully completed task list <strong>collapses to a one-line summary</strong> instead of lingering as a two-line folded panel until the next message.',
      'changelog.v1516b': '<strong class="text-amber-400">Prompt cache self-invalidation fixed</strong>: the <code class="text-amber-400">Provider</code> trait\'s <code class="text-amber-400">system</code> parameter changes from <code class="text-amber-400">&amp;str</code> to <code class="text-amber-400">&amp;SystemPrompt&lt;_\'&gt;</code> (new <code class="text-amber-400">stable</code> / <code class="text-amber-400">volatile</code> segments). The whole system prompt used to collapse into one string, so Anthropic could only emit a single text block with the breakpoint at its tail — while that content mixed in plenty of per-turn-varying material (tool availability, model-compat suffixes, subdirectory CLAUDE.md reminders, the Project Brief), and <strong>any single change re-priced the entire 1.6k–5k token block</strong>. The old comment claiming "reminders only append, so the prefix still caches" was wrong: appending after a breakpoint rewrites that prefix hash just the same. The stable segment now carries <code class="text-amber-400">cache_control: EPHEMERAL</code> and the volatile one does not (3 breakpoints, still within the limit of 4); the Project Brief is cached in buckets of <code class="text-amber-400">MEMORY_SNAPSHOT_REFRESH_TURNS = 10</code>, cutting cache churn to one tenth. <strong>[source-level breaking]</strong> custom <code class="text-amber-400">Provider</code> implementations must change signature (auxiliary LLM calls migrate in one line via <code class="text-amber-400">SystemPrompt::stable_only(...)</code>). <strong>[API removal]</strong> the <code class="text-amber-400">compact::COMPACT_TRIGGER_BUFFER</code> constant becomes a function.',
      'changelog.v1516c': '<strong class="text-amber-400">A batch of defects that looked like they worked but were silently no-ops</strong>: two <code class="text-amber-400">persist_cap</code> JSON limits <strong>never took effect from the day they shipped</strong> — <code class="text-amber-400">ToolUse.input</code> and tool_result <code class="text-amber-400">Blocks</code> were <code class="text-amber-400">to_string</code>-ed and then string-truncated, and the inserted <code class="text-amber-400">[truncated N bytes]</code> marker made the JSON <strong>certainly</strong> unparseable — with the failure branch being a silent <code class="text-amber-400">if let Ok(...)</code>, so oversized Edit/Write arguments and Bash output went into the request body and onto disk verbatim; now <code class="text-amber-400">shrink_json_to_budget()</code> trims the longest string leaf while keeping the JSON valid. <strong>Sub-agent permissions were stricter than the parent agent\'s</strong>: with the parent in Bypass (AutoApprove) a sub-agent fell back to the default Prompt mode, and since it has no approval UI, Prompt plus no UI channel fails closed at <strong>two</strong> gates (<code class="text-amber-400">PermissionPolicy::evaluate</code> and <code class="text-amber-400">confirm_tool</code>), so Bash/Edit/Write were rejected outright; the delegation itself is already approved by the parent, so the sub-agent now inherits AutoApprove — with a <strong>guardrail</strong>: an approval prompt appears only when the delegated type <em>could</em> receive side-effecting tools (Explore/Plan stay interruption-free). <strong>Orphaned tool_use pairing</strong>: a tool call truncated by <code class="text-amber-400">max_tokens</code> left a dangling <code class="text-amber-400">tool_result</code>, and once that history was persisted every <code class="text-amber-400">--resume</code> returned a permanent 400 with no user-recoverable path; a synthetic <code class="text-amber-400">is_error: true</code> result is now emitted instead. <strong>Checkpoint configuration overwritten on the TUI side</strong>: seven bare <code class="text-amber-400">new()</code> call sites plus <code class="text-amber-400">attach_agent_session</code> clobbered the properly configured store, so after a single model switch checkpoints stopped being capped and snapshots stopped going through CAS deduplication (a session regressed from under 5 MB to the 200 MB range); now a process-level <code class="text-amber-400">set_checkpoint_config()</code> plus <code class="text-amber-400">configured()</code> is used. <strong>Compaction usage went unrecorded</strong>: the LLM round-trip behind compaction was discarded, so <code class="text-amber-400">/cost</code> systematically underestimated real spend. <strong>Suspended cwd panic</strong>: when the working directory has been deleted, <code class="text-amber-400">current_dir()</code> returns an error that the call site <code class="text-amber-400">.unwrap()</code>s into a panic; it now reports a clear error and <strong>deliberately does not fall back to <code class="text-amber-400">$HOME</code></strong> (cwd decides the project root and session ownership, so silently switching directories is worse than failing). <code class="text-amber-400">/context</code> occupancy no longer hardcodes 200K, and the <code class="text-amber-400">agent.compacted</code> message moved from hardcoded Chinese to i18n. Four <strong>phantom config keys that never had any consumer</strong> are removed (<code class="text-amber-400">checkpoint_bytes_per_session</code> / <code class="text-amber-400">cas_total_bytes</code> / <code class="text-amber-400">cas_gc_on_start</code> / <code class="text-amber-400">checkpoint_ttl_days</code>, zero migration cost) — but note that <code class="text-amber-400">cas/</code> and <code class="text-amber-400">*.checkpoints/</code> still have no reclamation path at all.',
      'changelog.v1515': '<strong class="text-amber-400">Config loading scoped to .wyj-code/ (BREAKING)</strong>: from v1.5.15 onward wyj-code reads and writes <strong>only</strong> `~/.wyj-code/` (global) and `<git-root>/.wyj-code/` (project-scoped); it no longer reads `~/.claude/commands/`, `~/.claude/agents/`, `~/.claude.json`, `<cwd>/.mcp.json`, or `~/.codex/`. The Skill loader chain shrinks from six layers to four (builtin → `~/.wyj-code/skills` → plugin contributions → project `~/.wyj-code/skills`), and the SubAgent type chain gets the same six-to-four trim. Globally, `Config::load()` no longer merges `~/.claude.json`; project-scoped `merged_mcp_servers` no longer reads `<cwd>/.mcp.json`; `uninstall_mcp_server` drops its "native origin → disable record" branch in favor of a single "remove the line from the source file and remove the lockfile entry" path. The entire `/import` slash command, the TUI `ImportDialog`, `ExtensionCommand::Migrate`, `crates/store/src/import.rs` (700+ lines), and the Codex compatibility layer in `crates/config/src/codex.rs` (150+ lines plus 5 tests) are retired — cross-source imports become a manual copy/paste into `.wyj-code/`. The pre-existing `resolve_jev_api_key_prefers_field_over_env_when_both_set` test failure on master is fixed in the same release (split into 3 tests plus a process-level `JEV_ENV_LOCK: Mutex<()>` for serialization). The TUI `AgentEvent::TurnDone` branch gains a cleanup pass that auto-completes any todo still in `InProgress`, curing the "the AI has replied but the task list still shows in-progress" UX glitch. CLAUDE.md is updated to reflect the new Skill / Agent / MCP loading boundaries. 1099 workspace tests + `clippy -D warnings` clean.',
      'changelog.v1514': '<strong class="text-amber-400">Unified notification channel</strong>: new `wyj_core::notify` (moved up from `wyj_cli::notify` so both the TUI and the CLI can share it without an inverted `wyj-cli` / `wyj-tui` dependency cycle) covers four event kinds — `TurnFinished` / `TurnError` / `SubAgentDone` / `ScheduleFailed` — across two sinks (`BellSink` writes stderr `\x07`, universally portable; `DesktopSink` dispatches to macOS `osascript`, Linux `notify-send`, and Windows PowerShell BurntToast). Six of seven trigger points are wired (TUI TurnDone, TUI Error, background SubAgentDone, CLI `-p`, CLI `--headless` REPL, and cron schedule failures; the ACP / daemon long-running backend intentionally stays silent — it has no human session context). A new top-level `[notify]` block in the Config (`enabled` / `bell.enabled` / `desktop.enabled` / `events.*` / `rate_limit_seconds` / `include_session_id`) ships with `bell.enabled` opt-in (off by default to stay polite in shared TUI sessions) and every other field opt-out (on by default); `rate_limit_seconds = 30` dedupes same-kind events. The env override set is the minimal `WYJ_CODE_NOTIFY_OFF` / `_BELL` / `_DESKTOP`, read at init time and <strong>never</strong> written back into the cfg (same pattern as `Config::resolve_jev_api_key`). Every sink failure is swallowed and only logged once via `tracing::debug!` (gated by `OnceLock` to avoid log floods). When `include_session_id = true` the body gains a `[session:<id>]` suffix, capped to a 200-character total. Zero new third-party dependencies. 9 notify unit tests + 3 config tests cover the contract.',
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
      'cta.desc': 'A single binary with zero telemetry — 60 seconds from clone to running.',
      'cta.github': 'Go to GitHub',
      'docs.meta.title': 'wyj-code Docs — Architecture, Capabilities & Security',
      'docs.back.home': '← Back to home',
      'docs.kicker': 'Documentation',
      'docs.title': 'Architecture, capabilities & security model',
      'docs.desc': 'The home page answers "should I try it". This one answers "how does it actually work".',
      'security.kicker': 'Security Model',
      'security.title': 'What a repository can put into your context, and where the line sits',
      'security.desc': 'This chapter records how wyj-code draws its trust boundaries. <strong>The question is concrete</strong>: you <code>git clone</code> an unfamiliar repo and the other party can drop files into your directory — and some of those files are <strong>plain-text instructions</strong> that enter the conversation as model instructions. Which files take effect automatically, and which must ask you first, is the whole of this chapter.',
      'security.desc2': 'Three extension points carry trust decisions: <code class="text-amber-400">mcp.toml</code> (executes arbitrary commands), <code class="text-amber-400">.wyj-code/skills/</code> (the body is instructions), and hooks in <code class="text-amber-400">settings.json</code> (shell out at four lifecycle points). The first two have an explicit approval flow; <strong>hooks have no gate yet</strong> — a known gap, not an oversight.',
      'cta.install': 'Install in 60s',

      'footer.disclaimer': 'A personal engineering portfolio project, clean-room implemented against the public Anthropic Messages API, OpenAI Chat Completions API, and MCP specifications. Contains no third-party proprietary prompts or brand assets, and is not affiliated with Anthropic or OpenAI.',

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
