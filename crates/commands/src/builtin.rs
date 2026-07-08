//! 内置 Slash 命令

use crate::registry::{Command, CommandContext, CommandRegistry, CommandResult};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use wyj_i18n::{tr, tr_fmt};

// ── /help ─────────────────────────────────────────────────────────────────────

pub struct HelpCmd;

#[async_trait]
impl Command for HelpCmd {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> String {
        tr("help.desc")
    }
    fn usage(&self) -> String {
        "/help".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let version = env!("CARGO_PKG_VERSION");
        let text = tr_fmt("help.body", &[("version", version)]);
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
    fn description(&self) -> String {
        tr("clear.desc")
    }
    fn usage(&self) -> String {
        "/clear".to_string()
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
    fn description(&self) -> String {
        tr("compact.desc")
    }
    fn usage(&self) -> String {
        tr("compact.usage")
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
    fn description(&self) -> String {
        tr("cost.desc")
    }
    fn usage(&self) -> String {
        "/cost".to_string()
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
            // 缓存命中约 0.1x 输入价、缓存写入约 1.25x 输入价
            let cache_read_cost = ctx.cache_read_tokens as f64 / 1_000_000.0 * ip * 0.1;
            let cache_write_cost = ctx.cache_write_tokens as f64 / 1_000_000.0 * ip * 1.25;
            let total_cost = input_cost + output_cost + cache_read_cost + cache_write_cost;
            tr_fmt(
                "cost.line_with_price",
                &[
                    ("input", &format!("{:>10}", fmt_num(input))),
                    ("input_cost", &format!("{input_cost:.4}")),
                    ("output", &format!("{:>10}", fmt_num(output))),
                    ("output_cost", &format!("{output_cost:.4}")),
                    ("total", &format!("{:>10}", fmt_num(total))),
                    ("total_cost", &format!("{total_cost:.4}")),
                ],
            )
        } else {
            tr_fmt(
                "cost.line_no_price",
                &[
                    ("input", &format!("{:>10}", fmt_num(input))),
                    ("output", &format!("{:>10}", fmt_num(output))),
                    ("total", &format!("{:>10}", fmt_num(total))),
                ],
            )
        };

        // 缓存用量单列（有缓存活动时才显示）：展示缓存收益与真实成本
        let cache_line = if ctx.cache_read_tokens > 0 || ctx.cache_write_tokens > 0 {
            let saved_pct = {
                let full = ctx.cache_read_tokens as f64;
                let all_input = input as f64 + full;
                if all_input > 0.0 {
                    // 命中部分按 0.1x 计，相对全价的节省比例
                    full * 0.9 / all_input * 100.0
                } else {
                    0.0
                }
            };
            format!(
                "\n{}",
                tr_fmt(
                    "cost.cache_line",
                    &[
                        ("read", &fmt_num(ctx.cache_read_tokens)),
                        ("write", &fmt_num(ctx.cache_write_tokens)),
                        ("saved_pct", &format!("{saved_pct:.0}")),
                    ],
                )
            )
        } else {
            String::new()
        };

        let ctx_line = tr_fmt(
            "cost.context_line",
            &[
                ("pct", &format!("{ctx_pct:.0}")),
                ("estimated", &fmt_num(ctx.estimated_tokens)),
                ("window", &fmt_num(ctx.context_window)),
            ],
        );

        let header = tr("cost.header");
        let mut text = format!("{header}\n{cost_line}{cache_line}\n\n{ctx_line}");

        // 子 Agent 用量单列（有用量时才显示）
        let sub_total = ctx.sub_input_tokens + ctx.sub_output_tokens;
        if sub_total > 0 {
            text.push('\n');
            text.push_str(&tr_fmt(
                "cost.subagent_line",
                &[
                    ("input", &fmt_num(ctx.sub_input_tokens)),
                    ("output", &fmt_num(ctx.sub_output_tokens)),
                    ("total", &fmt_num(sub_total)),
                ],
            ));
        }
        Ok(CommandResult::Output(text))
    }
}

// ── /mcp ──────────────────────────────────────────────────────────────────────

pub struct McpCmd;

#[async_trait]
impl Command for McpCmd {
    fn name(&self) -> &str {
        "mcp"
    }
    fn description(&self) -> String {
        tr("mcp.desc")
    }
    fn usage(&self) -> String {
        "/mcp".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenMcpDialog)
    }
}

// ── /skills ───────────────────────────────────────────────────────────────────

pub struct SkillsCmd;

#[async_trait]
impl Command for SkillsCmd {
    fn name(&self) -> &str {
        "skills"
    }
    fn description(&self) -> String {
        tr("skills.desc")
    }
    fn usage(&self) -> String {
        "/skills".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenSkillsDialog)
    }
}

