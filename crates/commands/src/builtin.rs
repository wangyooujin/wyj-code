//! 内置 Slash 命令

use crate::registry::{
    Command, CommandContext, CommandRegistry, CommandResult, SubAgentControlAction,
};
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
    async fn run(&self, _args: &str, ctx: &CommandContext) -> Result<CommandResult> {
        let version = env!("CARGO_PKG_VERSION");
        let mut text = tr_fmt("help.body", &[("version", version)]);
        // 运行时动态发现的命令（Skill / .claude/commands）因人而异，不进静态
        // help.body 模板，在此追加为独立分组（见 CLAUDE.md「Slash 命令约定」）。
        if !ctx.dynamic_commands.is_empty() {
            text.push_str(&format!("\n\n## {}\n\n", tr("help.custom_commands_header")));
            for (_name, desc, usage) in &ctx.dynamic_commands {
                text.push_str(&format!("- `{usage}` — {desc}\n"));
            }
        }
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

// ── /new ────────────────────────────────────────────────────────────────────
// 对齐 Claude Code `/new`：开启新会话，自动保存当前会话历史到磁盘后分配新
// session_id、清空 TUI 内存状态。无二次确认弹窗（用户已经明确输入了命令名）。
pub struct NewCmd;

#[async_trait]
impl Command for NewCmd {
    fn name(&self) -> &str {
        "new"
    }
    fn description(&self) -> String {
        tr("new.desc")
    }
    fn usage(&self) -> String {
        tr("new.usage")
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::StartNewSession)
    }
}

// ── /checkpoint /rewind /branch ──────────────────────────────────────────────

pub struct CheckpointCmd;

#[async_trait]
impl Command for CheckpointCmd {
    fn name(&self) -> &str {
        "checkpoint"
    }
    fn description(&self) -> String {
        "Create or list recoverable conversation/workspace checkpoints".to_string()
    }
    fn usage(&self) -> String {
        "/checkpoint [list|name]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let args = args.trim();
        Ok(CommandResult::CreateCheckpoint {
            list: args.eq_ignore_ascii_case("list"),
            name: (!args.is_empty() && !args.eq_ignore_ascii_case("list"))
                .then(|| args.to_string()),
        })
    }
}

pub struct RewindCmd;

#[async_trait]
impl Command for RewindCmd {
    fn name(&self) -> &str {
        "rewind"
    }
    fn description(&self) -> String {
        "Preview or restore conversation/files from a checkpoint".to_string()
    }
    fn usage(&self) -> String {
        "/rewind [checkpoint-id|latest] [conversation|files|both] [--confirm]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let mut checkpoint_id = None;
        let mut scope = wyj_core::RewindScope::Both;
        let mut confirmed = false;
        for token in args.split_whitespace() {
            match token.to_ascii_lowercase().as_str() {
                "latest" => checkpoint_id = None,
                "conversation" => scope = wyj_core::RewindScope::Conversation,
                "files" => scope = wyj_core::RewindScope::Files,
                "both" => scope = wyj_core::RewindScope::Both,
                "--confirm" => confirmed = true,
                _ if checkpoint_id.is_none() => checkpoint_id = Some(token.to_string()),
                _ => anyhow::bail!("usage: {}", self.usage()),
            }
        }
        Ok(CommandResult::Rewind {
            checkpoint_id,
            scope,
            confirmed,
        })
    }
}

pub struct BranchCmd;

#[async_trait]
impl Command for BranchCmd {
    fn name(&self) -> &str {
        "branch"
    }
    fn description(&self) -> String {
        "Create a new session from a checkpoint without changing the original".to_string()
    }
    fn usage(&self) -> String {
        "/branch [checkpoint-id|latest] [--restore-files] [--confirm]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let mut checkpoint_id = None;
        let mut restore_files = false;
        let mut confirmed = false;
        for token in args.split_whitespace() {
            match token.to_ascii_lowercase().as_str() {
                "latest" => checkpoint_id = None,
                "--restore-files" => restore_files = true,
                "--confirm" => confirmed = true,
                _ if checkpoint_id.is_none() => checkpoint_id = Some(token.to_string()),
                _ => anyhow::bail!("usage: {}", self.usage()),
            }
        }
        Ok(CommandResult::BranchSession {
            checkpoint_id,
            restore_files,
            confirmed,
        })
    }
}

