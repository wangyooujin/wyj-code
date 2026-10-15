# crates/api 规约

本文件是 `crates/api/CLAUDE.md`。**按目录惰性加载**：只有当工具触达
`crates/api/` 下的文件时，加载器才把它拼进 system prompt
（`core::claude_md::ClaudeMdLoader::maybe_dir_reminder`，按目录去重、每会话只展示一次）。
根目录 `CLAUDE.md` 只留跨 crate 的通则与索引，本目录的实现细节全部沉在这里。


## System prompt 分段与 prompt cache


21. **System prompt 拆 stable / volatile 两段（v0.2.25+）**：修 Anthropic prompt cache 自我击穿。`Provider::stream/complete` 的 `system` 参数从 `&str` 改为 `&SystemPrompt<'_>`（`api/src/provider.rs`，含 `stable_only()` / `combined()` / `is_empty()`）。**背景**：旧签名迫使整个 system 压成一个字符串，Anthropic 侧只能发单个 text 块且断点打在块尾，而 system 里混了大量每轮会变的内容（工具可用性、模型兼容 suffix、子目录 AGENTS.md reminder、Project Brief），任一变化都让整段 1.6k~5k token 全价重算。**注意**：旧注释"reminder 只增不减、前缀仍可缓存"是**错的**——追加在断点之后同样改写前缀哈希。`build_system_blocks()` 只在 stable 块末尾打 `cache_control: EPHEMERAL`；断点预算 system 1 + tools 1 + 历史 1 = 3（Anthropic 上限 4）。OpenAI 侧无 system 块概念，用 `combined()` 拼成单条 system 消息。辅助 LLM 调用（记忆提取 / 标题摘要 / Evolution 候选提取）一律 `SystemPrompt::stable_only(...)`。**记忆快照分桶**：`MEMORY_SNAPSHOT_REFRESH_TURNS = 10`，Project Brief 按 `user_turns / 10` 分桶缓存（原本每轮重算相关性排序并拼进稳定前缀，等于每轮改写前缀）——代价是新提取的记忆最迟 10 轮后可见。


## Thinking 的跨轮回传

**两条 provider 路径对推理内容的回传语义完全不同，改任一侧都要先看这段。**

22. **请求侧的 thinking 形态由 vendor × 线协议共同决定，且两家原生形态互不兼容**（`thinking.rs::apply_anthropic_thinking` → `AnthropicThinkingPatch`）。**背景**：Anthropic 路径原先无条件发 `{type:"enabled", budget_tokens:N}`，这对官方 Anthropic 是对的，对 MiniMax 的 `/anthropic` 端点则是两个都不认的死字段——它的 `budget_tokens` 没有语义，思考深度由 `output_config.effort`（`low/medium/high/xhigh/max`）档位控制，于是 profile 里的 `reasoning_effort` 成了**从未上送的死配置**（改配置毫无效果，且碰巧默认 max 掩盖了问题）。端点只承认三种 `thinking.type`：**省略** / `adaptive` / `disabled`，三者的语义还随代际变：

   | 模型 | 省略 | `adaptive` | `disabled` |
   |---|---|---|---|
   | M3.1-Flash-Preview | 开 | 开 | **400** |
   | M3 | 关 | 开 | 保持关 |
   | M2.x | 开 | 开 | 接收但不生效 |

   因此 adapter 的硬约束是：绝不发 `enabled` / `budget_tokens`；`disabled` 只在 M3 上真能生效，M3.1 发它会 400、M2.x 发了也无效，两者一律降级成「不发 thinking 字段」；`effort` 只在 M3.1 上送且必须命中支持档位。**被否决的配置项不会静默丢弃**——进 `patch.ignored`，provider 打 warn，否则用户会一直以为配置生效了。**注意**：`max_tokens` 被抬高到 `budget+4096` 的兜底只对含 `budget_tokens` 的形态生效，MiniMax 的 adaptive 形态不受此约束；该兜底读的是 `patch.budget`（结构化 `Option<u32>`），**不是**从 `patch.thinking` 这个 `Value` 里反解键名——那是一次字符串往返，adapter 改个键名就会静默失效而后果是端点 400。

   **开/关只认显式 `thinking_switch`，`thinking_budget` 一律不算信号。**「被忽略的参数」不得反过来改变行为：若拿 budget 去翻开关，就会出现「一边记 `ignored`、一边按它改行为」的自相矛盾，且对 M3 是把思考从默认关翻成开这种用户没要求的行为翻转。切勿用 `effective_switch_type()`（它含「配了 budget/effort ⇒ 想开」的推断）判断这件事，用 `explicit_switch()`。

   - **⚠️ 真正的坑不在 adapter，在 catalog 层：adapter 写得对也可能是死代码。**`request_plan` 与 `agent.rs` 都以 `ThinkingSpec::for_vendor(...).effort_levels` 为唯一判据决定 `reasoning_effort` 是否放行，而 `ThinkingSpec::for_vendor` 一旦漏了某条 `(wire, vendor)` 组合（MiniMax + `AnthropicMessages` 曾经就漏着，落到 `anthropic_compatible()` 的 `effort_levels: &[]`），profile 里的 effort 会在**到达 provider 之前**被丢成 `DroppedParameter`，`output_config` 分支永不执行。现已补 `(AnthropicMessages, "minimax")` 臂（`ThinkingSpec::minimax_anthropic`）。**改任何 vendor × 线协议的 thinking 形态时，先确认 for_vendor 有对应臂，再写 adapter。**
   - **`ThinkingSpec::effort_levels` 是档位判据的唯一真相源**，adapter 一律调 `normalize_effort()`（这也是 `apply_anthropic` 为什么要收 `identity` 参数），禁止在 adapter 里维护第二份白名单。
   - **模型名分派只认版本段，不认营销词**：`is_minimax_m31_flash` 只匹配 `m3.1` / `m3-1` / `m31`。早先还带 `contains("flash")`，会把将来的 `M2.x-Flash` 误路由到 M3.1 从而白发 `output_config.effort`——分派错误在这里是 400 级事故。
   - **旧 config 里「budget + effort」都配**时，`budget_tokens_supported: false` 曾让 `request_plan` 的 match 臂在 budget 上短路成 `Disabled`，把 effort 连坐丢掉（用户只看到一条 thinking_budget dropped，reasoning 实际已整条关闭）。现已改为 budget 记 dropped 后继续走 effort 分支，`minimax_anthropic_spec_refuses_budget_but_keeps_effort` 锁住。
   - 单测：`minimax_m31_maps_effort_to_output_config_not_budget_tokens` / `minimax_m31_refuses_disabled_instead_of_sending_a_400` / `minimax_m3_honours_disabled_and_defaults_to_off` / `minimax_m2_forces_thinking_on_and_drops_effort` / `minimax_budget_alone_never_flips_the_switch` / `minimax_explicit_switch_on_maps_to_adaptive` / `official_anthropic_keeps_the_native_budget_shape` / `anthropic_native_reports_switch_that_budget_overrode` / `text_only_switch_is_honoured_on_official_anthropic_without_warning`。**其中最后两条是端到端的**（`minimax_effort_survives_the_catalog_layer` 从 profile 出发走 `RequestPlan::from_profile` → `from_request_plan` → adapter）——前面那些 adapter 单测都手工构造 `RequestOptions`，恰好绕开会杀人的那一层，写新 adapter 时必须补同形状的端到端断言。

