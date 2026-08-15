use super::*;

/// Pushing an empty frame and never writing to it produces no `Append`
/// — just the `Set`, the close-time metadata `Set`, and the `Close`.
#[tokio::test]
async fn empty_frame_pushes_and_closes_without_appends() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, ReasoningSection } from "frances:v1/sections";
        const f = new ReasoningSection();
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
            if matches!(&spec.kind, SectionKind::Reasoning { .. })
    ));
    assert!(
        frames
            .iter()
            .any(|f| matches!(f, SectionTranscript::Close { .. })),
        "closing the writable should seal the frame"
    );
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
        import { transcript, ReasoningSection } from "frances:v1/sections";
        const f = new ReasoningSection();
        transcript.push(f);
        const w = f.writable.getWriter();
        await w.write("pondering");
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
    assert!(matches!(
        &frames[0],
        SectionTranscript::Set { section: spec, .. }
            if matches!(&spec.kind, SectionKind::Reasoning { .. })
    ));
    assert!(matches!(&frames[1], SectionTranscript::Append { delta, .. } if delta == "pondering"));
}

#[tokio::test]
async fn write_to_earlier_frame_after_newer_push_still_works() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, ReasoningSection } from "frances:v1/sections";
        const a = new ReasoningSection();
        transcript.push(a);
        transcript.push(new ReasoningSection());
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
async fn frame_autoclose_can_be_disabled() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, ReasoningSection } from "frances:v1/sections";
        const f = new ReasoningSection();
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

/// `.close()` returns `this`; `transcript.push(frame.close())` emits
/// `Set` then `Close` in one shot.
#[tokio::test]
async fn frame_close_returns_this_for_chaining() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, ReasoningSection } from "frances:v1/sections";
        transcript.push(new ReasoningSection().close());
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
async fn frame_writable_is_stable_writable_stream() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, ErrorSection, ReasoningSection } from "frances:v1/sections";
        import { WritableStream } from "whatwg:web-streams";
        const f = new ReasoningSection();
        transcript.push(f);
        const w1 = f.writable;
        const w2 = f.writable;
        const shape = `ws=${w1 instanceof WritableStream} stable=${w1 === w2} hasWrite=${typeof ReasoningSection.prototype.write}`;
        transcript.push(new ErrorSection({ content: shape }));
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
