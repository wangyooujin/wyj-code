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
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".checkpoints") {
                continue;
            }
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
    let sessions_dir = config_base.join("sessions");

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
                let mut store = match wyj_core::CheckpointStore::new(&sessions_dir, session_id) {
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
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
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
