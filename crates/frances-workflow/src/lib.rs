//! Slash-command workflows.
//!
//! A workflow is a script-defined hook that takes over a turn. Workflows
//! are declared per-id in the layered config tree as
//! `workflows.<id>.file = "/path/to/foo.ts"` and invoked from the TUI by
//! typing `/<id> [args...]`.
//!
//! This crate owns:
//!
//! - [`WorkflowConfig`] — the config row shape.
//! - [`parse_slash_command`] — the input parser.
//! - [`WorkflowError`] — typed errors raised here.
//! - [`Runtime`] — the script runtime that exposes the `frances:v1/*`
//!   import surface to user scripts.

mod config;
mod deps;
mod error;
mod modules;
pub mod permission;
mod runtime;
mod slash;
mod storage;
mod transpile;

pub use config::WorkflowConfig;
pub use deps::{EditorFactory, ShellFactory, WorkflowDeps};
pub use error::WorkflowError;
pub use permission::{
    PermissionId, PermissionRequest, PermissionResponse, PermissionResponseWire, Permissions,
};
pub use runtime::{
    FrameId, FrameKind, FramePush, HostFrame, InboxItem, Invocation, Runtime, ShellState,
    UserInput, WorkflowHandle,
};
pub use slash::parse_slash_command;
pub use storage::{ExecResult, Row, RowStream, WorkflowDb, WorkflowDbError, WorkflowTx};

#[cfg(any(test, feature = "test-utils"))]
pub use runtime::{test_deps, test_drive};
