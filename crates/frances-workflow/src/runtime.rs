//! Script runtime.
//!
//! Each call to [`Runtime::start`] creates a fresh [`AsyncContext`],
//! installs the `frances:v1/*` virtual modules, evaluates the user
//! script as its own module, and tears the context down when the
//! script's top-level body settles or `exit()` is called. Module state
//! does not persist across invocations.
//!
//! The JS-side API is exposed exclusively through standard ES module
//! imports — no globals:
//!
//! - `import { exit } from "frances:v1/workflow"`
//! - `import { inbox } from "frances:v1/inbox"`
//! - `import { transcript, MarkdownSection, ErrorSection, JsonSection } from "frances:v1/sections"`
//! - `import { ChatSession } from "frances:v1/chat"` (LLM backend pending)
//! - `import.meta.args` — per-invocation slash-command args.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::Mutex as StdMutex;
use rquickjs::async_with;
use rquickjs::context::AsyncContext;
use rquickjs::function::This;
use rquickjs::module::Module;
use rquickjs::promise::MaybePromise;
use rquickjs::runtime::AsyncRuntime;
use rquickjs::{
    CatchResultExt, Ctx, Function, IntoJs, Object, Persistent, Promise, Result as JsResult, Value,
};
use tokio::runtime::Builder;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio::task::LocalSet;

use crate::WorkflowError;
use crate::deps::WorkflowDeps;
use crate::modules;
use crate::permission::PermissionRequest;
use crate::transpile::{SourceKind, ts_to_js};

/// Internal name we declare the user script under. Distinct from the
/// `frances:v1/*` namespace so the two don't visually clash.
const USER_MODULE_NAME: &str = "frances:user-script";

/// The transcript stream — ordered block-lifecycle deltas from the
/// workflow body. One consumer (the driver's emit path); cross-variant
/// order matters (a `Close` must follow its `Push`), so it's a union enum
/// on its own channel. The host maps these onto the wire `StreamFrame`
/// protocol; this enum is the host-API contract, not the protocol itself.
#[derive(Debug, Clone)]
pub enum SectionTranscript {
    /// Declarative upsert of the frame with the given id. The first
    /// `Set` for an id creates the block (with `frame.seed` as its
    /// initial body, if any); a later `Set` replaces its kind + bounded
    /// metadata — e.g. ShellOutput's state going `Running → Success` —
    /// carrying no body (`seed: None`). Does NOT seal anything: each
    /// frame type chooses when it's done (markdown emits `Close` for its
    /// predecessor before its own `Set`; shell output `Close`s when its
    /// state goes terminal).
    Set { id: SectionId, section: SectionSpec },
    /// Append text to the frame with the given id. Valid for as long
    /// as the frame remains open; the JS side enforces per-frame-type
    /// rules (active-markdown slot, ShellOutput's open flag). The body
    /// grows by delta — a full-value `Set` per chunk would be O(n²).
    Append { id: SectionId, delta: String },
    /// Close the frame with the given id. The host emits a `BlockStop`
    /// and persists the row. Idempotent on unknown ids (the JS side
    /// suppresses double-close).
    Close { id: SectionId },
}

/// Chrome a workflow declares — the `surfaces` output. Declarative
/// Set/Clear, not a stream: a re-`SetFooter` replaces the footer view,
/// `ClearFooter` removes it. Today the only surface is the footer busy
/// indicator; this grows a `Region`/`ViewNode` vocabulary only when a
/// second surface (panel, plan-editor) actually appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceCmd {
    /// Show `text` (with a spinner) in the footer busy indicator.
    SetFooter { text: String },
    /// Hide the footer busy indicator.
    ClearFooter,
}

/// The workflow's typed outputs: a bag of single-consumer channels. The
/// driver selects over the ones it must drive; the rest flow to their own
/// consumers. Asymmetric with the inbox (a union) on purpose — outputs are
/// independent concerns with no cross-channel ordering constraint.
pub struct WorkflowOutputs {
    /// Block-lifecycle stream (persisted to scrollback by the driver).
    pub transcript: UnboundedReceiver<SectionTranscript>,
    /// Chrome the workflow declares (footer busy indicator today). Set
    /// via `setStatus` from `frances:v1/workflow`. Never persisted.
    pub surfaces: UnboundedReceiver<SurfaceCmd>,
    /// Permission asks awaiting a user (or auto-approver) answer.
    pub permissions: UnboundedReceiver<PermissionRequest>,
    /// LLM token-usage telemetry. Side-channel; opens/closes no block and
    /// is never persisted (the TUI drops it during replay).
    pub usage: UnboundedReceiver<frances_models_llm::Usage>,
}

/// Paired senders for [`WorkflowOutputs`], bundled so `V1HostState` stays
/// readable. Cloned per emitter at install time.
#[derive(Clone)]
pub(crate) struct OutputSenders {
    pub transcript: UnboundedSender<SectionTranscript>,
    pub surfaces: UnboundedSender<SurfaceCmd>,
    pub permissions: UnboundedSender<PermissionRequest>,
    pub usage: UnboundedSender<frances_models_llm::Usage>,
}

pub use frances_models_tui::{ReasoningState, SectionId, SectionKind, ShellState, Source};

/// What a [`SectionTranscript::Set`] carries: the section's kind +
/// bounded metadata, plus an optional `seed` — the initial body chunk
/// for text-bodied kinds (Markdown / ShellOutput / Error). One-shot
/// data kinds (ToolUse / Json / Diff) and metadata-only re-`Set`s leave
/// it `None`. The streaming body never lives here; it grows via
/// [`SectionTranscript::Append`].
#[derive(Debug, Clone)]
pub struct SectionSpec {
    pub kind: SectionKind,
    pub seed: Option<String>,
}

/// A single user input event delivered to `inbox`.
#[derive(Debug, Clone)]
pub struct UserInput {
    pub content: String,
}

impl<'js> IntoJs<'js> for UserInput {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("content", self.content)?;
        Ok(obj.into_value())
    }
}

/// An item delivered to the workflow's `inbox` stream. Either a normal
/// user message or an out-of-band interrupt request (Esc in the TUI).
/// The body distinguishes them in JS: `Input` arrives as
/// `{ content }`, `Interrupt` arrives as the registered symbol
/// `Symbol.for("frances.interrupt")` (re-exported as `INTERRUPT` from
/// `frances:v1/inbox`).
#[derive(Debug, Clone)]
pub enum InboxItem {
    Input(UserInput),
    Interrupt,
}

impl<'js> IntoJs<'js> for InboxItem {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self {
            InboxItem::Input(input) => input.into_js(ctx),
            InboxItem::Interrupt => interrupt_symbol(ctx),
        }
    }
}

/// Fetch the process-wide `Symbol.for("frances.interrupt")` from the JS
/// global registry. Both the inbox iterator (yielding interrupts) and
/// the `frances:v1/inbox` module (exporting `INTERRUPT`) resolve the
/// same symbol this way, so `value === INTERRUPT` holds in workflows.
pub(crate) fn interrupt_symbol<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let symbol: Object<'js> = ctx.globals().get("Symbol")?;
    let for_fn: rquickjs::Function<'js> = symbol.get("for")?;
    for_fn.call(("frances.interrupt",))
}

/// Inputs the runtime supplies for one workflow invocation.
#[derive(Default)]
pub struct Invocation {
    pub source_path: PathBuf,
    pub args: Vec<String>,
    /// Stable entity uuid for the workflow — keys into `_migrations` and
    /// the per-runtime `WorkflowDb` cache. [`uuid::Uuid::nil`] when
    /// tests don't care about storage.
    pub entity: uuid::Uuid,
    /// Per-invocation instance uuid exposed to JS as
    /// `import.meta.instance`. The runtime allocates one fresh on a new
    /// push and round-trips it through `workflow_stack` so a restored
    /// instance reads the same value out of `import.meta.instance`.
    /// [`uuid::Uuid::nil`] when tests don't care.
    pub instance_id: uuid::Uuid,
    /// Ready-to-apply migrations, read from disk by the caller. Empty
    /// is fine — workflows without any tables just get an empty handle.
    pub migrations: Vec<frances_storage::Migration>,
}

/// Handle to a running workflow. The session runtime owns this; it delivers user
/// input via [`Self::input_tx`], drains the typed [`Self::outputs`] bag,
/// and learns about termination through [`Self::done`].
pub struct WorkflowHandle {
    /// Send user input (or an interrupt) to the workflow's `inbox`
    /// stream.
    pub input_tx: UnboundedSender<InboxItem>,
    /// Typed output channels the workflow emits on.
    pub outputs: WorkflowOutputs,
    /// Notified each time the body suspends on `inbox.next()` with an
    /// empty queue — i.e. it's parked waiting for input. The continuous
    /// driver doesn't consult this (it completes when the event loop
    /// drains); it's the signal the test harness uses to step a workflow
    /// turn-by-turn, so it's compiled only under test.
    #[cfg(any(test, feature = "test-utils"))]
    pub on_idle: Arc<Notify>,
    /// Resolves when the workflow terminates (body settled or `exit()`
    /// called). The inner result mirrors the body's outcome.
    pub done: oneshot::Receiver<Result<(), WorkflowError>>,
    /// Per-invocation instance uuid — same value the JS body sees as
    /// `import.meta.instance`. Set by [`Runtime::start`] from
    /// [`Invocation::instance_id`].
    pub instance: uuid::Uuid,
    /// Pulsed by [`Self::request_shutdown`] (and by JS `exit()`). The
    /// runtime's completion loop races this against the event loop; on
    /// fire it runs the workflow's shutdown hook then closes the inbox.
    shutdown_notify: Arc<Notify>,
}

impl WorkflowHandle {
    /// Pulse the shutdown signal. The body's
    /// `lifecycle.shutdown` hook fires (if registered), then the inbox
    /// closes; the runtime drains remaining frames and awaits
    /// [`Self::done`] to complete the wind-down. Idempotent — repeated
    /// calls are no-ops because `Notify` only delivers to currently-
    /// registered waiters and the lifecycle IIFE waits exactly once.
    pub fn request_shutdown(&self) {
        self.shutdown_notify.notify_waiters();
    }
}

/// Workflow script runtime. Owns a dedicated OS thread that runs all
/// workflow JS on a single-threaded tokio runtime + [`LocalSet`]: quickjs
/// is single-threaded per runtime anyway, and running the bodies via
/// `spawn_local` lets them hold `!Send` rquickjs handles across `await`.
/// `Runtime` itself holds only a `Send` dispatch channel, so it lives
/// happily on the multi-thread session runtime; cheap to share via `Arc`.
pub struct Runtime<D: WorkflowDeps> {
    start_tx: UnboundedSender<StartRequest>,
    // `D` is consumed by the JS thread, not stored here — marker only.
    _deps: PhantomData<D>,
    // Declared after `start_tx`: on drop the sender closes first, ending
    // the JS thread's loop, then this guard joins the thread.
    _js_thread: JsThreadGuard,
}

/// A request to start a workflow on the JS thread; the resulting handle
/// comes back over `reply`.
struct StartRequest {
    inv: Invocation,
    reply: oneshot::Sender<Result<WorkflowHandle, WorkflowError>>,
}

/// Joins the JS thread when the [`Runtime`] is dropped.
struct JsThreadGuard(Option<std::thread::JoinHandle<()>>);

impl Drop for JsThreadGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
struct TranspileCache {
    /// Source-hash → transpiled JS. xxhash3_64 of the on-disk bytes.
    by_hash: std::collections::HashMap<u64, Arc<str>>,
}

impl<D: WorkflowDeps> Runtime<D> {
    /// Spawn the JS thread and block until it has stood up its
    /// `AsyncRuntime` (so construction errors surface here).
    pub fn new(deps: D) -> Result<Self, WorkflowError> {
        let (start_tx, start_rx) = mpsc::unbounded_channel::<StartRequest>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<Result<(), WorkflowError>>();
        let handle = std::thread::Builder::new()
            .name("frances-workflow-js".to_owned())
            .spawn(move || js_thread_main(deps, start_rx, ack_tx))
            .map_err(WorkflowError::JsThreadSpawn)?;
        match ack_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                start_tx,
                _deps: PhantomData,
                _js_thread: JsThreadGuard(Some(handle)),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(WorkflowError::JsThreadGone),
        }
    }

    /// Start a workflow on the JS thread and return the handle the host
    /// uses to drive it. The JS thread does the source read + transpile,
    /// resolves the per-workflow [`WorkflowDb`](crate::WorkflowDb) (applying migrations on
    /// first touch), and `spawn_local`s the body.
    pub async fn start(&self, inv: Invocation) -> Result<WorkflowHandle, WorkflowError> {
        let (reply, reply_rx) = oneshot::channel();
        self.start_tx
            .send(StartRequest { inv, reply })
            .map_err(|_| WorkflowError::JsThreadGone)?;
        reply_rx.await.map_err(|_| WorkflowError::JsThreadGone)?
    }
}

/// The JS thread body: a current-thread tokio runtime driving a
/// [`LocalSet`] that owns the `AsyncRuntime` and runs every workflow.
fn js_thread_main<D: WorkflowDeps>(
    deps: D,
    mut start_rx: UnboundedReceiver<StartRequest>,
    ack_tx: std::sync::mpsc::Sender<Result<(), WorkflowError>>,
) {
    let rt = match Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(error) => {
            let _ = ack_tx.send(Err(WorkflowError::JsThreadRuntime(error)));
            return;
        }
    };
    let local = LocalSet::new();
    local.block_on(&rt, async move {
        let js = match AsyncRuntime::new() {
            Ok(js) => js,
            Err(error) => {
                let _ = ack_tx.send(Err(WorkflowError::Script(error)));
                return;
            }
        };
        let transpile_cache = StdMutex::new(TranspileCache::default());
        let _ = ack_tx.send(Ok(()));
        while let Some(StartRequest { inv, reply }) = start_rx.recv().await {
            let result = start_impl(&js, &transpile_cache, &deps, inv).await;
            let _ = reply.send(result);
        }
    });
}

