# wyj-code 使用国内大模型 vs Claude Code / Codex 深度对比分析

> **报告日期**:2026-08-23
> **数据时效**:2025-09 至 2026-08
> **维护责任**:wyj-code 维护组
> **本报告不包含实测 benchmark**,所有结论均来自公开网络资料、社区用户分享与 wyj-code 代码层事实复核

---

## 0. 摘要(TL;DR)

**5 条核心结论**:

1. **生态拐点已到**:2025-09 是国产编码模型的分水岭,GLM-4.6 / Kimi K2 / Qwen3-Max / DeepSeek V3.2 同月发布,SWE-bench Verified 集体突破 65%;Cursor/Cline/Roo Code 在同一天(2025-09-29)收录 GLM-4.6。
2. **价格断层悬殊**:DeepSeek V3.2-Exp 输出 $0.41/M,Claude Sonnet 4.5 输出 $15/M,**价差 35×**;GLM Coding Plan ¥20/月订阅进一步压缩计费复杂度。
3. **框架层 wyj-code 不输 Claude Code**:SubAgent、Hooks、CLAUDE.md 六层合并、Skills、/memory、/schedule、/workspace、/workflow 等核心抽象已对齐;主要差距集中在**协议层**(OpenAI Chat Completions 不支持 tool 内嵌图片、忽略 thinking 续轮、不支持原生 computer_20251124)。
4. **国产 vs Claude 模型层可消除 vs 不可消除**:可消除的有 thinking 显示化、tool schema 简化、parallel_tool_calls=false 默认;**不可消除**的有 OpenAI 协议下 tool 内嵌图片回传(影响 computer-use 截图)、R1/Reasoning 模型 thinking token 计费与稳定性。
5. **80/20 双剑合璧是社区共识**:复杂架构/跨文件疑难 → Claude 原版(Opus 4.5/4.6);日常 CRUD/单测/中文 commit/200K 中等文档 → 国产省钱;500K+ 长文档 → DeepSeek V4-Pro / 小米 MiMo。

### 三层对比表

| 维度 | Claude Code 原生 + Claude Sonnet/Opus | wyj-code + 国产模型(DeepSeek/GLM/Kimi/Qwen) | Codex + GPT/Codex |
|---|---|---|---|
| **协议层** | Anthropic Messages 原生(thinking、cache_control、native computer_20251124 全部支持) | 双 Provider:Anthropic 兼容路径(完整)+ OpenAI Chat Completions(降级) | OpenAI Responses / Chat Completions,reasoning_effort 参数体系 |
| **工具调用稳定性** | 极高(Anthropic 模型训练时即对齐 schema) | 国产 Bilingual 单工具路径已对齐 schema、简化字段;DeepSeek V3 ≈ GLM-4.6 ≈ Claude,但豆包/Qwen 偶发 schema 报错 | OpenAI 协议稳定性同 wyj-code OpenAI 路径 |
| **多轮 Agent 鲁棒性** | Claude 原生最强,几乎不死循环 | DeepSeek V3 ≈ GLM-4.6;R1/Qwen3-Max-Thinking 需手动关 thinking 或打补丁 prompt | reasoning_effort=high 可缓解但不根除 |
| **Extended Thinking** | 原生 beta(`interleaved-thinking-2025-05-14`)+ signature 回传 | Anthropic 路径同 Claude;OpenAI 路径**直接忽略** thinking_*(协议层硬限制) | Responses API 原生支持 reasoning |
| **Computer-use / Vision** | 原生 `computer_20251124` + 截图内嵌 tool_result | Anthropic 路径完整;OpenAI 路径注册即占位(name only),"名存实亡" | Responses API 支持 vision 但不走 tool 通道 |
| **长上下文** | 200K 实测 ~150K | 200K 实测 7-8 折;DeepSeek V4-Pro / 小米 MiMo 是 500K+ 主场 | GPT-5 上下文窗口与 Claude 接近 |
| **价格(输出 $/M)** | Sonnet 4.5 = $15,Opus 4.5 = $75 | DeepSeek V3.2-Exp $0.41,GLM-4.6 $1.74-2.20,豆包 lite ¥0.6 | GPT-5 区间,具体未公开 |
| **中文能力** | 优于 GPT,但不及国产母语级 | 国产 commit message / 中文 docstring / 中文 API 命名显著优于 Claude | 中文弱于 Claude |
| **成本控制工具** | Claude Code 自带 `/cost` | wyj-code `/cost` 单列子 Agent 用量;`trace_max_bytes_per_agent` 防 trace 爆盘;`cross_provider_fallback=false` 防误投 | Codex CLI 自带统计 |
| **Hooks / Skills / SubAgent** | 全套对齐 | 全套对齐 + 国产厂商踩坑防御(`prompt_cache=false` 强制等) | 较薄 |
| **最佳使用场景** | 复杂架构、跨文件疑难 Bug、computer-use | 日常 CRUD、单测、200K 中等文档、中文 commit、批量小任务 | 通用编码任务,reasoning-heavy |

---

## 1. 研究方法与资料来源

### 1.1 网络调研

- **官方文档**:DeepSeek / 智谱 GLM / 月之暗面 Kimi / 阿里 Qwen / 字节豆包 / 零一万物 Yi 各厂商 API 文档与公告
- **基准与排行**:SWE-bench Verified、LiveCodeBench、Aider Polyglot、BFCL(Berkeley Function Calling Leaderboard)、llm-stats
- **中文社区**:V2EX、知乎、即刻、CSDN、博客园、稀土掘金、量子位、凤凰科技、Trae IDE 实测帖
- **英文社区**:Composio、Medium、X(Twitter)、Reddit r/LocalLLaMA、Cursor 论坛、Cline Discussions、NomadTerrace、dev.to
- **Claude Code / Cursor 接入攻略**:juejin、Aliyun Developer、博客实战贴、cc-switch GitHub README

### 1.2 代码层调研(wyj-code)

读取文件(无修改):

- `crates/api/src/provider.rs` — Provider trait / RequestOptions / StreamEvent
- `crates/api/src/anthropic.rs` — Anthropic 协议完整实现
- `crates/api/src/openai.rs` — OpenAI Chat Completions 实现与能力降级点
- `crates/api/src/types.rs` — 中立 Session 模型(Message / ContentBlock / ToolDefinition / NativeToolSpec / StreamEvent)
- `crates/api/src/capabilities.rs` — ModelIdentity / ModelCapabilities / Capability 枚举
- `crates/api/src/capability_cache.rs` — 7 天 TTL 的能力 cache
- `crates/api/src/model_catalog.rs` — 国内 vendor 静态能力表 + base_url/model id 反推 vendor
- `crates/api/src/prompt_policy.rs` — `<model-compatibility>` 后缀生成
- `crates/api/src/request_plan.rs` — RequestPlan 把 capabilities 编译成下游可消费的 ReasoningRequest / ToolRequestPolicy / CachePolicy
- `crates/api/src/doctor.rs` — `/model doctor` 输出
- `crates/api/src/models.rs` — `PROFILE_TEMPLATES`(GLM / Volcengine / MiniMax / Kimi / DeepSeek / Qwen / Ollama / vLLM / Custom)
- `crates/api/src/retry.rs` — 流前重试与指数退避
- `crates/config/src/lib.rs` — Provider / WireProtocol / Profile / ModelRuntimeCfg / RoutingCfg / is_official_anthropic_endpoint / effective_*
- `crates/core/src/compact.rs` — `estimate_text_tokens`(CJK 1.5/字、其余 0.25/字、image ≈ b64×3/4/750,封顶 1600)+ `compact_session`
- `crates/core/src/agent.rs` — run_turn / run_turn_with_injection 在每轮按 route.capabilities 翻译 RequestOptions + 简化 schema
- `crates/core/src/prompts.rs` — main_system_prompt + `<env>` + git_status_snapshot + COMPUTER_USE_HINT
- `crates/tools/src/computer.rs` — `ComputerTool::new(max_dim, native)` + `definition()` 产出 NativeToolSpec
- `crates/tools/src/sub_agent.rs` — fake Provider impl(仅测试用)

### 1.3 时间窗与局限

- 数据时效:2025-09 至 2026-08
- 不做实测 benchmark:用户明确要求"基于网络调研和用户分享最佳实践",所有数字均为公开来源
- 模型价格与基准可能在数月内变动,使用前请核实厂商官网最新公告
- 部分国产厂商 extended thinking 实际效果未官方背书,见 §8

---

## 2. 模型格局与基准横向对比

### 2.1 主流厂商代表模型价位表(2025-Q4 至 2026-Q3)

