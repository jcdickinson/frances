//! `frances:v1/tools/shell` — bash primitive for workflow tool handlers.
//!
//! `new Shell()` represents quasi-persistent shell state. Spawning remains
//! lazy (deferred until the first `runOnce` so the JS constructor stays
//! sync), and all in-flight operations serialise on a per-Shell async
//! mutex — parallel tool calls that hit the same Shell queue up instead
//! of racing on the underlying `frances_shell::Shell` (which is
//! `&mut self`).
//!
//! Methods on the JS side:
//!
//! - `runOnce(cmd, opts)` — start a command, await its first stopping point.
//!   `opts` may contain wait tuning plus `stdin` bytes/text for fd 0 and
//!   `persist` exported-env names to capture after this run. Cwd always
//!   persists; `persist` is per-run, not a durable watch list.
//!   Resolves to `{ kind: "done", exit_code, output }`,
//!   `{ kind: "quiet", output, reason }`, or `{ kind: "dead", output }`.
//!   Throws if a command is already in flight on this Shell.
//! - `keepWaiting()` — resume waiting on the in-flight command.
//!   Same shape as `runOnce`. Throws if nothing is in flight.
//! - `kill()` — SIGKILL the in-flight command (no-op if nothing
//!   running). After kill, the next `keepWaiting` typically returns
//!   Done (with a non-zero exit) or Dead.
//! - `close()` — drop the bash subprocess. Future calls error.
//! - `isRunning()` — true while a command is suspended in Quiet state.
//! - `setVar(name, value, exported)` — bridge a Frances variable into
//!   bash: write `value` to a temp file and run either
//!   `<name>=$(cat 'tmp')` (shell var, current shell only) or
//!   `export <name>=$(cat 'tmp')` (env var, visible to subprocesses).
//!   Awaits Done; errors on non-zero exit.
//! - `captureVar(name)` — bridge a bash variable back into Frances:
//!   run `( set -u; printf '%s' "$<name>" > 'tmp' )`, await Done,
//!   return the temp file's contents as a string. Errors if the bash
//!   var is unset (the `set -u` subshell makes that visible instead of
//!   silently capturing `""`).
//!
//! The wait/quiet thresholds aren't exposed yet — `runOnce` /
//! `keepWaiting` use `frances_shell::WaitOpts::default()` (1s of output
//! silence, no max ceiling). Workflows that need different pacing can
//! layer a Timer + Promise.race on top.

use std::fs;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, Opt, This};
use rquickjs::promise::Promised;
use rquickjs::{Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use frances_shell::{
    QuietReason, ReadEvent, RunOpts as ShellRunOpts, RunOutcome, Shell, ShellError, ShellOptions,
    WaitOpts,
};

use super::throw_js as throw;
use crate::deps::WorkflowDeps;
use crate::io::{WorkflowIo, WorkflowShell};

type ShellOf<D> = <D as WorkflowIo>::Shell;

pub(crate) fn build_shell_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<ShellJs<ShellOf<D>>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> JsResult<Class<'js, ShellJs<ShellOf<D>>>> {
            // Construction is sync; the actual `Shell::spawn` is
            // deferred until the first `runOnce` (which is async).
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            Class::instance(
                ctx.clone(),
                ShellJs {
                    factory: deps.shell().clone(),
                    state: Arc::new(AsyncMutex::new(ShellState {
                        shell: None,
                        running: false,
                        closed: false,
                        event_tx: Some(event_tx),
                        editable_root: deps
                            .editable_roots()
                            .first()
                            .map(|p| p.to_string_lossy().into_owned()),
                    })),
                    event_rx: Arc::new(AsyncMutex::new(event_rx)),
                },
            )
        },
    )
}

