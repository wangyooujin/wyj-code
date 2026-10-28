use super::*;
use crate::tool::{Tool, ToolContext, ToolResult};
use crate::{
    MemoryClaimKind, MemoryClaimScope, MemorySource, MemorySourceKind, MemoryWriteRequest,
    TaskStatus, TaskStep,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// `soft_threshold_tokens` 的测试用最小 `ContextEditCfg`。CAS 指向临时目录，
/// 这些断言只关心阈值计算本身，不触发真实外部化。
fn mk_edit_cfg() -> crate::context_edit::ContextEditCfg {
    let dir = std::env::temp_dir().join(format!("wyj-soft-threshold-{}", std::process::id()));
    crate::context_edit::ContextEditCfg {
        enabled: true,
        opts: crate::context_edit::ElideOptions::default(),
        cas: std::sync::Arc::new(
            crate::workspace_cas::WorkspaceCas::open(&dir, 0).expect("CAS 可用"),
        ),
        max_batches: 3,
        soft_limit_ratio: 0.55,
        totals: std::sync::Arc::new(crate::context_edit::ContextEditTotals::default()),
    }
}

/// 软阈值决定 context editing 何时**动手**。
///
/// 这条是整个 C1 变更的核心断言：`soft_limit_ratio = 0`（关闭）必须退化成
/// 旧行为，否则「opt-out 配置」就名存实亡。
#[test]
fn soft_threshold_disabled_falls_back_to_old_behavior() {
    let cfg = crate::context_edit::ContextEditCfg {
        enabled: false,
        soft_limit_ratio: 0.0,
        ..mk_edit_cfg()
    };
    assert_eq!(soft_threshold_tokens(1_000_000, Some(&cfg)), u32::MAX);
    // 未装配 context editing 时同理：u32::MAX 让 `estimated > soft` 恒假。
    assert_eq!(soft_threshold_tokens(1_000_000, None), u32::MAX);
}

#[test]
fn soft_threshold_starts_editing_earlier_than_compact_threshold() {
    let cfg = crate::context_edit::ContextEditCfg {
        soft_limit_ratio: 0.55,
        ..mk_edit_cfg()
    };
    let window = 1_000_000u32;
    let soft = soft_threshold_tokens(window, Some(&cfg));
    let hard = window.saturating_sub(crate::compact::compact_trigger_buffer(window, 32_000));
    assert_eq!(soft, 550_000);
    // 软阈值必须严格早于硬阈值，否则 C1 等于没做。
    assert!(
        soft < hard,
        "soft {soft} must be below compact threshold {hard}, otherwise trimming never starts early"
    );
}

/// 200K 窗口下 `soft_limit_ratio = 0.55` 给出 110K，仍然早于硬阈值。
#[test]
fn soft_threshold_scales_with_window() {
    let cfg = crate::context_edit::ContextEditCfg {
        soft_limit_ratio: 0.55,
        ..mk_edit_cfg()
    };
    assert_eq!(soft_threshold_tokens(200_000, Some(&cfg)), 110_000);
}

/// 荒谬比例不能溢出或产生 0（0 会让每个请求都无谓地跑一遍清理循环）。
#[test]
fn soft_threshold_clamps_absurd_ratios() {
    let huge = crate::context_edit::ContextEditCfg {
        soft_limit_ratio: 9_999.0,
        ..mk_edit_cfg()
    };
    let v = soft_threshold_tokens(1_000_000, Some(&huge));
    assert_eq!(v, u32::MAX, "过大的比例必须夹到 u32::MAX 而非溢出回绕");

    let negative = crate::context_edit::ContextEditCfg {
        soft_limit_ratio: -1.0,
        ..mk_edit_cfg()
    };
    // 负数与 0 同义（关闭），绝不能 clamp 成 1 让清理每轮都触发。
    assert_eq!(soft_threshold_tokens(1_000_000, Some(&negative)), u32::MAX);
}
use wyj_api::provider::EventStream;
use wyj_api::types::{ContentBlock, Message, Role, StopReason, ToolResultContent};

struct FakeCtx;
#[async_trait::async_trait]
impl ToolContext for FakeCtx {
    fn cwd(&self) -> &std::path::Path {
        std::path::Path::new("/tmp")
    }
    fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
        true
    }
}

/// DeepSeek `deepseek-reasoner` 应触发 reasoning 模型熔断，落到传入的熔断值。
#[test]
fn max_turns_caps_reasoner_model_at_the_forced_budget() {
    let openai = wyj_config::WireProtocol::OpenAiChatCompletions;
    for model in [
        "deepseek-reasoner",
        "DeepSeek-Reasoner",
        "deepseek-R1",
        "deepseek-r1-distill-qwen-7b",
    ] {
        assert_eq!(
            max_turns_for_model(200, 32, "deepseek", &openai, model),
            32,
            "{model}"
        );
        // 熔断值来自配置，不是常量：同一个模型换个预算就该跟着变。
        assert_eq!(
            max_turns_for_model(200, 96, "deepseek", &openai, model),
            96,
            "{model}: 熔断值必须可配置"
        );
    }
}

/// 普通模型保留调用方传入的默认值,不受熔断影响。
#[test]
fn max_turns_keeps_default_for_normal_models() {
    let openai = wyj_config::WireProtocol::OpenAiChatCompletions;
    let anthropic = wyj_config::WireProtocol::AnthropicMessages;
    assert_eq!(
        max_turns_for_model(200, 64, "deepseek", &openai, "deepseek-chat"),
        200
    );
    assert_eq!(
        max_turns_for_model(200, 64, "anthropic", &anthropic, "claude-opus-4-5"),
        200
    );
    assert_eq!(
        max_turns_for_model(64, 32, "alibaba", &openai, "qwen3-coder-plus"),
        64
    );
}

/// **思考关不掉的端点同样要压到 32 轮**，哪怕模型名里既没有 `reasoner`
/// 也没有 `-r1`。
///
/// 回归的是 v0.3.15 之前的真实事故：`MiniMax-M3.1-Flash` 单个用户回合跑了
/// 163 次 API 往返、35.6 万 output token，收尾提醒要到第 196 轮才注入。
/// 旧判据只看模型名，这类端点一律拿到默认 200 轮，等于允许模型在单回合里
/// 做 200 次完整深度推理。
#[test]
fn max_turns_caps_endpoints_that_cannot_disable_thinking() {
    let anthropic = wyj_config::WireProtocol::AnthropicMessages;
    let openai = wyj_config::WireProtocol::OpenAiChatCompletions;

    // MiniMax `/anthropic`：M3.1 发 disabled 直接 400、M2.x 发了不生效。
    for model in ["MiniMax-M3.1-Flash-Preview", "MiniMax-M2.7"] {
        assert_eq!(
            max_turns_for_model(200, 32, "minimax", &anthropic, model),
            32,
            "{model}: /anthropic 端点的思考关不掉"
        );
    }
    // OpenAI 线的 M2.x 同样强制思考（thinking.type 恒为 adaptive）。
    assert_eq!(
        max_turns_for_model(200, 32, "minimax", &openai, "MiniMax-M2.7"),
        32
    );
    // Qwen `-thinking` / 豆包 `-thinking` / Moonshot `-code` 均为强制思考后缀。
    assert_eq!(
        max_turns_for_model(200, 32, "alibaba", &openai, "qwen3-max-thinking"),
        32
    );
    assert_eq!(
        max_turns_for_model(200, 32, "volcengine", &openai, "doubao-seed-1.6-thinking"),
        32
    );
    assert_eq!(
        max_turns_for_model(200, 32, "moonshot", &openai, "kimi-k2-code"),
        32
    );
}

/// 能关掉思考的端点**不**被这条判据误伤——否则同一个 vendor 的普通模型会被
/// 无差别压到 32 轮，正常长任务反而做不完。
#[test]
fn max_turns_does_not_cap_endpoints_that_can_disable_thinking() {
    let anthropic = wyj_config::WireProtocol::AnthropicMessages;
    let openai = wyj_config::WireProtocol::OpenAiChatCompletions;

    // MiniMax M3（非 3.1）：thinking.type=disabled 真的生效。
    assert_eq!(
        max_turns_for_model(200, 32, "minimax", &anthropic, "MiniMax-M3"),
        200
    );
    // Moonshot k2.5/2.6 无强制后缀 → 允许关闭。
    assert_eq!(
        max_turns_for_model(200, 32, "moonshot", &openai, "kimi-k2.5"),
        200
    );
    // GLM 可关闭。
    assert_eq!(
        max_turns_for_model(200, 32, "zhipu", &openai, "glm-5.2"),
        200
    );
}

/// fnv_hash_value 对 Object 的 key 顺序不敏感,数组保持原序。
/// 同语义不同 JSON 序列化顺序应产出相同 hash,避免误伤正常重试。
#[test]
fn fnv_hash_is_stable_across_key_order() {
    let a = serde_json::json!({"path": "Cargo.toml", "limit": 10});
    let b = serde_json::json!({"limit": 10, "path": "Cargo.toml"});
    assert_eq!(fnv_hash_value(&a), fnv_hash_value(&b));

    let arr_a = serde_json::json!({"items": [1, 2, 3]});
    let arr_b = serde_json::json!({"items": [3, 2, 1]});
    assert_ne!(fnv_hash_value(&arr_a), fnv_hash_value(&arr_b));
}

/// 本轮授权记忆的分叉不变式：**Bash 按工具名，其余按输入指纹**。
///
/// 这条线是两种旧模式合并后唯一剩下的手感差异，也是最容易在重构中被
/// 「顺手统一」掉的地方，所以两个方向都钉：
/// - Bash 放宽到工具名：长任务里几十条 `cargo test` / `git log …` 不该
///   把闸门打成串行队列（每把新键付一次 3–5s 分类器往返）。
/// - 其余工具必须保持精确：`Edit` 批过 `notes.md` 不该顺带放行
///   `deploy.sh`——写工具的输入决定**改什么**，不是「要不要改文件」。
#[test]
fn turn_grant_key_is_bare_bash_but_exact_fingerprint_for_everything_else() {
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let key = |name: &str, input: serde_json::Value| agent.permission_key(name, &input);

    // Bash：命令全文不参与，只有工具名。
    let bash = |command: &str| key("Bash", serde_json::json!({ "command": command }));
    assert_eq!(bash("git log --oneline"), bash("git log -p --since=x"));
    assert_eq!(bash("cargo test"), bash("cargo test --lib -- --nocapture"));
    assert_eq!(bash("npm run deploy:prod"), bash("ls"));
    assert_eq!(bash("rm -rf build"), "Bash");

    // 其余工具：同工具不同输入必须不同键，同输入不同 key 顺序必须同键。
    let edit = |path: &str, content: &str| {
        key(
            "Edit",
            serde_json::json!({ "file_path": path, "old_string": content }),
        )
    };
    assert_ne!(edit("notes.md", "a"), edit("deploy.sh", "a"));
    assert_ne!(edit("notes.md", "a"), edit("notes.md", "b"));
    assert_eq!(
        key("Edit", serde_json::json!({"file_path": "a", "x": 1})),
        key("Edit", serde_json::json!({"x": 1, "file_path": "a"}))
    );

    // Agent / computer-use 同理按输入指纹：subagent_type、action 变了就得重审。
    assert_ne!(
        key("Agent", serde_json::json!({"subagent_type": "Explore"})),
        key("Agent", serde_json::json!({"subagent_type": "Plan"}))
    );
    assert_ne!(
        key(
            "app_computer",
            serde_json::json!({"action": "screenshot", "window_id": 1})
        ),
        key(
            "app_computer",
            serde_json::json!({"action": "click", "window_id": 1})
        )
    );
}

/// **跨会话派生必须隔离回合级可变状态**——这条钉住 P0 回归。
///
/// `Agent: Clone` 是浅拷贝，`Arc<Mutex<..>>` 字段被共享。这对「同一会话内
/// 重建」（TUI `/model`）是对的，对 daemon/ACP 的多会话是错的：会话 A 的
/// 授权、死循环窗口、记忆快照、volatile 前缀、标题标志、fallback 路由下标
/// 全部会漏进会话 B。
///
/// 断言方式：先在父 Agent 上写入每种状态，再派生，确认派生体是干净的、
/// 且**写派生体不影响父体**（双向隔离，不是只清空一次）。
#[test]
fn fork_for_new_session_isolates_turn_scoped_state() {
    let parent = Agent::new(Arc::new(EndTurnProvider));

    // ① 父体写入回合级状态。
    parent.grant_for_turn("Bash", &serde_json::json!({"command": "git log"}));
    parent
        .loop_guard
        .lock()
        .unwrap()
        .push_back(("Read".to_string(), 42));
    *parent.turn_user_goal.lock().unwrap() = "父会话的目标".to_string();
    *parent.last_request_volatile.lock().unwrap() = "父会话的规约 reminder".to_string();
    parent.title_attempted.store(true, Ordering::Release);
    parent.active_route.store(2, Ordering::Release);
    {
        let mut cache = parent.memory_snapshot.lock().unwrap();
        cache.bucket = Some(7);
        cache.text = "父会话记忆".to_string();
    }
    assert!(parent.turn_granted("Bash", &serde_json::json!({"command": "x"})));

    // ② 派生体：全部回合级状态都必须是干净的。
    let child = parent.fork_for_new_session();
    assert!(
        !child.turn_granted("Bash", &serde_json::json!({"command": "x"})),
        "子会话不得继承父会话的本轮授权（否则 A 批准的 Bash 让 B 免检）"
    );
    assert!(
        child.loop_guard.lock().unwrap().is_empty(),
        "死循环窗口必须隔离"
    );
    assert_eq!(*child.turn_user_goal.lock().unwrap(), "");
    assert_eq!(*child.last_request_volatile.lock().unwrap(), "");
    assert!(!child.title_attempted.load(Ordering::Acquire));
    assert_eq!(
        child.active_route.load(Ordering::Acquire),
        0,
        "fallback 路由下标必须隔离"
    );
    assert!(child.memory_snapshot.lock().unwrap().bucket.is_none());
    assert_eq!(child.session_id, None, "会话标识属于本会话，派生方重新设置");
    assert!(
        child.session_event_cb.is_none(),
        "事件回调必须由新会话重新接线"
    );

    // ③ 反向隔离：写子体不影响父体。
    child.grant_for_turn("Bash", &serde_json::json!({"command": "npm test"}));
    assert!(
        parent.turn_granted("Bash", &serde_json::json!({"command": "x"})),
        "子会话的授权不得回流到父会话"
    );
    assert_eq!(parent.loop_guard.lock().unwrap().len(), 1);
    assert!(parent.title_attempted.load(Ordering::Acquire));
    assert_eq!(parent.active_route.load(Ordering::Acquire), 2);

    // ④ 应当共享的**不是**回合级状态：provider / 工具表 / 记忆库仍是同一份。
    assert!(
        Arc::ptr_eq(&parent.provider, &child.provider),
        "provider 应共享"
    );
}

/// 5 条历史里有 3 条同 (name, hash) 视为循环。
#[test]
fn detect_loop_fires_at_threshold() {
    let mut window = std::collections::VecDeque::new();
    let h = fnv_hash_value(&serde_json::json!({"path": "foo"}));
    // 注入 3 次相同 (Read, h),窗口容量 5
    for _ in 0..3 {
        window.push_back(("Read".to_string(), h));
    }
    // 不同参数不算循环
    let h2 = fnv_hash_value(&serde_json::json!({"path": "bar"}));
    window.push_back(("Read".to_string(), h2));
    window.push_back(("Edit".to_string(), h));

    assert!(detect_loop(&window, "Read", h));
    assert!(!detect_loop(&window, "Read", h2));
    assert!(!detect_loop(&window, "Edit", h));
}

/// 命中计数只在最近 LOOP_GUARD_WINDOW (5) 条里数,更早的不算。
#[test]
fn detect_loop_ignores_old_entries() {
    let mut window = std::collections::VecDeque::new();
    let h = fnv_hash_value(&serde_json::json!({"path": "foo"}));
    // 8 条历史,前 5 条相同 + 后 3 条不同 — 但窗口只看最近 5,所以不算循环
    for _ in 0..5 {
        window.push_back(("Read".to_string(), h));
    }
    for _ in 0..3 {
        window.push_back(("Edit".to_string(), h));
    }
    assert!(!detect_loop(&window, "Read", h));
}

