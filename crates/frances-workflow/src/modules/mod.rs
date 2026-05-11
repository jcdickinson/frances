//! Virtual modules exposed to workflow scripts.
//!
//! Two families:
//!
//! - `frances:v1/*` — the workflow host API. Per-invocation values
//!   (channels, flags) flow through a transient stash on `globalThis`;
//!   each module's body captures them into its local scope and the
//!   stash is then deleted, so user scripts can't reach it.
//! - `whatwg:*` — vendored polyfills with no per-invocation state.
//!   Pure JS source under `modules/whatwg/` at the workspace root,
//!   embedded via `include_str!` and refreshed by `update.sh`.
//!
//! `frances:v1/*` install flow: build the per-invocation host values,
//! stash them on `globalThis`, declare + evaluate each virtual module
//! (whose body runs `const __s = globalThis.__frances_v1_stash__` and
//! captures the references it needs), then delete the stash. The two
//! exceptions are `TimerError` (a JS class declared inside `io.js`,
//! whose constructor we then stash via `Ctx::store_userdata` for the
//! Rust reject path) and `cleanup_v1`, which must run before the
//! context drops or `JS_FreeRuntime` aborts at `list_empty`.
//!
//! `whatwg:*` install is trivial — just declare and evaluate the
//! module sources. No stash, no userdata, no cleanup.
//!
//! Module source files live as siblings under `js/` (v1) or under
//! `modules/whatwg/` at the workspace root.
//!
//! Modules:
//!
//! - `frances:v1/workflow`       — `exit` lifecycle function.
//! - `frances:v1/inbox`          — `inbox` async-iterable user-input stream.
//! - `frances:v1/frames`         — `transcript`, `MarkdownFrame`, `ErrorFrame`,
//!   `JsonFrame` (frame-objects-with-history API).
//! - `frances:v1/chat`           — `ChatSession` (LLM access). Constructor
//!   currently throws — see `chat.rs`.
//! - `frances:v1/io`             — `Timer` + `TimerError` (interval + one-shot
//!   awaitable + the Error subclass it rejects with).
//! - `whatwg:web-streams`        — `ReadableStream`, `WritableStream`,
//!   `TransformStream` and friends from web-streams-polyfill (the
//!   ponyfill build — named exports, no globalThis mutation).
//! - `whatwg:abortcontroller`    — `AbortController`, `AbortSignal`
//!   (hand-rolled, EventTarget-free).
//! - `whatwg:dom`                — minimal DOM Standard surface
//!   (currently just `DOMException`). Grows on a what-we-need basis;
//!   see `docs/js/whatwg.md`.

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

    let (chat_ctor, chat_inner_stream) =
        chat::build_chat_session_ctor(ctx, deps).map_err(script)?;
    stash.set("ChatSession", chat_ctor).map_err(script)?;
    stash
        .set("__chat_inner_stream", chat_inner_stream)
        .map_err(script)?;

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

/// Declares the `whatwg:*` polyfill modules. No per-invocation state,
/// so this is just `declare_and_eval` for each source. Independent of
/// `install_v1`; the runtime calls both before evaluating the user
/// script.
pub(crate) fn install_whatwg<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    // Order matters: `whatwg:abortcontroller` imports DOMException
    // from `whatwg:dom`, so the dom module must be declared first.
    declare_and_eval(ctx, "whatwg:dom", DOM_SRC)?;
    declare_and_eval(ctx, "whatwg:web-streams", WEB_STREAMS_SRC)?;
    declare_and_eval(ctx, "whatwg:abortcontroller", ABORTCONTROLLER_SRC)?;
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

// `whatwg:*` polyfills live at the workspace root so they can be
// refreshed by `modules/whatwg/update.sh` without touching this crate.
const DOM_SRC: &str = include_str!("../../../../modules/whatwg/dom.mjs");
const WEB_STREAMS_SRC: &str = include_str!("../../../../modules/whatwg/web-streams.mjs");
const ABORTCONTROLLER_SRC: &str = include_str!("../../../../modules/whatwg/abortcontroller.mjs");
