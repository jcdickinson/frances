use super::*;

#[tokio::test]
async fn set_status_emits_status_frames() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { setStatus } from "frances:v1/workflow";
        setStatus("working…");
        setStatus(null);
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
    assert!(matches!(done, Some(Ok(()))));
    let mut surfaces = Vec::new();
    while let Ok(s) = handle.outputs.surfaces.try_recv() {
        surfaces.push(s);
    }
    assert_eq!(
        surfaces,
        vec![
            SurfaceCmd::SetFooter {
                text: "working…".to_string()
            },
            SurfaceCmd::ClearFooter,
        ],
        "expected set then clear on the surfaces channel",
    );
}

#[tokio::test]
async fn set_title_emits_title_frames_and_get_title_tracks() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { setTitle, getTitle } from "frances:v1/workflow";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: `before:${getTitle()}` }));
        setTitle("fixing the bug");
        transcript.push(new MarkdownSection({ content: `after:${getTitle()}` }));
        setTitle(null);
        transcript.push(new MarkdownSection({ content: `cleared:${getTitle()}` }));
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

    let texts: Vec<String> = frames
        .iter()
        .map(text_of)
        .filter(|text| !text.is_empty())
        .collect();
    assert_eq!(
        texts,
        vec!["before:null", "after:fixing the bug", "cleared:null"],
        "getTitle should track setTitle locally",
    );

    let mut surfaces = Vec::new();
    while let Ok(s) = handle.outputs.surfaces.try_recv() {
        surfaces.push(s);
    }
    assert_eq!(
        surfaces,
        vec![
            SurfaceCmd::SetTitle {
                title: Some("fixing the bug".to_string())
            },
            SurfaceCmd::SetTitle { title: None },
        ],
        "expected set then clear on the surfaces channel",
    );
}

#[tokio::test]
async fn body_returns_terminates_workflow() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: "hi" }));
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
    assert_eq!(text_of(&frames[0]), "hi");
}

#[tokio::test]
async fn dangling_promise_does_not_keep_workflow_alive() {
    // A bare `new Promise(() => {})` has no backing host IO, so the
    // event loop empties around it and the workflow reaps cleanly
    // rather than hanging.
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: "before" }));
        await new Promise(() => {});
        transcript.push(new MarkdownSection({ content: "unreachable" }));
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
        frames.iter().any(|f| text_of(f) == "before"),
        "missing 'before': {frames:?}"
    );
    assert!(
        !frames.iter().any(|f| text_of(f) == "unreachable"),
        "should not run past the dangling await: {frames:?}"
    );
}

#[tokio::test]
async fn import_meta_args_populated() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: import.meta.args.join('|') }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        })
        .await
        .unwrap();

    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))));
    assert_eq!(text_of(&frames[0]), "a|b|c");
}

#[tokio::test]
async fn import_meta_instance_populated() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: import.meta.instance }));
        "#,
    );
    let instance = uuid::Uuid::from_u128(0xfeed_face_0000_0000_0000_0000_0000_0001);
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            instance_id: instance,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(handle.instance, instance);
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))));
    assert_eq!(text_of(&frames[0]), instance.to_string());
}

/// The workflow's `lifecycle.shutdown` handler fires when the host
/// calls `request_shutdown`. The handler emits a final frame, then
/// the inbox closes and the for-await loop unwinds.
#[tokio::test]
async fn lifecycle_shutdown_runs_on_request() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { lifecycle } from "frances:v1/lifecycle";

        lifecycle.shutdown = async () => {
            transcript.push(new MarkdownSection({ content: "bye" }));
        };
        for await (const _ of inbox) {
            transcript.push(new MarkdownSection({ content: "got input" }));
        }
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(frames.is_empty(), "got {frames:?}");
    assert!(done.is_none());

    handle.request_shutdown();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "bye");
}

/// `workflow.exit()` now also routes through the lifecycle IIFE, so
/// a registered shutdown handler fires before the body terminates.
#[tokio::test]
async fn lifecycle_shutdown_runs_on_exit() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { lifecycle } from "frances:v1/lifecycle";
        import { exit } from "frances:v1/workflow";

        lifecycle.shutdown = async () => {
            transcript.push(new MarkdownSection({ content: "bye" }));
        };
        queueMicrotask(() => exit());
        for await (const _ of inbox) {
            transcript.push(new MarkdownSection({ content: "got input" }));
        }
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(matches!(result, Ok(())), "result was {result:?}");
    assert_eq!(text_of(&frames[0]), "bye");
}

/// A workflow that never registers `lifecycle.shutdown` still
/// terminates promptly on `request_shutdown` — the IIFE closes the
/// inbox unconditionally after the (absent) handler returns.
#[tokio::test]
async fn request_shutdown_without_handler_terminates_promptly() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        for await (const _ of inbox) {}
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (_frames, done) = drive_one_cycle(&mut handle).await;
    assert!(done.is_none());

    handle.request_shutdown();
    let (_frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))));
}

#[tokio::test]
async fn fresh_context_per_invocation() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        globalThis.__counter = (globalThis.__counter ?? 0) + 1;
        transcript.push(new MarkdownSection({ content: String(globalThis.__counter) }));
        "#,
    );
    let path = file.path().to_path_buf();

    for _ in 0..3 {
        let mut handle = rt
            .start(Invocation {
                source_path: path.clone(),
                args: Vec::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        let (frames, done) = drive_one_cycle(&mut handle).await;
        assert!(matches!(done, Some(Ok(()))));
        assert_eq!(
            text_of(&frames[0]),
            "1",
            "expected counter=1 each invocation"
        );
    }
}

#[tokio::test]
async fn ts_transpile_strips_types() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "ts",
        r#"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const args: string[] = import.meta.args;
        transcript.push(new MarkdownSection({ content: args.length.toString() }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: vec!["x".into(), "y".into(), "z".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))));
    assert_eq!(text_of(&frames[0]), "3");
}

#[tokio::test]
async fn script_throw_surfaces_as_script_error() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source("js", "throw new Error('boom');");
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}

#[tokio::test]
async fn unknown_v1_module_fails_to_load() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source("js", r#"import { nope } from "frances:v1/nope";"#);
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}