- **Anthropic**：`thinking` 块连同 `signature` 原样回传（`anthropic.rs` 的 `to_api_messages`），是协议硬要求，缺了工具调用续轮被拒。
- **thinking 不得被任何截断逻辑改写**：`core::serialize::truncate_content_block` 的 `Thinking` 分支是 no-op，`persist_cap` 的 `thinking_bytes` / `reasoning_details_bytes` 已废弃（字段保留仅为兼容旧 `config.toml`，默认值归零）。**原因**：截断文本却保留 `signature` 会让签名校验失配，`reasoning_details` 则是改 `text` 却留下 `id` / `format`，产出结构不自洽的块——且这类改写**不可逆**。体积改由 context_edit / compact 收拾（语义是「丢弃整段历史」，不留半损坏的块）。**与之相对，CAS 外部化（>16KB 落盘外置）保持启用**：它可逆，resume 时 `materialize_block_with` 会原样还原。单测 `thinking_blocks_are_never_truncated_even_above_cap`。
- **OpenAI Chat Completions**：`reasoning_content` / `reasoning_details` 拼成 assistant 消息的**顶层字段**（不是 content 数组的一项），且**是否回传因 vendor 而异**——DeepSeek/Qwen/GLM/Doubao/Moonshot/MiniMax 要求回传（`echo_back_in_messages = true`），ollama / vllm 虽然响应里同样回填 `reasoning_content`，回传反而 400（`= false`）。这个判定由 `ThinkingResponseFields::echo_back_in_messages` 声明、`build_openai_request_body` 查表喂给 `to_api_messages`，**不是调用方传的开关**。
- **`echo_back_in_messages` 曾长期是死字段**：字段在 `thinking.rs` 里对六家 vendor 都标了 `true`，但 `to_api_messages` 的 assistant 分支只有 `Text` / `ToolUse` 两个 match、其余走 `_ => {}` 吞掉 `Thinking`。后果不只是「少回一个字段」——跨轮**没有任何历史推理作锚点**，每轮从 system prompt 独立起跑，输出语言漂移概率显著升高。已接线，单测 `assistant_echoes_reasoning_content_when_vendor_requires_it` / `assistant_omits_reasoning_fields_when_vendor_does_not_require_echo` / `body_echoes_reasoning_for_deepseek_but_not_ollama` 双向锁死。
- **反方向也要钉住**：字段带 `skip_serializing_if = "Option::is_none"` 且空 reasoning 不产出空字符串，多写一个未知字段就可能被第三方端点判 400。改动时**不得**为了「统一」把它挪进 content 数组或无条件序列化。

**思考语言没有任何代码强制**：解析 → 累积（`agent.rs` 纯 `push_str`）→ 存储 → 渲染（`render.rs::clean_thinking_lines` 只 `trim_end` + 丢空行）全链路字节级透传，唯一的语言约束是 system prompt 里那句 "reply in the user's language ... including your extended thinking / reasoning"。因此 thinking 的语言漂移只能从 prompt 侧解决，协议层改不动。
