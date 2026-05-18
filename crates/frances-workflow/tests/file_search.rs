//! Integration tests for the `frances:v1/tools/file_search`
//! `FileSearch` primitive and the LLM-facing `Search` tool class. Each
//! test drives a JS script through the workflow runtime against a
//! `StubDeps` with a tempdir cwd, then asserts on what the script
//! pushed back via `MarkdownFrame`.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use frances_workflow::{
    FrameKind, HostFrame, Invocation, Runtime, WorkflowError, WorkflowHandle, test_deps::StubDeps,
};

fn write_source(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".js")
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

const CYCLE_TIMEOUT: Duration = Duration::from_secs(5);

async fn drive_one_cycle(
    handle: &mut WorkflowHandle,
) -> (Vec<HostFrame>, Option<Result<(), WorkflowError>>) {
    match tokio::time::timeout(CYCLE_TIMEOUT, drive_one_cycle_inner(handle)).await {
        Ok(result) => result,
        Err(_) => panic!("drive_one_cycle timed out after {CYCLE_TIMEOUT:?} — workflow hung"),
    }
}

async fn drive_one_cycle_inner(
    handle: &mut WorkflowHandle,
) -> (Vec<HostFrame>, Option<Result<(), WorkflowError>>) {
    let mut out = Vec::new();
    loop {
        while let Ok(frame) = handle.frames.try_recv() {
            out.push(frame);
        }
        tokio::select! {
            biased;
            Some(frame) = handle.frames.recv() => out.push(frame),
            done = &mut handle.done => {
                let result = done.unwrap_or(Ok(()));
                while let Ok(frame) = handle.frames.try_recv() {
                    out.push(frame);
                }
                return (out, Some(result));
            }
            () = handle.parked.notified() => {
                while let Ok(frame) = handle.frames.try_recv() {
                    out.push(frame);
                }
                return (out, None);
            }
        }
    }
}

fn text_of(frame: &HostFrame) -> String {
    match frame {
        HostFrame::Push(p) => match &p.kind {
            FrameKind::Markdown { content, .. } => content.clone().unwrap_or_default(),
            FrameKind::Error { content } => content.clone(),
            FrameKind::Json { tag, value } => format!("[{tag}] {value}"),
            FrameKind::ShellOutput { state, content } => format!("[shell:{state:?}] {content}"),
        },
        HostFrame::Append { delta, .. } => delta.clone(),
        HostFrame::UpdateKind { id, kind } => format!("[update:{}] {kind:?}", id.0),
        HostFrame::Close { id } => format!("[close:{}]", id.0),
        HostFrame::Permission { request, .. } => {
            format!("[approval:{}] {}", request.id, request.prompt)
        }
    }
}

fn deps_with_cwd(cwd: PathBuf) -> StubDeps {
    let deps = StubDeps::default();
    deps.set_cwd(cwd);
    deps
}

async fn run_script(deps: StubDeps, script: &str) -> Vec<HostFrame> {
    let rt = Runtime::new(deps).unwrap();
    let file = write_source(script);
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
    frames
}

/// Runs the FileSearch primitive directly and emits the raw JSON
/// payload via a MarkdownFrame so tests can parse it.
const DUMP_RAW: &str = r#"
import { FileSearch } from "frances:v1/tools/file_search";
import { transcript, MarkdownFrame } from "frances:v1/frames";
const fs = new FileSearch();
const json = await fs.search(ARGS);
transcript.push(new MarkdownFrame({ content: json }));
"#;

fn dump_raw_script(args_literal: &str) -> String {
    DUMP_RAW.replace("ARGS", args_literal)
}

#[tokio::test]
async fn no_args_lists_files_with_gitignore_respected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("kept.txt"), "hello").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "secret").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script("{}")).await;
    let json = text_of(&frames[0]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("payload is JSON");
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"kept.txt"), "kept.txt missing: {paths:?}");
    assert!(
        !paths.contains(&"ignored.txt"),
        "ignored.txt leaked: {paths:?}"
    );
}

#[tokio::test]
async fn depth_one_alone_lists_cwd_children_only() {
    // `{ depth: 1 }` with no `paths` is the documented `ls` replacement.
    // The "no empty" rule must let this through (paths is omitted, not
    // `[]`), and the walker must keep results to immediate children.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("top.txt"), "").unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/nested.txt"), "").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script("{ depth: 1 }")).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["top.txt"],
        "depth: 1 should only list cwd children"
    );
}

#[tokio::test]
async fn empty_paths_without_search_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let deps = deps_with_cwd(dir.path().to_path_buf());
    // Catch the thrown rejection and surface its message via transcript.
    let script = r#"
        import { FileSearch } from "frances:v1/tools/file_search";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const fs = new FileSearch();
        let msg;
        try {
            await fs.search({ paths: [] });
            msg = "no-throw";
        } catch (e) {
            msg = String((e && e.message) || e);
        }
        transcript.push(new MarkdownFrame({ content: msg }));
    "#;
    let frames = run_script(deps, script).await;
    let msg = text_of(&frames[0]);
    assert!(
        msg.contains("at least one of"),
        "expected blunt error, got: {msg}"
    );
}

