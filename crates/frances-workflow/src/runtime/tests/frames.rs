use super::*;

/// `new MarkdownSection({ source })`, `{ content: undefined }`, and
/// `{ content: null }` all produce `SectionKind::Markdown` with no seed.
#[tokio::test]
async fn markdown_frame_content_can_be_omitted() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ source: "assistant" }));
        transcript.push(new MarkdownSection({ content: undefined }));
        transcript.push(new MarkdownSection({ content: null }));
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
    assert!(matches!(done, Some(Ok(()))));
    for (i, expect_source) in [Source::Assistant, Source::Internal, Source::Internal]
        .iter()
        .enumerate()
    {
        match &frames[i] {
            SectionTranscript::Set { section: spec, .. } => match &spec.kind {
                SectionKind::Markdown { source } => {
                    assert!(spec.seed.is_none(), "frame {i} should have no seed");
                    assert_eq!(source, expect_source, "frame {i} source");
                }
                other => panic!("frame {i} unexpected kind {other:?}"),
            },
            other => panic!("frame {i} unexpected {other:?}"),
        }
    }
}

/// Pushing an empty-content frame and never writing to it produces
/// `Set` + `Close` only — no `Append` in between.
#[tokio::test]
async fn empty_markdown_frame_pushes_and_closes_without_appends() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const f = new MarkdownSection({ source: "assistant" });
        transcript.push(f);
        await f.writable.close();
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
    assert!(matches!(done, Some(Ok(()))));
    assert!(matches!(
        &frames[0],
        SectionTranscript::Set { section: spec, .. }
            if matches!(&spec.kind, SectionKind::Markdown { .. }) && spec.seed.is_none()
    ));
    assert!(matches!(&frames[1], SectionTranscript::Close { .. }));
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, SectionTranscript::Append { .. })),
        "no Append should be emitted for a never-written frame"
    );
}

#[tokio::test]
async fn write_on_active_frame_emits_delta() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const f = new MarkdownSection({ content: "hello" });
        transcript.push(f);
        const w = f.writable.getWriter();
        await w.write(" world");
        await w.close();
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
    assert!(matches!(done, Some(Ok(()))));
    assert!(
        matches!(&frames[0], SectionTranscript::Set { section: spec, .. } if matches!(&spec.kind, SectionKind::Markdown { .. }) && spec.seed.as_deref() == Some("hello"))
    );
    assert!(matches!(&frames[1], SectionTranscript::Append { delta, .. } if delta == " world"));
}

#[tokio::test]
async fn markdown_frame_carries_source() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: "hi", source: "user" }));
        transcript.push(new MarkdownSection({ content: "ok" }));
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
    assert!(matches!(done, Some(Ok(()))));
    assert!(matches!(
        &frames[0],
        SectionTranscript::Set { section: spec, .. }
            if matches!(&spec.kind, SectionKind::Markdown { source: Source::User })
                && spec.seed.as_deref() == Some("hi")
    ));
    assert!(matches!(
        &frames[1],
        SectionTranscript::Set { section: spec, .. }
            if matches!(&spec.kind, SectionKind::Markdown { source: Source::Internal })
                && spec.seed.as_deref() == Some("ok")
    ));
}

#[tokio::test]
async fn markdown_frame_rejects_non_string_source() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        new MarkdownSection({ content: "hi", source: 42 });
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
    let err = done.expect("workflow done").expect_err("expected throw");
    assert!(
        format!("{err}").contains("source"),
        "error should mention source: {err}"
    );
}

#[tokio::test]
async fn markdown_frame_rejects_unknown_source_string() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        new MarkdownSection({ content: "hi", source: "frances" });
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
    let err = done.expect("workflow done").expect_err("expected throw");
    assert!(
        format!("{err}").contains("source"),
        "error should mention source: {err}"
    );
}

