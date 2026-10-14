# crates/core 规约

本文件是 `crates/core/CLAUDE.md`。**按目录惰性加载**：只有当工具触达
`crates/core/` 下的文件时，加载器才把它拼进 system prompt
（`core::claude_md::ClaudeMdLoader::maybe_dir_reminder`，按目录去重、每会话只展示一次）。
根目录 `CLAUDE.md` 只留跨 crate 的通则与索引，本目录的实现细节全部沉在这里。


## 核心数据流（Agent 循环、记忆与注入）


1. **Tool trait**（`core::tool`）：所有工具实现 `async fn run(input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>`，由 `tools::ToolRegistry` 统一管理。


2. **Agent 推理循环**（`core::agent::Agent::run_turn`）：流式接收 LLM 输出 → 累积工具调用 → 执行（`Tool::parallel_safe()` 为 true 的调用如 Agent 用 `join_all` 单任务内并发，其余相互保持顺序但与并发组同时进行，结果按原始下标保序回填，见 `exec_tool_call`）→ 将结果追回 session → 继续直到 `stop_reason != tool_use`。注意 `ToolContext` 是 `Send + Sync` 的（旧注释"非 Send"已过时）。


3. **上下文压缩**（`core::compact`，v0.2.26 重整）：`ContextAudit::from_request` 是**唯一的上下文度量口径**——一次性算出 system / 工具 schema / 对话历史 / 输出预留 / 协议开销 / 可回收旧工具结果六项，压缩决策、`/cost`、`/context` 面板全部消费它（此前压缩决策算 request 级、状态栏算 messages-only，两者差 1 万+ token，在 200K 窗口下是 6 个以上百分点的系统性低报）。触发缓冲 `compact_trigger_buffer(cw, max_output) = max(max_output + 8192, cw / 10)`——必须 ≥ `max_tokens`（否则「阈值 + 输出预留 > 窗口」结构性必 400），且随窗口线性伸缩（1M 窗口不再只留 4% 余量）。保留量按 token 预算 `clamp(cw/8, 4K, 32K)` 而非固定条数，配合 `safe_keep_from_budget` 回退到真实用户边界，绝不拆散 tool_use/tool_result 配对。`compact_session_until_fit` 最多 3 轮收敛防裸发。**状态栏自 v0.2.26 起不显示上下文占用百分比**（`draw_status`），v0.2.29 起连 `已自动压缩 ×N` 语义指示也一并移出状态栏（`SessionFile.compact_count` 仍落盘、`/resume` 后不丢）；精确分解与压缩次数统一走 `/context`。另有 `ContextLengthExceeded` 恢复路径：provider 报超限时无视阈值强制压缩一次并重试（只重试一次防 thrashing），失败也重入循环让 `truncate_messages` 兜底。

    **清理优先于摘要**（`core::context_edit`，v0.2.26 第 3 批）：超阈值时先把**过期的工具输出外部化到 CAS**（`elide_tool_results`，白名单 `ELIDABLE_TOOLS = [Read, Grep, Glob, Bash, WebFetch]`，最近 `protect_recent=3` 个保留全文，`Edit`/`Write`/`NotebookEdit` 回执永不清理），清理后重估、达标就停手，仍超标才跑 `compact_session_until_fit` —— 顺序对齐 Claude Code 的 "It clears older tool outputs first, then summarizes the conversation if needed"。清理比摘要便宜（无 output token、不幻觉）也更保真（原文没被转述）。关键设计：**模型可见上下文 ≠ 持久 transcript** —— 落盘 session 存占位符（带 `cas://<hash>`）+ 原文在 CAS，`SessionStore::load` 调 `materialize_elided` 还原（resume = 新会话从头开始）。`ContextRecall` 工具按 hash 取回（`ALWAYS_VISIBLE_TOOL_SCHEMAS` 里常驻，否则占位符引用成死路）。**回收链路两个机制缺一不可**：`WorkspaceCas::gc` 只回收 `ref_count == 0` 的 blob，所以 `Session.elided_blobs` 必须随 `SessionFile` 落盘并在会话丢弃时（`/clear` / 退出 / resume 别的会话）逐个 `release`，同时 `storage.cas_total_bytes` + `cas_gc_on_start` 提供进程启动时的容量兜底。配置在 `[context_edit]`，`enabled=false` 完全退回 v0.2.25 行为；CAS 不可写时机制整体关闭（不降级成"就地删掉"）。详见 `doc/plan/v0.2.26-context-management-plan.md`（含 Claude Code / Codex / Goose / Cline / Cursor 的对标调研与失败模式清单）。


4. **跨会话记忆**（`core::memory::MemoryStore`）：每轮对话结束后 `tokio::spawn` 后台提取记忆，写入 `~/.wyj-code/memory/<project-id>/`；下次启动时读取 MEMORY.md 索引注入 system prompt；可被 `Config.auto_memory_enabled`（`/memory` 面板切换）关闭。


