use super::*;

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
                call_id, content, ..
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
