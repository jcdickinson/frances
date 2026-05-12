//! Virtual modules exposed to workflow scripts.
//!
//! Two families:
//!
//! - `frances:v1/*` — the workflow host API. Per-invocation values
//!   (channels, flags) flow through a transient stash on `globalThis`;
//!   each module's body captures them into its local scope and the
//!   stash is then deleted, so user scripts can't reach it.
//! - `whatwg:*` — vendored polyfills under `modules/whatwg/`,
//!   embedded via `include_str!` and refreshed by `update.sh`. They
//!   used to be pure JS with no per-invocation state; today
//!   `whatwg:abortcontroller` also captures `_setSleep` from the
//!   stash, so the stash must be live when whatwg modules evaluate.
//!
//! Install flow (orchestrated by the runtime):
//!
//! 1. `install_stash` — build per-invocation host values, place them on
//!    `globalThis.__frances_v1_stash__`.
//! 2. `install_whatwg` — declare + eval `whatwg:*`. The polyfills that
//!    need stash entries grab them into their module scope here.
//! 3. `install_v1_modules` — declare + eval `frances:v1/*`. Each module
//!    body destructures `globalThis.__frances_v1_stash__` to capture the
//!    slots it needs into module scope.
//! 4. `remove_stash` — delete the stash from `globalThis` so user
//!    scripts can't reach it.
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
//! - `frances:v1/chat`           — `ChatSession` (LLM access).
//! - `frances:v1/io`             — `Timer` + `TimerError`. The user-facing
//!   surface is pure JS in `js/io.js`; Rust exposes a private sleep
//!   primitive (`_setSleep` / `_clearSleep`) on the install-time stash
//!   that the JS wrapper composes against.
//! - `whatwg:web-streams`        — `ReadableStream`, `WritableStream`,
//!   `TransformStream` and friends from web-streams-polyfill.
//! - `whatwg:abortcontroller`    — `AbortController`, `AbortSignal`
//!   (hand-rolled, EventTarget-free). `AbortSignal.timeout` builds on
//!   the stash's sleep primitive.
//! - `whatwg:dom`                — minimal DOM Standard surface
//!   (currently just `DOMException`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rquickjs::module::Module;
use rquickjs::{CatchResultExt, Ctx, Object};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::WorkflowError;
use crate::deps::WorkflowDeps;
use crate::runtime::{HostFrame, UserInput, caught};

pub mod chat;
pub mod file;
pub mod frames;
pub mod inbox;
pub mod io;
pub mod shell;
pub mod workflow;

/// Global key on `globalThis` where the module stash lives during
/// install. Deleted after every virtual module has captured its slot,
/// so user scripts can't reach it via `globalThis`.
const STASH_KEY: &str = "__frances_v1_stash__";

/// Per-invocation host state that gets stashed for module bodies to
/// capture. Bundled into a single struct so the install call site
/// stays readable.
pub(crate) struct V1HostState<D: WorkflowDeps> {
    pub frames_tx: UnboundedSender<HostFrame>,
    pub input_rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    pub closed: Arc<AtomicBool>,
    pub closed_notify: Arc<Notify>,
    pub parked: Arc<Notify>,
    pub deps: D,
}

