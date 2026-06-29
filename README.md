# wyj-code

一个用 **Rust** 编写的终端 AI 编程助手，单一静态二进制、零遥测。

> **独立实现声明**：本项目是 clean-room 净室实现，仅依据公开的 Anthropic Messages API、
> OpenAI Chat Completions API、MCP/LSP 公开规范实现等价功能。不包含任何第三方的
> 专有系统提示词、工具描述文案或品牌资产。所有 prompt 与文案均为原创。

## 特性（增量交付）

- M0 ✅ 工程骨架 + 配置读取
- M1 🔄 双格式 API 客户端（Anthropic / OpenAI）+ headless 闭环
- M2 🔄 核心工具（Bash/Read/Write/Edit/Glob/Grep/WebFetch）
- M3 🔄 ratatui 富终端 UI
- M4 🔄 Slash 命令系统
- M5 🔄 TodoWrite + 子 Agent/Task
- M6 🔄 MCP 客户端
- M7 🔄 单二进制多平台分发

## 构建

```bash
cargo build --release   # → target/release/wyj-code
cargo run -- --version
cargo run -- --config-status  # 查看配置状态
```

## 配置

配置目录：`~/.wyj-code/`，主配置文件 `config.toml`。

```toml
provider = "anthropic"   # 或 "openai"
model = "claude-opus-4-8"
base_url = ""            # 留空使用供应商默认端点
# api_key 优先从环境变量 WYJ_CODE_API_KEY 读取
```

## 许可证

MIT OR Apache-2.0
