//! ACP v1 stdio adapter and long-lived TCP daemon.
//!
//! This implements the current baseline JSON-RPC methods directly to preserve the workspace's
//! Rust 1.80 compatibility (the current official Rust SDK requires a newer compiler).  Shapes are
//! kept intentionally small and schema-compatible: initialize, session/new, session/load,
//! session/prompt, session/cancel, session/set_mode, session/update, and permission requests.
//! `_wyj/session/list` and `_wyj/session/control` expose schema-versioned daemon extensions for
//! reconnect/attach, rewind, branch, workflow control, interrupt, and close.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, RwLock};
use wyj_core::{
    extract_preview, extract_title, new_session_id, now_iso, Agent, AgentSessionRuntime,
    CheckpointKind, CheckpointStore, ExecutionSurface, RewindScope, Session, SessionControl,
    SessionEvent, SessionEventEnvelope, SessionFile, SessionStore, WorkspaceDiffSummary,
};
use wyj_tools::ctx::{PermissionDecision, UiAskRequest};
use wyj_tools::{PermissionMode, ToolCtx};

#[derive(Clone)]
struct SessionEntry {
    runtime: Arc<AgentSessionRuntime>,
    context: Arc<ToolCtx>,
    cwd: PathBuf,
    permission_client: Arc<RwLock<Option<Weak<ConnectionState>>>>,
}

type SessionRegistry = Arc<RwLock<HashMap<String, SessionEntry>>>;

struct ConnectionState {
    agent_template: Agent,
    context_template: ToolCtx,
    configured_cwd: PathBuf,
    session_store: Option<Arc<SessionStore>>,
    sessions: SessionRegistry,
    subscriptions: Mutex<HashSet<String>>,
    close_sessions_on_disconnect: bool,
    writer: mpsc::UnboundedSender<String>,
    pending_client_requests: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    next_request_id: AtomicU64,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
}

pub async fn run_stdio(
    agent: Agent,
    context: ToolCtx,
    cwd: PathBuf,
    session_store: Option<Arc<SessionStore>>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
) -> Result<()> {
    run_connection_with_registry(
        tokio::io::stdin(),
        tokio::io::stdout(),
        agent,
        context,
        cwd,
        session_store,
        plugin_runtime,
        Arc::new(RwLock::new(HashMap::new())),
        true,
    )
    .await
}

pub async fn run_daemon(
    listen: &str,
    agent: Agent,
    context: ToolCtx,
    cwd: PathBuf,
    session_store: Option<Arc<SessionStore>>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind daemon listener {listen}"))?;
    let address = listener.local_addr()?;
    eprintln!("wyj-code daemon listening on {address}");
    serve_listener(listener, agent, context, cwd, session_store, plugin_runtime).await
}

async fn serve_listener(
    listener: tokio::net::TcpListener,
    agent: Agent,
    context: ToolCtx,
    cwd: PathBuf,
    session_store: Option<Arc<SessionStore>>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
) -> Result<()> {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    loop {
        let (stream, peer) = listener.accept().await?;
        let (reader, writer) = stream.into_split();
        let agent = agent.clone();
        let context = context.fork_for_surface(ExecutionSurface::AcpClient);
        let cwd = cwd.clone();
        let store = session_store.clone();
        let plugin_runtime = plugin_runtime.clone();
        let sessions = sessions.clone();
        tokio::spawn(async move {
            if let Err(error) = run_connection_with_registry(
                reader,
                writer,
                agent,
                context,
                cwd,
                store,
                plugin_runtime,
                sessions,
                false,
            )
            .await
            {
                tracing::warn!("ACP daemon connection {peer} closed with error: {error}");
            }
        });
    }
}

#[cfg(test)]
async fn run_connection<R, W>(
    reader: R,
    writer: W,
    agent: Agent,
    context: ToolCtx,
    cwd: PathBuf,
    session_store: Option<Arc<SessionStore>>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run_connection_with_registry(
        reader,
        writer,
        agent,
        context,
        cwd,
        session_store,
        plugin_runtime,
        Arc::new(RwLock::new(HashMap::new())),
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_connection_with_registry<R, W>(
    reader: R,
    writer: W,
    agent: Agent,
    context: ToolCtx,
    cwd: PathBuf,
    session_store: Option<Arc<SessionStore>>,
    plugin_runtime: Arc<wyj_store::plugin_runtime::PluginRuntimeCatalog>,
    sessions: SessionRegistry,
    close_sessions_on_disconnect: bool,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = writer_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });

    let state = Arc::new(ConnectionState {
        agent_template: agent,
        context_template: context,
        configured_cwd: std::fs::canonicalize(cwd)?,
        session_store,
        sessions,
        subscriptions: Mutex::new(HashSet::new()),
        close_sessions_on_disconnect,
        writer: writer_tx,
        pending_client_requests: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        plugin_runtime,
    });

    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                send_error(
                    &state.writer,
                    Value::Null,
                    -32700,
                    &format!("parse error: {error}"),
                );
                continue;
            }
        };
        if message.get("method").is_none() {
            resolve_client_response(&state, &message);
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_message(state.clone(), message.clone()).await {
                if let Some(id) = message.get("id").cloned() {
                    send_error(&state.writer, id, -32603, &error.to_string());
                }
            }
        });
    }
    disconnect(&state).await;
    Ok(())
}

