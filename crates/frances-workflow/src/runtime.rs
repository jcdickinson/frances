//! Script runtime.
//!
//! Each call to [`Runtime::start`] creates a fresh [`AsyncContext`],
//! evaluates the workflow module's top-level body, and tears the context
//! down when the body settles or `workflow.exit()` is called. Module
//! state does not persist across invocations.
//!
//! The JS-side API surfaces under a single `workflow` global, plus
//! per-invocation args at `import.meta.args`:
//!
//! - `workflow.frame.{text, error, json}` — emit a [`HostFrame`].
//! - `workflow.user.input` — async-iterable user-input stream.
//! - `workflow.exit()` — explicit termination.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::async_with;
use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::context::AsyncContext;
use rquickjs::function::{Constructor, This};
use rquickjs::module::Module;
use rquickjs::promise::Promised;
use rquickjs::runtime::AsyncRuntime;
use rquickjs::{
    CatchResultExt, Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio::task::JoinHandle;

use crate::WorkflowError;
use crate::transpile::{SourceKind, ts_to_js};

/// Frames a workflow can emit during a turn. The daemon receiver maps
/// these onto the wire `StreamFrame` protocol; this enum is the
/// host-API contract, not the protocol itself.
#[derive(Debug, Clone)]
pub enum HostFrame {
    Text(String),
    Error(String),
    Json {
        tag: String,
        value: serde_json::Value,
    },
}

/// A single user input event delivered to `workflow.user.input`.
#[derive(Debug, Clone)]
pub struct UserInput {
    pub message: String,
}

impl<'js> IntoJs<'js> for UserInput {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("message", self.message)?;
        Ok(obj.into_value())
    }
}

/// Inputs the daemon supplies for one workflow invocation.
pub struct Invocation {
    pub source_path: PathBuf,
    pub args: Vec<String>,
}

/// Handle to a running workflow. The daemon owns this; it delivers user
/// input via [`Self::input_tx`], drains frames from [`Self::frames`],
/// and learns about lifecycle transitions through [`Self::parked`] and
/// [`Self::done`].
pub struct WorkflowHandle {
    /// Send user input to the workflow's `workflow.user.input` stream.
    pub input_tx: UnboundedSender<UserInput>,
    /// Frames the workflow emits.
    pub frames: UnboundedReceiver<HostFrame>,
    /// Notified each time the body suspends on `workflow.user.input.next()`
    /// with an empty queue — i.e. the body is parked waiting for input.
    /// One pulse per park; the daemon uses this to detect end-of-cycle.
    pub parked: Arc<Notify>,
    /// Resolves when the workflow terminates (body settled or `exit()`
    /// called). The inner result mirrors the body's outcome.
    pub done: oneshot::Receiver<Result<(), WorkflowError>>,
    /// Owns the spawned task; dropping the handle aborts the workflow.
    _join: JoinHandle<()>,
}

/// Workflow script runtime. One per daemon; cheap to share via `Arc`.
pub struct Runtime {
    js: AsyncRuntime,
    transpile_cache: Arc<StdMutex<TranspileCache>>,
}

#[derive(Default)]
struct TranspileCache {
    /// Source-hash → transpiled JS. xxhash3_64 of the on-disk bytes.
    by_hash: std::collections::HashMap<u64, Arc<str>>,
}

impl Runtime {
    pub fn new() -> Result<Self, WorkflowError> {
        let js = AsyncRuntime::new().map_err(script_err)?;
        Ok(Self {
            js,
            transpile_cache: Arc::new(StdMutex::new(TranspileCache::default())),
        })
    }

