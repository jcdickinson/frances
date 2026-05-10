//! Slash-command workflows.
//!
//! A workflow is a script-defined hook that takes over a turn. Workflows
//! are declared per-id in the layered config tree as
//! `workflows.<id>.file = "/path/to/foo.ts"` and invoked from the TUI by
//! typing `/<id> [args...]`.
//!
//! This crate currently owns:
//!
//! - [`WorkflowConfig`] — the config row shape.
//! - [`parse_slash_command`] — the input parser.
//! - [`WorkflowError`] — typed errors raised here.
//!
//! The script runtime ([`Runtime`]) lives alongside but is wired up across
//! follow-up commits; today it's a placeholder so callers can take the
//! type as a dependency without wrapping it in `Option`.

mod config;
mod error;
mod runtime;
mod slash;
mod transpile;

pub use config::WorkflowConfig;
pub use error::WorkflowError;
pub use runtime::{HostFrame, Invocation, Runtime, UserInput, WorkflowHandle};
pub use slash::parse_slash_command;
