//! 会话状态：消息历史 + 累计用量

use wyj_api::types::{ContentBlock, Message, Role};

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
    /// 实际随模型请求发送的工具 schema 估算 token 累计值。
    pub tool_schema_tokens: u32,
    /// lazy schema 相对全量工具集合累计避免发送的估算 token。
    pub tool_schema_tokens_saved: u32,
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
