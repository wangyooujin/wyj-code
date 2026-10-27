//! Agent 回合循环的纯辅助函数。
//!
//! 从 `agent.rs` 拆出。这里放的都是**不碰 `self`、不碰 IO、不持有状态**的
//! 自由函数：哈希与死循环检测、会话事件 id 派生、记忆快照组装、续跑判定。
//! 它们的共同点是「输入 → 输出」，改其中任何一个都不需要理解回合调度。
//!
//! 留在 `agent.rs` 的是与回合生命周期绑定的部分：`run_tool`（工具执行链路）、
//! `append_hook_feedback`（结果改写）、`touched_dir`（规约子目录定位）。
//!
//! 可见性一律 `pub(super)`：只有 `agent` 模块及其子模块（含测试）能看见，
//! 不进 crate 公共 API。

use crate::compact::estimate_text_tokens;
use crate::session::Session;
use wyj_api::types::ContentBlock;

pub(super) fn session_agent_event_id(call_id: &str) -> u64 {
    call_id
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

/// assistant 消息里有没有**用户能看见**的内容。
///
/// thinking / redacted_thinking 只是模型的中间过程：它们非空不能证明这一轮
/// 「说了话」。这正是「长会话跑几轮后突然停住、没有任何提示」的成因——
/// 输出预算被 reasoning 吃干净后，回复只剩 thinking 块，旧判据
/// `blocks.is_empty()` 认为「不是空响应」，循环按正常结束收尾。
pub(super) fn has_visible_output(blocks: &[ContentBlock]) -> bool {
    blocks.iter().any(|block| match block {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        ContentBlock::ToolUse { .. } => true,
        _ => false,
    })
}

/// 该端点**是否无法关闭思考**（强制思考）。
///
/// 判据走 `wyj_api::can_disable_thinking` 的唯一真值来源，而不是在 helpers 里
/// 再写一份 `user_can_disable` / `control` 的判定——那张表已经是「能不能关」的
/// 权威，另写一份必然漂移。截断重试与预防性降档都用它分流：关得掉就直接
/// `text_only` 把思考让位给正文，关不掉才退到降 effort 档位。
pub(super) fn thinking_cannot_be_disabled(identity: &wyj_api::ModelIdentity) -> bool {
    !wyj_api::can_disable_thinking(identity)
}

/// 取 vendor effort 档位表里**最弱**的一档。
///
/// 不能直接取 `levels.first()`：各 vendor 手写的档位表顺序不保证从弱到强
/// （Moonshot 是 low/high/max、MiniMax M3.1 是 low/medium/high/xhigh/max）。
/// 这里按固定的强弱阶梯找表里最弱档，确保「把思考压到最低」真正落到最低，
/// 而不是碰运气取到表里第一个。`adaptive` 不在阶梯上（它是服务端自决语义，
/// 不是强度），刻意跳过。
///
/// 供截断重试（关不掉思考时）与第三层预防性降档共用。
pub(super) fn lowest_reasoning_effort(levels: &[wyj_api::ReasoningEffort]) -> Option<&'static str> {
    const LADDER: &[wyj_api::ReasoningEffort] = &[
        wyj_api::ReasoningEffort::Minimal,
        wyj_api::ReasoningEffort::Low,
        wyj_api::ReasoningEffort::Medium,
        wyj_api::ReasoningEffort::High,
        wyj_api::ReasoningEffort::XHigh,
        wyj_api::ReasoningEffort::Max,
    ];
    LADDER
        .iter()
        .find(|level| levels.contains(level))
        .map(|level| level.as_str())
}

/// 历史里**回传的思考链**（`echo_back_in_messages` 的 assistant thinking 块）
/// 累计的粗估 token 数。第三层预防性降档用它当「长历史」的判据——只统计
/// thinking / redacted_thinking，因为回传给模型、从而放大下一轮思考长度的
/// 正是这部分，而不是 tool_result 正文。
pub(super) fn estimated_thinking_backlog_tokens(session: &Session) -> usize {
    session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|block| match block {
            ContentBlock::Thinking { thinking, .. } => estimate_text_tokens(thinking),
            ContentBlock::RedactedThinking { data } => data.len() / 4,
            _ => 0,
        })
        .sum()
}