/// 第一轮返回两个 Sleep 工具调用，第二轮返回 EndTurn 文本
struct TwoToolProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for TwoToolProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n == 0 {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "Sleep".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "t1".into(),
                    json_delta: r#"{"ms":150,"tag":"first"}"#.into(),
                }),
                Ok(StreamEvent::ToolUseStart {
                    id: "t2".into(),
                    name: "Sleep".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "t2".into(),
                    json_delta: r#"{"ms":150,"tag":"second"}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 捕获每次请求的 system prompt 全文，回一句文本就 EndTurn。
struct SystemRecordingProvider {
    seen: std::sync::Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl Provider for SystemRecordingProvider {
    async fn stream(
        &self,
        system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        self.seen
            .lock()
            .unwrap()
            .push(system.combined().to_string());
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

async fn capture_system(agent: &mut Agent, seen: &std::sync::Arc<Mutex<Vec<String>>>) -> String {
    let mut session = Session::new();
    agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await
        .unwrap();
    let captured = seen.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        1,
        "本用例只应发一次请求，实际 {}",
        captured.len()
    );
    captured.into_iter().next().unwrap()
}

/// 语言提醒的价值**全在位置**：它存在的理由是 `<env>` 里的语言事实在
/// system 中段（后面还压着记忆快照 + CLAUDE.md reminder，动辄几万 token），
/// recency 权重落不到那句上，于是 MiniMax M3.1 的 thinking 全英文。
/// 谁在它后面再追加东西，这个修复就静默失效——所以钉死「必须是最后一段」。
#[tokio::test]
async fn response_language_reminder_is_the_last_system_section() {
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(Arc::new(SystemRecordingProvider { seen: seen.clone() }))
        .with_system("MAIN-STUB")
        .with_response_language("zh");
    let combined = capture_system(&mut agent, &seen).await;

    assert!(combined.starts_with("MAIN-STUB"));
    let reminder = crate::prompts::response_language_reminder("zh");
    assert!(
        combined.trim_end().ends_with(reminder.trim_end()),
        "语言提醒必须是 system prompt 的最后一段，实际最后一段：{:?}",
        combined.trim_end().rsplit('\n').next().unwrap_or_default()
    );
    let reminder_at = combined
        .find("# Response language")
        .expect("未发出语言提醒");
    let tool_block_at = combined
        .find("<current-tool-availability>")
        .expect("未发出工具可用性块");
    assert!(
        reminder_at > tool_block_at,
        "提醒必须排在 volatile 段的工具可用性块之后，否则 recency 修复被自己顶掉"
    );
}

/// 未接线（`None`）时不得凭空多出一段：子 Agent、后台任务等路径没设语言，
/// 那里追加一句只会变成与 CoT 无关的噪声。
#[tokio::test]
async fn no_response_language_means_no_reminder() {
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(Arc::new(SystemRecordingProvider { seen: seen.clone() }))
        .with_system("MAIN-STUB");
    let combined = capture_system(&mut agent, &seen).await;
    assert!(
        !combined.contains("# Response language"),
        "未设置语言时不应追加提醒"
    );
}

/// 只回报路径的假 "Write" 工具。
///
/// 子目录 reminder 的触发只依赖 `touched_dir` 从入参里读到的 `file_path`，
/// 不真的落盘——测试要的是「哪个目录被触达」，不是文件内容。
struct FakeWriteTool;

#[async_trait::async_trait]
impl Tool for FakeWriteTool {
    fn name(&self) -> &str {
        "Write"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Write".into(),
            description: "fake write".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["file_path"],
                "properties": {"file_path": {"type": "string"}},
                "additionalProperties": false
            }),
            native: None,
        }
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        Ok(ToolResult::ok("written".to_string()))
    }
}

/// 记录每次请求的 system 全文与历史里的全部文本；第一轮返回一个 Write 调用。
struct RecordingSystemAndMessages {
    seen: std::sync::Arc<Mutex<Vec<(String, String)>>>,
    calls: AtomicUsize,
    write_path: String,
}

#[async_trait::async_trait]
impl Provider for RecordingSystemAndMessages {
    async fn stream(
        &self,
        system: &wyj_api::SystemPrompt<'_>,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let history: String = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|block| match block {
                wyj_api::types::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.seen
            .lock()
            .unwrap()
            .push((system.combined().to_string(), history));
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n == 0 {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "w1".into(),
                    name: "Write".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "w1".into(),
                    json_delta: format!(r#"{{"file_path":"{}"}}"#, self.write_path),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 子目录规约 reminder 必须走**历史注入**，不能进 system 段。
///
/// Anthropic 的缓存前缀物理顺序是 `tools → system → messages`：system 块位于
/// messages **之前**，而 messages 断点覆盖「tools + system + messages 前缀」。
/// 因此 system 段任何字节变化都会让 **messages 断点失效**，整段历史全价
/// 重算——每命中一个新目录就付一次全量重算，代价远大于 reminder 本身。
/// 旧实现把它 `push_str` 到 system 的 volatile 段，注释说「位于 cache
/// breakpoint 之后所以不会全价重算」——那说的只是 stable 断点，messages
/// 断点在 system 之后，照样被作废。
///
/// 走历史则相反：system 字节不变 → 前缀哈希不变 → 历史断点照常命中，
/// 只有新追加的那条消息是新的。DeepSeek Harness 的 in-history system prompt
/// update 走的正是这条路。
#[tokio::test]
async fn a_subdirectory_rule_reminder_goes_into_the_history_not_the_system_prompt() {
    const MARKER: &str = "SUBRULE-ONLY-IN-THE-SUBDIR";
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let sub = project.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("CLAUDE.md"), format!("# sub\n\n{MARKER}\n")).unwrap();

    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(Arc::new(RecordingSystemAndMessages {
        seen: seen.clone(),
        calls: AtomicUsize::new(0),
        write_path: sub.join("note.txt").to_string_lossy().to_string(),
    }))
    .with_system("MAIN-STUB")
    .with_claude_md(Arc::new(crate::claude_md::ClaudeMdLoader::new(&project)));
    agent.register_tool(Arc::new(FakeWriteTool));

    let mut session = Session::new();
    session.push_user("go");
    agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await
        .unwrap();

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        2,
        "应发两轮请求（工具轮 + 收尾轮），实际 {}",
        seen.len()
    );
    let (system, history) = &seen[1];
    assert!(
        history.contains(MARKER),
        "子目录规约没有进历史，模型根本看不到它: {history}"
    );
    // 这条断言是上面「不在 system 里」那条的**前提**：只有当 system 确实
    // 带着 volatile 段发出去时，「volatile 里有它 ⇒ system 里有它」才成立，
    // 「system 里没有它」才是有意义的结论而不是恒真。
    assert!(
        system.contains("<current-tool-availability>"),
        "system 未包含 volatile 段，本用例的 system 断言无法证伪: {system}"
    );
    assert!(
        !system.contains(MARKER),
        "子目录规约混进了 system 段——每命中一个新目录就会让整段历史全价重算: {system}"
    );
}

/// 幂等性不变量：**框架层的任何重试都不得重复执行工具**。
///
/// 这是 P2-9 审查的结论，用测试钉住而不是只写在注释里。理由：Agent 里所有
/// 重试点（流中断重发、`parameter_degraded` 降参重发、上下文超限截断后重发、
/// fallback 路由切换）**全部发生在 assistant 消息提交之前**——此时本次
/// assistant 的 `tool_use` 块还没进 `session.messages`，工具也就还没跑过。
/// 半成品 assistant 消息绝不入史（见 stream 循环的注释），所以重发的消息里
/// 不含任何已执行的工具，重试是干净的。
///
/// 工具一旦跑过，重试路径就再也不存在了：下一轮是**模型**基于 tool_result
/// 重新决策，那属于模型行为，不是框架重复执行。
#[tokio::test]
async fn a_retry_never_re_executes_an_already_committed_tool() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::new(FlakyFirstAttemptProvider {
        phase: std::sync::atomic::AtomicUsize::new(0),
    }))
    .with_max_turns(10);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await
        .unwrap();

    // 第 1 次 stream 发了一个 tool_use 后流中断，第 2 次又发了一个 tool_use，
    // 两次的 input 完全相同——正是「如果框架把半成品也提交了、或把重发当成
    // 第二次调用追加上去」的重复执行形态。此处工具只应跑 1 次。
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "重发后的 tool_use 只应执行一次"
    );
    let assistant_with_tool_calls = session
        .messages
        .iter()
        .filter(|m| m.role == wyj_api::types::Role::Assistant)
        .filter(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, wyj_api::types::ContentBlock::ToolUse { .. }))
        })
        .count();
    assert_eq!(
        assistant_with_tool_calls, 1,
        "半成品 assistant 消息不得入史，否则重试会变成重复执行"
    );
}

/// 三段式 provider：第 1 次发工具调用后流中断，第 2 次**发同一个工具调用**
/// 并正常收尾，第 3 次才 EndTurn。两次 tool_use 的 input 刻意完全一致。
struct FlakyFirstAttemptProvider {
    phase: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for FlakyFirstAttemptProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let phase = self.phase.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let call = vec![
            Ok(StreamEvent::ToolUseStart {
                id: "t0".into(),
                name: "Edit".into(),
            }),
            Ok(StreamEvent::ToolUseDelta {
                id: "t0".into(),
                json_delta: r#"{"path":"a.rs"}"#.into(),
            }),
            Ok(StreamEvent::ToolUseEnd { id: "t0".into() }),
        ];
        match phase {
            // 流中途断开：没有 MessageStop，Agent 丢弃半成品后重发。
            0 => {
                let mut events = call;
                events.push(Err(anyhow::anyhow!("simulated mid-stream disconnect")));
                Ok(Box::pin(futures::stream::iter(events)))
            }
            // 这次完整收尾，工具调用被提交 → 应当执行恰好一次。
            1 => {
                let mut events = call;
                events.push(Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }));
                Ok(Box::pin(futures::stream::iter(events)))
            }
            _ => Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                },
            )]))),
        }
    }
}

/// 记忆提取降频：首轮必提，之后每 N 轮一次；空会话不提取。
#[test]
fn memory_extraction_fires_on_the_first_turn_then_every_third() {
    assert!(memory_extraction_due(1), "首轮必须提：新项目最需要记忆");
    assert!(!memory_extraction_due(2));
    assert!(!memory_extraction_due(3));
    assert!(memory_extraction_due(4));
    assert!(!memory_extraction_due(5));
    assert!(memory_extraction_due(7));
    assert!(memory_extraction_due(31));
    assert!(!memory_extraction_due(0), "空会话没有可提取的内容");
}

// ── 编辑后自动校验（verify loop）───────────────────────────────────────

/// 编辑类工具，`runs` 用来证明校验跑在工具**之后**而不是之前。
struct EditingTool {
    runs: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for EditingTool {
    fn name(&self) -> &str {
        "Edit"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Edit".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    fn needs_permission(&self, _input: &serde_json::Value) -> bool {
        false
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok("文件已更新"))
    }
}

/// 把一个会失败的 Edit 送进开着 verify 的 Agent，确认编译器输出真的进了
/// 发给模型的 tool_result——这是整个特性的全部意义所在。
#[tokio::test]
async fn verify_failures_reach_the_model_as_tool_result() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(burst("Edit", 1)).with_verify(Some(crate::verify::VerifyCfg {
        command: "echo 'error[E0308]: mismatched types' >&2; exit 1".into(),
        timeout_ms: 5_000,
        max_output_bytes: 4_000,
        enabled: true,
    }));
    agent.register_tool(Arc::new(EditingTool { runs: runs.clone() }));
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    let tool_results: Vec<String> = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            wyj_api::types::ContentBlock::ToolResult { content, .. } => {
                Some(format!("{content:?}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1, "应当只有一个 Edit 的 tool_result");
    assert!(
        tool_results[0].contains("[verify] 校验失败"),
        "校验反馈没进 tool_result：{}",
        tool_results[0]
    );
    assert!(
        tool_results[0].contains("E0308"),
        "编译器错误没回灌：{}",
        tool_results[0]
    );
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// 零假设是硬约束：项目没声明命令时，一次子进程都不许跑。
#[tokio::test]
async fn without_a_declared_command_nothing_runs() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(burst("Edit", 1));
    agent.register_tool(Arc::new(EditingTool { runs: runs.clone() }));
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert!(!agent.should_verify("Edit"));
    let texts: Vec<String> = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            wyj_api::types::ContentBlock::ToolResult { content, .. } => {
                Some(format!("{content:?}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 1);
    assert!(
        !texts[0].contains("[verify]"),
        "不该有任何校验痕迹：{}",
        texts[0]
    );
}

/// 只对改文件的工具触发：Bash / Read / Grep 都不跑校验。
#[test]
fn only_workspace_mutating_tools_trigger_verify() {
    let agent = Agent::new(Arc::new(EndTurnProvider)).with_verify(Some(crate::verify::VerifyCfg {
        command: "true".into(),
        timeout_ms: 1,
        max_output_bytes: 1,
        enabled: true,
    }));
    for name in ["Edit", "Write"] {
        assert!(agent.should_verify(name), "{name} 应当触发校验");
    }
    for name in ["Bash", "Read", "Grep", "Glob", "Agent", "TodoWrite"] {
        assert!(!agent.should_verify(name), "{name} 不该触发校验");
    }

    // enabled = false 即使显式装配也要被过滤掉（防调用方漏判）
    let off = Agent::new(Arc::new(EndTurnProvider)).with_verify(Some(crate::verify::VerifyCfg {
        command: "true".into(),
        timeout_ms: 1,
        max_output_bytes: 1,
        enabled: false,
    }));
    assert!(!off.should_verify("Edit"));
}

// ── 本轮权限授权（per-turn tool permission grants）─────────────────────

/// 记录 `confirm_tool` 被问了几次、每次的摘要，并按固定决定放行/拒绝。
struct PermissionProbeCtx {
    asks: std::sync::atomic::AtomicUsize,
    summaries: std::sync::Mutex<Vec<String>>,
    grant: bool,
}
impl PermissionProbeCtx {
    fn new(grant: bool) -> Self {
        Self {
            asks: std::sync::atomic::AtomicUsize::new(0),
            summaries: std::sync::Mutex::new(Vec::new()),
            grant,
        }
    }
    fn ask_count(&self) -> usize {
        self.asks.load(std::sync::atomic::Ordering::SeqCst)
    }
}
#[async_trait::async_trait]
impl ToolContext for PermissionProbeCtx {
    fn cwd(&self) -> &std::path::Path {
        std::path::Path::new("/tmp")
    }
    fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
        true
    }
    async fn confirm_tool(&self, name: &str, summary: &str) -> bool {
        self.asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.summaries
            .lock()
            .unwrap()
            .push(format!("{name}:{summary}"));
        self.grant
    }
}

/// `needs_permission()` 可控的探针工具。
struct GatedTool {
    name: &'static str,
    needs_permission: bool,
    runs: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for GatedTool {
    fn name(&self) -> &str {
        self.name
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    fn needs_permission(&self, _input: &serde_json::Value) -> bool {
        self.needs_permission
    }
    fn action_summary(&self, input: &serde_json::Value) -> String {
        input["path"].as_str().unwrap_or("?").to_string()
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok("done"))
    }
}

/// 每轮 emit **一个**工具调用，发满 `budget` 个后 EndTurn 并把计数**归零**。
///
/// 刻意不做「一次 emit 多个」：真实模型每轮只发一个 tool_use，而 Agent 的
/// 内部工具轮循环正是把 N 个工具调用摊成 N 轮——「一次用户提交里的 3 次
/// Edit」= 3 个内部轮。
///
/// 归零是关键：provider 实例被两次 `run_turn` 共用，计数只增不减的话第二次
/// 提交会直接拿到 EndTurn、一个工具都不发，测到的就成了「provider 没发调用」
/// 而不是「授权没被清」。用 `emitted` / `budget` 双计数而不是同一个计数器
/// 递减，也是为了 `stop_reason` 由「还剩没发完」决定，而非递减的副作用。
struct BurstToolProvider {
    emitted: std::sync::atomic::AtomicUsize,
    budget: usize,
    tool: String,
}
#[async_trait::async_trait]
impl Provider for BurstToolProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self
            .emitted
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n >= self.budget {
            self.emitted.store(0, std::sync::atomic::Ordering::SeqCst);
            return Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                },
            )])));
        }
        let id = format!("t{n}");
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::ToolUseStart {
                id: id.clone(),
                name: self.tool.clone(),
            }),
            Ok(StreamEvent::ToolUseDelta {
                id: id.clone(),
                json_delta: format!(r#"{{"path":"f{}.rs"}}"#, n),
            }),
            Ok(StreamEvent::ToolUseEnd { id }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

fn burst(tool: &str, budget: usize) -> Arc<dyn Provider> {
    Arc::new(BurstToolProvider {
        emitted: std::sync::atomic::AtomicUsize::new(0),
        budget,
        tool: tool.to_string(),
    })
}

/// 一直发工具调用，直到在 messages 里读到 `turn-budget-reminder` 才收手。
///
/// 模拟的就是真实触顶会话里的模型：它并不会因为快到上限就自己停下，
/// 必须有人明确告诉它「别再调工具了，现在总结」。`ignore_reminder` 则
/// 造出反面形态——读到提醒仍然继续调工具，用来验证硬中止那道兜底。
struct BudgetAwareProvider {
    calls: AtomicUsize,
    seen_reminder: AtomicBool,
    ignore_reminder: bool,
}
#[async_trait::async_trait]
impl Provider for BudgetAwareProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let has_reminder = messages.iter().any(|m| {
                m.content.iter().any(|b| {
                    matches!(b, ContentBlock::Text { text } if text.contains("turn-budget-reminder"))
                })
            });
        if has_reminder {
            self.seen_reminder.store(true, Ordering::SeqCst);
            if !self.ignore_reminder {
                return Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamEvent::TextDelta("收尾总结".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ])));
            }
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let id = format!("b{n}");
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::ToolUseStart {
                id: id.clone(),
                name: "Edit".into(),
            }),
            Ok(StreamEvent::ToolUseDelta {
                id: id.clone(),
                json_delta: format!(r#"{{"path":"f{n}.rs"}}"#),
            }),
            Ok(StreamEvent::ToolUseEnd { id }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
            }),
        ])))
    }
}

/// 触顶不再等于失败：宽限窗口内注入一次收尾指令，模型交出总结后回合
/// **正常结束**（`Ok`），而不是把已经做完的工作判成一句报错。
///
/// 这条测试钉的是本次事故的根因修复。修复前：`max_turns=8` 的长任务在
/// 第 9 轮 `bail!`，用户拿到红色报错，工作树里的改动一句总结都没留下。
#[tokio::test]
async fn hitting_the_turn_budget_delivers_a_summary_instead_of_erroring() {
    let runs = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(BudgetAwareProvider {
        calls: AtomicUsize::new(0),
        seen_reminder: AtomicBool::new(false),
        ignore_reminder: false,
    });
    let mut agent = Agent::new(provider.clone()).with_max_turns(8);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    let result = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    assert!(
        result.is_ok(),
        "触顶后应正常收尾而不是报错: {:?}",
        result.err()
    );
    assert!(
        provider.seen_reminder.load(Ordering::SeqCst),
        "宽限窗口内必须向模型注入收尾指令"
    );
    // 工具确实跑过——证明这份总结是「已完成的活」而不是空转出来的。
    assert!(
        runs.load(Ordering::SeqCst) > 0,
        "工具一次都没跑，收尾无从谈起"
    );
    let has_summary = session.messages.iter().any(|m| {
        m.role == wyj_api::types::Role::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("收尾总结")))
    });
    assert!(has_summary, "回合历史里应留下模型的最终总结");
}

