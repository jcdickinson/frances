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
use frances_storage::Migration;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::io::WorkflowIo;
use crate::storage::{WorkflowDb, WorkflowDbError};

pub trait WorkflowDeps: Clone + Send + Sync + 'static {
    type ChatSessionManager: ChatSessionManager;
    /// Timer + shell + fs bundle. See [`crate::io`] for the sub-trait
    /// layout — each sub-piece is independently swappable, the umbrella
    /// is just for convenient routing.
    type Io: WorkflowIo;
    type EditorFactory: EditorFactory;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager;

    /// Timer + shell + fs surface. Production wires
    /// [`crate::io::real::RealIo`]; tests drag in
    /// [`crate::io::mock::MockIo`] from `test-utils`.
    fn io(&self) -> &Self::Io;

    /// Factory for the `frances:v1/tools/file` `Editor` primitive. Hands
    /// out a clone of the host's session-scoped `EditSession` so all
    /// workflow invocations within the same runtime see the same
    /// anchor cache.
    fn editor_factory(&self) -> &Self::EditorFactory;

    /// Snapshot of the latest invocation's environment. Used by
    /// `ChatSession.stream()` so the provider can resolve auth env vars
    /// (e.g. `OPENROUTER_API_KEY`) against the invoking process, not the
    /// session-runtime process. Returns an empty map before the
    /// invocation context is set.
    fn current_env(&self) -> HashMap<OsString, OsString>;

    /// Snapshot of the latest invocation's working directory. `Editor`
    /// resolves relative paths against this on every call, so a new
    /// invocation context with a different cwd takes effect immediately.
    /// `None` before the invocation context is set.
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

/// Hands out the host's session-scoped `EditSession`. The runtime's impl
/// returns clones of an `Arc` to the singleton stored on `ServerState`;
/// tests construct fresh sessions with a `FakeStore`.
pub trait EditorFactory: Clone + Send + Sync + 'static {
    type Store: AnchorStore + Send + Sync + 'static;
    fn session(&self) -> Arc<AsyncMutex<EditSession<Self::Store>>>;
}