async fn disconnect(state: &Arc<ConnectionState>) {
    let subscribed: Vec<String> = state
        .subscriptions
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    let entries: Vec<SessionEntry> = {
        let sessions = state.sessions.read().await;
        subscribed
            .iter()
            .filter_map(|id| sessions.get(id).cloned())
            .collect()
    };
    for entry in entries {
        let mut client = entry.permission_client.write().await;
        let owned = client
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, state));
        if owned {
            *client = None;
        }
        if state.close_sessions_on_disconnect {
            entry.runtime.interrupt();
        }
    }
    if state.close_sessions_on_disconnect {
        let mut sessions = state.sessions.write().await;
        for id in subscribed {
            sessions.remove(&id);
        }
    }
}

fn resolve_client_response(state: &ConnectionState, message: &Value) {
    let Some(id) = message.get("id") else {
        return;
    };
    let key = id_key(id);
    let sender = state.pending_client_requests.lock().unwrap().remove(&key);
    if let Some(sender) = sender {
        let value = message
            .get("result")
            .cloned()
            .unwrap_or_else(|| json!({"outcome": {"outcome": "cancelled"}}));
        let _ = sender.send(value);
    }
}

async fn handle_message(state: Arc<ConnectionState>, message: Value) -> Result<()> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing method"))?;
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let id = message.get("id").cloned();
    match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(u16::MAX as u64);
            respond(
                &state.writer,
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "agentCapabilities": {
                        "loadSession": state.session_store.is_some(),
                        "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                        "mcpCapabilities": {"http": true, "sse": true},
                        "sessionCapabilities": {"close": {}},
                        "auth": {}
                    },
                    "authMethods": [],
                    "agentInfo": {
                        "name": "wyj-code",
                        "title": "wyj-code",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "_meta": {
                        "wyjCode": {
                            "schemaVersion": wyj_core::INTERFACE_SCHEMA_VERSION,
                            "globalDaemonSessions": !state.close_sessions_on_disconnect,
                            "extensionMethods": ["_wyj/session/list", "_wyj/session/control"]
                        }
                    }
                }),
            );
        }
        "session/new" => {
            let requested_cwd = absolute_cwd(&params)?;
            ensure_same_cwd(&state.configured_cwd, &requested_cwd)?;
            let session_id = new_session_id();
            create_session(&state, session_id.clone(), Session::new(), requested_cwd).await?;
            respond(
                &state.writer,
                id,
                json!({"sessionId": session_id, "modes": modes("normal")}),
            );
        }
        "session/load" => {
            let requested_cwd = absolute_cwd(&params)?;
            ensure_same_cwd(&state.configured_cwd, &requested_cwd)?;
            let session_id = required_string(&params, "sessionId")?;
            if let Some(entry) = state.sessions.read().await.get(&session_id).cloned() {
                ensure_same_cwd(&entry.cwd, &requested_cwd)?;
                attach_session(&state, &entry).await;
                if !entry.runtime.is_running() {
                    stream_history(&entry, &state.writer).await;
                }
                respond(&state.writer, id, json!({"modes": modes("normal")}));
                return Ok(());
            }
            let file = state
                .session_store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("session/load is unavailable"))?
                .load(&session_id)?;
            let mut session = Session::new();
            session.messages = file.messages;
            session.total_input_tokens = file.input_tokens;
            session.total_output_tokens = file.output_tokens;
            session.routing_events = file.routing_events;
            session.current_checkpoint_id = file.current_checkpoint_id;
            session.branch_parent_session_id = file.branch_parent_session_id;
            session.branch_parent_checkpoint_id = file.branch_parent_checkpoint_id;
            let entry = create_session(&state, session_id.clone(), session, requested_cwd).await?;
            stream_history(&entry, &state.writer).await;
            respond(&state.writer, id, json!({"modes": modes("normal")}));
        }
        "session/prompt" => {
            let session_id = required_string(&params, "sessionId")?;
            let entry = session(&state, &session_id).await?;
            let blocks = prompt_blocks(&params)?;
            let outcome = entry.runtime.submit_blocks(blocks)?.await;
            let (stop_reason, event, error) = match &outcome {
                Ok(outcome) if outcome.error.is_none() => ("end_turn", "turn_finished", None),
                Ok(outcome) if outcome.cancelled => ("cancelled", "turn_error", None),
                Ok(outcome) => ("refusal", "turn_error", outcome.error.clone()),
                Err(error) => ("cancelled", "turn_error", Some(error.to_string())),
            };
            let payload = json!({
                "session_id": session_id,
                "cwd": entry.cwd,
                "error": error,
            });
            for result in state
                .plugin_runtime
                .emit_channel_event(event, &payload)
                .await
            {
                if !result.success {
                    tracing::warn!("plugin channel {} failed: {}", result.name, result.output);
                }
            }
            persist_session(&state, &entry).await;
            respond(&state.writer, id, json!({"stopReason": stop_reason}));
        }
        "session/cancel" => {
            let session_id = required_string(&params, "sessionId")?;
            if let Ok(entry) = session(&state, &session_id).await {
                entry.runtime.interrupt();
            }
            respond(&state.writer, id, json!({}));
        }
        "session/set_mode" => {
            let session_id = required_string(&params, "sessionId")?;
            let mode_id = required_string(&params, "modeId")?;
            let entry = session(&state, &session_id).await?;
            set_mode(&entry.context, &mode_id)?;
            send_update(
                &state.writer,
                &session_id,
                json!({"sessionUpdate": "current_mode_update", "currentModeId": mode_id}),
            );
            respond(&state.writer, id, json!({}));
        }
        "session/close" => {
            let session_id = required_string(&params, "sessionId")?;
            if let Some(entry) = state.sessions.write().await.remove(&session_id) {
                entry.runtime.interrupt();
                persist_session(&state, &entry).await;
            }
            respond(&state.writer, id, json!({}));
        }
        "_wyj/session/list" => {
            let entries: Vec<SessionEntry> =
                state.sessions.read().await.values().cloned().collect();
            let sessions = entries
                .into_iter()
                .map(|entry| {
                    json!({
                        "sessionId": entry.runtime.session_id(),
                        "cwd": entry.cwd,
                        "running": entry.runtime.is_running()
                    })
                })
                .collect::<Vec<_>>();
            respond(
                &state.writer,
                id,
                json!({"schemaVersion": wyj_core::INTERFACE_SCHEMA_VERSION, "sessions": sessions}),
            );
        }
        "_wyj/session/control" => {
            let session_id = required_string(&params, "sessionId")?;
            let control: SessionControl = serde_json::from_value(
                params
                    .get("control")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing control"))?,
            )?;
            let result = apply_session_control(
                &state,
                &session_id,
                control,
                params
                    .get("confirmed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )
            .await?;
            respond(&state.writer, id, result);
        }
        _ if method.starts_with('_') => {
            send_error_if_request(
                &state.writer,
                id,
                -32601,
                "unknown wyj-code extension method",
            );
        }
        _ => send_error_if_request(&state.writer, id, -32601, "method not found"),
    }
    Ok(())
}

