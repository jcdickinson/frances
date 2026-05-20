use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex as StdMutex;
use tokio::sync::Notify;
use tokio::sync::oneshot;

use crate::anchor_store::AnchorStoreImpl;
use crate::context::InvocationContext;
use crate::history::TursoHistoryStore;
use crate::llm::SessionConfigWriter;
use crate::protocol::{PermissionId, PermissionRequest};
use crate::session::Session;
use crate::workflows::{WorkflowConfig, WorkflowStack};
use frances_config::{ConfigBinding, ConfigHandle};
use frances_edit::EditSession;
use frances_llm::{ChatManagerDeps, ChatSessionManager, ProviderCache};
use frances_models_llm::wire::ToolCall;
use frances_storage::{Database, Migration};
use frances_workflow::{
    EditorFactory, PermissionResponse, Permissions, Runtime as WorkflowRuntime, WorkflowDb,
    WorkflowDbError, WorkflowDeps,
};

pub(crate) mod auto_judge;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

mod bootstrap;
mod client_rpc;
mod control;
mod error;
mod events;
mod logging;
mod turn;

pub use bootstrap::run;
pub use error::ServerError;
pub use logging::install_logging;

use events::EventsSocket;

/// Implementation-side deps the concrete `ChatSessionManager` reads from.
/// Cloneable, since `TursoHistoryStore` is a cheap handle.
#[derive(Clone)]
pub struct ServerChatDeps {
    pub history: TursoHistoryStore,
}

impl ChatManagerDeps for ServerChatDeps {
    type HistoryStore = TursoHistoryStore;

    fn history_store(&self) -> &TursoHistoryStore {
        &self.history
    }
}

/// Workflow deps: a daemon-local wrapper around the chat manager so we
/// can satisfy the orphan rule. The manager carries everything workflow
/// currently needs; future workflow-only deps land as additional fields
/// on this struct.
#[derive(Clone)]
pub struct ServerWorkflowDeps {
    pub chat: ChatSessionManager<ServerChatDeps>,
    /// Shared with `ServerState::last_context` so `current_env` /
    /// `current_cwd` reflect the latest client attach — the daemon's own
    /// env/cwd doesn't carry the user's API keys or project location.
    pub last_context: Arc<StdMutex<Option<InvocationContext>>>,
    pub editor_factory: DaemonEditorFactory,
    /// Shared with `ServerState::permissions` so the workflow JS surface
    /// and the `respond_permission` RPC handler talk to the same registry
    /// of pending oneshots.
    pub permissions: DaemonPermissions,
    /// Per-session [`Database`] — same handle as the daemon's other
    /// consumers (history, anchors, session config). Cloned in for the
    /// workflow storage surface; the connection lock is shared across
    /// every caller in the daemon and the workflow runtime.
    pub db: Database,
    /// Per-workflow `WorkflowDb` cache. First touch under an entity
    /// applies its migrations and inserts; subsequent touches hit the
    /// map. Wrapped in `Arc` so clones of `ServerWorkflowDeps` (one
    /// per workflow invocation) see the same cache.
    pub workflow_dbs: Arc<DashMap<Uuid, Arc<WorkflowDb>>>,
}

impl WorkflowDeps for ServerWorkflowDeps {
    type ChatSessionManager = ChatSessionManager<ServerChatDeps>;
    type ShellFactory = DaemonShellFactory;
    type EditorFactory = DaemonEditorFactory;
    type Permissions = DaemonPermissions;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager {
        &self.chat
    }

    fn shell_factory(&self) -> &Self::ShellFactory {
        &DaemonShellFactory
    }

    fn editor_factory(&self) -> &Self::EditorFactory {
        &self.editor_factory
    }

    fn permissions(&self) -> &Self::Permissions {
        &self.permissions
    }

    fn current_env(&self) -> HashMap<std::ffi::OsString, std::ffi::OsString> {
        self.last_context
            .lock()
            .as_ref()
            .map(|ctx| ctx.process.env.clone())
            .unwrap_or_default()
    }

    fn current_cwd(&self) -> Option<PathBuf> {
        self.last_context
            .lock()
            .as_ref()
            .and_then(|ctx| ctx.process.cwd.clone())
    }

    async fn workflow_db(
        &self,
        entity: Uuid,
        migrations: Cow<'_, [Migration]>,
    ) -> Result<Arc<WorkflowDb>, WorkflowDbError> {
        if let Some(existing) = self.workflow_dbs.get(&entity) {
            return Ok(existing.clone());
        }
        let schema = frances_storage::EntitySchema { entity, migrations };
        {
            let conn = self.db.connect().await;
            frances_storage::run(&conn, &schema).await?;
        }
        let db = Arc::new(WorkflowDb::new(self.db.clone(), entity));
        self.workflow_dbs.insert(entity, db.clone());
        Ok(db)
    }
}

