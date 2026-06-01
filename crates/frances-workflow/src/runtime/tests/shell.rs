use super::*;

#[tokio::test]
async fn shell_run_once_returns_done_for_short_command() {
    use super::test_deps::StubDepsRealShell;
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Shell } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const sh = new Shell();
        const outcome = await sh.runOnce("echo hello-shell");
        const summary = `kind=${outcome.kind} exit=${outcome.exit_code} hasOutput=${outcome.output.includes("hello-shell")}`;
        await sh.close();
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const sh = new Shell();
        // First run goes Quiet (short quiet, sleep still silent).
        const first = await sh.runOnce("sleep 3", { quiet: 0.3 });
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
        transcript.push(new MarkdownSection({
            content: `firstKind=${first.kind} caught=${caught.includes("busy") ? "busy" : caught}`,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const sh = new Shell();
        // Background a sleep + then echo. With a short quiet the first
        // runOnce goes Quiet while the sleep is still silent, and
        // keepWaiting (default quiet) catches the final exit + echo output.
        let first = await sh.runOnce("sleep 2 && echo finished", { quiet: 0.3 });
        let final_ = first;
        let waits = 0;
        while (final_.kind === "quiet" && waits < 10) {
            waits += 1;
            final_ = await sh.keepWaiting();
        }
        await sh.close();
        transcript.push(new MarkdownSection({
            content: `firstKind=${first.kind} finalKind=${final_.kind} exit=${final_.exit_code} hasFinished=${final_.output.includes("finished")}`,
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
    use super::test_deps::StubDepsRealShell;
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDepsRealShell::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "shell_run".to_owned(),
            arguments: json!({ "cmd": "echo from-run-tool" }),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
async fn shell_run_head_and_tail_clamp_model_output() {
    // `head` + `tail` bound the copy returned to the model; the middle is
    // elided with a marker. (The full stream still goes to the frame.)
    use super::test_deps::StubDepsRealShell;
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDepsRealShell::default();
    let call = ToolCall {
        error: None,
        id: "c1".to_owned(),
        name: "shell_run".to_owned(),
        arguments: json!({ "cmd": "seq 1 100", "head": 2, "tail": 2 }),
    };
    deps.script_next_run(
        vec![StreamEvent::ToolCall(call.clone())],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![call],
        },
    );

    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const chat = new ChatSession({ model_intents: ["x"] });
        const sh = new Shell();
        chat.tools.push(new Run(sh, { approve: false }), new Wait(sh), new Kill(sh));
        chat.push({ role: "user", content: "count" });
        const r = await chat.stream();
        const reader = r.events.getReader();
        while (true) { const { done } = await reader.read(); if (done) break; }
        reader.releaseLock();
        await r.completed;
        await sh.close();
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
    let (_frames, done) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        drive_one_cycle(&mut handle),
    )
    .await
    .expect("test should finish within 10s");
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

    let sessions = deps.sessions();
    let content = sessions[0]
        .pending()
        .iter()
        .find_map(|p| match p {
            frances_models_llm::chat::OwnedHistoryInput::ToolResult {
                call_id, content, ..
            } if call_id == "c1" => Some(content.clone()),
            _ => None,
        })
        .expect("shell_run result present");
    assert!(content.starts_with("Exit 0"), "got `{content}`");
    assert!(content.contains("1\n2\n"), "head lines kept: `{content}`");
    assert!(content.contains("99\n100"), "tail lines kept: `{content}`");
    assert!(content.contains("elided"), "middle elided: `{content}`");
    assert!(!content.contains("\n50\n"), "middle dropped: `{content}`");
}

#[tokio::test]
async fn shell_run_quiet_registers_turn_for_wait_kill_negotiation() {
    // We script several shell_wait rounds because keepWaiting's
    // default 1s quiet window can time out before the sentinel
    // arrives, especially under load — Run's turn will loop until
    // one of them catches Done.
    use super::test_deps::StubDepsRealShell;
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDepsRealShell::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "shell_run".to_owned(),
            arguments: json!({ "cmd": "sleep 3 && echo finished", "quiet": 0.3 }),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 3 && echo finished", "quiet": 0.3 }),
            }],
        },
    );
    for i in 0..5 {
        let id = format!("w-{i}");
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: id.clone(),
                name: "shell_wait".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
                call_id, content, ..
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
        } if call_id.starts_with("w-") && content.starts_with("Exit 0") => Some(content.clone()),
        _ => None,
    });
    let waited_done = waited_done.expect("at least one shell_wait should land Done");
    assert!(
        waited_done.contains("finished"),
        "Done result should contain final output: `{waited_done}`"
    );
}

