use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandRegistry, CommandResult};
use wyj_config::{AgentMode, Config, RoutingRole};
use wyj_core::{
    extract_preview, extract_title, new_session_id, now_iso, Agent, ExecutionSurface, HistoryEntry,
    HistoryStore, HookRunner, MemoryStore, Session, SessionFile, SessionStore, SummaryGenerator,
    ToolEvent,
};
use wyj_tools::{
    AskQuestionTool, PermissionMode, SubAgentTool, TodoStore, TodoWriteTool, ToolCtx, ToolRegistry,
};

mod extensions_cmd;
mod schedule_cmd;
mod trust_cmd;
mod update_cmd;

#[derive(Parser, Debug)]
#[command(name = "wyj-code", version = env!("CARGO_PKG_VERSION"),
          about = wyj_i18n::tr("cli.about"))]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(long)]
    config_status: bool,
    #[arg(short = 'p', long, help = wyj_i18n::tr("cli.prompt_help"))]
    prompt: Option<String>,
    #[arg(long, help = wyj_i18n::tr("cli.cwd_help"))]
    cwd: Option<std::path::PathBuf>,
    #[arg(long, help = wyj_i18n::tr("cli.headless_help"))]
    headless: bool,
    #[arg(long, help = wyj_i18n::tr("cli.plan_help"))]
    plan: bool,
    #[arg(long, help = wyj_i18n::tr("cli.bypass_help"))]
    bypass_permissions: bool,
    /// 本次进程允许调用的工具名；逗号分隔。仅工具名不会自动授权写入范围。
    #[arg(long, value_delimiter = ',')]
    allowed_tools: Vec<String>,
    /// 本次进程允许写入的目录，可重复指定。
    #[arg(long = "allow-write")]
    allow_write: Vec<std::path::PathBuf>,
    /// 本次进程允许访问的网络域名，可重复指定。
    #[arg(long = "allow-network")]
    allow_network: Vec<String>,
    /// Plan 模式本轮额外允许修改的单个文档路径，可重复指定。
    #[arg(long = "allow-plan-write")]
    allow_plan_write: Vec<std::path::PathBuf>,
    /// Bash/Agent 等进程工具没有 sandbox 时直接拒绝。
    #[arg(long)]
    require_sandbox: bool,
    #[arg(short = 'c', long = "continue", help = wyj_i18n::tr("cli.continue_help"))]
    continue_session: bool,
    #[arg(long, help = wyj_i18n::tr("cli.resume_help"))]
    resume: Option<String>,
    #[arg(long, help = wyj_i18n::tr("cli.profile_help"))]
    profile: Option<String>,
    #[arg(long, help = wyj_i18n::tr("cli.plugin_dir_help"))]
    plugin_dir: Option<std::path::PathBuf>,
    #[arg(long, help = wyj_i18n::tr("cli.no_hooks_help"))]
    no_hooks: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = wyj_i18n::tr("cli.update_about"))]
    Update {
        #[arg(short = 'y', long, help = wyj_i18n::tr("cli.update_yes_help"))]
        yes: bool,
    },
    /// 查看指定会话落盘的子 Agent 执行轨迹（`~/.wyj-code/sessions/<id>.subagents/`）。
    /// 纯读、不影响任何运行中状态；对应 TUI 内 `/subagents` 命令的 headless 版本。
    #[command(name = "subagent-trace", about = wyj_i18n::tr("cli.subagent_trace_about"))]
    SubagentTrace {
        #[arg(help = wyj_i18n::tr("cli.subagent_trace_session_id_help"))]
        session_id: String,
        #[arg(help = wyj_i18n::tr("cli.subagent_trace_sub_id_help"))]
        sub_id: Option<u64>,
        #[arg(long, help = wyj_i18n::tr("cli.subagent_trace_json_help"))]
        json: bool,
    },
    #[command(name = "extensions", about = "Manage Skill, MCP and Plugin resources")]
    Extensions {
        #[command(subcommand)]
        command: extensions_cmd::ExtensionCommand,
    },
    /// 管理定时任务（增删改查 + 同步系统 crontab）；`schedule run <id>` 是真正
    /// 被 crontab 调用的执行入口，对应 TUI 内 `/schedule` 面板的 headless 版本。
    #[command(name = "schedule", about = "Manage scheduled tasks (cron-triggered)")]
    Schedule {
        #[command(subcommand)]
        command: schedule_cmd::ScheduleCommand,
    },
    /// 批准当前项目级 MCP server（`.wyj-code/mcp.toml`/`.mcp.json`）的信任确认。
    /// 无 UI 通道的场景（`-p`/`--headless`/`schedule run`）会跳过未批准的
    /// 项目级 server 而不连接，配 cron 任务前先用这个命令批准一次。
    #[command(name = "trust-mcp", about = wyj_i18n::tr("cli.trust_mcp_about"))]
    TrustMcp,
    /// Inspect model identity, capabilities and optional live compatibility probes.
    #[command(name = "model")]
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Manage persisted session checkpoints, rewind and branches.
    #[command(name = "session")]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Inspect the effective OS sandbox and network-isolation capabilities.
    #[command(name = "sandbox")]
    Sandbox {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ModelCommand {
    /// Static diagnosis is free; --probe uses only WYJ_CODE_PROBE_API_KEY.
    Doctor {
        profile: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, value_parser = ["basic", "full"])]
        probe: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SessionCommand {
    Checkpoint {
        session_id: String,
        #[arg(long)]
        name: Option<String>,
    },
    Checkpoints {
        session_id: String,
    },
    Rewind {
        session_id: String,
        checkpoint_id: String,
        #[arg(long, default_value = "both", value_parser = ["conversation", "files", "both"])]
        scope: String,
        /// Required before any file is overwritten or removed.
        #[arg(long)]
        force: bool,
    },
    Branch {
        session_id: String,
        checkpoint_id: String,
        #[arg(long)]
        restore_files: bool,
        /// Required with --restore-files when the workspace differs.
        #[arg(long)]
        force: bool,
    },
}

/// `wyj-code subagent-trace <session_id> [<sub_id>] [--json]`：纯读命令，
/// 打印落盘的子 Agent trace（见 `wyj_tools::trace`）。不带 sub_id 列出该会话
/// 全部子 Agent 概览；带 sub_id 打印完整工具序列 + 全文 input/output + 最终结果，
/// `--json` 直接吐原始 JSONL 供管道处理。
fn run_subagent_trace_cmd(session_id: &str, sub_id: Option<u64>, json: bool) -> Result<()> {
    use wyj_tools::trace::{list_trace_ids, read_trace, trace_file};

    let sessions_dir = wyj_config::config_dir()?.join("sessions");

    match sub_id {
        None => {
            let ids = list_trace_ids(&sessions_dir, session_id);
            if ids.is_empty() {
                println!(
                    "{}",
                    wyj_i18n::tr_fmt("subagent_trace.no_records", &[("session", session_id)])
                );
                return Ok(());
            }
            for id in ids {
                let path = trace_file(&sessions_dir, session_id, id);
                let events = read_trace(&path).unwrap_or_default();
                print_subagent_trace_summary(id, &events);
            }
        }
        Some(id) => {
            let path = trace_file(&sessions_dir, session_id, id);
            let events = read_trace(&path).unwrap_or_default();
            if events.is_empty() {
                println!(
                    "{}",
                    wyj_i18n::tr_fmt(
                        "subagent_trace.not_found",
                        &[("session", session_id), ("id", &id.to_string())]
                    )
                );
                return Ok(());
            }
            if json {
                print!("{}", std::fs::read_to_string(&path).unwrap_or_default());
            } else {
                print_subagent_trace_detail(id, &events);
            }
        }
    }
    Ok(())
}

fn print_subagent_trace_summary(id: u64, events: &[wyj_tools::trace::TraceEvent]) {
    use wyj_tools::trace::TraceEvent as TE;
    let (mut agent_type, mut description) = (String::new(), String::new());
    let mut status = "interrupted";
    let mut elapsed = 0.0_f64;
    let mut tool_calls = 0usize;
    let (mut in_tok, mut out_tok) = (0u32, 0u32);
    for ev in events {
        match ev {
            TE::Started {
                agent_type: t,
                description: d,
                ..
            } => {
                agent_type = t.clone();
                description = d.clone();
            }
            TE::ToolStart { .. } => tool_calls += 1,
            TE::Usage {
                input_tokens,
                output_tokens,
            } => {
                in_tok += input_tokens;
                out_tok += output_tokens;
            }
            TE::Done {
                is_error,
                elapsed_secs,
                ..
            } => {
                status = if *is_error { "failed" } else { "done" };
                elapsed = *elapsed_secs;
            }
            TE::ToolEnd { .. } | TE::Control { .. } => {}
        }
    }
    println!(
        "a{id}  {agent_type}({description})  [{status}]  {elapsed:.1}s  {tool_calls} tool calls  ↑{in_tok} ↓{out_tok}"
    );
}

fn print_subagent_trace_detail(id: u64, events: &[wyj_tools::trace::TraceEvent]) {
    use wyj_tools::trace::TraceEvent as TE;
    println!("=== a{id} ===");
    for ev in events {
        match ev {
            TE::Started {
                agent_type,
                description,
                background,
                parent_tool_use_id,
            } => {
                println!(
                    "[started] {agent_type}({description})  background={background}  parent_tool_use_id={}",
                    parent_tool_use_id.as_deref().unwrap_or("-")
                );
            }
            TE::ToolStart {
                tool_name,
                input_json,
                truncated,
            } => {
                let mark = if *truncated { " [truncated]" } else { "" };
                println!("  > {tool_name}{mark}\n    input: {input_json}");
            }
            TE::ToolEnd {
                tool_name,
                is_error,
                elapsed_secs,
                output,
                truncated,
            } => {
                let ok = if *is_error { "✗" } else { "✓" };
                let mark = if *truncated { " [truncated]" } else { "" };
                println!("  {ok} {tool_name} ({elapsed_secs:.1}s){mark}\n    output: {output}");
            }
            TE::Usage {
                input_tokens,
                output_tokens,
            } => {
                println!("  usage: ↑{input_tokens} ↓{output_tokens}");
            }
            TE::Control { action, accepted } => {
                println!("  control: {action} accepted={accepted}");
            }
            TE::Done {
                result,
                is_error,
                elapsed_secs,
            } => {
                let ok = if *is_error { "✗" } else { "✓" };
                println!("[done] {ok} {elapsed_secs:.1}s\n{result}");
            }
        }
    }
}

/// TUI 模式下 tracing 日志的落盘位置：`~/.wyj-code/logs/wyj-code.log`（追加写入，
/// 不轮转/不清理——诊断用途，量级远小于会话历史）。失败（如权限问题）由调用方
/// 降级为丢弃日志，不回退到 stdout（那样又会重新污染 TUI 画面）。
fn open_tui_log_file() -> std::io::Result<std::fs::File> {
    let dir = wyj_config::config_dir().map_err(|e| std::io::Error::other(e.to_string()))?;
    let log_dir = dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("wyj-code.log"))
}

async fn run_model_command(command: ModelCommand, cfg: &Config) -> Result<()> {
    match command {
        ModelCommand::Doctor {
            profile,
            json,
            probe,
            refresh,
        } => {
            let selected = match profile {
                Some(name) => cfg
                    .profile_by_name(&name)
                    .ok_or_else(|| anyhow::anyhow!("未找到 Profile: {name}"))?,
                None => cfg.active_profile(),
            };
            let config_base = wyj_config::config_dir()?;
            let cache = wyj_api::CapabilityCache::new(&config_base);
            if let Some(level) = probe.as_deref() {
                let requests = if level == "full" { 4 } else { 2 };
                eprintln!(
                    "model doctor probe={level}: will send up to {requests} minimal request(s); no file, shell, MCP or computer tools are exposed"
                );
                let capabilities = run_model_probe(selected, level).await?;
                let identity = wyj_api::ModelCatalog::resolve(selected, None).identity;
                cache.store(identity, capabilities)?;
            }
            let report = wyj_api::ModelDoctorReport::static_report(
                selected,
                if refresh && probe.is_none() {
                    None
                } else {
                    Some(&cache)
                },
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_model_doctor_report(&report);
            }
        }
    }
    Ok(())
}