pub struct AgentControlCmd;

#[async_trait]
impl Command for AgentControlCmd {
    fn name(&self) -> &str {
        "agent-control"
    }
    fn description(&self) -> String {
        "Send follow-up, interrupt or retry to a running sub-agent".to_string()
    }
    fn usage(&self) -> String {
        "/agent-control <id> <follow-up text|interrupt|retry>".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let mut parts = args.trim().splitn(3, char::is_whitespace);
        let id = parts
            .next()
            .and_then(|value| value.trim_start_matches('a').parse::<u64>().ok())
            .ok_or_else(|| anyhow::anyhow!("usage: {}", self.usage()))?;
        let action = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("usage: {}", self.usage()))?;
        let action = match action {
            "interrupt" => SubAgentControlAction::Interrupt,
            "retry" | "retry-last" => SubAgentControlAction::RetryLast,
            "follow-up" | "followup" => {
                let text = parts.next().unwrap_or_default().trim();
                anyhow::ensure!(!text.is_empty(), "follow-up text is required");
                SubAgentControlAction::FollowUp(text.to_string())
            }
            _ => anyhow::bail!("usage: {}", self.usage()),
        };
        Ok(CommandResult::ControlSubAgent { id, action })
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
        let full_input = input + ctx.cache_read_tokens + ctx.cache_write_tokens;
        let hit_ratio = if full_input > 0 {
            ctx.cache_read_tokens as f64 / full_input as f64 * 100.0
        } else {
            0.0
        };
        let input_detail_line = format!(
            "\n{}",
            tr_fmt(
                "cost.input_detail_line",
                &[
                    ("full", &fmt_num(full_input)),
                    ("uncached", &fmt_num(input)),
                    ("hit_ratio", &format!("{hit_ratio:.0}")),
                ],
            )
        );

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
        let mut text =
            format!("{header}\n{cost_line}{input_detail_line}{cache_line}\n\n{ctx_line}");

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

// ── /schedule ─────────────────────────────────────────────────────────────────

pub struct ScheduleCmd;

#[async_trait]
impl Command for ScheduleCmd {
    fn name(&self) -> &str {
        "schedule"
    }
    fn description(&self) -> String {
        tr("schedule.desc")
    }
    fn usage(&self) -> String {
        "/schedule".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenScheduleDialog)
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

// ── /extensions ───────────────────────────────────────────────────────────────

pub struct ExtensionsCmd;

#[async_trait]
impl Command for ExtensionsCmd {
    fn name(&self) -> &str {
        "extensions"
    }
    fn description(&self) -> String {
        "统一查看和诊断 Skill / MCP / Plugin 资源".to_string()
    }
    fn usage(&self) -> String {
        "/extensions".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenExtensionsDialog)
    }
}

// ── /import ───────────────────────────────────────────────────────────────────

pub struct ImportCmd;

#[async_trait]
impl Command for ImportCmd {
    fn name(&self) -> &str {
        "import"
    }
    fn description(&self) -> String {
        tr("import.desc")
    }
    fn usage(&self) -> String {
        "/import".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenImportDialog)
    }
}

// ── /agents ───────────────────────────────────────────────────────────────────

pub struct AgentsCmd;

pub fn format_agents_text(defs: &[wyj_core::AgentDefinition]) -> String {
    let mut out = tr("agents.header");
    for d in defs {
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
    out
}

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
        let fallback_text = format_agents_text(&defs);
        Ok(CommandResult::OpenAgentsDialog {
            defs,
            fallback_text,
        })
    }
}

/// 打开/定位子 Agent 聚合面板；`args` 可选，形如 `a3` 或 `3`（Hub 分配的 id）。
pub struct SubAgentsCmd;

