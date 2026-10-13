//! Command parsing utilities shared across Codex crates.

pub mod shell_detect;
pub mod shell_snapshot;
mod startup;

pub use startup::shell_startup_script;

pub mod bash;
pub mod parse_command;
pub mod powershell;
