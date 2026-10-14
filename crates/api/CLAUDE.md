# crates/api 规约

本文件是 `crates/api/CLAUDE.md`。**按目录惰性加载**：只有当工具触达
`crates/api/` 下的文件时，加载器才把它拼进 system prompt
（`core::claude_md::ClaudeMdLoader::maybe_dir_reminder`，按目录去重、每会话只展示一次）。
根目录 `CLAUDE.md` 只留跨 crate 的通则与索引，本目录的实现细节全部沉在这里。


## System prompt 分段与 prompt cache


21. **System prompt 拆 stable / volatile 两段（v0.2.25+）**：修 Anthropic prompt cache 自我击穿。`Provider::stream/complete` 的 `system` 参数从 `&str` 改为 `&SystemPrompt<'_>`（`api/src/provider.rs`，含 `stable_only()` / `combined()` / `is_empty()`）。**背景**：旧签名迫使整个 system 压成一个字符串，Anthropic 侧只能发单个 text 块且断点打在块尾，而 system 里混了大量每轮会变的内容（工具可用性、模型兼容 suffix、子目录 AGENTS.md reminder、Project Brief），任一变化都让整段 1.6k~5k token 全价重算。**注意**：旧注释"reminder 只增不减、前缀仍可缓存"是**错的**——追加在断点之后同样改写前缀哈希。`build_system_blocks()` 只在 stable 块末尾打 `cache_control: EPHEMERAL`；断点预算 system 1 + tools 1 + 历史 1 = 3（Anthropic 上限 4）。OpenAI 侧无 system 块概念，用 `combined()` 拼成单条 system 消息。辅助 LLM 调用（记忆提取 / 标题摘要 / Evolution 候选提取）一律 `SystemPrompt::stable_only(...)`。**记忆快照分桶**：`MEMORY_SNAPSHOT_REFRESH_TURNS = 10`，Project Brief 按 `user_turns / 10` 分桶缓存（原本每轮重算相关性排序并拼进稳定前缀，等于每轮改写前缀）——代价是新提取的记忆最迟 10 轮后可见。
