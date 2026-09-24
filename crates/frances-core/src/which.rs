use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WhichError {
    #[error("executable lookup failed: {0}")]
    Lookup(#[from] which::Error),
    #[error("executable lookup task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Resolve on the calling machine using its effective PATH and working directory.
/// Filesystem probes run on Tokio's blocking pool.
pub async fn which_in(
    command: impl Into<OsString>,
    path: Option<OsString>,
    cwd: impl Into<PathBuf>,
) -> Result<PathBuf, WhichError> {
    let command = command.into();
    let cwd = cwd.into();
    Ok(tokio::task::spawn_blocking(move || which::which_in(command, path, cwd)).await??)
}