#[tokio::test]
async fn search_finds_match_with_line_number_and_skips_binary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("blob.bin"), vec![0u8; 1024]).unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script(r#"{ search: "println" }"#)).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "want only a.rs, got {entries:?}");
    let e = &entries[0];
    assert_eq!(e["path"], "a.rs");
    assert_eq!(e["binary"], false);
    assert_eq!(e["match_count"], 1);
    assert_eq!(e["first_match"]["line"], 2);
    assert!(
        e["first_match"]["text"]
            .as_str()
            .unwrap()
            .contains("println")
    );
}

#[tokio::test]
async fn paths_only_keeps_match_count_drops_first_match() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "x\nx\nx\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(
        deps,
        &dump_raw_script(r#"{ search: "x", paths_only: true }"#),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let e = &v["entries"].as_array().unwrap()[0];
    assert_eq!(e["match_count"], 3);
    assert!(
        e.get("first_match").is_none(),
        "first_match should be omitted under paths_only"
    );
}

#[tokio::test]
async fn binary_flag_set_when_no_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blob.bin"), vec![0u8; 16]).unwrap();
    std::fs::write(dir.path().join("text.txt"), "hello").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script("{}")).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let mut by_path: std::collections::HashMap<String, bool> = Default::default();
    for e in v["entries"].as_array().unwrap() {
        by_path.insert(
            e["path"].as_str().unwrap().to_string(),
            e["binary"].as_bool().unwrap(),
        );
    }
    assert_eq!(by_path.get("blob.bin"), Some(&true));
    assert_eq!(by_path.get("text.txt"), Some(&false));
}

#[tokio::test]
async fn json_repair_unwraps_double_encoded_paths() {
    // Pin the qwen3-coder quirk: `paths` arrives as a JSON-encoded
    // string of an array. Passing through the JS surface, JSON.stringify
    // → JSON.parse round-trip preserves the string-shape, and Rust-side
    // JsonRepair unwraps it before deserialising.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "").unwrap();
    std::fs::write(dir.path().join("b.txt"), "").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script(r#"{ paths: "[\"**/*.rs\"]" }"#)).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["a.rs"],
        "double-encoded paths arg not unwrapped"
    );
}

#[tokio::test]
async fn exclude_negates_a_glob() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("vendor")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn x(){}").unwrap();
    std::fs::write(dir.path().join("vendor/b.rs"), "fn y(){}").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(
        deps,
        &dump_raw_script(r#"{ paths: ["**/*.rs"], exclude: ["vendor/**"] }"#),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.iter().any(|p| p.ends_with("a.rs")));
    assert!(!paths.iter().any(|p| p.contains("vendor/")));
}

#[tokio::test]
async fn results_sort_alphabetically() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("zebra.txt"), "").unwrap();
    std::fs::write(dir.path().join("apple.txt"), "").unwrap();
    std::fs::write(dir.path().join("mango.txt"), "").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script("{}")).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["apple.txt", "mango.txt", "zebra.txt"]);
}

#[tokio::test]
async fn search_tool_into_stores_in_variables_and_returns_summary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "hello again\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let script = r#"
        import { FileSearch, Search } from "frances:v1/tools/file_search";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const fs = new FileSearch();
        const vars = new Variables();
        const tool = new Search(fs, vars);
        const r = await tool.handler({
            call: { id: "c1", name: "file_search",
                    arguments: { search: "hello", into: "hits" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));
        const stashed = vars.get("hits");
        transcript.push(new MarkdownFrame({ content: JSON.stringify(stashed) }));
    "#;
    let frames = run_script(deps, script).await;

    let tool_result = text_of(&frames[0]);
    assert!(
        tool_result.contains(r#""is_error":false"#),
        "tool result: {tool_result}"
    );
    assert!(
        tool_result.contains("hits = 2 entries"),
        "tool result missing summary header: {tool_result}"
    );
    // Inline preview should mention each path.
    assert!(tool_result.contains("a.txt"), "tool result: {tool_result}");
    assert!(tool_result.contains("b.txt"), "tool result: {tool_result}");

    let stashed = text_of(&frames[1]);
    let v: serde_json::Value = serde_json::from_str(&stashed).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn search_tool_without_into_returns_compact_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let script = r#"
        import { FileSearch, Search } from "frances:v1/tools/file_search";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownFrame } from "frances:v1/frames";
        const tool = new Search(new FileSearch(), new Variables());
        const r = await tool.handler({
            call: { id: "c1", name: "file_search",
                    arguments: { search: "hello" } },
            scope: null,
        });
        transcript.push(new MarkdownFrame({ content: JSON.stringify(r) }));
    "#;
    let frames = run_script(deps, script).await;
    let tool_result = text_of(&frames[0]);
    assert!(tool_result.contains(r#""is_error":false"#));
    // Inline format: `path:line:text  (N matches)`
    assert!(
        tool_result.contains("a.txt:1:hello"),
        "expected inline match line, got: {tool_result}"
    );
}