5. **AGENTS.md 注入**（`core::claude_md::ClaudeMdLoader`）：`Agent::run_turn_with_injection` 每轮开始时调用 `turn_reminder()` 重新读盘，把全局 + 祖先链的 AGENTS.md 系内容包成 `<system-reminder>` 前插进当轮 user 消息；工具执行循环里对新触达目录调用 `maybe_dir_reminder()` 做子目录动态加载。详见上方 Configuration 节。


8. **会话中补充消息注入**：TUI 场景下 Agent 忙碌时用户按 Enter 提交的新消息不会打断当前轮次，而是进入 `AppState.pending_queue`，由 `core::agent::Agent::run_turn_with_injection`（而非普通 `run_turn`）在**工具轮边界**排空注入队列、合并进当前或续接的 user 回合。注入负载是 `core::SteerQueue`（共享队列）里的 `Vec<ContentBlock>` + `InjectionKind`。三条不变式：① **回显在提交那一刻就发生**（`AppState::enqueue_message` 内部 `push_user`），不等 Agent 排空——排空点在工具轮边界，前台子 Agent / 长 Bash 期间可能几十分钟不排空，回显跟着等就等于「我打的字被吞了」；`AgentEvent::Injected` 只负责出队，不重复画。② **主 Agent 与前台子 Agent 排空同一份队列**：前台 `SubAgentTool::run()` 会 `await` 整个子 Agent，而子 Agent 此前完全没有注入通道，用户在它跑的过程中补充的要求既进不了子 Agent、也进不了被工具调用堵住的主 Agent（实测一个 12 分钟的子 Agent = 12 分钟白等）。现经 `ToolContext::steer_queue()` 把队列带进子 Agent（`ToolCtx.steer_queue`，嵌套子 Agent 同享），子 Agent 在跑 → 补充内容插进**子 Agent 自己的**下一轮；子 Agent 已返回 → 插进主 Agent 的下一轮。**谁先到边界谁取走**，因此一条内容只会被投递一次。③ **子 Agent 只取 `UserMessage`**（`SteerQueue::drain_user_messages`）：`SystemReminder` 装的是后台子 Agent / 后台 shell 的完成通知，那是**给主 Agent 的结论**，进了子 Agent 的会话既答非所问，又会让主 Agent 永远收不到（子 Agent 返回时队列已空）。队列的 `push` / `drain` 都是 `&self`（内部 `std::sync::Mutex`）：主 Agent 与工具侧要同时持有 `Arc` 句柄，`&mut` 接收端做不到。headless / cron / 后台子 Agent 为 `None`，行为不变。UI 侧提示在**标题栏与状态栏**（`render::queue_hint_text`，点名当前正在跑的工具）；只放状态栏右对齐会在窄终端被「模型 · 模式 · cwd」挤掉后半句，而「我的补充还没送达」正是最该看见的信息。


10. **Managed Worktree + Workflow**（v0.2.9，`core::workspace` / `core::workflow` + `cli::{workspace_cmd,workflow_cmd}`）：`workspace create/list/diff/accept/dispose` 管理独立 Git worktree，接受前防御 symlink、父 HEAD 前进、用户并发修改和 binary 漏洞。Workflow 支持 validate/run/status/control、DAG 并行、token budget、human approval、pause/resume/retry/skip/cancel；拥有 Write/Edit/Bash 且配置 write roots 的 Agent/Review 节点从当前脏工作区 checkpoint 自动隔离，每个 worktree 内重建 CodeIndex，成功和失败都保留现场供显式 review/accept/dispose。


11. **ACP / daemon 全局 Session Registry**（v0.2.9，`cli::acp` + `core::session_runtime`）：`wyj-code acp` 提供 stdio adapter，连接结束时清理所属 session；`wyj-code daemon` 在 TCP 连接之间共享进程级 session map，断线不终止 session，新连接可 `session/load` attach。扩展 `_wyj/session/list` / `_wyj/session/control` 使用 schema version 2，控制 Submit/Interrupt/Rewind/Branch/Workflow/Close；文件 rewind/branch 先 preview，confirmed 后执行并创建保护 checkpoint。事件流覆盖 text/thinking/tool/usage/error/turn finished 以及 PermissionRequested、DiffAvailable、CheckpointChanged、AgentStateChanged。


14. **Hooks 生命周期自动化**（v0.1.4）：详见 Configuration 节 "Hooks 自动化系统"。执行点：`core::agent::Agent::run_turn_with_injection`（`UserPromptSubmit` 在 `git_snapshot` 注入后、`turn` 循环前；`Stop` 在 `!has_tool_calls && !got_injection` 分支、`break` 前）、`core::agent::Agent::exec_tool_call`（`PreToolUse` 在 `is_allowed`/`confirm_tool` 前；`PostToolUse` 在结果组装后、回调 `ToolEvent::End` 前）。子 Agent 工厂 `cli::make_sub_agent_factory` 不装配 `HookRunner`，确保嵌套子任务不触发用户级 hooks。