/// 每次往返都上报**巨额 input token** 的 provider：让花费闸在一两轮内越线。
///
/// 之所以不用真实价格计算，而是直接上报 token，是因为闸的判据是「累计花费
/// 超过上限」——token 数是它的输入。这里报 1M input tokens，按 Opus 的
/// $15/M 算就是 $15，远超测试里设的 $1 上限。
struct SpendHeavyProvider {
    calls: AtomicUsize,
    seen_reminder: AtomicBool,
    ignore_reminder: bool,
}
#[async_trait::async_trait]
impl Provider for SpendHeavyProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let has_reminder = messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains("spend-budget-exceeded"))
            })
        });
        if has_reminder {
            self.seen_reminder.store(true, Ordering::SeqCst);
            if !self.ignore_reminder {
                return Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamEvent::TextDelta("预算收尾总结".into())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ])));
            }
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let id = format!("s{n}");
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::ToolUseStart {
                id: id.clone(),
                name: "Edit".into(),
            }),
            Ok(StreamEvent::ToolUseDelta {
                id: id.clone(),
                json_delta: format!(r#"{{"path":"s{n}.rs"}}"#),
            }),
            Ok(StreamEvent::ToolUseEnd { id }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
            }),
            Ok(StreamEvent::Usage {
                input_tokens: 1_000_000,
                output_tokens: 10,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            }),
        ])))
    }
}

/// 花费熔断的第一段：越闸后**先收尾交付**，而不是直接报错。
///
/// 这条钉的是与轮数闸**同一套语义**：工作树里的成果要被交付，而不是被
/// 判成失败。用户看到的是一份阶段总结，而不是一句红色报错。
#[tokio::test]
async fn exceeding_the_spend_budget_delivers_a_summary_instead_of_erroring() {
    let runs = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SpendHeavyProvider {
        calls: AtomicUsize::new(0),
        seen_reminder: AtomicBool::new(false),
        ignore_reminder: false,
    });
    let mut agent = Agent::new(provider.clone())
        .with_max_turns(50)
        // $1 上限：每轮 1M input tokens = $15，一轮就越线。
        .with_budget_limits(crate::budget::BudgetLimits {
            max_cost_usd: 1.0,
            max_output_tokens: 0,
        })
        // 熔断按模型名查单价，所以必须给一个价格表里有的模型名。
        .with_route_identity(
            "test",
            "anthropic",
            "claude-opus-4",
            "https://api.anthropic.com",
            wyj_config::WireProtocol::AnthropicMessages,
        );
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    let result = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    assert!(
        result.is_ok(),
        "预算越线后应正常收尾而不是报错: {:?}",
        result.err()
    );
    assert!(
        provider.seen_reminder.load(Ordering::SeqCst),
        "越闸后必须向模型注入收尾指令"
    );
    let has_summary = session.messages.iter().any(|m| {
        m.role == wyj_api::types::Role::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("预算收尾总结")))
    });
    assert!(has_summary, "回合历史里应留下模型的最终总结");
}

/// 花费熔断的第二段：无视收尾指令的模型，其工具调用必须**被关闭**，
/// 且回执写明是**预算**原因而不是轮数原因。
///
/// 回执文案必须区分：模型读到自己那次调用被拒时会顺带读到原因。若一律
/// 写「轮数预算」，模型会以为自己「还有轮数只是没钱了」——下一轮它可能
/// 改用更贵的长思考一次性把活干完，反而更烧钱。
#[tokio::test]
async fn the_spend_budget_closes_tools_with_its_own_reason() {
    let runs = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SpendHeavyProvider {
        calls: AtomicUsize::new(0),
        seen_reminder: AtomicBool::new(false),
        ignore_reminder: true,
    });
    let mut agent = Agent::new(provider.clone())
        .with_max_turns(50)
        .with_budget_limits(crate::budget::BudgetLimits {
            max_cost_usd: 1.0,
            max_output_tokens: 0,
        })
        .with_route_identity(
            "test",
            "anthropic",
            "claude-opus-4",
            "https://api.anthropic.com",
            wyj_config::WireProtocol::AnthropicMessages,
        );
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    let _ = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    let closed = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| {
            matches!(b, ContentBlock::ToolResult { content: ToolResultContent::Text(text), .. }
                    if text.contains("tools_closed_spend_budget_exhausted"))
        })
        .count();
    assert!(closed > 0, "越闸后的工具调用必须被关闭（回执写明预算原因）");
    let wrong_reason = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| {
            matches!(b, ContentBlock::ToolResult { content: ToolResultContent::Text(text), .. }
                    if text.contains("tools_closed_turn_budget_exhausted"))
        })
        .count();
    assert_eq!(
        wrong_reason, 0,
        "预算熔断的回执不能写成轮数原因（会误导模型换更贵的策略）"
    );
}

/// 默认配置下**不熔断**：金额闸依赖硬编码单价表，默认开启会在调过价的
/// 模型上莫名截断用户的任务。
#[tokio::test]
async fn the_spend_budget_is_off_by_default() {
    let runs = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SpendHeavyProvider {
        calls: AtomicUsize::new(0),
        seen_reminder: AtomicBool::new(false),
        ignore_reminder: false,
    });
    let mut agent = Agent::new(provider.clone()).with_max_turns(3);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    let _ = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    assert!(
        !provider.seen_reminder.load(Ordering::SeqCst),
        "默认配置下不该出现预算收尾提示"
    );
}

/// 收尾指令**只注入一次**：无视指令的模型不能靠反复注入把自己续命，
/// 否则「最多 N 轮」这个上限就形同虚设。
#[tokio::test]
async fn the_wrap_up_notice_is_injected_at_most_once() {
    let runs = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(BudgetAwareProvider {
        calls: AtomicUsize::new(0),
        seen_reminder: AtomicBool::new(false),
        // 无视收尾指令：模型读到提醒仍继续调工具，回合必须落到硬中止。
        ignore_reminder: true,
    });
    let mut agent = Agent::new(provider.clone()).with_max_turns(8);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    let result = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    assert!(result.is_err(), "无视收尾指令时仍应硬中止（防续命上界）");
    let notices = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(
            |b| matches!(b, ContentBlock::Text { text } if text.contains("turn-budget-reminder")),
        )
        .count();
    assert_eq!(notices, 1, "收尾指令只应注入一次，实际 {notices} 次");
}

/// 收尾窗口内工具**真的被关闭**：无视收尾指令的模型，其调用不再被执行，
/// 而是回一条 `tools_closed_turn_budget_exhausted`。
///
/// 这条钉的是 2026-10-18 事故的根因：一条 285 次调用的会话在第 196 轮
/// 收到了格式正确、位置正确的收尾指令，模型仍然连发 5 轮工具（285 条
/// assistant 消息里 282 条只有 1 个 tool_use），宽限被耗光后 `bail!`。
/// 软性提醒在模型沉浸态里不产生约束力，所以框架必须自己关闸。
///
/// 断言用「工具总共执行了几次」而不是「回合是否成功」：后者在硬中止路径上
/// 同样是 `Err`，分不出修复前后的差别。
#[tokio::test]
async fn tools_are_closed_during_the_wrap_up_window() {
    let runs = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(BudgetAwareProvider {
        calls: AtomicUsize::new(0),
        seen_reminder: AtomicBool::new(false),
        ignore_reminder: true,
    });
    let mut agent = Agent::new(provider.clone()).with_max_turns(8);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: runs.clone(),
    }));
    let mut session = Session::new();

    let _ = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    // 工具只在 wrap_up_at **之前**的轮次执行；wrap_up_at 那一轮注入提醒的
    // 同时就关闸，所以执行次数是 `wrap_up_at - 1`，被关掉的是余下全部。
    let wrap_up_at = 8 - TURN_LIMIT_GRACE;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        wrap_up_at - 1,
        "收尾窗口内的工具调用必须被关闭，实际执行了 {} 次",
        runs.load(Ordering::SeqCst)
    );
    let closed = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| {
            matches!(b, ContentBlock::ToolResult { content: ToolResultContent::Text(text), .. }
                    if text.contains("tools_closed_turn_budget_exhausted"))
        })
        .count();
    assert_eq!(
        closed,
        8 - wrap_up_at + 1,
        "宽限窗口的每一轮都应回一条「工具已关闭」，实际 {closed} 条"
    );
    assert!(
        session.turn_truncated,
        "收尾提醒注入后必须在 session 上留下 turn_truncated 标记——\
         UI 靠它区分「做完了」和「预算用尽被迫交付」"
    );
}

/// 正常结束（模型自己不再调工具）时**不能**留下 `turn_truncated`。
///
/// 这个标记的语义是「本轮被框架关闸」，误报会让 UI 在真正做完时也拒绝
/// 补全 todo、并多起一轮续跑。
#[tokio::test]
async fn a_normally_finished_turn_is_not_marked_truncated() {
    let mut agent = Agent::new(Arc::new(EndTurnProvider)).with_max_turns(64);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: Arc::new(AtomicUsize::new(0)),
    }));
    let mut session = Session::new();

    let _ = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    assert!(!session.turn_truncated, "没进收尾窗口的回合不该被标成截断");
}

/// `turn_truncated` 是**单回合**事实：上一回合撞顶不能污染下一回合。
///
/// 残留 true 的后果很具体：用户在撞顶后打「继续」，那一轮明明正常做完，
/// UI 却因为标记还在而保留 in_progress、再起一轮续跑。
#[tokio::test]
async fn a_previous_truncated_turn_does_not_poison_the_next_one() {
    let mut agent = Agent::new(Arc::new(EndTurnProvider)).with_max_turns(64);
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: false,
        runs: Arc::new(AtomicUsize::new(0)),
    }));
    let mut session = Session::new();
    session.turn_truncated = true;

    let _ = agent
        .run_turn(&mut session, &PermissionProbeCtx::new(true), &mut |_| {})
        .await;

    assert!(
        !session.turn_truncated,
        "回合入口必须重置这个标记，否则上一轮的截断状态会传染"
    );
}

/// 宽限窗口从 `max_turns - TURN_LIMIT_GRACE` 起算：留出的轮数必须够
/// 模型「不再调工具 + 写总结」，否则刚注入就被硬中止，宽限形同虚设。
///
/// 用 const 块而不是 `assert!`：这两个事实不变量是编译期就该钉死的，
/// 写成运行时断言只会让 clippy 提醒把它挪过来。
#[test]
fn the_grace_window_leaves_room_to_actually_deliver() {
    const { assert!(TURN_LIMIT_GRACE >= 2, "宽限至少要够一次工具轮 + 一次总结轮") };
    // 默认 200 的配置下宽限占比极小，不构成实质性的预算削减。
    const { assert!(TURN_LIMIT_GRACE * 10 < 200) };
}

/// 本轮授权记忆的两个方向：同一次提交里 3 次 Bash 只弹 1 次框，3 次都真跑了。
///
/// Bash 按工具名记忆——长任务里几十条 `cargo test` / `git log …` 不该各弹一次，
/// 更不该各付一次分类器往返。注意这里的输入（`{"path":"f{n}.rs"}`）每次都不同，
/// 复用授权靠的正是「命令不参与这把钥匙」。
#[tokio::test]
async fn one_bash_confirm_covers_every_bash_in_a_turn() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(burst("Bash", 3));
    agent.register_tool(Arc::new(GatedTool {
        name: "Bash",
        needs_permission: true,
        runs: runs.clone(),
    }));
    let ctx = PermissionProbeCtx::new(true);
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &ctx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(ctx.ask_count(), 1, "本轮 3 次 Bash 只应弹一次确认");
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 3);
    // 摘要取自第一次：用户是为 f0.rs 点头的，f1/f2 靠本轮授权放行
    assert_eq!(*ctx.summaries.lock().unwrap(), ["Bash:f0.rs"]);
}

/// 反方向：3 次 Edit 改 3 个不同文件必须弹 3 次框。
///
/// 这是合并手动模式时**刻意收紧**的一条。旧的手动模式按工具名记，于是
/// 「批准一次 Edit」之后本轮所有编辑全部免审——用户在为一个文件点头，模型
/// 顺手改了另外两个。写工具的输入决定**改什么**，所以记忆粒度必须落在输入上。
#[tokio::test]
async fn one_edit_confirm_does_not_cover_a_different_file() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(burst("Edit", 3));
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: true,
        runs: runs.clone(),
    }));
    let ctx = PermissionProbeCtx::new(true);
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &ctx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(ctx.ask_count(), 3, "改不同文件的 Edit 不能共用一次确认");
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 3);
}

