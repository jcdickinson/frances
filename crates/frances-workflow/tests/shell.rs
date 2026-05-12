//! Integration tests for `frances:v1/tools/shell`'s variable bridges:
//! the `Set` (shell_set with `set:`/`export:`) and `Capture`
//! (shell_capture) tool classes. Drives a real bash subprocess via
//! `StubDepsRealShell` so we exercise the actual tmpfile-trick paths
//! end to end.

use std::io::Write;
use std::time::Duration;

use frances_workflow::{
    FrameKind, HostFrame, Invocation, Runtime, WorkflowError, WorkflowHandle,
    test_deps::StubDepsRealShell,
};

fn write_source(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".js")
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

const CYCLE_TIMEOUT: Duration = Duration::from_secs(10);

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
            FrameKind::Markdown { content } | FrameKind::Error { content } => content.clone(),
            FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
        },
        HostFrame::Append { delta, .. } => delta.clone(),
    }
}

#[tokio::test]
async fn shell_set_set_form_is_not_exported() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill });

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
        transcript.push(new MarkdownFrame({ content: seen.content }));

        // But subprocesses don't inherit a non-exported var.
        const sub = await run.handler({
            call: { id: "r2", name: "shell_run",
                    arguments: { cmd: "bash -c 'echo \"[${FOO:-unset}]\"'" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: sub.content }));

        await sh.close();
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

    let seen = text_of(&frames[0]);
    assert!(seen.contains("[abc]"), "expected [abc] in {seen}");

    let sub = text_of(&frames[1]);
    assert!(sub.contains("[unset]"), "expected [unset] in {sub}");
}

#[tokio::test]
async fn shell_set_export_form_visible_to_subprocesses() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill });

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
        transcript.push(new MarkdownFrame({ content: out.content }));

        await sh.close();
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

    let out = text_of(&frames[0]);
    assert!(out.contains("alpha\nbeta\ngamma"), "got: {out}");
}

#[tokio::test]
async fn shell_set_object_value_is_json_encoded() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Run, Wait, Kill, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const setTool = new ShellSet(sh, vars);
        const run = new Run(sh, { wait, kill });

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
        transcript.push(new MarkdownFrame({ content: out.content }));

        await sh.close();
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

    let out = text_of(&frames[0]);
    assert!(out.contains(r#"{"a":1,"b":[2,3]}"#), "got: {out}");
}

#[tokio::test]
async fn shell_set_validates_xor_and_missing_from() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Set as ShellSet } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

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

        transcript.push(new MarkdownFrame({ content: JSON.stringify(both) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(neither) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(missing) }));
        transcript.push(new MarkdownFrame({ content: JSON.stringify(bad) }));

        await sh.close();
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
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const sh = new Shell();
        const wait = new Wait(sh);
        const kill = new Kill(sh);
        const vars = new Variables();
        const run = new Run(sh, { wait, kill });
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
        transcript.push(new MarkdownFrame({ content: vars.get("snapshot") }));

        // Parse via variable_assign + fromjson. The captured value is
        // always a string, so we route it through $snapshot rather
        // than `.` (which would be the destination's prior value, null).
        await assign.handler({
            call: { id: "a1", name: "variable_assign",
                    arguments: { name: "parsed", filter: "$snapshot | fromjson",
                                 inputs: ["snapshot"] } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(vars.get("parsed")) }));

        await sh.close();
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

    assert_eq!(text_of(&frames[0]), r#"{"a":1,"b":[2,3]}"#);
    assert_eq!(text_of(&frames[1]), r#"{"a":1,"b":[2,3]}"#);
}

#[tokio::test]
async fn shell_capture_unset_var_errors() {
    let rt = Runtime::new(StubDepsRealShell::default()).unwrap();
    let file = write_source(
        r#"
        import { Shell, Capture as ShellCapture } from "frances:v1/tools/shell";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const sh = new Shell();
        const vars = new Variables();
        const capture = new ShellCapture(sh, vars);

        const r = await capture.handler({
            call: { id: "c1", name: "shell_capture",
                    arguments: { name: "x", from: "DEFINITELY_UNSET_VAR_NAME" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));

        await sh.close();
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

    let r = text_of(&frames[0]);
    assert!(r.contains(r#""is_error":true"#), "r: {r}");
    assert!(
        r.contains("unset") || r.contains("expansion failed"),
        "expected unset-or-expansion-failed marker; got: {r}",
    );
}
