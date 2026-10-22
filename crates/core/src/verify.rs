//! 编辑后自动校验（verify loop）：把「改完代码让编译器说对不对」这件事从
//! 「模型自己记得跑」/「用户记得催」变成机制。
//!
//! ## 为什么需要它
//!
//! 复杂任务里模型最高频的失败模式不是不会写，而是**不知道自己写错了**：改完
//! 十个文件直接交付，没有编译器告诉它第五个文件的类型不匹配。Claude Code /
//! Codex 的差距不在提示词，而在这条默认回路——改代码 → 跑校验 → 失败作为
//! tool_result 回灌 → 模型自我修正。wyj-code 此前只有 hooks（用户自己配、默认
//! 空），也就是这条回路默认不存在。
//!
//! ## 为什么不做成硬编码 `cargo test`
//!
//! 本仓库就是 Rust，但 wyj-code 同时服务 Python / Node / Go 项目，且对同一
//! 项目**改十个文件跑十次全量测试**的代价是无法接受的。硬编码既跑错语言又贵。
//! 因此校验命令由项目自己在 `.wyj-code/settings.json` 里声明：
//!
//! ```json
//! { "verify": { "command": "cargo check --quiet --all-targets" } }
//! ```
//!
//! **未声明 = 完全静默**：不跑任何子进程、不产生任何 tool_result 字节、不改变
//! 任何行为。零假设是这个特性的第一原则——它不能给没配过的项目带来任何成本。
//!
//! ## 三源合并
//!
//! 与 `hooks::load_effective_hooks` 同一套来源与优先级（后者覆盖前者）：
//! `~/.wyj-code/settings.json` → `<git-root>/.wyj-code/settings.json` →
//! `<git-root>/.wyj-code/settings.local.json`。区别是 hooks 按事件**追加**，
//! verify 是单值**覆盖**——同一个项目同时只能有一个校验命令，多声明了后写的赢。
//!
//! 每次 `load_verify_cfg` 都重新读盘（与 `ClaudeMdLoader` 同一哲学：不缓存，
//! 保证运行期间改 settings 立即生效）。

use std::path::{Path, PathBuf};
use std::time::Instant;

/// 会触发校验的工具。这些都是「改动了工作区文件」的工具——模型改完之后代码处于
/// 一个可被编译/测试否定的状态，才是校验有意义的那一刻。
///
/// 注意**不含 Bash**：`rm -rf` 之后跑一次 cargo check 只会浪费 2 秒并报一堆
/// 无关错误；而 Bash 又能自己改代码，触发条件变得不可预测。保持一个清晰、可
/// 枚举的触发面，比覆盖率更重要。
pub const VERIFY_TRIGGER_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

/// 默认超时。`cargo check` 冷启动（含依赖编译）很容易超过 60s，120s 是在
/// 「用户还愿意等」和「卡住的校验不会永久挂起」之间的折中。
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// 回灌给模型的输出上限（字节）。编译器错误开头是 "error[E0308]"、结尾是
/// "error: could not compile"，中间是几百行引用上下文——模型真正需要的是首尾，
/// 中间那些 `   --> src/foo.rs:42:18` 定位行是噪声。两端各留一半。
const DEFAULT_MAX_OUTPUT_BYTES: usize = 4_000;

/// settings.json 里的 `verify` 块。
#[derive(Debug, Clone)]
pub struct VerifyCfg {
    /// 校验命令，经 `sh -c`（Windows `cmd /C`）执行。
    pub command: String,
    /// 超时（毫秒）。超过视为失败，但**不会**杀掉模型当前的推理流程。
    pub timeout_ms: u64,
    /// 回灌给模型的输出字节上限。
    pub max_output_bytes: usize,
    /// 纯开关，便于用户在 settings.json 里临时关掉而不删整块配置。
    pub enabled: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawVerifyCfg {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    enabled: Option<bool>,
}

impl RawVerifyCfg {
    /// `command` 空白或缺失时返回 None——**没有命令就等于没配置**，这是「零假设」
    /// 的落点：settings.json 里放了个空的 `verify: {}` 不应该让每个编辑都 spawn
    /// 一个 `sh` 然后立刻失败。
    fn into_cfg(self) -> Option<VerifyCfg> {
        let command = self.command?.trim().to_string();
        if command.is_empty() {
            return None;
        }
        Some(VerifyCfg {
            command,
            timeout_ms: self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(1),
            max_output_bytes: self
                .max_output_bytes
                .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
                .max(256),
            enabled: self.enabled.unwrap_or(true),
        })
    }
}

#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    pub passed: bool,
    pub duration_ms: u64,
    /// 直接拼在工具结果后面的文本，已经带好换行与分隔。
    pub report: String,
}

