//! Slash-command workflows.
//!
//! A workflow is a script-defined hook that drives a chat session and
//! its tool calls. Workflows
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
pub mod io;
mod modules;
pub mod permission;
mod runtime;
mod slash;
mod storage;
mod transpile;

pub use config::WorkflowConfig;
pub use deps::{EditorFactory, WorkflowDeps};
pub use io::{
    FsMetadata, SleepOutcome, WorkflowFs, WorkflowIo, WorkflowShell, WorkflowTimer,
    real::{RealFs, RealIo, RealShell, RealTimer},
};

pub use error::WorkflowError;
#[cfg(any(test, feature = "test-utils"))]
pub use io::mock::{MockFs, MockIo, MockShell, MockTimer, StubIo};
pub use permission::{PermissionRequest, PermissionResponse, PermissionResponseWire};
pub use runtime::{
    InboxItem, Invocation, ReasoningState, Runtime, SectionId, SectionKind, SectionSpec,
    SectionTranscript, ShellState, Source, SurfaceCmd, UserInput, WorkflowHandle, WorkflowOutputs,
};
pub use slash::parse_slash_command;
pub use storage::{ExecResult, Row, RowStream, WorkflowDb, WorkflowDbError, WorkflowTx};

#[cfg(any(test, feature = "test-utils"))]
pub use runtime::{test_deps, test_drive};
