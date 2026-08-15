use super::*;

use crate::runtime::EntityCmd;

fn entity_cmds(frames: &[SectionTranscript]) -> Vec<&EntityCmd> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            SectionTranscript::Entity(cmd) => Some(cmd),
            _ => None,
        })
        .collect()
}

/// The full producer lifecycle in order: creating Upsert, appends,
/// settle with artifacts — one FIFO, payloads carried verbatim.
#[tokio::test]
async fn create_append_settle_flow() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { createEntity } from "frances:v1/entities";
        const e = createEntity("shell", { cmd: "ls", state: "running" });
        e.append({ text: "a" });
        e.append({ text: "b" });
        e.updateSnapshot({ cmd: "ls", state: "running", bytes: 2 });
        e.settle({ cmd: "ls", state: "success" }, { artifacts: { llm_digest: "Exit 0" } });
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

    let cmds = entity_cmds(&frames);
    assert_eq!(cmds.len(), 5);

    let EntityCmd::Upsert {
        entity_id,
        kind,
        snapshot,
    } = cmds[0]
    else {
        panic!("expected creating Upsert, got {:?}", cmds[0]);
    };
    assert_eq!(kind, "shell");
    assert_eq!(snapshot["cmd"], "ls");
    let created_id = *entity_id;

    match cmds[1] {
        EntityCmd::Append { entity_id, payload } => {
            assert_eq!(*entity_id, created_id);
            assert_eq!(payload["text"], "a");
        }
        other => panic!("expected Append, got {other:?}"),
    }
    match cmds[3] {
        EntityCmd::Upsert { snapshot, .. } => assert_eq!(snapshot["bytes"], 2),
        other => panic!("expected metadata Upsert, got {other:?}"),
    }
    match cmds[4] {
        EntityCmd::Settle {
            entity_id,
            snapshot,
            artifacts,
        } => {
            assert_eq!(*entity_id, created_id);
            assert_eq!(snapshot["state"], "success");
            assert_eq!(
                artifacts.as_slice(),
                &[("llm_digest".to_owned(), serde_json::json!("Exit 0"))]
            );
        }
        other => panic!("expected Settle, got {other:?}"),
    }
}

/// `EntityRefSection` pushes a one-shot ref carrying the handle's id,
/// after the entity's creating Upsert on the same channel.
#[tokio::test]
async fn entity_ref_section_follows_upsert() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { createEntity } from "frances:v1/entities";
        import { transcript, EntityRefSection } from "frances:v1/sections";
        const e = createEntity("shell", { cmd: "true" });
        transcript.push(new EntityRefSection({ id: e.id }));
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

    let upsert_pos = frames
        .iter()
        .position(|f| matches!(f, SectionTranscript::Entity(EntityCmd::Upsert { .. })))
        .expect("creating Upsert");
    let (ref_pos, ref_entity) = frames
        .iter()
        .enumerate()
        .find_map(|(i, f)| match f {
            SectionTranscript::Set { section, .. } => match &section.kind {
                SectionKind::EntityRef { entity_id } => Some((i, *entity_id)),
                _ => None,
            },
            _ => None,
        })
        .expect("EntityRef Set");
    assert!(upsert_pos < ref_pos, "Upsert must precede the ref");

    let SectionTranscript::Entity(EntityCmd::Upsert { entity_id, .. }) = &frames[upsert_pos] else {
        unreachable!();
    };
    assert_eq!(*entity_id, ref_entity);
}

/// Every producer verb throws once the handle has settled.
#[tokio::test]
async fn settled_handle_rejects_further_verbs() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { createEntity } from "frances:v1/entities";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const e = createEntity("shell", {});
        e.settle({ state: "success" });
        const throws = (fn) => { try { fn(); return false; } catch { return true; } };
        const results = [
            throws(() => e.append({ text: "late" })),
            throws(() => e.updateSnapshot({})),
            throws(() => e.settle({})),
        ];
        transcript.push(new ErrorSection({ content: JSON.stringify(results), closed: true }));
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

    let verdicts = frames
        .iter()
        .find_map(|f| match f {
            SectionTranscript::Set { section, .. }
                if matches!(section.kind, SectionKind::Error) =>
            {
                section.seed.clone()
            }
            _ => None,
        })
        .expect("verdict section");
    assert_eq!(verdicts, "[true,true,true]");

    // Nothing after the settle reached the channel.
    let cmds = entity_cmds(&frames);
    assert_eq!(cmds.len(), 2, "creating Upsert + Settle only");
}