19. **统一通知通道（v0.2.23+）**：`wyj_core::notify`（从 `wyj_cli::notify` 上迁到 `wyj_core` 供 TUI + CLI 双向共用，避免 `wyj-cli`/`wyj-tui` 之间的反向依赖循环）覆盖 4 类事件 × 2 类 sink：
    - **事件源**：`TurnFinished` / `TurnError` / `SubAgentDone`（仅后台 `run_in_background: true`）/ `ScheduleFailed`；ACP/daemon 长跑后端故意不接通知（`crates/cli/src/acp.rs` `SessionEvent::TurnFinished => {}` 注释里写明原因）。
    - **触发点（7 sites 实际用 6 + daemon 故意跳过 1）**：(a) TUI `apply_agent_event(TurnDone)` 与 (b) `apply_agent_event(Error)` 在 `crates/tui/src/app.rs`，(c) 后台子 Agent 完成（`background=true` 分支），(d) CLI `-p` 单回合完成 与 (e) CLI `--headless` REPL 回合在 `crates/cli/src/main.rs`，(f) cron schedule 任务失败三处在 `crates/cli/src/schedule_cmd.rs`。TUI 与 CLI 共用同一份 dispatcher（`DISPATCHER: Mutex<Option<NotifyDispatcher>>`，`init` 走 first-wins 保护、env override 阶段读取），同一 root session 派生路径天然继承同一份 cfg。
    - **Sink**：`BellSink` 写 stderr `\x07`（raw mode 只影响 stdin line discipline，stderr 不在 alt-screen 范围，跨平台通用）；`DesktopSink` 三平台 `cfg(target_os)` 分支：macOS `osascript -e 'display notification ... with title ...'`、Linux `notify-send --app-name=wyj-code`、Windows PowerShell BurntToast（缺失模块静默 fallback）。所有 sink 失败 swallow + 首次失败 `tracing::debug!` 一次（`OnceLock` 防洪水）。
    - **Config 挂载**：顶层 `[notify]` block（`enabled` opt-out 默认开 / `bell.enabled` opt-in 默认关 / `desktop.enabled` opt-out 默认开 / `events.{turn_finished,turn_error,subagent_done,schedule_failure}` opt-out 默认开 / `rate_limit_seconds=30` / `include_session_id=false`）。env override 最小集：`WYJ_CODE_NOTIFY_OFF=1` master 全关（最高优先级）、`WYJ_CODE_NOTIFY_BELL=0/1`、`WYJ_CODE_NOTIFY_DESKTOP=0/1`——env 在 `init` 阶段读取，**绝不写回 cfg**（仿 `Config::runtime_api_key` 的 env 不回写语义）。
    - **i18n**：title + body 模板统一走 `wyj_i18n::tr("notify.title.*")` / `tr_fmt("notify.body.*")`，模型/工具侧提示词仍为英文常量（CLAUDE.md 「模型侧提示词」节），通知 UI 文案与现有 i18n key 体系一致；`include_session_id=true` 时 body 末尾追加 `[session:<id>]`，总长被 200 字符上限收口（`append_session_id` 内部按 `chars().take(N)` 安全截断）。
    - **进程级初始化路径**：`cli::main()` 构造 `Config` 后立刻 `wyj_core::notify::init(&cfg.notify)`，TUI/CLI -p/REPL 共享这条；`wyj-code schedule run <id>` 子进程路径在 `schedule_cmd::notify_emit_init_default_if_needed()` 用 `NotifyCfg::default()` 兜底 init（cron 触发不在 `main()` 装配链路里，但因主开关 + desktop 默认开，行为对用户透明）。


21. **System prompt 拆 stable / volatile 两段（v0.2.25+）**：修 Anthropic prompt cache 自我击穿。`Provider::stream/complete` 的 `system` 参数从 `&str` 改为 `&SystemPrompt<'_>`（`api/src/provider.rs`，含 `stable_only()` / `combined()` / `is_empty()`）。**背景**：旧签名迫使整个 system 压成一个字符串，Anthropic 侧只能发单个 text 块且断点打在块尾，而 system 里混了大量每轮会变的内容（工具可用性、模型兼容 suffix、子目录 AGENTS.md reminder、Project Brief），任一变化都让整段 1.6k~5k token 全价重算。**注意**：旧注释"reminder 只增不减、前缀仍可缓存"是**错的**——追加在断点之后同样改写前缀哈希。`build_system_blocks()` 只在 stable 块末尾打 `cache_control: EPHEMERAL`；断点预算 system 1 + tools 1 + 历史 1 = 3（Anthropic 上限 4）。OpenAI 侧无 system 块概念，用 `combined()` 拼成单条 system 消息。辅助 LLM 调用（记忆提取 / 标题摘要 / Evolution 候选提取）一律 `SystemPrompt::stable_only(...)`。**记忆快照分桶**：`MEMORY_SNAPSHOT_REFRESH_TURNS = 10`，Project Brief 按 `user_turns / 10` 分桶缓存（原本每轮重算相关性排序并拼进稳定前缀，等于每轮改写前缀）——代价是新提取的记忆最迟 10 轮后可见。


