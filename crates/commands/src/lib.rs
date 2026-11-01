//! wyj-commands — Slash 命令系统

pub mod builtin;
pub mod cost_view;
pub mod registry;
pub mod skill;

pub use builtin::{render_rules_optimize, standard_registry_with_skills};
pub use registry::{Command, CommandContext, CommandRegistry, CommandResult};