| 厂商 | 模型 | 开源/闭源 | 上下文 | 输入 ($/M) | 输出 ($/M) | Function Calling | Extended Thinking | Vision | Prompt Caching | 备注 |
|---|---|---|---|---|---|---|---|---|---|---|
| DeepSeek | **V3.2-Exp** (2025-09) | 开源 (MIT) | 164K | 0.27 | 0.41 | ✅ | DSA 稀疏注意力 | ❌ | ✅ ($0.028/M hit) | 价格屠夫 |
| DeepSeek | V3.1-Terminus (2025-09) | 开源 | 163.8K | 0.27 | 0.95 | ✅ | ✅ | ❌ | ✅ | — |
| DeepSeek | R1-0528 (2025-05) | 开源 (MIT) | 128K–164K | 0.14 | 2.19 | ✅ | ✅ 强(显式 `<think>`) | ❌ | ✅ ($0.014/M hit) | 多轮工具循环需关 thinking |
| 智谱 | **GLM-4.6** (2025-09-30) | 开源 (MIT) | 200K(1M Beta) | 0.43–0.60 | 1.74–2.20 | ✅(parallel) | ✅ | ❌ | ✅ | 性价比第二档,GLM Coding Plan ¥20/月 |
| 智谱 | GLM-4.5 (2025-07) | 开源 (MIT) | 128K | 0.40–0.60 | 1.60–2.20 | ✅ | ✅ | ❌ | ✅ | — |
| 智谱 | GLM-4.5 Air | 开源 | 128K | 0.13 | 0.85 | ✅ | — | ❌ | — | 低价档 |
| 智谱 | GLM-4.7 (2025-12) | 开源 | 200K | — | — | ✅ | — | ❌ | — | 公开数据少 |
| 月之暗面 | **Kimi K2** (2025-07) | 开源(权重) | 128K | 0.15 / 0.60 | 2.50 | ✅ | 较弱 | ❌ | ✅ | 官方 Anthropic 兼容端点可用 |
| 月之暗面 | Kimi K2-0905 / K2.5 / K2.6 | 开源 + 闭源 | 256K–262K | 0.55 | 2.20 | ✅ | — | ❌ | ✅ | 长文档主场 |
| 阿里 | **Qwen3-Coder-Plus** (2025-10) | 闭源 | 262K | 1.00 | 5.00 | ✅ | ❌ | ❌ | ✅ | 百炼有 Anthropic 兼容端点 |
| 阿里 | Qwen3-Max-Thinking (2025-09) | 闭源 | 256K | 1.20 | 6.00 | ✅ | ✅ | ❌ | ✅ | SWE-bench Verified 73.4% |
| 字节 | **Doubao-1.5-Coder-Pro** | 闭源 | — | ~$0.42(¥3/M) | ~$0.83(¥6/M) | ✅ | 弱/无 | — | — | 火山引擎 Ark |
| 字节 | Doubao-1.5-Coder-lite | 闭源 | — | ~$0.04(¥0.30/M) | ~$0.08(¥0.60/M) | ✅ | — | — | — | 极致低价 |
| MiniMax | abab-6.5s / M2 | 闭源 | 200K | — | — | ✅ | — | — | — | wyj-code `minimax` profile 模板默认 |
| 零一万物 | Yi-Lightning (2024-10) | 闭源 | 16K | ¥0.99/M | 同价 | ✅ | — | — | — | 上下文偏短 |
| 百度 | 文心 4.5 / X1 Turbo | 闭源 | 128K | — | — | ✅ | ✅ X1 reasoning | — | — | 编码数据稀缺 |
| 小米 | MiMo-V2.5-Pro | 闭源 | 1M | — | — | ✅ | ✅ | — | — | 500K+ 主场 |
| (参考) | Claude Sonnet 4.5 | 闭源 | 200K | 3.00 | 15.00 | ✅ | ✅(原生) | ✅ | ✅ | — |
| (参考) | Claude Opus 4.5 | 闭源 | 200K | 15.00 | 75.00 | ✅ | ✅(原生) | ✅ | ✅ | — |

**Sources**:
- DeepSeek V3.2-Exp: <https://api-docs.deepseek.com/news/news250929> / <https://therouter.ai/models/deepseek--deepseek-v3.2-exp/>
- DeepSeek R1-0528 + prompt cache: <https://api-docs.deepseek.com/>(cache hit $0.014/M)
- GLM-4.5/4.6: <https://z.ai/blog/glm-4.6> / <https://lmspeed.net/model/glm-4-6> / <https://aibreaking.org/blog/glm-4-6-china-coding-champion>
- GLM Coding Plan ¥20/月: <https://tech.ifeng.com/c/8mJoak9q60M>
- Kimi K2: <https://composio.dev/blog/kimi-k2-vs-claude-4-sonnet-on-swe-bench> / <https://kimi-k2.com/blog/kimi-k2-api-pricing-and-benchmarks-2025>
- Qwen3-Coder Plus: <https://www.helicone.ai/blog/qwen3-coder-plus-pricing-capabilities> / <https://www.alibabacloud.com/en/product/modelstudio/qwen3-coder-plus>
- Qwen3-Max: <https://www.helicone.ai/blog/qwen3-max-pricing-context-window-benchmarks-api-providers> / <https://docs.together.ai/docs/qwen3-max-preview>
- Doubao 1.5 Coder: <https://www.volcengine.com/product/doubao>
- Yi-Lightning: <https://www.usagepricing.com/blueprint/activity/01-ai-2025-01-22-packaging>

### 2.2 重要观察

- **价格断层**:DeepSeek 仍是绝对的价格屠夫(V3.2-Exp 输出 $0.41/M,比 GLM-4.6 便宜 4×,比 Claude Sonnet 4.5 便宜 35×);GLM-4.6、Qwen3-Coder-Plus、Kimi K2 处于第二梯队;字节豆包用人民币单价最低。
- **GLM Coding Plan 订阅**:¥20/月起(约 $2.8),"Claude 1/7 价格、3 倍额度",是绕开按量付费的杀手锏。
- **专用 Coder 变体**:除阿里(Qwen3-Coder-Flash/Next/Plus)和字节(Doubao-1.5-Coder)外,多数厂商在通用模型上做 coding post-training;DeepSeek-Coder/V3-Coder 已整合进 V3.x。
- **闭源 4 家**:阿里 Qwen3-Coder-Plus/Max + 字节 Doubao + 零一万物 Yi-Lightning + MiniMax abab。
- **Vision 支持稀缺**:仅 Yi-Vision-v2 等少数国产模型支持多模态;DeepSeek V3.2-Exp、GLM-4.6、Kimi K2、Qwen3-Coder 全部纯文本——对截图、UI 理解类任务仍有短板。
- **Extended Thinking**:DeepSeek R1-0528 强(显式 `<think>` 块)、Qwen3-Max-Thinking、智谱 GLM-4.6 走 Anthropic 兼容 `thinking` 字段;豆包/通义 Qwen3-Coder **不支持**思考模式。
- **Prompt Caching**:DeepSeek(hit $0.014/M)、GLM(通过 `cache_control` 断点支持)、Kimi(月之暗面 API 启用)、Qwen3-Coder(百炼 Anthropic 兼容层开启 `prompt-caching-2024-07-31` beta)均支持,但**实际命中率、缓存键规则、TTL 各自不同**。

### 2.3 基准成绩矩阵

| 基准 | GLM-4.6 | Qwen3-Max-Thinking | DeepSeek V3.2-Exp | Kimi K2 | Claude Sonnet 4 | Claude Sonnet 4.5 |
|---|---|---|---|---|---|---|
| **SWE-bench Verified** | 68.0% | 73.4% | 67.8–73.1%(视配置) | 51.8–65.8% | 67.8% | 77.2% |
| **LiveCodeBench v6** | 82.8%(无工具)/ 84.5%(带工具) | Top 5 | 74.1 | — | — | — |
| **HumanEval Pass@1** | ~90% | 96.6%(自报) | 92.0% | — | — | — |
| **BFCL v4** | GLM-4.5 **#1**(超 Opus 4.1) | — | — | — | Opus 4.1 #2 | — |
| **Codeforces rating** | — | — | 2386 | — | — | — |
| **TerminalBench** | — | — | 37.7 | — | — | — |
| **CC-Bench(智谱自建 74 任务)** | vs Sonnet 4 胜率 48.6% | — | — | 53.9% wins vs Sonnet 4 | — | — |

**Sources**:
- SWE-bench Verified: <https://www.swebench.com/> / <https://z.ai/blog/glm-4.6> / <https://docs.together.ai/docs/qwen3-max-preview>
- LiveCodeBench: <https://livecodebench.github.io>
- BFCL: <https://gorilla.cs.berkeley.edu/leaderboard.html>
- HumanEval: <https://www.volcengine.com/product/doubao>
- CC-Bench: <https://lmspeed.net/model/glm-4-6>

**关键提醒**:基准测的是**基座模型**或**模型 + 配套 scaffolding**的得分,不能直接等同于"在 Claude Code / Cursor / Cline 中实战的体验"。真实 IDE 表现还会受 prompt caching、tool schema 设计、上下文管理、UI 反馈影响。SWE-bench Verified 分数会因 scaffolding(多轮 vs 单轮、是否用 SWE-agent 框架、是否用 test-time scaling)浮动 5-10 个百分点。

### 2.4 IDE / 编辑器生态支持现状

| 工具 | DeepSeek | Kimi | GLM | Qwen | 豆包 |
|---|---|---|---|---|---|
| **Cursor** | ✅ 自定义 | ✅ 自定义 | ✅ 官方(2025-09-30) | ✅ 自定义 | — |
| **Cline** | ✅ 原生 + OpenAI 兼容 | ✅ OpenRouter | ✅ OpenAI 兼容 | ✅ OpenAI 兼容 | — |
| **Roo Code** | ✅ | ✅ | ✅ 官方(2025-09-29) | ✅ | — |
| **Continue.dev** | ✅ provider 配置 | ✅ openai-compatible | ✅ openai-compatible | ✅ openai-compatible | — |
| **Trae**(字节) | ✅ | ✅ | ✅ | ✅ | ✅ 内置 |
| **CodeGeeX**(智谱) | — | — | ✅ 内置 | — | — |
| **Qoder**(阿里) | — | — | — | ✅ 内置 | — |
| **Claude Code** | cc-switch/one-api 桥接 | ✅ 官方 Anthropic 兼容端点 | ✅ 官方 Anthropic 兼容端点 | ✅ 百炼 Anthropic 兼容端点 | — |
| **wyj-code**(本项目) | ✅ PROFILE_TEMPLATES | ✅ PROFILE_TEMPLATES | ✅ PROFILE_TEMPLATES | ✅ PROFILE_TEMPLATES | ✅ PROFILE_TEMPLATES |

**Sources**:<https://x.com/i/status/1972181378933342667> / <https://forum.cursor.com/t/new-model-z-ais-glm-4-6-is-now-in-public-preview-on-cursor/76135> / <https://github.com/cline/cline/discussions/4625>

---

## 3. wyj-code 模型适配架构现状(代码层事实)

### 3.1 Provider 抽象(`crates/api/src/provider.rs`)

wyj-code 用 `Provider` enum 二分派发(Anthropic | OpenAI),trait 单方法 `stream(...)`:

