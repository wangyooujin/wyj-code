//! wyj-commands — Slash 命令系统

pub mod builtin;
pub mod registry;
pub mod skill;

pub use builtin::standard_registry_with_skills;
pub use registry::{Command, CommandContext, CommandRegistry, CommandResult};
