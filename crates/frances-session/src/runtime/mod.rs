//! In-process Rust agent runtime and per-session state.
use crate::{
    anchor_store::AnchorStoreImpl,
    context::InvocationContext,
    entities::{EntityHub, SessionSnapshot, WorkspaceSnapshot},
    events::{PermissionResponseWire, StreamFrame},
    history::TursoHistoryStore,
    llm::{SessionConfigProvider, SessionConfigWriter},
    session::Session,
};
use frances_config::{ConfigHandle, ConfigProvider, EnvProvider, TomlProvider};
use frances_edit::{EditEngine, EditSession};
use frances_harness::{
    EditorFactory, HarnessDeps, HarnessFs, HarnessIo, PermissionResponse, RealFs, RealIo,
};
use frances_llm::{ChatManagerDeps, ChatSessionManager, ProviderCache};
use frances_models_llm::config::ModelConfig;
use frances_storage::Database;
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

pub(crate) mod auto_judge;
mod driver;
mod error;
mod events;
mod logging;
pub use error::RuntimeError;
pub use events::EventsChannel;
pub use logging::install_logging;

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

#[derive(Clone)]
pub struct HarnessDepsImpl<Io: HarnessIo = RealIo> {
    pub invocation: Arc<Mutex<InvocationContext>>,
    pub editor_factory: SessionEditorFactory,
    pub io: Io,
    pub editable_roots: Vec<PathBuf>,
}
impl<Io: HarnessIo> HarnessIo for HarnessDepsImpl<Io> {
    type Shell = Io::Shell;
    type Fs = Io::Fs;
    fn shell(&self) -> &Self::Shell {
        self.io.shell()
    }
    fn fs(&self) -> &Self::Fs {
        self.io.fs()
    }
}
impl<Io: HarnessIo> HarnessDeps for HarnessDepsImpl<Io> {
    type EditorFactory = SessionEditorFactory;
    fn editor_factory(&self) -> &Self::EditorFactory {
        &self.editor_factory
    }
    fn current_env(&self) -> Arc<HashMap<std::ffi::OsString, std::ffi::OsString>> {
        self.invocation.lock().process.env.clone()
    }
    fn current_cwd(&self) -> Option<PathBuf> {
        self.invocation.lock().process.cwd.clone()
    }
    fn editable_roots(&self) -> &[PathBuf] {
        &self.editable_roots
    }
}

#[derive(Debug, thiserror::Error)]
#[error("agent stopped waiting for this permission")]
pub struct PermissionDropped;

#[derive(Clone)]
pub struct SessionEditorFactory {
    pub engine: Arc<EditEngine<AnchorStoreImpl>>,
}
impl EditorFactory for SessionEditorFactory {
    type Store = AnchorStoreImpl;
    fn new_session(&self) -> EditSession<Self::Store> {
        EditSession::new(self.engine.clone())
    }
}

pub struct SessionRuntime<Io: HarnessIo = RealIo> {
    pub session: Session,
    pub invocation: Arc<Mutex<InvocationContext>>,
    pub editor_factory: SessionEditorFactory,
    pub events: EventsChannel,
    pub entities: Arc<EntityHub>,
    pub _config: ConfigHandle,
    pub history: TursoHistoryStore,
    pub cache: ProviderCache,
    pub chat: ChatSessionManager<ChatDepsImpl>,
    pub session_config_writer: SessionConfigWriter,
    pub cancel: CancellationToken,
    pub(crate) db: Database,
    pub(crate) instance_id: Uuid,
    deps: HarnessDepsImpl<Io>,
    input: mpsc::UnboundedSender<driver::Input>,
}

pub type ProviderCacheHook = Box<dyn FnOnce(&ProviderCache) + Send>;
#[derive(Default)]
pub struct StartOverrides {
    pub extra_config_providers: Vec<Arc<dyn ConfigProvider>>,
    pub on_cache: Option<ProviderCacheHook>,
}