pub struct ShellJs<F: WorkflowShell> {
    factory: F,
    state: Arc<AsyncMutex<ShellState>>,
    /// Receiver end of the per-shell event stream. JS pulls events
    /// one at a time via `nextEvent`. Held behind its own mutex so
    /// pull calls don't contend with the `state` mutex `runOnce` /
    /// `keepWaiting` hold during a read loop.
    event_rx: Arc<AsyncMutex<UnboundedReceiver<ReadEvent>>>,
}

struct ShellState {
    shell: Option<Shell>,
    /// True after a runOnce/keepWaiting returned Quiet. Cleared when
    /// Done/Dead lands.
    running: bool,
    /// Flipped by `close()` or a Dead outcome. Methods error after.
    closed: bool,
    /// Sender plumbed into the `Shell`'s `OutputReader` on lazy
    /// spawn. Dropped on `close()` so the receiver sees the channel
    /// terminate and `nextEvent` resolves to `null`.
    event_tx: Option<UnboundedSender<ReadEvent>>,
    /// The first editable root, exported as `$FRANCES_ROOT` at shell
    /// spawn time so subprocesses can discover the project root.
    editable_root: Option<String>,
}

impl<'js, F: WorkflowShell> Trace<'js> for ShellJs<F> {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js, F: WorkflowShell> JsLifetime<'js> for ShellJs<F> {
    type Changed<'to> = ShellJs<F>;
}

impl<'js, F: WorkflowShell> JsClass<'js> for ShellJs<F> {
    const NAME: &'static str = "Shell";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "runOnce",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, ShellJs<F>>>, cmd: String, opts: Opt<Object<'js>>| {
                    let borrow = this.0.borrow();
                    let state = borrow.state.clone();
                    let factory = borrow.factory.clone();
                    drop(borrow);
                    let opts = parse_run_opts(opts);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        ShellOpResult(run_once_inner(&factory, &state, cmd, opts).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "keepWaiting",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, ShellJs<F>>>, opts: Opt<Object<'js>>| {
                    let state = this.0.borrow().state.clone();
                    let wait = parse_wait_opts(opts);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        ShellOpResult(keep_waiting_inner(&state, wait).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "kill",
            Function::new(ctx.clone(), |this: This<Class<'js, ShellJs<F>>>| {
                let state = this.0.borrow().state.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    ShellUnitResult(kill_inner(&state).await)
                }))
            })?,
        )?;

        proto.set(
            "close",
            Function::new(ctx.clone(), |this: This<Class<'js, ShellJs<F>>>| {
                let state = this.0.borrow().state.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    let mut guard = state.lock().await;
                    // Dropping the shell releases its sender; dropping
                    // `event_tx` releases ours. With both gone the
                    // receiver sees the channel closing and a pending
                    // `nextEvent()` resolves to `null`.
                    guard.shell = None;
                    guard.event_tx = None;
                    guard.running = false;
                    guard.closed = true;
                }))
            })?,
        )?;

        proto.set(
            "nextEvent",
            Function::new(ctx.clone(), |this: This<Class<'js, ShellJs<F>>>| {
                let rx = this.0.borrow().event_rx.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    let mut guard = rx.lock().await;
                    NextEventResult(guard.recv().await)
                }))
            })?,
        )?;

        proto.set(
            "isRunning",
            Function::new(ctx.clone(), |this: This<Class<'js, ShellJs<F>>>| {
                let state = this.0.borrow().state.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    let guard = state.lock().await;
                    guard.running
                }))
            })?,
        )?;

        proto.set(
            "setVar",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, ShellJs<F>>>,
                 name: String,
                 value: String,
                 exported: bool| {
                    let borrow = this.0.borrow();
                    let state = borrow.state.clone();
                    let factory = borrow.factory.clone();
                    drop(borrow);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        ShellUnitResult(
                            set_var_inner(&factory, &state, name, value, exported).await,
                        )
                    }))
                },
            )?,
        )?;

        proto.set(
            "captureVar",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, ShellJs<F>>>, name: String| {
                    let borrow = this.0.borrow();
                    let state = borrow.state.clone();
                    let factory = borrow.factory.clone();
                    drop(borrow);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        ShellStringResult(capture_var_inner(&factory, &state, name).await)
                    }))
                },
            )?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Failures from the shell bridge's `*_inner` ops. Typed to the `IntoJs`
