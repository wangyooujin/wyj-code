//! 会话状态：消息历史 + 累计用量

use wyj_api::types::{ContentBlock, Message, Role};

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub messages: Vec<Message>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    /// 累计命中 prompt 缓存的输入 token 数（按约 0.1x 价格计费，不计入
    /// `total_input_tokens`，单独统计以便 /cost 展示缓存命中情况）
    pub total_cache_read_tokens: u32,
    /// 本会话累计发起的模型推理次数（每次 provider.stream 调用计 1），
    /// 供评测基准（WYJ_STATS_JSON）衡量任务完成所需的 API 往返数。
    pub api_calls: u32,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
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

    pub fn push_tool_result(&mut self, tool_use_id: String, output: String, is_error: bool) {
        use wyj_api::types::ToolResultContent;
        let block = ContentBlock::ToolResult {
            tool_use_id,
            content: ToolResultContent::text(output),
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

    pub fn add_cache_read(&mut self, cache_read: u32) {
        self.total_cache_read_tokens += cache_read;
    }

    pub fn cost_summary(&self) -> String {
        if self.total_cache_read_tokens > 0 {
            format!(
                "累计 tokens: 输入 {} (缓存命中 {}) / 输出 {}",
                self.total_input_tokens, self.total_cache_read_tokens, self.total_output_tokens
            )
        } else {
            format!(
                "累计 tokens: 输入 {} / 输出 {}",
                self.total_input_tokens, self.total_output_tokens
            )
        }
    }
}
