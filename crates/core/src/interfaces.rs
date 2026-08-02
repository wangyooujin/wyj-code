//! v1.4.4 冻结的后续扩展接口。
//!
//! 本模块只定义 worktree、workflow、daemon/ACP 与代码索引后续实现共同依赖的
//! 前端无关契约，不代表这些 P2 产品能力已经交付。新增字段应保持 serde 向后
//! 兼容；破坏性变更必须提升 [`INTERFACE_SCHEMA_VERSION`]。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const INTERFACE_SCHEMA_VERSION: u32 = 1;

// ── Execution workspace / worktree ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionWorkspaceKind {
    CurrentCheckout,
    GitWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionWorkspaceRequest {
    pub session_id: String,
    pub repository_root: PathBuf,
    pub base_revision: String,
    pub parent_checkpoint_id: Option<String>,
    pub purpose: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionWorkspace {
    pub id: String,
    pub root: PathBuf,
    pub kind: ExecutionWorkspaceKind,
    pub base_revision: String,
    pub parent_checkpoint_id: Option<String>,
    pub disposable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDiffSummary {
    pub changed_files: usize,
    pub insertions: u64,
    pub deletions: u64,
    pub paths: Vec<PathBuf>,
}

/// P2 worktree manager 的最小稳定边界。实现必须保护当前 checkout 的用户修改，
/// 且 dispose 只能处理由同一 manager 创建并标记为 disposable 的 workspace。
pub trait ExecutionWorkspaceManager: Send + Sync {
    fn create(&self, request: &ExecutionWorkspaceRequest) -> anyhow::Result<ExecutionWorkspace>;
    fn diff_summary(&self, workspace: &ExecutionWorkspace) -> anyhow::Result<WorkspaceDiffSummary>;
    fn dispose(&self, workspace: &ExecutionWorkspace) -> anyhow::Result<()>;
}

// ── Frontend-neutral daemon / ACP event stream ───────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolFinished {
        call_id: String,
        output: String,
        is_error: bool,
        elapsed_ms: u64,
    },
    PermissionRequested {
        request_id: String,
        tool_name: String,
        action_summary: String,
        one_shot_only: bool,
    },
    DiffAvailable {
        checkpoint_id: Option<String>,
        summary: WorkspaceDiffSummary,
    },
    CheckpointChanged {
        checkpoint_id: String,
        label: Option<String>,
    },
    AgentStateChanged {
        agent_id: u64,
        parent_id: Option<u64>,
        state: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        tool_schema_tokens: u64,
        tool_schema_tokens_saved: u64,
    },
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
    TurnFinished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventEnvelope {
    pub schema_version: u32,
    pub session_id: String,
    pub sequence: u64,
    pub timestamp: String,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionControl {
    Submit {
        text: String,
    },
    PermissionDecision {
        request_id: String,
        allow: bool,
    },
    Interrupt,
    Rewind {
        checkpoint_id: String,
        scope: String,
    },
    Branch {
        checkpoint_id: String,
        restore_files: bool,
    },
}

// ── Dynamic workflow / DAG ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Agent,
    Review,
    HumanApproval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPermissionCeiling {
    pub allowed_tools: Vec<String>,
    pub write_roots: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    pub require_sandbox: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeSpec {
    pub id: String,
    pub kind: WorkflowNodeKind,
    pub agent_type: Option<String>,
    pub prompt: String,
    pub depends_on: Vec<String>,
    pub permission_ceiling: WorkflowPermissionCeiling,
    pub max_retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub schema_version: u32,
    pub id: String,
    pub nodes: Vec<WorkflowNodeSpec>,
    pub max_parallel: usize,
    pub token_budget: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowControl {
    Pause,
    Resume,
    RetryNode { node_id: String },
    SkipNode { node_id: String },
    Cancel,
}

// ── Pluggable code index ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeIndexStatus {
    pub backend: String,
    pub ready: bool,
    pub indexed_files: usize,
    pub revision: Option<String>,
    pub fallback_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeQuery {
    pub text: String,
    pub path_prefix: Option<PathBuf>,
    pub language: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMatch {
    pub path: PathBuf,
    pub line: u32,
    pub symbol: Option<String>,
    pub kind: Option<String>,
    pub snippet: String,
    pub score_millis: u32,
}

/// 索引后端必须允许失效和降级；调用方在 `ready=false` 或查询失败时保留
/// rg/Glob fallback，不能让语义索引成为基础代码导航的单点故障。
pub trait CodeIndex: Send + Sync {
    fn status(&self) -> CodeIndexStatus;
    fn search(&self, query: &CodeQuery) -> anyhow::Result<Vec<CodeMatch>>;
    fn invalidate(&self, paths: &[PathBuf]) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_event_envelope_is_frontend_neutral_and_roundtrips() {
        let envelope = SessionEventEnvelope {
            schema_version: INTERFACE_SCHEMA_VERSION,
            session_id: "session-1".to_string(),
            sequence: 7,
            timestamp: "2026-08-02T00:00:00Z".to_string(),
            event: SessionEvent::PermissionRequested {
                request_id: "permission-1".to_string(),
                tool_name: "Bash".to_string(),
                action_summary: "cargo test".to_string(),
                one_shot_only: true,
            },
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["event"]["type"], "permission_requested");
        assert!(value["event"].get("response_tx").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEventEnvelope>(value).unwrap(),
            envelope
        );
    }

    #[test]
    fn workflow_nodes_carry_a_permission_ceiling() {
        let node = WorkflowNodeSpec {
            id: "review".to_string(),
            kind: WorkflowNodeKind::Review,
            agent_type: Some("reviewer".to_string()),
            prompt: "review the diff".to_string(),
            depends_on: vec!["implement".to_string()],
            permission_ceiling: WorkflowPermissionCeiling {
                allowed_tools: vec!["Read".to_string(), "Grep".to_string()],
                write_roots: Vec::new(),
                allowed_domains: Vec::new(),
                require_sandbox: true,
            },
            max_retries: 1,
        };
        assert!(node.permission_ceiling.require_sandbox);
        assert!(node.permission_ceiling.write_roots.is_empty());
    }
}
