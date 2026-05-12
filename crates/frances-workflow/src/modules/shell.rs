//! `frances:v1/tools/shell` — bash primitive for workflow tool handlers.
//!
//! `new Shell()` represents one long-lived bash subprocess. Spawning is
//! lazy (deferred until the first `runOnce` so the JS constructor stays
//! sync), and all in-flight operations serialise on a per-Shell async
//! mutex — parallel tool calls that hit the same Shell queue up instead
//! of racing on the underlying `frances_shell::Shell` (which is
//! `&mut self`).
//!
//! Methods on the JS side:
//!
//! - `runOnce(cmd)` — start a command, await its first stopping point.
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
use std::io::Write;
use std::sync::Arc;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use tokio::sync::Mutex as AsyncMutex;

use frances_shell::{QuietReason, RunOutcome, Shell, ShellOptions, WaitOpts};

use crate::deps::{ShellFactory, WorkflowDeps};

pub(crate) fn build_shell_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<ShellJs<D::ShellFactory>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> JsResult<Class<'js, ShellJs<D::ShellFactory>>> {
            // Construction is sync; the actual `Shell::spawn` is
            // deferred until the first `runOnce` (which is async).
            Class::instance(
                ctx.clone(),
                ShellJs {
                    factory: deps.shell_factory().clone(),
                    state: Arc::new(AsyncMutex::new(ShellState {
                        shell: None,
                        running: false,
                        closed: false,
                    })),
                },
            )
        },
    )
}

pub struct ShellJs<F: ShellFactory> {
    factory: F,
    state: Arc<AsyncMutex<ShellState>>,
}

struct ShellState {
    shell: Option<Shell>,
    /// True after a runOnce/keepWaiting returned Quiet. Cleared when
    /// Done/Dead lands.
    running: bool,
    /// Flipped by `close()` or a Dead outcome. Methods error after.
    closed: bool,
}

impl<'js, F: ShellFactory> Trace<'js> for ShellJs<F> {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js, F: ShellFactory> JsLifetime<'js> for ShellJs<F> {
    type Changed<'to> = ShellJs<F>;
}

impl<'js, F: ShellFactory> JsClass<'js> for ShellJs<F> {
    const NAME: &'static str = "Shell";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "runOnce",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, ShellJs<F>>>, cmd: String| {
                    let borrow = this.0.borrow();
                    let state = borrow.state.clone();
                    let factory = borrow.factory.clone();
                    drop(borrow);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        ShellOpResult(run_once_inner(&factory, &state, cmd).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "keepWaiting",
            Function::new(ctx.clone(), |this: This<Class<'js, ShellJs<F>>>| {
                let state = this.0.borrow().state.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    ShellOpResult(keep_waiting_inner(&state).await)
                }))
            })?,
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
                    guard.shell = None;
                    guard.running = false;
                    guard.closed = true;
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

async fn run_once_inner<F: ShellFactory>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    cmd: String,
) -> Result<Outcome, String> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err("shell is closed".to_owned());
    }
    if guard.running {
        return Err(
            "shell is busy; call keepWaiting or kill before issuing a new command".to_owned(),
        );
    }
    if guard.shell.is_none() {
        let shell = factory
            .spawn(ShellOptions::default())
            .await
            .map_err(|e| format!("spawn bash: {e}"))?;
        guard.shell = Some(shell);
    }
    let outcome = {
        let shell = guard.shell.as_mut().expect("shell is Some");
        shell
            .run(&cmd, WaitOpts::default())
            .await
            .map_err(|e| format!("run: {e}"))?
    };
    Ok(absorb_outcome(&mut guard, outcome))
}

async fn keep_waiting_inner(state: &Arc<AsyncMutex<ShellState>>) -> Result<Outcome, String> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err("shell is closed".to_owned());
    }
    if !guard.running {
        return Err("no command in flight; call runOnce first".to_owned());
    }
    let outcome = {
        let shell = guard
            .shell
            .as_mut()
            .ok_or_else(|| "shell handle gone".to_owned())?;
        shell
            .keep_waiting(WaitOpts::default())
            .await
            .map_err(|e| format!("keep_waiting: {e}"))?
    };
    Ok(absorb_outcome(&mut guard, outcome))
}

async fn kill_inner(state: &Arc<AsyncMutex<ShellState>>) -> Result<(), String> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err("shell is closed".to_owned());
    }
    if !guard.running {
        return Ok(());
    }
    let shell = guard
        .shell
        .as_mut()
        .ok_or_else(|| "shell handle gone".to_owned())?;
    shell.kill_running().await.map_err(|e| format!("kill: {e}"))
}

