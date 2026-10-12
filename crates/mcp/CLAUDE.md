# crates/mcp 规约

本文件是 `crates/mcp/CLAUDE.md`。**按目录惰性加载**：只有当工具触达
`crates/mcp/` 下的文件时，加载器才把它拼进 system prompt
（`core::claude_md::ClaudeMdLoader::maybe_dir_reminder`，按目录去重、每会话只展示一次）。
根目录 `CLAUDE.md` 只留跨 crate 的通则与索引，本目录的实现细节全部沉在这里。


## MCP 桥接


6. **MCP 桥接**（`mcp::bridge`）：连接外部 MCP server，将其工具包装成 `Tool` trait 对象注册到 Agent。
