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
        s._pushSystem("you are a summariser");
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
async fn chat_session_can_be_persisted_and_loaded_by_id() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession, loadChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const created = new ChatSession({ model_intents: ["x"] });
        if (created.id() !== null) throw new Error("fresh chat should not have id yet");
        const id = await created.ensurePersisted();
        if (id !== 1) throw new Error(`expected persisted id 1, got ${id}`);
        if (created.id() !== 1) throw new Error(`expected id() to return 1, got ${created.id()}`);

        const loaded = await loadChatSession(id);
        if (loaded.id() !== id) throw new Error(`loaded wrong id ${loaded.id()}`);
        transcript.push(new MarkdownSection({ content: `loaded ${loaded.id()}` }));
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
    assert_eq!(text_of(&frames[0]), "loaded 1");
}

#[tokio::test]
async fn chat_session_effort_can_be_set_cleared_and_loaded() {
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession, loadChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const created = new ChatSession({ model_intents: ["x"] });
        if (created.effort !== null) throw new Error("fresh effort should be null");
        created.effort = 42;
        if (created.effort !== 42) throw new Error(`expected 42, got ${created.effort}`);
        created.effort = null;
        if (created.effort !== null) throw new Error("cleared effort should be null");

        const id = await created.ensurePersisted();
        const loaded = await loadChatSession(id);
        if (loaded.effort !== null) throw new Error("loaded effort should start null");
        loaded.effort = 100;
        transcript.push(new MarkdownSection({ content: `effort ${loaded.effort}` }));
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
    assert_eq!(text_of(&frames[0]), "effort 100");

    let sessions = deps.sessions();
    assert_eq!(sessions.len(), 2);
    assert_eq!(frances_models_llm::ChatSession::effort(&sessions[0]), None);
    assert_eq!(
        frances_models_llm::ChatSession::effort(&sessions[1]).map(|effort| effort.get()),
        Some(100)
    );
}

#[tokio::test]
async fn chat_session_effort_rejects_invalid_values_without_mutating() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        const session = new ChatSession({ model_intents: ["x"] });
        session.effort = 25;

        for (const invalid of [undefined, true, "50", 1.5, -1, 101, NaN, Infinity, {}]) {
            let rejected = false;
            try {
                session.effort = invalid;
            } catch (error) {
                rejected = String(error).includes(
                    "ChatSession.effort must be null or an integer from 0 through 100",
                );
            }
            if (!rejected) throw new Error(`accepted invalid effort ${String(invalid)}`);
            if (session.effort !== 25) throw new Error("invalid assignment mutated effort");
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
    let (_frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
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
async fn chat_session_rejects_system_role() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        const s = new ChatSession({ model_intents: ["x"] });
        s.push({ role: "system", content: "not allowed" });
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
async fn chat_session_push_system_bypass_allows_multiple() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s._pushSystem("be terse");
        s._pushSystem("answer in english");
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
    // Only public prototype methods, the internal escape hatches
    // (`_envInfo`, `_pushSystem`), plus the JS-installed `stream`
    // should appear; the inner raw stream function must not.
    assert_eq!(
        text_of(&frames[0]),
        "proto=_envInfo,_pushSystem,effort,ensurePersisted,id,push,stream stash=true"
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
        const shape = `a=${Array.isArray(a.tools)} len=${a.tools.length} distinct=${a.tools !== b.tools} ps=${Array.isArray(a.promptSections)} psLen=${a.promptSections.length} psDistinct=${a.promptSections !== b.promptSections}`;
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
    assert_eq!(
        text_of(&frames[0]),
        "a=true len=0 distinct=true ps=true psLen=0 psDistinct=true"
    );
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
async fn stream_interrupt_during_dispatch_leaves_dangling_call_for_provider() {
    // When the signal aborts after the round streamed its tool calls but
    // while a handler is still running, dispatch stops waiting and leaves the
    // unfinished call unanswered. Validity is the provider's job now: it
    // backfills a synthetic "cancelled" result for any dangling tool call when
    // it assembles the request, so the ChatSession itself emits no result here.
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "h1".to_owned(),
            name: "hang".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "h1".to_owned(),
                name: "hang".to_owned(),
                arguments: json!({}),
            }],
        },
    );

    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { AbortController } from "whatwg:abortcontroller";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        // Handler never settles on its own — only the interrupt resolves it.
        s.tools.push({
            name: "hang",
            description: "never returns",
            parameters: { type: "object" },
            handler: () => new Promise(() => {}),
        });
        s.push({ role: "user", content: "go" });

        const ac = new AbortController();
        const r = await s.stream({ signal: ac.signal });
        // Drain events so the round settles and dispatch begins.
        const reader = r.events.getReader();
        while (true) { const { done } = await reader.read(); if (done) break; }
        reader.releaseLock();
        // Abort mid-dispatch — the hang handler is still in flight.
        ac.abort("stop");
        const { tool_calls } = await r.completed;
        transcript.push(new MarkdownSection({ content: `calls=${tool_calls.length}` }));
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
    assert_eq!(text_of(frames.last().expect("a frame")), "calls=1");

    // The unfinished call is left dangling — no tool result is pushed. The
    // provider synthesizes one when it next assembles the request.
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
    assert_eq!(tool_result, None);
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

