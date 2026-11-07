# crates/core

Agent 循环、会话、记忆、压缩、权限策略、存储布局。根 `CLAUDE.md` 已有跨 crate 约定，这里只写改本 crate 时会踩的不变式。

## 循环

- `agent.rs` 放回合调度（需要 `self` 或回合状态）。`agent/helpers.rs` 只放纯函数。不要把 `run_tool` / `append_hook_feedback` / `touched_dir` 抽出去。
- 框架重试（流中断、降参、超限、fallback）都发生在 assistant 消息提交**之前**。半成品不入史。已经执行过的工具，框架不得再跑一遍。
- 可见输出只认非空白 Text 或 ToolUse。纯 thinking 撞 `MaxTokens`：丢弃、不入史、放大预算并降一档 effort 后重试一次。降档按该 vendor 的 `effort_levels`，不要用固定的 max→high→medium 字符串。
- 同轮授权：Bash 按工具名，其余按输入指纹。`hard_deny` 在授权短路之前（根文件）。无交互面的拒绝不能写成「用户拒绝」。

## 上下文

`ContextAudit::from_request` 是压缩、`/cost`、`/context` 的唯一口径。

- 阈值比较用 `calibrated_prompt`，不用 `calibrated_total`。阈值已经扣过输出预留，再用含预留的量等于把 `max_tokens` 算两次，会压成死循环。400 恢复路径的 `note_context_overflow` 是例外，那里要含输出预留。
- 有效窗口 = 配置值，或端点报超限后记下的 `overflow_window_hint`（取小）。静态模型容量表不改写配置。`/context` 与 `/cost` 的分母用 audit 上的有效窗口。
- 真正的输出预留是 `max(max_tokens, thinking_budget + 4096)`。`fit_output_budget` 同时夹两者。夹完不许再用 `.max(budget)` 抬回去。放不进窗口的 budget 整段丢掉，不要放大用户的 `max_tokens`。
- 压缩切点 = 真实 user 消息，或已配对完整的 `assistant(tool_use)`。只认 user 时，长工具回合会整段被 reset。保留段以 assistant 开头时不要插入 ACK（会变成两条连续 assistant）。保留量 `clamp(窗口/8, 8K, 128K)`。压完必须重算 `session.context_audit` 并 `.with_window`。
- 清理停在软阈值的 80%，不要停在压缩硬阈值上，否则会贴线每轮清一次、反复打穿缓存。
- 先清理再摘要。可清理：`ELIDABLE_TOOLS` 与 `mcp__` 前缀。Edit / Write / BashOutput 永不清理。最近 `protect_recent` 条保留全文。CAS 不可写则整个机制关闭，不要就地删原文。`ContextRecall` 必须常驻可见。
- 压缩请求复用主对话前缀：同一 system、同一 `tools_for_request`、同一 volatile 尾。不要换成另一份 system，不要传空 tools，摘要指令是**新的一条** user 消息。手动 `/compact` 同样复用 `last_request_volatile`。
- 发送前超长截断只改副本，不改 `session.messages`（历史是缓存前缀）。
- `estimate_tools_tokens` 的缓存按 description 与 schema 失效，不能只按工具名。
- 摘要保留 thinking 的截断摘录（约 400 字）和「最终回答里看不到的推理」。整段丢掉会让压缩后重做探索、语言也漂移。

## 记忆与会话字段

- 注入顺序：memory v3 → evolution → memory v2。v3 在则 v2 不提取、不注入。
- 规约裁剪（`optimize_project_rules`，`/rules` 与 `wyj-code rules`）不调用模型：只删叙事句，约束句原样保留。默认不写盘。省不足 8% 且少于 400 字节则不写。
- 淘汰按 `retention_score`（新鲜度 0.30 + 使用强度 0.70），不是 `updated_at`。`Superseded` 永不淘汰。访问计数写 `access.json`，不要为一次命中重写 `records.json`。GC 先合并访问日志再截断。
- `search_sessions` 是全量扫盘，不得放进每轮自动注入。工具结果只索引开头约 800 字节；thinking 不索引；每会话最多 2 条。词法只走 `core::lexical`。中文精确短语加分不能省。
- `session_profile`、`session_effort` 随会话落盘，`/clear` 保留，旧文件缺字段读成 `None`。临时换模型不落盘。effort 优先级：env > 会话 > profile。`"off"` = 清空 effort 且 `thinking_switch = "disabled"`，不动 `thinking_budget`。
- 记忆快照每 10 次**用户提交**刷新（不要按含工具结果的 `Role::User` 计数）。后台提取每 3 次用户提交一次。

## 提示词与工具可见性

