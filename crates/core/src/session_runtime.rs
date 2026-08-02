//! Frontend-neutral session actor used by daemon and ACP adapters.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use anyhow::{bail, Result};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use wyj_api::types::ContentBlock;

use crate::tool::ToolContext;
use crate::{Agent, Session, SessionControl, SessionEvent, SessionEventEnvelope};

#[derive(Clone)]
pub struct SessionEventEmitter {
    session_id: String,
    sequence: Arc<AtomicU64>,
    tx: broadcast::Sender<SessionEventEnvelope>,
}

impl SessionEventEmitter {
    pub fn new(session_id: impl Into<String>, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.clamp(16, 4096));
        Self {
            session_id: session_id.into(),
            sequence: Arc::new(AtomicU64::new(1)),
            tx,
        }
    }

    pub fn emit(&self, event: SessionEvent) -> SessionEventEnvelope {
        let envelope = SessionEventEnvelope {
            schema_version: crate::INTERFACE_SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            timestamp: chrono::Utc::now().to_rfc3339(),
            event,
        };
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEventEnvelope> {
        self.tx.subscribe()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub cancelled: bool,
    pub error: Option<String>,
}

type ControlHandler = Arc<dyn Fn(&SessionControl) -> Result<()> + Send + Sync>;

pub struct AgentSessionRuntime {
    session_id: String,
    agent: Agent,
    context: Arc<dyn ToolContext>,
    session: tokio::sync::Mutex<Session>,
    emitter: SessionEventEmitter,
    running: Mutex<Option<JoinHandle<()>>>,
    control_handler: RwLock<Option<ControlHandler>>,
}

impl AgentSessionRuntime {
    pub fn new(
        session_id: impl Into<String>,
        agent: Agent,
        context: Arc<dyn ToolContext>,
        session: Session,
    ) -> Arc<Self> {
        let session_id = session_id.into();
        let emitter = SessionEventEmitter::new(session_id.clone(), 512);
        let event_emitter = emitter.clone();
        let agent = agent.with_session_event_callback(move |event| {
            event_emitter.emit(event);
        });
        Arc::new(Self {
            session_id,
            agent,
            context,
            session: tokio::sync::Mutex::new(session),
            emitter,
            running: Mutex::new(None),
            control_handler: RwLock::new(None),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn emitter(&self) -> SessionEventEmitter {
        self.emitter.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEventEnvelope> {
        self.emitter.subscribe()
    }

    pub fn set_control_handler(
        &self,
        handler: impl Fn(&SessionControl) -> Result<()> + Send + Sync + 'static,
    ) {
        *self.control_handler.write().unwrap() = Some(Arc::new(handler));
    }

    pub fn submit_text(self: &Arc<Self>, text: String) -> Result<oneshot::Receiver<TurnOutcome>> {
        self.submit_blocks(vec![ContentBlock::Text { text }])
    }

    pub fn submit_blocks(
        self: &Arc<Self>,
        blocks: Vec<ContentBlock>,
    ) -> Result<oneshot::Receiver<TurnOutcome>> {
        if blocks.is_empty() {
            bail!("prompt cannot be empty")
        }
        let mut running = self.running.lock().unwrap();
        if running.as_ref().is_some_and(|handle| !handle.is_finished()) {
            bail!("session {} is already processing a turn", self.session_id)
        }
        if let Some(finished) = running.take() {
            if !finished.is_finished() {
                *running = Some(finished);
                bail!("session {} is already processing a turn", self.session_id)
            }
        }
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let runtime = Arc::downgrade(self);
        let handle = tokio::spawn(run_turn(runtime, blocks, outcome_tx));
        *running = Some(handle);
        Ok(outcome_rx)
    }

    pub fn interrupt(&self) -> bool {
        let handle = self.running.lock().unwrap().take();
        if let Some(handle) = handle {
            if !handle.is_finished() {
                handle.abort();
                self.emitter.emit(SessionEvent::Error {
                    code: "cancelled".to_string(),
                    message: "session turn cancelled by client".to_string(),
                    retryable: true,
                });
                self.emitter.emit(SessionEvent::TurnFinished);
                return true;
            }
        }
        false
    }

    pub fn is_running(&self) -> bool {
        self.running
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    pub async fn replace_session(&self, session: Session) -> Result<()> {
        if self.is_running() {
            bail!("session {} is processing a turn", self.session_id)
        }
        *self.session.lock().await = session;
        Ok(())
    }

    pub fn control(
        self: &Arc<Self>,
        control: SessionControl,
    ) -> Result<Option<oneshot::Receiver<TurnOutcome>>> {
        match control {
            SessionControl::Submit { text } => self.submit_text(text).map(Some),
            SessionControl::Interrupt | SessionControl::Close => {
                self.interrupt();
                Ok(None)
            }
            other => {
                let handler = self.control_handler.read().unwrap().clone();
                match handler {
                    Some(handler) => handler(&other).map(|_| None),
                    None => bail!("session control is not supported by this frontend: {other:?}"),
                }
            }
        }
    }

    pub async fn session_snapshot(&self) -> Session {
        self.session.lock().await.clone()
    }
}

async fn run_turn(
    runtime: Weak<AgentSessionRuntime>,
    blocks: Vec<ContentBlock>,
    outcome_tx: oneshot::Sender<TurnOutcome>,
) {
    let Some(runtime) = runtime.upgrade() else {
        return;
    };
    let mut session = runtime.session.lock().await;
    session.push_user_with_blocks(blocks);
    let result = runtime
        .agent
        .run_turn(&mut session, runtime.context.as_ref(), &mut |_| {})
        .await;
    let outcome = match result {
        Ok(()) => TurnOutcome {
            cancelled: false,
            error: None,
        },
        Err(error) => {
            runtime.emitter.emit(SessionEvent::Error {
                code: "agent_turn_failed".to_string(),
                message: error.to_string(),
                retryable: error
                    .downcast_ref::<wyj_api::ProviderError>()
                    .is_some_and(|provider_error| provider_error.retryable),
            });
            runtime.emitter.emit(SessionEvent::TurnFinished);
            TurnOutcome {
                cancelled: false,
                error: Some(error.to_string()),
            }
        }
    };
    let _ = outcome_tx.send(outcome);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use std::path::{Path, PathBuf};
    use wyj_api::provider::{EventStream, Provider, RequestOptions};
    use wyj_api::types::{Message, StopReason, StreamEvent, ToolDefinition};

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
                Ok(StreamEvent::TextDelta("hello".to_string())),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                }),
            ])))
        }
    }

    struct TestContext(PathBuf);

    #[async_trait]
    impl ToolContext for TestContext {
        fn cwd(&self) -> &Path {
            &self.0
        }

        fn is_allowed(&self, _name: &str, _input: &serde_json::Value) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn runtime_streams_frontend_neutral_envelopes_and_keeps_session() {
        let runtime = AgentSessionRuntime::new(
            "s1",
            Agent::new(Arc::new(StaticProvider)),
            Arc::new(TestContext(PathBuf::from("/tmp"))),
            Session::new(),
        );
        let mut events = runtime.subscribe();
        let outcome = runtime
            .submit_text("hi".to_string())
            .unwrap()
            .await
            .unwrap();
        assert_eq!(outcome.error, None);
        let first = events.recv().await.unwrap();
        assert_eq!(first.sequence, 1);
        assert!(matches!(first.event, SessionEvent::TextDelta { ref text } if text == "hello"));
        let second = events.recv().await.unwrap();
        assert!(matches!(second.event, SessionEvent::TurnFinished));
        assert_eq!(runtime.session_snapshot().await.messages.len(), 2);
    }
}