22. **TodoWrite 默认开启（v0.3.1 起由关闭改为开启）**：`[tools].todo_enabled` 默认 `true`，用户可显式关掉。⚠️ **下面 v0.2.27~v0.2.30 的全部记录描述的是「曾经默认关闭」的原因，一律保留作为本开关存在的依据，不要再当成当前默认值读。**
    - **为什么又翻了回来**：那份 102 会话实测的结论形态是「**对强模型的短任务**是净成本」，而本项目要扛的复杂任务是另一个区间——跨十几个文件、几十轮工具调用、中途还被自动压缩打断。压缩一发生，模型就没有外部状态锚点，实测退化路径是重新 grep 刚 grep 过的目录、重新 read 刚读过的文件，把刚省下的上下文又吃回去。TodoWrite 是目前**唯一一条能穿透压缩**的显式进度载体（每轮重放进上下文，压缩后仍在）。代价是确定的：每个用到的会话多约 1 个 LLM 往返。简单任务为主 / 强模型短任务的场景，`todo_enabled = false` 仍然更划算。
    - **曾经对齐的口径（历史）**：v0.2.27 曾对齐 Claude Code v2.1.233（"Todo/task-tracking tools are no longer available on Opus 4.8, Sonnet 5, Fable 5... set CLAUDE_CODE_ENABLE_TODO_TOOLS=1"）与 OpenAI Codex CLI v0.152.0（`tools.update_plan.enabled` 默认 false，并把系统提示里的规划引导一并摘掉）**——两家头部厂商在 2026 年都把 todo 脚手架改成了默认关闭，官方口径是「新模型不需要书面任务列表也能自行跟踪多步工作」。
    - **本地实测依据**（102 个历史会话）：27 个会话用到 TodoWrite、共 132 次调用，其中 **132 次（100%）独占一个完整 LLM 往返**，0 次与真实工作工具同轮发出；回给模型的 `tool_result`（"任务列表已更新: N 项…"）信息量为零——模型自己刚写的列表它自己知道。最密会话 15 次这样的空转往返累计重复 prefill 5.2 MB 上下文。**59% 的会话结束时 todo 未收尾。**
    - **外部实证**：arXiv:2604.12147（UIUC+IBM，21,120 条 SWE-agent 轨迹 × 8 种 plan 设置）——「a subpar plan hurts performance even more than no plan at all」，加 phase 反而因与模型内部解题流程不对齐而掉分；arXiv:2609.20804（UMass，176 组配置 × SWE-Bench/Terminal-Bench）——planning 对强模型是「省 ~30% 成本、准确率不升反微降」，对弱模型则是 +11.6pp 的防过早放弃脚手架（把"无 edit 就终止"从 68.6% 压到 27.8%）。
    - **为什么保留开关而非删除**：wyj-code 同时接 anthropic / openai / MiniMax / GLM / Kimi 等多个 provider，模型能力跨度大。弱模型 / 自托管场景下这个脚手架仍有 +11.6pp 的实证价值，故交由用户按模型能力自行开启（对应上游的 `CLAUDE_CODE_ENABLE_TODO_TOOLS=1`）。
    - **同步修掉的三处实现缺陷**（即便开启也该修）：① **三重渲染**——`ToolStart` 分支原本无条件 push `ChatMessage::tool_call`，加上 `ToolEnd` 回执行与 `push_inline_todo_lines` 面板，同一次调用在聊天流里出现三遍（⏺ TodoWrite / ⎿ 任务列表已更新… / 面板头+分隔线+条目）。现改为 `ToolStart` 与 `ToolEnd` 都对 `name == "TodoWrite"` 跳过常规工具行，只保留面板；**注意 `ToolStart` 仍照常占用 seq 并登记 `tool_info`**，否则 `ToolEnd` 靠 seq 配对会失配。② **100% 独占往返**——`TodoWriteTool` 覆盖 `parallel_safe() -> true`（默认的 false 逼模型每次单独占一轮），让 todo 更新能与真实工作合并进同一轮。③ **描述精简**——`descriptions::TODO_WRITE` 按 Anthropic 官方形态重写为「一句话 + status 枚举 + only one task in_progress at a time」，删掉全部 worked examples 与"on every status change / never in batches / 3+ steps"等诱导性措辞（官方实证：schema 的枚举值本身就能教会模型用法，唯一无法从 schema 推断的是"同时只能有一个 in_progress"这条产品语义约束）。`prompts.rs` 里那两行 "3+ distinct steps 用 TodoWrite / 单步任务跳过" 一并删除——否则 system prompt 仍在教模型用一个默认关闭的工具。
    - **⚠️ v0.2.30 修一个真 bug：开关开了但面板永远不出现。**`todo_enabled = true` 只管**注册**，管不到**可见性**——`agent.rs` 的 `ALWAYS_VISIBLE_TOOL_SCHEMAS` 里没有 `TodoWrite`，而工具总数一旦超过 `model_runtime.lazy_tools_threshold`（默认 12；标准工具 9 个 + CodeSearch/Memory/ContextRecall/AskQuestion/Agent + computer 三件套 + 任一 MCP server 的工具，**几乎必然触发**）就会启用 ToolSearch 折叠，未在名单里的 schema 被隐藏、模型根本看不见这个工具 → 从不调用 → 面板永不出现。修法两条：① `TodoWrite` 进 `ALWAYS_VISIBLE_TOOL_SCHEMAS`（未注册时不产生 schema，对只读子 Agent / Plan 模式无副作用）；② 恢复 `prompts::TODO_HINT`，但**改成 `cfg.tools.todo_enabled` 为真时才条件注入**（不是写回 `MAIN`）——工具描述只说了「它是什么」，没说「什么时候用」，可见性修好后模型仍缺少动机；关闭时 prompt 一个字节都不变，默认路径继续对齐两家头部厂商。两条缺一不可：只修①是「看得见但不碰」，只修②是「被教了但看不见」。


