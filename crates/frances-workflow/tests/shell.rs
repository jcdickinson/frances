//! Integration tests for `frances:v1/tools/shell`'s variable bridges:
//! the `Set` (shell_set with `set:`/`export:`) and `Capture`
//! (shell_capture) tool classes. Drives a real bash subprocess via
//! `StubDepsRealShell` so we exercise the actual tmpfile-trick paths
//! end to end.

use std::io::Write;

use frances_workflow::{
    Invocation, PermissionRequest, PermissionResponse, Runtime, SectionKind, SectionTranscript,
    WorkflowHandle,
    test_deps::StubDepsRealShell,
    test_drive::{CYCLE_TIMEOUT, drive_one_cycle},
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

/// Collect content from every `ErrorSection` push in order, filtering out
/// other frames so indices remain stable regardless of other section
/// types. Empty-content pushes collapse to "".
fn error_pushes(frames: &[SectionTranscript]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|f| match f {
            SectionTranscript::Set { section: spec, .. } => match &spec.kind {
                SectionKind::Error => Some(spec.seed.clone().unwrap_or_default()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn shell_set_persists_across_runs_and_subprocesses() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill, approve: false });

        vars.set("v", "abc");
        await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { name: "FOO", from: "v" } },
            scope: null,
        });

        // A later run (a fresh bash) sees the persisted export.
        const seen = await run.handler({
            call: { id: "r1", name: "shell_run", arguments: { cmd: "echo \"[$FOO]\"" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: seen.content }));

        // Subprocesses inherit it too.
        const sub = await run.handler({
            call: { id: "r2", name: "shell_run",
                    arguments: { cmd: "bash -c 'echo \"[${FOO:-unset}]\"'" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: sub.content }));

        await sh.close();
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

    let pushes = error_pushes(&frames);
    assert!(
        pushes[0].contains("[abc]"),
        "expected [abc] in {:?}",
        pushes[0]
    );
    assert!(
        pushes[1].contains("[abc]"),
        "expected [abc] in {:?}",
        pushes[1]
    );
}

#[tokio::test]
async fn shell_set_multiline_value_survives() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill, approve: false });

        // Multi-line string value to confirm the tmp-file trick preserves
        // newlines.
        vars.set("payload", "alpha\nbeta\ngamma");
        await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { name: "PAYLOAD", from: "payload" } },
            scope: null,
        });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "bash -c 'echo \"$PAYLOAD\"'" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: out.content }));

        await sh.close();
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

    let pushes = error_pushes(&frames);
    let out = &pushes[0];
    assert!(out.contains("alpha\nbeta\ngamma"), "got: {out}");
}

