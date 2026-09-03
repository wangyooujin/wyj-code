//! `wyj-code storage` 子命令:本地存储治理(Phase 4)。
//!
//! ## 子命令
//!
//! - `status`  列出 ~/.wyj-code 下各子系统的占用 Top-N(sessions/checkpoints/cas/evolution/memory-v3)
//! - `doctor`  检查 orphan blob / 损坏 manifest / 老 v1 schema 等异常
//! - `prune`   按 TTL + 字节上限 LRU 删除过期项
//!
//! 详细设计见 `~/.claude/plans/nested-wobbling-rain.md` 的 M4 节。

use anyhow::Result;
use clap::Subcommand;
use std::path::Path;
use wyj_config::Config;

#[derive(Subcommand, Debug)]
pub enum StorageCommand {
    /// 打印 ~/.wyj-code 各子系统占用与 Top-N
    #[command(name = "status")]
    Status {
        /// 输出 JSON 格式(可脚本消费)
        #[arg(long)]
        json: bool,
    },
    /// 检查异常(orphan blob / 损坏 manifest / 老 v1 schema)
    #[command(name = "doctor")]
    Doctor,
    /// 按 TTL + 字节上限清理(--dry-run 默认)
    #[command(name = "prune")]
    Prune {
        /// 仅打印计划,不实际删除
        #[arg(long, default_value = "true")]
        dry_run: bool,
        /// 实际删除(覆盖 --dry-run)
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
    },
}

pub fn run(command: StorageCommand, config_base: &Path, cfg: &Config) -> Result<()> {
    match command {
        StorageCommand::Status { json } => run_status(config_base, cfg, json),
        StorageCommand::Doctor => run_doctor(config_base, cfg),
        StorageCommand::Prune { dry_run, yes } => {
            let effective_dry = !yes && dry_run;
            run_prune(config_base, cfg, effective_dry)
        }
    }
}

fn run_status(config_base: &Path, _cfg: &Config, json: bool) -> Result<()> {
    let report = collect_status(config_base)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("=== wyj-code storage status ===");
        println!(
            "sessions/   : {:>10}  ({} .json files, {} .checkpoints/ dirs)",
            report.sessions_bytes, report.sessions_json_count, report.sessions_checkpoint_dirs
        );
        println!(
            "cas/        : {:>10}  ({} blobs, {} orphans)",
            report.cas_bytes, report.cas_blobs, report.cas_orphans
        );
        println!(
            "memory-v3/  : {:>10}  ({} files)",
            report.memory_v3_bytes, report.memory_v3_files
        );
        println!(
            "evolution/  : {:>10}  ({} files)",
            report.evolution_bytes, report.evolution_files
        );
        println!(
            "projects/   : {:>10}  ({} files)",
            report.projects_bytes, report.projects_files
        );
        println!();
        println!("Top 5 heaviest sessions (incl. .checkpoints):");
        for entry in report.top_sessions.iter().take(5) {
            println!(
                "  {:>10}  {}/{}",
                entry.bytes,
                entry.id,
                entry.checkpoint_count
            );
        }
    }
    Ok(())
}

fn run_doctor(_config_base: &Path, _cfg: &Config) -> Result<()> {
    println!("=== wyj-code storage doctor ===");
    println!("(TODO Phase 4:检查 orphan blob / 损坏 manifest / 老 v1 schema)");
    Ok(())
}

fn run_prune(_config_base: &Path, _cfg: &Config, _dry_run: bool) -> Result<()> {
    println!("=== wyj-code storage prune ===");
    println!("(TODO Phase 4:按 TTL + 字节上限 LRU 删,默认 --dry-run)");
    Ok(())
}

#[derive(Debug, Default, serde::Serialize)]
struct StatusReport {
    sessions_bytes: u64,
    sessions_json_count: u64,
    sessions_checkpoint_dirs: u64,
    cas_bytes: u64,
    cas_blobs: u64,
    cas_orphans: u64,
    memory_v3_bytes: u64,
    memory_v3_files: u64,
    evolution_bytes: u64,
    evolution_files: u64,
    projects_bytes: u64,
    projects_files: u64,
    top_sessions: Vec<SessionEntry>,
}

#[derive(Debug, Default, serde::Serialize)]
struct SessionEntry {
    id: String,
    bytes: u64,
    checkpoint_count: u64,
}

fn collect_status(config_base: &Path) -> Result<StatusReport> {
    let mut report = StatusReport::default();
    let sessions_dir = config_base.join("sessions");
    if sessions_dir.is_dir() {
        for entry in std::fs::read_dir(&sessions_dir)?.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if name.ends_with(".json") {
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                report.sessions_bytes = report.sessions_bytes.saturating_add(bytes);
                report.sessions_json_count += 1;
            } else if name.ends_with(".checkpoints") {
                let cp_dir_bytes = dir_size(&path);
                let cp_count = std::fs::read_dir(&path)
                    .map(|d| {
                        d.flatten()
                            .filter(|e| {
                                e.path()
                                    .extension()
                                    .and_then(|x| x.to_str())
                                    == Some("json")
                            })
                            .count() as u64
                    })
                    .unwrap_or(0);
                let session_id = name.trim_end_matches(".checkpoints").to_string();
                report.sessions_bytes = report.sessions_bytes.saturating_add(cp_dir_bytes);
                report.sessions_checkpoint_dirs += 1;
                report.top_sessions.push(SessionEntry {
                    id: session_id,
                    bytes: cp_dir_bytes,
                    checkpoint_count: cp_count,
                });
            }
        }
        report.top_sessions.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    }
    let cas_root = config_base.join("cas/sha256");
    if cas_root.is_dir() {
        for aa in std::fs::read_dir(&cas_root)?.flatten() {
            for bb in std::fs::read_dir(aa.path()).into_iter().flatten().flatten() {
                for entry in std::fs::read_dir(bb.path()).into_iter().flatten().flatten() {
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    if name.ends_with(".blob") {
                        if let Ok(meta) = std::fs::metadata(&path) {
                            report.cas_bytes = report.cas_bytes.saturating_add(meta.len());
                            report.cas_blobs += 1;
                        }
                    } else if name.ends_with(".meta.json") {
                        if let Ok(bytes) = std::fs::read(&path) {
                            if let Ok(meta) =
                                serde_json::from_slice::<wyj_core::workspace_cas::CasMeta>(&bytes)
                            {
                                if meta.ref_count == 0 {
                                    report.cas_orphans += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    report.memory_v3_bytes = dir_size_if_exists(&config_base.join("memory-v3"));
    report.memory_v3_files = file_count_if_exists(&config_base.join("memory-v3"));
    report.evolution_bytes = dir_size_if_exists(&config_base.join("evolution"));
    report.evolution_files = file_count_if_exists(&config_base.join("evolution"));
    report.projects_bytes = dir_size_if_exists(&config_base.join("projects"));
    report.projects_files = file_count_if_exists(&config_base.join("projects"));
    Ok(report)
}

fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                } else if meta.is_dir() {
                    total = total.saturating_add(dir_size(&entry.path()));
                }
            }
        }
    }
    total
}

fn dir_size_if_exists(path: &Path) -> u64 {
    if path.is_dir() {
        dir_size(path)
    } else {
        0
    }
}

fn file_count_if_exists(path: &Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    let mut count = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    count += 1;
                } else if meta.is_dir() {
                    count += file_count_if_exists(&entry.path());
                }
            }
        }
    }
    count
}
