use super::*;

#[tokio::test]
async fn scope_lock_runs_after_batch_push() {
    // The post-batch turn registered via scope.lock fires AFTER all
    // initial tool_results have been pushed to chat history.
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "checker".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
        transcript.push(new MarkdownSection({ content: `turnRan=${turnRan}` }));
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
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![
            StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "first".to_owned(),
                name: "slow".to_owned(),
                arguments: json!({}),
            }),
            StreamEvent::ToolCall(ToolCall {
                error: None,
                id: "second".to_owned(),
                name: "fast".to_owned(),
                arguments: json!({}),
            }),
        ],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    error: None,
                    id: "first".to_owned(),
                    name: "slow".to_owned(),
                    arguments: json!({}),
                },
                ToolCall {
                    error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
        transcript.push(new MarkdownSection({ content: `turns=${turnOrder.join(",")}` }));
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
    assert_eq!(text_of(frames.last().unwrap()), "turns=fast,slow");
}

#[tokio::test]
async fn scope_lock_turn_can_drive_followup_stream() {
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "starter".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "starter".to_owned(),
                arguments: json!({}),
            }],
        },
    );
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c2".to_owned(),
            name: "followup".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
    let tool_results: Vec<(String, String)> = pending
        .iter()
        .filter_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id, content, ..
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
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "outer".to_owned(),
            name: "gated".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "outer".to_owned(),
                name: "gated".to_owned(),
                arguments: json!({}),
            }],
        },
    );
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "offscript".to_owned(),
            name: "forbidden".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "double".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
        frances_models_llm::chat::OwnedHistoryInput::ToolResult { content, .. } => {
            Some(content.clone())
        }
        _ => None,
    });
    assert_eq!(tool, Some("got-throw".to_owned()));
}

#[tokio::test]
async fn scope_lock_turn_fn_throw_does_not_crash_round() {
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDeps::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "thrower".to_owned(),
            arguments: json!({}),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
        transcript.push(new MarkdownSection({ content: "survived" }));
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
    assert_eq!(text_of(frames.last().unwrap()), "survived");

    let sessions = deps.sessions();
    let pending = sessions[0].pending();
    let synthetic = pending.iter().find_map(|p| match p {
        frances_models_llm::chat::OwnedHistoryInput::User { text } if text.contains("threw") => {
            Some(text.clone())
        }
        _ => None,
    });
    assert!(
        synthetic.is_some(),
        "expected synthetic user message in pending: {pending:?}"
    );
}