// ── /plugins ──────────────────────────────────────────────────────────────────

pub struct PluginsCmd;

#[async_trait]
impl Command for PluginsCmd {
    fn name(&self) -> &str {
        "plugins"
    }
    fn description(&self) -> String {
        tr("plugins.desc")
    }
    fn usage(&self) -> String {
        "/plugins".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenPluginsDialog)
    }
}

// ── /agents ───────────────────────────────────────────────────────────────────

pub struct AgentsCmd;

#[async_trait]
impl Command for AgentsCmd {
    fn name(&self) -> &str {
        "agents"
    }
    fn description(&self) -> String {
        tr("agents.desc")
    }
    fn usage(&self) -> String {
        "/agents".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        // 实时重读盘，方便验证新写的定义文件；但注册进模型的类型列表在
        // 启动时已固定，新增/修改定义需重启才对模型生效（下方注明）。
        let defs = wyj_core::load_agent_defs(&ctx.cwd, &ctx.plugin_agent_paths);
        let mut out = tr("agents.header");
        for d in &defs {
            out.push_str(&format!("\n● {} — {}\n", d.name, d.description));
            if let Some(m) = &d.model {
                out.push_str(&format!(
                    "  {}\n",
                    tr_fmt("agents.model_line", &[("profile", m)])
                ));
            }
            let tools = d
                .tools
                .as_ref()
                .map(|t| t.join(", "))
                .unwrap_or_else(|| tr("agents.tools_all"));
            out.push_str(&format!(
                "  {}\n",
                tr_fmt("agents.tools_line", &[("tools", &tools)])
            ));
            let source = if d.builtin {
                tr("agents.builtin_tag")
            } else {
                d.source
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            };
            out.push_str(&format!(
                "  {}\n",
                tr_fmt("agents.source_line", &[("source", &source)])
            ));
        }
        out.push_str(&format!("\n{}", tr("agents.reload_note")));
        Ok(CommandResult::Output(out))
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

// ── /hooks ───────────────────────────────────────────────────────────────────

pub struct HooksCmd;

#[async_trait]
impl Command for HooksCmd {
    fn name(&self) -> &str {
        "hooks"
    }
    fn description(&self) -> String {
        tr("hooks.desc")
    }
    fn usage(&self) -> String {
        "/hooks".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        // 与 `/agents` 同一哲学：实时重新读盘，保证显示的是最新配置文件状态。
        let settings = wyj_core::load_effective_hooks(&ctx.cwd);
        let mut out = String::new();
        if !ctx.hooks_enabled {
            out.push_str(&tr("hooks.disabled_banner"));
            out.push('\n');
        }
        if settings.hooks.is_empty() {
            out.push_str(&tr("hooks.empty_note"));
            return Ok(CommandResult::Output(out));
        }
        out.push_str(&tr("hooks.header"));

        let mut events: Vec<&String> = settings.hooks.keys().collect();
        events.sort();
        for event in events {
            let entries = &settings.hooks[event];
            if entries.iter().all(|e| e.hooks.is_empty()) {
                continue;
            }
            out.push_str(&format!(
                "\n{}",
                tr_fmt("hooks.event_line", &[("event", event)])
            ));
            for entry in entries {
                if entry.hooks.is_empty() {
                    continue;
                }
                let any_matcher = tr("hooks.matcher_any");
                let matcher = entry.matcher.as_deref().unwrap_or(&any_matcher);
                out.push_str(&format!(
                    "\n  {}",
                    tr_fmt("hooks.matcher_line", &[("matcher", matcher)])
                ));
                for cmd in &entry.hooks {
                    out.push_str(&format!(
                        "\n    {}",
                        tr_fmt(
                            "hooks.command_line",
                            &[
                                ("timeout", &cmd.timeout.unwrap_or(60).to_string()),
                                ("command", &cmd.command)
                            ]
                        )
                    ));
                }
            }
        }
        Ok(CommandResult::Output(out))
    }
}

// ── /memory ───────────────────────────────────────────────────────────────────

pub struct MemoryCmd;

#[async_trait]
impl Command for MemoryCmd {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> String {
        tr("memory.desc")
    }
    fn usage(&self) -> String {
        "/memory".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        // 面板本体（列出当前会话适用的 CLAUDE.md 系文件 + auto-memory 开关/索引）
        // 由 TUI 侧构建，此处只负责触发打开。
        Ok(CommandResult::OpenMemoryDialog)
    }
}

// ── /doctor ───────────────────────────────────────────────────────────────────

pub struct DoctorCmd;

#[async_trait]
impl Command for DoctorCmd {
    fn name(&self) -> &str {
        "doctor"
    }
    fn description(&self) -> String {
        tr("doctor.desc")
    }
    fn usage(&self) -> String {
        "/doctor".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::Config;
        let cfg = Config::load()?;
        let version = env!("CARGO_PKG_VERSION");

        let mut lines = vec![tr_fmt("doctor.header", &[("version", version)])];

        // API Key
        match cfg.api_key() {
            Ok(k) => lines.push(tr_fmt(
                "status.api_key_ok",
                &[("prefix", &k[..k.len().min(8)])],
            )),
            Err(_) => lines.push(tr("doctor.api_key_missing")),
        }

        // 配置文件
        match wyj_config::config_dir() {
            Ok(dir) => {
                let cfg_file = dir.join("config.toml");
                if cfg_file.exists() {
                    lines.push(tr_fmt(
                        "doctor.config_file_ok",
                        &[("path", &cfg_file.display().to_string())],
                    ));
                } else {
                    lines.push(tr_fmt(
                        "doctor.config_file_missing",
                        &[("path", &cfg_file.display().to_string())],
                    ));
                }
            }
            Err(e) => lines.push(tr_fmt("doctor.config_dir_err", &[("err", &e.to_string())])),
        }

        // Provider + Model
        lines.push(tr_fmt(
            "doctor.provider",
            &[("provider", &cfg.provider().to_string())],
        ));
        lines.push(tr_fmt("doctor.model", &[("model", &ctx.model)]));

        // CLAUDE.md 系文件（全局 + 祖先链）
        let claude_md_files = wyj_core::discover_files(&ctx.cwd);
        let existing: Vec<String> = claude_md_files
            .iter()
            .filter(|f| f.exists)
            .map(|f| f.path.display().to_string())
            .collect();
        if existing.is_empty() {
            lines.push(tr("doctor.claude_md_missing"));
        } else {
            lines.push(tr_fmt(
                "doctor.claude_md_ok",
                &[("paths", &existing.join(", "))],
            ));
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
            lines.push(tr_fmt(
                "doctor.memory_dir_with_count",
                &[
                    ("path", &mem_dir.display().to_string()),
                    ("count", &entry_count.to_string()),
                ],
            ));
        } else {
            lines.push(tr_fmt(
                "doctor.memory_dir_empty",
                &[("path", &mem_dir.display().to_string())],
            ));
        }

        // MCP servers（合并全局+项目配置、过滤禁用条目后的有效数量）
        let mcp_count = ctx.effective_mcp_count;
        if mcp_count > 0 {
            lines.push(tr_fmt(
                "doctor.mcp_configured",
                &[("count", &mcp_count.to_string())],
            ));
        } else {
            lines.push(tr("doctor.mcp_none"));
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
    fn description(&self) -> String {
        tr("model.desc")
    }
    fn usage(&self) -> String {
        "/model [profile-name]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        if args.trim().is_empty() {
            return Ok(CommandResult::OpenProfileDialog);
        }
        Ok(CommandResult::SwitchProfile(args.trim().to_string()))
    }
}

// ── /mode (占位，实际由 app.rs 硬编码拦截) ────────────────────────────────────

pub struct ModeCmd;

#[async_trait]
impl Command for ModeCmd {
    fn name(&self) -> &str {
        "mode"
    }
    fn description(&self) -> String {
        tr("mode.desc")
    }
    fn usage(&self) -> String {
        "/mode [normal|plan|bypass]".to_string()
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
    fn description(&self) -> String {
        tr("cwd.desc")
    }
    fn usage(&self) -> String {
        "/cwd".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Output(tr_fmt(
            "cwd.body",
            &[("cwd", &ctx.cwd.display().to_string())],
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
    fn description(&self) -> String {
        tr("init.desc")
    }
    fn usage(&self) -> String {
        "/init".to_string()
    }
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        // 对齐真实 Claude Code：/init 不是静态模板写文件，而是触发一次真正的 agent
        // 回合，让它自己去探索项目（Cargo.toml/package.json/README/目录结构等）
        // 生成或合并改进 CLAUDE.md。已存在则要求读取后合并而非整体覆盖。
        let prompt = tr_fmt(
            "init.agent_prompt",
            &[("cwd", &ctx.cwd.display().to_string())],
        );
        Ok(CommandResult::RunPrompt(prompt))
    }
}

// ── /config ───────────────────────────────────────────────────────────────────

pub struct ConfigCmd;

#[async_trait]
impl Command for ConfigCmd {
    fn name(&self) -> &str {
        "config"
    }
    fn description(&self) -> String {
        tr("config.desc")
    }
    fn usage(&self) -> String {
        "/config".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenSettingsDialog)
    }
}

// ── /resume ───────────────────────────────────────────────────────────────────

pub struct ResumeCmd;

#[async_trait]
impl Command for ResumeCmd {
    fn name(&self) -> &str {
        "resume"
    }
    fn description(&self) -> String {
        tr("resume.desc")
    }
    fn usage(&self) -> String {
        "/resume [session-id]".to_string()
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
    fn description(&self) -> String {
        tr("sessions.desc")
    }
    fn usage(&self) -> String {
        "/sessions".to_string()
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
    fn description(&self) -> String {
        tr("quit.desc")
    }
    fn usage(&self) -> String {
        "/quit".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::Quit)
    }
}

// ── GitHub 协作命令辅助 ────────────────────────────────────────────────────────

/// 探测 `gh` CLI 是否可用
async fn gh_available() -> bool {
    tokio::process::Command::new("gh")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 运行外部命令并捕获输出，返回 (是否成功, stdout, stderr)
async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
) -> (bool, String, String) {
    match tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// 从 origin remote URL 解析 `owner/repo`（支持 https 与 ssh 形式）
fn parse_github_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest
        .strip_suffix(".git")
        .unwrap_or(rest)
        .trim_end_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// URL 查询参数百分号编码
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 用系统默认浏览器打开 URL（best-effort），返回是否成功启动
async fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let (prog, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let (prog, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (prog, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    tokio::process::Command::new(prog)
        .args(&args)
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── /bug ──────────────────────────────────────────────────────────────────────

pub struct BugCmd;

#[async_trait]
impl Command for BugCmd {
    fn name(&self) -> &str {
        "bug"
    }
    fn description(&self) -> String {
        tr("bug.desc")
    }
    fn usage(&self) -> String {
        "/bug [简述]".to_string()
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let (ok, stdout, _) = run_capture("git", &["remote", "get-url", "origin"], &ctx.cwd).await;
        let Some(slug) = (if ok { parse_github_slug(&stdout) } else { None }) else {
            return Ok(CommandResult::Output(tr("bug.no_remote")));
        };
        let title = args.trim();
        let body = format!(
            "## 环境\n- wyj-code: {ver}\n- OS: {os} ({arch})\n- Model: {model}\n\n\
             ## 问题描述\n{desc}\n\n## 复现步骤\n1. \n\n## 期望行为\n",
            ver = env!("CARGO_PKG_VERSION"),
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            model = ctx.model,
            desc = title,
        );
        let mut url = format!(
            "https://github.com/{slug}/issues/new?body={}",
            percent_encode(&body)
        );
        if !title.is_empty() {
            url.push_str(&format!("&title={}", percent_encode(title)));
        }
        if open_browser(&url).await {
            Ok(CommandResult::Output(tr_fmt(
                "bug.opened",
                &[("url", &url)],
            )))
        } else {
            Ok(CommandResult::Output(tr_fmt(
                "bug.manual",
                &[("url", &url)],
            )))
        }
    }
}

// ── /review ───────────────────────────────────────────────────────────────────

pub struct ReviewCmd;

#[async_trait]
impl Command for ReviewCmd {
    fn name(&self) -> &str {
        "review"
    }
    fn description(&self) -> String {
        tr("review.desc")
    }
    fn usage(&self) -> String {
        "/review [PR编号]".to_string()
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        if !gh_available().await {
            return Ok(CommandResult::Output(tr("gh.missing")));
        }
        let pr = args.trim();
        let mut diff_args = vec!["pr", "diff"];
        if !pr.is_empty() {
            diff_args.push(pr);
        }
        let (ok, stdout, stderr) = run_capture("gh", &diff_args, &ctx.cwd).await;
        if !ok || stdout.trim().is_empty() {
            let err = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Ok(CommandResult::Output(tr_fmt(
                "review.failed",
                &[("err", err.trim())],
            )));
        }
        let prompt = tr_fmt("review.agent_prompt", &[("diff", &stdout)]);
        Ok(CommandResult::RunPrompt(prompt))
    }
}

// ── /pr-comments ──────────────────────────────────────────────────────────────

pub struct PrCommentsCmd;

#[async_trait]
impl Command for PrCommentsCmd {
    fn name(&self) -> &str {
        "pr-comments"
    }
    fn description(&self) -> String {
        tr("prcomments.desc")
    }
    fn usage(&self) -> String {
        "/pr-comments [PR编号]".to_string()
    }
    async fn run(&self, args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        if !gh_available().await {
            return Ok(CommandResult::Output(tr("gh.missing")));
        }
        let pr = args.trim();
        let mut view_args = vec!["pr", "view"];
        if !pr.is_empty() {
            view_args.push(pr);
        }
        view_args.push("--comments");
        let (ok, stdout, stderr) = run_capture("gh", &view_args, &ctx.cwd).await;
        if ok {
            Ok(CommandResult::Output(stdout))
        } else {
            Ok(CommandResult::Output(tr_fmt(
                "prcomments.failed",
                &[("err", stderr.trim())],
            )))
        }
    }
}

/// 创建包含所有内置命令的注册表
pub fn standard_registry() -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(CompactCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(AgentsCmd));
    reg.register(Arc::new(HooksCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ModelCmd));
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(McpCmd));
    reg.register(Arc::new(SkillsCmd));
    reg.register(Arc::new(PluginsCmd));
    reg.register(Arc::new(BugCmd));
    reg.register(Arc::new(ReviewCmd));
    reg.register(Arc::new(PrCommentsCmd));
    reg.register(Arc::new(QuitCmd));
    Arc::new(reg)
}

/// 创建包含内置命令 + 已加载 skill 的注册表
/// skill 先注册（优先级低），内置命令后注册（同名时覆盖 skill）
pub fn standard_registry_with_skills(
    home: &std::path::Path,
    cwd: &std::path::Path,
    disabled_skills: &std::collections::HashSet<String>,
    plugin_skill_sources: &[std::path::PathBuf],
) -> Arc<CommandRegistry> {
    let mut reg = CommandRegistry::new();

    // 先注册 skill（优先级低）
    for skill in crate::skill::load_skills(home, cwd, disabled_skills, plugin_skill_sources) {
        reg.register(skill);
    }

    // 再注册内置命令（后注册覆盖同名 skill）
    reg.register(Arc::new(HelpCmd));
    reg.register(Arc::new(ClearCmd));
    reg.register(Arc::new(CompactCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(AgentsCmd));
    reg.register(Arc::new(HooksCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ModelCmd));
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(McpCmd));
    reg.register(Arc::new(SkillsCmd));
    reg.register(Arc::new(PluginsCmd));
    reg.register(Arc::new(BugCmd));
    reg.register(Arc::new(ReviewCmd));
    reg.register(Arc::new(PrCommentsCmd));
    reg.register(Arc::new(QuitCmd));

    Arc::new(reg)
}

#[cfg(test)]
mod github_tests {
    use super::{parse_github_slug, percent_encode};

    #[test]
    fn slug_parses_https_and_ssh_forms() {
        assert_eq!(
            parse_github_slug("https://github.com/wangyooujin/wyj-code.git").as_deref(),
            Some("wangyooujin/wyj-code")
        );
        assert_eq!(
            parse_github_slug("git@github.com:wangyooujin/wyj-code.git").as_deref(),
            Some("wangyooujin/wyj-code")
        );
        assert_eq!(
            parse_github_slug("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_slug("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        // 非 GitHub 远程返回 None
        assert_eq!(parse_github_slug("https://gitlab.com/o/r.git"), None);
        assert_eq!(parse_github_slug("not-a-url"), None);
    }

    #[test]
    fn percent_encode_escapes_reserved_chars() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("x/y#z"), "x%2Fy%23z");
        // 未保留字符保持原样
        assert_eq!(percent_encode("Aa0-_.~"), "Aa0-_.~");
    }
}
