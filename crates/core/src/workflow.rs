//! Dynamic workflow DAG runtime.
//!
//! Workflow definitions are data, not programs: they carry prompts and permission ceilings but
//! cannot execute shell/filesystem/network operations themselves.  Only a caller-supplied
//! [`WorkflowNodeExecutor`] can perform a node, and every node is validated to be no more
//! permissive than the parent ceiling before anything is scheduled.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::{
    WorkflowControl, WorkflowNodeSpec, WorkflowNodeState, WorkflowPermissionCeiling, WorkflowSpec,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNodeOutput {
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl Default for WorkflowNodeOutput {
    fn default() -> Self {
        Self {
            value: serde_json::Value::Null,
            evidence: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }
}

impl WorkflowNodeOutput {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowNodeContext {
    pub workflow_id: String,
    pub node: WorkflowNodeSpec,
    /// Only direct dependency results are supplied.  The main conversation is never included.
    pub dependencies: BTreeMap<String, WorkflowNodeOutput>,
    /// Hard allocation for this attempt.  Executors must configure their model runtime to stay
    /// within it; the runtime also rejects an output that reports usage above the allocation.
    pub token_budget: Option<u64>,
}

#[async_trait]
pub trait WorkflowNodeExecutor: Send + Sync {
    async fn execute(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeRecord {
    pub state: WorkflowNodeState,
    pub attempts: u32,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub workflow_id: String,
    pub paused: bool,
    pub cancelled: bool,
    pub used_tokens: u64,
    pub nodes: BTreeMap<String, WorkflowNodeRecord>,
    /// Node results stay in the workflow store.  A frontend must explicitly inspect/export them;
    /// they are not appended to the user's session by the runtime.
    pub results: BTreeMap<String, WorkflowNodeOutput>,
}

impl WorkflowSnapshot {
    fn new(spec: &WorkflowSpec) -> Self {
        Self {
            workflow_id: spec.id.clone(),
            paused: false,
            cancelled: false,
            used_tokens: 0,
            nodes: spec
                .nodes
                .iter()
                .map(|node| {
                    (
                        node.id.clone(),
                        WorkflowNodeRecord {
                            state: WorkflowNodeState::Pending,
                            attempts: 0,
                            error: None,
                        },
                    )
                })
                .collect(),
            results: BTreeMap::new(),
        }
    }

    pub fn finished(&self) -> bool {
        self.nodes.values().all(|record| {
            matches!(
                record.state,
                WorkflowNodeState::Succeeded
                    | WorkflowNodeState::Failed
                    | WorkflowNodeState::Skipped
                    | WorkflowNodeState::Cancelled
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowEvent {
    State {
        snapshot: WorkflowSnapshot,
    },
    ControlRejected {
        control: WorkflowControl,
        reason: String,
    },
    Finished {
        snapshot: WorkflowSnapshot,
    },
}

#[derive(Clone)]
pub struct WorkflowHandle {
    control_tx: mpsc::UnboundedSender<WorkflowControl>,
    events: broadcast::Sender<WorkflowEvent>,
    snapshot: Arc<RwLock<WorkflowSnapshot>>,
}

impl WorkflowHandle {
    pub fn control(&self, control: WorkflowControl) -> Result<()> {
        self.control_tx
            .send(control)
            .map_err(|_| anyhow::anyhow!("workflow runtime has stopped"))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.events.subscribe()
    }

    pub fn snapshot(&self) -> WorkflowSnapshot {
        self.snapshot.read().unwrap().clone()
    }
}

pub struct WorkflowRuntime {
    spec: WorkflowSpec,
    parent_ceiling: WorkflowPermissionCeiling,
}

impl WorkflowRuntime {
    pub fn new(spec: WorkflowSpec, parent_ceiling: WorkflowPermissionCeiling) -> Result<Self> {
        validate_workflow(&spec, &parent_ceiling)?;
        Ok(Self {
            spec,
            parent_ceiling,
        })
    }

    pub fn start(
        self,
        executor: Arc<dyn WorkflowNodeExecutor>,
    ) -> (WorkflowHandle, JoinHandle<Result<WorkflowSnapshot>>) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(128);
        let snapshot = Arc::new(RwLock::new(WorkflowSnapshot::new(&self.spec)));
        let handle = WorkflowHandle {
            control_tx,
            events: events.clone(),
            snapshot: snapshot.clone(),
        };
        let task = tokio::spawn(run_workflow(
            self.spec,
            self.parent_ceiling,
            executor,
            control_rx,
            events,
            snapshot,
        ));
        (handle, task)
    }
}

pub fn validate_workflow(spec: &WorkflowSpec, parent: &WorkflowPermissionCeiling) -> Result<()> {
    if spec.schema_version == 0 || spec.schema_version > crate::INTERFACE_SCHEMA_VERSION {
        bail!(
            "unsupported workflow schema version {}",
            spec.schema_version
        )
    }
    if spec.id.trim().is_empty() {
        bail!("workflow id cannot be empty")
    }
    if spec.nodes.is_empty() {
        bail!("workflow must contain at least one node")
    }
    if !(1..=64).contains(&spec.max_parallel) {
        bail!("max_parallel must be between 1 and 64")
    }
    if spec.token_budget == Some(0) {
        bail!("token_budget must be positive when present")
    }
    let mut ids = HashSet::new();
    for node in &spec.nodes {
        if node.id.trim().is_empty() || !ids.insert(node.id.clone()) {
            bail!(
                "workflow node ids must be non-empty and unique: {}",
                node.id
            )
        }
        validate_ceiling(&node.permission_ceiling, parent)
            .map_err(|error| anyhow::anyhow!("node {}: {error}", node.id))?;
    }
    for node in &spec.nodes {
        for dependency in &node.depends_on {
            if dependency == &node.id || !ids.contains(dependency) {
                bail!("node {} has invalid dependency {}", node.id, dependency)
            }
        }
    }
    let dependencies: HashMap<_, _> = spec
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.depends_on.clone()))
        .collect();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for id in ids {
        visit(&id, &dependencies, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn validate_ceiling(
    child: &WorkflowPermissionCeiling,
    parent: &WorkflowPermissionCeiling,
) -> Result<()> {
    let parent_tools: HashSet<&str> = parent.allowed_tools.iter().map(String::as_str).collect();
    if child
        .allowed_tools
        .iter()
        .any(|tool| !parent_tools.contains(tool.as_str()))
    {
        bail!("allowed_tools expands the parent permission ceiling")
    }
    Ok(())
}

fn visit(
    id: &str,
    dependencies: &HashMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id.to_string()) {
        bail!("workflow contains a dependency cycle at {id}")
    }
    for dependency in dependencies.get(id).into_iter().flatten() {
        visit(dependency, dependencies, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    Ok(())
}

struct NodeCompletion {
    id: String,
    attempts: u32,
    token_budget: Option<u64>,
    result: Result<WorkflowNodeOutput, String>,
}

async fn run_workflow(
    spec: WorkflowSpec,
    _parent_ceiling: WorkflowPermissionCeiling,
    executor: Arc<dyn WorkflowNodeExecutor>,
    mut control_rx: mpsc::UnboundedReceiver<WorkflowControl>,
    events: broadcast::Sender<WorkflowEvent>,
    snapshot: Arc<RwLock<WorkflowSnapshot>>,
) -> Result<WorkflowSnapshot> {
    let nodes: HashMap<String, WorkflowNodeSpec> = spec
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.clone()))
        .collect();
    let descendants = descendants(&spec);
    let mut running = JoinSet::<NodeCompletion>::new();

    loop {
        schedule_ready(&spec, &nodes, &executor, &snapshot, &events, &mut running);

        if workflow_is_complete(&snapshot.read().unwrap()) {
            let final_snapshot = snapshot.read().unwrap().clone();
            let _ = events.send(WorkflowEvent::Finished {
                snapshot: final_snapshot.clone(),
            });
            return Ok(final_snapshot);
        }

        if running.is_empty() {
            let mut state = snapshot.write().unwrap();
            if state.paused {
                drop(state);
            } else {
                let terminal: HashMap<String, WorkflowNodeState> = state
                    .nodes
                    .iter()
                    .map(|(id, record)| (id.clone(), record.state))
                    .collect();
                let mut changed = false;
                for node in &spec.nodes {
                    let record = state.nodes.get_mut(&node.id).unwrap();
                    if record.state == WorkflowNodeState::Pending
                        && node.depends_on.iter().any(|dependency| {
                            matches!(
                                terminal.get(dependency),
                                Some(WorkflowNodeState::Failed | WorkflowNodeState::Cancelled)
                            )
                        })
                    {
                        record.state = WorkflowNodeState::Cancelled;
                        record.error = Some("dependency did not complete successfully".to_string());
                        changed = true;
                    }
                }
                if changed {
                    emit_state(&events, &state);
                    continue;
                }
                drop(state);
            }
        }

        tokio::select! {
            biased;
            Some(control) = control_rx.recv() => {
                apply_control(control, &descendants, &snapshot, &events, &mut running);
            }
            completion = running.join_next(), if !running.is_empty() => {
                if let Some(completion) = completion {
                    let completion = completion.map_err(|error| anyhow::anyhow!("workflow node task failed: {error}"))?;
                    let mut state = snapshot.write().unwrap();
                    state.nodes.get_mut(&completion.id).unwrap().attempts = completion.attempts;
                    match completion.result {
                        Ok(output) => {
                            if completion.token_budget.is_some_and(|limit| output.total_tokens() > limit) {
                                let record = state.nodes.get_mut(&completion.id).unwrap();
                                record.state = WorkflowNodeState::Failed;
                                record.error = Some("node exceeded its runtime token allocation".to_string());
                            } else {
                                state.used_tokens = state.used_tokens.saturating_add(output.total_tokens());
                                state.results.insert(completion.id.clone(), output);
                                let record = state.nodes.get_mut(&completion.id).unwrap();
                                record.state = WorkflowNodeState::Succeeded;
                                record.error = None;
                            }
                        }
                        Err(error) => {
                            let record = state.nodes.get_mut(&completion.id).unwrap();
                            record.state = WorkflowNodeState::Failed;
                            record.error = Some(error);
                        }
                    }
                    if spec.token_budget.is_some_and(|budget| state.used_tokens >= budget) {
                        for pending in state.nodes.values_mut() {
                            if pending.state == WorkflowNodeState::Pending {
                                pending.state = WorkflowNodeState::Cancelled;
                                pending.error = Some("workflow token budget exhausted".to_string());
                            }
                        }
                    }
                    emit_state(&events, &state);
                }
            }
            else => {
                let final_snapshot = snapshot.read().unwrap().clone();
                return Ok(final_snapshot);
            }
        }
    }
}

fn schedule_ready(
    spec: &WorkflowSpec,
    nodes: &HashMap<String, WorkflowNodeSpec>,
    executor: &Arc<dyn WorkflowNodeExecutor>,
    snapshot: &Arc<RwLock<WorkflowSnapshot>>,
    events: &broadcast::Sender<WorkflowEvent>,
    running: &mut JoinSet<NodeCompletion>,
) {
    let mut state = snapshot.write().unwrap();
    if state.paused || state.cancelled {
        return;
    }
    let capacity = spec.max_parallel.saturating_sub(running.len());
    if capacity == 0 {
        return;
    }
    let terminal: HashMap<String, WorkflowNodeState> = state
        .nodes
        .iter()
        .map(|(id, record)| (id.clone(), record.state))
        .collect();
    let ready: Vec<String> = spec
        .nodes
        .iter()
        .filter(|node| {
            state.nodes[&node.id].state == WorkflowNodeState::Pending
                && node.depends_on.iter().all(|dependency| {
                    matches!(
                        terminal.get(dependency),
                        Some(WorkflowNodeState::Succeeded | WorkflowNodeState::Skipped)
                    )
                })
        })
        .take(capacity)
        .map(|node| node.id.clone())
        .collect();
    if ready.is_empty() {
        return;
    }
    let allocation = allocated_budget(spec, &state, ready.len());
    for id in ready {
        let node = nodes[&id].clone();
        let dependencies: BTreeMap<String, WorkflowNodeOutput> = node
            .depends_on
            .iter()
            .filter_map(|dependency| {
                state
                    .results
                    .get(dependency)
                    .cloned()
                    .map(|result| (dependency.clone(), result))
            })
            .collect();
        if matches!(node.kind, crate::WorkflowNodeKind::HumanApproval) {
            state.nodes.get_mut(&id).unwrap().state = WorkflowNodeState::WaitingApproval;
            continue;
        }
        state.nodes.get_mut(&id).unwrap().state = WorkflowNodeState::Running;
        let executor = executor.clone();
        let workflow_id = spec.id.clone();
        running.spawn(async move {
            let mut attempts = 0_u32;
            loop {
                attempts += 1;
                let context = WorkflowNodeContext {
                    workflow_id: workflow_id.clone(),
                    node: node.clone(),
                    dependencies: dependencies.clone(),
                    token_budget: allocation,
                };
                match executor.execute(context).await {
                    Ok(output) => {
                        return NodeCompletion {
                            id,
                            attempts,
                            token_budget: allocation,
                            result: Ok(output),
                        }
                    }
                    Err(_error) if attempts <= node.max_retries => continue,
                    Err(error) => {
                        return NodeCompletion {
                            id,
                            attempts,
                            token_budget: allocation,
                            result: Err(error.to_string()),
                        }
                    }
                }
            }
        });
    }
    emit_state(events, &state);
}

fn allocated_budget(
    spec: &WorkflowSpec,
    snapshot: &WorkflowSnapshot,
    divisor: usize,
) -> Option<u64> {
    spec.token_budget.map(|budget| {
        budget
            .saturating_sub(snapshot.used_tokens)
            .checked_div(divisor.max(1) as u64)
            .unwrap_or(0)
            .max(1)
    })
}

fn workflow_is_complete(snapshot: &WorkflowSnapshot) -> bool {
    snapshot.finished()
        && (snapshot.cancelled
            || snapshot
                .nodes
                .values()
                .all(|record| record.state != WorkflowNodeState::Failed))
}

fn apply_control(
    control: WorkflowControl,
    descendants: &HashMap<String, BTreeSet<String>>,
    snapshot: &Arc<RwLock<WorkflowSnapshot>>,
    events: &broadcast::Sender<WorkflowEvent>,
    running: &mut JoinSet<NodeCompletion>,
) {
    let mut state = snapshot.write().unwrap();
    let rejected = match &control {
        WorkflowControl::Pause => {
            state.paused = true;
            None
        }
        WorkflowControl::Resume => {
            state.paused = false;
            None
        }
        WorkflowControl::ApproveNode { node_id } => match state.nodes.get_mut(node_id) {
            Some(record) if record.state == WorkflowNodeState::WaitingApproval => {
                record.state = WorkflowNodeState::Succeeded;
                record.error = None;
                state.results.insert(
                    node_id.clone(),
                    WorkflowNodeOutput {
                        value: serde_json::json!({"approved": true}),
                        evidence: vec!["approved by explicit workflow control".to_string()],
                        ..WorkflowNodeOutput::default()
                    },
                );
                None
            }
            Some(_) => Some("only a waiting approval node can be approved".to_string()),
            None => Some(format!("unknown workflow node {node_id}")),
        },
        WorkflowControl::Cancel => {
            state.cancelled = true;
            running.abort_all();
            for record in state.nodes.values_mut() {
                if matches!(
                    record.state,
                    WorkflowNodeState::Pending
                        | WorkflowNodeState::Running
                        | WorkflowNodeState::WaitingApproval
                ) {
                    record.state = WorkflowNodeState::Cancelled;
                    record.error = Some("workflow cancelled".to_string());
                }
            }
            None
        }
        WorkflowControl::SkipNode { node_id } => match state.nodes.get_mut(node_id) {
            Some(record)
                if matches!(
                    record.state,
                    WorkflowNodeState::Pending
                        | WorkflowNodeState::Failed
                        | WorkflowNodeState::WaitingApproval
                ) =>
            {
                record.state = WorkflowNodeState::Skipped;
                record.error = None;
                for id in descendants.get(node_id).into_iter().flatten() {
                    if let Some(descendant) = state.nodes.get_mut(id) {
                        if descendant.state == WorkflowNodeState::Cancelled
                            && descendant.error.as_deref()
                                == Some("dependency did not complete successfully")
                        {
                            descendant.state = WorkflowNodeState::Pending;
                            descendant.error = None;
                        }
                    }
                }
                None
            }
            Some(_) => Some("only pending, failed, or approval nodes can be skipped".to_string()),
            None => Some(format!("unknown workflow node {node_id}")),
        },
        WorkflowControl::RetryNode { node_id } => match state.nodes.get(node_id) {
            Some(record)
                if matches!(
                    record.state,
                    WorkflowNodeState::Succeeded
                        | WorkflowNodeState::Failed
                        | WorkflowNodeState::Skipped
                        | WorkflowNodeState::Cancelled
                ) =>
            {
                let mut reset = descendants.get(node_id).cloned().unwrap_or_default();
                if reset.iter().any(|id| {
                    state.nodes.get(id).is_some_and(|record| {
                        matches!(
                            record.state,
                            WorkflowNodeState::Running | WorkflowNodeState::WaitingApproval
                        )
                    })
                }) {
                    Some("a descendant is still running or waiting for approval".to_string())
                } else {
                    reset.insert(node_id.clone());
                    for id in reset {
                        if let Some(record) = state.nodes.get_mut(&id) {
                            record.state = WorkflowNodeState::Pending;
                            record.attempts = 0;
                            record.error = None;
                        }
                        if let Some(output) = state.results.remove(&id) {
                            state.used_tokens =
                                state.used_tokens.saturating_sub(output.total_tokens());
                        }
                    }
                    None
                }
            }
            Some(_) => Some("a running or pending node cannot be retried".to_string()),
            None => Some(format!("unknown workflow node {node_id}")),
        },
    };
    if let Some(reason) = rejected {
        let _ = events.send(WorkflowEvent::ControlRejected { control, reason });
    }
    emit_state(events, &state);
}

fn descendants(spec: &WorkflowSpec) -> HashMap<String, BTreeSet<String>> {
    let mut direct: HashMap<String, Vec<String>> = HashMap::new();
    for node in &spec.nodes {
        for dependency in &node.depends_on {
            direct
                .entry(dependency.clone())
                .or_default()
                .push(node.id.clone());
        }
    }
    let mut result = HashMap::new();
    for node in &spec.nodes {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([node.id.clone()]);
        while let Some(id) = queue.pop_front() {
            for child in direct.get(&id).into_iter().flatten() {
                if seen.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
        result.insert(node.id.clone(), seen);
    }
    result
}

fn emit_state(events: &broadcast::Sender<WorkflowEvent>, state: &WorkflowSnapshot) {
    let _ = events.send(WorkflowEvent::State {
        snapshot: state.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn ceiling() -> WorkflowPermissionCeiling {
        WorkflowPermissionCeiling {
            allowed_tools: vec!["Read".to_string(), "Grep".to_string(), "Edit".to_string()],
        }
    }

    fn node(id: &str, deps: &[&str]) -> WorkflowNodeSpec {
        WorkflowNodeSpec {
            id: id.to_string(),
            kind: crate::WorkflowNodeKind::Agent,
            agent_type: Some("general-purpose".to_string()),
            prompt: id.to_string(),
            depends_on: deps.iter().map(|v| (*v).to_string()).collect(),
            permission_ceiling: WorkflowPermissionCeiling {
                allowed_tools: vec!["Read".to_string()],
            },
            max_retries: 1,
        }
    }

    struct FakeExecutor {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    #[async_trait]
    impl WorkflowNodeExecutor for FakeExecutor {
        async fn execute(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(WorkflowNodeOutput {
                value: serde_json::json!({
                    "node": context.node.id,
                    "deps": context.dependencies.keys().cloned().collect::<Vec<_>>()
                }),
                evidence: vec!["fake".to_string()],
                input_tokens: 3,
                output_tokens: 2,
            })
        }
    }

    #[tokio::test]
    async fn dag_runs_ready_nodes_in_parallel_and_isolates_dependency_results() {
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "build-review".to_string(),
            nodes: vec![node("a", &[]), node("b", &[]), node("review", &["a", "b"])],
            max_parallel: 2,
            token_budget: Some(100),
        };
        let executor = Arc::new(FakeExecutor {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let (_handle, task) = runtime.start(executor.clone());
        let snapshot = task.await.unwrap().unwrap();
        assert!(snapshot.finished());
        assert_eq!(snapshot.used_tokens, 15);
        assert_eq!(executor.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(
            snapshot.results["review"].value["deps"],
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn validation_rejects_cycles_and_permission_expansion() {
        let mut a = node("a", &["b"]);
        let b = node("b", &["a"]);
        let cycle = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "cycle".to_string(),
            nodes: vec![a.clone(), b],
            max_parallel: 2,
            token_budget: None,
        };
        assert!(validate_workflow(&cycle, &ceiling()).is_err());

        a.depends_on.clear();
        a.permission_ceiling.allowed_tools.push("Bash".to_string());
        let expanded = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "expanded".to_string(),
            nodes: vec![a],
            max_parallel: 1,
            token_budget: None,
        };
        assert!(validate_workflow(&expanded, &ceiling()).is_err());
    }

    #[tokio::test]
    async fn pause_skip_and_resume_are_observable_controls() {
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "control".to_string(),
            nodes: vec![node("a", &[]), node("b", &["a"])],
            max_parallel: 1,
            token_budget: None,
        };
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let executor = Arc::new(FakeExecutor {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let (handle, task) = runtime.start(executor);
        handle.control(WorkflowControl::Pause).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        handle
            .control(WorkflowControl::SkipNode {
                node_id: "b".to_string(),
            })
            .unwrap();
        handle.control(WorkflowControl::Resume).unwrap();
        let snapshot = task.await.unwrap().unwrap();
        assert_eq!(snapshot.nodes["a"].state, WorkflowNodeState::Succeeded);
        assert_eq!(snapshot.nodes["b"].state, WorkflowNodeState::Skipped);
    }

    #[tokio::test]
    async fn human_approval_waits_for_explicit_control_before_downstream_runs() {
        let mut approval = node("approve", &[]);
        approval.kind = crate::WorkflowNodeKind::HumanApproval;
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "approval".to_string(),
            nodes: vec![approval, node("after", &["approve"])],
            max_parallel: 1,
            token_budget: None,
        };
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let executor = Arc::new(FakeExecutor {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let (handle, task) = runtime.start(executor);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            handle.snapshot().nodes["approve"].state,
            WorkflowNodeState::WaitingApproval
        );
        assert_eq!(
            handle.snapshot().nodes["after"].state,
            WorkflowNodeState::Pending
        );
        handle
            .control(WorkflowControl::ApproveNode {
                node_id: "approve".to_string(),
            })
            .unwrap();
        let snapshot = task.await.unwrap().unwrap();
        assert_eq!(
            snapshot.nodes["approve"].state,
            WorkflowNodeState::Succeeded
        );
        assert_eq!(snapshot.nodes["after"].state, WorkflowNodeState::Succeeded);
        assert_eq!(snapshot.results["approve"].value["approved"], true);
    }

    struct OverBudgetExecutor;

    #[async_trait]
    impl WorkflowNodeExecutor for OverBudgetExecutor {
        async fn execute(&self, _context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
            Ok(WorkflowNodeOutput {
                input_tokens: 60,
                output_tokens: 60,
                ..WorkflowNodeOutput::default()
            })
        }
    }

    struct RetryExecutor {
        calls: std::sync::Mutex<HashMap<String, usize>>,
    }

    #[async_trait]
    impl WorkflowNodeExecutor for RetryExecutor {
        async fn execute(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let call = calls.entry(context.node.id.clone()).or_default();
                *call += 1;
                *call
            };
            if context.node.id == "b" && call == 1 {
                bail!("review failed")
            }
            Ok(WorkflowNodeOutput {
                value: serde_json::json!({"node": context.node.id, "call": call}),
                input_tokens: 2,
                output_tokens: 3,
                ..WorkflowNodeOutput::default()
            })
        }
    }

    #[tokio::test]
    async fn retry_rewinds_descendants_and_reclaims_previous_token_usage() {
        let mut a = node("a", &[]);
        let mut b = node("b", &["a"]);
        a.max_retries = 0;
        b.max_retries = 0;
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "retry".to_string(),
            nodes: vec![a, b],
            max_parallel: 1,
            token_budget: Some(100),
        };
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let (handle, task) = runtime.start(Arc::new(RetryExecutor {
            calls: std::sync::Mutex::new(HashMap::new()),
        }));
        for _ in 0..100 {
            if handle.snapshot().nodes["b"].state == WorkflowNodeState::Failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(handle.snapshot().used_tokens, 5);
        handle
            .control(WorkflowControl::RetryNode {
                node_id: "a".to_string(),
            })
            .unwrap();
        let snapshot = task.await.unwrap().unwrap();
        assert_eq!(snapshot.nodes["a"].state, WorkflowNodeState::Succeeded);
        assert_eq!(snapshot.nodes["b"].state, WorkflowNodeState::Succeeded);
        assert_eq!(snapshot.used_tokens, 10);
        assert_eq!(snapshot.results["a"].value["call"], 2);
        assert_eq!(snapshot.results["b"].value["call"], 2);
    }

    struct AlwaysFailA;

    #[async_trait]
    impl WorkflowNodeExecutor for AlwaysFailA {
        async fn execute(&self, context: WorkflowNodeContext) -> Result<WorkflowNodeOutput> {
            if context.node.id == "a" {
                bail!("a failed")
            }
            Ok(WorkflowNodeOutput::default())
        }
    }

    #[tokio::test]
    async fn skipping_failed_node_releases_dependency_cancelled_descendants() {
        let mut a = node("a", &[]);
        a.max_retries = 0;
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "skip-failed".to_string(),
            nodes: vec![a, node("b", &["a"])],
            max_parallel: 1,
            token_budget: None,
        };
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let (handle, task) = runtime.start(Arc::new(AlwaysFailA));
        for _ in 0..100 {
            if handle.snapshot().nodes["a"].state == WorkflowNodeState::Failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        handle
            .control(WorkflowControl::SkipNode {
                node_id: "a".to_string(),
            })
            .unwrap();
        let snapshot = task.await.unwrap().unwrap();
        assert_eq!(snapshot.nodes["a"].state, WorkflowNodeState::Skipped);
        assert_eq!(snapshot.nodes["b"].state, WorkflowNodeState::Succeeded);
    }

    #[tokio::test]
    async fn node_output_above_allocation_fails_without_running_descendants() {
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "budget".to_string(),
            nodes: vec![node("a", &[]), node("b", &["a"])],
            max_parallel: 1,
            token_budget: Some(100),
        };
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let (handle, task) = runtime.start(Arc::new(OverBudgetExecutor));
        for _ in 0..100 {
            if handle.snapshot().nodes["a"].state == WorkflowNodeState::Failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(
            handle.snapshot().nodes["a"].state,
            WorkflowNodeState::Failed
        );
        assert_eq!(
            handle.snapshot().nodes["b"].state,
            WorkflowNodeState::Cancelled
        );
        handle.control(WorkflowControl::Cancel).unwrap();
        let snapshot = task.await.unwrap().unwrap();
        assert_eq!(snapshot.nodes["a"].state, WorkflowNodeState::Failed);
        assert_eq!(snapshot.nodes["b"].state, WorkflowNodeState::Cancelled);
        assert!(snapshot.results.is_empty());
    }

    #[test]
    fn token_budget_is_split_once_across_parallel_nodes() {
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "allocation".to_string(),
            nodes: vec![node("a", &[]), node("b", &[])],
            max_parallel: 2,
            token_budget: Some(100),
        };
        let snapshot = WorkflowSnapshot::new(&spec);
        assert_eq!(allocated_budget(&spec, &snapshot, 2), Some(50));
    }

    #[tokio::test]
    async fn cancel_marks_running_and_pending_nodes_cancelled() {
        let spec = WorkflowSpec {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            id: "cancel".to_string(),
            nodes: vec![node("a", &[]), node("b", &["a"])],
            max_parallel: 1,
            token_budget: None,
        };
        let runtime = WorkflowRuntime::new(spec, ceiling()).unwrap();
        let executor = Arc::new(FakeExecutor {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let (handle, task) = runtime.start(executor);
        tokio::time::sleep(Duration::from_millis(5)).await;
        handle.control(WorkflowControl::Cancel).unwrap();
        let snapshot = task.await.unwrap().unwrap();
        assert!(snapshot.cancelled);
        assert!(snapshot
            .nodes
            .values()
            .all(|record| record.state == WorkflowNodeState::Cancelled));
    }
}
