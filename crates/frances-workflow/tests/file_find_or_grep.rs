//! Integration tests for the `frances:v1/tools/file_find_or_grep`
//! `FileSearch` primitive and the LLM-facing `Search` tool class. Each
//! test drives a JS script through the workflow runtime against a
//! `StubDeps` with a tempdir cwd, then asserts on what the script
//! pushed back via `MarkdownSection`.

use std::io::Write;
use std::path::PathBuf;

use frances_workflow::{
    Invocation, Runtime, SectionKind, SectionTranscript, test_deps::StubDeps,
    test_drive::drive_one_cycle,
};

fn write_source(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(".js")
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

fn text_of(frame: &SectionTranscript) -> String {
    match frame {
        SectionTranscript::Set { section: spec, .. } => match &spec.kind {
            SectionKind::Markdown { .. } | SectionKind::Error => {
                spec.seed.clone().unwrap_or_default()
            }
            SectionKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
            SectionKind::ShellOutput { state, cmd } => format!(
                "[shell:{state:?}] $ {cmd}
{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Reasoning { state } => format!(
                "[reasoning:{state:?}]\n{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
        },
        SectionTranscript::Append { delta, .. } => delta.clone(),
        SectionTranscript::Close { id } => format!("[close:{}]", id.0),
    }
}

fn deps_with_cwd(cwd: PathBuf) -> StubDeps {
    let deps = StubDeps::default();
    deps.set_cwd(cwd);
    deps
}

async fn run_script(deps: StubDeps, script: &str) -> Vec<SectionTranscript> {
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
/// payload via a MarkdownSection so tests can parse it.
const DUMP_RAW: &str = r#"
import { FileSearch } from "frances:v1/tools/file_find_or_grep";
import { Editor } from "frances:v1/tools/file";
import { transcript, MarkdownSection } from "frances:v1/sections";
const fs = new FileSearch(new Editor());
const json = await fs.search(ARGS);
transcript.push(new MarkdownSection({ content: json }));
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
    let script = r#"
        import { FileSearch } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const fs = new FileSearch(new Editor());
        let msg;
        try {
            await fs.search({ paths: [] });
            msg = "no-throw";
        } catch (e) {
            msg = String((e && e.message) || e);
        }
        transcript.push(new MarkdownSection({ content: msg }));
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
        import { FileSearch, Search } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const fs = new FileSearch(new Editor());
        const vars = new Variables();
        const tool = new Search(fs, vars);
        const r = await tool.handler({
            call: { id: "c1", name: "file_find_or_grep",
                    arguments: { search: "hello", into: "hits" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: JSON.stringify(r) }));
        const stashed = vars.get("hits");
        transcript.push(new MarkdownSection({ content: JSON.stringify(stashed) }));
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
        import { FileSearch, Search } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const tool = new Search(new FileSearch(new Editor()), new Variables());
        const r = await tool.handler({
            call: { id: "c1", name: "file_find_or_grep",
                    arguments: { search: "hello" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: JSON.stringify(r) }));
    "#;
    let frames = run_script(deps, script).await;
    let tool_result = text_of(&frames[0]);
    assert!(tool_result.contains(r#""is_error":false"#));
    assert!(
        tool_result.contains("a.txt:1:hello"),
        "expected inline match line, got: {tool_result}"
    );
}

#[tokio::test]
async fn loop_guard_blocks_identical_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();

    let deps = StubDeps::default();
    deps.set_cwd(dir.path().to_path_buf());
    let script = r#"
        import { FileSearch } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const fs = new FileSearch(new Editor());
        await fs.search({ search: "hello" });
        let caught = "no-throw";
        try {
            await fs.search({ search: "hello" });
        } catch (e) {
            caught = String((e && e.message) || e);
        }
        transcript.push(new MarkdownSection({ content: caught }));
    "#;
    let frames = run_script(deps, script).await;
    let msg = text_of(&frames[0]);
    assert!(
        msg.contains("loop guard"),
        "expected loop guard error, got: {msg}"
    );
}

#[tokio::test]
async fn loop_guard_search_clears_after_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();

    let deps = StubDeps::default();
    deps.set_cwd(dir.path().to_path_buf());
    let script = r#"
        import { FileSearch } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const editor = new Editor();
        const fs = new FileSearch(editor);
        await fs.search({ search: "hello" });
        const read = await editor.readFile("a.txt");
        const line_b = read.split("\n")[1];
        await editor.edit({
            kind: "ReplaceLines",
            path: "a.txt",
            anchor: line_b,
            end_anchor: line_b,
            text: "WORLD",
        });
        const second = await fs.search({ search: "hello" });
        transcript.push(new MarkdownSection({ content: second }));
    "#;
    let frames = run_script(deps, script).await;
    let payload = text_of(&frames[0]);
    assert!(
        payload.contains("a.txt"),
        "expected hit after clear, got: {payload}"
    );
}

#[tokio::test]
async fn loop_guard_search_distinguishes_query() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();

    let deps = StubDeps::default();
    deps.set_cwd(dir.path().to_path_buf());
    let script = r#"
        import { FileSearch } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const fs = new FileSearch(new Editor());
        await fs.search({ search: "hello" });
        const second = await fs.search({ search: "world" });
        transcript.push(new MarkdownSection({ content: second }));
    "#;
    let frames = run_script(deps, script).await;
    let payload = text_of(&frames[0]);
    assert!(payload.contains("a.txt"), "expected hit, got: {payload}");
}