#[tokio::test]
async fn chat_push_system_leads_even_when_pushed_after_user() {
    // `_pushSystem(text)` pushes OwnedHistoryInput::System directly,
    // bypassing the public push() restriction on role "system". Even when
    // called after the user message is queued, the system content jumps
    // ahead of it (and multiple sections cluster in push order) so a
    // leading system message can become the Responses `instructions`.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.push({ role: "user", content: "hi" });
        // Regular push({ role: "system" }) would throw here, but
        // _pushSystem bypasses the guard.
        s._pushSystem("injected by section assembly");
        s._pushSystem("second system message");
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

    // Inspect the underlying stub session's pending queue.
    let sessions = deps.sessions();
    assert_eq!(sessions.len(), 1);
    let pending = sessions[0].pending();
    // Expected: system, system, user — systems lead in push order despite
    // the user message having been queued first.
    assert_eq!(pending.len(), 3);
    match &pending[0] {
        frances_models_llm::chat::OwnedHistoryInput::System { text } => {
            assert_eq!(text, "injected by section assembly");
        }
        other => panic!("expected System, got {other:?}"),
    }
    match &pending[1] {
        frances_models_llm::chat::OwnedHistoryInput::System { text } => {
            assert_eq!(text, "second system message");
        }
        other => panic!("expected System, got {other:?}"),
    }
    assert!(matches!(
        &pending[2],
        frances_models_llm::chat::OwnedHistoryInput::User { text } if text == "hi"
    ));
}

#[tokio::test]
async fn chat_push_system_bypass_works_before_user_message() {
    // `_pushSystem` also works before user messages, like normal system push.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s._pushSystem("early system");
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
    let (_frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

    let sessions = deps.sessions();
    let pending = sessions[0].pending();
    assert_eq!(pending.len(), 2);
    match &pending[0] {
        frances_models_llm::chat::OwnedHistoryInput::System { text } => {
            assert_eq!(text, "early system");
        }
        other => panic!("expected System, got {other:?}"),
    }
    assert!(matches!(
        &pending[1],
        frances_models_llm::chat::OwnedHistoryInput::User { text } if text == "hi"
    ));
}

// ---------------------------------------------------------------------------
// _envInfo() tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_env_info_returns_correct_shape() {
    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![PathBuf::from("/my/repo")]);
    deps.set_cwd(PathBuf::from("/my/repo/src"));

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        const info = s._envInfo();
        const keys = Object.keys(info).sort();
        const hasOs = typeof info.os === "string" && info.os.length > 0;
        const hasShell = typeof info.shell === "string" && info.shell.length > 0;
        const hasPlatform = typeof info.platform === "string" && info.platform.length > 0;
        const hasRepoRoot = info.repoRoot === "/my/repo";
        const hasCwd = info.cwd === "/my/repo/src";
        const hasDate = /^\d{4}-\d{2}-\d{2}$/.test(info.date);
        transcript.push(new MarkdownSection({ content: JSON.stringify({
            keys, hasOs, hasShell, hasPlatform, hasRepoRoot, hasCwd, hasDate,
        }) }));
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
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["keys"],
        serde_json::json!(["cwd", "date", "os", "platform", "repoRoot", "shell"]),
        "unexpected keys"
    );
    assert_eq!(result["hasOs"], true, "os should be non-empty string");
    assert_eq!(result["hasShell"], true, "shell should be non-empty string");
    assert_eq!(
        result["hasPlatform"], true,
        "platform should be non-empty string"
    );
    assert_eq!(result["hasRepoRoot"], true, "repoRoot should be /my/repo");
    assert_eq!(result["hasCwd"], true, "cwd should be /my/repo/src");
    assert_eq!(result["hasDate"], true, "date should be YYYY-MM-DD");
}