#[async_trait]
impl Command for SubAgentsCmd {
    fn name(&self) -> &str {
        "subagents"
    }
    fn description(&self) -> String {
        tr("subagents.desc")
    }
    fn usage(&self) -> String {
        "/subagents [id]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        let arg = args.trim();
        if arg.is_empty() {
            return Ok(CommandResult::OpenSubAgentsPanel(None));
        }
        let digits = arg.strip_prefix(['a', 'A']).unwrap_or(arg);
        match digits.parse::<u64>() {
            Ok(id) => Ok(CommandResult::OpenSubAgentsPanel(Some(id))),
            Err(_) => Ok(CommandResult::Output(tr_fmt(
                "subagents.bad_id",
                &[("arg", arg)],
            ))),
        }
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
        "/memory [clear-all]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        // 子命令：`clear-all` → TUI 弹二级确认面板；
        // 其它（含空 args / 任意未知子命令）→ 打开正常 Memory 面板。
        let trimmed = args.trim();
        if trimmed.eq_ignore_ascii_case("clear-all") || trimmed.eq_ignore_ascii_case("clear_all") {
            // 当前 active / superseded 数由 TUI 从 store.status() 取，命令层不依赖具体数。
            return Ok(CommandResult::OpenMemoryClearAllConfirm {
                active_count: 0,
                superseded_count: 0,
            });
        }
        Ok(CommandResult::OpenMemoryDialog)
    }
}

// ── /evolve ──────────────────────────────────────────────────────────────────

pub struct EvolveCmd;

