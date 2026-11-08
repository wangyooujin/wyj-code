# crates/tui

渲染与主循环。颜色只允许出现在 `theme.rs`。模型侧文案不在这里。

## 主循环

- UI 线程**不能** `await` session 锁。回合 task 会把这把锁持有几分钟。用 `try_session_lock`。
- 会阻塞的斜杠命令实现 `needs_background()`，放进后台 task，做完用 `AgentEvent` 把结果送回主循环。返回 `RunPrompt` 的命令不能标这个（`/review` 因此保持 inline）。
- `pending_queue` 只是 `SteerQueue` 的镜像。主 Agent 和前台子 Agent 都会取走消息，水位必须按真实队列对账，不能假设只有一处出队。
- 自动唤醒回合的 `text` 传 `None`。把 reminder 原文当用户消息写进历史，`/resume` 后还会显示。
- 退出轮询必须看 `should_quit`。否则 `kill_all` 会把每个后台任务误报成刚完成。
- 忙碌时不要把 `/new` 这类命令的字面文本排进模型。`runs_while_busy` 的默认与白名单见根文件。
- 内容区焦点的兜底分支不要 `continue` 吞掉 `Ctrl+C`。`Ctrl+C` 仍要落到 `interrupt()`。
- 面板里列出的 shell / 子 Agent 必须和后台真实存活的一致。口径只有 `running_shells()` 与 `visible_sub_agents()`，完成即从列表消失。读后台输出用 `tail`，不要用会推进游标的 `read_new`。

## 配色与排版

- 生产代码不许出现裸 `Color::`（含 Reset / Indexed）。例外只有欢迎页 logo 的渐变，走 `Theme` 的 RGB 辅助函数。新增或移动生产文件必须登记 `theme_tests::production_source`，否则扫描会漏掉整个文件。
- 新增槽位同时接 `ThemePalette`、`Default`、`from_json`、getter。`from_json` 对未知键静默忽略，漏接就是主题半截生效且无报错。全部槽位用 RGB，不用具名色。
- `inline_code` 不要并回 `code_fg`。Bypass 的状态栏芯片、标题栏、输入框边框三处都是 `danger`。状态栏不设显式背景。宽度按显示列宽算，不用 `chars().count()`；右段放不下就整段丢掉。
- 思考档位没设过，或当前端点不认 effort 时，状态栏不显示档位。
- `NO_COLOR` 只清颜色，保留粗体等修饰。非 truecolor 时 RGB 量化到 256 色，灰走灰阶。
- 聊天高度按**物理行**（含折行）算。内容不足一屏时贴底画。不要用 `para.line_count()` 扫整段历史。
- 折叠态不得读取 `content` 做预览。工具结果默认只画标题行，`Ctrl+O` 才出全文。`!` bash 输出是例外：全文，不折叠。
- Assistant 与 Thinking **纯文本折行**，不做 markdown 结构转换。流式 thinking 钉在输入框上方固定码头（不进聊天流）；定稿 Thinking 仍在历史，折叠头 + 尾摘要，外层缩进 4 列。流式视口固定 4 物理行（1 状态 + `THINKING_LIVE_TAIL_LINES`；不足垫空、超出取尾）。展开态按宽度缓存；流式不缓存。
- Bash 的完整命令单独成行，标题行里的摘要是截断过的。工具耗时走 `fmt_duration_precise`。
- 系统通知用 `markdown::wrap_plain_text_spans` 折行。不要交给 `Paragraph` 的 `WordWrapper`（中文没有空格，会被拦腰截断）。
- 子 Agent 详情是把 trace **翻译**成聊天消息再走同一条渲染管线，按事件条数缓存。不要在面板里铺原始 trace。交接摘要一行，报错进正文。
- 实时工具输出两道闸：`tool_use_id` 已登记，且 `is_thinking`。中断后泵仍可能推来尾帧。
- 聊天活动固定 **Compact**：成功探索/命令收成一行回合摘要；**Edit/Write 永不折叠**，始终画在输出区。未聚焦任务时 `Ctrl+O`（`output_expanded`）展开回合细节、定稿 thinking 与工具正文。失败的 Bash/命令折叠态不单独成行（展开后可见）；其它失败、进行中、授权、Plan、AskQuestion、用户 `!` bash 仍露出。`/density` 已删除；`[ui].activity_density` 仅兼容旧配置，渲染忽略。只改呈现层，不改 `ChatMessage` 入库。

## 输入与鼠标