async fn create_session(
    state: &Arc<ConnectionState>,
    session_id: String,
    session: Session,
    cwd: PathBuf,
) -> Result<SessionEntry> {
    if state.sessions.read().await.contains_key(&session_id) {
        bail!("session already exists: {session_id}")
    }
    let mut agent = state.agent_template.clone();
    agent.set_session_id(session_id.clone());
    let mut context = state
        .context_template
        .fork_for_surface(ExecutionSurface::AcpClient);
    let (ui_tx, ui_rx) = mpsc::channel(16);
    context.ui_ask_tx = Some(ui_tx);
    let context = Arc::new(context);
    let runtime = AgentSessionRuntime::new(session_id.clone(), agent, context.clone(), session);
    let permission_client = Arc::new(RwLock::new(Some(Arc::downgrade(state))));
    let entry = SessionEntry {
        runtime: runtime.clone(),
        context,
        cwd,
        permission_client: permission_client.clone(),
    };
    state
        .sessions
        .write()
        .await
        .insert(session_id.clone(), entry.clone());
    attach_session(state, &entry).await;
    tokio::spawn(permission_bridge(
        permission_client,
        runtime.emitter(),
        session_id,
        ui_rx,
    ));
    Ok(entry)
}

async fn attach_session(state: &Arc<ConnectionState>, entry: &SessionEntry) {
    *entry.permission_client.write().await = Some(Arc::downgrade(state));
    let session_id = entry.runtime.session_id().to_string();
    if state
        .subscriptions
        .lock()
        .unwrap()
        .insert(session_id.clone())
    {
        tokio::spawn(forward_events(
            state.writer.clone(),
            entry.runtime.subscribe(),
        ));
    }
}

async fn session(state: &ConnectionState, session_id: &str) -> Result<SessionEntry> {
    state
        .sessions
        .read()
        .await
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown session: {session_id}"))
}

async fn apply_session_control(
    state: &Arc<ConnectionState>,
    session_id: &str,
    control: SessionControl,
    confirmed: bool,
) -> Result<Value> {
    let entry = session(state, session_id).await?;
    match control {
        SessionControl::Submit { text } => {
            let outcome = entry.runtime.submit_text(text)?.await?;
            persist_session(state, &entry).await;
            Ok(json!({
                "accepted": outcome.error.is_none(),
                "cancelled": outcome.cancelled,
                "error": outcome.error
            }))
        }
        SessionControl::Interrupt => Ok(json!({"interrupted": entry.runtime.interrupt()})),
        SessionControl::Rewind {
            checkpoint_id,
            scope,
        } => rewind_session(state, &entry, &checkpoint_id, &scope, confirmed).await,
        SessionControl::Branch {
            checkpoint_id,
            restore_files,
        } => branch_session(state, &entry, &checkpoint_id, restore_files, confirmed).await,
        SessionControl::Workflow {
            workflow_id,
            control,
        } => {
            crate::workflow_cmd::queue_control(&entry.cwd, &workflow_id, &control)?;
            Ok(json!({"queued": true, "workflowId": workflow_id}))
        }
        SessionControl::Close => {
            if let Some(entry) = state.sessions.write().await.remove(session_id) {
                entry.runtime.interrupt();
                persist_session(state, &entry).await;
            }
            Ok(json!({"closed": true}))
        }
        SessionControl::PermissionDecision { .. } => {
            bail!("permission decisions must be returned as the JSON-RPC permission response")
        }
    }
}