impl SessionRuntime<RealIo> {
    pub async fn start(
        session: Session,
        db: Database,
        invocation: InvocationContext,
    ) -> crate::Result<(Arc<Self>, mpsc::UnboundedReceiver<StreamFrame>)> {
        Self::start_with(session, db, invocation, StartOverrides::default()).await
    }
    pub async fn start_with(
        session: Session,
        db: Database,
        invocation: InvocationContext,
        overrides: StartOverrides,
    ) -> crate::Result<(Arc<Self>, mpsc::UnboundedReceiver<StreamFrame>)> {
        Self::start_with_io(session, db, invocation, overrides, RealIo::default()).await
    }
}
impl<Io: HarnessIo> SessionRuntime<Io> {
    pub async fn start_with_io(
        session: Session,
        db: Database,
        invocation: InvocationContext,
        overrides: StartOverrides,
        io: Io,
    ) -> crate::Result<(Arc<Self>, mpsc::UnboundedReceiver<StreamFrame>)> {
        let StartOverrides {
            extra_config_providers,
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
            Some(cwd) => discover_root_with(io.fs(), cwd, &root_markers).await,
            None => invocation.workspace.primary_dir().to_path_buf(),
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
        let deps = HarnessDepsImpl {
            invocation: invocation.clone(),
            editor_factory: editor_factory.clone(),
            io,
            editable_roots,
        };
        let (input, input_rx) = mpsc::unbounded_channel();
        let instance_id = Uuid::parse_str(&session.id).expect("session ids are UUIDs");
        let runtime = Arc::new(Self {
            session,
            invocation,
            editor_factory,
            events,
            entities,
            _config: config,
            history,
            cache,
            chat,
            session_config_writer,
            cancel: CancellationToken::new(),
            db,
            instance_id,
            deps,
            input,
        });
        runtime.entities.attach_publish_all();
        tokio::spawn(driver::run(runtime.clone(), input_rx));
        Ok((runtime, events_rx))
    }
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
    pub async fn replay_initial_scrollback(self: &Arc<Self>) {
        if let Err(error) =
            crate::scrollback::replay_to_channel(&self.events, &self.db, self.instance_id).await
        {
            warn!(%error, "initial scrollback replay failed");
        }
    }
    pub fn prompt(self: &Arc<Self>, text: String) {
        if let Err(error) = self.input.send(driver::Input::Prompt(text)) {
            warn!(%error, "agent input closed");
        }
    }
    pub fn interrupt(self: &Arc<Self>) {
        if let Err(error) = self.input.send(driver::Input::Interrupt) {
            warn!(%error, "agent input closed");
        }
    }
    pub fn respond_permission(
        self: &Arc<Self>,
        reply: oneshot::Sender<PermissionResponse>,
        response: PermissionResponseWire,
    ) -> Result<(), PermissionDropped> {
        let (response, redirect) = match response {
            PermissionResponseWire::Yes { details } => (PermissionResponse::Yes { details }, None),
            PermissionResponseWire::No { details } => (PermissionResponse::No { details }, None),
            PermissionResponseWire::RedirectToChat { content } => {
                (PermissionResponse::No { details: None }, Some(content))
            }
        };
        reply.send(response).map_err(|_| PermissionDropped)?;
        if let Some(content) = redirect {
            self.prompt(content);
        }
        Ok(())
    }
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

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
    discover_root_with(&RealFs, cwd, markers).await
}

async fn discover_root_with(
    fs: &impl HarnessFs,
    cwd: &std::path::Path,
    markers: &[PathBuf],
) -> PathBuf {
    let mut dir = cwd;
    loop {
        for marker in markers {
            match fs.metadata(&dir.join(marker)).await {
                Ok(metadata) if metadata.is_dir => return dir.to_path_buf(),
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, ?dir, ?marker, "root marker unavailable"),
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

/// All built-in tools, including the permission judge, with the same strict
/// mode selection used by the model provider. Does not start a session.
pub fn tool_schemas() -> Vec<serde_json::Value> {
    frances_harness::tools::definitions()
        .into_iter()
        .chain(auto_judge::DECIDE_TOOLS.iter().cloned())
        .map(|frances_models_llm::ToolDef::Function(tool)| {
            serde_json::json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "strict": frances_models_llm::tool_args::is_strict_compatible(&tool.parameters),
                "parameters": tool.parameters,
            })
        })
        .collect()
}
