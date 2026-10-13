//! 会话状态：消息历史 + 累计用量

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use wyj_api::types::{ContentBlock, Message, Role};

/// 子 Agent 的用量快照（不含缓存写入时为 0）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubUsage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

/// 子 Agent 用量的**共享累加器**。
///
/// 子 Agent 跑在独立的 `Session` 上（`sub_agent.rs` 的 `Session::new()`），
/// 它自己的 token 既不会进主会话的 `total_*`，也不会进
/// `AgentEvent::Usage`——`UsageDelta` 只在 TUI 主 Agent 路径发射。因此在
/// `/cost` 里看到的主会话用量**系统性漏掉整棵子 Agent 扇出子树**。
///
/// 机制：主会话在自己的 `ToolContext` 上挂一个本累加器，子 Agent 结束时把
/// 自己那一轮 `Session` 的计数加进来（`add`），主 Agent 在每批工具执行后
/// 调 [`SubUsageTotals::take`] 抽干计入 `Session` 的 `sub_*` 字段
/// （`Session::drain_sub_usage`）。
///
/// 用原子量而非 `Mutex` 是因为子 Agent 可能 `parallel_safe` 并发跑，多个
/// 子 Agent 会在同一批里同时累加；`take` 用 `swap(0)` 保证每个 token 只被
/// 主会话计入一次，天然幂等。
#[derive(Debug, Default)]
pub struct SubUsageTotals {
    input: AtomicU32,
    output: AtomicU32,
    cache_read: AtomicU32,
    cache_write: AtomicU32,
}

impl SubUsageTotals {
    pub fn add(&self, usage: SubUsage) {
        self.input.fetch_add(usage.input, Ordering::Relaxed);
        self.output.fetch_add(usage.output, Ordering::Relaxed);
        self.cache_read
            .fetch_add(usage.cache_read, Ordering::Relaxed);
        self.cache_write
            .fetch_add(usage.cache_write, Ordering::Relaxed);
    }

    /// 抽走并清空累计值。多次并发调用不会重复计账。
    pub fn take(&self) -> SubUsage {
        SubUsage {
            input: self.input.swap(0, Ordering::Relaxed),
            output: self.output.swap(0, Ordering::Relaxed),
            cache_read: self.cache_read.swap(0, Ordering::Relaxed),
            cache_write: self.cache_write.swap(0, Ordering::Relaxed),
        }
    }

    pub fn snapshot(&self) -> SubUsage {
        SubUsage {
            input: self.input.load(Ordering::Relaxed),
            output: self.output.load(Ordering::Relaxed),
            cache_read: self.cache_read.load(Ordering::Relaxed),
            cache_write: self.cache_write.load(Ordering::Relaxed),
        }
    }
}

