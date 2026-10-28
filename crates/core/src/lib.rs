pub mod acceptance;
pub mod agent;
pub mod agent_def;
pub mod auto_mode;
pub mod auto_rules;
pub mod aux_usage;
pub mod budget;
pub mod cache_attribution;
pub mod checkpoint;
pub mod claude_md;
pub mod code_index;
pub mod compact;
pub mod context_edit;
pub mod disk_usage;
pub mod evolution;
pub mod frontmatter;
pub mod history;
pub mod hooks;
pub mod hooks_trust;
pub mod instance;
// 内部模块：公共 API 只走下面的扁平 re-export。全仓无人用
// `wyj_core::interfaces::` 这条路径，两条并行的公共路径只会让「改类型要改
// 几处」变得不确定。
mod interfaces;
pub mod lexical;
pub mod memory;
pub mod memory_mirror;
pub mod memory_v3;
pub mod notify;
pub mod permission;
pub mod pricing;
pub mod project;
pub mod prompt_fingerprint;
pub mod prompts;
pub mod retention;
pub mod secret;
pub mod serialize;
pub mod session;
pub mod session_journal;
pub mod session_memory;
pub mod session_runtime;
pub mod session_store;
pub mod summary;
pub mod textutil;
pub mod tool;
pub mod tool_arguments;
pub mod tool_search;
pub mod verify;
pub mod workflow;
pub mod workspace;
pub mod workspace_cas;

pub use agent::{Agent, AgentRoute, InjectionKind, SteerQueue, ToolEvent};
pub use agent_def::{builtin_defs, load_agent_defs, AgentDefinition};
pub use auto_mode::{
    AutoClassifierUsage, AutoCounters, AutoGate, AutoGateConfig, AutoGateImpl, AutoReviewRequest,
    AutoRuleOverrides, AutoVerdict, ResolvedRules,
};
pub use checkpoint::{
    set_checkpoint_config, Checkpoint, CheckpointConfig, CheckpointKind, CheckpointStore,
    CheckpointSummary, RewindPreview, RewindScope, WorkspaceSnapshot,
};
pub use claude_md::{
    audit_rules_files, discover_files, ClaudeMdLoader, ClaudeMdSource, DiscoveredFile,
    RulesFileAudit, RULES_FILE_LIMIT_BYTES, RULES_SIZE_WARN_BYTES,
};
pub use code_index::{CodeSearchTool, ProjectCodeIndex};
pub use compact::{
    compact_session, compact_trigger_buffer, estimate_request_tokens, estimate_text_tokens_f64,
    estimate_tokens,
};
pub use evolution::{
    CandidateKind, CandidatePayload, CandidateStatus, Episode, EpisodeCapture, EpisodeEvidence,
    EpisodeOutcome, EvidenceKind, EvolutionCandidate, EvolutionFeedback, EvolutionHealth,
    EvolutionMemory, EvolutionStatus, EvolutionStore, MemoryKind, MemoryScope, MemoryStatus,
    MigrationPreview, RepositoryCitation, SkillEvalCase, SkillEvalReport, EVOLUTION_SCHEMA_VERSION,
};
pub use history::{new_session_id, now_iso, HistoryEntry, HistoryStore};
pub use hooks::{
    load_effective_hooks, HookCommand, HookMatcherEntry, HookOutcome, HookRunner, HooksSettings,
};
pub use hooks_trust::ProjectHooksTrust;
pub use instance::{live_peers, InstanceGuard, InstanceRecord};
pub use interfaces::{
    CodeIndex, CodeIndexStatus, CodeMatch, CodeQuery, ExecutionWorkspace, ExecutionWorkspaceKind,
    ExecutionWorkspaceManager, ExecutionWorkspaceRequest, SessionControl, SessionEvent,
    SessionEventEnvelope, WorkflowControl, WorkflowNodeKind, WorkflowNodeSpec, WorkflowNodeState,
    WorkflowPermissionCeiling, WorkflowSpec, WorkspaceAcceptResult, WorkspaceDiff,
    WorkspaceDiffSummary, INTERFACE_SCHEMA_VERSION,
};
pub use memory::{project_id, MemoryStore};
pub use memory_v3::{
    ClearAllReport, MemoryClaimKind, MemoryClaimScope, MemoryClaimStatus, MemoryEvidence,
    MemoryJob, MemoryJobStatus, MemoryRecord, MemorySearchHit, MemorySource, MemorySourceKind,
    MemoryV3Status, MemoryV3Store, MemoryWriteRequest, MovedFile, ResetMarker, TaskStatus,
    TaskStep, MEMORY_V3_SCHEMA_VERSION,
};
pub use permission::{
    check_read_target, check_search_root, is_protected_dir_component, is_protected_dir_tree,
    is_protected_read_target, safe_resolve_write_target, DenyReason, ExecutionSurface,
    PermissionMode, PermissionPolicy, PermissionPrompt, PermissionRequest, PermissionVerdict,
    PROTECTED_DIR_NAMES,
};
pub use project::{project_key, project_root};
pub use secret::{redact_cow, redact_sensitive_text, REDACTED_SECRET};
pub use session::{RoutingEvent, Session, SharedSubUsage, SubUsage, SubUsageTotals};
pub use session_runtime::{AgentSessionRuntime, SessionEventEmitter, TurnOutcome};
pub use session_store::{
    extract_preview, extract_title, prompt_cache_state, SessionFile, SessionFileMeta, SessionMeta,
    SessionSearchHit, SessionStore,
};
pub use summary::SummaryGenerator;
pub use textutil::escape_harness_markers;
pub use tool::{Tool, ToolResult};
pub use tool_arguments::{
    simplified_tool_definition, ToolArgumentError, ToolArgumentErrorKind, ToolArgumentIssue,
    ToolArgumentPipeline, ValidatedToolCall,
};
pub use tool_search::{LazyToolState, ToolCatalogEntry, ToolSearchTool};
pub use workflow::{
    validate_workflow, WorkflowEvent, WorkflowHandle, WorkflowNodeContext, WorkflowNodeExecutor,
    WorkflowNodeOutput, WorkflowNodeRecord, WorkflowRuntime, WorkflowSnapshot,
};
pub use workspace::{GitWorktreeManager, ReconcileReport, WorkspaceHealth};
