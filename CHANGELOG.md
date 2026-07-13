# Changelog

本文件记录 wyj-code 各版本的主要变更，按版本从新到旧排列。

## [1.2.2]

- **统一 Extensions 资源平台**：新增 `wyj-code extensions` CLI 和 `/extensions` 入口，统一查看、诊断、迁移、安装、升级、启用、禁用和卸载 Skill / MCP / Plugin。
- **运行时热应用**：MCP 连接、插件 Agent 定义和工具快照在安全 Agent 回合边界原子更新；禁用/卸载后旧工具不会继续暴露给下一回合。
- **统一 Extensions TUI**：`/extensions` 提供列表、详情、启用、禁用、卸载和刷新操作，headless/CLI 继续提供稳定 JSON 输出。
- **安装可靠性与锁定**：lockfile/config 原子替换，Skill/MCP 写入失败回滚；插件依赖支持递归安装、循环检测和 semver 约束，记录 Git commit/MCP 包描述 digest。
- **lockfile v2**：保留旧字段兼容，同时新增跨类型 `extensions` 索引；安装流程会记录统一资源条目。
- **Claude MCP 兼容**：运行时读取项目 `.mcp.json` 和全局 `~/.claude.json` 的 `mcpServers`；支持显式迁移到 wyj-code 配置，原始文件保留。
- **MCP 传输扩展**：支持 stdio 与 Streamable HTTP，远程配置支持 URL、环境变量引用 header，工具名稳定映射为 `mcp__server__tool`。
- **Skill 命名空间与热发现**：递归加载 `.claude/commands/<namespace>/<name>.md`，映射为 `/namespace:name`；每个 slash 命令边界重新发现 Skill/Plugin 命令。
- **修复 AskQuestion 面板遮挡**：面板打开时强制视口回到贴底跟随并清掉选中锚点，保证选项区立即可见；面板期间豁免「最后可折叠 ToolResult」对冻结边界的封顶，提问前的长正文（分析表格等）冻结进终端 scrollback 可用滚轮回看，不再困在 Inline viewport 里被裁掉且无法查看。
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