/// 思考关不掉的模型在工具循环中容易陷入 reasoning-token 黑洞：每轮
/// reasoning 块计入 output tokens 但不一定推进工具调用决策。给它们一个
/// 保守的硬上限，普通模型保留调用方传入的默认值。
///
/// **判据有两层，缺一不可**：
///
/// 1. 模型名命中 `reasoner` / `-r1`（DeepSeek reasoner、R1 蒸馏系列）；
/// 2. vendor × wire × model 的 [`ThinkingSpec`] 表明**思考根本无法关闭**
///    （`user_can_disable == false` 或 `ignore_disabled_when_forced`）。
///
/// 第 2 层是 v0.3.15 补的。旧实现只认模型名，于是 MiniMax M2.x / M3.1
/// （`/anthropic` 端点发 `disabled` 直接 400）、Qwen `-thinking`、豆包
/// `-thinking`、Moonshot `-code` 这些**强制思考**的端点全都落到默认
/// 200 轮——而它们的每一轮都要完整跑一遍深度推理。实测一条
/// `MiniMax-M3.1-Flash` 会话：单个用户回合 163 次 API 往返、35.6 万
/// output token、墙钟 30 分钟以上，收尾提醒要到第 196 轮才注入，等于
/// 前 195 轮没有任何「该收敛了」的信号。名字里既没有 `reasoner` 也没有
/// `-r1`，旧判据完全看不见它。
///
/// 判据刻意走 `ThinkingSpec` 而不是再列一份 vendor 白名单：那张表已经是
/// 「这个端点能不能关思考」的唯一真值来源（thinking adapter 与 catalog 都
/// 只认它），在这里另写一份必然漂移——加一个 vendor 就要改两处。
///
/// 熔断值本身**不再写死**（v0.3.19）：`forced_max_turns` 由
/// `[agent].forced_thinking_max_turns` 传入。写死 32 的后果是「一轮一个
/// 工具调用」的端点在中型任务上必然撞顶——实测连续三个回合都在第 28 轮
/// （`32 - TURN_LIMIT_GRACE`）被关闸，任务被切成好几段，用户每次都得
/// 手打「继续」。死循环保护由 `loop_guard` 独立承担，这个值只是成本闸。
///
/// 抽成 free function 以便单测覆盖（不必构造完整 Agent）。
pub(super) fn max_turns_for_model(
    default_max_turns: usize,
    forced_max_turns: usize,
    vendor: &str,
    wire: &wyj_config::WireProtocol,
    model: &str,
) -> usize {
    let lower = model.to_ascii_lowercase();
    if lower.contains("reasoner") || lower.contains("-r1") {
        return forced_max_turns;
    }
    let spec = wyj_api::ThinkingSpec::for_vendor(vendor, wire, model);
    if !spec.user_can_disable || spec.ignore_disabled_when_forced {
        return forced_max_turns;
    }
    default_max_turns
}

/// 对 `serde_json::Value` 做规范化后做 FNV-1a 64-bit 哈希。Object 的 key
/// 递归排序、数组保留原序,Primitive 直接序列化。这样哈希在 schema 字段
/// 顺序变化、空白差异等情况下保持稳定,适合 loop detection 用。
///
/// 注：抽成 free function 便于单测覆盖 hash 稳定性。
pub(super) fn fnv_hash_value(value: &serde_json::Value) -> u64 {
    let normalized = normalize_for_hash(value);
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// 把 Object 的 key 递归排序,数组与 Primitive 保持原样。让同一份语义 JSON
/// 在 key 顺序打乱后仍产生相同 hash。
pub(super) fn normalize_for_hash(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                out.insert(k.clone(), normalize_for_hash(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_for_hash).collect()),
        other => other.clone(),
    }
}

/// LoopGuard 容量上限。每次 exec 后保留最近 N 条 (name, hash),超过则
/// 弹出最旧的,避免无界增长。
pub(super) const LOOP_GUARD_CAPACITY: usize = 16;
/// 触发跳过的同 name+hash 命中阈值。最近 N 条里命中 ≥ K 次视为死循环。
pub(super) const LOOP_GUARD_WINDOW: usize = 5;
pub(super) const LOOP_GUARD_THRESHOLD: usize = 3;

