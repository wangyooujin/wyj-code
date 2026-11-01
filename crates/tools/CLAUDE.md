# crates/tools

工具实现。权限与 hard_deny 见根 `CLAUDE.md`。子 Agent 的提示词外壳在 `core::prompts`。

## 溢出与搜索

- `write_spill` 失败时退回截断，不把工具结果改成空串或报错。返回值不要比截断更长。文件名带时间戳，不去重。超过 7 天的 spill 在下次写入时清理。
- WebSearch 只经 `build_backend`。未知 provider 或没 key 返回 `None` 且**不注册**工具，不要静默换后端。结果先 `escape_harness_markers` 再 spill。主 Agent、子 Agent 工厂、`/model` 重建三处都要 `with_default_domains`。
- Grep：有 `rg` 走 rg，否则 `ignore::WalkBuilder`。两条路径都跳过 `.git`，并遵守 `.gitignore` / `.ignore`。凭据目录过滤见根文件，rg 与 native 都要过。

## 子 Agent 与 Bash

- 类型：内置 general-purpose / Explore（只读）/ Plan，加 `~/.wyj-code/agents` 与 `<git-root>/.wyj-code/agents`。不读 `~/.claude/agents/`。`model` 字段是 **Profile 名**。嵌套深度与并发见 `[subagent]`。触顶时 Agent 工具从该层消失，而不是留着报错。
- `SubAgentHub::emit` 是事件唯一汇聚点。trace、`a<id>.handoff.json`、`a<id>.messages.jsonl` 互补：交接物给中断后的模型，jsonl 给进程被硬杀之后。不要做 resume。
- 中断回 `subagent_handoff_body`（位置、改了什么、卡在哪、三选一）。进度必须边跑边记，因为 `abort` 不会执行任务自己的收尾。不可信正文过 `escape_harness_markers`。
- 前台 Bash 把 `WYJ_CODE_SKILL_DATA` 指到 `layout::project_skill_data_root`。后台任务同样要有。不要用「指纹忽略某些目录」代替把产物搬出技能树。没写这个变量的命令不要因此创建空目录。
- 信号杀死的命令（`status.code()` 为 `None`）必须说「可能已有部分副作用」，不能报成退出码 -1。

## 桌面控制

`computer` / `window_capture` 要平台 + `profile.vision`。`app_computer` 只要 macOS；`vision=false` 时用 `without_image_channel()`，并改注入文本提示，不要注入假定「截图会回到眼前」的 `COMPUTER_USE_HINT`。provider 不参与「注不注册」的判定。官方 Anthropic 才用原生 computer 声明，其余走 custom schema，描述必须自解释。

- `generation` / `tree_id` 是十进制字符串，原样抄写。写成 JSON number 会在 2^53 处舍入。
- 坐标用截图空间。逻辑点与物理像素不是同一套；缩放在 `computer::scale`，不要在工具里再写一套。
- `tree_id` 只证明 ref 来自哪一次观察。界面是否变了看 `content_fingerprint`。
- 遮挡检测只在坐标降级时做：窗口截图能拍到被盖住的内容，HID 点击按 z-order 命中。
- 越界用矩形相交，不用中心点。多显示器原点可以是负数，不要把裁剪钳进 `[0, width)`。
- AX 对自绘应用会失败。降级到坐标时仍要 `InputLease`、事件标记、前后台 PID 校验、光标无条件复原。`description` 写明降级存在。只在 `requires_foreground_takeover` 时降级，`target_changed` / `preempted_by_user` 不降级。
- seqlock 读者必须有界。人在动鼠标时无界自旋会卡死。
- `list_elements` 默认紧凑文本，JSON 要显式要。`region` 用截图坐标。`include_text` 才能带上 `AXStaticText`（role 白名单否则会丢掉它）。
- 成功动作后的自动附图失败不得改变结果语义。元素 press 失败先试一次 `AXScrollToVisible`。
- 右键走 `show_menu`，不要为此退回全局 `computer`。拖拽必须插值。中文输入走剪贴板或 `type_text` 的 Unicode 通道，不要只用 ANSI keycode。
- OCR 只在 AX 树是空的时候用。AppleScript 只有编译期白名单 `run_recipe`，不给模型通用脚本。
- `/computer` 面板对齐走 `DiagPanel`，不要把空格写进 i18n 字符串。
