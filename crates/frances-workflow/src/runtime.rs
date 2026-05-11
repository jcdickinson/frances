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
//! - `import { transcript, MarkdownFrame, ErrorFrame, JsonFrame } from "frances:v1/frames"`
//! - `import { ChatSession } from "frances:v1/chat"` (LLM backend pending)
//! - `import.meta.args` — per-invocation slash-command args.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::Mutex as StdMutex;
use rquickjs::async_with;
use rquickjs::context::AsyncContext;
use rquickjs::module::Module;
use rquickjs::runtime::AsyncRuntime;
use rquickjs::{CatchResultExt, Ctx, IntoJs, Object, Result as JsResult, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio::task::JoinHandle;

use crate::WorkflowError;
use crate::deps::WorkflowDeps;
use crate::modules;
use crate::transpile::{SourceKind, ts_to_js};

/// Internal name we declare the user script under. Distinct from the
/// `frances:v1/*` namespace so the two don't visually clash.
const USER_MODULE_NAME: &str = "frances:user-script";

/// Frames a workflow can emit during a turn. The daemon receiver maps
/// these onto the wire `StreamFrame` protocol; this enum is the
/// host-API contract, not the protocol itself.
#[derive(Debug, Clone)]
pub enum HostFrame {
    /// A new frame was pushed onto the transcript. Implicitly seals the
    /// previously-active frame (the host should close that wire block
    /// before opening this one).
    Push(FramePush),
    /// Append text to the frame with the given id. Only valid while
    /// that frame is still the active one — the JS side enforces this.
    Append { id: FrameId, delta: String },
}

/// Frame identity, scoped to one invocation. Monotonically assigned by
/// `transcript.push`. Useful for the host to map back to its own block
/// ids.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct FrameId(pub u64);

#[derive(Debug, Clone)]
pub struct FramePush {
    pub id: FrameId,
    pub kind: FrameKind,
}

#[derive(Debug, Clone)]
pub enum FrameKind {
    /// `MarkdownFrame` — text content that may be extended with `append`.
    Markdown { content: String },
    /// `ErrorFrame` — text content (typically rendered as an error) that
    /// may be extended with `append`.
    Error { content: String },
    /// `JsonFrame` — single tagged JSON value. Immutable after push.
    Json {
        tag: String,
        value: serde_json::Value,
    },
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
    /// Send user input to the workflow's `inbox` stream.
    pub input_tx: UnboundedSender<UserInput>,
    /// Frames the workflow emits.
    pub frames: UnboundedReceiver<HostFrame>,
    /// Notified each time the body suspends on `inbox.next()` with an
    /// empty queue — i.e. the body is parked waiting for input. One
    /// pulse per park; the daemon uses this to detect end-of-cycle.
    pub parked: Arc<Notify>,
    /// Resolves when the workflow terminates (body settled or `exit()`
    /// called). The inner result mirrors the body's outcome.
    pub done: oneshot::Receiver<Result<(), WorkflowError>>,
    /// Owns the spawned task; dropping the handle aborts the workflow.
    _join: JoinHandle<()>,
}

/// Workflow script runtime. One per daemon; cheap to share via `Arc`.
pub struct Runtime<D: WorkflowDeps> {
    js: AsyncRuntime,
    transpile_cache: Arc<StdMutex<TranspileCache>>,
    deps: D,
}

#[derive(Default)]
struct TranspileCache {
    /// Source-hash → transpiled JS. xxhash3_64 of the on-disk bytes.
    by_hash: std::collections::HashMap<u64, Arc<str>>,
}

impl<D: WorkflowDeps> Runtime<D> {
    pub fn new(deps: D) -> Result<Self, WorkflowError> {
        let js = AsyncRuntime::new().map_err(script_err)?;
        Ok(Self {
            js,
            transpile_cache: Arc::new(StdMutex::new(TranspileCache::default())),
            deps,
        })
    }

    /// Start a workflow. Reads + transpiles the source synchronously,
    /// spawns the body on a Tokio task, and returns a handle the daemon
    /// uses to drive it.
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
        let task_deps = self.deps.clone();

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
                task_deps,
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
        if let Some(cached) = self.transpile_cache.lock().by_hash.get(&hash).cloned() {
            return Ok(cached.to_string());
        }
        let js = ts_to_js(path, source)?;
        self.transpile_cache
            .lock()
            .by_hash
            .insert(hash, Arc::<str>::from(js.as_str()));
        Ok(js)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "shared task-local state, packing buys nothing"
)]
async fn run_workflow<D: WorkflowDeps>(
    js: AsyncRuntime,
    js_source: String,
    args: Vec<String>,
    frames: UnboundedSender<HostFrame>,
    input_rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
    deps: D,
) -> Result<(), WorkflowError> {
    let context = AsyncContext::full(&js).await.map_err(script_err)?;

    async_with!(context => |ctx| {
        let result: Result<(), WorkflowError> = async {
            // `frances:v1/*` modules import from `whatwg:*` (frames
            // uses WritableStream, chat uses Readable/TransformStream),
            // so the polyfills have to be declared first.
            modules::install_whatwg(&ctx)?;
            modules::install_v1(
                &ctx,
                frames,
                input_rx,
                closed.clone(),
                closed_notify.clone(),
                parked,
                deps,
            )?;

            let user_module = Module::declare(ctx.clone(), USER_MODULE_NAME, js_source.as_bytes())
                .catch(&ctx)
                .map_err(|e| script_err(format!("{e}")))?;
            let meta = user_module
                .meta()
                .catch(&ctx)
                .map_err(|e| script_err(format!("{e}")))?;
            meta.set("args", args)
                .catch(&ctx)
                .map_err(|e| script_err(format!("{e}")))?;

            let (_module, promise) = user_module
                .eval()
                .catch(&ctx)
                .map_err(|e| script_err(format!("{e}")))?;
            promise
                .into_future::<()>()
                .await
                .catch(&ctx)
                .map_err(|e| script_err(format!("{e}")))?;
            Ok(())
        }
        .await;

        // Tear down any rquickjs `Persistent` values stashed in
        // userdata *before* the context drops. Skipping this aborts
        // the runtime at `JS_FreeRuntime: list_empty`. Runs whether
        // the body succeeded or errored.
        modules::cleanup_v1(&ctx);

        result
    })
    .await
}

