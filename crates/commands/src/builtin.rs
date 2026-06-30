//! 内置 Slash 命令

use crate::registry::{Command, CommandContext, CommandRegistry, CommandResult};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

// ── /help ─────────────────────────────────────────────────────────────────────

pub struct HelpCmd;

#[async_trait]
impl Command for HelpCmd {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> &str {
        "显示所有可用命令和快捷键"
    }
    fn usage(&self) -> &str {
        "/help"
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let version = env!("CARGO_PKG_VERSION");
        let text = format!(
            "wyj-code v{version} — 终端 AI 编程助手\n\
            \n\
            命令:\n\
              /help              显示此帮助\n\
              /clear             清空对话历史\n\
              /compact [提示]    手动压缩上下文，减少 token 消耗\n\
              /config            显示当前配置\n\
              /cost              显示本会话 token 用量与费用估算\n\
              /doctor            诊断环境（API Key / 配置 / 记忆）\n\
              /memory            查看项目跨会话记忆内容\n\
              /model [名称]      查看或切换 AI 模型（无参：显示当前）\n\
              /mode [模式]       切换运行模式（normal / plan / bypass）\n\
              /resume [id]       恢复历史会话（无参数=选择器，有 id=直接恢复）\n\
              /sessions          查看和切换历史会话\n\
              /init              在当前目录生成 WYJ.md 项目说明\n\
              /quit              退出 wyj-code\n\
            \n\
            快捷键:\n\
              Enter              发送消息\n\
              Shift+Enter        插入换行\n\
              Ctrl+C             中断当前任务（二次按下退出程序）\n\
              Esc                中断当前任务\n\
              Shift+Tab          循环切换模式 Normal → Plan → Bypass\n\
              Ctrl+O             展开/折叠工具结果\n\
              Ctrl+L             清屏（保留对话历史）\n\
              Ctrl+A / Ctrl+E    跳到行首 / 行尾\n\
              Ctrl+K             删除光标到行尾\n\
              Ctrl+U             删除光标到行首\n\
              Ctrl+W             删除前一个词\n\
              PageUp / PageDown  滚动对话区\n\
              ↑ / ↓              浏览输入历史\n\
              /                  命令补全（Tab 确认，Esc 取消）\n\
              !<命令>            直接执行 Bash 命令\n\
            \n\
            模式:\n\
              Normal    默认模式，工具调用前弹出权限确认对话框\n\
              Plan      只读模式，仅允许 read/glob/grep/web_fetch\n\
              Bypass    跳过所有权限确认，自动批准工具调用"
        );
        Ok(CommandResult::Output(text))
    }
}

// ── /clear ────────────────────────────────────────────────────────────────────

pub struct ClearCmd;

#[async_trait]
impl Command for ClearCmd {
    fn name(&self) -> &str {
        "clear"
    }
    fn description(&self) -> &str {
        "清空当前对话历史"
    }
    fn usage(&self) -> &str {
        "/clear"
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::ClearHistory)
    }
}

// ── /compact ──────────────────────────────────────────────────────────────────

pub struct CompactCmd;

#[async_trait]
impl Command for CompactCmd {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "手动压缩对话上下文，减少 token 消耗"
    }
    fn usage(&self) -> &str {
        "/compact [摘要提示]"
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::CompactHistory)
    }
}

// ── /cost ─────────────────────────────────────────────────────────────────────

pub struct CostCmd;

// (input_per_mtok, output_per_mtok) in USD
static PRICES: &[(&str, f64, f64)] = &[
    ("claude-opus-4", 15.0, 75.0),
    ("claude-opus-3", 15.0, 75.0),
    ("claude-sonnet-4", 3.0, 15.0),
    ("claude-sonnet-3", 3.0, 15.0),
    ("claude-haiku-4", 0.8, 4.0),
    ("claude-haiku-3", 0.25, 1.25),
];

