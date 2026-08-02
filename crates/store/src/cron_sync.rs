//! cron 表达式生成/校验/下次触发时间计算 + 系统 `crontab` 同步。
//!
//! 任务本身存标准 5 段 POSIX crontab 表达式（`分 时 日 月 周`），与系统
//! crontab 直接对应；`cron` crate（zslayton/cron）要求 6/7 段（多一个秒字段，
//! 可选年字段），仅在需要用它解析/计算下次触发时间时临时转换，不影响持久化
//! 格式。

use crate::schedule::ScheduleTask;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;

const BEGIN_MARKER: &str = "# BEGIN wyj-code schedule";
const END_MARKER: &str = "# END wyj-code schedule";

/// 常用频率预设，`frequency_to_cron` 生成标准 5 段表达式；`Custom` 覆盖高级
/// 用户直接手写 cron 表达式的逃生舱，不需要单独一套输入模式。
#[derive(Debug, Clone)]
pub enum Frequency {
    Daily {
        hour: u32,
        minute: u32,
    },
    /// `weekday`: 0=周日..6=周六（cron 标准 dow 取值）。
    Weekly {
        weekday: u32,
        hour: u32,
        minute: u32,
    },
    Hourly {
        minute: u32,
    },
    Custom(String),
}

pub fn frequency_to_cron(freq: &Frequency) -> String {
    match freq {
        Frequency::Daily { hour, minute } => format!("{minute} {hour} * * *"),
        Frequency::Weekly {
            weekday,
            hour,
            minute,
        } => format!("{minute} {hour} * * {weekday}"),
        Frequency::Hourly { minute } => format!("{minute} * * * *"),
        Frequency::Custom(expr) => expr.clone(),
    }
}

/// 标准 5 段（分 时 日 月 周）→ `cron` crate 要求的 7 段（秒 分 时 日 月 周 年），
/// 秒固定 0、年固定通配符。
fn to_cron_crate_expr(expr: &str) -> Result<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        anyhow::bail!(
            "cron 表达式必须是标准 5 段格式（分 时 日 月 周），实际得到 {} 段: \"{expr}\"",
            fields.len()
        );
    }
    Ok(format!(
        "0 {} {} {} {} {} *",
        fields[0], fields[1], fields[2], fields[3], fields[4]
    ))
}

/// 校验 5 段 cron 表达式语法是否合法。
pub fn validate_cron(expr: &str) -> Result<()> {
    let full = to_cron_crate_expr(expr)?;
    cron::Schedule::from_str(&full)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("非法 cron 表达式 \"{expr}\": {e}"))
}

/// 计算 `after` 之后下一次触发时间；面板展示"下次运行：..."用。
pub fn next_run_after(expr: &str, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let full = to_cron_crate_expr(expr)?;
    let schedule = cron::Schedule::from_str(&full)
        .map_err(|e| anyhow::anyhow!("非法 cron 表达式 \"{expr}\": {e}"))?;
    Ok(schedule.after(&after).next())
}

/// 系统 crontab 读写抽象，测试注入假实现，避免单测触碰真实用户 crontab。
pub trait CrontabIo {
    fn read(&self) -> Result<String>;
    fn write(&self, content: &str) -> Result<()>;
}

pub struct SystemCrontabIo;

impl CrontabIo for SystemCrontabIo {
    fn read(&self) -> Result<String> {
        match Command::new("crontab").arg("-l").output() {
            Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            // 用户从未设置过 crontab 时 `crontab -l` 以非 0 退出（stderr: "no
            // crontab for <user>"），视为空 crontab 而非错误。
            Ok(_) => Ok(String::new()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
                "当前系统未找到 crontab 命令，无法自动同步定时任务（Windows 暂不支持自动同步，可用任务计划程序自行调用 `wyj-code schedule run <id>`）"
            ),
            Err(e) => Err(e).context("执行 crontab -l 失败"),
        }
    }

    fn write(&self, content: &str) -> Result<()> {
        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
            .context("启动 crontab - 失败")?;
        child
            .stdin
            .as_mut()
            .context("无法写入 crontab 子进程 stdin")?
            .write_all(content.as_bytes())
            .context("写入 crontab 内容失败")?;
        let status = child.wait().context("等待 crontab - 完成失败")?;
        if !status.success() {
            anyhow::bail!("crontab - 写入失败（退出码 {:?}）", status.code());
        }
        Ok(())
    }
}

fn strip_managed_block(existing: &str) -> String {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        match line.trim() {
            BEGIN_MARKER => in_block = true,
            END_MARKER => in_block = false,
            _ if !in_block => out.push(line),
            _ => {}
        }
    }
    out.join("\n")
}

