//! wyj-tui — ratatui 终端 UI

pub mod app;
pub mod event;
pub mod input;
pub mod markdown;
pub mod render;
pub mod theme;

pub use app::{run_tui, RebuildFn};
pub use wyj_config::AgentMode;
