//! wyj-tui — ratatui 终端 UI

pub mod app;
pub mod event;
mod hyperlink;
pub mod input;
pub mod markdown;
pub mod render;
pub mod theme;
pub mod welcome;

pub use app::{run_tui, RebuildFn};
pub use theme::{apply_theme_json, ThemePalette};
pub use wyj_config::AgentMode;
