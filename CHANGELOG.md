# Changelog

本文件记录 wyj-code 各版本的主要变更，按版本从新到旧排列。

> **版本号已重编（自 0.3.1 起）**：旧的 `1.x.y` 编号不再延续，改用 `0.x.y`。
> 每条标题括注了它在旧编号下的原版本号，便于对照历史 release 与 commit。
> 映射规则是**按历史发布顺序连续编号**：`1.0.0~1.2.3` → `0.1.1~0.1.8`、
> `1.3.0~1.5.25` → `0.2.1~0.2.34`；没有写进本文件的补丁版本同样占号
> （如 1.5.8 → 0.2.17），所以编号序列里会看到空档——那是刻意保留的，
> 而不是漏发。**GitHub 上的旧 tag 仍是 `v1.x.y`**，改名之前历史 release 链接
> 与自更新比较（`store::self_update::is_newer`）都仍按旧编号走。

## [Unreleased]

## [0.3.2] - 2026-10-14

### Grep 工具改造成两级搜索后端：装了就用 ripgrep，没装也有得用

`GrepTool` 此前只有一条纯 Rust 路径（`ignore::WalkBuilder` + `regex`），全量读文件
逐行匹配。现在做成**两级、对外行为一致**的后端，`rg_binary` 只在 `PATH` 里探测一次，
没装 ripgrep 的机器自动落回纯 Rust 实现；测试里 `assert_both_ways` 对同一输入断言
两条路径的输出一致。

**rg 快路径**：`rg --color=never --no-heading -H --hidden --no-messages -g '!.git'
-g '!.git/**' -e <pattern> -- <root>`，流式读 stdout，命中 `head_limit` / 500 匹配
上限后直接 `kill` 掉子进程（不读完，避免白扫整棵树）；`files_with_matches` / `count`
模式额外按路径排序，抵消多线程遍历的乱序。三条硬约定：

- 子进程 `RIPGREP_CONFIG_PATH=""`。否则用户自己的 rg 配置（如 `--smart-case`）会
  **静默改变匹配语义**——同一个 pattern 在两台机器上给出不同结果，且没有任何报错。
- `-H` 必须带。否则 rg 搜**单个文件**时默认只打行号不打路径，输出直接不可解析。
- rg 插在相邻上下文组之间的 `--` 分隔符要剔除（原生实现不输出它）。

rg 退出码 ≥2（非法 pattern / glob）视为「rg 搞不定」，落回纯 Rust，而不是把 rg 的
错误原样透给用户。

**纯 Rust 兜底**：`ignore::WalkBuilder::build_parallel()`（线程数 =
`available_parallelism`，上限 16，命中上限后 `WalkState::Quit`）+ regex-automata
字面量预筛。预筛用 `regex_syntax` 在 Hir 上抽字面量（`Prefilter::new(MatchKind::
LeftmostFirst, …)`，走 memchr / memmem）：**先整文件字节级粗筛**，没有候选就连二进制
NUL 扫描与 UTF-8 校验都省掉，再逐行过滤后才进 regex 引擎。

**大小写不敏感必须用 `ParserBuilder::case_insensitive(true)` 解析出已折叠的 Hir
再抽字面量**——不能把 needle 简单转小写，那会漏掉 ſ / İ / K 这类 Unicode 折叠特例，
造成**静默漏报**：该匹配却报告「没找到」，比慢严重得多。抽不出字面量、或模式可匹配
空串时返回 `None` 退回逐行 `is_match`，宁可慢不能漏。

**忽略语义两条路径必须一致**（这是改造的另一半收益）：跳过 `.git` 目录，
`.gitignore` / `.ignore` / `.git/info/exclude` 在任何目录都生效。旧实现
`hidden(false)` 会把 `.git/objects` 一起遍历（本仓库 710 个文件里 473 个是 `.git`
内部文件），且 ignore crate 的 `require_git` 默认 true——**非 git 仓库里
`.gitignore` 静默失效**。

实测（release、Apple Silicon）：本仓库常规搜索 10~30×；832MB / 3 万文件的语料上
全量扫描 rg 快路径 4~11×、纯 Rust 兜底 2~7×。纯 Rust 兜底在大文件全量扫描时仍受
「整份 read 拷贝」限制（~600MB/s），没有走 mmap——`memmap2::Mmap::map` 是 unsafe，
且文件被并发截断时会 SIGBUS（进程直接死），不值得为兜底路径冒这个险。

### TUI 底部进度区文案此前根本没走 i18n

输入框标题栏 spinner、TodoWrite 面板头、排队提示、thinking 状态行——这一整块底部
区域此前是硬编码的中英混排（中文界面里夹着 `Waiting for approval`、
`Queuing message`、`Thinking`、`ctrl+t to expand`），切到英文则是另一种混排。新增
`spinner.*` 键族收敛这一片文案，`crates/i18n` 的测试对两个 locale 各查一遍并断言
`{count}` / `{running}` / `{op}` / `{frame}` 四种占位符都接得上——rust-i18n 未解析时
会**原样回显 key**，那正是「漏了一半翻译」最难被发现的形态。

`thinking.running` 同时从写死第一帧改成 `{frame}` 占位符，帧号由渲染层注入。原来那行
在流式期间**永远停在 `⠋`**：用户往回滚看历史时整屏只有标题栏在动，看起来像一行静态
文字。

### 权限弹窗开着的时候，标题栏却显示「Bash(cargo test) · 12.3s」

`Agent::exec_tool_call` 是先 `emit ToolEvent::Start`（TUI 据此写 `current_op`）**之后**
才 `ensure_tool_permission`。所以权限弹窗打开期间 `current_op` 必然非空，而旧优先级把
`current_op` 排在对话框前面——秒表继续走、看着「在忙」，实则整轮卡在等用户点 y。
`plan_dialog` 同构（`ExitPlanMode` 也是先 Start 再弹窗）。现在两个对话框都排到
`current_op` 前面，「等待批准 / 审阅计划」如实显示。

### 窄终端下第一个消失的偏偏是「我的补充还没送达」

旧实现先 `format!` 把整串拼好再交给 `truncate_line`，而后者是**从尾部**截的——排队提示
恰好拼在最后，于是实测 80 列以下第一个消失的就是它，与「这正是用户最需要看到的信息」
的判断正好相反。新增 `build_thinking_title` 改为**按整段丢弃 + 优先级**：
`tokens`(0) < `elapsed`(1) < `queue`(2)。token 是本轮用量、另有 `/cost` 可查，耗时可由
状态栏旁证，唯独排队提示一旦整段消失，用户就无从确认自己的补充有没有被接收。全部丢完
仍超宽时，最后才对 `label` 做省略号截断——它是唯一不可丢弃的一段。无排队时的排版与旧
实现**逐字符一致**：底部进度条的形态是用户长期习惯的，不该顺手改掉。

**1253 个 workspace 测试**（1241 → 1253）+ clippy `-D warnings` 全绿。

## [0.3.1] - 2026-10-14

### 编辑后自动验证闭环：把「跑一下测试」从人的嘴里挪到框架里

复杂任务里模型最高频的失败模式不是不会写，而是**不知道自己写错了**：改完十个
文件直接交付，没有编译器告诉它第五个文件的类型不匹配。新增 `core::verify`，
项目在 `.wyj-code/settings.json` 里声明校验命令（与 hooks 同三源合并，后者覆盖
前者；单值覆盖而非追加）：

```json
{ "verify": { "command": "cargo check --quiet --all-targets" } }
```

**未声明 = 完全静默**：不跑子进程、不产生 tool_result 字节、不改变任何行为——
零假设是这个特性的第一原则。命令不硬编码成 `cargo test`：wyj-code 同时服务
Python / Node / Go 项目，且同一项目改十个文件跑十次全量测试的代价无法接受。

### TodoWrite 默认开启

`[tools].todo_enabled` 默认由 `false` 改为 `true`，用户可显式关掉。

### `/context` 增加每轮时延画像

`core::session::TurnLatency` 记录上一趟请求的首 token 延迟、总耗时与其中的重试
开销。跟着 `AgentEvent::Usage` 走而不是新增事件——两者采集点与生命周期完全相同，
拆开只会让发送侧多做一半发送次数。面板据此把「慢」拆成「模型慢」与「重试慢」：
重试与退避的时间会单列，不计入推理耗时。

### 推理轮数触顶前自动存档

新增 `CheckpointKind::TurnLimit`：达到推理轮数上限、自动存档之前先落一次
checkpoint，避免触顶收尾时的状态丢失。

### MCP 超时错误措辞：状态不明 ≠ 调用失败

MCP 工具调用超时只说明**我们这边没收到响应**，远端可能仍在执行、也可能早已执行
完毕。错误文案明确写出这一点，避免模型把超时读成「没执行」而条件反射重试
一个有副作用的工具。

## [0.2.34] - 2026-10-13（原 1.5.25）

### `/cost` 里那张表其实根本没渲染出来，还把子 Agent 的开销整段漏掉了

改这批之前先复现了一次「`/cost` 显示」：在 headless 下打印是对的表格，切到 TUI 就塌成**六行**，列全是歪的。两条独立的根因，一条是渲染管线，一条是排版本身。

**① TUI 把表格揉成了一段散文**。TUI 收到 assistant 消息后交给 markdown 渲染，而 markdown 把「连续非空行」当一个段落、合并后再折行——一张 50 行的成本表在它眼里只是一段文字，于是原样折行、加行距、并到相邻段落里。headless 走裸 `println`、**不经过这一步**，所以只在 headless 验证会全部漏过。

新增 `CommandResult::OutputPlain(String)` 与 `MessageRole::Preformatted`：逐行原样渲染，只在单行超宽时截断，**不折行**。刻意不复用代码块围栏——那会给整块加 `╭─╮` 边框并再缩进 4 列，表格反而更窄。渲染层也**不再补缩进**：内容自带版式，再补一截会让标题行与表格行错开。钉子 `real_cost_output_survives_the_tui_render_pipeline` 把真实的 `/cost` 输出过一遍真实渲染管线，断言渲染行数 == 源行数。

**② 列宽是排版问题，不是翻译问题**。旧实现把对齐硬编码进 i18n 模板（`"  输入:   {input} tokens"`），靠中文字符恰好占 2 列来凑齐——切到英文立刻散架（`Input` 有缩进而 `Output`/`Total` 没有）。新增 `crates/commands/src/cost_view.rs` 作为独立排版引擎：宽度在 Rust 侧按**显示宽度**算（`unicode-width`，与 `tui/render.rs` 同版本，口径必须和实际渲染一致），标签只负责"叫什么"。宽度自适应聊天区可用列数（`CommandContext::terminal_width`，窄终端下先压标签列、再退化为省略号，**数字列永远保底**——它们才是用户真正要读的数）；宽度只在入口归一化一次，否则各子部件各自夹一次、拼起来必然超宽。

几处刻意如此：

- **未知 ≠ 0**。此前未上报的费用显示成 `~$0.0000`，那会被读成「这项免费」；现显示 `—` 占位并在脚注说明「金额为下界」。但**排除 `DOWNGRADED`**——那是端点拒绝 `cache_control`、缓存整个停用，与「能用但不报写入量」是两回事，混起来会误导。
- **无单价必须显式提示**而不是静默降级成「只有 token 数」——静默降级会让用户以为那就是完整账单。
- 费用固定 4 位小数：塌成 `0.0000` 同样会被误读成「没花钱」。
- 数字列 `pad_start` **超宽时原样返回**：宁可破坏对齐也不能截错数字。
- 宽度 < 32 列截断、> 160 列封顶（后者是不让超宽终端下表格拉成一行几百列的散沙）。

### 子 Agent 的 token 用量此前整段没进主会话口径

`AgentEvent::Usage` 只由主 Agent 发射，子 Agent 跑在自己的 `Session` 上——所以 `/cost` **系统性漏掉整棵子 Agent 子树**，而扇出的开销恰恰不在少数。TUI 侧原本有 `SubAgentEvent::Usage` 那条路，但它**不含 cache_read**、且 `/resume` 之后归零；子 Agent 的开销大头恰恰是缓存命中，等于漏上加漏。

新增 `core::session::SubUsage`（4 个 `AtomicU32` 的累加器）+ `SharedSubUsage` 句柄：

- **用原子量而非 Mutex**：子 Agent 可能 `parallel_safe` 并发跑；`take` 走 `swap(0)`，每个 token 只计一次，**天然幂等**。
- **逐层透传、不重复计数**：每层只把自己那层 `Session` 的 `total_*` 加进累加器，各层天然不相交，嵌套子 Agent 同样汇总到根。
- **抽干时机**必须等这一批工具全部 join 完，而不是单个工具返回时。
- `fork_for_surface`（ACP / daemon）**必须清掉**累加器，否则别的会话开销会记到本会话头上；钉子 `drain_moves_subagent_usage_into_session_without_touching_main_totals` 守着这件事。
- TUI 侧累加器句柄必须**跟着 task 走**而不是挂在 `ToolContext` 上活一回合——ctx 每回合重建，否则子 Agent 写完即随 ctx 丢弃，累加器永远是空的（这是实现时最先踩的坑）。
- `/clear` 会连带清空子 Agent 计数（换对话但计费也归零），但**保留** `prompt_cache_state`（它描述运行环境而非对话内容）。

`sub_*` 四个字段全部 `#[serde(default)]` 落盘，`/resume` 后继续累加；`wyj-code usage` 汇总与插件 channel 事件 payload、`WYJ_STATS_JSON` 同步补上。

### `/context` 面板改用同一套排版引擎

明细行（System prompt / 工具定义 / 对话历史 / 输出预留 / 协议开销）与合计行改走同一个 `cost_view::Table`，合计、百分比与阈值和自动压缩**同源**；headless 侧改读当前 Profile 的窗口而不是写死 200K。旧实现用 `{label:<18}` 补位——那是按**字符个数**补的，中文每字占 2 列，必然歪。

顺带修一处覆盖缺口：`session_store.rs` 的落盘回归测试逐字段比对了 cache / api_calls / tool_schema / prompt_cache_state / compact_count / context_edit，但**没有比对**新加的 4 个 `sub_*` 字段。

**1221 个 workspace 测试**（1184 → 1221） + clippy `-D warnings` 全绿。

## [0.2.33] - 2026-10-13（原 1.5.24）

### 长命令跑到一半，界面什么都不给：前台 Bash 的输出改为实时推进

`Bash(cargo build)`、`Bash(pytest)`、`Bash(npm run dev)` 这类命令此前只有两种状态：刚点下去的一瞬，和跑完之后的一大坨。中间那几分钟里界面只有一个转圈——用户既看不出命令在干什么，也判断不出它是死了还是在慢慢跑，而「命令还在动」这件事本来是唯一能让人安心的信号。更难受的是**中途没有任何回头路**：输出没到手，就不知道该不该中断。

改动集中在 `BashTool`：它改为**自己抽管道**，并在抽字节的同一个循环里按 100ms 节流把 stdout / stderr 的**累计尾部快照**推给 UI，TUI 收到后按 `sequence_no` 挂到对应的 `⏺` 行下方实时渲染，跑完即收起、由折叠态的 `⎿` 结果行接管正文。

几个刻意如此、且各自都有钉子的取舍：

- **推「快照」而不是「增量」**。每帧自带完整累计输出，所以通道满时 `try_send` 直接丢帧也是无损的——下一帧覆盖上一帧。反过来若传增量，丢一帧就是永久丢一段数据。工具侧一律**不 await** 这个通道：`send().await` 在工具 future 被 drop（ESC 中断）时会连带把工具挂死。
- **节流比较「累计字节数」而不是「快照长度」**。输出超过 8KB 保尾上限后快照长度会钉死在上限不变，只比长度会让 `cargo build` 这种持续刷新的命令视图**冻结在半途中**——这是最容易写错、也最不容易被测出来的一处。
- **抽字节与推快照合流在这一个循环里**，而不是另起一个 ticker task。回合被 ESC 中断时 `h.abort()` 掉的是回合 future，另起的 ticker 会变成孤儿、永远按周期往 TUI 推陈旧数据。合流后生命周期锚在管道 EOF 上：命令结束，泵自然结束。
- **尾部截断按 UTF-8 字符边界回退**。直接 `String::from_utf8_lossy(&bytes[len - max..])` 会在多字节字符中间切开，产出一串 U+FFFD 替换符（中文 / emoji 高发）。
- **渲染层只画尾部 30 行**（`LIVE_TAIL_LINES`），用 `str::rsplit` 反向惰性迭代取尾——取 N 行只解码 N 行，代价与正文长度无关。这是「折叠态一律不得触碰 content」那条性能不变式的第二处显式豁免，上界由 8KB 快照 + 30 行渲染双重钉死。
- **通道默认 `None`**（`ToolContext::tool_output()`）。headless / 子 Agent / ACP 没有接收端，工具据此静默降级为「跑完一次性返回」，回给模型的正文与本特性引入前逐字节一致——流式只影响**给人看的那块视图**。

UI 侧两道闸门缺一不可：① `tool_info` 里必须已登记该 `tool_use_id`；② `is_thinking` 必须为真。ESC 中断会 abort 掉回合 future，但 Bash 的 pump 会一直读到管道 EOF，中断之后仍可能推来最后几帧——不过闸就渲染出一块永远转圈的幽灵输出。另外实时刷新**不**点亮「未读」标记，否则用户上滚翻历史时每帧被拽一次。

顺带修两处同源缺陷：手写管道后若继续 `wait()` 不抽，子进程写满 64KB pipe buffer 会与这里互等死锁；**超时后只放弃等待是不行的**——`bash -c` 及其子进程会继续在后台跑完，而模型和用户都以为这条命令已经结束了。现超时走 TERM → 2s → KILL 的整组三段式（与 `bash_session::kill` 一致，`signal_group` 因此提为 `pub(crate)`），并有 `bash_times_out_and_kills_the_process_group` 用「子进程 sleep 后写文件、kill 生效则文件永不出现」验证。

### 流式 thinking 从「一个字符都不碰」改成「只看尾部 30 行」

上一条不变式被过度执行了：流式 thinking 期间**完全不碰 `content`**——旧实现每帧把整个 `thinking_buf` 重新折行一遍、再只取前 3 行扔掉，长 thinking 下是纯浪费；但改成「一个字符都不碰」之后，用户就**彻底看不见模型在想什么**了，而 thinking 恰恰是最需要实时可见的一段。现在两头兼顾：`rsplit` 取尾部 `LIVE_TAIL_LINES` 行，两端都不扫全文。钉子 `live_thinking_block_shows_tail_only_and_stays_cheap`（把 `rsplit` 改回 `lines()` 或改成全文折行再截断，行数断言立刻变红）。

