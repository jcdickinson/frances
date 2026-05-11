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
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use rquickjs::async_with;
use rquickjs::context::AsyncContext;
use rquickjs::module::Module;
use rquickjs::runtime::AsyncRuntime;
use rquickjs::{CatchResultExt, Ctx, IntoJs, Object, Result as JsResult, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio::task::JoinHandle;

use crate::WorkflowError;
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
        modules::install_v1(
            &ctx,
            frames,
            input_rx,
            closed.clone(),
            closed_notify.clone(),
            parked,
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
        Ok::<(), WorkflowError>(())
    })
    .await
}

pub(crate) fn script_err<E: std::fmt::Display>(err: E) -> WorkflowError {
    WorkflowError::Script(err.to_string())
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

    /// Drives a workflow until it parks on `inbox.next()` or terminates.
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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

    #[tokio::test]
    async fn append_on_active_frame_emits_delta() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const f = new MarkdownFrame({ content: "hello" });
            transcript.push(f);
            f.append(" world");
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
    async fn append_on_superseded_frame_throws() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const a = new MarkdownFrame({ content: "a" });
            transcript.push(a);
            transcript.push(new MarkdownFrame({ content: "b" }));
            a.append(" extra");  // should throw
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
        let rt = Runtime::new().unwrap();
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
    async fn chat_session_stream_throws_not_yet_wired() {
        let rt = Runtime::new().unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            s.stream();  // throws
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
}