#[tokio::test]
async fn write_to_earlier_frame_after_newer_push_still_works() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const a = new MarkdownSection({ content: "a" });
        transcript.push(a);
        transcript.push(new MarkdownSection({ content: "b" }));
        const w = a.writable.getWriter();
        await w.write(" extra");
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
    let (frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(matches!(result, Ok(())), "got {result:?}");
    let appends: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            SectionTranscript::Append { id, delta } => Some((*id, delta.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(appends.len(), 1, "expected one append, got {appends:?}");
    assert_eq!(appends[0].1, " extra");
}

#[tokio::test]
async fn shell_output_frame_pushes_streams_transitions_and_closes() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, ShellOutputSection } from "frances:v1/sections";
        const f = new ShellOutputSection({ cmd: "ls" });
        transcript.push(f);
        const w = f.writable.getWriter();
        await w.write("a\n");
        await w.write("b\n");
        f.exit(0);
        await w.close();   // autoclose fires frame.close()
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
    let (frames, result) = drive_one_cycle(&mut handle).await;
    assert!(matches!(result, Some(Ok(()))), "got {result:?}");

    let frame_id = match frames.first() {
        Some(SectionTranscript::Set { id, section: spec }) => {
            match &spec.kind {
                SectionKind::ShellOutput {
                    state: ShellState::Running,
                    cmd,
                } => {
                    assert_eq!(cmd, "ls");
                    assert_eq!(spec.seed.as_deref(), Some(""));
                }
                other => panic!("expected ShellOutput Running, got {other:?}"),
            }
            *id
        }
        other => panic!("expected first frame to be Set, got {other:?}"),
    };

    let appends: Vec<&String> = frames
        .iter()
        .filter_map(|f| match f {
            SectionTranscript::Append { id, delta } if *id == frame_id => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(
        appends.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["a\n", "b\n"]
    );

    let saw_exit = frames.iter().any(|f| {
        matches!(
            f,
            SectionTranscript::Set { id, section: spec } if *id == frame_id && matches!(&spec.kind, SectionKind::ShellOutput { state: ShellState::Exit(0), .. }),
        )
    });
    assert!(saw_exit, "expected a metadata Set(Exit(0)) for the frame");

    let saw_close = frames
        .iter()
        .any(|f| matches!(f, SectionTranscript::Close { id } if *id == frame_id));
    assert!(saw_close, "expected Close for the frame");
}

#[tokio::test]
async fn frame_autoclose_can_be_disabled() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const f = new MarkdownSection({ content: "" });
        f.autoclose = false;
        transcript.push(f);
        const w = f.writable.getWriter();
        await w.write("hi");
        await w.close();   // autoclose disabled — no Close emitted
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
    let (frames, _result) = drive_one_cycle(&mut handle).await;
    let close_count = frames
        .iter()
        .filter(|f| matches!(f, SectionTranscript::Close { .. }))
        .count();
    assert_eq!(close_count, 0, "autoclose=false should suppress Close");
}

/// `new MarkdownSection({ ..., closed: true })` pre-seals the frame:
/// `transcript.push` emits `Close` immediately after `Set`.
#[tokio::test]
async fn markdown_frame_closed_ctor_option_pushes_and_closes_in_one_shot() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: "hi", closed: true }));
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
    let push_id = match frames.first() {
        Some(SectionTranscript::Set { id, .. }) => *id,
        other => panic!("expected push first, got {other:?}"),
    };
    assert!(
        matches!(frames.get(1), Some(SectionTranscript::Close { id }) if *id == push_id),
        "second frame must be the matching Close: {frames:?}"
    );
}

/// `.close()` returns `this`; `transcript.push(frame.close())` emits
/// `Set` then `Close`, the same as `{ closed: true }`.
#[tokio::test]
async fn markdown_frame_close_returns_this_for_chaining() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: "hi" }).close());
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
    let push_id = match frames.first() {
        Some(SectionTranscript::Set { id, .. }) => *id,
        other => panic!("expected push first, got {other:?}"),
    };
    assert!(
        matches!(frames.get(1), Some(SectionTranscript::Close { id }) if *id == push_id),
        "expected Push then Close: {frames:?}"
    );
}

#[tokio::test]
async fn markdown_frame_writable_is_stable_writable_stream() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { WritableStream } from "whatwg:web-streams";
        const f = new MarkdownSection({ content: "hi" });
        transcript.push(f);
        const w1 = f.writable;
        const w2 = f.writable;
        const shape = `ws=${w1 instanceof WritableStream} stable=${w1 === w2} hasWrite=${typeof MarkdownSection.prototype.write}`;
        transcript.push(new MarkdownSection({ content: shape }));
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
    // The raw `write` is removed from the prototype — only the
    // WritableStream is the public path.
    let last = text_of(frames.last().expect("at least one frame"));
    assert_eq!(last, "ws=true stable=true hasWrite=undefined");
}