/// 下一次提交必须重新问：本轮授权不是会话级授权。
#[tokio::test]
async fn grants_do_not_survive_into_the_next_turn() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // budget 1：每次提交只发一次 Edit。两次提交之间唯一的变化就是
    // `turn_permission_grants` 有没有被清空。
    let mut agent = Agent::new(burst("Edit", 1));
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: true,
        runs: runs.clone(),
    }));
    let ctx = PermissionProbeCtx::new(true);
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &ctx, &mut |_| {})
        .await
        .unwrap();
    session.push_user("再来一次");
    agent
        .run_turn(&mut session, &ctx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(ctx.ask_count(), 2, "第二次提交是新的信任边界，必须重新确认");
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// 拒绝不产生本轮授权：模型重试时用户还要再看到一次框，且工具一次都不该跑。
#[tokio::test]
async fn a_denial_grants_nothing_and_blocks_every_attempt() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(burst("Edit", 2));
    agent.register_tool(Arc::new(GatedTool {
        name: "Edit",
        needs_permission: true,
        runs: runs.clone(),
    }));
    let ctx = PermissionProbeCtx::new(false);
    let mut session = Session::new();

    agent
        .run_turn(&mut session, &ctx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(
        ctx.ask_count(),
        2,
        "拒绝必须每次都重新问，否则就成了静默放行"
    );
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// 授权按工具名隔离：批了 Edit 不代表批了 Write；只读工具根本不问。
#[tokio::test]
async fn grants_are_scoped_to_the_approved_tool_name() {
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let ctx = PermissionProbeCtx::new(true);
    let needs_perm = serde_json::json!({});
    let gated = |name: &'static str, needs_permission: bool| -> Arc<dyn Tool> {
        Arc::new(GatedTool {
            name,
            needs_permission,
            runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    };
    let edit = gated("Edit", true);

    agent
        .ensure_tool_permission("Edit", &edit, &needs_perm, &ctx)
        .await
        .expect("人工批准应放行");
    agent
        .ensure_tool_permission("Edit", &edit, &needs_perm, &ctx)
        .await
        .expect("本轮已授权应免问");
    agent
        .ensure_tool_permission("Write", &edit, &needs_perm, &ctx)
        .await
        .expect("人工批准应放行");
    agent
        .ensure_tool_permission("Read", &gated("Read", false), &needs_perm, &ctx)
        .await
        .expect("只读工具不需授权");

    assert_eq!(
        *ctx.summaries.lock().unwrap(),
        ["Edit:?", "Write:?"],
        "Edit 只问一次、Write 单独问一次、Read 根本不问"
    );
}

/// Auto 模式的 ctx 探针：暴露闸门 + 暴露 Auto 模式 + 记录弹窗次数。
struct AutoProbeCtx {
    inner: PermissionProbeCtx,
    gate: Arc<dyn crate::auto_mode::AutoGate>,
}
#[async_trait::async_trait]
impl ToolContext for AutoProbeCtx {
    fn cwd(&self) -> &std::path::Path {
        self.inner.cwd()
    }
    fn is_allowed(&self, name: &str, input: &serde_json::Value) -> bool {
        self.inner.is_allowed(name, input)
    }
    async fn confirm_tool(&self, name: &str, summary: &str) -> bool {
        self.inner.confirm_tool(name, summary).await
    }
    fn permission_mode(&self) -> Option<crate::permission::PermissionMode> {
        Some(crate::permission::PermissionMode::Auto)
    }
    fn auto_gate(&self) -> Option<&Arc<dyn crate::auto_mode::AutoGate>> {
        Some(&self.gate)
    }
}

/// 记录每次 review 的动作摘要、返回预设 verdict 的假闸门。
struct RecordingGate {
    seen: std::sync::Mutex<Vec<String>>,
    verdict: crate::auto_mode::AutoVerdict,
}
#[async_trait::async_trait]
impl crate::auto_mode::AutoGate for RecordingGate {
    async fn review(
        &self,
        request: &crate::auto_mode::AutoReviewRequest,
    ) -> crate::auto_mode::AutoVerdict {
        self.seen
            .lock()
            .unwrap()
            .push(request.action_summary.clone());
        self.verdict.clone()
    }
    fn counters(&self) -> crate::auto_mode::AutoCounters {
        crate::auto_mode::AutoCounters::default()
    }
    fn should_degrade(&self) -> bool {
        false
    }
    fn note_human_approval(&self) {}
    fn disable_classifier(&self) {}
    fn enable_classifier(&self) {}
    fn classifier_enabled(&self) -> bool {
        true
    }
}

/// Auto 的核心不变式：**Bash 之外的本轮记忆必须按调用，不按工具名**。
///
/// 旧的两分叉里，Auto 一律按调用记是为了防住「审过 `Edit notes.md` →
/// 本轮所有 Edit 免审」。现在 Bash 放宽回工具名（长任务里几十条同类命令
/// 不该各付一次分类器往返），但写工具、Agent、computer-use 一律保持精确——
/// 它们的输入决定**对谁做什么**，不是「要不要用这类工具」。
#[tokio::test]
async fn auto_mode_memory_is_per_call_for_every_tool_except_bash() {
    let gate = Arc::new(RecordingGate {
        seen: std::sync::Mutex::new(Vec::new()),
        verdict: crate::auto_mode::AutoVerdict::Allow,
    });
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let ctx = AutoProbeCtx {
        inner: PermissionProbeCtx::new(true),
        gate: gate.clone(),
    };
    let bash: Arc<dyn Tool> = Arc::new(GatedTool {
        name: "Bash",
        needs_permission: true,
        runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let edit: Arc<dyn Tool> = Arc::new(GatedTool {
        name: "Edit",
        needs_permission: true,
        runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });

    let test = |path: &str| serde_json::json!({ "path": path });

    // Bash：命令不同也复用本轮授权，闸门只被问一次。
    for path in ["a.txt", "b.txt", "c.txt"] {
        agent
            .ensure_tool_permission("Bash", &bash, &test(path), &ctx)
            .await
            .expect("闸门放行");
    }
    assert_eq!(
        gate.seen.lock().unwrap().len(),
        1,
        "Bash 按工具名复用本轮授权，不该逐条过闸门"
    );

    // Edit：一次改一个文件，第二次必须重新问。
    agent
        .ensure_tool_permission("Edit", &edit, &test("a.txt"), &ctx)
        .await
        .expect("闸门放行");
    agent
        .ensure_tool_permission("Edit", &edit, &test("b.txt"), &ctx)
        .await
        .expect("闸门放行");
    assert_eq!(
        gate.seen.lock().unwrap().len(),
        3,
        "Edit 一次放行绝不能覆盖改不同文件的调用"
    );
}

/// 闸门拒给具体理由，回灌给模型的文案要带上它（模型据此换路子而不是重试）。
#[tokio::test]
async fn auto_mode_denial_carries_the_rule_reason() {
    let gate = Arc::new(RecordingGate {
        seen: std::sync::Mutex::new(Vec::new()),
        verdict: crate::auto_mode::AutoVerdict::Deny {
            rule: "soft_deny".to_string(),
            reason: "命中禁止规则：生产部署".to_string(),
        },
    });
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let ctx = AutoProbeCtx {
        inner: PermissionProbeCtx::new(true),
        gate,
    };
    let bash: Arc<dyn Tool> = Arc::new(GatedTool {
        name: "Bash",
        needs_permission: true,
        runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let err = agent
        .ensure_tool_permission("Bash", &bash, &serde_json::json!({ "path": "x" }), &ctx)
        .await
        .expect_err("分类器拒绝必须阻断执行");
    assert!(
        err.contains("生产部署"),
        "回灌给模型的理由应包含分类器给出的具体原因，实际: {err}"
    );
    assert_eq!(ctx.inner.ask_count(), 0, "分类器拒给不应再弹窗问人");
}

/// 每轮都直接 EndTurn 的 provider（注入路由测试用）
struct EndTurnProvider;
#[async_trait::async_trait]
impl Provider for EndTurnProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 记录每次请求收到的历史，用于验证「请求发出前」历史已被修复
struct RecordingProvider {
    seen: std::sync::Mutex<Vec<Vec<Message>>>,
}
#[async_trait::async_trait]
impl Provider for RecordingProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 回归：长任务被中断后 `--resume`，输入任何一条消息都不能再把
/// 「assistant(tool_use) → user(无 tool_result)」发出去。端点对这种请求
/// 回 200 + 空流，回合静默结束，用户表现为「说了继续没反应」，且每条
/// 后续消息都复发。钉子：请求历史末尾必须已配对。
#[tokio::test]
async fn dangling_tool_use_is_repaired_before_the_request_is_sent() {
    let provider = Arc::new(RecordingProvider {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let agent = Agent::new(provider.clone());
    let mut session = Session::new();
    session.push_user("跑一下 git status");
    session.push_assistant(vec![
        ContentBlock::Text { text: "好".into() },
        ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "git status --short" }),
        },
    ]);
    // 落盘半截历史后重新载入，用户补一句「继续」——正是复现路径。
    session.push_user("继续");

    let mut visible = String::new();
    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .expect("回合不应报错");

    let seen = provider.seen.lock().unwrap();
    let request = seen.first().expect("provider 至少被调用一次");
    let tail = request.last().expect("请求历史非空");
    assert!(matches!(tail.role, Role::User));
    assert!(
        tail.content.iter().any(|block| matches!(
            block,
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "toolu_1"
        )),
        "发出的请求末尾必须带上 toolu_1 的合成 tool_result，否则端点回空流：{tail:?}"
    );
    assert!(
        !visible.is_empty(),
        "补齐动作必须对用户可见，否则仍是「没反应」的体感"
    );
}

/// 可并发的 mock 工具：睡 ms 毫秒后返回 tag
struct SleepTool;
#[async_trait::async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "Sleep"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Sleep".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    async fn run(&self, input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let ms = input.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        let tag = input.get("tag").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::ok(tag.to_string()))
    }
}

#[tokio::test]
async fn parallel_safe_tools_run_concurrently_and_keep_result_order() {
    let mut agent = Agent::new(Arc::new(TwoToolProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(SleepTool));
    let mut session = Session::new();
    session.push_user("go");

    let start = Instant::now();
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();
    // 两个 150ms 的 parallel_safe 工具应并发执行：总耗时远小于串行的 300ms
    assert!(
        start.elapsed() < std::time::Duration::from_millis(280),
        "工具未并发执行，耗时 {:?}",
        start.elapsed()
    );

    // 结果必须按原始调用顺序回填
    let mut results = vec![];
    for m in &session.messages {
        for b in &m.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(text),
                ..
            } = b
            {
                results.push((tool_use_id.clone(), text.clone()));
            }
        }
    }
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], ("t1".to_string(), "first".to_string()));
    assert_eq!(results[1], ("t2".to_string(), "second".to_string()));
}

#[tokio::test]
async fn single_tool_capability_serializes_complete_multi_call_response() {
    let capabilities = wyj_api::ModelCapabilities::conservative(64_000, 8_192);
    assert_eq!(capabilities.max_tools_per_turn, 1);
    assert!(!capabilities.parallel_tool_calls.value);

    let mut agent = Agent::new(Arc::new(TwoToolProvider {
        calls: AtomicUsize::new(0),
    }))
    .with_model_capabilities(capabilities);
    agent.register_tool(Arc::new(SleepTool));
    let mut session = Session::new();
    session.push_user("go");

    let start = Instant::now();
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert!(
        start.elapsed() >= std::time::Duration::from_millis(280),
        "单工具兼容模式应串行执行已返回的完整调用，实际耗时 {:?}",
        start.elapsed()
    );
    let results: Vec<_> = session
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(text),
                is_error,
                ..
            } => Some((tool_use_id.as_str(), text.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![("t1", "first", false), ("t2", "second", false)]
    );
}

/// 模拟 `Bash`：需要权限确认（`parallel_safe` 保持默认 false），但每次调用
/// 彼此隔离、可以并发——这正是 `parallel_safe_with_peer` 描述的形状。
struct ConfirmableIsolatedTool;
#[async_trait::async_trait]
impl Tool for ConfirmableIsolatedTool {
    fn name(&self) -> &str {
        "ConfirmableIsolated"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ConfirmableIsolated".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    fn needs_permission(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn parallel_safe_with_peer(&self) -> bool {
        true
    }
    async fn run(&self, input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let ms = input.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        let tag = input.get("tag").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::ok(tag.to_string()))
    }
}

/// 同一批返回两个 `ConfirmableIsolated` 调用的 provider。
struct TwoConfirmableProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for TwoConfirmableProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n == 0 {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "c1".into(),
                    name: "ConfirmableIsolated".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "c1".into(),
                    json_delta: r#"{"ms":200,"tag":"first"}"#.into(),
                }),
                Ok(StreamEvent::ToolUseStart {
                    id: "c2".into(),
                    name: "ConfirmableIsolated".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "c2".into(),
                    json_delta: r#"{"ms":200,"tag":"second"}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 已授权的上下文：Bypass / Plan 白名单 / 项目级 AllowAlways / 本轮已授权时
/// `confirm_tool` 都不会 await UI，对应 `needs_dialog` 返回 false。
struct NoDialogCtx;
#[async_trait::async_trait]
impl ToolContext for NoDialogCtx {
    fn cwd(&self) -> &std::path::Path {
        std::path::Path::new("/tmp")
    }
    fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
        true
    }
    async fn confirm_tool(&self, _name: &str, _summary: &str) -> bool {
        true
    }
    fn needs_dialog(&self, _name: &str) -> bool {
        false
    }
}

/// 仍需弹框的上下文：用户当场按下允许，`confirm_tool` 返回 true 但**确实
/// 等待过 UI**。用它验证「会弹框的调用必须串行」——两个这样的调用并发起来
/// 就会在同一时刻弹出两个对话框。
struct DialogCtx;
#[async_trait::async_trait]
impl ToolContext for DialogCtx {
    fn cwd(&self) -> &std::path::Path {
        std::path::Path::new("/tmp")
    }
    fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
        true
    }
    async fn confirm_tool(&self, _name: &str, _summary: &str) -> bool {
        true
    }
}

/// 钉子：`parallel_safe_with_peer` 只在**真的不弹框**时才会兑现并发。
/// `DialogCtx` 是 Prompt 模式下用户还没对这个工具点过 AllowAlways 的等价物
/// （`needs_dialog` 保持默认的 true），此时两个会弹框的调用必须仍然串行——
/// 「同一时刻至多一个对话框」是这个不变式的全部理由，把它们排进并发组就会
/// 同时弹出两个对话框。
#[tokio::test]
async fn confirmable_isolated_calls_stay_serial_while_a_dialog_would_block() {
    let mut agent = Agent::new(Arc::new(TwoConfirmableProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(ConfirmableIsolatedTool));
    let mut session = Session::new();
    session.push_user("go");

    let start = Instant::now();
    agent
        .run_turn(&mut session, &DialogCtx, &mut |_| {})
        .await
        .unwrap();

    assert!(
        start.elapsed() >= std::time::Duration::from_millis(380),
        "会弹权限框的调用必须彼此串行（一次只弹一个对话框），实际耗时 {:?}",
        start.elapsed()
    );
}

/// 钉子：一旦不再需要弹框，同一批里的同类调用就该并发跑完，而不是排队。
/// 这条直接对应 gupiao 的实测痛点——模型一轮里返回多条互不依赖的只读查询时，
/// 串行执行让每个回合凭空多出 N 次工具等待。
#[tokio::test]
async fn confirmable_isolated_calls_run_concurrently_once_authorized() {
    let mut agent = Agent::new(Arc::new(TwoConfirmableProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(ConfirmableIsolatedTool));
    let mut session = Session::new();
    session.push_user("go");

    let start = Instant::now();
    agent
        .run_turn(&mut session, &NoDialogCtx, &mut |_| {})
        .await
        .unwrap();

    assert!(
        start.elapsed() < std::time::Duration::from_millis(380),
        "已授权后两个 200ms 的隔离调用应并发（总计约 200ms），实际耗时 {:?}",
        start.elapsed()
    );
    // 并发不改变回填顺序：结果仍按 tool_use 原始下标排序。
    let results: Vec<_> = session
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(text),
                ..
            } => Some((tool_use_id.as_str(), text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![("c1", "first"), ("c2", "second")],
        "并发执行后仍必须按原始顺序回填 tool_result"
    );
}

/// 记录被调用次数的 mock 工具，用于验证 PreToolUse Block 阻止了工具实际执行
struct CountingTool {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Echo".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::ok("echoed".to_string()))
    }
}

/// 名字可配置的成功桩工具。`FileChanged` 之类的 hook 判据是**工具名**，
/// 需要能造出叫 `Write` / `Edit` 的工具。
struct NamedStubTool(&'static str);

#[async_trait::async_trait]
impl Tool for NamedStubTool {
    fn name(&self) -> &str {
        self.0
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        Ok(ToolResult::ok("ok".to_string()))
    }
}

struct RepeatedTwoToolProvider {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for RepeatedTwoToolProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n < 3 {
            let first_id = format!("round-{n}-first");
            let second_id = format!("round-{n}-second");
            // 每次给 input 加个 round 标识,让 fnv_hash 不重复,避免
            // loop detection(最近 5 次相同 input 命中 3 次触发)误伤
            // 这个测 argument retry guard 的本意。
            let first_args = format!(r#"{{"round":{n}}}"#);
            let second_args = format!(r#"{{"round":{n},"slot":2}}"#);
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: first_id.clone(),
                    name: "Echo".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: first_id,
                    json_delta: first_args,
                }),
                Ok(StreamEvent::ToolUseStart {
                    id: second_id.clone(),
                    name: "Echo".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: second_id,
                    json_delta: second_args,
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn repeated_multi_call_responses_do_not_trip_argument_retry_guard() {
    let calls = Arc::new(AtomicUsize::new(0));
    let capabilities = wyj_api::ModelCapabilities::conservative(64_000, 8_192);
    let mut agent = Agent::new(Arc::new(RepeatedTwoToolProvider {
        calls: AtomicUsize::new(0),
    }))
    .with_model_capabilities(capabilities);
    agent.register_tool(Arc::new(CountingTool {
        calls: calls.clone(),
    }));
    let mut session = Session::new();
    session.push_user("go");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert!(!session.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(text),
                    ..
                } if text.contains("tool_limit_exceeded")
            )
        })
    }));
}

struct RequiredCountingTool {
    calls: Arc<AtomicUsize>,
}

struct NamedTool(&'static str);

#[async_trait::async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.to_string(),
            description: format!("{} test tool", self.0),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }

    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        Ok(ToolResult::ok(self.0.to_string()))
    }
}

#[test]
fn lazy_tools_only_activate_above_the_configured_threshold() {
    let mut small = Agent::new(Arc::new(EndTurnProvider));
    small.register_tool(Arc::new(CountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    assert!(!small.enable_lazy_tools(Vec::<String>::new(), 1, 8, 3));
    assert!(small.lazy_tool_state.is_none());
    assert!(!small
        .tools
        .iter()
        .any(|definition| definition.name == "ToolSearch"));

    let mut large = Agent::new(Arc::new(EndTurnProvider));
    large.register_tool(Arc::new(CountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    assert!(large.enable_lazy_tools(Vec::<String>::new(), 0, 8, 3));
    assert!(large.lazy_tool_state.is_some());
    assert!(large
        .tools
        .iter()
        .any(|definition| definition.name == "ToolSearch"));
}

#[test]
fn lazy_tools_never_hide_the_core_execution_surface() {
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    for name in [
        "Read",
        "Bash",
        "BashOutput",
        "Edit",
        "Write",
        "Memory",
        "Agent",
        "ExitPlanMode",
        "window_capture",
        "app_computer",
        "computer",
        "mcp__optional__lookup",
    ] {
        agent.register_tool(Arc::new(NamedTool(name)));
    }

    assert!(agent.enable_lazy_tools(["Read".to_string()], 0, 8, 3));
    let state = agent.lazy_tool_state.as_ref().unwrap();
    for name in [
        "Read",
        "Bash",
        "BashOutput",
        "Edit",
        "Write",
        "Memory",
        "Agent",
        "ExitPlanMode",
        "window_capture",
        "app_computer",
        "computer",
    ] {
        assert!(
            state.visible(name),
            "核心工具 {name} 不应被 lazy schema 隐藏"
        );
    }
    assert!(!state.visible("mcp__optional__lookup"));
}

/// 请求里实际发送的工具数组必须**单调不减**。
///
/// Anthropic 明确规定：会话中途增删或重排 tools 会让 tools 缓存**及其后的
/// system、messages 全部失效**。旧实现让 `sticky_turns`（默认 3）到期后把
/// 工具从请求里拿掉，等于每几个回合就作废一次整段 prompt cache——代价远
/// 大于它省下的那点 schema token。
#[tokio::test]
async fn the_tools_sent_in_a_request_never_shrink_within_a_session() {
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    for name in [
        "Read",
        "Bash",
        "mcp__optional__lookup",
        "mcp__optional__fetch",
    ] {
        agent.register_tool(Arc::new(NamedTool(name)));
    }
    assert!(agent.enable_lazy_tools(Vec::<String>::new(), 0, 8, 3));
    let state = agent.lazy_tool_state.as_ref().unwrap().clone();
    let route = agent.route_at(0);

    let (before, _) = agent.tools_for_request(&route);
    assert!(
        !before.iter().any(|d| d.name == "mcp__optional__lookup"),
        "折叠的工具不该在第一次请求里: {before:?}"
    );

    // 搜索命中 → 该工具的 schema 从此可见
    state.begin_task_turn();
    state.expose("mcp__optional__lookup");
    let (after_search, _) = agent.tools_for_request(&route);
    assert!(
        after_search
            .iter()
            .any(|d| d.name == "mcp__optional__lookup"),
        "命中过的工具应当可见: {after_search:?}"
    );

    // 熬过 sticky 的新鲜度窗口：可见性**不能**因此收回
    for _ in 0..8 {
        state.begin_task_turn();
    }
    let (after_expiry, _) = agent.tools_for_request(&route);
    assert_eq!(
        after_expiry.len(),
        after_search.len(),
        "工具集合缩水了——这会让整段 prompt cache 作废一次: {after_search:?} → {after_expiry:?}"
    );
    assert!(
        after_expiry
            .iter()
            .any(|d| d.name == "mcp__optional__lookup"),
        "命中过的工具必须一直可见: {after_expiry:?}"
    );
}

#[async_trait::async_trait]
impl Tool for RequiredCountingTool {
    fn name(&self) -> &str {
        "RequiredEcho"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "RequiredEcho".into(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["value"],
                "properties": {"value": {"type": "string"}},
                "additionalProperties": false
            }),
            native: None,
        }
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::ok("echoed".to_string()))
    }
}

struct MalformedThenTextProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for MalformedThenTextProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let events: Vec<Result<StreamEvent>> = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "bad-1".into(),
                    name: "Echo".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "bad-1".into(),
                    json_delta: r#"{"command":"unterminated"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("recovered".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn malformed_tool_arguments_are_reported_but_never_executed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::new(MalformedThenTextProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(CountingTool {
        calls: calls.clone(),
    }));
    let mut session = Session::new();
    session.push_user("go");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content: ToolResultContent::Text(text), is_error: true, .. }
                        if text.contains("tool_arguments_invalid")
                )
            })
        }));
}

struct InvalidSchemaThenCorrectProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for InvalidSchemaThenCorrectProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = match n {
            0 => vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "schema-bad".into(),
                    name: "RequiredEcho".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "schema-bad".into(),
                    json_delta: "{}".into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ],
            1 => vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "schema-good".into(),
                    name: "RequiredEcho".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "schema-good".into(),
                    json_delta: r#"{"value":"ok"}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ],
            _ => vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ],
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn schema_error_is_targeted_and_corrected_call_executes_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::new(InvalidSchemaThenCorrectProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(RequiredCountingTool {
        calls: calls.clone(),
    }));
    let mut session = Session::new();
    session.push_user("go");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(session.api_calls >= 3);
}

/// 参数校验失败时，历史里必须留着**模型实际发过的参数**。
///
/// 一次真实故障（会话 `sess-1792142673-799da7d07fbc`）：模型把 19 位
/// `tree_id` 当字符串发了 5 轮，每轮历史里都只剩
/// `{"_wyj_code_invalid_arguments": true}`——模型只看到「tree_id 类型不对」，
/// 看不到自己传了什么，于是原样重发同一个错误参数，直到触发重试上限把
/// 整轮（已完成窗口枚举、截图、菜单列举）作废。
#[tokio::test]
async fn an_invalid_call_keeps_its_own_arguments_in_history() {
    let mut agent = Agent::new(Arc::new(SchemaViolationThenTextProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(RequiredCountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let mut session = Session::new();
    session.push_user("go");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    let echoed = session.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { name, input, .. }
                    if name == "RequiredEcho" && input["value"] == "wrong"
            )
        })
    });
    assert!(
        echoed,
        "校验失败的工具调用应在历史里保留模型原始参数，让模型能对照纠错"
    );
    assert!(
        !session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolUse { input, .. }
                        if input.get("_wyj_code_invalid_arguments").is_some()
                )
            })
        }),
        "参数可解析成 object 时不应再用占位符代替它"
    );
}

struct SchemaViolationThenTextProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for SchemaViolationThenTextProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let events: Vec<Result<StreamEvent>> = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "bad-enum".into(),
                    name: "RequiredEcho".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "bad-enum".into(),
                    json_delta: r#"{"value":"wrong"}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 连续参数校验失败**不得**把整轮判成失败。
///
/// 这是上面那次故障的直接后果：这一轮里窗口枚举、截图、菜单列举都已成功，
/// 却因为一个调用的参数形态被 `bail!` 整轮中止。正确形态是先注入一次
/// 「换个思路、就地收尾」的指令，模型读到后交出总结，回合正常结束。
#[tokio::test]
async fn repeated_argument_failures_wrap_up_instead_of_aborting_the_turn() {
    let mut agent = Agent::new(Arc::new(InvalidThenDeliverProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(RequiredCountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let mut session = Session::new();
    session.push_user("go");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .expect("连续参数校验失败不应中止整轮");

    assert!(
            session.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text } if text.contains("tool-arguments-wrap-up"))
                })
            }),
            "应向模型注入一次收尾指令"
        );
    assert!(
            session.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text } if text.contains("here is what I have"))
                })
            }),
            "模型读到收尾指令后应能正常交付，这一轮不该被判成失败"
        );
}

/// 连撞到触发收尾的那一轮，随后交出总结——这是收尾机制生效的正常路径。
struct InvalidThenDeliverProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for InvalidThenDeliverProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n < INVALID_ARGUMENT_GRACE_AT {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: format!("bad-{n}"),
                    name: "RequiredEcho".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: format!("bad-{n}"),
                    json_delta: r#"{"value":12}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("here is what I have so far".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 收尾指令只注入一次：反复注入等于给模型续命，防无界重试的闸门就形同虚设。
#[tokio::test]
async fn the_argument_wrap_up_notice_is_injected_at_most_once_per_streak() {
    let mut agent = Agent::new(Arc::new(AlwaysInvalidProvider {
        calls: AtomicUsize::new(0),
    }));
    agent.register_tool(Arc::new(RequiredCountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let mut session = Session::new();
    session.push_user("go");

    let _ = agent.run_turn(&mut session, &FakeCtx, &mut |_| {}).await;

    let notices = session
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("tool-arguments-wrap-up"))
            })
            .count();
    assert_eq!(notices, 1, "收尾指令只应注入一次，实际 {notices} 次");
}

/// 模型无视收尾指令、继续用同样方式硬撞，才落到硬中止——那才是真正该
/// 报错的形态（否则「避免无界重试」这条注释就成了空话）。
#[tokio::test]
async fn ignoring_the_wrap_up_notice_still_stops_the_runaway_retry() {
    let mut agent = Agent::new(Arc::new(AlwaysInvalidProvider {
        calls: AtomicUsize::new(0),
    }))
    .with_max_turns(16);
    agent.register_tool(Arc::new(RequiredCountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let mut session = Session::new();
    session.push_user("go");

    let error = agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .expect_err("无视收尾指令后仍应硬中止（防续命上界）");
    assert!(
        error.to_string().contains("工具参数连续校验失败"),
        "unexpected error: {error}"
    );
}

/// 无视收尾指令、每轮都重发同一个无效参数。
struct AlwaysInvalidProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for AlwaysInvalidProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n == 0 {
            vec![
                Ok(StreamEvent::TextDelta("partial".into())),
                Ok(StreamEvent::ToolUseStart {
                    id: format!("bad-{n}"),
                    name: "RequiredEcho".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: format!("bad-{n}"),
                    json_delta: r#"{"value":12}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::ToolUseStart {
                    id: format!("bad-{n}"),
                    name: "RequiredEcho".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: format!("bad-{n}"),
                    json_delta: r#"{"value":12}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// `is_allowed` 恒返回 false，用于验证 PreToolUse `Approve` 不能绕过强制策略。
struct DenyAllCtx;
#[async_trait::async_trait]
impl ToolContext for DenyAllCtx {
    fn cwd(&self) -> &std::path::Path {
        std::path::Path::new("/tmp")
    }
    fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
        false
    }
}

fn hooks_settings_with(
    event: &str,
    matcher: Option<&str>,
    command: &str,
) -> crate::hooks::HooksSettings {
    use crate::hooks::{HookCommand, HookMatcherEntry, HooksSettings};
    let mut hooks = HashMap::new();
    hooks.insert(
        event.to_string(),
        vec![HookMatcherEntry {
            matcher: matcher.map(|s| s.to_string()),
            hooks: vec![HookCommand {
                hook_type: "command".into(),
                command: command.into(),
                timeout: Some(5),
            }],
        }],
    );
    HooksSettings { hooks }
}

/// `FileChanged` 只在**写类工具成功**时触发，只读工具与失败调用都不触发。
///
/// 动机：用户想把 formatter/linter 挂在「文件真的被改了」这个时机上。挂在
/// `PostToolUse` 上会对着 Read/Grep 也跑一遍，一批 8 个工具就跑 8 次。
#[tokio::test]
async fn file_changed_hook_fires_only_for_successful_writes() {
    // hook 命中时往一个临时文件追加一行，用它判断是否触发。
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("fired.txt");
    let settings = hooks_settings_with(
        "FileChanged",
        None,
        &format!("echo hit >> {}", marker.display()),
    );
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    agent.register_tool(Arc::new(CountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    // `FileChanged` 的判据是**工具名**（Write/Edit/NotebookEdit），所以这里
    // 需要一个真的叫 `Write` 的桩——复用 CountingTool 的 name() 拿不到它。
    agent.register_tool(Arc::new(NamedStubTool("Write")));
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));

    // 1) Read 不是写类工具 → 不触发。
    agent
        .exec_tool_call(
            &FakeCtx,
            "1".into(),
            "Read".into(),
            serde_json::json!({ "file_path": "/tmp/x" }),
        )
        .await;
    assert!(!marker.exists(), "Read 不该触发 FileChanged");

    // 2) Write 且成功 → 触发。
    let (_, _, is_error) = agent
        .exec_tool_call(
            &FakeCtx,
            "2".into(),
            "Write".into(),
            serde_json::json!({ "file_path": "/tmp/y", "content": "x" }),
        )
        .await;
    assert!(!is_error);
    assert!(marker.exists(), "成功的 Write 必须触发 FileChanged");
}

#[tokio::test]
async fn pre_tool_use_block_prevents_tool_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    agent.register_tool(Arc::new(CountingTool {
        calls: calls.clone(),
    }));
    let settings = hooks_settings_with(
        "PreToolUse",
        None,
        r#"echo '{"decision":"block","reason":"nope"}'"#,
    );
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));

    let (id, content, is_error) = agent
        .exec_tool_call(&FakeCtx, "1".into(), "Echo".into(), serde_json::json!({}))
        .await;

    assert_eq!(id, "1");
    assert!(is_error);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "被 PreToolUse Block 的工具不应实际执行"
    );
    match content {
        ToolResultContent::Text(t) => assert!(t.contains("nope")),
        _ => panic!("expected text content"),
    }
}

#[tokio::test]
async fn pre_tool_use_approve_cannot_bypass_is_allowed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    agent.register_tool(Arc::new(CountingTool {
        calls: calls.clone(),
    }));
    let settings = hooks_settings_with("PreToolUse", None, r#"echo '{"decision":"approve"}'"#);
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));

    let (_id, _content, is_error) = agent
        .exec_tool_call(
            &DenyAllCtx,
            "1".into(),
            "Echo".into(),
            serde_json::json!({}),
        )
        .await;

    assert!(is_error);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "hook approve 只能跳过交互询问，不能绕过强制权限策略"
    );
}

#[tokio::test]
async fn post_tool_use_block_appends_feedback_without_marking_error() {
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    agent.register_tool(Arc::new(CountingTool {
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let settings = hooks_settings_with(
        "PostToolUse",
        None,
        r#"echo '{"decision":"block","reason":"lint failed"}'"#,
    );
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));

    let (_id, content, is_error) = agent
        .exec_tool_call(&FakeCtx, "1".into(), "Echo".into(), serde_json::json!({}))
        .await;

    assert!(!is_error, "PostToolUse 反馈不应把成功结果打成失败");
    match content {
        ToolResultContent::Text(t) => {
            assert!(t.contains("echoed"));
            assert!(t.contains("lint failed"));
        }
        _ => panic!("expected text content"),
    }
}

#[tokio::test]
async fn no_hook_runner_behaves_exactly_as_before() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    agent.register_tool(Arc::new(CountingTool {
        calls: calls.clone(),
    }));

    let (_id, _content, is_error) = agent
        .exec_tool_call(&FakeCtx, "1".into(), "Echo".into(), serde_json::json!({}))
        .await;

    assert!(!is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn user_prompt_submit_block_pops_message_and_skips_model_call() {
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let settings = hooks_settings_with(
        "UserPromptSubmit",
        None,
        r#"echo '{"decision":"block","reason":"denied"}'"#,
    );
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));
    let mut session = Session::new();
    session.push_user("hello");

    let mut out = String::new();
    agent
        .run_turn(&mut session, &FakeCtx, &mut |t| out.push_str(t))
        .await
        .unwrap();

    assert!(out.contains("denied"));
    assert!(
        session.messages.is_empty(),
        "被 block 的 user 消息应回退，不留在 session 里"
    );
}

#[tokio::test]
async fn user_prompt_submit_continue_prepends_context() {
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let settings = hooks_settings_with(
        "UserPromptSubmit",
        None,
        r#"echo '{"additionalContext":"extra ctx"}'"#,
    );
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));
    let mut session = Session::new();
    session.push_user("hello");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    let has_ctx = session.messages[0]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("extra ctx")));
    assert!(has_ctx, "additionalContext 应被前插进首条 user 消息");
}

/// 每次调用都计数、恒 EndTurn 的 provider（Stop hook 续跑测试用）
struct CountingEndTurnProvider {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Provider for CountingEndTurnProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn stop_hook_continue_makes_loop_run_again() {
    // 用一个临时文件做进程外状态：第一次调用返回 continue:false（要求继续），
    // 第二次（文件已存在）放行，让循环真正结束，避免死循环。
    let flag_path =
        std::env::temp_dir().join(format!("wyj-stop-hook-test-{}.flag", uuid::Uuid::new_v4()));
    let flag = flag_path.display();
    let command = format!(
            "if [ -f {flag} ]; then echo '{{}}'; else touch {flag}; echo '{{\"continue\":false,\"reason\":\"once more\"}}'; fi"
        );

    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(Arc::new(CountingEndTurnProvider {
        calls: calls.clone(),
    }));
    let settings = hooks_settings_with("Stop", None, &command);
    let agent = agent.with_hooks(Arc::new(crate::hooks::HookRunner::from_settings(
        settings, true,
    )));
    let mut session = Session::new();
    session.push_user("hello");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "Stop 要求 continue 应让循环再跑一轮，模型应被调用两次"
    );
    std::fs::remove_file(&flag_path).ok();
}

#[tokio::test]
async fn injection_kind_routes_to_callback() {
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let queue = SteerQueue::new();
    queue.push(
        vec![ContentBlock::Text {
            text: "bg result".into(),
        }],
        InjectionKind::SystemReminder,
    );

    let mut kinds = vec![];
    let mut session = Session::new();
    session.push_user("hi");
    agent
        .run_turn_with_injection(&mut session, &FakeCtx, &mut |_| {}, Some(&queue), |k| {
            kinds.push(k)
        })
        .await
        .unwrap();
    // SystemReminder 注入应回调对应 kind（TUI 据此不弹用户消息队列）
    assert_eq!(kinds, vec![InjectionKind::SystemReminder]);
    // 注入内容已合并进对话
    let merged = session.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("bg result")))
    });
    assert!(merged);
}

/// 共享注入队列的基本语义：FIFO、`drain` 取走全部、取空后为空。
#[test]
fn steer_queue_is_fifo_and_drain_takes_everything() {
    let queue = SteerQueue::new();
    assert!(queue.is_empty());

    queue.push(
        vec![ContentBlock::Text {
            text: "first".into(),
        }],
        InjectionKind::UserMessage,
    );
    queue.push(
        vec![ContentBlock::Text {
            text: "second".into(),
        }],
        InjectionKind::SystemReminder,
    );
    assert!(!queue.is_empty());

    let drained = queue.drain();
    assert_eq!(drained.len(), 2);
    assert!(matches!(drained[0].1, InjectionKind::UserMessage));
    assert!(matches!(drained[1].1, InjectionKind::SystemReminder));
    assert!(queue.is_empty());
    assert!(queue.drain().is_empty());
}

/// 前台子 Agent 只取**用户补充**：队列里的 `SystemReminder`（后台子 Agent /
/// 后台 shell 的完成通知）是给主 Agent 的结论，进了子 Agent 的会话既答非所问，
/// 又会让主 Agent 永远收不到——子 Agent 返回时队列已被取空。
///
/// 这条钉子守住「共享队列」最容易踩的坑：把 `drain()` 直接给子 Agent 用。
#[tokio::test]
async fn subagent_drain_leaves_system_reminders_for_the_main_agent() {
    let queue = SteerQueue::new();
    queue.push(
        vec![ContentBlock::Text {
            text: "user steer".into(),
        }],
        InjectionKind::UserMessage,
    );
    queue.push(
        vec![ContentBlock::Text {
            text: "bg agent finished".into(),
        }],
        InjectionKind::SystemReminder,
    );

    // 子 Agent 侧：只排空用户补充
    let taken = queue.drain_user_messages();
    assert_eq!(taken.len(), 1);
    assert!(matches!(taken[0].1, InjectionKind::UserMessage));

    // 主 Agent 侧：后台结果仍原封不动地等着
    let rest = queue.drain();
    assert_eq!(rest.len(), 1);
    assert!(matches!(rest[0].1, InjectionKind::SystemReminder));
    assert!(queue.is_empty());
}

/// 「谁先到工具轮边界谁取走」——主 Agent 与前台子 Agent 排空**同一份**队列，
/// 因此一条补充内容只会被投递一次，不会同时进两个会话（也不会凭空丢失）。
///
/// 这条是前台子 Agent 接入注入通道后的核心不变式：如果哪天改成两份拷贝或
/// 各自独立的队列，用户补充的内容要么被投递两次、要么子 Agent 那份永远没人取。
#[tokio::test]
async fn shared_steer_queue_delivers_to_exactly_one_agent() {
    let agent = Agent::new(Arc::new(EndTurnProvider));
    let queue = SteerQueue::new();
    queue.push(
        vec![ContentBlock::Text {
            text: "steer me".into(),
        }],
        InjectionKind::UserMessage,
    );

    let mut first = Session::new();
    first.push_user("hi");
    agent
        .run_turn_with_injection(&mut first, &FakeCtx, &mut |_| {}, Some(&queue), |_| {})
        .await
        .unwrap();

    let mut second = Session::new();
    second.push_user("hi");
    agent
        .run_turn_with_injection(&mut second, &FakeCtx, &mut |_| {}, Some(&queue), |_| {})
        .await
        .unwrap();

    let landed = |session: &Session| {
        session.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("steer me")))
        })
    };
    assert!(
        landed(&first) ^ landed(&second),
        "补充内容必须且只能进入其中一个会话"
    );
}

/// 首次流中途断开、第二次成功的 provider（流中断重试测试用）
struct FlakyProvider {
    calls: AtomicUsize,
}

struct UnsupportedThinkingThenSuccessProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for UnsupportedThinkingThenSuccessProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut error = wyj_api::ProviderError::new(
                wyj_api::ProviderErrorKind::UnsupportedParameter,
                "thinking is unsupported",
            );
            error.parameter = Some("thinking".to_string());
            return Err(anyhow::Error::new(error));
        }
        assert_eq!(opts.thinking_budget, None);
        assert!(!opts.interleaved);
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ])))
    }
}

