# Changelog

本文件记录 wyj-code 各版本的主要变更，按版本从新到旧排列。

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
