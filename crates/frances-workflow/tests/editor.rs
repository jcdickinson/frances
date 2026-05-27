//! Integration tests for the `frances:v1/tools/file` `Editor`
//! primitive. Each test drives a JS script through the workflow
//! runtime against a `StubDeps` with a real `EditSession<FakeStore>`
//! and a tempdir cwd, then asserts on what the script pushed back via
//! `MarkdownFrame` / `ErrorFrame`.

use std::io::Write;
use std::path::PathBuf;

use frances_workflow::{
    FrameKind, Invocation, Runtime, TranscriptDelta, test_deps::StubDeps,
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

fn text_of(frame: &TranscriptDelta) -> String {
    match frame {
        TranscriptDelta::Set { frame: spec, .. } => match &spec.kind {
            FrameKind::Markdown { .. } | FrameKind::Error => spec.seed.clone().unwrap_or_default(),
            FrameKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
            FrameKind::ShellOutput { state, cmd } => format!(
                "[shell:{state:?}] $ {cmd}
{}",
                spec.seed.clone().unwrap_or_default()
            ),
            FrameKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
        },
        TranscriptDelta::Append { delta, .. } => delta.clone(),
        TranscriptDelta::Close { id } => format!("[close:{}]", id.0),
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
            ..Default::default()
        })
        .await
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
        const result = await editor.edit({
            kind: "ReplaceLines",
            path: "file.txt",
            anchor: line_b,
            end_anchor: line_b,
            text: "B2",
        });
        transcript.push(new MarkdownFrame({ content: result.text }));
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

    let diff = text_of(&frames[0]);
    assert!(diff.contains("§B2"), "diff missing new anchor: {diff}");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "a\nB2\nc\n");
}

#[tokio::test]
async fn editor_edit_replace_all_writes_disk_and_honors_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "old_1\nold_2\nkeep\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        await editor.readFile("file.txt");
        const result = await editor.edit({
            kind: "ReplaceAll",
            path: "file.txt",
            find: "old_(\\d)",
            replacement: "new_$1",
            count: 2,
        });
        transcript.push(new MarkdownFrame({ content: result.text }));
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

    let diff = text_of(&frames[0]);
    assert!(diff.contains("§new_1"), "diff missing new_1: {diff}");
    assert!(diff.contains("§new_2"), "diff missing new_2: {diff}");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "new_1\nnew_2\nkeep\n");
}