/// `Permissions` impl shared between the workflow runtime (which
/// allocates pending slots) and the client RPC handler (which resolves
/// them when the TUI sends a response). Cheap to clone — wraps an
/// `Arc`.
#[derive(Clone, Default)]
pub struct DaemonPermissions {
    inner: Arc<DaemonPermissionsInner>,
}

#[derive(Default)]
struct DaemonPermissionsInner {
    next_id: AtomicU64,
    pending: DashMap<PermissionId, oneshot::Sender<PermissionResponse>>,
}

impl Permissions for DaemonPermissions {
    fn allocate(
        &self,
        prompt: String,
        tool_call: Option<ToolCall>,
    ) -> (PermissionRequest, oneshot::Receiver<PermissionResponse>) {
        let id = PermissionId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.inner.pending.insert(id, tx);
        (
            PermissionRequest {
                id,
                prompt,
                tool_call,
            },
            rx,
        )
    }
}

impl DaemonPermissions {
    /// Settle a pending permission. Returns `Err` if the id is unknown
    /// (already responded or never allocated) or if the awaiter has
    /// gone away.
    pub fn respond(
        &self,
        id: PermissionId,
        response: PermissionResponse,
    ) -> Result<(), PermissionResponseError> {
        let (_, tx) = self
            .inner
            .pending
            .remove(&id)
            .ok_or(PermissionResponseError::UnknownId)?;
        tx.send(response)
            .map_err(|_| PermissionResponseError::Dropped)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionResponseError {
    #[error("no pending permission with that id")]
    UnknownId,
    #[error("workflow stopped waiting for this permission")]
    Dropped,
}

/// Stateless factory that spawns a fresh bash subprocess for each
/// `new Shell()` call from a workflow script. The daemon doesn't carry
/// per-workflow shell policy yet, so we use `ShellOptions::default()`
/// (inherits the daemon's cwd / env).
#[derive(Clone, Copy)]
pub struct DaemonShellFactory;

impl frances_workflow::ShellFactory for DaemonShellFactory {
    async fn spawn(
        &self,
        opts: frances_shell::ShellOptions,
    ) -> Result<frances_shell::Shell, frances_shell::ShellError> {
        frances_shell::Shell::spawn(opts).await
    }
}

/// Hands out the daemon's session-scoped `EditSession` — same `Arc`
/// every call, so all workflow invocations within the daemon session
/// share the anchor cache.
#[derive(Clone)]
pub struct DaemonEditorFactory {
    pub session: Arc<AsyncMutex<EditSession<AnchorStoreImpl>>>,
}

impl EditorFactory for DaemonEditorFactory {
    type Store = AnchorStoreImpl;

    fn session(&self) -> Arc<AsyncMutex<EditSession<AnchorStoreImpl>>> {
        self.session.clone()
    }
}

pub(crate) struct ServerState {
    pub session: Session,
    // TODO: This smells like a refactor needed
    pub client_attached: StdMutex<bool>,
    pub last_context: Arc<StdMutex<Option<InvocationContext>>>,
    pub daemon_pid: u32,
    /// Canonical handle to the per-daemon-session editor. The
    /// workflow runtime holds a clone of this same factory so JS
    /// `new Editor()` calls share the anchor cache with the host —
    /// e.g. host-side `end_turn` in `stream_prompt`.
    pub editor_factory: DaemonEditorFactory,
    pub events: EventsSocket,
    pub shutdown: Notify,
    /// Kept alive so the config-event-processor task stays running for the
    /// daemon's lifetime. The chat manager and provider cache hold their
    /// own bindings, but parking the handle here makes the lifetime
    /// guarantee explicit.
    pub _config: ConfigHandle,
    #[expect(
        dead_code,
        reason = "kept for future direct access; chat manager holds its own clone"
    )]
    pub history: TursoHistoryStore,
    #[expect(
        dead_code,
        reason = "kept for future direct access; chat manager holds its own clone"
    )]
    pub cache: ProviderCache,
    pub workflows: ConfigBinding<HashMap<String, WorkflowConfig>>,
    /// `default_workflow` config binding. `restore_or_seed` reads this
    /// to choose what to push when the `workflow_stack` table is empty.
    pub default_workflow: ConfigBinding<Option<String>>,
    pub workflow_runtime: Arc<WorkflowRuntime<ServerWorkflowDeps>>,
    pub workflow_stack: WorkflowStack,
    /// Registry of pending user-permission round-trips. Cloned into
    /// `ServerWorkflowDeps` so the workflow JS surface and the RPC
    /// handler both see the same pending slots.
    pub permissions: DaemonPermissions,
    /// Same `ChatSessionManager` the workflow runtime uses (it's a
    /// cheap `Arc`-backed clone). The daemon-level auto-judge calls
    /// `chat.complete` directly to score permission requests that
    /// opted into auto.
    pub chat: ChatSessionManager<ServerChatDeps>,
    /// Writes session-config rows and emits the matching events on the
    /// DB layer in one call. Held for future RPC handlers that mutate
    /// session config.
    #[expect(dead_code, reason = "wired for future session-config writers")]
    pub session_config_writer: SessionConfigWriter,
}