/// Build the channels + host state for one workflow and `spawn_local` its
/// body. Runs on the JS thread (inside the `LocalSet`).
async fn start_impl<D: WorkflowDeps>(
    js: &AsyncRuntime,
    transpile_cache: &StdMutex<TranspileCache>,
    deps: &D,
    inv: Invocation,
) -> Result<WorkflowHandle, WorkflowError> {
    let source = std::fs::read_to_string(&inv.source_path).map_err(WorkflowError::ReadSource)?;
    let js_source = match SourceKind::from_path(&inv.source_path) {
        SourceKind::JavaScript => source,
        SourceKind::TypeScript => transpile(transpile_cache, &inv.source_path, &source)?,
    };

    let workflow_db = deps
        .workflow_db(inv.entity, std::borrow::Cow::Borrowed(&inv.migrations))
        .await?;

    let (input_tx, input_rx) = mpsc::unbounded_channel::<InboxItem>();
    let (transcript_tx, transcript_rx) = mpsc::unbounded_channel::<SectionTranscript>();
    let (surfaces_tx, surfaces_rx) = mpsc::unbounded_channel::<SurfaceCmd>();
    let (permissions_tx, permissions_rx) = mpsc::unbounded_channel::<PermissionRequest>();
    let (usage_tx, usage_rx) = mpsc::unbounded_channel::<frances_models_llm::Usage>();
    let (done_tx, done_rx) = oneshot::channel::<Result<(), WorkflowError>>();
    let shutdown_notify = Arc::new(Notify::new());
    #[cfg(any(test, feature = "test-utils"))]
    let on_idle = Arc::new(Notify::new());

    let host = modules::V1HostState {
        senders: OutputSenders {
            transcript: transcript_tx,
            surfaces: surfaces_tx,
            permissions: permissions_tx,
            usage: usage_tx,
        },
        input_rx: Arc::new(AsyncMutex::new(input_rx)),
        closed: Arc::new(AtomicBool::new(false)),
        closed_notify: Arc::new(Notify::new()),
        #[cfg(any(test, feature = "test-utils"))]
        on_idle: on_idle.clone(),
        shutdown_notify: shutdown_notify.clone(),
        deps: deps.clone(),
        workflow_db,
    };
    let task = WorkflowTask {
        js: js.clone(),
        js_source,
        args: inv.args,
        instance_id: inv.instance_id,
        host,
    };

    tokio::task::spawn_local(async move {
        let _ = done_tx.send(run_workflow(task).await);
    });

    Ok(WorkflowHandle {
        input_tx,
        outputs: WorkflowOutputs {
            transcript: transcript_rx,
            surfaces: surfaces_rx,
            permissions: permissions_rx,
            usage: usage_rx,
        },
        #[cfg(any(test, feature = "test-utils"))]
        on_idle,
        done: done_rx,
        instance: inv.instance_id,
        shutdown_notify,
    })
}

fn transpile(
    transpile_cache: &StdMutex<TranspileCache>,
    path: &Path,
    source: &str,
) -> Result<String, WorkflowError> {
    let hash = twox_hash::XxHash3_64::oneshot(source.as_bytes());
    if let Some(cached) = transpile_cache.lock().by_hash.get(&hash).cloned() {
        return Ok(cached.to_string());
    }
    let js = ts_to_js(path, source)?;
    transpile_cache
        .lock()
        .by_hash
        .insert(hash, Arc::<str>::from(js.as_str()));
    Ok(js)
}

/// Everything [`run_workflow`] needs to drive one workflow body, bundled
/// so the spawn site isn't a long positional list. The host-state fields
/// live in [`modules::V1HostState`].
struct WorkflowTask<D: WorkflowDeps> {
    js: AsyncRuntime,
    js_source: String,
    args: Vec<String>,
    instance_id: uuid::Uuid,
    host: modules::V1HostState<D>,
}

async fn run_workflow<D: WorkflowDeps>(task: WorkflowTask<D>) -> Result<(), WorkflowError> {
    let WorkflowTask {
        js,
        js_source,
        args,
        instance_id,
        host,
    } = task;
    let context = AsyncContext::full(&js).await?;
    // Kept for the completion `select!` / inbox close below; `host` itself
    // is moved into phase 1.
    let shutdown = host.shutdown_notify.clone();
    let closed_for_shutdown = host.closed.clone();
    let closed_notify_for_shutdown = host.closed_notify.clone();

    // Phase 1 — install modules, evaluate the body, and hand the body
    // promise + the `lifecycle` object back as `Persistent`s so they stay
    // readable after the event loop drains (a `Promise<'js>` / `Object<'js>`
    // can't cross the `async_with!` boundary, but a `Persistent` held by
    // this `!Send` JS-thread task can). We do NOT await the body here:
    // completion is decided by the loop emptying, not by the body settling,
    // so a body that leaves a bare never-resolving promise still terminates.
    //
    // The install-time stash must be live before either family of modules
    // evaluates. `whatwg:abortcontroller` captures `_setSleep` from it (for
    // `AbortSignal.timeout`), and every `frances:v1/*` module captures its
    // own slots. The whatwg polyfills are declared before v1 because the v1
    // modules import from them (frames uses WritableStream, chat uses
    // Readable/TransformStream).
    let (body, lifecycle) = async_with!(context => |ctx| {
        let result: Result<(Persistent<Promise>, Persistent<Object>), WorkflowError> = async {
            let lifecycle = modules::install_stash(&ctx, host)?;
            modules::install_whatwg(&ctx)?;
            modules::install_v1_modules(&ctx)?;
            modules::remove_stash(&ctx)?;

            let user_module = Module::declare(ctx.clone(), USER_MODULE_NAME, js_source.as_bytes())
                .catch(&ctx)
                .map_err(caught("declare user-script"))?;
            let meta = user_module
                .meta()
                .catch(&ctx)
                .map_err(caught("user-script meta"))?;
            meta.set("args", args)
                .catch(&ctx)
                .map_err(caught("set import.meta.args"))?;
            meta.set("instance", instance_id.to_string())
                .catch(&ctx)
                .map_err(caught("set import.meta.instance"))?;

            let (_module, promise) = user_module
                .eval()
                .catch(&ctx)
                .map_err(caught("eval user-script"))?;
            Ok((Persistent::save(&ctx, promise), Persistent::save(&ctx, lifecycle)))
        }
        .await;

        result
    })
    .await?;

    // Phase 2 — drive jobs and spawned host futures (timers, inbox waits,
    // LLM calls, …) until the loop is empty. A bare `new Promise(() => {})`
    // registers no future, so it doesn't keep the loop alive. Shutdown (a
    // host dehydrate or `exit()`) is just a one-time interjection: run the
    // user's shutdown hook and close the inbox so a parked `inbox.next()`
    // unwinds — then the same `idle()` drains the rest. Normal completion
    // is simply the case where that interjection never fires.
    let shutdown = shutdown.notified();
    tokio::pin!(shutdown);
    shutdown.as_mut().enable();
    let mut shutdown_pending = true;
    loop {
        tokio::select! {
            () = js.idle() => break,
            () = &mut shutdown, if shutdown_pending => {
                shutdown_pending = false;
                run_shutdown_hook(&context, &lifecycle).await?;
                if !closed_for_shutdown.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    closed_notify_for_shutdown.notify_waiters();
                }
            }
        }
    }

    // Phase 3 — report the body's outcome. `result::<()>()` is `None` only
    // if the body promise never settled (a dangling promise the loop
    // emptied around): not an error, just nothing left to do.
    async_with!(context => |ctx| {
        let promise = body
            .restore(&ctx)
            .catch(&ctx)
            .map_err(caught("restore body promise"))?;
        match promise.result::<()>() {
            Some(outcome) => outcome.catch(&ctx).map_err(caught("await user-script promise")),
            None => {
                tracing::warn!(
                    "workflow event loop emptied while the body promise was still pending"
                );
                Ok(())
            }
        }
    })
    .await
}

/// Run the workflow's registered `lifecycle.shutdown` hook to completion
/// (a no-op if it set none). The lifecycle module stashes a runner that
/// wraps the user hook, so this is always a single awaitable.
async fn run_shutdown_hook(
    context: &AsyncContext,
    lifecycle: &Persistent<Object<'static>>,
) -> Result<(), WorkflowError> {
    async_with!(context => |ctx| {
        let result: Result<(), WorkflowError> = async {
            let lifecycle = lifecycle
                .clone()
                .restore(&ctx)
                .catch(&ctx)
                .map_err(caught("restore lifecycle"))?;
            // `null` ⇒ no hook registered; anything else must be callable.
            let hook: Option<Function> = lifecycle
                .get("shutdown")
                .catch(&ctx)
                .map_err(caught("read lifecycle.shutdown"))?;
            if let Some(hook) = hook {
                // Best-effort, like the old JS `try/catch`: run the hook
                // (sync or async — `MaybePromise` handles both) and log on
                // failure rather than failing the whole workflow.
                let outcome = async {
                    let ret: MaybePromise = hook.call((This(lifecycle.clone()),))?;
                    ret.into_future::<()>().await
                }
                .await
                .catch(&ctx);
                if let Err(error) = outcome {
                    tracing::warn!("workflow shutdown hook errored: {error}");
                }
            }
            Ok(())
        }
        .await;
        result
    })
    .await
}

