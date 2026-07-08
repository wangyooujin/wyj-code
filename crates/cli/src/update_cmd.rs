//! `wyj-code update` 子命令编排：调用 `wyj_store::self_update` 的纯逻辑，
//! 负责所有面向用户的文案输出（i18n）与 y/N 交互式确认。

use anyhow::Result;
use std::io::{self, BufRead, Write};
use wyj_store::self_update;

const RELEASE_NOTES_MAX_LINES: usize = 15;

pub async fn run(yes: bool) -> Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");

    println!("{}", wyj_i18n::tr("update.checking"));

    let client = reqwest::Client::builder()
        .user_agent(format!("wyj-code/{current_version}"))
        .build()?;

    let release = match self_update::fetch_latest_release(&client).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("update.network_error", &[("err", &e.to_string())])
            );
            std::process::exit(1);
        }
    };

    let remote_version = release.version();
    let is_newer = match self_update::is_newer(remote_version, current_version) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("update.network_error", &[("err", &e.to_string())])
            );
            std::process::exit(1);
        }
    };

    if !is_newer {
        println!(
            "{}",
            wyj_i18n::tr_fmt("update.up_to_date", &[("version", current_version)])
        );
        return Ok(());
    }

    println!(
        "{}",
        wyj_i18n::tr_fmt(
            "update.available",
            &[("current", current_version), ("latest", remote_version)]
        )
    );
    let notes = release.body.trim();
    if !notes.is_empty() {
        println!("{}", wyj_i18n::tr("update.release_notes_header"));
        for line in notes.lines().take(RELEASE_NOTES_MAX_LINES) {
            println!("  {line}");
        }
    }

    if !yes {
        print!("{}", wyj_i18n::tr("update.confirm_prompt"));
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;
        let confirmed = matches!(input.trim(), "y" | "Y");
        if !confirmed {
            println!("{}", wyj_i18n::tr("update.cancelled"));
            return Ok(());
        }
    }

    let target = match self_update::current_target() {
        Some(t) => t,
        None => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt(
                    "update.unsupported_platform",
                    &[
                        ("os", std::env::consts::OS),
                        ("arch", std::env::consts::ARCH)
                    ]
                )
            );
            std::process::exit(1);
        }
    };

    let (archive_name, sha256_name) = self_update::asset_names(remote_version, target);
    let archive_asset = release.asset(&archive_name);
    let sha256_asset = release.asset(&sha256_name);
    let (archive_asset, sha256_asset) = match (archive_asset, sha256_asset) {
        (Some(a), Some(s)) => (a, s),
        _ => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("update.asset_not_found", &[("target", target)])
            );
            std::process::exit(1);
        }
    };

    println!(
        "{}",
        wyj_i18n::tr_fmt("update.downloading", &[("name", &archive_asset.name)])
    );
    let archive_bytes = match self_update::download_and_verify(
        &client,
        &archive_asset.browser_download_url,
        &sha256_asset.browser_download_url,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("update.checksum_mismatch", &[("err", &e.to_string())])
            );
            std::process::exit(1);
        }
    };

    println!("{}", wyj_i18n::tr("update.extracting"));
    let binary_bytes = match self_update::extract_binary(&archive_bytes, target) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("update.extract_error", &[("err", &e.to_string())])
            );
            std::process::exit(1);
        }
    };

    println!("{}", wyj_i18n::tr("update.replacing"));
    let replaced_path = match self_update::atomic_replace_current_exe(&binary_bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}",
                wyj_i18n::tr_fmt("update.permission_error", &[("err", &e.to_string())])
            );
            std::process::exit(1);
        }
    };

    println!(
        "{}",
        wyj_i18n::tr_fmt(
            "update.success",
            &[
                ("version", remote_version),
                ("path", &replaced_path.display().to_string())
            ]
        )
    );
    Ok(())
}