#[tokio::test]
async fn chat_env_info_null_repo_root_and_cwd_when_missing() {
    // default editable_roots is vec!["/"] which is set, but let's use empty
    // to get null repoRoot. And don't set cwd to get null.
    // Actually, default editable_roots is vec!["/"] so repoRoot will be "/".
    // Let's use empty vec to get null repoRoot.
    let mut deps2 = StubDeps::default();
    deps2.set_editable_roots(vec![]);
    // cwd unset → null

    let rt = Runtime::new(deps2).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        const info = s._envInfo();
        transcript.push(new MarkdownSection({ content: JSON.stringify({
            repoRoot: info.repoRoot,
            cwd: info.cwd,
        }) }));
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
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["repoRoot"],
        serde_json::Value::Null,
        "repoRoot should be null with empty roots"
    );
    assert_eq!(
        result["cwd"],
        serde_json::Value::Null,
        "cwd should be null when not set"
    );
}

// ---------------------------------------------------------------------------
// Prompt section rendering tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_prompt_sections_render_before_stream() {
    // Sections pushed into promptSections are rendered and injected via
    // _pushSystem before the provider stream starts. We verify by inspecting
    // the stub session's pending queue — system content from sections should
    // appear before the user message.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.promptSections.push({
            name: "test-section",
            prompt(ctx) { return "test-instruction: be brief"; },
        });
        s.push({ role: "user", content: "hi" });
        // Trigger section rendering by calling stream (will error on stub,
        // but sections are rendered before the provider call).
        try { await s.stream(); } catch (_) {}
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
    // The system message from section rendering should be present.
    let system_entries: Vec<_> = pending
        .iter()
        .filter_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::System { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        system_entries
            .iter()
            .any(|t| t.contains("test-instruction: be brief")),
        "expected section content in system messages, got: {system_entries:?}"
    );
}

#[tokio::test]
async fn chat_prompt_sections_skip_null_results() {
    // Sections returning null are skipped; only non-null results are
    // concatenated into the system message.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.promptSections.push({
            name: "null-section",
            prompt(ctx) { return null; },
        });
        s.promptSections.push({
            name: "active-section",
            prompt(ctx) { return "active instruction"; },
        });
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
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
    let system_text: Vec<_> = pending
        .iter()
        .filter_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::System { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    // Should have one system message containing only the active section
    assert!(
        system_text.iter().any(|t| t == "active instruction"),
        "expected only active instruction, got: {system_text:?}"
    );
    assert!(
        !system_text.iter().any(|t| t.contains("null")),
        "null section should not produce content, got: {system_text:?}"
    );
}

#[tokio::test]
async fn chat_prompt_sections_all_null_no_system_push() {
    // When all sections return null, no system message is pushed at all.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.promptSections.push({
            name: "null-a",
            prompt(ctx) { return null; },
        });
        s.promptSections.push({
            name: "null-b",
            prompt(ctx) { return null; },
        });
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
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
    let has_system = pending.iter().any(|p| {
        matches!(
            p,
            frances_models_llm::chat::OwnedHistoryInput::System { .. }
        )
    });
    assert!(
        !has_system,
        "no system message should be pushed when all sections return null"
    );
}