fn task_line(exe: &Path, log_path: &Path, task: &ScheduleTask) -> String {
    format!(
        "{} {} schedule run {} >> {} 2>&1 # wyj-code:schedule:{}",
        task.cron,
        exe.display(),
        task.id,
        log_path.display(),
        task.id
    )
}

/// 纯字符串处理：把当前 crontab 内容里带 `# BEGIN/END wyj-code schedule`
/// 标记的区块整体替换成由 `tasks` 生成的新区块，标记块外的内容原样保留。
/// 没有任何启用任务时不追加空区块，保持 crontab 干净。
pub fn build_crontab_content(
    existing: &str,
    tasks: &[ScheduleTask],
    exe: &Path,
    log_path: &Path,
) -> String {
    let stripped = strip_managed_block(existing);
    let base = stripped.trim_end();
    let enabled: Vec<&ScheduleTask> = tasks.iter().filter(|t| t.enabled).collect();

    if enabled.is_empty() {
        return if base.is_empty() {
            String::new()
        } else {
            format!("{base}\n")
        };
    }

    let mut result = String::new();
    if !base.is_empty() {
        result.push_str(base);
        result.push('\n');
    }
    result.push_str(BEGIN_MARKER);
    result.push('\n');
    for task in enabled {
        result.push_str(&task_line(exe, log_path, task));
        result.push('\n');
    }
    result.push_str(END_MARKER);
    result.push('\n');
    result
}

/// 首次同步前把原始 crontab 内容备份一份，仅备份一次（用 `crontab.backup.done`
/// marker 文件记录"已经备份过"，避免每次同步都新增一份备份文件）。`state_dir`
/// 抽出为参数（而非内部直接调用 `schedule::schedule_dir()`），供测试注入临时
/// 目录，不依赖修改进程级 `$HOME` 环境变量（并行测试下不安全）。
fn ensure_backup(state_dir: &Path, existing: &str) -> Result<()> {
    std::fs::create_dir_all(state_dir).context("创建 schedule 目录失败")?;
    let marker = state_dir.join("crontab.backup.done");
    if marker.exists() {
        return Ok(());
    }
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let backup_path = state_dir.join(format!("crontab.backup.{ts}"));
    std::fs::write(&backup_path, existing).context("写入 crontab 备份失败")?;
    std::fs::write(&marker, backup_path.display().to_string()).context("写入备份 marker 失败")?;
    Ok(())
}

/// 把 `tasks` 里 `enabled == true` 的任务同步进系统 crontab，仅替换本工具
/// 管理的标记区块，不触碰用户其他 cron 条目。
pub fn sync_crontab(tasks: &[ScheduleTask]) -> Result<()> {
    let state_dir = crate::schedule::schedule_dir()?;
    sync_crontab_in(&SystemCrontabIo, &state_dir, tasks)
}

