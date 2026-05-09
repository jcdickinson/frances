use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::transport::TransportError;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("no client context — attach first")]
    NoClientContext,
    #[error("open daemon log {path}: {source}")]
    OpenDaemonLog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("dup2 stdout to daemon log: {0}")]
    Dup2Stdout(#[source] io::Error),
    #[error("dup2 stderr to daemon log: {0}")]
    Dup2Stderr(#[source] io::Error),
    #[error("install tracing subscriber: {0}")]
    InstallSubscriber(#[from] tracing_subscriber::util::TryInitError),
    #[error("create runtime dir {path}: {source}")]
    CreateRuntimeDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("write pid file for {session_id}: {source}")]
    WritePidFile {
        session_id: String,
        #[source]
        source: io::Error,
    },
    #[error("bind {label} socket {path}: {source}")]
    BindSocket {
        label: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("models::default is required")]
    DefaultModelMissing,
    #[error("llm task panicked: {0}")]
    LlmTaskPanicked(#[from] tokio::task::JoinError),
    #[error("send stream frame: {0}")]
    Send(#[from] TransportError),
    #[error("clean up {label} socket {path}: {source}")]
    CleanupSocket {
        label: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("control protocol I/O: {0}")]
    ControlIo(#[source] io::Error),
    #[error("client transport listen: {0}")]
    ClientListen(#[source] io::Error),
}
