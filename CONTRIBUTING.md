# 贡献指南

感谢你对 wyj-code 感兴趣。这份文档是给贡献者看的操作手册；项目整体架构、数据流、各 crate 职责
的详细说明见 [`CLAUDE.md`](./CLAUDE.md)——那份文档同时也是喂给 Claude Code 的项目上下文，
是最权威、更新最及时的架构参考。

## 构建 & 运行

```bash
cargo build --release            # 构建 release 版本 → target/release/wyj-code
cargo run                        # 启动 TUI 模式
cargo run -- --headless          # 启动 headless REPL 模式
cargo run -- -p "your prompt"    # 单次问答（不启动 TUI）
cargo run -- --config-status     # 查看当前配置和 API Key 状态

./build.sh                       # 等同 cargo build --release
./build.sh package               # 打包到 dist/<binary>-<version>-<platform>
```

## 测试 & Lint

提交前请确保以下命令全部通过：

```bash
cargo fmt                        # 格式化（max_width = 100，见 rustfmt.toml）
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Crate 职责

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session、HistoryStore、MemoryStore、ClaudeMdLoader、Hooks、上下文压缩 |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/Glob/Grep/WebFetch/WebSearch/TodoWrite/SubAgent 等）|
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/i18n` | `wyj-i18n` | 多语言资源（`en`/`zh`）与运行时语言切换 |
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/store` | `wyj-store` | MCP/Skill/Plugin 配置管理数据层、自更新逻辑 |
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

详细的数据流（Agent 推理循环、上下文压缩、CLAUDE.md 注入、Hooks 生命周期、子 Agent 编排等）
请阅读 `CLAUDE.md` 的 Architecture 一节，不在此重复。

## 提交约定

- **commit message 用简体中文**，首行 `type: 简述`（如 `fix: 修复xxx`、`feat: 新增xxx`），空行后列要点。
- **不要**在 commit message 里添加 `Co-Authored-By: Claude <noreply@anthropic.com>` 或任何指向
  Claude / Anthropic 的署名 trailer。本仓库希望提交历史只展示真实人类作者。
- 新增 `/xxx` slash 命令时，必须在同一提交里同步更新 `/help` 输出
  （`crates/i18n/locales/{en,zh}.yml` 的 `help.body` 模板），否则该命令对用户等同不可见。
- 面向用户的文案（TUI 对话框、slash 命令输出、CLI `--help`）需要走 i18n（`tr()` key），
  不要硬编码中文或英文字符串。

## 提交 PR

1. Fork 仓库，基于 `master` 建特性分支。
2. 完成改动后跑一遍上面的构建/测试/lint 命令，确保全绿。
3. 提交 PR，描述清楚改动动机（为什么，而不只是做了什么）与接口/行为变化。
4. 涉及 CLI 参数、配置项或架构决策的改动，建议同步更新 `CLAUDE.md` 对应章节。

## 报告问题

发现 bug 或有功能建议，请在 GitHub Issues 中描述：复现步骤、预期行为、实际行为，以及运行环境
（OS、`wyj-code --config-status` 输出）。
