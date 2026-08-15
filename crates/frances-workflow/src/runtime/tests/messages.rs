use super::*;

use crate::runtime::EntityCmd;

/// `postMessage` is one entity lifecycle in one call: creating Upsert
/// (kind "chat", full text), transcript ref, then Settle — in order.
#[tokio::test]
async fn post_message_creates_refs_and_settles() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { postMessage } from "frances:v1/messages";
        postMessage({ source: "assistant", content: "hello" });
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

    let EntityCmd::Upsert {
        entity_id,
        kind,
        snapshot,
    } = (match &frames[0] {
        SectionTranscript::Entity(cmd) => cmd,
        other => panic!("expected creating Upsert first, got {other:?}"),
    })
    else {
        panic!("expected creating Upsert first, got {frames:?}");
    };
    assert_eq!(kind, "chat");
    assert_eq!(snapshot["source"], "assistant");
    assert_eq!(snapshot["text"], "hello");
    let created_id = *entity_id;

    assert!(
        matches!(
            &frames[1],
            SectionTranscript::Set { section, .. }
                if matches!(section.kind, SectionKind::EntityRef { entity_id } if entity_id == created_id)
        ),
        "expected the transcript ref second: {frames:?}"
    );
    assert!(
        matches!(
            &frames[2],
            SectionTranscript::Entity(EntityCmd::Settle { entity_id, snapshot, .. })
                if *entity_id == created_id && snapshot["text"] == "hello"
        ),
        "expected the Settle third: {frames:?}"
    );
}

/// `openMessage` refreshes the snapshot with the full accumulated text
/// on every write and settles with the final text on close (writable
/// close included, via autoclose).
#[tokio::test]
async fn open_message_streams_snapshot_and_settles() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { openMessage } from "frances:v1/messages";
        const m = openMessage("user");
        m.write("hel");
        const w = m.writable.getWriter();
        await w.write("lo");
        await w.close();
        m.close(); // idempotent — no second Settle
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

    let cmds: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            SectionTranscript::Entity(cmd) => Some(cmd),
            _ => None,
        })
        .collect();

    // Creating Upsert, one Upsert per write, one Settle.
    assert_eq!(cmds.len(), 4, "got {cmds:?}");
    assert!(
        matches!(cmds[0], EntityCmd::Upsert { kind, snapshot, .. }
            if kind == "chat" && snapshot["source"] == "user" && snapshot["text"] == ""),
        "creating Upsert: {cmds:?}"
    );
    assert!(
        matches!(cmds[1], EntityCmd::Upsert { snapshot, .. } if snapshot["text"] == "hel"),
        "first write refresh: {cmds:?}"
    );
    assert!(
        matches!(cmds[2], EntityCmd::Upsert { snapshot, .. } if snapshot["text"] == "hello"),
        "second write refresh: {cmds:?}"
    );
    assert!(
        matches!(cmds[3], EntityCmd::Settle { snapshot, .. } if snapshot["text"] == "hello"),
        "settle with final text: {cmds:?}"
    );
}
