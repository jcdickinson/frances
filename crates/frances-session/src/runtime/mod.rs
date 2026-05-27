//! In-process session runtime.
//!
//! Owns everything that used to live behind the daemon's
//! socket boundary: per-session DB handles, the workflow runtime, the
//! chat manager, scrollback / history stores, permission registry, and
//! the events channel into the TUI.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex as StdMutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::anchor_store::AnchorStoreImpl;
use crate::context::InvocationContext;
use crate::events::{PermissionId, PermissionRequest, PermissionResponseWire, StreamFrame};
use crate::history::TursoHistoryStore;
use crate::llm::{SessionConfigProvider, SessionConfigWriter};
use crate::session::Session;
use crate::workflows::{DriverCmd, WorkflowConfig, WorkflowStack};
use frances_config::{ConfigBinding, ConfigHandle, ConfigProvider, EnvProvider, TomlProvider};
use frances_edit::{EditEngine, EditSession};
use frances_llm::{ChatManagerDeps, ChatSessionManager, ProviderCache};
use frances_models_llm::config::ModelConfig;
use frances_models_llm::wire::ToolCall;
use frances_storage::{Database, Migration};
use frances_workflow::{
    EditorFactory, PermissionResponse, Permissions, Runtime as WorkflowRuntime, WorkflowDb,
    WorkflowDbError, WorkflowDeps,
};

pub(crate) mod auto_judge;
mod error;
mod events;
mod logging;
mod replay;

pub use error::RuntimeError;
pub use events::EventsChannel;
pub use logging::install_logging;

/// Concrete `ChatManagerDeps` impl reading from the per-session
/// `TursoHistoryStore`. Cheap to clone.
#[derive(Clone)]
pub struct ChatDepsImpl {
    pub history: TursoHistoryStore,
}

impl ChatManagerDeps for ChatDepsImpl {
    type HistoryStore = TursoHistoryStore;

    fn history_store(&self) -> &TursoHistoryStore {
        &self.history
    }
}

/// Concrete `WorkflowDeps` impl. Holds the chat manager and the
/// per-session DB handle plus the same permission/editor factories the
/// TUI sees, so `current_env` / `current_cwd` / permission round-trips
/// share state with the host.
#[derive(Clone)]
pub struct WorkflowDepsImpl {
    pub chat: ChatSessionManager<ChatDepsImpl>,
    pub invocation: Arc<StdMutex<InvocationContext>>,
    pub editor_factory: SessionEditorFactory,
    pub permissions: SessionPermissions,
    pub db: Database,
    /// Per-workflow `WorkflowDb` cache. First touch under an entity
    /// applies its migrations and inserts; subsequent touches hit the
    /// map. Wrapped in `Arc` so clones (one per workflow invocation)
    /// see the same cache.
    pub workflow_dbs: Arc<DashMap<Uuid, Arc<WorkflowDb>>>,
}

impl WorkflowDeps for WorkflowDepsImpl {
    type ChatSessionManager = ChatSessionManager<ChatDepsImpl>;
    type ShellFactory = SessionShellFactory;
    type EditorFactory = SessionEditorFactory;
    type Permissions = SessionPermissions;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager {
        &self.chat
    }

    fn shell_factory(&self) -> &Self::ShellFactory {
        &SessionShellFactory
    }

    fn editor_factory(&self) -> &Self::EditorFactory {
        &self.editor_factory
    }

    fn permissions(&self) -> &Self::Permissions {
        &self.permissions
    }

    fn current_env(&self) -> HashMap<std::ffi::OsString, std::ffi::OsString> {
        self.invocation.lock().process.env.clone()
    }