async fn run_model_probe(
    profile: &wyj_config::Profile,
    level: &str,
) -> Result<wyj_api::ModelCapabilities> {
    use wyj_api::types::{ContentBlock, Message, ToolDefinition};
    use wyj_api::{Capability, CapabilitySource, Confidence};

    // 安全边界：live probe 绝不读取配置文件里可能已暴露/陈旧的 key，只接受
    // 用户为本次诊断显式注入的独立环境变量。
    let probe_key = std::env::var("WYJ_CODE_PROBE_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "live probe requires a rotated key in WYJ_CODE_PROBE_API_KEY; configured profile keys are intentionally ignored"
        )
    })?;
    if probe_key.trim().is_empty() {
        anyhow::bail!("WYJ_CODE_PROBE_API_KEY is empty");
    }
    let mut probe_profile = profile.clone();
    probe_profile.api_key_env = None;
    probe_profile.api_key = Some(probe_key);
    let provider = wyj_api::build_provider_from_profile(&probe_profile, None)?;
    let static_resolution = wyj_api::ModelCatalog::resolve(&probe_profile, None);
    let mut capabilities = static_resolution.capabilities;

    let text_result = provider
        .complete(
            "You are a compatibility probe. Reply with exactly OK.",
            &[Message::user("Reply OK")],
            &[],
            &wyj_api::provider::RequestOptions::text_only(32),
        )
        .await?;
    if text_result
        .content
        .iter()
        .all(|block| !matches!(block, ContentBlock::Text { text } if !text.trim().is_empty()))
    {
        anyhow::bail!("basic text probe returned no text");
    }
    capabilities.stream_usage = Capability::new(
        text_result.input_tokens > 0 || text_result.output_tokens > 0,
        CapabilitySource::LiveProbe,
        Confidence::Verified,
    );

    let echo = ToolDefinition {
        name: "probe_echo".to_string(),
        description: "Side-effect-free compatibility probe".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["value"],
            "properties": {"value": {"type": "string", "enum": ["ok"]}},
            "additionalProperties": false
        }),
        native: None,
    };
    let tool_result = provider
        .complete(
            "Call probe_echo exactly once with value set to ok. Do not answer with text.",
            &[Message::user("Run the echo compatibility probe")],
            std::slice::from_ref(&echo),
            &wyj_api::provider::RequestOptions::text_only(128),
        )
        .await?;
    let valid_echo = tool_result.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolUse { name, input, .. }
                if name == "probe_echo" && input.get("value").and_then(|v| v.as_str()) == Some("ok")
        )
    });
    if !valid_echo {
        anyhow::bail!("tool probe did not produce the required schema-compliant echo call");
    }
    capabilities.tool_calling =
        Capability::new(true, CapabilitySource::LiveProbe, Confidence::Verified);
    capabilities.strict_tool_schema =
        Capability::new(true, CapabilitySource::LiveProbe, Confidence::Verified);

    if level == "full" {
        let parallel = provider
            .complete(
                "Call probe_echo twice in one response, each with value ok. Do not answer with text.",
                &[Message::user("Run the parallel tool compatibility probe")],
                std::slice::from_ref(&echo),
                &wyj_api::provider::RequestOptions::text_only(192),
            )
            .await?;
        let valid_count = parallel
            .content
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    ContentBlock::ToolUse { name, input, .. }
                        if name == "probe_echo" && input.get("value").and_then(|v| v.as_str()) == Some("ok")
                )
            })
            .count();
        capabilities.parallel_tool_calls = Capability::new(
            valid_count >= 2,
            CapabilitySource::LiveProbe,
            Confidence::Verified,
        );

        if let Some(budget) = profile.thinking_budget.filter(|budget| *budget > 0) {
            provider
                .complete(
                    "Reply with exactly OK.",
                    &[Message::user(
                        "Run the configured reasoning parameter probe",
                    )],
                    &[],
                    &wyj_api::provider::RequestOptions {
                        max_tokens: budget.saturating_add(32),
                        thinking_budget: Some(budget),
                        interleaved: profile.interleaved_thinking,
                    },
                )
                .await?;
            capabilities.thinking = Capability::new(
                wyj_api::ThinkingMode::BudgetTokens,
                CapabilitySource::LiveProbe,
                Confidence::Verified,
            );
        }
    }
    Ok(capabilities)
}

fn print_model_doctor_report(report: &wyj_api::ModelDoctorReport) {
    println!("profile: {}", report.profile);
    println!("vendor: {}", report.identity.vendor);
    println!("model: {}", report.identity.model);
    println!("wire protocol: {}", report.identity.wire_protocol);
    println!("base url: {}", report.identity.base_url);
    println!("endpoint: {}", report.endpoint_type);
    println!("verification: {:?}", report.verification_status);
    println!("probe: {}", report.probe_status);
    if let Some(probed_at) = &report.probed_at {
        println!("probed at: {probed_at}");
    }
    println!(
        "context/output: {}/{}",
        report.capabilities.context_window, report.capabilities.max_output_tokens
    );
    println!(
        "vision={} thinking={:?} cache={:?} stream_usage={}",
        report.capabilities.vision.value,
        report.capabilities.thinking.value,
        report.capabilities.prompt_cache.value,
        report.capabilities.stream_usage.value
    );
    println!(
        "tools={} parallel={} strict_schema={} max_tools_per_turn={}",
        report.capabilities.tool_calling.value,
        report.capabilities.parallel_tool_calls.value,
        report.capabilities.strict_tool_schema.value,
        report.capabilities.max_tools_per_turn
    );
    for degradation in &report.known_degradations {
        println!("degradation: {degradation}");
    }
}

fn sandbox_report(config: &wyj_config::SandboxCfg) -> serde_json::Value {
    let status = wyj_sandbox::SandboxRunner::detect().status();
    serde_json::json!({
        "mode": if config.enabled { "enforce" } else { "disabled" },
        "backend": status.backend,
        "available": status.available,
        "filesystem_isolation": status.filesystem_isolation,
        "domain_network_isolation": status.domain_network_isolation,
        "network": if config.network.allowed_domains.is_empty() {
            serde_json::json!({"policy": "deny"})
        } else {
            serde_json::json!({"policy": "allowed_domains", "domains": config.network.allowed_domains})
        },
        "filesystem": {
            "allow_read": config.filesystem.allow_read,
            "allow_write": config.filesystem.allow_write,
            "deny_read": config.filesystem.deny_read,
            "deny_write": config.filesystem.deny_write,
        },
        "unsandboxed_fallback": {
            "tui_once": config.enabled && config.allow_unsandboxed_commands,
            "headless": false,
            "schedule": false,
            "sub_agent": false,
        },
        "fail_if_unavailable": config.fail_if_unavailable,
        "dependencies": status.dependencies,
        "detail": status.detail,
    })
}

fn print_sandbox_report(config: &wyj_config::SandboxCfg, json: bool) -> Result<()> {
    let report = sandbox_report(config);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("Sandbox");
    println!("  mode: {}", report["mode"].as_str().unwrap_or("unknown"));
    println!(
        "  backend: {} (available={})",
        report["backend"].as_str().unwrap_or("unknown"),
        report["available"].as_bool().unwrap_or(false)
    );
    println!(
        "  filesystem isolation: {}",
        report["filesystem_isolation"].as_bool().unwrap_or(false)
    );
    println!(
        "  domain network isolation: {}",
        report["domain_network_isolation"]
            .as_bool()
            .unwrap_or(false)
    );
    println!("  network: {}", report["network"]);
    println!("  overrides: {}", report["filesystem"]);
    println!(
        "  unsandboxed fallback: TUI one-shot={} · headless/schedule/sub-agent=false",
        report["unsandboxed_fallback"]["tui_once"]
            .as_bool()
            .unwrap_or(false)
    );
    println!(
        "  fail if unavailable: {}",
        report["fail_if_unavailable"].as_bool().unwrap_or(false)
    );
    if let Some(dependencies) = report["dependencies"].as_array() {
        for dependency in dependencies {
            println!("  dependency: {}", dependency.as_str().unwrap_or("unknown"));
        }
    }
    println!("  detail: {}", report["detail"].as_str().unwrap_or(""));
    Ok(())
}

fn model_for_routing_role(profile: &wyj_config::Profile, role: RoutingRole) -> String {
    match role {
        RoutingRole::Plan => profile
            .plan_model
            .as_deref()
            .unwrap_or(&profile.model)
            .to_string(),
        RoutingRole::Execute => profile
            .exec_model
            .as_deref()
            .unwrap_or(&profile.model)
            .to_string(),
        RoutingRole::Explore | RoutingRole::Review => profile.model.clone(),
    }
}