#[tokio::test]
async fn shell_negotiation_provider_error_reconciles_shell() {
    // Regression: a provider error *inside* the wait/kill negotiation must not
    // leave the command running with its frame open. Before the fix the
    // erroring stream propagated out of the lock turn (caught upstream as a
    // synthetic message), the shell stayed `running`, and the next `shell_run`
    // then wedged forever on the busy shell. Now the negotiation catches the
    // error, kills the command, and hands control back.
    //
    // Only the initial shell_run is scripted; the negotiation's first
    // `scope.stream()` finds no script and the stub returns
    // `ProviderUnavailable`, standing in for the upstream 500 that bit us.
    use super::test_deps::StubDepsRealShell;
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDepsRealShell::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "shell_run".to_owned(),
            arguments: json!({ "cmd": "sleep 30 && echo done", "quiet": 0.3 }),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done", "quiet": 0.3 }),
            }],
        },
    );

    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { ChatSession } from "frances:v1/chat";
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
        const stillRunning = await sh.isRunning();
        await sh.close();
        transcript.push(new MarkdownSection({ content: `stillRunning=${stillRunning}` }));
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
    let (frames, done) = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        drive_one_cycle(&mut handle),
    )
    .await
    .expect("test should finish within 15s (a hang here is the bug)");
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

    // The command was killed, so the shell returned to idle.
    assert!(
        frames.iter().any(|f| text_of(f) == "stillRunning=false"),
        "shell should be reconciled to not-running: {frames:?}"
    );

    // The model was told the command was aborted because of the provider error.
    let sessions = deps.sessions();
    let aborted = sessions[0].pending().iter().any(|p| {
        matches!(
            p,
            frances_models_llm::chat::OwnedHistoryInput::User { text }
                if text.contains("Aborted the shell command")
        )
    });
    assert!(aborted, "expected an 'Aborted' notice pushed to the model");
}

#[tokio::test]
async fn shell_run_quiet_scolds_then_kills_when_model_silent() {
    use super::test_deps::StubDepsRealShell;
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDepsRealShell::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "shell_run".to_owned(),
            arguments: json!({ "cmd": "sleep 30 && echo done", "quiet": 0.3 }),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done", "quiet": 0.3 }),
            }],
        },
    );
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
    // The model emits off-script tool calls each round. The gating hook turns
    // them into error tool_results; the no-progress counter still ticks and
    // the shell is eventually killed.
    use super::test_deps::StubDepsRealShell;
    use frances_models_llm::{CompletionOutcome, StreamEvent, ToolCall};
    use serde_json::json;

    let deps = StubDepsRealShell::default();
    deps.script_next_run(
        vec![StreamEvent::ToolCall(ToolCall {
            error: None,
            id: "c1".to_owned(),
            name: "shell_run".to_owned(),
            arguments: json!({ "cmd": "sleep 30 && echo done", "quiet": 0.3 }),
        })],
        CompletionOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                error: None,
                id: "c1".to_owned(),
                name: "shell_run".to_owned(),
                arguments: json!({ "cmd": "sleep 30 && echo done", "quiet": 0.3 }),
            }],
        },
    );
    for i in 0..3 {
        let id = format!("offscript-{i}");
        deps.script_next_run(
            vec![StreamEvent::ToolCall(ToolCall {
                error: None,
                id: id.clone(),
                name: "read_file".to_owned(),
                arguments: json!({}),
            })],
            CompletionOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    error: None,
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
        import { transcript, MarkdownSection } from "frances:v1/sections";
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