#[tokio::test]
async fn shell_set_object_value_is_json_encoded() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill, approve: false });

        vars.set("obj", { a: 1, b: [2, 3] });
        await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { name: "OBJ", from: "obj" } },
            scope: null,
        });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run", arguments: { cmd: "echo \"$OBJ\"" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: out.content }));

        await sh.close();
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

    let pushes = error_pushes(&frames);
    let out = &pushes[0];
    assert!(out.contains(r#"{"a":1,"b":[2,3]}"#), "got: {out}");
}

#[tokio::test]
async fn shell_set_validates_names_and_from() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const vars = new Variables();
        vars.set("v", "value");
        const setTool = new ShellSet(sh, vars);

        const noName = await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { from: "v" } },
            scope: null,
        });
        const missing = await setTool.handler({
            call: { id: "s2", name: "shell_set",
                    arguments: { name: "A", from: "nope" } },
            scope: null,
        });
        const bad = await setTool.handler({
            call: { id: "s3", name: "shell_set",
                    arguments: { name: "1bad", from: "v" } },
            scope: null,
        });
        const reserved = await setTool.handler({
            call: { id: "s4", name: "shell_set",
                    arguments: { name: "FRANCES_ROOT", from: "v" } },
            scope: null,
        });

        transcript.push(new ErrorSection({ content: JSON.stringify(noName) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(missing) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(bad) }));
        transcript.push(new ErrorSection({ content: JSON.stringify(reserved) }));

        await sh.close();
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

    for (idx, expected_fragment) in [
        (0usize, "missing `name`"),
        (1, "unknown variable"),
        (2, "invalid bash name"),
        (3, "reserved"),
    ] {
        let f = text_of(&frames[idx]);
        assert!(f.contains(r#""is_error":true"#), "frame {idx}: {f}");
        assert!(f.contains(expected_fragment), "frame {idx}: {f}");
    }
}

#[tokio::test]
async fn shell_capture_round_trip_via_variable_assign() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Capture as ShellCapture } from "frances:v1/tools/shell";
        import { Variables, Assign as VarAssign } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const run = new Run(sh, { wait, kill, approve: false });
        const capture = new ShellCapture(sh, vars);
        const assign = new VarAssign(vars);

        // Each run is a fresh bash: OUT must be exported and persisted
        // for the capture (a separate run) to see it.
        await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "export OUT='{\"a\":1,\"b\":[2,3]}'",
                                 persist: ["OUT"] } },
            scope: null,
        });
        await capture.handler({
            call: { id: "c1", name: "shell_capture",
                    arguments: { name: "snapshot", from: "OUT" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: vars.get("snapshot") }));

        // Parse via variable_assign + fromjson. The captured value is
        // always a string, so we route it through $snapshot rather
        // than `.` (which would be the destination's prior value, null).
        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "parsed", filter: "$snapshot | fromjson",
                                 inputs: ["snapshot"] } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(vars.get("parsed")) }));

        await sh.close();
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

    let pushes = error_pushes(&frames);
    assert_eq!(pushes[0], r#"{"a":1,"b":[2,3]}"#);
    assert_eq!(pushes[1], r#"{"a":1,"b":[2,3]}"#);
}

#[tokio::test]
async fn shell_capture_unset_var_errors() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Capture as ShellCapture } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const vars = new Variables();
        const capture = new ShellCapture(sh, vars);

        const r = await capture.handler({
            call: { id: "c1", name: "shell_capture",
                    arguments: { name: "x", from: "DEFINITELY_UNSET_VAR_NAME" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(r) }));

        await sh.close();
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

    let r = text_of(&frames[0]);
    assert!(r.contains(r#""is_error":true"#), "r: {r}");
    assert!(
        r.contains("unset") || r.contains("expansion failed"),
        "expected unset-or-expansion-failed marker; got: {r}",
    );
}

/// Wait until an approval request lands on the permissions channel.
/// Panics on timeout.
async fn await_approval(handle: &mut WorkflowHandle) -> PermissionRequest {
    tokio::time::timeout(CYCLE_TIMEOUT, async {
        match handle.outputs.permissions.recv().await {
            Some(req) => req,
            None => panic!("permissions channel closed before a request landed"),
        }
    })
    .await
    .expect("permission request did not arrive within timeout")
}

#[tokio::test]
async fn shell_run_approve_yes_executes_command() {
    let deps = StubDepsRealShell::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const run = new Run(sh, { wait, kill });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "echo approved-and-ran" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: out.content }));
        await sh.close();
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

    let req = await_approval(&mut handle).await;
    assert!(
        req.prompt.contains("echo approved-and-ran"),
        "prompt should contain the command: {}",
        req.prompt,
    );
    assert!(
        req.prompt.contains("```bash"),
        "prompt should be rendered as a bash code block: {}",
        req.prompt,
    );
    let call = req
        .tool_call
        .as_ref()
        .expect("permission request carries the originating tool call");
    assert_eq!(call.id, "r1");
    assert_eq!(call.name, "shell_run");

    assert!(
        req.reply
            .send(PermissionResponse::Yes { details: None })
            .is_ok(),
        "answer should land on the embedded reply slot",
    );

    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

    let pushes = error_pushes(&frames);
    let out = pushes
        .first()
        .expect("expected a push after approval");
    assert!(out.starts_with("Exit 0"), "got `{out}`");
    assert!(out.contains("approved-and-ran"), "got `{out}`");
}

