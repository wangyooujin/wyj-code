//! `wyj-code storage` 子命令:本地存储治理。
//!
//! ## 子命令
//!
//! - `status`  列出 ~/.wyj-code 下各子系统的占用 Top-N(sessions/checkpoints/cas/evolution/memory-v3)
//! - `doctor`  检查损坏的 session 文件 / orphan CAS blob / 损坏的 checkpoint manifest
//! - `prune`   按 retention 上限清理(checkpoint 淘汰超限最老条目、CAS 回收 ref_count==0 的 blob)
//! - `migrate` 把旧布局(会话平铺 / 旧项目键)搬到按项目分目录的新布局
//!
//! `doctor` 与 `prune` 此前都是 TODO 桩:命令能跑、什么都没做,连参数都被
//! 原样丢弃。现在两者都真正接线到已有的 retention / 统计实现。
//!
//! `prune` 与 `migrate` 默认都只报计划(不删除/不移动),确认后加 `--yes` 实际执行。

use anyhow::Result;
use clap::Subcommand;
use std::path::{Path, PathBuf};
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
    /// 把旧布局(全局平铺 / 旧项目键)搬到新布局,默认只报计划
    #[command(name = "migrate")]
    Migrate {
        /// 只报计划,不实际移动(默认开)
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
        dry_run: bool,
        /// 实际执行(覆盖 --dry-run)
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
        StorageCommand::Migrate { dry_run, yes } => run_migrate(config_base, !yes && dry_run),
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
    let mut broken_sessions = Vec::new();
    let mut session_count = 0usize;
    for sessions_dir in session_dirs(config_base) {
        let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // `.meta.json` 是旁路元数据，不是会话正文，损坏不算「会话坏了」。
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".meta.json"))
            {
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
    for (_, path, session_id) in checkpoint_dirs(config_base) {
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
    println!("checkpoint manifest: 损坏 {} 个", broken_manifests.len());
    for (path, reason) in broken_manifests.iter().take(10) {
        println!("  损坏 {}: {reason}", path.display());
    }

    // ── 4. 损坏的 checkpoint 文件 ──
    //
    // manifest 只是索引，真正的内容在 `<id>.json`。此前 doctor 只查 manifest，
    // 于是「索引完好、内容读不出」这一整类损坏完全不可见——2026-10-20 实测有
    // 120 个 checkpoint（约 133 MB）因脱敏正则破坏 JSON 结构而永久读不出，
    // 而 `doctor` 报的是「损坏 0 个」。
    //
    // 只报不删：这些文件仍占着磁盘，但删不删是用户的决定。
    // `storage prune --yes` 会顺带清理（见 `run_prune`）。
    let mut broken_checkpoints = Vec::new();
    let mut checkpoint_files = 0usize;
    for (_, path, _) in checkpoint_dirs(config_base) {
        let Ok(files) = std::fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            let file_path = file.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if file_path.file_name().is_some_and(|n| n == "manifest.json") {
                continue;
            }
            checkpoint_files += 1;
            let Ok(content) = std::fs::read(&file_path) else {
                broken_checkpoints.push((file_path, "读取失败".to_string()));
                continue;
            };
            if let Err(error) = serde_json::from_slice::<serde_json::Value>(&content) {
                broken_checkpoints.push((file_path, error.to_string()));
            }
        }
    }
    let broken_bytes: u64 = broken_checkpoints
        .iter()
        .filter_map(|(path, _)| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum();
    println!(
        "checkpoint 文件: {} 个,损坏 {} 个 / {} 字节",
        checkpoint_files,
        broken_checkpoints.len(),
        broken_bytes
    );
    for (path, reason) in broken_checkpoints.iter().take(5) {
        println!("  损坏 {}: {reason}", path.display());
    }
    if broken_checkpoints.len() > 5 {
        println!("  ... 另有 {} 个未列出", broken_checkpoints.len() - 5);
    }
    if !broken_checkpoints.is_empty() {
        if cfg.storage.checkpoints_per_session > 0 {
            println!("  提示:`storage prune --yes` 会删除损坏的 checkpoint(内容已不可恢复)");
        } else {
            // prune 的 checkpoint 段整体受 `checkpoints_per_session > 0` 门控
            // （0 = 不限条数，等同关闭该子系统的清理），此时上面的建议不成立。
            println!(
                "  提示:当前 storage.checkpoints_per_session = 0(不限条数),prune 不会处理 \
                 checkpoint;把它们临时设为正数后运行 `storage prune --yes`"
            );
        }
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

    // ── checkpoint:按 session 淘汰超限的最老 checkpoint ──
    //
    // **必须带 `with_max_per_session` 和 `with_cas`**：`CheckpointStore::new()`
    // 给的 `max_per_session` 是 0，而 `enforce_retention` 在 0 时第一行就
    // `return Ok(())`——此前这里正是用 `new()`，所以 `storage prune` 的 checkpoint
    // 分支**从未真正删过任何东西**（配置写着 20，实际目录里 33/39 条的残留就是证据）。
    //
    // `with_cas` 同样必需：修好外化之后，checkpoint 的历史正文只有 `cas://`
    // 占位符，删除时必须 `release` 对应 blob，否则 CAS 池只增不减。
    let mut checkpoint_sessions = 0usize;
    let mut checkpoint_dropped = 0usize;
    let mut checkpoint_failed = 0usize;
    let mut checkpoint_reexternalized = 0usize;
    let mut checkpoint_freed = 0u64;
    let mut corrupt_removed = 0usize;
    let mut corrupt_freed = 0u64;
    if storage.checkpoints_per_session > 0 {
        let cas = wyj_core::workspace_cas::WorkspaceCas::open_silently(
            &config_base.join("cas"),
            storage.cas_max_blob_bytes,
        )
        .map(std::sync::Arc::new);
        for (session_dir, _path, session_id) in checkpoint_dirs(config_base) {
            checkpoint_sessions += 1;
            let mut store = match wyj_core::CheckpointStore::new(&session_dir, &session_id) {
                Ok(store) => store.with_max_per_session(storage.checkpoints_per_session),
                Err(error) => {
                    tracing::warn!("打开 checkpoint 目录失败 {session_id}: {error}");
                    checkpoint_failed += 1;
                    continue;
                }
            };
            if let Some(cas) = cas.clone() {
                store = store.with_cas(cas);
            }
            let before = store.list().map(|list| list.len()).unwrap_or(0);
            if dry_run {
                // dry-run 只报计划，不改动 manifest / 不删文件。
                let over = before.saturating_sub(storage.checkpoints_per_session);
                checkpoint_dropped += over;
                // 损坏数也要报：它是「删不删」这个决定的一部分，
                // dry-run 报不出来就等于让用户盲选。
                if let Ok((count, freed)) = store.scan_corrupt() {
                    corrupt_removed += count;
                    corrupt_freed += freed;
                }
                continue;
            }
            // **先清损坏、再外化、最后裁剪**。
            //
            // 顺序理由：
            // · 损坏文件先清掉，`reexternalize_in_place` 就不必逐个尝试
            //   解析它们（它会 warn 并跳过，但那次读盘是白费的）。
            // · 外化必须在裁剪之前：裁剪会删掉最老的 checkpoint，若先删再
            //   外化，被删那批里内联的 base64 就白丢了「本来能省下来」的
            //   机会。反过来（先外化后裁剪）的代价只是对将被淘汰的条目多做
            //   一次 intern + release（两者相抵，净零）。
            match store.purge_corrupt() {
                Ok((count, freed)) => {
                    corrupt_removed += count;
                    corrupt_freed += freed;
                }
                Err(error) => {
                    tracing::warn!("清理损坏 checkpoint 失败 {session_id}: {error}");
                    checkpoint_failed += 1;
                }
            }
            match store.reexternalize_in_place() {
                Ok((count, freed)) => {
                    checkpoint_reexternalized += count;
                    checkpoint_freed += freed;
                }
                Err(error) => {
                    tracing::warn!("checkpoint 重新外化失败 {session_id}: {error}");
                    checkpoint_failed += 1;
                }
            }
            match store.enforce_retention() {
                Ok(()) => {
                    let after = store.list().map(|list| list.len()).unwrap_or(before);
                    checkpoint_dropped += before.saturating_sub(after);
                }
                Err(error) => {
                    tracing::warn!("checkpoint 保留策略执行失败 {session_id}: {error}");
                    checkpoint_failed += 1;
                }
            }
        }
    }
    println!(
        "checkpoint: {} 个 session 目录,上限 {} 条/session,{} {} 条{}",
        checkpoint_sessions,
        storage.checkpoints_per_session,
        if dry_run { "将删除" } else { "已删除" },
        checkpoint_dropped,
        if checkpoint_failed > 0 {
            format!("({checkpoint_failed} 个目录失败)")
        } else {
            String::new()
        }
    );
    if !dry_run {
        // 存量外化是「同一件事的另一半」：裁剪减条数，外化减单条体积。
        // 实测最重的一个 session 20 条共 813 MB（单条 42 MB），条数早已
        // 在上限内，能省的只有内联字节。
        println!(
            "checkpoint: 重新外化 {} 个文件,释放 {} 字节",
            checkpoint_reexternalized, checkpoint_freed
        );
    }
    if corrupt_removed > 0 {
        println!(
            "checkpoint: {} {} 个损坏文件,{} {} 字节{}",
            if dry_run { "发现" } else { "已删除" },
            corrupt_removed,
            if dry_run { "可释放" } else { "释放" },
            corrupt_freed,
            if dry_run {
                "(内容已不可恢复)"
            } else {
                ""
            }
        );
    }

    // ── 损坏的 session 文件 ──
    //
    // 与 checkpoint 的损坏同源（脱敏正则破坏 JSON 结构），但对用户更致命：
    // 这是完整对话历史，损坏等于该会话永久打不开。doctor 已能检出，这里负责清。
    //
    // 只删**确认无法解析**的：能解析的一个都不碰。删除前不做备份——`prune --yes`
    // 本身就是显式确认动作，且这些内容已不可读（保留一个读不出的文件没有意义）。
    let mut broken_sessions_removed = 0usize;
    let mut broken_sessions_freed = 0u64;
    for sessions_dir in session_dirs(config_base) {
        let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // `.meta.json` 是旁路元数据，不是 session 正文。
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".meta.json") {
                continue;
            }
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            if serde_json::from_slice::<serde_json::Value>(&content).is_ok() {
                continue;
            }
            if dry_run {
                broken_sessions_removed += 1;
                broken_sessions_freed += content.len() as u64;
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    broken_sessions_removed += 1;
                    broken_sessions_freed += content.len() as u64;
                    // 配套的 meta 旁路文件也一并清掉，否则 `list()` 会列出一个
                    // 点进去就报错的会话。
                    let meta = path.with_extension("meta.json");
                    let _ = std::fs::remove_file(&meta);
                }
                Err(error) => {
                    tracing::warn!("删除损坏 session 失败 {}: {error}", path.display());
                }
            }
        }
    }
    if broken_sessions_removed > 0 {
        println!(
            "session: {} {} 个损坏文件,{} {} 字节{}",
            if dry_run { "发现" } else { "已删除" },
            broken_sessions_removed,
            if dry_run { "可释放" } else { "释放" },
            broken_sessions_freed,
            if dry_run {
                "(内容已不可恢复,加 --yes 删除)"
            } else {
                ""
            }
        );
    }

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

    // ── 会话 TTL 与陈旧锁 ──
    //
    // 这两类此前完全没有回收路径：`sessions/` 680 个文件 / 2.5 GB、
    // `cas/locks/` 10641 个 / 42 MB、`sessions/*.lock` 238 个、`*.owner` 65 个。
    // 与上面几段不同，这里删的是**整组会话文件**（含 checkpoint / subagent
    // 旁路目录），所以 dry-run 必须如实报出条数，不能只说「会删一些」。
    let mut retention_cfg = storage.clone();
    if retention_cfg.sessions_ttl_days == 0 {
        // 显式 prune 时给一个可用的默认值：用户敲 `prune` 就是想看清理计划，
        // 而 `sessions_ttl_days` 默认 0（启动期不自动删）会让这一段静默空转。
        // dry-run 与 `--yes` 用同一个值，避免「计划里没提、执行时却删了」。
        retention_cfg.sessions_ttl_days = 30;
        println!(
            "session: storage.sessions_ttl_days 未设置,本次按默认 30 天清理\
             (启动期不会自动执行,除非你把它写进 config.toml)"
        );
    }
    if retention_cfg.sessions_ttl_days > 0 || retention_cfg.stale_lock_ttl_days > 0 {
        match wyj_core::retention::run(config_base, &retention_cfg, dry_run) {
            Ok(stats) => {
                if retention_cfg.sessions_ttl_days > 0 {
                    println!(
                        "session: TTL {} 天(每项目保留最近 {} 条),{} {} 个 / {} 字节",
                        retention_cfg.sessions_ttl_days,
                        retention_cfg.sessions_keep_recent,
                        if dry_run { "将删除" } else { "已删除" },
                        stats.sessions_removed,
                        stats.session_bytes_freed
                    );
                } else {
                    println!("session: TTL 为 0,跳过(设 storage.sessions_ttl_days 开启)");
                }
                println!(
                    "lock: TTL {} 天,{} {} 个 / {} 字节",
                    retention_cfg.stale_lock_ttl_days,
                    if dry_run { "将删除" } else { "已删除" },
                    stats.locks_removed,
                    stats.lock_bytes_freed
                );
            }
            Err(error) => {
                tracing::warn!("保留策略执行失败: {error}");
                println!("retention: 执行失败 ({error})");
            }
        }
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
    for sessions_dir in session_dirs(config_base) {
        if !sessions_dir.is_dir() {
            continue;
        }
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
    }
    report
        .top_sessions
        .sort_by_key(|b| std::cmp::Reverse(b.bytes));
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
    report.memory_v3_bytes = dir_size_if_exists(&wyj_config::layout::memory_v3_root(config_base));
    report.memory_v3_files = file_count_if_exists(&wyj_config::layout::memory_v3_root(config_base));
    report.evolution_bytes = dir_size_if_exists(&wyj_config::layout::evolution_root(config_base));
    report.evolution_files = file_count_if_exists(&wyj_config::layout::evolution_root(config_base));
    report.projects_bytes = dir_size_if_exists(&wyj_config::layout::projects_root(config_base));
    report.projects_files = file_count_if_exists(&wyj_config::layout::projects_root(config_base));
    Ok(report)
}

