//! Integration tests for the `frances:v1/tools/variable` module —
//! exercises the `Variables` store and the `Get`/`Set` tool classes
//! through the workflow runtime against `StubDeps`.

use std::io::Write;
use std::time::Duration;

use frances_workflow::{
    FrameKind, HostFrame, Invocation, Runtime, WorkflowError, WorkflowHandle, test_deps::StubDeps,
};

fn write_source(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".js")
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

const CYCLE_TIMEOUT: Duration = Duration::from_secs(5);

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
            FrameKind::Markdown { content, .. } | FrameKind::Error { content } => content.clone(),
            FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
        },
        HostFrame::Append { delta, .. } => delta.clone(),
        HostFrame::Approval(req) => format!("[approval:{}] {}", req.id, req.prompt),
    }
}

#[tokio::test]
async fn variables_round_trip_from_js() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const vars = new Variables();
        vars.set("plan", { steps: ["a", "b"], done: false });
        const back = vars.get("plan");
        transcript.push(new MarkdownFrame({ content: JSON.stringify(back) }));
        transcript.push(new MarkdownFrame({ content: String(vars.has("plan")) }));
        transcript.push(new MarkdownFrame({ content: String(vars.has("missing")) }));
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
    assert_eq!(text_of(&frames[0]), r#"{"steps":["a","b"],"done":false}"#);
    assert_eq!(text_of(&frames[1]), "true");
    assert_eq!(text_of(&frames[2]), "false");
}

#[tokio::test]
async fn variable_assign_evaluates_jq_against_dot_and_bindings() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables, Set, Assign } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const vars = new Variables();
        const set = new Set(vars);
        const assign = new Assign(vars);

        // Construct a fresh value (no `.` reference).
        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "plan",
                                 filter: "{steps: [\"a\",\"b\"], done: false}" } },
            scope: null,
        });

        // Mutate via `.` (the destination's current value).
        await assign.handler({
            call: { id: "a2", name: "variable_assign",
                    arguments: { name: "plan", filter: ".steps += [\"c\"]" } },
            scope: null,
        });

        // Bind another variable as $name.
        await set.handler({
            call: { id: "s1", name: "variable_set",
                    arguments: { name: "extra", value: 7 } },
            scope: null,
        });
        const r = await assign.handler({
            call: { id: "a3", name: "variable_assign",
                    arguments: { name: "summary",
                                 filter: "{count: ($plan.steps | length), extra: $extra}",
                                 inputs: ["plan", "extra"] } },
            scope: null,
        });

        transcript.push(new MarkdownFrame({ content: JSON.stringify(vars.get("plan")) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(vars.get("summary")) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));
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
        r#"{"steps":["a","b","c"],"done":false}"#
    );
    assert_eq!(text_of(&frames[1]), r#"{"count":3,"extra":7}"#);
    let r = text_of(&frames[2]);
    assert!(r.contains(r#""is_error":false"#), "result: {r}");
    assert!(
        r.contains(r#""content":"summary = object(2 keys)""#),
        "result: {r}",
    );
}

#[tokio::test]
async fn set_and_assign_responses_report_type_summary() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables, Set, Assign } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const vars = new Variables();
        const set = new Set(vars);
        const assign = new Assign(vars);

        async function run(name, value) {
            const r = await set.handler({
                call: { id: "c", name: "variable_set", arguments: { name, value } },
                scope: null,
            });
            transcript.push(new MarkdownFrame({ content: r.content }));
        }
        await run("a", { x: 1, y: 2, z: 3 });
        await run("b", [1, 2, 3, 4, 5]);
        await run("c", "hello");
        await run("d", 42);
        await run("e", true);
        await run("f", null);

        // qwen-style double-encoding: array passed as a JSON string.
        // The set tool stores it verbatim — the type summary tells the
        // model it landed as `string` so it can fromjson next round.
        await run("encoded", "[1,2,3]");

        // Recover via assign+fromjson.
        const recovered = await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "encoded", filter: "fromjson" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: recovered.content }));
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

    assert_eq!(text_of(&frames[0]), "a = object(3 keys)");
    assert_eq!(text_of(&frames[1]), "b = array(5 items)");
    assert_eq!(text_of(&frames[2]), "c = string");
    assert_eq!(text_of(&frames[3]), "d = number");
    assert_eq!(text_of(&frames[4]), "e = boolean");
    assert_eq!(text_of(&frames[5]), "f = null");
    assert_eq!(text_of(&frames[6]), "encoded = string");
    assert_eq!(text_of(&frames[7]), "encoded = array(3 items)");
}

#[tokio::test]
async fn variable_assign_introspection_and_errors() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables, Set, Assign } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const vars = new Variables();
        const set = new Set(vars);
        const assign = new Assign(vars);

        await set.handler({
            call: { id: "s1", name: "variable_set",
                    arguments: { name: "obj", value: { a: 1, b: 2 } } },
            scope: null,
        });

        // `keys` introspection via $-binding.
        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "obj_keys",
                                 filter: "$obj | keys",
                                 inputs: ["obj"] } },
            scope: null,
        });

        // Multi-output filter errors.
        const multi = await assign.handler({
            call: { id: "a2", name: "variable_assign",
                    arguments: { name: "x", filter: "1, 2, 3" } },
            scope: null,
        });

        // Unknown input binding errors.
        const missing = await assign.handler({
            call: { id: "a3", name: "variable_assign",
                    arguments: { name: "x", filter: ".",
                                 inputs: ["nope"] } },
            scope: null,
        });

        transcript.push(new MarkdownFrame({ content: JSON.stringify(vars.get("obj_keys")) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(multi) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(missing) }));
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

    assert_eq!(text_of(&frames[0]), r#"["a","b"]"#);

    let multi = text_of(&frames[1]);
    assert!(multi.contains(r#""is_error":true"#), "multi: {multi}");
    assert!(multi.contains("multiple outputs"), "multi: {multi}");

    let missing = text_of(&frames[2]);
    assert!(missing.contains(r#""is_error":true"#), "missing: {missing}");
    assert!(missing.contains("unknown variable"), "missing: {missing}");
}

#[tokio::test]
async fn variable_get_and_set_tool_handlers_work() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables, Get, Set } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const vars = new Variables();
        const get = new Get(vars);
        const set = new Set(vars);

        const setResult = await set.handler({
            call: { id: "c1", name: "variable_set", arguments: { name: "x", value: { n: 42 } } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(setResult) }));

        const getResult = await get.handler({
            call: { id: "c2", name: "variable_get", arguments: { name: "x" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(getResult) }));

        const missing = await get.handler({
            call: { id: "c3", name: "variable_get", arguments: { name: "nope" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(missing) }));
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

    let set_msg = text_of(&frames[0]);
    assert!(set_msg.contains(r#""is_error":false"#), "set: {set_msg}");
    assert!(
        set_msg.contains(r#""content":"x = object(1 keys)""#),
        "set: {set_msg}",
    );

    let get_msg = text_of(&frames[1]);
    assert!(get_msg.contains(r#""is_error":false"#), "get: {get_msg}");
    assert!(get_msg.contains(r#"\"n\": 42"#), "get: {get_msg}");

    let miss_msg = text_of(&frames[2]);
    assert!(miss_msg.contains(r#""is_error":true"#), "miss: {miss_msg}");
    assert!(miss_msg.contains("unknown variable"), "miss: {miss_msg}");
}
