use std::io;

use frances_core::Truncated;
use thiserror::Error;

pub type ShellResult<T> = Result<T, ShellError>;

/// Bounded view of a command's combined output embedded in an init-script
/// error. Keeps the leading chars — the start of a failing init script's
/// output is the most diagnostic part.
type InitOutput = Truncated<'static, 2000, true>;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("failed to spawn bash: {0}")]
    Spawn(#[source] io::Error),

    #[error("bash spawned without a PID")]
    MissingPid,

    #[error("bash spawned without stdin")]
    MissingStdin,

    #[error("bash spawned without stdout")]
    MissingStdout,

    #[error("init_script failed (exit {exit_code}): {output}")]
    InitScriptFailed { exit_code: i32, output: InitOutput },

    #[error("init_script did not complete within default wait")]
    InitScriptQuiet,

    #[error("bash died running init_script: {output}")]
    InitScriptDied { output: InitOutput },

    #[error("io error talking to shell: {0}")]
    Io(#[from] io::Error),

    #[error("no command is currently running")]
    NoRunningCommand,

    #[error("a command is already running")]
    CommandRunning,

    #[error("failed to signal shell process group: {0}")]
    Signal(#[source] io::Error),
}