- `Provider::stream(...)`(`provider.rs:46`)是协议中立的核心
- `RequestOptions { max_tokens, thinking_budget, interleaved }`(`provider.rs:13-19`)把 thinking 相关参数集中收口
- `EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>`(`provider.rs:9`)
- 中立 Session 模型在 `crates/api/src/types.rs`:`Message { Role, content: Vec<ContentBlock> }`;`ContentBlock::Text/ToolUse/ToolResult/Image/Thinking/RedactedThinking`;`ToolResultContent` 三种形态 `Text(String) | Parts(Vec<ToolResultPart>) | Blocks(Vec<Value>)`(`types.rs:81`);`ToolDefinition { name, description, input_schema, native }`(`types.rs:125`),`NativeToolSpec { tool_type, extra, beta }`(`types.rs:147`)

### 3.2 能力矩阵:Anthropic 路径 vs OpenAI 路径对等实现

| 能力 | Anthropic 路径 | OpenAI Chat Completions 路径 | 国内踩坑/已知问题 |
|---|---|---|---|
| SSE 流式输出 | ✅ 完整事件;usage 可来自 `message_start` 或 `message_delta`(`anthropic.rs:601-614` 注释:兼容 MiniMax 等只在后者返回真实用量) | ✅ eventsource + scan 维护 `PendingToolCall`;每 chunk 可同时带 finish_reason + usage(`openai.rs:317-321` + 单测 `finish_chunk_keeps_usage_for_exact_token_accounting`) | OpenAI 兼容代理偶发不发 `[DONE]`,需容忍 EOF |
| Tool calling(function_call) | ✅ `ContentBlockStart::ToolUse` → `InputJsonDelta` 流式拼参数 | ✅ `tool_calls` 数组 delta,按 `index` 入 HashMap | 豆包 Ark 模型名常是 `ep-xxxxxxxx-xxxxx` 而非固定名 |
| Tool result 回传结构 | ✅ ToolResult 嵌入 user content 数组;`Parts` 转 Anthropic 原生 image block | ⚠️ 走独立 `role: "tool"` 消息 + `tool_call_id`;`Parts` 强制 `display_text()` 占位(`openai.rs:171-179`) | **OpenAI `role: tool` 不支持图片块**:computer-use 截屏回传在 OpenAI 路径只能拿到占位 |
| Tool schema 严格校验 | ✅ 标准 JSON schema | ⚠️ `strict_tool_schema` 用 `ModelCapabilities` 协商;Bilingual + `RequiresSingleTool` 的 Profile 通过 `core::tool_arguments::simplified_tool_definition` 简化 schema(`agent.rs:826-834`) | 防国内代理对 `$schema / additionalProperties:false / anyOf` 报错 |
| Parallel tool calls | ✅ 默认允许 | ❌ 国内 OpenAI 代理常单 tool/turn(`model_catalog.rs:119` + quirks),`PromptPolicy` 给 Bilingual + single_tool 注入双语提示 | `force_single_tool` 路径由 capabilities 决定 |
| Prompt caching(cache_control) | ✅ 三处 ephemeral:system / 最后一个 tool / 历史末尾可承载块(`anthropic.rs:457-474`);拼 beta `prompt-caching-2024-07-31` | ❌ 不支持 cache_control 显式断点;仅解析 `prompt_tokens_details.cached_tokens` 作为只读信号(`openai.rs:143-147` + `252-268`) | GLM/Kimi 走 Anthropic 兼容路径时必须 `prompt_cache = false`,否则 beta header 触发 400 |
| Stream usage / token 账本 | ✅ `usage_event` 接收 input/output/cache_read/cache_creation | ✅ `stream_options.include_usage = effective_openai_stream_options_for_model(model)`,默认对 OpenAI / MiniMax / GLM / DeepSeek true,对 Ollama/vLLM/proxy false | 模型目录把 `minimax/glm/deepseek/bigmodel.cn/z.ai/deepseek.com` 标记为"需要 supplier-returned usage"(`lib.rs:255-265`) |
| Extended thinking | ✅ `ThinkingParam { type: enabled, budget_tokens }`;interleaved 单独走 `interleaved-thinking-2025-05-14` beta;Thinking + RedactedThinking 块 + signature 回传保留(`anthropic.rs:483-486` + `agent.rs:3006` 测试) | ❌ 直接 `// OpenAI 格式不支持 Anthropic 式 thinking 参数,忽略 opts.thinking_*`(`openai.rs:336`) | 用户配 `thinking_budget` 但 provider=OpenAI 会被 silently 丢弃;`RequestPlan::from_capabilities` 把 reasoning 设为 `Disabled` 并把 `thinking_budget` 加进 `dropped_parameters` |
| Vision(user message 内嵌图片) | ✅ `ContentBlock::Image` → native image source;vision=false 降级占位 | ⚠️ Chat Completions `content` 当前实现只走 String 路径(`openai.rs:194-199`),image block 在 `to_api_messages` 里被丢弃——`Profile.vision=true` 对 OpenAI 路径需要走 `content: [{type:"image_url", ...}]` 数组,**当前实现未完整对应** | `Profile.vision=false` 是必须的"防 400 兜底" |
| Vision(tool_result 内嵌图片) | ✅ `Parts` 转 native image(`anthropic.rs:299-326`) | ❌ 强制降级占位(`openai.rs:171-179`) | Computer-use 截图回传因此在 OpenAI 路径**只能拿到占位文本** |
| Computer-use / Native tool | ✅ `ToolDefinition.native` 分支走 `{type, name, ...extra}`,`native.beta` 拼进 anthropic-beta | ❌ `filter(|t| t.native.is_none())` 主动过滤(`openai.rs:348-359`) | `register_computer_tool_if_enabled` 在 `provider != Anthropic` 时**整体不注册** |
| Stop reason 归一化 | ✅ `end_turn / tool_use / max_tokens / stop_sequence` → `StopReason` | ✅ `stop / tool_calls / length` 映射(`openai.rs:241-248`) | Anthropic 第三方端点偶发 `stop_sequence`,agent 当作 EndTurn 继续轮换 |
| 重试(连接前阶段) | ✅ `crate::retry::send_with_retry`,覆盖 408/409/429/5xx 指数退避 + Retry-After | ✅ 同上 | 国内 API 常 529(overloaded)+ 不带 Retry-After,backoff 1s→32s 抖动 ±20% 兜底 |

### 3.3 国内厂商踩坑的代码层防御

- **`is_official_anthropic_endpoint`**(`crates/config/src/lib.rs:238-242`)= `provider == Anthropic && (base_url 空 || 等于 api.anthropic.com)`,与 `effective_prompt_cache` 和 `register_computer_tool_if_enabled` 共用同一判定。**教训**:早期把"是否注册"和"native == Anthropic"两件事合并,导致 MiniMax 这类走 `provider = "anthropic"` + 自定义 `base_url` 接入的第三方端点,要么被误判成官方端点收到无 schema 的原生工具直接 400,要么被整体拒绝注册;现在拆成"注册与否看协议、原生与否看端点"两层判断(详见 `doc/plan/v1.3.0-plan.md`)。

- **`effective_prompt_cache`**(`lib.rs:244-247`)= `prompt_cache.unwrap_or(is_official_anthropic_endpoint)`,**只有官方 Anthropic 才默认 true**,GLM/Kimi 走 Anthropic 兼容协议必须显式设 `prompt_cache = false`,否则 beta header 触发 400。Profile 模板里 glm/kimi 已默认 `Some(false)`(`models.rs:41, 80`),custom 默认 `None`。

- **`uses_provider_exact_token_usage_for_model`**(`lib.rs:255-265`)通过子串匹配 `minimax / glm / deepseek` 和 hostname `minimaxi.com / bigmodel.cn / z.ai / deepseek.com` 命中"必须用供应商返回 usage"的供应商,绕过启发式估算。`effective_openai_stream_options_for_model(model)`(`lib.rs:267-274`)在 `provider=OpenAI` 或上述供应商时默认 true,但 Ollama/vLLM 显式设为 false。

- **`ModelCapabilities` + `PromptPolicy::compatibility_suffix`**(`crates/api/src/prompt_policy.rs:8-26`)4 种组合产生最多 1 段 `<model-compatibility>` 后缀:
  - `Bilingual + 单工具`:双语,提示"每次回复最多调用一个工具,参数必须是仅含 schema 字段的严格 JSON 对象…"
  - `Bilingual + 可并发`:双语,宽松版
  - `Concise + 单工具`:纯英文
  - 其他:空串

  调用点在 `agent.rs:801-805`,仅当 `route.capabilities` 命中才追加,避免污染 reference 供应商。

- **`model_catalog.rs` vendor 表**(`crates/api/src/model_catalog.rs:118-133`):`vendor = "zhipu|minimax|moonshot|deepseek|alibaba|volcengine"` 一律设 `parallel_tool_calls = false`、`max_tools_per_turn = 1`、`preferred_prompt_dialect = Bilingual`、quirks 里塞 `RequiresSingleTool`。`BaseCapabilities` 用 `is_reference = (vendor in {anthropic, openai})` 同时驱动 strict schema、tool_choice、parallel_tool_calls 这些 reference 供应商才有保证的能力,避免对国内代理做错误假设。

### 3.4 Profile 模板(`crates/api/src/models.rs::PROFILE_TEMPLATES`)

| Key | Label | Provider | base_url | example_model | prompt_cache | openai_stream_options |
|---|---|---|---|---|---|---|
| `glm` | GLM 智谱 | Anthropic | `https://open.bigmodel.cn/api/anthropic` | `glm-5.2` | Some(false) | None |
| `volcengine` | 火山引擎 Ark | OpenAI | `https://ark.cn-beijing.volces.com/api/v3` | `doubao-seed-1-6` | None | Some(false) |
| `minimax` | MiniMax Coding Plan | OpenAI | `https://api.minimaxi.com/v1` | `MiniMax-M2` | None | Some(true) |
| `kimi` | Moonshot | Anthropic | `https://api.moonshot.cn/anthropic` | `kimi-k2-turbo-preview` | Some(false) | None |
| `deepseek` | DeepSeek | OpenAI | `https://api.deepseek.com` | `deepseek-chat` | None | Some(true) |
| `qwen-bailian` | 通义千问 | OpenAI | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `qwen3-coder-plus` | None | Some(true) |
| `ollama` | Ollama | OpenAI | `http://127.0.0.1:11434/v1` | `qwen3-coder` | None | Some(false) |
| `vllm` | vLLM | OpenAI | `http://127.0.0.1:8000/v1` | (空) | None | Some(false) |
| `custom` | 自定义 | Anthropic | (空) | (空) | None | None |

