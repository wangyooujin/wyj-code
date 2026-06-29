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
    /// 切换模型
    SetModel(String),
    /// 退出应用
    Quit,
    /// 无动作（静默成功）
    None,
}

/// 命令执行上下文
pub struct CommandContext {
    pub cwd: std::path::PathBuf,
    pub model: String,
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
        Self { commands: HashMap::new() }
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
            None => Some(Err(anyhow::anyhow!("未知命令: /{name}  (输入 /help 查看所有命令)"))),
        }
    }

    /// 返回以 prefix 开头的命令名（用于补全）
    pub fn complete(&self, prefix: &str) -> Vec<String> {
        let p = prefix.trim_start_matches('/');
        self.commands
            .keys()
            .filter(|k| k.starts_with(p))
            .map(|k| format!("/{k}"))
            .collect()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}