async fn rewind_session(
    state: &ConnectionState,
    entry: &SessionEntry,
    checkpoint_id: &str,
    scope: &str,
    confirmed: bool,
) -> Result<Value> {
    if entry.runtime.is_running() {
        bail!("cannot rewind a session while a turn is running")
    }
    let store = state
        .session_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("checkpoint storage is unavailable"))?;
    let checkpoints = CheckpointStore::new(store.dir(), entry.runtime.session_id())?;
    let checkpoint = checkpoints.load(checkpoint_id)?;
    let scope = parse_rewind_scope(scope)?;
    let preview = if matches!(scope, RewindScope::Files | RewindScope::Both) {
        Some(checkpoints.preview_files(checkpoint_id, &entry.cwd)?)
    } else {
        None
    };
    if preview
        .as_ref()
        .is_some_and(|preview| preview.requires_confirmation && !confirmed)
    {
        return Ok(json!({
            "applied": false,
            "requiresConfirmation": true,
            "preview": preview
        }));
    }
    let mut current = entry.runtime.session_snapshot().await;
    let protection = checkpoints.create(
        &entry.cwd,
        &current.messages,
        CheckpointKind::PreRewind,
        Some(format!("before ACP rewind {checkpoint_id}")),
    )?;
    if matches!(scope, RewindScope::Files | RewindScope::Both) {
        checkpoints.restore_files(checkpoint_id, &entry.cwd, confirmed)?;
    }
    if matches!(scope, RewindScope::Conversation | RewindScope::Both) {
        current.messages = checkpoint.messages;
    }
    current.current_checkpoint_id = Some(checkpoint_id.to_string());
    entry.runtime.replace_session(current).await?;
    entry
        .runtime
        .emitter()
        .emit(SessionEvent::CheckpointChanged {
            checkpoint_id: checkpoint_id.to_string(),
            label: checkpoint.name,
        });
    if let Some(preview) = &preview {
        entry.runtime.emitter().emit(SessionEvent::DiffAvailable {
            checkpoint_id: Some(checkpoint_id.to_string()),
            summary: WorkspaceDiffSummary {
                changed_files: preview.affected_files.len(),
                paths: preview.affected_files.clone(),
                ..WorkspaceDiffSummary::default()
            },
        });
    }
    persist_session(state, entry).await;
    Ok(json!({
        "applied": true,
        "checkpointId": checkpoint_id,
        "protectionCheckpointId": protection.id,
        "preview": preview
    }))
}

async fn branch_session(
    state: &Arc<ConnectionState>,
    entry: &SessionEntry,
    checkpoint_id: &str,
    restore_files: bool,
    confirmed: bool,
) -> Result<Value> {
    if entry.runtime.is_running() {
        bail!("cannot branch a session while a turn is running")
    }
    let store = state
        .session_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("session/checkpoint storage is unavailable"))?;
    persist_session(state, entry).await;
    let checkpoints = CheckpointStore::new(store.dir(), entry.runtime.session_id())?;
    let checkpoint = checkpoints.load(checkpoint_id)?;
    let preview = if restore_files {
        Some(checkpoints.preview_files(checkpoint_id, &entry.cwd)?)
    } else {
        None
    };
    if preview
        .as_ref()
        .is_some_and(|preview| preview.requires_confirmation && !confirmed)
    {
        return Ok(json!({
            "created": false,
            "requiresConfirmation": true,
            "preview": preview
        }));
    }
    if restore_files {
        let current = entry.runtime.session_snapshot().await;
        checkpoints.create(
            &entry.cwd,
            &current.messages,
            CheckpointKind::PreRewind,
            Some(format!("before ACP branch restore {checkpoint_id}")),
        )?;
        checkpoints.restore_files(checkpoint_id, &entry.cwd, confirmed)?;
    }
    let branch = store.branch_from_checkpoint(entry.runtime.session_id(), &checkpoint)?;
    let branch_id = branch.session_id.clone();
    let branch_entry = create_session(
        state,
        branch_id.clone(),
        session_from_file(branch),
        entry.cwd.clone(),
    )
    .await?;
    branch_entry
        .runtime
        .emitter()
        .emit(SessionEvent::CheckpointChanged {
            checkpoint_id: checkpoint_id.to_string(),
            label: checkpoint.name,
        });
    Ok(json!({
        "created": true,
        "sessionId": branch_id,
        "parentSessionId": entry.runtime.session_id(),
        "checkpointId": checkpoint_id,
        "preview": preview
    }))
}

fn parse_rewind_scope(scope: &str) -> Result<RewindScope> {
    match scope {
        "conversation" => Ok(RewindScope::Conversation),
        "files" => Ok(RewindScope::Files),
        "both" => Ok(RewindScope::Both),
        _ => bail!("rewind scope must be conversation, files, or both"),
    }
}

fn session_from_file(file: SessionFile) -> Session {
    let mut session = Session::new();
    session.messages = file.messages;
    session.total_input_tokens = file.input_tokens;
    session.total_output_tokens = file.output_tokens;
    session.routing_events = file.routing_events;
    session.current_checkpoint_id = file.current_checkpoint_id;
    session.branch_parent_session_id = file.branch_parent_session_id;
    session.branch_parent_checkpoint_id = file.branch_parent_checkpoint_id;
    session
}

async fn forward_events(
    writer: mpsc::UnboundedSender<String>,
    mut events: tokio::sync::broadcast::Receiver<SessionEventEnvelope>,
) {
    loop {
        let envelope = match events.recv().await {
            Ok(envelope) => envelope,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!("ACP session event subscriber lagged by {skipped} events");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        let session_id = envelope.session_id.clone();
        let schema_version = envelope.schema_version;
        let sequence = envelope.sequence;
        let timestamp = envelope.timestamp.clone();
        match envelope.event {
            SessionEvent::TextDelta { text } => send_update(
                &writer,
                &session_id,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }),
            ),
            SessionEvent::ThinkingDelta { text } => send_update(
                &writer,
                &session_id,
                json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": text}
                }),
            ),
            SessionEvent::ToolStarted {
                call_id,
                name,
                input,
            } => send_update(
                &writer,
                &session_id,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": name,
                    "kind": tool_kind(&name),
                    "status": "in_progress",
                    "rawInput": input
                }),
            ),
            SessionEvent::ToolFinished {
                call_id,
                output,
                is_error,
                ..
            } => send_update(
                &writer,
                &session_id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": if is_error { "failed" } else { "completed" },
                    "rawOutput": output
                }),
            ),
            SessionEvent::Error { message, .. } => send_update(
                &writer,
                &session_id,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": format!("\n[wyj-code error] {message}\n")}
                }),
            ),
            SessionEvent::TurnFinished => {}
            other => {
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "_wyj/session_event",
                    "params": SessionEventEnvelope {
                        schema_version,
                        session_id,
                        sequence,
                        timestamp,
                        event: other,
                    }
                });
                let _ = writer.send(notification.to_string());
            }
        }
    }
}

