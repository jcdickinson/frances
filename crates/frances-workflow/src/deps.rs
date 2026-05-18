//! Dep-bundle the host hands to the workflow runtime.
//!
//! Daemon (and tests) impl this on whatever struct they're carrying.
//! `Clone + Send + Sync + 'static` so it can be moved across the
//! tokio-task / async-context boundary cheaply.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use frances_edit::{AnchorStore, EditSession};
use frances_models_llm::chat::ChatSessionManager;
use frances_shell::{Shell, ShellError, ShellOptions};
use frances_storage::Migration;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::approval::ApprovalGateway;
use crate::storage::{WorkflowDb, WorkflowDbError};

pub trait WorkflowDeps: Clone + Send + Sync + 'static {
    type ChatSessionManager: ChatSessionManager;
    type ShellFactory: ShellFactory;
    type EditorFactory: EditorFactory;
    type ApprovalGateway: ApprovalGateway;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager;

    /// Factory for the `frances:v1/tools/shell` `Shell` primitive. The
    /// daemon impl wraps `frances_shell::Shell::spawn` with whatever cwd
    /// / env / init-script policy it cares about; tests stub it out.
    fn shell_factory(&self) -> &Self::ShellFactory;

    /// Factory for the `frances:v1/tools/file` `Editor` primitive. Hands
    /// out a clone of the host's session-scoped `EditSession` so all
    /// workflow invocations within the same daemon session see the same
    /// anchor cache.
    fn editor_factory(&self) -> &Self::EditorFactory;

    /// Gateway for the `frances:v1/approval` `approve()` function. The
    /// daemon impl bridges to the TUI; tests stub it.
    fn approval_gateway(&self) -> &Self::ApprovalGateway;

    /// Snapshot of the most recently attached client's environment.
    /// Used by `ChatSession.stream()` so the provider can resolve auth
    /// env vars (e.g. `OPENROUTER_API_KEY`) against the client process,
    /// not the daemon process. Returns an empty map if no client has
    /// attached yet.
    fn current_env(&self) -> HashMap<OsString, OsString>;

    /// Snapshot of the most recently attached client's working
    /// directory. `Editor` resolves relative paths against this on every
    /// call so re-attach with a different client cwd takes effect
    /// immediately. `None` when no client has attached yet.
    fn current_cwd(&self) -> Option<PathBuf>;

    /// Resolve a workflow's per-session SQL handle. On first touch the
    /// host applies `migrations` under `entity` (via the
    /// [`frances_storage`] migrator), caches an [`Arc<WorkflowDb>`],
    /// and returns it. Subsequent touches return the cached handle and
    /// ignore `migrations`.
    ///
    /// `migrations` is borrowed-or-owned so the caller can pass a
    /// reference into its own `Vec<Migration>` without cloning, or hand
    /// over ownership if it doesn't need the data again.
    fn workflow_db<'a>(
        &'a self,
        entity: Uuid,
        migrations: Cow<'a, [Migration]>,
    ) -> impl Future<Output = Result<Arc<WorkflowDb>, WorkflowDbError>> + Send + 'a;
}

/// Spawns bash subprocesses for workflows. Async because
/// `frances_shell::Shell::spawn` is async.
pub trait ShellFactory: Clone + Send + Sync + 'static {
    fn spawn(&self, opts: ShellOptions) -> impl Future<Output = Result<Shell, ShellError>> + Send;
}

/// Hands out the host's session-scoped `EditSession`. The daemon's impl
/// returns clones of an `Arc` to the singleton stored on `ServerState`;
/// tests construct fresh sessions with a `FakeStore`.
pub trait EditorFactory: Clone + Send + Sync + 'static {
    type Store: AnchorStore + Send + Sync + 'static;
    fn session(&self) -> Arc<AsyncMutex<EditSession<Self::Store>>>;
}
