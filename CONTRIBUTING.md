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

**改了 harness 相关的东西，还要跑评测基线。** prompt、工具描述、工具可见性、
压缩/清理策略、验收门禁——这些改动的效果**不在 `cargo test` 的覆盖范围里**：
`cargo test` 验证代码正确性，评测基线验证模型能力。两者是不同的东西。

```bash
cargo build --release
benchmarks/verify-tasks.sh          # 先自检任务集，不花 API 费用
benchmarks/run.sh --repeat 3        # 至少 3 次重复
benchmarks/compare.sh <baseline> <candidate>
```

三条纪律：**一次只改一个组件**、**每组至少 3 次重复**、**至少 2 个模型对照**。
理由与完整任务清单见 `benchmarks/README.md`。

> 为什么这条值得写进贡献指南：外部受控实验里 harness 的方差可达模型方差的 7.8 倍，
> 而**很多 harness 改动是负收益**（verifier 在某些基准上是 −0.8pp，多候选搜索是 −5.6pp）。
> 没有基线，这些改动只能靠感觉判断。

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

## 密钥与凭据

**不要提交任何真实凭据**——API Key、token、私有 endpoint、带密码的连接串都不行，
包括测试文件、README 示例和注释里的"临时这么写一下"。

本地开发用的密钥放两处之一：

- 环境变量（如 `WYJ_CODE_API_KEY`）；
- `~/.wyj-code/config.toml`（仓库外的全局配置，不受 git 管理）。

配置类仓库文件（`.wyj-code/settings.toml` 等）只写占位符，例如 `api_key = "your_ark_key"`。

提交前会自动扫描：执行一次 `./scripts/install-git-hooks.sh` 装上 `pre-commit` 钩子
（需要 `brew install gitleaks`）。CI 的 `secret-scan.yml` 会在每次 push / PR 上
扫描**全历史**，即使本地用 `--no-verify` 绕过也会被拦下。

规则在 `.gitleaks.toml`。确认是误报（测试里的假 token、占位符）时，把对应模式加进
`[allowlist]`，而不是关掉扫描。

**已经泄露了怎么办**：先立刻在服务商后台吊销/轮换密钥——清理 git 历史不能让她签发的
密钥失效。然后再处理历史（`git filter-repo --replace-text`）。

## 提交 PR

1. Fork 仓库，基于 `master` 建特性分支。
2. 完成改动后跑一遍上面的构建/测试/lint 命令，确保全绿。
3. 提交 PR，描述清楚改动动机（为什么，而不只是做了什么）与接口/行为变化。
4. 涉及 CLI 参数、配置项或架构决策的改动，建议同步更新 `CLAUDE.md` 对应章节。

## 报告问题

发现 bug 或有功能建议，请在 GitHub Issues 中描述：复现步骤、预期行为、实际行为，以及运行环境
（OS、`wyj-code --config-status` 输出）。