/// boundary, where it renders via `Display` into a JS exception.
#[derive(Debug, thiserror::Error)]
enum ShellToolError {
    #[error("shell is closed")]
    Closed,
    #[error("shell is busy; call keepWaiting or kill before issuing a new command")]
    Busy,
    #[error("no command in flight; call runOnce first")]
    NoCommandInFlight,
    #[error("shell handle gone")]
    HandleGone,
    #[error("spawn bash: {0}")]
    Spawn(#[source] ShellError),
    #[error("run: {0}")]
    Run(#[source] ShellError),
    #[error("keep_waiting: {0}")]
    KeepWaiting(#[source] ShellError),
    #[error("kill: {0}")]
    Kill(#[source] ShellError),
    #[error("tempfile: {0}")]
    Tempfile(#[source] io::Error),
    #[error("write tempfile: {0}")]
    WriteTempfile(#[source] io::Error),
    #[error("flush tempfile: {0}")]
    FlushTempfile(#[source] io::Error),
    #[error("read captured tempfile: {0}")]
    ReadCaptured(#[source] io::Error),
    #[error("{action} {name}: exit {exit}\n{output}")]
    SetVarFailed {
        action: &'static str,
        name: String,
        exit: i32,
        output: String,
    },
    #[error("capture {name}: unset or expansion failed (exit {exit})\n{output}")]
    CaptureFailed {
        name: String,
        exit: i32,
        output: String,
    },
    #[error("command went quiet (export/capture expect immediate Done):\n{output}")]
    WentQuiet { output: String },
    #[error("shell died:\n{output}")]
    Died { output: String },
    #[error("empty bash name")]
    EmptyBashName,
    #[error("invalid bash name: {0:?}")]
    InvalidBashName(String),
}

/// The shell tool's wall-clock ceiling when the model doesn't set `max`.
/// Unlike `frances_shell` (which leaves `max` unbounded by default), the
/// model-facing tool always applies a backstop so a chatty-but-endless
/// command (a streaming build, `tail -f`, a dev server) can't hold the
/// session forever — `quiet` never trips while output flows.
const DEFAULT_MAX: Duration = Duration::from_secs(120);
/// Smallest gap kept between the effective `quiet` and `max`. `max` is a
/// wall-clock backstop, so it must sit past `quiet` or it would pre-empt
/// the silence window. Too-small a `max` is clamped up, never rejected.
const MAX_MARGIN: Duration = Duration::from_secs(10);

/// Convert a JS-supplied number of seconds into a `Duration`. Non-finite
/// or negative values are treated as "not set" (`None`).
fn secs_to_duration(secs: f64) -> Option<Duration> {
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// Accepted JS options for `Shell.runOnce`.
///
/// `stdin` and `persist` are parsed at this boundary now so the JS API shape is
/// explicit. The lower-level shell engine consumes them in the quasi-persistent
/// execution refactor.
struct RunOpts {
    wait: WaitOpts,
    stdin: Option<String>,
    persist: Vec<String>,
}

/// Resolve the model-facing `{ quiet, max }` (seconds, both optional) into
/// the concrete `WaitOpts` the shell runs with. Quiet falls back to
/// `frances_shell::DEFAULT_QUIET`, max to [`DEFAULT_MAX`], and max is
/// clamped up to at least `quiet + MAX_MARGIN` so the ceiling can never
/// fire before the silence window.
fn parse_wait_opts(opts: Opt<Object<'_>>) -> WaitOpts {
    parse_wait_opts_from_obj(opts.0.as_ref())
}

fn parse_run_opts(opts: Opt<Object<'_>>) -> RunOpts {
    let obj = opts.0;
    let stdin = obj
        .as_ref()
        .and_then(|o| o.get::<_, Option<String>>("stdin").ok().flatten());
    let persist = obj
        .as_ref()
        .and_then(|o| o.get::<_, Option<Vec<String>>>("persist").ok().flatten())
        .unwrap_or_default();
    RunOpts {
        wait: parse_wait_opts_from_obj(obj.as_ref()),
        stdin,
        persist,
    }
}

fn parse_wait_opts_from_obj(obj: Option<&Object<'_>>) -> WaitOpts {
    let (mut quiet, mut max) = (None, None);
    if let Some(obj) = obj {
        if let Ok(Some(q)) = obj.get::<_, Option<f64>>("quiet") {
            quiet = secs_to_duration(q);
        }
        if let Ok(Some(m)) = obj.get::<_, Option<f64>>("max") {
            max = secs_to_duration(m);
        }
    }
    let quiet = quiet.unwrap_or(frances_shell::DEFAULT_QUIET);
    let max = max.unwrap_or(DEFAULT_MAX).max(quiet + MAX_MARGIN);
    WaitOpts {
        quiet: Some(quiet),
        max: Some(max),
    }
}

async fn run_once_inner<F: WorkflowShell>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    cmd: String,
    opts: RunOpts,
) -> Result<Outcome, ShellToolError> {
    let RunOpts {
        wait,
        stdin,
        persist,
    } = opts;
    let mut guard = state.lock().await;
    if guard.closed {
        return Err(ShellToolError::Closed);
    }
    if guard.running {
        return Err(ShellToolError::Busy);
    }
    ensure_shell(&mut guard, factory).await?;
    let outcome = {
        let shell = guard.shell.as_mut().expect("shell is Some");
        shell
            .run_with_opts(
                &cmd,
                ShellRunOpts {
                    stdin: stdin.map(String::into_bytes),
                    persist,
                },
                wait,
            )
            .await
            .map_err(ShellToolError::Run)?
    };
    Ok(absorb_outcome(&mut guard, outcome))
}

/// Lazy-spawn helper: on first use, ask the factory for a fresh
/// `Shell`, attach the per-`ShellJs` output sink so streaming pipes
/// through, and stash it on `guard`. No-op when a shell already exists.
async fn ensure_shell<F: WorkflowShell>(
    guard: &mut tokio::sync::MutexGuard<'_, ShellState>,
    factory: &F,
) -> Result<(), ShellToolError> {
    if guard.shell.is_some() {
        return Ok(());
    }
    let env = guard
        .editable_root
        .as_ref()
        .map(|root| vec![("FRANCES_ROOT".into(), root.clone().into())])
        .unwrap_or_default();
    let mut shell = factory
        .spawn(ShellOptions {
            env,
            ..ShellOptions::default()
        })
        .await
        .map_err(ShellToolError::Spawn)?;
    if let Some(tx) = guard.event_tx.as_ref() {
        shell.set_output_sink(Some(tx.clone()));
    }
    guard.shell = Some(shell);

    Ok(())
}

async fn keep_waiting_inner(
    state: &Arc<AsyncMutex<ShellState>>,
    wait: WaitOpts,
) -> Result<Outcome, ShellToolError> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err(ShellToolError::Closed);
    }
    if !guard.running {
        return Err(ShellToolError::NoCommandInFlight);
    }
    let outcome = {
        let shell = guard.shell.as_mut().ok_or(ShellToolError::HandleGone)?;
        shell
            .keep_waiting(wait)
            .await
            .map_err(ShellToolError::KeepWaiting)?
    };
    Ok(absorb_outcome(&mut guard, outcome))
}

async fn kill_inner(state: &Arc<AsyncMutex<ShellState>>) -> Result<(), ShellToolError> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err(ShellToolError::Closed);
    }
    if !guard.running {
        return Ok(());
    }
    let shell = guard.shell.as_mut().ok_or(ShellToolError::HandleGone)?;
    shell.kill_running().await.map_err(ShellToolError::Kill)
}

/// Bridge a Frances variable into bash via the `name=$(cat tmpfile)`
/// trick. With `exported=true` the assignment is prefixed `export` so
/// subprocesses inherit it; otherwise it's a plain shell variable
/// (current bash session only). The temp file goes through RAII drop
/// after the bash run settles. Caller must have already coerced
/// `value` into a string (raw for string values, JSON for everything
/// else).
async fn set_var_inner<F: WorkflowShell>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    name: String,
    value: String,
    exported: bool,
) -> Result<(), ShellToolError> {
    validate_bash_name(&name)?;
    let mut tmp = tempfile::NamedTempFile::new().map_err(ShellToolError::Tempfile)?;
    tmp.write_all(value.as_bytes())
        .map_err(ShellToolError::WriteTempfile)?;
    tmp.flush().map_err(ShellToolError::FlushTempfile)?;
    let prefix = if exported { "export " } else { "" };
    let cmd = format!(
        "{prefix}{name}=$(cat {})",
        shell_quote(tmp.path().to_string_lossy().as_ref()),
    );
    let (exit, output) = run_to_done(factory, state, &cmd).await?;
    if exit != 0 {
        let action = if exported { "export" } else { "set" };
        return Err(ShellToolError::SetVarFailed {
            action,
            name,
            exit,
            output,
        });
    }
    // `tmp` drops here — file is removed.
    drop(tmp);
    Ok(())
}

/// Bridge a bash variable into Frances by having bash `printf` the
/// value into a temp file we own, then reading it back. Runs inside a
/// `set -u` subshell so the expansion of `"$name"` fails fast if the
/// bash var is unset — otherwise bash would silently treat unset as
/// empty and we'd store `""` indistinguishably from a real empty
/// value. The subshell scopes the option change so the persistent
/// shell's settings aren't disturbed.
async fn capture_var_inner<F: WorkflowShell>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    name: String,
) -> Result<String, ShellToolError> {
    validate_bash_name(&name)?;
    let tmp = tempfile::NamedTempFile::new().map_err(ShellToolError::Tempfile)?;
    let cmd = format!(
        "( set -u; printf '%s' \"${name}\" > {} )",
        shell_quote(tmp.path().to_string_lossy().as_ref()),
    );
    let (exit, output) = run_to_done(factory, state, &cmd).await?;
    if exit != 0 {
        return Err(ShellToolError::CaptureFailed { name, exit, output });
    }
    let captured = fs::read_to_string(tmp.path()).map_err(ShellToolError::ReadCaptured)?;
    drop(tmp);
    Ok(captured)
}

/// Issue a one-shot bash command and require a `Done` outcome. Used
/// by export/capture, both of which run short deterministic commands
/// where Quiet would be a tool-side bug (an infinite-output trap or equivalent).
async fn run_to_done<F: WorkflowShell>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    cmd: &str,
) -> Result<(i32, String), ShellToolError> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err(ShellToolError::Closed);
    }
    if guard.running {
        return Err(ShellToolError::Busy);
    }
    ensure_shell(&mut guard, factory).await?;
    let outcome = {
        let shell = guard.shell.as_mut().expect("shell is Some");
        shell
            .run(cmd, WaitOpts::default())
            .await
            .map_err(ShellToolError::Run)?
    };
    let absorbed = absorb_outcome(&mut guard, outcome);
    match absorbed {
        Outcome::Done { exit_code, output } => Ok((exit_code, output)),
        Outcome::Quiet { output, .. } => Err(ShellToolError::WentQuiet { output }),
        Outcome::Dead { output } => Err(ShellToolError::Died { output }),
    }
}