/// Bridge a Frances variable into bash via the `name=$(cat tmpfile)`
/// trick. With `exported=true` the assignment is prefixed `export` so
/// subprocesses inherit it; otherwise it's a plain shell variable
/// (current bash session only). The temp file goes through RAII drop
/// after the bash run settles. Caller must have already coerced
/// `value` into a string (raw for string values, JSON for everything
/// else).
async fn set_var_inner<F: ShellFactory>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    name: String,
    value: String,
    exported: bool,
) -> Result<(), String> {
    validate_bash_name(&name)?;
    let mut tmp = tempfile::NamedTempFile::new().map_err(|e| format!("tempfile: {e}"))?;
    tmp.write_all(value.as_bytes())
        .map_err(|e| format!("write tempfile: {e}"))?;
    tmp.flush().map_err(|e| format!("flush tempfile: {e}"))?;
    let prefix = if exported { "export " } else { "" };
    let cmd = format!(
        "{prefix}{name}=$(cat {})",
        shell_quote(tmp.path().to_string_lossy().as_ref()),
    );
    let (exit, output) = run_to_done(factory, state, &cmd).await?;
    if exit != 0 {
        let action = if exported { "export" } else { "set" };
        return Err(format!("{action} {name}: exit {exit}\n{output}"));
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
async fn capture_var_inner<F: ShellFactory>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    name: String,
) -> Result<String, String> {
    validate_bash_name(&name)?;
    let tmp = tempfile::NamedTempFile::new().map_err(|e| format!("tempfile: {e}"))?;
    let cmd = format!(
        "( set -u; printf '%s' \"${name}\" > {} )",
        shell_quote(tmp.path().to_string_lossy().as_ref()),
    );
    let (exit, output) = run_to_done(factory, state, &cmd).await?;
    if exit != 0 {
        return Err(format!(
            "capture {name}: unset or expansion failed (exit {exit})\n{output}"
        ));
    }
    let captured =
        fs::read_to_string(tmp.path()).map_err(|e| format!("read captured tempfile: {e}"))?;
    drop(tmp);
    Ok(captured)
}

/// Issue a one-shot bash command and require a `Done` outcome. Used
/// by export/capture, both of which run short deterministic commands
/// where Quiet would be a tool-side bug (an infinite-output trap or
/// equivalent). Re-uses the same closed/busy/spawn checks as
/// `run_once_inner`.
async fn run_to_done<F: ShellFactory>(
    factory: &F,
    state: &Arc<AsyncMutex<ShellState>>,
    cmd: &str,
) -> Result<(i32, String), String> {
    let mut guard = state.lock().await;
    if guard.closed {
        return Err("shell is closed".to_owned());
    }
    if guard.running {
        return Err(
            "shell is busy; call keepWaiting or kill before issuing a new command".to_owned(),
        );
    }
    if guard.shell.is_none() {
        let shell = factory
            .spawn(ShellOptions::default())
            .await
            .map_err(|e| format!("spawn bash: {e}"))?;
        guard.shell = Some(shell);
    }
    let outcome = {
        let shell = guard.shell.as_mut().expect("shell is Some");
        shell
            .run(cmd, WaitOpts::default())
            .await
            .map_err(|e| format!("run: {e}"))?
    };
    let absorbed = absorb_outcome(&mut guard, outcome);
    match absorbed {
        Outcome::Done { exit_code, output } => Ok((exit_code, output)),
        Outcome::Quiet { output, .. } => Err(format!(
            "command went quiet (export/capture expect immediate Done):\n{output}"
        )),
        Outcome::Dead { output } => Err(format!("shell died:\n{output}")),
    }
}

/// Reject anything that isn't a plain bash identifier. The name lands
/// inside `export <name>=…` and `"$<name>"` unquoted, so this is the
/// single trust boundary against shell injection.
fn validate_bash_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first = chars.next().ok_or_else(|| "empty bash name".to_owned())?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!("invalid bash name: {name:?}"));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("invalid bash name: {name:?}"));
        }
    }
    Ok(())
}

/// Wrap a path in single quotes for bash, doubling any embedded
/// single-quote via `'\''`. NamedTempFile paths shouldn't contain
/// quotes in practice, but TMPDIR is user-controlled, so don't trust.
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
                reason: match reason {
                    QuietReason::NoOutput => "no_output",
                    QuietReason::MaxElapsed => "max_elapsed",
                },
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
struct ShellOpResult(Result<Outcome, String>);

impl<'js> IntoJs<'js> for ShellOpResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(outcome) => outcome.into_js(ctx),
            Err(msg) => Err(throw(ctx, &msg)),
        }
    }
}

struct ShellUnitResult(Result<(), String>);

impl<'js> IntoJs<'js> for ShellUnitResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
            Err(msg) => Err(throw(ctx, &msg)),
        }
    }
}

struct ShellStringResult(Result<String, String>);

impl<'js> IntoJs<'js> for ShellStringResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(s) => s.into_js(ctx),
            Err(msg) => Err(throw(ctx, &msg)),
        }
    }
}

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
