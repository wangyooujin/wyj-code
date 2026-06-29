//! launcher:定位 claude、构造 Command、execvp 进程替换。

use crate::config::Config;
use crate::config::EnvMap;
use crate::errors::WyjError;
use anyhow::Result;
use std::process::Command;

/// 定位 claude 可执行文件。
/// 优先级:config.claude_path → $WYJ_CODE_CLAUDE → $PATH 上的 `claude`。
/// 绝不硬编码版本路径(claude 是符号链接,自更新会失效)。
pub fn resolve_claude(config: &Config) -> Result<String> {
    let candidates: Vec<String> = config
        .claude_path
        .clone()
        .into_iter()
        .chain(std::env::var("WYJ_CODE_CLAUDE").ok())
        .chain(Some("claude".to_string()))
        .collect();

    for c in candidates {
        if c.is_empty() {
            continue;
        }
        // 绝对/相对路径:直接判断可执行;裸名:在 $PATH 查找。
        if c.contains('/') {
            let p = std::path::Path::new(&c);
            if p.exists() {
                return Ok(c);
            }
        } else if which(&c).is_some() {
            return Ok(c);
        }
    }
    Err(WyjError::ClaudeNotFound.into())
}

/// 在 $PATH 中查找可执行文件(等价 which)。
fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let full = dir.join(name);
        if full.is_file() {
            return Some(full.to_string_lossy().into_owned());
        }
    }
    None
}

/// 注入 env 后 execvp 启动 claude,透传额外参数。
/// Unix:execvp 替换当前进程(成功不返回);非 Unix:退化为 spawn+wait。
pub fn exec_claude(claude: &str, env: &EnvMap, args: &[String]) -> Result<()> {
    // 鉴权缺失警告(不硬阻断,兼容 API_KEY/外部鉴权)。
    if env.get("ANTHROPIC_AUTH_TOKEN").map(|s| s.is_empty()).unwrap_or(true)
        && env.get("ANTHROPIC_API_KEY").map(|s| s.is_empty()).unwrap_or(true)
        && env.get("ANTHROPIC_BASE_URL").is_some()
    {
        eprintln!("⚠️  当前 profile 未设置 AUTH_TOKEN/API_KEY,可能无法通过鉴权。");
    }

    let mut cmd = Command::new(claude);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec(); // 成功不返回
        return Err(anyhow::anyhow!("启动 claude 失败: {}", err).context("exec 失败"));
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status().context("启动 claude 失败")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
