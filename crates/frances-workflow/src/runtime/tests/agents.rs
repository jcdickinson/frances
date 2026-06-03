use super::*;

use std::fs;

#[tokio::test]
async fn agents_discover_local_agents_finds_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("AGENTS.md"), "# project instructions").unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverLocalAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverLocalAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    assert!(result.is_array(), "expected array, got {result}");
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["content"], "# project instructions");
    assert!(arr[0]["path"].as_str().unwrap().ends_with("AGENTS.md"));
}

#[tokio::test]
async fn agents_discover_local_agents_returns_null_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverLocalAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverLocalAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    assert!(result.is_null(), "expected null, got {result}");
}

#[tokio::test]
async fn agents_discover_local_agents_returns_null_no_roots() {
    let deps = StubDeps::default();
    // Default editable_roots is vec!["/"] which has no AGENTS.md.
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverLocalAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverLocalAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    assert!(result.is_null(), "expected null, got {result}");
}

#[tokio::test]
async fn agents_discover_local_agents_priority_order() {
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
        import { discoverLocalAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverLocalAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    let arr = result.as_array().unwrap();
    // CLAUDE.md is lowest priority, AGENTS.md is higher.
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["content"], "claude instructions");
    assert_eq!(arr[1]["content"], "agents instructions");
}

#[tokio::test]
async fn agents_discover_local_agents_content_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Same content in both files — content-hash dedup keeps the first.
    fs::write(root.join("CLAUDE.md"), "shared content").unwrap();
    fs::write(root.join("AGENTS.md"), "shared content").unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverLocalAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverLocalAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    let arr = result.as_array().unwrap();
    // Content dedup: same content → only first (CLAUDE.md) survives.
    assert_eq!(arr.len(), 1);
    assert!(arr[0]["path"].as_str().unwrap().ends_with("CLAUDE.md"));
}

#[tokio::test]
async fn agents_discover_nested_agents_finds_nested_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Root-level AGENTS.md — should be excluded from nested results.
    fs::write(root.join("AGENTS.md"), "root instructions").unwrap();
    // Nested AGENTS.md — should be included.
    fs::create_dir_all(root.join("crates").join("foo")).unwrap();
    fs::write(
        root.join("crates").join("foo").join("AGENTS.md"),
        "crate foo instructions",
    )
    .unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverNestedAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverNestedAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let path = arr[0].as_str().unwrap();
    assert!(path.contains("crates/foo/AGENTS.md"), "path was {path}");
}

#[tokio::test]
async fn agents_discover_nested_agents_returns_null_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverNestedAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverNestedAgents();
        transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
    assert!(result.is_null(), "expected null, got {result}");
}

#[tokio::test]
async fn agents_discover_global_agents_null_then_found() {
    // Two phases in one test to avoid parallel HOME interference.
    let original_home = std::env::var("HOME").ok();
    let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();

    // Phase 1: empty HOME → null
    {
        let home_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HOME", home_dir.path()); };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config")); };

        let deps = StubDeps::default();
        let rt = Runtime::new(deps).unwrap();
        let file = write_source(
            "js",
            r#"
            import { discoverGlobalAgents } from "frances:v1/agents";
            import { transcript, MarkdownSection } from "frances:v1/sections";

            const result = await discoverGlobalAgents();
            transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
        assert!(result.is_null(), "expected null, got {result}");
    }

    // Phase 2: HOME with AGENTS.md → found
    {
        let home_dir = tempfile::tempdir().unwrap();
        fs::write(home_dir.path().join("AGENTS.md"), "home agents").unwrap();
        unsafe { std::env::set_var("HOME", home_dir.path()); };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config")); };

        let deps = StubDeps::default();
        let rt = Runtime::new(deps).unwrap();
        let file = write_source(
            "js",
            r#"
            import { discoverGlobalAgents } from "frances:v1/agents";
            import { transcript, MarkdownSection } from "frances:v1/sections";

            const result = await discoverGlobalAgents();
            transcript.push(new MarkdownSection({ content: JSON.stringify(result) }));
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
        assert!(result.is_array(), "expected array, got {result}");
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["content"], "home agents");
    }

    // Restore env.
    match original_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    match original_xdg {
        Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
    }
}

#[tokio::test]
async fn agents_local_candidates_cover_claude_and_agents_and_local() {
    // Tests that the local candidates include .local.md variants and
    // the .agents/frances/ directory.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("CLAUDE.md"), "claude").unwrap();
    fs::write(root.join("CLAUDE.local.md"), "claude local").unwrap();
    fs::write(root.join("AGENTS.md"), "agents").unwrap();
    fs::write(root.join("AGENTS.local.md"), "agents local").unwrap();
    fs::create_dir_all(root.join(".agents").join("frances")).unwrap();
    fs::write(
        root.join(".agents").join("frances").join("AGENTS.md"),
        "frances agents",
    )
    .unwrap();

    let mut deps = StubDeps::default();
    deps.set_editable_roots(vec![root.to_path_buf()]);

    let rt = Runtime::new(deps).unwrap();
    let file = write_source(
        "js",
        r#"
        import { discoverLocalAgents } from "frances:v1/agents";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const result = await discoverLocalAgents();
        const paths = result.map(r => r.path.split("/").pop());
        transcript.push(new MarkdownSection({ content: JSON.stringify(paths) }));
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
    let names: Vec<&str> = result.as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(
        names,
        &["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md", "AGENTS.local.md", "AGENTS.md"]
    );
}

