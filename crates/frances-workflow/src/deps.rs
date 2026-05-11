//! Dep-bundle the host hands to the workflow runtime.
//!
//! Daemon (and tests) impl this on whatever struct they're carrying.
//! `Clone + Send + Sync + 'static` so it can be moved across the
//! tokio-task / async-context boundary cheaply.

use std::collections::HashMap;
use std::ffi::OsString;

use frances_models_llm::chat::ChatSessionManager;

pub trait WorkflowDeps: Clone + Send + Sync + 'static {
    type ChatSessionManager: ChatSessionManager;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager;

    /// Snapshot of the most recently attached client's environment.
    /// Used by `ChatSession.stream()` so the provider can resolve auth
    /// env vars (e.g. `OPENROUTER_API_KEY`) against the client process,
    /// not the daemon process. Returns an empty map if no client has
    /// attached yet.
    fn current_env(&self) -> HashMap<OsString, OsString>;
}
