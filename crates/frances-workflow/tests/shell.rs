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
            SectionKind::Markdown { .. } | SectionKind::Error => {
                spec.seed.clone().unwrap_or_default()
            }
            SectionKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
            SectionKind::ShellOutput { state, cmd } => format!(
                "[shell:{state:?}] $ {cmd}
{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Reasoning { state } => format!(
                "[reasoning:{state:?}]\n{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
        },
        SectionTranscript::Append { delta, .. } => delta.clone(),
        SectionTranscript::Close { id } => format!("[close:{}]", id.0),
    }
}

/// Collect content from every `MarkdownSection` push in order. Most
/// shell tests use `transcript.push(new MarkdownSection({ content }))`
/// to surface tool_result text and then index into the resulting
/// frames — but `Run.handler` now also pushes a `ShellOutputSection`
/// (plus its Append/UpdateKind/Close trail), which would otherwise
/// shift those indices around. Filtering by kind keeps the tests
/// focused on the markdown frames they care about. Empty-content
/// pushes (the `None` case) collapse to "" since the tests work in
/// terms of body text.
fn markdown_pushes(frames: &[SectionTranscript]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|f| match f {
            SectionTranscript::Set { section: spec, .. } => match &spec.kind {
                SectionKind::Markdown { .. } => Some(spec.seed.clone().unwrap_or_default()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn shell_set_set_form_is_not_exported() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill, approve: false });

        vars.set("v", "abc");
        await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { set: "FOO", from: "v" } },
            scope: null,
        });

        // Inside the shell, $FOO is set (visible to bash itself).
        const seen = await run.handler({
            call: { id: "r1", name: "shell_run", arguments: { cmd: "echo \"[$FOO]\"" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: seen.content }));

        // But subprocesses don't inherit a non-exported var.
        const sub = await run.handler({
            call: { id: "r2", name: "shell_run",
                    arguments: { cmd: "bash -c 'echo \"[${FOO:-unset}]\"'" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: sub.content }));

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

    let pushes = markdown_pushes(&frames);
    assert!(
        pushes[0].contains("[abc]"),
        "expected [abc] in {:?}",
        pushes[0]
    );
    assert!(
        pushes[1].contains("[unset]"),
        "expected [unset] in {:?}",
        pushes[1]
    );
}

#[tokio::test]
async fn shell_set_export_form_visible_to_subprocesses() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownSection } from "frances:v1/sections";

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
                    arguments: { export: "PAYLOAD", from: "payload" } },
            scope: null,
        });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "bash -c 'echo \"$PAYLOAD\"'" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: out.content }));

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

    let pushes = markdown_pushes(&frames);
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
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill, approve: false });

        vars.set("obj", { a: 1, b: [2, 3] });
        await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { set: "OBJ", from: "obj" } },
            scope: null,
        });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run", arguments: { cmd: "echo \"$OBJ\"" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: out.content }));

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

    let pushes = markdown_pushes(&frames);
    let out = &pushes[0];
    assert!(out.contains(r#"{"a":1,"b":[2,3]}"#), "got: {out}");
}

#[tokio::test]
async fn shell_set_validates_xor_and_missing_from() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const vars = new Variables();
        vars.set("v", "value");
        const setTool = new ShellSet(sh, vars);

        const both = await setTool.handler({
            call: { id: "s1", name: "shell_set",
                    arguments: { set: "A", export: "A", from: "v" } },
            scope: null,
        });
        const neither = await setTool.handler({
            call: { id: "s2", name: "shell_set",
                    arguments: { from: "v" } },
            scope: null,
        });
        const missing = await setTool.handler({
            call: { id: "s3", name: "shell_set",
                    arguments: { set: "A", from: "nope" } },
            scope: null,
        });
        const bad = await setTool.handler({
            call: { id: "s4", name: "shell_set",
                    arguments: { set: "1bad", from: "v" } },
            scope: null,
        });

        transcript.push(new MarkdownSection({ content: JSON.stringify(both) }));
        transcript.push(new MarkdownSection({ content: JSON.stringify(neither) }));
        transcript.push(new MarkdownSection({ content: JSON.stringify(missing) }));
        transcript.push(new MarkdownSection({ content: JSON.stringify(bad) }));

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
        (0usize, "exactly one"),
        (1, "exactly one"),
        (2, "unknown variable"),
        (3, "invalid bash name"),
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
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const run = new Run(sh, { wait, kill, approve: false });
        const capture = new ShellCapture(sh, vars);
        const assign = new VarAssign(vars);

        await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "OUT='{\"a\":1,\"b\":[2,3]}'" } },
            scope: null,
        });
        await capture.handler({
            call: { id: "c1", name: "shell_capture",
                    arguments: { name: "snapshot", from: "OUT" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: vars.get("snapshot") }));

        // Parse via variable_assign + fromjson. The captured value is
        // always a string, so we route it through $snapshot rather
        // than `.` (which would be the destination's prior value, null).
        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "parsed", filter: "$snapshot | fromjson",
                                 inputs: ["snapshot"] } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: JSON.stringify(vars.get("parsed")) }));

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

    let pushes = markdown_pushes(&frames);
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
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const vars = new Variables();
        const capture = new ShellCapture(sh, vars);

        const r = await capture.handler({
            call: { id: "c1", name: "shell_capture",
                    arguments: { name: "x", from: "DEFINITELY_UNSET_VAR_NAME" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: JSON.stringify(r) }));

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
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const run = new Run(sh, { wait, kill });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "echo approved-and-ran" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: out.content }));
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

    let pushes = markdown_pushes(&frames);
    let out = pushes
        .first()
        .expect("expected a markdown push after approval");
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
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const run = new Run(sh, { wait, kill });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "rm -rf /" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: JSON.stringify(out) }));
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
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const run = new Run(sh, { wait, kill, approve: false });

        const out = await run.handler({
            call: { id: "r1", name: "shell_run",
                    arguments: { cmd: "echo no-gate" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: out.content }));
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
    let pushes = markdown_pushes(&frames);
    let out = &pushes[0];
    assert!(out.starts_with("Exit 0"), "got `{out}`");
    assert!(out.contains("no-gate"), "got `{out}`");
}

#[tokio::test]
async fn shell_kill_after_quiet_does_not_return_still_running() {
    // Regression: when shell_kill was called on a Quiet command, the
    // handler used to do one keepWaiting pass and, if that also went
    // Quiet, return a "Still running … call shell_kill to stop" tool
    // result. The model would then call shell_kill again, spin
    // forever, and the frame would fill with `(killed)` lines. The
    // fix loops kill+drain a few times so the post-source sentinel
    // always lands, and as a last-resort closes the shell rather than
    // returning a quiet outcome.
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Kill } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const sh = new Shell();
        const kill = new Kill(sh);

        // Long sleep — runOnce returns Quiet after the default 1s
        // silence window.
        const r1 = await sh.runOnce("sleep 30");

        // Now drive the Kill tool the same way the workflow runtime
        // would.
        const result = await kill.handler({
            call: { id: "k1", name: "shell_kill", arguments: {} },
            scope: null,
        });

        // The shell should be idle after Kill finishes — the post-
        // source sentinel fires once the SIGKILL'd sleep is reaped.
        const stillRunning = await sh.isRunning();

        await sh.close();
        transcript.push(new MarkdownSection({
            content: `r1Kind=${r1.kind} isErr=${result.is_error} hasStillRunning=${result.content.includes("Still running")} running=${stillRunning} content=${JSON.stringify(result.content)}`,
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
    let pushes = markdown_pushes(&frames);
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
}