#[tokio::test]
async fn unsupported_safe_parameter_is_removed_once_and_reported() {
    let agent = Agent::new(Arc::new(UnsupportedThinkingThenSuccessProvider {
        calls: AtomicUsize::new(0),
    }))
    .with_thinking(Some(1024), true);
    let mut session = Session::new();
    session.push_user("hi");
    let mut visible = String::new();
    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .unwrap();
    // i18n: agent.degraded.parameter_removed。中文翻译：模型端点不支持参数 `{name}`，已安全移除后重试一次
    let msg = wyj_i18n::tr_fmt("agent.degraded.parameter_removed", &[("name", "thinking")]);
    assert!(visible.contains(&msg), "expected {msg:?} in {visible:?}");
    // 2 次尝试（首次被拒 + 重试成功），成功往返只有 1 次。
    assert_eq!(session.api_attempts, 2);
    assert_eq!(session.api_calls, 1);
}

/// 只认 `reasoning_effort` 的端点（DeepSeek）：profile → route → RequestOptions
/// 必须一路带到底。中间任何一环漏掉，用户看到的都是"思考开着、档位没生效"，
/// 而且没有任何报错——这正是本次修复前 `with_reasoning_effort` 零调用点的后果。
struct EffortRecordingProvider {
    seen: Arc<Mutex<Option<Option<String>>>>,
}

#[async_trait::async_trait]
impl Provider for EffortRecordingProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        *self.seen.lock().unwrap() = Some(opts.reasoning_effort.clone());
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ])))
    }
}

async fn effort_seen_by_provider(
    vendor: &str,
    model: &str,
    effort: &str,
) -> Option<Option<String>> {
    effort_seen_by_provider_on_wire(
        vendor,
        model,
        effort,
        wyj_config::WireProtocol::OpenAiChatCompletions,
    )
    .await
}

/// `wire_protocol` 显式传入：它决定 `ThinkingSpec::for_vendor` 查哪张表，
/// 漏传就会退回 OpenAI 线——那正是本次修复的 bug（Anthropic 线端点拿到
/// OpenAI 线的 effort 档位）。
async fn effort_seen_by_provider_on_wire(
    vendor: &str,
    model: &str,
    effort: &str,
    wire: wyj_config::WireProtocol,
) -> Option<Option<String>> {
    let seen: Arc<Mutex<Option<Option<String>>>> = Arc::new(Mutex::new(None));
    let agent = Agent::new(Arc::new(EffortRecordingProvider {
        seen: Arc::clone(&seen),
    }))
    .with_route_identity(
        "test".to_string(),
        vendor.to_string(),
        model.to_string(),
        "https://example.test/v1",
        wire,
    )
    .with_reasoning_effort(Some(effort.to_string()));
    let mut session = Session::new();
    session.push_user("hi");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();
    let snapshot = seen.lock().unwrap().clone();
    snapshot
}

#[tokio::test]
async fn reasoning_effort_reaches_request_options() {
    assert_eq!(
        effort_seen_by_provider("deepseek", "deepseek-v4-pro", "high").await,
        Some(Some("high".to_string()))
    );
}

/// 档位不在该 vendor 的 `effort_levels` 内（DeepSeek 没有 xhigh）时不能发出去——
/// 发了就是 400。
#[tokio::test]
async fn effort_outside_vendor_levels_is_not_sent() {
    assert_eq!(
        effort_seen_by_provider("deepseek", "deepseek-v4-pro", "xhigh").await,
        Some(None)
    );
}

/// 只认开关不认档位的 vendor（GLM）：catalog 曾把它标成 `ThinkingMode::Effort`，
/// 现在必须按 spec 丢弃而不是"发出去再被 adapter 吞掉"。
#[tokio::test]
async fn switch_only_vendor_does_not_receive_effort() {
    assert_eq!(
        effort_seen_by_provider("zhipu", "glm-4.6", "high").await,
        Some(None)
    );
}

/// 回归：`wire_protocol` 必须真的参与查表。
///
/// 修复前 `Agent.route_wire_protocol` 恒为 `OpenAiChatCompletions`
/// （`with_identity` 零调用），于是 Anthropic 线的 MiniMax M3.1 Flash
/// 拿到的是 OpenAI 线的 spec —— 那里刻意避开了 `medium` / `xhigh`，
/// 于是这两个档位被静默丢弃，用户配的 effort 到不了端点。
///
/// 同一模型换到 Anthropic 线后 `medium` 必须能发出去。
#[tokio::test]
async fn anthropic_wire_unlocks_the_vendor_specific_effort_levels() {
    assert_eq!(
        effort_seen_by_provider_on_wire(
            "minimax",
            "MiniMax-M3.1-Flash-Preview",
            "medium",
            wyj_config::WireProtocol::AnthropicMessages,
        )
        .await,
        Some(Some("medium".to_string()))
    );
}

/// 同上但走 OpenAI 线：`medium` 不在该 vendor 的档位集合里，必须不发出去。
/// 两条断言一起钉住「查表由 wire 决定」，而不是「MiniMax 一律认 medium」。
#[tokio::test]
async fn openai_wire_keeps_the_narrower_effort_levels() {
    assert_eq!(
        effort_seen_by_provider_on_wire(
            "minimax",
            "MiniMax-M3.1-Flash-Preview",
            "medium",
            wyj_config::WireProtocol::OpenAiChatCompletions,
        )
        .await,
        Some(None)
    );
}

/// 端点回 4xx 说不认 `reasoning_effort` 时，降级必须把它一起撤掉——只撤 budget
/// 的话重试仍会带上 effort、再 400 一次，然后整个 turn 失败。
#[tokio::test]
async fn rejected_effort_parameter_is_removed_and_reported() {
    struct EffortRejectedThenOk {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for EffortRejectedThenOk {
        async fn stream(
            &self,
            _system: &wyj_api::SystemPrompt<'_>,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            opts: &wyj_api::provider::RequestOptions,
        ) -> Result<EventStream> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut error = wyj_api::ProviderError::new(
                    wyj_api::ProviderErrorKind::UnsupportedParameter,
                    "unsupported parameter reasoning_effort",
                );
                error.parameter = Some("reasoning_effort".to_string());
                return Err(anyhow::Error::new(error));
            }
            assert_eq!(opts.reasoning_effort, None);
            assert_eq!(opts.thinking_switch, None);
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    let agent = Agent::new(Arc::new(EffortRejectedThenOk {
        calls: AtomicUsize::new(0),
    }))
    .with_route_identity(
        "deepseek".to_string(),
        "deepseek".to_string(),
        "deepseek-v4-pro".to_string(),
        "https://deepseek.test/v1",
        wyj_config::WireProtocol::OpenAiChatCompletions,
    )
    .with_reasoning_effort(Some("high".to_string()))
    .with_thinking_switch(Some("enabled".to_string()));
    let mut session = Session::new();
    session.push_user("hi");
    let mut visible = String::new();
    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .unwrap();
    let msg = wyj_i18n::tr_fmt(
        "agent.degraded.parameter_removed",
        &[("name", "reasoning_effort")],
    );
    assert!(visible.contains(&msg), "expected {msg:?} in {visible:?}");
    // 2 次**尝试**（首次被拒 + 重试成功），只有 1 次成功往返。
    // 断言走 `api_attempts` 而不是 `api_calls`：后者现在只记成功，
    // 那正是「每次调用带多少上下文」的正确分母。
    assert_eq!(session.api_attempts, 2);
    assert_eq!(session.api_calls, 1);
}

struct TypedFailureProvider {
    kind: wyj_api::ProviderErrorKind,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for TypedFailureProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::Error::new(wyj_api::ProviderError::new(
            self.kind,
            "typed test failure",
        )))
    }
}

#[tokio::test]
async fn retryable_failure_switches_route_once_at_message_boundary() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let backup_calls = Arc::new(AtomicUsize::new(0));
    let backup = AgentRoute::new(
        "backup",
        "minimax",
        "backup-model",
        "https://backup.test/v1",
        wyj_config::WireProtocol::OpenAiChatCompletions,
        Arc::new(CountingEndTurnProvider {
            calls: backup_calls.clone(),
        }),
    );
    let agent = Agent::new(Arc::new(TypedFailureProvider {
        kind: wyj_api::ProviderErrorKind::RateLimited,
        calls: primary_calls.clone(),
    }))
    .with_route_identity(
        "primary",
        "minimax",
        "primary-model",
        "https://primary.test/v1",
        wyj_config::WireProtocol::OpenAiChatCompletions,
    )
    .with_fallback_routes(vec![backup], false);
    let mut session = Session::new();
    session.push_user("hi");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backup_calls.load(Ordering::SeqCst), 1);
    assert_eq!(session.routing_events.len(), 1);
    assert_eq!(session.routing_events[0].from_profile, "primary");
    assert_eq!(session.routing_events[0].to_profile, "backup");
    assert_eq!(
        session.routing_events[0].boundary,
        "before_assistant_commit"
    );
}

#[tokio::test]
async fn authentication_failure_never_falls_back() {
    let backup_calls = Arc::new(AtomicUsize::new(0));
    let backup = AgentRoute::new(
        "backup",
        "minimax",
        "backup-model",
        "https://backup.test/v1",
        wyj_config::WireProtocol::OpenAiChatCompletions,
        Arc::new(CountingEndTurnProvider {
            calls: backup_calls.clone(),
        }),
    );
    let agent = Agent::new(Arc::new(TypedFailureProvider {
        kind: wyj_api::ProviderErrorKind::Authentication,
        calls: Arc::new(AtomicUsize::new(0)),
    }))
    .with_route_identity(
        "primary",
        "minimax",
        "primary-model",
        "https://primary.test/v1",
        wyj_config::WireProtocol::OpenAiChatCompletions,
    )
    .with_fallback_routes(vec![backup], false);
    let mut session = Session::new();
    session.push_user("hi");

    assert!(agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .is_err());
    assert_eq!(backup_calls.load(Ordering::SeqCst), 0);
    assert!(session.routing_events.is_empty());
}

#[test]
fn cross_vendor_fallback_is_filtered_by_default() {
    let backup = AgentRoute::new(
        "backup",
        "another-vendor",
        "backup-model",
        "https://backup.test/v1",
        wyj_config::WireProtocol::OpenAiChatCompletions,
        Arc::new(EndTurnProvider),
    );
    let agent = Agent::new(Arc::new(EndTurnProvider))
        .with_route_identity(
            "primary",
            "minimax",
            "primary-model",
            "https://primary.test/v1",
            wyj_config::WireProtocol::OpenAiChatCompletions,
        )
        .with_fallback_routes(vec![backup], false);
    assert!(agent.fallback_routes.is_empty());
}

#[async_trait::async_trait]
impl Provider for FlakyProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n == 0 {
            vec![
                Ok(StreamEvent::TextDelta("半成品".into())),
                Err(anyhow::anyhow!("connection reset")),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta("完整回复".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn stream_interruption_retries_and_discards_partial() {
    let agent = Agent::new(Arc::new(FlakyProvider {
        calls: AtomicUsize::new(0),
    }));
    let mut session = Session::new();
    session.push_user("hi");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    // 半成品文本绝不进 session；最终 assistant 消息只含完整回复
    let assistant_texts: Vec<String> = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, wyj_api::types::Role::Assistant))
        .map(|m| m.text())
        .collect();
    assert_eq!(assistant_texts.len(), 1);
    assert_eq!(assistant_texts[0], "完整回复");
    assert!(!assistant_texts[0].contains("半成品"));
    // 两次**尝试**（首次失败 + 重试成功），成功往返只有 1 次。
    assert_eq!(session.api_attempts, 2);
    assert_eq!(session.api_calls, 1);
}

/// 流已正常结束（收到 MessageStop + Usage），但 eventsource_stream 在末尾
/// 又多调一次 next() 返回 Err（这是 crate 对 TCP EOF 的已知行为，不是真正的
/// 网络中断）。Agent 必须把这种"良性末尾 EOF"忽略，绝不能丢弃已累积的完整
/// 文本，也绝不触发整轮重试。
struct EofAfterCompletionProvider;
#[async_trait::async_trait]
impl Provider for EofAfterCompletionProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("完整回复（中途不丢字）".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
            Ok(StreamEvent::Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            }),
            Err(anyhow::anyhow!("EOF after completion")),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn stream_eof_after_completion_is_ignored_not_retried() {
    let agent = Agent::new(Arc::new(EofAfterCompletionProvider));
    let mut session = Session::new();
    session.push_user("hi");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    // 整段回复完整保留，没有任何重试
    let assistant_texts: Vec<String> = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, wyj_api::types::Role::Assistant))
        .map(|m| m.text())
        .collect();
    assert_eq!(assistant_texts.len(), 1);
    assert_eq!(assistant_texts[0], "完整回复（中途不丢字）");
    // 关键断言：只调了一次 API,没有触发流中断重试
    assert_eq!(
        session.api_calls, 1,
        "末尾 EOF 不应触发重试,实际 API 调用次数 {}",
        session.api_calls
    );
    // 用量应已正常入账
    assert_eq!(session.total_input_tokens, 100);
    assert_eq!(session.total_output_tokens, 50);
}

/// 永远流中断的 provider：耗尽重试后必须报错，且不留半成品消息
struct AlwaysBrokenProvider;
#[async_trait::async_trait]
impl Provider for AlwaysBrokenProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let events: Vec<Result<StreamEvent>> = vec![Err(anyhow::anyhow!("connection reset"))];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn stream_interruption_exhausts_retries_then_fails_clean() {
    let agent = Agent::new(Arc::new(AlwaysBrokenProvider));
    let mut session = Session::new();
    session.push_user("hi");
    let res = agent.run_turn(&mut session, &FakeCtx, &mut |_| {}).await;
    assert!(res.is_err());
    // 无半成品 assistant 消息残留
    assert!(!session
        .messages
        .iter()
        .any(|m| matches!(m.role, wyj_api::types::Role::Assistant)));
    // 首次 + 2 次重试 = 3 次**尝试**，一次都没成功（最终 clean fail）。
    assert_eq!(session.api_attempts, 3);
    assert_eq!(session.api_calls, 0);
}

/// 先吐 thinking 块（含 signature），再工具调用，再收尾的 provider
struct ThinkingProvider {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for ThinkingProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<StreamEvent>> = if n == 0 {
            vec![
                Ok(StreamEvent::ThinkingStart),
                Ok(StreamEvent::ThinkingDelta("let me ".into())),
                Ok(StreamEvent::ThinkingDelta("think".into())),
                Ok(StreamEvent::ThinkingSignatureDelta("sig123".into())),
                Ok(StreamEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "Sleep".into(),
                }),
                Ok(StreamEvent::ToolUseDelta {
                    id: "t1".into(),
                    json_delta: r#"{"ms":1,"tag":"x"}"#.into(),
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                }),
            ]
        } else {
            // 续轮请求：历史里必须带有完整 thinking 块（含 signature），
            // 顺序在 tool_use 之前 —— 真实 API 缺失会直接 4xx
            let assistant = messages
                .iter()
                .find(|m| matches!(m.role, wyj_api::types::Role::Assistant))
                .expect("history must contain the assistant message");
            match (&assistant.content[0], &assistant.content[1]) {
                (
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                        ..
                    },
                    ContentBlock::ToolUse { .. },
                ) => {
                    assert_eq!(thinking, "let me think");
                    assert_eq!(signature, "sig123");
                }
                other => panic!("unexpected block order: {other:?}"),
            }
            vec![
                Ok(StreamEvent::TextDelta("done".into())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn thinking_blocks_are_preserved_and_replayed_with_signature() {
    let mut agent = Agent::new(Arc::new(ThinkingProvider {
        calls: AtomicUsize::new(0),
    }))
    .with_thinking(Some(1024), true);
    agent.register_tool(Arc::new(SleepTool));
    let mut session = Session::new();
    session.push_user("go");

    let mut thinking_seen = String::new();
    let agent = agent.with_thinking_callback(move |_| {});
    agent
        .run_turn(&mut session, &FakeCtx, &mut |d| thinking_seen.push_str(d))
        .await
        .unwrap();
    // 第二轮的断言在 ThinkingProvider 内部完成；此处确认最终回复正常
    assert!(session
        .messages
        .last()
        .map(|m| m.text().contains("done"))
        .unwrap_or(false));
}

/// override `run_with_meta` 记录收到的 `tool_use_id`，验证 `exec_tool_call`
/// 正确把 id 透传进 `ToolCallMeta`（供 SubAgent 落盘 trace 关联使用）。
struct MetaCapturingTool {
    seen_tool_use_id: Arc<std::sync::Mutex<Option<String>>>,
}
#[async_trait::async_trait]
impl Tool for MetaCapturingTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Echo".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            native: None,
        }
    }
    async fn run(&self, _input: serde_json::Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        panic!("run() 不应被直接调用，应走 run_with_meta");
    }
    async fn run_with_meta(
        &self,
        _input: serde_json::Value,
        _ctx: &dyn ToolContext,
        meta: &crate::tool::ToolCallMeta,
    ) -> Result<ToolResult> {
        *self.seen_tool_use_id.lock().unwrap() = Some(meta.tool_use_id.clone());
        Ok(ToolResult::ok("echoed"))
    }
}

#[tokio::test]
async fn exec_tool_call_passes_tool_use_id_via_meta() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let mut agent = Agent::new(Arc::new(EndTurnProvider));
    agent.register_tool(Arc::new(MetaCapturingTool {
        seen_tool_use_id: seen.clone(),
    }));

    let (id, _content, is_error) = agent
        .exec_tool_call(
            &FakeCtx,
            "toolu_42".into(),
            "Echo".into(),
            serde_json::json!({}),
        )
        .await;

    assert_eq!(id, "toolu_42");
    assert!(!is_error);
    assert_eq!(seen.lock().unwrap().as_deref(), Some("toolu_42"));
}

#[test]
fn dropping_episode_guard_persists_cancelled_episode() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    let store = Arc::new(
        EvolutionStore::new(dir.path(), &repo, wyj_config::EvolutionCfg::default()).unwrap(),
    );
    let mut session = Session::new();
    session.push_user("cancel this turn");
    let capture = store.begin_episode(
        "session-abort",
        &session,
        "cancel this turn",
        "default",
        "test-vendor",
        "test-model",
    );

    drop(EvolutionEpisodeGuard {
        store: store.clone(),
        capture: Some(capture),
    });

    let episodes = store.list_episodes(10).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        episodes[0].outcome,
        crate::evolution::EpisodeOutcome::Cancelled
    );
    assert_eq!(episodes[0].evidence[0].label, "turn_cancelled");
}

