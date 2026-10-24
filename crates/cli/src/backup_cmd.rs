//! `wyj-code export` / `wyj-code import`：配置备份的 CLI 入口。
//!
//! 数据层的合并规则与归档格式全在 `wyj_store::bundle`，这里只负责三件事：
//! 参数校验、**面向用户的确认与输出**（含明文密钥的醒目提示）、以及把
//! "写到哪里"解析成生产路径。
//!
//! # 为什么导出要警告、导入要确认
//!
//! 导出包默认带明文 `api_key`（换机场景下这是它的全部价值），所以导出成功后
//! 必须**明确告诉用户这个文件等同于他的凭据**，并说明它已按 `0600` 收紧。
//!
//! 导入是唯一会改 `config.toml` 与 `~/.wyj-code/skills/` 的操作，因此默认走
//! dry-run（只报计划），加 `--yes` 才真写——与 `storage prune` / `storage
//! migrate` 的既有约定一致，用户不需要为此记两套手感。dry-run 结束后若在真
//! 终端里，会顺手问一句 y/N，避免"看完计划还得再敲一遍完整命令"。

use anyhow::Result;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use wyj_config::Config;

/// `wyj-code export <dest>`。
pub fn run_export(cfg: &Config, home: &Path, dest: &Path, force: bool) -> Result<()> {
    if dest.exists() && !force {
        anyhow::bail!(
            "{} 已存在，未覆盖。确认要替换上一份备份请加 --force",
            dest.display()
        );
    }
    let report = wyj_store::bundle::export(
        cfg,
        home,
        dest,
        env!("CARGO_PKG_VERSION"),
        &wyj_core::now_iso(),
    )?;

    println!(
        "{}",
        wyj_i18n::tr_fmt(
            "backup.export_done",
            &[
                ("path", &report.path.display().to_string()),
                ("size", &human_bytes(report.bytes)),
                ("profiles", &report.manifest.profiles.len().to_string()),
                ("skills", &report.manifest.skills.len().to_string()),
            ]
        )
    );
    if !report.manifest.profiles.is_empty() {
        println!(
            "  {}",
            wyj_i18n::tr_fmt(
                "backup.export_profiles",
                &[("names", &report.manifest.profiles.join(", "))]
            )
        );
    }
    if !report.manifest.skills.is_empty() {
        println!(
            "  {}",
            wyj_i18n::tr_fmt(
                "backup.export_skills",
                &[("names", &report.manifest.skills.join(", "))]
            )
        );
    }
    // 安全提示放在最后、单独成段：这是导出唯一需要用户真正记住的一句话。
    if !report.profiles_with_keys.is_empty() {
        println!();
        println!(
            "{}",
            wyj_i18n::tr_fmt(
                "backup.export_secrets_warning",
                &[("names", &report.profiles_with_keys.join(", "))]
            )
        );
    }
    Ok(())
}

/// `wyj-code import <src>`。
///
/// `dry_run` 由调用方算好（`--yes` 覆盖 `--dry-run`），本函数不再判断标志。
pub fn run_import(cfg: &mut Config, src: &Path, dry_run: bool) -> Result<()> {
    // 先只读检查一遍：包不对时立刻报错，别让用户以为"跑了就等于恢复了"。
    let manifest = wyj_store::bundle::inspect(src)?;
    println!(
        "{}",
        wyj_i18n::tr_fmt(
            "backup.import_source",
            &[
                ("version", &manifest.app_version),
                ("time", &manifest.exported_at),
                ("host", dash_if_empty(&manifest.hostname)),
            ]
        )
    );
    if manifest.contains_secrets {
        println!("{}", wyj_i18n::tr("backup.import_contains_secrets"));
    }

    let targets = wyj_store::bundle::ImportTargets::production()?;
    let report = wyj_store::bundle::import(src, cfg, &targets, dry_run)?;
    println!();
    print_report(&report);

    if dry_run {
        println!();
        println!("{}", wyj_i18n::tr("backup.dry_run_hint"));
        // 交互终端里顺手问一句；脚本 / cron / 管道一律停留在 dry-run，
        // 在非交互环境里等确认等于挂死。
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            return confirm_and_apply(cfg, src, &targets);
        }
        return Ok(());
    }
    println!();
    println!("{}", wyj_i18n::tr("backup.import_done"));
    Ok(())
}

fn print_report(report: &wyj_store::bundle::ImportReport) {
    for name in &report.profiles_added {
        println!(
            "  {}",
            wyj_i18n::tr_fmt("backup.profile_added", &[("name", name)])
        );
    }
    for name in &report.profiles_replaced {
        println!(
            "  {}",
            wyj_i18n::tr_fmt("backup.profile_replaced", &[("name", name)])
        );
    }
    if let Some((old, new)) = &report.active_profile_change {
        println!(
            "  {}",
            wyj_i18n::tr_fmt(
                "backup.active_profile_switched",
                &[("old", old), ("new", new)]
            )
        );
    }
    for name in &report.skills_added {
        println!(
            "  {}",
            wyj_i18n::tr_fmt("backup.skill_added", &[("name", name)])
        );
    }
    for name in &report.skills_skipped {
        println!(
            "  {}",
            wyj_i18n::tr_fmt("backup.skill_skipped", &[("name", name)])
        );
    }
    if report.lockfile_entries > 0 {
        println!(
            "  {}",
            wyj_i18n::tr_fmt(
                "backup.skill_meta_merged",
                &[("count", &report.lockfile_entries.to_string())]
            )
        );
    }
    if report.profiles_added.is_empty()
        && report.profiles_replaced.is_empty()
        && report.skills_added.is_empty()
        && report.lockfile_entries == 0
    {
        println!("{}", wyj_i18n::tr("backup.import_nothing_to_do"));
    }
}

/// dry-run 后的人机确认。只在真终端里调用。
///
/// 确认后**重读配置**再写：dry-run 期间磁盘可能已被另一个实例改过，用新基线
/// 才符合 `Config::save_to` 的三方合并语义。
fn confirm_and_apply(
    cfg: &mut Config,
    src: &Path,
    targets: &wyj_store::bundle::ImportTargets,
) -> Result<()> {
    print!("{}", wyj_i18n::tr("backup.confirm_prompt"));
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    if !matches!(input.trim(), "y" | "Y") {
        println!("{}", wyj_i18n::tr("backup.cancelled"));
        return Ok(());
    }
    *cfg = Config::load()?;
    wyj_store::bundle::import(src, cfg, targets, false)?;
    println!("{}", wyj_i18n::tr("backup.import_done"));
    Ok(())
}

fn dash_if_empty(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