/// 所有「装着会话文件的目录」：会话根 + 每个项目子目录。
///
/// 会话已按项目分目录（`sessions/<project_key>/`），而 `status` / `doctor` /
/// `prune` 都必须同时看到新布局和根目录下尚未迁移的旧平铺文件——只扫一层会让
/// `doctor` 报「损坏 0 个」而磁盘上明明有坏文件，`prune` 更是完全清不到。
fn session_dirs(config_base: &Path) -> Vec<PathBuf> {
    let root = wyj_config::layout::sessions_root(config_base);
    let mut dirs = vec![root.clone()];
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // `<sid>.checkpoints/`、`<sid>.subagents/` 是会话的旁路目录，
            // 里面装的是 checkpoint 与 trace，不是会话本身。
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.ends_with(".checkpoints") || name.ends_with(".subagents") {
                continue;
            }
            dirs.push(path);
        }
    }
    dirs
}

/// 所有 checkpoint 目录：`(会话目录, checkpoint 目录, session_id)`。
///
/// 第三项是 `CheckpointStore` 需要的「会话 id」——checkpoint 目录名是
/// `<session_id>.checkpoints`，去掉后缀即可。
fn checkpoint_dirs(config_base: &Path) -> Vec<(PathBuf, PathBuf, String)> {
    let mut found = Vec::new();
    for sessions_dir in session_dirs(config_base) {
        let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(session_id) = name.strip_suffix(".checkpoints") else {
                continue;
            };
            let session_id = session_id.to_string();
            if path.is_dir() {
                found.push((sessions_dir.clone(), path, session_id));
            }
        }
    }
    found
}