fn configured_route_names(cfg: &Config, role: RoutingRole) -> Vec<String> {
    let mut names = Vec::new();
    for name in cfg.routing.roles.for_role(role) {
        if cfg.profile_by_name(name).is_none() {
            tracing::warn!("routing profile `{name}` does not exist; skipped");
            continue;
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

fn build_fallback_routes(
    cfg: &Config,
    role: RoutingRole,
    primary_profile: &str,
) -> Vec<wyj_core::AgentRoute> {
    let primary_resolution = wyj_api::ModelCatalog::resolve(
        cfg.profile_by_name(primary_profile)
            .unwrap_or_else(|| cfg.active_profile()),
        None,
    );
    configured_route_names(cfg, role)
        .into_iter()
        .filter(|name| name != primary_profile)
        .filter_map(|name| {
            let profile = cfg.profile_by_name(&name)?;
            let model = model_for_routing_role(profile, role);
            let resolution = wyj_api::ModelCatalog::resolve(profile, Some(&model));
            if !cfg.routing.cross_provider_fallback
                && resolution.identity.vendor != primary_resolution.identity.vendor
            {
                tracing::warn!(
                    "routing fallback `{name}` skipped: vendor {} differs from primary {}",
                    resolution.identity.vendor,
                    primary_resolution.identity.vendor
                );
                return None;
            }
            let provider = match wyj_api::build_provider_from_profile(profile, Some(&model)) {
                Ok(provider) => provider,
                Err(error) => {
                    tracing::warn!("routing fallback `{name}` unavailable: {error}");
                    return None;
                }
            };
            Some(
                wyj_core::AgentRoute::new(name, resolution.identity.vendor, model, provider)
                    .with_capabilities(resolution.capabilities)
                    .with_limits(profile.max_tokens, profile.context_window)
                    .with_thinking(profile.thinking_budget, profile.interleaved_thinking),
            )
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    // 先加载 config 拿 language 字段并 set_locale，确保 Cli::parse() 生成的
    // --help 文本、以及后续所有输出都使用正确的语言。
    let mut cfg = Config::load()?;
    let lang = cfg
        .language
        .clone()
        .unwrap_or_else(|| wyj_i18n::detect_system_locale().to_string());
    wyj_i18n::set_locale(&lang);

    let mut cli = Cli::parse();
    let profile_was_explicit = cli.profile.is_some();

    if let Some(cmd) = cli.command.take() {
        match cmd {
            Commands::Update { yes } => return update_cmd::run(yes).await,
            Commands::SubagentTrace {
                session_id,
                sub_id,
                json,
            } => return run_subagent_trace_cmd(&session_id, sub_id, json),
            Commands::Extensions { command } => {
                let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
                return extensions_cmd::run(command, &cwd).await;
            }
            Commands::Schedule { command } => {
                let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
                return schedule_cmd::run(command, &cwd).await;
            }
            Commands::TrustMcp => {
                let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
                return trust_cmd::run(&cwd).await;
            }
            Commands::Model { command } => return run_model_command(command, &cfg).await,
            Commands::Session { command } => return run_session_command(command),
            Commands::Sandbox { json } => return print_sandbox_report(&cfg.sandbox, json),
        }
    }

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    // TUI 交互模式下绝不能让 tracing 写向 stdout/stderr：crossterm 的 alternate
    // screen + ratatui 的增量渲染都假定独占了终端输出，任何外部直接写入的文本
    // （哪怕只是一行 WARN 日志，如 MCP 连接超时）都会把当前帧"打穿"，且因为不在
    // ratatui 的绘制 diff 范围内，后续帧不会自动清除这些残留字符，导致画面持续
    // 错乱、键盘输入看起来毫无反应（实际是渲染状态错位，不是真的卡死）。
    // headless/-p/--config-status 没有 alternate screen，继续写 stdout 没有这个问题。
    let is_tui_mode = !cli.headless && cli.prompt.is_none() && !cli.config_status;
    let log_writer = if is_tui_mode {
        match open_tui_log_file() {
            Ok(file) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(move || {
                file.try_clone().expect("clone wyj-code.log file handle")
            }),
            Err(_) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(io::sink),
        }
    } else {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(io::stdout)
    };
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_ansi(!is_tui_mode)
        .with_env_filter(filter)
        .init();

    if let Some(name) = cli.profile.take() {
        if !cfg.profiles.iter().any(|p| p.name == name) {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("cli.profile_not_found", &[("name", &name)])
            );
            std::process::exit(1);
        }
        // 仅覆盖本次运行使用的分组，不落盘、不改 active_profile 持久值
        cfg.active_profile = name;
    }

    if cli.config_status {
        let status_cwd = cli
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let active = cfg.active_profile().clone();
        println!(
            "{}",
            wyj_i18n::tr_fmt("status.active_profile", &[("name", &active.name)])
        );
        println!(
            "{}",
            wyj_i18n::tr_fmt(
                "status.provider",
                &[("provider", &active.provider.to_string())]
            )
        );
        println!(
            "{}",
            wyj_i18n::tr_fmt("status.model", &[("model", &active.model)])
        );
        if let Some(m) = &active.plan_model {
            println!("{}", wyj_i18n::tr_fmt("status.plan_model", &[("model", m)]));
        }
        if let Some(m) = &active.exec_model {
            println!("{}", wyj_i18n::tr_fmt("status.exec_model", &[("model", m)]));
        }
        println!(
            "{}",
            wyj_i18n::tr_fmt("status.endpoint", &[("url", cfg.resolved_base_url())])
        );
        match cfg.api_key() {
            Ok(_) => println!(
                "{}",
                wyj_i18n::tr_fmt(
                    "status.api_key_configured",
                    &[("prefix", &cfg.redacted_api_key().unwrap_or_default())]
                )
            ),
            Err(e) => println!(
                "{}",
                wyj_i18n::tr_fmt("status.api_key_error", &[("err", &e.to_string())])
            ),
        }
        let others: Vec<&str> = cfg
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .filter(|n| *n != active.name)
            .collect();
        if !others.is_empty() {
            println!(
                "{}",
                wyj_i18n::tr_fmt("status.other_profiles", &[("names", &others.join(", "))])
            );
        }
        let effective_count =
            wyj_store::mcp_install::effective_mcp_servers(&cfg, &status_cwd).len();
        println!(
            "{}",
            wyj_i18n::tr_fmt(
                "status.mcp_servers",
                &[("count", &effective_count.to_string())]
            )
        );
        return Ok(());
    }

    let cwd = cli.cwd.unwrap_or_else(|| std::env::current_dir().unwrap());
    let config_base = wyj_config::config_dir()?;

    let history_store = HistoryStore::new(config_base.join("history")).ok();
    let session_store = SessionStore::new(config_base.join("sessions")).ok();

    // 根据 --continue/-c 或 --resume 恢复历史会话
    let (session_id, initial_messages) = match (&cli.resume, cli.continue_session) {
        (Some(id), _) => {
            let msgs = session_store
                .as_ref()
                .and_then(|s| s.load(id).ok())
                .map(|f| f.messages)
                .unwrap_or_default();
            if msgs.is_empty() {
                eprintln!(
                    "{}",
                    wyj_i18n::tr_fmt("main.session_not_found", &[("id", id)])
                );
            } else {
                eprintln!(
                    "{}",
                    wyj_i18n::tr_fmt(
                        "main.session_resumed",
                        &[("id", id), ("count", &msgs.len().to_string())]
                    )
                );
            }
            (id.clone(), msgs)
        }
        (None, true) => {
            // 按项目隔离：-c 恢复「当前项目」最近会话，而非全局最新
            let last = session_store
                .as_ref()
                .and_then(|s| s.last_for_project(&cwd).ok().flatten());
            match last {
                Some(meta) => {
                    let msgs = session_store
                        .as_ref()
                        .and_then(|s| s.load(&meta.session_id).ok())
                        .map(|f| f.messages)
                        .unwrap_or_default();
                    if msgs.is_empty() {
                        (new_session_id(), vec![])
                    } else {
                        eprintln!(
                            "{}",
                            wyj_i18n::tr_fmt(
                                "main.session_resumed_last",
                                &[("id", &meta.session_id), ("count", &msgs.len().to_string())]
                            )
                        );
                        (meta.session_id, msgs)
                    }
                }
                None => {
                    eprintln!("{}", wyj_i18n::tr("main.no_session_history"));
                    (new_session_id(), vec![])
                }
            }
        }
        _ => (new_session_id(), vec![]),
    };

    let session_store_arc = session_store.map(std::sync::Arc::new);
    let checkpoint_store = session_store_arc
        .as_ref()
        .and_then(|store| wyj_core::CheckpointStore::new(store.dir(), session_id.clone()).ok())
        .map(Arc::new);

    let memory_store = MemoryStore::new(&config_base, &cwd)
        .map(|m| {
            m.set_enabled(cfg.auto_memory_enabled);
            Arc::new(m)
        })
        .map_err(|e| tracing::warn!("记忆存储初始化失败: {e}"))
        .ok();

    // CLAUDE.md 系记忆文件加载器：全局 + 祖先链，主 Agent 与 sub-agent 共用同一份
    // （共享子目录动态加载去重状态）。
    let claude_md_loader = Arc::new(wyj_core::ClaudeMdLoader::new(&cwd));

    // 供 TUI 语言/模型切换重建 Agent 时复用（避免重建后丢失记忆能力）
    let memory_store_for_rebuild = memory_store.clone();
    let claude_md_for_rebuild = claude_md_loader.clone();

    // 确定当前运行模式
    let mode = if cli.plan {
        AgentMode::Plan
    } else if cli.bypass_permissions {
        AgentMode::Bypass
    } else {
        AgentMode::Normal
    };

    let routing_role = if matches!(mode, AgentMode::Plan) {
        RoutingRole::Plan
    } else {
        RoutingRole::Execute
    };
    if !profile_was_explicit {
        if let Some(primary) = configured_route_names(&cfg, routing_role)
            .into_iter()
            .next()
        {
            cfg.active_profile = primary;
        }
    }

    // 按模式选择模型
    let model_name = model_for_routing_role(cfg.active_profile(), routing_role);

    let provider = wyj_api::build_provider_with_model(&cfg, &model_name)?;

    // 恢复的长会话预压缩：--resume/--continue 全量回放的历史若已占掉大半上下文，
    // 恢复后首轮就会全价发送巨量旧消息（且很快再触发一次运行中压缩）。
    // 这里在会话开始前主动压缩一次，恢复即瘦身。
    let mut initial_messages = initial_messages;
    {
        let window = cfg.active_profile().context_window;
        if !initial_messages.is_empty() && wyj_core::estimate_tokens(&initial_messages) > window / 2
        {
            let mut tmp = Session::new();
            tmp.messages = std::mem::take(&mut initial_messages);
            match wyj_core::compact_session(&mut tmp, provider.as_ref(), window).await {
                Ok(r) => eprintln!(
                    "{}",
                    wyj_i18n::tr_fmt(
                        "main.resume_compacted",
                        &[("count", &r.messages_removed.to_string())]
                    )
                ),
                Err(e) => tracing::warn!("恢复会话预压缩失败: {e}"),
            }
            initial_messages = tmp.messages;
        }
    }

    // 始终注册全部工具（模式过滤在运行时由 ToolCtx.permission_mode 负责，支持运行时切换）
    let mut registry = ToolRegistry::standard();

    // 初始工具上下文权限（headless/single-shot 模式用；TUI 模式在 spawn 闭包内动态创建）
    let tool_ctx = ToolCtx::new(&cwd);
    tool_ctx
        .apply_sandbox_config(&cfg.sandbox)
        .map_err(|error| anyhow::anyhow!("sandbox config: {error}"))?;
    tool_ctx.set_execution_surface(if cli.prompt.is_some() {
        ExecutionSurface::SinglePrompt
    } else if cli.headless {
        ExecutionSurface::HeadlessRepl
    } else {
        ExecutionSurface::TuiInteractive
    });
    tool_ctx.require_sandbox(cli.require_sandbox || cfg.sandbox.fail_if_unavailable);
    for path in &cli.allow_write {
        let resolved = tool_ctx
            .allow_write_root(path)
            .map_err(|error| anyhow::anyhow!("--allow-write {}: {error}", path.display()))?;
        eprintln!("allow-write: {}", resolved.display());
    }
    for path in &cli.allow_plan_write {
        let resolved = tool_ctx
            .allow_plan_document(path)
            .map_err(|error| anyhow::anyhow!("--allow-plan-write {}: {error}", path.display()))?;
        eprintln!("allow-plan-write: {}", resolved.display());
    }
    for domain in &cli.allow_network {
        tool_ctx.allow_network_domain(domain.clone());
    }

    // Hooks 生命周期自动化：按 `~/.claude/settings.json` + 项目 `.claude/settings.json`
    // + `.claude/settings.local.json` 三源合并加载。`--no-hooks` 时构造空 runner。
    let hook_runner = Arc::new(HookRunner::load(&cwd, !cli.no_hooks));
    if hook_runner.is_enabled() && hook_runner.has_any() {
        eprintln!(
            "{}",
            wyj_i18n::tr_fmt(
                "main.hooks_loaded_notice",
                &[("count", &hook_runner.total_hook_count().to_string())]
            )
        );
    }
    let mut initial_permission = match &mode {
        AgentMode::Plan => {
            let set: std::collections::HashSet<String> = [
                "Read",
                "Glob",
                "Grep",
                "WebFetch",
                "WebSearch",
                "AskQuestion",
                "Write",
                "Edit",
                "Bash",
                "BashOutput",
                "ExitPlanMode",
                "TodoWrite",
                "Agent",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            PermissionMode::Plan(set)
        }
        AgentMode::Bypass => PermissionMode::AutoApprove,
        AgentMode::Normal => PermissionMode::Prompt,
    };
    if !cli.allowed_tools.is_empty() {
        let explicit: std::collections::HashSet<String> =
            cli.allowed_tools.iter().cloned().collect();
        initial_permission = match initial_permission {
            PermissionMode::Plan(base) => {
                PermissionMode::Plan(base.intersection(&explicit).cloned().collect())
            }
            _ => PermissionMode::Allowlist(explicit),
        };
    }
    tool_ctx.set_permission_mode(initial_permission);

    let todo_store = Arc::new(Mutex::new(TodoStore::default()));
    registry.register_arc(Arc::new(TodoWriteTool::new(todo_store.clone())));
    registry.register_arc(Arc::new(AskQuestionTool::new()));

    // WebSearch：仅当配置了搜索 API Key 时注册（否则模型看不到该工具，避免误调）
    if let Some(key) = cfg.search_api_key.as_deref().filter(|k| !k.is_empty()) {
        registry.register_arc(Arc::new(wyj_tools::WebSearchTool::new(key)));
    }
    // computer-use：仅 macOS/Windows + vision profile + Anthropic provider；
    // 返回值决定下面是否追加 COMPUTER_USE_HINT（教模型用 Bash 启动应用）
    let computer_use_enabled = register_computer_tool_if_enabled(&mut registry, &cfg);
    // WindowCapture：独立工具，注册门槛与 computer-use 完全一致（同样要把
    // 截图作为 image block 塞进 tool_result），见该函数文档。
    register_window_capture_tool_if_enabled(&mut registry, &cfg);
    register_app_computer_tool_if_enabled(&mut registry, &cfg);

    // --plugin-dir：临时加载本地开发插件（不落盘、不经过 marketplace/lockfile，
    // 仅当次进程生效），与 TUI「添加本地插件」的持久化路径是两条独立路径。
    let local_plugin: Option<wyj_store::lockfile::PluginContributions> = match &cli.plugin_dir {
        Some(path) => {
            let manifest = wyj_store::plugin_install::load_local_plugin(path)?;
            Some(wyj_store::plugin_install::resolve_contributions(
                &manifest, path,
            ))
        }
        None => None,
    };

    // agent 类型定义：内置三类型 + ~/.claude/agents 与项目 .claude/agents 的自定义定义
    // + 已启用插件贡献的 agent 定义 + --plugin-dir 临时加载的 agent 定义
    let mut plugin_agent_paths = wyj_store::plugin_install::enabled_plugin_agent_paths(&cwd);
    if let Some(local) = &local_plugin {
        plugin_agent_paths.extend(local.agent_paths.clone());
    }
    let agent_defs = Arc::new(wyj_core::load_agent_defs(&cwd, &plugin_agent_paths));
    let shared_agent_defs: wyj_tools::SharedAgentDefinitions =
        Arc::new(std::sync::RwLock::new((*agent_defs).clone()));
    // 子 Agent 执行轨迹落盘（`SubAgentCfg::trace_enabled`，默认开启）：与
    // TUI/headless 共用同一个 Hub，`emit()` 集中接入，两端自动获得持久化能力。
    let sub_agent_hub = Arc::new(if cfg.subagent.trace_enabled {
        wyj_tools::SubAgentHub::new().with_trace(
            config_base.join("sessions"),
            session_id.clone(),
            cfg.subagent.trace_max_bytes_per_agent,
        )
    } else {
        wyj_tools::SubAgentHub::new()
    });
    // 当前已连接 MCP 工具的共享快照：`-p`/`--headless`/TUI 三种模式各自的
    // MCP 连接时机不同，但都在工具注册成功时 push 进这个句柄，供子 Agent
    // 工厂与 `/model` 重建共同读取（子 Agent 只能看到 spawn 时刻已连好的）。
    let mcp_tools: wyj_tools::SharedMcpTools = Arc::new(std::sync::RwLock::new(Vec::new()));
    let sub_agent_factory =
        make_sub_agent_factory(cfg.clone(), claude_md_loader.clone(), mcp_tools.clone());
    registry.register_arc(Arc::new(SubAgentTool::new_shared(
        shared_agent_defs.clone(),
        sub_agent_hub.clone(),
        {
            let f = sub_agent_factory.clone();
            move |def| f(def)
        },
    )));

    // -p（单次问答）模式：仍在启动时连接 MCP server，但只等一个远小于
    // `MCP_CONNECT_TIMEOUT` 的宽限期（见 `MCP_STARTUP_GRACE`）——`-p` 是真正的
    // 单轮、进程跑完即退出，没有"界面已经打开、稍后再补"的空间，但也不能照抄
    // 最坏 N×15s 的全量等待。宽限期内连完的正常注册，没连完的这次不等了（本次
    // 调用看不到这些工具，仅打印提示），避免个别慢 server 拖慢整个启动。
    //
    // `--headless`（多轮 REPL）不在这里连接：其 Agent 在整个进程生命周期内
    // 有多轮机会，交给 `repl()` 内部完全对称于 TUI 的"后台连接 + 每轮非阻塞
    // 排空结果"方案（见 repl() 内 shared_agent 部分），不阻塞任何一轮。
    // TUI 交互模式同样不在这里连接，交给 tui_main 在界面已经可用之后于后台连接。
    if cli.prompt.is_some() {
        // 未信任的项目级 MCP server（.wyj-code/mcp.toml/.mcp.json）在这里一律
        // 跳过、不连接：`-p` 常被脚本/cron 无 TTY 调用，没有交互通道可以弹窗
        // 确认，静默放行等于让克隆到的陌生仓库能无感执行任意命令。用户需要
        // 先在 TUI 里批准一次，或运行 `wyj-code trust-mcp` 批准。
        let (mut effective_mcp_servers, untrusted_servers) =
            wyj_store::mcp_install::effective_mcp_servers_trust_split(&cfg, &cwd);
        if !untrusted_servers.is_empty() {
            let mut names: Vec<_> = untrusted_servers.iter().map(|s| s.name.clone()).collect();
            names.sort();
            eprintln!(
                "[以下项目级 MCP server 尚未信任批准，本次未连接: {}；运行 `wyj-code trust-mcp` 批准]",
                names.join(", ")
            );
        }
        if let Some(local) = &local_plugin {
            effective_mcp_servers.extend(local.mcp_servers.clone());
        }
        let mut pending_names: std::collections::HashSet<String> = effective_mcp_servers
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let mut mcp_connect_tasks = tokio::task::JoinSet::new();
        for mcp_cfg in effective_mcp_servers {
            mcp_connect_tasks.spawn(async move {
                let result = tokio::time::timeout(
                    wyj_mcp::bridge::MCP_CONNECT_TIMEOUT,
                    wyj_mcp::bridge::connect_mcp_server(&mcp_cfg),
                )
                .await;
                (mcp_cfg.name, result)
            });
        }
        let grace = tokio::time::sleep(wyj_mcp::bridge::MCP_STARTUP_GRACE);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                joined = mcp_connect_tasks.join_next() => {
                    match joined {
                        None => break,
                        Some(Ok((name, Ok(Ok(tools))))) => {
                            pending_names.remove(&name);
                            let count = tools.len();
                            for tool in tools {
                                let t: Arc<dyn wyj_tools::Tool> = Arc::new(tool);
                                mcp_tools.write().unwrap().push(t.clone());
                                registry.register_arc(t);
                            }
                            tracing::info!("MCP [{name}] 连接成功，注册 {count} 个工具");
                        }
                        Some(Ok((name, Ok(Err(e))))) => {
                            pending_names.remove(&name);
                            tracing::warn!("MCP [{name}] 连接失败: {e}");
                        }
                        Some(Ok((name, Err(_)))) => {
                            pending_names.remove(&name);
                            tracing::warn!(
                                "MCP [{name}] 连接超时（>{}s），已跳过",
                                wyj_mcp::bridge::MCP_CONNECT_TIMEOUT.as_secs()
                            );
                        }
                        Some(Err(e)) => tracing::warn!("MCP 连接任务异常退出: {e}"),
                    }
                }
                _ = &mut grace => break,
            }
        }
        if !pending_names.is_empty() {
            let mut names: Vec<_> = pending_names.into_iter().collect();
            names.sort();
            eprintln!(
                "[以下 MCP server 启动耗时较长，本次未加载: {}]",
                names.join(", ")
            );
        }
        // mcp_connect_tasks 在此处 drop：尚未完成的任务被中止（JoinSet 的
        // Drop 行为），不影响已经跑完并注册进 registry 的连接结果。
    }

    // 主 system prompt：英文静态提示 + <env> 环境块（会话内稳定字段，进缓存）。
    // git 状态快照单独走首轮 user 消息注入（会变的字段进 system 会击穿缓存）。
    let env_info = wyj_core::prompts::EnvInfo::collect(&cwd, &model_name);
    let model_resolution = wyj_api::ModelCatalog::resolve(cfg.active_profile(), Some(&model_name));
    let model_capabilities = model_resolution.capabilities.clone();
    let fallback_routes = build_fallback_routes(&cfg, routing_role, &cfg.active_profile().name);
    let enable_lazy_tool_schemas = model_capabilities.tool_calling.value;
    let mut agent = Agent::new(provider)
        .with_system(wyj_core::prompts::main_system_prompt(&env_info))
        .with_git_snapshot(wyj_core::prompts::git_status_snapshot(&cwd))
        .with_max_tokens(cfg.active_profile().max_tokens)
        .with_context_window(cfg.active_profile().context_window)
        .with_model_capabilities(model_capabilities)
        .with_route_identity(
            cfg.active_profile().name.clone(),
            model_resolution.identity.vendor,
            model_name.clone(),
        )
        .with_fallback_routes(fallback_routes, cfg.routing.cross_provider_fallback)
        .with_thinking(
            cfg.active_profile().thinking_budget,
            cfg.active_profile().interleaved_thinking,
        );

    // system_prompt_extra 记录 append_system() 追加的内容（原样，含前导 "\n\n"），
    // 供 TUI 侧重建 Agent 时在默认提示词后原样拼回这些追加内容（目前含 Plan 模式
    // 限制说明与 computer-use 使用提示；CLAUDE.md 系文件不焊死进 system prompt，
    // 见 with_claude_md）。
    let mut system_prompt_extra = String::new();

    // Plan 模式在系统提示中说明只读约束
    if matches!(mode, AgentMode::Plan) {
        let extra = wyj_core::prompts::PLAN_MODE;
        agent = agent.append_system(extra);
        system_prompt_extra.push_str("\n\n");
        system_prompt_extra.push_str(extra);
    }

    // computer-use 已注册：教模型优先用 Bash 启动应用（而非在 GUI 里瞎找），
    // 且每个变更动作已有独立确认弹窗、无需先在聊天里问用户"允许"
    if computer_use_enabled {
        let extra = wyj_core::prompts::COMPUTER_USE_HINT;
        agent = agent.append_system(extra);
        system_prompt_extra.push_str("\n\n");
        system_prompt_extra.push_str(extra);
    }

    // headless/单次问答模式没有 UI 可交互，AskQuestion 会被自动取消：
    // 告知模型不要调用该工具，直接给出假设并继续（不写入 system_prompt_extra，
    // 因为该变量只服务于 TUI 运行时重建 Agent，headless/-p 路径不会触发重建）。
    if cli.headless || cli.prompt.is_some() {
        agent = agent.append_system(wyj_core::prompts::NON_INTERACTIVE);
    }

    agent = agent
        .with_claude_md(claude_md_loader.clone())
        .with_hooks(hook_runner.clone());

    if let Some(store) = &checkpoint_store {
        agent = agent.with_checkpoint_store(store.clone());
    }

    if let Some(mem) = memory_store {
        agent = agent.with_memory(mem);
    }

    for def in registry.definitions() {
        let name = def.name.clone();
        if let Some(t) = registry.get(&name) {
            agent.register_tool(t);
        }
    }
    if enable_lazy_tool_schemas {
        agent.enable_lazy_tools(
            ["Read", "Glob", "Grep", "AskQuestion", "TodoWrite"]
                .into_iter()
                .map(str::to_string),
            cfg.model_runtime.lazy_tools_threshold,
            cfg.model_runtime.lazy_tools_top_k,
            cfg.model_runtime.lazy_tools_sticky_turns,
        );
    }

    // headless/single-shot 模式：子 Agent 进度以纯文本行打印到 stderr
    if cli.headless || cli.prompt.is_some() {
        sub_agent_hub.set_event_cb(|ev| {
            use wyj_tools::SubAgentEvent as E;
            match ev {
                E::Started {
                    id,
                    agent_type,
                    description,
                    background,
                    ..
                } => {
                    let bg = if background { " ◇bg" } else { "" };
                    eprintln!(
                        "\x1b[38;2;215;119;87m⏺ [a{id}]{bg} {agent_type}({description})\x1b[0m"
                    );
                }
                E::ToolStart {
                    id,
                    tool_name,
                    arg_summary,
                    ..
                } => {
                    eprintln!(
                        "\x1b[38;2;150;150;150m  [a{id}] ⏺ {tool_name}({arg_summary})\x1b[0m"
                    );
                }
                E::ToolEnd { .. } | E::Usage { .. } => {}
                E::Control {
                    id,
                    action,
                    accepted,
                } => {
                    eprintln!("  [a{id}] control {action} accepted={accepted}");
                }
                E::Done {
                    id,
                    agent_type,
                    result,
                    is_error,
                    elapsed_secs,
                    background,
                    ..
                } => {
                    if is_error {
                        eprintln!(
                            "\x1b[38;2;255;107;128m✗ [a{id}] {agent_type} · {elapsed_secs:.1}s\x1b[0m"
                        );
                    } else {
                        eprintln!(
                            "\x1b[38;2;78;186;101m✓ [a{id}] {agent_type} · {elapsed_secs:.1}s\x1b[0m"
                        );
                    }
                    // 后台任务的结果无法回填进已结束的对话轮次，直接打印
                    if background {
                        eprintln!("{result}");
                    }
                }
            }
        });
    }

    // headless/single-shot 模式：注册格式化工具事件输出到 stderr
    if cli.headless || cli.prompt.is_some() {
        let mode_info = match mode {
            AgentMode::Plan => wyj_i18n::tr("main.mode_info_plan"),
            AgentMode::Bypass => wyj_i18n::tr("main.mode_info_bypass"),
            AgentMode::Normal => String::new(),
        };
        if !mode_info.is_empty() {
            eprintln!("\x1b[38;2;150;150;150m{mode_info}\x1b[0m");
        }
    }

    let agent = if cli.headless || cli.prompt.is_some() {
        agent
            // thinking 以暗灰打到 stderr，不进 stdout（保持管道输出纯净）
            .with_thinking_callback(|d| {
                eprint!("\x1b[2;3m{d}\x1b[0m");
            })
            .with_tool_callback(|event| match event {
                ToolEvent::Start {
                    id: _,
                    name,
                    input: _,
                } => {
                    eprintln!("\x1b[38;2;215;119;87m⏺ {name}\x1b[0m");
                }
                ToolEvent::End {
                    id: _,
                    name,
                    is_error,
                    elapsed_secs,
                    output: _,
                } => {
                    if is_error {
                        // 红色 ✗
                        eprintln!("\x1b[38;2;255;107;128m✗ {name} · {elapsed_secs:.1}s\x1b[0m");
                    } else {
                        // 绿色 ✓
                        eprintln!("\x1b[38;2;78;186;101m✓ {name} · {elapsed_secs:.1}s\x1b[0m");
                    }
                }
            })
    } else {
        agent
    };

    // 配置会话标题生成器：持有 SessionStore 引用，首轮后后台生成标题写盘
    let agent = if let Some(store) = &session_store_arc {
        let provider = wyj_api::build_provider_from_profile(cfg.active_profile(), None)
            .unwrap_or_else(|e| {
                tracing::warn!("标题生成器 provider 构建失败: {e}，回退到主 provider");
                wyj_api::build_provider(&cfg).expect("主 provider 已在启动时构建成功")
            });
        let gen = Arc::new(SummaryGenerator::new(store.clone(), provider));
        agent
            .with_summary(gen)
            .with_session_id(session_id.clone())
            // headless 模式：标题生成后直接打印 OSC 0 设置终端窗口标题
            .with_title_callback(|title| {
                print!("\x1b]0;{}\x07", title);
                let _ = io::stdout().flush();
            })
    } else {
        agent
    };

    let context_window = cfg.active_profile().context_window;

    // Shared rebuild path for TUI profile switching and headless Skill
    // `model:` frontmatter.  It deliberately reuses the current MCP snapshot,
    // so a scoped Skill Agent sees exactly the same tools as the next normal
    // turn without changing the session's active profile.
    let todo_store_for_rebuild = todo_store.clone();
    let agent_defs_for_rebuild = shared_agent_defs.clone();
    let hub_for_rebuild = sub_agent_hub.clone();
    let store_for_rebuild = session_store_arc.clone();
    let sid_for_rebuild = session_id.clone();
    let cwd_for_rebuild = cwd.clone();
    let hook_runner_for_rebuild = hook_runner.clone();
    let checkpoint_store_for_rebuild = checkpoint_store.clone();
    let mcp_tools_for_rebuild = mcp_tools.clone();
    let rebuild_fn: wyj_tui::RebuildFn = Arc::new(move |cfg: &Config, new_model: &str| {
        let provider = wyj_api::build_provider_with_model(cfg, new_model)?;
        let routing_role = if cfg.active_profile().plan_model.as_deref() == Some(new_model) {
            RoutingRole::Plan
        } else {
            RoutingRole::Execute
        };
        let model_resolution =
            wyj_api::ModelCatalog::resolve(cfg.active_profile(), Some(new_model));
        let model_capabilities = model_resolution.capabilities.clone();
        let fallback_routes = build_fallback_routes(cfg, routing_role, &cfg.active_profile().name);
        let enable_lazy_tool_schemas = model_capabilities.tool_calling.value;
        let env_info = wyj_core::prompts::EnvInfo::collect(&cwd_for_rebuild, new_model);
        let mut new_agent = Agent::new(provider)
            .with_system(wyj_core::prompts::main_system_prompt(&env_info))
            .with_git_snapshot(wyj_core::prompts::git_status_snapshot(&cwd_for_rebuild))
            .with_max_tokens(cfg.active_profile().max_tokens)
            .with_context_window(cfg.active_profile().context_window)
            .with_model_capabilities(model_capabilities)
            .with_route_identity(
                cfg.active_profile().name.clone(),
                model_resolution.identity.vendor,
                new_model.to_string(),
            )
            .with_fallback_routes(fallback_routes, cfg.routing.cross_provider_fallback)
            .with_thinking(
                cfg.active_profile().thinking_budget,
                cfg.active_profile().interleaved_thinking,
            )
            .with_claude_md(claude_md_for_rebuild.clone())
            .with_hooks(hook_runner_for_rebuild.clone());
        if let Some(store) = &checkpoint_store_for_rebuild {
            new_agent = new_agent.with_checkpoint_store(store.clone());
        }
        if let Some(mem) = &memory_store_for_rebuild {
            new_agent = new_agent.with_memory(mem.clone());
        }
        if let Some(store) = &store_for_rebuild {
            let title_provider = wyj_api::build_provider_from_profile(cfg.active_profile(), None)
                .unwrap_or_else(|e| {
                    tracing::warn!("重建后标题生成器 provider 构建失败: {e}");
                    wyj_api::build_provider(cfg).expect("重建后主 provider 构建不应失败")
                });
            let gen = Arc::new(SummaryGenerator::new(store.clone(), title_provider));
            new_agent = new_agent
                .with_summary(gen)
                .with_session_id(sid_for_rebuild.clone());
        }
        let mut reg = ToolRegistry::standard();
        reg.register_arc(Arc::new(TodoWriteTool::new(todo_store_for_rebuild.clone())));
        reg.register_arc(Arc::new(AskQuestionTool::new()));
        if let Some(key) = cfg.search_api_key.as_deref().filter(|k| !k.is_empty()) {
            reg.register_arc(Arc::new(wyj_tools::WebSearchTool::new(key)));
        }
        register_computer_tool_if_enabled(&mut reg, cfg);
        register_window_capture_tool_if_enabled(&mut reg, cfg);
        register_app_computer_tool_if_enabled(&mut reg, cfg);
        for tool in mcp_tools_for_rebuild.read().unwrap().iter() {
            reg.register_arc(tool.clone());
        }
        let sub_factory = make_sub_agent_factory(
            cfg.clone(),
            claude_md_for_rebuild.clone(),
            mcp_tools_for_rebuild.clone(),
        );
        reg.register_arc(Arc::new(SubAgentTool::new_shared(
            agent_defs_for_rebuild.clone(),
            hub_for_rebuild.clone(),
            move |def| sub_factory(def),
        )));
        for def in reg.definitions() {
            if let Some(t) = reg.get(&def.name) {
                new_agent.register_tool(t);
            }
        }
        if enable_lazy_tool_schemas {
            new_agent.enable_lazy_tools(
                ["Read", "Glob", "Grep", "AskQuestion", "TodoWrite"]
                    .into_iter()
                    .map(str::to_string),
                cfg.model_runtime.lazy_tools_threshold,
                cfg.model_runtime.lazy_tools_top_k,
                cfg.model_runtime.lazy_tools_sticky_turns,
            );
        }
        Ok(new_agent)
    });

    if let Some(prompt) = cli.prompt {
        let mut session = Session::new();
        if let Some(file) = session_store_arc
            .as_ref()
            .and_then(|store| store.load(&session_id).ok())
        {
            session.total_input_tokens = file.input_tokens;
            session.total_output_tokens = file.output_tokens;
            session.routing_events = file.routing_events;
            session.current_checkpoint_id = file.current_checkpoint_id;
            session.branch_parent_session_id = file.branch_parent_session_id;
            session.branch_parent_checkpoint_id = file.branch_parent_checkpoint_id;
        }
        session.messages = initial_messages;
        session.push_user(prompt);
        let turns = session.messages.len();
        let started = std::time::Instant::now();
        agent
            .run_turn(&mut session, &tool_ctx, &mut |d| {
                print!("{d}");
                let _ = io::stdout().flush();
            })
            .await?;
        println!();
        // 评测基准：WYJ_STATS_JSON=1 时向 stderr 输出一行机器可读统计，
        // 供 benchmarks/run.sh 解析做改进前后对比。先补一个换行：thinking
        // 增量用 eprint!（无换行）输出，否则 JSON 会被拼接到思考文本尾部。
        if std::env::var("WYJ_STATS_JSON").is_ok_and(|v| v == "1") {
            let full_input = session
                .total_input_tokens
                .saturating_add(session.total_cache_read_tokens)
                .saturating_add(session.total_cache_write_tokens);
            let cache_hit_ratio = if full_input > 0 {
                session.total_cache_read_tokens as f64 / full_input as f64
            } else {
                0.0
            };
            let context_tokens = wyj_core::estimate_tokens(&session.messages);
            eprintln!();
            eprintln!(
                "{{\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"full_input_tokens\":{},\"cache_hit_ratio\":{:.4},\"context_tokens\":{},\"context_window\":{},\"api_calls\":{},\"tool_schema_tokens\":{},\"tool_schema_tokens_saved\":{},\"duration_secs\":{:.1}}}",
                session.total_input_tokens,
                session.total_output_tokens,
                session.total_cache_read_tokens,
                session.total_cache_write_tokens,
                full_input,
                cache_hit_ratio,
                context_tokens,
                context_window,
                session.api_calls,
                session.tool_schema_tokens,
                session.tool_schema_tokens_saved,
                started.elapsed().as_secs_f64()
            );
        }
        // 结束前等待全部后台子 Agent 完成（结果由 Done 事件回调打印）
        let bg_count = sub_agent_hub.background_count();
        if bg_count > 0 {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("subagent.waiting_bg", &[("count", &bg_count.to_string())])
            );
        }
        sub_agent_hub.wait_background().await;
        // 杀掉全部后台 Bash 任务的进程组，防止孤儿进程
        wyj_tools::BashSessionManager::global().kill_all();
        // 升级版会话统计
        let in_tok = session.total_input_tokens;
        let out_tok = session.total_output_tokens;
        eprintln!("\n── {} ──", wyj_i18n::tr("main.session_stats"));
        eprintln!("  tokens: {in_tok}↑ {out_tok}↓");
        if let Some(hs) = &history_store {
            let _ = hs.append(&HistoryEntry {
                timestamp: now_iso(),
                session_id: session_id.clone(),
                input_tokens: in_tok,
                output_tokens: out_tok,
                turns,
                cwd: cwd.display().to_string(),
            });
        }
        if let Some(store) = &session_store_arc {
            let _ = store.save(&SessionFile {
                session_id: session_id.clone(),
                title: extract_title(&session.messages),
                last_preview: extract_preview(&session.messages),
                cwd: cwd.display().to_string(),
                timestamp: now_iso(),
                turns,
                input_tokens: in_tok,
                output_tokens: out_tok,
                messages: session.messages.clone(),
                routing_events: session.routing_events.clone(),
                current_checkpoint_id: session.current_checkpoint_id.clone(),
                branch_parent_session_id: session.branch_parent_session_id.clone(),
                branch_parent_checkpoint_id: session.branch_parent_checkpoint_id.clone(),
                title_generated: false,
            });
        }
    } else if cli.headless {
        repl(
            agent,
            rebuild_fn.clone(),
            tool_ctx,
            history_store,
            session_store_arc.clone(),
            session_id,
            cwd,
            initial_messages,
            cfg.clone(),
            mode.clone(),
            shared_agent_defs.clone(),
            local_plugin.clone(),
            !cli.no_hooks,
            mcp_tools.clone(),
            sub_agent_hub.clone(),
        )
        .await?;
    } else {
        wyj_tui::run_tui(
            agent,
            rebuild_fn,
            cwd,
            history_store,
            session_store_arc,
            initial_messages,
            session_id,
            model_name,
            context_window,
            mode,
            todo_store,
            system_prompt_extra,
            cfg,
            sub_agent_hub.clone(),
            local_plugin.clone(),
            mcp_tools.clone(),
            shared_agent_defs,
        )
        .await?;
    }
    Ok(())
}

/// 按 agent 定义的 `tools` 白名单（`None` 表示不限制）从给定 registry 里选出
/// 允许该子 Agent 使用的工具集。纯函数，便于脱离 Provider/网络构建单测。
fn select_sub_agent_tools(
    def: &wyj_core::AgentDefinition,
    registry: &ToolRegistry,
) -> Vec<Arc<dyn wyj_tools::Tool>> {
    registry
        .definitions()
        .into_iter()
        .filter(|tdef| {
            def.tools
                .as_ref()
                .map_or(true, |list| list.iter().any(|n| n == &tdef.name))
        })
        .filter_map(|tdef| registry.get(&tdef.name))
        .collect()
}

/// computer-use 是否应当注册并按需注册：仅 macOS/Windows 编译进 `wyj-tools`
/// （其余平台该 crate 内 `computer` 模块整体不存在，见 wyj-tools/src/lib.rs），
/// 且需要当前 profile 支持 vision（截图靠 image content block 回传）+
/// `provider == Anthropic`（Anthropic Messages API 协议本身——tool_result 内
/// 嵌原生 image block——才有截图回传给模型的通路；OpenAI Chat Completions 的
/// `tool` 角色消息不支持图片，`crates/api/src/openai.rs` 会把截图降级成纯
/// 文本占位符，模型看不到画面，computer-use 名存实亡，因此 provider=OpenAI
/// 时不注册）。
///
/// 是否用**原生**工具声明由 `Profile::is_official_anthropic_endpoint()` 决定：
/// - 官方 api.anthropic.com：注册为原生 `computer_20251124` 工具（无 description/
///   input_schema）——Claude 训练时习得了这个空 schema 工具的调用约定。
/// - 第三方 Anthropic 协议兼容端点（MiniMax/GLM/Kimi 等，常见于
///   `provider = "anthropic"` + 自定义 `base_url`）：说的是同一套 Messages API
///   协议、截图回传通路一样打通，但没有 Claude 那层专属训练，收到无 schema
///   的原生工具类型会因为不知道怎么调用而报错。这类端点改注册为**普通 custom
///   工具**（带完整 description + input_schema，`ComputerTool::new(_, false)`），
///   任何具备基本工具调用能力的模型都能按标准协议使用。`run()` 的动作分派逻辑
///   两种模式完全一致，只是对外声明方式不同。
///
/// 子 Agent 工厂（`make_sub_agent_factory`）不调用本函数——与 Agent/AskQuestion
/// 一致，默认不给子 Agent。
///
/// 返回值：是否实际注册了。调用方据此决定要不要追加
/// `wyj_core::prompts::COMPUTER_USE_HINT`（教模型优先走稳定窗口后台路径、用
/// Bash 直接启动应用，并把旧 `computer` 视为显式前台兼容能力）。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn register_computer_tool_if_enabled(registry: &mut ToolRegistry, cfg: &Config) -> bool {
    let profile = cfg.active_profile();
    if !profile.vision || !matches!(profile.provider, wyj_config::Provider::Anthropic) {
        return false;
    }
    let native = profile.is_official_anthropic_endpoint();
    let fallback = match cfg.computer_use.foreground_fallback {
        wyj_config::ForegroundFallback::Disabled => {
            wyj_tools::computer::ForegroundFallbackPolicy::Disabled
        }
        wyj_config::ForegroundFallback::Ask => wyj_tools::computer::ForegroundFallbackPolicy::Ask,
        wyj_config::ForegroundFallback::IdleOnly => {
            wyj_tools::computer::ForegroundFallbackPolicy::IdleOnly
        }
    };
    registry.register_arc(Arc::new(
        wyj_tools::computer::ComputerTool::new_with_policy(
            wyj_tools::computer::DEFAULT_MAX_DIM,
            native,
            wyj_tools::computer::ForegroundPolicy {
                fallback,
                quiet_period: std::time::Duration::from_millis(cfg.computer_use.quiet_period_ms),
                max_defer: std::time::Duration::from_secs(cfg.computer_use.max_defer_secs),
                restore_context: cfg.computer_use.restore_context,
            },
        ),
    ));
    wyj_computer::activity::ensure_monitor();
    true
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn register_computer_tool_if_enabled(_registry: &mut ToolRegistry, _cfg: &Config) -> bool {
    false
}

/// WindowCapture：独立于 computer-use 的只读按窗口截图工具（v1.4，见
/// `tools::window_capture::WindowCaptureTool` 文档），注册门槛与
/// `register_computer_tool_if_enabled` 完全一致（同样需要把截图作为 image
/// block 塞进 tool_result，因此同样要求 vision + Anthropic provider）。
/// 刻意复刻同一段短门控，保持此工具可独立演进。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn register_window_capture_tool_if_enabled(registry: &mut ToolRegistry, cfg: &Config) {
    let profile = cfg.active_profile();
    if !profile.vision || !matches!(profile.provider, wyj_config::Provider::Anthropic) {
        return;
    }
    registry.register_arc(Arc::new(wyj_tools::window_capture::WindowCaptureTool::new(
        wyj_tools::computer::DEFAULT_MAX_DIM,
    )));
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn register_window_capture_tool_if_enabled(_registry: &mut ToolRegistry, _cfg: &Config) {}

/// macOS 后台优先 computer-use：按稳定窗口目标截图，用 Accessibility/目标 PID
/// 事件执行，不移动全局光标、不激活应用。Windows 在 v1.4 先保持安全边界，
/// 只提供稳定窗口截图和默认关闭的前台兼容工具。
#[cfg(target_os = "macos")]
fn register_app_computer_tool_if_enabled(registry: &mut ToolRegistry, cfg: &Config) {
    let profile = cfg.active_profile();
    if !profile.vision || !matches!(profile.provider, wyj_config::Provider::Anthropic) {
        return;
    }
    wyj_computer::activity::ensure_monitor();
    registry.register_arc(Arc::new(wyj_tools::app_computer::AppComputerTool::new(
        wyj_tools::computer::DEFAULT_MAX_DIM,
        std::time::Duration::from_millis(cfg.computer_use.quiet_period_ms),
    )));
}

#[cfg(not(target_os = "macos"))]
fn register_app_computer_tool_if_enabled(_registry: &mut ToolRegistry, _cfg: &Config) {}

/// 构建子 Agent 工厂：按 agent 定义解析 Profile 与模型，注册按定义过滤后的工具集。
/// 模型解析优先级：定义的 model 字段（Profile 名）→ [subagent].explore_profile（仅
/// Explore 类型）→ [subagent].default_profile → 主 Agent 当前激活分组的 Normal 模型。
fn make_sub_agent_factory(
    cfg: Config,
    claude_md: Arc<wyj_core::ClaudeMdLoader>,
    mcp_tools: wyj_tools::SharedMcpTools,
) -> wyj_tools::AgentFactory {
    Arc::new(move |def: &wyj_core::AgentDefinition| {
        let routing_role = if def.name.eq_ignore_ascii_case("explore") {
            RoutingRole::Explore
        } else if def.name.to_ascii_lowercase().contains("review") {
            RoutingRole::Review
        } else {
            RoutingRole::Execute
        };
        let mut profile = None;
        let mut routing_enabled = false;
        if let Some(name) = &def.model {
            profile = cfg.profile_by_name(name);
            if profile.is_none() {
                tracing::warn!(
                    "agent 定义 `{}` 引用的 Profile `{}` 不存在，回退默认",
                    def.name,
                    name
                );
            }
        }
        if profile.is_none() {
            if let Some(name) = configured_route_names(&cfg, routing_role)
                .into_iter()
                .next()
            {
                profile = cfg.profile_by_name(&name);
                routing_enabled = profile.is_some();
            }
        }
        if profile.is_none() && def.name == "Explore" {
            if let Some(name) = &cfg.subagent.explore_profile {
                profile = cfg.profile_by_name(name);
                if profile.is_none() {
                    tracing::warn!("[subagent].explore_profile `{name}` 不存在，回退默认");
                }
            }
        }
        if profile.is_none() {
            if let Some(name) = &cfg.subagent.default_profile {
                profile = cfg.profile_by_name(name);
                if profile.is_none() {
                    tracing::warn!("[subagent].default_profile `{name}` 不存在，回退默认");
                }
            }
        }
        let (p, model) = match profile {
            Some(p) => (p, model_for_routing_role(p, routing_role)),
            None => (
                cfg.active_profile(),
                cfg.model_for_mode(&AgentMode::Normal).to_string(),
            ),
        };
        let provider = wyj_api::build_provider_from_profile(p, Some(&model))?;
        let model_resolution = wyj_api::ModelCatalog::resolve(p, Some(&model));
        let model_capabilities = model_resolution.capabilities.clone();
        let fallback_routes = if routing_enabled {
            build_fallback_routes(&cfg, routing_role, &p.name)
        } else {
            Vec::new()
        };
        let enable_lazy_tool_schemas = model_capabilities.tool_calling.value;

        let mut sub_agent = Agent::new(provider)
            .with_max_tokens(p.max_tokens)
            .with_context_window(p.context_window)
            .with_model_capabilities(model_capabilities)
            .with_route_identity(p.name.clone(), model_resolution.identity.vendor, model)
            .with_fallback_routes(fallback_routes, cfg.routing.cross_provider_fallback)
            .with_thinking(p.thinking_budget, p.interleaved_thinking)
            .with_claude_md(claude_md.clone());
        if !def.system_prompt.is_empty() {
            sub_agent = sub_agent.with_system(def.system_prompt.clone());
        }

        let mut sub_registry = ToolRegistry::standard();
        // WebSearch：与主 Agent 同样的"仅配置了 search_api_key 才注册"语义，
        // 让子 Agent 类型定义（如 general-purpose 的 tools: None）能拿到它。
        if let Some(key) = cfg.search_api_key.as_deref().filter(|k| !k.is_empty()) {
            sub_registry.register_arc(Arc::new(wyj_tools::WebSearchTool::new(key)));
        }
        // MCP：读取 spawn 此刻已连接成功的快照；此后才连完的 server，本次
        // 创建的子 Agent 看不到（与主 Agent「界面先开、后台陆续补」同款权衡）。
        for tool in mcp_tools.read().unwrap().iter() {
            sub_registry.register_arc(tool.clone());
        }
        for t in select_sub_agent_tools(def, &sub_registry) {
            sub_agent.register_tool(t);
        }
        if enable_lazy_tool_schemas {
            sub_agent.enable_lazy_tools(
                ["Read", "Glob", "Grep"].into_iter().map(str::to_string),
                cfg.model_runtime.lazy_tools_threshold,
                cfg.model_runtime.lazy_tools_top_k,
                cfg.model_runtime.lazy_tools_sticky_turns,
            );
        }
        if let Some(list) = &def.tools {
            for n in list {
                if sub_registry.get(n).is_none() {
                    tracing::warn!("agent 定义 `{}` 引用了未知工具 `{n}`，已忽略", def.name);
                }
            }
        }
        Ok(sub_agent)
    })
}

/// 返回当前进程应当使用的 MCP 配置。项目配置和启用插件贡献在每个 Agent
/// 边界重新读取，因此 `/extensions enable|disable|remove` 不需要重启进程。
/// 未信任的项目级 server 被静默排除在外（每次调用都重新查信任状态，
/// 因此用户在另一个终端跑 `wyj-code trust-mcp` 批准后，下一轮 reconcile
/// 会自动捡起，不需要重启这个 REPL 进程）；调用方如需提示用户"有 server
/// 待批准"，另行调用 `wyj_store::project_trust::trust_status(cwd)`。
fn effective_mcp_servers_for_runtime(
    cfg: &Config,
    cwd: &std::path::Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) -> Vec<wyj_config::McpServerConfig> {
    let (mut servers, _pending) =
        wyj_store::mcp_install::effective_mcp_servers_trust_split(cfg, cwd);
    if let Some(local) = local_plugin {
        let mut names: std::collections::HashSet<String> =
            servers.iter().map(|server| server.name.clone()).collect();
        for server in &local.mcp_servers {
            if names.insert(server.name.clone()) {
                servers.push(server.clone());
            }
        }
    }
    servers
}

fn refresh_agent_definitions(
    shared_defs: &wyj_tools::SharedAgentDefinitions,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    cwd: &std::path::Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) {
    let mut sources = wyj_store::plugin_install::enabled_plugin_agent_paths(cwd);
    if let Some(local) = local_plugin {
        sources.extend(local.agent_paths.clone());
    }
    let defs = wyj_core::load_agent_defs(cwd, &sources);
    if let Ok(mut current) = shared_defs.write() {
        *current = defs;
    }
    let mut agent = (**shared_agent.read().unwrap()).clone();
    agent.refresh_tool_definitions();
    *shared_agent.write().unwrap() = Arc::new(agent);
}

fn resolve_checkpoint_id(
    store: &wyj_core::CheckpointStore,
    requested: Option<String>,
) -> Result<String> {
    match requested {
        Some(id) => Ok(id),
        None => store
            .latest()?
            .map(|checkpoint| checkpoint.id)
            .ok_or_else(|| anyhow::anyhow!("当前会话还没有 checkpoint")),
    }
}

fn checkpoint_list_text(store: &wyj_core::CheckpointStore) -> Result<String> {
    let checkpoints = store.list()?;
    if checkpoints.is_empty() {
        return Ok("当前会话还没有 checkpoint。".to_string());
    }
    Ok(checkpoints
        .into_iter()
        .rev()
        .map(|checkpoint| {
            format!(
                "{}  {:?}  {}  {} messages",
                checkpoint.id,
                checkpoint.kind,
                checkpoint.name.as_deref().unwrap_or("-"),
                checkpoint.message_count
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn rewind_preview_text(preview: &wyj_core::RewindPreview) -> String {
    let mut lines = vec![format!(
        "checkpoint {} affects {} file(s)",
        preview.checkpoint_id,
        preview.affected_files.len()
    )];
    lines.extend(
        preview
            .affected_files
            .iter()
            .take(50)
            .map(|path| format!("- {}", path.display())),
    );
    if preview.affected_files.len() > 50 {
        lines.push(format!(
            "- ... and {} more",
            preview.affected_files.len() - 50
        ));
    }
    if let Some(note) = &preview.note {
        lines.push(format!("note: {note}"));
    }
    lines.join("\n")
}

fn parse_rewind_scope(scope: &str) -> wyj_core::RewindScope {
    match scope {
        "conversation" => wyj_core::RewindScope::Conversation,
        "files" => wyj_core::RewindScope::Files,
        _ => wyj_core::RewindScope::Both,
    }
}

fn run_session_command(command: SessionCommand) -> Result<()> {
    let sessions = SessionStore::new(wyj_config::config_dir()?.join("sessions"))?;
    match command {
        SessionCommand::Checkpoint { session_id, name } => {
            let file = sessions.load(&session_id)?;
            let store = wyj_core::CheckpointStore::new(sessions.dir(), session_id)?;
            let checkpoint = store.create(
                Path::new(&file.cwd),
                &file.messages,
                wyj_core::CheckpointKind::Manual,
                name,
            )?;
            println!("{}", checkpoint.id);
        }
        SessionCommand::Checkpoints { session_id } => {
            let store = wyj_core::CheckpointStore::new(sessions.dir(), session_id)?;
            println!("{}", checkpoint_list_text(&store)?);
        }
        SessionCommand::Rewind {
            session_id,
            checkpoint_id,
            scope,
            force,
        } => {
            let mut file = sessions.load(&session_id)?;
            let cwd = PathBuf::from(&file.cwd);
            let store = wyj_core::CheckpointStore::new(sessions.dir(), session_id)?;
            let checkpoint = store.load(&checkpoint_id)?;
            let scope = parse_rewind_scope(&scope);
            if matches!(
                scope,
                wyj_core::RewindScope::Files | wyj_core::RewindScope::Both
            ) {
                let preview = store.preview_files(&checkpoint_id, &cwd)?;
                println!("{}", rewind_preview_text(&preview));
                if preview.requires_confirmation && !force {
                    anyhow::bail!("file rewind requires --force after reviewing the preview");
                }
            }
            let protection = store.create(
                &cwd,
                &file.messages,
                wyj_core::CheckpointKind::PreRewind,
                Some(format!("before rewind {checkpoint_id}")),
            )?;
            if matches!(
                scope,
                wyj_core::RewindScope::Files | wyj_core::RewindScope::Both
            ) {
                store.restore_files(&checkpoint_id, &cwd, force)?;
            }
            if matches!(
                scope,
                wyj_core::RewindScope::Conversation | wyj_core::RewindScope::Both
            ) {
                file.messages = checkpoint.messages;
                file.turns = file
                    .messages
                    .iter()
                    .filter(|message| matches!(message.role, wyj_api::types::Role::User))
                    .count();
                file.title = extract_title(&file.messages);
                file.last_preview = extract_preview(&file.messages);
                file.timestamp = now_iso();
                file.title_generated = false;
            }
            file.current_checkpoint_id = Some(checkpoint_id.clone());
            file.timestamp = now_iso();
            sessions.save(&file)?;
            println!(
                "rewound to {checkpoint_id}; protection checkpoint {}",
                protection.id
            );
        }
        SessionCommand::Branch {
            session_id,
            checkpoint_id,
            restore_files,
            force,
        } => {
            let file = sessions.load(&session_id)?;
            let cwd = PathBuf::from(&file.cwd);
            let store = wyj_core::CheckpointStore::new(sessions.dir(), session_id.clone())?;
            let checkpoint = store.load(&checkpoint_id)?;
            if restore_files {
                let preview = store.preview_files(&checkpoint_id, &cwd)?;
                println!("{}", rewind_preview_text(&preview));
                if preview.requires_confirmation && !force {
                    anyhow::bail!(
                        "branch file restore requires --force after reviewing the preview"
                    );
                }
                store.create(
                    &cwd,
                    &file.messages,
                    wyj_core::CheckpointKind::PreRewind,
                    Some(format!("before branch restore {checkpoint_id}")),
                )?;
                store.restore_files(&checkpoint_id, &cwd, force)?;
            }
            let branch = sessions.branch_from_checkpoint(&session_id, &checkpoint)?;
            println!("{}", branch.session_id);
        }
    }
    Ok(())
}

/// 在安全 Agent 边界原子替换 MCP 工具快照。
///
/// `Agent` 是按回合读取的不可变快照：正在执行的旧快照可以安全完成，下一回合
/// 读取到的新快照则只包含 runtime 当前仍然连接且启用的 server。共享给子 Agent
/// 工厂和 profile 重建的工具列表也同步替换，避免出现主 Agent 与子 Agent 看到的
/// MCP 集合不一致。
fn apply_mcp_runtime_snapshot(
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    mcp_tools: &wyj_tools::SharedMcpTools,
) {
    let tools = runtime_tools(mcp_tools);
    let mut new_agent = (**shared_agent.read().unwrap()).clone();
    new_agent.remove_tools_where(|name| name.starts_with("mcp__"));
    for tool in &tools {
        new_agent.register_tool(tool.clone());
    }
    *shared_agent.write().unwrap() = Arc::new(new_agent);
}

fn runtime_tools(mcp_tools: &wyj_tools::SharedMcpTools) -> Vec<Arc<dyn wyj_tools::Tool>> {
    mcp_tools.read().unwrap().clone()
}

fn refresh_mcp_runtime(
    runtime: &mut wyj_mcp::McpRuntime,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
    mcp_tools: &wyj_tools::SharedMcpTools,
    fallback_cfg: &Config,
    cwd: &std::path::Path,
    local_plugin: Option<&wyj_store::lockfile::PluginContributions>,
) {
    let live_cfg = wyj_config::Config::load().unwrap_or_else(|e| {
        tracing::debug!("读取运行时配置失败，继续使用启动配置: {e}");
        fallback_cfg.clone()
    });
    let servers = effective_mcp_servers_for_runtime(&live_cfg, cwd, local_plugin);
    for event in runtime.reconcile(&servers) {
        if let wyj_mcp::McpRuntimeEvent::Removed { name } = event {
            println!("[MCP {name}] 已从下一回合工具快照移除");
        }
    }
    for event in runtime.drain() {
        match event {
            wyj_mcp::McpRuntimeEvent::Connected { name, tool_count } => {
                println!("[MCP {name}] 已连接，{tool_count} 个工具已就绪");
            }
            wyj_mcp::McpRuntimeEvent::Failed { name, reason } => {
                eprintln!("[MCP {name}] 连接失败: {reason}");
            }
            wyj_mcp::McpRuntimeEvent::Removed { name } => {
                println!("[MCP {name}] 已从下一回合工具快照移除");
            }
        }
    }
    let snapshot = runtime.tools();
    {
        let mut shared = mcp_tools.write().unwrap();
        *shared = snapshot;
    }
    apply_mcp_runtime_snapshot(shared_agent, mcp_tools);
}

#[allow(clippy::too_many_arguments)]
async fn repl(
    agent: Agent,
    rebuild_fn: wyj_tui::RebuildFn,
    ctx: ToolCtx,
    history_store: Option<HistoryStore>,
    session_store: Option<Arc<SessionStore>>,
    session_id: String,
    cwd: std::path::PathBuf,
    initial_messages: Vec<wyj_api::types::Message>,
    cfg: Config,
    mode: AgentMode,
    shared_agent_defs: wyj_tools::SharedAgentDefinitions,
    local_plugin: Option<wyj_store::lockfile::PluginContributions>,
    hooks_enabled: bool,
    mcp_tools: wyj_tools::SharedMcpTools,
    sub_agent_hub: Arc<wyj_tools::SubAgentHub>,
) -> Result<()> {
    use std::io::BufRead;
    println!(
        "wyj-code v{} — 输入问题回车发送，/quit 退出，Ctrl-D 退出",
        env!("CARGO_PKG_VERSION")
    );
    let mut session_id = session_id;
    let mut checkpoint_store = session_store
        .as_ref()
        .and_then(|store| wyj_core::CheckpointStore::new(store.dir(), session_id.clone()).ok())
        .map(Arc::new);
    let mut session = Session::new();
    if let Some(file) = session_store
        .as_ref()
        .and_then(|store| store.load(&session_id).ok())
    {
        session.total_input_tokens = file.input_tokens;
        session.total_output_tokens = file.output_tokens;
        session.routing_events = file.routing_events;
        session.current_checkpoint_id = file.current_checkpoint_id;
        session.branch_parent_session_id = file.branch_parent_session_id;
        session.branch_parent_checkpoint_id = file.branch_parent_checkpoint_id;
    }
    session.messages = initial_messages;
    let stdin = io::stdin();
    let mut turns = 0usize;
    let repl_home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let mut plugin_agent_paths: Vec<std::path::PathBuf>;
    let mut effective_mcp_count: usize;
    let mut cmd_registry: Arc<CommandRegistry>;

    // `--headless` 多轮 REPL 对称于 TUI 的 MCP 连接方案：不在启动时同步等待
    // （那是 -p 单轮模式的做法，见上方 main() 里的宽限期逻辑），而是后台连接、
    // 每轮 read_line 之后、run_turn 之前非阻塞排空一次结果，用克隆-注册-替换
    // 的手法动态挂载新工具。已知局限：`run_turn` 开始时对 Agent 的只读快照
    // 决定了"本轮开始后新连上的工具在这一轮不生效，下一轮才可见"——这是 TUI
    // 架构本来就有、已被接受的行为，此处对称搬过来，不重新设计。
    // 与 `-p` 单次模式一致：未信任的项目级 MCP server 只提示一次，不在每轮
    // reconcile 时重复刷屏（`effective_mcp_servers_for_runtime` 本身已经在
    // 每轮静默排除它们，这里只是让用户知道"为什么少了几个工具"）。
    if let wyj_store::TrustStatus::Pending(servers) = wyj_store::project_trust::trust_status(&cwd) {
        let mut names: Vec<_> = servers.iter().map(|s| s.name.clone()).collect();
        names.sort();
        eprintln!(
            "[以下项目级 MCP server 尚未信任批准，本次未连接: {}；运行 `wyj-code trust-mcp` 批准]",
            names.join(", ")
        );
    }

    let shared_agent = Arc::new(std::sync::RwLock::new(Arc::new(agent)));
    let mut mcp_runtime = wyj_mcp::McpRuntime::new();
    mcp_runtime.reconcile(&effective_mcp_servers_for_runtime(
        &cfg,
        &cwd,
        local_plugin.as_ref(),
    ));

    loop {
        print!("\n> ");
        io::stdout().flush()?;
        let mut input = String::new();
        match stdin.lock().read_line(&mut input) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("读取失败: {e}");
                break;
            }
        }
        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        // ── ! Bash 内联执行 ──────────────────────────────────────────────────
        if let Some(cmd_str) = trimmed.strip_prefix('!') {
            let cmd_str = cmd_str.trim();
            use std::process::Command;
            match Command::new("sh").arg("-c").arg(cmd_str).output() {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !stdout.is_empty() {
                        print!("{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprint!("{stderr}");
                    }
                    if !out.status.success() {
                        eprintln!("[exit {}]", out.status.code().unwrap_or(-1));
                    }
                }
                Err(e) => eprintln!("执行失败: {e}"),
            }
            continue;
        }

        let home_dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        // Resource mutations are observed here, immediately before command
        // dispatch.  This is the safe boundary between two Agent turns.
        refresh_mcp_runtime(
            &mut mcp_runtime,
            &shared_agent,
            &mcp_tools,
            &cfg,
            &cwd,
            local_plugin.as_ref(),
        );
        refresh_agent_definitions(
            &shared_agent_defs,
            &shared_agent,
            &cwd,
            local_plugin.as_ref(),
        );
        plugin_agent_paths = wyj_store::plugin_install::enabled_plugin_agent_paths(&cwd);
        if let Some(local) = &local_plugin {
            plugin_agent_paths.extend(local.agent_paths.clone());
        }
        let live_cfg = wyj_config::Config::load().unwrap_or_else(|_| cfg.clone());
        effective_mcp_count =
            effective_mcp_servers_for_runtime(&live_cfg, &cwd, local_plugin.as_ref()).len();
        // Reload dynamic Skill/Plugin command sources at every input boundary. This
        // makes enable/disable and edits visible in the next command without a restart.
        let disabled_skills = wyj_store::disabled_skill_names(&cwd);
        let mut current_plugin_skill_sources =
            wyj_store::plugin_install::enabled_plugin_skill_paths(&cwd);
        if let Some(local) = &local_plugin {
            current_plugin_skill_sources.extend(local.skill_paths.clone());
        }
        cmd_registry = standard_registry_with_skills(
            &repl_home,
            &cwd,
            &disabled_skills,
            &current_plugin_skill_sources,
        );
        let dynamic_commands: Vec<(String, String, String)> = cmd_registry
            .list()
            .iter()
            .filter(|c| c.is_dynamic())
            .map(|c| (c.name().to_string(), c.description(), c.usage()))
            .collect();
        let cmd_ctx = CommandContext {
            cwd: cwd.clone(),
            model: "".to_string(),
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            cache_read_tokens: session.total_cache_read_tokens,
            cache_write_tokens: session.total_cache_write_tokens,
            context_window: 200_000,
            estimated_tokens: wyj_core::estimate_tokens(&session.messages),
            home_dir,
            sub_input_tokens: 0,
            sub_output_tokens: 0,
            effective_mcp_count,
            plugin_agent_paths: plugin_agent_paths.clone(),
            hooks_enabled,
            dynamic_commands,
        };
        if let Some(result) = cmd_registry.dispatch(trimmed, &cmd_ctx).await {
            match result {
                Ok(CommandResult::Output(out)) => {
                    println!("{out}");
                }
                Ok(CommandResult::ClearHistory) => {
                    session.clear_conversation();
                    println!("对话已清空。");
                }
                Ok(CommandResult::CompactHistory) => {
                    println!("[headless 模式不支持 /compact]");
                }
                Ok(CommandResult::CreateCheckpoint { name, list }) => match &checkpoint_store {
                    Some(store) if list => println!("{}", checkpoint_list_text(store)?),
                    Some(store) => {
                        let checkpoint = store.create(
                            &cwd,
                            &session.messages,
                            wyj_core::CheckpointKind::Manual,
                            name,
                        )?;
                        session.current_checkpoint_id = Some(checkpoint.id.clone());
                        println!("checkpoint created: {}", checkpoint.id);
                    }
                    None => println!("checkpoint storage is unavailable"),
                },
                Ok(CommandResult::Rewind {
                    checkpoint_id,
                    scope,
                    confirmed,
                }) => {
                    let Some(store) = checkpoint_store.as_ref() else {
                        println!("checkpoint storage is unavailable");
                        continue;
                    };
                    let id = resolve_checkpoint_id(store, checkpoint_id)?;
                    let checkpoint = store.load(&id)?;
                    if matches!(
                        scope,
                        wyj_core::RewindScope::Files | wyj_core::RewindScope::Both
                    ) {
                        let preview = store.preview_files(&id, &cwd)?;
                        if preview.requires_confirmation && !confirmed {
                            println!("{}", rewind_preview_text(&preview));
                            println!("re-run with `--confirm` to restore these files");
                            continue;
                        }
                    }
                    let protection = store.create(
                        &cwd,
                        &session.messages,
                        wyj_core::CheckpointKind::PreRewind,
                        Some(format!("before rewind {id}")),
                    )?;
                    if matches!(
                        scope,
                        wyj_core::RewindScope::Files | wyj_core::RewindScope::Both
                    ) {
                        let preview = store.restore_files(&id, &cwd, confirmed)?;
                        println!("{}", rewind_preview_text(&preview));
                    }
                    if matches!(
                        scope,
                        wyj_core::RewindScope::Conversation | wyj_core::RewindScope::Both
                    ) {
                        session.messages = checkpoint.messages;
                        session.current_checkpoint_id = Some(id.clone());
                        turns = session
                            .messages
                            .iter()
                            .filter(|message| matches!(message.role, wyj_api::types::Role::User))
                            .count();
                    }
                    println!(
                        "rewound to {id}; safety checkpoint before rewind: {}",
                        protection.id
                    );
                }
                Ok(CommandResult::BranchSession {
                    checkpoint_id,
                    restore_files,
                    confirmed,
                }) => {
                    let (Some(store), Some(session_files)) =
                        (checkpoint_store.as_ref(), session_store.as_ref())
                    else {
                        println!("session/checkpoint storage is unavailable");
                        continue;
                    };
                    let id = resolve_checkpoint_id(store, checkpoint_id)?;
                    let checkpoint = store.load(&id)?;
                    if restore_files {
                        let preview = store.preview_files(&id, &cwd)?;
                        if preview.requires_confirmation && !confirmed {
                            println!("{}", rewind_preview_text(&preview));
                            println!(
                                "re-run with `--restore-files --confirm` to restore files and branch"
                            );
                            continue;
                        }
                        store.create(
                            &cwd,
                            &session.messages,
                            wyj_core::CheckpointKind::PreRewind,
                            Some(format!("before branch restore {id}")),
                        )?;
                        store.restore_files(&id, &cwd, confirmed)?;
                    }
                    session_files.save(&SessionFile {
                        session_id: session_id.clone(),
                        title: extract_title(&session.messages),
                        last_preview: extract_preview(&session.messages),
                        cwd: cwd.display().to_string(),
                        timestamp: now_iso(),
                        turns,
                        input_tokens: session.total_input_tokens,
                        output_tokens: session.total_output_tokens,
                        messages: session.messages.clone(),
                        routing_events: session.routing_events.clone(),
                        current_checkpoint_id: session.current_checkpoint_id.clone(),
                        branch_parent_session_id: session.branch_parent_session_id.clone(),
                        branch_parent_checkpoint_id: session.branch_parent_checkpoint_id.clone(),
                        title_generated: false,
                    })?;
                    let branch = session_files.branch_from_checkpoint(&session_id, &checkpoint)?;
                    session_id = branch.session_id.clone();
                    session = Session::new();
                    session.messages = branch.messages;
                    session.current_checkpoint_id = branch.current_checkpoint_id;
                    session.branch_parent_session_id = branch.branch_parent_session_id;
                    session.branch_parent_checkpoint_id = branch.branch_parent_checkpoint_id;
                    turns = branch.turns;
                    let new_store = Arc::new(wyj_core::CheckpointStore::new(
                        session_files.dir(),
                        session_id.clone(),
                    )?);
                    checkpoint_store = Some(new_store.clone());
                    let mut updated_agent = (**shared_agent.read().unwrap()).clone();
                    updated_agent.set_session_id(session_id.clone());
                    updated_agent.set_checkpoint_store(new_store);
                    *shared_agent.write().unwrap() = Arc::new(updated_agent);
                    println!("created and switched to branch session {session_id} from {id}");
                }
                Ok(CommandResult::ControlSubAgent { id, action }) => {
                    let result = match action {
                        wyj_commands::registry::SubAgentControlAction::FollowUp(text) => {
                            sub_agent_hub.send_follow_up(
                                id,
                                vec![wyj_api::types::ContentBlock::Text { text }],
                            )
                        }
                        wyj_commands::registry::SubAgentControlAction::Interrupt => {
                            sub_agent_hub.interrupt(id)
                        }
                        wyj_commands::registry::SubAgentControlAction::RetryLast => {
                            sub_agent_hub.retry_last(id)
                        }
                    };
                    println!("sub-agent a{id} control result: {result:?}");
                }
                Ok(CommandResult::OpenProfileDialog) | Ok(CommandResult::SwitchProfile(_)) => {
                    println!("{}", wyj_i18n::tr("profile.headless_unsupported"));
                }
                Ok(CommandResult::ModelDoctor(profile_name)) => {
                    let live_cfg = wyj_config::Config::load().unwrap_or_else(|_| cfg.clone());
                    let selected = profile_name
                        .as_deref()
                        .and_then(|name| live_cfg.profile_by_name(name))
                        .unwrap_or_else(|| live_cfg.active_profile());
                    let cache = wyj_config::config_dir()
                        .ok()
                        .map(|base| wyj_api::CapabilityCache::new(&base));
                    let report =
                        wyj_api::ModelDoctorReport::static_report(selected, cache.as_ref());
                    print_model_doctor_report(&report);
                }
                Ok(CommandResult::SandboxStatus) => {
                    let live_cfg = wyj_config::Config::load().unwrap_or_else(|_| cfg.clone());
                    print_sandbox_report(&live_cfg.sandbox, false)?;
                }
                Ok(CommandResult::RunPrompt(prompt)) => {
                    // Skill 展开后的 prompt → 当作用户消息发给 agent
                    session.push_user(prompt);
                    turns += 1;
                    println!();
                    let agent_snapshot = shared_agent.read().unwrap().clone();
                    if let Err(e) = agent_snapshot
                        .run_turn(&mut session, &ctx, &mut |d| {
                            print!("{d}");
                            let _ = io::stdout().flush();
                        })
                        .await
                    {
                        eprintln!("\n[错误] {e}");
                    }
                    println!();
                    let in_tok = session.total_input_tokens;
                    let out_tok = session.total_output_tokens;
                    eprintln!("  tokens: {in_tok}↑ {out_tok}↓");
                }
                Ok(CommandResult::RunPromptScoped {
                    text,
                    allowed_tools,
                    profile,
                }) => {
                    // 带 allowed-tools 的自定义命令：临时把权限模式收紧为 Allowlist，
                    // 跑完这一轮（无论成功失败）都还原快照，不影响 --plan/--bypass-permissions
                    // 等其他模式设定的基线权限。`model:` 只影响这一轮，不改变当前会话
                    // 的 active profile。
                    let agent_snapshot = if let Some(profile_name) = profile {
                        let mut scoped_cfg =
                            wyj_config::Config::load().unwrap_or_else(|_| cfg.clone());
                        if scoped_cfg.profile_by_name(&profile_name).is_none() {
                            eprintln!("[skill] 未找到 Profile: {profile_name}");
                            continue;
                        }
                        scoped_cfg.active_profile = profile_name;
                        let scoped_model = scoped_cfg.model_for_mode(&mode).to_string();
                        match rebuild_fn(&scoped_cfg, &scoped_model) {
                            Ok(agent) => Arc::new(agent),
                            Err(e) => {
                                eprintln!("[skill] 构造临时 Profile Agent 失败: {e}");
                                continue;
                            }
                        }
                    } else {
                        shared_agent.read().unwrap().clone()
                    };
                    session.push_user(text);
                    turns += 1;
                    println!();
                    let prev_mode = ctx.permission_mode.read().unwrap().clone();
                    if let Some(tools) = allowed_tools {
                        let scoped = tools.into_iter().collect();
                        ctx.set_permission_mode(if matches!(mode, AgentMode::Plan) {
                            PermissionMode::Plan(scoped)
                        } else {
                            PermissionMode::Allowlist(scoped)
                        });
                    }
                    let run_result = agent_snapshot
                        .run_turn(&mut session, &ctx, &mut |d| {
                            print!("{d}");
                            let _ = io::stdout().flush();
                        })
                        .await;
                    ctx.set_permission_mode(prev_mode);
                    if let Err(e) = run_result {
                        eprintln!("\n[错误] {e}");
                    }
                    println!();
                    let in_tok = session.total_input_tokens;
                    let out_tok = session.total_output_tokens;
                    eprintln!("  tokens: {in_tok}↑ {out_tok}↓");
                }
                Ok(CommandResult::OpenSessionPicker) => {
                    println!(
                        "[headless 模式不支持会话选择器，请用 --resume <session-id> 恢复指定会话]"
                    );
                }
                Ok(CommandResult::ResumeSession(id)) => {
                    println!("[headless 模式：请用 wyj-code --resume {id} 恢复该会话]");
                }
                Ok(CommandResult::OpenSettingsDialog) => {
                    println!("[headless 模式不支持设置面板，请直接编辑 ~/.wyj-code/config.toml]");
                }
                Ok(CommandResult::OpenMemoryDialog) => {
                    println!(
                        "[headless 模式不支持 /memory 面板，请直接编辑 CLAUDE.md 或 ~/.wyj-code/memory/ 下的文件]"
                    );
                }
                Ok(CommandResult::OpenMcpDialog) => {
                    println!("{}", wyj_i18n::tr("mcp.headless_unsupported"));
                }
                Ok(CommandResult::OpenSkillsDialog) => {
                    println!("{}", wyj_i18n::tr("skills.headless_unsupported"));
                }
                Ok(CommandResult::OpenPluginsDialog) => {
                    println!("{}", wyj_i18n::tr("plugins.headless_unsupported"));
                }
                Ok(CommandResult::OpenExtensionsDialog) => {
                    match wyj_store::extensions::doctor(&cwd) {
                        Ok(report) => println!("{}", serde_json::to_string_pretty(&report)?),
                        Err(e) => eprintln!("[extensions] {e}"),
                    }
                }
                Ok(CommandResult::OpenImportDialog) => {
                    println!("{}", wyj_i18n::tr("import.headless_unsupported"));
                }
                Ok(CommandResult::OpenAgentsDialog { fallback_text, .. }) => {
                    println!("{fallback_text}");
                }
                Ok(CommandResult::OpenSubAgentsPanel(_)) => {
                    println!("{}", wyj_i18n::tr("subagents.headless_unsupported"));
                }
                Ok(CommandResult::OpenScheduleDialog) => {
                    println!("{}", wyj_i18n::tr("schedule.headless_unsupported"));
                }
                Ok(CommandResult::Quit) | Ok(CommandResult::None) => break,
                Err(e) => eprintln!("[命令错误] {e}"),
            }
            continue;
        }

        if trimmed == "/history" {
            if let Some(hs) = &history_store {
                match hs.recent(10) {
                    Ok(entries) => {
                        for e in &entries {
                            println!(
                                "[{}] {} tokens:{}+{}",
                                e.timestamp, e.session_id, e.input_tokens, e.output_tokens
                            );
                        }
                    }
                    Err(e) => eprintln!("读取历史失败: {e}"),
                }
            }
            continue;
        }

        session.push_user(trimmed);
        turns += 1;
        println!();
        let agent_snapshot = shared_agent.read().unwrap().clone();
        if let Err(e) = agent_snapshot
            .run_turn(&mut session, &ctx, &mut |d| {
                print!("{d}");
                let _ = io::stdout().flush();
            })
            .await
        {
            eprintln!("\n[错误] {e}");
        }
        println!();
        // 升级版统计
        let in_tok = session.total_input_tokens;
        let out_tok = session.total_output_tokens;
        eprintln!("  tokens: {in_tok}↑ {out_tok}↓");
    }
    if let Some(hs) = &history_store {
        let _ = hs.append(&HistoryEntry {
            timestamp: now_iso(),
            session_id: session_id.clone(),
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            turns,
            cwd: cwd.display().to_string(),
        });
    }
    // 退出时保存 SessionFile，使 REPL 会话可通过 --resume 恢复
    if let Some(store) = &session_store {
        let _ = store.save(&SessionFile {
            session_id,
            title: extract_title(&session.messages),
            last_preview: extract_preview(&session.messages),
            cwd: cwd.display().to_string(),
            timestamp: now_iso(),
            turns,
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            messages: session.messages.clone(),
            routing_events: session.routing_events.clone(),
            current_checkpoint_id: session.current_checkpoint_id.clone(),
            branch_parent_session_id: session.branch_parent_session_id.clone(),
            branch_parent_checkpoint_id: session.branch_parent_checkpoint_id.clone(),
            title_generated: false,
        });
    }
    println!("再见！");
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn config_status_flag_parses() {
        let cli = Cli::try_parse_from(["wyj-code", "--config-status"]).unwrap();
        assert!(cli.config_status);
        assert!(!cli.plan);
        assert!(!cli.bypass_permissions);
        assert!(!cli.no_hooks);
    }

    #[test]
    fn no_hooks_flag_parses() {
        let cli = Cli::try_parse_from(["wyj-code", "--no-hooks"]).unwrap();
        assert!(cli.no_hooks);
    }

    #[test]
    fn plan_flag_parses() {
        let cli = Cli::try_parse_from(["wyj-code", "--plan"]).unwrap();
        assert!(cli.plan);
        assert!(!cli.bypass_permissions);
    }

    #[test]
    fn bypass_permissions_flag_parses() {
        let cli = Cli::try_parse_from(["wyj-code", "--bypass-permissions"]).unwrap();
        assert!(cli.bypass_permissions);
        assert!(!cli.plan);
    }

    #[test]
    fn plan_and_bypass_are_mutually_settable_but_not_exclusive_at_parse_time() {
        // clap 本身不互斥这两个 flag（互斥语义由运行时逻辑决定，不在 Cli 定义里
        // 用 conflicts_with），这里只验证解析层面两者可以同时置位，不代表运行时
        // 行为——冒烟测试覆盖的是"没有 panic、字段值符合预期"。
        let cli = Cli::try_parse_from(["wyj-code", "--plan", "--bypass-permissions"]).unwrap();
        assert!(cli.plan);
        assert!(cli.bypass_permissions);
    }

    #[test]
    fn no_flags_default_to_false() {
        let cli = Cli::try_parse_from(["wyj-code"]).unwrap();
        assert!(!cli.config_status);
        assert!(!cli.plan);
        assert!(!cli.bypass_permissions);
        assert!(!cli.no_hooks);
        assert!(cli.prompt.is_none());
        assert!(cli.resume.is_none());
    }

    #[test]
    fn prompt_and_resume_capture_their_values() {
        let cli = Cli::try_parse_from(["wyj-code", "-p", "hello", "--resume", "abc123"]).unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("hello"));
        assert_eq!(cli.resume.as_deref(), Some("abc123"));
    }

    #[test]
    fn update_subcommand_parses_with_yes_flag() {
        let cli = Cli::try_parse_from(["wyj-code", "update", "-y"]).unwrap();
        match cli.command {
            Some(Commands::Update { yes }) => assert!(yes),
            other => panic!("expected Update subcommand, got {other:?}"),
        }
    }

    #[test]
    fn subagent_trace_subcommand_parses_session_id_and_optional_sub_id() {
        let cli = Cli::try_parse_from(["wyj-code", "subagent-trace", "sess-123"]).unwrap();
        match cli.command {
            Some(Commands::SubagentTrace {
                session_id,
                sub_id,
                json,
            }) => {
                assert_eq!(session_id, "sess-123");
                assert_eq!(sub_id, None);
                assert!(!json);
            }
            other => panic!("expected SubagentTrace subcommand, got {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["wyj-code", "subagent-trace", "sess-123", "3", "--json"]).unwrap();
        match cli.command {
            Some(Commands::SubagentTrace {
                session_id,
                sub_id,
                json,
            }) => {
                assert_eq!(session_id, "sess-123");
                assert_eq!(sub_id, Some(3));
                assert!(json);
            }
            other => panic!("expected SubagentTrace subcommand, got {other:?}"),
        }
    }

    #[test]
    fn model_doctor_and_sandbox_subcommands_parse_without_live_probe() {
        let cli = Cli::try_parse_from([
            "wyj-code",
            "model",
            "doctor",
            "minimax",
            "--json",
            "--refresh",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Model {
                command:
                    ModelCommand::Doctor {
                        profile,
                        json,
                        probe,
                        refresh,
                    },
            }) => {
                assert_eq!(profile.as_deref(), Some("minimax"));
                assert!(json);
                assert!(probe.is_none());
                assert!(refresh);
            }
            other => panic!("expected model doctor, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["wyj-code", "sandbox", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Sandbox { json: true })
        ));
    }

    // ── select_sub_agent_tools：子 Agent 是否能拿到 WebSearch/MCP 工具 ──────

    struct FakeMcpTool(&'static str);

    #[async_trait::async_trait]
    impl wyj_tools::Tool for FakeMcpTool {
        fn name(&self) -> &str {
            self.0
        }
        fn definition(&self) -> wyj_api::types::ToolDefinition {
            wyj_api::types::ToolDefinition {
                name: self.0.to_string(),
                description: "fake mcp tool for tests".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                native: None,
            }
        }
        async fn run(
            &self,
            _input: serde_json::Value,
            _ctx: &dyn wyj_core::tool::ToolContext,
        ) -> Result<wyj_tools::ToolResult> {
            Ok(wyj_tools::ToolResult::ok(String::new()))
        }
    }

    fn registry_with_mcp_and_websearch() -> ToolRegistry {
        let mut r = ToolRegistry::standard();
        r.register_arc(Arc::new(wyj_tools::WebSearchTool::new("dummy-key")));
        r.register_arc(Arc::new(FakeMcpTool("mcp_echo")));
        r
    }

    #[test]
    fn general_purpose_gets_websearch_and_mcp() {
        let defs = wyj_core::builtin_defs();
        let general = defs.iter().find(|d| d.name == "general-purpose").unwrap();
        let registry = registry_with_mcp_and_websearch();
        let names: Vec<String> = select_sub_agent_tools(general, &registry)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "WebSearch"));
        assert!(names.iter().any(|n| n == "mcp_echo"));
    }

    #[test]
    fn explore_excludes_websearch_and_mcp() {
        let defs = wyj_core::builtin_defs();
        let explore = defs.iter().find(|d| d.name == "Explore").unwrap();
        let registry = registry_with_mcp_and_websearch();
        let names: Vec<String> = select_sub_agent_tools(explore, &registry)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == "WebSearch"));
        assert!(!names.iter().any(|n| n == "mcp_echo"));
        let expected: std::collections::HashSet<&str> = wyj_core::agent_def::READONLY_TOOLS
            .iter()
            .copied()
            .collect();
        let actual: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn custom_whitelist_matches_mcp_tool_by_exact_name() {
        let def = wyj_core::AgentDefinition {
            name: "custom".to_string(),
            description: String::new(),
            tools: Some(vec!["Read".to_string(), "mcp_echo".to_string()]),
            model: None,
            system_prompt: String::new(),
            builtin: false,
            source: None,
        };
        let registry = registry_with_mcp_and_websearch();
        let names: Vec<String> = select_sub_agent_tools(&def, &registry)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "mcp_echo"));
        assert!(!names.iter().any(|n| n == "WebSearch"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn selected_tools_never_include_agent_or_todo_or_ask_question() {
        // 防嵌套回归守护：即便 registry 里混入了 WebSearch/MCP，select_sub_agent_tools
        // 也不该产出 Agent/TodoWrite/AskQuestion/ExitPlanMode —— 这些工具本就
        // 从不进入子 Agent 用的 sub_registry（standard() 不含它们）。
        let defs = wyj_core::builtin_defs();
        let general = defs.iter().find(|d| d.name == "general-purpose").unwrap();
        let registry = registry_with_mcp_and_websearch();
        let names: Vec<String> = select_sub_agent_tools(general, &registry)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        for forbidden in ["Agent", "TodoWrite", "AskQuestion", "ExitPlanMode"] {
            assert!(!names.iter().any(|n| n == forbidden));
        }
    }
}
