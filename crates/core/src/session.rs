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
    /// 同角色模型 fallback 记录。只保存 profile 名与错误分类，不保存请求正文、
    /// endpoint query 或认证信息。
    pub routing_events: Vec<RoutingEvent>,
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
    pub fn clear_conversation(&mut self) {
        let current_checkpoint_id = self.current_checkpoint_id.clone();
        let branch_parent_session_id = self.branch_parent_session_id.clone();
        let branch_parent_checkpoint_id = self.branch_parent_checkpoint_id.clone();
        *self = Self {
            current_checkpoint_id,
            branch_parent_session_id,
            branch_parent_checkpoint_id,
            ..Self::default()
        };
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
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
    /// 用于每轮对话开始时把 CLAUDE.md `<system-reminder>` 插在用户实际输入之前。
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
