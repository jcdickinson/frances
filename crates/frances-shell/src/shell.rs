use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use tokio::sync::mpsc::UnboundedSender;

use frances_core::Truncated;

use crate::child::{list_children, signal_pid};
use crate::error::{HandshakeFailure, ShellError, ShellResult};
use crate::proto::{Sentinel, handshake_bytes, make_nonce, wrapper_bytes};
pub use crate::reader::QuietReason;
use crate::reader::{OutputReader, ReadEvent, ReadOutcome};

/// Output-silence window used when [`WaitOpts::quiet`] is `None`. This is
/// the mechanism's only built-in default — a `None` `max` stays unbounded.
/// Higher-level callers that want a wall-clock ceiling (e.g. the shell
/// tool) layer their own default and any quiet/max relationship on top.
pub const DEFAULT_QUIET: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A long-lived bash subprocess.
///
/// State (env vars, current directory, shell functions, aliases, sourced
/// scripts) survives across calls to [`Shell::run`]. Each command is written
/// to a per-shell tmpfile and dot-sourced into the same bash process; there
/// is no fresh `bash -c '...'` per call. Callers can write *anything* bash
/// supports — multi-line `if`/`for`, pipelines, subshells, heredocs, function
/// definitions, redirections — exactly as they would type it at a real bash
/// prompt. No escaping or wrapping is required.
///
/// Pipes-only: stdin/stdout are connected by anonymous pipes, no PTY.
/// Interactive apps that hard-require a TTY (`vim`, `top`, `psql` without
/// `-c`) will not work in this mode. Non-interactive equivalents (`psql -c
/// "SELECT 1"`, `ssh host cmd`) work fine.
pub struct Shell {
    // Field declaration order is load-bearing for drop: fields drop
    // top-to-bottom, so `stdin` closes before `_child`. Closing stdin asks
    // bash to exit gracefully (it sees EOF); the Command's `kill_on_drop`
    // is the backstop when `_child` drops a moment later.
    stdin: ChildStdin,
    reader: OutputReader<ChildStdout>,
    // Held only for its `kill_on_drop` side effect; never read after spawn.
    _child: Child,
    bash_pid: u32,
    nonce: String,
    // Held for RAII cleanup of the per-shell tmpdir on drop.
    _tmpdir: TempDir,
    cmd_path: PathBuf,
    alive: bool,
    running: bool,
}

/// Configuration for [`Shell::spawn`].
#[derive(Debug, Clone, Default)]
pub struct ShellOptions {
    /// Working directory for the bash process. `None` inherits from the
    /// parent.
    pub cwd: Option<PathBuf>,
    /// Environment overrides applied at spawn. Inherited env is preserved
    /// for keys not listed here.
    pub env: Vec<(OsString, OsString)>,
    /// Bash code dot-sourced after the startup handshake completes —
    /// useful for loading secrets or setting up shell functions once. Treated
    /// as a normal command: a non-zero exit aborts spawn with
    /// [`ShellError::Handshake`].
    pub init_script: Option<String>,
}

/// How long [`Shell::run`] / [`Shell::keep_waiting`] are willing to block
/// before returning [`RunOutcome::Quiet`].
///
/// Both fields are independent and either can be `None`:
/// - `quiet = None` falls back to [`DEFAULT_QUIET`] (10s).
/// - `max = None` disables the wall-clock ceiling — only output silence
///   (or the sentinel) returns early. Either way `max` only yields
///   `Quiet`; the command keeps running, so it's never a kill.
#[derive(Debug, Clone, Copy, Default)]
pub struct WaitOpts {
    /// Return [`RunOutcome::Quiet`] after this much output silence. The
    /// timer resets every time bytes arrive.
    pub quiet: Option<Duration>,
    /// Return [`RunOutcome::Quiet`] after this much wall-clock time has
    /// elapsed since the call started, regardless of streaming activity.
    pub max: Option<Duration>,
}

