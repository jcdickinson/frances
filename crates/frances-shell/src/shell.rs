use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc::UnboundedSender;

use frances_core::Truncated;

use crate::child::signal_pgid;
use crate::error::{HandshakeFailure, ShellError, ShellResult};
use crate::proto::{Sentinel, make_nonce, wrapper_script};
pub use crate::reader::QuietReason;
use crate::reader::{OutputReader, ReadEvent, ReadOutcome};

/// Output-silence window used when [`WaitOpts::quiet`] is `None`. This is
/// the mechanism's only built-in default — a `None` `max` stays unbounded.
/// Higher-level callers that want a wall-clock ceiling (e.g. the shell
/// tool) layer their own default and any quiet/max relationship on top.
pub const DEFAULT_QUIET: Duration = Duration::from_secs(10);

/// A quasi-persistent bash execution context.
///
/// Each [`Shell::run`] spawns a fresh bash process for one user script.
/// Only state captured by Frances is carried between runs: logical cwd always
/// persists, and exported environment variables named in a run's
/// [`RunOpts::persist`] are copied into the stored snapshot after that run's
/// teardown completes. Shell functions, aliases, non-exported variables, traps,
/// and other in-process bash state intentionally do not survive the process.
///
/// Pipes-only: stdin/stdout are connected by anonymous pipes, no PTY.
/// Interactive apps that hard-require a TTY (`vim`, `top`, `psql` without
/// `-c`) will not work in this mode. Non-interactive equivalents (`psql -c
/// "SELECT 1"`, `ssh host cmd`) work fine.
pub struct Shell {
    state: ShellSnapshot,
    in_flight: Option<InFlight>,
    output_sink: Option<UnboundedSender<ReadEvent>>,
    nonce: String,
    tmpdir: TempDir,
    next_run_id: u64,
    alive: bool,
}

struct InFlight {
    child: Child,
    reader: OutputReader<ChildStdout>,
    pgid: u32,
    capture: RunCapture,
}

struct RunCapture {
    cwd_path: PathBuf,
    env_path: PathBuf,
    persist: Vec<String>,
}

/// Stored shell state that callers may persist outside `frances-shell`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSnapshot {
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl ShellSnapshot {
    pub fn new(cwd: PathBuf, env: BTreeMap<String, String>) -> Self {
        Self { cwd, env }
    }
}

/// Configuration for [`Shell::spawn`].
#[derive(Debug, Clone, Default)]
pub struct ShellOptions {
    /// Initial logical working directory. `None` inherits from the parent.
    pub cwd: Option<PathBuf>,
    /// Initial exported environment overrides. Inherited env is preserved for
    /// child processes, but only values stored here are part of Frances'
    /// snapshot surface.
    pub env: Vec<(OsString, OsString)>,
    /// Bash code run once during construction. It is executed through the same
    /// one-shot machinery as a normal command; a non-zero exit aborts spawn.
    pub init_script: Option<String>,
}

/// Per-run execution options.
#[derive(Debug, Clone, Default)]
pub struct RunOpts {
    /// Bytes delivered to fd 0 for this invocation. When absent, stdin is EOF.
    pub stdin: Option<Vec<u8>>,
    /// Exported environment names to copy back into the stored snapshot after
    /// this invocation's teardown. This is one-shot, not a durable watch list.
    pub persist: Vec<String>,
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
    /// produced (stdout + stderr merged), `exit_code` is the user script's
    /// status.
    Done { exit_code: i32, output: String },
    /// One of the wait thresholds tripped. The invocation is still alive —
    /// call [`Shell::keep_waiting`] again (or [`Shell::interrupt`] /
    /// [`Shell::kill_running`] to stop it).
    Quiet { output: String, reason: QuietReason },
    /// EOF before the sentinel: the invocation died before Frances could run
    /// teardown and frame the result. Stored state is left unchanged.
    Dead { output: String },
}

