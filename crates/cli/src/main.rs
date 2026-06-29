use anyhow::Result;
use clap::Parser;
use std::io::{self, Write};
use tracing_subscriber::EnvFilter;
use wyj_config::Config;
use wyj_core::{Agent, Session};

#[derive(Parser, Debug)]
#[command(
    name = "wyj-code",
    version = env!("CARGO_PKG_VERSION"),
    about = "wyj-code — 终端 AI 编程助手"
)]
struct Cli {
    /// 打印配置状态并退出
    #[arg(long)]
    config_status: bool,

    /// 单次 headless 问答（不进入交互模式）
    #[arg(short = 'p', long)]
    prompt: Option<String>,
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

    let provider = wyj_api::build_provider(&cfg)?;
    let agent = Agent::new(provider).with_max_tokens(cfg.max_tokens);

    if let Some(prompt) = cli.prompt {
        // 单次 headless 问答
        let mut session = Session::new();
        session.push_user(prompt);
        agent
            .run_turn(&mut session, &mut |delta| {
                print!("{delta}");
                let _ = io::stdout().flush();
            })
            .await?;
        println!();
        eprintln!("\n{}", session.cost_summary());
    } else {
        // 交互式 headless REPL（M3 之前的简易版本）
        repl(agent).await?;
    }

    Ok(())
}

async fn repl(agent: Agent) -> Result<()> {
    use std::io::BufRead;

    println!(
        "wyj-code v{} — 输入问题，回车发送。Ctrl-C 或 Ctrl-D 退出。",
        env!("CARGO_PKG_VERSION")
    );

    let mut session = Session::new();
    let stdin = io::stdin();

    loop {
        print!("\n> ");
        io::stdout().flush()?;

        let mut input = String::new();
        match stdin.lock().read_line(&mut input) {
            Ok(0) => break, // EOF
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
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }

        session.push_user(trimmed);
        println!();

        if let Err(e) = agent
            .run_turn(&mut session, &mut |delta| {
                print!("{delta}");
                let _ = io::stdout().flush();
            })
            .await
        {
            eprintln!("\n[错误] {e}");
        }
        println!();
        eprintln!("{}", session.cost_summary());
    }

    println!("再见！");
    Ok(())
}