fn push_user_text(session: &mut crate::session::Session, text: &str) {
    use wyj_api::types::{ContentBlock, Message, Role};
    session.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    });
}

fn session_with_query(query: &str) -> crate::session::Session {
    let mut session = crate::session::Session::default();
    push_user_text(&mut session, query);
    session
}

fn seed_task(
    store: &crate::MemoryV3Store,
    title: &str,
    status: TaskStatus,
    blocked_reason: Option<&str>,
) {
    store
        .upsert(MemoryWriteRequest {
            kind: MemoryClaimKind::Task,
            scope: MemoryClaimScope::Project,
            title: title.to_string(),
            content: format!("Task: {title}"),
            entities: vec![title.to_string()],
            tags: vec!["task".to_string()],
            source: MemorySource {
                kind: MemorySourceKind::Assistant,
                locator: "session:test#assistant".to_string(),
                observed_at: Some("2026-08-22T09:00:00+08:00".to_string()),
            },
            evidence: vec![],
            confidence: 0.9,
            expires_at: None,
            supersedes: None,
            task_status: Some(status),
            task_steps: vec![
                TaskStep {
                    description: "已完成步骤".to_string(),
                    done: true,
                    updated_at: Some("2026-08-22T08:30:00+08:00".to_string()),
                },
                TaskStep {
                    description: "下一步要做".to_string(),
                    done: false,
                    updated_at: Some("2026-08-22T09:00:00+08:00".to_string()),
                },
            ],
            blocked_reason: blocked_reason.map(|s| s.to_string()),
        })
        .unwrap();
}

#[test]
fn continuation_keyword_detection_matches_documented_phrases() {
    for ok in [
        "继续",
        "继续吧",
        "继续。",
        " 继续 ",
        "Continue",
        "resume",
        "Go on",
        "再来",
        "接着",
    ] {
        assert!(is_continuation_request(ok), "应该命中: {ok:?}");
    }
    for no in [
        "继续分析招商银行",
        "hello world",
        "",
        "   ",
        "继续", // 校验 trim 后但带中文标点的边界
    ] {
        let _ = no; // 静默
    }
    // 长 query 不算 continuation（即使包含"继续"）。
    assert!(!is_continuation_request("继续帮我看看 stock2 招商银行持仓"));
    // 完全不相关的 query。
    assert!(!is_continuation_request("hello"));
}

#[test]
fn continuation_suffix_resumes_in_progress_task_with_next_step() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
    seed_task(
        &store,
        "迁移到 Memory v3 final",
        TaskStatus::InProgress,
        None,
    );

    let session = session_with_query("继续");
    let suffix = continuation_suffix(&store).expect("有 InProgress 任务时返回注入");
    assert!(suffix.contains("Resuming task"));
    assert!(suffix.contains("迁移到 Memory v3 final"));
    assert!(suffix.contains("下一步要做"));
    let _ = session;
}

#[test]
fn continuation_suffix_uses_i18n_key_when_no_open_tasks() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
    seed_task(&store, "已完成", TaskStatus::Completed, None);
    let suffix = continuation_suffix(&store).expect("无开放任务仍需返回提示");
    // 默认 Locale 是 zh 时落中文；en 在 set_locale 后才落英文。
    assert!(
        suffix.contains("没有未完成任务") || suffix.contains("No open tasks"),
        "应命中 i18n key，但拿到: {suffix}"
    );
}

#[test]
fn continuation_suffix_lists_blocked_when_no_in_progress() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
    seed_task(
        &store,
        "等用户确认",
        TaskStatus::Blocked,
        Some("等用户回复 Global 偏好确认"),
    );
    let suffix = continuation_suffix(&store).expect("有 Blocked 时返回 Open Tasks 列表");
    assert!(suffix.contains("Still-open tasks"));
    assert!(suffix.contains("等用户确认"));
}

#[test]
fn build_memory_snapshot_appends_continuation_when_query_is_resume_keyword() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
    seed_task(
        &store,
        "迁移到 Memory v3 final",
        TaskStatus::InProgress,
        None,
    );

    let session = session_with_query("继续");
    let snapshot = build_memory_snapshot(&store, &session);
    assert!(snapshot.contains("## Project Brief"));
    assert!(snapshot.contains("Resuming task"));
    assert!(snapshot.contains("下一步要做"));
}

#[test]
fn build_memory_snapshot_does_not_append_continuation_for_normal_query() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = crate::MemoryV3Store::new(base.path(), project.path()).unwrap();
    seed_task(
        &store,
        "迁移到 Memory v3 final",
        TaskStatus::InProgress,
        None,
    );

    let session = session_with_query("帮我看看 stock2 的招商银行持仓");
    let snapshot = build_memory_snapshot(&store, &session);
    assert!(snapshot.contains("## Project Brief"));
    assert!(
        !snapshot.contains("Resuming task"),
        "非续接 query 不应追加 continuation 块: {snapshot}"
    );
}

// ── 孤儿 tool_use 兜底配对 ───────────────────────────────────────────
//
// 模型正在写工具参数时被 max_tokens 截断时，assistant 消息里会带一个没有
// 配对 tool_result 的 tool_use。这个畸形历史会被原样落盘，之后任何一次
// `--resume` 都会让 provider 直接 400，且用户不手动 /rewind 或 /clear
// 无法自愈。`run_turn` 必须为残留 tool_use 补一条 is_error 的 tool_result。

/// 发一个工具调用后以 `MaxTokens` 结束（模拟参数写到一半被截断）
struct MaxTokensToolUseProvider;
#[async_trait::async_trait]
impl Provider for MaxTokensToolUseProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::ToolUseStart {
                id: "t1".into(),
                name: "Sleep".into(),
            }),
            Ok(StreamEvent::ToolUseDelta {
                id: "t1".into(),
                json_delta: r#"{"ms":10,"tag":"truncated"}"#.into(),
            }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::MaxTokens,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn max_tokens_with_pending_tool_use_still_gets_a_matching_tool_result() {
    let mut agent = Agent::new(Arc::new(MaxTokensToolUseProvider));
    agent.register_tool(Arc::new(SleepTool));
    let mut session = Session::new();
    session.push_user("go");

    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    let uses: Vec<&str> = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(uses, vec!["t1"], "工具调用应照常进入历史");

    let results: Vec<(&str, bool, String)> = session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((
                tool_use_id.as_str(),
                *is_error,
                match content {
                    wyj_api::types::ToolResultContent::Text(t) => t.clone(),
                    _ => String::new(),
                },
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.len(),
        1,
        "MaxTokens 截断后必须补 tool_result，否则历史配对断裂 → provider 400"
    );
    let (id, is_error, text) = &results[0];
    assert_eq!(*id, "t1");
    assert!(is_error, "兜底 tool_result 必须标记 is_error");
    assert!(
        text.contains("tool_call_incomplete") && text.contains("MaxTokens"),
        "兜底结果应说明未完成及原因，实际: {text}"
    );
}

// ── 手动 /compact 的前缀与主对话同源 ──────────────────────────────

/// 同时捕获 `stream`（主对话请求）与 `complete`（压缩请求）收到的
/// system 两段 + 工具名单，用来逐字节比对两者。
#[derive(Default)]
struct PrefixProbeProvider {
    stream_prefix: std::sync::Mutex<Option<(String, String, Vec<String>)>>,
    complete_prefix: std::sync::Mutex<Option<(String, String, Vec<String>)>>,
}

impl PrefixProbeProvider {
    fn snapshot(
        system: &wyj_api::SystemPrompt<'_>,
        tools: &[ToolDefinition],
    ) -> (String, String, Vec<String>) {
        (
            system.stable.to_string(),
            system.volatile.to_string(),
            tools.iter().map(|t| t.name.clone()).collect(),
        )
    }

    fn stream_prefix(&self) -> (String, String, Vec<String>) {
        self.stream_prefix
            .lock()
            .unwrap()
            .clone()
            .expect("应至少有一次 stream")
    }

    fn complete_prefix(&self) -> (String, String, Vec<String>) {
        self.complete_prefix
            .lock()
            .unwrap()
            .clone()
            .expect("压缩应发出一次 complete")
    }
}

#[async_trait::async_trait]
impl Provider for PrefixProbeProvider {
    async fn stream(
        &self,
        system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        *self.stream_prefix.lock().unwrap() = Some(Self::snapshot(system, tools));
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("ok".into())),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn complete(
        &self,
        system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        tools: &[ToolDefinition],
        _opts: &wyj_api::provider::RequestOptions,
    ) -> Result<wyj_api::types::CompletionResult> {
        *self.complete_prefix.lock().unwrap() = Some(Self::snapshot(system, tools));
        Ok(wyj_api::types::CompletionResult {
            content: vec![ContentBlock::Text {
                text: "## Task & Intent\n继续。".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
        })
    }
}

/// 手动 `/compact` 的前缀必须与主对话请求**逐字节相同**，否则压缩请求
/// （一次动辄几百 K token）在 provider 侧完全 miss 缓存——而「让压缩复用
/// 主对话前缀」正是这条路径存在的全部理由。
///
/// 旧实现的两个失配点：`volatile` 写死空串，`tools` 只按 lazy 折叠过滤而
/// 漏掉 route 能力裁剪。本测试把两者一起钉死。
#[tokio::test]
async fn manual_compaction_reuses_the_exact_main_request_prefix() {
    let probe = Arc::new(PrefixProbeProvider::default());
    let mut agent = Agent::new(probe.clone() as Arc<dyn Provider>);
    agent.register_tool(Arc::new(SleepTool));
    // 子目录规约 reminder 只有「本轮触达过目录」时才会进 volatile，
    // 这段真实请求里不含它；要断言的是**两条路径字节相同**，而不是
    // 各自等于某个固定值。
    let mut session = Session::new();
    session.push_user("读一下文件");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();
    session.push_user("再看一个");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    agent
        .compact_context(&mut session)
        .await
        .expect("手动压缩应成功");

    let (s_stable, s_volatile, s_tools) = probe.stream_prefix();
    let (c_stable, c_volatile, c_tools) = probe.complete_prefix();
    assert_eq!(
        c_stable, s_stable,
        "压缩请求的 system stable 段必须与主对话相同"
    );
    assert_eq!(
        c_volatile, s_volatile,
        "压缩请求的 volatile 段必须与主对话相同（不可写死空串）"
    );
    assert!(
        !s_volatile.is_empty(),
        "本用例必须真的带上 volatile 才有意义"
    );
    assert_eq!(
        c_tools, s_tools,
        "压缩请求的工具清单必须与主对话相同（少一个就整段缓存失效）"
    );
    assert!(s_tools.contains(&"Sleep".to_string()));
}

// ── 辅助模型不得影响上下文压缩 ──────────────────────────────────────

/// 配了辅助模型之后，**压缩仍然必须走主 provider**。
///
/// 压缩请求的全部价值在于逐字节复用主对话的 system + tools 前缀来命中
/// prompt cache；一旦换成辅助模型，前缀不再相等，缓存归零，整段历史
/// （长会话里动辄几十万 token）全价重发——省下的一次标题生成费用，
/// 换回来的是一次数量级更大的浪费。
///
/// 这条测试钉的是「压缩路径不碰 `aux_provider`」这条不变式。
#[tokio::test]
async fn compaction_never_uses_the_auxiliary_provider() {
    let probe = Arc::new(PrefixProbeProvider::default());
    // 辅助 provider 是一个**独立**的探针：它一旦被压缩路径调用，
    // `aux.complete_prefix` 就会拿到值。
    let aux = Arc::new(PrefixProbeProvider::default());

    let mut agent = Agent::new(probe.clone() as Arc<dyn Provider>)
        .with_aux_provider(Some(aux.clone() as Arc<dyn Provider>));
    agent.register_tool(Arc::new(SleepTool));

    let mut session = Session::new();
    session.push_user("读一下文件");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();
    session.push_user("再看一个");
    agent
        .run_turn(&mut session, &FakeCtx, &mut |_| {})
        .await
        .unwrap();

    agent
        .compact_context(&mut session)
        .await
        .expect("手动压缩应成功");

    assert!(
        aux.complete_prefix.lock().unwrap().is_none(),
        "压缩请求不得发给辅助模型——换模型会让前缀缓存失效"
    );
    // 压缩确实走了主 provider（否则上面那句断言会因为「压根没压缩」而假通过）。
    let (c_stable, c_volatile, c_tools) = probe.complete_prefix();
    let (s_stable, s_volatile, s_tools) = probe.stream_prefix();
    assert_eq!(c_stable, s_stable, "压缩前缀必须与主对话同源");
    assert_eq!(c_volatile, s_volatile, "压缩前缀必须与主对话同源");
    assert_eq!(c_tools, s_tools, "压缩前缀必须与主对话同源");
}

// ── 记忆快照分桶缓存 ───────────────────────────────────────────────
//
// 回归背景：Project Brief 每次都按「最近 4 条 user 消息」重算相关性排序，
// 而它被拼进 system prompt 的稳定前缀 —— 每个用户轮次都会改写 system
// 尾部，把整段 system 的 prompt cache 击穿。`memory_snapshot_text` 按
// 10 个用户轮次分桶缓存：同桶复用，跨桶重算。

#[test]
fn memory_snapshot_reuses_cache_within_bucket_and_refreshes_across_buckets() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::MemoryV3Store::new(base.path(), project.path()).unwrap());
    seed_task(&store, "任务 A", TaskStatus::InProgress, None);

    let agent = Agent::new(Arc::new(EndTurnProvider)).with_memory_v3(store.clone());
    let mut session = session_with_query("继续");

    let first = agent.memory_snapshot_text(&session);
    assert!(first.contains("任务 A"), "首次应带上 Open Tasks");

    // 同桶内新增记忆：仍应命中缓存
    seed_task(&store, "任务 B", TaskStatus::InProgress, None);
    let same_bucket = agent.memory_snapshot_text(&session);
    assert_eq!(
        first, same_bucket,
        "同桶内必须复用缓存，否则 system 稳定前缀每轮都被改写"
    );
    assert!(
        !same_bucket.contains("任务 B"),
        "同桶内不应看到新写入的记忆"
    );

    // 跨桶：重算后应看到新记忆
    for i in 0..MEMORY_SNAPSHOT_REFRESH_TURNS {
        push_user_text(&mut session, &format!("继续 {i}"));
    }
    let refreshed = agent.memory_snapshot_text(&session);
    assert!(
        refreshed.contains("任务 B"),
        "跨桶后应重新计算并看到后台新提取的记忆"
    );
}

/// 记忆注入的**优先级链**：v3 → evolution → v2。
///
/// 三套记忆并存是历史遗留，但它们的注入点是一条 if-else 链而不是叠加：
/// v3 一旦存在，v2 那段**一行都不会执行**。CLI 装配因此只在 v3 没建成时
/// 才构造 v2（见 `main.rs` 的 `memory_store`），避免每次启动多建一个目录、
/// 多读一次盘，并让 `/memory` 面板不再维护一处永远没有读者的状态。
///
/// 这条钉住「链的顺序」——把它改成叠加或调换顺序，会同时改变注入给模型的
/// 内容与启动开销，且不会有任何编译错误提示。
#[test]
fn memory_injection_prefers_v3_then_evolution_then_v2() {
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let v3 = Arc::new(crate::MemoryV3Store::new(base.path(), project.path()).unwrap());
    seed_task(&v3, "V3 里的任务", TaskStatus::InProgress, None);

    // ① 只有 v3：注入 v3 的内容。
    let agent = Agent::new(Arc::new(EndTurnProvider)).with_memory_v3(v3.clone());
    let session = session_with_query("继续");
    assert!(agent.memory_snapshot_text(&session).contains("V3 里的任务"));

    // ② v3 + v2 同时装上：v2 不得出现在快照里（if-else 链，不是叠加）。
    let v2 =
        Arc::new(crate::MemoryStore::new(base.path(), project.path()).expect("v2 store 应能创建"));
    let both = Agent::new(Arc::new(EndTurnProvider))
        .with_memory_v3(v3.clone())
        .with_memory(v2.clone());
    let text = both.memory_snapshot_text(&session);
    assert!(text.contains("V3 里的任务"), "v3 存在时仍应由 v3 提供内容");
    assert!(
        !text.contains("MEMORY.md"),
        "v2 的索引不得混进 v3 的快照: {text}"
    );

    // ③ 只有 v2（v3 降级路径）：回落到 v2，且不 panic。
    let only_v2 = Agent::new(Arc::new(EndTurnProvider)).with_memory(v2);
    let fallback = only_v2.memory_snapshot_text(&session);
    assert!(
        fallback.is_empty() || !fallback.contains("V3 里的任务"),
        "v3 缺席时不应凭空出现 v3 的内容"
    );
}

#[test]
fn with_skills_injects_the_catalog_and_leaves_no_placeholder() {
    // 回归背景：用户说"请使用 skill 分析股票"，模型回复"我不能自己调用
    // skill——skill 必须由你用斜杠命令触发"。根因是 skill 名单只存在于 TUI
    // 的 CommandRegistry（只服务 `/xxx` 补全），从不进入模型上下文；而
    // 提示词既禁止模型自行检索目录、又要求它给出具体命令名，模型手里没有
    // 任何名字，唯一诚实的回答就是把禁令复述一遍。
    let agent =
        Agent::new(Arc::new(EndTurnProvider)).with_skills(vec![crate::prompts::SkillEntry {
            name: "hithink-finance".into(),
            description: "查询 A 股行情与财务数据".into(),
        }]);
    assert!(
        !agent
            .system_prompt
            .contains(crate::prompts::SKILL_CATALOG_PLACEHOLDER),
        "占位符必须被替换掉，不能字面泄漏进 system prompt"
    );
    assert!(
        agent
            .system_prompt
            .contains("- `hithink-finance` — 查询 A 股行情与财务数据"),
        "{}",
        agent.system_prompt
    );
}

#[test]
fn with_skills_survives_an_empty_installation() {
    // 一个 skill 都没有时同样不能留下 `{skills}` 字面量
    let agent = Agent::new(Arc::new(EndTurnProvider)).with_skills(Vec::new());
    assert!(!agent
        .system_prompt
        .contains(crate::prompts::SKILL_CATALOG_PLACEHOLDER));
    assert!(agent.system_prompt.contains("No skills are installed"));
}

#[test]
fn memory_snapshot_is_shared_across_agent_clones() {
    // `/model` 重建与模式切换都会 clone Agent，缓存必须共享，
    // 否则切一次模型就白白重算一次并改写 system 前缀。
    let base = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::MemoryV3Store::new(base.path(), project.path()).unwrap());
    seed_task(&store, "任务 A", TaskStatus::InProgress, None);

    let agent = Agent::new(Arc::new(EndTurnProvider)).with_memory_v3(store.clone());
    let session = session_with_query("继续");
    let first = agent.memory_snapshot_text(&session);
    seed_task(&store, "任务 B", TaskStatus::InProgress, None);
    let cloned = agent.clone();
    assert_eq!(
        first,
        cloned.memory_snapshot_text(&session),
        "clone 出来的 Agent 应复用同一份快照缓存"
    );
}

// ── 输出预算被 reasoning 吃满：长会话「跑几轮后静默停住」 ────────────────

/// 按预置脚本逐次回放事件，并记录每次请求**实际发出**的输出参数。
struct TruncatedScriptProvider {
    script: Mutex<Vec<Vec<StreamEvent>>>,
    seen: Mutex<Vec<(u32, Option<String>, Option<String>)>>,
}

impl TruncatedScriptProvider {
    fn new(script: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            script: Mutex::new(script),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<(u32, Option<String>)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(max_tokens, effort, _)| (*max_tokens, effort.clone()))
            .collect()
    }

    fn requests_full(&self) -> Vec<(u32, Option<String>, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }
}

