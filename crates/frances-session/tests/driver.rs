//! Native harness integration: real session storage and scripted model calls.
use frances_config::{InMemoryProvider, Value as ConfigValue};
use frances_llm::test_util::{StubProvider, StubScript};
use frances_models_llm::{CompletionOutcome, OwnedHistoryInput, StreamEvent, ToolCall};
use frances_session::{
    context::InvocationContext,
    events::{Lifecycle, PermissionResponseWire, StreamFrame},
    runtime::{SessionRuntime, StartOverrides},
    session::Paths,
    store,
    workspace::Workspace,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedReceiver;

struct Harness {
    runtime: Arc<SessionRuntime>,
    events: UnboundedReceiver<StreamFrame>,
    stub: Arc<StubProvider>,
    temp: tempfile::TempDir,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.runtime.shutdown();
    }
}
impl Harness {
    async fn new(scripts: Vec<StubScript>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            state_root: temp.path().join("state"),
            runtime_root: temp.path().join("runtime"),
        };
        paths.ensure_layout().unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();
        let session = paths.create_session(&workspace).unwrap();
        let db = store::open(&session).await.unwrap();
        let mut config = InMemoryProvider::new();
        for (path, value) in [
            ("models.default.model_provider", "test"),
            ("models.default.id", "test-model"),
            ("model_providers.test.kind", "openai-chat"),
            ("model_providers.test.base_url", "http://stub.invalid"),
            ("model_providers.test.auth.token", "stub-token"),
        ] {
            config = config.set(
                path.split('.')
                    .map(|s| ConfigValue::String(s.into()))
                    .collect::<Vec<_>>(),
                value,
            );
        }
        let stub = Arc::new(StubProvider::new());
        for script in scripts {
            stub.push_script(script);
        }
        let inserted = stub.clone();
        let (runtime, events) = SessionRuntime::start_with(
            session,
            db,
            InvocationContext::capture(workspace),
            StartOverrides {
                extra_config_providers: vec![Arc::new(config)],
                on_cache: Some(Box::new(move |cache| cache.insert_stub("test", inserted))),
            },
        )
        .await
        .unwrap();
        Self {
            runtime,
            events,
            stub,
            temp,
        }
    }
    async fn frame(&mut self) -> StreamFrame {
        tokio::time::timeout(Duration::from_secs(10), self.events.recv())
            .await
            .expect("driver timed out")
            .expect("driver closed")
    }
    async fn idle(&mut self) -> Vec<StreamFrame> {
        let mut frames = vec![];
        let mut started = false;
        loop {
            let frame = self.frame().await;
            if let StreamFrame::EntityUpsert { envelope, snapshot } = &frame
                && envelope.kind == "session"
            {
                if snapshot["busy"].is_string() {
                    started = true;
                } else if started {
                    frames.push(frame);
                    return frames;
                }
            }
            if let StreamFrame::Error(error) = &frame {
                panic!("driver error: {error}");
            }
            frames.push(frame);
        }
    }
    async fn permission(&mut self) -> frances_harness::PermissionRequest {
        loop {
            if let StreamFrame::Permission(request) = self.frame().await {
                return request;
            }
        }
    }
    async fn idle_after_started(&mut self) {
        loop {
            if let StreamFrame::EntityUpsert { envelope, snapshot } = self.frame().await
                && envelope.kind == "session"
                && snapshot["busy"].is_null()
            {
                return;
            }
        }
    }
}
fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments,
        error: None,
    }
}
fn calls(calls: Vec<ToolCall>) -> StubScript {
    StubScript {
        events: vec![],
        outcome: CompletionOutcome {
            text: String::new(),
            tool_calls: calls,
        },
    }
}
fn text(text: &str) -> StubScript {
    StubScript {
        events: vec![StreamEvent::TextDelta(text.into())],
        outcome: CompletionOutcome {
            text: text.into(),
            tool_calls: vec![],
        },
    }
}
fn reject() -> StubScript {
    calls(vec![call(
        "decide",
        json!({"verdict":"reject","reason":"ask the user"}),
    )])
}

#[tokio::test]
async fn ordinary_chat_streams_and_slashes_are_plain_input() {
    let mut h = Harness::new(vec![text("Hello"), text("Again")]).await;
    h.runtime.prompt("/this/is/a/path".into());
    let frames = h.idle().await;
    assert!(frames.iter().any(|frame| matches!(frame, StreamFrame::EntityUpsert { envelope, snapshot } if envelope.lifecycle == Lifecycle::Settled && snapshot["text"] == "Hello")));
    h.runtime.prompt("Continue".into());
    h.idle().await;
    let requests = h.stub.captured();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].session_id, requests[1].session_id);
    assert!(requests[0].new_inputs.iter().any(
        |input| matches!(input, OwnedHistoryInput::User { text } if text == "/this/is/a/path")
    ));
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|name| name.starts_with("plan_") || name.starts_with("mcp"))
    );
    assert!(requests[0].tools.iter().any(|name| name == "file_read"));
    assert_eq!(requests[0].tools, requests[1].tools);
}