- 文本编辑只有 `input::apply_edit_key` 一份。多行面板必须把每一行画出来。
- 输入框按光标开窗，上限约 8 行正文。附件占位符在输入框和聊天区必须逐字节相同。
- `[ui].mouse` 默认 `auto`（只有 `TERM` 空或 `dumb` 才关）。捕获鼠标后，选择和复制由应用做，松开写入系统剪贴板。拖选不跨内容区与输入框。滚动不清除选区，选区按锚点跟着文字走。带修饰位的字符键不是正文（`Cmd+C` 不能变成插入 `c`）。
- `Shift+↑` 进内容区：用 `contains(SHIFT)` 且排除 Control/Alt/Super，不要用 `modifiers == SHIFT`。判定要在补全菜单之前。
- 斜杠补全只匹配第一个命令名，参数不参与查询。前缀优先，其次是有序子序列和相邻笔误；单字符只做前缀。命令名已写对且后面有参数时收起菜单。接受建议时替换命令名、保留参数。不要在 TUI 里把候选再按名字重排，排序在 `CommandRegistry::complete`。
- 欢迎页不重复 model / cwd。每一行显示宽度含缩进和图标，必须 `<=` 栏宽。窄终端整条丢掉命令，不要截断成半个命令名。logo 与版本号紧挨着，和提示区之间空两行。

## 面板

- `/profile` 的字段表是 `PROFILE_FIELD_SPECS`。顺序即渲染顺序。新字段要同时接读写 match，并且 `en.yml` / `zh.yml` 都有 label key（缺 key 会原样显示 key）。枚举候选第一项是空串，表示跟随默认。思考字段按线协议分流，不要两套都铺出来。模型字段 Enter 能手填，不能只有拉取菜单。
- 「仅本会话使用」的分组存在 `session_profile_override`，不写 `config.toml`。任何写盘路径用 `state.config`，不用叠了覆盖的有效配置。状态栏要显示分组名。切换成功后收起面板。
- `/evolve` 保持 Active / Candidates / Episodes / Health 四视图。危险动作二次确认。
- Todo 插在本轮工作消息之后、最终助手正文 / `streaming_buf` 之前（不是流尾、不是底部面板）。三层：任务标题 → 步骤摘要 → 执行内容。Enter/Space 聚焦任务后展开步骤+执行；Ctrl+T / Ctrl+O 在已聚焦时分别切步骤 / 执行层。进行中任务默认展开步骤；执行层默认折叠。有 tasklist 时已归因的 ToolCall/ToolResult 从主聊天归并到任务下。全部完成时摘要只留一行，Ctrl+T 可再展开整表。`TurnDone` 不清 `current_todos`；正常结束才把残留的 in_progress 补成完成，截断的回合不要补（见 `core` 的 `turn_truncated`）。
- 子 Agent 内嵌在聊天流：贴在各自 Agent ToolCall 正下方，固定 8 行视口（`agents_inline::AGENT_LIVE_VIEWPORT`）follow_tail；不再占底部 `BottomPanel::SubAgents`，也不整屏盖住主聊天。完成后视口消失，主聊天只留 Agent ToolResult / handoff（轨迹不灌进主历史）。本轮全部完成时留一行 `agents [N/N]` 摘要，Ctrl+A 展开整表（有 agents 时 Ctrl+A 优先于行首跳转）。前后台同一套 UI；后台完成通知保留。
- 新建分组的 `max_tools_per_turn` 预填 4。留空会让国内端点按目录判成每轮 1 个工具。

## 渲染时不要退回去的行为

- `draw_chat` 只把视口里的行交给 `Paragraph`。`scroll` 是 `u16`，整段历史塞进去会卡，也会滚不到底。高度按物理行算。
- 未聚焦任务时 `Ctrl+O` 是「自动 ↔ 全展开」，不是「全折叠 ↔ 全展开」。正在执行的步骤保持展开。Edit/Write 折叠态仍显示最多约 20 行 diff；不要把这个例外扩到 Bash 这类大输出。聚焦任务时 `Ctrl+O` 只切该任务执行层。
- 退出前排空在途的按键 release，panic hook 无条件 `PopKeyboardEnhancementFlags`。否则 Ghostty 的 `CSI u` 会漏进 shell，提示符后多出 `9;5;3u`。
- 鼠标报告只开 1000/1002/1006，不要开 1003，也不要改用 crossterm 的 `EnableMouseCapture`。
- 不要再试 Inline viewport 来做贴底。那条路已经因为光标查询竞态失败过。
- 标题栏 token 只累加 `UsageDelta`。`Usage` 要到回合结束才到，用它做差会整轮显示 0。新工具要补 `tool_activity.*` 的中英条文案。
- 链接画在最终 Buffer 上，不要把 OSC 8 写进 `Span`。打开命令按单个参数调用，不经 shell 拼接。
- `/new` 不能把字面命令排进模型。清会话时把后台 shell 记进 `stale_shells` 再 kill，只 kill 不记名单，下一帧会把它们当成新任务唤醒。
- 子 Agent / shell 的内容区焦点要先吞掉 `Ctrl+C`，否则浏览轨迹时会把整轮和前台子 Agent 一起杀掉。Chat 焦点下 `Ctrl+C` 仍是中断。
- 详情滚动用 `follow_tail`，不要把 `scroll` 取反。上滚必须离开实时尾部。
- `panic_guard::install` 在进入备用屏幕后 `mark_active`。按字节截断字符串先走 `floor_char_boundary`。
- 源码扫描测试定位函数体时按花括号配平，不要用「下一个顶格 `fn`」当边界。