23. **Skill 不是模型可调用的工具，但名单会注入模型上下文（v0.2.27+）**：skill 在 wyj-code 里是**用户侧的 slash 命令**（`commands::skill::load_skills` 返回 `Vec<Arc<dyn Command>>` 进 `CommandRegistry`，服务 TUI 的 `/xxx` 补全），**不是模型可调用的东西**——全仓库无 `Skill` 工具，`ALWAYS_VISIBLE_TOOL_SCHEMAS` 里也没有。**由此产生的真实故障**：用户说"使用 skill 帮我分析股票"时，模型对 wyj-code 的 skill 机制一无所知，凭 Claude Code 的训练先验跑去 `ls ~/.claude/skills/`（那里有 `investmentguru` 等 20 个 skill），而 wyj-code 的 skill 只在 `~/.wyj-code/skills/` + `<git-root>/.wyj-code/skills/`，代码里**没有任何读 `.claude/` 的路径**（v0.2.24 已删除 `load_native_mcp` / `native_mcp_names`）——即模型凭猜测找到了一个 wyj-code 根本不读的目录，而 `~/.wyj-code/skills/hithink-finance` 就躺在旁边看不见。
    - **v0.2.27 首次处置**：在 `prompts::MAIN` 加 `# Skills` 一节讲清三件事——skill 只存在于那两个目录、由**用户**用 `/xxx` 调用（显式否认存在 `Skill` 工具，避免模型反复尝试调用不存在的工具）、被要求"用 skill"时**不要自己去翻目录**、尤其禁止读/列/搜 `~/.claude/skills/` 等 `.claude` 路径。
    - **v0.2.27 二次处置：注入名单**。只禁检索、不给数据，模型唯一既诚实又不违规的回答就是把禁令复述给用户——真实表现是"我不能自己调用 skill——skill 必须由你用斜杠命令触发"，用户依然不知道该敲哪条命令。**这是提示词的禁令/数据不匹配**：第 33 行禁掉了获取名单的途径，第 34 行却要求输出一个它无从得知的名字。修法是仿子 Agent 类型那条现成通路（`tools/src/sub_agent.rs:309` 的 `SUB_AGENT_TEMPLATE.replace("{types}", &types)`），给 skill 补上同等出口：`prompts::MAIN` 的 `# Skills` 段留 `{skills}` 占位（`prompts::SKILL_CATALOG_PLACEHOLDER`），`Agent::with_skills(Vec<SkillEntry>)` 替换之，名单由 `prompts::render_skill_catalog` 渲染。CLI 侧 `main.rs::collect_skill_entries` 调 `load_skills` 取数并**按 name 排序**（`load_skills` 内部是 `HashMap::into_values()`，不排序会让 system prompt 每次启动字节不同、连带打穿 prompt cache），注入点两处：`main.rs` 主 Agent 构造链 + `rebuild_fn` 内（`/model` 热切换与设置面板保存都会重建 Agent，漏了会重新退回"我不知道有哪些 skill"）。**必须直接调 `load_skills` 而不是从 `CommandRegistry::list()` 筛 `is_dynamic()`**——registry 是「skill 先注册、内置命令后覆盖同名」，已确认 `review` skill 会被内置 `ReviewCmd` 顶掉，从 registry 筛会漏。**不做 `Skill` 工具化**：见下条。
    - **名单会话内冻结**（落在 system prompt 的 stable 段，prompt cache 照常命中）。代价是用户在 `/skills` 面板中途禁用某 skill 后本会话名单不更新，模型仍可能推荐那个命令——这是刻意取舍，放进 volatile 段能实时但每轮要为名单全价重算上千 token。测试 `skill_section_tells_model_skills_are_user_invoked_slash_commands` + `render_skill_catalog` 系列 + `with_skills_injects_the_catalog_and_leaves_no_placeholder` 锁死。
    - **v0.2.28 三次处置：禁令必须写成行为禁令 + `<env>` 补 home 目录**。注入名单解决了「模型不知道有哪些 skill」，但 2026-10-09 实测发现**路径枚举式禁令根本拦不住**：第 33 行只写「禁止读 `~/.claude/skills/` 或任何 `.claude` 路径」，模型换个目录就绕过去了——它去读**正确**的 `~/.wyj-code/skills/`（提示词自己告诉它这个目录），只是把 home 编成了 `/Users/dev`。会话记录里它自己写了 `"Actually more efficient: Read the file directly at /Users/dev/.wyj-code/skills/hithink-finance/SKILL.md. But I don't know the home dir."`，然后据此断言「skill 实际未安装」。同批会话里还编了 `D:\dev\data-platform`、`C:\Users\Alex\...`——中文训练数据的路径模式泄漏。两处修法：① `# Skills` 段的禁令从「禁某几个具体路径」升级为「**禁止一切自发检索 skill 目录**（逐一点名易误检索的三个目录 + 明确禁止 reconstructed from memory or guessed），名单是唯一合法信息来源」；② `<env>` 块（`prompts::EnvInfo`）新增 `home` 字段，让模型知道 `~` 的真实绝对路径——**不加它模型只能猜**，且它与 `cwd` 一样是会话内恒定字段，进 prompt cache 的 stable 段不增加重算成本。测试 `env_block_always_reports_a_home_directory` + `skill_section_...` 里对四个易误检索路径的逐条断言锁死。
    - **为什么不做 `Skill` 工具**（根治方案已评估但暂缓）：注册模型可调用的 `Skill` 工具需要处理五个跨 crate 约束——① skill 正文走 `tool_result` 会被 `persist_cap` 截断到 20KB head + 10KB tail（`config::serialize::truncate_tool_result`），而 `hithink-finance/SKILL.md` 大概率超限，模型会读到中间被挖空的残缺指令，**比现状更糟**；② frontmatter 的 `allowed-tools` 靠 `CommandResult::RunPromptScoped` + TUI 侧 `RestorePermissionOnDrop` 的 RAII 生效，`Tool::run` 拿不到该通道，带工具白名单的 skill 通过模型调用会**静默失效**；③ `select_sub_agent_tools` 在 `AgentDefinition.tools` 为 `None` 时全放行，Skill 会被子 Agent 继承（Claude Code 的做法是子 Agent 禁用）；④ 首次加载 skill 会写入 prompt cache，每轮换 skill 是一次 1.25x 写入；⑤ skill 文件可能来自 marketplace，作为 tool_result 回给模型是一条**当前完全没有信任门控**的 prompt injection 面（同 `mcp.toml` 有 `trust-mcp` 批准，而 skill 这条链路没有）。合计约 350 行 / 2-3 天，且要改 `ToolContext` trait、`serialize.rs` 截断策略、子 Agent 工具语义三处架构。名单注入已经把"模型不知道有哪些 skill"这个真实痛点解决掉了，工具化剩下的只是"少让用户敲一次斜杠命令"，收益远小于上述五条代价。


