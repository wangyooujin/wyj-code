//! 子 Agent 详情视图的数据准备：把执行轨迹翻译成与主聊天区同构的消息序列。
//!
//! 详情视图曾经把 trace 逐行铺开（`in:` / `out:` 前缀一行一条），那份文本墙既没有
//! 层级也不能折叠——一次 `Read` 的输出就是几百行 `path:line-content`，读起来像日志
//! 倾倒而不是「它到底在干什么」。
//!
//! 现在改为**复用聊天区的渲染管线**：轨迹先被翻译成 [`ChatMessage`] 序列（工具调用 /
//! 工具结果 / 助手文本 / 系统通知），再交给 `render::render_message_range`。于是子
//! Agent 的执行过程与主聊天流长得一模一样：`⏺ 工具名(参数)` 标题行、`⎿` 结果行、
//! markdown 正文、Ctrl+O 折叠/展开——**同一条管线，同一套折叠语义**。
//!
//! 本模块只做翻译，不碰绘制：所有颜色、缩进、折叠都在渲染层。

use super::*;

/// 一次尚未收到结果的工具调用在消息序列中的位置。
struct PendingCall {
    /// 在消息序列里的下标。结果行要插到它**紧后面**，插入后其余待配对项要跟着挪。
    index: usize,
    /// 与主聊天流同一套序号：工具调用行与结果行靠它配对成一块。
    seq: usize,
    /// 注册名（未折短的原始工具名），配对结果行用。
    tool_name: String,
}

/// 子 Agent 详情视图的缓存版本戳。
///
/// 轨迹是**只追加**的、工具流水只增不减，所以「条数」就是完整的版本号：任何一处
/// 变化都会改变它，缓存随之失效重建。终态结论与状态是在最后一次工具调用**之后**
/// 才落下的，条数不变而内容变了，所以这两项也要进版本戳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubAgentDetailVersion {
    /// 落盘轨迹的事件条数（无轨迹时为 0）。
    pub trace_len: usize,
    /// 面板工具流水的条数。
    pub tool_log_len: usize,
    /// 终态结论的长度（`None` = 还没有结论）。
    pub final_result_len: Option<usize>,
    /// 交接物里「停下原因」的长度（`None` = 没有交接物或没有原因）。
    ///
    /// 原因会作为一条系统通知进正文（无落盘轨迹时的兜底路径），所以它也得进
    /// 版本戳——它是在最后一次工具调用**之后**才落下的。
    pub handoff_reason_len: Option<usize>,
    /// 子 Agent 当前状态。
    pub status: SubAgentStatus,
}

/// 详情视图的聊天化消息缓存条目。
pub struct SubAgentDetailCache {
    /// 缓存属于哪个子 Agent。
    pub agent_id: u64,
    /// 生成这份缓存时的版本戳。
    pub version: SubAgentDetailVersion,
    /// 翻译好的消息序列。
    pub messages: Vec<ChatMessage>,
}

impl AppState {
    /// 刷新详情视图的聊天化消息缓存（版本变了才重建）。
    ///
    /// 缓存是**必需**的，不是优化：轨迹里每次工具输出都可能有几十 KB，每帧重建
    /// 等于每帧搬一遍全部输出、再把每段助手文本的 markdown 重解析一次——而详情
    /// 视图是每帧都画的。
    pub fn refresh_sub_agent_detail_cache(&mut self, id: u64) {
        let events = self.sub_agent_trace_events(id);
        let version = self.sub_agent_detail_version(id, events.as_ref().map(|e| e.len()));
        if self
            .sub_agent_detail_cache
            .as_ref()
            .is_some_and(|c| c.agent_id == id && c.version == version)
        {
            return;
        }
        let messages = match self.sub_agents.get(&id) {
            Some(s) => sub_agent_messages(s, events.as_deref().map(|v| &v[..])),
            None => Vec::new(),
        };
        self.sub_agent_detail_cache = Some(SubAgentDetailCache {
            agent_id: id,
            version,
            messages,
        });
    }

