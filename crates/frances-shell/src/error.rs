use std::io;

use frances_core::Truncated;
use thiserror::Error;

pub type ShellResult<T> = Result<T, ShellError>;

/// Bounded view of a command's combined output embedded in a handshake
/// error. Keeps the leading chars — the start of a failing init script's
/// output is the most diagnostic part.
type InitOutput = Truncated<'static, 2000, true>;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("failed to spawn bash: {0}")]
    Spawn(#[source] io::Error),

    #[error("bash startup handshake failed: {0}")]
    Handshake(#[source] HandshakeFailure),

    #[error("io error talking to shell: {0}")]
    Io(#[from] io::Error),

    #[error("shell is dead, spawn a new one")]
    Dead,

    #[error("no command is currently running")]
    NoRunningCommand,

    #[error("could not locate bash's child process: {0}")]
    NoChild(#[source] io::Error),
}

/// What went wrong during [`Shell::spawn`](crate::Shell::spawn)'s startup
/// handshake (and optional `init_script`).
#[derive(Debug, Error)]
pub enum HandshakeFailure {
    #[error("bash spawned without a PID")]
    MissingPid,

    #[error("bash spawned without stdin")]
    MissingStdin,

    #[error("bash spawned without stdout")]
    MissingStdout,

    #[error("writing handshake to stdin")]
    WriteFailed(#[source] io::Error),

    #[error("flushing handshake to stdin")]
    FlushFailed(#[source] io::Error),

    #[error("reading handshake sentinel")]
    ReadFailed(#[source] io::Error),

    #[error("timed out waiting for handshake sentinel")]
    SentinelTimedOut,

    #[error("bash exited during startup")]
    ExitedDuringStartup,

    #[error("init_script failed (exit {exit_code}): {output}")]
    InitScriptFailed { exit_code: i32, output: InitOutput },

    #[error("init_script did not complete within default wait")]
    InitScriptQuiet,

    #[error("bash died running init_script: {output}")]
    BashDiedDuringInit { output: InitOutput },
}
