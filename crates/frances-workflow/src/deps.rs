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

/// The dep bundle the host hands to the workflow runtime.
///
/// `WorkflowDeps` is itself a [`WorkflowIo`] — implementations supply
/// the timer/shell/fs surface directly, no separate `io()` accessor.
pub trait WorkflowDeps: WorkflowIo + Clone {
    type ChatSessionManager: ChatSessionManager;
    type EditorFactory: EditorFactory;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager;

    /// Factory for the `frances:v1/tools/file` `Editor` primitive.
    fn editor_factory(&self) -> &Self::EditorFactory;

    /// Snapshot of the latest invocation's environment. Returns an empty map
    /// before the invocation context is set.
    fn current_env(&self) -> Arc<HashMap<OsString, OsString>>;

    /// Snapshot of the latest invocation's working directory. `None` before
    /// the invocation context is set.
    fn current_cwd(&self) -> Option<PathBuf>;

    /// Resolve a workflow's per-session SQL handle. On first touch the
    /// host applies `migrations` under `entity` (via the
    /// [`frances_storage`] migrator), caches an [`Arc<WorkflowDb>`],
    /// and returns it. Subsequent touches return the cached handle and
    /// ignore `migrations`.
    fn workflow_db<'a>(
        &'a self,
        entity: Uuid,
        migrations: Cow<'a, [Migration]>,
    ) -> impl Future<Output = Result<Arc<WorkflowDb>, WorkflowDbError>> + Send + 'a;
}

/// Hands out fresh per-context edit sessions over the host's shared anchor
/// engine.
pub trait EditorFactory: Clone + Send + Sync + 'static {
    type Store: AnchorStore + Send + Sync + 'static;

    /// Mint a fresh read context — an empty read cache and loop guard over the
    /// shared anchor engine. The workflow calls this per context (each `new
    /// Editor()`), so "have I read this here?" resets when context clears.
    fn new_session(&self) -> EditSession<Self::Store>;
}

/// The per-context edit session an `Editor` — and the `FileSearch` bound to
/// it — operate on. Wrapped in `Arc<AsyncMutex<_>>` for the interior
/// mutability the JS primitives need across concurrent tool calls.
pub type EditorSession<D> =
    Arc<AsyncMutex<EditSession<<<D as WorkflowDeps>::EditorFactory as EditorFactory>::Store>>>;
