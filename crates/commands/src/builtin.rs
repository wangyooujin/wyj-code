//! 内置 Slash 命令

use crate::registry::{Command, CommandContext, CommandRegistry, CommandResult};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

// ── /help ─────────────────────────────────────────────────────────────────────

pub struct HelpCmd;

#[async_trait]
impl Command for HelpCmd {
    fn name(&self) -> &str { "help" }
    fn description(&self) -> &str { "显示所有可用命令" }
    fn usage(&self) -> &str { "/help" }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let text = "\
可用命令：
  /help        — 显示此帮助
  /clear       — 清空对话历史
  /model [名称] — 查看或切换模型
  /cwd         — 显示当前工作目录
  /init        — 生成 WYJ.md 项目说明
  /config      — 显示配置信息
  /quit        — 退出 wyj-code
";
        Ok(CommandResult::Output(text.to_string()))
    }
}

// ── /clear ────────────────────────────────────────────────────────────────────

pub struct ClearCmd;

#[async_trait]
impl Command for ClearCmd {
    fn name(&self) -> &str { "clear" }
    fn description(&self) -> &str { "清空当前对话历史" }
    fn usage(&self) -> &str { "/clear" }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::ClearHistory)
    }
}

// ── /model ────────────────────────────────────────────────────────────────────

pub struct ModelCmd;

#[async_trait]
impl Command for ModelCmd {
    fn name(&self) -> &str { "model" }
    fn description(&self) -> &str { "查看或切换模型 (如: /model MiniMax-M3)" }
    fn usage(&self) -> &str { "/model [model-name]" }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        if args.is_empty() {
            return Ok(CommandResult::Output(format!("当前模型: {}", ctx.model)));
        }
        Ok(CommandResult::SetModel(args.trim().to_string()))
    }
}

// ── /cwd ─────────────────────────────────────────────────────────────────────

pub struct CwdCmd;

#[async_trait]
impl Command for CwdCmd {
    fn name(&self) -> &str { "cwd" }
    fn description(&self) -> &str { "显示当前工作目录" }
    fn usage(&self) -> &str { "/cwd" }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Output(format!("工作目录: {}", ctx.cwd.display())))
    }
}

// ── /init ─────────────────────────────────────────────────────────────────────

pub struct InitCmd;

#[async_trait]
impl Command for InitCmd {
    fn name(&self) -> &str { "init" }
    fn description(&self) -> &str { "在当前目录生成 WYJ.md 项目说明文件" }
    fn usage(&self) -> &str { "/init" }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let path = ctx.cwd.join("WYJ.md");
        if path.exists() {
            return Ok(CommandResult::Output(format!("{} 已存在", path.display())));
        }
        let content = format!(
            "# 项目说明\n\n工作目录: {}\n\n在此填写项目背景、技术栈、开发约定等信息，\n供 wyj-code AI 助手参考。\n",
            ctx.cwd.display()
        );
        std::fs::write(&path, content)?;
        Ok(CommandResult::Output(format!("已创建 {}", path.display())))
    }
}

// ── /config ───────────────────────────────────────────────────────────────────

pub struct ConfigCmd;

#[async_trait]
impl Command for ConfigCmd {
    fn name(&self) -> &str { "config" }
    fn description(&self) -> &str { "显示当前配置信息" }
    fn usage(&self) -> &str { "/config" }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::Config;
        let cfg = Config::load()?;
        let key_status = match cfg.api_key() {
            Ok(k) => format!("{}...", &k[..k.len().min(8)]),
            Err(_) => "未配置".to_string(),
        };
        let out = format!(
            "供应商:  {}\n模型:    {}\n端点:    {}\nAPI Key: {}\n工作目录: {}",
            cfg.provider,
            cfg.model,
            cfg.resolved_base_url(),
            key_status,
            ctx.cwd.display()
        );
        Ok(CommandResult::Output(out))
    }
}

// ── /quit ─────────────────────────────────────────────────────────────────────

pub struct QuitCmd;

#[async_trait]
impl Command for QuitCmd {
    fn name(&self) -> &str { "quit" }
    fn description(&self) -> &str { "退出 wyj-code" }
    fn usage(&self) -> &str { "/quit" }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Quit)
    }
}

/// 创建包含所有内置命令的注册表
pub fn standard_registry() -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(ModelCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(QuitCmd));
    Arc::new(reg)
}
