use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use wyj_config::Config;
use wyj_core::{
    CandidatePayload, CandidateStatus, CheckpointKind, CheckpointStore, EvolutionStore,
};
use wyj_store::lockfile::InstallScope;

#[derive(Subcommand, Debug)]
pub enum EvolveCommand {
    /// Show the current project's Evolution summary.
    Status,
    /// List active/proposed Memories, candidates, or Episodes.
    List {
        #[arg(default_value = "all", value_parser = ["all", "memories", "candidates", "episodes"])]
        target: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one Memory, candidate, or Episode with its evidence.
    Review { id: String },
    /// Record explicit user acceptance or rejection for an Episode.
    Feedback {
        #[arg(value_parser = ["good", "bad"])]
        sentiment: String,
        episode_id: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Turn a successful Episode into a manually reviewed Skill candidate.
    Skillize { episode_id: String },
    /// Activate a Memory/Rule or install a validated Skill after a protection checkpoint.
    Approve {
        id: String,
        #[arg(long, default_value = "project", value_parser = ["project", "global"])]
        scope: String,
    },
    /// Reject a Rule/Skill candidate.
    Reject {
        candidate_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Roll back an active Rule or generated Skill.
    Rollback { candidate_id: String },
    /// Forget a Memory so it can no longer be injected.
    Forget { memory_id: String },
    /// Immediately analyze an Episode instead of waiting for the idle worker.
    Run { episode_id: Option<String> },
    /// Explicitly include an externally sourced Episode in repository-scope learning.
    Include { episode_id: String },
    /// Preview or apply the atomic Memory v1 migration.
    Migrate {
        #[arg(long)]
        apply: bool,
    },
    /// Export a redacted local snapshot to stdout or a file.
    Export {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Diagnose configuration, schema, budgets, health, and migration state.
    Doctor,
}

fn store(cfg: &Config, cwd: &Path) -> Result<EvolutionStore> {
    EvolutionStore::new(&wyj_config::config_dir()?, cwd, cfg.evolution.clone())
}

fn print_value(value: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else if let Some(text) = value.as_str() {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

fn checkpoint_before_activation(cwd: &Path, id: &str) -> Result<String> {
    let sessions = wyj_config::config_dir()?.join("sessions");
    let store = CheckpointStore::new(&sessions, format!("evolution-approval-{id}"))?;
    let summary = store.create(
        cwd,
        &[],
        CheckpointKind::Manual,
        Some(format!("before Evolution approval {id}")),
    )?;
    Ok(summary.id)
}

fn infer_install_scope(path: &Path, cwd: &Path) -> Result<InstallScope> {
    let global = wyj_config::config_dir()?.join("skills");
    if path.starts_with(&global) {
        Ok(InstallScope::Global)
    } else if path.starts_with(wyj_config::project_config_dir(cwd).join("skills")) {
        Ok(InstallScope::Project)
    } else {
        anyhow::bail!("candidate activation path is outside managed Skill directories")
    }
}

fn status_text(store: &EvolutionStore) -> Result<String> {
    let status = store.status()?;
    Ok(format!(
        "Evolution project={}\nstore={}\nepisodes={} active_memories={} proposed_memories={} pending_candidates={} active_candidates={}\nsize={} bytes\nhealth: failures={} last_error={} last_success={}",
        status.project_id,
        status.directory.display(),
        status.episodes,
        status.active_memories,
        status.proposed_memories,
        status.pending_candidates,
        status.active_candidates,
        status.store_bytes,
        status.health.consecutive_failures,
        status.health.last_error.as_deref().unwrap_or("none"),
        status.health.last_success_at.as_deref().unwrap_or("never"),
    ))
}

pub async fn run(command: EvolveCommand, json: bool, cwd: &Path, cfg: &Config) -> Result<()> {
    let store = store(cfg, cwd)?;
    match command {
        EvolveCommand::Status => {
            if json {
                println!("{}", serde_json::to_string_pretty(&store.status()?)?);
            } else {
                println!("{}", status_text(&store)?);
            }
        }
        EvolveCommand::List { target, limit } => {
            let value = match target.as_str() {
                "memories" => serde_json::to_value(store.list_memories()?)?,
                "candidates" => serde_json::to_value(store.list_candidates()?)?,
                "episodes" => serde_json::to_value(store.list_episodes(limit)?)?,
                _ => serde_json::json!({
                    "memories": store.list_memories()?,
                    "candidates": store.list_candidates()?,
                    "episodes": store.list_episodes(limit)?,
                }),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                match target.as_str() {
                    "memories" => {
                        for item in store.list_memories()? {
                            println!(
                                "{}  {:?} {:?}  {}",
                                item.id, item.kind, item.status, item.summary
                            );
                        }
                    }
                    "candidates" => {
                        for item in store.list_candidates()? {
                            println!(
                                "{}  {:?} {:?}  {}",
                                item.id, item.kind, item.status, item.title
                            );
                        }
                    }
                    "episodes" => {
                        for item in store.list_episodes(limit)? {
                            println!("{}  {:?}  {}", item.id, item.outcome, item.goal_summary);
                        }
                    }
                    _ => {
                        println!("{}", status_text(&store)?);
                        println!("\nUse `wyj-code evolve list memories|candidates|episodes` for details.");
                    }
                }
            }
        }
        EvolveCommand::Review { id } => {
            let value = store
                .list_memories()?
                .into_iter()
                .find(|item| item.id == id)
                .map(|item| serde_json::to_value(item).expect("serialize Memory"))
                .or_else(|| {
                    store
                        .list_candidates()
                        .ok()?
                        .into_iter()
                        .find(|item| item.id == id)
                        .map(|item| serde_json::to_value(item).expect("serialize candidate"))
                })
                .or_else(|| {
                    store
                        .list_episodes(usize::MAX)
                        .ok()?
                        .into_iter()
                        .find(|item| item.id == id)
                        .map(|item| serde_json::to_value(item).expect("serialize Episode"))
                })
                .with_context(|| format!("Evolution item not found: {id}"))?;
            print_value(&value, json)?;
        }
        EvolveCommand::Feedback {
            sentiment,
            episode_id,
            reason,
        } => {
            let positive = sentiment == "good";
            let id = if let Some(id) = episode_id {
                store.feedback_episode(&id, positive, reason, true)?;
                id
            } else {
                store.feedback_latest(positive, reason)?
            };
            print_value(
                &serde_json::json!({"episode_id": id, "accepted": positive, "queued_for_reanalysis": true}),
                json,
            )?;
        }
        EvolveCommand::Skillize { episode_id } => {
            let id = store.create_skill_candidate_from_episode(&episode_id)?;
            print_value(&serde_json::json!({"candidate_id": id}), json)?;
        }
        EvolveCommand::Approve { id, scope } => {
            if store.list_memories()?.iter().any(|item| item.id == id) {
                store.activate_memory(&id)?;
                print_value(
                    &serde_json::json!({"memory_id": id, "status": "active"}),
                    json,
                )?;
                return Ok(());
            }
            let candidate = store
                .list_candidates()?
                .into_iter()
                .find(|candidate| candidate.id == id)
                .with_context(|| format!("candidate not found: {id}"))?;
            anyhow::ensure!(
                matches!(
                    candidate.status,
                    CandidateStatus::Proposed | CandidateStatus::Validated
                ),
                "candidate must be proposed or validated before approval"
            );
            let checkpoint = checkpoint_before_activation(cwd, &id)?;
            let activated_path = match &candidate.payload {
                CandidatePayload::Rule { .. } => None,
                CandidatePayload::Skill {
                    skill_name,
                    skill_md,
                    eval,
                    ..
                } => {
                    anyhow::ensure!(eval.structural_pass, "Skill candidate eval did not pass");
                    let scope = if scope == "global" {
                        InstallScope::Global
                    } else {
                        InstallScope::Project
                    };
                    Some(wyj_store::skill_install::install_generated_skill(
                        &wyj_store::skill_install::GeneratedSkillInstallRequest {
                            name: skill_name,
                            content: skill_md,
                            scope,
                            source_id: &id,
                        },
                        cwd,
                    )?)
                }
            };
            if let Err(error) = store.mark_candidate_active(&id, activated_path.clone()) {
                if let (CandidatePayload::Skill { skill_name, .. }, Some(path)) =
                    (&candidate.payload, activated_path.as_deref())
                {
                    if let Ok(scope) = infer_install_scope(path, cwd) {
                        let _ = wyj_store::skill_install::rollback_generated_skill(
                            skill_name, &id, scope, cwd,
                        );
                    }
                }
                return Err(error)
                    .context("mark approved candidate active; installation rolled back");
            }
            print_value(
                &serde_json::json!({
                    "candidate_id": id,
                    "status": "active",
                    "activated_path": activated_path,
                    "checkpoint_id": checkpoint,
                    "takes_effect": "next_agent_turn"
                }),
                json,
            )?;
        }
        EvolveCommand::Reject {
            candidate_id,
            reason,
        } => {
            store.reject_candidate(&candidate_id, reason)?;
            print_value(
                &serde_json::json!({"candidate_id": candidate_id, "status": "rejected"}),
                json,
            )?;
        }
        EvolveCommand::Rollback { candidate_id } => {
            let candidate = store
                .list_candidates()?
                .into_iter()
                .find(|candidate| candidate.id == candidate_id)
                .with_context(|| format!("candidate not found: {candidate_id}"))?;
            anyhow::ensure!(
                candidate.status == CandidateStatus::Active,
                "candidate is not active"
            );
            if let CandidatePayload::Skill { skill_name, .. } = &candidate.payload {
                let path = candidate
                    .activated_path
                    .as_deref()
                    .context("active Skill candidate has no activation path")?;
                let scope = infer_install_scope(path, cwd)?;
                wyj_store::skill_install::rollback_generated_skill(
                    skill_name,
                    &candidate_id,
                    scope,
                    cwd,
                )?;
            }
            store.rollback_candidate(&candidate_id)?;
            print_value(
                &serde_json::json!({"candidate_id": candidate_id, "status": "rolled_back"}),
                json,
            )?;
        }
        EvolveCommand::Forget { memory_id } => {
            store.forget_memory(&memory_id)?;
            print_value(
                &serde_json::json!({"memory_id": memory_id, "status": "forgotten"}),
                json,
            )?;
        }
        EvolveCommand::Run { episode_id } => {
            let id = match episode_id {
                Some(id) => id,
                None => {
                    store
                        .list_episodes(1)?
                        .into_iter()
                        .next()
                        .context("no Evolution Episode exists")?
                        .id
                }
            };
            let profile = if cfg.evolution.evolution_profile.trim().is_empty() {
                cfg.active_profile()
            } else {
                cfg.profile_by_name(&cfg.evolution.evolution_profile)
                    .unwrap_or_else(|| cfg.active_profile())
            };
            let provider = wyj_api::build_provider_from_profile(profile, None)?;
            let tokens = store.analyze_now(&id, provider).await?;
            print_value(
                &serde_json::json!({"episode_id": id, "tokens": tokens}),
                json,
            )?;
        }
        EvolveCommand::Include { episode_id } => {
            store.include_episode(&episode_id)?;
            print_value(
                &serde_json::json!({"episode_id": episode_id, "included": true, "queued_for_reanalysis": true}),
                json,
            )?;
        }
        EvolveCommand::Migrate { apply } => {
            let preview = if apply {
                store.migrate_legacy()?
            } else {
                store.migration_preview()?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&preview)?);
            } else {
                println!(
                    "legacy={} entries={} backup={} mode={}",
                    preview.legacy_directory.display(),
                    preview.entries,
                    preview.backup_directory.display(),
                    if apply { "applied" } else { "preview" }
                );
                for path in preview.files {
                    println!("  {}", path.display());
                }
            }
        }
        EvolveCommand::Export { output } => {
            let value = store.export_redacted()?;
            let bytes = serde_json::to_vec_pretty(&value)?;
            if let Some(path) = output {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, &path)?;
                if !json {
                    println!("{}", path.display());
                } else {
                    println!("{}", serde_json::json!({"output": path}));
                }
            } else {
                println!("{}", String::from_utf8(bytes)?);
            }
        }
        EvolveCommand::Doctor => {
            let status = store.status()?;
            let migration = store.migration_preview()?;
            let report = serde_json::json!({
                "enabled": cfg.evolution.enabled,
                "use_experiences": cfg.evolution.use_experiences,
                "generate_experiences": cfg.evolution.generate_experiences,
                "auto_activate_memories": cfg.evolution.auto_activate_memories,
                "auto_activate_rules": cfg.evolution.auto_activate_rules,
                "auto_install_skills": cfg.evolution.auto_install_skills,
                "allow_self_code_experiments": cfg.evolution.allow_self_code_experiments,
                "schema_version": wyj_core::EVOLUTION_SCHEMA_VERSION,
                "status": status,
                "legacy_migration_entries": migration.entries,
                "budgets": {
                    "daily_tokens": cfg.evolution.max_daily_tokens,
                    "daily_wall_secs": cfg.evolution.max_daily_wall_secs,
                    "project_store_bytes": cfg.evolution.max_project_store_bytes,
                    "idle_delay_secs": cfg.evolution.idle_delay_secs,
                    "background_workers": cfg.evolution.max_background_workers,
                },
                "safety": {
                    "external_context_excluded": cfg.evolution.exclude_external_context,
                    "rule_requires_approval": !cfg.evolution.auto_activate_rules,
                    "skill_requires_approval": !cfg.evolution.auto_install_skills,
                    "core_self_code_disabled": !cfg.evolution.allow_self_code_experiments,
                }
            });
            print_value(&report, json)?;
        }
    }
    Ok(())
}
