use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("open session log {path}: {source}")]
    OpenSessionLog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("install tracing subscriber: {0}")]
    InstallSubscriber(#[from] tracing_subscriber::util::TryInitError),
    #[error("create runtime dir {path}: {source}")]
    CreateRuntimeDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("models::default is required")]
    DefaultModelMissing,
    #[error("llm task panicked: {0}")]
    LlmTaskPanicked(#[from] tokio::task::JoinError),
}
