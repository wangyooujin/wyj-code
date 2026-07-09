use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::{
    extract_preview, extract_title, new_session_id, now_iso, Agent, HistoryEntry, HistoryStore,
    HookRunner, MemoryStore, Session, SessionFile, SessionStore, SummaryGenerator, ToolEvent,
};
use wyj_tools::{
    AskQuestionTool, PermissionMode, SubAgentTool, TodoStore, TodoWriteTool, ToolCtx, ToolRegistry,
};

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
            TE::ToolEnd { .. } => {}
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

    if let Some(cmd) = cli.command.take() {
        match cmd {
            Commands::Update { yes } => return update_cmd::run(yes).await,
            Commands::SubagentTrace {
                session_id,
                sub_id,
                json,
            } => return run_subagent_trace_cmd(&session_id, sub_id, json),
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
            Ok(k) => println!(
                "{}",
                wyj_i18n::tr_fmt(
                    "status.api_key_configured",
                    &[("prefix", &k[..k.len().min(8)])]
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

    // 按模式选择模型
    let model_name = cfg.model_for_mode(&mode).to_string();

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
    tool_ctx.set_permission_mode(match &mode {
        AgentMode::Plan => {
            let set: std::collections::HashSet<String> = [
                "Read",
                "Glob",
                "Grep",
                "WebFetch",
                "WebSearch",
                "AskQuestion",
                "Write",
                "Bash",
                "BashOutput",
                "ExitPlanMode",
                "TodoWrite",
                "Agent",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            PermissionMode::Allowlist(set)
        }
        AgentMode::Bypass => PermissionMode::AutoApprove,
        AgentMode::Normal => PermissionMode::Prompt,
    });

    let todo_store = Arc::new(Mutex::new(TodoStore::default()));
    registry.register_arc(Arc::new(TodoWriteTool::new(todo_store.clone())));
    registry.register_arc(Arc::new(AskQuestionTool::new()));

    // WebSearch：仅当配置了搜索 API Key 时注册（否则模型看不到该工具，避免误调）
    if let Some(key) = cfg.search_api_key.as_deref().filter(|k| !k.is_empty()) {
        registry.register_arc(Arc::new(wyj_tools::WebSearchTool::new(key)));
    }

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
    let sub_agent_factory = make_sub_agent_factory(cfg.clone(), claude_md_loader.clone());
    registry.register_arc(Arc::new(SubAgentTool::new(
        agent_defs.clone(),
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
        let mut effective_mcp_servers = wyj_store::mcp_install::effective_mcp_servers(&cfg, &cwd);
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
                                registry.register_arc(Arc::new(tool));
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
    let mut agent = Agent::new(provider)
        .with_system(wyj_core::prompts::main_system_prompt(&env_info))
        .with_git_snapshot(wyj_core::prompts::git_status_snapshot(&cwd))
        .with_max_tokens(cfg.active_profile().max_tokens)
        .with_context_window(cfg.active_profile().context_window)
        .with_thinking(
            cfg.active_profile().thinking_budget,
            cfg.active_profile().interleaved_thinking,
        );

    // system_prompt_extra 记录 append_system() 追加的内容（原样，含前导 "\n\n"），
    // 供 TUI 侧重建 Agent 时在默认提示词后原样拼回这些追加内容（目前仅 Plan 模式
    // 限制说明；CLAUDE.md 系文件不焊死进 system prompt，见 with_claude_md）。
    let mut system_prompt_extra = String::new();

    // Plan 模式在系统提示中说明只读约束
    if matches!(mode, AgentMode::Plan) {
        let extra = wyj_core::prompts::PLAN_MODE;
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

    if let Some(mem) = memory_store {
        agent = agent.with_memory(mem);
    }

    for def in registry.definitions() {
        let name = def.name.clone();
        if let Some(t) = registry.get(&name) {
            agent.register_tool(t);
        }
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

    if let Some(prompt) = cli.prompt {
        let mut session = Session::new();
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
            eprintln!();
            eprintln!(
                "{{\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"api_calls\":{},\"duration_secs\":{:.1}}}",
                session.total_input_tokens,
                session.total_output_tokens,
                session.total_cache_read_tokens,
                session.total_cache_write_tokens,
                session.api_calls,
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
                title_generated: false,
            });
        }
    } else if cli.headless {
        repl(
            agent,
            tool_ctx,
            history_store,
            session_store_arc.clone(),
            session_id,
            cwd,
            initial_messages,
            cfg.clone(),
            local_plugin.clone(),
            !cli.no_hooks,
        )
        .await?;
    } else {
        let todo_store_for_rebuild = todo_store.clone();
        let agent_defs_for_rebuild = agent_defs.clone();
        let hub_for_rebuild = sub_agent_hub.clone();
        let store_for_rebuild = session_store_arc.clone();
        let sid_for_rebuild = session_id.clone();
        let cwd_for_rebuild = cwd.clone();
        let hook_runner_for_rebuild = hook_runner.clone();
        let rebuild_fn: wyj_tui::RebuildFn = Arc::new(move |cfg: &Config, new_model: &str| {
            let provider = wyj_api::build_provider_with_model(cfg, new_model)?;
            let env_info = wyj_core::prompts::EnvInfo::collect(&cwd_for_rebuild, new_model);
            let mut new_agent = Agent::new(provider)
                .with_system(wyj_core::prompts::main_system_prompt(&env_info))
                .with_git_snapshot(wyj_core::prompts::git_status_snapshot(&cwd_for_rebuild))
                .with_max_tokens(cfg.active_profile().max_tokens)
                .with_context_window(cfg.active_profile().context_window)
                .with_thinking(
                    cfg.active_profile().thinking_budget,
                    cfg.active_profile().interleaved_thinking,
                )
                .with_claude_md(claude_md_for_rebuild.clone())
                .with_hooks(hook_runner_for_rebuild.clone());
            if let Some(mem) = &memory_store_for_rebuild {
                new_agent = new_agent.with_memory(mem.clone());
            }
            // 重建后保留会话标题生成能力
            if let Some(store) = &store_for_rebuild {
                let title_provider =
                    wyj_api::build_provider_from_profile(cfg.active_profile(), None)
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
            // 用重建时的最新配置构建子 Agent 工厂，保证 Profile 变更即时生效
            let sub_factory = make_sub_agent_factory(cfg.clone(), claude_md_for_rebuild.clone());
            reg.register_arc(Arc::new(SubAgentTool::new(
                agent_defs_for_rebuild.clone(),
                hub_for_rebuild.clone(),
                move |def| sub_factory(def),
            )));
            for def in reg.definitions() {
                if let Some(t) = reg.get(&def.name) {
                    new_agent.register_tool(t);
                }
            }
            Ok(new_agent)
        });
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
        )
        .await?;
    }
    Ok(())
}

/// 构建子 Agent 工厂：按 agent 定义解析 Profile 与模型，注册按定义过滤后的工具集。
/// 模型解析优先级：定义的 model 字段（Profile 名）→ [subagent].explore_profile（仅
/// Explore 类型）→ [subagent].default_profile → 主 Agent 当前激活分组的 Normal 模型。
fn make_sub_agent_factory(
    cfg: Config,
    claude_md: Arc<wyj_core::ClaudeMdLoader>,
) -> wyj_tools::AgentFactory {
    Arc::new(move |def: &wyj_core::AgentDefinition| {
        let mut profile = None;
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
            Some(p) => (p, p.model.clone()),
            None => (
                cfg.active_profile(),
                cfg.model_for_mode(&AgentMode::Normal).to_string(),
            ),
        };
        let provider = wyj_api::build_provider_from_profile(p, Some(&model))?;

        let mut sub_agent = Agent::new(provider)
            .with_max_tokens(p.max_tokens)
            .with_context_window(p.context_window)
            .with_thinking(p.thinking_budget, p.interleaved_thinking)
            .with_claude_md(claude_md.clone());
        if !def.system_prompt.is_empty() {
            sub_agent = sub_agent.with_system(def.system_prompt.clone());
        }

        let sub_registry = ToolRegistry::standard();
        for tdef in sub_registry.definitions() {
            let allowed = def
                .tools
                .as_ref()
                .map_or(true, |list| list.iter().any(|n| n == &tdef.name));
            if allowed {
                if let Some(t) = sub_registry.get(&tdef.name) {
                    sub_agent.register_tool(t);
                }
            }
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

/// `--headless` REPL 每轮开始前非阻塞排空一次后台 MCP 连接结果（`try_recv`
/// 而非 `recv`，队列为空时立即返回，不拖慢当前轮次）。成功的连接克隆当前
/// Agent 快照、注册新工具、原子换回 `shared_agent`，与 TUI `tui_main` 里
/// `/model` 热切换同一套手法一致。
async fn drain_mcp_connections(
    mcp_rx: &mut tokio::sync::mpsc::Receiver<(
        String,
        Result<Vec<wyj_mcp::bridge::McpBridgeTool>, String>,
    )>,
    shared_agent: &Arc<std::sync::RwLock<Arc<Agent>>>,
) {
    while let Ok((name, result)) = mcp_rx.try_recv() {
        match result {
            Ok(tools) => {
                let count = tools.len();
                let mut new_agent = (**shared_agent.read().unwrap()).clone();
                for tool in tools {
                    new_agent.register_tool(Arc::new(tool));
                }
                *shared_agent.write().unwrap() = Arc::new(new_agent);
                println!("[MCP {name}] 已连接，{count} 个工具已就绪");
            }
            Err(e) => {
                eprintln!("[MCP {name}] 连接失败: {e}");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn repl(
    agent: Agent,
    ctx: ToolCtx,
    history_store: Option<HistoryStore>,
    session_store: Option<Arc<SessionStore>>,
    session_id: String,
    cwd: std::path::PathBuf,
    initial_messages: Vec<wyj_api::types::Message>,
    cfg: Config,
    local_plugin: Option<wyj_store::lockfile::PluginContributions>,
    hooks_enabled: bool,
) -> Result<()> {
    use std::io::BufRead;
    println!(
        "wyj-code v{} — 输入问题回车发送，/quit 退出，Ctrl-D 退出",
        env!("CARGO_PKG_VERSION")
    );
    let mut session = Session::new();
    session.messages = initial_messages;
    let stdin = io::stdin();
    let mut turns = 0usize;
    let repl_home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let disabled_skills = wyj_store::disabled_skill_names(&cwd);
    let mut plugin_skill_sources = wyj_store::plugin_install::enabled_plugin_skill_paths(&cwd);
    let mut plugin_agent_paths = wyj_store::plugin_install::enabled_plugin_agent_paths(&cwd);
    let mut effective_mcp_count = wyj_store::mcp_install::effective_mcp_servers(&cfg, &cwd).len();
    if let Some(local) = &local_plugin {
        plugin_skill_sources.extend(local.skill_paths.clone());
        plugin_agent_paths.extend(local.agent_paths.clone());
        effective_mcp_count += local.mcp_servers.len();
    }
    let cmd_registry =
        standard_registry_with_skills(&repl_home, &cwd, &disabled_skills, &plugin_skill_sources);

    // `--headless` 多轮 REPL 对称于 TUI 的 MCP 连接方案：不在启动时同步等待
    // （那是 -p 单轮模式的做法，见上方 main() 里的宽限期逻辑），而是后台连接、
    // 每轮 read_line 之后、run_turn 之前非阻塞排空一次结果，用克隆-注册-替换
    // 的手法动态挂载新工具。已知局限：`run_turn` 开始时对 Agent 的只读快照
    // 决定了"本轮开始后新连上的工具在这一轮不生效，下一轮才可见"——这是 TUI
    // 架构本来就有、已被接受的行为，此处对称搬过来，不重新设计。
    let shared_agent = Arc::new(std::sync::RwLock::new(Arc::new(agent)));
    let (mcp_tx, mut mcp_rx) = tokio::sync::mpsc::channel::<(
        String,
        Result<Vec<wyj_mcp::bridge::McpBridgeTool>, String>,
    )>(16);
    {
        let mut effective_mcp_servers = wyj_store::mcp_install::effective_mcp_servers(&cfg, &cwd);
        if let Some(local) = &local_plugin {
            effective_mcp_servers.extend(local.mcp_servers.clone());
        }
        for mcp_cfg in effective_mcp_servers {
            let tx = mcp_tx.clone();
            tokio::spawn(async move {
                let name = mcp_cfg.name.clone();
                let result = tokio::time::timeout(
                    wyj_mcp::bridge::MCP_CONNECT_TIMEOUT,
                    wyj_mcp::bridge::connect_mcp_server(&mcp_cfg),
                )
                .await;
                let mapped = match result {
                    Ok(Ok(tools)) => Ok(tools),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err(format!(
                        "连接超时（>{}s）",
                        wyj_mcp::bridge::MCP_CONNECT_TIMEOUT.as_secs()
                    )),
                };
                let _ = tx.send((name, mapped)).await;
            });
        }
    }

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
                    session = Session::new();
                    println!("对话已清空。");
                }
                Ok(CommandResult::CompactHistory) => {
                    println!("[headless 模式不支持 /compact]");
                }
                Ok(CommandResult::OpenProfileDialog) | Ok(CommandResult::SwitchProfile(_)) => {
                    println!("{}", wyj_i18n::tr("profile.headless_unsupported"));
                }
                Ok(CommandResult::RunPrompt(prompt)) => {
                    // Skill 展开后的 prompt → 当作用户消息发给 agent
                    session.push_user(prompt);
                    turns += 1;
                    println!();
                    drain_mcp_connections(&mut mcp_rx, &shared_agent).await;
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
                    profile: _,
                }) => {
                    // 带 allowed-tools 的自定义命令：临时把权限模式收紧为 Allowlist，
                    // 跑完这一轮（无论成功失败）都还原快照，不影响 --plan/--bypass-permissions
                    // 等其他模式设定的基线权限。`profile`（model: frontmatter 字段）本版本
                    // 仅解析存储、不做运行期切换，此处忽略。
                    session.push_user(text);
                    turns += 1;
                    println!();
                    drain_mcp_connections(&mut mcp_rx, &shared_agent).await;
                    let agent_snapshot = shared_agent.read().unwrap().clone();
                    let prev_mode = ctx.permission_mode.read().unwrap().clone();
                    if let Some(tools) = allowed_tools {
                        ctx.set_permission_mode(PermissionMode::Allowlist(
                            tools.into_iter().collect(),
                        ));
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
                Ok(CommandResult::OpenSubAgentsPanel(_)) => {
                    println!("{}", wyj_i18n::tr("subagents.headless_unsupported"));
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
        drain_mcp_connections(&mut mcp_rx, &shared_agent).await;
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
}