async fn permission_bridge(
    client: Arc<RwLock<Option<Weak<ConnectionState>>>>,
    emitter: wyj_core::SessionEventEmitter,
    session_id: String,
    mut requests: mpsc::Receiver<UiAskRequest>,
) {
    while let Some(request) = requests.recv().await {
        let state = client.read().await.as_ref().and_then(Weak::upgrade);
        match request {
            UiAskRequest::ToolPermission {
                tool_name,
                action_summary,
                response_tx,
            } => {
                let result = match state.as_deref() {
                    Some(state) => {
                        request_permission(
                            state,
                            &emitter,
                            &session_id,
                            &tool_name,
                            &action_summary,
                        )
                        .await
                    }
                    None => PermissionDecision::Deny,
                };
                let _ = response_tx.send(result);
            }
            UiAskRequest::ExitPlanMode { response_tx, .. } => {
                let result = match state.as_deref() {
                    Some(state) => {
                        request_permission(
                            state,
                            &emitter,
                            &session_id,
                            "ExitPlanMode",
                            "Switch from plan mode to execution mode",
                        )
                        .await
                    }
                    None => PermissionDecision::Deny,
                };
                let _ = response_tx.send(!matches!(result, PermissionDecision::Deny));
            }
            UiAskRequest::Questions { response_tx, .. } => {
                // ACP elicitation is optional and not advertised. Fail closed instead of silently
                // fabricating answers.
                let _ = response_tx.send(None);
            }
        }
    }
}

async fn request_permission(
    state: &ConnectionState,
    emitter: &wyj_core::SessionEventEmitter,
    session_id: &str,
    tool_name: &str,
    action_summary: &str,
) -> PermissionDecision {
    let id = format!(
        "wyj-permission-{}",
        state.next_request_id.fetch_add(1, Ordering::Relaxed)
    );
    let (tx, rx) = oneshot::channel();
    let request_key = id_key(&Value::String(id.clone()));
    state
        .pending_client_requests
        .lock()
        .unwrap()
        .insert(request_key.clone(), tx);
    emitter.emit(SessionEvent::PermissionRequested {
        request_id: id.clone(),
        tool_name: tool_name.to_string(),
        action_summary: action_summary.to_string(),
        one_shot_only: false,
    });
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": format!("permission-{tool_name}"),
                "title": action_summary,
                "kind": tool_kind(tool_name),
                "status": "pending"
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}
            ]
        }
    });
    if state.writer.send(request.to_string()).is_err() {
        state
            .pending_client_requests
            .lock()
            .unwrap()
            .remove(&request_key);
        return PermissionDecision::Deny;
    }
    let result = match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(value)) => value,
        _ => {
            state
                .pending_client_requests
                .lock()
                .unwrap()
                .remove(&request_key);
            return PermissionDecision::Deny;
        }
    };
    match result.pointer("/outcome/optionId").and_then(Value::as_str) {
        Some("allow_once") => PermissionDecision::AllowOnce,
        Some("allow_always") => PermissionDecision::AllowAlways,
        _ => PermissionDecision::Deny,
    }
}

fn prompt_blocks(params: &Value) -> Result<Vec<wyj_api::types::ContentBlock>> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("prompt must be an array"))?;
    let mut result = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => result.push(wyj_api::types::ContentBlock::Text {
                text: required_string(block, "text")?,
            }),
            Some("resource_link") => result.push(wyj_api::types::ContentBlock::Text {
                text: format!(
                    "[Referenced resource: {} ({})]",
                    required_string(block, "name")?,
                    required_string(block, "uri")?
                ),
            }),
            Some(other) => bail!("unsupported ACP prompt content type: {other}"),
            None => bail!("prompt content block is missing type"),
        }
    }
    if result.is_empty() {
        bail!("prompt cannot be empty")
    }
    Ok(result)
}

fn set_mode(context: &ToolCtx, mode: &str) -> Result<()> {
    match mode {
        "normal" => context.set_permission_mode(PermissionMode::Prompt),
        "plan" => {
            let allowed = [
                "Read",
                "Glob",
                "Grep",
                "CodeSearch",
                "WebFetch",
                "WebSearch",
                "AskQuestion",
                "Write",
                "Edit",
                "Bash",
                "BashOutput",
                "ExitPlanMode",
                "TodoWrite",
                "Agent",
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            context.set_permission_mode(PermissionMode::Plan(allowed));
        }
        _ => bail!("unsupported session mode: {mode}"),
    }
    Ok(())
}

fn modes(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": [
            {"id": "normal", "name": "Normal", "description": "Interactive coding with per-tool permission requests"},
            {"id": "plan", "name": "Plan", "description": "Read-only analysis plus approved plan-document writes"}
        ]
    })
}