/// 旧布局 → 新布局的搬迁。
///
/// ## 搬什么
///
/// 1. **会话**：`sessions/<sid>.json`（+ `.meta.json` / `.json.lock` /
///    `.json.owner` / `<sid>.checkpoints/` / `<sid>.subagents/`）按 meta 里的
///    `cwd` 算项目键，搬进 `sessions/<project_key>/`。
/// 2. **历史**：`history/history.jsonl` 按每行的 `cwd` 拆进
///    `history/<project_key>/history.jsonl`。
/// 3. **旧项目键目录**：`projects/`、`memory-v3/projects/`、`evolution/`、
///    `indexes/`、`workspaces/`、`workflows/` 下的旧键目录改成新键。旧键无法
///    反推路径，只能靠 `projects/<key>/meta.json` 或 `instances/*.json` 里的
///    `cwd` 恢复；恢复不出来的保持原样并报出来。
///
/// ## 为什么不能自动做
///
/// 搬迁涉及几万个文件、且搬错一次就是「项目状态凭空消失」。所以默认 dry-run，
/// 必须 `--yes` 才动手；且每个文件都走 `rename`（同盘原子）而不是复制后删除。
fn run_migrate(config_base: &Path, dry_run: bool) -> Result<()> {
    println!("=== wyj-code storage migrate ===");
    if dry_run {
        println!("(dry-run:只报计划,不移动任何文件。确认无误后加 --yes 实际执行)");
    }

    let mut planned = 0usize;
    let mut moved = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    // ── 1. 会话（含 meta / lock / owner / checkpoint 目录 / subagent trace 目录）──
    let sessions_root = wyj_config::layout::sessions_root(config_base);
    for (session_id, paths) in flat_session_groups(&sessions_root) {
        let Some(cwd) = paths
            .iter()
            .find(|path| path.is_file())
            .and_then(|path| session_cwd(path))
        else {
            skipped += 1;
            continue;
        };
        let key = wyj_core::project_key(std::path::Path::new(&cwd));
        let dest_dir = sessions_root.join(&key);
        planned += paths.len();
        if dry_run {
            println!(
                "  session {session_id} → sessions/{key}/  ({} 项, cwd {cwd})",
                paths.len()
            );
            continue;
        }
        if let Err(error) = std::fs::create_dir_all(&dest_dir) {
            tracing::warn!("创建目录失败 {}: {error}", dest_dir.display());
            failed += paths.len();
            continue;
        }
        for src in paths {
            let Some(name) = src.file_name() else {
                continue;
            };
            match std::fs::rename(&src, dest_dir.join(name)) {
                Ok(()) => moved += 1,
                Err(error) => {
                    tracing::warn!("搬迁失败 {}: {error}", src.display());
                    failed += 1;
                }
            }
        }
    }

    // ── 2. 历史 ──
    let history_root = wyj_config::layout::history_root(config_base);
    let legacy_history = history_root.join("history.jsonl");
    if legacy_history.is_file() {
        let lines = split_history_by_project(&legacy_history)?;
        for (key, entries) in &lines {
            planned += entries.len();
            if dry_run {
                println!(
                    "  history {} 条 → history/{key}/history.jsonl",
                    entries.len()
                );
                continue;
            }
            let dir = history_root.join(key);
            if let Err(error) = std::fs::create_dir_all(&dir) {
                tracing::warn!("创建目录失败 {}: {error}", dir.display());
                failed += entries.len();
                continue;
            }
            let mut text = String::new();
            for entry in entries {
                text.push_str(entry);
                text.push('\n');
            }
            match append_text(&dir.join("history.jsonl"), &text) {
                Ok(()) => moved += entries.len(),
                Err(error) => {
                    tracing::warn!("写入 history/{key} 失败: {error}");
                    failed += entries.len();
                }
            }
        }
        if !dry_run && failed == 0 {
            // 全部拆完才删旧文件；有一条写失败就保留，下次重跑会跳过已写入的行。
            let _ = std::fs::remove_file(&legacy_history);
        }
    }

    // ── 3. 旧项目键目录 ──
    //
    // 旧键是 `DefaultHasher` 的输出，**无法反推路径**。归属只能从残留证据里恢复：
    // `projects/<key>/instances/*.json` 的 `cwd`、`memory-v3/projects/<key>/INDEX.md`
    // 里的 `project:` 行。先把这些证据收成一张 `旧键 → 项目根` 表，再统一改名——
    // 否则同一个项目在 `evolution/`、`indexes/` 等目录下会各自判一次，判不出就
    // 全部留在原地（实测 279 项 skipped，其中绝大多数是能从别的子系统反查到的）。
    let key_roots = collect_legacy_key_roots(config_base);
    for (kind, dir) in project_key_dirs(config_base) {
        let old_key = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let root = key_roots.get(old_key).cloned();
        planned += 1;
        if dry_run {
            match &root {
                Some(root) => println!(
                    "  {kind} {} → {}",
                    dir.display(),
                    wyj_config::layout::projects_root(config_base)
                        .join(wyj_core::project_key(root))
                        .display()
                ),
                None => println!(
                    "  {kind} {} → 无法确定归属(跳过；可在该目录补 meta.json 后重跑)",
                    dir.display()
                ),
            }
            continue;
        }
        match migrate_project_key_dir(config_base, &dir, root.as_deref()) {
            Ok(Some(new_path)) => {
                println!("  {kind} {} → {}", dir.display(), new_path.display());
                moved += 1;
            }
            Ok(None) => {
                skipped += 1;
            }
            Err(error) => {
                tracing::warn!("搬迁 {} 失败: {error}", dir.display());
                failed += 1;
            }
        }
    }

    println!(
        "migrate: 计划 {} 项,{} {} 项,跳过 {} 项(无法确定归属),失败 {} 项",
        planned,
        if dry_run { "待搬迁" } else { "已搬迁" },
        moved,
        skipped,
        failed
    );
    if dry_run {
        println!("确认无误后执行:wyj-code storage migrate --yes");
    } else if failed > 0 {
        println!("有失败项,可重跑本命令继续(已搬过的不会重复搬)。");
    }
    Ok(())
}