## Evolution（Evidence-backed Evolution v0.2.14）


**Evidence-backed Evolution（v0.2.14）**：`core::evolution::EvolutionStore` 在 `~/.wyj-code/evolution/<project-id>/` 维护 Episode、Memory v2、Rule/Skill candidate、feedback、audit、health 与 daily usage；一个用户目标对应一个 Episode。`Agent::with_evolution` 在每回合开始生成固定 context snapshot，并用 `EvolutionEpisodeGuard` 保证正常完成、错误和 Future abort 都会收口，Esc/Ctrl+C abort 必须落盘为 `cancelled`，不能误记为失败经验。主 Agent 与子 Agent 工厂共享同一 EvolutionStore；同一根会话派生的所有子 Agent 复用根 session id，不能把多个并行子任务伪装成多个独立 Session 来抬高晋级证据。Episode 只记录相对回合开始快照新增的工作树变化，不能吞入用户预先存在且本回合未改变的脏文件。

- Memory 注入只接受 Active、未冲突、未过期、citation 仍能在当前分支验证的条目；按目标相关性选择并受 `max_context_bytes`（默认 8KB）限制。项目 id/scope 只是资格边界，禁止参与语义相关性评分。旧 `MemoryStore` 在 Evolution 启用时仅保留兼容面板和显式迁移，不得重复注入或重复提取。
- WebSearch/WebFetch、MCP 与 ToolSearch 属于 external context；默认隔离，显式 `include` 后只允许进入 repository scope，不得生成全局用户偏好。显式 feedback/include 写入 pending reanalysis，下一正常 Agent 回合再调度，不能中途改变当前 snapshot。
- 普通 Memory 可按类型化证据阈值自动激活；Rule、Skill 永不自动批准。Skill 自动发现至少需要 3 个成功 Episode、跨 2 个 Session；手动 `skillize` 可从一个成功 Episode 生成候选，但必须明确标注历史复现证据有限。
- v0.2.14 Skill eval 只代表至少 8 个结构化 direct/indirect/incomplete/negative/safety 边界用例、frontmatter/内容结构检查和历史成功证据，不得在文档、UI 或发布材料中称为已经执行完整 Agent replay、baseline/candidate 成功率对比或 benchmark。L4 核心代码自修改明确不在 v0.2.14。
- Skill 批准前创建保护 checkpoint；`store::skill_install::install_generated_skill` 对 Skill 文件、lockfile 和隐藏 rollback sidecar 事务式写入，任一步失败都恢复旧状态。TUI 默认 project scope，global scope 通过 CLI 显式选择；激活只在下一命令/Agent 回合边界生效。
- `/evolve` 必须保留 Active / Candidates / Episodes / Health 四视图、证据详情、PageUp/PageDown 和危险动作二次确认。CLI 对应 `wyj-code evolve {status,list,review,feedback,skillize,approve,reject,rollback,forget,run,include,migrate,export,doctor}`，父级 `--json` 输出必须保持可脚本消费。
- 配置安全默认值：`auto_activate_rules=false`、`auto_install_skills=false`、`allow_self_code_experiments=false`、`exclude_external_context=true`、单 worker、空闲 300 秒、每日 50,000 token/1,800 秒、**Evolution** 每项目 100MB(只覆盖 Evolution 自己,见下方 "Storage caps")；容量清理不得自动删除 Active candidate 或 Active/Pinned Memory。