    /// Start a workflow. Reads + transpiles the source synchronously,
    /// spawns the body on a Tokio task, and returns a handle the
    /// daemon uses to drive it.
    pub fn start(&self, inv: Invocation) -> Result<WorkflowHandle, WorkflowError> {
        let source =
            std::fs::read_to_string(&inv.source_path).map_err(WorkflowError::ReadSource)?;
        let js_source = match SourceKind::from_path(&inv.source_path) {
            SourceKind::JavaScript => source,
            SourceKind::TypeScript => self.transpile(&inv.source_path, &source)?,
        };

        let (input_tx, input_rx) = mpsc::unbounded_channel::<UserInput>();
        let (frames_tx, frames_rx) = mpsc::unbounded_channel::<HostFrame>();
        let (done_tx, done_rx) = oneshot::channel::<Result<(), WorkflowError>>();
        let parked = Arc::new(Notify::new());

        let task_input_rx = Arc::new(AsyncMutex::new(input_rx));
        let task_closed = Arc::new(AtomicBool::new(false));
        let task_closed_notify = Arc::new(Notify::new());
        let task_parked = parked.clone();
        let task_frames = frames_tx;
        let task_args = inv.args;
        let task_js = self.js.clone();

        let join = tokio::spawn(async move {
            let result = run_workflow(
                task_js,
                js_source,
                task_args,
                task_frames,
                task_input_rx,
                task_closed,
                task_closed_notify,
                task_parked,
            )
            .await;
            let _ = done_tx.send(result);
        });

        Ok(WorkflowHandle {
            input_tx,
            frames: frames_rx,
            parked,
            done: done_rx,
            _join: join,
        })
    }

    fn transpile(&self, path: &Path, source: &str) -> Result<String, WorkflowError> {
        let hash = twox_hash::XxHash3_64::oneshot(source.as_bytes());
        if let Some(cached) = self
            .transpile_cache
            .lock()
            .expect("transpile cache poisoned")
            .by_hash
            .get(&hash)
            .cloned()
        {
            return Ok(cached.to_string());
        }
        let js = ts_to_js(path, source)?;
        self.transpile_cache
            .lock()
            .expect("transpile cache poisoned")
            .by_hash
            .insert(hash, Arc::<str>::from(js.as_str()));
        Ok(js)
    }
}

/// Internal module name we register every workflow source under.
const MODULE_NAME: &str = "frances:workflow";

#[expect(
    clippy::too_many_arguments,
    reason = "shared task-local state, packing buys nothing"
)]
async fn run_workflow(
    js: AsyncRuntime,
    js_source: String,
    args: Vec<String>,
    frames: UnboundedSender<HostFrame>,
    input_rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
) -> Result<(), WorkflowError> {
    let context = AsyncContext::full(&js).await.map_err(script_err)?;

    async_with!(context => |ctx| {
        install_workflow_globals(
            &ctx,
            frames,
            input_rx,
            closed.clone(),
            closed_notify.clone(),
            parked,
        )
        .catch(&ctx)
        .map_err(|e| script_err(format!("{e}")))?;

        let module = Module::declare(ctx.clone(), MODULE_NAME, js_source.as_bytes())
            .catch(&ctx)
            .map_err(|e| script_err(format!("{e}")))?;
        let meta = module
            .meta()
            .catch(&ctx)
            .map_err(|e| script_err(format!("{e}")))?;
        meta.set("args", args)
            .catch(&ctx)
            .map_err(|e| script_err(format!("{e}")))?;

        let (_module, promise) = module
            .eval()
            .catch(&ctx)
            .map_err(|e| script_err(format!("{e}")))?;
        promise
            .into_future::<()>()
            .await
            .catch(&ctx)
            .map_err(|e| script_err(format!("{e}")))?;
        Ok::<(), WorkflowError>(())
    })
    .await
}

fn install_workflow_globals<'js>(
    ctx: &Ctx<'js>,
    frames: UnboundedSender<HostFrame>,
    input_rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
) -> JsResult<()> {
    let frame_obj = build_frame_object(ctx, frames)?;
    let input_class = Class::instance(
        ctx.clone(),
        WorkflowInput {
            rx: input_rx,
            closed: closed.clone(),
            closed_notify: closed_notify.clone(),
            parked,
        },
    )?;
    let user_obj = Object::new(ctx.clone())?;
    user_obj.set("input", input_class)?;

    let workflow = Object::new(ctx.clone())?;
    workflow.set("frame", frame_obj)?;
    workflow.set("user", user_obj)?;
    workflow.set("exit", build_exit_fn(ctx, closed, closed_notify)?)?;

    ctx.globals().set("workflow", workflow)?;
    Ok(())
}

