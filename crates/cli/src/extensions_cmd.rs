use anyhow::Result;
use clap::Subcommand;
use std::path::Path;
use wyj_store::extensions;
use wyj_store::import;
use wyj_store::InstallScope;

#[derive(Subcommand, Debug)]
pub enum ExtensionCommand {
    /// List installed, unmanaged and disabled resources.
    List {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install a resource. Non-interactive installs require --yes.
    Install {
        kind: String,
        reference: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value = "project")]
        scope: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Upgrade an installed resource from its recorded source.
    Upgrade {
        id: String,
        #[arg(long, default_value = "project")]
        scope: String,
        #[arg(long)]
        json: bool,
    },
    /// Diagnose configuration, migration and resource health.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Import Codex / Claude Code configuration (MCP servers, custom commands,
    /// agents) into wyj-code without deleting the source files.
    Migrate {
        /// Source app to import from: codex | claude | all.
        #[arg(long, default_value = "all")]
        from: String,
        /// Scan and list importable items without writing anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Enable a resource, for example mcp:postgres or plugin:reviewer.
    Enable {
        id: String,
        #[arg(long, default_value = "project")]
        scope: String,
        #[arg(long)]
        json: bool,
    },
    /// Disable a resource.
    Disable {
        id: String,
        #[arg(long, default_value = "project")]
        scope: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a resource from the selected scope.
    Remove {
        id: String,
        #[arg(long, default_value = "project")]
        scope: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(command: ExtensionCommand, cwd: &Path) -> Result<()> {
    match command {
        ExtensionCommand::List { kind, json } => {
            let mut records = extensions::list(cwd)?;
            if let Some(kind) = kind {
                records.retain(|record| format!("{:?}", record.kind).eq_ignore_ascii_case(&kind));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else if records.is_empty() {
                println!("No extensions found.");
            } else {
                for record in records {
                    println!(
                        "{:<32} {:<8} {:<8} {:<10} {}",
                        record.id,
                        format!("{:?}", record.scope).to_lowercase(),
                        if record.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        record.health,
                        record.version.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        ExtensionCommand::Doctor { json } => {
            let report = extensions::doctor(cwd)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Extensions doctor");
                println!("  resources: {}", report.records.len());
                println!("  issues: {}", report.issues.len());
                for issue in report.issues {
                    println!("  [{}] {}: {}", issue.severity, issue.code, issue.message);
                    if let Some(remediation) = issue.remediation {
                        println!("      -> {remediation}");
                    }
                }
            }
        }
        ExtensionCommand::Install {
            kind,
            reference,
            source,
            scope,
            yes,
            json,
        } => {
            if !yes {
                anyhow::bail!("resource installation can execute external commands; re-run with --yes after reviewing the source");
            }
            let scope = parse_scope(&scope)?;
            let report = match kind.to_ascii_lowercase().as_str() {
                "skill" | "skills" => {
                    let marketplace_url = source.ok_or_else(|| {
                        anyhow::anyhow!("skill install requires --source <marketplace-git-url>")
                    })?;
                    let entries =
                        wyj_store::marketplace::sync_marketplace(&marketplace_url).await?;
                    let entry = entries
                        .into_iter()
                        .find(|entry| entry.name == reference || entry.path == reference)
                        .ok_or_else(|| {
                            anyhow::anyhow!("skill not found in marketplace: {reference}")
                        })?;
                    let req = wyj_store::skill_install::SkillInstallRequest {
                        marketplace_id: wyj_store::marketplace::marketplace_id(&marketplace_url),
                        marketplace_url,
                        entry,
                        scope,
                        name_override: None,
                    };
                    wyj_store::skill_install::install_skill(&req, cwd)?;
                    serde_json::json!({"kind":"skill","id":format!("skill:{}", req.entry.name),"version":req.entry.version})
                }
                "mcp" => {
                    let registry_url = source
                        .unwrap_or_else(|| wyj_store::registry::REGISTRY_BASE_URL.to_string());
                    let client = wyj_store::registry::RegistryClient::new(registry_url.clone());
                    let server = client
                        .search_servers(&reference)
                        .await?
                        .into_iter()
                        .find(|server| {
                            server.name == reference
                                || wyj_store::mcp_install::default_server_short_name(&server.name)
                                    == reference
                        })
                        .ok_or_else(|| anyhow::anyhow!("MCP server not found: {reference}"))?;
                    let package = wyj_store::mcp_install::choose_package(&server.packages);
                    let name = wyj_store::mcp_install::default_server_short_name(&server.name);
                    let version = package.package_version().unwrap_or_default().to_string();
                    let req = wyj_store::mcp_install::McpInstallRequest {
                        server,
                        package,
                        scope,
                        name_override: None,
                        registry_url,
                    };
                    wyj_store::mcp_install::install_mcp_server(&req, cwd)?;
                    serde_json::json!({"kind":"mcp","id":format!("mcp:{name}"),"version":version})
                }
                "plugin" | "plugins" => {
                    let marketplace_url = source.ok_or_else(|| {
                        anyhow::anyhow!("plugin install requires --source <marketplace-git-url>")
                    })?;
                    let marketplace_id =
                        wyj_store::plugin_install::plugin_marketplace_id(&marketplace_url);
                    wyj_store::plugin_install::add_plugin_marketplace(&marketplace_url)?;
                    let manifest =
                        wyj_store::plugin_install::sync_plugin_marketplace(&marketplace_id).await?;
                    let entry = manifest
                        .plugins
                        .iter()
                        .find(|entry| entry.manifest.name.as_deref() == Some(reference.as_str()))
                        .cloned()
                        .ok_or_else(|| {
                            anyhow::anyhow!("plugin not found in marketplace: {reference}")
                        })?;
                    let report = wyj_store::plugin_install::resolve_and_install_from_marketplace(
                        &marketplace_id,
                        &marketplace_url,
                        &entry,
                        scope,
                        None,
                        cwd,
                    )
                    .await?;
                    serde_json::json!({"kind":"plugin","id":format!("plugin:{}",report.name),"version":report.version})
                }
                _ => anyhow::bail!("kind must be skill, mcp or plugin"),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("installed {}", report);
            }
        }
        ExtensionCommand::Upgrade { id, scope, json } => {
            let scope = parse_scope(&scope)?;
            let (kind, name) = id
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("resource ID must be kind:name"))?;
            let outcome = match kind {
                "skill" => wyj_store::skill_install::upgrade_skill(name, scope, cwd).await?,
                "mcp" => wyj_store::mcp_install::upgrade_mcp_server(name, scope, cwd).await?,
                "plugin" => wyj_store::plugin_install::upgrade_plugin(name, scope, cwd).await?,
                _ => anyhow::bail!("only skill, mcp and plugin resources can be upgraded"),
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({"id":id,"outcome":format!("{:?}",outcome),"version":outcome.version()})
                );
            } else {
                println!("{id}: {:?}", outcome);
            }
        }
        ExtensionCommand::Migrate {
            from,
            dry_run,
            json,
        } => {
            let filter = match from.to_ascii_lowercase().as_str() {
                "codex" => import::ImportFilter::Codex,
                "claude" => import::ImportFilter::Claude,
                "all" => import::ImportFilter::All,
                _ => anyhow::bail!("--from must be codex, claude or all, got {from}"),
            };
            let targets = import::ImportTargets::from_real_home(cwd)?;
            let scan = import::scan_importable(&targets, filter)?;
            if dry_run {
                if json {
                    println!("{}", serde_json::to_string_pretty(&scan)?);
                } else if scan.candidates.is_empty() {
                    println!("Nothing to import.");
                } else {
                    for c in &scan.candidates {
                        let mut flags = Vec::new();
                        if c.conflict.is_some() {
                            flags.push("conflict");
                        }
                        if c.shadowed {
                            flags.push("shadowed");
                        }
                        println!(
                            "{:<8} {:<32} {:<8} {:<24} {}",
                            c.kind.as_str(),
                            c.name,
                            format!("{:?}", c.scope).to_lowercase(),
                            flags.join(","),
                            c.source_path.display()
                        );
                    }
                }
                for error in &scan.errors {
                    eprintln!("Scan error: {error}");
                }
                return Ok(());
            }
            // 非交互语义：同名冲突项一律跳过并列出，想覆盖请去 TUI /import 勾选。
            let (selected, skipped): (Vec<_>, Vec<_>) = scan
                .candidates
                .into_iter()
                .partition(|c| c.conflict.is_none());
            let outcome = import::apply_import(&targets, &selected)?;
            if json {
                let skipped_labels: Vec<String> = skipped
                    .iter()
                    .map(|c| format!("{}:{}", c.kind.as_str(), c.name))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "outcome": outcome,
                        "skipped_conflicts": skipped_labels,
                        "scan_errors": scan.errors,
                        "runtime_apply": "next_agent_boundary"
                    })
                );
            } else {
                println!("Imported: {}", outcome.applied.join(", "));
                if !skipped.is_empty() {
                    println!(
                        "Skipped conflicting items (use TUI /import to overwrite): {}",
                        skipped
                            .iter()
                            .map(|c| format!("{}:{}", c.kind.as_str(), c.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if !outcome.shadow_warnings.is_empty() {
                    println!("Shadowed by live Claude Code files (imported copy takes effect only after the original is removed):");
                    for warning in &outcome.shadow_warnings {
                        println!("  {warning}");
                    }
                }
                for error in scan.errors.iter().chain(outcome.errors.iter()) {
                    eprintln!("Migration error: {error}");
                }
                println!("Changes apply at the next agent boundary.");
            }
        }
        ExtensionCommand::Enable { id, scope, json } => {
            let scope = parse_scope(&scope)?;
            extensions::set_enabled(&id, scope, cwd, true)?;
            emit_mutation("enabled", &id, scope, json)?;
        }
        ExtensionCommand::Disable { id, scope, json } => {
            let scope = parse_scope(&scope)?;
            extensions::set_enabled(&id, scope, cwd, false)?;
            emit_mutation("disabled", &id, scope, json)?;
        }
        ExtensionCommand::Remove { id, scope, json } => {
            let scope = parse_scope(&scope)?;
            extensions::remove(&id, scope, cwd)?;
            emit_mutation("removed", &id, scope, json)?;
        }
    }
    Ok(())
}

fn parse_scope(scope: &str) -> Result<InstallScope> {
    match scope.to_ascii_lowercase().as_str() {
        "global" => Ok(InstallScope::Global),
        "project" => Ok(InstallScope::Project),
        _ => anyhow::bail!("scope must be global or project, got {scope}"),
    }
}

fn emit_mutation(action: &str, id: &str, scope: InstallScope, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": extensions::EXTENSIONS_SCHEMA_VERSION,
                "action": action,
                "id": id,
                "scope": format!("{:?}", scope).to_lowercase(),
                "runtime_apply": "next_agent_boundary"
            })
        );
    } else {
        println!(
            "{action} {id} ({}) — applies at the next agent boundary",
            format!("{:?}", scope).to_lowercase()
        );
    }
    Ok(())
}
