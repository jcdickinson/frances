//! Integration tests for the `frances:v1/tools/variable` module —
//! exercises the `Variables` store and the `Get`/`Set` tool classes
//! through the workflow runtime against `StubDeps`.

use std::io::Write;

use frances_workflow::{
    Invocation, Runtime, SectionKind, SectionTranscript, test_deps::StubDeps,
    test_drive::drive_one_cycle,
};

fn write_source(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".js")
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

fn text_of(frame: &SectionTranscript) -> String {
    match frame {
        SectionTranscript::Set { section: spec, .. } => match &spec.kind {
            SectionKind::Error => spec.seed.clone().unwrap_or_default(),
            SectionKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
            SectionKind::Reasoning { state } => format!(
                "[reasoning:{state:?}]\n{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
            SectionKind::EntityRef { entity_id } => format!("[entity:{entity_id}]"),
        },
        SectionTranscript::Append { delta, .. } => delta.clone(),
        SectionTranscript::Close { id } => format!("[close:{}]", id.0),
        SectionTranscript::Entity(cmd) => format!("[entity:{cmd:?}]"),
    }
}

#[tokio::test]
async fn variables_round_trip_from_js() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const vars = new Variables();
        vars.set("plan", { steps: ["a", "b"], done: false });
        const back = vars.get("plan");
        transcript.push(new ErrorSection({ content: JSON.stringify(back) }));
        transcript.push(new ErrorSection({ content: String(vars.has("plan")) }));
        transcript.push(new ErrorSection({ content: String(vars.has("missing")) }));
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
        import { transcript, ErrorSection } from "frances:v1/sections";

        const vars = new Variables();
        const set = new Set(vars);
        const assign = new Assign(vars);

        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "plan",
                                 filter: "{steps: [\"a\",\"b\"], done: false}" } },
            scope: null,
        });

        await assign.handler({
            call: { id: "a2", name: "variable_assign",
                    arguments: { name: "plan", filter: ".steps += [\"c\"]" } },
            scope: null,
        });

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

        transcript.push(new ErrorSection({ content: JSON.stringify(vars.get("plan")) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(vars.get("summary")) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(r) }));
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
        import { transcript, ErrorSection } from "frances:v1/sections";

        const vars = new Variables();
        const set = new Set(vars);
        const assign = new Assign(vars);

        async function run(name, value) {
            const r = await set.handler({
                call: { id: "c", name: "variable_set", arguments: { name, value } },
                scope: null,
            });
            transcript.push(new ErrorSection({ content: r.content }));
        }
        await run("a", { x: 1, y: 2, z: 3 });
        await run("b", [1, 2, 3, 4, 5]);
        await run("c", "hello");
        await run("d", 42);
        await run("e", true);
        await run("f", null);

        await run("encoded", "[1,2,3]");

        const recovered = await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "encoded", filter: "fromjson" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: recovered.content }));
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
        import { transcript, ErrorSection } from "frances:v1/sections";

        const vars = new Variables();
        const set = new Set(vars);
        const assign = new Assign(vars);

        await set.handler({
            call: { id: "s1", name: "variable_set",
                    arguments: { name: "obj", value: { a: 1, b: 2 } } },
            scope: null,
        });

        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "obj_keys",
                                 filter: "$obj | keys",
                                 inputs: ["obj"] } },
            scope: null,
        });

        const multi = await assign.handler({
            call: { id: "a2", name: "variable_assign",
                    arguments: { name: "x", filter: "1, 2, 3" } },
            scope: null,
        });

        const missing = await assign.handler({
            call: { id: "a3", name: "variable_assign",
                    arguments: { name: "x", filter: ".",
                                 inputs: ["nope"] } },
            scope: null,
        });

        transcript.push(new ErrorSection({ content: JSON.stringify(vars.get("obj_keys")) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(multi) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(missing) }));
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

    assert_eq!(text_of(&frames[0]), r#"["a","b"]"#);

    let multi = text_of(&frames[1]);
    assert!(multi.contains(r#""is_error":true"#), "multi: {multi}");
    assert!(multi.contains("multiple outputs"), "multi: {multi}");

    let missing = text_of(&frames[2]);
    assert!(missing.contains(r#""is_error":true"#), "missing: {missing}");
    assert!(missing.contains("unknown variable"), "missing: {missing}");
}

#[tokio::test]
async fn variable_get_with_filter_lenses_into_stored_value() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables, Get, Set } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const vars = new Variables();
        const get = new Get(vars);
        const set = new Set(vars);

        await set.handler({
            call: { id: "s1", name: "variable_set",
                    arguments: { name: "plan", value: { steps: ["a","b","c"], done: false } } },
            scope: null,
        });
        await set.handler({
            call: { id: "s2", name: "variable_set",
                    arguments: { name: "text", value: "L1\nL2\nL3\nL4\nL5" } },
            scope: null,
        });

        const objLens = await get.handler({
            call: { id: "g1", name: "variable_get",
                    arguments: { name: "plan", filter: ".steps" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(objLens) }));

        const textLens = await get.handler({
            call: { id: "g2", name: "variable_get",
                    arguments: { name: "text",
                                 filter: "split(\"\n\") | .[1:4] | join(\"\n\")" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(textLens) }));

        const broken = await get.handler({
            call: { id: "g3", name: "variable_get",
                    arguments: { name: "plan", filter: "this is not jq" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(broken) }));

        const missing = await get.handler({
            call: { id: "g4", name: "variable_get",
                    arguments: { name: "nope", filter: "." } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(missing) }));
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

    let obj_lens = text_of(&frames[0]);
    assert!(obj_lens.contains(r#""is_error":false"#), "obj: {obj_lens}");
    assert!(obj_lens.contains(r#"\"a\""#), "obj: {obj_lens}");
    assert!(obj_lens.contains(r#"\"c\""#), "obj: {obj_lens}");

    let text_lens = text_of(&frames[1]);
    assert!(
        text_lens.contains(r#""is_error":false"#),
        "text: {text_lens}"
    );
    assert!(text_lens.contains(r#"L2\\nL3\\nL4"#), "text: {text_lens}");

    let broken = text_of(&frames[2]);
    assert!(broken.contains(r#""is_error":true"#), "broken: {broken}");

    let missing = text_of(&frames[3]);
    assert!(missing.contains(r#""is_error":true"#), "missing: {missing}",);
    assert!(missing.contains("unknown variable"), "missing: {missing}",);
}

#[tokio::test]
async fn variable_get_and_set_tool_handlers_work() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        r#"
        import { Variables, Get, Set } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const vars = new Variables();
        const get = new Get(vars);
        const set = new Set(vars);

        const setResult = await set.handler({
            call: { id: "c1", name: "variable_set", arguments: { name: "x", value: { n: 42 } } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(setResult) }));

        const getResult = await get.handler({
            call: { id: "c2", name: "variable_get", arguments: { name: "x" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(getResult) }));

        const missing = await get.handler({
            call: { id: "c3", name: "variable_get", arguments: { name: "nope" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(missing) }));
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
