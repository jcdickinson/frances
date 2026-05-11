use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex as StdMutex;
use tokio::sync::Notify;

use crate::anchor_store::AnchorStoreImpl;
use crate::context::InvocationContext;
use crate::edit_session::EditSession;
use crate::history::TursoHistoryStore;
use crate::llm::SessionConfigWriter;
use crate::session::Session;
use crate::tools::ToolRegistry;
use crate::workflows::{WorkflowConfig, WorkflowStack};
use frances_config::{ConfigBinding, ConfigHandle};
use frances_llm::{ChatManagerDeps, ChatSession, ChatSessionManager, ProviderCache};
use frances_shell::Shell;
use frances_workflow::{Runtime as WorkflowRuntime, WorkflowDeps};

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
pub(crate) use turn::run_legacy_llm_turn;

use events::EventsRouter;

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
    /// Shared with `ServerState::last_context` so `current_env` reflects
    /// the latest client attach — the daemon's own env doesn't carry
    /// the user's API keys.
    pub last_context: Arc<StdMutex<Option<InvocationContext>>>,
}

impl WorkflowDeps for ServerWorkflowDeps {
    type ChatSessionManager = ChatSessionManager<ServerChatDeps>;

    fn chat_session_manager(&self) -> &Self::ChatSessionManager {
        &self.chat
    }

    fn current_env(&self) -> HashMap<std::ffi::OsString, std::ffi::OsString> {
        self.last_context
            .lock()
            .as_ref()
            .map(|ctx| ctx.process.env.clone())
            .unwrap_or_default()
    }
}

pub(crate) struct ServerState {
    pub session: Session,
    // TODO: This smells like a refactor needed
    pub client_attached: StdMutex<bool>,
    pub last_context: Arc<StdMutex<Option<InvocationContext>>>,
    pub daemon_pid: u32,
    pub edit_session: tokio::sync::Mutex<EditSession<AnchorStoreImpl>>,
    pub shell: tokio::sync::Mutex<Option<Shell>>,
    pub tool_registry: ToolRegistry,
    pub events: EventsRouter,
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
    pub chat: ChatSessionManager<ServerChatDeps>,
    /// The session driving the TUI's hardcoded turn workflow. There's
    /// only one for now; loaded (or created) once at daemon startup.
    pub primary_chat: ChatSession<ServerChatDeps>,
    pub workflows: ConfigBinding<HashMap<String, WorkflowConfig>>,
    pub workflow_runtime: Arc<WorkflowRuntime<ServerWorkflowDeps>>,
    pub workflow_stack: WorkflowStack,
    /// Writes session-config rows and emits the matching events on the
    /// DB layer in one call. Held for future RPC handlers that mutate
    /// session config.
    #[expect(dead_code, reason = "wired for future session-config writers")]
    pub session_config_writer: SessionConfigWriter,
}
