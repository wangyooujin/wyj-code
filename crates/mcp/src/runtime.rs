//! Runtime coordinator for MCP connections.
//!
//! MCP configuration is mutable while the application is running: a user can
//! install, disable, remove, or upgrade a server from a slash command or a
//! management panel.  The coordinator keeps the connection set separate from
//! an Agent snapshot and applies changes only when the caller reaches a safe
//! Agent boundary.

use crate::bridge::{connect_mcp_server, McpBridgeTool, MCP_CONNECT_TIMEOUT};
use std::collections::HashMap;
use std::sync::Arc;
use wyj_config::McpServerConfig;
use wyj_core::Tool;

struct ConnectionResult {
    name: String,
    fingerprint: String,
    result: std::result::Result<Vec<McpBridgeTool>, String>,
}

struct PendingConnection {
    fingerprint: String,
    task: tokio::task::JoinHandle<()>,
}

struct ConnectedServer {
    fingerprint: String,
    tools: Vec<Arc<dyn Tool>>,
}

/// Observable result of a runtime reconciliation pass.
pub enum McpRuntimeEvent {
    Connected { name: String, tool_count: usize },
    Failed { name: String, reason: String },
    Removed { name: String },
}

/// Owns the currently connected MCP servers and asynchronously reconciles them
/// with the effective configuration supplied by the host application.
pub struct McpRuntime {
    desired: HashMap<String, String>,
    connected: HashMap<String, ConnectedServer>,
    pending: HashMap<String, PendingConnection>,
    failed: HashMap<String, String>,
    tx: tokio::sync::mpsc::UnboundedSender<ConnectionResult>,
    rx: tokio::sync::mpsc::UnboundedReceiver<ConnectionResult>,
}

