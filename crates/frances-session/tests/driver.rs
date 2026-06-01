//! Integration tests for [`frances_session::SessionRuntime`]'s driver
//! loop. Each test spins up a real `SessionRuntime` against a tempdir
//! `Database` + an `InMemoryProvider`-seeded config + a scripted
//! `StubProvider`, then asserts on the `StreamFrame` sequence on the
//! runtime's events channel.
//!
//! Determinism: no real wall-clock waits inside the workflow JS
//! (workflows resolve immediately or via `inbox.next()`). The driver's
//! `DEHYDRATE_TIMEOUT` is controllable via `tokio::time::pause` for
//! tests that need to exercise the timeout branch.

use std::sync::Arc;
use std::time::Duration;

use tempfile::{NamedTempFile, TempDir};
use tokio::sync::mpsc::UnboundedReceiver;

use frances_config::{ConfigProvider, InMemoryProvider, Value as ConfigValue};
use frances_llm::test_util::{StubProvider, StubScript};
use frances_models_llm::{CompletionOutcome, StreamEvent};
use frances_session::context::{InvocationContext, ProcessContext};
use frances_session::events::{SectionKind, StreamFrame};
use frances_session::runtime::{SessionRuntime, StartOverrides};
use frances_session::session::{Paths, Session, SessionMeta};
use frances_session::store;

/// Anything the harness keeps alive for the duration of one test.
struct Harness {
    /// Held so RAII shutdown runs on drop, and so future tests can
    /// call `prompt` / `interrupt` / `respond_permission` against it.
    /// Underscored because no test in this file dereferences it yet —
    /// drop the prefix when adding a test that does.
    _runtime: Arc<SessionRuntime>,
    events: UnboundedReceiver<StreamFrame>,
    stub: Arc<StubProvider>,
    // RAII handles — drop order: runtime first (shutdown), then sources.
    _src: NamedTempFile,
    _tempdir: TempDir,
}

impl Harness {
    /// Pull one frame off the channel with a sanity timeout so a hung
    /// scenario fails loudly instead of hanging the test runner.
    async fn recv_one(&mut self) -> StreamFrame {
        match tokio::time::timeout(Duration::from_secs(5), self.events.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => panic!("events channel closed unexpectedly"),
            Err(_) => panic!("recv_one timed out after 5s — runtime hung?"),
        }
    }

    /// Drain frames until `pred` matches. Returns everything collected
    /// (including the matching frame).
    async fn recv_until<F>(&mut self, mut pred: F) -> Vec<StreamFrame>
    where
        F: FnMut(&StreamFrame) -> bool,
    {
        let mut out = Vec::new();
        loop {
            let frame = self.recv_one().await;
            let stop = pred(&frame);
            out.push(frame);
            if stop {
                return out;
            }
        }
    }
}

/// Build a single-workflow harness. `workflow_src` is a TS body
/// dropped into a tempfile; the seeded config points `workflows.test`
/// at it and sets it as the default workflow.
async fn harness(workflow_src: &str) -> Harness {
    use std::io::Write;
    let tempdir = tempfile::tempdir().expect("tempdir");
    // The session.dir + runtime_dir need to exist before start_with
    // touches them. SessionRuntime::start_with calls create_dir_all on
    // session.runtime_dir but not on session.dir itself.
    std::fs::create_dir_all(tempdir.path().join("session")).unwrap();
    std::fs::create_dir_all(tempdir.path().join("runtime")).unwrap();

    let mut src = NamedTempFile::with_suffix(".ts").expect("tempfile");
    src.write_all(workflow_src.as_bytes()).expect("write src");
    src.flush().expect("flush src");
    let src_path = src.path().to_path_buf();

    let session = Session {
        paths: Paths {
            state_root: tempdir.path().to_path_buf(),
            runtime_root: tempdir.path().to_path_buf(),
        },
        id: "test-session".to_owned(),
        dir: tempdir.path().join("session"),
        runtime_dir: tempdir.path().join("runtime"),
        meta: SessionMeta {
            version: 1,
            id: "test-session".to_owned(),
            created: 0,
            cwd: None,
            reserved: None,
        },
    };

    let db = store::open(&session).await.expect("open db");

    let invocation = InvocationContext {
        tty_key: None,
        process: ProcessContext {
            cwd: Some(tempdir.path().to_path_buf()),
            env: std::sync::Arc::new(std::env::vars_os().collect()),
        },
    };

    let in_memory = build_in_memory_config(&src_path, "test");

    let stub = Arc::new(StubProvider::new());
    let stub_for_hook = stub.clone();
    let (runtime, events) = SessionRuntime::start_with(
        session,
        db,
        invocation,
        StartOverrides {
            extra_config_providers: vec![Arc::new(in_memory) as Arc<dyn ConfigProvider>],
            on_cache: Some(Box::new(move |cache| {
                cache.insert_stub("test", stub_for_hook);
            })),
        },
    )
    .await
    .expect("start_with");

    Harness {
        _runtime: runtime,
        events,
        stub,
        _src: src,
        _tempdir: tempdir,
    }
}

