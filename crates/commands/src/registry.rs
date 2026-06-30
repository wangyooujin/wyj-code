//! 命令注册表与解析

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// 命令执行结果
#[derive(Debug)]
pub enum CommandResult {
    /// 输出文本给用户
    Output(String),
    /// 清空对话历史
    ClearHistory,
    /// 切换模型（运行时热切换）
    SetModel(String),
    /// 手动触发上下文压缩
    CompactHistory,
    /// 退出应用
    Quit,
    /// 无动作（静默成功）
    None,
    /// Skill 执行结果：包含展开后的 prompt，由调用方转发给 agent
    RunPrompt(String),
    /// 打开会话选择器
    OpenSessionPicker,
    /// 直接恢复指定 session（session-id）
    ResumeSession(String),
}

/// 命令执行上下文
pub struct CommandContext {
    pub cwd: std::path::PathBuf,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub context_window: u32,
    pub estimated_tokens: u32,
    pub home_dir: std::path::PathBuf,
}

/// Slash 命令 trait
#[async_trait]
pub trait Command: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn usage(&self) -> &str;
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult>;
}

pub struct CommandRegistry {
    commands: HashMap<String, Arc<dyn Command>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    pub fn register(&mut self, cmd: Arc<dyn Command>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        self.commands.get(name)
    }

    pub fn list(&self) -> Vec<&Arc<dyn Command>> {
        let mut v: Vec<_> = self.commands.values().collect();
        v.sort_by_key(|c| c.name());
        v
    }

    /// 解析并执行 "/command args"，返回 None 表示不是 slash 命令
    pub async fn dispatch(
        &self,
        input: &str,
        ctx: &CommandContext,
    ) -> Option<Result<CommandResult>> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let rest = &trimmed[1..];
        let (name, args) = match rest.find(char::is_whitespace) {
            Some(pos) => (&rest[..pos], rest[pos + 1..].trim()),
            None => (rest, ""),
        };
        if name.is_empty() {
            return None;
        }
        match self.commands.get(name) {
            Some(cmd) => Some(cmd.run(args, ctx).await),
            None => Some(Err(anyhow::anyhow!(
                "未知命令: /{name}  (输入 /help 查看所有命令)"
            ))),
        }
    }

    /// 返回以 prefix 开头的 (命令名, 描述) 对（用于补全）
    pub fn complete(&self, prefix: &str) -> Vec<(String, String)> {
        let p = prefix.trim_start_matches('/');
        self.commands
            .iter()
            .filter(|(k, _)| k.starts_with(p))
            .map(|(k, v)| (format!("/{k}"), v.description().to_string()))
            .collect()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}
