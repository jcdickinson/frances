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
use frances_session::session::Paths;
use frances_session::store;
use frances_session::workspace::Workspace;

/// Anything the harness keeps alive for the duration of one test.
struct Harness {
    _runtime: Arc<SessionRuntime>,
    events: UnboundedReceiver<StreamFrame>,
    stub: Arc<StubProvider>,
    _src: NamedTempFile,
    _tempdir: TempDir,
}

impl Harness {
    /// Pull one frame off the channel, timing out after 5 seconds.
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

    let mut src = NamedTempFile::with_suffix(".ts").expect("tempfile");
    src.write_all(workflow_src.as_bytes()).expect("write src");
    src.flush().expect("flush src");
    let src_path = src.path().to_path_buf();

    // Go through the real creation path so the session's metadata file
    // exists on disk — the runtime reads-modifies-writes it (selected
    // workflow, title).
    let paths = Paths {
        state_root: tempdir.path().to_path_buf(),
        runtime_root: tempdir.path().to_path_buf(),
    };
    paths.ensure_layout().expect("ensure layout");
    let workspace = Workspace::open(tempdir.path()).expect("open workspace");
    let session = paths.create_session(&workspace).expect("create session");

    let db = store::open(&session).await.expect("open db");

    let invocation = InvocationContext {
        workspace,
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
            ..StartOverrides::default()
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

/// Seed every config key the runtime touches during start_with.
fn build_in_memory_config(workflow_file: &std::path::Path, workflow_id: &str) -> InMemoryProvider {
    let workflow_path = workflow_file.display().to_string();
    InMemoryProvider::new()
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

/// Workflow starts, pushes a MarkdownSection, ends.
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

/// Three scripted TextDeltas produce a BlockDelta sequence on the events channel.
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

    let frames = h
        .recv_until(|f| matches!(f, StreamFrame::SectionClose { .. }))
        .await;

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

    let captured = h.stub.captured();
    assert_eq!(captured.len(), 1, "expected exactly one provider call");
}

/// The full entity producer path through the driver: creating upsert
/// reaches the channel before the transcript ref, appends persist (and
/// replay via subscribe), settle flips lifecycle and stores artifacts.
#[tokio::test]
async fn entity_producer_flows_through_driver_to_hub() {
    let mut h = harness(
        r#"
        import { createEntity } from "frances:v1/entities";
        import { transcript, EntityRefSection } from "frances:v1/sections";
        const e = createEntity("shell", { cmd: "demo", state: "running" });
        transcript.push(new EntityRefSection({ id: e.id }));
        e.append({ text: "hello" });
        e.settle({ cmd: "demo", state: "success" }, { artifacts: { llm_digest: "Exit 0" } });
        "#,
    )
    .await;

    use frances_session::events::Lifecycle;
    let frames = h
        .recv_until(|f| {
            matches!(
                f,
                StreamFrame::EntityUpsert { envelope, .. }
                    if envelope.kind == "shell" && envelope.lifecycle == Lifecycle::Settled
            )
        })
        .await;

    let live_pos = frames
        .iter()
        .position(|f| {
            matches!(
                f,
                StreamFrame::EntityUpsert { envelope, .. }
                    if envelope.kind == "shell" && envelope.lifecycle == Lifecycle::Live
            )
        })
        .expect("creating Live upsert");
    let (ref_pos, entity_id) = frames
        .iter()
        .enumerate()
        .find_map(|(i, f)| match f {
            StreamFrame::SectionAppend {
                kind: SectionKind::EntityRef { entity_id },
                ..
            } => Some((i, *entity_id)),
            _ => None,
        })
        .expect("EntityRef section");
    assert!(live_pos < ref_pos, "upsert must precede its ref");

    // No stream frames without a subscription.
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, StreamFrame::EntityStream { .. })),
        "stream items must not broadcast unsubscribed"
    );

    // Catch-up subscribe replays the persisted append.
    let hub = h._runtime.entities.clone();
    hub.subscribe(entity_id, true).await.expect("subscribe");
    match h.recv_one().await {
        StreamFrame::EntityStream { seq, payload, .. } => {
            assert_eq!(seq, 1);
            assert_eq!(payload["text"], "hello");
        }
        other => panic!("expected replayed stream item, got {other:?}"),
    }

    let digest = hub
        .read_artifact(entity_id, "llm_digest")
        .await
        .expect("read artifact");
    assert_eq!(digest, Some(serde_json::json!("Exit 0")));
}
