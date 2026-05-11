//! Virtual modules exposed to workflow scripts under `frances:v1/*`.
//!
//! Each invocation gets a fresh QuickJS context (`AsyncContext::full`).
//! Before evaluating the user script, we build the per-invocation host
//! values (a `Function` for `exit`, a `Class` instance for `inbox`, the
//! transcript proxy, the frame-class constructors) and declare a small
//! virtual module for each one. Those modules just re-export the values
//! out of a hidden global stash — keeping all the per-invocation state
//! inside the captured closures, not in any runtime-wide map.
//!
//! The stash is set on `globalThis` only long enough for each virtual
//! module's body to evaluate. After we force-eval the modules (so each
//! module's `const __s = globalThis.__frances_v1_stash__` line runs and
//! captures references into the module's local scope), we delete the
//! key. User scripts can no longer reach the stash through
//! `globalThis`. The exported names are still reachable the proper
//! way, via `import` statements.
//!
//! Module source files live as siblings under `js/`. They're embedded
//! via `include_str!` at compile time; the Rust here just orchestrates
//! the install order and the two-phase TimerError handoff.
//!
//! Modules:
//!
//! - `frances:v1/workflow` — `exit` lifecycle function.
//! - `frances:v1/inbox`    — `inbox` async-iterable user-input stream.
//! - `frances:v1/frames`   — `transcript`, `MarkdownFrame`, `ErrorFrame`,
//!   `JsonFrame` (frame-objects-with-history API).
//! - `frances:v1/chat`     — `ChatSession` (LLM access). Constructor
//!   currently throws — see `chat.rs`.
//! - `frances:v1/io`       — `Timer` + `TimerError` (interval + one-shot
//!   awaitable + the Error subclass it rejects with).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rquickjs::function::Constructor;
use rquickjs::module::Module;
use rquickjs::{CatchResultExt, Ctx, JsLifetime, Object, Persistent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::WorkflowError;
use crate::runtime::{HostFrame, UserInput};

pub mod chat;
pub mod frames;
pub mod inbox;
pub mod io;
pub mod workflow;

/// Global key on `globalThis` where the module stash lives during
/// install. Deleted after every virtual module has captured its slot,
/// so user scripts can't reach it via `globalThis`.
const STASH_KEY: &str = "__frances_v1_stash__";

/// Userdata wrapper for the per-invocation `TimerError` constructor.
/// Stored via `Ctx::store_userdata` so the Timer reject path can
/// retrieve it without exposing anything to user JS. **Must be
/// removed via `cleanup_v1` before `AsyncContext` drops** — the
/// underlying `Persistent` holds a JS value, and dropping it after
/// the runtime starts tearing down aborts the process.
pub(crate) struct TimerErrorUserData(pub(crate) Persistent<Constructor<'static>>);

unsafe impl<'js> JsLifetime<'js> for TimerErrorUserData {
    type Changed<'to> = TimerErrorUserData;
}

/// Wires the `frances:v1/*` virtual modules into `ctx`. Builds the
/// per-invocation host values (closures over the channels/flags),
/// stashes them on `globalThis`, declares + evaluates each virtual
/// module, and finally deletes the stash.
pub(crate) fn install_v1<'js, D: crate::deps::WorkflowDeps>(
    ctx: &Ctx<'js>,
    frames_tx: UnboundedSender<HostFrame>,
    input_rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
    deps: D,
) -> Result<(), WorkflowError> {
    let stash = Object::new(ctx.clone()).map_err(script)?;

    let exit_fn =
        workflow::build_exit(ctx, closed.clone(), closed_notify.clone()).map_err(script)?;
    stash.set("exit", exit_fn).map_err(script)?;

    let inbox_instance =
        inbox::build_inbox(ctx, input_rx, closed.clone(), closed_notify.clone(), parked)
            .map_err(script)?;
    stash.set("inbox", inbox_instance).map_err(script)?;

    let (transcript_proxy, md_ctor, err_ctor, json_ctor) =
        frames::build_frames(ctx, frames_tx).map_err(script)?;
    stash.set("transcript", transcript_proxy).map_err(script)?;
    stash.set("MarkdownFrame", md_ctor).map_err(script)?;
    stash.set("ErrorFrame", err_ctor).map_err(script)?;
    stash.set("JsonFrame", json_ctor).map_err(script)?;

    let chat_ctor = chat::build_chat_session_ctor(ctx, deps).map_err(script)?;
    stash.set("ChatSession", chat_ctor).map_err(script)?;

    let timer_ctor = io::build_timer_ctor(ctx, closed, closed_notify).map_err(script)?;
    stash.set("Timer", timer_ctor).map_err(script)?;

    ctx.globals().set(STASH_KEY, stash).map_err(script)?;

    // Declare and evaluate each virtual module. Evaluation runs the
    // module body, which captures the stash references into each
    // module's local `__s` binding. After all modules are evaluated
    // we delete the stash from globalThis.
    declare_and_eval(ctx, "frances:v1/workflow", WORKFLOW_SRC)?;
    declare_and_eval(ctx, "frances:v1/inbox", INBOX_SRC)?;
    declare_and_eval(ctx, "frances:v1/frames", FRAMES_SRC)?;
    declare_and_eval(ctx, "frances:v1/chat", CHAT_SRC)?;
    let io_module = declare_and_eval(ctx, "frances:v1/io", IO_SRC)?;

    // Stash the `TimerError` constructor on the Ctx so the Timer
    // reject path can look it up. We use `Ctx::store_userdata`
    // rather than a JS global so it's invisible to user code; the
    // matching `cleanup_v1` must run before the context drops so the
    // underlying `Persistent` is released safely.
    let io_namespace = io_module
        .namespace()
        .catch(ctx)
        .map_err(|e| WorkflowError::Script(format!("frances:v1/io namespace: {e}")))?;
    let timer_error: Constructor<'js> = io_namespace
        .get("TimerError")
        .catch(ctx)
        .map_err(|e| WorkflowError::Script(format!("frances:v1/io.TimerError: {e}")))?;
    let _ = ctx.store_userdata(TimerErrorUserData(Persistent::save(ctx, timer_error)));

    ctx.globals().remove(STASH_KEY).map_err(script)?;

    Ok(())
}

/// Teardown counterpart to `install_v1`. Must be called inside the
/// same `async_with!` block as `install_v1`, before the closure ends
/// and the context drops. Dropping the `TimerErrorUserData` from
/// `userdata` here ensures the contained `Persistent` is released
/// while the JS context is still alive — if it slipped to runtime
/// drop, `JS_FreeRuntime` would abort with `list_empty` failing.
pub(crate) fn cleanup_v1<'js>(ctx: &Ctx<'js>) {
    let _ = ctx.remove_userdata::<TimerErrorUserData>();
}

