use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tokio::net::UnixListener;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::Result;
use crate::anchor_store::AnchorStoreImpl;
use crate::edit_session::EditSession;
use crate::history::TursoHistoryStore;
use crate::llm::SessionConfigProvider;
use crate::server::ServerChatDeps;
use crate::session::Session;
use crate::store::Database;
use crate::tools::ToolRegistry;
use crate::transport::remove_socket_if_present;
use crate::workflows::{WorkflowConfig, WorkflowStack};
use frances_config::{ConfigHandle, ConfigProvider, EnvProvider, TomlProvider};
use frances_edit::EditEngine;
use frances_llm::{ChatSessionManager, ProviderCache};
use frances_models_llm::ChatSessionManager as ChatSessionManagerTrait;
use frances_models_llm::chat::ChatSessionBuilder;
use frances_models_llm::config::ModelConfig;
use frances_workflow::Runtime as WorkflowRuntime;

use super::client_rpc::serve_client;
use super::control::serve_control;
use super::events::{EventsRouter, accept_events};
use super::{ServerError, ServerState};

pub async fn run(session: Session, db: Database) -> Result<()> {
    debug!(session_id = %session.id, "starting daemon server");

    fs::create_dir_all(&session.runtime_dir).map_err(|source| ServerError::CreateRuntimeDir {
        path: session.runtime_dir.clone(),
        source,
    })?;

    remove_socket_if_present(&session.control_socket_path()).map_err(|source| {
        ServerError::CleanupSocket {
            label: "control",
            path: session.control_socket_path(),
            source,
        }
    })?;
    remove_socket_if_present(&session.client_socket_path()).map_err(|source| {
        ServerError::CleanupSocket {
            label: "client",
            path: session.client_socket_path(),
            source,
        }
    })?;
    remove_socket_if_present(&session.events_socket_path()).map_err(|source| {
        ServerError::CleanupSocket {
            label: "events",
            path: session.events_socket_path(),
            source,
        }
    })?;

    fs::write(session.pid_path(), std::process::id().to_string()).map_err(|source| {
        ServerError::WritePidFile {
            session_id: session.id.clone(),
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
        .map_err(|_| ServerError::DefaultModelMissing)?;
    let cache = ProviderCache::new(config.clone())?;
    let workflows = config.bind::<HashMap<String, WorkflowConfig>>("workflows")?;

    let history = TursoHistoryStore::new(db);
    let chat_deps = ServerChatDeps {
        history: history.clone(),
    };
    let chat = ChatSessionManager::new(chat_deps, config.clone(), default_model, cache.clone())?;
    let primary_chat = ChatSessionManagerTrait::primary(
        &chat,
        ChatSessionBuilder::new().with_model_intents(["chat".to_string()]),
    )
    .await?;

    let last_context = Arc::new(StdMutex::new(None));
    let workflow_runtime = Arc::new(WorkflowRuntime::new(super::ServerWorkflowDeps {
        chat: chat.clone(),
        last_context: last_context.clone(),
    })?);

    let state = Arc::new(ServerState {
        session: session.clone(),
        client_attached: StdMutex::new(false),
        last_context,
        daemon_pid: std::process::id(),
        edit_session: tokio::sync::Mutex::new(EditSession::new(edit_engine)),
        shell: tokio::sync::Mutex::new(None),
        tool_registry: ToolRegistry::builtin(),
        events: EventsRouter::default(),
        shutdown: Notify::new(),
        config,
        history,
        cache,
        chat,
        primary_chat,
        workflows,
        workflow_runtime,
        workflow_stack: WorkflowStack::new(),
        session_config_writer,
    });

    let events_listener = UnixListener::bind(session.events_socket_path()).map_err(|source| {
        ServerError::BindSocket {
            label: "events",
            path: session.events_socket_path(),
            source,
        }
    })?;
    let events_state = state.clone();
    tokio::spawn(async move {
        accept_events(events_listener, events_state).await;
    });

    let control_state = state.clone();
    let control_path = session.control_socket_path();
    tokio::spawn(async move {
        if let Err(error) = serve_control(control_path, control_state).await {
            warn!(%error, "control listener exited");
        }
    });

    let client_state = state.clone();
    let client_path = session.client_socket_path();
    tokio::spawn(async move {
        if let Err(error) = serve_client(client_path, client_state).await {
            warn!(%error, "client listener exited");
        }
    });

    info!(
        session_id = %session.id,
        control = %session.control_socket_path().display(),
        client = %session.client_socket_path().display(),
        events = %session.events_socket_path().display(),
        "daemon listening"
    );

    state.shutdown.notified().await;
    info!("shutdown signaled");

    let _ = fs::remove_file(session.pid_path());
    let _ = fs::remove_file(session.control_socket_path());
    let _ = fs::remove_file(session.client_socket_path());
    let _ = fs::remove_file(session.events_socket_path());

    Ok(())
}

/// Builds the layered config provider stack for the daemon. Order is
/// low → high priority — `ConfigHandle::build` applies them in sequence,
/// so later providers override earlier ones.
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