#[tokio::test]
async fn replace_all_tool_class_reports_count_cap_error_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "x\nx\nx\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor, ReplaceAll } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const replace = new ReplaceAll(editor, new Variables());
        await editor.readFile("file.txt");
        const result = await replace.handler({
            call: { id: "c1", name: "file_replace_all",
                    arguments: { path: "file.txt", find: "x", replacement: "y", count: 2 } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(result) }));
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

    let result = text_of(&frames[0]);
    assert!(result.contains(r#""is_error":true"#), "result: {result}");
    assert!(result.contains("matched 3 times"), "result: {result}");
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "x\nx\nx\n");
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
            ..Default::default()
        })
        .await
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
async fn read_into_var_stores_raw_and_skips_registration() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "alpha\nbeta\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor, Read, ReplaceLines } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";

        const editor = new Editor();
        const vars = new Variables();
        const read = new Read(editor, vars);
        const replace = new ReplaceLines(editor, vars);

        const r = await read.handler({
            call: { id: "c1", name: "file_read", arguments: { path: "note.txt", into: "blob" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));
        transcript.push(new MarkdownFrame({ content: vars.get("blob") }));

        // The into-read did NOT register the file. file_replace_lines should fail.
        const edit = await replace.handler({
            call: { id: "c2", name: "file_replace_lines",
                    arguments: { path: "note.txt",
                                 anchor: "Apple§alpha",
                                 end_anchor: "Apple§alpha",
                                 text: "X" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(edit) }));
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
    assert!(r.contains(r#""content":"blob = string""#), "r: {r}");
    assert!(r.contains(r#""is_error":false"#), "r: {r}");
    assert_eq!(text_of(&frames[1]), "alpha\nbeta\n");

    let edit = text_of(&frames[2]);
    assert!(edit.contains(r#""is_error":true"#), "edit: {edit}");
}

#[tokio::test]
async fn write_from_var_pulls_text() {
    let dir = tempfile::tempdir().unwrap();
    let str_path = dir.path().join("string.txt");
    let json_path = dir.path().join("object.json");

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let str_arg = str_path.to_str().unwrap();
    let json_arg = json_path.to_str().unwrap();
    let script = format!(
        r#"
        import {{ Editor, New }} from "frances:v1/tools/file";
        import {{ Variables }} from "frances:v1/tools/variable";
        const editor = new Editor();
        const vars = new Variables();
        const create = new New(editor, vars);

        vars.set("plain", "hello\nworld\n");
        await create.handler({{
            call: {{ id: "c1", name: "file_new", arguments: {{ path: {str:?}, from: "plain" }} }},
            scope: null,
        }});

        vars.set("obj", {{ a: 1, b: [2, 3] }});
        await create.handler({{
            call: {{ id: "c2", name: "file_new", arguments: {{ path: {json:?}, from: "obj" }} }},
            scope: null,
        }});
        "#,
        str = str_arg,
        json = json_arg,
    );
    let file = write_source(&script);
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

    let plain = std::fs::read_to_string(&str_path).unwrap();
    assert_eq!(plain.trim_end_matches('\n'), "hello\nworld");

    let obj = std::fs::read_to_string(&json_path).unwrap();
    assert_eq!(obj.trim_end(), r#"{"a":1,"b":[2,3]}"#);
}

#[tokio::test]
async fn write_text_and_from_both_set_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.txt");

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let path_arg = path.to_str().unwrap();
    let script = format!(
        r#"
        import {{ Editor, New }} from "frances:v1/tools/file";
        import {{ Variables }} from "frances:v1/tools/variable";
        import {{ transcript, MarkdownFrame }} from "frances:v1/frames";
        const editor = new Editor();
        const vars = new Variables();
        const create = new New(editor, vars);

        vars.set("x", "from-value");
        const both = await create.handler({{
            call: {{ id: "c1", name: "file_new",
                     arguments: {{ path: {path:?}, text: "lit", from: "x" }} }},
            scope: null,
        }});
        const neither = await create.handler({{
            call: {{ id: "c2", name: "file_new",
                     arguments: {{ path: {path:?} }} }},
            scope: null,
        }});
        const missing = await create.handler({{
            call: {{ id: "c3", name: "file_new",
                     arguments: {{ path: {path:?}, from: "nope" }} }},
            scope: null,
        }});
        transcript.push(new MarkdownFrame({{ content: JSON.stringify(both) }}));
        transcript.push(new MarkdownFrame({{ content: JSON.stringify(neither) }}));
        transcript.push(new MarkdownFrame({{ content: JSON.stringify(missing) }}));
        "#,
        path = path_arg,
    );
    let file = write_source(&script);
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

    let both = text_of(&frames[0]);
    assert!(both.contains(r#""is_error":true"#), "both: {both}");
    assert!(both.contains("exactly one of"), "both: {both}");

    let neither = text_of(&frames[1]);
    assert!(neither.contains(r#""is_error":true"#), "neither: {neither}");

    let missing = text_of(&frames[2]);
    assert!(missing.contains(r#""is_error":true"#), "missing: {missing}");
    assert!(missing.contains("unknown variable"), "missing: {missing}");
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
        transcript.push(new MarkdownFrame({{ content: out.text }}));
        "#,
        path = nested_str,
    );
    let file = write_source(&script);
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

    assert!(nested.exists(), "expected file to be created");
    let body = std::fs::read_to_string(&nested).unwrap();
    assert_eq!(body, "hello\nworld\n");

    let diff = text_of(&frames[0]);
    assert!(diff.contains("§hello"), "missing §hello in: {diff}");
    assert!(diff.contains("§world"), "missing §world in: {diff}");
}

#[tokio::test]
async fn editor_read_ranges_returns_disjoint_anchored_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ranges.txt"),
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
    )
    .unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor, Read } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        
        const editor = new Editor();
        const vars = new Variables();
        const read = new Read(editor, vars);
        
        const r = await read.handler({
            call: { id: "c1", name: "file_read", arguments: { path: "ranges.txt", ranges: [[1, 2], [8, 12]] } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: r.content || JSON.stringify(r) }));
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

    let rendered = text_of(&frames[0]);
    let lines: Vec<&str> = rendered.lines().collect();

    // 1, 2, separator, 8, 9, 10 -> 6 lines total
    assert_eq!(lines.len(), 6, "got: {rendered:?}");
    assert!(lines[0].ends_with("§1"), "line 0: {}", lines[0]);
    assert!(lines[1].ends_with("§2"), "line 1: {}", lines[1]);
    assert!(lines[2] == "…§", "line 2: {}", lines[2]);
    assert!(lines[3].ends_with("§8"), "line 3: {}", lines[3]);
    assert!(lines[4].ends_with("§9"), "line 4: {}", lines[4]);
    assert!(lines[5].ends_with("§10"), "line 5: {}", lines[5]);
}

#[tokio::test]
async fn read_into_and_ranges_mutually_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mut.txt"), "1\n2\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor, Read } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        
        const editor = new Editor();
        const vars = new Variables();
        const read = new Read(editor, vars);
        
        const r = await read.handler({
            call: { id: "c1", name: "file_read", arguments: { path: "mut.txt", into: "blob", ranges: [[1, 2]] } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));
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

    let rendered = text_of(&frames[0]);
    assert!(
        rendered.contains(r#""is_error":true"#),
        "rendered: {rendered}"
    );
    assert!(
        rendered.contains("provide exactly one of `into` or `ranges`, not both"),
        "rendered: {rendered}"
    );
}

#[tokio::test]
async fn editor_read_ranges_reversed_throws() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ranges.txt"), "1\n2\n3\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor, Read } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        
        const editor = new Editor();
        const vars = new Variables();
        const read = new Read(editor, vars);
        
        const r = await read.handler({
            call: { id: "c1", name: "file_read", arguments: { path: "ranges.txt", ranges: [[2, 1]] } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));
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

    let rendered = text_of(&frames[0]);
    assert!(
        rendered.contains(r#""is_error":true"#),
        "rendered: {rendered}"
    );
    assert!(
        rendered.contains("must be <= end") || rendered.contains("reverse range"),
        "rendered: {rendered}"
    );
}

#[tokio::test]
async fn loop_guard_blocks_identical_read_on_unchanged_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("loop.txt"), "a\nb\nc\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        await editor.readFile("loop.txt");
        let caught = "no-throw";
        try {
            await editor.readFile("loop.txt");
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
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let msg = text_of(&frames[0]);
    assert!(
        msg.contains("loop guard"),
        "expected loop guard error, got: {msg}"
    );
}

#[tokio::test]
async fn loop_guard_clears_after_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("loop.txt"), "a\nb\nc\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const first = await editor.readFile("loop.txt");
        const line_b = first.split("\n")[1];
        await editor.edit({
            kind: "ReplaceLines",
            path: "loop.txt",
            anchor: line_b,
            end_anchor: line_b,
            text: "B2",
        });
        // Same args as the first read — but the edit cleared the ring,
        // so this should succeed rather than tripping the guard.
        const second = await editor.readFile("loop.txt");
        transcript.push(new MarkdownFrame({ content: second }));
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
    let rendered = text_of(&frames[0]);
    assert!(
        rendered.contains("§B2"),
        "expected post-edit content, got: {rendered}"
    );
}

#[tokio::test]
async fn loop_guard_lets_through_after_size_change() {
    // The LoopKey::Read includes both mtime and size; changing content
    // size (which our `fs::write` does) is enough to miss the ring,
    // independent of filesystem mtime resolution.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const first = await editor.readFile("loop.txt");
        transcript.push(new MarkdownFrame({ content: first }));
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

    // Replace with a different-sized payload — size delta forces a ring miss.
    std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").unwrap();

    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const second = await editor.readFile("loop.txt");
        transcript.push(new MarkdownFrame({ content: second }));
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
    let rendered = text_of(&frames[0]);
    assert!(
        rendered.contains("§alpha"),
        "expected post-write content, got: {rendered}"
    );
}

#[tokio::test]
async fn loop_guard_distinguishes_ranges() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ranges.txt"),
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
    )
    .unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        r#"
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const editor = new Editor();
        const a = await editor.readFile({ path: "ranges.txt", ranges: [[1, 2]] });
        // Different ranges → different args hash → no collision.
        const b = await editor.readFile({ path: "ranges.txt", ranges: [[5, 6]] });
        transcript.push(new MarkdownFrame({ content: a + "|||" + b }));
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
    let rendered = text_of(&frames[0]);
    let parts: Vec<&str> = rendered.split("|||").collect();
    assert_eq!(parts.len(), 2, "rendered: {rendered}");
    assert!(parts[0].contains("§1"), "first range: {}", parts[0]);
    assert!(parts[1].contains("§5"), "second range: {}", parts[1]);
}
