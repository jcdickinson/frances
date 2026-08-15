use super::*;

fn outcome(
    text: &str,
    tool_calls: Vec<frances_models_llm::ToolCall>,
) -> frances_models_llm::CompletionOutcome {
    frances_models_llm::CompletionOutcome {
        text: text.to_owned(),
        tool_calls,
    }
}

fn decide_call() -> frances_models_llm::ToolCall {
    frances_models_llm::ToolCall {
        error: None,
        id: "c1".into(),
        name: "decide".into(),
        arguments: serde_json::json!({ "verdict": "approve" }),
    }
}

#[tokio::test]
async fn complete_plain_returns_text() {
    let deps = StubDeps::default();
    deps.script_next_run(Vec::new(), outcome("the answer", Vec::new()));
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { complete } from "frances:v1/chat";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const r = await complete({
            intents: ["default"],
            input: [{ role: "user", content: "hi" }],
        });
        transcript.push(new ErrorSection({ content: r.text }));
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
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(result.is_ok(), "got {result:?}");
    assert_eq!(text_of(&frames[0]), "the answer");
}

#[tokio::test]
async fn complete_required_returns_tool_call() {
    let deps = StubDeps::default();
    deps.script_next_run(Vec::new(), outcome("", vec![decide_call()]));
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { complete } from "frances:v1/chat";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const r = await complete({
            intents: ["default"],
            input: [{ role: "user", content: "decide please" }],
            tools: [{ name: "decide", description: "d", parameters: { type: "object" } }],
            requireToolCall: true,
        });
        transcript.push(new ErrorSection({ content: r.tool_calls.map((c) => c.name).join(",") }));
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
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(result.is_ok(), "got {result:?}");
    assert_eq!(text_of(&frames[0]), "decide");
}

#[tokio::test]
async fn complete_enforced_retries_then_succeeds() {
    let deps = StubDeps::default();
    // Round 1: no tool call → scold. Round 2: the demanded call.
    deps.script_next_run(Vec::new(), outcome("thinking", Vec::new()));
    deps.script_next_run(Vec::new(), outcome("", vec![decide_call()]));
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { complete } from "frances:v1/chat";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const r = await complete({
            intents: ["default"],
            input: [{ role: "user", content: "decide" }],
            toolChoice: "decide",
        });
        transcript.push(new ErrorSection({ content: r.tool_calls.map((c) => c.name).join(",") }));
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
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(result.is_ok(), "got {result:?}");
    assert_eq!(text_of(&frames[0]), "decide");
}

#[tokio::test]
async fn complete_unsatisfied_rejects() {
    let deps = StubDeps::default();
    // retries defaults to 1 ⇒ two rounds, neither calls a tool.
    deps.script_next_run(Vec::new(), outcome("nope", Vec::new()));
    deps.script_next_run(Vec::new(), outcome("still nope", Vec::new()));
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { complete } from "frances:v1/chat";
        import { transcript, ErrorSection } from "frances:v1/sections";
        try {
            await complete({
                intents: ["default"],
                input: [{ role: "user", content: "decide" }],
                requireToolCall: true,
            });
            transcript.push(new ErrorSection({ content: "NO THROW" }));
        } catch (e) {
            transcript.push(new ErrorSection({ content: "threw:" + String((e && e.message) || e) }));
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
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(result.is_ok(), "got {result:?}");
    let rendered = text_of(&frames[0]);
    assert!(
        rendered.starts_with("threw:") && rendered.contains("forced tool not satisfied"),
        "expected an enforce rejection, got {rendered:?}",
    );
}

#[tokio::test]
async fn complete_flags_schema_invalid_tool_call() {
    let deps = StubDeps::default();
    // `decide_call` supplies only `verdict`; the schema also requires
    // `reason`, so the chat layer flags the call.
    deps.script_next_run(Vec::new(), outcome("", vec![decide_call()]));
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { complete } from "frances:v1/chat";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const r = await complete({
            intents: ["default"],
            input: [{ role: "user", content: "decide" }],
            tools: [{ name: "decide", description: "d", parameters: {
                type: "object",
                additionalProperties: false,
                properties: { verdict: { type: "string" }, reason: { type: "string" } },
                required: ["verdict", "reason"],
            } }],
        });
        const c = r.tool_calls[0];
        const ok = c.error && c.expectedSchema && c.expectedSchema.required.includes("reason");
        transcript.push(new ErrorSection({ content: ok ? "flagged:" + c.name : "clean" }));
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
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(result.is_ok(), "got {result:?}");
    assert_eq!(text_of(&frames[0]), "flagged:decide");
}