/// Declares and evaluates a virtual module. Our modules are
/// synchronous (no top-level await) so the promise from `eval` is
/// fulfilled immediately and we can discard it — the export bindings
/// are valid as soon as `eval` returns.
fn declare_and_eval<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    source: &str,
) -> Result<rquickjs::module::Module<'js, rquickjs::module::Evaluated>, WorkflowError> {
    let module = Module::declare(ctx.clone(), name, source)
        .catch(ctx)
        .map_err(|e| WorkflowError::Script(format!("declare {name}: {e}")))?;
    let (evaluated, _promise) = module
        .eval()
        .catch(ctx)
        .map_err(|e| WorkflowError::Script(format!("eval {name}: {e}")))?;
    Ok(evaluated)
}

fn script<E: std::fmt::Display>(err: E) -> WorkflowError {
    WorkflowError::Script(err.to_string())
}

// ---- Module source strings ------------------------------------------------
//
// Each module just re-exports its slot from the stash. The stash is
// dropped from globalThis after all modules evaluate, but by then the
// module bodies have already captured the values via the `const __s = …`
// binding.

const WORKFLOW_SRC: &str = include_str!("js/workflow.js");
const INBOX_SRC: &str = include_str!("js/inbox.js");
const FRAMES_SRC: &str = include_str!("js/frames.js");
const CHAT_SRC: &str = include_str!("js/chat.js");
const IO_SRC: &str = include_str!("js/io.js");