/// Reject anything that isn't a plain bash identifier. The name lands
/// inside `export <name>=…` and `"$<name>"` unquoted, so this is the
/// single trust boundary against shell injection.
fn validate_bash_name(name: &str) -> Result<(), ShellToolError> {
    let mut chars = name.chars();
    let first = chars.next().ok_or(ShellToolError::EmptyBashName)?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(ShellToolError::InvalidBashName(name.to_owned()));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(ShellToolError::InvalidBashName(name.to_owned()));
        }
    }
    Ok(())
}

/// Wrap a path in single quotes for bash, doubling any embedded
/// single-quote via `'\''`. TMPDIR is user-controlled so the input
/// cannot be trusted to be quote-free.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn absorb_outcome(state: &mut ShellState, outcome: RunOutcome) -> Outcome {
    match outcome {
        RunOutcome::Done { exit_code, output } => {
            state.running = false;
            Outcome::Done { exit_code, output }
        }
        RunOutcome::Quiet { output, reason } => {
            state.running = true;
            Outcome::Quiet {
                output,
                reason: quiet_reason_str(reason),
            }
        }
        RunOutcome::Dead { output } => {
            state.running = false;
            state.shell = None;
            state.closed = true;
            Outcome::Dead { output }
        }
    }
}

enum Outcome {
    Done {
        exit_code: i32,
        output: String,
    },
    Quiet {
        output: String,
        reason: &'static str,
    },
    Dead {
        output: String,
    },
}