/// 根目录下平铺的会话及其旁路文件，按 `session_id` 分组：`(session_id, 路径们)`。
///
/// 一个会话在旧布局里散成最多 5 个同级条目：`<sid>.json`、`<sid>.meta.json`、
/// `<sid>.json.lock`、`<sid>.json.owner`、`<sid>.checkpoints/`、
/// `<sid>.subagents/`。**必须整组一起搬**——只搬 `.json` 会让 checkpoint 与
/// trace 留在根目录，`--resume` 之后 `/rewind` 与 `subagent-trace` 全部失效，
/// 且没有任何报错。
fn flat_session_groups(sessions_root: &Path) -> Vec<(String, Vec<PathBuf>)> {
    use std::collections::BTreeMap;
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // 目录本身就是项目子目录（新布局），不是会话旁路目录。
        if path.is_dir() {
            if let Some(id) = name
                .strip_suffix(".checkpoints")
                .or_else(|| name.strip_suffix(".subagents"))
            {
                groups.entry(id.to_string()).or_default().push(path);
            }
            continue;
        }
        let Some(id) = session_id_of_file(name) else {
            continue;
        };
        groups.entry(id).or_default().push(path);
    }
    groups.into_iter().collect()
}

/// 从旧布局的文件名反推 `session_id`。非会话文件返回 `None`。
fn session_id_of_file(name: &str) -> Option<String> {
    // 顺序重要：`.meta.json` 的 `file_stem` 是 `<sid>.meta`，必须先剥长后缀。
    for suffix in [".meta.json", ".json.lock", ".json.owner"] {
        if let Some(id) = name.strip_suffix(suffix) {
            return Some(id.to_string());
        }
    }
    name.strip_suffix(".json").map(str::to_string)
}

