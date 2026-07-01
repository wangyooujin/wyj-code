use anyhow::Result;
use clap::Parser;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::{
    extract_preview, extract_title, new_session_id, now_iso, Agent, HistoryEntry, HistoryStore,
    MemoryStore, Session, SessionFile, SessionStore, ToolEvent,
};
use wyj_tools::{
    AskQuestionTool, PermissionMode, SubAgentTool, TodoStore, TodoWriteTool, ToolCtx, ToolRegistry,
};

#[derive(Parser, Debug)]
#[command(name = "wyj-code", version = env!("CARGO_PKG_VERSION"),
          about = wyj_i18n::tr("cli.about"))]
struct Cli {
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
}

#[tokio::main]
async fn main() -> Result<()> {
    // 先加载 config 拿 language 字段并 set_locale，确保 Cli::parse() 生成的
    // --help 文本、以及后续所有输出都使用正确的语言。
    let cfg = Config::load()?;
    let lang = cfg
        .language
        .clone()
        .unwrap_or_else(|| wyj_i18n::detect_system_locale().to_string());
    wyj_i18n::set_locale(&lang);

    let cli = Cli::parse();

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if cli.config_status {
        println!(
            "{}",
            wyj_i18n::tr_fmt(
                "status.provider",
                &[("provider", &cfg.provider.to_string())]
            )
        );
        println!(
            "{}",
            wyj_i18n::tr_fmt("status.model", &[("model", &cfg.model)])
        );
        if let Some(m) = &cfg.plan_model {
            println!("{}", wyj_i18n::tr_fmt("status.plan_model", &[("model", m)]));
        }
        if let Some(m) = &cfg.exec_model {
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
        println!(
            "{}",
            wyj_i18n::tr_fmt(
                "status.mcp_servers",
                &[("count", &cfg.mcp_servers.len().to_string())]
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
            let last = session_store.as_ref().and_then(|s| s.last().ok().flatten());
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
        .map(Arc::new)
        .map_err(|e| tracing::warn!("记忆存储初始化失败: {e}"))
        .ok();

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

    let cfg_clone = cfg.clone();
    let provider = wyj_api::build_provider_with_model(&cfg, &model_name)?;

    // 始终注册全部工具（模式过滤在运行时由 ToolCtx.permission_mode 负责，支持运行时切换）
    let mut registry = ToolRegistry::standard();

    // 初始工具上下文权限（headless/single-shot 模式用；TUI 模式在 spawn 闭包内动态创建）
    let mut tool_ctx = ToolCtx::new(&cwd);
    tool_ctx.permission_mode = match &mode {
        AgentMode::Plan => {
            let set: std::collections::HashSet<String> = [
                "Read",
                "Glob",
                "Grep",
                "WebFetch",
                "AskQuestion",
                "Write",
                "Bash",
                "ExitPlanMode",
                "TodoWrite",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            PermissionMode::Allowlist(set)
        }
        AgentMode::Bypass => PermissionMode::AutoApprove,
        AgentMode::Normal => PermissionMode::Prompt,
    };

    let todo_store = Arc::new(Mutex::new(TodoStore::default()));
    registry.register_arc(Arc::new(TodoWriteTool::new(todo_store.clone())));
    registry.register_arc(Arc::new(AskQuestionTool::new()));

    registry.register_arc(Arc::new(SubAgentTool::new(move || {
        let sub_model = cfg_clone.model_for_mode(&AgentMode::Normal).to_string();
        let sub_provider = wyj_api::build_provider_with_model(&cfg_clone, &sub_model)
            .expect("子 Agent 创建 provider 失败");
        let sub_registry = ToolRegistry::standard();
        let mut sub_agent = Agent::new(sub_provider).with_max_tokens(cfg_clone.max_tokens);
        for def in sub_registry.definitions() {
            if let Some(t) = sub_registry.get(&def.name) {
                sub_agent.register_tool(t);
            }
        }
        sub_agent
    })));

    for mcp_cfg in &cfg.mcp_servers {
        match wyj_mcp::bridge::connect_mcp_server(mcp_cfg).await {
            Ok(tools) => {
                let count = tools.len();
                for tool in tools {
                    registry.register_arc(Arc::new(tool));
                }
                tracing::info!("MCP [{}] 连接成功，注册 {} 个工具", mcp_cfg.name, count);
            }
            Err(e) => tracing::warn!("MCP [{}] 连接失败: {e}", mcp_cfg.name),
        }
    }

    let mut agent = Agent::new(provider)
        .with_max_tokens(cfg.max_tokens)
        .with_context_window(cfg.context_window);

    // system_prompt_extra 记录 append_system() 追加的内容（原样，含前导 "\n\n"），
    // 供 TUI 侧在运行时切换语言、需要用新语言重建 system prompt 时，能在新的
    // default 提示词后原样拼回这些追加内容（WYJ.md 说明、Plan 模式限制等），
    // 避免语言切换把这些内容冲掉。
    let mut system_prompt_extra = String::new();

    // Plan 模式在系统提示中说明只读约束
    if matches!(mode, AgentMode::Plan) {
        let extra = wyj_i18n::tr("system_prompt.plan_mode");
        agent = agent.append_system(extra.clone());
        system_prompt_extra.push_str("\n\n");
        system_prompt_extra.push_str(&extra);
    }

    let wyj_md = cwd.join("WYJ.md");
    if wyj_md.exists() {
        if let Ok(content) = std::fs::read_to_string(&wyj_md) {
            if !content.trim().is_empty() {
                let extra =
                    wyj_i18n::tr_fmt("system_prompt.wyjmd_header", &[("content", &content)]);
                agent = agent.append_system(extra.clone());
                system_prompt_extra.push_str("\n\n");
                system_prompt_extra.push_str(&extra);
                tracing::debug!("已加载 WYJ.md ({} 字节)", content.len());
            }
        }
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
        agent.with_tool_callback(|event| match event {
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

    let context_window = cfg.context_window;

    if let Some(prompt) = cli.prompt {
        let mut session = Session::new();
        session.messages = initial_messages;
        session.push_user(prompt);
        let turns = session.messages.len();
        agent
            .run_turn(&mut session, &tool_ctx, &mut |d| {
                print!("{d}");
                let _ = io::stdout().flush();
            })
            .await?;
        println!();
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
            });
        }
    } else if cli.headless {
        repl(
            agent,
            tool_ctx,
            history_store,
            session_id,
            cwd,
            initial_messages,
        )
        .await?;
    } else {
        let todo_store_for_rebuild = todo_store.clone();
        let rebuild_fn: wyj_tui::RebuildFn = Arc::new(move |cfg: &Config, new_model: &str| {
            let provider = wyj_api::build_provider_with_model(cfg, new_model)?;
            let mut new_agent = Agent::new(provider)
                .with_max_tokens(cfg.max_tokens)
                .with_context_window(cfg.context_window);
            let mut reg = ToolRegistry::standard();
            reg.register_arc(Arc::new(TodoWriteTool::new(todo_store_for_rebuild.clone())));
            reg.register_arc(Arc::new(AskQuestionTool::new()));
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
        )
        .await?;
    }
    Ok(())
}

async fn repl(
    agent: Agent,
    ctx: ToolCtx,
    history_store: Option<HistoryStore>,
    session_id: String,
    cwd: std::path::PathBuf,
    initial_messages: Vec<wyj_api::types::Message>,
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
    let cmd_registry = standard_registry_with_skills(&repl_home, &cwd);

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
        let cmd_ctx = CommandContext {
            cwd: cwd.clone(),
            model: "".to_string(),
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            context_window: 200_000,
            estimated_tokens: wyj_core::estimate_tokens(&session.messages),
            home_dir,
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
                Ok(CommandResult::SetModel(m)) => println!("模型已切换: {m}（重启生效）"),
                Ok(CommandResult::RunPrompt(prompt)) => {
                    // Skill 展开后的 prompt → 当作用户消息发给 agent
                    session.push_user(prompt);
                    turns += 1;
                    println!();
                    if let Err(e) = agent
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
        if let Err(e) = agent
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
            session_id,
            input_tokens: session.total_input_tokens,
            output_tokens: session.total_output_tokens,
            turns,
            cwd: cwd.display().to_string(),
        });
    }
    println!("再见！");
    Ok(())
}