### `Ctrl+O` 的两种态不是「全折叠 ↔ 全展开」，而是「自动 ↔ 全展开」

`AppState.output_expanded == false`（默认）现在是**自动模式**：正在流式输出的 thinking 与正在执行的工具实时展开，跑完即收起，历史保持干净；`true` 才是把全部 thinking 与工具结果一次性摊开。判定式 `output_expanded || is_live`。按第二次 Ctrl+O 回到自动模式后，新执行的步骤会自动展开，而不是像旧语义那样永远保持折叠。`/help` 文案同步改成「强制展开/收起全部…」。

### Ghostty 下 Ctrl+C 退出后，提示符后凭空多出 `9;5;3u`

现场：按两次 Ctrl+C 退出 TUI，回到 shell 后提示符后面多出七个字符 `9;5;3u`（用 Ctrl+D 退出则是 `100;5;3u`）。

根因在 Kitty 键盘协议。Ghostty 路径压了 `DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES`，按协议 `ctrl+c` 不再以 `0x03` 送达，而是 `CSI 99;5;1u`（press）+ `CSI 99;5;3u`（**release**，事件类型字段由 `REPORT_EVENT_TYPES` 追加）。而 **release 的产生时刻 = 用户松手时刻**：TUI 收到 press 就立刻置 `should_quit` 退出，松手才生成的那串字节此刻还躺在内核 tty 队列里没人读。`PopKeyboardEnhancementFlags`（`CSI < u`）只改写**之后**的编码，杀不掉已发出的字节，于是它们被 shell 读到——zsh 的 ZLE 不认识带参数的 `CSI u`，吃掉 `ESC [` 后把参数当普通字符插进命令行。协议只把 Enter/Tab/Backspace 排除在 release 之外（好让崩掉的程序之后还能敲 `reset`），**Ctrl+C 不在豁免名单里**。

三处收口缺一不可：

- `await_quit_key_release` 在置 `should_quit` 前按 20ms 切片等回配对的 release（上限 300ms；非 Ghostty 路径无 release 事件，直接跳过）。
- `terminal_input::drain_pending_input` 在 pop 之后、关 raw mode 之前，以 `O_NONBLOCK` 读 `/dev/tty` 到 `EAGAIN`，按 40ms 切片轮询、上限 200ms（覆盖粘贴尾巴等没有配对按键的残留）。**顺序不可换**：pop 之前排空没用——终端还在增强模式、随时可能再写新的 release 进来；关 raw mode 之后排空有害——canonical 模式的 `ISIG` 会让队列里残留的 `0x03` 给本进程发 SIGINT。
- `panic_guard::restore_terminal_best_effort` 必须**无条件**发 `PopKeyboardEnhancementFlags`。panic 路径拿不到 `TerminalWheelRouting`，漏 pop 会让 shell **永久停在增强模式**：Ctrl+C 不再有 SIGINT，之后每次按键都变 `CSI u`。同时只在 TUI 所属线程上排空队列——worker panic 同样触发 hook，从 worker 读 `/dev/tty` 会吞掉用户正在 TUI 里敲的键。

残留边角：按住 Ctrl+C 超过 300ms 才松手时，release 仍会漏给 shell，属可接受的极小概率。

**1184 个 workspace 测试** + clippy `-D warnings` 全绿。

## [0.2.31] - 2026-10-12（原 1.5.22）

### 修 review 抓出的 5 个缺陷：其中两个是「修卡死」这个改动自己引入的新卡死

对上一批未提交的改动做了一轮完整 review，抓出 5 个实质缺陷。其中**前两个出在「修卡死」那条改动自己身上**——它引入的 `continue` 作用域过大，制造出一种比原 bug 更隐蔽的假死；共享注入队列的镜像没跟着真实水位走，让「一条消息只投递一次」这条自己声明的不变式当场破掉。

**① autosave 的 `continue` 跳过整个输入层（严重）**。回合 task 持锁期间取不到 session 锁，于是主循环的保存分支 `continue` 下一帧。问题是 `continue` 跳回的是**整个主循环**，被跳过的正是紧随其后的 `ui_ask_rx` 消费与 `event::poll()`：按键、权限确认弹窗、AskQuestion、计划批准全部停摆。而 `terminal.draw()` 在该行**之前**，画面照常刷新——用户看到的是「界面活着但按键全哑」，比原来那个"完全不刷新"更难判断。ESC/Ctrl+C 也救不回来，因为真正终止回合的心跳看门狗排在 `continue` **上游**，现场更像「莫名卡几分钟后自己恢复」。

触发极其常见：`TurnDone` 置位 `save_needed` 与自动唤醒 / 排队兜底的 spawn 发生在同一帧，因此**每一条排队消息触发的自动续跑都会踩中**，整个新回合全程失灵。

改为把保存整段收进 `if let Some(sess) = try_session_lock(&session) { … } else { save_needed = true }`，拿不到锁就自然落到下面继续消费输入层——存盘晚一帧没有代价，等锁才有。`event::poll()` 按键块**内部**的 `continue` 不受影响（消费一个按键、下一帧继续）。不变式「主循环无条件区段拿不到锁只能跳过这件事、不能 `continue`」写进 `crates/tui/CLAUDE.md`，钉子 `autosave_never_continues_past_the_input_layer`（静态检查该分支内不得出现 `continue`）。

**② 子 Agent 取走的排队消息被重复投递（严重）**。`pending_queue` 是 steer 队列的 UI 侧镜像，正常靠 `AgentEvent::Injected` 出队。但共享队列之后**取走者不再唯一**：主 Agent 排空全部、前台子 Agent 只排空 `UserMessage`、嵌套子 Agent 同享同一份——而子 Agent 没有 UI 事件通道、`on_inject` 是空闭包，**发不出 `Injected`**。镜像于是停在旧水位，回合结束后兜底重发会把已经投递给子 Agent 的消息**再当一条全新用户消息发一遍**：同一条要求子 Agent 干一遍、主 Agent 再干一遍，token 双倍。这直接违反同一批改动自己写下的「一条内容只会被投递一次」。

修法不是给每条取走路径补事件通道，而是新增 `SteerQueue::pending_user_messages()` 并在 UI 侧加 `reconcile_pending_queue()`：在 `TurnDone` / `Error` 里、`injector` 被清空**之前**按队列真实水位裁剪镜像。按水位而不是「谁取走的」，三种取走路径一律正确，且不需要动 `ToolContext` trait 与 `SubAgentTool` 的构造签名。反向钉子 `undelivered_steerage_survives_reconciliation_for_the_race_fallback` 守住另一半：确实没被取走的消息必须留下，否则真正的竞态兜底会静默吞掉用户输入。

**③ 自由文本的光标在长行上彻底消失**。`visible_window` 的回退循环不变量写反了：`w` 初值是「光标之前的总宽度」，每轮减的却是「即将丢到左窗外的字符」，减完之后 `w` 实际是**已丢弃前缀**的宽度。循环于是在「左侧丢弃的文本塞得下 `inner_w-1`」时就停——与意图恰好相反，光标偏移量随之无界增长。

`freetext_line` 只给窗口 `[start, end)` 内的 `cursor_col` 画反色块，光标被挤出区间、又不等于行尾时，**整块光标完全不渲染**。实测（80 列终端 `inner_w≈70`）：ASCII 第 139 个字符、中文第 70 个字起光标消失，120 列终端阈值翻倍。对旧代码是**回归**——旧的假 `_` 位置虽然永远错，但永远看得见。

改为向前丢弃字符、比较「留在窗口内的前缀宽度」；光标自身的预留也从写死 1 列改成按 `char_width` 实际宽度（CJK 是 2 列，否则窗口宽恰好等于前缀宽时 CJK 光标仍会被挤出）。穷举验证（ASCII / 中文 / emoji / 中英混排 × 7 种宽度 × 全部光标位）：新实现 0 例真实失败，旧实现 12258 例；剩余 857 例全是退化情形（1 列窗口里的宽字符，任何实现都画不下）。

**④ 计划批准面板的多行输入只画第一行**。这次给 `PlanApprovalDialog` 接上了 `apply_edit_key`，Shift+Enter / Ctrl+J / 行尾 `\`+Enter 都能在"手动输入"里产生多行，提交时取的是 `input.text()`——但渲染写死 `input.lines.first()` 加一个假 `_`，面板高度也写死 5 行。用户写三行只看见一行，回车却把三行一起发出去。同一批改动已经把 AskQuestion 的自由输入改成逐行渲染，这里是对齐遗漏的兄弟面板。现统一走 `freetext_line`，`bottom_panel_size` 的高度按 `3 + input.lines.len()` 算。

**⑤ 一条"本地红 CI 绿"的环境相关单测**。`ctrl_v_in_freetext_does_not_type_a_literal_v` 直接读**用户真实剪贴板**，并断言输入框文本逐字不变——在剪贴板为空的机器与 CI 上过，在有内容的开发机上就红（本次 review 在这台机器上就撞到了，`cargo test --workspace` 唯一那条失败）。这种测试比稳定失败更糟：没人会再信任它。现把剪贴板做成可注入（`with_clipboard_text_override`），测试拆成两个分支分别断言——有内容时**应粘贴**、空剪贴板时**不该敲出字母 v**——比原来那条更强。

顺带修掉 CHANGELOG 里一处置复的标题行。**1163 个 workspace 测试** + clippy `-D warnings` 全绿。

### 弹窗与配置面板里的手写输入难用：编辑键各写各的、自由文本连光标都不显示

同一个编辑动作，在主输入框能按、在弹窗里按不动——这是"交互反人类"的根因，不是审美问题。现状是**三套按键表**：主输入框一套、`InputOwner` 借用态（/model /mcp /skills /plugins /schedule 的配置输入）一套、AskQuestion 与计划批准的"其他"自由输入又一套（只有 `Char` / `Backspace` / `Left` / `Right`，连 `Delete`、`Home`、`End`、`Ctrl+U/A/E/K/W`、`Alt+←→`、粘贴都没有）。而 `InputBox` 的原语其实**早就齐全**（`move_word_*` / `kill_*_of_line` / `delete_word_backward` / `delete_char_forward`），缺的纯粹是接线。

- **`input::apply_edit_key`：纯文本编辑按键的唯一实现**。主输入框、配置面板借用态、两个弹窗的自由输入全部走它。`Enter` / `Esc` / `Tab` / `↑↓` / 粘贴刻意留在调用方——它们各有应用层语义（提交、取消、翻页、历史、附件）。主输入框只保留 5 个不走共享实现的键，各自原因写在代码注释里：`Backspace` / `Delete` / `Ctrl+W` 要连带删除附件占位标记（`backspace_composer` 等），`Home` / `End` 在输入框为空时是"跳到对话首尾"。
- **打字直接进"其他"**：选项态敲任意非空白字符立即进入自由输入并把它当首字符，少一次 `↓↓` + 回车。空格保留多选 toggle 语义。（参考：Claude Code 目前**没有**这个快捷路径，仍需 `↓` 到 Other 行；这里是更快的等价实现，`↓`+`Enter` 那条路保留不变。）
- **自由文本补齐编辑能力**：三种换行写法 `Shift+Enter` / `Ctrl+J` / 行尾 `\` + `Enter`（后两者是 Claude Code 文档推荐的兜底——`Shift+Enter` 只有终端上报键盘协议时才区分得出来，tmux 默认设置下会被原样报成普通 Enter 直接提交）；`↑↓` 翻输入历史（弹窗自己的 `InputHistory`，无历史时回退成原来的翻题/移选项）；`Ctrl+V` 粘贴（此前 `Ctrl+V` 会把字母 `v` 直接敲进输入框）；提交时按 `input.text()` 取值，不再 `lines.join("")` 把多行抹成一行。
- **自由文本终于看得见光标**：此前**只画第一行**并把一个假的 `_` 永远贴在行尾——多行内容直接不可见、光标位置永远是错的。现按行渲染 + 反色块光标 + 超宽时按光标横向滚动 + 字数。光标用反色块而非移动硬件光标：弹窗是浮层，硬件光标同时被底部主输入框占用，两者会打架。
- **修嵌套弹窗遮挡**：`draw_action_menu` 与二级确认此前和父面板用**同一个整屏 `area` 居中**，坐标完全重合，父面板文字从弹窗两侧露出来（实测 `/model` 的第二行 profile 被切掉一半），看着像渲染错位而不是"打开了一个子菜单"。现改为贴着父面板摆放：右侧放不下落到下方，两边都不行才退回居中。

回归钉子：`edit_key_table_is_shared_by_composer_and_dialogs`、`freetext_supports_the_same_editing_keys_as_the_composer`、`typing_a_character_enters_other_with_that_character`、`space_still_toggles_checkbox_instead_of_opening_other`、`shift_enter_newline_survives_submission`、`newline_works_via_ctrl_j_and_trailing_backslash`、`up_down_recall_freetext_history`、`ctrl_v_in_freetext_does_not_type_a_literal_v`。


### 子 Agent / 长命令跑到一半补不进话：注入通道在工具轮边界，前台子 Agent 是一段不透光的等待

2026-10-12 实测复现：前台 `Bash(sleep 30)` 期间提交补充内容，回显了、状态栏提示「已排队」，但直到 `sleep` **返回的那一刻**才送达；前台子 Agent 更糟——用户补充的内容既进不了子 Agent（它的循环 `run_turn` 压根没有注入通道），也进不了被 `SubAgentTool::run()` 这个「不返回的工具调用」整个堵住的主 Agent。12 分钟的子 Agent = 12 分钟白等。后台任务实测不受影响（主回合已结束，回车直接开新一轮）。

两处修复：

- **回显与投递解耦**：排队消息在**提交那一刻**就画进聊天流（`AppState::enqueue_message`），`AgentEvent::Injected` 只负责出队。此前回显也挂在排空事件上，与真正的投递同一个时机点，前台长工具期间用户那边看就是「我输入的字凭空消失」。提示文案也搬进**标题栏**并点名当前正在跑的工具——原先只有状态栏右对齐那一句，窄终端会被「模型 · 模式 · cwd」挤掉后半句（实测只剩「…将在」），而「我的补充还没送达」正是最该看见的信息。
- **注入通道打通进前台子 Agent**：mpsc 接收端是单消费者且子 Agent 拿不到，改为共享队列 `core::SteerQueue`（`Arc` + 内部 `Mutex`，`push`/`drain` 都是 `&self`——主 Agent 与工具侧要同时持有句柄，`&mut` 做不到），经 `ToolContext::steer_queue()` / `ToolCtx.steer_queue` 带进子 Agent（含嵌套子 Agent）。**谁先到工具轮边界谁取走**：子 Agent 在跑则插进子 Agent 的下一轮，子 Agent 已返回则插进主 Agent 的下一轮。

引入共享队列时避开了一个陷阱：**子 Agent 只排空 `UserMessage`**（`SteerQueue::drain_user_messages`）。`SystemReminder` 装的是后台子 Agent / 后台 shell 的完成通知，那是给**主 Agent** 的结论；被子 Agent 抢走会答非所问，更糟的是子 Agent 返回时队列已空，主 Agent 永远收不到那条结果。

回归钉子：`queued_message_is_echoed_on_submit_and_not_duplicated_on_drain`（提交即回显、排空不重复）、`steer_queue_is_fifo_and_drain_takes_everything`、`shared_steer_queue_delivers_to_exactly_one_agent`（一条内容只进一个会话）、`subagent_drain_leaves_system_reminders_for_the_main_agent`。真机验证：前台子 Agent 跑到一半提交一条要求，子 Agent 在自己的下一个工具边界取走并执行，主 Agent 的最终回复里确认收到了该要求。


### 「执行久了整个界面卡死」：UI 线程在等一把回合 task 持有着的锁

2026-10-12 实测定位（用户现场那个跑了 44 分钟的进程仍在，取证后修复）。表象是「跑一阵子之后界面完全不动、ESC 和 Ctrl+C 都按不动」，但 0% CPU、12 个线程全 idle —— 不是慢、不是内存、不是渲染。

`lldb` 主线程栈只有 4 层用户帧且两次采样相隔 5 分钟**逐帧相同**：

```
main → lang_start_internal → tokio Runtime::block_on → CachedParkThread::block_on
     → CondvarParker::park → pthread_cond_wait