    /// 详情视图的版本戳。见 [`SubAgentDetailVersion`]。
    fn sub_agent_detail_version(&self, id: u64, trace_len: Option<usize>) -> SubAgentDetailVersion {
        let s = self.sub_agents.get(&id);
        SubAgentDetailVersion {
            trace_len: trace_len.unwrap_or(0),
            tool_log_len: s.map(|s| s.tool_log.len()).unwrap_or(0),
            final_result_len: s.and_then(|s| s.final_result.as_ref()).map(|r| r.len()),
            handoff_reason_len: s
                .and_then(|s| s.handoff.as_ref())
                .and_then(|h| h.reason.as_ref())
                .map(|r| r.len()),
            status: s.map(|s| s.status).unwrap_or(SubAgentStatus::Running),
        }
    }
}

/// 子 Agent 详情视图的消息序列：落盘轨迹优先，缺失时退回面板工具流水。
pub(super) fn sub_agent_messages(
    state: &SubAgentUiState,
    events: Option<&[TraceEvent]>,
) -> Vec<ChatMessage> {
    match events {
        Some(events) if !events.is_empty() => messages_from_trace(events, state),
        _ => messages_from_tool_log(state),
    }
}

/// 落盘轨迹 → 聊天消息。
///
/// 配对规则：`ToolStart` 先进「待配对」表，结果事件按**工具名**取走最早那条还没
/// 配上的同名调用（子 Agent 的工具可以并发，同名并发只能按顺序配）。结果行插到
/// 调用行**紧后面**——聊天区的渲染管线靠「相邻且序号相同」把两者认成一块，插到
/// 别处会退化成两行各自独立的 `⏺` / `⎿`。
fn messages_from_trace(events: &[TraceEvent], state: &SubAgentUiState) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut pending: Vec<PendingCall> = Vec::new();
    let mut seq = 0usize;
    let mut has_done = false;
    // 逐轮助手文本：终态结果里会把它们再拼一遍，先记下来好去重。
    let mut assistant_texts: Vec<String> = Vec::new();

    for ev in events {
        match ev {
            TraceEvent::Started { .. } => {
                // 类型、描述、后台标记都画在详情视图的标题行上，这里不重复。
            }
            TraceEvent::ToolStart {
                tool_name,
                input_json,
                truncated,
            } => {
                seq += 1;
                let index = messages.len();
                let mut call = ChatMessage::tool_call(
                    tool_call_display(tool_name, input_json, *truncated),
                    seq,
                );
                // Bash 的完整命令：标题行里是截断过的摘要，子 Agent 详情视图
                // 与主聊天流必须同口径（否则子 Agent 里跑的命令反而看不全）。
                call.full_command = tool_call_full_command(tool_name, input_json);
                messages.push(call);
                pending.push(PendingCall {
                    index,
                    seq,
                    tool_name: tool_name.clone(),
                });
            }
            TraceEvent::ToolEnd {
                tool_name,
                is_error,
                elapsed_secs,
                output,
                truncated,
            } => {
                let matched = pending.iter().position(|p| p.tool_name == *tool_name);
                let (call_seq, call_index) = match matched {
                    Some(pos) => {
                        let call = pending.remove(pos);
                        (call.seq, Some(call.index))
                    }
                    None => {
                        // 没有配对的调用行（轨迹从中间接上、或调用行被丢弃）：
                        // 结果单独成一条，渲染成 `⎿ result` 行。
                        seq += 1;
                        (seq, None)
                    }
                };
                let mut result = ChatMessage::tool_result(
                    output.clone(),
                    *is_error,
                    *elapsed_secs,
                    call_seq,
                    tool_name.clone(),
                    tool_result_summary(tool_name, output, *is_error),
                );
                if *truncated {
                    // 落盘时截断过的输出要**明说**——否则用户读到的是一段少了几千
                    // 字节的输出，却以为那就是全部。
                    result.display_summary = format!(
                        "{} {}",
                        result.display_summary,
                        wyj_i18n::tr("subagent.output_truncated")
                    );
                }
                match call_index {
                    Some(index) => {
                        let insert_at = index + 1;
                        messages.insert(insert_at, result);
                        for p in pending.iter_mut() {
                            if p.index >= insert_at {
                                p.index += 1;
                            }
                        }
                    }
                    None => messages.push(result),
                }
            }
            TraceEvent::Usage { .. } => {
                // token 用量画在详情视图的标题行上，正文不重复。
            }
            TraceEvent::Assistant { text } => {
                // 每轮的落点：工具行只说明「它做了什么」，这段文字说明「它由此
                // 得出了什么」——是轨迹里唯一有结论性质的内容。
                if !text.trim().is_empty() {
                    assistant_texts.push(text.clone());
                    messages.push(ChatMessage::assistant(text.clone()));
                }
            }
            TraceEvent::Control { action, accepted } => {
                let text = wyj_i18n::tr_fmt(
                    "subagent.control_line",
                    &[
                        ("action", action),
                        ("accepted", if *accepted { "yes" } else { "no" }),
                    ],
                );
                messages.push(ChatMessage::system(text));
            }
            TraceEvent::Done {
                result,
                is_error,
                elapsed_secs: _,
            } => {
                has_done = true;
                // 终态结果是**各轮助手文本的拼接**（`sub_agent.rs` 里的
                // `outputs.join("\n\n")`），逐轮文本已经各自成条——不剥掉就会让
                // 人把同一段话读两遍。剥完剩下的才是新信息：出错时的报错、
                // 中断时的「已被中断」说明。
                let rest = strip_rendered_turns(result, &assistant_texts);
                if !rest.trim().is_empty() {
                    let msg = if *is_error {
                        ChatMessage::assistant_err(rest)
                    } else {
                        ChatMessage::assistant(rest)
                    };
                    messages.push(msg);
                }
            }
        }
    }

    // 轨迹里没有收尾事件（被强杀 / 进程崩溃）时补一条终态提示：否则用户看到的是
    // 一串停在半路的工具调用，不知道是「还在跑」还是「已经死了」。
    if !has_done && state.status == SubAgentStatus::Interrupted {
        messages.push(ChatMessage::system(wyj_i18n::tr("subagent.interrupted")));
    }

    messages
}

