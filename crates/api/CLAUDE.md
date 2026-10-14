# crates/api 规约

本文件是 `crates/api/CLAUDE.md`。**按目录惰性加载**：只有当工具触达
`crates/api/` 下的文件时，加载器才把它拼进 system prompt
（`core::claude_md::ClaudeMdLoader::maybe_dir_reminder`，按目录去重、每会话只展示一次）。
根目录 `CLAUDE.md` 只留跨 crate 的通则与索引，本目录的实现细节全部沉在这里。


## System prompt 分段与 prompt cache


21. **System prompt 拆 stable / volatile 两段（v0.2.25+）**：修 Anthropic prompt cache 自我击穿。`Provider::stream/complete` 的 `system` 参数从 `&str` 改为 `&SystemPrompt<'_>`（`api/src/provider.rs`，含 `stable_only()` / `combined()` / `is_empty()`）。**背景**：旧签名迫使整个 system 压成一个字符串，Anthropic 侧只能发单个 text 块且断点打在块尾，而 system 里混了大量每轮会变的内容（工具可用性、模型兼容 suffix、子目录 AGENTS.md reminder、Project Brief），任一变化都让整段 1.6k~5k token 全价重算。**注意**：旧注释"reminder 只增不减、前缀仍可缓存"是**错的**——追加在断点之后同样改写前缀哈希。`build_system_blocks()` 只在 stable 块末尾打 `cache_control: EPHEMERAL`；断点预算 system 1 + tools 1 + 历史 1 = 3（Anthropic 上限 4）。OpenAI 侧无 system 块概念，用 `combined()` 拼成单条 system 消息。辅助 LLM 调用（记忆提取 / 标题摘要 / Evolution 候选提取）一律 `SystemPrompt::stable_only(...)`。**记忆快照分桶**：`MEMORY_SNAPSHOT_REFRESH_TURNS = 10`，Project Brief 按 `user_turns / 10` 分桶缓存（原本每轮重算相关性排序并拼进稳定前缀，等于每轮改写前缀）——代价是新提取的记忆最迟 10 轮后可见。


## Thinking 的跨轮回传

**两条 provider 路径对推理内容的回传语义完全不同，改任一侧都要先看这段。**

- **Anthropic**：`thinking` 块连同 `signature` 原样回传（`anthropic.rs` 的 `to_api_messages`），是协议硬要求，缺了工具调用续轮被拒。
- **OpenAI Chat Completions**：`reasoning_content` / `reasoning_details` 拼成 assistant 消息的**顶层字段**（不是 content 数组的一项），且**是否回传因 vendor 而异**——DeepSeek/Qwen/GLM/Doubao/Moonshot/MiniMax 要求回传（`echo_back_in_messages = true`），ollama / vllm 虽然响应里同样回填 `reasoning_content`，回传反而 400（`= false`）。这个判定由 `ThinkingResponseFields::echo_back_in_messages` 声明、`build_openai_request_body` 查表喂给 `to_api_messages`，**不是调用方传的开关**。
- **`echo_back_in_messages` 曾长期是死字段**：字段在 `thinking.rs` 里对六家 vendor 都标了 `true`，但 `to_api_messages` 的 assistant 分支只有 `Text` / `ToolUse` 两个 match、其余走 `_ => {}` 吞掉 `Thinking`。后果不只是「少回一个字段」——跨轮**没有任何历史推理作锚点**，每轮从 system prompt 独立起跑，输出语言漂移概率显著升高。已接线，单测 `assistant_echoes_reasoning_content_when_vendor_requires_it` / `assistant_omits_reasoning_fields_when_vendor_does_not_require_echo` / `body_echoes_reasoning_for_deepseek_but_not_ollama` 双向锁死。
- **反方向也要钉住**：字段带 `skip_serializing_if = "Option::is_none"` 且空 reasoning 不产出空字符串，多写一个未知字段就可能被第三方端点判 400。改动时**不得**为了「统一」把它挪进 content 数组或无条件序列化。

**思考语言没有任何代码强制**：解析 → 累积（`agent.rs` 纯 `push_str`）→ 存储 → 渲染（`render.rs::clean_thinking_lines` 只 `trim_end` + 丢空行）全链路字节级透传，唯一的语言约束是 system prompt 里那句 "reply in the user's language ... including your extended thinking / reasoning"。因此 thinking 的语言漂移只能从 prompt 侧解决，协议层改不动。