#[tokio::test]
async fn file_tools_execute_batches_and_keep_history_and_diff() {
    let mut h = Harness::new(vec![
        calls(vec![call(
            "file_new",
            json!({"path":"nested/test.txt","text":"before"}),
        )]),
        calls(vec![call("file_read", json!({"path":"nested/test.txt"}))]),
        calls(vec![call(
            "file_overwrite",
            json!({"path":"nested/test.txt","text":"after"}),
        )]),
        text("Done"),
    ])
    .await;
    h.runtime.prompt("Create and edit a file".into());
    let frames = h.idle().await;
    assert_eq!(
        std::fs::read_to_string(h.temp.path().join("nested/test.txt")).unwrap(),
        "after\n"
    );
    assert!(frames.iter().any(|f| matches!(
        f,
        StreamFrame::Section(frances_session::events::SectionKind::Diff { .. })
    )));
    let requests = h.stub.captured();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert!(request.new_inputs.iter().any(|i| matches!(
            i,
            OwnedHistoryInput::ToolResult {
                is_error: false,
                ..
            }
        )));
    }
    let db = store::open(&h.runtime.session).await.unwrap();
    assert!(
        !frances_session::scrollback::load(&db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_and_unknown_calls_never_execute() {
    let mut h = Harness::new(vec![
        calls(vec![
            call("file_new", json!({"path":"oops"})),
            call("plan_exit", json!({})),
        ]),
        text("Recovered"),
    ])
    .await;
    h.runtime.prompt("Test malformed calls".into());
    h.idle().await;
    assert!(!h.temp.path().join("oops").exists());
    let requests = h.stub.captured();
    let results: Vec<_> = requests[1]
        .new_inputs
        .iter()
        .filter(|i| matches!(i, OwnedHistoryInput::ToolResult { is_error: true, .. }))
        .collect();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn denied_shell_has_no_effect_and_continues() {
    let mut h = Harness::new(vec![
        calls(vec![call("shell_run", json!({"cmd":"touch denied"}))]),
        reject(),
        text("Denied"),
    ])
    .await;
    h.runtime.prompt("Run a command".into());
    let request = h.permission().await;
    h.runtime
        .respond_permission(
            request.reply,
            PermissionResponseWire::No {
                details: Some("No".into()),
            },
        )
        .unwrap();
    h.idle_after_started().await;
    assert!(!h.temp.path().join("denied").exists());
    assert!(h.stub.captured().last().unwrap().new_inputs.iter().any(|i| matches!(i, OwnedHistoryInput::ToolResult { is_error:true, content, .. } if content.contains("permission denied"))));
}

#[tokio::test]
async fn interrupt_permission_settles_remaining_calls_and_waits_for_user() {
    let mut h = Harness::new(vec![
        calls(vec![
            call("shell_run", json!({"cmd":"touch denied"})),
            call("file_new", json!({"path":"skipped","text":"no"})),
        ]),
        reject(),
        text("Resumed"),
    ])
    .await;
    h.runtime.prompt("Run a batch".into());
    let request = h.permission().await;
    h.runtime.interrupt();
    h.idle_after_started().await;
    assert!(request.reply.is_closed());
    assert!(!h.temp.path().join("denied").exists());
    assert!(!h.temp.path().join("skipped").exists());
    assert_eq!(h.stub.captured().len(), 2);
    h.runtime.prompt("Resume".into());
    h.idle().await;
    let requests = h.stub.captured();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2]
            .new_inputs
            .iter()
            .filter(|i| matches!(i, OwnedHistoryInput::ToolResult { is_error: true, .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn approved_shell_publishes_output_and_interrupt_kills_it() {
    let mut h = Harness::new(vec![
        calls(vec![call(
            "shell_run",
            json!({"cmd":"printf 'ready%.0s' {1..1000}; sleep 30; touch late","quiet":60,"max":60}),
        )]),
        reject(),
    ])
    .await;
    h.runtime.prompt("Run".into());
    let request = h.permission().await;
    h.runtime
        .respond_permission(request.reply, PermissionResponseWire::Yes { details: None })
        .unwrap();
    loop {
        if let StreamFrame::EntityUpsert { envelope, snapshot } = h.frame().await
            && envelope.kind == "shell"
            && snapshot["teaser"]
                .as_str()
                .is_some_and(|s| s.contains("ready"))
        {
            break;
        }
    }
    h.runtime.interrupt();
    h.idle_after_started().await;
    assert!(!h.temp.path().join("late").exists());
    assert_eq!(h.stub.captured().len(), 2);
}