#[tokio::test]
async fn shell_run_approve_no_skips_command_and_returns_error() {
    let deps = StubDepsRealShell::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const run = new Run(sh, { wait, kill });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "rm -rf /" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: JSON.stringify(out) }));
        await sh.close();
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

    let req = await_approval(&mut handle).await;
    assert!(req.prompt.contains("rm -rf /"));

    assert!(
        req.reply
            .send(PermissionResponse::No {
                details: Some("too scary".into()),
            })
            .is_ok()
    );

    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

    let out = frames
        .iter()
        .find_map(|f| match f {
            set @ SectionTranscript::Set { .. } => Some(text_of(set)),
            _ => None,
        })
        .expect("expected a tool result transcript push");
    assert!(out.contains(r#""is_error":true"#), "got `{out}`");
    assert!(out.contains("denied"), "got `{out}`");
    assert!(out.contains("too scary"), "got `{out}`");
}

#[tokio::test]
async fn shell_run_approve_false_skips_gate() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const run = new Run(sh, { wait, kill, approve: false });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "echo no-gate" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: out.content }));
        await sh.close();
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
    assert!(
        handle.outputs.permissions.try_recv().is_err(),
        "approve:false should not emit a permission ask",
    );
    let pushes = error_pushes(&frames);
    let out = &pushes[0];
    assert!(out.starts_with("Exit 0"), "got `{out}`");
    assert!(out.contains("no-gate"), "got `{out}`");
}

#[tokio::test]
async fn shell_kill_after_quiet_does_not_return_still_running() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Kill } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const kill = new Kill(sh);

        // Long sleep — a short quiet makes runOnce return Quiet while
        // the sleep is still silent.
        const r1 = await sh.runOnce("sleep 30", { quiet: 0.3 });

        // Now drive the Kill tool the same way the workflow runtime
        // would.
        const result = await kill.handler({
            call: { id: "k1", name: "shell_kill", arguments: {} },
            scope: null,
        });

        // The shell should be idle after Kill finishes — the post-
        // source sentinel fires once the SIGKILL'd sleep is reaped.
        const stillRunning = await sh.isRunning();

        // Regression: SIGKILL takes bash down with the command (Dead),
        // which must not brick the Shell — the next run spawns a fresh
        // bash and works.
        const revived = await sh.runOnce("echo revived");

        await sh.close();
        transcript.push(new ErrorSection({
            content: `r1Kind=${r1.kind} isErr=${result.is_error} hasStillRunning=${result.content.includes("Still running")} running=${stillRunning} revivedKind=${revived.kind} revivedExit=${revived.exit_code} content=${JSON.stringify(result.content)}`,
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
    let pushes = error_pushes(&frames);
    let summary = pushes.last().expect("summary frame");
    assert!(summary.contains("r1Kind=quiet"), "got `{summary}`");
    assert!(
        summary.contains("hasStillRunning=false"),
        "Kill must not return a `Still running` tool result: `{summary}`",
    );
    assert!(
        summary.contains("running=false"),
        "shell must be idle after Kill drained: `{summary}`",
    );
    assert!(
        summary.contains("revivedKind=done") && summary.contains("revivedExit=0"),
        "a killed run must not brick the shell: `{summary}`",
    );
}

// ---- shell entity production ----------------------------------------------

fn entity_cmds(frames: &[SectionTranscript]) -> Vec<&frances_workflow::EntityCmd> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            SectionTranscript::Entity(cmd) => Some(cmd),
            _ => None,
        })
        .collect()
}

/// The pure cap policy: head passes through live, the tail ring evicts
/// from the front counting dropped bytes, flush emits the elision
/// marker followed by the ring.
#[tokio::test]
async fn shell_cap_policy_head_ring_flush() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { _capState, _capPush, _capFlush } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const cap = _capState({ head: 4, tail: 6 });
        const r = [];
        r.push(JSON.stringify(_capPush(cap, "ab")));   // within head
        r.push(JSON.stringify(_capPush(cap, "cd")));   // fills head
        r.push(JSON.stringify(_capPush(cap, "ef")));   // ring
        r.push(JSON.stringify(_capPush(cap, "ghij"))); // ring (6 bytes)
        r.push(JSON.stringify(_capPush(cap, "kl")));   // evicts "ef"
        r.push(JSON.stringify(_capFlush(cap)));
        transcript.push(new ErrorSection({ content: r.join("|") }));
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
    let pushes = error_pushes(&frames);
    let got = pushes.first().expect("cap summary");
    let want = concat!(
        r#"[{"text":"ab"}]|[{"text":"cd"}]|[]|[]|[]|"#,
        r#"[{"dropped":2},{"text":"ghij"},{"text":"kl"}]"#,
    );
    assert_eq!(got, want);
}

