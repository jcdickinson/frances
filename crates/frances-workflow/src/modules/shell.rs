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
//!
//! The wait/quiet thresholds aren't exposed yet — `runOnce` /
//! `keepWaiting` use `frances_shell::WaitOpts::default()` (1s of output
//! silence, no max ceiling). Workflows that need different pacing can
//! layer a Timer + Promise.race on top.

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

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