impl<'js> IntoJs<'js> for Outcome {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        match self {
            Outcome::Done { exit_code, output } => {
                obj.set("kind", "done")?;
                obj.set("exit_code", exit_code)?;
                obj.set("output", output)?;
            }
            Outcome::Quiet { output, reason } => {
                obj.set("kind", "quiet")?;
                obj.set("output", output)?;
                obj.set("reason", reason)?;
            }
            Outcome::Dead { output } => {
                obj.set("kind", "dead")?;
                obj.set("output", output)?;
            }
        }
        Ok(obj.into_value())
    }
}

/// Promise-payload that resolves to the outcome or rejects with the
/// error string.
struct ShellOpResult(Result<Outcome, ShellToolError>);

impl<'js> IntoJs<'js> for ShellOpResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(outcome) => outcome.into_js(ctx),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

struct ShellUnitResult(Result<(), ShellToolError>);

impl<'js> IntoJs<'js> for ShellUnitResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

struct ShellStringResult(Result<String, ShellToolError>);

impl<'js> IntoJs<'js> for ShellStringResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(s) => s.into_js(ctx),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

/// Stable string form of [`QuietReason`] used on the JS wire.
fn quiet_reason_str(reason: QuietReason) -> &'static str {
    match reason {
        QuietReason::NoOutput => "no_output",
        QuietReason::MaxElapsed => "max_elapsed",
    }
}

