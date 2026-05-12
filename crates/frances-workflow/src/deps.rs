//! Dep-bundle the host hands to the workflow runtime.
//!
//! Daemon (and tests) impl this on whatever struct they're carrying.
//! `Clone + Send + Sync + 'static` so it can be moved across the
//! tokio-task / async-context boundary cheaply.

use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;

use frances_models_llm::chat::ChatSessionManager;
use frances_shell::{Shell, ShellError, ShellOptions};

pub trait WorkflowDeps: Clone + Send + Sync + 'static {
    type ChatSessionManager: ChatSessionManager;
    type ShellFactory: ShellFactory;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager;

    /// Factory for the `frances:v1/tools/shell` `Shell` primitive. The
    /// daemon impl wraps `frances_shell::Shell::spawn` with whatever cwd
    /// / env / init-script policy it cares about; tests stub it out.
    fn shell_factory(&self) -> &Self::ShellFactory;

    /// Snapshot of the most recently attached client's environment.
    /// Used by `ChatSession.stream()` so the provider can resolve auth
    /// env vars (e.g. `OPENROUTER_API_KEY`) against the client process,
    /// not the daemon process. Returns an empty map if no client has
    /// attached yet.
    fn current_env(&self) -> HashMap<OsString, OsString>;
}

/// Spawns bash subprocesses for workflows. Async because
/// `frances_shell::Shell::spawn` is async.
pub trait ShellFactory: Clone + Send + Sync + 'static {
    fn spawn(&self, opts: ShellOptions) -> impl Future<Output = Result<Shell, ShellError>> + Send;
}
