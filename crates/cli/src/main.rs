use anyhow::Result;
use clap::Parser;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use wyj_commands::{standard_registry_with_skills, CommandContext, CommandResult};
use wyj_config::{AgentMode, Config};
use wyj_core::{
    new_session_id, now_iso, Agent, HistoryEntry, HistoryStore, MemoryStore, Session, ToolEvent,
};
use wyj_tools::{
    AskQuestionTool, PermissionMode, SubAgentTool, TodoStore, TodoWriteTool, ToolCtx, ToolRegistry,
};

#[derive(Parser, Debug)]
#[command(name = "wyj-code", version = env!("CARGO_PKG_VERSION"),
          about = "wyj-code — 终端 AI 编程助手")]
struct Cli {
    #[arg(long)]
    config_status: bool,
    /// 单次问答（不启动 TUI）
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// 工作目录（默认当前目录）
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,
    /// 强制使用 headless REPL 模式
    #[arg(long)]
    headless: bool,
    /// Plan 模式：仅启用只读工具（read/glob/grep/web_fetch），适合规划分析
    #[arg(long)]
    plan: bool,
    /// Bypass 模式：自动允许所有工具调用，不弹权限确认对话框
    #[arg(long)]
    bypass_permissions: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if cli.config_status {
        println!("供应商:  {}", cfg.provider);
        println!("模型:    {}", cfg.model);
        if let Some(m) = &cfg.plan_model {
            println!("Plan 模型: {m}");
        }
        if let Some(m) = &cfg.exec_model {
            println!("Exec 模型: {m}");
        }
        println!("端点:    {}", cfg.resolved_base_url());
        match cfg.api_key() {
            Ok(k) => println!("API Key: {}...（已配置）", &k[..k.len().min(8)]),
            Err(e) => println!("API Key: {}", e),
        }
        println!("MCP servers: {}", cfg.mcp_servers.len());
        return Ok(());
    }

    let cwd = cli.cwd.unwrap_or_else(|| std::env::current_dir().unwrap());
    let session_id = new_session_id();
    let config_base = wyj_config::config_dir()?;

    let history_store = HistoryStore::new(config_base.join("history")).ok();

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
            let set: std::collections::HashSet<String> =
                ["Read", "Glob", "Grep", "WebFetch", "AskQuestion"]
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

    // Plan 模式在系统提示中说明只读约束
    if matches!(mode, AgentMode::Plan) {
        agent = agent.append_system(
            "## 当前模式：Plan（规划分析）\n\n\
            你只能使用只读工具（read / glob / grep / web_fetch）。\n\
            不得写入、编辑文件或执行 shell 命令。\n\
            请专注于分析代码、规划方案、解释架构。",
        );
    }

    let wyj_md = cwd.join("WYJ.md");
    if wyj_md.exists() {
        if let Ok(content) = std::fs::read_to_string(&wyj_md) {
            if !content.trim().is_empty() {
                agent = agent.append_system(format!("## 当前项目说明 (WYJ.md)\n\n{content}"));
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
            AgentMode::Plan => " [plan 模式：仅只读工具]",
            AgentMode::Bypass => " [bypass 模式：跳过权限确认]",
            AgentMode::Normal => "",
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
        eprintln!("\n── 会话统计 ──");
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
    } else if cli.headless {
        repl(agent, tool_ctx, history_store, session_id, cwd).await?;
    } else {
        let cfg_for_rebuild = cfg.clone();
        let todo_store_for_rebuild = todo_store.clone();
        let rebuild_fn: wyj_tui::RebuildFn = Arc::new(move |new_model: &str| {
            let provider = wyj_api::build_provider_with_model(&cfg_for_rebuild, new_model)?;
            let mut new_agent = Agent::new(provider)
                .with_max_tokens(cfg_for_rebuild.max_tokens)
                .with_context_window(cfg_for_rebuild.context_window);
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
            session_id,
            model_name,
            context_window,
            mode,
            todo_store,
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
) -> Result<()> {
    use std::io::BufRead;
    println!(
        "wyj-code v{} — 输入问题回车发送，/quit 退出，Ctrl-D 退出",
        env!("CARGO_PKG_VERSION")
    );
    let mut session = Session::new();
    let stdin = io::stdin();
    let mut turns = 0usize;
    let repl_home = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default();
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