/// 从终态结果里剥掉已经单独展示过的逐轮文本。
///
/// `TraceEvent::Done.result` 是各轮助手文本的拼接（见 `sub_agent.rs` 的
/// `outputs.join("\n\n")`），逐轮文本已经各自成条——不剥掉就会让人把同一段话
/// 读两遍。剥完剩下的才是新信息：出错时的报错、中断时的「已被中断」说明。
///
/// 逐轮文本在落盘时被截到 4KB（`ASSISTANT_HEAD_BYTES`），终态结果里那份却可能
/// 有 20KB——两者对不上时剥不掉，终态结果会整段再出现一次。这是有意的降级：
/// 与其猜边界把内容切碎，不如原样展示。
fn strip_rendered_turns(result: &str, turns: &[String]) -> String {
    let mut rest = result.to_string();
    for turn in turns {
        if turn.is_empty() {
            continue;
        }
        if let Some(pos) = rest.find(turn.as_str()) {
            rest.replace_range(pos..pos + turn.len(), "");
        }
    }
    rest.trim().to_string()
}

/// 没有落盘轨迹时，用面板那份工具流水兜底。
///
/// 流水只有「工具名 + 参数摘要 + 耗时」，没有输出正文，所以结果行是空的——展开后
/// 看到的是一行状态摘要，而不是（并不存在的）输出。
fn messages_from_tool_log(state: &SubAgentUiState) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = Vec::new();

    for (idx, tl) in state.tool_log.iter().enumerate() {
        let seq = idx + 1;
        let display = tool_call_display_from_parts(&tl.tool_name, &tl.arg_summary);
        messages.push(ChatMessage::tool_call(display, seq));
        let summary = if tl.is_error {
            wyj_i18n::tr("subagent.tool_log_failed")
        } else {
            wyj_i18n::tr("subagent.tool_log_done")
        };
        let mut result = ChatMessage::tool_result(
            String::new(),
            tl.is_error,
            tl.elapsed_secs.unwrap_or(0.0),
            seq,
            tl.tool_name.clone(),
            summary,
        );
        // 还在执行的调用没有耗时：让 `⏺` 行不显示 `· 0.0s`。
        result.elapsed_secs = tl.elapsed_secs;
        messages.push(result);
    }

    // 正在跑的工具：流水里最后一条通常就是它（`elapsed_secs` 还没落定），
    // 那时不再重复画一行。
    let running_in_log = state
        .tool_log
        .last()
        .is_some_and(|tl| tl.elapsed_secs.is_none());
    if !running_in_log {
        if let Some(cur) = &state.current_tool {
            messages.push(ChatMessage::tool_call(cur.clone(), messages.len() + 1));
        }
    }

    match state.status {
        SubAgentStatus::Running => {}
        SubAgentStatus::Done | SubAgentStatus::Failed => {
            if let Some(result) = &state.final_result {
                if !result.trim().is_empty() {
                    let msg = if state.status == SubAgentStatus::Failed {
                        ChatMessage::assistant_err(result.clone())
                    } else {
                        ChatMessage::assistant(result.clone())
                    };
                    messages.push(msg);
                }
            }
        }
        SubAgentStatus::Interrupted => {
            messages.push(ChatMessage::system(wyj_i18n::tr("subagent.interrupted")));
        }
    }

    // 停下的原因：交接摘要那一行放不下它（多半是一整段 provider 报错），但它是
    // 「为什么停了」唯一的直接答案——放进正文当一条系统通知。
    if let Some(reason) = state
        .handoff
        .as_ref()
        .and_then(|h| h.reason.as_deref())
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
            "subagent.reason_line",
            &[("text", reason)],
        )));
    }

    messages
}

