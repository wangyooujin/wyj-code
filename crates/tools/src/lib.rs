//! wyj-tools — 工具注册表与所有工具实现

pub mod agent_hub;
#[cfg(target_os = "macos")]
pub mod app_computer;
pub mod ask_question;
mod bash;
mod bash_output;
pub mod bash_session;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod computer;
pub mod context_recall;
pub mod ctx;
pub mod descriptions;
mod diff;
mod edit;
pub mod exit_plan_mode;
mod glob;
mod grep;
pub mod memory;
mod read;
pub mod registry;
pub mod sub_agent;
pub mod textutil;
pub mod todo;
pub mod trace;
mod webfetch;
mod websearch;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod window_capture;
mod write;

pub use agent_hub::{
    AgentControl, AgentControlResult, SubAgentEvent, SubAgentHub, MAX_CONCURRENT_SUBAGENTS,
};
#[cfg(target_os = "macos")]
pub use app_computer::AppComputerTool;
pub use ask_question::AskQuestionTool;
pub use bash_session::BashSessionManager;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use computer::ComputerTool;
pub use ctx::{PermissionDecision, PermissionMode, ToolCtx, UiAskRequest};
// TUI 的 diff 渲染层直接复用真实生成结果（而非手写样本），这样改了
// `make_diff` 的输出形状会立刻反映到渲染快照上，而不是被一份过期的样本盖住。
pub use diff::{make_added, make_diff};
pub use exit_plan_mode::ExitPlanModeTool;
pub use grep::backend_label as grep_backend_label;
pub use memory::MemoryTool;
pub use registry::ToolRegistry;
pub use sub_agent::{AgentFactory, SharedAgentDefinitions, SubAgentTool};
pub use todo::{TodoStore, TodoWriteTool};
pub use trace::{TraceEvent, TraceWriter};
pub use websearch::WebSearchTool;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use window_capture::WindowCaptureTool;
pub use wyj_core::tool::{Tool, ToolResult};

/// 当前已连接成功的 MCP 桥接工具快照，供子 Agent 工厂 / `/model` 重建等
/// 多个消费点共享读取；三种运行模式（`-p`/`--headless`/TUI）各自在 MCP
/// 连接成功时把工具 push 进来。
pub type SharedMcpTools = std::sync::Arc<std::sync::RwLock<Vec<std::sync::Arc<dyn Tool>>>>;