fn build_frame_object<'js>(
    ctx: &Ctx<'js>,
    frames: UnboundedSender<HostFrame>,
) -> JsResult<Object<'js>> {
    let obj = Object::new(ctx.clone())?;

    let tx = frames.clone();
    obj.set(
        "text",
        Function::new(ctx.clone(), move |s: String| {
            let _ = tx.send(HostFrame::Text(s));
            Ok::<_, rquickjs::Error>(())
        })?,
    )?;

    let tx = frames.clone();
    obj.set(
        "error",
        Function::new(ctx.clone(), move |s: String| {
            let _ = tx.send(HostFrame::Error(s));
            Ok::<_, rquickjs::Error>(())
        })?,
    )?;

    let tx = frames;
    obj.set(
        "json",
        Function::new(ctx.clone(), move |tag: String, value: Value<'js>| {
            // Round-trip via the JSON stringifier so any JS value becomes
            // a serde_json::Value. Drop frames where the value isn't
            // JSON-representable rather than throwing.
            let value_ctx = value.ctx().clone();
            let json_str: String = value_ctx
                .json_stringify(value.clone())?
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| "null".to_string());
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) else {
                return Ok::<_, rquickjs::Error>(());
            };
            let _ = tx.send(HostFrame::Json { tag, value: parsed });
            Ok(())
        })?,
    )?;

    Ok(obj)
}

fn build_exit_fn<'js>(
    ctx: &Ctx<'js>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move || {
        if !closed.swap(true, Ordering::AcqRel) {
            closed_notify.notify_waiters();
        }
        Ok::<_, rquickjs::Error>(())
    })
}

fn script_err<E: std::fmt::Display>(err: E) -> WorkflowError {
    WorkflowError::Script(err.to_string())
}

// ---------------------------------------------------------------------
// JS class: WorkflowInput
// ---------------------------------------------------------------------

/// `workflow.user.input` — async-iterable + iterator (returns `this`).
///
/// `next()` pulls from the input mpsc; when the buffer is empty it
/// pulses `parked` before suspending. `return()` and `workflow.exit()`
/// flip `closed`, breaking any in-flight `next()` with `{done:true}`.
pub struct WorkflowInput {
    rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
}

impl<'js> Trace<'js> for WorkflowInput {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for WorkflowInput {
    type Changed<'to> = WorkflowInput;
}

impl<'js> JsClass<'js> for WorkflowInput {
    const NAME: &'static str = "WorkflowInput";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            PredefinedAtom::SymbolAsyncIterator,
            Function::new(ctx.clone(), |this: This<Class<'js, WorkflowInput>>| {
                // Iterable returns itself — same backing channel and
                // FIFO queue for every `for await` consumer.
                Ok::<_, rquickjs::Error>(this.0.clone())
            })?,
        )?;

        proto.set(
            PredefinedAtom::Next,
            Function::new(ctx.clone(), |this: This<Class<'js, WorkflowInput>>| {
                let borrow = this.0.borrow();
                let rx = borrow.rx.clone();
                let closed = borrow.closed.clone();
                let closed_notify = borrow.closed_notify.clone();
                let parked = borrow.parked.clone();
                drop(borrow);
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    if closed.load(Ordering::Acquire) {
                        return IterResult::done();
                    }
                    let mut guard = rx.lock().await;
                    if closed.load(Ordering::Acquire) {
                        return IterResult::done();
                    }
                    if let Ok(value) = guard.try_recv() {
                        return IterResult::value(value);
                    }
                    parked.notify_one();
                    tokio::select! {
                        msg = guard.recv() => match msg {
                            Some(input) => IterResult::value(input),
                            None => IterResult::done(),
                        },
                        () = closed_notify.notified() => IterResult::done(),
                    }
                }))
            })?,
        )?;

        proto.set(
            PredefinedAtom::Return,
            Function::new(ctx.clone(), |this: This<Class<'js, WorkflowInput>>| {
                let borrow = this.0.borrow();
                if !borrow.closed.swap(true, Ordering::AcqRel) {
                    borrow.closed_notify.notify_waiters();
                }
                Ok::<_, rquickjs::Error>(IterResult::done())
            })?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// IteratorResult — `{value, done}` for the JS iterator protocol.
struct IterResult {
    value: Option<UserInput>,
    done: bool,
}

impl IterResult {
    fn value(v: UserInput) -> Self {
        Self {
            value: Some(v),
            done: false,
        }
    }

    fn done() -> Self {
        Self {
            value: None,
            done: true,
        }
    }
}

