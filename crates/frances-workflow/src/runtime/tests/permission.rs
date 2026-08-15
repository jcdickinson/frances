use super::*;

/// `approve()` emits a HostFrame::Permission, awaits the host's response,
/// and resumes with the reply value.
#[tokio::test]
async fn approve_yes_round_trip() {
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { approve } from "frances:v1/approval";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const choice = await approve({ prompt: "delete /tmp/foo?" });
        transcript.push(new ErrorSection({
            content: `${choice.type}:${choice.details ?? ""}`,
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

    let req = tokio::time::timeout(CYCLE_TIMEOUT, async {
        loop {
            if let Some(req) = handle.outputs.permissions.recv().await {
                return req;
            }
        }
    })
    .await
    .expect("approval frame did not arrive in time");
    assert_eq!(req.prompt, "delete /tmp/foo?");
    assert!(!req.allow_auto, "default allowAuto should be false");

    assert!(
        req.reply
            .send(PermissionResponse::Yes {
                details: Some("scoped to /tmp".into()),
            })
            .is_ok(),
        "answer should land on the embedded reply slot",
    );

    let result = tokio::time::timeout(CYCLE_TIMEOUT, &mut handle.done)
        .await
        .expect("workflow did not finish in time")
        .expect("done channel closed without value");
    assert!(matches!(result, Ok(())), "got {result:?}");

    let mut deltas = Vec::new();
    while let Ok(d) = handle.outputs.transcript.try_recv() {
        deltas.push(d);
    }
    let last = deltas
        .iter()
        .rev()
        .find_map(|d| match d {
            SectionTranscript::Set { section, .. } => Some(section),
            _ => None,
        })
        .expect("expected an error set after approval");
    assert!(
        matches!(&last.kind, SectionKind::Error)
            && last.seed.as_deref() == Some("yes:scoped to /tmp"),
        "got {last:?}",
    );
}

/// Non-string prompts throw a TypeError before any frame is emitted.
#[tokio::test]
async fn approve_rejects_non_string_prompt() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { approve } from "frances:v1/approval";
        await approve(42);
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
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}

// --- `complete` direct export -----------------------------------------