/// 工具调用标题行：`工具名(参数摘要)`，与主聊天流同一口径。
fn tool_call_display(tool_name: &str, input_json: &str, truncated: bool) -> String {
    tool_call_display_from_parts(tool_name, &tool_call_arg(tool_name, input_json, truncated))
}

/// 从落盘轨迹的入参 JSON 里取展示用的参数摘要。
///
/// 入参被截断后不再是合法 JSON：退回展示原文的一小段，别把整段残缺 JSON
/// 塞进标题行。
fn tool_call_arg(tool_name: &str, input_json: &str, truncated: bool) -> String {
    match serde_json::from_str::<serde_json::Value>(input_json) {
        Ok(value) => tool_display_arg(tool_name, &value),
        Err(_) => {
            if truncated {
                input_json.chars().take(40).collect::<String>()
            } else {
                String::new()
            }
        }
    }
}

/// 落盘轨迹里的 Bash 完整命令（与主聊天流同一口径）。
///
/// 轨迹的入参 JSON 可能被截断成非法 JSON，此时取不到完整命令——降级为
/// 「只有摘要」，与主聊天流里同一场景的表现一致。
fn tool_call_full_command(tool_name: &str, input_json: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(input_json).ok()?;
    tool_full_command(tool_name, &value)
}

/// 由「工具名 + 参数摘要」拼出标题行（并挂上后端标注）。
fn tool_call_display_from_parts(tool_name: &str, arg: &str) -> String {
    let short_name = tool_display_name(tool_name);
    let display = if arg.is_empty() {
        short_name
    } else {
        format!("{short_name}({arg})")
    };
    // 后端标注挂在整行尾部（`⏺ Grep(pat in .) · rg`），与主聊天流同一口径。
    format!("{display}{}", tool_backend_badge(tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_tools::trace::TraceEvent;

    fn running() -> SubAgentUiState {
        SubAgentUiState {
            agent_type: "general-purpose".to_string(),
            description: "task".to_string(),
            background: false,
            status: SubAgentStatus::Running,
            started_at: std::time::Instant::now(),
            final_elapsed: None,
            input_tokens: 0,
            output_tokens: 0,
            tool_calls: 0,
            current_tool: None,
            tool_log: vec![],
            has_result: false,
            finished_at: None,
            final_result: None,
            handoff: None,
        }
    }

    /// 工具调用行与结果行必须**相邻且序号相同**——聊天区的渲染管线靠这条把两者
    /// 认成一块；不相邻就会退化成两行各自独立的 `⏺` / `⎿`。
    #[test]
    fn a_tool_call_and_its_result_stay_adjacent() {
        let events = vec![
            TraceEvent::ToolStart {
                tool_name: "Read".into(),
                input_json: r#"{"file_path":"/a.rs"}"#.into(),
                truncated: false,
            },
            TraceEvent::ToolEnd {
                tool_name: "Read".into(),
                is_error: false,
                elapsed_secs: 0.1,
                output: "hello".into(),
                truncated: false,
            },
        ];
        let messages = messages_from_trace(&events, &running());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::ToolCall);
        assert_eq!(messages[1].role, MessageRole::ToolResult);
        assert_eq!(messages[0].sequence_no, messages[1].sequence_no);
    }

    /// 并发调用时，先完成的那个结果仍要贴着自己的调用行。
    #[test]
    fn interleaved_tool_calls_keep_their_own_result_beside_them() {
        let events = vec![
            TraceEvent::ToolStart {
                tool_name: "Read".into(),
                input_json: r#"{"file_path":"/a.rs"}"#.into(),
                truncated: false,
            },
            TraceEvent::ToolStart {
                tool_name: "Grep".into(),
                input_json: r#"{"pattern":"x","path":"."}"#.into(),
                truncated: false,
            },
            TraceEvent::ToolEnd {
                tool_name: "Grep".into(),
                is_error: false,
                elapsed_secs: 0.2,
                output: "hit".into(),
                truncated: false,
            },
            TraceEvent::ToolEnd {
                tool_name: "Read".into(),
                is_error: false,
                elapsed_secs: 0.1,
                output: "body".into(),
                truncated: false,
            },
        ];
        let messages = messages_from_trace(&events, &running());
        assert_eq!(messages.len(), 4);
        for pair in messages.chunks(2) {
            assert_eq!(pair[0].role, MessageRole::ToolCall, "{pair:?}");
            assert_eq!(pair[1].role, MessageRole::ToolResult, "{pair:?}");
            assert_eq!(pair[0].sequence_no, pair[1].sequence_no, "{pair:?}");
        }
        // 每个结果紧跟在自己的调用行后面（不是「谁先完成谁先出现」）。
        for pair in messages.chunks(2) {
            let call = &pair[0].content;
            let body = &pair[1].content;
            if call.contains("Grep") {
                assert!(body.contains("hit"), "{pair:?}");
            } else {
                assert!(call.contains("Read"), "{pair:?}");
                assert!(body.contains("body"), "{pair:?}");
            }
        }
    }

    /// 终态结果是各轮助手文本的拼接——逐轮文本已经各自成条，不能再拼一遍。
    #[test]
    fn the_final_result_does_not_repeat_the_per_turn_texts() {
        let events = vec![
            TraceEvent::Assistant {
                text: "first turn".into(),
            },
            TraceEvent::Assistant {
                text: "second turn".into(),
            },
            TraceEvent::Done {
                result: "first turn\n\nsecond turn".into(),
                is_error: false,
                elapsed_secs: 1.0,
            },
        ];
        let messages = messages_from_trace(&events, &running());
        assert_eq!(messages.len(), 2, "逐轮文本两条，终态结果被剥空了");
        assert_eq!(messages[0].content, "first turn");
        assert_eq!(messages[1].content, "second turn");
    }

    /// 终态结果里**多出来的**部分（出错时的报错）不能被一起剥掉。
    #[test]
    fn the_error_part_of_the_final_result_survives() {
        let events = vec![
            TraceEvent::Assistant {
                text: "partial work".into(),
            },
            TraceEvent::Done {
                result: "partial work\n\nprovider exploded".into(),
                is_error: true,
                elapsed_secs: 1.0,
            },
        ];
        let messages = messages_from_trace(&events, &running());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert!(messages[1].is_error);
        assert!(messages[1].content.contains("provider exploded"));
        assert!(!messages[1].content.contains("partial work"));
    }

    /// 工具调用没有等到结果（进程被强杀）时，调用行独立存在，不该丢。
    #[test]
    fn a_tool_call_without_a_result_stays_alone() {
        let events = vec![TraceEvent::ToolStart {
            tool_name: "Bash".into(),
            input_json: r#"{"command":"sleep 99"}"#.into(),
            truncated: false,
        }];
        let messages = messages_from_trace(&events, &running());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::ToolCall);
        assert!(messages[0].content.contains("sleep 99"));
    }
}
