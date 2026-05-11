use thiserror::Error;

use crate::llm::session_provider::SessionConfigWriteError;
use crate::server::ServerError;
use crate::session::SessionError;
use crate::shell_classifier::ShellClassifierError;
use crate::store::DatabaseError;
use crate::tools::ToolRegistryError;
use crate::tools::file::FileToolError;
use crate::tools::shell::ShellToolError;
use crate::workflows::WorkflowError;
use frances_llm::ProviderCacheError;
use frances_models_llm::chat::{ChatError, HistoryError};

/// All errors raised by the daemon. Each variant `#[from]`s a per-module
/// typed error so callers and tests can match on the exact failure mode.
/// This enum holds no generic carriers (no `Io`, no `String`, no
/// `Box<dyn Error>`) — every `?` call site routes through a typed module
/// error first.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Transport(#[from] crate::transport::TransportError),

    #[error(transparent)]
    Migration(#[from] crate::migrations::MigrationError),

    #[error(transparent)]
    Edit(#[from] crate::edit_session::EditError),

    #[error(transparent)]
    AnchorStore(#[from] frances_edit::StoreError),

    #[error(transparent)]
    ConfigBuild(#[from] frances_config::BuildError),

    #[error(transparent)]
    ConfigBind(#[from] frances_config::ConfigBindError),

    #[error(transparent)]
    Chat(#[from] ChatError),

    #[error(transparent)]
    History(#[from] HistoryError),

    #[error(transparent)]
    Server(#[from] ServerError),

    #[error(transparent)]
    Session(#[from] SessionError),

    #[error(transparent)]
    ShellClassifier(#[from] ShellClassifierError),

    #[error(transparent)]
    FileTool(#[from] FileToolError),

    #[error(transparent)]
    ShellTool(#[from] ShellToolError),

    #[error(transparent)]
    ToolRegistry(#[from] ToolRegistryError),

    #[error(transparent)]
    Workflow(#[from] WorkflowError),

    #[error(transparent)]
    ProviderCache(#[from] ProviderCacheError),

    #[error(transparent)]
    SessionConfigWrite(#[from] SessionConfigWriteError),

    #[error(transparent)]
    Database(#[from] DatabaseError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