/// A completed shell_run produces the full entity flow: creating
/// Upsert (Live, kind `shell`), an EntityRef after it, output appends,
/// and a Settle whose snapshot is terminal and whose `llm_digest`
/// artifact is the exact tool-result content.
#[tokio::test]
async fn shell_run_produces_settled_entity_with_digest() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const run = new Run(sh, { approve: false });
        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "echo hello-entity" } },
            scope: null,
        });
        transcript.push(new ErrorSection({ content: out.content }));
        await sh.close();
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

    let cmds = entity_cmds(&frames);
    let frances_workflow::EntityCmd::Upsert {
        entity_id,
        kind,
        snapshot,
    } = cmds.first().expect("creating upsert")
    else {
        panic!("first entity cmd should be Upsert, got {:?}", cmds.first());
    };
    assert_eq!(kind, "shell");
    assert_eq!(snapshot["cmd"], "echo hello-entity");
    assert_eq!(snapshot["state"]["type"], "running");

    // The transcript ref follows the creating upsert on the same channel.
    let upsert_pos = frames
        .iter()
        .position(|f| matches!(f, SectionTranscript::Entity(_)))
        .unwrap();
    let ref_pos = frames
        .iter()
        .position(|f| matches!(
            f,
            SectionTranscript::Set { section, .. }
                if matches!(&section.kind, SectionKind::EntityRef { entity_id: id } if id == entity_id)
        ))
        .expect("EntityRef section for the shell entity");
    assert!(upsert_pos < ref_pos);

    // Output landed as stream appends.
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            frances_workflow::EntityCmd::Append { payload, .. }
                if payload["text"].as_str().is_some_and(|t| t.contains("hello-entity"))
        )),
        "expected an output append; cmds: {cmds:?}"
    );

    // Settle: terminal state + digest === the tool-result content.
    let digest = error_pushes(&frames);
    let tool_result_content = digest.first().expect("tool result push");
    let settle = cmds
        .iter()
        .find_map(|cmd| match cmd {
            frances_workflow::EntityCmd::Settle {
                snapshot,
                artifacts,
                ..
            } => Some((snapshot, artifacts)),
            _ => None,
        })
        .expect("Settle cmd");
    assert_eq!(settle.0["state"]["type"], "success");
    assert_eq!(
        settle.1.as_slice(),
        &[(
            "llm_digest".to_owned(),
            serde_json::Value::String(tool_result_content.clone())
        )]
    );
}

/// Quiet keeps the entity Live across tool calls: shell_run's quiet
/// outcome must not settle, and the follow-up shell_wait (after a
/// kill) settles the same entity.
#[tokio::test]
async fn shell_quiet_keeps_entity_live_until_wait_settles() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        // scope.lock is a no-op here: the test drives Wait manually.
        const scope = { lock: () => {} };
        const run = new Run(sh, { wait, kill, approve: false });

        const r1 = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "echo start; sleep 30", quiet: 0.3 } },
            scope,
        });
        transcript.push(new ErrorSection({ content: "MARK-QUIET", closed: true }));

        await sh.kill();
        const r2 = await wait.handler({
            call: { id: "w1", name: "shell_wait", arguments: {} },
            scope: null,
        });
        transcript.push(new ErrorSection({
            content: `r1Still=${r1.content.includes("Still running")}`,
            closed: true,
        }));
        await sh.close();
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

    let pushes = error_pushes(&frames);
    assert!(
        pushes.iter().any(|p| p.contains("r1Still=true")),
        "run should have gone quiet: {pushes:?}"
    );

    let mark_pos = frames
        .iter()
        .position(|f| {
            matches!(
                f,
                SectionTranscript::Set { section, .. }
                    if section.seed.as_deref() == Some("MARK-QUIET")
            )
        })
        .expect("marker section");
    let settle_positions: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            matches!(
                f,
                SectionTranscript::Entity(frances_workflow::EntityCmd::Settle { .. })
            )
            .then_some(i)
        })
        .collect();
    assert_eq!(
        settle_positions.len(),
        1,
        "exactly one settle expected: {frames:?}"
    );
    assert!(
        settle_positions[0] > mark_pos,
        "quiet must not settle the entity; settle came before the marker"
    );
}
