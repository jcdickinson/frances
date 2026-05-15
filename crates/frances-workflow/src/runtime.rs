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
use crate::approval::ApprovalRequest;
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
    /// Ask the user a question. The corresponding answer arrives back
    /// through the `ApprovalGateway`'s response oneshot.
    Approval(ApprovalRequest),
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
    /// `sender` labels the speaker (e.g. `"you"`, `"frances"`); the host
    /// renders it as a block prefix. `None` ⇒ no prefix.
    Markdown {
        content: String,
        sender: Option<String>,
    },
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
        let js = AsyncRuntime::new()?;
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
    let context = AsyncContext::full(&js).await?;

    async_with!(context => |ctx| {
        let result: Result<(), WorkflowError> = async {
            // The install-time stash must be live before either family
            // of modules evaluates. `whatwg:abortcontroller` captures
            // `_setSleep` from it (for `AbortSignal.timeout`), and
            // every `frances:v1/*` module captures its own slots. The
            // whatwg polyfills are declared before v1 because the v1
            // modules import from them (frames uses WritableStream,
            // chat uses Readable/TransformStream).
            modules::install_stash(
                &ctx,
                modules::V1HostState {
                    frames_tx: frames,
                    input_rx,
                    closed: closed.clone(),
                    closed_notify: closed_notify.clone(),
                    parked,
                    deps,
                },
            )?;
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

            let (_module, promise) = user_module
                .eval()
                .catch(&ctx)
                .map_err(caught("eval user-script"))?;
            promise
                .into_future::<()>()
                .await
                .catch(&ctx)
                .map_err(caught("await user-script promise"))?;
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
pub mod test_deps {
    //! In-memory `WorkflowDeps` for tests. `push` records to a local
    //! Vec; `run` errors out (no provider) by default. Tests that need a
    //! happy-path provider stub the next `run` with a script via
    //! `StubDeps::script_next_run` — that call configures the events to
    //! emit and the `CompletionOutcome` to return.

    use async_trait::async_trait;
    use frances_edit::{EditEngine, EditSession, FakeStore};
    use frances_models_llm::chat::{
        ChatError, ChatSession, ChatSessionBuilder, ChatSessionId, ChatSessionManager,
        HistoryError, OwnedHistoryInput,
    };
    use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolChoice, ToolDef};
    use frances_shell::{Shell, ShellError, ShellOptions};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    use crate::approval::{
        ApprovalChoice, ApprovalGateway, ApprovalId, ApprovalKind, ApprovalRequest,
    };
    use crate::deps::{EditorFactory, ShellFactory, WorkflowDeps};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::oneshot;

    #[derive(Clone, Default)]
    pub struct StubDeps {
        manager: StubManager,
        shell_factory: StubShellFactory,
        editor_factory: StubEditorFactory,
        approvals: StubApprovalGateway,
        cwd: Arc<Mutex<Option<PathBuf>>>,
    }

    impl StubDeps {
        /// Resolve the most-recently-allocated approval slot with the
        /// given choice. Returns `false` if there's no pending slot or
        /// it already settled.
        pub fn answer_approval(&self, id: ApprovalId, choice: ApprovalChoice) -> bool {
            self.approvals.answer(id, choice)
        }
    }

    impl StubDeps {
        /// Sets the cwd reported by `current_cwd`. Lets editor tests
        /// point relative paths at a tempdir without spinning up a full
        /// `InvocationContext`.
        pub fn set_cwd(&self, cwd: PathBuf) {
            *self.cwd.lock() = Some(cwd);
        }
    }

    impl WorkflowDeps for StubDeps {
        type ChatSessionManager = StubManager;
        type ShellFactory = StubShellFactory;
        type EditorFactory = StubEditorFactory;
        type ApprovalGateway = StubApprovalGateway;

        fn chat_session_manager(&self) -> &Self::ChatSessionManager {
            &self.manager
        }

        fn shell_factory(&self) -> &Self::ShellFactory {
            &self.shell_factory
        }

        fn editor_factory(&self) -> &Self::EditorFactory {
            &self.editor_factory
        }

        fn approval_gateway(&self) -> &Self::ApprovalGateway {
            &self.approvals
        }

        fn current_env(&self) -> HashMap<OsString, OsString> {
            HashMap::new()
        }

        fn current_cwd(&self) -> Option<PathBuf> {
            self.cwd.lock().clone()
        }
    }

    /// In-memory `ApprovalGateway` for tests. Records each allocation
    /// and lets the test settle the oneshot via `answer`.
    #[derive(Clone, Default)]
    pub struct StubApprovalGateway {
        inner: Arc<StubApprovalInner>,
    }

    #[derive(Default)]
    struct StubApprovalInner {
        next_id: AtomicU64,
        pending: Mutex<HashMap<ApprovalId, oneshot::Sender<ApprovalChoice>>>,
    }

    impl ApprovalGateway for StubApprovalGateway {
        fn allocate(
            &self,
            prompt: String,
            kind: ApprovalKind,
        ) -> (ApprovalRequest, oneshot::Receiver<ApprovalChoice>) {
            let id = ApprovalId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
            let (tx, rx) = oneshot::channel();
            self.inner.pending.lock().insert(id, tx);
            (ApprovalRequest { id, prompt, kind }, rx)
        }
    }

    impl StubApprovalGateway {
        fn answer(&self, id: ApprovalId, choice: ApprovalChoice) -> bool {
            match self.inner.pending.lock().remove(&id) {
                Some(tx) => tx.send(choice).is_ok(),
                None => false,
            }
        }
    }

    /// Real-bash shell factory for shell-specific tests. Spawns an
    /// actual bash subprocess. Tests that don't need bash use
    /// `StubShellFactory` (the default).
    #[derive(Clone, Default)]
    pub struct RealShellFactory;

    impl ShellFactory for RealShellFactory {
        async fn spawn(&self, opts: ShellOptions) -> Result<Shell, ShellError> {
            Shell::spawn(opts).await
        }
    }

    /// Stub shell factory — errors on spawn unless overridden. Most
    /// tests don't need real bash; the few that do can construct their
    /// own factory and inject it.
    #[derive(Clone, Default)]
    pub struct StubShellFactory;

    impl ShellFactory for StubShellFactory {
        async fn spawn(&self, _opts: ShellOptions) -> Result<Shell, ShellError> {
            Err(ShellError::Handshake(
                "stub shell factory: no real bash available in this test".to_owned(),
            ))
        }
    }

    /// Variant of `StubDeps` that uses `RealShellFactory` for tests that
    /// need to drive a real bash subprocess.
    #[derive(Clone, Default)]
    pub struct StubDepsRealShell {
        manager: StubManager,
        shell_factory: RealShellFactory,
        editor_factory: StubEditorFactory,
        approvals: StubApprovalGateway,
    }

    impl WorkflowDeps for StubDepsRealShell {
        type ChatSessionManager = StubManager;
        type ShellFactory = RealShellFactory;
        type EditorFactory = StubEditorFactory;
        type ApprovalGateway = StubApprovalGateway;

        fn chat_session_manager(&self) -> &Self::ChatSessionManager {
            &self.manager
        }

        fn shell_factory(&self) -> &Self::ShellFactory {
            &self.shell_factory
        }

        fn editor_factory(&self) -> &Self::EditorFactory {
            &self.editor_factory
        }

        fn approval_gateway(&self) -> &Self::ApprovalGateway {
            &self.approvals
        }

        fn current_env(&self) -> HashMap<OsString, OsString> {
            HashMap::new()
        }

        fn current_cwd(&self) -> Option<PathBuf> {
            None
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

        /// Resolve the pending approval slot `id` with `choice`. Mirrors
        /// `StubDeps::answer_approval` for the real-shell deps variant
        /// used by `frances:v1/tools/shell` tests.
        pub fn answer_approval(&self, id: ApprovalId, choice: ApprovalChoice) -> bool {
            self.approvals.answer(id, choice)
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
    }

    #[derive(Clone, Default)]
    pub struct StubManager {
        next_script: Arc<Mutex<std::collections::VecDeque<Script>>>,
        sessions: Arc<Mutex<Vec<StubSession>>>,
    }

    #[derive(Clone)]
    struct Script {
        events: Vec<StreamEvent>,
        outcome: CompletionOutcome,
    }

    #[async_trait]
    impl ChatSessionManager for StubManager {
        type Session = StubSession;

        fn create(&self, _builder: ChatSessionBuilder) -> Self::Session {
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
            _tools: Vec<ToolDef>,
            _tool_choice: Option<ToolChoice>,
            mut on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send>,
        ) -> Result<CompletionOutcome, ChatError> {
            let script = self.next_script.lock().pop_front();
            match script {
                Some(s) => {
                    for ev in s.events {
                        on_event(ev)?;
                    }
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
mod tests {
    use super::*;
    use crate::approval::ApprovalChoice;
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
                FrameKind::Markdown { content, .. } | FrameKind::Error { content } => {
                    content.clone()
                }
                FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
            },
            HostFrame::Append { delta, .. } => delta.clone(),
            HostFrame::Approval(req) => format!("[approval:{}] {}", req.id, req.prompt),
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
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
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
            matches!(&frames[0], HostFrame::Push(p) if matches!(&p.kind, FrameKind::Markdown { content, .. } if content == "hello"))
        );
        assert!(matches!(&frames[1], HostFrame::Append { delta, .. } if delta == " world"));
    }

    #[tokio::test]
    async fn markdown_frame_carries_sender() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            transcript.push(new MarkdownFrame({ content: "hi", sender: "you" }));
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
        assert!(matches!(
            &frames[0],
            HostFrame::Push(p)
                if matches!(&p.kind, FrameKind::Markdown { content, sender: Some(s) }
                    if content == "hi" && s == "you")
        ));
        assert!(matches!(
            &frames[1],
            HostFrame::Push(p)
                if matches!(&p.kind, FrameKind::Markdown { content, sender: None }
                    if content == "ok")
        ));
    }

    #[tokio::test]
    async fn markdown_frame_rejects_non_string_sender() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            new MarkdownFrame({ content: "hi", sender: 42 });
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();
        let (_frames, done) = drive_one_cycle(&mut handle).await;
        let err = done.expect("workflow done").expect_err("expected throw");
        assert!(
            format!("{err}").contains("sender"),
            "error should mention sender: {err}"
        );
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
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
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
    async fn chat_tools_array_is_per_instance_and_initially_empty() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const a = new ChatSession({ model_intents: ["x"] });
            const b = new ChatSession({ model_intents: ["x"] });
            const shape = `a=${Array.isArray(a.tools)} len=${a.tools.length} distinct=${a.tools !== b.tools}`;
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
        assert_eq!(text_of(&frames[0]), "a=true len=0 distinct=true");
    }

    #[tokio::test]
    async fn chat_tools_duplicate_names_throw_on_stream() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { ChatSession } from "frances:v1/chat";
            import { transcript, MarkdownFrame, ErrorFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({ name: "echo", description: "d", parameters: {}, handler: () => {} });
            s.tools.push({ name: "echo", description: "d", parameters: {}, handler: () => {} });
            s.push({ role: "user", content: "hi" });
            try {
                await s.stream();
                transcript.push(new ErrorFrame({ content: "BUG: stream did not throw" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({ content: String(e) }));
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
            import { transcript, MarkdownFrame, ErrorFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.tools.push({ name: "echo" }); // missing description / parameters
            s.push({ role: "user", content: "hi" });
            try {
                await s.stream();
                transcript.push(new ErrorFrame({ content: "BUG: stream did not throw" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({ content: String(e) }));
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
        let msg = text_of(&frames[0]);
        assert!(msg.contains("description"), "got `{msg}`");
    }

    #[tokio::test]
    async fn chat_stream_surfaces_tool_calls_in_completed_and_events() {
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![
                StreamEvent::TextDelta("Calling tool...".to_owned()),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: json!({ "text": "hi" }),
                }),
            ],
            CompletionOutcome {
                text: "Calling tool...".to_owned(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame, ErrorFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: summary }));
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            s.push({ role: "tool", call_id: "abc", content: "result body", is_error: false });
            transcript.push(new MarkdownFrame({ content: "ok" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame, ErrorFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            let caught = "";
            try {
                s.push({ role: "tool", call_id: 123, content: "x", is_error: false });
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
        let msg = text_of(&frames[0]);
        assert!(msg.contains("call_id"), "got `{msg}`");
    }

    #[tokio::test]
    async fn stream_dispatches_tool_calls_internally() {
        // chat.stream() owns dispatch: when the LLM emits tool calls,
        // their handlers run inside the stream call and their results
        // get pushed back into the session before the next round.
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        // Round 1: model emits a tool call.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({ "text": "from round 1" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({
                content: `text="${finalText}" handlerCalls=${handlerCalls}`,
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({ "text": "hi" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({
                content: `pre=${preCount} post=${postCount}`,
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "echo".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "nonexistent".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const s = new ChatSession({ model_intents: ["x"] });
            s.push({ role: "user", content: "hi" });
            const r = await s.stream();
            await r.completed;
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        // Outer round: LLM calls `outer`.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "outer1".to_owned(),
                name: "outer".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
                id: "inner1".to_owned(),
                name: "inner".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const sh = new Shell();
            const outcome = await sh.runOnce("echo hello-shell");
            const summary = `kind=${outcome.kind} exit=${outcome.exit_code} hasOutput=${outcome.output.includes("hello-shell")}`;
            await sh.close();
            transcript.push(new MarkdownFrame({ content: summary }));
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({
                content: `firstKind=${first.kind} caught=${caught.includes("busy") ? "busy" : caught}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({
                content: `firstKind=${first.kind} finalKind=${final_.kind} exit=${final_.exit_code} hasFinished=${final_.output.includes("finished")}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "echo from-run-tool" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 3 && echo finished" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
                    id: id.clone(),
                    name: "shell_wait".to_owned(),
                    arguments: json!({}),
                })],
                CompletionOutcome {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        // Round 1: shell_run on a long-running command.
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDepsRealShell::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done" }),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
                    id: id.clone(),
                    name: "read_file".to_owned(),
                    arguments: json!({}),
                })],
                CompletionOutcome {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "checker".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: `turnRan=${turnRan}` }));
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "first".to_owned(),
                    name: "slow".to_owned(),
                    arguments: json!({}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "second".to_owned(),
                    name: "fast".to_owned(),
                    arguments: json!({}),
                }),
            ],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "first".to_owned(),
                        name: "slow".to_owned(),
                        arguments: json!({}),
                    },
                    ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: `turns=${turnOrder.join(",")}` }));
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
        assert_eq!(text_of(frames.last().unwrap()), "turns=fast,slow");
    }

    #[tokio::test]
    async fn scope_lock_turn_can_drive_followup_stream() {
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "starter".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".to_owned(),
                    name: "starter".to_owned(),
                    arguments: json!({}),
                }],
            },
        );
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c2".to_owned(),
                name: "followup".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "outer".to_owned(),
                name: "gated".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "outer".to_owned(),
                    name: "gated".to_owned(),
                    arguments: json!({}),
                }],
            },
        );
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "offscript".to_owned(),
                name: "forbidden".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "double".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "done" }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
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
        use frances_models_llm::wire::{CompletionOutcome, StreamEvent, ToolCall};
        use serde_json::json;

        let deps = StubDeps::default();
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "thrower".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({ content: "survived" }));
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
    async fn timer_reject_preserves_error_identity() {
        // Rejection identity is now preserved verbatim — the caught
        // value IS the original Error, not a wrapped copy.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            const original = new Error("nope");
            queueMicrotask(() => t.reject(original));
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({
                    content: `same=${e === original} msg=${e.message}`,
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
    async fn timer_reject_with_timer_error_is_instance() {
        // When the caller explicitly rejects with a TimerError, the
        // identity is preserved and `instanceof TimerError` holds. We
        // no longer auto-wrap arbitrary rejections.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer, TimerError } from "frances:v1/io";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            queueMicrotask(() => t.reject(new TimerError("boom")));
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

    #[tokio::test]
    async fn timer_reject_with_object_preserves_identity() {
        // Non-Error rejection values are also preserved verbatim — no
        // string coercion, no auto-wrapping.
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const t = new Timer(60_000);
            const payload = { kind: "custom", n: 42 };
            queueMicrotask(() => t.reject(payload));
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({
                    content: `same=${e === payload} kind=${e.kind} n=${e.n}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const ac = new AbortController();
            ac.abort("pre-aborted");
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            const start = Date.now();
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                const elapsed = Date.now() - start;
                transcript.push(new MarkdownFrame({
                    content: `caught=${e} fast=${elapsed < 100}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const ac = new AbortController();
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            queueMicrotask(() => ac.abort(new Error("user cancelled")));
            const start = Date.now();
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                const elapsed = Date.now() - start;
                transcript.push(new MarkdownFrame({
                    content: `err=${e instanceof Error} msg=${e.message} fast=${elapsed < 1000}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const ac = new AbortController();
            const reason = { kind: "signal-reason", id: 7 };
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            queueMicrotask(() => ac.abort(reason));
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                transcript.push(new MarkdownFrame({
                    content: `same=${e === reason} kind=${e.kind} id=${e.id}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const ac = new AbortController();
            const t = new Timer({ delay: 60_000, signal: ac.signal });
            t.reject(new Error("manual"));
            // After reject, the timer is terminal. Aborting the signal
            // should not throw / not mutate anything observable.
            ac.abort("late");
            try {
                await t;
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                // We rejected with our own Error before abort fired —
                // the late abort must not have replaced the reason.
                transcript.push(new MarkdownFrame({
                    content: `msg=${e.message} aborted=${ac.signal.aborted}`,
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
            })
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

    #[tokio::test]
    async fn abortsignal_timeout_fires_after_delay() {
        let rt = Runtime::new(StubDeps::default()).unwrap();
        let file = write_source(
            "js",
            r#"
            import { AbortSignal } from "whatwg:abortcontroller";
            import { DOMException } from "whatwg:dom";
            import { Timer } from "frances:v1/io";
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const start = Date.now();
            const s = AbortSignal.timeout(15);
            // Wait long enough for the timeout to fire.
            await new Timer(60);
            const elapsed = Date.now() - start;
            transcript.push(new MarkdownFrame({
                content: `aborted=${s.aborted} name=${s.reason && s.reason.name} dom=${s.reason instanceof DOMException} fast=${elapsed < 200}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const start = Date.now();
            try {
                await new Timer({ delay: 60_000, signal: AbortSignal.timeout(15) });
                transcript.push(new MarkdownFrame({ content: "BUG: resolved" }));
            } catch (e) {
                const elapsed = Date.now() - start;
                transcript.push(new MarkdownFrame({
                    content: `name=${e.name} msg=${e.message} fast=${elapsed < 1000}`,
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const cases = ["nope", -5, NaN, undefined, {}];
            const threw = cases.map((c) => {
                try { AbortSignal.timeout(c); return "no-throw"; }
                catch (_) { return "threw"; }
            });
            transcript.push(new MarkdownFrame({ content: threw.join(",") }));
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
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
            transcript.push(new MarkdownFrame({
                content: `first=${reasonAfterFirst} after=${out.reason} aborted=${out.aborted}`,
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
        assert_eq!(text_of(&frames[0]), "first=first after=first aborted=true");
    }

    /// `approve()` emits a HostFrame::Approval and awaits the host's
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
            import { transcript, MarkdownFrame } from "frances:v1/frames";
            const choice = await approve("delete /tmp/foo?");
            transcript.push(new MarkdownFrame({
                content: `${choice.type}:${choice.details ?? ""}`,
            }));
            "#,
        );
        let mut handle = rt
            .start(Invocation {
                source_path: file.path().to_path_buf(),
                args: Vec::new(),
            })
            .unwrap();

        let req = tokio::time::timeout(CYCLE_TIMEOUT, async {
            loop {
                if let Some(HostFrame::Approval(req)) = handle.frames.recv().await {
                    return req;
                }
            }
        })
        .await
        .expect("approval frame did not arrive in time");
        assert_eq!(req.prompt, "delete /tmp/foo?");

        assert!(
            deps.answer_approval(
                req.id,
                ApprovalChoice::Yes {
                    details: Some("scoped to /tmp".into()),
                },
            ),
            "answer should land on the pending slot",
        );

        let result = tokio::time::timeout(CYCLE_TIMEOUT, &mut handle.done)
            .await
            .expect("workflow did not finish in time")
            .expect("done channel closed without value");
        assert!(matches!(result, Ok(())), "got {result:?}");

        let mut frames = Vec::new();
        while let Ok(f) = handle.frames.try_recv() {
            frames.push(f);
        }
        let last = frames
            .iter()
            .rev()
            .find_map(|f| match f {
                HostFrame::Push(p) => Some(p),
                _ => None,
            })
            .expect("expected a markdown push after approval");
        assert!(
            matches!(&last.kind, FrameKind::Markdown { content, .. } if content == "yes:scoped to /tmp"),
            "got {:?}",
            last.kind,
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
            })
            .unwrap();
        let (_frames, result) = drive_one_cycle(&mut handle).await;
        let result = result.expect("workflow should have terminated");
        assert!(
            matches!(result, Err(WorkflowError::ScriptCaught { .. })),
            "got {result:?}"
        );
    }
}
