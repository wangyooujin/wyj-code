//! Agent 事件 → UI 状态的翻译层。
//!
//! 从 `app.rs` 拆出。`apply_agent_event` 是 TUI 里最长的单个函数：模型流的
//! 每一种增量（thinking / text / tool call / tool result / usage / 错误）都要
//! 落到具体字段上，还要维护「工具调用与结果的配对顺序」「流式缓冲的固化时机」
//! 「自动折叠的边界」等不变量。它不持有任何自己的状态，形态上是
//! `AppState` 的一大块方法，因此这里用 `impl AppState` 而不是自由函数。
//!
//! 与 `render` 的关系：本模块只写入状态，不读终端尺寸、不排版。

use super::*;

impl AppState {
    pub(super) fn apply_agent_event(&mut self, event: AgentEvent) {
        // 回合心跳：任何事件都算「有进展」。看门狗判的是「多久没动静」，
        // 不是「这轮跑了多久」——长回合只要还在吐 delta / 跑工具就不该判死。
        self.last_turn_activity = Instant::now();
        match event {
            AgentEvent::TextDelta(d) => {
                // thinking 结束、正文开始：先折叠思考行
                self.flush_thinking();
                self.stream_rate.note(&d);
                self.streaming_buf.push_str(&d);
                self.note_new_chat_content();
            }
            AgentEvent::ThinkingDelta(d) => {
                if self.thinking_buf.is_empty() {
                    self.thinking_started = Some(std::time::Instant::now());
                }
                // thinking 也是模型在吐 token，且用户看到的是同一个 spinner，
                // 速率必须把它算进去——否则开思考的模型会一直显示 0 tok/s。
                self.stream_rate.note(&d);
                self.thinking_buf.push_str(&d);
                self.note_new_chat_content();
            }

            AgentEvent::ToolStart {
                id,
                name,
                input_json,
            } => {
                // TodoWrite 有专属面板（push_inline_todo_lines），走常规工具行会
                // 让同一次调用在聊天流里出现三遍：⏺ TodoWrite / ⎿ 任务列表已更新… /
                // TodoWrite 面板头 + 分隔线 + 条目。面板已经把"改了什么"讲得更完整，
                // 常规行只剩噪音。这里仍然照常登记 tool_info 与占用 seq（ToolEnd 靠
                // seq 配对回执，见下方同名分支），只是不往聊天流插消息。
                let is_todo = name == "TodoWrite";
                self.tool_call_count += 1;
                let seq = self.tool_call_count;
                let arg = tool_display_arg(&name, &input_json);
                let short_name = tool_display_name(&name);
                let display = if arg.is_empty() {
                    short_name
                } else {
                    format!("{short_name}({arg})")
                };
                // 后端标注挂在整行尾部（`⏺ Grep(pat in .) · rg`），三处消费同一串：
                // 聊天流 ToolCall、标题栏 current_op、思考折叠标题。
                let display = format!("{display}{}", tool_backend_badge(&name));
                self.current_op = Some(display.clone());
                self.current_tool_name = Some(name.clone());
                // 模型已经停止生成、开始调工具：速率归零。工具可能跑几十秒，
                // 期间继续显示上一个 tok/s 就是在报一个已经不存在的事实。
                self.stream_rate.reset();
                // 步骤计时起点：并发多个工具时后到的 ToolStart 会覆盖成最后一个，
                // 与 `current_op` 保持同一口径（标题栏只展示"最近开始的那一步"）。
                self.current_op_started = Some(Instant::now());
                self.tool_info.insert(id, (name.clone(), seq));
                self.flush_streaming();
                if !is_todo {
                    let mut msg = ChatMessage::tool_call_with_command(
                        display,
                        seq,
                        tool_full_command(&name, &input_json),
                    );
                    // ToolCall 也记录工具名，供 SubAgent Started 事件 FIFO 配对
                    msg.tool_name = Some(name);
                    self.push_tracked_message(msg);
                }
            }

            AgentEvent::ToolEnd {
                id,
                output,
                is_error,
                elapsed_secs,
            } => {
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                let (name, seq) = self.tool_info.remove(&id).unwrap_or_default();
                // 实时输出块就此收工：后续由折叠态的 ⎿ 结果行接管正文。
                // 漏删的话，`apply_tool_output_chunk` 的闸门会挡住它，但哈希表
                // 里会一直留着一份没人渲染的快照（每个执行过的 Bash 一份 8KB）。
                self.live_tool_out.remove(&id);
                // ToolStart 已跳过 TodoWrite 的常规行，这里对称地跳过它的回执。
                // 不能只依赖"call_idx 找不到配对"来自然落空——那样会走
                // `push_tracked_message` 把 "⎿ 任务列表已更新: N 项…" 追加到流尾。
                if name == "TodoWrite" {
                    // TodoWrite 没有 ToolCall 行，因此也不存在 sub_agent_id 回填。
                } else {
                    let summary = tool_result_summary(&name, &output, is_error);
                    // 找到对应的 ToolCall 消息位置：多个工具调用在同一轮里并发执行时，
                    // ToolEnd 到达顺序未必与 ToolStart 一致，必须按 seq 定位插入点，
                    // 而不是无条件 push 到列表尾部——否则并发调用会导致所有 ⏺ 标题
                    // 先聚在一起，之后所有 ⎿ 结果再聚在一起，无法一一对应。
                    let call_idx = self.messages.iter().rposition(|m| {
                        matches!(m.role, MessageRole::ToolCall) && m.sequence_no == Some(seq)
                    });
                    // Agent 工具：把 ToolCall 上绑定的子 Agent id 带到 ToolResult，
                    // 供展开时渲染内部工具调用明细
                    let sub_id = if name == "Agent" {
                        call_idx.and_then(|i| self.messages[i].sub_agent_id)
                    } else {
                        None
                    };
                    let mut msg = ChatMessage::tool_result(
                        output,
                        is_error,
                        elapsed_secs,
                        seq,
                        name,
                        summary,
                    );
                    msg.sub_agent_id = sub_id;
                    match call_idx {
                        Some(i) => {
                            self.insert_tracked_message_after(i, msg);
                        }
                        None => {
                            self.push_tracked_message(msg);
                        }
                    }
                    if let Some(said) = sub_id {
                        if let Some(s) = self.sub_agents.get_mut(&said) {
                            s.has_result = true;
                        }
                    }
                }
            }

            AgentEvent::ToolPermissionRequest {
                tool_name,
                action_summary,
                response_tx,
            } => {
                self.permission_dialog = Some(PermissionDialog {
                    tool_name,
                    action_summary,
                    response_tx,
                });
            }

            AgentEvent::TurnDone { truncated } => {
                self.flush_streaming();
                self.stream_rate.reset();
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                self.turns += 1;
                self.save_needed = true;
                // 先对齐排队镜像再清 injector：那之后就拿不到共享队列了
                self.reconcile_pending_queue();
                self.injector = None;
                if truncated {
                    // 被推理轮数预算关闸收尾：任务**没有**做完，只是框架把工具
                    // 关掉了。这里绝不能把 in_progress 补成 completed——那正是
                    // 「AI 说做完了、面板全打勾，实际还剩一半」的来源。
                    //
                    // 改成保留 in_progress 并记下「待续跑」标记，由主循环在
                    // 空闲帧自动起下一轮（见 `todo_resume_pending`）。用户的
                    // 观感从「停住等我说继续」变成「这一轮预算用完，自动接上」。
                    //
                    // **只有确实还有未完成项才续跑**：模型可能在撞顶那一轮已经
                    // 把清单全部标成 completed，此时再续一轮就是白白多烧一个
                    // 完整回合。
                    let unfinished = self.unfinished_todos();
                    let over_budget = self.todo_resume_count >= TODO_RESUME_LIMIT;
                    self.todo_resume_pending = !unfinished.is_empty() && !over_budget;
                    if !unfinished.is_empty() {
                        if over_budget {
                            self.push_tracked_message(ChatMessage::system(wyj_i18n::tr_fmt(
                                "spinner.todo_resume_exhausted",
                                &[("count", &unfinished.len().to_string())],
                            )));
                        } else {
                            self.push_tracked_message(ChatMessage::system(wyj_i18n::tr_fmt(
                                "spinner.todo_budget_resume",
                                &[("count", &unfinished.len().to_string())],
                            )));
                        }
                    }
                } else {
                    // 回合已正常结束（`stop_reason != tool_use`，模型已产出最终
                    // 回答）：任何仍处于 InProgress 的 todo 都视为已完成。否则
                    // UI 会卡在 "[N/N] ⋯ xxx" 这种永远不收口的"进行中"状态（即便
                    // todo_stats 不再累加耗时，已渲染行的状态字段读不到 Completed
                    // 也会显示 ⋯），直到下一次 TodoWrite 才有机会刷新——而很多
                    // 模型会在最后一次回答里直接出最终输出，不再调用一次 TodoWrite
                    // 把最后一项标 completed，于是用户看到"AI 已回完，但任务列表
                    // 还显示进行中"的割裂感。
                    //
                    // 走与 TodoUpdate handler 里 was_in_progress → not_in_progress
                    // 同款 started_at → elapsed_secs 收口，统计数字保持连续。
                    self.todo_resume_pending = false;
                    if let Some(items) = self.current_todos.as_mut() {
                        for item in items.iter_mut() {
                            if item.status == TodoStatus::InProgress {
                                item.status = TodoStatus::Completed;
                                if let Some(s) = self.todo_stats.get_mut(&item.id) {
                                    if let Some(start) = s.started_at.take() {
                                        s.elapsed_secs += start.elapsed().as_secs_f64();
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(start) = self.turn_start_time.take() {
                    let elapsed = start.elapsed().as_secs_f64();
                    let d_in = self
                        .total_input_tokens
                        .saturating_sub(self.turn_start_input_tokens);
                    let d_out = self
                        .total_output_tokens
                        .saturating_sub(self.turn_start_output_tokens);
                    self.last_turn_elapsed_secs = Some(elapsed);
                    self.last_turn_input_tokens = d_in;
                    self.last_turn_output_tokens = d_out;
                    self.push_tracked_message(ChatMessage::turn_summary(elapsed, d_in, d_out));
                    // 统一通知：主回合正常完成。token 走 `fmt_tokens`（与状态栏
                    // 同一套口径）：上万后压成 `12.3k`，避免长会话把通知正文顶爆。
                    let summary = format!("{}↑ {}↓", fmt_tokens(d_in), fmt_tokens(d_out));
                    let summary = truncate_notify_body(&summary);
                    notify_emit(NotificationEvent::TurnFinished {
                        duration_ms: (elapsed * 1000.0) as u64,
                        summary,
                        session_id: if self.current_session_id.is_empty() {
                            None
                        } else {
                            Some(self.current_session_id.clone())
                        },
                    });
                }
            }

            // ── 后台任务（非回合）完成 ───────────────────────────────
            // 这四个事件对应「离开主循环执行的长任务」：`/compact` 的 LLM 往返、
            // `needs_background` 的 slash 命令、`/checkpoint` 的工作区快照、
            // `/rewind` / `/branch` 的快照 + 文件还原、`/evolve` 的候选激活。
            // 它们共用的收尾形状：摘掉标题栏的操作名、清 thinking 标志，让
            // 用户输入立刻恢复。
            AgentEvent::CompactFinished {
                result,
                compact_count,
                compact_generation,
            } => {
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                self.compact_count = compact_count;
                self.compact_generation = compact_generation;
                self.save_needed = true;
                match result {
                    Ok(r) if r.messages_removed > 0 => {
                        self.push_tracked_message(ChatMessage::assistant(wyj_i18n::tr_fmt(
                            "compact.done",
                            &[
                                ("removed", &r.messages_removed.to_string()),
                                ("saved", &r.tokens_saved_estimate.to_string()),
                            ],
                        )));
                        // 代际越深，摘要越像「地图」而不是「记录」。这是用户唯一
                        // 能直接看到「模型手里的历史已经叠了几层」的地方——
                        // 静默累积到第 8 代而不提示，等于让用户以为压缩无损。
                        if r.generation > wyj_core::compact::MAX_COMPACT_GENERATIONS {
                            self.push_tracked_message(ChatMessage::system(wyj_i18n::tr_fmt(
                                "compact.generation_warning",
                                &[("generation", &r.generation.to_string())],
                            )));
                        }
                    }
                    Ok(_) => {
                        self.push_tracked_message(ChatMessage::assistant(wyj_i18n::tr(
                            "compact.noop",
                        )));
                    }
                    Err(e) => {
                        self.push_tracked_message(ChatMessage::assistant_err(format!(
                            "[{}] {e}",
                            wyj_i18n::tr("compact.failed_tag")
                        )));
                    }
                }
            }

            AgentEvent::BackgroundCommandDone { name, result } => {
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                match result {
                    Ok(CommandResult::Output(out)) => {
                        self.push_tracked_message(ChatMessage::assistant(out));
                    }
                    Ok(CommandResult::OutputPlain(out)) => {
                        // 保留原始排版：不走 markdown，否则列对齐会被段落折行揉碎
                        self.push_tracked_message(ChatMessage::preformatted(out));
                    }
                    Ok(CommandResult::None) => {}
                    // `needs_background` 的契约只允许上面三种（见
                    // `Command::needs_background` 文档）。真返回了别的，提示而不是
                    // 静默丢弃——否则就是一个"命令跑了但什么都没发生"的幽灵。
                    Ok(other) => {
                        self.push_tracked_message(ChatMessage::assistant_err(wyj_i18n::tr_fmt(
                            "command.background_unsupported",
                            &[("name", &name), ("result", &format!("{other:?}"))],
                        )));
                    }
                    Err(e) => {
                        self.push_tracked_message(ChatMessage::assistant_err(format!(
                            "[/{name}] {e}"
                        )));
                    }
                }
            }

            AgentEvent::CheckpointCreated(result) => {
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                match result {
                    Ok(id) => {
                        self.save_needed = true;
                        self.push_tracked_message(ChatMessage::assistant(wyj_i18n::tr_fmt(
                            "checkpoint.created",
                            &[("id", &id)],
                        )));
                    }
                    Err(e) => {
                        self.push_tracked_message(ChatMessage::assistant_err(format!(
                            "[{}] {e}",
                            wyj_i18n::tr("checkpoint.failed_tag")
                        )));
                    }
                }
            }

            AgentEvent::RewindPrepared(result) => {
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                match result {
                    Ok(RewindPrepared::NeedsConfirm(text)) => {
                        self.push_tracked_message(ChatMessage::assistant(text));
                    }
                    Ok(RewindPrepared::Ready {
                        checkpoint_id,
                        restored_conversation,
                        protection_id,
                        file_text,
                        messages,
                        compact_count,
                        compact_generation,
                        turns,
                    }) => {
                        // live session 已由后台 task 写回，这里只重建展示层
                        if restored_conversation {
                            self.messages = reconstruct_display(&messages);
                            self.welcome_frozen = true;
                            self.frozen_up_to = 0;
                        }
                        self.compact_count = compact_count;
                        self.compact_generation = compact_generation;
                        self.turns = turns;
                        self.save_needed = true;
                        self.push_tracked_message(ChatMessage::assistant(format!(
                            "已回退到 {checkpoint_id}；回退前保护 checkpoint：{protection_id}{file_text}"
                        )));
                    }
                    Err(e) => {
                        self.push_tracked_message(ChatMessage::assistant_err(format!(
                            "[rewind 失败] {e}"
                        )));
                    }
                }
            }

            AgentEvent::BranchPrepared(result) => {
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                match result {
                    Ok(branch) => {
                        self.messages = reconstruct_display(&branch.messages);
                        self.compact_count = 0;
                        // 代际跟着分支历史走（不能归零，见 `BranchSwitch`）。
                        self.compact_generation = branch.compact_generation;
                        self.turns = branch.turns;
                        self.total_input_tokens = 0;
                        self.total_output_tokens = 0;
                        self.frozen_up_to = 0;
                        self.welcome_frozen = true;
                        self.save_needed = true;
                        // 主循环下一帧把自己的本地 session id 镜像同步过来
                        // （见 `apply_agent_event` 调用点之后的 re-sync）。
                        self.current_session_id = branch.session_id.clone();
                        self.push_tracked_message(ChatMessage::assistant(format!(
                            "已从 checkpoint {} 创建并切换到分支会话 {}",
                            branch.checkpoint_id, branch.session_id
                        )));
                    }
                    Err(e) => {
                        self.push_tracked_message(ChatMessage::assistant_err(format!(
                            "[branch 失败] {e}"
                        )));
                    }
                }
            }

            AgentEvent::EvolutionActionDone(result) => {
                self.is_thinking = false;
                self.current_op = None;
                self.current_tool_name = None;
                self.current_op_started = None;
                if let Some(dialog) = self.evolution_dialog.as_mut() {
                    dialog.confirm = None;
                    match result {
                        Ok(message) => {
                            dialog.reload();
                            dialog.error = Some(message);
                        }
                        Err(error) => dialog.error = Some(error),
                    }
                }
            }

            AgentEvent::Error(e) => {
                self.flush_streaming();
                self.stream_rate.reset();
                if let Some(start) = self.turn_start_time.take() {
                    self.last_turn_elapsed_secs = Some(start.elapsed().as_secs_f64());
                    self.last_turn_input_tokens = self
                        .total_input_tokens
                        .saturating_sub(self.turn_start_input_tokens);
                    self.last_turn_output_tokens = self
                        .total_output_tokens
                        .saturating_sub(self.turn_start_output_tokens);
                }
                self.is_thinking = false;
                self.reconcile_pending_queue();
                self.injector = None;
                self.push_tracked_message(ChatMessage::assistant_err(format!("[错误] {e}")));
                // 统一通知：主回合出错
                notify_emit(NotificationEvent::TurnError {
                    error: truncate_notify_body(&e),
                    session_id: if self.current_session_id.is_empty() {
                        None
                    } else {
                        Some(self.current_session_id.clone())
                    },
                });
            }

            AgentEvent::Injected => {
                // 只出队：聊天流里的回显在**提交那一刻**就画好了（见 Enter 分支）。
                // 这里再 push_user 一次就是同一条消息显示两遍。
                if !self.pending_queue.is_empty() {
                    self.pending_queue.remove(0);
                }
            }

            AgentEvent::Usage {
                input,
                output,
                compact_count,
                compact_generation,
                tool_schema_tokens,
                tool_schema_tokens_saved,
                latency,
            } => {
                self.session_latency = latency;
                self.total_input_tokens = input;
                self.total_output_tokens = output;
                self.compact_count = compact_count;
                self.compact_generation = compact_generation;
                self.tool_schema_tokens = tool_schema_tokens;
                self.tool_schema_tokens_saved = tool_schema_tokens_saved;
            }

            AgentEvent::UsageDelta {
                input_tokens,
                output_tokens,
            } => {
                // 逐请求累加成本轮实时用量（标题栏 `title_token_text` 消费）。
                // saturating_add：极端长会话下 u32 溢出在 debug 构建会 panic，
                // 而这里只是展示，饱和截断完全可接受。
                self.turn_live_input_tokens =
                    self.turn_live_input_tokens.saturating_add(input_tokens);
                self.turn_live_output_tokens =
                    self.turn_live_output_tokens.saturating_add(output_tokens);
                let active_ids: Vec<String> = self
                    .current_todos
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter(|t| t.status == TodoStatus::InProgress)
                    .map(|t| t.id.clone())
                    .collect();
                if !active_ids.is_empty() {
                    let in_shares = split_evenly(input_tokens, active_ids.len());
                    let out_shares = split_evenly(output_tokens, active_ids.len());
                    for (i, id) in active_ids.iter().enumerate() {
                        let s = self.todo_stats.entry(id.clone()).or_default();
                        s.input_tokens += in_shares[i];
                        s.output_tokens += out_shares[i];
                    }
                }
                // active_ids 为空：没有任务处于 in_progress，静默丢弃，不计入任何任务
            }

            AgentEvent::TodoUpdate(items) => {
                let old_all_done = self.current_todos.as_ref().is_some_and(|old| {
                    !old.is_empty() && old.iter().all(|t| t.status == TodoStatus::Completed)
                });
                let new_all_done =
                    !items.is_empty() && items.iter().all(|t| t.status == TodoStatus::Completed);
                let is_new_round = is_new_todo_round(self.current_todos.as_deref(), &items);
                if is_new_round || (new_all_done && !old_all_done) {
                    // 新一轮任务开始，或本轮刚全部完成：重置为默认折叠态
                    self.todo_panel_expanded = false;
                }
                if is_new_round {
                    self.todo_stats.clear();
                    self.todo_execution_logs.clear();
                }

                // 状态转换检测：新一轮时旧状态视为空表（所有任务都视为之前不是
                // in_progress），否则用更新前的 current_todos 按 id 查旧状态。
                let old_status: HashMap<&str, &TodoStatus> = if is_new_round {
                    HashMap::new()
                } else {
                    self.current_todos
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|t| (t.id.as_str(), &t.status))
                        .collect()
                };
                for item in &items {
                    let was_in_progress = old_status
                        .get(item.id.as_str())
                        .is_some_and(|s| **s == TodoStatus::InProgress);
                    let is_in_progress = item.status == TodoStatus::InProgress;
                    if is_in_progress && !was_in_progress {
                        self.todo_stats
                            .entry(item.id.clone())
                            .or_default()
                            .started_at = Some(Instant::now());
                    } else if !is_in_progress && was_in_progress {
                        if let Some(s) = self.todo_stats.get_mut(&item.id) {
                            if let Some(start) = s.started_at.take() {
                                s.elapsed_secs += start.elapsed().as_secs_f64();
                            }
                        }
                    }
                }

                self.current_todos = Some(items);
                self.ensure_selected_todo();
            }

            AgentEvent::AskQuestions {
                questions,
                response_tx,
            } => {
                // 面板打开后拦截全部按键，用户不再有任何滚动手段；若沿用此前
                // 上滚留下的视口位置，附加在聊天区尾部的选项区会被裁出可视范围。
                // 这里强制回到贴底跟随，保证选项与操作提示立即可见。
                self.chat_follow_tail = true;
                self.unseen_messages = false;
                self.ask_question_dialog = Some(AskQuestionDialog::new(questions, response_tx));
            }

            AgentEvent::BashResult {
                output,
                exit_code,
                elapsed_secs,
            } => {
                self.push_tracked_message(ChatMessage::bash_output(
                    output,
                    exit_code,
                    elapsed_secs,
                ));
            }

            AgentEvent::PlanApprovalRequest { plan, response_tx } => {
                // 注意：不要在这里把 is_thinking 设为 false。Agent 此刻仍在
                // `exit_plan_mode().await` 上挂起，TaskList / spinner 仍应继续
                // 表示"调研中"。若预先清掉 is_thinking，弹窗期间主循环的
                // `if state.is_thinking` 守卫会让 Ctrl+C 中断路径直接失效。
                // 批准分支会显式写回 true；取消分支走 interrupt() 自行恢复。
                // 计划正文作为普通消息并入应用内聊天流，
                // plan_dialog 只保留贴底的三选一选择器状态。
                self.push_tracked_message(ChatMessage::plan_proposal(plan));
                self.plan_dialog = Some(PlanApprovalDialog::new(response_tx));
            }

            AgentEvent::SubAgent(ev) => self.apply_sub_agent_event(ev),

            AgentEvent::ShellKilled(id) => {
                self.push_tracked_message(ChatMessage::system(wyj_i18n::tr_fmt(
                    "shells.kill_done",
                    &[("id", id.as_str())],
                )));
            }

            AgentEvent::TitleGenerated(title) => {
                // 后台标题生成完成 → 更新终端窗口标题（OSC 0）
                // 标题已由 SummaryGenerator 直接写盘，这里仅更新终端显示
                let _ = write!(io::stdout(), "\x1b]0;{title}\x07");
                let _ = io::stdout().flush();
            }

            AgentEvent::ModelsFetched {
                entry_idx,
                field_idx,
                result,
            } => {
                if let Some(dialog) = &mut self.profile_dialog {
                    let matches_pending = matches!(
                        dialog.overlay,
                        ProfileOverlay::FetchingModels {
                            entry_idx: e,
                            field_idx: f,
                        } if e == entry_idx && f == field_idx
                    );
                    if matches_pending {
                        dialog.overlay = ProfileOverlay::None;
                        // 拉取失败/为空时也要给出「手动输入」入口，否则 Model 字段
                        // 的 Enter 永远只会重试拉取，端点不支持 /models 的用户就
                        // 再也改不了模型名了。
                        let manual_only = || {
                            ActionMenu::new(
                                ProfileRow::Field(entry_idx, field_idx),
                                vec![ActionMenuItem {
                                    label: format!(
                                        "✎ {}",
                                        wyj_i18n::tr("profile.menu.manual_edit")
                                    ),
                                    action: ProfileMenuAction::Field(
                                        ProfileFieldAction::ManualEdit,
                                    ),
                                    dangerous: false,
                                    disabled: false,
                                    disabled_reason: None,
                                }],
                            )
                        };
                        match result {
                            Ok(models) if models.is_empty() => {
                                dialog.error = Some(wyj_i18n::tr("profile.fetch.empty"));
                                dialog.menu = Some(manual_only());
                            }
                            Ok(models) => {
                                // 首项固定「手动输入」：第三方的 /models 常常
                                // 不全或干脆不支持，用户必须随时能退回手填。
                                // 它的 action 不带下标（`ManualEdit`），不会与
                                // `pending_models` 的 0-based 下标冲突。
                                let mut items = vec![ActionMenuItem {
                                    label: format!(
                                        "✎ {}",
                                        wyj_i18n::tr("profile.menu.manual_edit")
                                    ),
                                    action: ProfileMenuAction::Field(
                                        ProfileFieldAction::ManualEdit,
                                    ),
                                    dangerous: false,
                                    disabled: false,
                                    disabled_reason: None,
                                }];
                                items.extend(models.iter().enumerate().map(|(i, name)| {
                                    ActionMenuItem {
                                        label: name.clone(),
                                        action: ProfileMenuAction::ModelChoice(i),
                                        dangerous: false,
                                        disabled: false,
                                        disabled_reason: None,
                                    }
                                }));
                                dialog.pending_models = models;
                                dialog.menu = Some(ActionMenu::new(
                                    ProfileRow::Field(entry_idx, field_idx),
                                    items,
                                ));
                            }
                            Err(e) => {
                                dialog.error =
                                    Some(wyj_i18n::tr_fmt("profile.fetch.failed", &[("err", &e)]));
                                dialog.menu = Some(manual_only());
                            }
                        }
                    }
                }
            }

            AgentEvent::ModelListFetched { result } => {
                if let Some(dialog) = &mut self.model_picker {
                    if dialog.loading {
                        dialog.loading = false;
                        match result {
                            Ok(mut models) => {
                                // 拉到的模型按名称排序：供应商返回顺序不保证，
                                // 用户在列表里是按名字找模型的。
                                models.sort();
                                dialog.models = models;
                                dialog.selected = 0;
                                dialog.error = None;
                            }
                            Err(e) => {
                                // 拉不到不是死路：「手动输入」那一项始终在，
                                // 端点不实现 /models 的用户照样能切模型。
                                dialog.error =
                                    Some(wyj_i18n::tr_fmt("model.picker.failed", &[("err", &e)]));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpRegistryFetched { result } => {
                if let Some(dialog) = &mut self.mcp_dialog {
                    if matches!(dialog.overlay, McpOverlay::Searching) {
                        dialog.overlay = McpOverlay::None;
                        match result {
                            Ok(results) => {
                                dialog.status = if results.is_empty() {
                                    Some(wyj_i18n::tr("mcp.dialog.no_results"))
                                } else {
                                    None
                                };
                                dialog.browse_results = results;
                                // rows() 里 Browse tab 的搜索行固定占位置 0，
                                // 第一条真实结果落在位置 1；没有结果时留在搜索行。
                                dialog.cursor = if dialog.browse_results.is_empty() {
                                    0
                                } else {
                                    1
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "mcp.error.search_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::SkillMarketplaceSynced {
                marketplace_id,
                git_url,
                result,
            } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    let matches_pending = matches!(
                        &dialog.overlay,
                        SkillsOverlay::Syncing { marketplace_id: id, .. } if *id == marketplace_id
                    );
                    if matches_pending {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok(entries) => {
                                dialog.active_marketplace_id = marketplace_id;
                                dialog.active_marketplace_git_url = git_url;
                                dialog.browse_results = entries;
                                // 这批结果是 git marketplace 来的，必须记下来源，
                                // 否则后面安装时会按注册表路径去下载 zip。
                                dialog.browse_source = SkillsBrowseSource::Marketplace;
                                dialog.tab = SkillsDialogTab::Browse;
                                // rows() 里位置 0 是搜索行，第一条真实结果在 1。
                                dialog.cursor = if dialog.browse_results.is_empty() {
                                    0
                                } else {
                                    1
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.sync_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::SkillRegistrySearched { result } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    // 只接受当前仍在等待的那次搜索的结果，避免连按回车时旧响应
                    // 覆盖新结果。
                    if matches!(dialog.overlay, SkillsOverlay::Searching) {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok(results) => {
                                dialog.status = if results.is_empty() {
                                    Some(wyj_i18n::tr("skills.dialog.no_results"))
                                } else {
                                    None
                                };
                                dialog.registry_results = results;
                                dialog.browse_results = Vec::new();
                                dialog.browse_source = SkillsBrowseSource::Registry;
                                dialog.tab = SkillsDialogTab::Browse;
                                // 位置 0 是搜索行；有结果时停在第一条上。
                                dialog.cursor = if dialog.registry_results.is_empty() {
                                    0
                                } else {
                                    1
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.registry_search_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::SkillRegistryArchiveFetched { result } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    if matches!(dialog.overlay, SkillsOverlay::FetchingArchive) {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok((preview, archive)) => {
                                // 回填发起下载时选中的那条技能；期间用户可能已经
                                // 移动过光标，所以按 slug 回查而不是信任当前 cursor。
                                let Some(idx) = dialog
                                    .registry_results
                                    .iter()
                                    .position(|s| s.slug == dialog.pending_registry_slug.clone())
                                else {
                                    return;
                                };
                                let Some(skill) = dialog.registry_results.get(idx).cloned() else {
                                    return;
                                };
                                dialog.overlay = SkillsOverlay::RegistryInstallConfirm {
                                    skill: Box::new(skill),
                                    preview,
                                    archive,
                                    scope: wyj_store::InstallScope::Global,
                                };
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.registry_download_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpUpgraded { row_idx, result } => {
                let mut upgraded = false;
                if let Some(dialog) = &mut self.mcp_dialog {
                    let matches_pending = matches!(dialog.overlay, McpOverlay::Upgrading { row_idx: r } if r == row_idx);
                    if matches_pending {
                        dialog.overlay = McpOverlay::None;
                        match result {
                            Ok(wyj_store::UpgradeOutcome::Upgraded { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "mcp.upgrade.done",
                                    &[("version", &version)],
                                ));
                                upgraded = true;
                            }
                            Ok(wyj_store::UpgradeOutcome::AlreadyLatest { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "mcp.upgrade.already_latest",
                                    &[("version", &version)],
                                ));
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "mcp.error.upgrade_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
                if upgraded {
                    // 见 reload_config_into_state 文档：Global scope 的 upgrade 同样
                    // 直接改磁盘 config.toml，不刷新这份内存快照的话，面板会一直显示
                    // 升级前的版本号，看起来像“升级了但没生效”。
                    reload_config_into_state(self);
                    if let Some(dialog) = &mut self.mcp_dialog {
                        dialog.refresh_installed(&self.config, &self.cwd);
                    }
                }
            }

            AgentEvent::SkillUpgraded { row_idx, result } => {
                if let Some(dialog) = &mut self.skills_dialog {
                    let matches_pending = matches!(
                        dialog.overlay,
                        SkillsOverlay::Upgrading { row_idx: r } if r == row_idx
                    );
                    if matches_pending {
                        dialog.overlay = SkillsOverlay::None;
                        match result {
                            Ok(wyj_store::UpgradeOutcome::Upgraded { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "skills.upgrade.done",
                                    &[("version", &version)],
                                ));
                                let home = wyj_config::home_dir().unwrap_or_default();
                                dialog.refresh_installed(&home, &self.cwd);
                            }
                            Ok(wyj_store::UpgradeOutcome::AlreadyLatest { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "skills.upgrade.already_latest",
                                    &[("version", &version)],
                                ));
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "skills.error.upgrade_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::McpBackgroundConnected { name, tool_count } => {
                self.mcp_connection_status
                    .insert(name.clone(), McpConnStatus::Connected { tool_count });
                self.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                    "mcp.background.connected",
                    &[("name", &name), ("count", &tool_count.to_string())],
                )));
            }

            AgentEvent::McpBackgroundFailed { name, reason } => {
                let status = match &reason {
                    crate::event::McpConnFailReason::Error(_) => McpConnStatus::Failed,
                    crate::event::McpConnFailReason::Timeout => McpConnStatus::TimedOut,
                };
                self.mcp_connection_status.insert(name.clone(), status);
                let key = match &reason {
                    crate::event::McpConnFailReason::Error(_) => "mcp.background.failed_error",
                    crate::event::McpConnFailReason::Timeout => "mcp.background.failed_timeout",
                };
                let err_text = match &reason {
                    crate::event::McpConnFailReason::Error(e) => e.clone(),
                    crate::event::McpConnFailReason::Timeout => String::new(),
                };
                let mut msg = ChatMessage::system(wyj_i18n::tr_fmt(
                    key,
                    &[("name", &name), ("err", &err_text)],
                ));
                msg.is_error = true;
                self.messages.push(msg);
            }

            AgentEvent::PluginMarketplaceSynced {
                marketplace_id,
                result,
            } => {
                if let Some(dialog) = &mut self.plugins_dialog {
                    let matches_pending = matches!(
                        &dialog.overlay,
                        PluginOverlay::Syncing { marketplace_id: id } if *id == marketplace_id
                    );
                    if matches_pending {
                        dialog.overlay = PluginOverlay::None;
                        match result {
                            Ok(manifest) => {
                                dialog.active_marketplace_id = marketplace_id.clone();
                                dialog.active_marketplace_location = dialog
                                    .marketplaces
                                    .iter()
                                    .find(|m| m.id == marketplace_id)
                                    .map(|m| m.location.clone())
                                    .unwrap_or_default();
                                dialog.browse_results = manifest.plugins;
                                dialog.tab = PluginsDialogTab::Browse;
                                dialog.cursor = 0;
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "plugins.error.sync_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                        // sync 成功会更新 marketplace 的 display_name/owner_name/plugin_count，
                        // 重新读盘刷新一下列表展示。
                        dialog.marketplaces = wyj_store::plugin_install::list_plugin_marketplaces()
                            .unwrap_or_default();
                    }
                }
            }

            AgentEvent::PluginInstalled { result } => {
                if let Some(dialog) = &mut self.plugins_dialog {
                    if matches!(dialog.overlay, PluginOverlay::Installing) {
                        match result {
                            Ok(report) => {
                                dialog.refresh_installed(&self.cwd);
                                dialog.overlay = PluginOverlay::InstallReport { report };
                            }
                            Err(e) => {
                                dialog.overlay = PluginOverlay::None;
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "plugins.error.install_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }

            AgentEvent::PluginUpgraded { row_idx, result } => {
                if let Some(dialog) = &mut self.plugins_dialog {
                    let matches_pending = matches!(
                        dialog.overlay,
                        PluginOverlay::Upgrading { row_idx: r } if r == row_idx
                    );
                    if matches_pending {
                        dialog.overlay = PluginOverlay::None;
                        match result {
                            Ok(wyj_store::UpgradeOutcome::Upgraded { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "plugins.upgrade.done",
                                    &[("version", &version)],
                                ));
                                dialog.refresh_installed(&self.cwd);
                            }
                            Ok(wyj_store::UpgradeOutcome::AlreadyLatest { version }) => {
                                dialog.status = Some(wyj_i18n::tr_fmt(
                                    "plugins.upgrade.already_latest",
                                    &[("version", &version)],
                                ));
                            }
                            Err(e) => {
                                dialog.error = Some(wyj_i18n::tr_fmt(
                                    "plugins.error.upgrade_failed",
                                    &[("err", &e)],
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    /// 处理子 Agent 生命周期事件（更新 sub_agents 状态、绑定消息、路由后台结果）
    pub(super) fn apply_sub_agent_event(&mut self, ev: wyj_tools::SubAgentEvent) {
        use wyj_tools::SubAgentEvent as E;
        match ev {
            E::Started {
                id,
                agent_type,
                description,
                background,
                parent_id: _,
                // 落盘 trace 关联用，TUI 内存态摘要不需要
                parent_tool_use_id,
            } => {
                self.trace_push(
                    id,
                    TraceEvent::Started {
                        agent_type: agent_type.clone(),
                        description: description.clone(),
                        background,
                        parent_tool_use_id,
                    },
                );
                // FIFO 绑定最早一条未绑定的 Agent ToolCall 消息，并把内容改写为 类型(描述)
                if let Some(msg) = self.messages.iter_mut().find(|m| {
                    matches!(m.role, MessageRole::ToolCall)
                        && m.tool_name.as_deref() == Some("Agent")
                        && m.sub_agent_id.is_none()
                }) {
                    msg.sub_agent_id = Some(id);
                    msg.content = format!("{agent_type}({description})");
                }
                self.sub_agents.insert(
                    id,
                    SubAgentUiState {
                        agent_type,
                        description,
                        background,
                        status: SubAgentStatus::Running,
                        started_at: Instant::now(),
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
                    },
                );
            }
            E::ToolStart {
                id,
                tool_name,
                arg_summary,
                // 完整 input 只落盘（trace.rs），TUI 内存态摘要不保留全文（见
                // AGENTS.md/plan：避免长会话下常驻全文内存暴涨）
                input,
            } => {
                let (input_json, truncated) = wyj_tools::trace::truncate_input(&input);
                self.trace_push(
                    id,
                    TraceEvent::ToolStart {
                        tool_name: tool_name.clone(),
                        input_json,
                        truncated,
                    },
                );
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.tool_calls += 1;
                    let short_name = tool_display_name(&tool_name);
                    let current = if arg_summary.is_empty() {
                        short_name
                    } else {
                        format!("{short_name}({arg_summary})")
                    };
                    // 与主聊天流同一口径：子 Agent 里的 Grep 也标出后端
                    s.current_tool = Some(format!("{current}{}", tool_backend_badge(&tool_name)));
                    s.tool_log.push(SubToolLine {
                        // 存原始注册名：ToolEnd 靠它配对，渲染层再用
                        // `tool_display_name` 折成可读名（MCP → `server:tool`）。
                        tool_name,
                        arg_summary,
                        is_error: false,
                        elapsed_secs: None,
                    });
                }
            }
            E::ToolEnd {
                id,
                tool_name,
                is_error,
                elapsed_secs,
                // 完整 output 只落盘（trace.rs），TUI 内存态摘要不保留全文
                output,
            } => {
                let (output, truncated) = wyj_tools::trace::truncate_output(&output);
                self.trace_push(
                    id,
                    TraceEvent::ToolEnd {
                        tool_name: tool_name.clone(),
                        is_error,
                        elapsed_secs,
                        output,
                        truncated,
                    },
                );
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.current_tool = None;
                    if let Some(line) = s
                        .tool_log
                        .iter_mut()
                        .rev()
                        .find(|l| l.elapsed_secs.is_none() && l.tool_name == tool_name)
                    {
                        line.is_error = is_error;
                        line.elapsed_secs = Some(elapsed_secs);
                    }
                }
            }
            E::Usage {
                id,
                input_tokens,
                output_tokens,
            } => {
                self.trace_push(
                    id,
                    TraceEvent::Usage {
                        input_tokens,
                        output_tokens,
                    },
                );
                self.sub_input_tokens += input_tokens;
                self.sub_output_tokens += output_tokens;
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.input_tokens += input_tokens;
                    s.output_tokens += output_tokens;
                }
            }
            E::Assistant { id, text } => {
                // 只进内存态 trace 缓存（详情面板用），不驱动聊天区：
                // 聊天区只展示子 Agent 的最终结果，中间落点属于详情。
                self.trace_push(
                    id,
                    TraceEvent::Assistant {
                        text: wyj_tools::textutil::truncate_head_tail(
                            &text,
                            wyj_tools::trace::ASSISTANT_HEAD_BYTES,
                            0,
                        ),
                    },
                );
            }
            E::Control {
                id,
                action,
                accepted,
            } => {
                self.trace_push(
                    id,
                    TraceEvent::Control {
                        action: action.clone(),
                        accepted,
                    },
                );
                if accepted && action == "interrupt" {
                    if let Some(agent) = self.sub_agents.get_mut(&id) {
                        agent.status = SubAgentStatus::Interrupted;
                        agent.current_tool = None;
                        agent.finished_at = Some(Instant::now());
                    }
                    // 被中断的条目从面板列表消失，选中态要重新对齐
                    self.reconcile_sub_agent_selection();
                }
            }
            E::Done {
                id,
                agent_type,
                description,
                result,
                is_error,
                elapsed_secs,
                background,
                handoff_body,
            } => {
                // 会话归属过滤：`reset_for_new_session`（`/new`、`/clear`、resume
                // 切会话）会 `sub_agents.clear()`，所以「这个 id 不在表里」= 它
                // 属于**上一段会话**。此时既不能注入 reminder、也不能弹通知——
                // 它们会落到 `pending_bg_reminders`，而主循环的自动唤醒判据
                // （`app.rs` 的 `!is_thinking && !pending_bg_reminders.is_empty()`）
                // 只看「有没有待注入内容」，不认来源，于是新会话第一帧就起一轮
                // 模型请求。这正是「/new 之后 AI 还在继续调用」的形状：旧会话的
                // 后台子 Agent 跑完了，reminder 顺着新会话的自动唤醒灌了进去。
                //
                // 放在事件入口而不是注入前：SubAgentHub 的 `event_cb` 是全局的，
                // 通道里可能积压着 reset 之前发出的 Done，逐条丢弃最省事。
                // 未来新增后台生产者只要走 `sub_agents` 登记，就自动被这条覆盖。
                if !self.sub_agents.contains_key(&id) {
                    return;
                }
                self.trace_push(
                    id,
                    TraceEvent::Done {
                        result: result.clone(),
                        is_error,
                        elapsed_secs,
                    },
                );
                if let Some(s) = self.sub_agents.get_mut(&id) {
                    s.status = if is_error {
                        SubAgentStatus::Failed
                    } else {
                        SubAgentStatus::Done
                    };
                    s.final_elapsed = Some(elapsed_secs);
                    s.current_tool = None;
                    s.finished_at = Some(Instant::now());
                    s.final_result = Some(result.clone());
                    // 交接物从盘上读回：Done 事件只带模型侧外壳，人读的摘要
                    // （状态/结论/改了哪些文件）在 a<id>.handoff.json 里。
                    s.handoff = self.sessions_dir.as_ref().and_then(|dir| {
                        wyj_tools::handoff::read_handoff(dir, &self.current_session_id, id)
                    });
                }
                // 完成即从面板列表消失，选中态（若正指向它）要重新对齐到
                // 仍在运行的条目；一个都不剩时收起面板并把焦点交还输入框。
                self.reconcile_sub_agent_selection();
                if background {
                    // 统一通知：后台子 Agent 完成（前台调用不通知，避免和主回合
                    // TurnFinished 在同一面板里重复提示）
                    let summary = format!("{agent_type} ({:.1}s)", elapsed_secs);
                    notify_emit(NotificationEvent::SubAgentDone {
                        agent_id: format!("a{id}"),
                        summary: truncate_notify_body(&summary),
                    });
                    // 结果包成 system-reminder：主 Agent 忙则经注入通道在工具边界
                    // 送达；空闲则暂存，下一轮起手合并进 user 消息。
                    // `handoff_body` 为 Some（未完成）时用它替换普通报告外壳，
                    // 保证后台与前台两条路径给模型的交接措辞逐字相同。
                    let reminder = wyj_core::prompts::bg_agent_reminder_with(
                        &format!("a{id}"),
                        &agent_type,
                        &description,
                        &format_hms(elapsed_secs),
                        &result,
                        handoff_body.as_deref(),
                    );
                    match &self.injector {
                        Some(queue) => {
                            queue.push(
                                vec![ContentBlock::Text { text: reminder }],
                                InjectionKind::SystemReminder,
                            );
                        }
                        None => self.pending_bg_reminders.push(reminder),
                    }
                    self.messages.push(ChatMessage::system(wyj_i18n::tr_fmt(
                        "subagent.bg_done_notice",
                        &[
                            ("id", format!("a{id}").as_str()),
                            ("type", &agent_type),
                            ("desc", &description),
                        ],
                    )));
                }
            }
        }
    }
}