async fn stream_history(entry: &SessionEntry, writer: &mpsc::UnboundedSender<String>) {
    let session = entry.runtime.session_snapshot().await;
    for message in session.messages {
        let role = match message.role {
            wyj_api::types::Role::User => "user_message_chunk",
            wyj_api::types::Role::Assistant => "agent_message_chunk",
        };
        for block in message.content {
            if let wyj_api::types::ContentBlock::Text { text } = block {
                send_update(
                    writer,
                    entry.runtime.session_id(),
                    json!({
                        "sessionUpdate": role,
                        "content": {"type": "text", "text": text}
                    }),
                );
            }
        }
    }
}

async fn persist_session(state: &ConnectionState, entry: &SessionEntry) {
    let Some(store) = &state.session_store else {
        return;
    };
    let session = entry.runtime.session_snapshot().await;
    let _ = store.save(&SessionFile {
        session_id: entry.runtime.session_id().to_string(),
        title: extract_title(&session.messages),
        last_preview: extract_preview(&session.messages),
        cwd: entry.cwd.display().to_string(),
        timestamp: now_iso(),
        turns: session.messages.len(),
        input_tokens: session.total_input_tokens,
        output_tokens: session.total_output_tokens,
        messages: session.messages,
        routing_events: session.routing_events,
        current_checkpoint_id: session.current_checkpoint_id,
        branch_parent_session_id: session.branch_parent_session_id,
        branch_parent_checkpoint_id: session.branch_parent_checkpoint_id,
        title_generated: false,
    });
}

fn absolute_cwd(params: &Value) -> Result<PathBuf> {
    let path = PathBuf::from(required_string(params, "cwd")?);
    if !path.is_absolute() {
        bail!("ACP cwd must be absolute")
    }
    std::fs::canonicalize(path).context("canonicalize ACP cwd")
}

fn ensure_same_cwd(configured: &Path, requested: &Path) -> Result<()> {
    if configured != requested {
        bail!(
            "this daemon was scoped to {}; requested session cwd is {}",
            configured.display(),
            requested.display()
        )
    }
    Ok(())
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("missing or invalid {field}"))
}

fn tool_kind(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "read" => "read",
        "write" | "edit" => "edit",
        "glob" | "grep" | "codesearch" | "toolsearch" | "websearch" => "search",
        "webfetch" => "fetch",
        "bash" | "bashoutput" | "killshell" | "agent" => "execute",
        "exitplanmode" => "switch_mode",
        _ => "other",
    }
}

fn respond(writer: &mpsc::UnboundedSender<String>, id: Option<Value>, result: Value) {
    if let Some(id) = id {
        let _ = writer.send(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
    }
}

fn send_update(writer: &mpsc::UnboundedSender<String>, session_id: &str, update: Value) {
    let _ = writer.send(
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update}
        })
        .to_string(),
    );
}

fn send_error_if_request(
    writer: &mpsc::UnboundedSender<String>,
    id: Option<Value>,
    code: i64,
    message: &str,
) {
    if let Some(id) = id {
        send_error(writer, id, code, message);
    }
}

fn send_error(writer: &mpsc::UnboundedSender<String>, id: Value, code: i64, message: &str) {
    let _ = writer.send(
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        })
        .to_string(),
    );
}