/// Wire form of one `nextEvent` resolution. `None` (all senders gone)
/// maps to JS `null`. Otherwise:
/// - `Output(bytes)` → `{ kind: "output", data: string }`
/// - `Quiet { reason }` → `{ kind: "quiet", reason: "no_output" | "max_elapsed" }`
/// - `Done { exit_code }` → `{ kind: "done", exit_code }`
/// - `Dead` → `{ kind: "dead" }`
struct NextEventResult(Option<ReadEvent>);

impl<'js> IntoJs<'js> for NextEventResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let Some(event) = self.0 else {
            return Ok(Value::new_null(ctx.clone()));
        };
        let obj = Object::new(ctx.clone())?;
        match event {
            ReadEvent::Output(bytes) => {
                obj.set("kind", "output")?;
                obj.set("data", String::from_utf8_lossy(&bytes).into_owned())?;
            }
            ReadEvent::Quiet { reason } => {
                obj.set("kind", "quiet")?;
                obj.set("reason", quiet_reason_str(reason))?;
            }
            ReadEvent::Done { exit_code } => {
                obj.set("kind", "done")?;
                obj.set("exit_code", exit_code)?;
            }
            ReadEvent::Dead => {
                obj.set("kind", "dead")?;
            }
        }
        Ok(obj.into_value())
    }
}
