use super::*;

fn main_workflow_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/workflows/main.ts")
}

fn main_invocation(entity: uuid::Uuid, instance_id: uuid::Uuid) -> Invocation {
    Invocation {
        source_path: main_workflow_path(),
        args: Vec::new(),
        entity,
        instance_id,
        migrations: Vec::new(),
    }
}

fn contains_text(frames: &[SectionTranscript], needle: &str) -> bool {
    frames.iter().any(|frame| text_of(frame).contains(needle))
}

async fn wait_for_text(handle: &mut WorkflowHandle, needle: &str) -> Vec<SectionTranscript> {
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + CYCLE_TIMEOUT;
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for {needle:?}; saw {frames:?}"
        );
        let remaining = deadline - now;
        let frame = tokio::time::timeout(remaining, handle.outputs.transcript.recv())
            .await
            .expect("timed out waiting for transcript")
            .expect("transcript closed before expected text");
        frames.push(frame);
        if contains_text(&frames, needle) {
            return frames;
        }
    }
}

#[tokio::test]
async fn main_workflow_hydrates_restart_state_without_ready_banner() {
    let deps = StubDeps::default();
    let entity = uuid::Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0001);
    let instance = uuid::Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001);

    let rt = Runtime::new(deps.clone()).unwrap();
    let seeder = write_source(
        "js",
        r####"
        import { ChatSession } from "frances:v1/chat";
        import { db } from "frances:v1/storage";

        const instanceId = String(import.meta.instance);
        const chat = new ChatSession({ model_intents: ["chat"] });
        const chatId = await chat.ensurePersisted();
        const state = {
          schemaVersion: 1,
          instanceId,
          mode: "planning",
          plan: {
            title: "Restart regression plan",
            prelude: "Persisted plan context",
            updatedAt: new Date().toISOString(),
            steps: [{
              id: "step-a",
              title: "Keep state",
              body: "I keep state across restart.",
              status: "active"
            }]
          },
          nextStepId: 2,
          currentChat: { id: chatId, mode: "planning", pendingSeed: null },
          variables: [["restart_marker", { survived: true, count: 1 }]],
          stepTranscript: { entries: ["## User\nseeded before restart"], summary: null },
          pending: { completion: null, planExit: true, planBegin: null }
        };
        await db.exec(
          "CREATE TABLE IF NOT EXISTS main_workflow_state (" +
          "instance_id TEXT PRIMARY KEY, " +
          "version INTEGER NOT NULL, " +
          "state_json TEXT NOT NULL, " +
          "updated_at TEXT NOT NULL" +
          ")"
        );
        await db.exec(
          "INSERT INTO main_workflow_state (instance_id, version, state_json, updated_at) " +
          "VALUES (?, ?, ?, ?)",
          [instanceId, 1, JSON.stringify(state), new Date().toISOString()]
        );
        "####,
    );
    let mut seed_handle = rt
        .start(Invocation {
            source_path: seeder.path().to_path_buf(),
            args: Vec::new(),
            entity,
            instance_id: instance,
            migrations: Vec::new(),
        })
        .await
        .unwrap();
    let (_frames, done) = drive_to_done(&mut seed_handle).await;
    assert!(done.is_ok(), "seed workflow failed: {done:?}");

    let mut restored = rt.start(main_invocation(entity, instance)).await.unwrap();
    let (boot_frames, done) = drive_one_cycle(&mut restored).await;
    assert!(
        done.is_none(),
        "main workflow should wait for input: {done:?}"
    );
    assert!(
        !contains_text(&boot_frames, "frances ready"),
        "restored workflow must not re-emit ready banner: {boot_frames:?}"
    );

    restored
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "resume from restored state".to_owned(),
        }))
        .unwrap();
    let frames = wait_for_text(&mut restored, "Planning complete").await;
    assert!(
        contains_text(&frames, "Planning complete"),
        "pending plan_exit should be consumed after hydrate: {frames:?}"
    );
}

#[tokio::test]
async fn variables_entries_replace_round_trip_store_contents() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r####"
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { Variables } from "frances:v1/tools/variable";

        const vars = new Variables();
        vars.replace([["restart_marker", { survived: true, count: 1 }]]);
        const restored = new Variables();
        restored.replace(vars.entries());
        transcript.push(new MarkdownSection({
          content: JSON.stringify(restored.get("restart_marker")),
          closed: true
        }));
        "####,
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
    assert!(matches!(done, Some(Ok(()))), "workflow failed: {done:?}");
    assert_eq!(text_of(&frames[0]), r#"{"survived":true,"count":1}"#);
}
