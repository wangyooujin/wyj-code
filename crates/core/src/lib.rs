pub mod agent;
pub mod agent_def;
pub mod claude_md;
pub mod compact;
pub mod history;
pub mod hooks;
pub mod memory;
pub mod project;
pub mod prompts;
pub mod session;
pub mod session_store;
pub mod summary;
pub mod tool;

pub use agent::{Agent, InjectionKind, ToolEvent};
pub use agent_def::{builtin_defs, load_agent_defs, AgentDefinition};
pub use claude_md::{discover_files, ClaudeMdLoader, ClaudeMdSource, DiscoveredFile};
pub use compact::{compact_session, estimate_tokens};
pub use history::{new_session_id, now_iso, HistoryEntry, HistoryStore};
pub use hooks::{
    load_effective_hooks, HookCommand, HookMatcherEntry, HookOutcome, HookRunner, HooksSettings,
};
pub use memory::{project_id, MemoryStore};
pub use project::{project_key, project_root, same_project};
pub use session::Session;
pub use session_store::{extract_preview, extract_title, SessionFile, SessionMeta, SessionStore};
pub use summary::SummaryGenerator;
pub use tool::{Tool, ToolResult};
