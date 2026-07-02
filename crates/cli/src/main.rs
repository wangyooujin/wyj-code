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
    #[arg(long, help = wyj_i18n::tr("cli.profile_help"))]
    profile: Option<String>,
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

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

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
                "Agent",
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

    // agent 类型定义：内置三类型 + ~/.claude/agents 与项目 .claude/agents 的自定义定义
    let agent_defs = Arc::new(wyj_core::load_agent_defs(&cwd));
    let sub_agent_hub = Arc::new(wyj_tools::SubAgentHub::new());
    let sub_agent_factory = make_sub_agent_factory(cfg.clone(), claude_md_loader.clone());
    registry.register_arc(Arc::new(SubAgentTool::new(
        agent_defs.clone(),
        sub_agent_hub.clone(),
        {
            let f = sub_agent_factory.clone();
            move |def| f(def)
        },
    )));

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
        .with_max_tokens(cfg.active_profile().max_tokens)
        .with_context_window(cfg.active_profile().context_window);

    // system_prompt_extra 记录 append_system() 追加的内容（原样，含前导 "\n\n"），
    // 供 TUI 侧在运行时切换语言、需要用新语言重建 system prompt 时，能在新的
    // default 提示词后原样拼回这些追加内容（目前仅 Plan 模式限制说明；CLAUDE.md
    // 系文件不再焊死进 system prompt，而是每轮重新读盘注入对话历史，见 with_claude_md）。
    let mut system_prompt_extra = String::new();

    // Plan 模式在系统提示中说明只读约束
    if matches!(mode, AgentMode::Plan) {
        let extra = wyj_i18n::tr("system_prompt.plan_mode");
        agent = agent.append_system(extra.clone());
        system_prompt_extra.push_str("\n\n");
        system_prompt_extra.push_str(&extra);
    }

    // headless/单次问答模式没有 UI 可交互，AskQuestion 会被自动取消：
    // 告知模型不要调用该工具，直接给出假设并继续（不写入 system_prompt_extra，
    // 因为该变量只服务于 TUI 运行时重建 Agent，headless/-p 路径不会触发重建）。
    if cli.headless || cli.prompt.is_some() {
        let extra = wyj_i18n::tr("system_prompt.non_interactive");
        agent = agent.append_system(extra);
    }

    agent = agent.with_claude_md(claude_md_loader.clone());

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

    let context_window = cfg.active_profile().context_window;

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
        // 结束前等待全部后台子 Agent 完成（结果由 Done 事件回调打印）
        let bg_count = sub_agent_hub.background_count();
        if bg_count > 0 {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("subagent.waiting_bg", &[("count", &bg_count.to_string())])
            );
        }
        sub_agent_hub.wait_background().await;
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
        let agent_defs_for_rebuild = agent_defs.clone();
        let hub_for_rebuild = sub_agent_hub.clone();
        let rebuild_fn: wyj_tui::RebuildFn = Arc::new(move |cfg: &Config, new_model: &str| {
            let provider = wyj_api::build_provider_with_model(cfg, new_model)?;
            let mut new_agent = Agent::new(provider)
                .with_max_tokens(cfg.active_profile().max_tokens)
                .with_context_window(cfg.active_profile().context_window)
                .with_claude_md(claude_md_for_rebuild.clone());
            if let Some(mem) = &memory_store_for_rebuild {
                new_agent = new_agent.with_memory(mem.clone());
            }
            let mut reg = ToolRegistry::standard();
            reg.register_arc(Arc::new(TodoWriteTool::new(todo_store_for_rebuild.clone())));
            reg.register_arc(Arc::new(AskQuestionTool::new()));
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
            sub_input_tokens: 0,
            sub_output_tokens: 0,
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
                Ok(CommandResult::OpenMemoryDialog) => {
                    println!(
                        "[headless 模式不支持 /memory 面板，请直接编辑 CLAUDE.md 或 ~/.wyj-code/memory/ 下的文件]"
                    );
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