impl<'js> IntoJs<'js> for IterResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("done", self.done)?;
        if let Some(v) = self.value {
            obj.set("value", v)?;
        }
        Ok(obj.into_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_source(ext: &str, body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .expect("tempfile");
        f.write_all(body.as_bytes()).expect("write");
        f
    }

    /// Drains a workflow until it parks on `next()` or terminates.
    /// Returns the frames collected and, if the body finished, the body's
    /// result.
    async fn drive_one_cycle(
        handle: &mut WorkflowHandle,
    ) -> (Vec<HostFrame>, Option<Result<(), WorkflowError>>) {
        let mut out = Vec::new();
        loop {
            while let Ok(frame) = handle.frames.try_recv() {
                out.push(frame);
            }
            tokio::select! {
                biased;
                Some(frame) = handle.frames.recv() => out.push(frame),
                done = &mut handle.done => {
                    let result = done.unwrap_or(Ok(()));
                    while let Ok(frame) = handle.frames.try_recv() {
                        out.push(frame);
                    }
                    return (out, Some(result));
                }
                () = handle.parked.notified() => {
                    while let Ok(frame) = handle.frames.try_recv() {
                        out.push(frame);
                    }
                    return (out, None);
                }
            }
        }
    }

    #[tokio::test]
    async fn iterator_delivers_messages_in_order() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            for await (const input of workflow.user.input) {
                workflow.frame.text("got:" + input.message);
                if (input.message === "stop") {
                    workflow.exit();
                    break;
                }
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();

        // Initial cycle: body reaches the first `next()` and parks.
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty(), "got {frames:?}");
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                message: "a".into(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "got:a"));
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                message: "b".into(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "got:b"));
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                message: "stop".into(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "got:stop"));
        assert!(matches!(done, Some(Ok(()))));
    }

    #[tokio::test]
    async fn body_returns_terminates_workflow() {
        let rt = Runtime::new().unwrap();
        let file = write_source("js", "workflow.frame.text('hi');");
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "hi"));
    }

    #[tokio::test]
    async fn import_meta_args_populated() {
        let rt = Runtime::new().unwrap();
        let file = write_source("js", "workflow.frame.text(import.meta.args.join('|'));");
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: vec!["a".into(), "b".into(), "c".into()],
            })
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "a|b|c"));
    }

    #[tokio::test]
    async fn fresh_context_per_invocation() {
        // Module-level state must not survive across invocations.
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            globalThis.__counter = (globalThis.__counter ?? 0) + 1;
            workflow.frame.text(String(globalThis.__counter));
            "#,
        );
        let path = file.path().to_path_buf();

        for _ in 0..3 {
            let mut handle = rt
                .start(Invocation {
                    source_path: path.clone(),
                    args: Vec::new(),
                })
                .unwrap();
            let (frames, done) = drive_one_cycle(&mut handle).await;
            assert!(matches!(done, Some(Ok(()))));
            assert!(
                matches!(&frames[0], HostFrame::Text(t) if t == "1"),
                "expected counter=1 each invocation, got {frames:?}",
            );
        }
    }

    #[tokio::test]
    async fn exit_unblocks_pending_next() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            queueMicrotask(() => workflow.exit());
            for await (const _ of workflow.user.input) {
                workflow.frame.text("got input");
            }
            workflow.frame.text("after-loop");
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "after-loop"));
    }

    #[tokio::test]
    async fn symbol_async_iterator_returns_self() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            const it = workflow.user.input[Symbol.asyncIterator]();
            workflow.frame.text(it === workflow.user.input ? "same" : "different");
            workflow.exit();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "same"));
    }

    #[tokio::test]
    async fn concurrent_next_fifo() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            const a = workflow.user.input.next();
            const b = workflow.user.input.next();
            const [ra, rb] = await Promise.all([a, b]);
            workflow.frame.text(`${ra.value.message},${rb.value.message}`);
            workflow.exit();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        // Body parks on the first `next()`; deliver two inputs.
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty());
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                message: "first".into(),
            })
            .unwrap();
        handle
            .input_tx
            .send(UserInput {
                message: "second".into(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "first,second"));
    }

    #[tokio::test]
    async fn ts_transpile_strips_types() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "ts",
            r#"
            const args: string[] = import.meta.args;
            workflow.frame.text(args.length.toString());
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: vec!["x".into(), "y".into(), "z".into()],
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(matches!(&frames[0], HostFrame::Text(t) if t == "3"));
    }

    #[tokio::test]
    async fn script_throw_surfaces_as_script_error() {
        let rt = Runtime::new().unwrap();
        let file = write_source("js", "throw new Error('boom');");
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
            "got {result:?}"
        );
    }
}