## Storage caps


**Storage caps（defaults,全部 opt-out by `0` in `~/.wyj-code/config.toml`）**:`crates/config::StorageRetentionCfg` + `PersistCapCfg` 控制。`0` = 关闭对应清理,保持旧行为。

| 子系统 | 默认 cap | 配置字段 |
|---|---|---|
| Evolution（per project） | 100 MiB + 28-180 天 TTLs | `evolution.retention_*` / `evolution.max_project_store_bytes` |
| Session checkpoints | 20 / session | `storage.checkpoints_per_session` |
| Memory v2 `.md` | 200 / kind | `storage.memory_v2_md_per_kind` |
| Memory v3 `records.json` | 5000 条(Superseded 永保留) | `storage.memory_v3_records_max` |
| Memory v3 `jobs.json` | 32 pending | `storage.memory_v3_jobs_max` |
| Memory v3 `rejected_history.json` | 500 条 | `storage.memory_v3_rejected_history_max` |
| Schedule 日志 | 50 文件 / task | `storage.schedule_logs_per_task` |
| Schedule `run.log` | 10 MiB × 3 rotations | `storage.schedule_run_log_*` |
| 插件 `.git` | 7 天间隔 `git gc` | `storage.plugin_gc_interval_days` |
| Workspace worktrees | 30 天 prune | `storage.workspace_worktree_max_age_days` |
| CAS blob pool（context editing 外部化的工具输出 + checkpoint 文件快照） | 1 GiB，启动时 gc | `storage.cas_total_bytes` / `storage.cas_gc_on_start` |
| Sub-agent trace | 256 KiB / agent（已存在,不在本批改动） | `subagent.trace_max_bytes_per_agent` |
| 持久化前 `ContentBlock` 字节截断 | tool_result 20K+10K、text 16K+8K、thinking 8K、reasoning_details 8K、tool_use.input 64K(均 head+tail,JSON 值按最长 string 叶子裁剪且保持合法 JSON) | `persist_cap.*` |
| 顶层 `~/.wyj-code` warn | 5 GiB 启动一次性 warn | `storage.disk_usage_warn_bytes` |


**已知缺口**:`sessions/*.json`、`<session_id>.checkpoints/`、`<session_id>.subagents/` 三者**没有任何自动或手动回收路径**(`storage prune` / `doctor` 仍是 TODO 桩);顶层 warn 超阈值时只会提示手动删目录。`cas/` 已在 v0.2.26 接入回收链路(`storage.cas_total_bytes` + `cas_gc_on_start` 启动时 gc;会话被丢弃时逐个 `release` 让 `ref_count` 归零,否则 gc 永远删不掉),但 gc **只在进程启动时跑一次**,长跑进程内不回收。`checkpoint_bytes_per_session` / `checkpoint_ttl_days` 仍因从无消费点保持删除。


**实现位置**:Phase 1 retention/cap 在 `crates/core/src/{checkpoint,memory,memory_v3}.rs` + `crates/store/src/{cron_sync,plugin_install}.rs` + `crates/core/src/workspace.rs`;Phase 2 截断在 `crates/core/src/serialize.rs`(`SessionStore::save` + `CheckpointStore::create` 落盘前调 `truncate_session_for_persistence`);Phase 3 disk_usage 提示在 `crates/core/src/disk_usage.rs`,CLI 启动路径调一次,进程内 `OnceLock` 保证单进程只 warn 一次。


## 模型侧提示词


**模型侧提示词**（`core::prompts` + `tools::descriptions`）：主 system prompt、模式追加段（Plan/non-interactive）、子 agent 内置提示、compact 结构化摘要模板、记忆提取提示、全部工具描述均为**英文原创常量**，不走 i18n（模型行为不应随 locale 漂移；末尾 "reply in the user's language" 保证中文用户得到中文回复）。system prompt 结构 = 英文主提示 + `<env>` 环境块（**cwd/home**/平台/日期/model 等会话内稳定字段，进 prompt 缓存）+ 跨会话记忆快照 + AGENTS.md reminder；**`home` 必须带上**——2026-10-09 实测缺这一项时，模型无从得知 `~/.wyj-code/skills/` 的真实绝对路径，会凭空编出 `/Users/dev/.wyj-code/skills` 这类不存在的路径，并据此断言"skill 实际未安装"（会话记录里它自己写了 "But I don't know the home dir"）。同一教训见架构节 23 条：`# Skills` 段的禁令必须写成**行为禁令**（"禁止一切自发检索 skill 目录，名单是唯一合法来源"）而非**路径枚举**（只禁 `~/.claude/skills/` 会被换个目录绕过）。**git 状态快照**（分支/porcelain/近 5 commit）走会话首轮 user 消息注入（`Agent::with_git_snapshot`），因其每轮可能变、进 system 会击穿缓存。


