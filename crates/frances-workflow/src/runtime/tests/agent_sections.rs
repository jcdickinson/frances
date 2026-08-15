use super::*;

use std::fs;

#[tokio::test]
async fn agent_sections_local_agents_emits_formatted_content() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("AGENTS.md"), "# project instructions").unwrap();
    fs::write(root.join("CLAUDE.md"), "claude rules").unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { localAgents } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const result = await localAgents.prompt({});
        transcript.push(new ErrorSection({ content: result }));
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
    let output = text_of(&frames[0]);
    assert!(
        output.contains("Project instruction files"),
        "missing header, got: {output}"
    );
    assert!(
        output.contains("lowest priority first"),
        "missing precedence note, got: {output}"
    );
    assert!(
        output.contains("# project instructions"),
        "missing AGENTS.md content, got: {output}"
    );
    assert!(
        output.contains("claude rules"),
        "missing CLAUDE.md content, got: {output}"
    );
    assert!(
        output.contains("---") && output.contains("AGENTS.md"),
        "missing path label, got: {output}"
    );
    assert!(
        output.contains("`.local` files take precedence"),
        "missing .local precedence note, got: {output}"
    );
    assert!(
        output.contains("Project instructions take precedence over global"),
        "missing global precedence note, got: {output}"
    );
}

#[tokio::test]
async fn agent_sections_local_agents_returns_null_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { localAgents } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const result = await localAgents.prompt({});
        transcript.push(new ErrorSection({ content: String(result) }));
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
    assert_eq!(text_of(&frames[0]), "null");
}

#[tokio::test]
async fn agent_sections_global_agents_section_shape() {
    // The Rust-backed discovery is tested in the agents module tests.
    // This test verifies the section object works end-to-end: prompt()
    // returns either null or a correctly-formatted string (may find files
    // from the runner's real HOME in parallel test environments).
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { globalAgents } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const result = await globalAgents.prompt({});
        const isNullOrString = result === null || typeof result === "string";
        const hasCorrectName = globalAgents.name === "global-agents";

        // If a string was returned, verify formatting structure
        let hasHeader = true;
        let hasPrecedence = true;
        if (result !== null) {
            hasHeader = result.includes("Global instruction files");
            hasPrecedence = result.includes("Project and local instructions take precedence");
        }

        transcript.push(new ErrorSection({ content: JSON.stringify({
            isNullOrString,
            hasCorrectName,
            hasHeader,
            hasPrecedence,
        }) }));
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
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["isNullOrString"], true,
        "prompt should return null or string"
    );
    assert_eq!(
        result["hasCorrectName"], true,
        "section name should be global-agents"
    );
    assert_eq!(
        result["hasHeader"], true,
        "non-null output should contain header"
    );
    assert_eq!(
        result["hasPrecedence"], true,
        "non-null output should contain precedence note"
    );
}

#[tokio::test]
async fn agent_sections_nested_agents_inventory_emits_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("AGENTS.md"), "root instructions").unwrap();
    fs::create_dir_all(root.join("crates").join("foo")).unwrap();
    fs::write(
        root.join("crates").join("foo").join("AGENTS.md"),
        "crate foo",
    )
    .unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { nestedAgentsInventory } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const result = await nestedAgentsInventory.prompt({});
        transcript.push(new ErrorSection({ content: result }));
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
    let output = text_of(&frames[0]);
    assert!(
        output.contains("Nested instruction files found in subdirectories"),
        "missing header, got: {output}"
    );
    assert!(
        output.contains("crates/foo/AGENTS.md"),
        "missing nested path, got: {output}"
    );
    assert!(
        output.contains("file_read"),
        "missing read nudge, got: {output}"
    );
    // Root-level AGENTS.md should NOT appear in the nested inventory
    assert!(
        !output.contains("root instructions"),
        "should not contain root-level content, got: {output}"
    );
}

#[tokio::test]
async fn agent_sections_nested_agents_returns_null_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { nestedAgentsInventory } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const result = await nestedAgentsInventory.prompt({});
        transcript.push(new ErrorSection({ content: String(result) }));
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
    assert_eq!(text_of(&frames[0]), "null");
}

#[tokio::test]
async fn agent_sections_are_stable_objects() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { globalAgents, localAgents, nestedAgentsInventory } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        transcript.push(new ErrorSection({ content: JSON.stringify({
            globalStable: globalAgents === globalAgents,
            localStable: localAgents === localAgents,
            nestedStable: nestedAgentsInventory === nestedAgentsInventory,
            globalName: globalAgents.name,
            localName: localAgents.name,
            nestedName: nestedAgentsInventory.name,
            globalHasPrompt: typeof globalAgents.prompt === "function",
            localHasPrompt: typeof localAgents.prompt === "function",
            nestedHasPrompt: typeof nestedAgentsInventory.prompt === "function",
        }) }));
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
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["globalStable"], true);
    assert_eq!(result["localStable"], true);
    assert_eq!(result["nestedStable"], true);
    assert_eq!(result["globalName"], "global-agents");
    assert_eq!(result["localName"], "local-agents");
    assert_eq!(result["nestedName"], "nested-agents-inventory");
    assert_eq!(result["globalHasPrompt"], true);
    assert_eq!(result["localHasPrompt"], true);
    assert_eq!(result["nestedHasPrompt"], true);
}

#[tokio::test]
async fn agent_sections_local_agents_priority_order_in_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("CLAUDE.md"), "claude instructions").unwrap();
    fs::write(root.join("AGENTS.md"), "agents instructions").unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { localAgents } from "frances:v1/agent-sections";
        import { transcript, ErrorSection } from "frances:v1/sections";

        const result = await localAgents.prompt({});
        transcript.push(new ErrorSection({ content: result }));
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
    let output = text_of(&frames[0]);
    // CLAUDE.md (lowest priority) must appear before AGENTS.md (higher priority)
    let claude_pos = output.find("claude instructions").unwrap();
    let agents_pos = output.find("agents instructions").unwrap();
    assert!(
        claude_pos < agents_pos,
        "CLAUDE.md should appear before AGENTS.md in output"
    );
}
