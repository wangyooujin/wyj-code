use anyhow::Result;
use clap::Parser;
use std::io::{self, Write};
use tracing_subscriber::EnvFilter;
use std::sync::{Arc, Mutex};
use wyj_config::Config;
use wyj_core::{Agent, Session};
use wyj_tools::{ToolCtx, ToolRegistry, TodoStore, TodoWriteTool, SubAgentTool};

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if cli.config_status {
        println!("供应商:  {}", cfg.provider);
        println!("模型:    {}", cfg.model);
        println!("端点:    {}", cfg.resolved_base_url());
        match cfg.api_key() {
            Ok(k) => println!("API Key: {}...（已配置）", &k[..k.len().min(8)]),
            Err(e) => println!("API Key: {}", e),
        }
        return Ok(());
    }

    let cwd = cli.cwd.unwrap_or_else(|| std::env::current_dir().unwrap());
    let cfg_clone = cfg.clone();
    let provider = wyj_api::build_provider(&cfg)?;
    let mut registry = ToolRegistry::standard();
    let tool_ctx = ToolCtx::new(&cwd);

    // 注册 TodoWrite（共享 store）
    let todo_store = Arc::new(Mutex::new(TodoStore::default()));
    registry.register_arc(Arc::new(TodoWriteTool::new(todo_store)));

    // 注册 SubAgent（工厂函数创建子 agent，含全套工具）
    registry.register_arc(Arc::new(SubAgentTool::new(move || {
        let sub_provider = wyj_api::build_provider(&cfg_clone)
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

    let mut agent = Agent::new(provider).with_max_tokens(cfg.max_tokens);
    for def in registry.definitions() {
        let name = def.name.clone();
        if let Some(t) = registry.get(&name) {
            agent.register_tool(t);
        }
    }

    if let Some(prompt) = cli.prompt {
        let mut session = Session::new();
        session.push_user(prompt);
        agent.run_turn(&mut session, &tool_ctx, &mut |d| { print!("{d}"); let _ = io::stdout().flush(); }).await?;
        println!();
        eprintln!("\n{}", session.cost_summary());
    } else if cli.headless {
        repl(agent, tool_ctx).await?;
    } else {
        wyj_tui::run_tui(agent, tool_ctx, cwd).await?;
    }
    Ok(())
}

async fn repl(agent: Agent, ctx: ToolCtx) -> Result<()> {
    use std::io::BufRead;
    println!("wyj-code v{} headless — 输入问题回车发送，Ctrl-D 退出", env!("CARGO_PKG_VERSION"));
    let mut session = Session::new();
    let stdin = io::stdin();
    loop {
        print!("\n> ");
        io::stdout().flush()?;
        let mut input = String::new();
        match stdin.lock().read_line(&mut input) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => { eprintln!("读取失败: {e}"); break; }
        }
        let trimmed = input.trim();
        if trimmed.is_empty() { continue; }
        if matches!(trimmed, "/exit" | "/quit") { break; }
        session.push_user(trimmed);
        println!();
        if let Err(e) = agent.run_turn(&mut session, &ctx, &mut |d| { print!("{d}"); let _ = io::stdout().flush(); }).await {
            eprintln!("\n[错误] {e}");
        }
        println!();
        eprintln!("{}", session.cost_summary());
    }
    println!("再见！");
    Ok(())
}