/// Result of a [`Shell::run`] / [`Shell::keep_waiting`] call.
#[derive(Debug)]
pub enum RunOutcome {
    /// Sentinel arrived: the command is done. `output` is everything
    /// produced (stdout + stderr merged), `exit_code` is the command's
    /// status.
    Done { exit_code: i32, output: String },
    /// One of the wait thresholds tripped. The shell is still alive and
    /// the command is still running — call [`Shell::keep_waiting`] again
    /// (or [`Shell::interrupt`] / [`Shell::kill_running`] to stop it).
    Quiet { output: String, reason: QuietReason },
    /// EOF on bash's stdout: the bash subprocess is gone (e.g., the user
    /// ran `exit`). [`Shell::is_alive`] is now `false`. Caller must spawn
    /// a fresh [`Shell`].
    Dead { output: String },
}

impl Shell {
    /// Spawn a fresh bash subprocess and complete the startup handshake.
    pub async fn spawn(opts: ShellOptions) -> ShellResult<Self> {
        let nonce = make_nonce();
        let tmpdir = TempDir::new()?;
        let cmd_path = tmpdir.path().join("cmd.sh");

        let mut cmd = Command::new("bash");
        cmd.arg("--norc")
            .arg("--noprofile")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // Plain-output hygiene: the TUI shows shell stdout as text, so
        // ANSI colour sequences are noise at best and broken at worst.
        // `TERM=dumb` makes terminfo-aware tools emit no escapes;
        // `NO_COLOR=1` is the no-color.org standard honoured by ripgrep,
        // cargo, bat, jq, gcc, ls --color=auto, etc.; `CLICOLOR=0` and
        // `FORCE_COLOR=0` cover the BSD- and Node-flavoured holdouts;
        // `PAGER=cat` keeps less from grabbing the pty for things like
        // `git log`. Applied before `opts.env` so callers can override
        // any of them per-shell.
        cmd.env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("FORCE_COLOR", "0")
            .env("PAGER", "cat");
        if let Some(cwd) = &opts.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(ShellError::Spawn)?;
        let bash_pid = child
            .id()
            .ok_or(ShellError::Handshake(HandshakeFailure::MissingPid))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(ShellError::Handshake(HandshakeFailure::MissingStdin))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ShellError::Handshake(HandshakeFailure::MissingStdout))?;

        let mut reader = OutputReader::new(stdout, Sentinel::new(&nonce));

        stdin
            .write_all(&handshake_bytes(&nonce))
            .await
            .map_err(|e| ShellError::Handshake(HandshakeFailure::WriteFailed(e)))?;
        stdin
            .flush()
            .await
            .map_err(|e| ShellError::Handshake(HandshakeFailure::FlushFailed(e)))?;

        match reader
            .read_until_sentinel(None, Some(HANDSHAKE_TIMEOUT))
            .await
            .map_err(|e| ShellError::Handshake(HandshakeFailure::ReadFailed(e)))?
        {
            ReadOutcome::Done { .. } => {}
            ReadOutcome::Quiet { .. } => {
                return Err(ShellError::Handshake(HandshakeFailure::SentinelTimedOut));
            }
            ReadOutcome::Eof { .. } => {
                return Err(ShellError::Handshake(HandshakeFailure::ExitedDuringStartup));
            }
        }

        let mut shell = Shell {
            stdin,
            reader,
            _child: child,
            bash_pid,
            nonce,
            _tmpdir: tmpdir,
            cmd_path,
            alive: true,
            running: false,
        };

        if let Some(script) = opts.init_script {
            match shell.run(&script, WaitOpts::default()).await? {
                RunOutcome::Done { exit_code: 0, .. } => {}
                RunOutcome::Done { exit_code, output } => {
                    return Err(ShellError::Handshake(HandshakeFailure::InitScriptFailed {
                        exit_code,
                        output: Truncated::new(output),
                    }));
                }
                RunOutcome::Quiet { .. } => {
                    return Err(ShellError::Handshake(HandshakeFailure::InitScriptQuiet));
                }
                RunOutcome::Dead { output } => {
                    return Err(ShellError::Handshake(
                        HandshakeFailure::BashDiedDuringInit {
                            output: Truncated::new(output),
                        },
                    ));
                }
            }
        }