- 英文常量，不走 i18n。`<env>` 带 `home` 和 `Response language: <英文名> (code)`。不要只吐 `zh`。`language = ""` 等于未配置，解析收口到 `resolve_locale`。
- `response_language_reminder` 必须是 volatile **最后一段**。后面再拼东西，语言约束就失效。子 Agent 的 `with_system` 会抹掉语言段，覆盖后要补回。语言相关改动用新会话验证，旧会话里的英文 thinking 会自我强化。
- tools 数组只增不减。折叠掉的工具要在可用性块里按来源报数量，写明 folded, not missing，不要展开 schema。折叠列表为空时，这块输出必须与未折叠时逐字节相同。
- TodoWrite 默认开。开关只管注册；开启时写入 TodoStore + `.wyj-code/progress.md`（压缩后 `splice_progress_after_compact` 从磁盘重新注入）。`progress.md` 不绑 session：新会话 / `/new` / `/clear` / 全新启动必须清掉，只有 `-c`/`--resume` 才回填面板。`TODO_HINT` 仅在开启时注入。聊天流不要为它画三遍（跳过常规工具行，只留面板）。`parallel_safe = true`。`ALWAYS_VISIBLE` 含 TodoWrite 名，未注册时无 schema。独立 Progress 工具已并入 TodoWrite。
- Skill 不是工具。名单按 name 排序后冻进 stable 段。禁令是「不要自己去翻任何 skill 目录」，不是禁某几个路径。不要做成工具：正文会被 `persist_cap` 截断，`allowed-tools` 在 `Tool::run` 里不生效，子 Agent 会继承，市场上下载的正文也没有信任门控。
- 不可信文本（子 Agent 报告、网页、后台 shell 输出）过 `escape_harness_markers`。成功报告套 `<subagent-report>` 并写明其中的指令不是用户的。

## 轮数、验收、校验

- `max_turns` 默认 200。思考关不掉的端点（`ThinkingSpec`，不要另写 vendor 表）用 `forced_thinking_max_turns`（默认 64）。两个值都要可配置。
- 触顶前 `TURN_LIMIT_GRACE` 轮：注入**一次**收尾提醒，并拒绝执行新工具（回 `tool_result`，不要改 tools 数组）。仍不交卷才硬中止，中止前写 `CheckpointKind::TurnLimit`。
- `turn_truncated` 区分「做完了」和「预算没了」。截断时保留未完成 todo，不要自动打勾。自动续跑最多 8 次，指令里写明每一条未完成项，并声明这不是新请求、不要重写清单。
- `verify`：项目没写 `verify.command` 就完全不跑。触发工具是 Edit / Write / MultiEdit，不含 Bash。顺序是编辑成功 → PostToolUse hook → verify。
- `acceptance`：模型停下来时由框架判，不收录需要模型语义判断的判据。未声明则零成本。Plan 模式跳过。失败把证据注入历史并 `continue`，重试有上限，耗尽则按未完成交付。判据配错或路径逃出工作目录 = 不通过。`continue` 时不要提取记忆。

## 存储与布局

- 目录名只从 `wyj_config::layout` 取，调用方不写 `"sessions"` 这类字面量。
- `project_key` 是 `v1-<目录名>-<sha256 前 16>`。不用 `DefaultHasher`。
- 会话、信任文件不进 git。`storage.scope = "project"` 时会话进 `<git-root>/.wyj-code/local/`，并写 gitignore。不跟随 scope：`memory-v3/global`、`projects/`、`cas/`、`workspaces/`、`workflows/`。
- 旧的平铺 `sessions/` 与根上 `history.jsonl` 仍要能读。`locate()` 跨项目找到会话后写回原目录。doctor/prune 扫根目录和每个项目子目录，跳过 `.checkpoints/` 与 `.subagents/`。
- 切到 project scope 时，新位置还没有标志文件才从 home **复制**。不覆盖已有文件，不删旧目录。
- 探测锁用 `filelock::is_locked`，不用 `try_acquire`（后者会刷新 mtime，导致永远删不掉）。启动清理排在 CAS gc 之后。会话 TTL 默认 30 天（对齐 Claude Code；`0` = 永不删）。
- thinking 不参与 `persist_cap` 截断。窗口被 thinking 吃满时，调 context editing 或 effort，不要去设 `thinking_bytes`。`<sid>.subagents/` 仍没有自动回收。

## 其它

- Hooks：`UserPromptSubmit`、`Stop`、`PreToolUse`、`PostToolUse`。子 Agent 工厂不装 `HookRunner`。
- 通知正文的数字走 `core::humanize`。回合用 `fmt_duration_ms`，单次工具用 `fmt_duration_precise`（亚秒是整数毫秒，不要显示成 `0.0s`）。桌面通知优先终端 OSC；`osascript` 的点击会弹出选文件面板。daemon 不接通知。
- Evolution：只注入 Active、未冲突、未过期、citation 仍能在当前分支验证的条目。scope 不参与相关性评分。外部检索默认不进全局偏好。Rule / Skill 永不自动批准。不要把 skill eval 说成完整 replay。默认关闭自动激活规则、自动安装 skill、自改核心代码。
- 指纹：`prompt_fingerprint` 只比较 system 的 main 与本次发出的 tools。`/resume` 后第一次请求再比。`restore_usage_from` 必须同时清掉「已检查」标记和上次的漂移结论。
