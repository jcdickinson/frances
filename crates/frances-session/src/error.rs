use thiserror::Error;

use crate::llm::session_provider::SessionConfigWriteError;
use crate::runtime::RuntimeError;
use crate::scrollback::ScrollbackError;
use crate::session::SessionError;
use crate::store::DatabaseError;
use crate::workflows::{WorkflowError, WorkflowStackError};
use frances_edit::EditError;
use frances_llm::ProviderCacheError;
use frances_models_llm::chat::{ChatError, HistoryError};

/// All errors raised by the session runtime.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Migration(#[from] frances_storage::MigrationError),

    #[error(transparent)]
    Edit(#[from] EditError),

    #[error(transparent)]
    ConfigBuild(#[from] frances_config::BuildError),

    #[error(transparent)]
    ConfigBind(#[from] frances_config::ConfigBindError),

    #[error(transparent)]
    Chat(#[from] ChatError),

    #[error(transparent)]
    History(#[from] HistoryError),

    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    #[error(transparent)]
    Session(#[from] SessionError),

    #[error(transparent)]
    Workflow(#[from] WorkflowError),

    #[error(transparent)]
    WorkflowStack(#[from] WorkflowStackError),

    #[error(transparent)]
    ProviderCache(#[from] ProviderCacheError),

    #[error(transparent)]
    SessionConfigWrite(#[from] SessionConfigWriteError),

    #[error(transparent)]
    Database(#[from] DatabaseError),

    #[error(transparent)]
    Scrollback(#[from] ScrollbackError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