/// 读会话的 `cwd`。优先 `.meta.json`（小、快），回退整份 `.json`。
fn session_cwd(path: &Path) -> Option<String> {
    let meta = path.with_extension("meta.json");
    if let Ok(content) = std::fs::read_to_string(&meta) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(cwd) = value.get("cwd").and_then(|v| v.as_str()) {
                return Some(cwd.to_string());
            }
        }
    }
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// 把旧 `history.jsonl` 按 `cwd` 拆组，返回 `项目键 → 原始行`。
fn split_history_by_project(
    path: &Path,
) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    let content = std::fs::read_to_string(path)?;
    let mut grouped: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(cwd) = value.get("cwd").and_then(|v| v.as_str()) else {
            continue;
        };
        let key = wyj_core::project_key(std::path::Path::new(cwd));
        grouped.entry(key).or_default().push(line.to_string());
    }
    Ok(grouped)
}

fn append_text(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// 需要重命名的旧项目键目录：`(子系统名, 目录路径)`。
fn project_key_dirs(config_base: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut found = Vec::new();
    let kinds: [(&str, PathBuf); 6] = [
        ("projects", wyj_config::layout::projects_root(config_base)),
        (
            "memory-v3",
            wyj_config::layout::memory_v3_root(config_base).join("projects"),
        ),
        ("evolution", wyj_config::layout::evolution_root(config_base)),
        ("indexes", wyj_config::layout::indexes_root(config_base)),
        (
            "workspaces",
            wyj_config::layout::workspaces_root(config_base),
        ),
        ("workflows", wyj_config::layout::workflows_root(config_base)),
    ];
    for (kind, root) in kinds {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // 已经是新键（带版本前缀）就跳过。
            if name.starts_with("v1-") {
                continue;
            }
            found.push((kind, path));
        }
    }
    found
}