#[tokio::test]
async fn chat_prompt_sections_render_in_push_order() {
    // Sections are rendered in the order they were pushed. The concatenated
    // system message should reflect that ordering.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.promptSections.push({
            name: "first",
            prompt(ctx) { return "AAA"; },
        });
        s.promptSections.push({
            name: "second",
            prompt(ctx) { return "BBB"; },
        });
        s.promptSections.push({
            name: "third",
            prompt(ctx) { return "CCC"; },
        });
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
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
    let system_text: Vec<_> = pending
        .iter()
        .filter_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::System { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        system_text.len(),
        1,
        "expected one system message, got {system_text:?}"
    );
    let text = &system_text[0];
    let aaa_pos = text.find("AAA").expect("should contain AAA");
    let bbb_pos = text.find("BBB").expect("should contain BBB");
    let ccc_pos = text.find("CCC").expect("should contain CCC");
    assert!(aaa_pos < bbb_pos, "AAA should appear before BBB");
    assert!(bbb_pos < ccc_pos, "BBB should appear before CCC");
}

#[tokio::test]
async fn chat_prompt_sections_ctx_includes_tools() {
    // The ctx passed to section.prompt() includes the tools array from the
    // chat session, allowing sections like toolGuidance to inspect tools.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.tools.push({ name: "my_tool", description: "test", parameters: {}, handler: async () => ({}) });
        let capturedCtx = null;
        s.promptSections.push({
            name: "ctx-capture",
            prompt(ctx) { capturedCtx = ctx; return "captured"; },
        });
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
        // capturedCtx is populated synchronously before the stream errors
        const hasTools = Array.isArray(capturedCtx.tools) && capturedCtx.tools.length === 1;
        const hasTool = capturedCtx && capturedCtx.tools[0].name === "my_tool";
        transcript.push(new MarkdownSection({ content: JSON.stringify({ hasTools, hasTool }) }));
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
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["hasTools"], true,
        "ctx.tools should be an array with 1 element"
    );
    assert_eq!(
        result["hasTool"], true,
        "ctx.tools[0].name should be my_tool"
    );
}

#[tokio::test]
async fn chat_prompt_sections_ctx_includes_env_info() {
    // The ctx passed to section.prompt() includes the env info fields
    // from _envInfo() (os, shell, platform, repoRoot, cwd, date).
    let deps = StubDeps::default();
    deps.set_cwd(PathBuf::from("/test/dir"));

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        let capturedCtx = null;
        s.promptSections.push({
            name: "env-capture",
            prompt(ctx) { capturedCtx = ctx; return "captured"; },
        });
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
        const hasOs = typeof capturedCtx.os === "string";
        const hasShell = typeof capturedCtx.shell === "string";
        const hasPlatform = typeof capturedCtx.platform === "string";
        const hasDate = typeof capturedCtx.date === "string";
        const hasCwd = capturedCtx.cwd === "/test/dir";
        transcript.push(new MarkdownSection({ content: JSON.stringify({
            hasOs, hasShell, hasPlatform, hasDate, hasCwd,
        }) }));
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
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["hasOs"], true, "ctx should have os");
    assert_eq!(result["hasShell"], true, "ctx should have shell");
    assert_eq!(result["hasPlatform"], true, "ctx should have platform");
    assert_eq!(result["hasDate"], true, "ctx should have date");
    assert_eq!(result["hasCwd"], true, "ctx should have cwd from _envInfo");
}

#[tokio::test]
async fn chat_prompt_sections_support_async_prompt() {
    // Sections may return promises (async prompt functions). The rendering
    // loop should await each section's result.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        s.promptSections.push({
            name: "async-section",
            async prompt(ctx) {
                await Promise.resolve();
                return "async-result";
            },
        });
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
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
    let has_async = pending.iter().any(|p| matches!(
        p,
        frances_models_llm::chat::OwnedHistoryInput::System { text } if text.contains("async-result")
    ));
    assert!(
        has_async,
        "async section result should appear in system messages"
    );
}

#[tokio::test]
async fn chat_empty_prompt_sections_no_system_push() {
    // When promptSections is an empty array, no system message is pushed.
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const s = new ChatSession({ model_intents: ["x"] });
        // promptSections is [] by default — no sections pushed
        s.push({ role: "user", content: "hi" });
        try { await s.stream(); } catch (_) {}
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
    let has_system = pending.iter().any(|p| {
        matches!(
            p,
            frances_models_llm::chat::OwnedHistoryInput::System { .. }
        )
    });
    assert!(
        !has_system,
        "no system message should be pushed with empty promptSections"
    );
}
