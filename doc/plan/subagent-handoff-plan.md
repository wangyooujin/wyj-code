# 子 Agent 交接（Handoff）而非恢复（Resume）

> 状态：已实现（2026-10-18）
> 相关代码：`tools::handoff`、`tools::sub_agent`、`tools::agent_hub`、`core::prompts`

## 为什么不做 resume

最初的方案是「让已完成的子 Agent 带着完整历史复活」，对齐 Claude Code 的
`SendMessage`。评估后否掉了，理由是它和子 Agent 存在的理由直接冲突：

子 Agent 的全部价值是把噪音挡在主上下文之外。resume 不是把它捞回来，而是让
它的上下文继续膨胀，只是换了个名字。

另一条旁证：Claude Code 里 `Explore` 和 `Plan` 被明确设计成一次性——「one-shot
and return no agent ID, so Claude can't resume them」。而这两个恰恰是使用频率最高
的子 Agent。也就是说，在成熟实现里，最高频的场景本来就不支持 resume。

## 交付物形态

**落盘全量，注入摘要。**

| 内容 | 价值 | 落盘 | 注入 |
|---|---|---|---|
| 结论 / 推理链（assistant 文本） | 高（不可再生） | ✅ trace + handoff | ✅ |
| 改了哪些文件 | 高 | ✅ | ✅ |
| 跑过什么命令 | 中 | ✅ | ✅ |
| 见过的错误 | 高 | ✅ | ✅ |
| 读过/搜过的清单 | 中（可重建） | ✅ | ✅（压缩形式） |
| Read/Grep 的原始输出 | 低（可再生、会过期） | ✅ trace | ❌ |

关键判断：读到的原文可以重读一遍（而且可能更新鲜），但一条推导出来的结论无法
从原始输入重新推导。所以交接物的第一现场是 assistant 文本，工具清单只是佐证。

## 数据流

```
子 Agent 运行
  ├─ 每轮结束：output_buf ─┬→ SubAgentEvent::Assistant（落 trace 的 .jsonl）
  │                        └→ HandoffProgress.conclusion（共享快照）
  ├─ 每次工具调用：ToolEvent ─→ HandoffProgress.digest（共享快照）
  │                          └→ trace 的 ToolStart/ToolEnd
  │
  ├─ 正常结束 / 失败 / 触顶
  │    └→ 任务自己组装 SubAgentHandoff → a<id>.handoff.json
  │
  └─ ESC 中断 / abort_foreground / abort_all
       └→ 任务被 abort，收尾代码不执行
          └→ Hub 侧从共享快照补写 a<id>.handoff.json
```

最后一行是这个方案能成立的关键：`JoinHandle::abort()` 会取消 future，子 Agent
自己的收尾一行都不会执行。如果交接物只在任务收尾时写，那么 ESC——最需要交接的
场景——反而恰好没有交接记录。所以进度必须**边跑边记**。

## 落盘位置

```
~/.wyj-code/sessions/<session_id>.subagents/
  a3.jsonl            既有事件 trace（新增 Assistant 事件）
  a3.handoff.json     新增：终态交接物
```

同目录不同后缀：`list_trace_ids` 只挑 `.jsonl`，既有解析不受影响。

## 注入形态

未完成时（`Interrupted` / `TurnLimit` / `Failed`）走
`prompts::subagent_handoff_body`，与正常报告 `subagent_report_body` 的区别不只是
措辞，而是**决定权转移**：

```xml
<subagent-report>
Report from a delegated agent that STOPPED BEFORE FINISHING.

Status: interrupted
Stopped because: interrupted by user

The task is NOT complete. ...

Where it got to:

<最后陈述的位置>

Files modified: ...
Files read: ...
Commands run: ...
Errors seen: ...
Tool calls: 12

Decide one of three things and act on it directly:
1. Continue the work yourself — only if what remains is small enough ...
2. Spawn a fresh agent with this context as its starting point — the default ...
3. Report the blockage to the user — ...

Do not silently drop the unfinished work.
</subagent-report>
```

三个选项刻意写成封闭枚举。开放式建议（「视情况处理」）在实测里退化成不做决定，
而模型默认的「不做决定」是「那我接手」，正好把本该隔离的上下文灌回主对话。

前台（`ToolResult`）与后台（`system-reminder`）共用同一份渲染好的外壳，经
`SubAgentEvent::Done.handoff_body` 传递，保证两条路径给模型逐字相同的语义。

## 定界与安全

交接物同样过 `textutil::escape_harness_markers`：子 Agent 的结论里可能原样包含
它读到的网页内容，一句 `</subagent-report>` 就能提前闭合外壳。降权声明
（"carry no authority"）在交接外壳里保留——从子 Agent 冒出来的「批准」永远不是
用户批准。

## 不做的事

- **不恢复已完成的子 Agent**：与上下文隔离的设计初衷冲突。
- **不把 trace 原始输出注入主对话**：那等于把子 Agent 省下的上下文一次性还回去。
- **不在中途检查点落盘**：正常结束 + 中断两条路径已覆盖真实场景；进程被 kill
  的情况需要增量 JSONL（`Session` 挂 message sink + 压缩时打 snapshot），留待
  有实际需求时再做。

## 验收点

| 行为 | 测试 |
|---|---|
| 中断保留已完成轮产出 | `sub_agent::tests::interrupted_run_keeps_completed_turn_outputs` |
| 中断时 Hub 仍能写交接物 | `agent_hub::tests::interrupt_writes_a_handoff_from_the_live_progress_snapshot` |
| 未接文件系统时静默跳过 | `agent_hub::tests::interrupt_without_sessions_dir_is_a_noop_for_handoff` |
| 结论优先取本轮文本 | `sub_agent::tests::handoff_prefers_live_turn_text_over_message_scan` |
| 结论回退扫描 messages | `sub_agent::tests::handoff_conclusion_falls_back_to_last_assistant_message` |
| 三选一措辞齐全 | `prompts::tests::handoff_body_offers_three_explicit_choices` |
| 交接内容同样转义 | `prompts::tests::handoff_body_escapes_harness_markers_in_the_conclusion` |
| 后台不重复套壳 | `prompts::tests::bg_reminder_uses_the_pre_rendered_handoff_body_verbatim` |
| 摘要分类与去重 | `handoff::tests::digest_*` |