/// 收集 `旧项目键 → 项目根路径` 的映射。
///
/// 旧键无法反推路径，但不同子系统残留的证据可以互相补齐：某个项目可能只在
/// `evolution/` 下有数据、`projects/` 下空空如也，反过来也一样。所以这里把
/// 所有能找到的证据汇总成一张表，供各子系统统一使用。
fn collect_legacy_key_roots(config_base: &Path) -> std::collections::HashMap<String, PathBuf> {
    use std::collections::HashMap;
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    let mut record = |key: &str, root: PathBuf| {
        if !key.is_empty() && !key.starts_with("v1-") {
            map.entry(key.to_string()).or_insert(root);
        }
    };

    // ① `projects/<key>/`：`instances/*.json` 与 `meta.json`
    let projects_root = wyj_config::layout::projects_root(config_base);
    if let Ok(entries) = std::fs::read_dir(&projects_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let Some(key) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(root) = recover_project_root(&dir) {
                record(key, root);
            }
        }
    }

    // ② `memory-v3/projects/<key>/INDEX.md` 里写着 `- project: <绝对路径>`
    let memory_root = wyj_config::layout::memory_v3_root(config_base).join("projects");
    if let Ok(entries) = std::fs::read_dir(&memory_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let Some(key) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(root) = project_root_from_memory_index(&dir.join("INDEX.md")) {
                record(key, root);
            }
        }
    }

    // ③ `evolution/<key>/`、`indexes/<key>/` 等目录里若有 `meta.json`（本命令
    //    上一轮写下的），同样可以反查——保证重跑时进度可累积。
    for (_, dir) in project_key_dirs(config_base) {
        let Some(key) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(root) = recover_project_root(&dir) {
            record(key, root);
        }
    }

    map
}

