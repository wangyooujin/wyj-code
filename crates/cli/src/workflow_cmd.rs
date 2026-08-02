use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use wyj_api::types::{ContentBlock, Role};
use wyj_core::{
    validate_workflow, CheckpointKind, CheckpointStore, CodeIndex, CodeQuery, ExecutionSurface,
    ExecutionWorkspace, ExecutionWorkspaceManager, ExecutionWorkspaceRequest, GitWorktreeManager,
    Session, WorkflowControl, WorkflowEvent, WorkflowNodeContext, WorkflowNodeExecutor,
    WorkflowNodeKind, WorkflowNodeOutput, WorkflowPermissionCeiling, WorkflowRuntime,
    WorkflowSnapshot, WorkflowSpec, WorkspaceSnapshot,
};
use wyj_tools::{AgentFactory, PermissionMode, ToolCtx};

#[derive(Subcommand, Debug)]
pub enum WorkflowCommand {
    /// Validate a workflow JSON document without executing it.
    Validate {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Execute a workflow using independent Agent sessions per node.
    Run {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Inspect the last persisted workflow snapshot.
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Send a control request to a currently running workflow process.
    Control {
        id: String,
        #[command(subcommand)]
        action: WorkflowControlCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorkflowControlCommand {
    Pause,
    Resume,
    Approve { node_id: String },
    Retry { node_id: String },
    Skip { node_id: String },
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedWorkflow {
    schema_version: u32,
    spec: WorkflowSpec,
    snapshot: WorkflowSnapshot,
    running: bool,
    pid: u32,
    updated_at: String,
}

pub fn is_run(command: &WorkflowCommand) -> bool {
    matches!(command, WorkflowCommand::Run { .. })
}

pub fn run_args(command: WorkflowCommand) -> Result<(PathBuf, bool)> {
    match command {
        WorkflowCommand::Run { file, json } => Ok((file, json)),
        _ => bail!("workflow command is not run"),
    }
}

pub fn run_offline(
    command: WorkflowCommand,
    cwd: &Path,
    parent: &WorkflowPermissionCeiling,
) -> Result<()> {
    match command {
        WorkflowCommand::Validate { file, json } => {
            let spec = load_spec(&file)?;
            validate_workflow(&spec, parent)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "valid": true,
                        "workflow_id": spec.id,
                        "nodes": spec.nodes.len(),
                        "max_parallel": spec.max_parallel,
                        "token_budget": spec.token_budget,
                    }))?
                );
            } else {
                println!(
                    "workflow {} is valid ({} nodes, max_parallel={})",
                    spec.id,
                    spec.nodes.len(),
                    spec.max_parallel
                );
            }
        }
        WorkflowCommand::Status { id, json } => {
            let state = load_state(cwd, &id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                println!(
                    "workflow {} running={} used_tokens={}",
                    state.spec.id, state.running, state.snapshot.used_tokens
                );
                for (id, record) in state.snapshot.nodes {
                    println!(
                        "  {}\t{:?}\tattempts={}{}",
                        id,
                        record.state,
                        record.attempts,
                        record
                            .error
                            .as_deref()
                            .map(|error| format!("\t{error}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        WorkflowCommand::Control { id, action } => {
            let state = load_state(cwd, &id)?;
            if !state.running {
                bail!("workflow {id} is not running")
            }
            let control = match action {
                WorkflowControlCommand::Pause => WorkflowControl::Pause,
                WorkflowControlCommand::Resume => WorkflowControl::Resume,
                WorkflowControlCommand::Approve { node_id } => {
                    WorkflowControl::ApproveNode { node_id }
                }
                WorkflowControlCommand::Retry { node_id } => WorkflowControl::RetryNode { node_id },
                WorkflowControlCommand::Skip { node_id } => WorkflowControl::SkipNode { node_id },
                WorkflowControlCommand::Cancel => WorkflowControl::Cancel,
            };
            append_control(cwd, &id, &control)?;
            println!("control queued for workflow {id}");
        }
        WorkflowCommand::Run { .. } => bail!("workflow run requires the Agent runtime"),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_workflow(
    file: PathBuf,
    json: bool,
    cwd: PathBuf,
    parent: WorkflowPermissionCeiling,
    context_template: ToolCtx,
    factory: AgentFactory,
    definitions: wyj_tools::SharedAgentDefinitions,
    code_index: Arc<dyn CodeIndex>,
    output_style: Option<String>,
) -> Result<()> {
    let spec = load_spec(&file)?;
    validate_workflow(&spec, &parent)?;
    validate_id(&spec.id)?;

    let workspace = prepare_workflow_workspace(&cwd, &spec)?;

    let executor = Arc::new(CliWorkflowExecutor {
        cwd: cwd.clone(),
        context_template,
        factory,
        definitions,
        code_index,
        output_style,
        workspace,
    });
    let runtime = WorkflowRuntime::new(spec.clone(), parent)?;
    let (handle, mut task) = runtime.start(executor);
    let mut events = handle.subscribe();
    let control_path = control_path(&cwd, &spec.id)?;
    if let Some(parent) = control_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&control_path, [])?;
    let mut processed_controls = 0_usize;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
    persist_state(
        &cwd,
        PersistedWorkflow {
            schema_version: wyj_core::INTERFACE_SCHEMA_VERSION,
            spec: spec.clone(),
            snapshot: handle.snapshot(),
            running: true,
            pid: std::process::id(),
            updated_at: wyj_core::now_iso(),
        },
    )?;

    let final_snapshot = loop {
        tokio::select! {
            result = &mut task => {
                break result.context("workflow task join failed")??;
            }
            event = events.recv() => {
                match event {
                    Ok(WorkflowEvent::State { snapshot })
                    | Ok(WorkflowEvent::Finished { snapshot }) => {
                        persist_state(&cwd, PersistedWorkflow {
                            schema_version: wyj_core::INTERFACE_SCHEMA_VERSION,
                            spec: spec.clone(),
                            snapshot,
                            running: true,
                            pid: std::process::id(),
                            updated_at: wyj_core::now_iso(),
                        })?;
                    }
                    Ok(WorkflowEvent::ControlRejected { control, reason }) => {
                        eprintln!("workflow control rejected {:?}: {}", control, reason);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            _ = ticker.tick() => {
                for control in read_new_controls(&control_path, &mut processed_controls)? {
                    handle.control(control)?;
                }
            }
        }
    };

    persist_state(
        &cwd,
        PersistedWorkflow {
            schema_version: wyj_core::INTERFACE_SCHEMA_VERSION,
            spec,
            snapshot: final_snapshot.clone(),
            running: false,
            pid: std::process::id(),
            updated_at: wyj_core::now_iso(),
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&final_snapshot)?);
    } else {
        println!(
            "workflow {} finished, used_tokens={}",
            final_snapshot.workflow_id, final_snapshot.used_tokens
        );
        for (id, record) in final_snapshot.nodes {
            println!("  {}\t{:?}\tattempts={}", id, record.state, record.attempts);
        }
    }
    Ok(())
}

struct CliWorkflowExecutor {
    cwd: PathBuf,
    context_template: ToolCtx,
    factory: AgentFactory,
    definitions: wyj_tools::SharedAgentDefinitions,
    code_index: Arc<dyn CodeIndex>,
    output_style: Option<String>,
    workspace: Option<WorkflowWorkspace>,
}

#[derive(Clone)]
struct WorkflowWorkspace {
    manager: Arc<GitWorktreeManager>,
    repository_root: PathBuf,
    base_revision: String,
    parent_checkpoint_id: String,
    index_root: PathBuf,
}

#[async_trait]
impl WorkflowNodeExecutor for CliWorkflowExecutor {
    async fn execute(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
        match context.node.kind {
            WorkflowNodeKind::Index => self.execute_index(context),
            WorkflowNodeKind::HumanApproval => {
                bail!("human approval nodes must be approved through workflow control")
            }
            WorkflowNodeKind::Agent | WorkflowNodeKind::Review => self.execute_agent(context).await,
        }
    }
}

impl CliWorkflowExecutor {
    fn execute_index(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
        let matches = self.code_index.search(&CodeQuery {
            text: context.node.prompt,
            path_prefix: None,
            language: context.node.agent_type,
            limit: 20,
        })?;
        let status = self.code_index.status();
        Ok(WorkflowNodeOutput {
            value: serde_json::to_value(&matches)?,
            evidence: vec![format!(
                "code index backend={} indexed_files={} fallback={}",
                status.backend, status.indexed_files, status.fallback_available
            )],
            ..WorkflowNodeOutput::default()
        })
    }

    async fn execute_agent(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
        let mut definition = {
            let definitions = self.definitions.read().unwrap();
            let requested = context.node.agent_type.as_deref();
            requested
                .and_then(|name| {
                    definitions
                        .iter()
                        .find(|definition| definition.name.eq_ignore_ascii_case(name))
                })
                .cloned()
                .or_else(|| {
                    if context.node.kind == WorkflowNodeKind::Review {
                        definitions
                            .iter()
                            .find(|definition| {
                                definition.name.to_ascii_lowercase().contains("review")
                            })
                            .cloned()
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    definitions
                        .iter()
                        .find(|definition| definition.name == "general-purpose")
                        .cloned()
                })
                .ok_or_else(|| anyhow::anyhow!("no workflow Agent definition is available"))?
        };
        if context.node.kind == WorkflowNodeKind::Review
            && !definition.name.to_ascii_lowercase().contains("review")
        {
            definition.name = "workflow-review".to_string();
            definition.system_prompt.push_str(
                "\n\nReview the supplied dependency results. Prioritize correctness, security, regressions, and missing tests. Return concrete evidence.",
            );
        }
        let isolated_workspace = if context.node.kind == WorkflowNodeKind::Agent
            && node_can_modify_workspace(&context.node)
        {
            self.workspace
                .as_ref()
                .map(|workspace| workspace.create(&context))
                .transpose()?
        } else {
            None
        };
        let execution_cwd = isolated_workspace
            .as_ref()
            .map(|workspace| workspace.root.clone())
            .unwrap_or_else(|| self.cwd.clone());

        let mut agent = (self.factory)(&definition)?;
        if let Some(style) = &self.output_style {
            agent = agent.append_system(style.clone());
        }
        if let Some(workspace) = &isolated_workspace {
            agent = agent.append_system(format!(
                "\nThis workflow node runs in an isolated Git worktree at {}. Treat it as the repository root. Do not modify the parent checkout. Leave changes in this worktree for explicit diff review and selective acceptance.",
                workspace.root.display()
            ));
            agent.remove_tools_where(|name| name == "CodeSearch");
            let index_path = self
                .workspace
                .as_ref()
                .expect("isolated workspace configuration")
                .index_root
                .join(format!("{}.json", workspace.id));
            let index: Arc<dyn CodeIndex> = Arc::new(wyj_core::ProjectCodeIndex::new(
                &workspace.root,
                index_path,
            )?);
            agent.register_tool(Arc::new(wyj_core::CodeSearchTool::new(index)));
        }
        if let Some(budget) = context.token_budget {
            agent = agent.with_max_tokens(u32::try_from(budget).unwrap_or(u32::MAX).max(1));
        }
        let allowed: HashSet<String> = context
            .node
            .permission_ceiling
            .allowed_tools
            .iter()
            .cloned()
            .collect();
        agent.remove_tools_where(|name| !allowed.contains(name));
        agent.set_session_id(format!(
            "workflow-{}-{}",
            context.workflow_id, context.node.id
        ));

        let ctx = ToolCtx::new(&execution_cwd);
        ctx.set_execution_surface(ExecutionSurface::SubAgent);
        ctx.set_sandbox_available(*self.context_template.sandbox_available.read().unwrap());
        ctx.allow_unsandboxed_fallback(false);
        {
            let parent_sandbox = self.context_template.sandbox_policy.read().unwrap();
            let mut sandbox = ctx.sandbox_policy.write().unwrap();
            sandbox.mode = parent_sandbox.mode;
            sandbox.read_roots = parent_sandbox.read_roots.clone();
            sandbox.deny_read_roots = parent_sandbox.deny_read_roots.clone();
            sandbox.deny_write_roots = parent_sandbox.deny_write_roots.clone();
        }
        ctx.set_permission_mode(PermissionMode::Allowlist(allowed));
        {
            let mut policy = ctx.permission_policy.write().unwrap();
            policy.allowed_write_roots.clear();
            policy.allowed_domains.clear();
            policy.require_sandbox = context.node.permission_ceiling.require_sandbox;
        }
        for root in &context.node.permission_ceiling.write_roots {
            let root = isolated_workspace
                .as_ref()
                .and_then(|workspace| remap_workspace_path(&workspace.root, &self.cwd, root))
                .unwrap_or_else(|| root.clone());
            ctx.allow_write_root(&root)
                .map_err(|error| anyhow::anyhow!("workflow write root: {error}"))?;
        }
        for domain in &context.node.permission_ceiling.allowed_domains {
            ctx.allow_network_domain(domain.clone());
        }

        let dependencies = serde_json::to_string_pretty(&context.dependencies)?;
        let prompt = format!(
            "{}\n\n<workflow-direct-dependencies>\n{}\n</workflow-direct-dependencies>",
            context.node.prompt, dependencies
        );
        let mut session = Session::new();
        session.push_user(prompt);
        if let Err(error) = agent.run_turn(&mut session, &ctx, &mut |_| {}).await {
            if let Some(workspace) = &isolated_workspace {
                bail!(
                    "workflow node failed in preserved workspace {} at {}: {error}",
                    workspace.id,
                    workspace.root.display()
                )
            }
            return Err(error);
        }
        let mut evidence = vec![format!("agent_type={}", definition.name)];
        if let (Some(config), Some(workspace)) = (&self.workspace, &isolated_workspace) {
            let review = config.manager.review(workspace)?;
            evidence.push(format!(
                "workspace_id={} root={} changed_files={} insertions={} deletions={} review=`wyj-code workspace diff {}` accept=`wyj-code workspace accept {} <paths...>`",
                workspace.id,
                workspace.root.display(),
                review.summary.changed_files,
                review.summary.insertions,
                review.summary.deletions,
                workspace.id,
                workspace.id
            ));
        }
        Ok(WorkflowNodeOutput {
            value: serde_json::Value::String(last_assistant_text(&session)),
            evidence,
            input_tokens: u64::from(session.total_input_tokens),
            output_tokens: u64::from(session.total_output_tokens),
        })
    }
}

impl WorkflowWorkspace {
    fn create(&self, context: &WorkflowNodeContext) -> Result<ExecutionWorkspace> {
        self.manager.create(&ExecutionWorkspaceRequest {
            session_id: format!("workflow-{}-{}", context.workflow_id, context.node.id),
            repository_root: self.repository_root.clone(),
            base_revision: self.base_revision.clone(),
            parent_checkpoint_id: Some(self.parent_checkpoint_id.clone()),
            purpose: context.node.prompt.clone(),
        })
    }
}

fn prepare_workflow_workspace(
    cwd: &Path,
    spec: &WorkflowSpec,
) -> Result<Option<WorkflowWorkspace>> {
    prepare_workflow_workspace_at(cwd, spec, &wyj_config::config_dir()?)
}

fn prepare_workflow_workspace_at(
    cwd: &Path,
    spec: &WorkflowSpec,
    config: &Path,
) -> Result<Option<WorkflowWorkspace>> {
    if !spec.nodes.iter().any(node_can_modify_workspace) {
        return Ok(None);
    }
    let repository_root = std::fs::canonicalize(wyj_core::project_root(cwd))?;
    let is_git = std::process::Command::new("git")
        .arg("-C")
        .arg(&repository_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !is_git {
        return Ok(None);
    }
    let checkpoint_store =
        CheckpointStore::new(&config.join("sessions"), format!("workflow-{}", spec.id))?;
    let checkpoint = checkpoint_store.create(
        &repository_root,
        &[],
        CheckpointKind::Manual,
        Some(format!("workflow {} worktree base", spec.id)),
    )?;
    let loaded = checkpoint_store.load(&checkpoint.id)?;
    let WorkspaceSnapshot::Git(snapshot) = loaded.workspace else {
        return Ok(None);
    };
    let project = wyj_core::project_id(&repository_root);
    Ok(Some(WorkflowWorkspace {
        manager: Arc::new(GitWorktreeManager::new(
            config.join("workspaces").join(&project),
        )?),
        repository_root,
        base_revision: snapshot.commit,
        parent_checkpoint_id: checkpoint.id,
        index_root: config.join("indexes").join(project).join("workflow"),
    }))
}

fn node_can_modify_workspace(node: &wyj_core::WorkflowNodeSpec) -> bool {
    !node.permission_ceiling.write_roots.is_empty()
        && node
            .permission_ceiling
            .allowed_tools
            .iter()
            .any(|tool| matches!(tool.as_str(), "Write" | "Edit" | "Bash"))
}

fn remap_workspace_path(worktree: &Path, repository_root: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repository_root.join(path)
    };
    absolute
        .strip_prefix(repository_root)
        .ok()
        .map(|relative| worktree.join(relative))
}

fn last_assistant_text(session: &Session) -> String {
    session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub fn load_spec(path: &Path) -> Result<WorkflowSpec> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read workflow spec {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse workflow JSON {}", path.display()))
}

fn state_dir(cwd: &Path) -> Result<PathBuf> {
    Ok(wyj_config::config_dir()?
        .join("workflows")
        .join(wyj_core::project_id(&wyj_core::project_root(cwd))))
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("workflow id may contain only letters, digits, '-', '_' and '.'")
    }
    Ok(())
}

fn state_path(cwd: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(state_dir(cwd)?.join(format!("{id}.json")))
}

fn control_path(cwd: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(state_dir(cwd)?.join(format!("{id}.controls.jsonl")))
}

fn persist_state(cwd: &Path, state: PersistedWorkflow) -> Result<()> {
    let path = state_path(cwd, &state.spec.id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&state)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn load_state(cwd: &Path, id: &str) -> Result<PersistedWorkflow> {
    let path = state_path(cwd, id)?;
    serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("parse workflow state {}", path.display()))
}

fn append_control(cwd: &Path, id: &str, control: &WorkflowControl) -> Result<()> {
    let path = control_path(cwd, id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, control)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

pub fn queue_control(cwd: &Path, id: &str, control: &WorkflowControl) -> Result<()> {
    let state = load_state(cwd, id)?;
    if !state.running {
        bail!("workflow {id} is not running")
    }
    append_control(cwd, id, control)
}

fn read_new_controls(path: &Path, processed: &mut usize) -> Result<Vec<WorkflowControl>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let controls = lines
        .iter()
        .skip(*processed)
        .map(|line| serde_json::from_str(line).context("parse workflow control"))
        .collect::<Result<Vec<_>>>()?;
    *processed = lines.len();
    Ok(controls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_node() -> wyj_core::WorkflowNodeSpec {
        wyj_core::WorkflowNodeSpec {
            id: "implement".to_string(),
            kind: WorkflowNodeKind::Agent,
            agent_type: Some("general-purpose".to_string()),
            prompt: "change the implementation".to_string(),
            depends_on: Vec::new(),
            permission_ceiling: WorkflowPermissionCeiling {
                allowed_tools: vec!["Read".to_string(), "Edit".to_string()],
                write_roots: vec![PathBuf::from("src")],
                allowed_domains: Vec::new(),
                require_sandbox: true,
            },
            max_retries: 0,
        }
    }

    #[test]
    fn workflow_write_nodes_start_from_dirty_checkpoint_in_managed_worktree() {
        let repo = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/lib.rs"), "fn original() {}\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        std::fs::write(repo.path().join("src/lib.rs"), "fn dirty_base() {}\n").unwrap();

        let spec = WorkflowSpec {
            schema_version: wyj_core::INTERFACE_SCHEMA_VERSION,
            id: "auto-worktree".to_string(),
            nodes: vec![write_node()],
            max_parallel: 2,
            token_budget: None,
        };
        let workspace = prepare_workflow_workspace_at(repo.path(), &spec, config.path())
            .unwrap()
            .unwrap();
        let execution = workspace
            .create(&WorkflowNodeContext {
                workflow_id: spec.id.clone(),
                node: spec.nodes[0].clone(),
                dependencies: Default::default(),
                token_budget: None,
            })
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(execution.root.join("src/lib.rs")).unwrap(),
            "fn dirty_base() {}\n"
        );
        assert!(execution.parent_checkpoint_id.is_some());
        workspace.manager.dispose(&execution).unwrap();
    }

    #[test]
    fn worktree_path_mapping_only_rebases_repository_paths() {
        let repo = Path::new("/repo");
        let worktree = Path::new("/managed/worktree");
        assert_eq!(
            remap_workspace_path(worktree, repo, Path::new("src/lib.rs")),
            Some(PathBuf::from("/managed/worktree/src/lib.rs"))
        );
        assert_eq!(
            remap_workspace_path(worktree, repo, Path::new("/outside/file")),
            None
        );
    }
}
