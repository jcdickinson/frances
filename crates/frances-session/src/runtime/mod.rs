//! In-process session runtime.
//!
//! Owns the per-session state: per-session DB handles, the workflow runtime, the
//! chat manager, scrollback / history stores, and the events channel
//! into the UI.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::anchor_store::AnchorStoreImpl;
use crate::context::InvocationContext;
use crate::entities::{EntityHub, SessionSnapshot, WorkspaceSnapshot};
use crate::events::{PermissionResponseWire, StreamFrame};
use crate::history::TursoHistoryStore;
use crate::llm::{SessionConfigProvider, SessionConfigWriter};
use crate::session::Session;
use crate::workflows::{ActiveWorkflow, DriverCmd, WorkflowConfig};
use frances_config::{
    ConfigBinding, ConfigEvent, ConfigHandle, ConfigProvider, EnvProvider, EventSender, Path,
    ProviderError, TomlProvider, Value as ConfigValue,
};
use frances_edit::{EditEngine, EditSession};
use frances_llm::{ChatManagerDeps, ChatSessionManager, ProviderCache};
use frances_models_llm::config::ModelConfig;
use frances_storage::{Database, Migration};
use frances_workflow::{
    EditorFactory, PermissionResponse, RealIo, Runtime as WorkflowRuntime, WorkflowDb,
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
/// per-session DB handle plus the same editor factory the UI sees, so
/// `current_env` / `current_cwd` share state with the host.
///
/// Generic on the `Io` bundle (`WorkflowIo`); production defaults to
/// [`RealIo`] (real tokio timer + real bash shell + real `tokio::fs`).
/// Tests can plug in `frances_workflow::StubIo<MockTimer, ...>` via
/// [`SessionRuntime::start_with_io`] to control the clock without
/// affecting production callers.
#[derive(Clone)]
pub struct WorkflowDepsImpl<Io: frances_workflow::WorkflowIo = RealIo> {
    pub chat: ChatSessionManager<ChatDepsImpl>,
    pub invocation: Arc<Mutex<InvocationContext>>,
    pub editor_factory: SessionEditorFactory,
    pub db: Database,
    /// Per-workflow `WorkflowDb` cache. First touch under an entity
    /// applies its migrations and inserts; subsequent touches hit the
    /// map. Wrapped in `Arc` so clones (one per workflow invocation)
    /// see the same cache.
    pub workflow_dbs: Arc<DashMap<Uuid, Arc<WorkflowDb>>>,
    /// Same hub [`SessionRuntime`] publishes through. The workflow
    /// driver writes the session title into it on `SurfaceCmd::SetTitle`;
    /// this side only reads it (to seed a booting workflow's `getTitle`).
    pub entities: Arc<EntityHub>,
    /// IO bundle (timer + shell + fs). Production wires `RealIo`;
    /// tests wire a `StubIo` variant.
    pub io: Io,
    /// Project roots that define which files are editable. Discovered once
    /// at session start from the initial cwd.
    pub editable_roots: Vec<PathBuf>,
}

impl<Io: frances_workflow::WorkflowIo> frances_workflow::WorkflowIo for WorkflowDepsImpl<Io> {
    type Timer = Io::Timer;
    type Shell = Io::Shell;
    type Fs = Io::Fs;

    fn timer(&self) -> &Self::Timer {
        self.io.timer()
    }
    fn shell(&self) -> &Self::Shell {
        self.io.shell()
    }
    fn fs(&self) -> &Self::Fs {
        self.io.fs()
    }
}

impl<Io: frances_workflow::WorkflowIo> WorkflowDeps for WorkflowDepsImpl<Io> {
    type ChatSessionManager = ChatSessionManager<ChatDepsImpl>;
    type EditorFactory = SessionEditorFactory;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager {
        &self.chat
    }

    fn editor_factory(&self) -> &Self::EditorFactory {
        &self.editor_factory
    }

    fn current_env(&self) -> Arc<HashMap<std::ffi::OsString, std::ffi::OsString>> {
        self.invocation.lock().process.env.clone()
    }

    fn current_cwd(&self) -> Option<PathBuf> {
        self.invocation.lock().process.cwd.clone()
    }

    fn session_title(&self) -> Option<String> {
        self.entities.session_title()
    }

    fn editable_roots(&self) -> &[PathBuf] {
        &self.editable_roots
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

/// The workflow stopped waiting for a permission before the user (or
/// auto-judge) answered — the embedded reply oneshot's receiver is gone.
#[derive(Debug, thiserror::Error)]
#[error("workflow stopped waiting for this permission")]
pub struct PermissionDropped;

/// Hands out fresh per-context read sessions over one shared anchor engine,
/// so workflow contexts share the persistent anchor state but each gets its
/// own "have I read this here?" cache.
#[derive(Clone)]
pub struct SessionEditorFactory {
    pub engine: Arc<EditEngine<AnchorStoreImpl>>,
}

impl EditorFactory for SessionEditorFactory {
    type Store = AnchorStoreImpl;

    fn new_session(&self) -> EditSession<AnchorStoreImpl> {
        EditSession::new(self.engine.clone())
    }
}

/// The session runtime. Holds the per-session state; produces frames
/// into [`EventsChannel`] and accepts prompt / permission input from
/// the UI.
///
/// Generic on the workflow `Io` bundle so tests can inject a
/// `MockTimer`-bearing IO via [`SessionRuntime::start_with_io`].
/// Production defaults to [`RealIo`] — every existing caller resolves
/// to `SessionRuntime<RealIo>` via the default parameter.
pub struct SessionRuntime<Io: frances_workflow::WorkflowIo = RealIo> {
    pub session: Session,
    pub invocation: Arc<Mutex<InvocationContext>>,
    pub editor_factory: SessionEditorFactory,
    pub events: EventsChannel,
    /// Publish point for entity state — singleton chrome (workspace
    /// dirs, session title / usage / busy) and every instanced entity
    /// (shells, …). Shared with [`WorkflowDepsImpl`].
    pub entities: Arc<EntityHub>,
    /// Kept alive so the config-event-processor task stays running for the
    /// runtime's lifetime. The chat manager and provider cache hold their
    /// own bindings, but parking the handle here makes the lifetime
    /// guarantee explicit.
    pub _config: ConfigHandle,
    pub history: TursoHistoryStore,
    pub cache: ProviderCache,
    pub workflows: ConfigBinding<HashMap<String, WorkflowConfig>>,
    /// `default_workflow` config binding. Used when the session metadata
    /// has no selected workflow yet.
    pub default_workflow: ConfigBinding<Option<String>>,
    pub workflow_runtime: Arc<WorkflowRuntime<WorkflowDepsImpl<Io>>>,
    pub active_workflow: ActiveWorkflow,
    /// Control channel into the long-lived workflow driver task. Slash
    /// workflow switches go here; plain input/interrupts bypass it and
    /// land on the active inbox via [`ActiveWorkflow`]'s live sender.
    pub(crate) workflow_cmd: mpsc::UnboundedSender<DriverCmd>,
    /// Same `ChatSessionManager` the workflow runtime uses (it's a
    /// cheap `Arc`-backed clone). The auto-judge calls `chat.complete`
    /// directly to score permission requests that opted into auto.
    pub chat: ChatSessionManager<ChatDepsImpl>,
    /// Writes session-config rows and emits the matching events on the
    /// DB layer in one call.
    pub session_config_writer: SessionConfigWriter,
    /// Cancelled by [`SessionRuntime::shutdown`]. Visible to any
    /// in-flight `prompt` task that wants to bail early.
    pub cancel: CancellationToken,
}

/// Boxed `FnOnce` over the freshly-built [`ProviderCache`]. Used by
/// tests to `cache.insert_stub(<id>, Arc::new(StubProvider::new()))`.
pub type ProviderCacheHook = Box<dyn FnOnce(&ProviderCache) + Send>;

struct DefaultWorkflowProvider {
    default_workflow: String,
}

#[async_trait]
impl ConfigProvider for DefaultWorkflowProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        let event = ConfigEvent::new(
            Path::parse("default_workflow"),
            ConfigValue::String(self.default_workflow.clone().into()),
        );
        let _ = events.send(vec![event]).await;
        Ok(())
    }
}

/// Knobs passed to [`SessionRuntime::start_with`] /
/// [`SessionRuntime::start_with_io`]. Default is the production
/// behaviour (empty providers vec, no-op cache hook), so callers that
/// only care about *one* override can build the rest via
/// `..Default::default()`.
#[derive(Default)]
pub struct StartOverrides {
    /// Extra `ConfigProvider`s appended to the default chain (highest
    /// priority, "last writer wins"). Tests pass an `InMemoryProvider`
    /// here to seed `models.default`, `model_providers.<id>`, and
    /// `workflows.<id>.file` without touching XDG.
    pub extra_config_providers: Vec<Arc<dyn ConfigProvider>>,
    /// Override the workflow selected for a newly-created session.
    pub default_workflow: Option<String>,
    /// Closure run against the freshly-built [`ProviderCache`] before
    /// the [`ChatSessionManager`] is constructed.
    pub on_cache: Option<ProviderCacheHook>,
}

impl SessionRuntime<RealIo> {
    /// Build the runtime with the production IO bundle, restore the
    /// selected workflow, and return it alongside the events
    /// receiver the UI should drain. Initial scrollback replay is
    /// not done here — call
    /// [`SessionRuntime::replay_initial_scrollback`] after the receiver
    /// is hooked up.
    pub async fn start(
        session: Session,
        db: Database,
        invocation: InvocationContext,
    ) -> crate::Result<(Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<StreamFrame>)> {
        Self::start_with(session, db, invocation, StartOverrides::default()).await
    }

    /// Production-IO variant of [`start_with_io`](
    /// SessionRuntime::start_with_io). Tests reach for `start_with_io`
    /// directly when they need to inject a mock-clock IO.
    pub async fn start_with(
        session: Session,
        db: Database,
        invocation: InvocationContext,
        overrides: StartOverrides,
    ) -> crate::Result<(Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<StreamFrame>)> {
        Self::start_with_io(session, db, invocation, overrides, RealIo::default()).await
    }
}

impl<Io: frances_workflow::WorkflowIo> SessionRuntime<Io> {
    /// Build the runtime with a caller-supplied IO bundle. Production
    /// always passes [`RealIo`] (via [`start`](Self::start) /
    /// [`start_with`](Self::start_with)); tests pass a
    /// `StubIo<MockTimer, ...>` so the workflow's JS-side `Timer`
    /// goes through the virtual clock.
    pub async fn start_with_io(
        session: Session,
        db: Database,
        invocation: InvocationContext,
        overrides: StartOverrides,
        io: Io,
    ) -> crate::Result<(Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<StreamFrame>)> {
        let StartOverrides {
            mut extra_config_providers,
            default_workflow,
            on_cache,
        } = overrides;

        std::fs::create_dir_all(&session.runtime_dir).map_err(|source| {
            RuntimeError::CreateRuntimeDir {
                path: session.runtime_dir.clone(),
                source,
            }
        })?;

        let edit_engine = EditEngine::new(AnchorStoreImpl::new(db.clone()));

        let session_provider = Arc::new(SessionConfigProvider::new(db.clone()));
        if let Some(default_workflow) = default_workflow {
            extra_config_providers.push(Arc::new(DefaultWorkflowProvider { default_workflow }));
        }
        let config_providers =
            build_config_providers(session_provider.clone(), extra_config_providers);
        let config = ConfigHandle::build(config_providers).await?;
        let session_config_writer = session_provider
            .writer()
            .expect("SessionConfigProvider::load ran during ConfigHandle::build");
        let default_model = config
            .bind::<ModelConfig>(["models", "default"])?
            .required()
            .map_err(|_| RuntimeError::DefaultModelMissing)?;
        let cache = ProviderCache::new(config.clone())?;
        if let Some(hook) = on_cache {
            hook(&cache);
        }
        let workflows = config.bind::<HashMap<String, WorkflowConfig>>("workflows")?;
        let default_workflow = config.bind::<Option<String>>("default_workflow")?;

        let history = TursoHistoryStore::new(db.clone());
        let chat_deps = ChatDepsImpl {
            history: history.clone(),
        };
        let chat =
            ChatSessionManager::new(chat_deps, config.clone(), default_model, cache.clone())?;

        let root_markers = config
            .bind::<Vec<PathBuf>>("root_markers")?
            .get()
            .map(|r| (*r).clone())
            .unwrap_or_else(default_root_markers);
        let editable_root = match invocation.process.cwd.as_ref() {
            Some(cwd) => discover_root(cwd, &root_markers).await,
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let editable_roots = vec![editable_root];

        let editor_factory = SessionEditorFactory {
            engine: Arc::new(edit_engine),
        };
        let (events, events_rx) = EventsChannel::new();
        // Force-settles any Live rows from a previous process, then
        // loads what's persisted.
        let entities = Arc::new(EntityHub::open(db.clone(), events.clone()).await?);
        entities
            .set_workspace(&WorkspaceSnapshot {
                directories: invocation
                    .workspace
                    .dirs()
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect(),
            })
            .await;
        entities
            .update_session(|session_snapshot| {
                *session_snapshot = SessionSnapshot {
                    title: session.meta.title.clone(),
                    ..SessionSnapshot::default()
                };
            })
            .await;
        let invocation = Arc::new(Mutex::new(invocation));
        let workflow_runtime = Arc::new(WorkflowRuntime::new(WorkflowDepsImpl {
            chat: chat.clone(),
            invocation: invocation.clone(),
            editor_factory: editor_factory.clone(),
            db: db.clone(),
            workflow_dbs: Arc::new(DashMap::new()),
            entities: entities.clone(),
            io,
            editable_roots,
        })?);

        let (workflow_cmd, cmd_rx) = mpsc::unbounded_channel();

        let runtime = Arc::new(Self {
            session: session.clone(),
            invocation,
            editor_factory,
            events,
            entities,
            _config: config,
            history,
            cache,
            workflows,
            default_workflow,
            workflow_runtime,
            active_workflow: ActiveWorkflow::new(db),
            workflow_cmd,
            chat,
            session_config_writer,
            cancel: CancellationToken::new(),
        });

        // Restore the selected workflow — or, if the session metadata has
        // none, seat the configured `default_workflow`. The
        // returned instance becomes the driver's initial active workflow;
        // anything it emits during top-level evaluation buffers in the
        // handle's frame channel and flushes once the driver pumps.
        let initial = match crate::workflows::restore_or_start_default(&runtime).await {
            Ok(initial) => initial,
            Err(error) => {
                warn!(%error, "workflow session restore failed");
                None
            }
        };
        // Publish the active wires synchronously (before the driver task
        // is scheduled) so `replay_initial_scrollback` sees the
        // active instance immediately.
        runtime.active_workflow.seat_initial(initial.as_ref());
        // Queue the attach snapshot; the UI drains it when it attaches
        // to the events receiver. Same ordered channel as the replay
        // burst below it, so every entity snapshot arrives before any
        // replayed section that references it.
        runtime.entities.attach_publish_all();

        tokio::spawn(crate::workflows::run_driver(
            runtime.clone(),
            cmd_rx,
            initial,
        ));

        Ok((runtime, events_rx))
    }

    /// Replace the latest invocation context. Workflows that read
    /// `current_env` / `current_cwd` see the new value on next access.
    pub async fn update_invocation(&self, ctx: InvocationContext) {
        let directories = ctx
            .workspace
            .dirs()
            .iter()
            .map(|dir| dir.display().to_string())
            .collect();
        *self.invocation.lock() = ctx;
        self.entities
            .set_workspace(&WorkspaceSnapshot { directories })
            .await;
    }

    /// Run the initial scrollback replay for the currently-active
    /// workflow instance. Sends a `ScrollbackFrame::Reset` / replay /
    /// `ScrollbackFrame::End` burst into the events channel.
    pub async fn replay_initial_scrollback(self: &Arc<Self>) {
        let active_instance = self.active_workflow.active_instance().await;
        if let Err(error) =
            replay::write_initial_replay(&self.events, self.active_workflow.db(), active_instance)
                .await
        {
            warn!(%error, "initial scrollback replay failed");
        }
    }

    /// Deliver user input. Slash commands switch workflow (via the
    /// driver's control channel); anything else lands on the active
    /// workflow's inbox as plain input. Non-blocking — input is just
    /// IO, decoupled from any cycle. Frames flow through `self.events`.
    pub fn prompt(self: &Arc<Self>, text: String) {
        crate::workflows::dispatch_input(self, &text);
    }

    /// Deliver an interrupt to the active workflow's inbox (Esc in the
    /// UI). The workflow decides how to react.
    pub fn interrupt(self: &Arc<Self>) {
        crate::workflows::dispatch_interrupt(self);
    }

    /// Resolve a permission request previously emitted as
    /// [`StreamFrame::Permission`] by sending on its embedded `reply`
    /// slot. If the user picked `RedirectToChat`, the workflow sees a
    /// denial and a fresh prompt is dispatched with the user's text.
    pub fn respond_permission(
        self: &Arc<Self>,
        reply: oneshot::Sender<PermissionResponse>,
        response: PermissionResponseWire,
    ) -> Result<(), PermissionDropped> {
        let (workflow_response, redirect) = match response {
            PermissionResponseWire::Yes { details } => (PermissionResponse::Yes { details }, None),
            PermissionResponseWire::No { details } => (PermissionResponse::No { details }, None),
            PermissionResponseWire::RedirectToChat { content } => {
                (PermissionResponse::No { details: None }, Some(content))
            }
        };

        reply
            .send(workflow_response)
            .map_err(|_| PermissionDropped)?;

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
///   5. `extras` — anything the caller wants to override the chain
///      with (last wins). Production passes an empty vec; tests pass
///      an `InMemoryProvider`.
///
/// Each TOML file is `.optional()` — running with no config files
/// present is a supported configuration.
fn build_config_providers(
    session_provider: Arc<SessionConfigProvider>,
    extras: Vec<Arc<dyn ConfigProvider>>,
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
    providers.extend(extras);

    providers
}

/// Walk up from `cwd` looking for any directory named in `markers`. Returns the
/// first ancestor (or `cwd` itself) that contains one; falls back to `cwd` when
/// no marker is found.
pub async fn discover_root(cwd: &std::path::Path, markers: &[PathBuf]) -> PathBuf {
    let mut dir = cwd;
    loop {
        for marker in markers {
            if tokio::fs::metadata(dir.join(marker))
                .await
                .is_ok_and(|m| m.is_dir())
            {
                return dir.to_path_buf();
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

/// Default markers used when `root_markers` is not configured.
pub fn default_root_markers() -> Vec<PathBuf> {
    vec![PathBuf::from(".jj"), PathBuf::from(".git")]
}