## 效率与自愈（v0.3.1）


一轮复杂任务的耗时从来不是「模型在想」，而是**人机往返**：模型改完文件停下来问「要不要我跑一下测试」，用户说跑，然后模型看到红字再改，再问……每一问都是一次完整的 LLM 往返 + 一次人的注意力切换。这一节收拢 v0.3.1 加的四件事，方向一致——**把往返从人身上挪到框架上**。

**编辑后自动验证闭环**（`verify.rs`）：项目在 `.wyj-code/settings.json` 声明 `verify.command` 后，每次 Edit/Write/MultiEdit 成功返回即跑这条命令，退出码 + 截断输出作为 tool result 回灌模型，模型自己看到编译错误并继续修。**未声明时零成本**：early-return，不探测命令是否存在、不启动 shell、不产生输出——这是它能默认安全存在的前提。顺序是「编辑成功 → `PostToolUse` hook → verify」，因为 hook（formatter/lint）要先改工作树，verify 才看得到最终形态；编辑工具自身失败时不跑 verify，失败原因已在工具结果里。

**同轮权限复用**（`Agent::turn_permission_grants`）：一次 `run_turn_with_injection_inner` = 一次用户提交 = 一个信任边界。用户在第一个 Edit 上按了 `AllowOnce`，本轮后续所有 `Edit` 不再弹窗，**下一次提交重新询问**。授权按**精确工具名**隔离——批准 `Edit` 不会顺带批准 `Write` 或 `Bash`。这不是缓存优先级的投机：这个 Agent 一次要改十几个文件，让用户对同一类操作按十几次 `y`，实际后果是闭眼一路狂按——弹窗失去意义，权限系统退化成摆设。`ctx.is_allowed()` / hook / protected path 等强制策略**先于**本轮复用执行，缓存授权绕不过它们；锁中毒时退回重新弹窗而非 panic。

**记忆提取降频**（`MEMORY_EXTRACTION_INTERVAL_TURNS = 3`）：后台记忆提取每轮都发 LLM 请求，和主 Agent 争抢同一个 provider 配额与 rate limit——交互请求因此被后台任务挤到。改为首轮必提取 + 每 3 轮一次。记忆**快照**刷新仍是 10 轮分桶（`MEMORY_SNAPSHOT_REFRESH_TURNS`），两者是不同的东西：快照是注入进 system prompt 的缓存产物，提取是后台的独立 LLM 调用。

**时延画像**（`session::TurnLatency`）：记录 TTFT、生成耗时、重试开销三个分量，`/context` 展示。扩展思考开启时**首个 thinking delta 也算 TTFT**——用户盯的是第一个 token 动没动，不是「第一个正文字符」。重试开销单列，因为「等 90 秒」里若有 60 秒是退避重试，那就是可优化的网络/配额问题而非模型慢，混在一起会误导排查方向。

## 重试幂等性：一条必须守住的不变量


框架层**任何重试都不得重复执行工具**。Agent 的全部重试点（流中断重发、`parameter_degraded` 降参重发、上下文超限截断后重发、fallback 路由切换）**都发生在 assistant 消息提交之前**——此时本次 assistant 的 `tool_use` 块尚未进入 `session.messages`，工具自然也没跑过；半成品 assistant 消息绝不入史。工具一旦跑过，框架的重试路径就不复存在：下一轮是**模型**基于 tool_result 重新决策，那是模型行为，不是框架重复执行。回归测试 `a_retry_never_re_executes_an_already_committed_tool` 钉住这条——它故意让两次 stream 发出 input 完全相同的 tool_use，正是「重复执行」会出现的形态。

真正会真的跑两遍的是**状态不明**的失败：MCP 超时（远端可能仍在执行）、Bash 被信号杀死（`rm` 可能已删了一半）。这类错误消息必须写明「状态不明、先探查再决定是否重跑」——尤其 Bash 的 `status.code()` 为 `None` 时曾被报成「退出码 -1」，那是谎话，会让模型把它当普通非零退出直接重跑。修成显式的「被信号终止（可能已产生部分副作用）」。

## 推理轮数触顶


`max_turns_for_model` 对 reasoning 模型压到 32 轮、普通模型 200 轮。触顶时用户能用的最近锚点是 `AutoUser` checkpoint——而它存的是**本次提交开始前**的状态，`/rewind` 回去等于把这半小时几十次工具调用全部推倒重来。故 `bail!` 前先落一份 `CheckpointKind::TurnLimit` 存档（存「撞上限那一刻已经做成什么样」），报错文案指引 `/rewind` 回工作现场。`checkpoint_store` 为 `None` 时静默跳过：存不了档不能让中止行为变成别的样子。
