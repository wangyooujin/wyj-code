//! 会话状态：消息历史 + 累计用量

use wyj_api::types::{ContentBlock, Message, Role};

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub messages: Vec<Message>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
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
        // 工具结果追加到最后一条 user 消息，若最后是 assistant 则新建 user 消息
        match self.messages.last_mut() {
            Some(m) if matches!(m.role, wyj_api::types::Role::User) => {
                m.content.push(block);
            }
            _ => {
                self.messages.push(Message {
                    role: Role::User,
                    content: vec![block],
                });
            }
        }
    }

    pub fn add_usage(&mut self, input: u32, output: u32) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
    }

    pub fn cost_summary(&self) -> String {
        format!(
            "累计 tokens: 输入 {} / 输出 {}",
            self.total_input_tokens, self.total_output_tokens
        )
    }
}