    fn current_cwd(&self) -> Option<PathBuf> {
        self.invocation.lock().process.cwd.clone()
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
/// allocates pending slots) and the TUI (which resolves them via
/// [`SessionRuntime::respond_permission`]). Cheap to clone — wraps an
/// `Arc`.
#[derive(Clone, Default)]
pub struct SessionPermissions {
    inner: Arc<SessionPermissionsInner>,
}

#[derive(Default)]
struct SessionPermissionsInner {
    next_id: AtomicU64,
    pending: DashMap<PermissionId, oneshot::Sender<PermissionResponse>>,
}

impl Permissions for SessionPermissions {
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

impl SessionPermissions {
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
/// `new Shell()` call from a workflow script. We use
/// `ShellOptions::default()`, which inherits the runtime's cwd / env.
#[derive(Clone, Copy)]
pub struct SessionShellFactory;

impl frances_workflow::ShellFactory for SessionShellFactory {
    async fn spawn(
        &self,
        opts: frances_shell::ShellOptions,
    ) -> Result<frances_shell::Shell, frances_shell::ShellError> {
        frances_shell::Shell::spawn(opts).await
    }
}

/// Hands out the session-scoped `EditSession` — same `Arc` every call,
/// so all workflow invocations share the anchor cache with the host.
#[derive(Clone)]
pub struct SessionEditorFactory {
    pub session: Arc<AsyncMutex<EditSession<AnchorStoreImpl>>>,
}

impl EditorFactory for SessionEditorFactory {
    type Store = AnchorStoreImpl;

    fn session(&self) -> Arc<AsyncMutex<EditSession<AnchorStoreImpl>>> {
        self.session.clone()
    }
}

/// The session runtime. Holds the per-session state previously owned
/// by the daemon process; produces frames into [`EventsChannel`] and
/// accepts prompt / permission input from the TUI.
pub struct SessionRuntime {
    pub session: Session,
    pub invocation: Arc<StdMutex<InvocationContext>>,
    pub editor_factory: SessionEditorFactory,
    pub events: EventsChannel,
    /// Kept alive so the config-event-processor task stays running for the
    /// runtime's lifetime. The chat manager and provider cache hold their
    /// own bindings, but parking the handle here makes the lifetime
    /// guarantee explicit.
    pub _config: ConfigHandle,
    pub history: TursoHistoryStore,
    pub cache: ProviderCache,
    pub workflows: ConfigBinding<HashMap<String, WorkflowConfig>>,
    /// `default_workflow` config binding. `restore_or_seed` reads this
    /// to choose what to push when the `workflow_stack` table is empty.
    pub default_workflow: ConfigBinding<Option<String>>,
    pub workflow_runtime: Arc<WorkflowRuntime<WorkflowDepsImpl>>,
    pub workflow_stack: WorkflowStack,
    /// Control channel into the long-lived workflow driver task. Slash
    /// pushes go here; plain input/interrupts bypass it and land on the
    /// active inbox via [`WorkflowStack`]'s live sender.
    pub(crate) workflow_cmd: mpsc::UnboundedSender<DriverCmd>,
    /// Registry of pending user-permission round-trips. Cloned into
    /// [`WorkflowDepsImpl`] so the workflow JS surface and
    /// [`SessionRuntime::respond_permission`] see the same pending slots.
    pub permissions: SessionPermissions,
    /// Same `ChatSessionManager` the workflow runtime uses (it's a
    /// cheap `Arc`-backed clone). The auto-judge calls `chat.complete`
    /// directly to score permission requests that opted into auto.
    pub chat: ChatSessionManager<ChatDepsImpl>,
    /// Writes session-config rows and emits the matching events on the
    /// DB layer in one call. Held for future config writers.
    pub session_config_writer: SessionConfigWriter,
    /// Cancelled by [`SessionRuntime::shutdown`]. Visible to any
    /// in-flight `prompt` task that wants to bail early.
    pub cancel: CancellationToken,
}

impl SessionRuntime {
    /// Build the runtime, restore the persisted workflow stack, and
    /// return it alongside the events receiver the TUI should drain.
    /// Initial scrollback replay is not done here — call
    /// [`SessionRuntime::replay_initial_scrollback`] after the receiver
    /// is hooked up.
    pub async fn start(
        session: Session,
        db: Database,
        invocation: InvocationContext,
    ) -> crate::Result<(Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<StreamFrame>)> {
        std::fs::create_dir_all(&session.runtime_dir).map_err(|source| {
            RuntimeError::CreateRuntimeDir {
                path: session.runtime_dir.clone(),
                source,
            }
        })?;

        let edit_engine = EditEngine::new(AnchorStoreImpl::new(db.clone()));

        let session_provider = Arc::new(SessionConfigProvider::new(db.clone()));
        let config_providers = build_config_providers(session_provider.clone());
        let config = ConfigHandle::build(config_providers).await?;
        let session_config_writer = session_provider
            .writer()
            .expect("SessionConfigProvider::load ran during ConfigHandle::build");
        let default_model = config
            .bind::<ModelConfig>(["models", "default"])?
            .required()
            .map_err(|_| RuntimeError::DefaultModelMissing)?;
        let cache = ProviderCache::new(config.clone())?;
        let workflows = config.bind::<HashMap<String, WorkflowConfig>>("workflows")?;
        let default_workflow = config.bind::<Option<String>>("default_workflow")?;

        let history = TursoHistoryStore::new(db.clone());
        let chat_deps = ChatDepsImpl {
            history: history.clone(),
        };
        let chat =
            ChatSessionManager::new(chat_deps, config.clone(), default_model, cache.clone())?;

        let invocation = Arc::new(StdMutex::new(invocation));
        let editor_factory = SessionEditorFactory {
            session: Arc::new(AsyncMutex::new(EditSession::new(edit_engine))),
        };
        let permissions = SessionPermissions::default();
        let workflow_runtime = Arc::new(WorkflowRuntime::new(WorkflowDepsImpl {
            chat: chat.clone(),
            invocation: invocation.clone(),
            editor_factory: editor_factory.clone(),
            permissions: permissions.clone(),
            db: db.clone(),
            workflow_dbs: Arc::new(DashMap::new()),
        })?);

        let (events, events_rx) = EventsChannel::new();
        let (workflow_cmd, cmd_rx) = mpsc::unbounded_channel();

        let runtime = Arc::new(Self {
            session: session.clone(),
            invocation,
            editor_factory,
            events,
            _config: config,
            history,
            cache,
            workflows,
            default_workflow,
            workflow_runtime,
            workflow_stack: WorkflowStack::new(db),
            workflow_cmd,
            permissions,
            chat,
            session_config_writer,
            cancel: CancellationToken::new(),
        });

        // Restore the persisted workflow stack — or, if the table is
        // literally empty, seat the configured `default_workflow`. The
        // returned instance becomes the driver's initial active workflow;
        // anything it emits during top-level evaluation buffers in the
        // handle's frame channel and flushes once the driver pumps.
        let initial = match crate::workflows::restore_or_seed(&runtime).await {
            Ok(initial) => initial,
            Err(error) => {
                warn!(%error, "workflow stack restore failed");
                None
            }
        };
        // Publish the active wires synchronously (before the driver task
        // is scheduled) so `replay_initial_scrollback` / attach see the
        // active instance immediately.
        runtime.workflow_stack.seat_initial(initial.as_ref());

        tokio::spawn(crate::workflows::run_driver(
            runtime.clone(),
            cmd_rx,
            initial,
        ));

        Ok((runtime, events_rx))
    }

    /// Replace the latest invocation context. Workflows that read
    /// `current_env` / `current_cwd` see the new value on next access.
    pub fn update_invocation(&self, ctx: InvocationContext) {
        *self.invocation.lock() = ctx;
    }

    /// Run the initial scrollback replay for the currently-active
    /// workflow instance. Sends a `ScrollbackReset` / replay /
    /// `ScrollbackReplayEnd` burst into the events channel.
    pub async fn replay_initial_scrollback(self: &Arc<Self>) {
        let active_instance = self.workflow_stack.active_instance().await;
        if let Err(error) =
            replay::write_initial_replay(&self.events, self.workflow_stack.db(), active_instance)
                .await
        {
            warn!(%error, "initial scrollback replay failed");
        }
    }

    /// Deliver user input. Slash commands push a workflow (via the
    /// driver's control channel); anything else lands on the active
    /// workflow's inbox as plain input. Non-blocking — input is just
    /// IO, decoupled from any cycle. Frames flow through `self.events`.
    pub fn prompt(self: &Arc<Self>, text: String) {
        crate::workflows::dispatch_input(self, &text);
    }

    /// Deliver an interrupt to the active workflow's inbox (Esc in the
    /// TUI). The workflow decides how to react.
    pub fn interrupt(self: &Arc<Self>) {
        crate::workflows::dispatch_interrupt(self);
    }

    /// Resolve a pending permission request previously emitted as
    /// [`StreamFrame::Permission`]. If the user picked
    /// `RedirectToChat`, the workflow sees a denial and a fresh prompt
    /// is dispatched with the user's text.
    pub fn respond_permission(
        self: &Arc<Self>,
        id: PermissionId,
        response: PermissionResponseWire,
    ) -> Result<(), PermissionResponseError> {
        let (workflow_response, redirect) = match response {
            PermissionResponseWire::Yes { details } => (PermissionResponse::Yes { details }, None),
            PermissionResponseWire::No { details } => (PermissionResponse::No { details }, None),
            PermissionResponseWire::RedirectToChat { content } => {
                (PermissionResponse::No { details: None }, Some(content))
            }
        };

        self.permissions.respond(id, workflow_response)?;

        if let Some(content) = redirect {
            self.prompt(content);
        }
        Ok(())
    }

    /// Signal cancellation to any in-flight prompt task. Callers
    /// typically drop the `Arc<SessionRuntime>` shortly after.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

/// Builds the layered config provider stack. Order is low → high
/// priority — `ConfigHandle::build` applies them in sequence, so later
/// providers override earlier ones.
///
///   1. XDG system config dirs (`XDG_CONFIG_DIRS`, default `/etc/xdg`).
///      Spec orders these most-preferred first; we push in reverse so
///      the most-preferred ends up last among the system layers.
///   2. XDG user config dir (`XDG_CONFIG_HOME`, default `~/.config`).
///   3. `FRANCES__*` env vars.
///   4. Per-session DB rows.
///
/// Each TOML file is `.optional()` — running with no config files
/// present is a supported configuration.
fn build_config_providers(
    session_provider: Arc<SessionConfigProvider>,
) -> Vec<Arc<dyn ConfigProvider>> {
    let xdg_dirs = xdg::BaseDirectories::with_prefix("frances");

    let mut providers: Vec<Arc<dyn ConfigProvider>> = Vec::new();

    for dir in xdg_dirs.get_config_dirs().iter().rev() {
        let path = dir.join("config.toml");
        providers.push(Arc::new(TomlProvider::new(path).optional()));
    }

    if let Some(home) = xdg_dirs.get_config_home() {
        providers.push(Arc::new(
            TomlProvider::new(home.join("config.toml")).optional(),
        ));
    }

    providers.push(Arc::new(EnvProvider::with_prefix("FRANCES")));
    providers.push(session_provider);

    providers
}