fn id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wyj_api::provider::{EventStream, Provider, RequestOptions};
    use wyj_api::types::{Message, StopReason, StreamEvent, ToolDefinition};
    use wyj_core::tool::{Tool, ToolContext, ToolResult};

    #[test]
    fn prompt_parser_supports_baseline_text_and_resource_links() {
        let blocks = prompt_blocks(&json!({
            "prompt": [
                {"type": "text", "text": "review this"},
                {"type": "resource_link", "name": "lib.rs", "uri": "file:///repo/lib.rs"}
            ]
        }))
        .unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn modes_never_expose_bypass_permissions() {
        let value = modes("normal").to_string();
        assert!(value.contains("normal"));
        assert!(value.contains("plan"));
        assert!(!value.contains("bypass"));
    }

    #[test]
    fn tool_kind_is_stable_for_acp_rendering() {
        assert_eq!(tool_kind("Read"), "read");
        assert_eq!(tool_kind("Edit"), "edit");
        assert_eq!(tool_kind("Bash"), "execute");
    }

    struct StaticProvider;

    #[async_trait]
    impl Provider for StaticProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &RequestOptions,
        ) -> Result<EventStream> {
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::TextDelta("ACP hello".to_string())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    struct PermissionProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for PermissionProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &RequestOptions,
        ) -> Result<EventStream> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "tool-1".to_string(),
                        name: "ApprovalTool".to_string(),
                    }),
                    Ok(StreamEvent::ToolUseDelta {
                        id: "tool-1".to_string(),
                        json_delta: r#"{"value":"ok"}"#.to_string(),
                    }),
                    Ok(StreamEvent::ToolUseEnd {
                        id: "tool-1".to_string(),
                    }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::ToolUse,
                    }),
                ])))
            } else {
                Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::TextDelta("permission granted".to_string())),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                    }),
                ])))
            }
        }
    }

    struct ApprovalTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ApprovalTool {
        fn name(&self) -> &str {
            "ApprovalTool"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "A permission-gated test tool".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["value"],
                    "properties": {"value": {"type": "string"}},
                    "additionalProperties": false
                }),
                native: None,
            }
        }

        fn needs_permission(&self, _input: &Value) -> bool {
            true
        }

        fn action_summary(&self, _input: &Value) -> String {
            "approve ACP test tool".to_string()
        }

        async fn run(&self, _input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::ok("approved"))
        }
    }

    struct BlockingProvider;

    #[async_trait]
    impl Provider for BlockingProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _opts: &RequestOptions,
        ) -> Result<EventStream> {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(Box::pin(stream::empty()))
        }
    }

    async fn read_session_id<R>(lines: &mut tokio::io::Lines<R>) -> String
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        loop {
            let value: Value = serde_json::from_str(
                &lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("ACP connection closed before session/new response"),
            )
            .unwrap();
            if value["id"] == 2 {
                return value
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string();
            }
        }
    }

    async fn read_response<R>(lines: &mut tokio::io::Lines<R>, id: u64) -> Value
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        loop {
            let value: Value = serde_json::from_str(
                &lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("ACP connection closed before response"),
            )
            .unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    #[tokio::test]
    async fn stdio_protocol_runs_initialize_new_and_prompt_end_to_end() {
        let cwd = tempfile::tempdir().unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server_task = tokio::spawn(run_connection(
            server_read,
            server_write,
            Agent::new(Arc::new(StaticProvider)),
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            None,
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = BufReader::new(client_read).lines();

        client_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd.path(),"mcpServers":[]}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        let mut session_id = None;
        for _ in 0..2 {
            let value: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if value["id"] == 2 {
                session_id = value
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        let session_id = session_id.unwrap();
        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc":"2.0",
                        "id":3,
                        "method":"session/prompt",
                        "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hi"}]}
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        let mut saw_update = false;
        let mut saw_response = false;
        for _ in 0..4 {
            let Some(line) = lines.next_line().await.unwrap() else {
                break;
            };
            let value: Value = serde_json::from_str(&line).unwrap();
            saw_update |= value["method"] == "session/update"
                && value.pointer("/params/update/sessionUpdate")
                    == Some(&Value::String("agent_message_chunk".to_string()));
            if value["id"] == 3 {
                assert_eq!(
                    value.pointer("/result/stopReason").and_then(Value::as_str),
                    Some("end_turn")
                );
                saw_response = true;
                break;
            }
        }
        assert!(saw_update);
        assert!(saw_response);
        drop(client_write);
        server_task.abort();
    }

    #[tokio::test]
    async fn permission_request_response_round_trip_executes_the_tool() {
        let cwd = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(Arc::new(PermissionProvider {
            calls: AtomicUsize::new(0),
        }));
        agent.register_tool(Arc::new(ApprovalTool {
            calls: calls.clone(),
        }));
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server_task = tokio::spawn(run_connection(
            server_read,
            server_write,
            agent,
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            None,
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = BufReader::new(client_read).lines();
        client_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd.path()}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let session_id = read_session_id(&mut lines).await;
        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc":"2.0",
                        "id":3,
                        "method":"session/prompt",
                        "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"use tool"}]}
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        let mut saw_permission = false;
        let mut saw_response = false;
        for _ in 0..12 {
            let line = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["method"] == "session/request_permission" {
                saw_permission = true;
                let response = json!({
                    "jsonrpc":"2.0",
                    "id":value["id"].clone(),
                    "result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}
                });
                client_write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                client_write.flush().await.unwrap();
            }
            if value["id"] == 3 {
                assert_eq!(
                    value.pointer("/result/stopReason").and_then(Value::as_str),
                    Some("end_turn")
                );
                saw_response = true;
                break;
            }
        }
        assert!(saw_permission);
        assert!(saw_response);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(client_write);
        server_task.abort();
    }

    #[tokio::test]
    async fn cancel_interrupts_an_active_prompt_and_responds_to_both_requests() {
        let cwd = tempfile::tempdir().unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server_task = tokio::spawn(run_connection(
            server_read,
            server_write,
            Agent::new(Arc::new(BlockingProvider)),
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            None,
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = BufReader::new(client_read).lines();
        client_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd.path()}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let session_id = read_session_id(&mut lines).await;
        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"wait"}]}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":4,"method":"session/cancel","params":{"sessionId":session_id}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        let mut prompt_cancelled = false;
        let mut cancel_responded = false;
        for _ in 0..8 {
            let line = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == 3 {
                prompt_cancelled = value.pointer("/result/stopReason").and_then(Value::as_str)
                    == Some("cancelled");
            }
            if value["id"] == 4 {
                cancel_responded = true;
            }
            if prompt_cancelled && cancel_responded {
                break;
            }
        }
        assert!(prompt_cancelled);
        assert!(cancel_responded);
        drop(client_write);
        server_task.abort();
    }

    #[tokio::test]
    async fn session_load_streams_persisted_history() {
        let cwd = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::new(sessions.path().to_path_buf()).unwrap());
        store
            .save(&SessionFile {
                session_id: "persisted".to_string(),
                title: "saved".to_string(),
                last_preview: "saved answer".to_string(),
                cwd: cwd.path().display().to_string(),
                timestamp: now_iso(),
                turns: 2,
                input_tokens: 1,
                output_tokens: 2,
                messages: vec![
                    Message::user("saved question"),
                    Message::assistant_text("saved answer"),
                ],
                routing_events: Vec::new(),
                current_checkpoint_id: None,
                branch_parent_session_id: None,
                branch_parent_checkpoint_id: None,
                title_generated: false,
            })
            .unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server_task = tokio::spawn(run_connection(
            server_read,
            server_write,
            Agent::new(Arc::new(StaticProvider)),
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            Some(store),
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = BufReader::new(client_read).lines();
        client_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"cwd":cwd.path(),"sessionId":"persisted"}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let mut saw_history = false;
        let mut saw_loaded = false;
        for _ in 0..6 {
            let value: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if value["method"] == "session/update"
                && value
                    .pointer("/params/update/content/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("saved answer"))
            {
                saw_history = true;
            }
            if value["id"] == 2 {
                saw_loaded = true;
            }
            if saw_history && saw_loaded {
                break;
            }
        }
        assert!(saw_history);
        assert!(saw_loaded);
        drop(client_write);
        server_task.abort();
    }

    #[tokio::test]
    async fn extension_control_rewinds_and_branches_persisted_sessions() {
        let cwd = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::new(sessions.path().to_path_buf()).unwrap());
        std::fs::write(cwd.path().join("state.txt"), "before\n").unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server_task = tokio::spawn(run_connection(
            server_read,
            server_write,
            Agent::new(Arc::new(StaticProvider)),
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            Some(store.clone()),
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = BufReader::new(client_read).lines();
        client_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd.path()}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let session_id = read_session_id(&mut lines).await;
        let checkpoints = CheckpointStore::new(store.dir(), session_id.clone()).unwrap();
        let checkpoint = checkpoints
            .create(
                cwd.path(),
                &[Message::user("before")],
                CheckpointKind::Manual,
                Some("before".to_string()),
            )
            .unwrap();
        std::fs::write(cwd.path().join("state.txt"), "after\n").unwrap();

        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc":"2.0",
                        "id":3,
                        "method":"_wyj/session/control",
                        "params":{
                            "sessionId":session_id,
                            "control":{"type":"rewind","checkpoint_id":checkpoint.id,"scope":"files"}
                        }
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let preview = read_response(&mut lines, 3).await;
        assert_eq!(
            preview.pointer("/result/requiresConfirmation"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("state.txt")).unwrap(),
            "after\n"
        );

        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc":"2.0",
                        "id":4,
                        "method":"_wyj/session/control",
                        "params":{
                            "sessionId":session_id,
                            "confirmed":true,
                            "control":{"type":"rewind","checkpoint_id":checkpoint.id,"scope":"files"}
                        }
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let applied = read_response(&mut lines, 4).await;
        assert_eq!(applied.pointer("/result/applied"), Some(&Value::Bool(true)));
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("state.txt")).unwrap(),
            "before\n"
        );

        client_write
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc":"2.0",
                        "id":5,
                        "method":"_wyj/session/control",
                        "params":{
                            "sessionId":session_id,
                            "control":{"type":"branch","checkpoint_id":checkpoint.id,"restore_files":false}
                        }
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let branched = read_response(&mut lines, 5).await;
        let branch_id = branched
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .unwrap();
        assert_ne!(branch_id, session_id);
        assert_eq!(
            store
                .load(branch_id)
                .unwrap()
                .branch_parent_checkpoint_id
                .as_deref(),
            Some(checkpoint.id.as_str())
        );
        drop(client_write);
        server_task.abort();
    }

    async fn tcp_new_session(address: std::net::SocketAddr, cwd: &Path) -> String {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        write.flush().await.unwrap();
        loop {
            let value: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if value["id"] == 2 {
                return value
                    .pointer("/result/sessionId")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string();
            }
        }
    }

    #[tokio::test]
    async fn tcp_daemon_accepts_multiple_connections_and_sessions() {
        let cwd = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_listener(
            listener,
            Agent::new(Arc::new(StaticProvider)),
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            None,
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));
        let (first, second) = tokio::join!(
            tcp_new_session(address, cwd.path()),
            tcp_new_session(address, cwd.path())
        );
        assert_ne!(first, second);
        server.abort();
    }

    #[tokio::test]
    async fn daemon_sessions_can_be_attached_and_cancelled_from_another_connection() {
        let cwd = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_listener(
            listener,
            Agent::new(Arc::new(BlockingProvider)),
            ToolCtx::new(cwd.path()),
            cwd.path().to_path_buf(),
            None,
            Arc::new(wyj_store::plugin_runtime::PluginRuntimeCatalog::default()),
        ));

        let first = tokio::net::TcpStream::connect(address).await.unwrap();
        let (first_read, mut first_write) = first.into_split();
        let mut first_lines = BufReader::new(first_read).lines();
        first_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd.path()}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        first_write.flush().await.unwrap();
        let session_id = read_session_id(&mut first_lines).await;
        first_write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"wait"}]}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        first_write.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let second = tokio::net::TcpStream::connect(address).await.unwrap();
        let (second_read, mut second_write) = second.into_split();
        let mut second_lines = BufReader::new(second_read).lines();
        second_write
            .write_all(
                format!(
                    "{}\n{}\n{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"cwd":cwd.path(),"sessionId":session_id}}),
                    json!({"jsonrpc":"2.0","id":3,"method":"_wyj/session/list","params":{}}),
                    json!({"jsonrpc":"2.0","id":4,"method":"session/cancel","params":{"sessionId":session_id}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        second_write.flush().await.unwrap();

        let mut attached = false;
        let mut listed = false;
        let mut cancelled = false;
        for _ in 0..10 {
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(2), second_lines.next_line())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            attached |= value["id"] == 2 && value.get("result").is_some();
            listed |= value["id"] == 3
                && value
                    .pointer("/result/sessions")
                    .and_then(Value::as_array)
                    .is_some_and(|sessions| {
                        sessions.iter().any(|session| {
                            session.get("sessionId").and_then(Value::as_str)
                                == Some(session_id.as_str())
                        })
                    });
            cancelled |= value["id"] == 4;
            if attached && listed && cancelled {
                break;
            }
        }
        assert!(attached && listed && cancelled);

        let mut prompt_cancelled = false;
        for _ in 0..8 {
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(2), first_lines.next_line())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == 3 {
                prompt_cancelled = value.pointer("/result/stopReason").and_then(Value::as_str)
                    == Some("cancelled");
                break;
            }
        }
        assert!(prompt_cancelled);
        server.abort();
    }
}
