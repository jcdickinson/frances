use std::io;
use thiserror::Error;

pub type ShellResult<T> = Result<T, ShellError>;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("failed to spawn bash: {0}")]
    Spawn(#[source] io::Error),

    #[error("bash startup handshake failed: {0}")]
    Handshake(String),

    #[error("io error talking to shell: {0}")]
    Io(#[from] io::Error),

    #[error("shell is dead, spawn a new one")]
    Dead,

    #[error("no command is currently running")]
    NoRunningCommand,

    #[error("could not locate bash's child process: {0}")]
    NoChild(String),
}