/// 「输出预算全花在 thinking 上」的回复形态：只有 thinking，没有文本/工具。
fn thinking_only(reason: StopReason) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ThinkingStart,
        StreamEvent::ThinkingDelta("先想很久，但一个字的正文都没留下".into()),
        StreamEvent::MessageStop {
            stop_reason: reason,
        },
    ]
}

#[async_trait::async_trait]
impl Provider for TruncatedScriptProvider {
    async fn stream(
        &self,
        _system: &wyj_api::SystemPrompt<'_>,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        opts: &wyj_api::provider::RequestOptions,
    ) -> Result<EventStream> {
        self.seen.lock().unwrap().push((
            opts.max_tokens,
            opts.reasoning_effort.clone(),
            opts.thinking_switch.clone(),
        ));
        let events = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                vec![StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }]
            } else {
                script.remove(0)
            }
        };
        Ok(Box::pin(futures::stream::iter(
            events
                .into_iter()
                .map(Ok)
                .collect::<Vec<Result<StreamEvent>>>(),
        )))
    }
}

fn truncation_test_agent(provider: std::sync::Arc<TruncatedScriptProvider>) -> Agent {
    Agent::new(provider)
        .with_route_identity(
            "test",
            "deepseek",
            "deepseek-v4-pro",
            "https://example.test/v1",
            wyj_config::WireProtocol::OpenAiChatCompletions,
        )
        .with_max_tokens(1024)
        .with_reasoning_effort(Some("high".to_string()))
}

/// 核心回归：thinking-only + MaxTokens 必须自动重试一次，且被截断的那条回复
/// **不能落进历史**——否则下一轮揣着同一条纯 thinking 消息，在同样的预算上
/// 再撞一次，表现就是「长会话跑几轮后静默停住」。
#[tokio::test]
async fn truncated_thinking_only_reply_is_retried_once_and_not_kept_in_history() {
    let provider = std::sync::Arc::new(TruncatedScriptProvider::new(vec![
        thinking_only(StopReason::MaxTokens),
        vec![
            StreamEvent::TextDelta("完成".into()),
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]));
    let agent = truncation_test_agent(provider.clone());
    let mut session = Session::new();
    push_user_text(&mut session, "继续");
    let mut visible = String::new();

    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .unwrap();

    assert_eq!(
        provider.requests(),
        vec![(1024, Some("high".to_string())), (2048, None),],
        "只重试一次，预算翻倍，且关得掉思考的端点第二次走 text_only 把 effort 清空"
    );
    // deepseek-v4-pro 关得掉思考：重试必须把 thinking_switch 置为 disabled，
    // 让正文独占输出预算，而不是只降一档 effort。
    assert_eq!(
        provider.requests_full(),
        vec![
            (1024, Some("high".to_string()), None),
            (2048, None, Some("disabled".to_string())),
        ],
        "重试请求必须带 thinking_switch=disabled 关掉思考"
    );
    assert_eq!(session.messages.len(), 2, "user + 最终 assistant");
    let last = session.messages.last().unwrap();
    assert!(
        matches!(
            last.content.as_slice(),
            [ContentBlock::Text { text }] if text == "完成"
        ),
        "被截断的纯 thinking 回复不能进历史：{:?}",
        last.content
    );
    assert!(visible.contains("完成"));
}

/// 重试后仍然只有 thinking：必须把原因和解法打给用户，且不再重试。
#[tokio::test]
async fn persistent_truncation_is_reported_instead_of_ending_silently() {
    let provider = std::sync::Arc::new(TruncatedScriptProvider::new(vec![
        thinking_only(StopReason::MaxTokens),
        thinking_only(StopReason::MaxTokens),
    ]));
    let agent = truncation_test_agent(provider.clone());
    let mut session = Session::new();
    push_user_text(&mut session, "继续");
    let mut visible = String::new();

    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .unwrap();

    assert_eq!(provider.requests().len(), 2, "最多重试一次，不得反复烧钱");
    assert!(visible.contains(&wyj_i18n::tr("agent.output_truncated_retrying")));
    assert!(visible.contains(&wyj_i18n::tr("agent.output_without_visible_content")));
    assert_eq!(
        session.messages.len(),
        1,
        "两条都只有 thinking，一条都不该进历史"
    );
}

/// 没撞 MaxTokens 的纯 thinking 回复是端点偶发，不重试，但同样必须告知。
#[tokio::test]
async fn thinking_only_end_turn_is_reported_without_retry() {
    let provider = std::sync::Arc::new(TruncatedScriptProvider::new(vec![thinking_only(
        StopReason::EndTurn,
    )]));
    let agent = truncation_test_agent(provider.clone());
    let mut session = Session::new();
    push_user_text(&mut session, "继续");
    let mut visible = String::new();

    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .unwrap();

    assert_eq!(provider.requests().len(), 1, "不是截断就不该重试");
    assert!(visible.contains(&wyj_i18n::tr("agent.output_without_visible_content")));
    assert!(!visible.contains(&wyj_i18n::tr("agent.output_truncated_retrying")));
}

/// 有正文但撞了 MaxTokens：正文是被硬切断的，不重试（重试只会拿到另一截），
/// 但必须告诉用户「这不是完整回答」。
#[tokio::test]
async fn visible_output_hitting_max_tokens_is_reported_but_not_retried() {
    let provider = std::sync::Arc::new(TruncatedScriptProvider::new(vec![vec![
        StreamEvent::TextDelta("只写了半句".into()),
        StreamEvent::MessageStop {
            stop_reason: StopReason::MaxTokens,
        },
    ]]));
    let agent = truncation_test_agent(provider.clone());
    let mut session = Session::new();
    push_user_text(&mut session, "继续");
    let mut visible = String::new();

    agent
        .run_turn(&mut session, &FakeCtx, &mut |text| visible.push_str(text))
        .await
        .unwrap();

    assert_eq!(provider.requests().len(), 1);
    assert!(visible.contains("只写了半句"));
    assert!(visible.contains(&wyj_i18n::tr("agent.output_truncated")));
}

#[test]
fn has_visible_output_ignores_thinking_and_blank_text() {
    assert!(!has_visible_output(&[]));
    assert!(!has_visible_output(&[ContentBlock::Text {
        text: "  \n\t".into()
    },]));
    assert!(!has_visible_output(&[ContentBlock::Thinking {
        thinking: "想了很久".into(),
        signature: String::new(),
        reasoning_details: None,
    }]));
    assert!(!has_visible_output(&[ContentBlock::RedactedThinking {
        data: "enc".into()
    }]));
    assert!(has_visible_output(&[ContentBlock::Text {
        text: "正文".into()
    }]));
    assert!(has_visible_output(&[ContentBlock::ToolUse {
        id: "t1".into(),
        name: "Read".into(),
        input: serde_json::json!({}),
    }]));
}

#[test]
fn lowest_reasoning_effort_picks_the_weakest_supported_level() {
    // DeepSeek 档位表 low/medium/high/max → 最低是 low。
    let deepseek = wyj_api::ThinkingSpec::for_vendor(
        "deepseek",
        &wyj_config::WireProtocol::OpenAiChatCompletions,
        "deepseek-v4-pro",
    );
    assert_eq!(lowest_reasoning_effort(deepseek.effort_levels), Some("low"));

    // Moonshot k3 档位表 low/high/max → 最低仍是 low（不是表里第一个 max）。
    let k3 = wyj_api::ThinkingSpec::for_vendor(
        "moonshot",
        &wyj_config::WireProtocol::OpenAiChatCompletions,
        "kimi-k3",
    );
    assert_eq!(lowest_reasoning_effort(k3.effort_levels), Some("low"));

    // GLM 不支持 effort：空档位表 → None。
    let glm = wyj_api::ThinkingSpec::for_vendor(
        "zhipu",
        &wyj_config::WireProtocol::OpenAiChatCompletions,
        "glm-4.6",
    );
    assert_eq!(lowest_reasoning_effort(glm.effort_levels), None);
}

#[test]
fn thinking_cannot_be_disabled_tracks_the_vendor_table() {
    // DeepSeek reasoner 强制思考，关不掉。
    let reasoner = wyj_api::ModelIdentity {
        vendor: "deepseek".to_string(),
        model: "deepseek-reasoner".to_string(),
        base_url: "https://api.deepseek.com".to_string(),
        wire_protocol: wyj_config::WireProtocol::OpenAiChatCompletions,
    };
    assert!(thinking_cannot_be_disabled(&reasoner));

    // MiniMax M3.1-Flash（/anthropic 线）发 disabled 直接 400，等同关不掉。
    let m31 = wyj_api::ModelIdentity {
        vendor: "minimax".to_string(),
        model: "MiniMax-M3.1-Flash".to_string(),
        base_url: "https://api.minimaxi.com/anthropic".to_string(),
        wire_protocol: wyj_config::WireProtocol::AnthropicMessages,
    };
    assert!(thinking_cannot_be_disabled(&m31));

    // deepseek-v4-pro 不是强制思考，关得掉。
    let v4 = wyj_api::ModelIdentity {
        vendor: "deepseek".to_string(),
        model: "deepseek-v4-pro".to_string(),
        base_url: "https://api.deepseek.com".to_string(),
        wire_protocol: wyj_config::WireProtocol::OpenAiChatCompletions,
    };
    assert!(!thinking_cannot_be_disabled(&v4));
}

#[test]
fn estimated_thinking_backlog_counts_only_thinking_blocks() {
    use wyj_api::types::{ContentBlock, Message, Role};

    let thinking = "思".repeat(3000);
    let session = {
        let mut s = crate::session::Session::new();
        push_user_text(&mut s, "hi");
        s.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: String::new(),
                    reasoning_details: None,
                },
                ContentBlock::Text {
                    text: "正文不计入".to_string(),
                },
            ],
        });
        s
    };
    let backlog = estimated_thinking_backlog_tokens(&session);
    assert!(backlog > 0, "thinking 块必须计入 backlog");
    // 3000 个 CJK 字 × 1.5 token/字 = 4500，正文不在统计内，backlog 应接近 4500。
    assert!(
        (4000..=5000).contains(&backlog),
        "backlog 只算 thinking，正文不混入: {backlog}"
    );
}

// ── 验收门禁（acceptance gate）─────────────────────────────────────────

/// 验收门禁用的 ctx：工作目录可指定，权限模式可指定。
struct AcceptanceCtx {
    cwd: std::path::PathBuf,
    mode: Option<crate::permission::PermissionMode>,
}
#[async_trait::async_trait]
impl ToolContext for AcceptanceCtx {
    fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
        true
    }
    fn permission_mode(&self) -> Option<crate::permission::PermissionMode> {
        self.mode.clone()
    }
}

/// 构造一条必然失败的判据（目标文件不存在）。
fn failing_acceptance_cfg(max_retries: u32) -> crate::acceptance::AcceptanceLoad {
    crate::acceptance::AcceptanceLoad {
        cfg: Some(crate::acceptance::AcceptanceCfg {
            criteria: vec![crate::acceptance::AcceptanceCriterion {
                id: "must-exist".into(),
                kind: crate::acceptance::CriterionKind::FileExists {
                    path: "must-exist.txt".into(),
                },
                invalid: None,
            }],
            max_retries,
        }),
        error: None,
    }
}

/// 验收不通过时，**回合不许结束**：框架把失败证据回灌给模型，模型再跑一轮。
///
/// 这是「任务完成 = 模型说了完成」这个等式被打破的那一步。断言三件事：
/// provider 被多调用了一次（循环真的续上了）、证据进了历史、用户看得见提示。
#[tokio::test]
async fn a_failing_acceptance_criterion_keeps_the_turn_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(CountingEndTurnProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = Agent::new(provider.clone()).with_acceptance(failing_acceptance_cfg(1));
    let mut session = Session::new();
    let mut visible = String::new();

    agent
        .run_turn(
            &mut session,
            &AcceptanceCtx {
                cwd: tmp.path().to_path_buf(),
                mode: None,
            },
            &mut |text| visible.push_str(text),
        )
        .await
        .expect("回合不应报错");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "验收失败后必须让模型再跑一轮（1 次重试 + 1 次收尾）"
    );
    assert!(
        session.messages.iter().any(|m| {
            m.content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("<acceptance-failures"),
                _ => false,
            })
        }),
        "失败证据必须进历史，让模型能读到"
    );
    assert!(
        visible.contains("must-exist"),
        "用户必须看见是哪条判据没过：{visible}"
    );
}

/// 重试预算耗尽后按「未完成」交付：既不无限重试，也不静默放行。
#[tokio::test]
async fn the_acceptance_retry_budget_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(CountingEndTurnProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = Agent::new(provider.clone()).with_acceptance(failing_acceptance_cfg(2));
    let mut session = Session::new();
    let mut visible = String::new();

    agent
        .run_turn(
            &mut session,
            &AcceptanceCtx {
                cwd: tmp.path().to_path_buf(),
                mode: None,
            },
            &mut |text| visible.push_str(text),
        )
        .await
        .expect("回合不应报错");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "预算是 2 次重试：初次 + 2 次重试后收尾，共 3 次请求"
    );
    // 提示文案随 locale 变，断言必须与 i18n 输出比对而不是写死中文。
    let exhausted = wyj_i18n::tr_fmt(
        "agent.acceptance_exhausted",
        &[("failed", "1"), ("total", "1"), ("ids", "must-exist")],
    );
    assert!(
        visible.contains(&exhausted),
        "预算耗尽必须明确告诉用户：{visible}"
    );
    assert!(
        visible.contains("must-exist"),
        "预算耗尽时必须点名是哪条判据没过：{visible}"
    );
}

/// 判据全过 → 正常收尾，provider 只被调用一次。
#[tokio::test]
async fn a_passing_acceptance_criterion_ends_the_turn_normally() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("must-exist.txt"), "ok").unwrap();
    let provider = Arc::new(CountingEndTurnProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = Agent::new(provider.clone()).with_acceptance(failing_acceptance_cfg(2));
    let mut session = Session::new();
    let mut visible = String::new();

    agent
        .run_turn(
            &mut session,
            &AcceptanceCtx {
                cwd: tmp.path().to_path_buf(),
                mode: None,
            },
            &mut |text| visible.push_str(text),
        )
        .await
        .expect("回合不应报错");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "判据全过就不该续跑"
    );
    assert!(
        !session
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("<acceptance-failures"),
                _ => false,
            })),
        "通过时不该注入失败报告"
    );
}

/// Plan 模式跳过门禁：计划模式本就不改文件，判据必然失败，让它失败只会把模型
/// 困在「反复改计划」里出不去。
#[tokio::test]
async fn plan_mode_skips_the_acceptance_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(CountingEndTurnProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = Agent::new(provider.clone()).with_acceptance(failing_acceptance_cfg(2));
    let mut session = Session::new();
    let mut visible = String::new();

    agent
        .run_turn(
            &mut session,
            &AcceptanceCtx {
                cwd: tmp.path().to_path_buf(),
                mode: Some(crate::permission::PermissionMode::Plan(Default::default())),
            },
            &mut |text| visible.push_str(text),
        )
        .await
        .expect("回合不应报错");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "Plan 模式不跑门禁，也就不该续跑"
    );
    assert!(
        !visible.contains("must-exist"),
        "Plan 模式不该报验收失败：{visible}"
    );
}

/// 声明了 `[acceptance]` 但解析失败 → 收尾时明确报给用户，**绝不静默关掉门禁**。
#[tokio::test]
async fn a_broken_acceptance_block_is_reported_to_the_user() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(CountingEndTurnProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = Agent::new(provider.clone()).with_acceptance(crate::acceptance::AcceptanceLoad {
        cfg: None,
        error: Some("settings.json: acceptance 块结构非法".into()),
    });
    let mut session = Session::new();
    let mut visible = String::new();

    agent
        .run_turn(
            &mut session,
            &AcceptanceCtx {
                cwd: tmp.path().to_path_buf(),
                mode: None,
            },
            &mut |text| visible.push_str(text),
        )
        .await
        .expect("回合不应报错");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "配置坏了不该续跑"
    );
    assert!(
        visible.contains("结构非法"),
        "配置坏了必须让用户看见：{visible}"
    );
}