fn lookup_price(model: &str) -> Option<(f64, f64)> {
    let model_lower = model.to_lowercase();
    PRICES
        .iter()
        .find(|(prefix, _, _)| model_lower.contains(prefix))
        .map(|(_, i, o)| (*i, *o))
}

#[async_trait]
impl Command for CostCmd {
    fn name(&self) -> &str {
        "cost"
    }
    fn description(&self) -> &str {
        "显示本会话 token 用量与费用估算"
    }
    fn usage(&self) -> &str {
        "/cost"
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let input = ctx.input_tokens;
        let output = ctx.output_tokens;
        let total = input + output;
        let ctx_pct = if ctx.context_window > 0 {
            ctx.estimated_tokens as f64 / ctx.context_window as f64 * 100.0
        } else {
            0.0
        };

        let cost_line = if let Some((ip, op)) = lookup_price(&ctx.model) {
            let input_cost = input as f64 / 1_000_000.0 * ip;
            let output_cost = output as f64 / 1_000_000.0 * op;
            let total_cost = input_cost + output_cost;
            format!(
                "  输入:   {:>10} tokens  (~${:.4})\n\
                   输出:   {:>10} tokens  (~${:.4})\n\
                   合计:   {:>10} tokens  (~${:.4})",
                fmt_num(input),
                input_cost,
                fmt_num(output),
                output_cost,
                fmt_num(total),
                total_cost,
            )
        } else {
            format!(
                "  输入:   {:>10} tokens\n\
                   输出:   {:>10} tokens\n\
                   合计:   {:>10} tokens",
                fmt_num(input),
                fmt_num(output),
                fmt_num(total),
            )
        };

        let text = format!(
            "本会话 Token 用量\n\
            {cost_line}\n\
            \n\
              上下文: {ctx_pct:.0}% ({estimated} / {window} tokens)",
            estimated = fmt_num(ctx.estimated_tokens),
            window = fmt_num(ctx.context_window),
        );
        Ok(CommandResult::Output(text))
    }
}

fn fmt_num(n: u32) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ── /memory ───────────────────────────────────────────────────────────────────

pub struct MemoryCmd;

#[async_trait]
impl Command for MemoryCmd {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "查看当前项目的跨会话记忆内容"
    }
    fn usage(&self) -> &str {
        "/memory"
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let pid = wyj_core::project_id(&ctx.cwd);
        let mem_dir = ctx.home_dir.join(".wyj-code").join("memory").join(&pid);

        if !mem_dir.exists() {
            return Ok(CommandResult::Output(
                "当前项目暂无跨会话记忆。".to_string(),
            ));
        }

        let index_path = mem_dir.join("MEMORY.md");
        if !index_path.exists() {
            return Ok(CommandResult::Output(
                "当前项目暂无跨会话记忆。".to_string(),
            ));
        }

        let index = std::fs::read_to_string(&index_path)?;
        if index.trim().is_empty() {
            return Ok(CommandResult::Output(
                "当前项目暂无跨会话记忆。".to_string(),
            ));
        }

        let mut out = format!("## 项目记忆索引\n\n{index}\n");

        // 读取各记忆文件正文
        let mut details = Vec::new();
        for line in index.lines() {
            if let Some(fname) = extract_md_link(line) {
                let fpath = mem_dir.join(&fname);
                if let Ok(content) = std::fs::read_to_string(&fpath) {
                    let body = strip_frontmatter(&content);
                    if !body.trim().is_empty() {
                        details.push(format!("### {fname}\n\n{}", body.trim()));
                    }
                }
            }
        }

        if !details.is_empty() {
            out.push_str("\n---\n\n## 记忆详情\n\n");
            out.push_str(&details.join("\n\n---\n\n"));
        }

        Ok(CommandResult::Output(out))
    }
}

fn extract_md_link(line: &str) -> Option<String> {
    let start = line.find("](")? + 2;
    let end = line[start..].find(')')? + start;
    let target = line[start..end].trim();
    if target.ends_with(".md") && !target.starts_with("http") {
        Some(target.to_string())
    } else {
        None
    }
}

fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---") {
        return content;
    }
    let after_first = &content[3..];
    if let Some(pos) = after_first.find("\n---") {
        &after_first[pos + 4..]
    } else {
        content
    }
}

// ── /doctor ───────────────────────────────────────────────────────────────────

pub struct DoctorCmd;

#[async_trait]
impl Command for DoctorCmd {
    fn name(&self) -> &str {
        "doctor"
    }
    fn description(&self) -> &str {
        "诊断运行环境（API Key / 配置 / WYJ.md / 记忆）"
    }
    fn usage(&self) -> &str {
        "/doctor"
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::Config;
        let cfg = Config::load()?;
        let version = env!("CARGO_PKG_VERSION");

        let mut lines = vec![format!("wyj-code v{version} 环境诊断\n")];

        // API Key
        match cfg.api_key() {
            Ok(k) => lines.push(format!(
                "  ✓  API Key      {}... (已配置)",
                &k[..k.len().min(8)]
            )),
            Err(_) => lines.push(
                "  ✗  API Key      未配置（请设置 WYJ_CODE_API_KEY 或 config.toml）".to_string(),
            ),
        }

        // 配置文件
        match wyj_config::config_dir() {
            Ok(dir) => {
                let cfg_file = dir.join("config.toml");
                if cfg_file.exists() {
                    lines.push(format!("  ✓  配置文件    {}", cfg_file.display()));
                } else {
                    lines.push(format!(
                        "  ✗  配置文件    {} (不存在，使用默认值)",
                        cfg_file.display()
                    ));
                }
            }
            Err(e) => lines.push(format!("  ✗  配置目录    {e}")),
        }

        // Provider + Model
        lines.push(format!("  ✓  供应商      {}", cfg.provider));
        lines.push(format!("  ✓  模型        {}", ctx.model));

        // WYJ.md
        let wyj_md = ctx.cwd.join("WYJ.md");
        if wyj_md.exists() {
            lines.push(format!("  ✓  WYJ.md      {}", wyj_md.display()));
        } else {
            lines.push("  ✗  WYJ.md      未找到（运行 /init 可生成）".to_string());
        }

        // 记忆目录
        let pid = wyj_core::project_id(&ctx.cwd);
        let mem_dir = ctx.home_dir.join(".wyj-code").join("memory").join(&pid);
        let index_path = mem_dir.join("MEMORY.md");
        if index_path.exists() {
            let entry_count = std::fs::read_to_string(&index_path)
                .unwrap_or_default()
                .lines()
                .filter(|l| l.starts_with("- ["))
                .count();
            lines.push(format!(
                "  ✓  记忆目录    {} ({entry_count} 条)",
                mem_dir.display()
            ));
        } else {
            lines.push(format!("  ✗  记忆目录    {} (暂无记忆)", mem_dir.display()));
        }

        // MCP servers
        let mcp_count = cfg.mcp_servers.len();
        if mcp_count > 0 {
            lines.push(format!("  ✓  MCP 服务    {mcp_count} 个已配置"));
        } else {
            lines.push("  -  MCP 服务    未配置".to_string());
        }

        Ok(CommandResult::Output(lines.join("\n")))
    }
}

// ── /model ────────────────────────────────────────────────────────────────────

pub struct ModelCmd;

#[async_trait]
impl Command for ModelCmd {
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> &str {
        "查看或切换 AI 模型 (如: /model claude-sonnet-4-6)"
    }
    fn usage(&self) -> &str {
        "/model [model-name]"
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        if args.is_empty() {
            return Ok(CommandResult::Output(format!("当前模型: {}", ctx.model)));
        }
        Ok(CommandResult::SetModel(args.trim().to_string()))
    }
}

// ── /mode (占位，实际由 app.rs 硬编码拦截) ────────────────────────────────────

pub struct ModeCmd;