注意模型厂商字符串是 `minimax / zhipu / moonshot|kimi / deepseek / alibaba|qwen / volcengine|doubao`,normalize 规则在 `model_catalog.rs::normalize_vendor`(`models.rs` 注释同时给出 `GLM(智谱/Z.ai Coding Plan)` 等说明)。`ModelCatalog::resolve` 把 base_url 或 model id 含 `minimax/bigmodel/z.ai/glm/moonshot/kimi/deepseek/dashscope/aliyun/qwen/volces/doubao` 的字符串映射成 vendor(`model_catalog.rs:214-249`),即使 profile 没显式写 vendor 也能识别。

### 3.5 易扩展之处与痛点

**易扩展**:
- Profile 是 serde-tagged,加新字段不会破坏旧 config.toml
- ProfileTemplate 是 const 数组,新增一个国内厂商只要再加一行
- vendor 字符串开放,`model_catalog.rs::infer_vendor` + `normalize_vendor` 已留好 keyword 列表
- v1.4 后的分层:`ModelRuntimeCfg`(`lib.rs:380-402`)管 probe 模式 / TTL / tool_argument_retries / lazy_tools_threshold / top_k / sticky_turns;`RoutingCfg` + `RoutingRoles`(`lib.rs:303-336`)做同角色 profile 池 + 可恢复错误 fallback;`cross_provider_fallback` 默认 `false`(`lib.rs:298-302`)避免用户未授权时把对话发给别的厂商

**痛点**:
- 新协议本质要碰 `wyj-api` 里三个函数 + `ProfileTemplate` + `model_catalog::infer_vendor/normalize_vendor` + `capabilities::ModelCapabilities` 的能力字段,**不是"热插拔"**
- Profile `wire_protocol` 字段已枚举 `OpenAiResponses | QwenNative | Gemini`(`lib.rs:110-116`),但 `crates/api` 的 `build_provider` 只读 `cfg.provider()` 二分选择,**新协议需改三处 match**
- `max_tools_per_turn = 1` + `RequiresSingleTool` quirk 是国内 vendor 的保守默认(`model_catalog.rs:119, 128-131`),需要并发调用时建议改 Profile + 清 capability cache

---

## 4. 维度深度分析

### 4.1 工具调用稳定性 + 多轮 Agent 鲁棒性

#### wyj-code 侧

- Tool 调用按 `parallel_safe()` 决定是否并发:agent 推理循环(`core::agent::Agent::run_turn`)→ 流式接收 LLM 输出 → 累积工具调用 → 执行(`Tool::parallel_safe()` 为 true 的调用如 Agent 用 `join_all` 单任务内并发,其余相互保持顺序但与并发组同时进行,结果按原始下标保序回填)
- OpenAI 路径 `Provider::PendingToolCall` HashMap 按 index 维护(`openai.rs:389-417`),start + arguments 都支持,单测 `one_chunk_can_emit_tool_start_and_arguments` 钉死
- `force_single_tool` 在 Bilingual 路径上开启:`agent.rs:798-846` 的 round 入口按 `route.capabilities` 翻译 `RequestOptions`,配合 `request_plan.rs` 的 `ToolRequestPolicy`
- `simplified_tool_definition`(`core/src/tool_arguments.rs`)简化 schema 防国内代理对 `$schema/additionalProperties:false/anyOf/format:"uri"` 等敏感字段的解析失败
- `PromptPolicy::compatibility_suffix` 注入双语"每次最多调一个工具,参数必须是仅含 schema 字段的严格 JSON 对象"提示
- `usage_event` 已在 `anthropic.rs:601-614` 双源提取(`MessageStart.usage` 和 `MessageDelta.usage`),**为了兼容 MiniMax、GLM 等只在 message_delta 返回真实用量的端点**

#### 国内 vs Claude 真实差距

- **DeepSeek V3** 多轮最稳(10/10 不死循环),适合自动化批量任务 — <https://www.toutiao.com/article/7611687521872904713>
- **GLM-4.6** 工具选择精准,参数填充极少出错,几乎无幻觉调用
- **豆包 / 千问** 中文语义理解有加分(金融/电商类)
- **千问 / DeepSeek** 参数格式规范(OpenAI 兼容 schema 严格)
- **多工具编排** 排序:DeepSeek > Kimi > 千问 > 豆包 ≈ GLM — <https://blog.csdn.net/2511_94663557/article/details/161365671>
- **R1 类推理模型** Cline 明确警告 R1 默认开启 thinking,**多轮工具调用可能 400** — <https://www.cnblogs.com/ljbguanli/p/19535678>

#### 社区共识踩坑清单

1. R1 / Qwen3-Max-Thinking 类模型**别直接当 Claude 替身**,需要 prompt 注入补丁("严格按 tool_use 格式输出 JSON")
2. DeepSeek V3 / GLM-4.6 是目前**对 Claude Code / Cline / Roo Code 协议兼容性最好**的国产模型
3. 智谱 BigModel 的 Anthropic 兼容端点已透传 `cache_control` 和 `tool_use`,但 `thinking` 字段需明确开启且效果不一定等同 Claude 原生
4. Base URL 末尾 `/v1` 加不加(各工具不一致)
5. Token 计算方式与 Anthropic 不同
6. 工具 schema 冗长时国产模型易丢参数
7. 配置文件改完**必须重启终端**才生效

#### 多轮 Agent 稳定性故障模式

| 故障模式 | 触发原因 | 缓解措施 |
|---|---|---|
| **死循环**(同工具反复调) | R1 思考过度、错误信息未吸收、缺少熔断 | Agent 框架加 max_iterations + loop detection |
| **幻觉工具结果**(编造"文件已写入") | 模型认为"应该成功" | 强制 tool result 必回灌;不要相信模型自报结果 |
| **越权**(擅自 git push / rm -rf) | Prompt 注入 / 不清权限边界 | 显式 confirm_tool + 沙箱(Seatbelt / bubblewrap) |
| **格式错**(tool_use JSON 非法) | 国产模型 schema 理解弱 | 严格用 `tool_choice: "auto"`;不要 forced tool use |

来源:<https://www.cnblogs.com/ljbguanli/p/19535678> / <https://tonybai.com/2025/07/30/six-principles-production-ai-agents/> / <https://www.secrss.com/articles/89756>

Agent 工程实践(DeepSeek V4 报告):
- 工具层:try-catch 硬隔离、结构化错误返回
- 推理层:熔断 / max-iteration / loop detection
- 规划层:self-correction / goal anchoring
- 多 agent:watchdog 观察者模式 / 有限状态机 / token bucket 预算

DeepSeek V4-Flash 实测(CowAgent 框架):6 个场景全一次跑通,**零死循环**,35-50 次工具调用稳定执行 — <https://www.cnblogs.com/zhayujie/p/19935607/deepseek-v4-eval>

### 4.2 长上下文 + RAG + 跨文件能力

#### wyj-code 侧

- `core::compact::estimate_text_tokens`(`compact.rs:104-115`)全 provider 共用同一启发式:`CJK 1.5 token/字 + 其他字符 0.25 token/字`(≈4 字符/token),`estimate_image_tokens = min(b64_len*3/4/750, 1600)`(`compact.rs:99-101`)
- `compact_session`(`compact.rs:135-235`)触发缓冲为 `min(40000, max(4000, context_window / 5))`,调 `provider.complete(... RequestOptions::text_only(...))` — 把 thinking 关掉避免压缩摘要被一次大额 budget 占用;summary 上限 `min(ctx/10, 16000)`
- **完全 provider-neutral** — 所有 provider 共用同一估算式;真实账本依靠 usage 事件覆盖
- wyj-code 没有专门的 long-context 优化,统一靠 compact;`Profile.context_window` 字段是触发阈值输入
- CLAUDE.md 提到的 `context_window` 是 Profile 字段,实际 compact 阈值与 `ctx` 一致

#### 国产长上下文实测缩水表

| 模型 | 标称 | 实测可缩 | 备注 |
|---|---|---|---|
| Claude Opus 4.5 | 200K | ~150K | "原生长程能力"已训练 |
| GLM-4.6 / 4.7 | 200K(1M Beta) | ~120-150K | 跨窗口复杂任务**偶发健忘** |
| Kimi K2-0905 | 256K | ~180K | 长文档分析主场 |
| DeepSeek V3.2-Exp | 164K | ~120K | DSA 稀疏注意力优化 |
| DeepSeek V4-Pro | 1M | ~600-700K | "上下文太长会炸",需拆会话 |
| Qwen3-Coder-Flash | 256K | ~150-200K | 200K 内文档 Claude / 国产都行 |
| 小米 MiMo-V2.5-Pro | 1M | 同 DeepSeek V4 | 与 DeepSeek 并列超长主场 |

来源:<https://www.53ai.com/news/OpenSourceLLM/2025122367250.html> / <https://blog.csdn.net/weixin_32487557/article/details/162160945> / <https://watermelonwater.tech/insights/月花200测claude国产模型差27>

#### 关键发现

- **200K 是国产模型接入 Claude Code 的"及格线"**,实测需要打 7-8 折
- **超长上下文(500K+)恰恰是国产模型相对 Claude 的真正优势**,但要做好"炸"的预案——长任务拆短会话、阶段性重置上下文
- 跨多个上下文窗口的复杂 Agent 任务,**仍首选 Claude 原版**(Opus 4.5/4.6)
- **对 wyj-code 的影响**:跨文件复杂 Agent 任务仍首选 Claude 原生,国产主要赢在"中等长度(50K-150K)+ 大量调用"

### 4.3 Extended Thinking / Reasoning 模型适配

#### wyj-code 侧

