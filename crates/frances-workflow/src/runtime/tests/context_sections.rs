use super::*;

#[tokio::test]
async fn context_sections_env_block_emits_environment_info() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { envBlock } from "frances:v1/context-sections";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        // Simulate the ctx that ChatSession builds from _envInfo() + tools
        const ctx = {
            os: "linux",
            shell: "/bin/bash",
            platform: "unix",
            repoRoot: "/home/user/project",
            cwd: "/home/user/project/src",
            date: "2024-01-15",
            tools: [],
        };

        const result = envBlock.prompt(ctx);
        transcript.push(new MarkdownSection({ content: result }));
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
        output.contains("Environment:"),
        "missing Environment header"
    );
    assert!(output.contains("- OS: linux"), "missing OS");
    assert!(output.contains("- Shell: /bin/bash"), "missing Shell");
    assert!(output.contains("- Platform: unix"), "missing Platform");
    assert!(
        output.contains("- Repo root: /home/user/project"),
        "missing Repo root"
    );
    assert!(output.contains("- Date: 2024-01-15"), "missing Date");
    assert!(
        output.contains("Shell behavior:"),
        "missing Shell behavior section"
    );
    assert!(
        output.contains("persistent"),
        "missing persistent shell rule"
    );
    assert!(
        output.contains("Do not prefix commands with `cd`"),
        "missing cd guidance"
    );
    assert!(
        output.contains("Prefer the dedicated tools"),
        "missing prefer-tools guidance"
    );
}

#[tokio::test]
async fn context_sections_env_block_omits_repo_root_when_null() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { envBlock } from "frances:v1/context-sections";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const ctx = {
            os: "macos",
            shell: "/bin/zsh",
            platform: "unix",
            repoRoot: null,
            cwd: "/tmp",
            date: "2024-06-01",
            tools: [],
        };

        const result = envBlock.prompt(ctx);
        transcript.push(new MarkdownSection({ content: result }));
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
        !output.contains("Repo root:"),
        "should not contain Repo root when null, got: {output}"
    );
    assert!(output.contains("OS: macos"), "missing OS");
}

#[tokio::test]
async fn context_sections_cwd_block_emits_cwd() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { cwdBlock } from "frances:v1/context-sections";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const ctx = {
            cwd: "/home/user/project/src",
        };

        const result = cwdBlock.prompt(ctx);
        transcript.push(new MarkdownSection({ content: result }));
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
    assert_eq!(output, "Current working directory: /home/user/project/src");
}

#[tokio::test]
async fn context_sections_cwd_block_returns_null_when_no_cwd() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { cwdBlock } from "frances:v1/context-sections";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const ctx = {
            cwd: null,
        };

        const result = cwdBlock.prompt(ctx);
        transcript.push(new MarkdownSection({ content: String(result) }));
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
    assert_eq!(output, "null", "expected null when cwd is null");
}

#[tokio::test]
async fn context_sections_are_stable_objects() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { envBlock, cwdBlock } from "frances:v1/context-sections";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        // Import twice — must be the same object (=== identity)
        const env1 = envBlock;
        const env2 = envBlock;
        const cwd1 = cwdBlock;
        const cwd2 = cwdBlock;

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            envStable: env1 === env2,
            cwdStable: cwd1 === cwd2,
            envHasName: "name" in envBlock,
            cwdHasName: "name" in cwdBlock,
            envName: envBlock.name,
            cwdName: cwdBlock.name,
            envHasPrompt: typeof envBlock.prompt === "function",
            cwdHasPrompt: typeof cwdBlock.prompt === "function",
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
    assert_eq!(result["envStable"], true);
    assert_eq!(result["cwdStable"], true);
    assert_eq!(result["envHasName"], true);
    assert_eq!(result["cwdHasName"], true);
    assert_eq!(result["envName"], "env");
    assert_eq!(result["cwdName"], "cwd");
    assert_eq!(result["envHasPrompt"], true);
    assert_eq!(result["cwdHasPrompt"], true);
}

#[tokio::test]
async fn context_sections_env_block_always_returns_string() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { envBlock } from "frances:v1/context-sections";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        // Even with minimal ctx
        const ctx = {
            os: "linux",
            shell: "unknown",
            platform: "unix",
            repoRoot: null,
            cwd: null,
            date: "2024-01-01",
            tools: [],
        };

        const result = envBlock.prompt(ctx);
        transcript.push(new MarkdownSection({ content: typeof result }));
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
    assert_eq!(text_of(&frames[0]), "string");
}
