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

### 基础配置

```toml
provider = "anthropic"   # 或 "openai"
model = "claude-opus-4-8"
base_url = ""            # 留空使用供应商默认端点
# api_key 优先从环境变量 WYJ_CODE_API_KEY 读取
```

### 不同模式下的模型

wyj-code 有三种 Agent 运行模式，可为不同模式分别指定模型，留空时回退到 `model` 字段。

| 模式      | 触发方式（示例）   | 行为                                     | 专用配置字段     |
| --------- | ------------------ | ---------------------------------------- | ---------------- |
| `normal`  | 默认               | 全部工具可用，TUI 下工具调用前弹确认     | `exec_model`     |
| `plan`    | Plan 分析          | 仅允许只读工具（read / glob / grep / web_fetch） | `plan_model`     |
| `bypass`  | 自动放行           | 自动允许所有工具调用，不弹确认对话框     | `exec_model`     |

> 说明：`normal` 与 `bypass` 共享 `exec_model`（执行型模型），`plan` 模式单独使用
> `plan_model`（规划型模型）。通过 `Config::model_for_mode(mode)` 在运行时解析。

#### 示例：规划用快模型、执行用强模型

```toml
provider    = "anthropic"
model       = "claude-opus-4-8"          # 全局兜底模型
plan_model  = "claude-haiku-4-5"         # Plan 模式：规划分析，便宜快速
exec_model  = "claude-opus-4-8"          # Normal/Bypass：实际改代码，用强力模型
base_url    = ""
```

#### 示例：Anthropic 主模型 + OpenAI 备援

```toml
provider    = "anthropic"
model       = "claude-opus-4-8"
plan_model  = "gpt-4o-mini"              # 用 OpenAI 兼容端点跑轻量规划
exec_model  = "claude-opus-4-8"
base_url    = ""                          # provider 决定协议与默认端点
```

如果 `plan_model` / `exec_model` 留空或省略，则使用 `model` 字段。

### 运行时切换模型

在 TUI 会话中可用 `/model` 命令查看或临时切换当前会话模型：

```text
/model                  # 显示当前模型
/model claude-haiku-4-5 # 切换当前会话模型（不写盘）
```

持久化的模式-模型映射请直接编辑 `~/.wyj-code/config.toml`。

## 许可证

MIT OR Apache-2.0