/// 触顶前的收尾宽限轮数：距离上限还剩这么多轮时，向模型注入一次「停止调用
/// 工具、立即总结交付」的指令（见 `run_turn_with_injection_inner`）。
///
/// 取 4 而不是 1：模型读到指令后仍可能先补一次工具调用（比如把刚跑完的
/// 测试结果读回来再作答），宽限太窄会让它来不及交出总结就被硬中止，白白
/// 浪费已经完成的全部工作。4 轮足够它「不再调工具 + 写总结」，同时相对 200
/// 的总预算可忽略不计。
pub(super) const TURN_LIMIT_GRACE: usize = 4;

/// 连续工具参数校验失败到第几轮时，向模型注入一次「换个思路、就地收尾」的指令。
///
/// 取 3 而非原来的「3 轮直接 bail」：原实现把「同一类参数错误重复 3 次」直接
/// 判成整轮失败，把这一轮其它工具已经产出的成果一起丢掉。实测一次真实故障
/// （模型把 19 位 `tree_id` 反复当字符串发）里，前几轮已经成功完成了窗口枚举、
/// 截图和菜单列举，最后却因为一个调用的参数形态而整轮中止。现在改成先收尾。
///
/// 硬中止保留在 `GRACE_AT + 1` 轮之后：模型无视收尾指令、继续用同样方式硬撞，
/// 那才是真正需要报错终止的情形。
pub(super) const INVALID_ARGUMENT_GRACE_AT: usize = 3;

/// 参数校验连续失败时的收尾指令。措辞要点是**换路径**而不是「再试一次」——
/// 模型卡住的原因通常是某个调用的参数形态它表达不出来，继续原样重试只会
/// 复现同一个失败。
pub(super) fn invalid_argument_wrap_up_notice() -> &'static str {
    "<tool-arguments-wrap-up>\n\
     Several consecutive tool calls were rejected because their arguments did not \
     match the tool schema. Do not repeat the same call shape again. Either fix the \
     specific field the error named, or switch to a different tool or approach that \
     does not need that argument. If the task is already blocked, say so plainly and \
     report what you learned so far.\n\
     连续数次工具调用因参数不符合 schema 被拒绝。不要再用同样的参数形态重试。\
     请修正报错点名的字段，或改用不需要该参数的其他工具与方式；若任务确实卡住，\
     请如实说明并汇报目前已知的信息。\n\
     </tool-arguments-wrap-up>"
}

/// 用户在权限对话框里主动拒绝时回灌给模型的原因。
///
/// 措辞刻意命令式：模型收到「用户拒绝了」之后的典型失败模式是换个参数再试一次，
/// 那既没用又招人烦。Auto 分类器的拒绝走各自的 `reason`，不带这句。
pub(super) const USER_DENIED_REASON: &str =
    "用户拒绝执行该操作。请不要重试该操作；改用其他方式，或先向用户询问原因。";

/// 检查 (name, hash) 是否已经在最近 LOOP_GUARD_WINDOW 次调用中命中
/// LOOP_GUARD_THRESHOLD 次以上。是则返回 true 表示模型陷入循环,应跳过本次执行。
///
/// 抽成 free function 便于单测覆盖。
pub(super) fn detect_loop(
    window: &std::collections::VecDeque<(String, u64)>,
    name: &str,
    hash: u64,
) -> bool {
    window
        .iter()
        .rev()
        .take(LOOP_GUARD_WINDOW)
        .filter(|(n, h)| n == name && *h == hash)
        .count()
        >= LOOP_GUARD_THRESHOLD
}

pub(super) fn last_user_goal(session: &Session) -> String {
    session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == wyj_api::types::Role::User)
        .map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.trim()),
                    _ => None,
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                "attachment-only user goal".to_string()
            } else {
                text
            }
        })
        .unwrap_or_else(|| "unknown user goal".to_string())
}