```

async 帧不进调用栈，栈这么浅本身就是结论：**整个 `async main` 挂在一个永不返回的 `.await` 上**，而不是事件循环在 poll 超时。两条独立缺陷叠加：

- **回合 task 在整个 agent 回合期间持有 `session` 这把 `tokio::sync::Mutex`**（`spawn_agent_turn`：`let mut sess = session_c.lock().await` 之后 `run_turn_with_injection(&mut sess, …)` 一路到 task 结束，LLM 往返 + 全部工具调用都在锁内）。而 TUI 主循环有 20 多处无超时 `session.lock().await`，其中自动保存分支排在 `event::poll()` **之前**：一旦停在那里，整个界面就没有心跳了——不重绘、不读按键。现场证据完全吻合：会话文件 mtime 停在 10:10:24 再没写过、turn task 仍在发新 bash 子进程、界面冻住。
- **终态事件是阻塞式 send**（`mpsc::channel::<AgentEvent>(256)`）。`TurnDone` 一到，UI 置 `save_needed`、下一帧就来取锁；turn task 若发事件时**仍持锁**，就是「UI 等锁 / turn task 等 UI 排空通道」的环形等待。工具事件走 `try_send`（满了丢弃）不会堵，但回合收尾的 `Usage` / `TurnDone` / `Error` 会，长回合期间通道早已填满。

**为什么 v0.2.30 的看门狗救不了**：回合心跳 `abort_stalled_turn()` 排在保存分支**上游**，主循环一停就不再进下一轮；而按键处理在 `event::poll` **下游**。专门用来救卡死回合的机制，恰好被造成卡死的那把锁挡在身后 —— 这是本次排查里最值得记的一条。

修复两端，缺一不可：

- **UI 线程永不阻塞等锁**：新增 `try_session_lock()`，主循环里凡是碰 session 的地方全部改走它，拿不到就提示「上一轮还在运行」（3 秒去重防刷屏）并跳过本帧。保存路径保留 `save_needed = true` 下一帧重试；`/rewind`、`/branch` 这些自带 `Result` 通道的命令改走 `anyhow::bail!`；会话选择器分支改为**入口一次取锁**复用 4 处（拿不到就保留选择器不关）。计划批准 / 确认切执行模式这两处 checkpoint 改为后台 task（这两处必然发生在回合中，原代码等于"批准计划就冻住直到本轮跑完"）。
- **发终态事件前先放锁**：三处 turn task 统一为「锁内取 Usage 快照 → `drop(sess)` → 发事件」。

回归钉子：`try_session_lock_never_waits_for_a_running_turn`（带超时兜底，改回 `lock().await` 会挂在 timeout 上而不是静默通过）、`try_session_lock_succeeds_when_no_turn_is_running`、`every_turn_task_releases_the_session_lock_before_terminal_events`（静态检查三处 turn task 的顺序）。不变式写进 `crates/tui/CLAUDE.md`，明确「回合 task 内部的 `lock().await` 不要动——那正是持锁者」。


### 粘贴的图片在消息区变了样：输入框 `[Image #1]`，发出去变成末尾一行 `[图片 402×288]`

同一个附件在输入框和聊天流里长成两个样子，而且**位置也丢了**——用户在正文中间粘贴的图，发送后跑到了消息末尾。根因不是渲染写错了，而是**位置信息在发送那一刻就被销毁了**：`InputBox` 用不可见的原子标记 `U+FFFC` 记录"这个附件贴在哪儿"，`InputBox::text()`（也就是发给模型的正文）会把它过滤掉，而消息区展示文本当时是用 `text + attachments` 现拼的——那个 `attachments` 只有"有几个附件"和"每个多大"，**没有位置**，于是只能统一追加到末尾。早期把维度写进文案（`[图片 402×288]`）反而掩盖了这点：看起来像是个精心设计的摘要，实际是信息已经丢了。

修法不是把两处文案改成一样，而是**让两处走同一条投影线**：

- 新增 `render::attachment_display_text(input, attachments)`，复用输入框绘制用的 `input_with_attachment_placeholders`，**占位符停在用户粘贴它的那个位置**，文案逐字节相同（`[Image #1]` / `[File: name]`）。
- 关键约束：**展示文本必须在 `input.take()` 之前算好**（发送路径先 `input.clone()` 留快照）。凡是消息要离开输入框的路径都把 `display` 一起带走——`pending_queue` 从 `Vec<(String, Vec<Attachment>)>` 改成 `Vec<OutgoingMessage>`（正文 + 附件 + 展示文本），`ExecModeConfirmDialog` 增 `pending_display`。**不允许在消费端重算**：那里已经没有输入框了，重算等于把 bug 换条路径再犯一遍。
- 正文里 `@path` 展开出来的文件没有标记（用户敲的是路径文本，不是粘贴），由同一个函数在末尾单列。这里有个坑：`input_with_attachment_placeholders` 末尾有一段「旧状态兼容」兜底会把无标记附件补进行首，聊天区只投影 `attachments[..marked]` 避开它——否则同一个附件显示两次（这条是新写的测试当场抓到的）。
- **`/resume` 另一处独立缺口**：`reconstruct_display` 里图片块被 `_ => {}` 整个吞掉，恢复历史会话后聊天流里**完全看不到当初发过图**。现在补 `[Image #n]`（位置从未落盘，只能在末尾）；纯图片、没正文的消息也会正常出现，不会凭空消失。

回归钉子三条：`chat_display_is_byte_identical_to_the_input_box`（两处必须逐字节相同）、`at_referenced_files_without_a_marker_are_listed_at_the_end`、`user_images_survive_into_the_reconstructed_display`。

### 项目规约文件恢复回退链：根目录只有 `CLAUDE.md` 的仓库此前对模型完全失明

v0.2.27 做的是**硬切换**：`ClaudeMdLoader` 只认 `AGENTS.md` / `AGENTS.local.md`，`CLAUDE.md` 系列一律不读，并由测试 `load_dir_files_ignores_claude_md_entirely` 主动锁死。这条设计的前提是「AGENTS.md 已是开放标准，用户会改名」——但现实中大量既有仓库的根目录只有 `CLAUDE.md`，`git clone` 下来之后 wyj-code 会把它们判定为**没有任何项目规约**：模型既不知道构建命令、测试命令，也不知道这个仓库的编码约定，而这些恰恰是作者写下来、指望 AI 助手读的东西。漏读一份规约的代价，远大于多试几次 `is_file()`。

现改为**按槽位独立回退、命中第一个即停**：

- 主槽位 `AGENTS.md` → `CLAUDE.md` → `GEMINI.md` → `AGENT.md`
- 个人覆盖槽 `AGENTS.local.md` → `CLAUDE.local.md`

`AGENTS.md` 仍是首选，回退只是兜底。两个设计点值得记下来：

- **槽位独立而非「整个家族二选一」**：这样主文件走 `AGENTS.md`、私有覆盖写在 `CLAUDE.local.md` 时仍然生效，不会被静默丢弃；反过来若按家族整体接管，已有 `CLAUDE.local.md` 个人覆盖的用户会突然失效。两槽位各只取一个文件，因此**永远不存在同一份规约被注入两遍**（既烧 token 又让冲突时的模型行为不可预测）。
- **判定用「有实际内容」而非单纯 `is_file()`**：0 字节 / 全空白的主文件继续往下探，而不是就此停下并在 `/memory` 面板里显示一个实际不生效的文件。

连带修掉三处会**报出与事实相反的结果**的地方：`doctor` 的 `claude_md_ok` / `claude_md_missing` 把 `AGENTS.md` 硬编码在 i18n 文案里（回退生效时会显示「AGENTS.md 未找到」，而实际正在读 CLAUDE.md）；注入文本的英文 intro 同样硬编码了 `AGENTS.md`；`/memory` 面板此前是另一套 `is_file()` 判断。现 `/memory` 与注入共用同一个 `resolve_slot`，面板显示的就是实际会注入的那个文件。锁死旧行为的测试被替换为回退链系列（主槽回退、命中即停、尾部候选、local 槽回退、空白穿透、面板与注入同源、祖先链逐级回退）。

不变的部分：全局路径仍只有 `~/.wyj-code/`，任何 `~/.claude/` 一律不读——被读的是**项目目录内**的 `CLAUDE.md` 文件，与读取外部工具的全局配置目录是两回事；prompt cache 行为不变（注入文本含路径与文件名，文件增删时自然失效）；`/init` 的生成目标恒为规范名 `AGENTS.md`，但 prompt 已要求把 `CLAUDE.md` 等其它工具的规则文件一并整合进去。

## [0.2.30] - 2026-10-11（原 1.5.21）

### 长任务跑到一半停住，之后输入任何内容都没反应

「执行久了就会停」是表象，真正的原因是**回合 task 一旦卡死不再返回，UI 就永久锁在 thinking 态**：task 全程持有 session Mutex，而 `is_thinking` 的清零只认 `TurnDone` / `Error`——这两个事件只在 task 收尾时才发。用户后续输入全部滞留 `pending_queue`，连主循环里 `!is_thinking && !pending_queue.is_empty()` 那条唯一的排空兜底分支也被同一个标志挡住。时长只是把尾部概率放大的放大器，不是成因。

既有的 `reap_lost_turn_task` 看门狗只看 `task.is_finished()`，**只兜「task 已死」（panic / 被 abort），管不了「task 还活着却卡住」**——后者那个标志恒为 false。三层一起补：

- **MCP 工具调用超时**（`mcp/src/bridge.rs`）：`call_tool` 原本是裸 `await` 零超时。更隐蔽的是它持有 `client` 这把 mutex——一次 hang 会让**同一 server 的后续全部调用排队等一把永远不会开的锁**。现把取锁与调用包在同一个 `tokio::time::timeout` 里（默认 120s，`WYJ_CODE_MCP_CALL_SECS` 覆盖）。
- **响应头等待超时**（`api/src/retry.rs`）：`connect_timeout` 只管到 TLS 握手完成；握手之后到响应头返回之间此前**无期限**，而流级看门狗（`stream_guard`）此时还没生效。两处读错误响应体的 `resp.text()` 同样无保护。新增 `HEADER_TIMEOUT` 90s，超时按连接层错误走既有退避重试。
- **回合心跳看门狗**（`tui/src/app.rs`）：新增 `abort_stalled_turn()`，与 `reap_lost_turn_task` 并列调用，兜住所有未知阻塞。判据是**「多久没动静」而非「跑了多久」**——`apply_agent_event` 每收到任何事件就重置基准，长回合只要还在吐 delta 或跑工具就不会被判死（默认 900s，`WYJ_CODE_TURN_STALL_SECS` 覆盖，0 = 关闭）。两个刻意决定：**弹窗打开期间不计入心跳**（权限确认 / AskQuestion / 计划批准时回合合法地 await 在 oneshot 上，用户可能离开几分钟才回来，把它算进去等于「出去倒杯水」就杀掉一段长工作，比原 bug 更糟）；**阈值严格大于 Bash 的 600s 上限**，让工具侧的超时错误永远先于心跳触发，否则报错会把原因指向错误的地方。

顺带修掉两个既存缺陷：`stream_guard.rs` 的两个测试**从一开始就没法编译**（reqwest 0.12 的 `Client` 没有 `connect_timeout()` / `timeout()` getter，只有 builder 上的 setter，`cargo test -p wyj-api` 在本批改动前就是坏的），改为断言常量边界 + 一个从源码文本确认 `build_client` 里没有 `.timeout(` 的静态钉子；`spawn_agent_turn` 尾部重复构造 `TurnTask`（`spawn_turn_task` 已返回句柄，却又手工造一个并引用作用域外的 `handle`/`finished`）。

### 任务列表面板配了 `todo_enabled = true` 却永远不出现（真 bug）

开关只管**注册**、管不到**可见性**：`TodoWrite` 不在 `agent.rs` 的 `ALWAYS_VISIBLE_TOOL_SCHEMAS` 里，而工具总数一旦超过 `model_runtime.lazy_tools_threshold`（默认 12；标准工具 9 个 + CodeSearch/Memory/ContextRecall/AskQuestion/Agent + computer 三件套 + 任一 MCP server 的工具，**几乎必然触发**）就会启用 ToolSearch 折叠，未在名单里的 schema 被隐藏——模型根本不知道这个工具存在，自然从不调用。两条一起修：`TodoWrite` 进常驻名单；`prompts::TODO_HINT` 恢复但改成**开关为真时才条件注入**（工具描述只说了「它是什么」没说「什么时候用」，只修可见性会变成「看得见但不碰」；关闭时 prompt 一个字节都不变，默认路径继续对齐 Claude Code v2.1.233 / Codex v0.152.0）。

### Edit/Write 的 diff 折叠后要按 Ctrl+O 才看得见

diff 一直都在生成（`tools/src/diff.rs`），但 `render_tool_result_block` 在折叠态直接 return，于是「文件被改了」看得见、「改成了什么」看不见。现改为**折叠态内联渲染 diff 前 20 行 + `… 还有 N 行（Ctrl+O 展开全文）`**，配色与缩进沿用展开态同一套，切 Ctrl+O 不会让已看惯的行换样。只对 Edit/Write 的**成功**回执生效（`Bash` 那种几 MB 的输出仍然一个字符不碰）。

### shells / subagents 面板不再列出已完成的任务

两个面板口径统一为「只显示运行中」，完成即离开列表（对齐 Claude Code）。底层状态仍保留全部条目——退出状态要等 `poll_shells` 读、自动唤醒 reminder 也要读，只是不再展示。选中项在状态翻转时（`poll_shells` 检出退出、子 Agent `Done`、interrupt、ESC 中断）重新对齐到仍在运行的条目；一个都不剩时收起面板并把焦点交还输入框，而不是让焦点悬空在一个看不见的幽灵行上。已完成的条目并非不可查：子 Agent 完整轨迹仍落盘，`wyj-code subagent-trace <session> [id]` 可读。

### `reasoning_effort` 此前根本没发出去：整条链断在装配层

profile 里的 `reasoning_effort` / `thinking_switch` 写进配置、TUI 面板能编辑、catalog 与 adapter 也都实现了对应字段名，但**这两个值从未到达任何端点**：`Agent` / `AgentRoute` 上的 `with_reasoning_effort` / `with_thinking_switch` 全仓库**零调用点**。结果是最坏的一种——不报错、不降级，用户填了档位却发现思考强度纹丝不动，且日志里什么都没有。实际只有 `thinking_budget` 真的发得出去。

- **装配层补齐**：主 Agent、`/model` 热切换重建、子 Agent、routing fallback 四处全部改为透传 `effective_reasoning_effort()` / `effective_thinking_switch()`（用 effective 版，顺带让 env `WYJ_CODE_THINKING_EFFORT` / `WYJ_CODE_THINKING_SWITCH` 对 fallback route 与子 Agent 同样生效，此前只有主 profile 认）。
- **判据收敛成一处**：新增 `thinking::normalize_effort`，以 `ThinkingSpec::effort_levels` 为唯一真值，agent 组装 `RequestOptions` 与 provider 序列化 body 两处都过它。此前用 `ModelCapabilities` 那个**单值** `ThinkingMode` 标签判断，同时犯了两个方向的错：Qwen 的标签是 `BudgetTokens`，于是它支持 effort 这件事被整个忽略（`QwenOpenAiAdapter` 里的 effort 分支是死代码）；GLM / Ollama / Moonshot k2 / vLLM 的标签是 `Effort`，于是档位被一路放行、在 adapter 里无人接收地消失。
- **填错档位不再换 400**：给 DeepSeek 填 `xhigh`（它只有 low/medium/high/max）这类情况，现在记入 `dropped_parameter` 并说明该模型期望的档位，`wyj-code model doctor` 与启动日志可见。
- **vLLM 补上 adapter**：此前它在 `adapter_for` 里没有分支，直接落到 Generic 兜底，用户配的 thinking 开关被静默丢弃；现在按 vLLM 的方式发 `chat_template_kwargs.enable_thinking`。
- **4xx 降级补全**：端点回"不认 `reasoning_effort` / `thinking.type`"时，agent 此前只撤 `thinking_budget`，effort 仍留在重试请求里 → 再 400 一次，整个 turn 直接失败。现在 thinking 家族任一参数被拒都会整体撤掉并重试一次，并把拒绝记进 `CapabilityCache`。
- **回归测试**：`normalize_effort` 的逐 vendor 档位矩阵、vLLM adapter、Qwen effort 不再是死代码、`xhigh` 落到 DeepSeek 被挡、GLM 收不到档位、能力缓存禁掉 thinking 时静态 spec 不作数，以及 agent 层"effort 确实进了 `RequestOptions` / 被拒后确实被撤掉"两条端到端用例。

### HTTP 技能注册表（SkillHub）：不 clone 整个仓库就能搜技能

此前 wyj-code 的技能来源只有 git 型市场（`store::marketplace`）：搜一个关键字要先 `git clone --depth 1` 整个仓库再本地过滤。本版接入 SkillHub 的 HTTP 接口作为**第二来源**，形状刻意复刻已有的 MCP Registry 客户端（`store::registry`），并保留全部既有安装记账。

- **新增 `store::skillhub`**：`GET {base}/search?q=&limit=` 搜索，`GET {base}/download?slug=&namespace=` 下载 zip。与 git 市场并存——面板 Browse tab 记住当前列表的来源（`SkillsBrowseSource`），按来源走对应安装路径。
- **逐条确认后才落盘**：面板里选中搜索结果回车后，**先下载并扫描但不写盘**，确认框展示来源 / 作者 / 版本 / 文件数 / 体积；命中 `scripts/`、`hooks/`、`bin/` 时逐行列出可执行脚本并警告，用户 `y` 才解压。命令行 `wyj-code skillhub install` 同理，非 TTY 必须显式 `--yes`——第三方技能正文会整段作为模型指令进入对话，且注册表社区源实测多为 clawhub 镜像，这是不设门控就默认打开的 prompt injection 面。
- **解压按解不可信归档处理**：拒绝 `..` 穿越、绝对路径、符号链接条目；限制条目数 / 下载体积 / 解压总字节（防 zip bomb）；要求根目录含 `SKILL.md`；先解到同级 staging 目录再原子 rename，lockfile 写失败回滚磁盘。安装目录名先 `sanitize_install_name`（远程 slug 是发布者可控字符串，会直接成为 skills 目录名），同名目录仅在 lockfile 有纳管条目时允许覆盖。
- **修掉两个目录形态的既有缺陷**：注册表装的是目录 `<name>/SKILL.md`（带 `references/`、`scripts/`），而 marketplace 装的是单文件 `<name>.md`。此前 `scan_skill_dir_for_display` 只扫 `*.md`（注册表技能装了却**在 Installed tab 里看不见**）、`uninstall_skill` 只删 `.md`（残留目录，且 `references/*.md` 会被加载器误注册成额外 slash command）。两处均已补上目录形态。
- **`lockfile::SkillSource` 新增 `registry` 字段**（`{base_url, slug, namespace}`，与 git marketplace 字段互斥，`#[serde(default)]` 兼容旧 lockfile）。`upgrade_skill` 按来源分流——否则会对一个 https URL 执行 `git clone`。实测重复升级返回 `AlreadyLatest`，不会每次重装。
- **新 CLI**：`wyj-code skillhub search|info|install`，`/skills` 面板在 headless 下不可用时的等价入口。`search --json` 便于脚本消费。
- **顺带修掉一个 i18n 缺陷**：`zh.yml` 里整块技能面板文案挂在错误的顶层键 `import:` 下（无任何代码引用），导致**所有中文技能面板文案一直回落英文**。已改回 `skills:`。

## [0.2.29] - 2026-10-10（原 1.5.20）

> 补记：v0.2.29 发布时本文件漏写了版本小节，本次一并补齐。

### token 账本：内存里有计数器，落盘时一个都没写

`SessionFile` 补齐 `cache_read` / `cache_write` / `api_calls` / `tool_schema_tokens` 与 `prompt_cache_state` 六个字段（全部 `serde(default)`，旧会话文件可直接读）。**根因不在字段本身，而在 12 处构造点各自手写全量字段**——新增字段不可能不漏。现收敛为 `SessionFile::from_session` / `Session::restore_usage_from` 一对，漏改从此是编译期错误。新增 `wyj-code usage` 跨会话汇总（`--since` / `--project` / `--json`），按月按项目聚合；`api_calls` 缺失时 per-call 返回 `None` 而非 0，区分「没记录」与「真的是 0」。

### 上下文清理此前从未运行

实测 129 个会话 `compact_count` 与 `context_edit_freed_tokens` 全为 0：`context_window = 1M` 把压缩触发线推到 900K，实际会话最大约 292K。新增 `[context_edit] soft_limit_ratio`（默认 0.55）作为动手线，硬阈值仍是停手线，收敛目标不变；`= 0` 完全退回旧行为并有回归测试钉住。

### prompt cache 对第三方端点可用且失败安全

`cache_control` 与 `anthropic-beta` 头解耦：第三方开启缓存时只发前者。首次 400 且分类为 `UnsupportedParameter` / `InvalidRequest` 时自动降级并重试一次，进程内单调置位；**`ContextLengthExceeded` 被显式排除**，避免上下文一满就永久失去缓存。降级事实经 `Provider::prompt_cache_state()` 回写会话并落盘，`/cost` 据此区分「没配缓存」与「配了但端点用不了」。`model_catalog` 的 `prompt_cache` 能力改为采信显式配置，不再把 `protocol_default` 猜测当作最终结论。

### 并行工具能力收敛到单一写入点

`parallel_tool_calls` / `max_tools_per_turn` / `RequiresSingleTool` 统一经 `apply_parallel_policy`，配三字段同步不变式的回归钉子。`model doctor` 改走 `resolve_with_cache`，报告能力与运行时实际用的同一份；`max_tools_per_turn` 进入 `/model` 面板并有往返测试。

### `/cost` 与配置提示

新增 API 调用次数与平均单次输入行、缓存降级说明行；补 MiniMax 单价档位，单价表未覆盖时显式提示而非静默降级成「无金额」。`--config-status` 对未经探测的超大 `context_window` 给出警告。

### 站点

`docs.html` / `index.html` / `script.js` 更新到 v0.2.29；顺带修掉 `delivery.title`/`desc` 的 zh/en 字典停在 v0.2.27（`applyLang` 用字典覆盖 HTML，线上 delivery 标题一直是旧版本）。

## [0.2.28] - 2026-10-09（原 1.5.19）

### 【BREAKING】项目级 skill 信任确认：仓库自带的 skill 需显式批准

`<git-root>/.wyj-code/skills/` 随 `git clone` 一起落地，正文由**仓库作者**（可能是陌生人）控制。此前它们与你自己写的 skill 走同一条加载路径——克隆一个陌生仓库、对方在里面放几个 `.wyj-code/skills/*.md`，你敲 `/xxx` 时整段敌对文本就会作为模型指令进入对话。**注入面其实已经开了一半**：skill 的 `description`（≤240 字符）自 v0.2.27 起就经 `{skills}` 占位注入 system prompt，模型会主动向用户推荐这些命令。

本版补上门控，强度与 `mcp.toml` 完全对齐。

- **⚠️ 升级必读**：仓库里带有 `.wyj-code/skills/` 的项目，首次启动会弹出信任面板。**不批准**（`n`/`Esc`）则这些 skill 本次不加载——斜杠补全里不出现，模型也看不见、不会推荐；下次启动会重新询问。批准记录落 `~/.wyj-code/projects/<project_key>/skill_trust.json`，**在仓库内容控制不到的位置**（否则被信任的仓库自己就能在受版本控制的文件里把「已批准」标记改掉）。内容变化后指纹失配、重新变为待批准——**含「被 `git pull` 悄悄替换过」的场景**。
- **指纹覆盖整棵树、含 `references/`**：`references/` 虽然不会被注册成命令，但它是**经由已批准的 SKILL.md 可达的二阶注入面**（SKILL.md 可以写「先读 `references/x.md` 再动手」，而模型有 Read 工具）。只哈希 SKILL.md 的话，攻击者可以提交一份干净 SKILL.md + 恶意 references，你批准后再改子文档即可绕过。因此哈希范围刻意做成加载器注册范围的**严格超集**：不 follow symlink（防符号链接环 DoS）、symlink 按 link target 字符串入哈希、哈希一切普通文件不过滤扩展名。指纹是**纯内容哈希**（不含 mtime/inode/权限），`git checkout` 的字节级还原不会被误判成「内容变了」。回归测试 `fingerprint_changes_when_reference_doc_changes` 是这条不变式的钉子。
- **批准后本会话即生效**：批准时同步重建命令注册表与 Agent——斜杠补全立刻可用，模型也在本轮就知道这些 skill 存在。顺带修掉一个既有缺陷：`rebuild_fn` 原先把 skill 名单当不可变快照捕获，连 `/model` 热切换都刷不到新名单，现在改为每次重建时重算，附带修好 `/skills` 面板 enable/disable 后模型名单不刷新的老问题。
- **CLI**：`trust-mcp` **一字不改**（已上线、可能被脚本化），另设 `trust-skills` 专管 skill、`trust` 为两者总入口（配 cron 定时任务前一次批完）。**信任决策必须逐类可见**——把 skill 并进 `trust-mcp` 会产生「以为在批准 MCP 却连带批准了仓库作者写的指令正文」的真实误操作。
- **无 UI 通道场景**（`-p` / `--headless` / `schedule run`）与 MCP 同构：静默排除 + 打印一次提示，**不做 stdin 阻塞式确认**（`-p` 常被 cron 无 TTY 调用，阻塞会挂起）。
- **只覆盖项目级来源**：全局 `~/.wyj-code/skills/`、插件贡献路径、内置 skill 均不受影响——它们不在仓库内容控制范围内。

### 工具结果折叠：默认只留标题行，`Ctrl+O` 出全文

TUI 聊天流里工具调用的结果此前走静态预览：**最多 3 个视觉行**，超出显示 `...`。Bash 的 JSON 输出永远只有头三行，尾部的关键信息（`request_id` / `reason` / traceback 末行）看不到，也不知道下面还有多少内容。现有代码里已有三处针对这个缺陷打的补丁（错误输出改 head+tail、`⎿` 行跳过「退出码 N」横幅、Read 剥离行号），全都是在「3 行预算」这个错误前提下做取舍。

本版把 v0.2.27 的 thinking 折叠模式套到**所有工具结果**上，对齐 Claude Code。

- **默认折叠**：`⏺ Bash(cd /tmp && …) · 0.0s` 之下**一行正文都不出**；Edit/Write 的彩色 diff 同理。失败结果不例外，一律折叠，标题行的红色 ` · failed` 保留。`Ctrl+O` 全局切换后出**全文**、不再有任何行数上限。折叠头**不加任何元信息**（不加行数、不加展开提示）——只留现有标题行。
- **`!` bash 输出是唯一例外**（输入框敲 `!` 直接执行的命令）保持三行预览 + `...`，`TASK_PREVIEW_MAX_ROWS` / `push_capped_preview_lines` 现在只服务这一条路径。
- **内容加工逻辑按「是否预算启发式」分化**：纯预算启发式（三行截断 `...`、错误 head+tail 的 `⋮`、空行过滤）全部删除——展开已是全文，保留 `⋮` 反而会**主动删掉用户明确要求看到的中间内容**；与预算无关的表达层规范化（剔除「退出码 N」横幅、Read 剥行号、Edit 只显示 diff 段、错误首行红/其余 dim）全部保留。
- **连带收益（默认路径反而更快）**：折叠态在 `tool_result_content_lines`（会把整个 `content` `lines().collect()` 成 Vec）**之前**就 return，彻底消灭了旧实现「每帧为每条工具结果构建完整 preview 再只取 3 行扔掉」的浪费——与 thinking 流式态当初消灭的是同一类问题。测试 `collapsed_tool_result_never_touches_content` 是这条不变式的回归钉子。
- **Todo 详情面板跟随同一 flag**：它重放的是工具消息，硬编码折叠会让面板只剩一排 `⏺ Tool(...)` 标题、正文全空。

### 修 skill 幻觉路径：`<env>` 补 home 目录 + 禁令改成行为禁令

用户输入「请用 skills 分析股票」，模型回复「技能目录 `/Users/dev/.wyj-code/skills` 在这台机器上不存在」——**`/Users/dev` 这个路径全仓库和 skill 目录里都不存在，是模型编的**（你的用户名是 `wangyoujin`）。会话记录里抓到了它的 thinking 原文：「Read the file directly at /Users/dev/… **But I don't know the home dir**」。同批会话里还编了 `D:\dev\data-platform`、`C:\Users\Alex\…`——中文训练数据的路径模式泄漏。

skill 名单注入本身是正常的（模型能准确说出 `/hithink-finance`），真正暴露的是两个缺陷：

- **禁令是路径枚举，模型换个目录就绕过去了**。原提示只写「禁止读 `~/.claude/skills/` 或任何 `.claude` 路径」，而模型转而去读**正确**的 `~/.wyj-code/skills/`（提示词自己告诉它这个目录）——想去的地方没错，只是把 home 编错了。现改成**行为禁令**：「禁止一切自发检索 skill 目录」，逐一点名三个易误检索目录 + 明确禁止 reconstructed from memory or guessed，并指明名单是唯一合法信息来源。
- **`<env>` 块没有 home 目录**，模型无从得知 `~/.wyj-code/` 的真实绝对路径，只能猜。现新增 `Home directory` 字段（与会话内恒定的 `cwd` 同性质，进 prompt cache 的 stable 段，不增加重算成本）。

### 顺手修掉的既有缺陷

- **i18n 快捷键列表缺 `Ctrl+O`**：全仓 `Ctrl+O` 此前只出现在 thinking 折叠头和注释里。本次改动让工具结果折叠头不带提示，发现入口更窄，故在 `zh.yml` / `en.yml` 的快捷键列表各补一行。
- **`trust-mcp` 之外新增两个子命令**，见上文「CLI」条。

## [0.2.27] - 2026-10-09（原 1.5.18）

### 【BREAKING】彻底切断 `.claude/` 依赖：`CLAUDE.md` → `AGENTS.md` 硬切换

wyj-code 自本版起**不再读取任何 `.claude/` 路径**（不是"优先读 A、回退读 B"，是硬切换、不保留回退）。这不是洁癖，是一个被实测出来的真实故障：模型凭 Claude Code 的训练先验去找 `~/.claude/skills/`，那里有 20 个 wyj-code 根本不读的 skill，而 wyj-code 自己的 `~/.wyj-code/skills/hithink-finance` 就躺在旁边看不见。

- **⚠️ 升级必读**：仓库根目录的 `CLAUDE.md` **不再被读取**。请改名为 `AGENTS.md`（AGENTS.md 开放标准，Codex / Cursor / Gemini CLI 均采用），或同时保留两份。
- **记忆文件名统一为 `AGENTS.md`（复数）**：`ClaudeMdLoader` 查找范围为全局 `~/.wyj-code/AGENTS.md` + 从项目根到 cwd 的祖先链（`.git` / `.wyj-code/project.toml` 定位仓库根，找不到则只用 cwd 本身）；每级目录内 `AGENTS.md` 与 `AGENTS.local.md` 都存在则都读（local 视作个人覆盖追加、不提交 git）。不再兼容旧版 `WYJ.md`，也不保留 `CLAUDE.local.md`。i18n key 仍沿用历史拼写 `claude_md_ok` / `claude_md_missing`（key 是代码契约，改名会波及所有调用点，值已改成 AGENTS.md）。测试 `load_dir_files_ignores_claude_md_entirely` 锁死该行为。
- **回退链塌缩为纯两级**：原「全局 CLAUDE.md → 祖先链 CLAUDE.md → 两者皆无才读 AGENTS.md」的三级兜底塌缩为「全局 + 项目祖先链」两级，回退机制本身就是模型行为漂移的来源之一。
- **hooks 三源全部迁入 `.wyj-code/`**：`~/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.local.json`（后者追加不覆盖，供个人临时覆盖、不提交 git）；原三个 `.claude/settings*.json` 源不再被读取。`hooks.rs` 的 git root 探测改用更强的 `config::project_root`（额外识别 `.wyj-code/project.toml`），删除随之失效的 `find_git_root` 死代码并补 3 个 git root 测试。4 个事件（`PreToolUse` / `PostToolUse` / `UserPromptSubmit` / `Stop`）、exit 2 = block、stdout JSON 表达 decision/reason/additionalContext 的语义全部不变。
- **`/init` 生成目标改为 `AGENTS.md`**（否则 `/init` 会生成一个永远不会被读取的文件）；`claude_home_dir()` 删除、替换为中性命名的 `global_config_dir()`；`frontmatter.rs` / `agent_def.rs` / `prompts.rs` 中过时的 `~/.claude/agents/`、`~/.claude/` 注释与断言同步清理。
- 工具（Read/Edit/Write/Glob/Grep）触达新子目录时的 `maybe_dir_reminder` 动态加载机制不变。

### TodoWrite 任务列表默认关闭

`[tools].todo_enabled` 默认 `false`，`TodoWriteTool` 不再无条件注册——未注册 = 工具不产生 schema、模型目录里看不到它。**对齐 Claude Code v2.1.233 与 OpenAI Codex CLI v0.152.0：两家头部厂商在 2026 年都把 todo 脚手架改成了默认关闭**，官方口径是「新模型不需要书面任务列表也能自行跟踪多步工作」。

- **本地实测依据**（102 个历史会话）：27 个会话用到 TodoWrite、共 132 次调用，其中 **132 次（100%）独占一个完整 LLM 往返**，0 次与真实工作工具同轮发出；回给模型的 `tool_result`（"任务列表已更新: N 项…"）信息量为零——模型自己刚写的列表它自己知道。最密会话 15 次这样的空转往返累计重复 prefill 5.2 MB 上下文。**59% 的会话结束时 todo 未收尾。**
- **外部实证**：arXiv:2604.12147（UIUC+IBM，21,120 条 SWE-agent 轨迹 × 8 种 plan 设置）——「a subpar plan hurts performance even more than no plan at all」；arXiv:2609.20804（UMass，176 组配置 × SWE-Bench/Terminal-Bench）——对强模型 planning 是「省 ~30% 成本、准确率不升反微降」，对弱模型则是 +11.6pp 的防过早放弃脚手架。
- **保留开关而非删除**：wyj-code 同时接 anthropic / openai / MiniMax / GLM / Kimi 等多个 provider，弱模型 / 自托管场景下这个脚手架仍有实证价值，交由用户按模型能力自行开启。
- **同步修掉三处实现缺陷**（即便开启也该修）：① **三重渲染**——同一次调用在聊天流里出现三遍（`⏺ TodoWrite` / `⎿ 任务列表已更新…` / 面板头+分隔线+条目），现改为 `ToolStart` 与 `ToolEnd` 都对 `TodoWrite` 跳过常规工具行只保留面板（**`ToolStart` 仍照常占用 seq 并登记 `tool_info`**，否则 `ToolEnd` 靠 seq 配对会失配）；② **100% 独占往返**——`TodoWriteTool` 覆盖 `parallel_safe() -> true`（默认的 false 逼模型每次单独占一轮）；③ **描述精简**——按 Anthropic 官方形态重写为「一句话 + status 枚举 + 单任务 in_progress」。

### skill 机制：先讲清规则，再注入可用名单

用户在 TUI 输入「请使用 skill 分析股票」，模型回复「我不能自己调用 skill——skill 必须由你用斜杠命令触发」，用户依然不知道该敲哪条命令。

- **首次处置（`prompts::MAIN` 加 `# Skills` 段）**：讲清 skill 只存在于 `~/.wyj-code/skills/` 与 `<git-root>/.wyj-code/skills/`、由**用户**用 `/xxx` 调用（显式否认存在 `Skill` 工具，避免模型反复尝试调用不存在的工具）、被要求"用 skill"时不要自己去翻目录、尤其禁止读/列/搜 `~/.claude/skills/`。
- **二次处置（注入名单）**：只禁检索、不给数据，模型唯一既诚实又不违规的回答就是把禁令复述给用户——**这是提示词的禁令/数据不匹配**，禁掉了获取名单的途径却又要求输出一个无从得知的名字。修法是仿子 Agent 类型那条现成通路（`tools/src/sub_agent.rs` 的 `SUB_AGENT_TEMPLATE.replace("{types}", &types)`），给 skill 补上同等出口：`prompts::MAIN` 的 `# Skills` 段留 `{skills}` 占位，`Agent::with_skills(Vec<SkillEntry>)` 替换之，名单由 `prompts::render_skill_catalog` 渲染。
  - 名单**直接来自 `load_skills`**（CLI 侧 `main.rs::collect_skill_entries`），不从 `CommandRegistry::list()` 筛 `is_dynamic()`——registry 是「skill 先注册、内置命令后覆盖同名」，已确认 `review` skill 会被内置 `ReviewCmd` 顶掉，从 registry 筛会漏。
  - 名单**按 name 排序**后注入：`load_skills` 内部是 `HashMap::into_values()`，不排序会让 system prompt 每次进程启动字节不同、连带打穿 prompt cache。
  - 注入点两处：主 Agent 构造链 + `rebuild_fn` 内（`/model` 热切换与设置面板保存都会重建 Agent，漏了会重新退回"我不知道有哪些 skill"）。列表落在 system prompt 的 **stable 段**，会话内冻结、prompt cache 照常命中；代价是 `/skills` 面板中途禁用某 skill 后本会话名单不更新——刻意取舍，放 volatile 段能实时但每轮要为名单全价重算上千 token。
  - **仍不做 `Skill` 工具化**：skill 正文走 `tool_result` 会被 `persist_cap` 截断（20KB head + 10KB tail，超长 SKILL.md 会让模型读到中间被挖空的残缺指令，比现状更糟）；`allowed-tools` 靠 `CommandResult::RunPromptScoped` 生效、`Tool::run` 拿不到该通道；子 Agent 会继承 Skill（Claude Code 的做法是禁用）；marketplace skill 作为 tool_result 回给模型是一条**当前完全没有信任门控**的 prompt injection 面。名单注入已解决"模型不知道有哪些 skill"这个真实痛点。

### thinking 块默认折叠，附带修掉字形缺失与行数语义不明

原实现固定显示前 3 行正文 + 一个 `· N lines` 标记，头部字符 `✻`(U+273B, Dingbats) 在终端字体里缺字形画成豆腐块。

- **默认折叠成一行元信息**：`⎿ 已思考 12s（41 行）  ·  Ctrl+O 展开`，`Ctrl+O` 全局切换展开/折叠后出**全文**。`⎿`(U+23BF) 与同文件高频的 `⏺`(U+23FA) 同属 Miscellaneous Technical 区，字体覆盖率不是一个量级。
- **行数口径修正**：`N lines` 数的是去掉空行后的**原始**行数，与屏幕上经折行再截断的**视觉**行数没有对应关系，因此它只能当元信息、不能当"下面还有多少内容"的提示。
- **取头改为可展开取全文**：thinking 是流式追加的，最新结论在尾部，旧实现固定取前 3 行意味着几十行思考的结论永远看不到（用户看到的一直是复述任务的开头）。
- **流式期间不再每帧全量折行**：旧实现每帧把整个 `thinking_buf` 重新 `clean + wrap` 一遍、再只取前 3 行扔掉（500 行 thinking 在 20fps 下约每秒 10000 次无用切分与分配）。现在流式态只画一行 `⎿ ⠋ 思考中…`，**完全不碰 content**。
- **接线两个一直存在却从未被消费的东西**：`AppState.thinking_started` 自 `ThinkingDelta` 首块写入后**只有写点、零读点**（固化时直接丢弃），现于 `flush_thinking` 清空前算出耗时落到新的 `ChatMessage.thinking_elapsed_secs`（**不复用** `elapsed_secs`——那个字段按工具语义被 6 处渲染代码消费，混入思考耗时会被误显示为工具耗时）；i18n 里 `thinking.done`（"已思考 {secs}s（{lines} 行）"）长期零引用，现在复活并接上。`/resume` 恢复的旧会话没有耗时记录，故新增 `thinking.done_no_elapsed` 分支而不是显示假的 `0.0s`。
- **不重新引入聊天区消息选中游标**：`Ctrl+O` 是全局开关而非逐条展开——`crates/tui/src/app.rs` 里 `_ => leave_content_focus()` 兜底与 Enter「回输入框」的语义都是按"聊天区无选中"设计的，把历史上被刻意移除的消息选中搬回来回归面过大。

### TUI 工具结果预览

- **`⎿` 行直接展示真实错误首行**，不再被「退出码 N」横幅占掉（第三方 API 的关键字段如 `request id` / `reason` / traceback 末行几乎总在尾部）。错误块整体降级为 dim。

### 测试

1000 个 workspace 测试全部通过（新增 8 个：thinking 折叠/展开/流式/无耗时四种渲染形态、`flush_thinking` 耗时接线、`thinking_expanded` 默认值与复位、`render_skill_catalog` 四例、`with_skills` 注入与空安装防占位符泄漏、`collect_skill_entries` 端到端读取与排序），`clippy --all-targets -D warnings` 零警告。

## [0.2.26] - 2026-10-08（原 1.5.17）

### 上下文管理：口径统一 + 状态栏去百分比 + 两个会真 400 的硬缺陷

调研对标 Claude Code / Codex CLI / Aider / Cline / Goose / Cursor 2025-2026 公开实现后，定位到 wyj-code 的上下文由**三条口径互不一致的链路**分别度量：压缩决策用 `estimate_request_tokens`（system + 工具 schema + 输出预留 + framing），状态栏用 `estimate_tokens`（**只有 messages**），记忆注入用 8,000 字节硬截断。实测隐形开销 ≥1.3 万 token（主 system prompt 2,245 + ≥16 个常驻工具 schema + `max_tokens` 8,192 + 协议开销），在 200K 窗口下就是 **≥6.5 个百分点的系统性低报**——进度条的颜色和实际压缩时机对不上号。详细调研与业界基线见 `doc/plan/v0.2.26-context-management-plan.md`。

- **状态栏移除上下文占用百分比与进度条**（`crates/tui/src/render.rs::draw_status`）：条与数字对用户没有可操作性，只是持续制造焦虑；`Theme::progress_{normal,warn,danger,empty}` 四个主题色一并删除。改为显示**语义**指示 `已自动压缩 ×N`——压缩对用户不可见，除以数字之外必须让"自动管理发生过"本身可见，否则就成了黑盒。计数落盘在 `SessionFile.compact_count`，`/resume` 后不丢。2 个回归测试锁死（`draw_status_omits_context_percentage_and_bar` / `draw_status_shows_auto_compacted_chip_when_history_was_summarized`）。
- **新增 `ContextAudit` 消灭口径分裂**（`crates/core/src/compact.rs`）：一次性算出 system / 工具 schema / 对话历史 / 输出预留 / 协议开销 / 可回收旧工具结果六项分解。压缩决策、`/cost` 百分比、`/context` 面板**全部共用这一个结构体**，从根上杜绝"显示的数字和实际决策量不是同一个"这类偏差。`estimate_request_tokens` 保留为 `ContextAudit::total()` 的薄封装（行为完全等价，有回归测试）。
- **新增 `/context` slash 命令**：按类目展示上下文分解 + 自动压缩阈值 + 可回收工具结果量 + 本会话压缩次数。已按 CLAUDE.md 硬性约定同步注册进 `/help`（中英双语文案走 i18n）。原来的"看上下文占比"需求从常驻状态栏搬到了按需命令——愿意看的人主动查，不想看的人不再被打扰。
- **【会导致真 400 的硬缺陷】压缩 buffer 必须 ≥ `max_tokens`**：`compact_trigger_buffer` 旧公式 `min(40000, max(cw/5, 4000))` 绝对封顶 40K，在 `max_tokens = 64000` + `context_window = 200000` 时算出阈值 160K，加 64K 输出 = **224K > 200K，结构性必 400**；`max_tokens` 在 config 层又没有任何上限钳制。改为 `max(max_output_tokens + 8192, cw / 10)`。这同时修掉「大窗口反而触发越晚」（旧公式在 1M 窗口下只留 4% 余量、阈值 96%；新公式 200K/1M 都是 90%，与 Codex CLI 的 `window × 0.9` 对齐）。Claude Code 官方文档对 `CLAUDE_CODE_MAX_OUTPUT_TOKENS` 的说明（"Increasing this value reduces the effective context window available before auto-compaction triggers"）即此意。有回归测试穷举多组 `(window, max_output)` 断言 `阈值 + max_tokens <= 窗口`。
- **【会导致真 400 的硬缺陷】没有 context 超限恢复路径**：`ProviderErrorKind::ContextLengthExceeded` 早在 `crates/api/src/error.rs` 就有分类，但 `agent.rs` 的 stream 错误分支只处理 `UnsupportedParameter`（thinking 降级）和同角色模型 fallback，**没有任何一处消费它**——启发式一旦低估，400 就直接抛给用户。现在：命中该分类时无视阈值强制压缩一次后重试（`context_recovered` 标志保证只重试一次，对标 Claude Code 的 "Autocompact is thrashing" 熔断）；压缩本身压不动（消息太少）也照常重试，因为重入循环体会再跑一遍 `truncate_messages` 兜底。同步补全 provider 侧识别表——Anthropic 官方的 `prompt is too long: N tokens > M maximum` 一句都不含旧表的三个关键词，会掉进 `is_client_error` 被误判成普通 `InvalidRequest`。3 个回归测试覆盖。
- **压缩保留量从固定 6 条改为 token 预算**：旧实现 `COMPACT_KEEP_RECENT = 6` 在两种场景下都是错的——工具密集回合里 6 条可能就是一整个巨型 tool_result（保留太多压不下去），纯对话回合里 6 条可能只有 1K token（保留太少，压完仍超标才要连跑 3 轮 pass）。改为 `clamp(context_window / 8, 4_000, 32_000)`，保留比例随窗口线性伸缩，参照 Goose 的 `compute_tool_call_cutoff`（`3 × limit / 20000` clamp 到 10–500）。新增 `safe_keep_from_budget`（按预算从尾部累加定出朴素索引，再复用既有的 `safe_keep_from` 做真实用户边界回退，绝不拆散 tool_use/tool_result 配对），3 个回归测试覆盖短消息/长消息/单条超大三种场景。
- **压缩事件改写为结构化系统消息**：`[已自动压缩 · 第 N 次 · 丢弃 X 条旧消息 · 保留最近 Y 条原文 · 释放约 Z tokens]`（旧版只有 removed/saved 两项，用户既不知道发生过几次、也不知道保留了多少原文）。i18n `agent.compacted` / `agent.context_overflow` / `status.auto_compacted` / `context.*` 中英同步。
- **`AppState.context_tokens` → `compact_count`**：TUI 侧那套独立的 `estimate_tokens(messages)` 计算（状态栏、5 处 session 加载点、事件流）整体删除，改为消费 Agent 侧已算好的 `ContextAudit`。`AgentEvent::Usage` 相应把 `context_tokens` 换成 `compact_count`。

### Tool-result context editing：清理优先于摘要（第 3 批）

超阈值时先把**过期的工具输出外部化到 CAS**，清理得动就不去跑整段摘要。对齐 Claude Code 官方文档的原话顺序："It clears older tool outputs first, then summarizes the conversation if needed." 清理比摘要便宜（无 output token、不产生幻觉）**也更保真**（原文只是移出模型视野，没被转述）——而摘要最先丢的恰好是负知识（被否决的假设）、精确值（路径/行号/常量）、顺序因果。

- **新增 `crates/core/src/context_edit.rs`**：`elide_tool_results` / `materialize_elided` / `recall_elided` / `release_session_blobs` / `elided_count`。
- **新增 `ContextRecall` 工具**：按占位符里的 `cas://<hash>` 取回原文，支持 `offset`/`max_bytes` 分页、按 char boundary 切分（CJK/emoji 安全）、hash 必须是 32-64 位 hex（挡路径穿越）。取不回来时明确告诉模型"去重跑工具"，而不是让它把失败理解成"内容本来就没有"。**进 `ALWAYS_VISIBLE_TOOL_SCHEMAS`**——一旦被 lazy 工具折叠，占位符里的引用就成了死路，清理变不可逆。
- **保护规则用白名单**：`ELIDABLE_TOOLS = [Read, Grep, Glob, Bash, WebFetch]`，新增工具默认不被清理。`Edit`/`Write`/`NotebookEdit` 的回执被清理会让模型以为自己没改过从而重做，代价远高于漏删。`ToolResult` 不携带工具名（只有 `tool_use_id`），所以先扫 `ToolUse` 块建 `id → name` 映射；映射不到的历史块保守跳过。最近 3 个工具结果保留全文。
- **「模型可见上下文」与「持久 transcript」分离**：落盘的 session 文件里存占位符（体积小）+ 原文在 CAS；`SessionStore::load` 调 `materialize_elided` 把原文还原回内存——resume 的语义是「新会话从完整上下文开始」，之后随增长再逐步重新清理。blob 已被 gc 时保留占位符（让模型知道"这里曾经有内容、来自哪个工具"并决定重跑），不变成空洞。
- **回收链路补齐**：`WorkspaceCas::gc` 此前只被单测调用、`cas_total_bytes`/`cas_gc_on_start` 因「无消费点」被删。v0.2.26 恢复这两个配置项并接上真实生产消费点——CLI 启动时按 `cas_total_bytes` 跑一次 `gc`；`Session.elided_blobs` 随 `SessionFile` 落盘，在 TUI `/clear`、TUI 退出、headless `/clear`、headless 退出四处 `release`。**CAS 的 `gc` 只回收 `ref_count == 0` 的 blob，release 与 gc 缺一就是无界增长。**
- **新增 `[context_edit]` 配置块**：`enabled`（默认 true，关闭即完全退回 v0.2.25 行为）/ `protect_recent` / `batch_size` / `min_result_bytes` / `max_batches`。CAS 不可写时机制**整体关闭**而不是降级成"就地删掉"——没有 CAS 意味着原文真的会丢，而 Bash/WebFetch 的输出重跑也拿不回来。
- **主提示词新增 `# Context management` 段**：教模型看到 `[context-edited]` 占位符就 `ContextRecall`、不要凭空猜内容，也不要投机性召回（占位符里的工具名和首行通常就够判断值不值得）。
- `/context` 面板新增「已清理 N 条 / 释放约 M tokens」一行，与「可回收」并列——只报持续增长的可回收量而不报处理进度，用户会以为机制没生效。
- 11 个新测试：保护规则、往返（清理→占位可解析→召回→还原一致）、blob 丢失时保留占位符、幂等（同内容去重但 ref 仍按次数记账）、release→gc 闭环、以及一条策略级测试锁住「纯清理就能落回阈值内、不需要摘要」并断言占位符开销远小于释放量。

### 已知未修（见设计文档第 4 节）

- 压缩会**永久摧毁记忆检索质量**：`memory_query_context` / `build_memory_snapshot` 取最近 4 条 User 角色消息当检索 query，而 compact 把 `session.messages` 重写成 `vec![summary]` 且该摘要 role = User；`MEMORY_SNAPSHOT_REFRESH_TURNS = 10` 的分桶计数也按 messages 里的 User 数算，一并被重置。
- 压缩**不可回看**（原文只在当轮开始的 `AutoUser` checkpoint，上限 20/session）；业界通行做法是 append-only transcript + 压缩点标记。
- tool_result context editing 的 3 个已知限制（均已写进设计文档）：① 单请求清理上限 30 条（3 批 × 10），极端会话需跨 2 个推理轮次才收敛；② 清理必然击穿 prompt cache 前缀（Anthropic 官方明写），目前 `/cost` 还不能区分"正常 miss"与"上下文编辑导致的预期 miss"；③ CAS 的 gc 只在进程启动时跑一次，长跑进程内不回收。

## [0.2.25] - 2026-10-04（原 1.5.16）

### 后台任务可见性与自动续跑（本次主线）

- **后台任务完成 → 自动唤醒 Agent 续跑**：此前后台 subagent 的结果只暂存在 `pending_bg_reminders`、等用户发下一条消息才注入；后台 shell 则只返回 `bash_N` id、输出全靠模型自己轮询 `BashOutput`。两条路都会停在等用户输入，"派完活就不动了"。现在主循环 idle 钩子（`AppState::poll_shells` + reminder 投递）在检测到后台任务完成的下一帧自动起新 turn 把结果喂回主 Agent，无需人工输入。多任务同帧完成时合并成**一轮**（`reminders.join("\n\n")`），避免连续多轮 LLM 调用浪费 token。
- **`spawn_agent_turn` 新增自动唤醒模式**（`text: Option<String>`）：常规回合传 `Some(用户输入)`，自动唤醒传 `None`——此时 reminder 本身就是这一轮唯一的 user 输入。此前若直接传 `Some(reminder 原文)`，reminder 会被当成"用户说的话"写进 session 历史并在 `/resume` 后继续显示，既污染上下文又让模型误以为用户催过它。聊天流侧只显示可读的 `⚙ 后台任务已完成，正在自动继续执行` 系统提示，不暴露 reminder 原文。
- **ESC 中断后暂停自动唤醒**（`AppState.auto_wake_paused`）：用户按 ESC 意味着"先别继续"，后台任务这时刚好完成也不应该自动把 Agent 唤醒续跑（那会显得不听话）。下一次用户自己发消息时自动恢复。
- **后台 Shell 实时输出面板**（新增，`BottomPanel::ShellProcesses` + `render::draw_shell_panel`）：此前 `BashSessionManager` 有完整输出缓冲但 TUI 侧一行代码都没接，所有 `run_in_background` 任务在界面上完全不可见——用户报告"所有 shell 后台执行的任务界面上看不见执行的内容"。现在有运行中任务时底部面板自动出现（列表：id / 命令摘要 / 状态图标 / 耗时 / 退出码），`Enter`/`Space` 展开实时输出，`PageUp`/`PageDown` 翻页看历史（默认贴底跟随新输出），`k` 终止选中任务的进程组（SIGTERM → 2s 兜底 SIGKILL）。任务退出时往聊天流追加 `⚙ 后台任务 bash_1 已退出（code 0，耗时 12.3s）`。
- **新增 `/shells [id]` slash 命令**：手动打开面板查看已结束任务的历史输出（参数接受 `bash_1` 或裸数字 `1`，带 id 直接定位；无参数定位到最近一个并展开）。已在 `/help` 正文注册（CLAUDE.md 硬性约定：新增 slash 命令必须同步 `/help`），中英双语文案走 i18n。
- **面板优先级为「用户焦点 > 有运行中任务」**：子 Agent 和后台 shell 经常同时跑（典型场景：派子 Agent 改代码 + 自己起 dev server）。若固定让子 Agent 面板占住底部位置，后台任务就永远看不见也按不了 k；现在 `Shift+↑` + `↓` 导航到 Shells 焦点即可查看，跨区链条为 `Chat ↓ → Todos ↓ → SubAgents ↓ → Shells`（反向同理）。
- **`BackgroundJob::tail()` 只读快照 API**（`crates/tools/src/bash_session.rs`）：面板每帧需要读输出，但既有的 `read_new()` 推进的是 `BashOutput` 工具的增量读游标，两者共用会把对方的增量内容抢走（面板看到空白、工具看到"无新输出"）。`tail(max_bytes)` 只做只读快照、按 char boundary 收口（中文/emoji 安全），与工具游标完全隔离。2 个回归测试锁定该不变量。
- **退出误唤醒防护**：TUI 退出时 `kill_all()` 给所有 Running job 发 SIGKILL，而 `spawn_prepared` 里的 `child.wait()` task 仍会把它们翻成 `Exited(-1)`。轮询检测若不加守卫，退出那一帧会把**每个**后台任务都误判为"刚完成"并触发一次自动唤醒。`should_quit` 守卫拦在轮询之前。
- **后台任务跨轮次保留、会话级才清**：`begin_new_turn`（每轮对话）**刻意不清** `shells`——dev server 这类任务的生命周期跨对话轮次，在这儿清会让正在运行的任务从面板凭空消失（既看不到输出也按不了 k）。真正作废的是 `reset_for_new_session`（`/clear`、新会话、resume 切会话），那里 shell 进程本身也已被 `kill_all` 收走。
- **任务列表全部完成后收成一行摘要**：`TurnDone` 刻意不清 `current_todos`（现有测试 `turn_done_auto_completes_remaining_in_progress_todos` 用 `expect("TurnDone 不应清空 current_todos")` 锁死了该行为），加上 `TodoUpdate` 全部完成时只置折叠，导致"任务已完成 [N/N]"的 2 行折叠面板（头部 + `─` 分隔线）一直挂在聊天流里，直到用户发下一条消息才消失。现在 `all_done` 时只画一行摘要（保留耗时/token 统计的可追溯信息），不画分隔线、不列条目、不给展开提示；`Ctrl+T` 在该状态下不再展开成永远渲染不出来的空壳。
- **输出回显给模型**：后台 shell 退出时读取输出尾部（64 KiB，再经 `truncate_head_tail` 收口到 4 KB）拼进 reminder，模型不主动调 `BashOutput` 也能知道结果。`wyj_core::prompts::bg_shell_done_reminder` 与既有的 `bg_agent_done_reminder` 同构。
- i18n 新增 `shell.*`（面板标题 / focus_hint / detail_hint / no_output / exited）、`shells.*`（命令 desc / bad_id / not_found / empty / headless_unsupported / exited_notice / kill_done / kill_missing / kill_nothing_selected）、`bg_wake.notice` 三组 key，中英同步。

### Prompt cache 自我击穿修复（Provider 契约变更）

- **system prompt 拆成 stable / volatile 两段**：此前 `Provider::stream/complete` 的 `system: &str` 签名强迫整个 system 压成一个字符串，Anthropic 侧只能发单个 text 块、断点打在块尾。而 system 内容里混了大量每轮会变的东西（当前工具可用性、模型兼容 suffix、子目录 CLAUDE.md reminder、Project Brief），**任何一项变化都会让整段 1.6k~5k token 的 system 全价重算**。旧代码注释里"reminder 只增不减、前缀仍可缓存"的说法是错的——追加在断点之后同样会改写断点处的前缀哈希。
  - `crates/api/src/provider.rs` 新增 `SystemPrompt<'a> { stable, volatile }`（带 `stable_only()` / `combined()` / `is_empty()`），`Provider::stream` / `complete` 签名从 `&str` 改为 `&SystemPrompt<'_>`。
  - `crates/api/src/anthropic.rs` 新增 `build_system_blocks()`：stable 块末尾打 `cache_control: EPHEMERAL`，volatile 块不打。断点预算 system 1 + tools 1 + 历史 1 = 3，仍在 Anthropic 上限 4 内。
  - `crates/api/src/openai.rs` 的 Chat Completions 无 system 块概念，用 `system.combined()` 拼成一条 system 消息。
  - `crates/core/src/agent.rs` 把 system 拆成 `system_stable`（主提示 + 记忆快照 + CLAUDE.md 祖先链）与 `system_volatile`（初始为空），循环内每轮只往 volatile 追加。
  - **记忆快照分桶缓存**（`MEMORY_SNAPSHOT_REFRESH_TURNS = 10`）：Project Brief 原本每次都按"最近 4 条 user 消息"重算相关性排序并拼进稳定前缀，等于每轮都改写前缀。改为按 `user_turns / 10` 分桶，同桶复用缓存文本、跨桶才重算——缓存抖动降到 1/10，代价是新提取的记忆最迟 10 轮后可见。
  - **【源码级破坏】** `Provider` trait 的 `system` 参数类型变更，所有自定义 `Provider` 实现必须改签名。仓内已全部改完（含约 20 个测试 mock）。辅助 LLM 调用（记忆提取 / 标题摘要 / Evolution 候选提取）一律用 `SystemPrompt::stable_only(...)` 一行迁移。
  - **【API 移除】** `wyj_core::compact::COMPACT_TRIGGER_BUFFER` 公开常量删除，改为 `compact_trigger_buffer(context_window)` 函数。全仓库无引用，但对把 `wyj_core` 当库用的下游是公开 API 移除。

### 静默失效的持久化上限修复

- **`persist_cap` 的两条 JSON 上限从上线起从未生效**：`ContentBlock::ToolUse.input` 与 `ToolResultContent::Blocks` 都是 `serde_json::Value`，旧实现把它们 `to_string` 后做字符串级 head+tail 截断（中间插入 `[truncated N bytes]` 标记），再 `from_str` 回来——插入的标记使 JSON **必然**解析失败，而失败分支是 `if let Ok(...)`，错误被静默吞掉。结果 `persist_cap.tool_use_input_bytes`（默认 64 KiB）和 tool_result 的 Blocks 路径形同虚设，超限的 `Edit`/`Write` 参数、`Bash` 大段输出直接原样进请求体并落盘。
  - `crates/core/src/serialize.rs` 新增 `shrink_json_to_budget(v, budget)`：每轮重新序列化度量字节数，找出当前最长的 string 叶子按"超出量 + 64 B 余量"做 head 截断，最多 32 轮，全程保持合法 JSON；截断走 char-boundary 回退，CJK/emoji 安全。
  - 已知边界并显式记录：没有 string 叶子的 JSON（例如几万个数字的数组）无法在不删语义的前提下裁剪，原样保留。
  - 5 个新回归测试覆盖实际截断 / 合法性 / 最长叶子优先 / char boundary 安全 / 无 string 叶子时放弃。

### 子 Agent 权限继承修复（本批风险最高）

- **修复「子 Agent 比主 Agent 更严」**：`ToolContext::allowed_tools()` 只在白名单语义（`Plan`/`Allowlist`）下返回 `Some`，对 `Prompt` 与 `AutoApprove` 一律返回 `None`。子 Agent 过去只据它决定自身模式，于是父级处于 **Bypass(AutoApprove)** 时子 Agent 落回默认 `Prompt`；而子 Agent 没有审批 UI（`ui_ask_tx` 恒为 `None`），`Prompt` + 无 UI 通道在**两道**关卡上 fail-closed（`PermissionPolicy::evaluate` 判 Deny + `ToolCtx::confirm_tool` 返回 false）。结果子 Agent 的 Bash/Edit/Write 被**全量拒掉**——用户显式选了"全部放行"，子 Agent 却比主 Agent 更严。
  - `ToolContext` 新增 `permission_mode() -> Option<PermissionMode>`（带默认实现返回 `None`，对下游 implementor 源码兼容）。
  - 新增 `derive_sub_agent_permission_mode(parent_mode, allowed, parent_is_plan)`：AutoApprove 原样传；**Prompt → AutoApprove**（委派这一步本身已在父级审批过，子 Agent 不应、也没有再弹窗的通道）；`Plan` 父级继续收窄只读白名单；`Allowlist` 原样继承；`permission_mode() == None` 时退回旧的 `allowed_tools()` 派生。
  - **安全护栏**：放宽子 Agent 内部权限的同时，`SubAgentTool` 新增 `needs_permission()`——只有被委派的 agent 类型**可能**拿到副作用工具时才弹审批（`tools: None` 的 general-purpose 需审批；`tools: Some(READONLY_TOOLS)` 的 Explore/Plan 不打断）。Explore/Plan 委派零打扰，general-purpose 委派在父级弹一次窗、进去后 Bash/Edit/Write 正常。
  - **行为变更**：Bypass 模式下子 Agent 现在真的会执行 Bash/Edit/Write（此前全量被拒）。
- **Agent 工具 schema：`description` 由必填改为可选**：多 agent 并行场景下模型经常只填 `prompt` 漏 `description`，直接触发 `tool_arguments_invalid`（用户体感是"分派失败"），而 `run_impl` 本来就会从 `prompt` 前缀派生 fallback 描述。schema 的 `required` 从 `["description", "prompt"]` 改为 `["prompt"]`，运行时校验同步只校验 prompt。回归测试 `definition_schema_accepts_missing_description` 覆盖三态。

### Agent 循环健壮性

- **孤儿 tool_use 兜底配对（修 resume 后永久 400）**：模型写工具参数时被 `max_tokens` 截断时，assistant 消息里会带一个没有配对 `tool_result` 的 `tool_use`（`pending_tools` 与 `assistant_blocks` 从同一批 `StreamedBlock` 构造、不看 `stop_reason`）。后果有二：本回合下一次请求被 provider 判为协议错误；更糟的是**该历史已落盘，任何一次 `--resume` 都会复现，用户不手动 `/rewind` 或 `/clear` 无法自愈**。现在为每个残留 tool_use 合成一条 `is_error: true` 的 tool_result（JSON 含 `error: "tool_call_incomplete"` / `stop_reason` / `instruction`），并回调 `ToolEvent::End` 让 UI 正常收口。回归测试 `max_tokens_with_pending_tool_use_still_gets_a_matching_tool_result`。

### Checkpoint 配置注入（TUI 侧失效修复）

- **`CheckpointStore::new()` 的 `max_per_session` 硬编码 0（不限）、`cas` 恒为 `None`**：CLI 装配阶段会正确配置，但 TUI 侧有 7 处用裸 `new()` 构造，其中 `attach_agent_session` 还会**覆盖掉**装配好的 store。后果是 TUI 用户只要切换过一次模型 / 模式，checkpoint 就不再封顶、文件快照也不再走 CAS 去重（同 session 从 <5 MB 退回 200 MB 量级）。
  - 新增 `CheckpointConfig { max_per_session, cas }` + 进程级 `set_checkpoint_config()` + `CheckpointStore::configured()`（用 `Mutex<Option<>>` 而非 `OnceLock` 以便单测重置）。
  - TUI 侧 7 处 + ACP 3 处 + `evolve_cmd` / `workflow_cmd` / `session checkpoint|rewind|branch` 全部改走 `configured()`。未配置时（单测、库调用方）退化为裸 `new()` 语义。

### 压缩与存储治理

- **压缩计入用量**：`/cost` 与 `WYJ_STATS_JSON` 系统性低估真实花费——压缩本身是一次真实 LLM 往返，旧实现直接丢弃 `CompletionResult` 的 usage 也不累加 `api_calls`，压缩越频繁偏差越大。现在计入 `input/output/cache_read/cache_write` 并 `api_calls += 1`，且**计入时机放在空摘要检查之前**（这轮 token 即使摘要失败也已花掉）。**行为变更**：`/cost` 数字会变高。
- **typed error 替代字符串匹配**：`compact_session_until_fit` 靠 `error.to_string().contains("消息数量过少")` 子串匹配识别"已无可压缩"的良性跳过，一旦被 i18n 本地化或改措辞就静默变成"真失败"。新增 typed `enum CompactSkip { TooFewMessages(usize), NoSafeBoundary }` + downcast helper，`Display` 仍输出原中文文案，既有日志与单测行为不变。
- **【配置项幻象清理】** 删除 `StorageRetentionCfg` 的 4 个 Phase 4 字段：`checkpoint_bytes_per_session` / `cas_total_bytes` / `cas_gc_on_start` / `checkpoint_ttl_days`。它们有非零默认值但全仓库无任何消费点（`WorkspaceCas::gc` 只被单测调用，`storage prune` / `doctor` 仍是 TODO 桩），删除不改变任何运行时行为、**迁移成本为零**（`#[serde(default)]` + 无 `deny_unknown_fields`，用户既有 config.toml 里写了这些键会被静默忽略）。**需注意**：`~/.wyj-code/cas` 与 `*.checkpoints/` 至今仍**没有任何自动或手动回收路径**，磁盘告警超阈值时只会提示手动删目录。
- **磁盘告警文案指向真实路径**：旧文案推荐 `wyj-code session prune`，但该子命令**不存在**，用户按提示操作只会得到"未知命令"。新文案直接给出可手动删除的绝对路径并说明"尚无自动清理路径"。

### CLI 健壮性与文案

- **悬空 cwd 不再 panic**：`std::env::current_dir()` 在当前工作目录已被删除时返回 `Err(NotFound)`（Unix `getcwd(2)` 的 ENOENT 语义），两处调用点曾用 `.unwrap()`——从已删除目录启动二进制会 panic，用户只看到裸 Rust 报错。现在报明确中文错误并提示"先 `cd` 到有效目录，或用 `--cwd <目录>`"，同时点明"这是 shell 层面的问题，不只影响 wyj-code"。**刻意不做 `$HOME` 兜底**：cwd 决定 project root、会话归属与项目级配置 `.wyj-code/`，静默换目录比报错危险得多。
- **`/context` 占用率不再写死 200K**：`context_window` 从写死的 `200_000` 改为 `cfg.active_profile().context_window`，非 200K 模型下占用百分比不再算错。
- **界面文案本地化修复**：`agent.compacted`（"已压缩对话历史：移除 N 条消息，节省约 M tokens"）原先是硬编码中文内联字符串，英文界面下会显示中文，现走 i18n（`agent.rs` → `wyj_i18n::tr_fmt`）。顺带修掉 `subagent` 段里 `waiting_bg` / `panel_title` / `inline_running` 三个 key 的重复定义（en.yml 与 zh.yml 各一份，后者静默覆盖前者）。
- **子 Agent 派发引导对齐 Claude Code**：Agent 工具描述从"已知就两三个文件就别派"改写为"Delegate to subagents proactively — it is the default, not a last resort"，并补上"子 Agent 有独立 context window、探索输出不污染主上下文""并行调用"等引导；主 system prompt 的 Agent 段落同步改写；三个内置类型的 description 全部重写为 Claude Code 官方措辞（强调 Explore 是只读且"任何需要读超过 handful 文件的探索都应主动派它"）。

### 回归 / 验证

`cargo fmt` + `cargo clippy --workspace --all-targets` 零警告 + `cargo test --workspace` 全绿（本次新增约 30 个回归测试：shell 面板 5 + tasklist 摘要 2 + 自动唤醒与跨轮次粒度 5 + 焦点导航 3 + ESC 暂停 1 + `tail()` 游标隔离 2 + `persist_cap` JSON 裁剪 5 + 压缩用量 1 + compact typed error 1 + `/shells` 命令 4 + Agent 工具 schema 1 + 孤儿 tool_use 1），release 构建通过。

## [0.2.24] - 2026-10-03（原 1.5.15）

- **BREAKING: 配置加载收敛到 `.wyj-code/`**（全局 + 项目级）。wyj-code 自 v0.2.24 起只读写 `~/.wyj-code/`（全局）与 `<git-root>/.wyj-code/`（项目级），不再读取 `~/.claude/`（commands / agents）、`~/.claude.json`、`<cwd>/.mcp.json`、`~/.codex/` 等任何外部配置。Skill 加载链由 6 层（内置 → `~/.wyj-code/skills` → `~/.claude/commands` → 插件 → 项目 `~/.wyj-code/skills` → 项目 `.claude/commands`）裁为 4 层（内置 → `~/.wyj-code/skills` → 插件 → 项目 `~/.wyj-code/skills`）；SubAgent 类型加载链同方向 6 层裁为 4 层。MCP 全局配置：`Config::load()` 不再合并 `~/.claude.json`，与 `Config::load_file_only()` 行为一致；项目级 `merged_mcp_servers` 不再读 `<cwd>/.mcp.json`；`project_scoped_mcp_servers` 同款；`uninstall_mcp_server` 删"原生来源 → 落禁用记录"兜底分支，统一走"从来源文件删行 + lockfile 移除"。MCP 卸载相关 4 个测试改写（`native_project_mcp_overrides_legacy_toml` 删除；`uninstall_native_origin_server_disables_instead_of_deleting` → `uninstall_wyj_config_server_removes_from_source_and_lockfile`；`uninstalled_native_server_disappears_from_installed_list` → `uninstalled_wyj_project_server_disappears_from_installed_list`；`reads_native_stdio_and_http_servers` 删除）。
- **/import 全链路删除**：`/import` slash 命令 + TUI `ImportDialog` + `ExtensionCommand::Migrate` + `crates/store/src/import.rs` 整文件（700+ 行） + `crates/config/src/codex.rs` 整文件（Codex 兼容层 150+ 行 + 5 个测试）+ `crates/commands::ImportCmd` + `commands::registry::OpenImportDialog` 枚举成员 + `crates/tui::ImportDialog` struct + `crates/tui::render::draw_import_dialog` 函数（180 行）+ `crates/tui::app::import_dialog` 状态字段 + 键盘 handler + 关闭路径 + `crates/cli::main` `OpenImportDialog` handler + `crates/cli::extensions_cmd::ExtensionCommand::Migrate` 枚举成员 + dispatch + `wyj-config` 的 `pub use codex::*` 与 `pub use project_mcp::{load_native_mcp, native_mcp_names}` re-export。i18n `help.body` 模板里 `/import` 行删除（中英同步），`import.*` keys（desc / headless_unsupported / dialog / label / report / applied_notice / nothing / shadowed_note / errors / overwritten 等）整段删除。`extensions list/doctor/install mcp/remove mcp:*` 行为不变；用户跨来源导入需求改为手工复制粘贴到 `.wyj-code/`。
- **删除死代码工具函数**：`wyj_config::load_native_mcp`、`wyj_config::native_mcp_names`、`wyj_config::codex_home_dir`、`wyj_config::load_codex_mcp`、`wyj_store::extensions::native_mcp_candidates` 全部删除（仅被 `/import` 子链路引用）；`serde_json::Value` import 在 `project_mcp.rs` 不再需要也清理。`doctor` 命令里"提示迁移原生 MCP 配置"的循环删除。
- **测试清理**：`crates/store/src/mcp_install::tests::install_then_uninstall_roundtrip` 不变（已经走 wyj 路径）；`crates/commands::skill::tests` 中 mock `.claude/commands/` 的 4 个测试改写为 `.wyj-code/skills/` 路径；`crates/core::agent_def::tests` 中 mock `.claude/agents/` 的 3 个测试改写为 `.wyj-code/agents/` 路径；Skill 链裁层后"同作用域内真 CC 覆盖 wyj"边界测试删除，等价边界由 `.wyj-code/skills` 同层测试覆盖。
- **回归 / 验证**：cargo check + cargo test 全绿（1099 测试通过，本次改动引入的失败 0；CLAUDE.md 同步更新 Skill 链 / Agent 链 / MCP 加载边界。
- **TUI 任务列表收口**：LLM 完成最后一回合直接出最终回答而不再调用一次 TodoWrite 把最后一项 InProgress 改成 Completed 时，UI 会卡在"[N/N] ⋯ xxx"这种永远不收口的"进行中"状态——典型场景：用户看到 AI 已回复但任务列表还显示进行中。`AgentEvent::TurnDone` 分支（`crates/tui/src/app.rs`）增加清扫，把任何遗留 InProgress 的 todo 自动改为 Completed，并复用既有 TodoUpdate 路径同款 `started_at.take()` + `elapsed_secs +=` 收口逻辑，保证 stats 数字连续。`Error` 分支保持现状（保留 InProgress 让用户知道哪条卡住）。新增 2 个回归测试（`turn_done_auto_completes_remaining_in_progress_todos` 核心场景 + `turn_done_with_no_in_progress_keeps_state_untouched` no-op 边界）。

## [0.2.23] - 2026-10-03（原 1.5.14）

- **统一通知通道**：任务完成 / 错误 / 后台子 Agent 完成 / 定时任务失败首次接入系统通知。新增 `wyj_core::notify`（从 `wyj_cli::notify` 上迁到 `wyj_core` 供 TUI + CLI 双向共用），覆盖 4 类事件（`TurnFinished` / `TurnError` / `SubAgentDone` / `ScheduleFailed`）+ 2 类 sink（终端响铃 stderr `\x07` + 桌面通知 macOS `osascript` / Linux `notify-send` / Windows PowerShell BurntToast）。零新第三方依赖。Config 新增顶层 `[notify]` block（`enabled`/`bell.enabled`/`desktop.enabled`/`events.{turn_finished,turn_error,subagent_done,schedule_failure}`/`rate_limit_seconds`/`include_session_id`），主开关与桌面通知 opt-out 默认开、终端响铃 opt-in 默认关、events.subagent_done 用户选择 opt-out 默认开，rate-limit 默认 30s（同类事件最小间隔防滥用，0 = 关闭）。env override 最小集：`WYJ_CODE_NOTIFY_OFF=1` master 全关、`WYJ_CODE_NOTIFY_BELL=0/1` 覆盖 `bell.enabled`、`WYJ_CODE_NOTIFY_DESKTOP=0/1` 覆盖 `desktop.enabled`（env 在 init 阶段读取，绝不写回 cfg，仿 `Config::runtime_api_key` 的 env 不回写语义）。所有 sink 失败 swallow + 首次失败 `tracing::debug!` 一次（`OnceLock` 防洪水）。7 个触发点（TUI TurnDone / TUI Error / TUI 后台 SubAgentDone / CLI `-p` / CLI headless REPL / cron schedule 失败 / daemon ACP 故意不接）已全部接入，ACP/daemon 长跑后端无人类会话上下文故不通知。9 个 notify 单测 + 3 个 config 单测全绿。
- **Ghostty 终端适配提示**：macOS 上 osascript 的通知归属是当前终端 app 的 bundle ID。在 Ghostty / iTerm / Cursor 等非 Terminal.app 终端跑 `wyj-code` 时，需在 `System Settings > Notifications > <终端 app 名>` 打开 Allow notifications；BellSink 写 stderr `\x07` 在 Ghostty 默认静音，需在 `~/.config/ghostty/config` 启用 `bell-on-urgent = true` + `bell-features = audible`。

## [0.2.21] - 2026-09-08（原 1.5.12）

- **TUI 终端 panic 兜底还原**：新增 `crates/tui/src/panic_guard.rs` 进程级 `panic::set_hook`，`cli::main()` 入口最前 `wyj_tui::panic_guard::install()`（`take_hook` 链式保留前一个 hook），`run_tui` 在 `enter/leave_terminal_screen` 配对 `mark_active()` / `mark_inactive()`（全局 `AtomicBool TUI_SCREEN_ACTIVE`），panic 触发时若 active 就 best-effort 还原终端（`DisableMouseCapture` → `LeaveAlternateScreen` → `disable_raw_mode` → `Show`，每步吞错）再调 prev hook 写 panic 信息。告别"TUI 进程 panic 后终端卡死在 raw mode + alternate screen 残留 frame + panic 写 stderr 覆盖 ratatui cell 造成画面撕裂"的复合失败模式。
- **CJK / emoji 字符串截断安全**：新增 `wyj_core::textutil::floor_char_boundary(&str, usize) -> usize`，按字节切字符串时先回退到最近 char boundary 再 `String::truncate`，治本 `memory_v3.rs` 拼装 Active Memory 上下文超过 `MAX_CONTEXT_BYTES` 时撞 CJK/emoji char boundary 触发的 `is_char_boundary` assertion panic（2026-09 用户跑 `stock_fenxi` 项目时实触发）。与 `wyj-tools::textutil::truncate_str` 分工：这里是 `usize` 索引版本，配合 `String::truncate` 做原地截断避免重复分配。
- **TUI 标题栏 / 状态栏文案精简**：`thinking_status_label` 改回单层优先级静态字符串（InProgress TodoItem 的 `active_form` / `content` → current_op 映射 `Reading file / Running command / Editing file / Updating todos / Delegating task / Browsing / Preparing plan / Running <Tool>` → permission_dialog → plan_dialog → pending_queue → fallback `Thinking`），砍掉旧的"大象装进冰箱"4 秒旋转短语（多语言干扰、跨平台兼容性差）。`draw_input` 标题栏只在 thinking 态保留 `⠋ {label}{suffix}`（spinner + label + animated dots）+ 加粗品牌橙；非 thinking 态 Plan/Bypass/Normal 三种模式标题栏分别精简为 `[plan] Enter to send` / `[bypass] Enter to send` / `Enter to send`，把 `↑↓ history / Shift+Enter newline / ! bash / / commands / Shift+Tab mode` 等长串提示砍掉（这些在顶部 `/help` 与 docs 里有完整说明）。状态栏（`draw_status`）默认右侧不再放 `ctrl+d or ctrl+c twice to exit  /help`，留给左侧"模型 / 进度 / 用量 / cwd"；thinking 指示器单点（标题栏）而非上下两处同闪烁。
- **修复 TUI 思考时光标卡死的回归**：删除 `draw_input` 在 `is_thinking=true` 时提前 `return` 的旧逻辑——那个 `return` 跳过了末尾 `f.set_cursor_position(...)`，crossterm 硬件光标永远停在"上次成功提交瞬间"的位置、用户主观感觉光标卡住/错位。现在 thinking 态仍调 `set_cursor_position` 让硬件光标跟随用户实际操作，配套新增两个 `TestBackend` 回归测试（`draw_input_keeps_cursor_visible_while_thinking` + `draw_input_title_shows_thinking_spinner_when_thinking`）防止再次回归。

## [0.2.20] - 2026-09-05（原 1.5.11）

- **Session 存储 CAS + Delta 重构（M1-M4）**：新增 `crates/core/src/workspace_cas.rs` 提供 sha256 内容寻址 Blob Pool（`intern / get / release / gc / stats`），CAS 路径 `~/.wyj-code/cas/sha256/aa/bb/<hash>.blob + .meta.json`。`FileEntry { hash, inline_bytes, size, sha256 }` 替代内联字节，serde 用 `#[serde(default, alias = "bytes")]` 兼容旧 v0.2.19 checkpoint。`WorkspaceSnapshot::Delta(DeltaSnapshot)` 同 cwd 自动 fold 父链（最多 20 层），跨 cwd 强制 baseline。`externalize_block_with` 把 >32KB image 与 >16KB thinking 外置，`cas://<hash>` 引用 + `materialize_block_with` 在 resume 时还原。`gc()` 用 `last_ref_at` LRU 淘汰 0-ref blob，对齐 git pack-files 语义。实测：单 checkpoint 11MB → ~100KB，21 checkpoint 长会话从 ~230MB → ~3MB（~99% 压缩）。
- **`/new` slash 命令**：对齐 Claude Code 新会话语义——自动保存当前会话历史后分配新 session_id、清空 TUI 状态、无二次确认弹窗；与 `/clear` 区分（清空 ≠ 全新会话）。
- **`wyj-code storage {status,doctor,prune}` 子命令**：占用诊断与 GC 治理，`status --json` 可脚本消费。
- 869 workspace 测试 + clippy clean。

## [0.2.19] - 2026-08-31（原 1.5.10）

- 50GB 级磁盘占用默认 cap：Evolution per-project 100MiB + 28–180 天 TTL；Session checkpoint 20/session、Memory v2/v3 records/Superseded 单独计数；Schedule 日志 50 文件 + run.log 10MiB×3 轮转；plugin .git 7 天 `git gc`；workspace worktree 30 天 prune；持久化前 content 截断（tool_result 20K+10K、thinking 8K、tool_use.input 64K）；顶层 `~/.wyj-code` 达 5GiB 启动一次性 warn；全部 opt-out by `0` in config。
- 首次启动 `~/.wyj-code` 缺失时 TUI 自动打开 /model 引导用户填写 API Key（焦点预置 api_key 字段），写盘后 chmod 0600 自动重建 agent，无需重启；headless / -p / ACP / daemon 无 UI 时继续报错但给出指向 TUI 与 `WYJ_CODE_API_KEY` 的可复制 hint。
- MiniMax M3 thinking 按 `effort_levels` 拆出 `ReasoningEffort`，与 M2 协议并行走不同 vendor dispatch 与 capability 路径。

## [0.2.16] - 2026-08-26（原 1.5.7）

- v0.2.16 国产模型适配 8 项收口：reasoning_content 落盘 / image_url / tool_result 降级 / R1 max_iterations / loop detection / doctor。详见对应 plan。

## [0.2.15] - 2026-08-22（原 1.5.6）

- **Memory v3 最终设计落地**：把 v3 收敛为 Global / Project 两层（删除共享 Workspace scope 与自动迁移旧数据），AI 自动管理项目记忆、Project 覆盖 Global 冲突、reference 不进入 Brief；Global 背景提取走 `PendingGlobalCandidate`，由模型用 `Memory` 工具 `list_pending_global_candidates / confirm_global_candidate / reject_global_candidate` 三步自然语言确认，reject 后的 `(scope,kind,title,content_fingerprint)` 写入 `rejected_history.json`，重复提议同一指纹会被立即拒绝。Evolution 收敛到 `GovernanceOnly`：`EvolutionStore::new` 不再 `create_dir_all("memories")`，普通 Memory 数据层只保留兼容桩，cfg 删除 `auto_activate_memories` / `generate_experiences` 字段，普通 Memory 不再被注入也不再自动生成。
- **Task 类型 + Project Brief + "继续" 语义**：新增 `MemoryClaimKind::Task` + `TaskStatus { InProgress, Completed, Cancelled, Blocked }` + `TaskStep[]` + `blocked_reason`；同 title + InProgress 的新写入自动 supersede 旧 InProgress 任务。`MemoryV3Store::project_brief` 动态生成 Project Brief（Open Tasks 含 next step + blocked_reason、Latest Mutable State、Recent Important Events、Top Relevant Project/Global Claims，reference 类型不出现），不写盘避免与结构化记忆双写漂移。Agent 在用户输入 `继续 / 接着 / continue / resume / 再来 / go on` 时自动注入最近 InProgress Task 详情 + 全部 Open Tasks 概览；无开放任务走 `memory.continuation.no_open_tasks` i18n key。
- **`/memory clear-all` 一键清空重建**：TUI `/memory` 面板新增 `ClearAll` 行 + 二级确认（Enter 进确认态、y 真正执行、Esc/n 取消），CLI `wyj-code memory clear-all [--yes]`（未带 `--yes` 直接 hard-bail）。`MemoryV3Store::clear_all` 把 `records/jobs/audit` 全部 `fs::rename` 到 `backups/<ts>/`、写 `manifest.json`、落 `reset_marker.json`，**保留 `rejected_history.json`**（用户曾拒绝的指纹不可遗忘），重建目录后 `active_records == 0`。Memory 工具收到 `action=clear_all` 立即 `ToolResult::err` 拒绝 AI 直调，与"AI 不允许静默触发清空"原则对齐。
- **Evolution 普通 Memory 死代码清理**：与 v3 数据面收敛同步，删除 `EvolutionStore` 内不再可达的 9 个普通 Memory 治理 helpers（`memory_relevance` / `is_relevance_stopword` / `should_activate_memory` / `detect_memory_conflicts` / `refresh_memory_statuses` / `repository_identity` / `memory_status_rank` / `memory_injection_priority` / `dedupe_memories`）、2 个 candidate 写入函数（`upsert_skill_candidate_unlocked` / `upsert_rule_candidate_unlocked`）、`MemoryIndex` 结构与 `MEMORY_INDEX_FILE` / `CONTEXT_HEADER` 常量、`AnalysisItem.explicit_user_statement` / `user_quote` 死字段、`resolve_legacy` 兼容方法；`cargo clippy --workspace --all-targets` 零警告零错误，775 个测试全绿。

## [0.2.14] - 2026-08-04（原 1.5.5）

- **证据化本地自进化 L0-L3**：每个用户目标落盘为独立 Episode，并按用户反馈、确定性测试、Review、工具结果和取消状态形成可审计 outcome；从成功/失败证据提炼带 scope、citation、TTL、冲突和当前分支验证的 Memory v2，按当前目标相关性和 8KB 预算选择性注入。Web/MCP/ToolSearch Episode 默认隔离，只有显式 include 后才允许形成仓库级候选。
- **Rule / Skill 候选与人工治理**：重复工作流和失败模式可生成 Rule/Skill 候选，Skill 自动构造直接、间接、信息不全、负向和安全边界共至少 8 个结构化 eval，并展示历史成功 Episode/Session 证据；当前不执行逐例 Agent replay、安装前后成功率对照或完整 benchmark。Rule 和 Skill 均不会自动激活。新增 `/evolve` 四视图治理中心与 `wyj-code evolve {status,list,review,feedback,skillize,approve,reject,rollback,forget,run,include,migrate,export,doctor}`，批准 Skill 前创建保护 checkpoint，项目/全局安装通过原子文件与 lockfile 写入，支持跨进程恢复旧内容。
- **本地、限额、可迁移**：Evolution 默认空闲 5 分钟、单 worker、每日 50,000 token / 30 分钟、每项目 100 MB；连续可恢复错误最多三次退避，三次失败后在 Health 暴露。旧 Markdown Memory 先 preview、再原子迁移并保留带时间戳备份；L4 核心代码自修改明确延后 v0.4.0，v0.2.14 不包含无人监督的自改代码。
- **TUI 多图片编辑修复**：输入框内图片占位符按顺序显示为 `[Image #1]` / `[Image #2]`；支持连续粘贴多张不同图片，并可在真实文本起点用 Backspace 从右向左逐个删除图片/文件附件，占位符仍只用于渲染，不会混入发送给模型的正文。
- **TUI 鼠标滚轮不再翻动输入历史**：根因是 Ghostty 在 alternate screen 且应用关闭 mouse capture 时，DEC mode 1007 会把滚轮转译为无修饰 `Up/Down`，随后被 Composer 的 `navigate_input_or_history` 误当真实键盘。Ghostty 直连路径现通过 Kitty 键盘 release/repeat 事件区分两者，滚轮只滚动内容区，真实 `↑/↓` 仍保留输入光标/历史语义；其它终端及 tmux/zellij 路径会成对保存、关闭、恢复 mode 1007，防止伪方向键污染输入。全程仍保持 `DisableMouseCapture`，不回退无需 Shift/Option 的终端原生拖选复制。
- **本地 Bash 环境与联网修复**：sandbox 新增 `network.allow_all` 以及 `environment.inherit/allow/deny` 配置，解决宿主网络正常时 Bash 仍表现为 DNS 解析失败、以及自定义环境变量被 `env_clear()` 无差别清空的问题；默认严格边界不变，启用继承时仍默认隔离 wyj-code 自身 provider/search/probe key。
- **computer-use 跨会话记忆污染修复**：当前请求显式注入真实工具清单并声明其高于历史记忆，`default/bypass` 仅影响审批而不移除 schema；自动记忆提取与解析同时拒绝保存"本轮/本会话缺少 Bash 或 computer-use"等瞬时运行状态。

### 0.2.14 发布后追溯补录（功能随 v0.2.14 同期 ship，但条目最初漏记在 CHANGELOG 中；本次 v0.2.15 文档整理时一并补全）

- **macOS App 启动走一次性宿主审批**：定位 `open -a` 的 LaunchServices `-54` 为 Seatbelt 专用 `lsopen` 权限缺失；通用 Bash profile 继续不授予该外部副作用，避免任意沙箱命令无提示启动 App、打开 URL/文件。Bash 新增 `run_outside_sandbox=true` 显式越界请求，仅非 Plan 的 TUI/ACP 可逐次批准且不可持久化，plan、headless、schedule、hook、SubAgent 继续 fail-closed；模型侧 GUI 指引同步要求使用该受控路径，并对旧 `-54` 输出给出可执行诊断。
- **TUI 图片占位符跟随粘贴位置**：图片/文件附件改为输入编辑序列中的结构化原子，`[Image #n]` / `[File: ...]` 会显示在实际粘贴光标处，之后输入的文字保留在占位符之后；Backspace/Delete 可在对应位置删除附件，内部标记不会进入历史或 Provider 正文。
- **畸形 Markdown 表格恢复**：当模型把标题与紧随其后的表头错误连成同一行、但下一行仍是同列数分隔行时，TUI 会在解析前安全补回换行，使持仓快照等表格恢复为网格展示；合法 Markdown 与普通标题竖线保持原行为。
- **Sandbox 联网根因修复**：`allow_all` 与域名白名单现在统一通过宿主侧回环代理解析远端域名，优先使用固定 IP 启动且证书校验的 DNS-over-HTTPS，并把系统 DNS 仅作为后备，解决网关 DNS 间歇 `SERVFAIL` 或返回 TCP 可连但 TLS 重置的错误地址时 Bash 仍表现为"沙箱不能联网"的问题；公网访问已在 macOS Seatbelt 内实测 HTTP 200，未授权域名仍 fail-closed。每次模型请求同时注入当前 sandbox 网络策略，压缩摘要与历史 `CLAUDE.md` 不再能把旧网络错误固化为当前能力；TUI `/sandbox` 也会正确显示 `allow_all`。
- **状态栏降噪**：底部状态栏移除 `schema … sent/… saved` 文案，只保留本次进程的输入/输出 token 总量；schema 统计仍保留在底层会话与诊断数据中。
- **Release CI 可恢复发布**：Release workflow 现在支持从默认分支手动选择并严格校验既有 annotated tag，checkout、tag dereference 与 `HEAD` 必须一致后才允许测试和打包；用于修复 CI 基础设施时无需移动已公开 tag，也不会把 tag 之后的产品代码混入旧版本资产。
- **Linux sandbox 发布门禁**：GitHub Ubuntu Runner 显式安装 bubblewrap；若 Ubuntu 24.04 AppArmor 启用了 `kernel.apparmor_restrict_unprivileged_userns`，只在临时 Runner 内解除限制并执行 bwrap 预检，确保 Linux 环境隔离测试验证真实 sandbox，而不是因 runner 缺少依赖或 namespace 权限误失败。

## [0.2.13]（原 1.5.4）

- **Computer-use 只读权限与工具语义对齐**：统一权限策略现在按 `action` 区分 `computer` / `app_computer` 的只读观察与变更操作。`screenshot`、`zoom`、`cursor_position`、`wait`、`list_windows`、`inspect_element` 可在 headless/daemon 的 Prompt 模式下执行，不再因为缺少交互审批通道被整类工具误拒。
- **变更操作继续失败关闭**：点击、输入、滚动及未知/缺失 action 在无 UI 表面仍拒绝；交互表面仍返回逐调用审批，AutoApprove、Allowlist、Plan、sandbox、foreground compatibility 与项目级授权边界均未扩大。
- **不可移动发布边界**：v0.2.12 已完成五平台 Release、11 个资产和 Pages 发布，因此保持原 tag 不变；该发布后发现的权限前置判断缺口以 v0.2.13 新补丁交付。

## [0.2.12]（原 1.5.3）

- **Computer-use lazy schema 保底**：ToolSearch 在工具集很大时仍保留已注册的 `window_capture`、`app_computer` 与兼容 `computer` schema，避免国内模型把“当前未展示”误判成“本会话不支持 GUI”；工具是否注册、foreground compatibility 开关、权限与 sandbox 限制均不扩张。
- **跨平台 checksum 可移植性**：Windows 打包不再用产生 CRLF 的 `Out-File` 写 sidecar，而是显式写入 ASCII + LF；`SHA256SUMS` 聚合时同时防御性移除行尾 `\r`，因此 Unix `sha256sum/shasum` 不会把 Windows 文件名解析为带回车字符。
- **发布前实物校验**：Publish Release 在上传前对五个平台归档执行 `sha256sum --check SHA256SUMS`，任一 archive、sidecar 或聚合内容不一致都会阻断 Release，不再把 checksum 可下载误当成 checksum 可用。
- **不可移动边界**：v0.2.11 的 Test/Lint、五平台 Build 与 Publish Release 均成功，但发布后实物验收发现 Windows checksum CRLF 缺陷；v0.2.12 以新 tag 修复，所有历史 tag 保持不变。

## [0.2.11]（原 1.5.2）

- **ToolSearch 核心执行面保底**：lazy schema 在大工具集下始终保留 Read/Glob/Grep/CodeSearch、Bash/Edit/Write、AskQuestion/TodoWrite、Agent/ExitPlanMode 等已注册核心工具，只延迟暴露可选集成；避免国内模型只看到搜索入口却看不到完成编码任务所需的执行工具。Plan/只读子 Agent 仍只暴露其实际注册和授权的子集。
- **Rust 1.96 严格门禁兼容**：修复 Rust/Clippy 1.96 新增的 `collapsible_match` 与 `unnecessary_sort_by` 告警，覆盖 Agent/Skill frontmatter、工具参数对象提取、MCP 工具稳定排序和 TUI import 确认路径；行为保持不变。
- **可复现 CI 工具链**：Release 的 Test/Lint 与五平台 Build、Review Action 都固定 Rust `1.96.0`，不再让可变的 `stable` 在 tag 推送后引入未本地复现的新 lint。发布前同时用 Linux Rust 1.96.1 容器执行全 workspace/all-targets 严格 Clippy。
- **不可移动发布边界**：`v0.2.10` 的 workspace tests 通过，但远端 `stable` 已升级到 Clippy 1.96，新增 lint 使其 Release Action 失败且未生成资产；`v0.2.11` 作为新的补丁版本接替发布，`v0.2.8`、`v0.2.9`、`v0.2.10` 均保持不可移动。

## [0.2.10]（原 1.5.1）

- **国内模型多工具调用兼容修复**：保守能力目录仍可声明 `max_tools_per_turn = 1` 和禁止并行，但模型若已经在一个完整响应中返回多个合法 `tool_use`，执行器会按能力声明受控串行执行并逐个回填 `tool_result`，不再把额外调用伪装成参数错误，也不会因连续完整多调用响应误触“参数重试耗尽”。工具原始 schema、权限与 sandbox 校验继续逐项生效。
- **Release CI 跨平台修复**：修复 Linux `detect_backend` 的 `clippy::needless_return`，并按实际使用平台/测试条件编译 computer-use 窗口 generation helper；本机与 Linux/Rust 1.94 的 `cargo clippy --workspace --all-targets --locked -- -D warnings` 均作为发布门禁。
- **发布边界**：`v0.2.9` annotated tag 已公开且其首轮 Release Action 在 Test & Lint 阶段失败，因此保持不可移动；`v0.2.10` 随后也因 CI 的可变 `stable` 升级到 Clippy 1.96 而未生成 Release 资产，最终发布修复迁移到 `v0.2.11`。

## [0.2.9]（原 1.5.0）

- **Workflow 自动隔离编码节点**：`wyj-code workflow validate/run/status/control` 已交付 DAG runtime、并发上限、token budget、human approval、pause/resume/retry/skip/cancel 和持久化状态。拥有 Write/Edit/Bash 且配置 `write_roots` 的 Agent/Review 节点会从当前脏工作区 checkpoint 自动创建独立 managed Git worktree；成功和失败现场都保留，结果返回 diff/review/accept 命令，不自动覆盖父 checkout。
- **Managed Worktree 完整生命周期**：`wyj-code workspace create/list/diff/accept/dispose` 支持 binary-capable diff、遗漏路径提示和选择性接受；接受前防御 symlink 逃逸、父 checkout HEAD 前进、用户并发修改与 binary 漏应用，强制清理必须显式 `--force`。
- **ACP 与全局 daemon session**：新增 stdio ACP adapter 和本地 TCP daemon。daemon 使用进程级 session registry，连接断开不再终止活动 session，新连接可 `session/load` attach，并通过 `_wyj/session/list` / `_wyj/session/control` 跨连接列出、提交、打断、rewind、branch、控制 workflow 或关闭 session；Rewind/Branch 文件恢复先 preview，确认后执行并创建保护 checkpoint。接口 schema 升级为 version 2。
- **前端无关事件流**：统一发出 text/thinking/tool/usage/error/turn finished，以及 PermissionRequested、DiffAvailable、CheckpointChanged、AgentStateChanged，供 ACP/IDE 客户端消费；stdio 连接仍在结束时清理自己的 session，daemon session 则全局存活。
- **CodeIndex 与真实 Plugin LSP 查询**：本地词法/符号索引带 ignore-aware direct-scan fallback；插件 LSP 完成 `Content-Length` framing、initialize/initialized 和 `workspace/symbol`，解析 file URI、symbol kind、container/path/line 后与本地结果合并、去重、排序。LSP 故障保持 fail-soft，不影响本地搜索。
- **Plugin Runtime 事务式激活**：已启用插件可贡献 hooks、output styles、themes、channels、LSP servers、monitors、settings schema 与 userConfig；任一 runtime contribution 无效时整插件回滚，不再留下半激活状态，名称冲突保持先到先得并记录 warning。
- **Review 与执行安全收口**：新增 `wyj-code review run` 和 GitHub Review Action，扫描 rename、空格路径、binary numstat，并对 secret evidence 脱敏；headless REPL 的 `!command` 统一走 SandboxRunner，不再通过 `sh -c` 绕过隔离。Release CI 强制执行 workspace 全量测试与 `clippy -D warnings`。
- **整合 TUI 交互改进**：纳入 Markdown 表格/timeline 物理行网格、终端原生鼠标拖选、OSC 8 文件/网页超链接、稠密渲染、welcome/theme 调整，并保持普通 `↑/↓` 留在 Composer，已退休的 `Shift+↑ content` 文案不再恢复。
- **国内模型边界不夸大**：未读取或使用此前暴露的 MiniMax Key；live probe 仍只接受独立 `WYJ_CODE_PROBE_API_KEY`。MiniMax 和其他无独立轮换 Key 的国内模型继续标记为 `static_only` / protocol-compatible。
- **版本**：工作区版本升级到 `0.2.9`；已发布的 annotated tag `v0.2.8` 保持不可移动。

## [0.2.8]（原 1.4.4）

- **国内模型可信运行时**：新增 vendor / wire protocol 分离的 `ModelCapabilities`、能力来源与置信度、静态模型目录和 TTL cache；覆盖 GLM、MiniMax、Kimi、DeepSeek、Qwen/百炼、豆包/火山，以及 Ollama/vLLM/OpenAI-compatible 兼容端点。`wyj-code model doctor` 与 `/model doctor` 默认只做静态诊断，显式 live probe 只读取独立的 `WYJ_CODE_PROBE_API_KEY`。
- **工具调用不再带病执行**：原始工具参数先经过有限 JSON 语法修复，再按原始 schema 校验；缺少必填字段或语义不明时把精确错误回灌给模型定向重试，重试耗尽后停止，禁止退化为空对象或 `null` 执行。Provider 错误统一分类，安全参数可见降级，同厂商/同角色 fallback 只在完整消息边界和可恢复错误上发生。
- **权限默认失败关闭**：无 UI 的 headless、单次 `-p`、schedule 与 SubAgent 不再把“无法询问”当成批准；Plan 模式只允许在 `doc/plan/**`、`docs/plan/**`、`.wyj-code/plans/**` 写规划文档，额外文档必须逐路径授权，源码、脚本、配置和 Bash 写入绕过继续拒绝。
- **Claude Code 式 OS sandbox**：前台/后台 Bash 和 TUI `!command` 统一进入同一 runner。macOS 使用 Seatbelt，并通过 sandbox 外的 host/port 校验代理执行域名级网络授权；Linux 使用 bubblewrap 文件系统/网络 namespace，域名代理桥接尚不可验证时明确失败关闭。凭证目录默认拒读；只有交互式 TUI 可批准一次性、不可持久化的无隔离降级。
- **效率与恢复基础**：大工具集启用 `ToolSearch` + lazy schema，小工具集保持全量 schema；sticky 生命周期、top-K 与阈值可配置，状态栏和 `WYJ_STATS_JSON` 显示 schema sent/saved。新增 checkpoint、conversation/files/both rewind、session branch，并保留分支血缘和用户真实 Git index。
- **SubAgent 与 schedule 收口**：SubAgentHub 新增 follow-up、interrupt、retry-last、父子元数据和控制事件 trace；follow-up 只在完整模型/工具边界注入。旧 schedule 自动禁用并要求权限复核，TUI 可编辑 allowed tools、write roots、allowed domains 与 require sandbox，复核后仍需用户再次显式启用。
- **Secret 与后续接口**：Profile 支持 `api_key_env`，运行时 Key 不会因设置面板保存而物化进 `config.toml`，doctor/config-status 只显示末尾掩码；冻结 `ExecutionWorkspace`、workflow/DAG、前端无关 `SessionEvent`/ACP 和 `CodeIndex` 的 P2 接口，但不宣称已交付完整 worktree、daemon、workflow 或语义索引。
- **验证状态说明**：本版本没有使用用户此前暴露的 MiniMax Key；MiniMax 与其他未提供轮换 Key 的国内模型保持 `static_only` / protocol-compatible，不宣称 live verified。Linux 域名 allowlist、原生 Windows 同等级 sandbox 仍是明确边界。
- **版本**：工作区版本升级到 `0.2.8`。

## [0.2.7]（原 1.4.2）

- **恢复终端原生鼠标选中**：TUI 启动时显式关闭 mouse capture，聊天内容可直接用鼠标拖选复制，不再要求 Shift/Option；松开修饰键后应用也不会立即用鼠标事件冲掉选区。应用内历史继续通过 PageUp/PageDown 等键盘入口浏览，OSC 8 文件与网页链接仍由终端原生 Command/Ctrl+点击打开。
- **内容区连续键盘导航**：普通 `↑/↓` 始终保留给输入框和输入历史；Todo 与 SubAgent 支持键盘选择和单任务详情逐行阅读，`Esc` 按详情 → 列表 → 输入框逐级返回并保留草稿。
- **Codex 风格静态执行流**：移除聊天消息选中、高亮、`▶`、展开/折叠、详情滚动和 `Ctrl+O`；Thinking、ToolResult 与 BashOutput 标题下最多展示 3 个终端视觉行，超出以 ASCII `...` 收束，用户输入和 AI 最终回答保持完整展示并自然换行。
- **Edit/Write 自动 diff 预览**：编辑和写入结果直接展示红色删除、绿色新增、灰色上下文，不再需要手动展开；长 diff 同样遵守三视觉行上限，历史会话恢复后仍保留工具名并正确识别 diff。
- **稳定的尾部跟随语义**：默认视口跟随内容最后一行；用户主动上滚后保持阅读位置并显示新消息提示，流式 token、工具结果和新消息不会抢走视口，滚回底部后自动恢复跟随。
- 中英文 `/help` 与输入框快捷键提示同步新交互，并补齐三行预览、长单行换行、diff 配色、跨区域导航和上滚新消息等回归测试。
- **版本**：工作区版本升级到 `0.2.7`。

## [0.2.6]（原 1.4.1）

- **TUI 剪贴板与附件体验修复**：Ctrl/Command+V 会主动读取系统剪贴板，纯图片剪贴板不再因为终端没有产生 bracketed-paste 事件而无法粘贴；文字、图片与文件路径统一走同一条处理链，配置类面板借用输入框时粘贴内容不会再穿透到聊天输入框。
- 图片与文件改为在输入框内持久显示 `[Image]` / `[File: name]` 占位符，不再额外占用一整条附件面板；占位符只参与渲染，不会混入发送给模型的正文。支持只发送附件，ESC/Ctrl+C 会一次清空文字与待发送附件，并避免重复附加同一图片或文件。
- **修复旧版前台 `computer` 连续动作误报 `target_changed`**：前台窗口识别改为保留系统 z-order，不再从展示排序后的同 App 多窗口中任取一个；截图观察在真实前台窗口未变化时可跨多次动作复用，因此“点击输入框 → 立即输入”不再被错误拦截，原有约 20 次动作上限与每次动作前的窗口身份复核仍保留。
- 强化第三方模型 computer-use 指引：精确文字、配置与诊断内容必须由工具结果或 zoom 裁剪证实；AXPress 不支持按固定控件能力处理，不盲目重试；切换到无目标绑定的旧 `computer` 前必须重新确认目标 App 位于最前台。
- **版本**：工作区版本升级到 `0.2.6`。

## [0.2.5]（原 1.4.0）

- **v0.2.5 computer-use 人机互不干扰架构**：默认改为稳定窗口目标 + macOS Accessibility/目标 PID 后台动作，不移动物理光标、不主动切换前台 App；旧全局 `computer` 降级为默认关闭的 foreground compatibility 工具，禁止从后台失败静默回退。
- 新增 marker-based `InputArbiter`：精确排除自身合成事件，前台租约被人类输入立即撤销；后台动作按事件类型与目标窗口区域识别冲突，因此用户可持续在其它 App 输入/移动鼠标，只有碰到 Agent 目标窗口时才抢占。Event Tap、权限、锁屏或事件历史异常时失败关闭。
- 新增 `window_capture list/capture`、`app_computer`、结构化安全错误、前台 PID 前后校验与会话内不兼容动作熔断；删除动作级“检测到用户输入，是否继续”暂停弹窗，headless/cron/子 Agent 无条件禁止旧前台接管。
- `/computer` 扩展为完整只读诊断：AX/Input Monitoring、稳定窗口枚举、前台回退配置和本地路径/抢占/熔断计数，其中自动前台回退计数是恒零安全不变量。
- computer-use 权限确认改为**项目级首次批准即记住**：`computer`/`app_computer` 首次按 y、Enter 或 a 后分别写入当前项目的 `allowed_tools.json`，同项目后续动作及重新打开项目不再弹窗，不同项目仍需独立确认；拒绝不落盘，普通工具的“允许一次”语义不变。
- 项目 `.wyj-code/` 资源统一按 Git 仓库根自动发现：从任意子目录启动都能加载根目录的 `settings.toml`、`mcp.toml`、`skills/`、`agents/` 和 `installed.json`；Skill 同时支持 `name.md` 与标准 `name/SKILL.md`，目录式 Skill 内的 references/assets Markdown 不会误注册为额外命令。
- 新增**项目级 MCP server 信任确认**：`.wyj-code/mcp.toml`/`.mcp.json` 里的 server 会被当子进程执行，随仓库 clone 落地即可能静默跑任意命令，因此改为按内容指纹首次需人工批准；批准记录落在仓库控制不到的 `~/.wyj-code/projects/<key>/`，TUI 面板确认，`wyj-code trust-mcp` 提供无 UI 场景下的手动批准入口，`-p`/headless/`schedule run` 未批准时静默跳过并提示。
- `.wyj-code/settings.toml` 新增 `disabled_skills`/`disabled_mcp_servers`，按名字禁用 Skill/MCP（不限来源，覆盖六层合并链任意一层），与 lockfile 的 `enabled: false`（仅覆盖 `/extensions install` 装入的条目）互补。

## [0.2.4]（原 1.3.3）

- **新增 `/schedule` 定时任务系统**（TUI 面板 + CLI `wyj-code schedule {list,add,remove,enable,disable,sync,run}`，详见 `doc/plan/v0.2.4-plan.md`）：
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

## [0.2.1]（原 1.3.0）

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

## [0.1.7]（原 1.2.2）

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
- **版本**：工作区版本升级到 `0.1.7`。

## [0.1.5]（原 1.2.0）

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

## [0.1.4]（原 1.1.0）

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

## [0.1.3]（原 1.0.2）

- 修复工具结果展开正文与摘要首行重复的问题。
- ExitPlanMode 计划面板重构为消息流内的 `PlanProposal`，支持原生鼠标滚轮滚动。
- 新增 `wyj-code update` 自更新命令（检查 GitHub Release、下载校验、原地替换二进制）。
- 新增 `build.sh release` 一键发版脚本。

## [0.1.2]（原 1.0.1）

- 逐调用工具权限确认（Edit/Write/Bash 前弹出权限对话框，支持 AllowOnce/AllowAlways/Deny）。
- 会话按项目（git 仓库根）隔离存储与恢复。
- 新增 WebSearch（Tavily）工具。
- 新增 GitHub 相关 slash 命令（`/bug` `/review` `/pr-comments` 等）。
- TUI 聊天区改用终端原生 scrollback（`ratatui::Viewport::Inline` + `insert_before`），
  对齐 Claude Code 的鼠标滚轮/原生选中复制体验。

## [0.1.1]（原 1.0.0）

- 首个正式版本：Agent 推理循环、双协议 LLM 适配（Anthropic/OpenAI）、内置工具集
  （Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite）、ratatui TUI、Profile 分组配置、
  子 Agent 编排、上下文压缩、跨会话记忆、CLAUDE.md 记忆机制、i18n（中/英）、
  MCP/Skill/Plugin 三市场、多平台 GitHub Actions Release CI。
