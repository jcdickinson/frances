//! Integration tests for the `frances:v1/tools/file` `Editor`
//! primitive. Each test drives a JS script through the workflow
//! runtime against a `StubDeps` with a real `EditSession<FakeStore>`
//! and a tempdir cwd, then asserts on what the script pushed back via
//! `MarkdownFrame` / `ErrorFrame`.

use std::io::Write;
use std::path::PathBuf;
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
            FrameKind::Markdown { content } | FrameKind::Error { content } => content.clone(),
            FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
        },
        HostFrame::Append { delta, .. } => delta.clone(),
    }
}

fn deps_with_cwd(cwd: PathBuf) -> StubDeps {
    let deps = StubDeps::default();
    deps.set_cwd(cwd);
    deps
}

#[tokio::test]
async fn editor_read_returns_anchored_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const content = await editor.readFile("hello.txt");
        transcript.push(new MarkdownFrame({ content }));
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

    let rendered = text_of(&frames[0]);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 3, "got: {rendered:?}");
    for line in &lines {
        assert!(line.contains('§'), "line missing anchor: {line:?}");
    }
}

#[tokio::test]
async fn editor_edit_replace_writes_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const read = await editor.readFile("file.txt");
        // Pick the second line's full anchor field (Word§b).
        const line_b = read.split("\n")[1];
        const diff = await editor.edit({
            kind: "Replace",
            path: "file.txt",
            anchor: line_b,
            end_anchor: line_b,
            text: "B2",
        });
        transcript.push(new MarkdownFrame({ content: diff }));
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

    let diff = text_of(&frames[0]);
    assert!(diff.contains("§B2"), "diff missing new anchor: {diff}");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "a\nB2\nc\n");
}

#[tokio::test]
async fn editor_anchor_not_found_throws() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "a\nb\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        await editor.readFile("file.txt");
        let caught = "";
        try {
            await editor.edit({
                kind: "InsertAfter",
                path: "file.txt",
                anchor: "Wizard§a",
                text: "X",
            });
            caught = "no-throw";
        } catch (e) {
            caught = String((e && e.message) || e);
        }
        transcript.push(new MarkdownFrame({ content: caught }));
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

    let msg = text_of(&frames[0]);
    assert!(
        msg.contains("not found") || msg.contains("Wizard"),
        "expected anchor-not-found error, got: {msg}",
    );
}

#[tokio::test]
async fn editor_new_creates_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a").join("b").join("c.txt");
    assert!(!nested.parent().unwrap().exists());

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    // Pass the absolute nested path so the test is robust to whether
    // `current_cwd` is honoured or not.
    let nested_str = nested.to_str().unwrap();
    let script = format!(
        r#"
        import {{ Editor }} from "frances:v1/tools/file";
        import {{ transcript, MarkdownFrame }} from "frances:v1/frames";
        const editor = new Editor();
        const out = await editor.edit({{
            kind: "New",
            path: {path:?},
            text: "hello\nworld",
        }});
        transcript.push(new MarkdownFrame({{ content: out }}));
        "#,
        path = nested_str,
    );
    let file = write_source(&script);
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
        })
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");

    assert!(nested.exists(), "expected file to be created");
    let body = std::fs::read_to_string(&nested).unwrap();
    assert_eq!(body, "hello\nworld\n");

    let diff = text_of(&frames[0]);
    assert!(diff.contains("§hello"), "missing §hello in: {diff}");
    assert!(diff.contains("§world"), "missing §world in: {diff}");
}
