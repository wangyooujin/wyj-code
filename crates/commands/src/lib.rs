//! wyj-commands — Slash 命令系统

pub mod registry;
pub mod builtin;

pub use registry::{Command, CommandContext, CommandRegistry, CommandResult};
