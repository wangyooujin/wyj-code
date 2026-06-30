# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build --release            # 构建 release 版本 → target/release/wyj-code
cargo run                        # 启动 TUI 模式
cargo run -- --headless          # 启动 headless REPL 模式
cargo run -- -p "your prompt"    # 单次问答（不启动 TUI）
cargo run -- --config-status     # 查看当前配置和 API Key 状态

./build.sh                       # 等同 cargo build --release
./build.sh package               # 打包到 dist/<binary>-<version>-<platform>
./build.sh install               # 安装到 ~/.local/bin/wyj-code
./build.sh cross linux-x86_64    # 交叉编译（支持 linux-x86_64, linux-aarch64, macos-*）
```

## Test & Lint

```bash
cargo test                       # 全量测试
cargo test -p wyj-core           # 指定 crate 测试
cargo fmt                        # 格式化（max_width = 100，见 rustfmt.toml）
cargo clippy                     # lint
```

## Configuration

配置文件：`~/.wyj-code/config.toml`，API Key 优先读取环境变量 `WYJ_CODE_API_KEY`。

```toml
provider = "anthropic"       # 或 "openai"
model = "claude-opus-4-8"
base_url = ""                # 留空使用供应商默认端点
max_tokens = 8192
context_window = 200000
log_level = "warn"           # 调试时设为 "debug"

[[mcp_servers]]
name = "my-server"
transport = "stdio"
command = "/path/to/server"
args = ["--flag"]
```

**WYJ.md**：在项目根目录放置 `WYJ.md`，其内容会自动追加到 system prompt，类似 CLAUDE.md 的作用。

## Architecture

这是一个 Rust workspace，单一 `wyj-code` 二进制，零遥测。各 crate 职责：

| Crate | 名称 | 职责 |
|---|---|---|
| `crates/config` | `wyj-config` | 配置加载（`~/.wyj-code/config.toml`）、MCP 配置结构 |
| `crates/api` | `wyj-api` | LLM Provider 抽象 trait + Anthropic/OpenAI 双格式实现，SSE 流式解析 |
| `crates/core` | `wyj-core` | Agent 推理循环、Session、HistoryStore、MemoryStore、上下文压缩 |
| `crates/tools` | `wyj-tools` | 工具实现（Read/Write/Edit/Bash/Glob/Grep/WebFetch/TodoWrite/SubAgent）|
| `crates/commands` | `wyj-commands` | Slash 命令注册表与内置命令（/help、/compact 等）|
| `crates/mcp` | `wyj-mcp` | MCP 客户端桥接（stdio/http 传输）|
| `crates/tui` | `wyj-tui` | ratatui TUI：渲染、输入框、权限确认对话框 |
| `crates/cli` | 二进制入口 | 组装所有 crate，解析 CLI 参数，启动 TUI/REPL/单次模式 |

### 核心数据流

1. **Tool trait**（`core::tool`）：所有工具实现 `async fn run(input: Value, ctx: &dyn ToolContext) -> Result<ToolResult>`，由 `tools::ToolRegistry` 统一管理。
2. **Agent 推理循环**（`core::agent::Agent::run_turn`）：流式接收 LLM 输出 → 累积工具调用 → 顺序执行（因 `ToolContext` 非 Send）→ 将结果追回 session → 继续直到 `stop_reason != tool_use`。
3. **上下文压缩**（`core::compact`）：估算 token 数（字符数/3 粗略），当 `estimated > context_window - 40_000` 时调用 LLM 生成摘要替换旧消息，保留最近 6 条。
4. **跨会话记忆**（`core::memory::MemoryStore`）：每轮对话结束后 `tokio::spawn` 后台提取记忆，写入 `~/.wyj-code/memory/<project-id>/`；下次启动时读取 MEMORY.md 索引注入 system prompt。
5. **MCP 桥接**（`mcp::bridge`）：连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。
6. **SubAgent**：`tools::SubAgentTool` 通过工厂函数创建拥有独立 provider 和工具集的子 Agent，顺序执行不嵌套并发。

### 权限模型（TUI）

工具调用前通过 `mpsc` channel 发送 `AgentEvent::PermissionRequest`，TUI 渲染确认对话框，用户按 `y`/`n` 回复，Agent 协程阻塞等待。