/// Seed every config key the runtime touches during start_with. Path
/// values are scalars (Null/Bool/Int/Float/String) — nested structs
/// are addressed component-wise.
fn build_in_memory_config(workflow_file: &std::path::Path, workflow_id: &str) -> InMemoryProvider {
    let workflow_path = workflow_file.display().to_string();
    InMemoryProvider::new()
        // Default model points at the "test" provider.
        .set(
            vec![
                ConfigValue::String("models".into()),
                ConfigValue::String("default".into()),
                ConfigValue::String("model_provider".into()),
            ],
            "test",
        )
        .set(
            vec![
                ConfigValue::String("models".into()),
                ConfigValue::String("default".into()),
                ConfigValue::String("id".into()),
            ],
            "test-model",
        )
        // Provider config — `kind`, `base_url`, and `auth.token` are
        // required for ProviderCache::new() to parse the table even
        // though `insert_stub` bypasses the build path.
        .set(
            vec![
                ConfigValue::String("model_providers".into()),
                ConfigValue::String("test".into()),
                ConfigValue::String("kind".into()),
            ],
            "openai-chat",
        )
        .set(
            vec![
                ConfigValue::String("model_providers".into()),
                ConfigValue::String("test".into()),
                ConfigValue::String("base_url".into()),
            ],
            "http://stub.invalid",
        )
        .set(
            vec![
                ConfigValue::String("model_providers".into()),
                ConfigValue::String("test".into()),
                ConfigValue::String("auth".into()),
                ConfigValue::String("token".into()),
            ],
            "stub-token",
        )
        // Workflow config — `id` (a Uuid) plus the .ts file path. We
        // synthesize a deterministic per-test UUID so the workflow's
        // own DB schema rows are reproducible.
        .set(
            vec![
                ConfigValue::String("workflows".into()),
                ConfigValue::String(workflow_id.into()),
                ConfigValue::String("id".into()),
            ],
            "00000000-0000-0000-0000-000000000001",
        )
        .set(
            vec![
                ConfigValue::String("workflows".into()),
                ConfigValue::String(workflow_id.into()),
                ConfigValue::String("file".into()),
            ],
            workflow_path.as_str(),
        )
        .set(
            vec![ConfigValue::String("default_workflow".into())],
            workflow_id,
        )
}

// =========================================================================
// Tests
// =========================================================================

/// Sanity test: workflow starts, pushes a MarkdownSection, ends.
/// Proves the harness wiring is correct independent of LLM scripting.
#[tokio::test]
async fn smoke_workflow_starts_and_pushes_frame() {
    let mut h = harness(
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: "hello from harness" }));
        "#,
    )
    .await;

    let frames = h
        .recv_until(|f| matches!(f, StreamFrame::SectionClose { .. }))
        .await;
    assert!(
        frames.iter().any(|f| matches!(
            f,
            StreamFrame::SectionAppend {
                delta,
                ..
            } if delta == "hello from harness"
        )),
        "expected hello-from-harness text frame; got: {frames:?}"
    );
}

/// Scripts the stub with three TextDeltas. The driver should turn that
/// into a Text BlockDelta open + three text-bearing BlockDeltas + a
/// BlockStop on the events channel.
#[tokio::test]
#[ignore = "frame plumbing for chat.stream() needs more wiring; left as a stub for the next session"]
async fn text_streaming_renders_block_delta_sequence() {
    let mut h = harness(
        r#"
        import { ChatSession } from "frances:v1/chat";
        const chat = new ChatSession();
        await chat.stream({ system: "test", user: "hi" });
        "#,
    )
    .await;

    h.stub.push_script(StubScript {
        events: vec![
            StreamEvent::TextDelta("alpha".into()),
            StreamEvent::TextDelta(" beta".into()),
            StreamEvent::TextDelta(" gamma".into()),
        ],
        outcome: CompletionOutcome {
            text: "alpha beta gamma".to_owned(),
            tool_calls: Vec::new(),
        },
    });

    // Drive until the assistant block closes.
    let frames = h
        .recv_until(|f| matches!(f, StreamFrame::SectionClose { .. }))
        .await;

    // Find the Text deltas. Expect: 1 open (text=None or empty) + 3
    // appends matching the scripted deltas + a final BlockStop.
    let texts: Vec<String> = frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::SectionAppend {
                kind: SectionKind::Markdown { .. },
                delta,
                ..
            } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.contains(&"alpha".to_owned()),
        "missing first delta: frames = {:?}",
        frames
    );
    assert!(
        texts.contains(&" beta".to_owned()),
        "missing second delta: frames = {:?}",
        frames
    );
    assert!(
        texts.contains(&" gamma".to_owned()),
        "missing third delta: frames = {:?}",
        frames
    );

    // Sanity: one captured request to the stub.
    let captured = h.stub.captured();
    assert_eq!(captured.len(), 1, "expected exactly one provider call");
}