        Ok(shell)
    }

    /// Whether bash is still alive. Once `false`, every method returns
    /// [`ShellError::Dead`]; spawn a new [`Shell`] to recover.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Attach (or detach) a streaming event sink. While set, every
    /// `run` / `keep_waiting` ships safe-not-sentinel output bytes
    /// through the channel as [`ReadEvent::Output`] events, and emits
    /// exactly one terminal [`ReadEvent::Done`] / `Quiet` / `Dead`
    /// before returning. `RunOutcome::*` payloads still carry the
    /// full output bytes so direct callers don't need to consume the
    /// channel.
    pub fn set_output_sink(&mut self, sink: Option<UnboundedSender<ReadEvent>>) {
        self.reader.set_sink(sink);
    }

    /// Submit `cmd` to the shell and read until the sentinel, an output
    /// silence of `wait.quiet`, or `wait.max` wall-clock — whichever fires
    /// first.
    ///
    /// `cmd` is bash code, written verbatim into the per-shell tmpfile and
    /// dot-sourced. Pipelines, redirections, subshells, multi-line
    /// `if`/`for`/`while`, function definitions, heredocs, `set -e`,
    /// `trap`, etc. all work as they would in a real interactive bash —
    /// the caller does not need to wrap anything in `bash -c`, escape
    /// special characters, or single-line their script.
    ///
    /// State changes (env vars, `cd`, function defs, sourced files,
    /// `shopt`s) persist into the next `run`.
    ///
    /// If a previous call returned [`RunOutcome::Quiet`], the same command
    /// is still running; passing a new `cmd` here is a logic error — call
    /// [`Shell::keep_waiting`] instead.
    pub async fn run(&mut self, cmd: &str, wait: WaitOpts) -> ShellResult<RunOutcome> {
        if !self.alive {
            return Err(ShellError::Dead);
        }
        if !self.running {
            tokio::fs::write(&self.cmd_path, cmd).await?;
            let bytes = wrapper_bytes(&self.cmd_path, &self.nonce);
            self.stdin.write_all(&bytes).await?;
            self.stdin.flush().await?;
            self.running = true;
        }
        self.read_outcome(wait).await
    }

    /// Continue waiting on the in-flight command. Returns the same shape
    /// as [`Shell::run`]. Errors with [`ShellError::NoRunningCommand`] if
    /// no command is currently in flight.
    pub async fn keep_waiting(&mut self, wait: WaitOpts) -> ShellResult<RunOutcome> {
        if !self.alive {
            return Err(ShellError::Dead);
        }
        if !self.running {
            return Err(ShellError::NoRunningCommand);
        }
        self.read_outcome(wait).await
    }

    /// Send `SIGINT` to bash's foreground child(ren). Bash itself is
    /// untouched, so the shell stays alive: the running command dies, the
    /// sentinel fires from the post-source line, and the next
    /// [`Shell::keep_waiting`] returns [`RunOutcome::Done`] with a non-zero
    /// exit. No-op if no command is currently running.
    pub async fn interrupt(&mut self) -> ShellResult<()> {
        self.signal_running(libc::SIGINT)
    }

    /// Like [`Shell::interrupt`] but `SIGKILL`. Used when a command
    /// ignores `SIGINT`.
    pub async fn kill_running(&mut self) -> ShellResult<()> {
        self.signal_running(libc::SIGKILL)
    }

    fn signal_running(&self, sig: libc::c_int) -> ShellResult<()> {
        if !self.alive {
            return Err(ShellError::Dead);
        }
        if !self.running {
            return Ok(());
        }
        let kids = list_children(self.bash_pid).map_err(ShellError::NoChild)?;
        for pid in kids {
            let _ = signal_pid(pid, sig);
        }
        Ok(())
    }

    async fn read_outcome(&mut self, wait: WaitOpts) -> ShellResult<RunOutcome> {
        let outcome = self
            .reader
            .read_until_sentinel(Some(wait.quiet.unwrap_or(DEFAULT_QUIET)), wait.max)
            .await?;
        Ok(match outcome {
            ReadOutcome::Done { output, exit_code } => {
                self.running = false;
                RunOutcome::Done {
                    exit_code,
                    output: String::from_utf8_lossy(&output).into_owned(),
                }
            }
            ReadOutcome::Quiet { output, reason } => RunOutcome::Quiet {
                output: String::from_utf8_lossy(&output).into_owned(),
                reason,
            },
            ReadOutcome::Eof { output } => {
                self.alive = false;
                self.running = false;
                RunOutcome::Dead {
                    output: String::from_utf8_lossy(&output).into_owned(),
                }
            }
        })
    }
}
