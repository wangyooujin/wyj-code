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
    /// 打开分组（Profile）管理面板（/model 无参触发）
    OpenProfileDialog,
    /// 按名切换激活分组（/model <name> 触发）
    SwitchProfile(String),
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
    /// 打开配置设置面板（/config 命令触发）
    OpenSettingsDialog,
    /// 打开 CLAUDE.md 记忆面板（/memory 命令触发）
    OpenMemoryDialog,
    /// 打开 MCP server 管理面板（/mcp 命令触发）
    OpenMcpDialog,
    /// 打开 Skill 管理面板（/skills 命令触发）
    OpenSkillsDialog,
    /// 打开插件管理面板（/plugins 命令触发）
    OpenPluginsDialog,
}

/// 命令执行上下文
pub struct CommandContext {
    pub cwd: std::path::PathBuf,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 命中 prompt 缓存的输入 token（0.1x 计费，不含在 input_tokens 内）
    pub cache_read_tokens: u32,
    /// 写入 prompt 缓存的输入 token（1.25x 计费，不含在 input_tokens 内）
    pub cache_write_tokens: u32,
    pub context_window: u32,
    pub estimated_tokens: u32,
    pub home_dir: std::path::PathBuf,
    /// 子 Agent 累计 token 用量（与主会话分开统计，/cost 单列；headless 传 0）
    pub sub_input_tokens: u32,
    pub sub_output_tokens: u32,
    /// 合并全局+项目配置、过滤禁用条目后实际会连接的 MCP server 数量
    /// （由调用方用 `wyj_store::mcp_install::effective_mcp_servers` 算好传入，
    /// `crates/commands` 本身不依赖 `wyj-store` 以保持 crate 边界干净）。
    pub effective_mcp_count: usize,
    /// 已启用插件贡献的 agent 定义文件/目录路径（同上，由调用方用
    /// `wyj_store::plugin_install::enabled_plugin_agent_paths` 算好传入），
    /// 供 `/agents` 展示时一并列出插件贡献的自定义 agent 类型。
    pub plugin_agent_paths: Vec<std::path::PathBuf>,
}

/// Slash 命令 trait
#[async_trait]
pub trait Command: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn usage(&self) -> String;
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
                "{}",
                wyj_i18n::tr_fmt("command.unknown", &[("name", name)])
            ))),
        }
    }

    /// 返回以 prefix 开头的 (命令名, 描述) 对（用于补全）
    pub fn complete(&self, prefix: &str) -> Vec<(String, String)> {
        let p = prefix.trim_start_matches('/');
        self.commands
            .iter()
            .filter(|(k, _)| k.starts_with(p))
            .map(|(k, v)| (format!("/{k}"), v.description()))
            .collect()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}