**Anthropic 路径完整支持**:
- `ThinkingParam { type: enabled, budget_tokens }`(`anthropic.rs:415-425 + 476-487`);自动抬高 max_tokens 到 budget+4096
- interleaved 单独走 `interleaved-thinking-2025-05-14` beta(`anthropic.rs:493-497`)
- `Thinking/RedactedThinking` 块 + signature 必须原样保留以支撑续轮(`anthropic.rs:351-360` + `agent.rs:3006` 测试)

**OpenAI 路径协议层硬缺口**:
- `opts.thinking_*` 在 `openai.rs:336` 直接被忽略,注释"OpenAI 格式不支持 Anthropic 式 thinking 参数"
- `RequestPlan::from_capabilities` 会把 reasoning 设为 `Disabled` 并把 `thinking_budget` 加进 `dropped_parameters`
- TUI 未来可以可视化这个丢弃(测试 `unsupported_reasoning_is_dropped_visibly`)

**这是国产 R1 / Qwen3-Max-Thinking 的最大协议层缺口**。

#### 国产实现差异

| 模型 | Extended Thinking | 备注 |
|---|---|---|
| **DeepSeek R1-0528** | ✅ 强,显式 `<think>` 块,**计费** | 32K-64K token reasoning;多轮工具循环中需关闭 |
| **GLM-4.6** | ✅ Anthropic 兼容 `thinking` 字段,走 `ENABLE_THINKING=true` | 实际效果未官方背书 |
| **Qwen3-Max-Thinking** | ✅ | 73.4% SWE-bench Verified |
| **Kimi K2** | 较弱 | 走 R1 类推理但思考深度不及 DeepSeek |
| **Qwen3-Coder 系列** | ❌ 不支持 | 官方明确 |
| **豆包 Doubao** | 弱/无 | — |

#### 实战最佳实践

- **主对话用 R1 / Qwen3-Max-Thinking 拆解复杂问题 → 子任务用 V3 / GLM-4.6 跑工具调用**
- 单一模型跑全程时,**关闭 thinking** 比开启稳(避免 token 浪费 + 死循环风险)

#### 对 wyj-code 的建议

- 在 OpenAI 路径上**至少识别** R1/Reasoning 模型的 thinking 输出(DeepSeek 已支持 `reasoning_content` 字段),把它当 `ContentBlock::Thinking` 显式存储
- 即使不回传给模型也用于上下文展示,让用户能看到 reasoning 过程
- 详见 §7 改进建议

### 4.4 成本 / 速度 / 中文能力

#### 价格梯队(以每 M token **输出**计)

| 档位 | 模型 | 输入 ($/M) | 输出 ($/M) | 相对 Claude Sonnet 4.5 |
|---|---|---|---|---|
| **超低价** | DeepSeek V3.2-Exp | 0.27 | 0.41 | 1/35 |
| **超低价** | DeepSeek V3.1-Terminus | 0.27 | 0.95 | 1/16 |
| **低价** | GLM-4.5 Air | 0.13 | 0.85 | 1/18 |
| **低价** | Doubao-1.5-Coder-lite | ~0.04(¥0.30/M) | ~0.08(¥0.60/M) | 1/30 |
| **中价** | GLM-4.6 | 0.43–0.60 | 1.74–2.20 | 1/7 |
| **中价** | Qwen3-Coder-Plus | 1.00 | 5.00 | 1/3 |
| **中价** | Qwen3-Max-Preview | 1.20 | 6.00 | 1/2.5 |
| **高价** | Claude Sonnet 4.5 | 3.00 | 15.00 | 1× |
| **极高** | Claude Opus 4.5 | 15.00 | 75.00 | 5× |

#### 速度(社区观察)

- **最快**:GLM-4.5 Air、DeepSeek V3.2-Exp(开启 cache 后几乎秒回)
- **中等**:Qwen3-Coder-Flash、GLM-4.6、Kimi K2
- **最慢**:DeepSeek R1(reasoning token 阻塞首 token)、Qwen3-Max-Thinking

#### 中文能力

- **国产模型在中文任务上普遍优于 Claude**(中文是母语级数据),尤其在注释、文档、commit message、代码风格遵循("按阿里巴巴 Java 开发手册"等)上
- **英文代码质量**:Claude / GPT 在英文代码注释、英文 docstring 上仍更地道
- **GLM-4.5 中文 prompt 中"thinking in Chinese"**是社区推荐做法,比英文 prompt 效果更稳
- **混排输入**(如中文变量名 + 英文 API 文档 + 中文错误信息):DeepSeek V3 / 通义 Qwen3 表现最好

#### wyj-code 的成本控制

- `/cost` 单列子 Agent 用量
- `trace_max_bytes_per_agent`(默认 256KB)防 trace 文件爆盘
- `cross_provider_fallback` 默认 `false` 防"用户未授权时把对话发给别的厂商"
- 订阅化趋势:GLM Coding Plan ¥20/月、Kimi 套餐、Qwen3-Coder Plus 是绕开按量付费的杀手锏

#### Prompt Cache 实际命中率

| 模型 | Cache Hit 价 | 自动缓存 | 缓存键 | TTL |
|---|---|---|---|---|
| **DeepSeek** | $0.014/M(-90%) | ✅ 自动 | 前缀匹配 | 数小时 |
| **GLM** | -50% 至 -90% | ✅ | `cache_control` 断点 | 5-10 分钟 |
| **Kimi** | -75% | ✅ | 前缀匹配 | 数小时 |
| **Qwen3(百炼)** | -90% | ✅ | `cache_control` 断点 | 5-10 分钟 |
| **豆包** | 不公开 | — | — | — |

