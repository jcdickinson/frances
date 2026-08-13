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
//! - `import { ChatSession } from "frances:v1/chat"`
//! - `import.meta.args` — per-invocation slash-command args.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
use crate::closed::WorkflowClosed;
use crate::deps::WorkflowDeps;
use crate::modules;
use crate::permission::PermissionRequest;
use crate::transpile::{SourceKind, ts_to_js};

/// Internal name we declare the user script under.
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
/// `ClearFooter` removes it. This grows a `Region`/`ViewNode` vocabulary
/// only when a richer surface (panel, plan-editor) actually appears.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SurfaceCmd {
    /// Show `text` (with a spinner) in the footer busy indicator.
    SetFooter { text: String },
    /// Hide the footer busy indicator.
    ClearFooter,
    /// Set (`Some`) or clear (`None`) the session title. Unlike the
    /// footer this one outlives the workflow: the driver persists it
    /// into session metadata before forwarding it to the UI.
    SetTitle { title: Option<String> },
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
    /// is never persisted (the UI drops it during replay).
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

pub use frances_models_ui::{ReasoningState, SectionId, SectionKind, ShellState, Source};

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
/// user message or an out-of-band interrupt request (Esc in the UI).
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
    /// selection. The session runtime currently passes the session id so a
    /// restored instance reads the same value out of `import.meta.instance`.
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
        js.set_loader(modules::EmbeddedResolver, modules::EmbeddedLoader)
            .await;
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
    let source = tokio::fs::read_to_string(&inv.source_path)
        .await
        .map_err(WorkflowError::ReadSource)?;
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
        closed: Arc::new(WorkflowClosed::default()),
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
                closed_for_shutdown.close();
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
                // Best-effort: run the hook (sync or async — `MaybePromise`
                // handles both) and log on failure rather than failing the
                // whole workflow.
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
    context: &'static str,
) -> impl FnOnce(rquickjs::CaughtError<'js>) -> WorkflowError {
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
        ChatError, ChatSession, ChatSessionBuilder, ChatSessionId, ChatSessionManager,
        OwnedHistoryInput,
    };
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolChoice, ToolDef};
    use frances_storage::{EntitySchema, Migration};
    use parking_lot::Mutex;
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
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

    #[derive(Clone)]
    pub struct StubDeps {
        manager: StubManager,
        io: DefaultIo,
        editor_factory: StubEditorFactory,
        cwd: Arc<Mutex<Option<PathBuf>>>,
        storage: StubStorage,
        editable_roots: Vec<PathBuf>,
    }

    impl Default for StubDeps {
        fn default() -> Self {
            Self {
                manager: StubManager::default(),
                io: DefaultIo::default(),
                editor_factory: StubEditorFactory::default(),
                cwd: Arc::new(Mutex::new(None)),
                storage: StubStorage::default(),
                editable_roots: vec![PathBuf::from("/")],
            }
        }
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

        /// Overrides the editable roots. Useful for integration tests that
        /// need to make certain paths "out-of-repo".
        pub fn set_editable_roots(&mut self, roots: Vec<PathBuf>) {
            self.editable_roots = roots;
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

        fn current_env(&self) -> Arc<HashMap<OsString, OsString>> {
            Arc::new(HashMap::new())
        }

        fn current_cwd(&self) -> Option<PathBuf> {
            self.cwd.lock().clone()
        }

        fn session_title(&self) -> Option<String> {
            None
        }

        fn editable_roots(&self) -> &[PathBuf] {
            &self.editable_roots
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
        editable_roots: Vec<PathBuf>,
    }

    impl Default for StubDepsRealShell {
        fn default() -> Self {
            Self {
                manager: StubManager::default(),
                io: DefaultIo::with_real_shell(),
                editor_factory: StubEditorFactory::default(),
                storage: StubStorage::default(),
                editable_roots: vec![PathBuf::from("/")],
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

        fn current_env(&self) -> Arc<HashMap<OsString, OsString>> {
            Arc::new(HashMap::new())
        }

        fn current_cwd(&self) -> Option<PathBuf> {
            None
        }

        fn session_title(&self) -> Option<String> {
            None
        }

        fn editable_roots(&self) -> &[PathBuf] {
            &self.editable_roots
        }

        async fn workflow_db(
            &self,
            entity: Uuid,
            migrations: Cow<'_, [Migration]>,
        ) -> Result<Arc<WorkflowDb>, WorkflowDbError> {
            self.storage.workflow_db(entity, migrations).await
        }
    }

    /// Test editor factory: hands out fresh in-memory read contexts over a
    /// shared `FakeStore`-backed engine. Each clone shares the engine (so
    /// anchors persist), but every `new_session` gets its own read cache.
    #[derive(Clone)]
    pub struct StubEditorFactory {
        engine: Arc<EditEngine<FakeStore>>,
    }

    impl Default for StubEditorFactory {
        fn default() -> Self {
            Self {
                engine: Arc::new(EditEngine::new(FakeStore::new())),
            }
        }
    }

    impl EditorFactory for StubEditorFactory {
        type Store = FakeStore;

        fn new_session(&self) -> EditSession<FakeStore> {
            EditSession::new(self.engine.clone())
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
                id: Arc::new(Mutex::new(None)),
                effort: Arc::new(Mutex::new(None)),
                pending: Arc::new(Mutex::new(Vec::new())),
                next_script: self.next_script.clone(),
            };
            self.sessions.lock().push(session.clone());
            session
        }

        async fn load(&self, id: ChatSessionId) -> Result<Self::Session, ChatError> {
            let session = StubSession {
                effort: Arc::new(Mutex::new(None)),
                id: Arc::new(Mutex::new(Some(id))),
                pending: Arc::new(Mutex::new(Vec::new())),
                next_script: self.next_script.clone(),
            };
            self.sessions.lock().push(session.clone());
            Ok(session)
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
        id: Arc<Mutex<Option<ChatSessionId>>>,
        effort: Arc<Mutex<Option<frances_models_llm::NormalizedEffort>>>,
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
        fn id(&self) -> Option<ChatSessionId> {
            *self.id.lock()
        }

        fn effort(&self) -> Option<frances_models_llm::NormalizedEffort> {
            *self.effort.lock()
        }

        fn set_effort(&self, effort: Option<frances_models_llm::NormalizedEffort>) {
            *self.effort.lock() = effort;
        }

        async fn ensure_persisted(&self) -> Result<Option<ChatSessionId>, ChatError> {
            let mut id = self.id.lock();
            if id.is_none() {
                *id = Some(ChatSessionId(1));
            }
            Ok(*id)
        }

        fn push(&self, input: OwnedHistoryInput) {
            self.pending.lock().push(input);
        }

        fn push_system(&self, input: OwnedHistoryInput) {
            let mut pending = self.pending.lock();
            let pos = pending
                .iter()
                .rposition(|m| matches!(m, OwnedHistoryInput::System { .. }))
                .map_or(0, |i| i + 1);
            pending.insert(pos, input);
        }

        async fn run(
            &self,
            _env: Arc<HashMap<OsString, OsString>>,
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
    }
}

#[cfg(test)]
mod tests;