#[async_trait]
impl Command for EvolveCmd {
    fn name(&self) -> &str {
        "evolve"
    }
    fn description(&self) -> String {
        tr("evolve.desc")
    }
    fn usage(&self) -> String {
        "/evolve".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenEvolutionDialog)
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
            Ok(_) => lines.push(tr_fmt(
                "status.api_key_ok",
                &[("prefix", &cfg.redacted_api_key().unwrap_or_default())],
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
        let mem_dir = wyj_config::global_config_dir_in(&ctx.home_dir)
            .join("memory")
            .join(&pid);
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

// ── /computer ─────────────────────────────────────────────────────────────────

pub struct ComputerCmd;

#[async_trait]
impl Command for ComputerCmd {
    fn name(&self) -> &str {
        "computer"
    }
    fn description(&self) -> String {
        tr("computer.desc")
    }
    fn usage(&self) -> String {
        "/computer".to_string()
    }
    async fn run(&self, _args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        use wyj_config::{Config, ForegroundFallback, Provider};

        let mut lines = vec![tr("computer.header")];

        let os = std::env::consts::OS;
        if wyj_computer::SUPPORTED {
            lines.push(tr_fmt("computer.platform_supported", &[("os", os)]));
        } else {
            lines.push(tr_fmt("computer.platform_unsupported", &[("os", os)]));
        }

        // 门控逻辑需与 `cli::register_computer_tool_if_enabled` 的真实注册条件
        // 保持一致（平台 → vision → provider），否则诊断结果会跟实际是否
        // 注册该工具对不上。
        let cfg = Config::load()?;
        let profile = cfg.active_profile();

        if !wyj_computer::SUPPORTED {
            lines.push(tr("computer.not_registered_platform"));
        } else if !profile.vision {
            lines.push(tr("computer.not_registered_vision"));
        } else if !matches!(profile.provider, Provider::Anthropic) {
            lines.push(tr_fmt(
                "computer.not_registered_provider",
                &[("provider", &profile.provider.to_string())],
            ));
        } else if profile.is_official_anthropic_endpoint() {
            lines.push(tr("computer.mode_native"));
        } else {
            lines.push(tr("computer.mode_custom"));
        }

        let fallback = match cfg.computer_use.foreground_fallback {
            ForegroundFallback::Disabled => "disabled",
            ForegroundFallback::Ask => "ask",
            ForegroundFallback::IdleOnly => "idle_only",
        };
        lines.push(tr_fmt(
            "computer.foreground_policy",
            &[
                ("policy", fallback),
                ("quiet_ms", &cfg.computer_use.quiet_period_ms.to_string()),
                ("max_defer", &cfg.computer_use.max_defer_secs.to_string()),
            ],
        ));

        #[cfg(target_os = "macos")]
        {
            lines.push(tr("computer.background_supported"));
            if wyj_computer::accessibility::is_process_trusted() {
                lines.push(tr("computer.ax_trusted"));
            } else {
                lines.push(tr("computer.ax_untrusted"));
            }
        }
        #[cfg(not(target_os = "macos"))]
        lines.push(tr("computer.background_unavailable"));

        // 实时探测（截图/光标）即使工具未注册也照跑：既能帮用户提前定位权限
        // 问题（先解决系统权限，再切换 profile 满足注册条件），也不需要
        // 真的注册一份 ComputerTool 才能自检。
        if wyj_computer::SUPPORTED {
            let initial_status = wyj_computer::activity::ensure_monitor();
            if initial_status == wyj_computer::activity::InputMonitorStatus::Starting {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            match wyj_computer::activity::snapshot() {
                Ok(snapshot) => {
                    let idle = snapshot
                        .external_idle_secs
                        .map(|idle| format!("{idle:.2}"))
                        .unwrap_or_else(|| "n/a".to_string());
                    lines.push(tr_fmt(
                        "computer.input_monitor",
                        &[
                            ("status", snapshot.monitor_status.label()),
                            ("idle", &idle),
                            (
                                "seq",
                                &snapshot
                                    .external_event_seq
                                    .map(|seq| seq.to_string())
                                    .unwrap_or_else(|| "n/a".to_string()),
                            ),
                        ],
                    ));
                    if let Some(error) = snapshot.monitor_error {
                        lines.push(tr_fmt("computer.input_monitor_error", &[("err", &error)]));
                    }
                }
                Err(error) => lines.push(tr_fmt(
                    "computer.input_monitor_error",
                    &[("err", &error.to_string())],
                )),
            }

            match wyj_computer::target::list_windows() {
                Ok(windows) => lines.push(tr_fmt(
                    "computer.window_count",
                    &[("count", &windows.len().to_string())],
                )),
                Err(error) => lines.push(tr_fmt(
                    "computer.window_error",
                    &[("err", &error.to_string())],
                )),
            }

            let metrics = wyj_computer::telemetry::snapshot();
            lines.push(tr_fmt(
                "computer.telemetry_paths",
                &[
                    ("background", &metrics.background_actions.to_string()),
                    ("targeted", &metrics.targeted_pid_events.to_string()),
                    ("foreground", &metrics.foreground_actions.to_string()),
                    (
                        "auto_fallback",
                        &metrics.automatic_foreground_fallbacks.to_string(),
                    ),
                ],
            ));
            lines.push(tr_fmt(
                "computer.telemetry_guards",
                &[
                    ("preempted", &metrics.preempted_by_user.to_string()),
                    ("changed", &metrics.target_changed.to_string()),
                    ("requires", &metrics.requires_foreground.to_string()),
                    ("fuses", &metrics.background_focus_fuses.to_string()),
                ],
            ));

            match wyj_computer::primary_display_size() {
                Ok(d) => {
                    let (tw, th) = wyj_computer::scale::fit_within(
                        d.physical_width,
                        d.physical_height,
                        wyj_computer::DEFAULT_MAX_DIM,
                    );
                    lines.push(tr_fmt(
                        "computer.display_ok",
                        &[
                            ("pw", &d.physical_width.to_string()),
                            ("ph", &d.physical_height.to_string()),
                            ("tw", &tw.to_string()),
                            ("th", &th.to_string()),
                        ],
                    ));
                }
                Err(e) => lines.push(tr_fmt("computer.display_err", &[("err", &e.to_string())])),
            }

            match wyj_computer::capture_primary(64) {
                Ok(cap) => lines.push(tr_fmt(
                    "computer.screenshot_ok",
                    &[("bytes", &cap.png.len().to_string())],
                )),
                Err(e) => lines.push(tr_fmt(
                    "computer.screenshot_err",
                    &[("err", &e.to_string())],
                )),
            }

            match wyj_computer::cursor_location() {
                Ok((x, y)) => lines.push(tr_fmt(
                    "computer.input_ok",
                    &[("x", &x.to_string()), ("y", &y.to_string())],
                )),
                Err(e) => lines.push(tr_fmt("computer.input_err", &[("err", &e.to_string())])),
            }

            // 光标位置读取成功不代表输入合成一定生效：macOS 未授权「辅助功能」
            // 时 Enigo::new() 往往仍会成功，点击/按键被系统静默丢弃、不报错，
            // 这种失败模式没有错误可捕获，只能靠固定提醒兜底。
            lines.push(tr("computer.accessibility_hint"));
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
        "/model [profile-name] | /model doctor [profile-name]".to_string()
    }
    async fn run(&self, args: &str, _ctx: &CommandContext) -> Result<CommandResult> {
        if args.trim().is_empty() {
            return Ok(CommandResult::OpenProfileDialog);
        }
        let mut parts = args.split_whitespace();
        if parts.next() == Some("doctor") {
            return Ok(CommandResult::ModelDoctor(parts.next().map(str::to_string)));
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
        // 骨架创建是确定性代码，不依赖 LLM：先确保 .wyj-code/ 目录 + 带注释的
        // 空 mcp.toml/settings.toml 模板存在（已存在则不动，防止覆盖用户已填
        // 内容），再触发 agent 回合生成/合并 CLAUDE.md。失败按 best-effort
        // 处理——骨架初始化是锦上添花，不应该让 /init 的核心功能（生成
        // CLAUDE.md）因为这一步失败而整体失败。
        ensure_project_config_skeleton(&ctx.cwd);

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

const PROJECT_MCP_TEMPLATE: &str = r#"# 项目级 MCP server 定义（随仓库共享，团队成员克隆后自动生效）。
# 格式与全局 ~/.wyj-code/config.toml 的 [[mcp_servers]] 段一致，同名覆盖全局配置。
# 首次连接前需要在 TUI 里确认信任（防止克隆到陌生仓库时静默执行任意命令）。
#
# [[mcp_servers]]
# name = "postgres"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-postgres"]

mcp_servers = []
"#;

const PROJECT_SETTINGS_TEMPLATE: &str = r#"# 项目级开关：控制本项目禁用哪些 skill / MCP server（按名字禁用，无论
# 条目来源——六层合并链任意一层、手写进 mcp.toml 的条目均适用）。
# 不影响 skill 文件内容或 mcp.toml 里的 server 定义本身，只是一层开关。
#
# disabled_skills = ["some-skill-name"]
# disabled_mcp_servers = ["some-server-name"]

disabled_skills = []
disabled_mcp_servers = []
"#;

/// 确保 `<git-root>/.wyj-code/` 目录 + 带注释的空 `mcp.toml`/`settings.toml` 模板存在。
/// 幂等：已存在的文件不覆盖。不预建空的 `skills/`/`agents/` 子目录——Git 不
/// 追踪空目录，预建了也不会随 `/init` 一起被提交，等真正需要时由既有的
/// `skill_install.rs` 等惰性 `create_dir_all` 即可。
fn ensure_project_config_skeleton(cwd: &std::path::Path) {
    let dir = wyj_config::project_config_dir(cwd);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("创建项目配置目录失败: {e}");
        return;
    }
    let mcp_path = dir.join("mcp.toml");
    if !mcp_path.exists() {
        if let Err(e) = std::fs::write(&mcp_path, PROJECT_MCP_TEMPLATE) {
            tracing::warn!("写入 mcp.toml 模板失败: {e}");
        }
    }
    let settings_path = dir.join("settings.toml");
    if !settings_path.exists() {
        if let Err(e) = std::fs::write(&settings_path, PROJECT_SETTINGS_TEMPLATE) {
            tracing::warn!("写入 settings.toml 模板失败: {e}");
        }
    }
}

#[cfg(test)]
mod init_tests {
    use super::*;

    #[test]
    fn creates_skeleton_files_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        ensure_project_config_skeleton(dir.path());

        let mcp_path = dir.path().join(".wyj-code").join("mcp.toml");
        let settings_path = dir.path().join(".wyj-code").join("settings.toml");
        assert!(mcp_path.exists());
        assert!(settings_path.exists());

        // 生成的模板必须是合法可解析的 TOML，且是空配置（不预设任何 server/开关）。
        let servers = wyj_config::load_project_mcp(dir.path()).unwrap();
        assert!(servers.is_empty());
        let settings = wyj_config::load_project_settings(dir.path()).unwrap();
        assert!(settings.disabled_skills.is_empty());
        assert!(settings.disabled_mcp_servers.is_empty());
    }

    #[test]
    fn does_not_overwrite_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        wyj_config::save_project_mcp(
            dir.path(),
            &[wyj_config::McpServerConfig {
                name: "existing".to_string(),
                transport: wyj_config::McpTransport::Stdio,
                command: Some("npx".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
                headers: Default::default(),
            }],
        )
        .unwrap();

        ensure_project_config_skeleton(dir.path());

        let servers = wyj_config::load_project_mcp(dir.path()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "existing");
    }

    #[test]
    fn init_from_nested_cwd_creates_skeleton_at_git_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("crates").join("demo");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_project_config_skeleton(&nested);

        assert!(repo.path().join(".wyj-code").join("mcp.toml").exists());
        assert!(repo.path().join(".wyj-code").join("settings.toml").exists());
        assert!(!nested.join(".wyj-code").exists());
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
    reg.register(Arc::new(NewCmd));
    reg.register(Arc::new(CheckpointCmd));
    reg.register(Arc::new(RewindCmd));
    reg.register(Arc::new(BranchCmd));
    reg.register(Arc::new(AgentControlCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(AgentsCmd));
    reg.register(Arc::new(SubAgentsCmd));
    reg.register(Arc::new(HooksCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(EvolveCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ComputerCmd));
    reg.register(Arc::new(ModelCmd));
    // SandboxCmd 已随 OS sandbox 一起移除；/sandbox 不再注册。
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(McpCmd));
    reg.register(Arc::new(ScheduleCmd));
    reg.register(Arc::new(SkillsCmd));
    reg.register(Arc::new(PluginsCmd));
    reg.register(Arc::new(ExtensionsCmd));
    reg.register(Arc::new(ImportCmd));
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
    reg.register(Arc::new(NewCmd));
    reg.register(Arc::new(CheckpointCmd));
    reg.register(Arc::new(RewindCmd));
    reg.register(Arc::new(BranchCmd));
    reg.register(Arc::new(AgentControlCmd));
    reg.register(Arc::new(CostCmd));
    reg.register(Arc::new(AgentsCmd));
    reg.register(Arc::new(SubAgentsCmd));
    reg.register(Arc::new(HooksCmd));
    reg.register(Arc::new(MemoryCmd));
    reg.register(Arc::new(EvolveCmd));
    reg.register(Arc::new(DoctorCmd));
    reg.register(Arc::new(ComputerCmd));
    reg.register(Arc::new(ModelCmd));
    // SandboxCmd 已随 OS sandbox 一起移除；/sandbox 不再注册。
    reg.register(Arc::new(ModeCmd));
    reg.register(Arc::new(CwdCmd));
    reg.register(Arc::new(ResumeCmd));
    reg.register(Arc::new(SessionsCmd));
    reg.register(Arc::new(InitCmd));
    reg.register(Arc::new(ConfigCmd));
    reg.register(Arc::new(McpCmd));
    reg.register(Arc::new(ScheduleCmd));
    reg.register(Arc::new(SkillsCmd));
    reg.register(Arc::new(PluginsCmd));
    reg.register(Arc::new(ExtensionsCmd));
    reg.register(Arc::new(ImportCmd));
    reg.register(Arc::new(BugCmd));
    reg.register(Arc::new(ReviewCmd));
    reg.register(Arc::new(PrCommentsCmd));
    reg.register(Arc::new(QuitCmd));

    Arc::new(reg)
}

#[cfg(test)]
mod help_tests {
    use super::*;

    fn ctx_with_dynamic(dynamic_commands: Vec<(String, String, String)>) -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            context_window: 0,
            estimated_tokens: 0,
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands,
        }
    }

    #[tokio::test]
    async fn help_appends_custom_commands_section_when_dynamic_commands_present() {
        let ctx = ctx_with_dynamic(vec![(
            "fix-issue".to_string(),
            "修复一个 issue".to_string(),
            "/fix-issue <issue-number>".to_string(),
        )]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains(&tr("help.custom_commands_header")));
        assert!(text.contains("/fix-issue <issue-number>"));
        assert!(text.contains("修复一个 issue"));
    }

    #[tokio::test]
    async fn help_omits_custom_commands_section_when_none_dynamic() {
        let ctx = ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(!text.contains(&tr("help.custom_commands_header")));
    }

    #[tokio::test]
    async fn help_body_documents_subagents_command() {
        let ctx = ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains("/subagents"));
    }

    #[tokio::test]
    async fn evolve_is_registered_and_documented_in_help() {
        let registry = standard_registry();
        let command = registry.get("evolve").expect("/evolve is registered");
        assert_eq!(command.usage(), "/evolve");

        let ctx = ctx_with_dynamic(vec![]);
        let CommandResult::Output(text) = HelpCmd.run("", &ctx).await.unwrap() else {
            panic!("expected Output");
        };
        assert!(text.contains("/evolve"));

        let result = registry.dispatch("/evolve", &ctx).await.unwrap().unwrap();
        assert!(matches!(result, CommandResult::OpenEvolutionDialog));
    }
}

#[cfg(test)]
mod agents_tests {
    use super::*;

    fn empty_ctx() -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            context_window: 0,
            estimated_tokens: 0,
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands: vec![],
        }
    }

    #[tokio::test]
    async fn agents_command_returns_dialog_data_with_text_fallback() {
        let result = AgentsCmd.run("", &empty_ctx()).await.unwrap();
        let CommandResult::OpenAgentsDialog {
            defs,
            fallback_text,
        } = result
        else {
            panic!("expected OpenAgentsDialog");
        };
        assert!(defs.iter().any(|d| d.name == "general-purpose"));
        assert!(fallback_text.contains("general-purpose"));
    }
}

#[cfg(test)]
mod subagents_tests {
    use super::*;

    fn empty_ctx() -> CommandContext {
        CommandContext {
            cwd: std::path::PathBuf::new(),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            context_window: 0,
            estimated_tokens: 0,
            home_dir: std::path::PathBuf::new(),
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            effective_mcp_count: 0,
            plugin_agent_paths: vec![],
            hooks_enabled: false,
            dynamic_commands: vec![],
        }
    }

    #[tokio::test]
    async fn no_args_opens_panel_without_target_id() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("", &ctx).await.unwrap();
        assert!(matches!(result, CommandResult::OpenSubAgentsPanel(None)));
    }

    #[tokio::test]
    async fn numeric_arg_parses_to_target_id() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("3", &ctx).await.unwrap();
        assert!(matches!(result, CommandResult::OpenSubAgentsPanel(Some(3))));
    }

    #[tokio::test]
    async fn a_prefixed_arg_parses_to_target_id() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("a12", &ctx).await.unwrap();
        assert!(matches!(
            result,
            CommandResult::OpenSubAgentsPanel(Some(12))
        ));
    }

    #[tokio::test]
    async fn garbage_arg_reports_error_instead_of_opening_panel() {
        let ctx = empty_ctx();
        let result = SubAgentsCmd.run("not-a-number", &ctx).await.unwrap();
        match result {
            CommandResult::Output(text) => assert!(text.contains("not-a-number")),
            other => panic!("expected Output error text, got {other:?}"),
        }
    }
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
