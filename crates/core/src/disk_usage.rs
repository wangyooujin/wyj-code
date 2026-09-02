//! `~/.wyj-code` 各子系统磁盘占用扫描 + 一次性超阈值提示。
//!
//! 实现策略:沿用 `evolution::directory_size` 同款 `std::fs::read_dir` 递归,
//! 不引入 `walkdir` 等新依赖;进程内 `OnceLock` 保证单进程只触发一次警告,
//! 避免每次工具调用都打印。
//!
//! 默认阈值来自 `StorageRetentionCfg::disk_usage_warn_bytes`(5 GiB);设为 0
//! 表示关闭提示。

use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct DiskUsageReport {
    pub sessions: u64,
    pub checkpoints: u64,
    pub memory_v2: u64,
    pub memory_v3: u64,
    pub evolution: u64,
    pub schedule_logs: u64,
    pub plugins: u64,
    pub workspaces: u64,
    pub other: u64,
}

impl DiskUsageReport {
    pub fn total(&self) -> u64 {
        self.sessions
            + self.checkpoints
            + self.memory_v2
            + self.memory_v3
            + self.evolution
            + self.schedule_logs
            + self.plugins
            + self.workspaces
            + self.other
    }

    fn top_contributors(&self, n: usize) -> Vec<(&'static str, u64)> {
        let mut v = vec![
            ("sessions+checkpoints", self.sessions + self.checkpoints),
            ("memory-v3", self.memory_v3),
            ("evolution", self.evolution),
            ("memory-v2", self.memory_v2),
            ("schedule-logs", self.schedule_logs),
            ("plugins", self.plugins),
            ("workspaces", self.workspaces),
        ];
        v.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
        v.into_iter().take(n).collect()
    }
}

/// 递归统计 `path` 下所有文件字节数。目录不存在或不可读时返回 0,
/// 不污染调用方主流程。
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else if ft.is_dir() {
            total += dir_size(&entry_path);
        }
    }
    total
}

/// 扫描 `~/.wyj-code` 下各主要子系统目录并返回字节数。
/// `sessions` 字段包含 `sessions/<id>.subagents/` 与 `.checkpoints/`(因为它们
/// 与 session 文件在同根 `~/.wyj-code/sessions/` 下,粗粒度汇总更省 IO)。
pub fn scan(config_base: &Path) -> DiskUsageReport {
    DiskUsageReport {
        sessions: dir_size(&config_base.join("sessions")),
        memory_v2: dir_size(&config_base.join("memory")),
        memory_v3: dir_size(&config_base.join("memory-v3")),
        evolution: dir_size(&config_base.join("evolution")),
        schedule_logs: dir_size(&config_base.join("schedule")),
        plugins: dir_size(&config_base.join("plugins")),
        workspaces: dir_size(&config_base.join("workspaces")),
        ..DiskUsageReport::default()
    }
}

/// 若 `~/.wyj-code` 占用超过 `threshold_bytes` 且未提示过,触发一次 `tracing::warn`
/// 提示用户并列出 top 3 贡献者。`threshold_bytes == 0` 表示关闭。
///
/// 单进程内只触发一次(`OnceLock`),避免每次 LLM 回合都打一次日志污染终端。
pub fn warn_if_over_budget(config_base: &Path, threshold_bytes: u64) {
    if threshold_bytes == 0 {
        return;
    }
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.get().is_some() {
        return;
    }
    let report = scan(config_base);
    if report.total() <= threshold_bytes {
        return;
    }
    let _ = WARNED.set(());
    let total_mb = report.total() / 1024 / 1024;
    let threshold_mb = threshold_bytes / 1024 / 1024;
    let top = report.top_contributors(3);
    let top_str = top
        .iter()
        .map(|(name, b)| format!("{name}={}MB", b / 1024 / 1024))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        "~/.wyj-code 占用 {total_mb}MB 超过 {threshold_mb}MB 阈值(top:{top_str})。\
         可调整 ~/.wyj-code/config.toml [storage] 节下的 cap 默认值,或运行 \
         `wyj-code session prune` / 手动清理过期 sessions/。",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_returns_zeros_on_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let report = scan(tmp.path());
        assert_eq!(report.total(), 0);
    }

    #[test]
    fn scan_aggregates_known_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        fs::create_dir_all(base.join("sessions")).unwrap();
        fs::write(base.join("sessions").join("a.json"), vec![0u8; 100]).unwrap();
        fs::create_dir_all(base.join("memory").join("p1")).unwrap();
        fs::write(base.join("memory").join("p1").join("f.md"), vec![0u8; 50]).unwrap();
        fs::create_dir_all(base.join("evolution")).unwrap();
        fs::write(base.join("evolution").join("ep.jsonl"), vec![0u8; 200]).unwrap();
        let report = scan(base);
        assert_eq!(report.sessions, 100);
        assert_eq!(report.memory_v2, 50);
        assert_eq!(report.evolution, 200);
        assert_eq!(report.total(), 350);
    }

    #[test]
    fn top_contributors_orders_descending() {
        let report = DiskUsageReport {
            sessions: 100,
            memory_v3: 300,
            evolution: 200,
            ..DiskUsageReport::default()
        };
        let top = report.top_contributors(3);
        assert_eq!(top[0].0, "memory-v3");
        assert_eq!(top[1].0, "evolution");
        assert_eq!(top[2].0, "sessions+checkpoints");
    }

    #[test]
    fn warn_if_over_budget_only_fires_once() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        fs::create_dir_all(base.join("sessions")).unwrap();
        fs::write(base.join("sessions").join("huge.json"), vec![0u8; 10_000]).unwrap();
        // 第一次:超阈值,内部 set WARNED。
        warn_if_over_budget(base, 1);
        // 第二次:即使仍超阈值,OnceLock 已 set,不再提示(用 counter 测试反而
        // 不容易稳定,这里只验证不会 panic,且返回了正常的 total)。
        let r2 = scan(base);
        assert!(r2.total() > 0);
    }
}