/// 共享句柄别名，供 `ToolContext::sub_usage_sink` 使用。
pub type SharedSubUsage = Arc<SubUsageTotals>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RoutingEvent {
    pub timestamp: String,
    pub from_profile: String,
    pub to_profile: String,
    pub error_kind: wyj_api::ProviderErrorKind,
    /// v1.4.4 只允许在尚未提交 assistant 消息或工具执行结果的完整边界切换。
    pub boundary: String,
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub messages: Vec<Message>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// 累计命中 prompt 缓存的输入 token 数（按约 0.1x 价格计费，不计入
    /// `total_input_tokens`，单独统计以便 /cost 展示缓存命中情况）
    pub total_cache_read_tokens: u32,
    /// 累计写入 prompt 缓存的输入 token 数（按约 1.25x 价格计费，同样
    /// 不计入 `total_input_tokens`；不统计会系统性低估成本）
    pub total_cache_write_tokens: u32,
    /// 本会话累计发起的模型推理次数（每次 provider.stream 调用计 1），
    /// 供评测基准（WYJ_STATS_JSON）衡量任务完成所需的 API 往返数。
    pub api_calls: u32,
    /// **累计**随模型请求发送的工具 schema 估算 token（全会话求和，每次
    /// `provider.stream` 前累加一次）。它回答的是"本次会话为工具定义总共
    /// 付了多少"，不是"当前上下文里占了多少"。
    ///
    /// ⚠️ 不要拿它当单请求的稳态值展示：一次 200 次调用的会话会累到百万级
    /// （实测最高 1,457,192），而真实的单请求值只有 6,281。单请求口径看
    /// [`Self::context_audit`] 的 `tool_schema_tokens`——`/context` 面板用的
    /// 就是它。
    pub tool_schema_tokens: u32,
    /// **最近一次请求**实际发送的工具 schema 估算 token（稳态值）。
    ///
    /// 与 [`crate::compact::ContextAudit::tool_schema_tokens`] 同源同值，
    /// 单独存一份是为了让 JSON / 插件事件这类拿不到 `context_audit` 的消费
    /// 方也能拿到稳态口径，不必自己重算。
    pub tool_schema_tokens_last: u32,
    /// lazy schema 相对全量工具集合累计避免发送的估算 token。
    pub tool_schema_tokens_saved: u32,
    /// 子 Agent 扇出累计消耗的输入 token。与 `total_input_tokens` **不重叠**
    /// ——子 Agent 跑在自己的 `Session` 上，其用量由
    /// [`Self::drain_sub_usage`] 从 `ToolContext` 的共享累加器搬进来。
    pub sub_input_tokens: u32,
    /// 子 Agent 累计输出 token。
    pub sub_output_tokens: u32,
    /// 子 Agent 累计缓存命中 token。
    pub sub_cache_read_tokens: u32,
    /// 子 Agent 累计缓存写入 token（当前配置的第三方端点不上报，恒为 0，
    /// 与主会话的 `total_cache_write_tokens` 同因，详见 `/cost` 的说明行）。
    pub sub_cache_write_tokens: u32,
    /// 最近一次模型请求的上下文占用分解。与压缩决策**同源**，供 TUI 的
    /// `/context` 面板使用——旧实现里面板/状态栏自己调 `estimate_tokens`
    /// （只算 messages），与 `estimate_request_tokens`（含 system + tools +
    /// 输出预留）差出一万多 token，百分比和压缩时机对不上号。
    pub context_audit: crate::compact::ContextAudit,
    /// 本会话自动压缩发生次数（含因 provider 报 context 超限触发的强制压缩）。
    /// 压缩对用户不可见，除以数字之外必须让"自动管理发生过"本身可见。
    pub compact_count: u32,
    /// 最近一次压缩释放的估算 token。
    pub last_compact_saved_tokens: u32,
    /// 本会话被 context editing 外部化到 CAS 的 blob hash。会话被丢弃时
    /// （`/clear` / 新会话 / 退出 / resume 别的会话）必须逐个 `release`——
    /// `WorkspaceCas::gc` 只回收 `ref_count == 0` 的 blob，不记就永不回收。
    pub elided_blobs: Vec<String>,
    /// 本会话 context editing 累计释放的估算 token。
    pub context_edit_freed_tokens: u32,
    /// 同角色模型 fallback 记录。只保存 profile 名与错误分类，不保存请求正文、
    /// endpoint query 或认证信息。
    pub routing_events: Vec<RoutingEvent>,
    /// 本会话实际生效的 prompt cache 模式，取值见
    /// [`crate::session_store::prompt_cache_state`]。2 = 曾开启但端点 400 已降级，
    /// 让 `/cost` 能解释「缓存为什么是 0」。
    pub prompt_cache_state: u8,
    pub current_checkpoint_id: Option<String>,
    pub branch_parent_session_id: Option<String>,
    pub branch_parent_checkpoint_id: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空对话与本次用量，但保留当前 session 的 checkpoint/branch 血缘。
    /// `/clear` 不是创建新 session，不能把分支归属悄悄抹掉。
    ///
    /// ⚠️ 这会**连带清空 `elided_blobs`**。调用方必须先
    /// [`Session::take_elided_blobs`] 取出来并 `context_edit::release_session_blobs`
    /// 释放，否则 CAS 里那些 blob 的 `ref_count` 永远停在 1，`gc` 永远删不掉。
    pub fn clear_conversation(&mut self) {
        let current_checkpoint_id = self.current_checkpoint_id.clone();
        let branch_parent_session_id = self.branch_parent_session_id.clone();
        let branch_parent_checkpoint_id = self.branch_parent_checkpoint_id.clone();
        // prompt_cache_state 描述的是「当前 profile/端点是否真的在用缓存」，
        // 属于运行环境而非本次对话内容——`/clear` 换对话不换端点，抹掉它会让
        // 紧接着的 `/cost` 把"已降级"误报成"从未开启"。
        let prompt_cache_state = self.prompt_cache_state;
        *self = Self {
            current_checkpoint_id,
            branch_parent_session_id,
            branch_parent_checkpoint_id,
            prompt_cache_state,
            ..Self::default()
        };
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
    }

    /// 取走并清空本会话外部化过的 CAS blob 引用。
    ///
    /// 必须在会话真正被丢弃的路径（`/clear`、新会话、退出、resume 别的会话）
    /// 调 [`crate::context_edit::release_session_blobs`] 之前调用。
    pub fn take_elided_blobs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.elided_blobs)
    }

    /// 推送含多个内容块的用户消息（支持图片/文件附件）
    pub fn push_user_with_blocks(&mut self, blocks: Vec<ContentBlock>) {
        self.messages.push(Message {
            role: Role::User,
            content: blocks,
        });
    }

    pub fn push_assistant(&mut self, blocks: Vec<ContentBlock>) {
        self.messages.push(Message {
            role: Role::Assistant,
            content: blocks,
        });
    }

    pub fn push_tool_result(
        &mut self,
        tool_use_id: String,
        content: wyj_api::types::ToolResultContent,
        is_error: bool,
    ) {
        let block = ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        };
        self.push_user_blocks_merged(vec![block]);
    }

    /// 追加内容块：若最后一条已是 user 消息则合并进去，否则新建一条 user 消息。
    /// 用于工具结果、以及 Agent 忙碌时用户补充输入的运行时注入，二者都必须
    /// 保持与已有 user 消息同一轮次，以满足 Provider 对角色严格交替的要求。
    pub fn push_user_blocks_merged(&mut self, blocks: Vec<ContentBlock>) {
        match self.messages.last_mut() {
            Some(m) if matches!(m.role, Role::User) => {
                m.content.extend(blocks);
            }
            _ => {
                self.messages.push(Message {
                    role: Role::User,
                    content: blocks,
                });
            }
        }
    }

    /// 把内容块前插到最后一条消息（必须是 user 消息）的内容开头。
    /// 用于每轮对话开始时把 AGENTS.md `<system-reminder>` 插在用户实际输入之前。
    pub fn prepend_to_last_user(&mut self, blocks: Vec<ContentBlock>) {
        if let Some(m) = self.messages.last_mut() {
            if matches!(m.role, Role::User) {
                let mut new_content = blocks;
                new_content.append(&mut m.content);
                m.content = new_content;
            }
        }
    }

    pub fn add_usage(&mut self, input: u32, output: u32) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
    }

    pub fn add_cache_usage(&mut self, cache_read: u32, cache_write: u32) {
        self.total_cache_read_tokens += cache_read;
        self.total_cache_write_tokens += cache_write;
    }

    /// 把子 Agent 累加器里已记录的用量搬进本会话的 `sub_*` 字段。
    ///
    /// 由 `Agent::run_turn` 在**每批工具执行后**调用一次（子 Agent 可能
    /// `parallel_safe` 并发，抽干必须等这批 join 完）。`take` 内部是
    /// `swap(0)`，所以重复调用不会重复计账；没有累加器时是 no-op。
    pub fn drain_sub_usage(&mut self, sink: Option<&SubUsageTotals>) {
        let Some(sink) = sink else { return };
        let usage = sink.take();
        self.sub_input_tokens = self.sub_input_tokens.saturating_add(usage.input);
        self.sub_output_tokens = self.sub_output_tokens.saturating_add(usage.output);
        self.sub_cache_read_tokens = self.sub_cache_read_tokens.saturating_add(usage.cache_read);
        self.sub_cache_write_tokens = self
            .sub_cache_write_tokens
            .saturating_add(usage.cache_write);
    }

    /// 本会话的子 Agent 用量合计（`/cost` 的「子 Agent」行用）。
    pub fn sub_usage(&self) -> SubUsage {
        SubUsage {
            input: self.sub_input_tokens,
            output: self.sub_output_tokens,
            cache_read: self.sub_cache_read_tokens,
            cache_write: self.sub_cache_write_tokens,
        }
    }

    /// 从落盘文件恢复**全部计量与血缘字段**。
    ///
    /// 与 [`SessionFile::from_session`] 成对：这两个函数是 `Session` ↔
    /// `SessionFile` 之间计量字段的唯一起止点。resume / `/resume` 切换 / 分支
    /// 三条恢复路径以前各自手写 `sess.total_input_tokens = file.input_tokens`
    /// 这一串，新增字段必然漏改（这正是 cache/api_calls 此前只能活在内存里的
    /// 根因）。集中到这里后，漏改在编译期暴露。
    ///
    /// 注意**不**碰 `messages`——各恢复路径对消息体的处理不同（是否
    /// `materialize_elided`、是否替换为 checkpoint 内容），由调用方自己决定。
    pub fn restore_usage_from(&mut self, file: &crate::session_store::SessionFile) {
        self.total_input_tokens = file.input_tokens;
        self.total_output_tokens = file.output_tokens;
        self.total_cache_read_tokens = file.cache_read_tokens;
        self.total_cache_write_tokens = file.cache_write_tokens;
        self.api_calls = file.api_calls;
        self.tool_schema_tokens = file.tool_schema_tokens;
        self.tool_schema_tokens_saved = file.tool_schema_tokens_saved;
        self.sub_input_tokens = file.sub_input_tokens;
        self.sub_output_tokens = file.sub_output_tokens;
        self.sub_cache_read_tokens = file.sub_cache_read_tokens;
        self.sub_cache_write_tokens = file.sub_cache_write_tokens;
        self.prompt_cache_state = file.prompt_cache_state;
        self.compact_count = file.compact_count;
        self.context_edit_freed_tokens = file.context_edit_freed_tokens;
        self.routing_events = file.routing_events.clone();
        self.current_checkpoint_id = file.current_checkpoint_id.clone();
        self.branch_parent_session_id = file.branch_parent_session_id.clone();
        self.branch_parent_checkpoint_id = file.branch_parent_checkpoint_id.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 子 Agent 用量回灌 ────────────────────────────────────────────────

    fn usage(input: u32, output: u32, cache_read: u32, cache_write: u32) -> SubUsage {
        SubUsage {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn sub_totals_take_is_draining_not_peeking() {
        let totals = SubUsageTotals::default();
        totals.add(usage(100, 20, 5_000, 7));
        let first = totals.take();
        assert_eq!(first, usage(100, 20, 5_000, 7));
        // 抽干后必须归零：否则主 Agent 每批工具执行后 drain 一次，
        // 同一批子 Agent 的用量会被反复计入（take 的语义是 drain）。
        assert_eq!(totals.take(), SubUsage::default());
    }

    #[test]
    fn concurrent_subagents_accumulate_without_loss() {
        let totals = Arc::new(SubUsageTotals::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let totals = Arc::clone(&totals);
                std::thread::spawn(move || totals.add(usage(10, 2, 100, 0)))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(totals.take(), usage(80, 16, 800, 0));
    }

    #[test]
    fn drain_moves_subagent_usage_into_session_without_touching_main_totals() {
        let totals = SubUsageTotals::default();
        totals.add(usage(3_000, 400, 120_000, 0));
        let mut session = Session::new();
        session.add_usage(500, 50);
        session.add_cache_usage(9_000, 0);

        session.drain_sub_usage(Some(&totals));

        // 主会话口径不变——子 Agent 用量必须**不重叠**，否则 /cost 会双计。
        assert_eq!(session.total_input_tokens, 500);
        assert_eq!(session.total_output_tokens, 50);
        assert_eq!(session.total_cache_read_tokens, 9_000);
        // 子 Agent 口径独立。
        assert_eq!(session.sub_input_tokens, 3_000);
        assert_eq!(session.sub_output_tokens, 400);
        assert_eq!(session.sub_cache_read_tokens, 120_000);
    }

    #[test]
    fn repeated_drain_is_idempotent() {
        let totals = SubUsageTotals::default();
        totals.add(usage(1_000, 100, 50_000, 0));
        let mut session = Session::new();
        session.drain_sub_usage(Some(&totals));
        session.drain_sub_usage(Some(&totals));
        session.drain_sub_usage(Some(&totals));
        assert_eq!(session.sub_input_tokens, 1_000);
        assert_eq!(session.sub_cache_read_tokens, 50_000);
    }

    #[test]
    fn drain_without_sink_is_a_noop() {
        let mut session = Session::new();
        session.drain_sub_usage(None);
        assert_eq!(session.sub_usage(), SubUsage::default());
    }

    #[test]
    fn clear_conversation_also_resets_subagent_usage() {
        let totals = SubUsageTotals::default();
        totals.add(usage(700, 70, 21_000, 0));
        let mut session = Session::new();
        session.drain_sub_usage(Some(&totals));
        assert!(session.sub_input_tokens > 0);
        // /clear 换的是对话内容，但计费用量一并归零——否则新对话的 /cost
        // 会把上一段对话的子 Agent 开销算进来。
        session.clear_conversation();
        assert_eq!(session.sub_usage(), SubUsage::default());
    }

    // ── 工具 schema 双口径 ───────────────────────────────────────────────

    /// 回归：`tool_schema_tokens` 是**全会话累加**，`tool_schema_tokens_last`
    /// 才是最近一次请求的稳态值。历史上只有前者且被当作稳态值展示，
    /// 225 次调用的会话会显示成 1,457,192（真实稳态值只有 6,281）。
    #[test]
    fn tool_schema_accumulates_while_last_stays_steady() {
        let mut session = Session::new();
        for _ in 0..225 {
            session.tool_schema_tokens_last = 6_281;
            session.tool_schema_tokens = session.tool_schema_tokens.saturating_add(6_281);
        }
        assert_eq!(session.tool_schema_tokens, 225 * 6_281);
        assert_eq!(session.tool_schema_tokens_last, 6_281);
        assert!(session.tool_schema_tokens > session.tool_schema_tokens_last * 100);
    }

    #[test]
    fn clear_conversation_preserves_checkpoint_lineage() {
        let mut session = Session::new();
        session.push_user("hello");
        session.total_input_tokens = 10;
        session.current_checkpoint_id = Some("checkpoint-1".to_string());
        session.branch_parent_session_id = Some("parent".to_string());
        session.branch_parent_checkpoint_id = Some("parent-checkpoint".to_string());
        session.clear_conversation();
        assert!(session.messages.is_empty());
        assert_eq!(session.total_input_tokens, 0);
        assert_eq!(
            session.current_checkpoint_id.as_deref(),
            Some("checkpoint-1")
        );
        assert_eq!(session.branch_parent_session_id.as_deref(), Some("parent"));
        assert_eq!(
            session.branch_parent_checkpoint_id.as_deref(),
            Some("parent-checkpoint")
        );
    }
}