#[async_trait]
impl Command for ModeCmd {
    fn name(&self) -> &str {
        "mode"
    }
    fn description(&self) -> &str {
        "切换运行模式: /mode [normal|plan|bypass]"
    }
    fn usage(&self) -> &str {
        "/mode [normal|plan|bypass]"
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        // app.rs 在 dispatch 之前已拦截 /mode，此分支实际不会执行
        Ok(CommandResult::None)
    }
}

// ── /cwd ─────────────────────────────────────────────────────────────────────

pub struct CwdCmd;

#[async_trait]
impl Command for CwdCmd {
    fn name(&self) -> &str {
        "cwd"
    }
    fn description(&self) -> &str {
        "显示当前工作目录"
    }
    fn usage(&self) -> &str {
        "/cwd"
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Output(format!(
            "工作目录: {}",
            ctx.cwd.display()
        )))
    }
}

// ── /init ─────────────────────────────────────────────────────────────────────

pub struct InitCmd;

#[async_trait]
impl Command for InitCmd {
    fn name(&self) -> &str {
        "init"
    }
    fn description(&self) -> &str {
        "在当前目录生成 WYJ.md 项目说明文件"
    }
    fn usage(&self) -> &str {
        "/init"
    }
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
    fn name(&self) -> &str {
        "config"
    }
    fn description(&self) -> &str {
        "显示当前配置信息"
    }
    fn usage(&self) -> &str {
        "/config"
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::Config;
        let cfg = Config::load()?;
        let key_status = match cfg.api_key() {
            Ok(k) => format!("{}... (已配置)", &k[..k.len().min(8)]),
            Err(_) => "未配置".to_string(),
        };
        let out = format!(
            "供应商:       {}\n\
             模型:         {}\n\
             端点:         {}\n\
             API Key:      {}\n\
             最大 tokens:  {}\n\
             上下文窗口:   {}\n\
             工作目录:     {}",
            cfg.provider,
            cfg.model,
            cfg.resolved_base_url(),
            key_status,
            cfg.max_tokens,
            cfg.context_window,
            ctx.cwd.display()
        );
        Ok(CommandResult::Output(out))
    }
}

// ── /resume ───────────────────────────────────────────────────────────────────

pub struct ResumeCmd;

#[async_trait]
impl Command for ResumeCmd {
    fn name(&self) -> &str {
        "resume"
    }
    fn description(&self) -> &str {
        "恢复历史会话 (无参数=选择器, /resume <session-id>=直接恢复)"
    }
    fn usage(&self) -> &str {
        "/resume [session-id]"
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let id = args.trim();
        if id.is_empty() {
            Ok(CommandResult::OpenSessionPicker)
        } else {
            Ok(CommandResult::ResumeSession(id.to_string()))
        }
    }
}

// ── /sessions ─────────────────────────────────────────────────────────────────

pub struct SessionsCmd;

#[async_trait]
impl Command for SessionsCmd {
    fn name(&self) -> &str {
        "sessions"
    }
    fn description(&self) -> &str {
        "查看和切换历史会话"
    }
    fn usage(&self) -> &str {
        "/sessions"
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenSessionPicker)
    }
}

// ── /quit ─────────────────────────────────────────────────────────────────────

pub struct QuitCmd;

#[async_trait]
impl Command for QuitCmd {
    fn name(&self) -> &str {
        "quit"
    }
    fn description(&self) -> &str {
        "退出 wyj-code"
    }
    fn usage(&self) -> &str {
        "/quit"
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Quit)
    }
}

/// 创建包含所有内置命令的注册表
pub fn standard_registry() -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(CompactCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ModelCmd));
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(QuitCmd));
    Arc::new(reg)
}

/// 创建包含内置命令 + 已加载 skill 的注册表
/// skill 先注册（优先级低），内置命令后注册（同名时覆盖 skill）
pub fn standard_registry_with_skills(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();

    // 先注册 skill（优先级低）
    for skill in crate::skill::load_skills(home, cwd) {
        reg.register(skill);
    }

    // 再注册内置命令（后注册覆盖同名 skill）
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(CompactCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ModelCmd));
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(QuitCmd));

    Arc::new(reg)
}
