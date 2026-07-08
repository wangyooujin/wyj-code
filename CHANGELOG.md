# Changelog

本文件记录 wyj-code 各版本的主要变更，按版本从新到旧排列。

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