/// Builds the install-time stash and places it at
/// `globalThis.__frances_v1_stash__`. Must run before any virtual
/// module is evaluated; the matching `remove_stash` must run once
/// every module has captured its slots.
pub(crate) fn install_stash<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    host: V1HostState<D>,
) -> Result<(), WorkflowError> {
    let V1HostState {
        frames_tx,
        input_rx,
        closed,
        closed_notify,
        parked,
        deps,
    } = host;

    let stash = Object::new(ctx.clone())?;

    let exit_fn = workflow::build_exit(ctx, closed.clone(), closed_notify.clone())?;
    stash.set("exit", exit_fn)?;

    let inbox_instance =
        inbox::build_inbox(ctx, input_rx, closed.clone(), closed_notify.clone(), parked)?;
    stash.set("inbox", inbox_instance)?;

    let (transcript_proxy, md_ctor, err_ctor, json_ctor) = frames::build_frames(ctx, frames_tx)?;
    stash.set("transcript", transcript_proxy)?;
    stash.set("MarkdownFrame", md_ctor)?;
    stash.set("ErrorFrame", err_ctor)?;
    stash.set("JsonFrame", json_ctor)?;

    let (chat_ctor, chat_inner_stream) = chat::build_chat_session_ctor(ctx, deps.clone())?;
    stash.set("ChatSession", chat_ctor)?;
    stash.set("__chat_inner_stream", chat_inner_stream)?;

    let shell_ctor = shell::build_shell_ctor(ctx, deps.clone())?;
    stash.set("Shell", shell_ctor)?;

    let editor_ctor = file::build_editor_ctor(ctx, deps)?;
    stash.set("Editor", editor_ctor)?;

    let (set_sleep, clear_sleep) = io::build_sleep_primitives(ctx, closed, closed_notify)?;
    stash.set("_setSleep", set_sleep)?;
    stash.set("_clearSleep", clear_sleep)?;

    ctx.globals().set(STASH_KEY, stash)?;
    Ok(())
}

/// Declares the `whatwg:*` polyfill modules. Evaluation order matters:
/// `whatwg:abortcontroller` imports `DOMException` from `whatwg:dom`,
/// and captures `_setSleep` from the install stash (which must already
/// be live).
pub(crate) fn install_whatwg<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    declare_and_eval(ctx, "whatwg:dom", DOM_SRC)?;
    declare_and_eval(ctx, "whatwg:web-streams", WEB_STREAMS_SRC)?;
    declare_and_eval(ctx, "whatwg:abortcontroller", ABORTCONTROLLER_SRC)?;
    Ok(())
}

/// Declares + evaluates the `frances:v1/*` virtual modules. The stash
/// must already be live so each module body can `const __s =
/// globalThis.__frances_v1_stash__` and capture its slots.
pub(crate) fn install_v1_modules<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    declare_and_eval(ctx, "frances:v1/workflow", WORKFLOW_SRC)?;
    declare_and_eval(ctx, "frances:v1/inbox", INBOX_SRC)?;
    declare_and_eval(ctx, "frances:v1/frames", FRAMES_SRC)?;
    declare_and_eval(ctx, "frances:v1/chat", CHAT_SRC)?;
    declare_and_eval(ctx, "frances:v1/io", IO_SRC)?;
    declare_and_eval(ctx, "frances:v1/tools/shell", SHELL_SRC)?;
    declare_and_eval(ctx, "frances:v1/tools/file", FILE_SRC)?;
    Ok(())
}

/// Removes the install-time stash from `globalThis`. Must run after
/// every virtual module has been evaluated and captured its slots,
/// otherwise the bindings inside module scope become stale lookups.
pub(crate) fn remove_stash<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    ctx.globals().remove(STASH_KEY)?;
    Ok(())
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
        .map_err(caught(format!("declare {name}")))?;
    let (evaluated, _promise) = module
        .eval()
        .catch(ctx)
        .map_err(caught(format!("eval {name}")))?;
    Ok(evaluated)
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
const SHELL_SRC: &str = include_str!("js/shell.js");
const FILE_SRC: &str = include_str!("js/file.js");

// `whatwg:*` polyfills live at the workspace root so they can be
// refreshed by `modules/whatwg/update.sh` without touching this crate.
const DOM_SRC: &str = include_str!("../../../../modules/whatwg/dom.mjs");
const WEB_STREAMS_SRC: &str = include_str!("../../../../modules/whatwg/web-streams.mjs");
const ABORTCONTROLLER_SRC: &str = include_str!("../../../../modules/whatwg/abortcontroller.mjs");
