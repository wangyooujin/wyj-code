use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use wyj_config::Config;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    // 初始化日志（依据配置中的 log_level）
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

    println!("wyj-code v{}", env!("CARGO_PKG_VERSION"));
    println!("供应商: {}  模型: {}", cfg.provider, cfg.model);
    println!("（TUI 将在 M3 实现，当前为骨架阶段）");

    Ok(())
}