/// Memory v3 检索不能只看最后一句。把最近几个真实 user task 合并后，像“继续”
/// “再分析一下”这样的续接请求仍然携带上一个主题，同时不把 assistant 推断当查询。
pub(super) fn memory_query_context(session: &Session) -> String {
    let mut goals = session
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == wyj_api::types::Role::User)
        .filter_map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.trim()),
                    _ => None,
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        })
        .take(4)
        .collect::<Vec<_>>();
    goals.reverse();
    if goals.is_empty() {
        "unknown user goal".to_string()
    } else {
        goals.join("\n")
    }
}

/// "继续/接着/continue/resume/再来/go on" 等纯续接请求；命中时按最近
/// InProgress Task 注入恢复点。混合句（"继续看 XX"）由 `memory_query_context`
/// 自然消化，不算 continuation。
pub(super) fn is_continuation_request(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }
    let normalized = trimmed
        .trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, '。' | '，' | ',' | '；' | ';' | '！' | '!' | '?' | '？')
        })
        .to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    // 长度上限防止误命中（"继续"两个字后面有空格+大段说明就当普通 query 处理）。
    if normalized.chars().count() > 12 {
        return false;
    }
    matches!(
        normalized.as_str(),
        "继续"
            | "继续吧"
            | "接着"
            | "接着干"
            | "接着来"
            | "再来"
            | "再来一次"
            | "go on"
            | "continue"
            | "resume"
            | "proceed"
    )
}

/// 拼装 v3 memory 注入：Project Brief + (可选) 续接 Task 详情 + 少量高相关
/// Project / Global claim。"继续"分支叠加 Open Tasks 提示，避免 Brief 与
/// 续接恢复点信息互相吞掉。
pub(super) fn build_memory_snapshot(memory: &crate::MemoryV3Store, session: &Session) -> String {
    let query_context = memory_query_context(session);
    let brief = memory.project_brief(&query_context);

    let last_query = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == wyj_api::types::Role::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let mut out = brief;
    if is_continuation_request(&last_query) {
        if let Some(suffix) = continuation_suffix(memory) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&suffix);
        }
    }
    out
}

/// "继续" 命中时的追加注入：找到最近 InProgress Task → 写入 next step；
/// 若最近任务是 Blocked → 显式标注阻塞原因；所有任务都已关闭 →
/// 用 i18n key `memory.continuation.no_open_tasks` 提示用户给新任务。
pub(super) fn continuation_suffix(memory: &crate::MemoryV3Store) -> Option<String> {
    match memory.find_latest_in_progress_task() {
        Ok(Some(task)) => {
            let next_step = task
                .task_steps
                .iter()
                .find(|step| !step.done)
                .map(|step| step.description.clone())
                .unwrap_or_else(|| "(no next step recorded)".to_string());
            let blocked_note = match task.task_status {
                Some(crate::TaskStatus::Blocked) => task
                    .blocked_reason
                    .as_deref()
                    .map(|reason| format!("\nblocked_reason: {reason}"))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Some(format!(
                "<continuation>\nResuming task [{}] {}\nstatus: {:?}\nnext: {}{}\n</continuation>",
                task.id, task.title, task.task_status, next_step, blocked_note
            ))
        }
        Ok(None) => {
            // 没有 InProgress：检查是否有 Blocked，有则提示恢复 Blocked。
            match memory.find_all_open_tasks() {
                Ok(open) if !open.is_empty() => {
                    let head: String = open
                        .iter()
                        .take(5)
                        .map(|t| {
                            format!(
                                "- [{}] {} ({:?}{})",
                                t.id,
                                t.title,
                                t.task_status,
                                t.blocked_reason
                                    .as_deref()
                                    .map(|r| format!(", blocked: {r}"))
                                    .unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Some(format!(
                        "<continuation>\nNo in-progress task found. Still-open tasks:\n{head}\n</continuation>"
                    ))
                }
                _ => Some(wyj_i18n::tr("memory.continuation.no_open_tasks")),
            }
        }
        Err(error) => Some(format!(
            "<continuation>\nFailed to look up open tasks: {error}\n</continuation>"
        )),
    }
}