/// 读三源合并后的 verify 配置。没有可用命令 / 显式关闭 → None。
///
/// **项目级两源先过信任门控**（与 `hooks::load_effective_hooks` 同一判据）：
/// `verify.command` 同样会被 `sh -c` 直接执行，随 `git clone` 落地的仓库不该
/// 在用户批准前就能跑任意命令。
pub fn load_verify_cfg(cwd: &Path) -> Option<VerifyCfg> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(global) = wyj_config::global_config_dir() {
        paths.push(global.join("settings.json"));
    }
    if crate::hooks_trust::project_trusted(cwd) {
        let root = wyj_config::project_root(cwd);
        paths.push(root.join(".wyj-code").join("settings.json"));
        paths.push(root.join(".wyj-code").join("settings.local.json"));
    }

    let mut cfg: Option<VerifyCfg> = None;
    for path in &paths {
        if let Some(next) = read_verify_cfg(path) {
            cfg = Some(next);
        }
    }
    cfg.filter(|c| c.enabled)
}

fn read_verify_cfg(path: &Path) -> Option<VerifyCfg> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let raw = value.get("verify")?.clone();
    match serde_json::from_value::<RawVerifyCfg>(raw) {
        Ok(cfg) => cfg.into_cfg(),
        Err(e) => {
            tracing::warn!("verify: 解析 {} 中的 verify 字段失败: {e}", path.display());
            None
        }
    }
}

#[cfg(not(windows))]
fn build_tokio_command(command: &str, cwd: &Path) -> std::io::Result<tokio::process::Child> {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    c.spawn()
}

#[cfg(windows)]
fn build_tokio_command(command: &str, cwd: &Path) -> std::io::Result<tokio::process::Child> {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/C").arg(command);
    c.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    c.spawn()
}

/// 跑一次校验并把结果渲染成可回灌的文本。
///
/// **任何失败都不许 panic、不许挂住**：spawn 失败（命令不存在）、超时、非零退出，
/// 一律转成一段说明文字交给模型。校验机制自己出错时最坏结果是「这一轮没有校验
/// 反馈」，而不是整个任务失败。
pub async fn run_verify(cfg: &VerifyCfg, cwd: &Path) -> VerifyOutcome {
    let started = Instant::now();
    let spawn_failed = |e: std::io::Error| VerifyOutcome {
        passed: false,
        duration_ms: 0,
        report: format!(
            "\n\n[verify] 校验命令无法启动：{e}\n命令：`{}`",
            cfg.command
        ),
    };

    let child = match build_tokio_command(&cfg.command, cwd) {
        Ok(c) => c,
        Err(e) => return spawn_failed(e),
    };

    let timed_out = tokio::time::timeout(
        std::time::Duration::from_millis(cfg.timeout_ms),
        child.wait_with_output(),
    )
    .await;

    let duration_ms = started.elapsed().as_millis() as u64;

    let (passed, detail) = match timed_out {
        Ok(Ok(out)) => {
            let code = out.status.code();
            let passed = code == Some(0);
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let exit = match code {
                Some(c) => format!("exit {c}"),
                None => "被信号终止".to_string(),
            };
            (passed, (exit, text))
        }
        Ok(Err(e)) => (false, ("执行失败".to_string(), format!("{e}"))),
        Err(_) => (
            false,
            (
                format!("超过 {}s 超时", cfg.timeout_ms / 1000),
                String::new(),
            ),
        ),
    };

    let (exit, text) = detail;
    let report = if passed {
        format!(
            "\n\n[verify] 校验通过（`{}`，{exit}，{:.1}s）",
            cfg.command,
            duration_ms as f64 / 1000.0
        )
    } else {
        let clipped = clip_output(&text, cfg.max_output_bytes);
        let body = if clipped.trim().is_empty() {
            "(无输出)".to_string()
        } else {
            clipped
        };
        format!(
            "\n\n[verify] 校验失败（`{}`，{exit}，{:.1}s）——请依据下面的错误修正刚才的改动，\
             修完再跑一次校验确认：\n```\n{body}\n```",
            cfg.command,
            duration_ms as f64 / 1000.0
        )
    };

    VerifyOutcome {
        passed,
        duration_ms,
        report,
    }
}