pub(crate) fn caught<'js>(
    context: impl Into<String>,
) -> impl FnOnce(rquickjs::CaughtError<'js>) -> WorkflowError {
    let context = context.into();
    move |e| WorkflowError::ScriptCaught {
        context,
        detail: e.to_string(),
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub mod test_drive {
    //! Shared workflow-driving helper for this crate's unit tests and the
    //! `tests/` integration suites. Drives a body until it parks on
    //! `inbox.next()` or terminates, collecting the transcript it emits.
    //!
    //! Only the transcript is collected — it's the ordered stream tests
    //! assert against. The other outputs (surfaces / permissions / usage)
    //! buffer on their own channels; the few tests that care read them
    //! directly off [`WorkflowHandle::outputs`].
    use super::{SectionTranscript, WorkflowError, WorkflowHandle};

    /// Hard ceiling on how long an individual cycle is allowed to run.
    /// Real workflow turns are interactive (a body can wait for input
    /// indefinitely); in tests, anything past a few seconds is a bug.
    /// Panicking with a clear message beats a hung test process.
    pub const CYCLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Drives a workflow until it parks on `inbox.next()` or terminates.
    /// Panics if `CYCLE_TIMEOUT` is exceeded so tests fail fast.
    pub async fn drive_one_cycle(
        handle: &mut WorkflowHandle,
    ) -> (Vec<SectionTranscript>, Option<Result<(), WorkflowError>>) {
        match tokio::time::timeout(CYCLE_TIMEOUT, drive_one_cycle_inner(handle)).await {
            Ok(result) => result,
            Err(_) => panic!("drive_one_cycle timed out after {CYCLE_TIMEOUT:?} — workflow hung"),
        }
    }

    /// Drive a workflow to termination, accumulating every transcript
    /// delta. Loops past transient parks — a body parked on `inbox.next()`
    /// that a pending `exit()`/shutdown is about to unblock would otherwise
    /// be reported as `None` by [`drive_one_cycle`] (the park and the
    /// completion race across the JS-thread boundary). Use this for
    /// workflows that terminate on their own; for interactive multi-turn
    /// tests use `drive_one_cycle` and feed input between calls.
    pub async fn drive_to_done(
        handle: &mut WorkflowHandle,
    ) -> (Vec<SectionTranscript>, Result<(), WorkflowError>) {
        let mut frames = Vec::new();
        loop {
            let (mut batch, outcome) = drive_one_cycle(handle).await;
            frames.append(&mut batch);
            if let Some(result) = outcome {
                return (frames, result);
            }
        }
    }

    async fn drive_one_cycle_inner(
        handle: &mut WorkflowHandle,
    ) -> (Vec<SectionTranscript>, Option<Result<(), WorkflowError>>) {
        let mut out = Vec::new();
        loop {
            while let Ok(delta) = handle.outputs.transcript.try_recv() {
                out.push(delta);
            }
            tokio::select! {
                biased;
                Some(delta) = handle.outputs.transcript.recv() => out.push(delta),
                done = &mut handle.done => {
                    let result = done.unwrap_or(Ok(()));
                    while let Ok(delta) = handle.outputs.transcript.try_recv() {
                        out.push(delta);
                    }
                    return (out, Some(result));
                }
                () = handle.on_idle.notified() => {
                    while let Ok(delta) = handle.outputs.transcript.try_recv() {
                        out.push(delta);
                    }
                    return (out, None);
                }
            }
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub mod test_deps {
    //! In-memory `WorkflowDeps` for tests. `push` records to a local
    //! Vec; `run` errors out (no provider) by default. Tests that need a
    //! happy-path provider stub the next `run` with a script via
    //! `StubDeps::script_next_run` — that call configures the events to
    //! emit and the `CompletionOutcome` to return.

    use async_trait::async_trait;
    use dashmap::DashMap;
    use frances_edit::{EditEngine, EditSession, FakeStore};
    use frances_models_llm::chat::{
        ChatCheckpoint, ChatError, ChatSession, ChatSessionBuilder, ChatSessionId,
        ChatSessionManager, HistoryError, OwnedHistoryInput,
    };
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolChoice, ToolDef};
    use frances_storage::{EntitySchema, Migration};
    use parking_lot::Mutex;
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    use uuid::Uuid;

    use crate::deps::{EditorFactory, WorkflowDeps};
    use crate::io::WorkflowIo;
    use crate::io::mock::StubIo;
    use crate::storage::{WorkflowDb, WorkflowDbError};
    use tokio::sync::OnceCell;

    /// Default `Io` for workflow tests: real timer + mock shell
    /// (errors on spawn) + real fs. Existing tests measure real
    /// elapsed time and seed tempdirs through `set_cwd`, so swapping
    /// timer or fs to mocks would break them. Tests that want
    /// determinism construct a different `StubIo` variant themselves.
    type DefaultIo = StubIo;

    #[derive(Clone, Default)]
    pub struct StubDeps {
        manager: StubManager,
        io: DefaultIo,
        editor_factory: StubEditorFactory,
        cwd: Arc<Mutex<Option<PathBuf>>>,
        storage: StubStorage,
    }

    impl StubDeps {
        /// Sets the cwd reported by `current_cwd`. Lets editor tests
        /// point relative paths at a tempdir without spinning up a full
        /// `InvocationContext`.
        pub fn set_cwd(&self, cwd: PathBuf) {
            *self.cwd.lock() = Some(cwd);
        }

        /// Drop the cached `WorkflowDb` for `entity` so the next
        /// `workflow_db()` call re-applies its migrations. The
        /// underlying turso connection (and any `_migrations` rows
        /// already recorded for the entity) is preserved — this is how
        /// tests exercise the migrator's drift-detection path.
        pub fn forget_workflow_db(&self, entity: Uuid) {
            if let Some(state) = self.storage.inner.state.get() {
                state.entities.remove(&entity);
            }
        }

        /// Hand back the IO bundle. Lets tests reach into the mock
        /// shell or any other swappable sub-piece directly.
        pub fn io(&self) -> &DefaultIo {
            &self.io
        }
    }

    impl WorkflowIo for StubDeps {
        type Timer = <DefaultIo as WorkflowIo>::Timer;
        type Shell = <DefaultIo as WorkflowIo>::Shell;
        type Fs = <DefaultIo as WorkflowIo>::Fs;
        fn timer(&self) -> &Self::Timer {
            self.io.timer()
        }
        fn shell(&self) -> &Self::Shell {
            self.io.shell()
        }
        fn fs(&self) -> &Self::Fs {
            self.io.fs()
        }
    }

    impl WorkflowDeps for StubDeps {
        type ChatSessionManager = StubManager;
        type EditorFactory = StubEditorFactory;

        fn chat_session_manager(&self) -> &Self::ChatSessionManager {
            &self.manager
        }

        fn editor_factory(&self) -> &Self::EditorFactory {
            &self.editor_factory
        }

        fn current_env(&self) -> HashMap<OsString, OsString> {
            HashMap::new()
        }

        fn current_cwd(&self) -> Option<PathBuf> {
            self.cwd.lock().clone()
        }

        async fn workflow_db(
            &self,
            entity: Uuid,
            migrations: Cow<'_, [Migration]>,
        ) -> Result<Arc<WorkflowDb>, WorkflowDbError> {
            self.storage.workflow_db(entity, migrations).await
        }
    }

    /// Shared in-memory turso connection plus per-entity `WorkflowDb`
    /// cache. Both stub-deps flavours hold a clone; tests can co-opt the
    /// same connection across stub instances if they construct via
    /// `StubStorage::shared`.
    #[derive(Clone, Default)]
    pub struct StubStorage {
        inner: Arc<StubStorageInner>,
    }

    #[derive(Default)]
    struct StubStorageInner {
        state: OnceCell<StubStorageState>,
    }

    struct StubStorageState {
        db: frances_storage::Database,
        entities: DashMap<Uuid, Arc<WorkflowDb>>,
    }

    impl StubStorage {
        async fn workflow_db(
            &self,
            entity: Uuid,
            migrations: Cow<'_, [Migration]>,
        ) -> Result<Arc<WorkflowDb>, WorkflowDbError> {
            let state = self
                .inner
                .state
                .get_or_try_init(|| async {
                    let db = frances_storage::Database::open_in_memory()
                        .await
                        .map_err(|source| WorkflowDbError::Turso { entity, source })?;
                    {
                        let conn = db.connect().await;
                        frances_storage::ensure_table(&conn).await?;
                    }
                    Ok::<_, WorkflowDbError>(StubStorageState {
                        db,
                        entities: DashMap::new(),
                    })
                })
                .await?;
            if let Some(existing) = state.entities.get(&entity) {
                return Ok(existing.clone());
            }
            let schema = EntitySchema { entity, migrations };
            {
                let conn = state.db.connect().await;
                frances_storage::run(&conn, &schema).await?;
            }
            let db = Arc::new(WorkflowDb::new(state.db.clone(), entity));
            state.entities.insert(entity, db.clone());
            Ok(db)
        }
    }

    /// Variant of `StubDeps` for shell tests that need a real bash
    /// subprocess. Same `StubIo` underneath, but with the shell
    /// sub-piece swapped for the real-bash impl via
    /// `StubIo::with_real_shell()`. Timer and fs stay at their
    /// `StubIo` defaults (real timer + real fs).
    #[derive(Clone)]
    pub struct StubDepsRealShell {
        manager: StubManager,
        io: DefaultIo,
        editor_factory: StubEditorFactory,
        storage: StubStorage,
    }

    impl Default for StubDepsRealShell {
        fn default() -> Self {
            Self {
                manager: StubManager::default(),
                io: DefaultIo::with_real_shell(),
                editor_factory: StubEditorFactory::default(),
                storage: StubStorage::default(),
            }
        }
    }

    impl WorkflowIo for StubDepsRealShell {
        type Timer = <DefaultIo as WorkflowIo>::Timer;
        type Shell = <DefaultIo as WorkflowIo>::Shell;
        type Fs = <DefaultIo as WorkflowIo>::Fs;
        fn timer(&self) -> &Self::Timer {
            self.io.timer()
        }
        fn shell(&self) -> &Self::Shell {
            self.io.shell()
        }
        fn fs(&self) -> &Self::Fs {
            self.io.fs()
        }
    }

    impl WorkflowDeps for StubDepsRealShell {
        type ChatSessionManager = StubManager;
        type EditorFactory = StubEditorFactory;

        fn chat_session_manager(&self) -> &Self::ChatSessionManager {
            &self.manager
        }

        fn editor_factory(&self) -> &Self::EditorFactory {
            &self.editor_factory
        }

        fn current_env(&self) -> HashMap<OsString, OsString> {
            HashMap::new()
        }

        fn current_cwd(&self) -> Option<PathBuf> {
            None
        }

        async fn workflow_db(
            &self,
            entity: Uuid,
            migrations: Cow<'_, [Migration]>,
        ) -> Result<Arc<WorkflowDb>, WorkflowDbError> {
            self.storage.workflow_db(entity, migrations).await
        }
    }

    /// Test editor factory: hands out a fresh in-memory `EditSession`
    /// backed by `FakeStore`. Each clone shares the same session, so
    /// reads and edits in a single test see the same anchor cache.
    #[derive(Clone)]
    pub struct StubEditorFactory {
        session: Arc<AsyncMutex<EditSession<FakeStore>>>,
    }

    impl Default for StubEditorFactory {
        fn default() -> Self {
            Self {
                session: Arc::new(AsyncMutex::new(EditSession::new(EditEngine::new(
                    FakeStore::new(),
                )))),
            }
        }
    }

    impl EditorFactory for StubEditorFactory {
        type Store = FakeStore;

        fn session(&self) -> Arc<AsyncMutex<EditSession<FakeStore>>> {
            self.session.clone()
        }
    }

    impl StubDepsRealShell {
        pub fn script_next_run(&self, events: Vec<StreamEvent>, outcome: CompletionOutcome) {
            self.manager
                .next_script
                .lock()
                .push_back(Script { events, outcome });
        }

        pub fn sessions(&self) -> Vec<StubSession> {
            self.manager.sessions.lock().clone()
        }
    }

    impl StubDeps {
        /// Queue a scripted response for the next session's first `run`
        /// call. Subsequent calls fall back to the default
        /// `ProviderUnavailable` error unless re-scripted.
        pub fn script_next_run(&self, events: Vec<StreamEvent>, outcome: CompletionOutcome) {
            self.manager
                .next_script
                .lock()
                .push_back(Script { events, outcome });
        }

        /// All sessions handed out by the manager so tests can inspect
        /// pending-input history after the run.
        pub fn sessions(&self) -> Vec<StubSession> {
            self.manager.sessions.lock().clone()
        }

        /// Every `ChatSessionBuilder` handed to `manager.create`, in
        /// order. Lets tests assert constructor options round-trip.
        pub fn chat_builders(&self) -> Vec<ChatSessionBuilder> {
            self.manager.builders()
        }
    }

    #[derive(Clone, Default)]
    pub struct StubManager {
        next_script: Arc<Mutex<std::collections::VecDeque<Script>>>,
        sessions: Arc<Mutex<Vec<StubSession>>>,
        builders: Arc<Mutex<Vec<ChatSessionBuilder>>>,
    }

    impl StubManager {
        /// Every `ChatSessionBuilder` handed to `create`, in order. Lets
        /// JS-surface tests assert that constructor options round-trip.
        pub fn builders(&self) -> Vec<ChatSessionBuilder> {
            self.builders.lock().clone()
        }
    }

    #[derive(Clone)]
    struct Script {
        events: Vec<StreamEvent>,
        outcome: CompletionOutcome,
    }

    #[async_trait]
    impl ChatSessionManager for StubManager {
        type Session = StubSession;

        fn create(&self, builder: ChatSessionBuilder) -> Self::Session {
            self.builders.lock().push(builder);
            let session = StubSession {
                pending: Arc::new(Mutex::new(Vec::new())),
                next_script: self.next_script.clone(),
            };
            self.sessions.lock().push(session.clone());
            session
        }

        async fn load(&self, _id: ChatSessionId) -> Result<Self::Session, ChatError> {
            Err(ChatError::History(HistoryError::ChatSessionNotFound(
                ChatSessionId(0),
            )))
        }

        async fn complete(
            &self,
            req: frances_models_llm::chat::CompleteRequest<'_>,
        ) -> Result<CompletionOutcome, ChatError> {
            // Reuse the same script queue as `StubSession::run`; each
            // call pops one scripted outcome (events are ignored — a
            // one-shot complete has no event sink). `complete_enforced`
            // is the trait default, so it pops one script per round.
            // Annotate like the real chat layer so tests see bad-arg flags.
            match self.next_script.lock().pop_front() {
                Some(mut s) => {
                    frances_models_llm::tool_args::annotate(&mut s.outcome.tool_calls, req.tools);
                    Ok(s.outcome)
                }
                None => Err(ChatError::ProviderUnavailable(
                    "stub manager: no script wired for complete".to_owned(),
                )),
            }
        }
    }

    #[derive(Clone)]
    pub struct StubSession {
        pending: Arc<Mutex<Vec<OwnedHistoryInput>>>,
        next_script: Arc<Mutex<std::collections::VecDeque<Script>>>,
    }

    impl StubSession {
        pub fn pending(&self) -> Vec<OwnedHistoryInput> {
            self.pending.lock().clone()
        }
    }

    #[async_trait]
    impl ChatSession for StubSession {
        fn push(&self, input: OwnedHistoryInput) {
            self.pending.lock().push(input);
        }

        async fn run(
            &self,
            _env: HashMap<OsString, OsString>,
            tools: Vec<ToolDef>,
            _tool_choice: Option<ToolChoice>,
            cancel: tokio_util::sync::CancellationToken,
            _max_tool_calls: Option<usize>,
            mut on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send>,
        ) -> Result<CompletionOutcome, ChatError> {
            if cancel.is_cancelled() {
                return Err(ChatError::Cancelled);
            }
            let script = self.next_script.lock().pop_front();
            match script {
                Some(mut s) => {
                    for ev in s.events {
                        on_event(ev)?;
                    }
                    // Annotate like the real chat layer so workflow tests
                    // see schema-invalid calls flagged on `tool_calls`.
                    frances_models_llm::tool_args::annotate(&mut s.outcome.tool_calls, &tools);
                    Ok(s.outcome)
                }
                None => Err(ChatError::ProviderUnavailable(
                    "stub session: no provider wired in tests".to_owned(),
                )),
            }
        }

        async fn checkpoint(&self) -> Result<ChatCheckpoint, ChatError> {
            Ok(ChatCheckpoint {
                persisted: None,
                pending_len: self.pending.lock().len(),
            })
        }

        async fn rollback(&self, checkpoint: ChatCheckpoint) -> Result<(), ChatError> {
            let mut pending = self.pending.lock();
            if checkpoint.pending_len < pending.len() {
                pending.truncate(checkpoint.pending_len);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionResponse;
    use std::io::Write;

    use super::test_deps::StubDeps;

    fn write_source(ext: &str, body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .expect("tempfile");
        f.write_all(body.as_bytes()).expect("write");
        f
    }

    use super::test_drive::{CYCLE_TIMEOUT, drive_one_cycle, drive_to_done};

    fn text_of(delta: &SectionTranscript) -> String {
        match delta {
            SectionTranscript::Set { section: spec, .. } => match &spec.kind {
                SectionKind::Markdown { .. } | SectionKind::Error => {
                    spec.seed.clone().unwrap_or_default()
                }
                SectionKind::ToolUse { name, detail } => match detail {
                    Some(d) => format!("→ {name}  {d}"),
                    None => format!("→ {name}"),
                },
                SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
                SectionKind::Reasoning { state } => format!(
                    "[reasoning:{state:?}]\n{}",
                    spec.seed.clone().unwrap_or_default()
                ),
                SectionKind::ShellOutput { state, cmd } => {
                    format!(
                        "[shell:{state:?}] $ {cmd}\n{}",
                        spec.seed.clone().unwrap_or_default()
                    )
                }
                SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
            },
            SectionTranscript::Append { delta, .. } => delta.clone(),
            SectionTranscript::Close { id } => format!("[close:{}]", id.0),
        }
    }

    #[tokio::test]
    async fn set_status_emits_status_frames() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { setStatus } from "frances:v1/workflow";
            setStatus("working…");
            setStatus(null);
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        // setStatus lands on the surfaces channel, not the transcript.
        let mut surfaces = Vec::new();
        while let Ok(s) = handle.outputs.surfaces.try_recv() {
            surfaces.push(s);
        }
        assert_eq!(
            surfaces,
            vec![
                SurfaceCmd::SetFooter {
                    text: "working…".to_string()
                },
                SurfaceCmd::ClearFooter,
            ],
            "expected set then clear on the surfaces channel",
        );
    }

    #[tokio::test]
    async fn iterator_delivers_messages_in_order() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { exit } from "frances:v1/workflow";
            for await (const input of inbox) {
                transcript.push(new MarkdownSection({ content: "got:" + input.content }));
                if (input.content === "stop") { exit(); break; }
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty(), "got {frames:?}");
        assert!(done.is_none());

        handle
            .input_tx
            .send(InboxItem::Input(UserInput {
                content: "a".into(),
            }))
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert_eq!(text_of(&frames[0]), "got:a");
        assert!(done.is_none());

        handle
            .input_tx
            .send(InboxItem::Input(UserInput {
                content: "b".into(),
            }))
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert_eq!(text_of(&frames[0]), "got:b");
        assert!(done.is_none());

        handle
            .input_tx
            .send(InboxItem::Input(UserInput {
                content: "stop".into(),
            }))
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert_eq!(text_of(&frames[0]), "got:stop");
        assert!(matches!(done, Some(Ok(()))));
    }

    #[tokio::test]
    async fn body_returns_terminates_workflow() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: "hi" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "hi");
    }

    #[tokio::test]
    async fn dangling_promise_does_not_keep_workflow_alive() {
        // A bare `new Promise(() => {})` has no backing host IO, so the
        // event loop empties around it and the workflow reaps cleanly
        // rather than hanging. (Under the old "await the body promise"
        // model this hung until CYCLE_TIMEOUT.)
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: "before" }));
            await new Promise(() => {});
            transcript.push(new MarkdownSection({ content: "unreachable" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert!(
            frames.iter().any(|f| text_of(f) == "before"),
            "missing 'before': {frames:?}"
        );
        assert!(
            !frames.iter().any(|f| text_of(f) == "unreachable"),
            "should not run past the dangling await: {frames:?}"
        );
    }

    #[tokio::test]
    async fn import_meta_args_populated() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: import.meta.args.join('|') }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: vec!["a".into(), "b".into(), "c".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), "a|b|c");
    }

    #[tokio::test]
    async fn import_meta_instance_populated() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: import.meta.instance }));
            "#,
        );
        let instance = uuid::Uuid::from_u128(0xfeed_face_0000_0000_0000_0000_0000_0001);
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                instance_id: instance,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(handle.instance, instance);
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), instance.to_string());
    }

    /// The workflow's `lifecycle.shutdown` handler fires when the host
    /// calls `request_shutdown`. The handler emits a final frame, then
    /// the inbox closes and the for-await loop unwinds.
    #[tokio::test]
    async fn lifecycle_shutdown_runs_on_request() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { lifecycle } from "frances:v1/lifecycle";

            lifecycle.shutdown = async () => {
                transcript.push(new MarkdownSection({ content: "bye" }));
            };
            for await (const _ of inbox) {
                transcript.push(new MarkdownSection({ content: "got input" }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                ..Default::default()
            })
            .await
            .unwrap();
        // Workflow parks on inbox.next() first.
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty(), "got {frames:?}");
        assert!(done.is_none());

        handle.request_shutdown();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "bye");
    }

    /// `workflow.exit()` now also routes through the lifecycle IIFE, so
    /// a registered shutdown handler fires before the body terminates.
    #[tokio::test]
    async fn lifecycle_shutdown_runs_on_exit() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { lifecycle } from "frances:v1/lifecycle";
            import { exit } from "frances:v1/workflow";

            lifecycle.shutdown = async () => {
                transcript.push(new MarkdownSection({ content: "bye" }));
            };
            queueMicrotask(() => exit());
            for await (const _ of inbox) {
                transcript.push(new MarkdownSection({ content: "got input" }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                ..Default::default()
            })
            .await
            .unwrap();
        // exit() is queued before the body parks, so the park and the
        // shutdown-driven completion collapse into one logical cycle —
        // drive to done rather than racing the transient park.
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(matches!(result, Ok(())), "result was {result:?}");
        assert_eq!(text_of(&frames[0]), "bye");
    }

    /// A workflow that never registers `lifecycle.shutdown` still
    /// terminates promptly on `request_shutdown` — the IIFE closes the
    /// inbox unconditionally after the (absent) handler returns.
    #[tokio::test]
    async fn request_shutdown_without_handler_terminates_promptly() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            for await (const _ of inbox) {}
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(done.is_none());

        handle.request_shutdown();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
    }

    #[tokio::test]
    async fn fresh_context_per_invocation() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            globalThis.__counter = (globalThis.__counter ?? 0) + 1;
            transcript.push(new MarkdownSection({ content: String(globalThis.__counter) }));
            "#,
        );
        let path = file.path().to_path_buf();

        for _ in 0..3 {
            let mut handle = rt
                .start(Invocation {
                    source_path: path.clone(),
                    args: Vec::new(),
                    ..Default::default()
                })
                .await
                .unwrap();
            let (frames, done) = drive_one_cycle(&mut handle).await;
            assert!(matches!(done, Some(Ok(()))));
            assert_eq!(
                text_of(&frames[0]),
                "1",
                "expected counter=1 each invocation"
            );
        }
    }

    #[tokio::test]
    async fn exit_unblocks_pending_next() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { exit } from "frances:v1/workflow";
            queueMicrotask(() => exit());
            for await (const _ of inbox) {
                transcript.push(new MarkdownSection({ content: "got input" }));
            }
            transcript.push(new MarkdownSection({ content: "after-loop" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(matches!(result, Ok(())), "result was {result:?}");
        assert_eq!(text_of(&frames[0]), "after-loop");
    }

    #[tokio::test]
    async fn symbol_async_iterator_returns_self() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { exit } from "frances:v1/workflow";
            const it = inbox[Symbol.asyncIterator]();
            transcript.push(new MarkdownSection({ content: it === inbox ? "same" : "different" }));
            exit();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), "same");
    }

    #[tokio::test]
    async fn concurrent_next_fifo() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { exit } from "frances:v1/workflow";
            const a = inbox.next();
            const b = inbox.next();
            const [ra, rb] = await Promise.all([a, b]);
            transcript.push(new MarkdownSection({ content: `${ra.value.content},${rb.value.content}` }));
            exit();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty());
        assert!(done.is_none());

        handle
            .input_tx
            .send(InboxItem::Input(UserInput {
                content: "first".into(),
            }))
            .unwrap();
        handle
            .input_tx
            .send(InboxItem::Input(UserInput {
                content: "second".into(),
            }))
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), "first,second");
    }

    #[tokio::test]
    async fn ts_transpile_strips_types() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "ts",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const args: string[] = import.meta.args;
            transcript.push(new MarkdownSection({ content: args.length.toString() }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: vec!["x".into(), "y".into(), "z".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), "3");
    }

    #[tokio::test]
    async fn script_throw_surfaces_as_script_error() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source("js", "throw new Error('boom');");
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    /// `new MarkdownSection({ source })` (and `{ content: undefined }`
    /// and `{ content: null }`) all produce `SectionKind::Markdown` with
    /// `content: None`. The wire opener carries no body, so the TUI
    /// defers measure / render until the workflow writes into it.
    #[tokio::test]
    async fn markdown_frame_content_can_be_omitted() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ source: "assistant" }));
            transcript.push(new MarkdownSection({ content: undefined }));
            transcript.push(new MarkdownSection({ content: null }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        for (i, expect_source) in [Source::Assistant, Source::Internal, Source::Internal]
            .iter()
            .enumerate()
        {
            match &frames[i] {
                SectionTranscript::Set { section: spec, .. } => match &spec.kind {
                    SectionKind::Markdown { source } => {
                        assert!(spec.seed.is_none(), "frame {i} should have no seed");
                        assert_eq!(source, expect_source, "frame {i} source");
                    }
                    other => panic!("frame {i} unexpected kind {other:?}"),
                },
                other => panic!("frame {i} unexpected {other:?}"),
            }
        }
    }

    /// Pushing an empty-content frame and never writing to it produces
    /// `Push` + `Close` only — no `Append` in between. The runtime side
    /// uses this signal to skip persisting the row and the client uses
    /// the absent body delta to skip rendering.
    #[tokio::test]
    async fn empty_markdown_frame_pushes_and_closes_without_appends() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const f = new MarkdownSection({ source: "assistant" });
            transcript.push(f);
            await f.writable.close();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(
            &frames[0],
            SectionTranscript::Set { section: spec, .. }
                if matches!(&spec.kind, SectionKind::Markdown { .. }) && spec.seed.is_none()
        ));
        assert!(matches!(&frames[1], SectionTranscript::Close { .. }));
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, SectionTranscript::Append { .. })),
            "no Append should be emitted for a never-written frame"
        );
    }

    #[tokio::test]
    async fn write_on_active_frame_emits_delta() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const f = new MarkdownSection({ content: "hello" });
            transcript.push(f);
            const w = f.writable.getWriter();
            await w.write(" world");
            await w.close();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(
            matches!(&frames[0], SectionTranscript::Set { section: spec, .. } if matches!(&spec.kind, SectionKind::Markdown { .. }) && spec.seed.as_deref() == Some("hello"))
        );
        assert!(matches!(&frames[1], SectionTranscript::Append { delta, .. } if delta == " world"));
    }

    #[tokio::test]
    async fn markdown_frame_carries_source() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: "hi", source: "user" }));
            transcript.push(new MarkdownSection({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(
            &frames[0],
            SectionTranscript::Set { section: spec, .. }
                if matches!(&spec.kind, SectionKind::Markdown { source: Source::User })
                    && spec.seed.as_deref() == Some("hi")
        ));
        assert!(matches!(
            &frames[1],
            SectionTranscript::Set { section: spec, .. }
                if matches!(&spec.kind, SectionKind::Markdown { source: Source::Internal })
                    && spec.seed.as_deref() == Some("ok")
        ));
    }

    #[tokio::test]
    async fn markdown_frame_rejects_non_string_source() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            new MarkdownSection({ content: "hi", source: 42 });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        let err = done.expect("workflow done").expect_err("expected throw");
        assert!(
            format!("{err}").contains("source"),
            "error should mention source: {err}"
        );
    }

    #[tokio::test]
    async fn markdown_frame_rejects_unknown_source_string() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            new MarkdownSection({ content: "hi", source: "frances" });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        let err = done.expect("workflow done").expect_err("expected throw");
        assert!(
            format!("{err}").contains("source"),
            "error should mention source: {err}"
        );
    }

    #[tokio::test]
    async fn write_to_earlier_frame_after_newer_push_still_works() {
        // Pushing a second frame doesn't seal the first — multiple
        // frames can be writeable at once. Each frame's writes route
        // by id, so a workflow can stream into a long-running shell
        // block while also emitting markdown text alongside it.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const a = new MarkdownSection({ content: "a" });
            transcript.push(a);
            transcript.push(new MarkdownSection({ content: "b" }));
            const w = a.writable.getWriter();
            await w.write(" extra");
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(matches!(result, Ok(())), "got {result:?}");
        // Two Push frames (a, b) then one Append carrying " extra"
        // for `a`'s id.
        let appends: Vec<_> = frames
            .iter()
            .filter_map(|f| match f {
                SectionTranscript::Append { id, delta } => Some((*id, delta.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(appends.len(), 1, "expected one append, got {appends:?}");
        assert_eq!(appends[0].1, " extra");
    }

    #[tokio::test]
    async fn shell_output_frame_pushes_streams_transitions_and_closes() {
        // Exercise the ShellOutputSection lifecycle: push (Running), pipe
        // stdout in, transition to exit, autoclose seals the block.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, ShellOutputSection } from "frances:v1/sections";
            const f = new ShellOutputSection({ cmd: "ls" });
            transcript.push(f);
            const w = f.writable.getWriter();
            await w.write("a\n");
            await w.write("b\n");
            f.exit(0);
            await w.close();   // autoclose fires frame.close()
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_one_cycle(&mut handle).await;
        assert!(matches!(result, Some(Ok(()))), "got {result:?}");

        // Expect: one Set (Running, seed ""), two Appends ("a\n",
        // "b\n"), one metadata Set (Exit(0)), one Close.
        let frame_id = match frames.first() {
            Some(SectionTranscript::Set { id, section: spec }) => {
                match &spec.kind {
                    SectionKind::ShellOutput {
                        state: ShellState::Running,
                        cmd,
                    } => {
                        assert_eq!(cmd, "ls");
                        assert_eq!(spec.seed.as_deref(), Some(""));
                    }
                    other => panic!("expected ShellOutput Running, got {other:?}"),
                }
                *id
            }
            other => panic!("expected first frame to be Set, got {other:?}"),
        };

        let appends: Vec<&String> = frames
            .iter()
            .filter_map(|f| match f {
                SectionTranscript::Append { id, delta } if *id == frame_id => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(
            appends.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["a\n", "b\n"]
        );

        let saw_exit = frames.iter().any(|f| {
            matches!(
                f,
                SectionTranscript::Set { id, section: spec } if *id == frame_id && matches!(&spec.kind, SectionKind::ShellOutput { state: ShellState::Exit(0), .. }),
            )
        });
        assert!(saw_exit, "expected a metadata Set(Exit(0)) for the frame");

        let saw_close = frames
            .iter()
            .any(|f| matches!(f, SectionTranscript::Close { id } if *id == frame_id));
        assert!(saw_close, "expected Close for the frame");
    }

    #[tokio::test]
    async fn frame_autoclose_can_be_disabled() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const f = new MarkdownSection({ content: "" });
            f.autoclose = false;
            transcript.push(f);
            const w = f.writable.getWriter();
            await w.write("hi");
            await w.close();   // autoclose disabled — no Close emitted
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, _result) = drive_one_cycle(&mut handle).await;
        let close_count = frames
            .iter()
            .filter(|f| matches!(f, SectionTranscript::Close { .. }))
            .count();
        assert_eq!(close_count, 0, "autoclose=false should suppress Close");
    }

    #[tokio::test]
    async fn unknown_v1_module_fails_to_load() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source("js", r#"import { nope } from "frances:v1/nope";"#);
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_accepts_system_and_user_roles() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["summarize"] });
            s.push({ role: "system", content: "you are a summariser" });
            s.push({ role: "user", content: "hi" });
            transcript.push(new MarkdownSection({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), "ok");
    }

    #[tokio::test]
    async fn chat_session_default_is_not_ephemeral() {
        let deps = StubDeps::default();
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            new ChatSession({ model_intents: ["x"] });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let builders = deps.chat_builders();
        assert_eq!(builders.len(), 1);
        assert!(!builders[0].ephemeral, "default should be persisted");
        assert_eq!(
            builders[0]
                .model_intents
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<_>>(),
            vec!["x"]
        );
    }

    #[tokio::test]
    async fn chat_session_ephemeral_flag_threads_to_builder() {
        let deps = StubDeps::default();
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            new ChatSession({ model_intents: ["classify"], ephemeral: true });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let builders = deps.chat_builders();
        assert_eq!(builders.len(), 1);
        assert!(builders[0].ephemeral);
    }

    #[tokio::test]
    async fn chat_session_ephemeral_rejects_non_bool() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            new ChatSession({ model_intents: ["x"], ephemeral: "yes" });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_rejects_system_after_user() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            s.push({ role: "system", content: "too late" });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_allows_multiple_consecutive_system_messages() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "system", content: "be terse" });
            s.push({ role: "system", content: "answer in english" });
            s.push({ role: "user", content: "hi" });
            transcript.push(new MarkdownSection({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "ok");
    }

    #[tokio::test]
    async fn chat_session_rejects_assistant_role() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "assistant", content: "nope" });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_stream_returns_iterable_and_completed() {
        // StubSession::run errors out (no provider). r.completed should
        // reject; iterating r.events should still terminate cleanly
        // because the spawn task drops the sender on error.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { ReadableStream } from "whatwg:web-streams";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            if (!(r.events instanceof ReadableStream)) throw new Error("events not a ReadableStream");
            if (typeof r.completed?.then !== "function") throw new Error("completed not a Promise");
            // Drain events (will be empty since the stub never sends any).
            for await (const _ of r.events) { /* never fires */ }
            try {
                await r.completed;
                throw new Error("expected completed to reject");
            } catch (e) {
                if (!String(e).includes("stub session")) throw e;
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(matches!(result, Ok(())), "got {result:?}");
    }

    #[tokio::test]
    async fn chat_session_raw_inner_stream_is_not_exposed() {
        // The Rust-level "start raw stream" function is captured into
        // closure by `chat.js` from a stash key that the host deletes
        // before user code runs. After install, neither
        // `ChatSession.prototype` nor `globalThis` should expose it.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const protoKeys = Object.getOwnPropertyNames(ChatSession.prototype)
                .filter((k) => k !== "constructor");
            const stashGone = typeof globalThis.__frances_v1_stash__ === "undefined";
            transcript.push(new MarkdownSection({
                content: `proto=${protoKeys.sort().join(",")} stash=${stashGone}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        // Only the public prototype methods (`push`, `checkpoint`,
        // `rollback`) plus the JS-installed `stream` should appear; the
        // inner raw stream function must not.
        assert_eq!(
            text_of(&frames[0]),
            "proto=checkpoint,push,rollback,stream stash=true"
        );
    }

    #[tokio::test]
    async fn chat_session_stream_text_locks_events() {
        // Per WHATWG, `pipeThrough` locks its source. Touching `r.text`
        // must therefore prevent any subsequent direct read of
        // `r.events` — that's how we enforce single-consumer semantics
        // without exposing the raw async-iterable.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            const _text = r.text;  // locks events via pipeThrough
            let locked = false;
            try { r.events.getReader(); }
            catch (_) { locked = true; }
            transcript.push(new MarkdownSection({
                content: `locked=${locked} stableText=${r.text === _text}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "locked=true stableText=true");
    }

    #[tokio::test]
    async fn chat_session_stream_pipes_into_markdown_frame_writable() {
        // Stub emits zero events, so the pipe completes when the
        // source closes (Rust drops the sender after the run errors).
        // We're verifying the wiring: pipeTo from `r.text` into a
        // MarkdownSection's `.writable` resolves without throwing.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            const out = new MarkdownSection({ content: "" });
            transcript.push(out);
            await r.text.pipeTo(out.writable);
            try { await r.completed; } catch (_) { /* stub error — expected */ }
            transcript.push(new MarkdownSection({ content: "piped-ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        // Second push frame carries the "piped-ok" sentinel; the first
        // push is the empty `out` frame. No Append frames since stub
        // emits no text deltas.
        let last = text_of(frames.last().expect("at least one frame"));
        assert_eq!(last, "piped-ok");
    }

    #[tokio::test]
    async fn chat_session_text_pipe_closes_markdown_frame_on_completion() {
        use frances_models_llm::{CompletionOutcome, StreamEvent};

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::TextDelta("hello".to_owned())],
            CompletionOutcome {
                text: "hello".to_owned(),
                tool_calls: vec![],
            },
        );
        let rt = Runtime::new(deps).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            const out = new MarkdownSection({ source: "assistant" });
            transcript.push(out);
            await r.text.pipeTo(out.writable);
            await r.completed;
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let push_id = match frames.first() {
            Some(SectionTranscript::Set { id, .. }) => *id,
            other => panic!("expected first frame push, got {other:?}"),
        };
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, SectionTranscript::Append { id, delta } if *id == push_id && delta == "hello")),
            "expected text append for markdown frame: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, SectionTranscript::Close { id } if *id == push_id)),
            "expected markdown frame to close after text pipe: {frames:?}"
        );
    }

    /// `new MarkdownSection({ ..., closed: true })` pre-seals the frame:
    /// `transcript.push` emits the `Close` immediately after the `Push`
    /// so the TUI never paints the active-block spinner over the
    /// frame. Mirrors the workflow's one-shot patterns (greeting,
    /// echoed user message, scold messages from `shell.js`).
    #[tokio::test]
    async fn markdown_frame_closed_ctor_option_pushes_and_closes_in_one_shot() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: "hi", closed: true }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let push_id = match frames.first() {
            Some(SectionTranscript::Set { id, .. }) => *id,
            other => panic!("expected push first, got {other:?}"),
        };
        assert!(
            matches!(frames.get(1), Some(SectionTranscript::Close { id }) if *id == push_id),
            "second frame must be the matching Close: {frames:?}"
        );
    }

    /// `new MarkdownSection(...).close()` returns `this`, so the
    /// construct-and-seal idiom can be a one-liner. Same wire effect
    /// as the `{ closed: true }` ctor option: pre-push close just
    /// records the intent; `transcript.push` emits Push then Close.
    #[tokio::test]
    async fn markdown_frame_close_returns_this_for_chaining() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            transcript.push(new MarkdownSection({ content: "hi" }).close());
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let push_id = match frames.first() {
            Some(SectionTranscript::Set { id, .. }) => *id,
            other => panic!("expected push first, got {other:?}"),
        };
        assert!(
            matches!(frames.get(1), Some(SectionTranscript::Close { id }) if *id == push_id),
            "expected Push then Close: {frames:?}"
        );
    }

    #[tokio::test]
    async fn chat_session_stream_aborts_with_signal() {
        // Pre-aborted AbortSignal errors the events stream synchronously
        // during `stream()`, so the first read sees the reason.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const ac = new AbortController();
            ac.abort("user wanted out");
            const r = await s.stream({ signal: ac.signal });
            let caught;
            try {
                for await (const _ of r.events) { /* shouldn't fire */ }
                caught = "no-throw";
            } catch (e) {
                caught = String(e);
            }
            transcript.push(new MarkdownSection({ content: caught }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "user wanted out");
    }

    #[tokio::test]
    async fn chat_session_completed_rejects_with_abort_reason_on_cancel() {
        // The `completed` promise rejects via the structurally-tagged
        // cancellation error (Rust sets `err.cancelled`), which chat.js
        // converts into `signal.reason` to match the events/text streams.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const ac = new AbortController();
            ac.abort("user wanted out");
            const r = await s.stream({ signal: ac.signal });
            let caught;
            try {
                await r.completed;
                caught = "no-throw";
            } catch (e) {
                caught = String(e);
            }
            transcript.push(new MarkdownSection({ content: caught }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "user wanted out");
    }

    #[tokio::test]
    async fn chat_tools_array_is_per_instance_and_initially_empty() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const a = new ChatSession({ model_intents: ["x"] });
            const b = new ChatSession({ model_intents: ["x"] });
            const shape = `a=${Array.isArray(a.tools)} len=${a.tools.length} distinct=${a.tools !== b.tools}`;
            transcript.push(new MarkdownSection({ content: shape }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "a=true len=0 distinct=true");
    }

    #[tokio::test]
    async fn chat_tools_duplicate_names_throw_on_stream() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection, ErrorSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({ name: "echo", description: "d", parameters: {}, handler: () => {} });
            s.tools.push({ name: "echo", description: "d", parameters: {}, handler: () => {} });
            s.push({ role: "user", content: "hi" });
            try {
                await s.stream();
                transcript.push(new ErrorSection({ content: "BUG: stream did not throw" }));
            } catch (e) {
                transcript.push(new MarkdownSection({ content: String(e) }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let msg = text_of(&frames[0]);
        assert!(msg.contains("duplicate tool name `echo`"), "got `{msg}`");
    }

    #[tokio::test]
    async fn chat_tools_missing_fields_throw_on_stream() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection, ErrorSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({ name: "echo" }); // missing description / parameters
            s.push({ role: "user", content: "hi" });
            try {
                await s.stream();
                transcript.push(new ErrorSection({ content: "BUG: stream did not throw" }));
            } catch (e) {
                transcript.push(new MarkdownSection({ content: String(e) }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let msg = text_of(&frames[0]);
        assert!(msg.contains("description"), "got `{msg}`");
    }

    #[tokio::test]
    async fn chat_stream_surfaces_tool_calls_in_completed_and_events() {
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![
                StreamEvent::TextDelta("Calling tool...".to_owned()),
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "call_1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: json!({ "text": "hi" }),
                }),
            ],
            CompletionOutcome {
                text: "Calling tool...".to_owned(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "call_1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: json!({ "text": "hi" }),
                }],
            },
        );

        let rt = Runtime::new(deps).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection, ErrorSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            let handlerCalls = 0;
            s.tools.push({
                name: "echo",
                description: "echoes the input",
                parameters: { type: "object", properties: { text: { type: "string" } } },
                handler: async ({ call }) => {
                    handlerCalls += 1;
                    return {
                        role: "tool", call_id: call.id,
                        content: call.arguments.text, is_error: false,
                    };
                },
            });
            s.push({ role: "user", content: "hi" });

            const r = await s.stream();
            // Drain events; collect any tool_call events seen on the wire.
            let toolCallSeen = "no";
            const reader = r.events.getReader();
            while (true) {
                const { done, value } = await reader.read();
                if (done) break;
                if (value.type === "tool_call") toolCallSeen = `${value.name}:${value.arguments.text}`;
            }
            const final = await r.completed;
            const summary = `events=${toolCallSeen} text="${final.text}" calls=${final.tool_calls.length} first=${final.tool_calls[0].name}(${final.tool_calls[0].arguments.text}) handlerCalls=${handlerCalls}`;
            transcript.push(new MarkdownSection({ content: summary }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(frames.last().expect("at least one frame")),
            r#"events=echo:hi text="Calling tool..." calls=1 first=echo(hi) handlerCalls=1"#
        );
    }

    #[tokio::test]
    async fn chat_push_tool_role_queues_result() {
        let deps = StubDeps::default();
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            s.push({ role: "tool", call_id: "abc", content: "result body", is_error: false });
            transcript.push(new MarkdownSection({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        // Inspect the underlying stub session's pending queue. There
        // should be exactly two entries — the user message and the
        // tool result — in that order.
        let sessions = deps.sessions();
        assert_eq!(sessions.len(), 1);
        let pending = sessions[0].pending();
        assert_eq!(pending.len(), 2);
        assert!(matches!(
            &pending[0],
            frances_models_llm::chat::OwnedHistoryInput::User { text } if text == "hi"
        ));
        match &pending[1] {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(call_id, "abc");
                assert_eq!(content, "result body");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_push_tool_role_validates_fields() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection, ErrorSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            let caught = "";
            try {
                s.push({ role: "tool", call_id: 123, content: "x", is_error: false });
                caught = "no-throw";
            } catch (e) {
                caught = String(e);
            }
            transcript.push(new MarkdownSection({ content: caught }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        let msg = text_of(&frames[0]);
        assert!(msg.contains("call_id"), "got `{msg}`");
    }

    #[tokio::test]
    async fn stream_dispatches_tool_calls_internally() {
        // chat.stream() owns dispatch: when the LLM emits tool calls,
        // their handlers run inside the stream call and their results
        // get pushed back into the session before the next round.
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        // Round 1: model emits a tool call.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({ "text": "from round 1" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: json!({ "text": "from round 1" }),
                }],
            },
        );
        // Round 2: model finishes with plain text, no tool calls.
        deps.script_next_run(
            vec![StreamEvent::TextDelta("done.".to_owned())],
            CompletionOutcome {
                text: "done.".to_owned(),
                tool_calls: vec![],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            let handlerCalls = 0;
            s.tools.push({
                name: "echo",
                description: "echoes the input",
                parameters: { type: "object" },
                handler: async ({ call }) => {
                    handlerCalls += 1;
                    return {
                        role: "tool", call_id: call.id,
                        content: `echoed:${call.arguments.text}`, is_error: false,
                    };
                },
            });
            s.push({ role: "user", content: "go" });

            let finalText = "";
            while (true) {
                const r = await s.stream();
                const reader = r.events.getReader();
                while (true) { const { done } = await reader.read(); if (done) break; }
                reader.releaseLock();
                const { text, tool_calls } = await r.completed;
                finalText = text;
                if (tool_calls.length === 0) break;
            }
            transcript.push(new MarkdownSection({
                content: `text="${finalText}" handlerCalls=${handlerCalls}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(frames.last().expect("at least one frame")),
            r#"text="done." handlerCalls=1"#
        );

        // The tool result should have been pushed back to the session
        // between rounds.
        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let tool_result = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            } => Some((call_id.clone(), content.clone(), *is_error)),
            _ => None,
        });
        assert_eq!(
            tool_result,
            Some(("c1".to_owned(), "echoed:from round 1".to_owned(), false))
        );
    }

    #[tokio::test]
    async fn tool_call_hook_intercepts_dispatch() {
        // chat.toolCall is middleware around every dispatch: it can
        // pre-process, swap in a different result, or `await invoke()`
        // to fall through to the default behaviour.
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({ "text": "hi" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: json!({ "text": "hi" }),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            let preCount = 0, postCount = 0;
            s.tools.push({
                name: "echo",
                description: "echoes",
                parameters: { type: "object" },
                handler: async ({ call }) => ({
                    role: "tool", call_id: call.id,
                    content: `inner:${call.arguments.text}`, is_error: false,
                }),
            });
            s.toolCall = async ({ call, invoke }) => {
                preCount += 1;
                const result = await invoke();
                postCount += 1;
                return { ...result, content: `wrapped(${result.content})` };
            };
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({
                content: `pre=${preCount} post=${postCount}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(frames.last().expect("frame")), "pre=1 post=1");

        // The hook wrapped the inner result before it was pushed.
        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let tool_content = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        });
        assert_eq!(tool_content, Some("wrapped(inner:hi)".to_owned()));
    }

    #[tokio::test]
    async fn tool_call_hook_throw_becomes_error_result() {
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({
                name: "echo", description: "", parameters: {},
                handler: async ({ call }) => ({
                    role: "tool", call_id: call.id, content: "ok", is_error: false,
                }),
            });
            s.toolCall = async () => { throw new Error("gated"); };
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let tool = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        });
        assert_eq!(tool, Some(("gated".to_owned(), true)));
    }

    #[tokio::test]
    async fn missing_tool_pushes_synthetic_error_result() {
        // LLM hallucinates a tool name not in chat.tools — dispatch
        // synthesises an is_error: true result instead of crashing.
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "nonexistent".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "nonexistent".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let tool = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        });
        assert_eq!(tool, Some(("tool not found: nonexistent".to_owned(), true)));
    }

    #[tokio::test]
    async fn scope_tool_call_hook_isolated_to_nested_stream() {
        // A handler that sets `scope.toolCall` and calls `scope.stream()`
        // gets its hook used for the nested round. The outer chat's
        // `toolCall` is unaffected.
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        // Outer round: LLM calls `outer`.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "outer1".to_owned(),
                name: "outer".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "outer1".to_owned(),
                    name: "outer".to_owned(),
                    arguments: json!({}),
                }],
            },
        );
        // Inner round (driven by outer's handler via scope.stream()):
        // LLM calls `inner`.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "inner1".to_owned(),
                name: "inner".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "inner1".to_owned(),
                    name: "inner".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            let scopeHookCalls = 0;
            s.tools.push({
                name: "outer", description: "", parameters: {},
                handler: async ({ call, scope }) => {
                    scope.toolCall = async ({ call: c, invoke }) => {
                        scopeHookCalls += 1;
                        return await invoke();
                    };
                    const r = await scope.stream();
                    await r.completed;
                    return { role: "tool", call_id: call.id,
                             content: `outer-done; scopeHookCalls=${scopeHookCalls}`,
                             is_error: false };
                },
            });
            s.tools.push({
                name: "inner", description: "", parameters: {},
                handler: async ({ call }) => ({
                    role: "tool", call_id: call.id,
                    content: "inner-ran", is_error: false,
                }),
            });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        // Two tool results: outer (after inner ran) and inner.
        let results: Vec<_> = pending
            .iter()
            .filter_map(|p| match p {
                frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                    call_id,
                    content,
                    ..
                } => Some((call_id.clone(), content.clone())),
                _ => None,
            })
            .collect();
        assert!(
            results.iter().any(|(_, c)| c == "inner-ran"),
            "expected inner tool result, got {results:?}"
        );
        assert!(
            results
                .iter()
                .any(|(_, c)| c == "outer-done; scopeHookCalls=1"),
            "expected outer tool result with scopeHookCalls=1, got {results:?}"
        );
    }

    #[tokio::test]
    async fn shell_run_once_returns_done_for_short_command() {
        use super::test_deps::StubDepsRealShell;
        let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Shell } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const sh = new Shell();
            const outcome = await sh.runOnce("echo hello-shell");
            const summary = `kind=${outcome.kind} exit=${outcome.exit_code} hasOutput=${outcome.output.includes("hello-shell")}`;
            await sh.close();
            transcript.push(new MarkdownSection({ content: summary }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "kind=done exit=0 hasOutput=true");
    }

    #[tokio::test]
    async fn shell_busy_errors_on_double_run() {
        use super::test_deps::StubDepsRealShell;
        let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Shell } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const sh = new Shell();
            // First run goes Quiet (sleeps past the default 1s quiet
            // threshold).
            const first = await sh.runOnce("sleep 3");
            let caught = "";
            try {
                await sh.runOnce("echo nope");
                caught = "no-throw";
            } catch (e) {
                caught = String(e);
            }
            // Kill the in-flight sleep so the shell can be torn down
            // cleanly.
            await sh.kill();
            try { await sh.keepWaiting(); } catch (_) {}
            await sh.close();
            transcript.push(new MarkdownSection({
                content: `firstKind=${first.kind} caught=${caught.includes("busy") ? "busy" : caught}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "firstKind=quiet caught=busy");
    }

    #[tokio::test]
    async fn shell_keep_waiting_resumes_quiet_command() {
        use super::test_deps::StubDepsRealShell;
        let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Shell } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const sh = new Shell();
            // Background a sleep + then echo. The first runOnce will go
            // Quiet (because sleep is silent for >1s), and keepWaiting
            // will catch the final exit + echo output.
            let first = await sh.runOnce("sleep 2 && echo finished");
            let final_ = first;
            let waits = 0;
            while (final_.kind === "quiet" && waits < 10) {
                waits += 1;
                final_ = await sh.keepWaiting();
            }
            await sh.close();
            transcript.push(new MarkdownSection({
                content: `firstKind=${first.kind} finalKind=${final_.kind} exit=${final_.exit_code} hasFinished=${final_.output.includes("finished")}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive_one_cycle(&mut handle),
        )
        .await
        .expect("test should finish within 10s");
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "firstKind=quiet finalKind=done exit=0 hasFinished=true"
        );
    }

    #[tokio::test]
    async fn shell_run_tool_handler_formats_done_outcome() {
        // Wire the Run tool through chat.tools and dispatch a fake
        // shell_run tool call via the stubbed provider.
        use super::test_deps::StubDepsRealShell;
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "echo from-run-tool" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "shell_run".to_owned(),
                    arguments: json!({ "cmd": "echo from-run-tool" }),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const chat = new ChatSession({ model_intents: ["x"] });
            const sh = new Shell();
            chat.tools.push(new Run(sh, { approve: false }), new Wait(sh), new Kill(sh));
            chat.push({ role: "user", content: "do it" });
            const r = await chat.stream();
            const reader = r.events.getReader();
            while (true) { const { done } = await reader.read(); if (done) break; }
            reader.releaseLock();
            await r.completed;
            await sh.close();
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive_one_cycle(&mut handle),
        )
        .await
        .expect("test should finish within 10s");
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let tool_result = sessions[0].pending().iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            } if call_id == "c1" => Some((content.clone(), *is_error)),
            _ => None,
        });
        let (content, is_error) = tool_result.expect("tool result present");
        assert!(content.starts_with("Exit 0"), "got `{content}`");
        assert!(content.contains("from-run-tool"), "got `{content}`");
        assert!(!is_error);
    }

    #[tokio::test]
    async fn shell_run_quiet_registers_turn_for_wait_kill_negotiation() {
        // Long-running command goes Quiet → Run.handler registers a
        // scope.lock turn. The turn streams; scripted LLM emits
        // shell_wait → Wait.handler runs shell.keepWaiting until Done.
        //
        // We script several shell_wait rounds because keepWaiting's
        // default 1s quiet window can time out before the sentinel
        // arrives, especially under load — Run's turn will loop until
        // one of them catches Done.
        use super::test_deps::StubDepsRealShell;
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 3 && echo finished" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "shell_run".to_owned(),
                    arguments: json!({ "cmd": "sleep 3 && echo finished" }),
                }],
            },
        );
        for i in 0..5 {
            let id = format!("w-{i}");
            deps.script_next_run(
                vec![StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: id.clone(),
                    name: "shell_wait".to_owned(),
                    arguments: json!({}),
                })],
                CompletionOutcome {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        error: None,
                        id,
                        name: "shell_wait".to_owned(),
                        arguments: json!({}),
                    }],
                },
            );
        }

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const chat = new ChatSession({ model_intents: ["x"] });
            const sh = new Shell();
            const wait = new Wait(sh);
            const kill = new Kill(sh);
            chat.tools.push(new Run(sh, { wait, kill, approve: false }), wait, kill);
            chat.push({ role: "user", content: "run something slow" });
            const r = await chat.stream();
            const reader = r.events.getReader();
            while (true) { const { done } = await reader.read(); if (done) break; }
            reader.releaseLock();
            await r.completed;
            await sh.close();
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            drive_one_cycle(&mut handle),
        )
        .await
        .expect("test should finish within 15s");
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let initial = pending
            .iter()
            .find_map(|p| match p {
                frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                    call_id,
                    content,
                    ..
                } if call_id == "c1" => Some(content.clone()),
                _ => None,
            })
            .expect("initial shell_run result present");
        assert!(
            initial.contains("Still running"),
            "initial result should be quiet: `{initial}`"
        );

        // At least one shell_wait round should land Done with the
        // command's final output.
        let waited_done = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id, content, ..
            } if call_id.starts_with("w-") && content.starts_with("Exit 0") => {
                Some(content.clone())
            }
            _ => None,
        });
        let waited_done = waited_done.expect("at least one shell_wait should land Done");
        assert!(
            waited_done.contains("finished"),
            "Done result should contain final output: `{waited_done}`"
        );
    }

    #[tokio::test]
    async fn shell_run_quiet_scolds_then_kills_when_model_silent() {
        // Quiet shell + a model that emits no tool calls for multiple
        // rounds: Run should scold up to `maxScolds` times, then SIGKILL
        // the in-flight command and push a "killed" notice.
        use super::test_deps::StubDepsRealShell;
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        // Round 1: shell_run on a long-running command.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "shell_run".to_owned(),
                    arguments: json!({ "cmd": "sleep 30 && echo done" }),
                }],
            },
        );
        // Rounds 2-4 (in turn): model emits text but no tool calls.
        // maxScolds=2 → round 2 scolds, round 3 scolds, round 4 kills.
        for _ in 0..3 {
            deps.script_next_run(
                vec![StreamEvent::TextDelta("I don't want to.".to_owned())],
                CompletionOutcome {
                    text: "I don't want to.".to_owned(),
                    tool_calls: vec![],
                },
            );
        }

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const chat = new ChatSession({ model_intents: ["x"] });
            const sh = new Shell();
            const wait = new Wait(sh);
            const kill = new Kill(sh);
            chat.tools.push(new Run(sh, { wait, kill, approve: false }), wait, kill);
            chat.push({ role: "user", content: "run something slow" });
            const r = await chat.stream();
            const reader = r.events.getReader();
            while (true) { const { done } = await reader.read(); if (done) break; }
            reader.releaseLock();
            await r.completed;
            await sh.close();
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            drive_one_cycle(&mut handle),
        )
        .await
        .expect("test should finish within 15s");
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let scolds: Vec<_> = pending
            .iter()
            .filter_map(|p| match p {
                frances_models_llm::chat::OwnedHistoryInput::User { text }
                    if text.contains("still running") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(scolds.len(), 2, "expected exactly 2 scolds, got {scolds:?}");

        let killed = pending.iter().any(|p| {
            matches!(
                p,
                frances_models_llm::chat::OwnedHistoryInput::User { text }
                    if text.contains("Killed the shell command")
            )
        });
        assert!(killed, "expected 'Killed' message in pending: {pending:?}");

        // Scold and kill notices should also surface to the UI as
        // transcript frames so the user sees what's happening.
        let scold_frames = frames
            .iter()
            .filter(|f| text_of(f).contains("still running"))
            .count();
        assert_eq!(
            scold_frames, 2,
            "expected 2 scold frames, got {scold_frames}: {frames:?}"
        );
        let kill_frame_present = frames
            .iter()
            .any(|f| text_of(f).contains("Killed the shell command"));
        assert!(
            kill_frame_present,
            "expected a kill notice frame: {frames:?}"
        );
    }

    #[tokio::test]
    async fn shell_run_quiet_scolds_off_script_calls_then_kills() {
        // Same as the silent case but the model emits off-script tool
        // calls each round. The gating hook turns them into error
        // tool_results; the no-progress counter still ticks and the
        // shell is eventually killed.
        use super::test_deps::StubDepsRealShell;
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "shell_run".to_owned(),
                    arguments: json!({ "cmd": "sleep 30 && echo done" }),
                }],
            },
        );
        for i in 0..3 {
            let id = format!("offscript-{i}");
            deps.script_next_run(
                vec![StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: id.clone(),
                    name: "read_file".to_owned(),
                    arguments: json!({}),
                })],
                CompletionOutcome {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        error: None,
                        id,
                        name: "read_file".to_owned(),
                        arguments: json!({}),
                    }],
                },
            );
        }

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const chat = new ChatSession({ model_intents: ["x"] });
            const sh = new Shell();
            const wait = new Wait(sh);
            const kill = new Kill(sh);
            chat.tools.push(new Run(sh, { wait, kill, approve: false }), wait, kill);
            chat.push({ role: "user", content: "run something slow" });
            const r = await chat.stream();
            const reader = r.events.getReader();
            while (true) { const { done } = await reader.read(); if (done) break; }
            reader.releaseLock();
            await r.completed;
            await sh.close();
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            drive_one_cycle(&mut handle),
        )
        .await
        .expect("test should finish within 15s");
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();

        let gated: Vec<_> = pending
            .iter()
            .filter_map(|p| match p {
                frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                    call_id,
                    content,
                    is_error,
                } if call_id.starts_with("offscript-") => Some((content.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(gated.len(), 3, "expected 3 gated results, got {gated:?}");
        for (content, is_error) in &gated {
            assert!(
                content.contains("'read_file' is disabled"),
                "got `{content}`"
            );
            assert!(*is_error);
        }

        let scolds: Vec<_> = pending
            .iter()
            .filter_map(|p| match p {
                frances_models_llm::chat::OwnedHistoryInput::User { text }
                    if text.contains("still running") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(scolds.len(), 2, "expected 2 scolds, got {scolds:?}");

        let killed = pending.iter().any(|p| {
            matches!(
                p,
                frances_models_llm::chat::OwnedHistoryInput::User { text }
                    if text.contains("Killed the shell command")
            )
        });
        assert!(killed, "expected 'Killed' message in pending: {pending:?}");

        // Scold and kill notices should also be in the transcript.
        let scold_frames = frames
            .iter()
            .filter(|f| text_of(f).contains("still running"))
            .count();
        assert_eq!(
            scold_frames, 2,
            "expected 2 scold frames, got {scold_frames}: {frames:?}"
        );
        let kill_frame_present = frames
            .iter()
            .any(|f| text_of(f).contains("Killed the shell command"));
        assert!(
            kill_frame_present,
            "expected a kill notice frame: {frames:?}"
        );
    }

    #[tokio::test]
    async fn scope_lock_runs_after_batch_push() {
        // The post-batch turn registered via scope.lock fires AFTER all
        // initial tool_results have been pushed to chat history.
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "checker".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "checker".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            let turnRan = false;
            s.tools.push({
                name: "checker", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => { turnRan = true; });
                    return { role: "tool", call_id: call.id, content: "initial", is_error: false };
                },
            });
            s.push({ role: "user", content: "go" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: `turnRan=${turnRan}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(frames.last().unwrap()), "turnRan=true");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        assert!(pending.iter().any(|p| matches!(
            p,
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id, content, ..
            } if call_id == "c1" && content == "initial"
        )));
    }

    #[tokio::test]
    async fn scope_lock_turns_run_in_finish_order() {
        // Two tools register turns. "fast" finishes before "slow"; turns
        // run in finish order, not tool_calls order.
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "first".to_owned(),
                    name: "slow".to_owned(),
                    arguments: json!({}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "second".to_owned(),
                    name: "fast".to_owned(),
                    arguments: json!({}),
                }),
            ],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        error: None,
                        id: "first".to_owned(),
                        name: "slow".to_owned(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        error: None,
                        id: "second".to_owned(),
                        name: "fast".to_owned(),
                        arguments: json!({}),
                    },
                ],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            const turnOrder = [];
            s.tools.push({
                name: "slow", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => { turnOrder.push("slow"); });
                    for (let i = 0; i < 10; i++) await Promise.resolve();
                    return { role: "tool", call_id: call.id, content: "slow-done", is_error: false };
                },
            });
            s.tools.push({
                name: "fast", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => { turnOrder.push("fast"); });
                    return { role: "tool", call_id: call.id, content: "fast-done", is_error: false };
                },
            });
            s.push({ role: "user", content: "go" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: `turns=${turnOrder.join(",")}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(frames.last().unwrap()), "turns=fast,slow");
    }

    #[tokio::test]
    async fn scope_lock_turn_can_drive_followup_stream() {
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "starter".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "starter".to_owned(),
                    arguments: json!({}),
                }],
            },
        );
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c2".to_owned(),
                name: "followup".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c2".to_owned(),
                    name: "followup".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({
                name: "starter", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => {
                        const r = await scope.stream();
                        const reader = r.events.getReader();
                        while (true) { const { done } = await reader.read(); if (done) break; }
                        reader.releaseLock();
                        await r.completed;
                    });
                    return { role: "tool", call_id: call.id, content: "starter-done", is_error: false };
                },
            });
            s.tools.push({
                name: "followup", description: "", parameters: { type: "object" },
                handler: async ({ call }) => ({
                    role: "tool", call_id: call.id, content: "followup-done", is_error: false,
                }),
            });
            s.push({ role: "user", content: "go" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let tool_results: Vec<(String, String)> = pending
            .iter()
            .filter_map(|p| match p {
                frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                    call_id,
                    content,
                    ..
                } => Some((call_id.clone(), content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_results,
            vec![
                ("c1".to_owned(), "starter-done".to_owned()),
                ("c2".to_owned(), "followup-done".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn scope_lock_gating_hook_scolds_off_script_calls() {
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "outer".to_owned(),
                name: "gated".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "outer".to_owned(),
                    name: "gated".to_owned(),
                    arguments: json!({}),
                }],
            },
        );
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "offscript".to_owned(),
                name: "forbidden".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "offscript".to_owned(),
                    name: "forbidden".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({
                name: "gated", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => {
                        scope.toolCall = async ({ call: c, invoke }) => {
                            throw new Error(`'${c.name}' is disabled`);
                        };
                        const r = await scope.stream();
                        const reader = r.events.getReader();
                        while (true) { const { done } = await reader.read(); if (done) break; }
                        reader.releaseLock();
                        await r.completed;
                    });
                    return { role: "tool", call_id: call.id, content: "ok", is_error: false };
                },
            });
            s.tools.push({
                name: "forbidden", description: "", parameters: { type: "object" },
                handler: async ({ call }) => ({
                    role: "tool", call_id: call.id, content: "should-not-run", is_error: false,
                }),
            });
            s.push({ role: "user", content: "go" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let scolded = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            } if call_id == "offscript" => Some((content.clone(), *is_error)),
            _ => None,
        });
        let (content, is_error) = scolded.expect("scolded result present");
        assert!(
            content.contains("'forbidden' is disabled"),
            "got `{content}`"
        );
        assert!(is_error);
    }

    #[tokio::test]
    async fn scope_lock_double_register_throws() {
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "double".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "double".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({
                name: "double", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => {});
                    let caught = "no-throw";
                    try {
                        scope.lock(async () => {});
                    } catch (e) {
                        caught = String(e);
                    }
                    return {
                        role: "tool", call_id: call.id,
                        content: caught.includes("already registered") ? "got-throw" : caught,
                        is_error: false,
                    };
                },
            });
            s.push({ role: "user", content: "go" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let tool = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        });
        assert_eq!(tool, Some("got-throw".to_owned()));
    }

    #[tokio::test]
    async fn scope_lock_turn_fn_throw_does_not_crash_round() {
        use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "thrower".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
                    id: "c1".to_owned(),
                    name: "thrower".to_owned(),
                    arguments: json!({}),
                }],
            },
        );

        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({
                name: "thrower", description: "", parameters: { type: "object" },
                handler: async ({ call, scope }) => {
                    scope.lock(async () => { throw new Error("boom"); });
                    return { role: "tool", call_id: call.id, content: "initial", is_error: false };
                },
            });
            s.push({ role: "user", content: "go" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownSection({ content: "survived" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(frames.last().unwrap()), "survived");

        let sessions = deps.sessions();
        let pending = sessions[0].pending();
        let synthetic = pending.iter().find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::User { text }
                if text.contains("threw") =>
            {
                Some(text.clone())
            }
            _ => None,
        });
        assert!(
            synthetic.is_some(),
            "expected synthetic user message in pending: {pending:?}"
        );
    }

    #[tokio::test]
    async fn markdown_frame_writable_is_stable_writable_stream() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { WritableStream } from "whatwg:web-streams";
            const f = new MarkdownSection({ content: "hi" });
            transcript.push(f);
            const w1 = f.writable;
            const w2 = f.writable;
            const shape = `ws=${w1 instanceof WritableStream} stable=${w1 === w2} hasWrite=${typeof MarkdownSection.prototype.write}`;
            transcript.push(new MarkdownSection({ content: shape }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        // The raw `write` is removed from the prototype — only the
        // WritableStream is the public path.
        let last = text_of(frames.last().expect("at least one frame"));
        assert_eq!(last, "ws=true stable=true hasWrite=undefined");
    }

    #[tokio::test]
    async fn timer_fires_after_interval() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const start = Date.now();
            await new Timer(20);
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownSection({ content: elapsed >= 15 ? "ok" : `too fast: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "ok");
    }

    #[tokio::test]
    async fn timer_fire_resolves_pending_await() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);  // long enough that the test would hang if fire() didn't work
            queueMicrotask(() => t.fire());
            const start = Date.now();
            await t;
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownSection({ content: elapsed < 1000 ? "fast" : `slow: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "fast");
    }

    #[tokio::test]
    async fn timer_disable_then_fire_wakes_await() {
        // `disable()` pauses the timer (no auto-firing). `fire()`
        // still works — that's the manual-trigger mode the user asked
        // for. Without the fire(), the await would suspend forever
        // (and `drive_one_cycle` would time out).
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            queueMicrotask(() => {
                t.disable();
                t.fire();
            });
            const start = Date.now();
            await t;
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownSection({
                content: elapsed < 1000 ? "fast" : `slow: ${elapsed}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "fast");
    }

    #[tokio::test]
    async fn timer_reject_preserves_error_identity() {
        // Rejection identity is now preserved verbatim — the caught
        // value IS the original Error, not a wrapped copy.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            const original = new Error("nope");
            queueMicrotask(() => t.reject(original));
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownSection({
                    content: `same=${e === original} msg=${e.message}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "same=true msg=nope");
    }

    #[tokio::test]
    async fn timer_rejected_is_terminal() {
        // After reject(), every mutating method throws. Only the
        // construction of a fresh Timer can escape it.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer, TimerError } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            t.reject(new Error("done"));
            const results = [];
            for (const op of [
                ["reject", () => t.reject(new Error("again"))],
                ["disable", () => t.disable()],
                ["enable", () => t.enable()],
                ["fire", () => t.fire()],
                ["set", () => t.set({ delay: 1 })],
            ]) {
                try { op[1](); results.push(`${op[0]}: no-throw`); }
                catch (e) { results.push(`${op[0]}: threw`); }
            }
            transcript.push(new MarkdownSection({ content: results.join("; ") }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "reject: threw; disable: threw; enable: threw; fire: threw; set: threw"
        );
    }

    #[tokio::test]
    async fn timer_reject_with_timer_error_is_instance() {
        // When the caller explicitly rejects with a TimerError, the
        // identity is preserved and `instanceof TimerError` holds. We
        // no longer auto-wrap arbitrary rejections.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer, TimerError } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            queueMicrotask(() => t.reject(new TimerError("boom")));
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownSection({
                    content: `te=${e instanceof TimerError} err=${e instanceof Error} msg=${e.message}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "te=true err=true msg=boom");
    }

    #[tokio::test]
    async fn timer_reject_with_no_arg_rejects_with_default_timer_error() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            queueMicrotask(() => t.reject());
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownSection({
                    content: `caught: error=${e instanceof Error} name=${e.name} msg=${e.message}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "caught: error=true name=TimerError msg=timer rejected"
        );
    }

    #[tokio::test]
    async fn timer_disable_then_enable_revives() {
        // `enable()` re-applies the schedule (clearing `fired_once`),
        // so a disabled timer can be brought back without `set(...)`.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer({ delay: 10 });
            t.disable();
            t.enable();
            await t;
            transcript.push(new MarkdownSection({ content: t.enabled ? "enabled" : "still-off" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "enabled");
    }

    #[tokio::test]
    async fn timer_getters_reflect_schedule_and_state() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer({ delay: 100, interval: 50 });
            const before = `enabled=${t.enabled} delay=${t.delay} interval=${t.interval}`;
            t.disable();
            const after = `enabled=${t.enabled} delay=${t.delay} interval=${t.interval}`;
            transcript.push(new MarkdownSection({ content: `${before} | ${after}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        // Schedule survives disable() — the getters still report it.
        assert_eq!(
            text_of(&frames[0]),
            "enabled=true delay=100 interval=50 | enabled=false delay=100 interval=50"
        );
    }

    #[tokio::test]
    async fn timer_repeat_ticks_multiple_times() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const tick = new Timer({ interval: 5 });
            let count = 0;
            for (let i = 0; i < 3; i += 1) { await tick; count += 1; }
            tick.disable();
            transcript.push(new MarkdownSection({ content: `count=${count}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "count=3");
    }

    #[tokio::test]
    async fn timer_non_repeat_second_await_resolves_immediately() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(10);
            await t;
            const start = Date.now();
            await t;  // already fired — no wait
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownSection({ content: elapsed < 5 ? "instant" : `slow: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "instant");
    }

    #[tokio::test]
    async fn timer_constructor_rejects_garbage() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            new Timer("nope");
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn timer_object_delay_form() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            await new Timer({ delay: 5 });
            transcript.push(new MarkdownSection({ content: "fired" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "fired");
    }

    #[tokio::test]
    async fn timer_delay_then_interval_combo() {
        // `{ delay, interval }` should wait `delay` before the first
        // fire, then `interval` between subsequent fires.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const tick = new Timer({ delay: 30, interval: 5 });
            const t0 = Date.now();
            await tick;
            const first = Date.now() - t0;
            await tick;
            const second = Date.now() - t0;
            await tick;
            const third = Date.now() - t0;
            tick.disable();
            const ok = first >= 25 && (second - first) < 25 && (third - second) < 25;
            transcript.push(new MarkdownSection({ content: ok ? "ok" : `bad: ${first} ${second} ${third}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "ok");
    }

    #[tokio::test]
    async fn timer_object_needs_delay_or_interval() {
        // Empty object is rejected — must carry at least one field.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            new Timer({});
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn timer_set_after_cancel_reuses_timer() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            t.disable();
            // Cancelled — without set(), the next await would reject.
            t.set({ delay: 10 });
            await t;
            transcript.push(new MarkdownSection({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "ok");
    }

    #[tokio::test]
    async fn timer_set_changes_schedule_and_resets_fired_once() {
        // One-shot fires, then set() flips it to repeating; subsequent
        // awaits must actually wait (proving fired_once was cleared).
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer({ delay: 5 });
            await t;             // fires, fired_once = true
            t.set({ interval: 15 });
            const t0 = Date.now();
            await t;
            await t;
            const elapsed = Date.now() - t0;
            transcript.push(new MarkdownSection({ content: elapsed >= 25 ? "ok" : `too fast: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "ok");
    }

    #[tokio::test]
    async fn timer_set_rejects_empty_args() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            const t = new Timer(10);
            t.set({});
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn timer_exit_unblocks_pending_await() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            import { exit } from "frances:v1/workflow";
            const t = new Timer(60_000);
            queueMicrotask(() => exit());
            await t;  // should resolve when the workflow closes, not reject
            transcript.push(new MarkdownSection({ content: "after-await" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "after-await");
    }

    #[tokio::test]
    async fn timer_reject_with_object_preserves_identity() {
        // Non-Error rejection values are also preserved verbatim — no
        // string coercion, no auto-wrapping.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const t = new Timer(60_000);
            const payload = { kind: "custom", n: 42 };
            queueMicrotask(() => t.reject(payload));
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownSection({
                    content: `same=${e === payload} kind=${e.kind} n=${e.n}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "same=true kind=custom n=42");
    }

    #[tokio::test]
    async fn timer_signal_already_aborted_rejects_immediately() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const ac = new AbortController();
            ac.abort("pre-aborted");
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            const start = Date.now();
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                const elapsed = Date.now() - start;
                transcript.push(new MarkdownSection({
                    content: `caught=${e} fast=${elapsed < 100}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "caught=pre-aborted fast=true");
    }

    #[tokio::test]
    async fn timer_signal_aborts_mid_wait() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const ac = new AbortController();
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            queueMicrotask(() => ac.abort(new Error("user cancelled")));
            const start = Date.now();
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                const elapsed = Date.now() - start;
                transcript.push(new MarkdownSection({
                    content: `err=${e instanceof Error} msg=${e.message} fast=${elapsed < 1000}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "err=true msg=user cancelled fast=true");
    }

    #[tokio::test]
    async fn timer_signal_reason_preserved_verbatim() {
        // The rejection IS signal.reason, by identity — not a wrapped
        // copy. Mirrors WHATWG fetch semantics.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const ac = new AbortController();
            const reason = { kind: "signal-reason", id: 7 };
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            queueMicrotask(() => ac.abort(reason));
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownSection({
                    content: `same=${e === reason} kind=${e.kind} id=${e.id}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "same=true kind=signal-reason id=7");
    }

    #[tokio::test]
    async fn timer_signal_listener_removed_on_terminal() {
        // After the timer settles via reject(), an abort on the
        // original signal must not double-fire on the timer — the
        // listener should have been removed at terminal transition.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer, TimerError } from "frances:v1/io";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const ac = new AbortController();
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            t.reject(new Error("manual"));
            // After reject, the timer is terminal. Aborting the signal
            // should not throw / not mutate anything observable.
            ac.abort("late");
            try {
                await t;
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                // We rejected with our own Error before abort fired —
                // the late abort must not have replaced the reason.
                transcript.push(new MarkdownSection({
                    content: `msg=${e.message} aborted=${ac.signal.aborted}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "msg=manual aborted=true");
    }

    #[tokio::test]
    async fn timer_non_signal_object_rejected_by_constructor() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            new Timer({ delay: 10, signal: { aborted: false } });  // missing addEventListener
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    // ---- whatwg:* smoke tests --------------------------------------------
    //
    // These verify the module-library wiring (the import resolves, the
    // named exports are present), not the polyfill internals. The
    // polyfill upstreams have their own test suites; we just care that
    // our virtual-module declaration didn't fumble.

    #[tokio::test]
    async fn whatwg_dom_exports_dom_exception() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { DOMException } from "whatwg:dom";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const e = new DOMException("nope", "AbortError");
            transcript.push(new MarkdownSection({
                content: `err=${e instanceof Error} name=${e.name} msg=${e.message}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "err=true name=AbortError msg=nope");
    }

    #[tokio::test]
    async fn whatwg_web_streams_exports_constructors() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import {
                ReadableStream,
                WritableStream,
                TransformStream,
            } from "whatwg:web-streams";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const shape = [
                typeof ReadableStream,
                typeof WritableStream,
                typeof TransformStream,
            ].join(",");
            transcript.push(new MarkdownSection({ content: shape }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "function,function,function");
    }

    #[tokio::test]
    async fn whatwg_abortcontroller_basic_lifecycle() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { AbortController, AbortSignal } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const ac = new AbortController();
            const before = ac.signal.aborted;
            let fired = false;
            ac.signal.addEventListener("abort", () => { fired = true; });
            ac.abort("nope");
            const after = ac.signal.aborted;
            const reason = ac.signal.reason;
            const isSignal = ac.signal instanceof AbortSignal;
            transcript.push(new MarkdownSection({
                content: `before=${before} after=${after} fired=${fired} reason=${reason} sig=${isSignal}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "before=false after=true fired=true reason=nope sig=true"
        );
    }

    #[tokio::test]
    async fn abortsignal_timeout_fires_after_delay() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { AbortSignal } from "whatwg:abortcontroller";
            import { DOMException } from "whatwg:dom";
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const start = Date.now();
            const s = AbortSignal.timeout(15);
            // Wait long enough for the timeout to fire.
            await new Timer(60);
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownSection({
                content: `aborted=${s.aborted} name=${s.reason && s.reason.name} dom=${s.reason instanceof DOMException} fast=${elapsed < 200}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "aborted=true name=TimeoutError dom=true fast=true"
        );
    }

    #[tokio::test]
    async fn abortsignal_timeout_composes_with_timer_signal() {
        // The whole point of the primitive split: an AbortSignal.timeout
        // signal can be fed directly into a Timer's `signal:` option.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { AbortSignal } from "whatwg:abortcontroller";
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const start = Date.now();
            try {
                await new Timer({ delay: 60_000, signal: AbortSignal.timeout(15) });
                transcript.push(new MarkdownSection({ content: "BUG: resolved" }));
            } catch (e) {
                const elapsed = Date.now() - start;
                transcript.push(new MarkdownSection({
                    content: `name=${e.name} msg=${e.message} fast=${elapsed < 1000}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "name=TimeoutError msg=signal timed out fast=true"
        );
    }

    #[tokio::test]
    async fn abortsignal_timeout_rejects_garbage() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { AbortSignal } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const cases = ["nope", -5, NaN, undefined, {}];
            const threw = cases.map((c) => {
                try { AbortSignal.timeout(c); return "no-throw"; }
                catch (_) { return "threw"; }
            });
            transcript.push(new MarkdownSection({ content: threw.join(",") }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "threw,threw,threw,threw,threw");
    }

    #[tokio::test]
    async fn abortsignal_any_first_source_wins_and_listener_cleaned_up() {
        // The combined signal aborts on the first source. The cleanup
        // path must remove the listener from BOTH sources, so a later
        // abort on the second source does not overwrite `out.reason`.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { AbortController, AbortSignal } from "whatwg:abortcontroller";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const a = new AbortController();
            const b = new AbortController();
            const out = AbortSignal.any([a.signal, b.signal]);
            a.abort("first");
            const reasonAfterFirst = out.reason;
            // If listeners weren't cleaned up, the second abort would
            // run propagate again and (no-op since out is already
            // aborted, but it would still re-fire the listener walk).
            // The observable signal: out.reason must stay "first".
            b.abort("second");
            transcript.push(new MarkdownSection({
                content: `first=${reasonAfterFirst} after=${out.reason} aborted=${out.aborted}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "first=first after=first aborted=true");
    }

    /// `approve()` emits a HostFrame::Permission and awaits the host's
    /// response. Unlike `inbox.next()` it does not park, so we drive
    /// the body inline: wait for the first approval frame, answer it,
    /// then drain to `done`.
    #[tokio::test]
    async fn approve_yes_round_trip() {
        let deps = StubDeps::default();
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { approve } from "frances:v1/approval";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const choice = await approve({ prompt: "delete /tmp/foo?" });
            transcript.push(new MarkdownSection({
                content: `${choice.type}:${choice.details ?? ""}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();

        let req = tokio::time::timeout(CYCLE_TIMEOUT, async {
            loop {
                if let Some(req) = handle.outputs.permissions.recv().await {
                    return req;
                }
            }
        })
        .await
        .expect("approval frame did not arrive in time");
        assert_eq!(req.prompt, "delete /tmp/foo?");
        assert!(!req.allow_auto, "default allowAuto should be false");

        assert!(
            req.reply
                .send(PermissionResponse::Yes {
                    details: Some("scoped to /tmp".into()),
                })
                .is_ok(),
            "answer should land on the embedded reply slot",
        );

        let result = tokio::time::timeout(CYCLE_TIMEOUT, &mut handle.done)
            .await
            .expect("workflow did not finish in time")
            .expect("done channel closed without value");
        assert!(matches!(result, Ok(())), "got {result:?}");

        let mut deltas = Vec::new();
        while let Ok(d) = handle.outputs.transcript.try_recv() {
            deltas.push(d);
        }
        let last = deltas
            .iter()
            .rev()
            .find_map(|d| match d {
                SectionTranscript::Set { section, .. } => Some(section),
                _ => None,
            })
            .expect("expected a markdown set after approval");
        assert!(
            matches!(&last.kind, SectionKind::Markdown { .. })
                && last.seed.as_deref() == Some("yes:scoped to /tmp"),
            "got {last:?}",
        );
    }

    /// Non-string prompts throw a TypeError before any frame is emitted.
    #[tokio::test]
    async fn approve_rejects_non_string_prompt() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { approve } from "frances:v1/approval";
            await approve(42);
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }

    // --- `complete` direct export -----------------------------------------

    fn outcome(
        text: &str,
        tool_calls: Vec<frances_models_llm::ToolCall>,
    ) -> frances_models_llm::CompletionOutcome {
        frances_models_llm::CompletionOutcome {
            text: text.to_owned(),
            tool_calls,
        }
    }

    fn decide_call() -> frances_models_llm::ToolCall {
        frances_models_llm::ToolCall {
            error: None,
            id: "c1".into(),
            name: "decide".into(),
            arguments: serde_json::json!({ "verdict": "approve" }),
        }
    }

    #[tokio::test]
    async fn complete_plain_returns_text() {
        let deps = StubDeps::default();
        deps.script_next_run(Vec::new(), outcome("the answer", Vec::new()));
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { complete } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const r = await complete({
                intents: ["default"],
                input: [{ role: "user", content: "hi" }],
            });
            transcript.push(new MarkdownSection({ content: r.text }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(text_of(&frames[0]), "the answer");
    }

    #[tokio::test]
    async fn complete_required_returns_tool_call() {
        let deps = StubDeps::default();
        deps.script_next_run(Vec::new(), outcome("", vec![decide_call()]));
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { complete } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const r = await complete({
                intents: ["default"],
                input: [{ role: "user", content: "decide please" }],
                tools: [{ name: "decide", description: "d", parameters: { type: "object" } }],
                requireToolCall: true,
            });
            transcript.push(new MarkdownSection({ content: r.tool_calls.map((c) => c.name).join(",") }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(text_of(&frames[0]), "decide");
    }

    #[tokio::test]
    async fn complete_enforced_retries_then_succeeds() {
        let deps = StubDeps::default();
        // Round 1: no tool call → scold. Round 2: the demanded call.
        deps.script_next_run(Vec::new(), outcome("thinking", Vec::new()));
        deps.script_next_run(Vec::new(), outcome("", vec![decide_call()]));
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { complete } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const r = await complete({
                intents: ["default"],
                input: [{ role: "user", content: "decide" }],
                toolChoice: "decide",
            });
            transcript.push(new MarkdownSection({ content: r.tool_calls.map((c) => c.name).join(",") }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(text_of(&frames[0]), "decide");
    }

    #[tokio::test]
    async fn complete_unsatisfied_rejects() {
        let deps = StubDeps::default();
        // retries defaults to 1 ⇒ two rounds, neither calls a tool.
        deps.script_next_run(Vec::new(), outcome("nope", Vec::new()));
        deps.script_next_run(Vec::new(), outcome("still nope", Vec::new()));
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { complete } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            try {
                await complete({
                    intents: ["default"],
                    input: [{ role: "user", content: "decide" }],
                    requireToolCall: true,
                });
                transcript.push(new MarkdownSection({ content: "NO THROW" }));
            } catch (e) {
                transcript.push(new MarkdownSection({ content: "threw:" + String((e && e.message) || e) }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(result.is_ok(), "got {result:?}");
        let rendered = text_of(&frames[0]);
        assert!(
            rendered.starts_with("threw:") && rendered.contains("forced tool not satisfied"),
            "expected an enforce rejection, got {rendered:?}",
        );
    }

    #[tokio::test]
    async fn complete_flags_schema_invalid_tool_call() {
        let deps = StubDeps::default();
        // `decide_call` supplies only `verdict`; the schema also requires
        // `reason`, so the chat layer flags the call.
        deps.script_next_run(Vec::new(), outcome("", vec![decide_call()]));
        let rt = Runtime::new(deps.clone()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { complete } from "frances:v1/chat";
            import { transcript, MarkdownSection } from "frances:v1/sections";
            const r = await complete({
                intents: ["default"],
                input: [{ role: "user", content: "decide" }],
                tools: [{ name: "decide", description: "d", parameters: {
                    type: "object",
                    additionalProperties: false,
                    properties: { verdict: { type: "string" }, reason: { type: "string" } },
                    required: ["verdict", "reason"],
                } }],
            });
            const c = r.tool_calls[0];
            const ok = c.error && c.expectedSchema && c.expectedSchema.required.includes("reason");
            transcript.push(new MarkdownSection({ content: ok ? "flagged:" + c.name : "clean" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, result) = drive_to_done(&mut handle).await;
        assert!(result.is_ok(), "got {result:?}");
        assert_eq!(text_of(&frames[0]), "flagged:decide");
    }
}