impl Default for McpRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRuntime {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            desired: HashMap::new(),
            connected: HashMap::new(),
            pending: HashMap::new(),
            failed: HashMap::new(),
            tx,
            rx,
        }
    }

    /// Reconcile the desired server set and start missing/changed connections.
    ///
    /// This method never waits for a child process or an HTTP handshake.  It
    /// only removes obsolete snapshots immediately and schedules new work.  A
    /// caller should invoke [`Self::drain`] before starting the next Agent turn
    /// and atomically install [`Self::tools`] into its Agent snapshot.
    pub fn reconcile(&mut self, servers: &[McpServerConfig]) -> Vec<McpRuntimeEvent> {
        let desired: HashMap<String, String> = servers
            .iter()
            .map(|server| (server.name.clone(), fingerprint(server)))
            .collect();
        let mut events = Vec::new();

        // A disabled/removed server must disappear from the next snapshot even
        // when its old connection task is still winding down.
        let stale: Vec<String> = self
            .connected
            .iter()
            .filter(|(name, current)| desired.get(*name) != Some(&current.fingerprint))
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            self.connected.remove(&name);
            events.push(McpRuntimeEvent::Removed { name });
        }

        // Abort pending work for a server whose desired definition changed or
        // disappeared.  This prevents an old npx/uvx process from being
        // promoted into the new snapshot after a disable/upgrade operation.
        let stale_pending: Vec<String> = self
            .pending
            .iter()
            .filter(|(name, pending)| desired.get(*name) != Some(&pending.fingerprint))
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale_pending {
            if let Some(pending) = self.pending.remove(&name) {
                pending.task.abort();
            }
        }

        self.failed
            .retain(|name, fingerprint| desired.get(name) == Some(fingerprint));

        self.desired = desired;

        for server in servers {
            let fp = fingerprint(server);
            let already_connected = self
                .connected
                .get(&server.name)
                .is_some_and(|current| current.fingerprint == fp);
            if already_connected
                || self.pending.contains_key(&server.name)
                || self.failed.get(&server.name) == Some(&fp)
            {
                continue;
            }

            let name = server.name.clone();
            let tx = self.tx.clone();
            let config = server.clone();
            let fingerprint = fp.clone();
            let task = tokio::spawn(async move {
                let result = tokio::time::timeout(MCP_CONNECT_TIMEOUT, connect_mcp_server(&config))
                    .await
                    .map_err(|_| format!("连接超时（>{}s）", MCP_CONNECT_TIMEOUT.as_secs()))
                    .and_then(|result| result.map_err(|e| e.to_string()));
                let _ = tx.send(ConnectionResult {
                    name,
                    fingerprint,
                    result,
                });
            });
            self.pending.insert(
                server.name.clone(),
                PendingConnection {
                    fingerprint: fp,
                    task,
                },
            );
        }

        events
    }

    /// Apply all completed connection attempts without waiting.
    pub fn drain(&mut self) -> Vec<McpRuntimeEvent> {
        let mut events = Vec::new();
        while let Ok(result) = self.rx.try_recv() {
            let is_current_pending = self
                .pending
                .get(&result.name)
                .is_some_and(|pending| pending.fingerprint == result.fingerprint);
            if !is_current_pending {
                // The task was superseded by a newer configuration.  Dropping
                // its result also drops its MCP connection and child process.
                continue;
            }
            self.pending.remove(&result.name);

            if self.desired.get(&result.name) != Some(&result.fingerprint) {
                continue;
            }

            match result.result {
                Ok(tools) => {
                    self.failed.remove(&result.name);
                    let tool_count = tools.len();
                    let tools = tools
                        .into_iter()
                        .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
                        .collect();
                    self.connected.insert(
                        result.name.clone(),
                        ConnectedServer {
                            fingerprint: result.fingerprint,
                            tools,
                        },
                    );
                    events.push(McpRuntimeEvent::Connected {
                        name: result.name,
                        tool_count,
                    });
                }
                Err(reason) => {
                    self.failed.insert(result.name.clone(), result.fingerprint);
                    events.push(McpRuntimeEvent::Failed {
                        name: result.name,
                        reason,
                    });
                }
            }
        }
        events
    }

    /// Flatten the currently connected tools in stable server/name order.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut servers: Vec<_> = self.connected.iter().collect();
        servers.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut tools = Vec::new();
        for (_, server) in servers {
            let mut server_tools = server.tools.clone();
            server_tools.sort_by(|a, b| a.name().cmp(b.name()));
            tools.extend(server_tools);
        }
        tools
    }

    pub fn connected_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.connected.keys().cloned().collect();
        names.sort();
        names
    }
}

fn fingerprint(server: &McpServerConfig) -> String {
    serde_json::to_string(server).unwrap_or_else(|_| {
        format!(
            "{}:{:?}:{:?}:{:?}:{:?}",
            server.name, server.transport, server.command, server.args, server.url
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wyj_config::McpTransport;

    fn server(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(command.to_string()),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
        }
    }

    #[tokio::test]
    async fn reconcile_removes_obsolete_connections_from_snapshot() {
        let mut runtime = McpRuntime::new();
        runtime.connected.insert(
            "old".to_string(),
            ConnectedServer {
                fingerprint: fingerprint(&server("old", "old")),
                tools: Vec::new(),
            },
        );
        let events = runtime.reconcile(&[]);
        assert!(matches!(events.as_slice(), [McpRuntimeEvent::Removed { name }] if name == "old"));
        assert!(runtime.tools().is_empty());
        assert!(runtime.connected_names().is_empty());
    }

    #[tokio::test]
    async fn changed_pending_connection_is_superseded() {
        let mut runtime = McpRuntime::new();
        runtime.reconcile(&[server("missing", "definitely-not-a-server")]);
        runtime.reconcile(&[server("missing", "another-definitely-not-a-server")]);
        assert_eq!(runtime.pending.len(), 1);
        assert_eq!(
            runtime.pending["missing"].fingerprint,
            fingerprint(&server("missing", "another-definitely-not-a-server"))
        );
    }
}