impl Shell {
    /// Create a fresh quasi-persistent shell state holder.
    pub async fn spawn(opts: ShellOptions) -> ShellResult<Self> {
        let cwd = match opts.cwd {
            Some(cwd) => cwd,
            None => std::env::current_dir()?,
        };
        let env = opts
            .env
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();

        let mut shell = Shell {
            state: ShellSnapshot { cwd, env },
            in_flight: None,
            output_sink: None,
            nonce: make_nonce(),
            tmpdir: TempDir::new()?,
            next_run_id: 0,
            alive: true,
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

    /// Whether this state holder can accept new runs. A `Dead` outcome marks
    /// it unusable to preserve the existing public lifecycle contract.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Return a clone of the persisted cwd/exported-env snapshot.
    pub fn snapshot(&self) -> ShellSnapshot {
        self.state.clone()
    }

    /// Replace the stored cwd/exported-env snapshot.
    pub fn update_snapshot(&mut self, snapshot: ShellSnapshot) -> ShellResult<()> {
        if self.in_flight.is_some() {
            return Err(ShellError::CommandRunning);
        }
        self.state = snapshot;
        Ok(())
    }

    /// Attach (or detach) a streaming event sink.
    pub fn set_output_sink(&mut self, sink: Option<UnboundedSender<ReadEvent>>) {
        self.output_sink = sink.clone();
        if let Some(in_flight) = self.in_flight.as_mut() {
            in_flight.reader.set_sink(sink);
        }
    }

    /// Submit `cmd` to a fresh bash process and read until the sentinel, an
    /// output silence of `wait.quiet`, or `wait.max` wall-clock — whichever
    /// fires first.
    pub async fn run(&mut self, cmd: &str, wait: WaitOpts) -> ShellResult<RunOutcome> {
        self.run_with_opts(cmd, RunOpts::default(), wait).await
    }

    /// Like [`Shell::run`], with fd-0 input and per-run env persistence names.
    pub async fn run_with_opts(
        &mut self,
        cmd: &str,
        opts: RunOpts,
        wait: WaitOpts,
    ) -> ShellResult<RunOutcome> {
        if !self.alive {
            return Err(ShellError::Dead);
        }
        if self.in_flight.is_some() {
            return Err(ShellError::CommandRunning);
        }

        self.start_run(cmd, opts).await?;
        self.read_outcome(wait).await
    }

    /// Continue waiting on the in-flight command. Returns the same shape as
    /// [`Shell::run`]. Errors with [`ShellError::NoRunningCommand`] if no
    /// command is currently in flight.
    pub async fn keep_waiting(&mut self, wait: WaitOpts) -> ShellResult<RunOutcome> {
        if !self.alive {
            return Err(ShellError::Dead);
        }
        if self.in_flight.is_none() {
            return Err(ShellError::NoRunningCommand);
        }
        self.read_outcome(wait).await
    }

    /// Send `SIGINT` to the in-flight process group. No-op if no command is
    /// currently running.
    pub async fn interrupt(&mut self) -> ShellResult<()> {
        self.signal_running(libc::SIGINT)
    }

    /// Like [`Shell::interrupt`] but `SIGKILL`. Used when a command ignores
    /// `SIGINT`.
    pub async fn kill_running(&mut self) -> ShellResult<()> {
        self.signal_running(libc::SIGKILL)
    }

    async fn start_run(&mut self, cmd: &str, opts: RunOpts) -> ShellResult<()> {
        let run_id = self.next_run_id;
        self.next_run_id = self.next_run_id.wrapping_add(1);

        let user_path = self.tmpdir.path().join(format!("user-{run_id}.sh"));
        let wrapper_path = self.tmpdir.path().join(format!("wrapper-{run_id}.sh"));
        let cwd_path = self.tmpdir.path().join(format!("cwd-{run_id}.txt"));
        let env_path = self.tmpdir.path().join(format!("env-{run_id}.nul"));

        tokio::fs::write(&user_path, cmd).await?;
        let wrapper = wrapper_script(
            &user_path,
            &cwd_path,
            &env_path,
            &self.state.cwd,
            &self.state.env,
            &self.nonce,
        );
        tokio::fs::write(&wrapper_path, wrapper).await?;

        let mut command = Command::new("bash");
        command
            .arg("--norc")
            .arg("--noprofile")
            .arg(&wrapper_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if opts.stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        apply_output_env(&mut command);
        for (name, value) in &self.state.env {
            command.env(name, value);
        }
        unsafe {
            command.pre_exec(|| {
                let rc = libc::setsid();
                if rc == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(ShellError::Spawn)?;
        let pgid = child
            .id()
            .ok_or(ShellError::Handshake(HandshakeFailure::MissingPid))?;
        if let Some(stdin) = opts.stdin {
            let mut child_stdin = child
                .stdin
                .take()
                .ok_or(ShellError::Handshake(HandshakeFailure::MissingStdin))?;
            child_stdin.write_all(&stdin).await?;
            child_stdin.flush().await?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or(ShellError::Handshake(HandshakeFailure::MissingStdout))?;
        let mut reader = OutputReader::new(stdout, Sentinel::new(&self.nonce));
        reader.set_sink(self.output_sink.clone());

        self.in_flight = Some(InFlight {
            child,
            reader,
            pgid,
            capture: RunCapture {
                cwd_path,
                env_path,
                persist: opts.persist,
            },
        });
        Ok(())
    }

    fn signal_running(&self, sig: libc::c_int) -> ShellResult<()> {
        if !self.alive {
            return Err(ShellError::Dead);
        }
        let Some(in_flight) = self.in_flight.as_ref() else {
            return Ok(());
        };
        signal_pgid(in_flight.pgid, sig).map_err(ShellError::Signal)
    }

    async fn read_outcome(&mut self, wait: WaitOpts) -> ShellResult<RunOutcome> {
        let read = {
            let in_flight = self
                .in_flight
                .as_mut()
                .ok_or(ShellError::NoRunningCommand)?;
            in_flight
                .reader
                .read_until_sentinel(Some(wait.quiet.unwrap_or(DEFAULT_QUIET)), wait.max)
                .await?
        };

        Ok(match read {
            ReadOutcome::Done { output, exit_code } => {
                let mut in_flight = self.in_flight.take().expect("in_flight exists after read");
                let _ = in_flight.child.wait().await;
                self.apply_capture(&in_flight.capture).await?;
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
                let mut in_flight = self.in_flight.take().expect("in_flight exists after read");
                let _ = in_flight.child.wait().await;
                self.alive = false;
                RunOutcome::Dead {
                    output: String::from_utf8_lossy(&output).into_owned(),
                }
            }
        })
    }

    async fn apply_capture(&mut self, capture: &RunCapture) -> ShellResult<()> {
        let cwd = tokio::fs::read_to_string(&capture.cwd_path).await?;
        let cwd = cwd.trim_end_matches('\n');
        if !cwd.is_empty() {
            self.state.cwd = PathBuf::from(cwd);
        }

        if capture.persist.is_empty() {
            return Ok(());
        }
        let env_bytes = tokio::fs::read(&capture.env_path).await?;
        let exported = parse_env_nul(&env_bytes);
        for name in &capture.persist {
            if name == "FRANCES_ROOT" {
                continue;
            }
            match exported.get(name) {
                Some(value) => {
                    self.state.env.insert(name.clone(), value.clone());
                }
                None => {
                    self.state.env.remove(name);
                }
            }
        }
        Ok(())
    }
}

fn apply_output_env(cmd: &mut Command) {
    // Plain-output hygiene: the TUI shows shell stdout as text, so ANSI colour
    // sequences are noise at best and broken at worst.
    cmd.env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("FORCE_COLOR", "0")
        .env("PAGER", "cat");
}

fn parse_env_nul(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for entry in bytes.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(eq) = entry.iter().position(|b| *b == b'=') else {
            continue;
        };
        let name = String::from_utf8_lossy(&entry[..eq]).into_owned();
        let value = String::from_utf8_lossy(&entry[eq + 1..]).into_owned();
        env.insert(name, value);
    }
    env
}