/// 首尾各留一半的字节截断，char-boundary 安全（切在 UTF-8 多字节字符中间会
/// 产生替换字符，喂给模型就是纯噪声）。
pub fn clip_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes - head_budget;
    // 头部向下取整、尾部向上取整：两边保留的字节数都不超过各自预算。
    // 此前尾部也用 floor，保留长度会超出 `tail_budget` 最多 3 字节。
    let head_end = crate::textutil::floor_char_boundary(s, head_budget);
    let tail_start = crate::textutil::ceil_char_boundary(s, s.len() - tail_budget);
    format!(
        "{}\n…[verify] 中间 {} 字节已省略…\n{}",
        &s[..head_end],
        s.len() - tail_budget - head_budget,
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(dir: &Path, name: &str, body: &str) {
        let p = dir.join(".wyj-code").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// 项目级 verify 声明在批准前不会生效（`hooks_trust` 门控）。
    fn write_trusted_settings(dir: &Path, name: &str, body: &str) {
        write_settings(dir, name, body);
        crate::hooks_trust::approve(dir).unwrap();
    }

    #[test]
    fn a_project_declaring_a_command_is_verified() {
        let tmp = tempfile::tempdir().unwrap();
        write_trusted_settings(
            tmp.path(),
            "settings.json",
            r#"{"verify":{"command":"true"}}"#,
        );
        // approve 落在真实 config_dir 下，用例结束收尾（见 hooks_trust 的说明）。
        let _cleanup = crate::hooks_trust::ProjectStateCleanupForTests(tmp.path());
        let cfg = load_verify_cfg(tmp.path()).expect("应当解析出 verify 配置");
        assert_eq!(cfg.command, "true");
        assert!(cfg.enabled);
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    /// 未批准的项目级 verify 必须完全跳过——它随 `git clone` 落地，
    /// 批准前执行等于仓库作者能跑任意命令。
    #[test]
    fn unapproved_project_verify_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_settings(
            tmp.path(),
            "settings.json",
            r#"{"verify":{"command":"true"}}"#,
        );
        assert!(load_verify_cfg(tmp.path()).is_none());
    }

    #[test]
    fn no_declaration_means_no_verify_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            load_verify_cfg(tmp.path()).is_none(),
            "零假设：没声明校验命令的项目必须完全不受影响"
        );
    }

    #[test]
    fn an_empty_command_is_not_a_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        // 这三种写法都不该让每个 Edit 都 spawn 一次 sh 然后失败
        write_settings(tmp.path(), "settings.json", r#"{"verify":{}}"#);
        assert!(load_verify_cfg(tmp.path()).is_none());
        write_settings(
            tmp.path(),
            "settings.json",
            r#"{"verify":{"command":"  "}}"#,
        );
        assert!(load_verify_cfg(tmp.path()).is_none());
        write_settings(tmp.path(), "settings.json", r#"{"verify":{"command":""}}"#);
        assert!(load_verify_cfg(tmp.path()).is_none());
    }

    #[test]
    fn enabled_false_disables_without_losing_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        write_settings(
            tmp.path(),
            "settings.json",
            r#"{"verify":{"command":"true","enabled":false}}"#,
        );
        assert!(load_verify_cfg(tmp.path()).is_none());
    }

    #[tokio::test]
    async fn a_failing_command_reports_the_error_text() {
        let cfg = VerifyCfg {
            command: "echo boom >&2; exit 3".into(),
            timeout_ms: 5_000,
            max_output_bytes: 4_000,
            enabled: true,
        };
        let out = run_verify(&cfg, std::path::Path::new("/tmp")).await;
        assert!(!out.passed);
        assert!(out.report.contains("校验失败"), "{}", out.report);
        assert!(out.report.contains("exit 3"), "{}", out.report);
        assert!(out.report.contains("boom"), "{}", out.report);
    }

    #[tokio::test]
    async fn a_passing_command_says_so_without_dumping_output() {
        let cfg = VerifyCfg {
            command: "exit 0".into(),
            timeout_ms: 5_000,
            max_output_bytes: 4_000,
            enabled: true,
        };
        let out = run_verify(&cfg, std::path::Path::new("/tmp")).await;
        assert!(out.passed);
        assert!(out.report.contains("校验通过"), "{}", out.report);
    }

    #[tokio::test]
    async fn a_hanging_command_times_out_instead_of_hanging_the_turn() {
        let cfg = VerifyCfg {
            command: "sleep 30".into(),
            timeout_ms: 200,
            max_output_bytes: 4_000,
            enabled: true,
        };
        let out = run_verify(&cfg, std::path::Path::new("/tmp")).await;
        assert!(!out.passed);
        assert!(out.report.contains("超时"), "{}", out.report);
        assert!(out.duration_ms < 5_000, "实际耗时 {}ms", out.duration_ms);
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_report_not_a_crash() {
        let cfg = VerifyCfg {
            command: "definitely-not-a-real-binary-xyz".into(),
            timeout_ms: 5_000,
            max_output_bytes: 4_000,
            enabled: true,
        };
        let out = run_verify(&cfg, std::path::Path::new("/tmp")).await;
        assert!(!out.passed);
        assert!(out.report.contains("校验失败"), "{}", out.report);
    }

    #[test]
    fn clipping_keeps_both_ends_and_stays_on_char_boundaries() {
        let long = "错".repeat(4_000); // 12_000 字节，每字符 3 字节
        let clipped = clip_output(&long, 1_000);
        assert!(clipped.len() < long.len());
        assert!(clipped.starts_with('错') && clipped.contains("已省略") && clipped.ends_with('错'));
        // 切点不能落在多字节字符中间：能原样 round-trip 成合法字符串即证明确
        // 边界正确（非法 UTF-8 会带 U+FFFD）。
        assert!(!clipped.contains('\u{FFFD}'), "截断切在了字符中间");
    }

    #[test]
    fn short_output_is_left_alone() {
        assert_eq!(clip_output("hi", 4_000), "hi");
    }
}
