use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tokio::sync::Notify;

use crate::anchor_store::AnchorStoreImpl;
use crate::chat::{ChatSession, ChatSessionManager};
use crate::context::InvocationContext;
use crate::edit_session::EditSession;
use crate::llm::SessionConfigWriter;
use crate::session::Session;
use crate::tools::ToolRegistry;
use crate::workflows::WorkflowConfig;
use frances_config::{ConfigBinding, ConfigHandle};
use frances_shell::Shell;

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

use events::EventsRouter;

pub(crate) struct ServerState {
    pub session: Session,
    pub client_attached: StdMutex<bool>,
    pub last_context: StdMutex<Option<InvocationContext>>,
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
    #[expect(dead_code, reason = "lifetime anchor for the config event processor")]
    pub config: ConfigHandle,
    pub chat: Arc<ChatSessionManager>,
    /// The session driving the TUI's hardcoded turn workflow. There's
    /// only one for now; loaded (or created) once at daemon startup.
    pub primary_chat: Arc<ChatSession>,
    pub workflows: ConfigBinding<HashMap<String, WorkflowConfig>>,
    /// Writes session-config rows and emits the matching events on the
    /// DB layer in one call. Held for future RPC handlers that mutate
    /// session config.
    #[expect(dead_code, reason = "wired for future session-config writers")]
    pub session_config_writer: SessionConfigWriter,
}
