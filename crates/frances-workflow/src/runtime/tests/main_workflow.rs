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

fn main_workflow_source() -> String {
    std::fs::read_to_string(main_workflow_path()).expect("read main workflow source")
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
          schemaVersion: 3,
          instanceId,
          mode: "planning",
          effort: 73,
          plan: {
            title: "Restart regression plan",
            prelude: "Persisted plan context",
            updatedAt: new Date().toISOString(),
            steps: [{

              title: "Keep state",
              body: "I keep state across restart.",
              status: "active"
            }]
          },

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
          [instanceId, 3, JSON.stringify(state), new Date().toISOString()]
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
    let sessions = deps.sessions();
    assert_eq!(
        frances_models_llm::ChatSession::effort(sessions.last().unwrap())
            .map(|effort| effort.get()),
        Some(73)
    );
}

#[tokio::test]
async fn effort_command_updates_session_without_entering_chat_history() {
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let entity = uuid::Uuid::new_v4();
    let instance = uuid::Uuid::new_v4();
    let mut handle = rt.start(main_invocation(entity, instance)).await.unwrap();
    let (_boot_frames, done) = drive_one_cycle(&mut handle).await;
    assert!(done.is_none());

    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: " /effort 73 ".to_owned(),
        }))
        .unwrap();
    let frames = wait_for_text(&mut handle, "Effort: 73%").await;
    assert!(contains_text(&frames, "/effort 73"));
    assert!(contains_text(&frames, "Effort: 73%"));

    let sessions = deps.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        frances_models_llm::ChatSession::effort(&sessions[0]).map(|effort| effort.get()),
        Some(73)
    );
    assert!(sessions[0].pending().is_empty());

    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "/effort nope".to_owned(),
        }))
        .unwrap();
    let frames = wait_for_text(&mut handle, "Usage: /effort [default|0-100]").await;
    assert!(contains_text(&frames, "Usage: /effort [default|0-100]"));
    assert_eq!(
        frances_models_llm::ChatSession::effort(&sessions[0]).map(|effort| effort.get()),
        Some(73)
    );
}

#[tokio::test]
async fn effort_command_persists_across_workflow_restart_and_can_clear() {
    let deps = StubDeps::default();
    let rt = Runtime::new(deps.clone()).unwrap();
    let entity = uuid::Uuid::new_v4();
    let instance = uuid::Uuid::new_v4();

    let mut first = rt.start(main_invocation(entity, instance)).await.unwrap();
    let (_boot_frames, done) = drive_one_cycle(&mut first).await;
    assert!(done.is_none());
    first
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "/effort 61".to_owned(),
        }))
        .unwrap();
    wait_for_text(&mut first, "Effort: 61%").await;
    drop(first);

    let mut restored = rt.start(main_invocation(entity, instance)).await.unwrap();
    let (_boot_frames, done) = drive_one_cycle(&mut restored).await;
    assert!(done.is_none());
    restored
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "/effort".to_owned(),
        }))
        .unwrap();
    wait_for_text(&mut restored, "61%").await;

    restored
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "/effort default".to_owned(),
        }))
        .unwrap();
    wait_for_text(&mut restored, "Effort: default").await;
    let sessions = deps.sessions();
    assert_eq!(
        frances_models_llm::ChatSession::effort(sessions.last().unwrap()),
        None
    );
}

#[test]
fn planning_contract_uses_atomic_protected_zero_based_operations() {
    let source = main_workflow_source();

    for operation in ["add", "update", "remove", "move"] {
        assert!(source.contains(&format!("enum: [\"{operation}\"]")));
    }
    assert!(source.contains("const next = steps.map((step) => ({ ...step }))"));
    assert!(source.contains("const boundary = editableBoundary(next)"));
    assert!(source.contains("step ${index} is protected"));
    assert!(source.contains("move crosses the protected boundary"));
    assert!(source.contains("add index must be between ${boundary} and ${next.length}"));
    assert!(source.contains("`${index}. ${step.title}`"));
    assert!(source.contains("Plan updated:\\n${planTitles() || \"(no steps)\"}"));
    assert!(!source.contains("nextStepId"));
    assert!(!source.contains("step_id"));
    assert!(!source.contains("current_step"));
}

#[test]
fn finish_step_contract_is_sequential_complete_or_skip_only() {
    let source = main_workflow_source();

    assert!(source.contains("enum: [\"complete\"]"));
    assert!(source.contains("required: [\"action\", \"summary\", \"proof\"]"));
    assert!(source.contains("enum: [\"skip\"]"));
    assert!(source.contains("required: [\"action\", \"reason\"]"));
    assert!(source.contains("if (!currentStep()) return _errResult(call.id, \"no active step\")"));
    assert!(source.contains("const judgement = await referee(signal)"));
    assert!(source.contains("if (judgement.type === \"decline\")"));
    assert!(source.contains("activateNextStep()"));
    assert!(source.contains("Completion is referee-reviewed; skipping advances directly."));
    assert!(!source.contains("task_complete"));
    assert!(!source.contains("outcome:"));
}

#[tokio::test]
async fn variables_entries_replace_round_trip_store_contents() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r####"
        import { transcript, ErrorSection } from "frances:v1/sections";
        import { Variables } from "frances:v1/tools/variable";

        const vars = new Variables();
        vars.replace([["restart_marker", { survived: true, count: 1 }]]);
        const restored = new Variables();
        restored.replace(vars.entries());
        transcript.push(new ErrorSection({
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
