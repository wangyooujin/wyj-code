//! wyj-tools — 工具注册表与所有工具实现

pub mod agent_hub;
pub mod ask_question;
mod bash;
mod bash_output;
pub mod bash_session;
pub mod ctx;
pub mod descriptions;
mod diff;
mod edit;
pub mod exit_plan_mode;
mod glob;
mod grep;
mod read;
pub mod registry;
pub mod sub_agent;
pub mod textutil;
pub mod todo;
pub mod trace;
mod webfetch;
mod websearch;
mod write;

pub use agent_hub::{SubAgentEvent, SubAgentHub, MAX_CONCURRENT_SUBAGENTS};
pub use ask_question::AskQuestionTool;
pub use bash_session::BashSessionManager;
pub use ctx::{PermissionDecision, PermissionMode, ToolCtx, UiAskRequest};
pub use exit_plan_mode::ExitPlanModeTool;
pub use registry::ToolRegistry;
pub use sub_agent::{AgentFactory, SubAgentTool};
pub use todo::{TodoStore, TodoWriteTool};
pub use trace::{TraceEvent, TraceWriter};
pub use websearch::WebSearchTool;
pub use wyj_core::tool::{Tool, ToolResult};