pub(crate) fn script_err<E: std::fmt::Display>(err: E) -> WorkflowError {
    WorkflowError::Script(err.to_string())
}

#[cfg(test)]
pub(crate) mod test_deps {
    //! In-memory `WorkflowDeps` for tests. `push` records to a local
    //! Vec; `run` errors out (no provider). Sufficient for the JS-shape
    //! tests; a real backend lands when we wire end-to-end tests.

    use async_trait::async_trait;
    use frances_models_llm::chat::{
        ChatError, ChatSession, ChatSessionBuilder, ChatSessionId, ChatSessionManager,
        HistoryError, OwnedHistoryInput,
    };
    use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolChoice, ToolDef};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::Arc;

    use crate::deps::WorkflowDeps;

    #[derive(Clone, Default)]
    pub(crate) struct StubDeps {
        manager: StubManager,
    }

    impl WorkflowDeps for StubDeps {
        type ChatSessionManager = StubManager;

        fn chat_session_manager(&self) -> &Self::ChatSessionManager {
            &self.manager
        }

        fn current_env(&self) -> HashMap<OsString, OsString> {
            HashMap::new()
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct StubManager;

    #[async_trait]
    impl ChatSessionManager for StubManager {
        type Session = StubSession;

        fn create(&self, _builder: ChatSessionBuilder) -> Self::Session {
            StubSession {
                pending: Arc::new(Mutex::new(Vec::new())),
            }
        }

        async fn load(&self, _id: ChatSessionId) -> Result<Self::Session, ChatError> {
            Err(ChatError::History(HistoryError::ChatSessionNotFound(
                ChatSessionId(0),
            )))
        }

        async fn primary(&self, builder: ChatSessionBuilder) -> Result<Self::Session, ChatError> {
            Ok(self.create(builder))
        }
    }

    #[derive(Clone)]
    pub(crate) struct StubSession {
        pending: Arc<Mutex<Vec<OwnedHistoryInput>>>,
    }

    #[async_trait]
    impl ChatSession for StubSession {
        fn push(&self, input: OwnedHistoryInput) {
            self.pending.lock().push(input);
        }

        async fn run(
            &self,
            _env: HashMap<OsString, OsString>,
            _tools: Vec<ToolDef>,
            _tool_choice: Option<ToolChoice>,
            _on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send>,
        ) -> Result<CompletionOutcome, ChatError> {
            Err(ChatError::ProviderUnavailable(
                "stub session: no provider wired in tests".to_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Hard ceiling on how long an individual cycle is allowed to run.
    /// Real workflow turns are interactive (a body can wait for input
    /// indefinitely); in tests, anything past a few seconds is a bug.
    /// Panicking with a clear message beats a hung test process.
    const CYCLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// Drives a workflow until it parks on `inbox.next()` or terminates.
    /// Panics if `CYCLE_TIMEOUT` is exceeded so tests fail fast.
    async fn drive_one_cycle(
        handle: &mut WorkflowHandle,
    ) -> (Vec<HostFrame>, Option<Result<(), WorkflowError>>) {
        match tokio::time::timeout(CYCLE_TIMEOUT, drive_one_cycle_inner(handle)).await {
            Ok(result) => result,
            Err(_) => panic!("drive_one_cycle timed out after {CYCLE_TIMEOUT:?} — workflow hung"),
        }
    }

    async fn drive_one_cycle_inner(
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

    fn text_of(frame: &HostFrame) -> String {
        match frame {
            HostFrame::Push(p) => match &p.kind {
                FrameKind::Markdown { content } | FrameKind::Error { content } => content.clone(),
                FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
            },
            HostFrame::Append { delta, .. } => delta.clone(),
        }
    }

    #[tokio::test]
    async fn iterator_delivers_messages_in_order() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            import { exit } from "frances:v1/workflow";
            for await (const input of inbox) {
                transcript.push(new MarkdownFrame({ content: "got:" + input.content }));
                if (input.content === "stop") { exit(); break; }
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty(), "got {frames:?}");
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                content: "a".into(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert_eq!(text_of(&frames[0]), "got:a");
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                content: "b".into(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert_eq!(text_of(&frames[0]), "got:b");
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                content: "stop".into(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            transcript.push(new MarkdownFrame({ content: "hi" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "hi");
    }

    #[tokio::test]
    async fn import_meta_args_populated() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            transcript.push(new MarkdownFrame({ content: import.meta.args.join('|') }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: vec!["a".into(), "b".into(), "c".into()],
            })
            .unwrap();

        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(text_of(&frames[0]), "a|b|c");
    }

    #[tokio::test]
    async fn fresh_context_per_invocation() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            globalThis.__counter = (globalThis.__counter ?? 0) + 1;
            transcript.push(new MarkdownFrame({ content: String(globalThis.__counter) }));
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            import { exit } from "frances:v1/workflow";
            queueMicrotask(() => exit());
            for await (const _ of inbox) {
                transcript.push(new MarkdownFrame({ content: "got input" }));
            }
            transcript.push(new MarkdownFrame({ content: "after-loop" }));
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
        assert_eq!(text_of(&frames[0]), "after-loop");
    }

    #[tokio::test]
    async fn symbol_async_iterator_returns_self() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            import { exit } from "frances:v1/workflow";
            const it = inbox[Symbol.asyncIterator]();
            transcript.push(new MarkdownFrame({ content: it === inbox ? "same" : "different" }));
            exit();
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
        assert_eq!(text_of(&frames[0]), "same");
    }

    #[tokio::test]
    async fn concurrent_next_fifo() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { inbox } from "frances:v1/inbox";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            import { exit } from "frances:v1/workflow";
            const a = inbox.next();
            const b = inbox.next();
            const [ra, rb] = await Promise.all([a, b]);
            transcript.push(new MarkdownFrame({ content: `${ra.value.content},${rb.value.content}` }));
            exit();
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(frames.is_empty());
        assert!(done.is_none());

        handle
            .input_tx
            .send(UserInput {
                content: "first".into(),
            })
            .unwrap();
        handle
            .input_tx
            .send(UserInput {
                content: "second".into(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const args: string[] = import.meta.args;
            transcript.push(new MarkdownFrame({ content: args.length.toString() }));
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
            "got {result:?}"
        );
    }

    #[tokio::test]
    async fn write_on_active_frame_emits_delta() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const f = new MarkdownFrame({ content: "hello" });
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
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert!(
            matches!(&frames[0], HostFrame::Push(p) if matches!(&p.kind, FrameKind::Markdown { content } if content == "hello"))
        );
        assert!(matches!(&frames[1], HostFrame::Append { delta, .. } if delta == " world"));
    }

    #[tokio::test]
    async fn write_on_superseded_frame_throws() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const a = new MarkdownFrame({ content: "a" });
            transcript.push(a);
            transcript.push(new MarkdownFrame({ content: "b" }));
            const w = a.writable.getWriter();
            await w.write(" extra");  // should reject — a is no longer active
            "#,
        );
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

    #[tokio::test]
    async fn unknown_v1_module_fails_to_load() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source("js", r#"import { nope } from "frances:v1/nope";"#);
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

    #[tokio::test]
    async fn chat_session_accepts_system_and_user_roles() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["summarize"] });
            s.push({ role: "system", content: "you are a summariser" });
            s.push({ role: "user", content: "hi" });
            transcript.push(new MarkdownFrame({ content: "ok" }));
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
        assert_eq!(text_of(&frames[0]), "ok");
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "system", content: "be terse" });
            s.push({ role: "system", content: "answer in english" });
            s.push({ role: "user", content: "hi" });
            transcript.push(new MarkdownFrame({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
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
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const protoKeys = Object.getOwnPropertyNames(ChatSession.prototype)
                .filter((k) => k !== "constructor");
            const stashGone = typeof globalThis.__frances_v1_stash__ === "undefined";
            transcript.push(new MarkdownFrame({
                content: `proto=${protoKeys.sort().join(",")} stash=${stashGone}`,
            }));
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
        // Only `push` and the JS-installed `stream` should be on the
        // prototype; the inner raw stream function must not appear.
        assert_eq!(text_of(&frames[0]), "proto=push,stream stash=true");
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            const _text = r.text;  // locks events via pipeThrough
            let locked = false;
            try { r.events.getReader(); }
            catch (_) { locked = true; }
            transcript.push(new MarkdownFrame({
                content: `locked=${locked} stableText=${r.text === _text}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        // MarkdownFrame's `.writable` resolves without throwing.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            const out = new MarkdownFrame({ content: "" });
            transcript.push(out);
            await r.text.pipeTo(out.writable);
            try { await r.completed; } catch (_) { /* stub error — expected */ }
            transcript.push(new MarkdownFrame({ content: "piped-ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
    async fn chat_session_stream_aborts_with_signal() {
        // Pre-aborted AbortSignal errors the events stream synchronously
        // during `stream()`, so the first read sees the reason.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { AbortController } from "whatwg:abortcontroller";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: caught }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "user wanted out");
    }

    #[tokio::test]
    async fn markdown_frame_writable_is_stable_writable_stream() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            import { WritableStream } from "whatwg:web-streams";
            const f = new MarkdownFrame({ content: "hi" });
            transcript.push(f);
            const w1 = f.writable;
            const w2 = f.writable;
            const shape = `ws=${w1 instanceof WritableStream} stable=${w1 === w2} hasWrite=${typeof MarkdownFrame.prototype.write}`;
            transcript.push(new MarkdownFrame({ content: shape }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const start = Date.now();
            await new Timer(20);
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownFrame({ content: elapsed >= 15 ? "ok" : `too fast: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);  // long enough that the test would hang if fire() didn't work
            queueMicrotask(() => t.fire());
            const start = Date.now();
            await t;
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownFrame({ content: elapsed < 1000 ? "fast" : `slow: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            queueMicrotask(() => {
                t.disable();
                t.fire();
            });
            const start = Date.now();
            await t;
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownFrame({
                content: elapsed < 1000 ? "fast" : `slow: ${elapsed}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "fast");
    }

    #[tokio::test]
    async fn timer_reject_with_error_carries_message() {
        // Identity isn't preserved (we capture a string at reject() time
        // to avoid leaking a JS value past the runtime lifetime), but
        // the Error message comes through and the caught value is an
        // Error instance.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            queueMicrotask(() => t.reject(new Error("nope")));
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({
                    content: `caught: error=${e instanceof Error} msg=${e.message}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "caught: error=true msg=nope");
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: results.join("; ") }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "reject: threw; disable: threw; enable: threw; fire: threw; set: threw"
        );
    }

    #[tokio::test]
    async fn timer_reject_makes_instance_of_timer_error() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer, TimerError } from "frances:v1/io";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            queueMicrotask(() => t.reject(new Error("boom")));
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({
                    content: `te=${e instanceof TimerError} err=${e instanceof Error} msg=${e.message}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            queueMicrotask(() => t.reject());
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({
                    content: `caught: error=${e instanceof Error} name=${e.name} msg=${e.message}`,
                }));
            }
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer({ delay: 10 });
            t.disable();
            t.enable();
            await t;
            transcript.push(new MarkdownFrame({ content: t.enabled ? "enabled" : "still-off" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer({ delay: 100, interval: 50 });
            const before = `enabled=${t.enabled} delay=${t.delay} interval=${t.interval}`;
            t.disable();
            const after = `enabled=${t.enabled} delay=${t.delay} interval=${t.interval}`;
            transcript.push(new MarkdownFrame({ content: `${before} | ${after}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const tick = new Timer({ interval: 5 });
            let count = 0;
            for (let i = 0; i < 3; i += 1) { await tick; count += 1; }
            tick.disable();
            transcript.push(new MarkdownFrame({ content: `count=${count}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(10);
            await t;
            const start = Date.now();
            await t;  // already fired — no wait
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownFrame({ content: elapsed < 5 ? "instant" : `slow: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            await new Timer({ delay: 5 });
            transcript.push(new MarkdownFrame({ content: "fired" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: ok ? "ok" : `bad: ${first} ${second} ${third}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            t.disable();
            // Cancelled — without set(), the next await would reject.
            t.set({ delay: 10 });
            await t;
            transcript.push(new MarkdownFrame({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer({ delay: 5 });
            await t;             // fires, fired_once = true
            t.set({ interval: 15 });
            const t0 = Date.now();
            await t;
            await t;
            const elapsed = Date.now() - t0;
            transcript.push(new MarkdownFrame({ content: elapsed >= 25 ? "ok" : `too fast: ${elapsed}` }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::Script(_))),
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            import { exit } from "frances:v1/workflow";
            const t = new Timer(60_000);
            queueMicrotask(() => exit());
            await t;  // should resolve when the workflow closes, not reject
            transcript.push(new MarkdownFrame({ content: "after-await" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(text_of(&frames[0]), "after-await");
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const e = new DOMException("nope", "AbortError");
            transcript.push(new MarkdownFrame({
                content: `err=${e instanceof Error} name=${e.name} msg=${e.message}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const shape = [
                typeof ReadableStream,
                typeof WritableStream,
                typeof TransformStream,
            ].join(",");
            transcript.push(new MarkdownFrame({ content: shape }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const ac = new AbortController();
            const before = ac.signal.aborted;
            let fired = false;
            ac.signal.addEventListener("abort", () => { fired = true; });
            ac.abort("nope");
            const after = ac.signal.aborted;
            const reason = ac.signal.reason;
            const isSignal = ac.signal instanceof AbortSignal;
            transcript.push(new MarkdownFrame({
                content: `before=${before} after=${after} fired=${fired} reason=${reason} sig=${isSignal}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
        assert_eq!(
            text_of(&frames[0]),
            "before=false after=true fired=true reason=nope sig=true"
        );
    }
}
