# crates/mcp

`mcp::bridge` 把外部 MCP server 的工具包装成 `Tool` 注册进 Agent。stdio / http 两种传输。

项目级 `mcp.toml` 的信任门控在 `crates/store/CLAUDE.md`，不在这里。提供 shell 的 MCP 工具必须接上 `hard_deny_denial()`（见根 `CLAUDE.md`）。