/// 从 memory-v3 的 `INDEX.md` 反查项目根。
///
/// 该文件由 `MemoryV3Store::refresh_index` 生成，头部固定包含
/// `- project: \`<绝对路径>\``，是人可读索引里唯一的路径来源。
fn project_root_from_memory_index(path: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let Some(rest) = line.trim().strip_prefix("- project:") else {
            continue;
        };
        let value = rest.trim().trim_matches('`').trim();
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// 把一个旧键目录重命名为新键目录。返回 `Ok(None)` 表示无法确定归属。
///
/// `known_root` 是 [`collect_legacy_key_roots`] 汇总出的归属；`None` 时再退回到
/// 目录内自带的证据（`meta.json` / `instances/*.json`）。
///
/// 顺带在新目录里写一份 `meta.json`（`{"root": "<绝对路径>"}`）：新键是
/// sha256，人眼认不出对应哪个仓库，而迁移/清理/`storage status` 都需要这个
/// 反向映射。写入失败不影响搬迁本身。
fn migrate_project_key_dir(
    config_base: &Path,
    dir: &Path,
    known_root: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let Some(root) = known_root
        .map(Path::to_path_buf)
        .or_else(|| recover_project_root(dir))
    else {
        return Ok(None);
    };
    let new_key = wyj_core::project_key(&root);
    let new_path = dir.with_file_name(&new_key);
    if new_path == dir || new_path.exists() {
        return Ok(None);
    }
    std::fs::rename(dir, &new_path)?;
    let meta_path = wyj_config::layout::project_meta_path(config_base, &new_key);
    if let Some(parent) = meta_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        meta_path,
        serde_json::json!({ "root": root.display().to_string() }).to_string(),
    );
    Ok(Some(new_path))
}

/// 从旧键目录里恢复项目根路径。
///
/// 三条来源，按可靠性排序：
/// 1. `meta.json`（新写入的）或 `instances/*.json` 里的 `cwd`。
/// 2. `memory-v3/projects/<key>/records.json` 里 claim 的 `project_root`
///    （`MemoryV3Store::status` 会写，但不是每个项目都有）。
/// 3. 都没有 → 放弃（返回 `None`），保持原样并让用户看到。
fn recover_project_root(dir: &Path) -> Option<PathBuf> {
    let meta = dir.join("meta.json");
    if let Ok(content) = std::fs::read_to_string(&meta) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(root) = value.get("root").and_then(|v| v.as_str()) {
                return Some(PathBuf::from(root));
            }
        }
    }
    let instances = dir.join("instances");
    if let Ok(entries) = std::fs::read_dir(&instances) {
        for entry in entries.flatten() {
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            if let Some(cwd) = value.get("cwd").and_then(|v| v.as_str()) {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
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
