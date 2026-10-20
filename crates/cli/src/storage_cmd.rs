//! `wyj-code storage` 子命令:本地存储治理。
//!
//! ## 子命令
//!
//! - `status`  列出 ~/.wyj-code 下各子系统的占用 Top-N(sessions/checkpoints/cas/evolution/memory-v3)
//! - `doctor`  检查损坏的 session 文件 / orphan CAS blob / 损坏的 checkpoint manifest
//! - `prune`   按 retention 上限清理(checkpoint 淘汰超限最老条目、CAS 回收 ref_count==0 的 blob)
//!
//! `doctor` 与 `prune` 此前都是 TODO 桩:命令能跑、什么都没做,连参数都被
//! 原样丢弃。现在两者都真正接线到已有的 retention / 统计实现。
//!
//! `prune` 默认只报计划(不删除),确认后加 `--yes` 实际执行。

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
    /// 按 retention 上限清理(checkpoint / CAS blob),默认只报计划
    #[command(name = "prune")]
    Prune {
        /// 只报计划,不实际删除(默认开)
        ///
        /// 用 `ArgAction::Set` + `default_value_t` 而非 `SetTrue`：`SetTrue`
        /// 的 bool 缺席恒为 true、无法置 false,`--dry-run=false` 会被 clap
        /// 判成 "unexpected value"。这里改成取值型,`--dry-run false` 可用。
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
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
                entry.bytes, entry.id, entry.checkpoint_count
            );
        }
    }
    Ok(())
}

/// 存储健康检查。
///
/// 此前是纯 TODO 桩：命令存在、能跑、什么都没查。现在真正检查三件能查
/// 出来的事：
/// 1. **损坏的 session 文件**（JSON 解析失败）——`/resume` 会直接报错。
/// 2. **orphan CAS blob**（`ref_count == 0` 且不在 LRU 保留集内）——占着
///    磁盘不释放。
/// 3. **checkpoint manifest 损坏**——`/rewind` 会看不到该会话的存档。
fn run_doctor(config_base: &Path, cfg: &Config) -> Result<()> {
    println!("=== wyj-code storage doctor ===");

    // ── 1. 损坏的 session 文件 ──
    let sessions_dir = config_base.join("sessions");
    let mut broken_sessions = Vec::new();
    let mut session_count = 0usize;
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            session_count += 1;
            let Ok(content) = std::fs::read_to_string(&path) else {
                broken_sessions.push((path, "读取失败".to_string()));
                continue;
            };
            if let Err(error) = serde_json::from_str::<serde_json::Value>(&content) {
                broken_sessions.push((path, error.to_string()));
            }
        }
    }
    println!(
        "session 文件: {} 个,损坏 {} 个",
        session_count,
        broken_sessions.len()
    );
    for (path, reason) in broken_sessions.iter().take(10) {
        println!("  损坏 {}: {reason}", path.display());
    }
    if broken_sessions.len() > 10 {
        println!("  ... 另有 {} 个未列出", broken_sessions.len() - 10);
    }

    // ── 2. orphan CAS blob ──
    match wyj_core::workspace_cas::WorkspaceCas::open_silently(
        &config_base.join("cas"),
        cfg.storage.cas_max_blob_bytes,
    ) {
        Some(cas) => match cas.stats() {
            Ok(stats) => {
                println!(
                    "CAS blob: {} 个 / {} 字节,其中 orphan {} 个 / {} 字节",
                    stats.total_blobs, stats.total_bytes, stats.orphan_blobs, stats.orphan_bytes
                );
                if stats.orphan_blobs > 0 {
                    println!("  提示:`storage prune` 会回收 ref_count == 0 的 blob");
                }
            }
            Err(error) => println!("CAS 统计失败: {error}"),
        },
        None => println!("CAS: 未启用(目录不可用)"),
    }

    // ── 3. 损坏的 checkpoint manifest ──
    let mut broken_manifests = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(session_id) = name.strip_suffix(".checkpoints") else {
                continue;
            };
            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&manifest_path) else {
                broken_manifests.push((manifest_path, "读取失败".to_string()));
                continue;
            };
            if let Err(error) = serde_json::from_str::<serde_json::Value>(&content) {
                broken_manifests.push((manifest_path, error.to_string()));
            } else {
                let _ = session_id;
            }
        }
    }
    println!("checkpoint manifest: 损坏 {} 个", broken_manifests.len());
    for (path, reason) in broken_manifests.iter().take(10) {
        println!("  损坏 {}: {reason}", path.display());
    }

    Ok(())
}

/// 清理可按 retention 上限收口的子系统。
///
/// 此前这里是纯 TODO 桩:两个参数(`--dry-run` / `--yes`)被 clap 解析、绑定、
/// 求值,然后原样丢掉——`storage prune` 与 `storage prune --yes` 的输出逐字
/// 相同。现在真正接线到已有的 retention 实现。
fn run_prune(config_base: &Path, cfg: &Config, dry_run: bool) -> Result<()> {
    println!("=== wyj-code storage prune ===");
    if dry_run {
        println!("(dry-run:只报计划,不删除。确认无误后加 --yes 实际执行)");
    }

    let storage = &cfg.storage;
    let sessions_dir = config_base.join("sessions");

    // ── checkpoint:按 session 淘汰超限的最老 checkpoint ──
    // `checkpoints_per_session` 上限由 `CheckpointStore::enforce_retention`
    // 实现(它只在 store 打开时被调用),这里对每个 session 主动触发一次。
    let mut checkpoint_sessions = 0usize;
    if storage.checkpoints_per_session > 0 {
        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(session_id) = name.strip_suffix(".checkpoints") else {
                    continue;
                };
                checkpoint_sessions += 1;
                if dry_run {
                    continue;
                }
                match wyj_core::CheckpointStore::new(&sessions_dir, session_id) {
                    Ok(store) => {
                        if let Err(error) = store.enforce_retention() {
                            tracing::warn!("checkpoint 保留策略执行失败 {session_id}: {error}");
                        }
                    }
                    Err(error) => {
                        tracing::warn!("打开 checkpoint 目录失败 {session_id}: {error}");
                    }
                }
            }
        }
    }
    println!(
        "checkpoint: {} 个 session 目录,上限 {} 条/session{}",
        checkpoint_sessions,
        storage.checkpoints_per_session,
        if dry_run { "(未执行)" } else { "" }
    );

    // ── CAS blob:按总量上限回收 ref_count == 0 的 blob ──
    if storage.cas_total_bytes > 0 {
        match wyj_core::workspace_cas::WorkspaceCas::open_silently(
            &config_base.join("cas"),
            storage.cas_max_blob_bytes,
        ) {
            Some(cas) => {
                if dry_run {
                    println!(
                        "cas: 上限 {} 字节(dry-run 未执行 gc)",
                        storage.cas_total_bytes
                    );
                } else {
                    match cas.gc(storage.cas_total_bytes) {
                        Ok(stats) => println!(
                            "cas: 回收 {} 个 blob / {} 字节,当前 {} 字节",
                            stats.deleted_blobs, stats.freed_bytes, stats.total_bytes
                        ),
                        Err(error) => {
                            tracing::warn!("CAS gc 失败: {error}");
                            println!("cas: gc 失败 ({error})");
                        }
                    }
                }
            }
            None => println!("cas: 未启用(目录不可用)"),
        }
    } else {
        println!("cas: 总量上限为 0,跳过");
    }

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
                                e.path().extension().and_then(|x| x.to_str()) == Some("json")
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
        report
            .top_sessions
            .sort_by_key(|b| std::cmp::Reverse(b.bytes));
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
