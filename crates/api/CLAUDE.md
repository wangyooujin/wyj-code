# crates/api

Provider、SSE、能力表。改 thinking 或 vendor 时先确认 `ThinkingSpec::for_vendor` 有对应臂，再改 adapter。adapter 单测绕开 catalog；新行为要有从 profile 走到 adapter 的端到端测试。

## vendor

`infer_vendor`（`models.rs`）是全仓唯一一份。`Profile.vendor` / `wire_protocol` 已删除，线协议由 `provider` 决定。顺序不能调：

1. 本地端点（ollama 端口 11434，vLLM 路径或端口 8000）先于模型名。`qwen3-coder` 跑在本地不能认成阿里云。vLLM 默认 URL 不含 "vllm" 字样。
2. `base_url` 为空 → provider 的官方端点。必须在「模型名带 `/` 或 `:`」之前，否则 OpenAI 微调 ID 会被当成聚合路由。
3. 域名精确或子域命中 `VENDOR_DOMAINS`。不要用子串（`z.ai` 会误伤 `siz.ai`）。
4. 聚合路由：域名特征词，或模型名带 `/`、`:` → `"custom"`。中转端点不认厂商专属字段，发了会 400。
5. 模型名 needle。兜底必须是 `"custom"`，不能是 `"anthropic"`。

## 缓存分段

前缀顺序 `tools → system → messages`。tools 只增不减。system 任何字节变化都让 messages 断点失效。能进历史就别进 system。断点打在 stable 段末尾。辅助调用（记忆、标题、evolution）用 `SystemPrompt::stable_only`。`capabilities.prompt_cache.value` 是缓存开关的唯一真相源。

## Thinking

- 截断逻辑不得改写 thinking / `reasoning_details`。签名对不上，续轮会被拒，且不可逆。体积交给 context edit / compact。
- 被丢弃的参数进 `patch.ignored` 并 warn，不能静默丢掉。
- 开/关只认显式 `thinking_switch`。`thinking_budget` 不能反过来当开关。用 `explicit_switch()`，不用 `effective_switch_type()`。
- `ThinkingSpec::effort_levels` 是档位唯一白名单。adapter 调 `normalize_effort()`，不要再维护一份。
- 型号分派只认版本段（`m3.1` / `m3-1` / `m31`），不要加 `flash` 这类营销词。次版本只认 1 位，避免把日期段当成主版本。
- MiniMax `/anthropic`：不发 `enabled` / `budget_tokens`。`disabled` 只在 M3 真能关；M3.1 发它会 400，M2.x 发了无效，两者改为不发该字段。`effort` 只在 M3.1 上送。
- 官方 Anthropic：4.6 及更早才发 manual `budget_tokens`；4.7+ 拒绝它。`disabled` 在允许的型号上必须真的发出 `{type:"disabled"}`。Opus 5.5 关不掉。budget 与 effort 可共存（`BudgetAndEffort`）；互斥端点（如 Qwen）不要同发。思考 token 读 `output_tokens_details.thinking_tokens`，并 `min(output_tokens)`。
- 官方 OpenAI：只发顶层 `reasoning_effort`，不回传 reasoning，不发 `thinking` / `max_tokens`（用 `max_completion_tokens`）。`text_only` 降到 `low`，不是 `"none"`。早期 o1-mini / o3-mini 档位为空就记 dropped。第三方仍用 `max_tokens`，两个字段不要同发。
- OpenAI 兼容线：`reasoning_content` 与 `reasoning` 都要解析，取非空的一个，不要拼接。是否回传看 `echo_back_in_messages`（DeepSeek/Qwen/GLM/豆包/Moonshot/MiniMax 要回传；ollama/vLLM 回传会 400）。空字段不要序列化。
- 同一 SSE chunk 里的 reasoning、content、tool_calls、finish_reason 都要发出，顺序固定，finish 最后。
- 声明了未实现的 `open_ai_responses` 时，把 identity 归一到 Chat Completions。
- `prompt_cache_state()` 按 catalog 回报。自动缓存端点不能回报成未启用。
- `interleaved_thinking` = 用户想开 × 端点有思考 × Anthropic 线。不要用 `thinking_budget > 0` 当判据。
- `max_output_tokens` 在命中 `known_model_capacity()` 时夹紧；未命中保持用户配置，不要猜。