/// `sync_crontab` 的可测试核心：`io`/`state_dir` 均可注入，测试用临时目录 +
/// 假 `CrontabIo`，不触碰真实用户 crontab 或 `~/.wyj-code`。
pub fn sync_crontab_in(io: &dyn CrontabIo, state_dir: &Path, tasks: &[ScheduleTask]) -> Result<()> {
    let exe = std::env::current_exe().context("无法定位 wyj-code 可执行文件路径")?;
    let log_path = state_dir.join("run.log");
    let existing = io.read()?;
    ensure_backup(state_dir, &existing)?;
    let content = build_crontab_content(&existing, tasks, &exe, &log_path);
    io.write(&content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::ScheduleTask;
    use std::cell::RefCell;
    use std::path::PathBuf;

    fn fake_task(id: &str, cron: &str, enabled: bool) -> ScheduleTask {
        ScheduleTask {
            id: id.to_string(),
            name: "测试".to_string(),
            prompt: "做点事".to_string(),
            cron: cron.to_string(),
            cwd: PathBuf::from("/tmp"),
            enabled,
            needs_permission_review: false,
            permissions: crate::schedule::SchedulePermissions::default(),
            notify_on_failure: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_run: None,
        }
    }

    #[test]
    fn frequency_presets_generate_expected_cron() {
        assert_eq!(
            frequency_to_cron(&Frequency::Daily { hour: 8, minute: 0 }),
            "0 8 * * *"
        );
        assert_eq!(
            frequency_to_cron(&Frequency::Weekly {
                weekday: 1,
                hour: 20,
                minute: 30
            }),
            "30 20 * * 1"
        );
        assert_eq!(
            frequency_to_cron(&Frequency::Hourly { minute: 15 }),
            "15 * * * *"
        );
    }

    #[test]
    fn validate_cron_accepts_standard_five_field() {
        assert!(validate_cron("0 8 * * *").is_ok());
        assert!(validate_cron("*/15 * * * *").is_ok());
    }

    #[test]
    fn validate_cron_rejects_wrong_field_count() {
        assert!(validate_cron("0 8 * *").is_err());
        assert!(validate_cron("0 0 8 * * *").is_err());
    }

    #[test]
    fn validate_cron_rejects_garbage() {
        assert!(validate_cron("not a cron expr").is_err());
    }

    #[test]
    fn next_run_after_computes_upcoming_daily_fire() {
        let after = DateTime::parse_from_rfc3339("2026-07-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = next_run_after("0 8 * * *", after).unwrap().unwrap();
        assert_eq!(
            next.format("%Y-%m-%d %H:%M").to_string(),
            "2026-07-19 08:00"
        );
    }

    #[test]
    fn build_crontab_content_preserves_unrelated_lines_and_replaces_managed_block() {
        let existing = "0 3 * * * /usr/bin/backup.sh\n";
        let exe = Path::new("/usr/local/bin/wyj-code");
        let log = Path::new("/home/user/.wyj-code/schedule/run.log");
        let tasks = vec![fake_task("id1", "0 8 * * *", true)];

        let content = build_crontab_content(existing, &tasks, exe, log);
        assert!(content.contains("/usr/bin/backup.sh"));
        assert!(content.contains(BEGIN_MARKER));
        assert!(content.contains(END_MARKER));
        assert!(content.contains("wyj-code:schedule:id1"));
        assert!(content.contains("schedule run id1"));

        // 再来一轮同步（模拟第二次保存），标记块整体替换，不重复、不产生第二份。
        let tasks2 = vec![fake_task("id1", "0 9 * * *", true)];
        let content2 = build_crontab_content(&content, &tasks2, exe, log);
        assert!(content2.contains("/usr/bin/backup.sh"));
        assert_eq!(content2.matches(BEGIN_MARKER).count(), 1);
        assert!(content2.contains("0 9 * * *"));
        assert!(!content2.contains("0 8 * * *"));
    }

    #[test]
    fn build_crontab_content_skips_disabled_tasks() {
        let tasks = vec![fake_task("id1", "0 8 * * *", false)];
        let content = build_crontab_content(
            "",
            &tasks,
            Path::new("/bin/wyj-code"),
            Path::new("/tmp/run.log"),
        );
        assert!(!content.contains("wyj-code:schedule:id1"));
    }

    #[test]
    fn build_crontab_content_no_enabled_tasks_produces_no_empty_block() {
        let existing = "0 3 * * * /usr/bin/backup.sh\n";
        let content = build_crontab_content(
            existing,
            &[],
            Path::new("/bin/wyj-code"),
            Path::new("/tmp/run.log"),
        );
        assert!(!content.contains(BEGIN_MARKER));
        assert!(content.contains("/usr/bin/backup.sh"));
    }

    struct FakeCrontabIo {
        content: RefCell<String>,
    }

    impl CrontabIo for FakeCrontabIo {
        fn read(&self) -> Result<String> {
            Ok(self.content.borrow().clone())
        }
        fn write(&self, content: &str) -> Result<()> {
            *self.content.borrow_mut() = content.to_string();
            Ok(())
        }
    }

    #[test]
    fn sync_crontab_in_only_touches_managed_block() {
        let io = FakeCrontabIo {
            content: RefCell::new("0 3 * * * /usr/bin/backup.sh\n".to_string()),
        };
        let state_dir = tempfile::tempdir().unwrap();
        let tasks = vec![fake_task("id1", "0 8 * * *", true)];
        sync_crontab_in(&io, state_dir.path(), &tasks).unwrap();
        let written = io.content.borrow().clone();
        assert!(written.contains("/usr/bin/backup.sh"));
        assert!(written.contains("wyj-code:schedule:id1"));
        assert!(state_dir.path().join("crontab.backup.done").exists());
    }

    #[test]
    fn sync_crontab_in_only_backs_up_once() {
        let io = FakeCrontabIo {
            content: RefCell::new("0 3 * * * /usr/bin/backup.sh\n".to_string()),
        };
        let state_dir = tempfile::tempdir().unwrap();
        let tasks = vec![fake_task("id1", "0 8 * * *", true)];
        sync_crontab_in(&io, state_dir.path(), &tasks).unwrap();
        let backups_after_first: Vec<_> = std::fs::read_dir(state_dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("crontab.backup.2")
            })
            .collect();
        sync_crontab_in(&io, state_dir.path(), &tasks).unwrap();
        let backups_after_second: Vec<_> = std::fs::read_dir(state_dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("crontab.backup.2")
            })
            .collect();
        assert_eq!(backups_after_first.len(), backups_after_second.len());
    }
}