**关键技巧**(来自 DeepSeek 官方 <https://api-docs.deepseek.com/>):
1. 把稳定 system prompt + 工具定义 + few-shot 放在前缀
2. 会话历史尽量不要修改前缀内容(避免缓存失效)
3. 多轮对话中插新内容时,用 **incremental prefix** 而非 in-place 编辑
4. 实测缓存命中率可达 80%+,前提是 prompt 结构稳定

---

## 5. wyj-code vs Claude Code vs Codex 体验差距

### 5.1 协议层差距

| 维度 | Claude Code | wyj-code | Codex |
|---|---|---|---|
| 主协议 | Anthropic Messages 原生 | 双 Provider:Anthropic Messages + OpenAI Chat Completions | OpenAI Responses / Chat Completions |
| Extended Thinking | 原生 beta(`interleaved-thinking-2025-05-14`)+ signature 回传 | Anthropic 路径同 Claude;OpenAI 路径**直接忽略** thinking_*(协议层硬限制) | Responses API 原生支持 reasoning_effort |
| Prompt Cache | cache_control 写断点 + `prompt-caching-2024-07-31` beta | Anthropic 路径写断点(只对官方端点默认 true);OpenAI 路径只读 `cached_tokens` | Responses API 支持 `prompt_cache_key` |
| Native Computer-use | `computer_20251124` + `anthropic-beta: computer-use-2025-11-24` | Anthropic 路径原生(对官方端点);OpenAI 路径整体不注册 | Responses API 支持 computer-use 工具 |
| Vision 内嵌 user message | ✅ | Anthropic ✅;OpenAI ⚠️(当前实现 image block 丢失) | ✅ Responses API 支持 `input_image` |
| Vision 内嵌 tool_result | ✅ | Anthropic ✅;OpenAI ❌(`role: tool` 不支持图片) | ⚠️ Responses API 部分支持 |
| Stop Reason | end_turn / tool_use / max_tokens / stop_sequence | 同 Claude + OpenAI 双映射 | length / tool_calls |
| 重试 | 流前重试 + 流中断重试 | 同 | 同 |

### 5.2 框架层差距

**wyj-code 与 Claude Code 对齐的部分**:
- SubAgent 系统(`tools::agent_hub`):内置 general-purpose / Explore(只读)/ Plan,与 Claude Code 一致
- 并行 8、后台任务、Trace 落盘(`tools::trace::TraceWriter` 写到 `~/.wyj-code/sessions/<session_id>.subagents/a<id>.jsonl`)
- Hooks 三源合并:`~/.claude/settings.json` → `<git-root>/.claude/settings.json` → `<git-root>/.claude/settings.local.json`
- CLAUDE.md 注入机制六层合并链:`内置 → 全局 ~/.wyj-code/skills → 全局真 CC ~/.claude/commands → 已启用插件贡献路径 → 项目 <git-root>/.wyj-code/skills → 项目真 CC .claude/commands`
- 斜杠命令覆盖:/help /compact /config /model /init /memory /skills /agents /schedule /workspace /workflow /mcp /cost /subagents
- Computer-use "礼让机制"(`tools::app_computer` + `computer::activity::InputArbiter`)独家实现
- 定时任务系统(v1.4):schedule.rs + cron_sync.rs + ScheduleDialog
- Managed Worktree + Workflow(v1.5.0)
- ACP / daemon 全局 Session Registry(v1.5.0)
- EvolutionStore 证据驱动进化(v1.5.5)

**wyj-code 与 Codex 的主要差异**:
- Codex 主要在 OpenAI Responses API 上做了很多 reasoning + tool 联动优化
- Claude Code 在 thinking + tool 交错上有官方 beta
- Codex CLI 缺乏 Claude Code 的 Skill/SubAgent/Hooks/CLAUDE.md 体系
- Codex 缺乏 computer-use 的礼让机制

### 5.3 模型能力层:可消除 vs 不可消除

**可消除(通过 wyj-code 自有策略)**:
- ✅ Thinking 显示化:在 OpenAI 路径解析 `reasoning_content` 字段(DeepSeek 已支持)并转成 `ContentBlock::Thinking`
- ✅ Tool schema 简化:`simplified_tool_definition` 已实现,防国内代理对敏感字段报错
- ✅ parallel_tool_calls=false 默认:`model_catalog.rs` 对国内 vendor 一律设 false
- ✅ Image part 降级提示:`openai.rs:171-179` 走 `display_text()` 占位
- ✅ 双语 system prompt 后缀:`PromptPolicy::compatibility_suffix` 给 Bilingual 路径注入
- ✅ `prompt_cache=false` 自动防御:`is_official_anthropic_endpoint` 对第三方端点返回 false
- ✅ `openai_stream_options=true` 对国内 vendor 自动开启

**不可消除(协议层硬限制)**:
- ❌ OpenAI Chat Completions 不支持 tool 内嵌图片回传(`role: tool` 不支持 image)
- ❌ OpenAI Chat Completions 不支持原生 `computer_20251124`(影响 vision+computer 链)
- ❌ OpenAI Chat Completions 不支持 cache_control 写断点
- ❌ OpenAI Chat Completions 不支持 Extended Thinking 的 thinking_budget 与 signature 回传
- ❌ Chat Completions 协议下 `vision=true` 的 user message 图片当前实现 image block 丢失(代码层 bug,需补)

**半消除(可缓解但难根除)**:
- ⚠️ R1/Reasoning 模型在工具循环中的 thinking token 计费:OpenAI 路径协议层无 thinking budget,但可解析 reasoning_content 用于上下文展示
- ⚠️ R1/Reasoning 模型在工具循环中的稳定性:需要 prompt 补丁或 max_iterations 熔断
- ⚠️ 长上下文实测缩水:200K 标称 → 实测 7-8 折,通过"长任务拆短会话"工程化缓解

---

## 6. 选型矩阵与最佳实践

### 6.1 三档模型映射(社区共识)

| Claude 级别 | 国产替代 | 适用场景 |
|---|---|---|
| Opus | DeepSeek R1-0528 / Qwen3-Max-Thinking | 复杂架构、跨文件疑难 Bug |
| Sonnet | DeepSeek V3.2-Exp / GLM-4.6 / Qwen3-Coder-Plus | 编程上下文理解、单文件重构、单元测试 |
| Haiku | GLM-4.5 Air / Qwen3-Coder-Flash / Doubao-1.5-Coder-lite | 低成本高频检索、批量小任务 |

### 6.2 wyj-code 实操配置示例(可直接复制)

#### DeepSeek V3.2-Exp(OpenAI 兼容路径)

```toml
# ~/.wyj-code/config.toml
provider = "openai"
model = "deepseek-chat"
base_url = "https://api.deepseek.com"
api_key = "sk-xxx"
max_tokens = 8192
context_window = 164000
vision = false
openai_stream_options = true  # DeepSeek 支持 stream_options
```

#### GLM-4.6(Anthropic 兼容路径)

```toml
provider = "anthropic"
model = "glm-4.6"
base_url = "https://open.bigmodel.cn/api/anthropic"
api_key = "your_zhipu_key"
max_tokens = 8192
context_window = 200000
vision = false
prompt_cache = false  # 必须!非官方端点避免 beta header 400
thinking_budget = 8000  # 可选,GLM-4.6 支持
interleaved_thinking = true
```

#### Kimi K2(Anthropic 兼容路径)

```toml
provider = "anthropic"
model = "kimi-k2-0905-preview"
base_url = "https://api.moonshot.cn/anthropic"
api_key = "your_moonshot_key"
max_tokens = 8192
context_window = 256000
vision = false
prompt_cache = false  # 必须!
```

#### Qwen3-Coder-Plus(OpenAI 兼容路径)

```toml
provider = "openai"
model = "qwen3-coder-plus"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
api_key = "your_bailian_key"
max_tokens = 16384
context_window = 262000
vision = false
openai_stream_options = true
```

#### 火山引擎豆包(OpenAI 兼容)

```toml
provider = "openai"
# 模型名以火山引擎控制台当前可用为准,常为 ep-xxxxxxxx-xxxxx
model = "ep-2025xxxx-xxxxx"
base_url = "https://ark.cn-beijing.volces.com/api/v3"
api_key = "your_ark_key"
max_tokens = 8192
context_window = 128000
vision = false
openai_stream_options = false  # 火山引擎历史版本不支持 stream_options.include_usage
```

#### MiniMax Coding Plan(OpenAI 兼容)

```toml
provider = "openai"
model = "MiniMax-M3"
base_url = "https://api.minimaxi.com/v1"
api_key = "your_minimax_key"
max_tokens = 8192
context_window = 200000
vision = false
openai_stream_options = true
```

#### Claude Code 官方 / Codex 配置参考

```toml
# Claude Code 原生
provider = "anthropic"
model = "claude-opus-4-5"
base_url = ""  # 留空使用官方 api.anthropic.com
api_key = "sk-ant-xxx"
max_tokens = 8192
context_window = 200000
vision = true
prompt_cache = true  # 默认 true
thinking_budget = 16000
interleaved_thinking = true
```

```toml
# Codex CLI 走 OpenAI
provider = "openai"
model = "gpt-5-codex"
base_url = ""  # 留空使用官方 api.openai.com
api_key = "sk-xxx"
max_tokens = 16384
context_window = 200000
```

### 6.3 80/20 双剑合璧原则

- **复杂架构 / 跨文件疑难 Bug** → Claude 原版(Opus 4.5/4.6)
- **日常 CRUD / 单测 / 老代码解释** → 国产省钱
- **200K 中等文档** → GLM-4.7 / Kimi K2 / MiniMax 无感切换
- **500K+ 长文档** → DeepSeek V4-Pro / 小米 MiMo
- **跨多窗口复杂 Agent** → 仍首选 Claude Opus 4.5/4.6
- **中文 commit / 中文 docstring / 中文 API 命名** → 国产显著优于 Claude
- **多轮工具循环稳定性** → DeepSeek V3 ≈ GLM-4.6
- **批处理 / 自动化批量任务** → DeepSeek V3.2-Exp(价格屠夫)

### 6.4 选型矩阵

| 场景 | 第一推荐 | 备选 |
|---|---|---|
| 日常编码(成本优先) | **DeepSeek V3.2-Exp** | GLM-4.6 / Qwen3-Coder-Plus |
| 复杂架构(质量优先) | **Claude Sonnet 4.5** | Qwen3-Max-Thinking / GLM-4.6 |
| 长文档 / 超长上下文 | **DeepSeek V4-Pro / 小米 MiMo** | Kimi K2-0905 |
| 工具循环稳定性 | **DeepSeek V3** | GLM-4.6 |
| 中文任务 | **豆包 / Qwen3-Coder** | DeepSeek V3 / GLM-4.6 |
| 集成 Claude Code | **GLM-4.6**(官方 Anthropic 端点) | Qwen3-Coder-Plus(百炼 Anthropic 端点) |
| 集成 Cursor | **GLM-4.6**(官方收录) | DeepSeek V3 |
| 集成 Cline / Roo Code | **GLM-4.6 / DeepSeek V3** | Kimi K2 / Qwen3-Coder |
| 集成 wyj-code | **DeepSeek V3.2-Exp / GLM-4.6**(PROFILE_TEMPLATES 已内置) | Kimi K2 / Qwen3-Coder-Plus |

### 6.5 减少翻车的辅助配置

- **`.claudecodeignore`**:在项目根目录排除 `node_modules`、`dist`、大体积 log。上下文越小模型越稳
- **关闭实验性 beta 标志**:`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`(国产模型对部分 beta header 报错)
- **别用临时 export**:调试可以,日常写 `~/.zshrc` 永久生效
- **切换配置后必须重启终端**:新规则要新进程加载
- **监控 token 消耗**:国产模型价格虽低,但 R1 类思考模型单次任务可能消耗 50K+ token,**按月计费时注意 quota**
- **Base URL 三种坑**:
  1. Anthropic 兼容路径 vs OpenAI 兼容路径不一致
  2. 末尾 `/v1` 加不加(各工具不一致)
  3. 直连 vs 中转 vs 代理的延迟差异

### 6.6 Fallback 策略

```toml
# ~/.wyj-code/config.toml
provider = "openai"
model = "deepseek-chat"
base_url = "https://api.deepseek.com"

[routing]
# 主模型失败时自动切换
fallback_profiles = ["glm-fallback", "claude-fallback"]
```

具体见 `crates/config/src/lib.rs::RoutingCfg`(`lib.rs:303-336`)与 `cross_provider_fallback` 默认 `false` 的安全策略。

---

## 7. wyj-code 改进建议(代码层缺口清单)

按优先级:

### P1(协议层硬缺口,需产品决策)

- **OpenAI 路径 tool_result image 不支持**:协议层硬限制,暂无法消除,需要等 OpenAI Responses API 或厂商原生 vision tool
- **OpenAI 路径不支持原生 computer_20251124**:`register_computer_tool_if_enabled` 对 OpenAI 路径整体不注册。**建议**:在配置文档中明确告知用户 computer-use 需要 provider=Anthropic

### P1(稳定性)

- **Reasoning 模型在 OpenAI 路径 thinking 输出被忽略**:建议在 `OpenAIProvider::stream` 中解析 DeepSeek 的 `reasoning_content` 字段(已在 OpenAI 兼容端点出现),把它转成 `ContentBlock::Thinking` 落盘,即使不回传给模型也用于上下文展示。代码改动点:`crates/api/src/openai.rs::stream_events_from_chunk` 的 chunk 解析分支
- **GLM/Kimi/MiniMax 等 Anthropic 兼容端点的 usage 提取**:已在 `anthropic.rs:601-614` 双源(`message_start` + `message_delta`),但要继续监控是否所有国内厂商都遵守这一约定
- **R1/Reasoning 模型在工具循环中的稳定性**:在 `RequestPlan` 中对 vendor=DeepSeek 且 base_url 含 `reasoner` 的 Profile,自动设 `max_iterations = 32`(当前无明确上限,可能陷入 reasoning-token 黑洞)

### P2(易踩坑)

- **OpenAI 路径 `vision=true` 时 user message 内嵌图片走 String 丢失**:补 `image_url` 序列化分支(`crates/api/src/openai.rs:194-199` 的 user message content 构造),参考 OpenAI Chat Completions 官方 spec `content: [{type:"image_url", image_url:{url:"data:image/png;base64,..."}}]`
- **诊断能力**:在 `wyj-code doctor`(`/model doctor`)增加一段"国产模型配置体检",检查:
  - `prompt_cache` 是否与 `base_url` 匹配(走 Anthropic 兼容路径但 `prompt_cache=true` 会触发 400)
  - `openai_stream_options` 是否与 vendor 匹配
  - `thinking_budget` 是否对 OpenAI provider 设置(会被静默丢弃)
  - `vision=true` 是否对 OpenAI provider 设置(图片会丢失)

### P3(文档与最佳实践)

- **Profile 模板 note 字段**:给 `PROFILE_TEMPLATES` 数组的每一项加 `note` 字段,提示用户:
  - GLM/Kimi:必须保持 `prompt_cache = false`
  - 豆包 Ark:模型名常为 `ep-xxxxx`,以控制台为准
  - R1/Reasoning 模型:多轮工具循环需关闭 thinking 或手动打补丁
- **`/analysis` 命令**:可选,在 TUI `/help` 中新增一条命令入口,直接调阅本报告的关键章节
- **README 顶部加一行链接**:`see [国产模型对比报告](doc/analysis/domestic-models-vs-claude-code.md)`(待用户决定)

### P3(生态)

- **新协议扩展**:`Profile.wire_protocol` 字段已枚举 `OpenAiResponses | QwenNative | Gemini` 但 `build_provider` 未读。**建议**:在 v1.6 路线图中考虑实现 OpenAI Responses API 支持,这是消除 OpenAI Chat Completions 多项硬限制的关键
- **cc-switch 集成**:开源 GUI 工具 cc-switch 本地起代理把 Anthropic 请求重定向到任意 OpenAI 兼容端点(支持 Qwen / GLM / DeepSeek / Kimi / 豆包 / 百川 / MiniMax)。wyj-code 是否需要类似工具?当前 PROFILE_TEMPLATES 已覆盖主流,不需要

### 7.1 v1.5.7 实施状态

| 优先级 | 改进项 | 状态 | 关键改动 |
|---|---|---|---|
| **P1** | Reasoning thinking 落盘(DeepSeek `reasoning_content`) | ✅ 已实施 | `crates/api/src/openai.rs` `Delta` 加 `reasoning_content` 字段;`stream_events_from_chunk` 独立 emit `ThinkingDelta`;3 个新单测 |
| **P2** | OpenAI 路径 vision `image_url` 序列化 | ✅ 已实施 | `OpenAIProvider.supports_vision` 字段;`to_api_messages(messages, supports_vision)` 处理 `ContentBlock::Image` → `image_url` 数组;3 个新单测 |
| **P2** | tool_result image 优雅降级标记 | ✅ 已实施 | `crates/api/src/types.rs` `display_text()` 把 `ToolResultPart::Image` 改为 `[image omitted: media_type=..., ~NKB]`,带 data 长度估算 KB |
| **P1** | R1/Reasoning `max_iterations=32` 熔断 | ✅ 已实施 | `crates/core/src/agent.rs` `max_turns_for_model(default, model)` free function:model 含 `reasoner` / `-r1` 时降到 32,普通模型保留默认;`Agent::run_turn_with_injection_inner` 入口处覆盖;2 个新单测 |
| **P2** | loop detection(同工具+参数哈希近似) | ✅ 已实施 | `crates/core/src/agent.rs` `Agent.loop_guard: Arc<Mutex<VecDeque<(String, u64)>>>`;`fnv_hash_value` + `normalize_for_hash`(key 顺序不敏感);`detect_loop` 最近 5 条命中 ≥3 次触发;3 个新单测 + 1 个 mock provider 微调 |
| **P3** | `/model doctor` 国产配置体检 | ✅ 已实施 | `crates/api/src/doctor.rs` `ModelDoctorReport` 加 7 个 profile_* / effective_* / requires_supplier_usage 字段;`crates/tui/src/app.rs::format_tui_model_doctor` 输出 `profile:` 行;3 个新单测 |
| **P3** | Profile 模板 `note` 字段(踩坑提示) | ✅ 已实施 | `crates/api/src/models.rs::PROFILE_TEMPLATES` 9 个模板全部填充踩坑提示(GLM prompt_cache=false、Kimi prompt_cache=false、DeepSeek reasoning_content & max_iterations=32、火山引擎 ep-xxxxx 模型名、Qwen 不支持 extended thinking、Ollama 不支持 stream_options.include_usage、custom 完全开放) |
| **P3** | README + `/help` 加报告链接 | ✅ 已实施 | `README.md` 加报告链接行(版本号待 v1.5.7 发版时改);`crates/i18n/locales/{zh,en}.yml` `help.body` 末尾新增"## 报告"章节指向 GitHub blob |

**实施统计**:
- 8 项全部完成,1 个合并 commit(v1.5.7 发版时)
- 测试增量:11 个新单元测试(`crates/api` 6 + `crates/core` 4 + 已存在 1 个 `parts_display_text_uses_placeholder_for_images` 跑过),1 个既有 mock provider 调整(`RepeatedTwoToolProvider` 输入加 round 标识避免误伤 loop detection)
- 测试覆盖:`cargo clippy --all-targets` 干净,`cargo test --workspace` 789 个测试 0 失败

**未在本版本实施(留给 v1.6+)**:
- OpenAI Responses API 支持(消除 tool 内嵌图片 / cache_control 写 / 原生 computer 工具等多项硬限制)
- Anthropic 协议第三方端点能力深度扩展(GLM/Kimi 的 thinking 实际效果)
- `display_text()` 中期方案:`ToolResultPart::Image` 加 `width/height/filename` 字段从源头携带
- `Profile.max_iterations` 字段持久化(当前用 model 字符串关键字)
- `ToolResultContent::Blocks` 旧格式迁移
- `/benchmark` TUI 命令(实测 benchmark 跑分)

---

## 8. 信息缺口与后续工作

### 8.1 信息缺口

1. **GLM-4.6 / Qwen3-Coder extended thinking 实际效果**——官方未明确"thinking"字段是否等同 Claude 原生。建议企业内部 PoC 实测
2. **DeepSeek 官方 Anthropic 兼容端点**——目前主要靠 cc-switch / one-api 桥接,期待官方原生支持
3. **Yi / Baichuan / 腾讯混元** 在 coding agent 上的最新公开数据较少
4. **Aider Polyglot** 国产模型的官方分数仍稀缺,社区 leaderboard 数据滞后
5. **prompt cache 实际命中率**因 prompt 结构差异巨大,**官方数字往往乐观**,建议业务侧自行埋点统计
6. **MiniMax M3(M2)**:wyj-code 的 PROFILE_TEMPLATES 默认模型是 MiniMax-M2,但报告调研时使用 M3 数据,需确认 M3 是否已上线公开 API

### 8.2 后续工作

- **wyj-code 实测 benchmark**:跑同一组 Claude Code / Codex / wyj-code+国产 在固定任务(SWE-bench Lite 子集 + Aider Polyglot)上的对比,生成可比数据
- **`/benchmark` slash 命令**:让用户在 TUI 中一键跑内置 benchmark 套件,生成自家环境的实测对比表
- **持续更新本报告**:每个 wyj-code 大版本(v1.6/v1.7)后刷新"能力矩阵"与"踩坑清单"
- **OpenAI Responses API 支持**:v1.6 路线图候选,消除 OpenAI Chat Completions 多项硬限制
- **国内厂商新模型追踪**:每月更新主流厂商新模型与价格(参考 <https://api-docs.deepseek.com/news/>、<https://z.ai/blog>、<https://kimi-k2.com/blog>)

---

## 9. 参考资料

### 9.1 国内厂商官方

- DeepSeek: <https://api-docs.deepseek.com/> / <https://api-docs.deepseek.com/news/news250929>
- 智谱 GLM: <https://z.ai/blog/glm-4.6> / <https://open.bigmodel.cn/>
- 月之暗面 Kimi: <https://kimi-k2.com/blog/kimi-k2-api-pricing-and-benchmarks-2025>
- 阿里 Qwen3: <https://qwenlm.github.io/zh/blog/qwen3-coder> / <https://www.alibabacloud.com/help/zh/model-studio/claude-code>
- 字节豆包: <https://www.volcengine.com/product/doubao>
- 零一万物: <https://platform.lingyiwanwu.com/docs/api-reference>
- MiniMax: <https://api.minimaxi.com/>

### 9.2 基准与排行

- SWE-bench Verified: <https://www.swebench.com/>
- LiveCodeBench: <https://livecodebench.github.io>
- Aider Polyglot: <https://aider.chat/docs/leaderboards/>
- BFCL (Berkeley Function Calling): <https://gorilla.cs.berkeley.edu/leaderboard.html>
- CC-Bench(智谱自建):<https://lmspeed.net/model/glm-4-6>
- llm-stats 工具调用榜: <https://www.llm-stats.com/leaderboards/best-ai-for-tool-calling>

### 9.3 中文社区评测

- 智谱 GLM-4.6 解读: <https://aibreaking.org/blog/glm-4-6-china-coding-champion>
- 量子位 GLM-4.6: <https://www.qbitai.com/2025/09/338660.html>
- 凤凰科技 GLM Coding Plan: <https://tech.ifeng.com/c/8mJoak9q60M>
- IT 之家 GLM-4.6: <https://www.ithome.com/0/886/901.htm>
- 国产横评: <https://www.boluoblog.com/review/kimi-k2-vs-glm-4-6-vs-qwen3>
- 5 国产大模型实测: <https://www.cnblogs.com/lzhdim/p/20941516>
- 4 大开源横评: <https://blog.csdn.net/weixin_50937681/article/details/162944682>

### 9.4 Claude Code / Cursor 接入攻略

- GLM-4.6 接入: <https://juejin.cn/post/7555803454866980910> / <https://blog.csdn.net/2301_78677192/article/details/155456167>
- Qwen3 接入: <https://developer.aliyun.com/article/1739195> / <https://www.jxxy.net/ai/paths/claudecode-basics/claudecode-basics-02-03-configure-qwen>
- cc-switch 避坑: <https://juejin.cn/post/7660041027784458280>
- 全套接入指南: <https://www.iqilian.com/learn/claude-code-jieru-guochan-moxing> / <https://blog.csdn.net/qq_41684621/article/details/160308331>
- 200K 实测缩水: <https://blog.csdn.net/weixin_32487557/article/details/162160945>
- Cursor 配国产: <https://blog.csdn.net/xzx19930928/article/details/158961710>

### 9.5 工具调用稳定性

- Trae 实测: <https://www.toutiao.com/article/7611687521872904713>
- 金融数据 10 题横评: <https://blog.csdn.net/2511_94663557/article/details/161365671>
- DeepSeek V4 tool use 评测: <https://openllm.wavise.com/blog/deepseek-v4-tool-use-mcp-performance>
- AutoBe 本地 LLM 编码评测: <https://autobe.dev/articles/local-llm-benchmark-about-backend-generation.html>

### 9.6 Agent 工程实践

- 死循环架构复盘: <https://www.cnblogs.com/ljbguanli/p/19535678>
- 6 大工程原则: <https://tonybai.com/2025/07/30/six-principles-production-ai-agents/>
- DeepSeek V4 技术报告深读: <https://www.secrss.com/articles/89756>
- DeepSeek V4 实测: <https://www.cnblogs.com/zhayujie/p/19935607/deepseek-v4-eval>
- 字节面试官追问: <https://developer.aliyun.com/article/1732414>

### 9.7 Reddit / X / 英文社区

- Composio Kimi K2: <https://composio.dev/blog/kimi-k2-vs-claude-4-sonnet-on-swe-bench>
- Composio Kimi K2 analysis: <https://composio.dev/blog/kimi-k2-outperforms-claude-4-sonnet-on-swe-bench>
- Medium Kimi K2 underrated: <https://medium.com/@jarielisabeth/kimi-k2-the-most-underrated-ai-coding-agent-in-2025-a-coding-agent-benchmark-analysis>
- Medium Kimi K2 vs Sonnet 4.5: <https://medium.com/@lillykumari/kimi-k2-vs-claude-sonnet-4-5-open-weights-vs-closed-source-coding-models-a-coding-agent-benchmark-analysis>
- Coding Agent Routing 2026: <https://therouter.ai/blog/coding-agent-model-routing-comparison-2026>
- Cursor / Cline / Roo Code 同日收录 GLM-4.6: <https://x.com/i/status/1972181378933342667>
- Kimi K2 frontend: <https://nomadterrace.com/articles/agentic-ai-for-5x-less-why-kimi-k2-is-a-frontend-game-changer-1ynpulce>
- Backend 工程师视角: <https://dev.to/truelane/deepseek-vs-qwen-vs-kimi-vs-glm-a-backend-engineers-take-2cg0>
- Cline 40+ Provider: <https://github.com/cline/cline/discussions/4625>
- Cursor 官方收 GLM-4.6: <https://forum.cursor.com/t/new-model-z-ais-glm-4-6-is-now-in-public-preview-on-cursor/76135>
- AutoBe GLM vs Qwen vs DeepSeek: <https://autobe.dev/articles/local-llm-benchmark-about-backend-generation.html>

### 9.8 wyj-code 内部代码引用

| 文件 | 行号 | 引用内容 |
|---|---|---|
| `crates/api/src/provider.rs` | 9 | `EventStream` 定义 |
| `crates/api/src/provider.rs` | 13-19 | `RequestOptions` |
| `crates/api/src/provider.rs` | 46 | `Provider::stream(...)` 单方法 |
| `crates/api/src/anthropic.rs` | 147-178 | `build_api_tool` native 分支 |
| `crates/api/src/anthropic.rs` | 289-336 | `to_api_messages` Parts 转 image |
| `crates/api/src/anthropic.rs` | 351-360 | Thinking + RedactedThinking signature 回传 |
| `crates/api/src/anthropic.rs` | 383-386 | native tool beta header 拼装 |
| `crates/api/src/anthropic.rs` | 415-425 | `ThinkingParam` 构造 |
| `crates/api/src/anthropic.rs` | 457-474 | cache_control 三处 ephemeral |
| `crates/api/src/anthropic.rs` | 476-487 | max_tokens 自动抬高 |
| `crates/api/src/anthropic.rs` | 493-497 | interleaved-thinking beta |
| `crates/api/src/anthropic.rs` | 523-527 | `flat_map` 单 chunk 多 StreamEvent |
| `crates/api/src/anthropic.rs` | 587-593 | ToolUse 流式拼参数 |
| `crates/api/src/anthropic.rs` | 601-614 | `usage_event` 双源提取(message_start + message_delta) |
| `crates/api/src/anthropic.rs` | 643-674 | vision=false 降级占位测试 |
| `crates/api/src/openai.rs` | 89-101 | `ApiTool { type: function }` |
| `crates/api/src/openai.rs` | 143-147 | `prompt_tokens_details.cached_tokens` 解析 |
| `crates/api/src/openai.rs` | 171-179 | `role: tool` Parts 降级占位 |
| `crates/api/src/openai.rs` | 181-199 | tool result 用独立 `role: tool` 消息 |
| `crates/api/src/openai.rs` | 194-199 | vision message content 当前只走 String |
| `crates/api/src/openai.rs` | 204-217 | assistant 消息 `tool_calls` 数组 |
| `crates/api/src/openai.rs` | 241-248 | stop reason 映射 |
| `crates/api/src/openai.rs` | 252-268 | `cached_tokens` 解析 |
| `crates/api/src/openai.rs` | 287-314 | `PendingToolCall` HashMap |
| `crates/api/src/openai.rs` | 317-321 | finish_chunk 保留 usage |
| `crates/api/src/openai.rs` | 336 | `// OpenAI 格式不支持 Anthropic 式 thinking 参数` |
| `crates/api/src/openai.rs` | 348-359 | filter native tool |
| `crates/api/src/openai.rs` | 367-369 | `stream_options.include_usage` 控制 |
| `crates/api/src/openai.rs` | 389-417 | `PendingToolCall` per-index 维护 |
| `crates/api/src/types.rs` | 55-65 | `ContentBlock::ToolUse.id` 中性映射 |
| `crates/api/src/types.rs` | 69-72 | Thinking / RedactedThinking 块定义 |
| `crates/api/src/types.rs` | 81 | `ToolResultContent` 三形态 |
| `crates/api/src/types.rs` | 125 | `ToolDefinition { name, description, input_schema, native }` |
| `crates/api/src/types.rs` | 140 | `RawToolCall` |
| `crates/api/src/types.rs` | 147 | `NativeToolSpec` |
| `crates/api/src/types.rs` | 173 | `StreamEvent` 定义 |
| `crates/api/src/capabilities.rs` | — | `ModelIdentity` / `ModelCapabilities` |
| `crates/api/src/capability_cache.rs` | — | 7 天 TTL 能力 cache |
| `crates/api/src/model_catalog.rs` | 115-116 | structured_output / tool_choice reference-only |
| `crates/api/src/model_catalog.rs` | 118-133 | 国内 vendor = `parallel_tool_calls=false` + `RequiresSingleTool` |
| `crates/api/src/model_catalog.rs` | 214-249 | `ModelCatalog::resolve` vendor 推断 |
| `crates/api/src/prompt_policy.rs` | 8-26 | `compatibility_suffix` 4 组合 |
| `crates/api/src/request_plan.rs` | — | `RequestPlan::from_capabilities` 把 reasoning 设为 Disabled |
| `crates/api/src/doctor.rs` | — | `/model doctor` 输出 |
| `crates/api/src/models.rs` | 29-133 | `PROFILE_TEMPLATES` |
| `crates/api/src/retry.rs` | 54-68 | backoff 1s→32s 抖动 ±20% |
| `crates/api/src/retry.rs` | 73-136 | `send_with_retry` |
| `crates/api/src/lib.rs` | 35-69 | `build_provider` 三处 match |
| `crates/config/src/lib.rs` | 96-103 | `Provider` enum |
| `crates/config/src/lib.rs` | 105-107 | 二分法取舍注释 |
| `crates/config/src/lib.rs` | 110-116 | `WireProtocol` 枚举 |
| `crates/config/src/lib.rs` | 142-193 | `Profile` 字段定义 |
| `crates/config/src/lib.rs` | 238-242 | `is_official_anthropic_endpoint` |
| `crates/config/src/lib.rs` | 244-247 | `effective_prompt_cache` |
| `crates/config/src/lib.rs` | 255-265 | `uses_provider_exact_token_usage_for_model` |
| `crates/config/src/lib.rs` | 267-274 | `effective_openai_stream_options_for_model` |
| `crates/config/src/lib.rs` | 298-302 | `cross_provider_fallback=false` |
| `crates/config/src/lib.rs` | 303-336 | `RoutingCfg` + `RoutingRoles` |
| `crates/config/src/lib.rs` | 380-402 | `ModelRuntimeCfg` |
| `crates/core/src/compact.rs` | 99-101 | `estimate_image_tokens` |
| `crates/core/src/compact.rs` | 104-115 | `estimate_text_tokens` |
| `crates/core/src/compact.rs` | 135-235 | `compact_session` |
| `crates/core/src/agent.rs` | 777-781 | 流中断重试注释 |
| `crates/core/src/agent.rs` | 780-781 | 半成品不入账 |
| `crates/core/src/agent.rs` | 798-846 | round 入口按 `route.capabilities` 翻译 |
| `crates/core/src/agent.rs` | 801-805 | `PromptPolicy::compatibility_suffix` 调用点 |
| `crates/core/src/agent.rs` | 826-834 | `simplified_tool_definition` 调用 |
| `crates/core/src/agent.rs` | 3006 | `ThinkingProvider` 测试 |
| `crates/core/src/prompts.rs` | 61-115 | `EnvInfo` + `<env>` 块 |
| `crates/core/src/prompts.rs` | 122-150 | git 状态快照 |
| `crates/tools/src/computer.rs` | 540-594 | `ComputerTool::new(max_dim, native)` + `definition()` |
| `crates/tools/src/sub_agent.rs` | — | fake `Provider` impl(仅测试) |

---

**报告维护**:
- 首次产出:2026-08-23
- 数据时效:2025-09 至 2026-08
- 下次刷新:每个 wyj-code 大版本发布后同步更新"能力矩阵"与"踩坑清单"
- 联系方式:wyj-code 维护组(参见 `CONTRIBUTING.md`)
