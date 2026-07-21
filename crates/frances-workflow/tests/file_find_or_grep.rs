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
async fn huge_matching_line_returns_bounded_match_centered_excerpt() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = vec![b'a'; 10 * 1024 * 1024];
    let needle = b"UNIQUE_NEEDLE";
    let needle_at = body.len() / 2;
    body[needle_at..needle_at + needle.len()].copy_from_slice(needle);
    std::fs::write(dir.path().join("minified.js"), &body).unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script(r#"{ search: "UNIQUE_NEEDLE" }"#)).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let first = &v["entries"][0]["first_match"];
    let text = first["text"].as_str().unwrap();

    assert!(text.contains("UNIQUE_NEEDLE"), "excerpt lost match: {text}");
    assert!(text.len() <= 512, "excerpt was {} bytes", text.len());
    assert_eq!(first["text_truncated"], true);
    assert_eq!(first["line_bytes"], body.len());
    assert!(
        text.starts_with('…') && text.ends_with('…'),
        "middle excerpt should mark both omitted sides: {text}"
    );
}

#[tokio::test]
async fn matching_excerpt_handles_utf8_at_slice_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("q{}UNICODE_NEEDLE{}", "é".repeat(300), "界".repeat(300));
    std::fs::write(dir.path().join("unicode.txt"), &body).unwrap();

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let frames = run_script(deps, &dump_raw_script(r#"{ search: "UNICODE_NEEDLE" }"#)).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let text = v["entries"][0]["first_match"]["text"].as_str().unwrap();

    assert!(
        text.contains("UNICODE_NEEDLE"),
        "excerpt lost match: {text}"
    );
    assert!(text.len() <= 512, "excerpt was {} bytes", text.len());
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
async fn search_tool_caps_aggregate_inline_output() {
    let dir = tempfile::tempdir().unwrap();
    // Unicode keeps the character count well below the byte count. This
    // catches accidentally enforcing a UTF-16-code-unit limit in JS while
    // claiming the budget is bytes.
    let body = format!("needle {}\n", "界".repeat(170));
    for i in 0..40 {
        std::fs::write(dir.path().join(format!("match-{i:02}.txt")), &body).unwrap();
    }

    let deps = deps_with_cwd(dir.path().to_path_buf());
    let script = r#"
        import { FileSearch, Search } from "frances:v1/tools/file_find_or_grep";
        import { Editor } from "frances:v1/tools/file";
        import { Variables } from "frances:v1/tools/variable";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        const tool = new Search(new FileSearch(new Editor()), new Variables());
        const r = await tool.handler({
            call: { id: "c1", name: "file_find_or_grep",
                    arguments: { search: "needle" } },
            scope: null,
        });
        transcript.push(new MarkdownSection({ content: JSON.stringify(r) }));
    "#;
    let frames = run_script(deps, script).await;
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let content = result["content"].as_str().unwrap();

    assert!(
        content.len() <= 16 * 1024,
        "output was {} bytes",
        content.len()
    );
    assert!(
        content.contains("entries omitted from this response"),
        "missing explicit output-limit notice: {content}"
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

// ── root parameter integration tests ──────────────────────────────

#[tokio::test]
async fn root_external_searches_outside_cwd() {
    // Create an external directory with files — separate from cwd
    let external = tempfile::tempdir().unwrap();
    std::fs::write(external.path().join("external.txt"), "data").unwrap();
    std::fs::create_dir_all(external.path().join("src")).unwrap();
    std::fs::write(external.path().join("src/lib.rs"), "fn lib() {}").unwrap();

    // cwd is an empty tempdir — proves the walk is rooted at external, not cwd
    let cwd_dir = tempfile::tempdir().unwrap();
    let deps = deps_with_cwd(cwd_dir.path().to_path_buf());
    let root_arg = format!("\"{}\"", external.path().display());
    let frames = run_script(
        deps,
        &dump_raw_script(&format!("{{ root: {root_arg}, paths: [\"**/*.rs\"] }}")),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("lib.rs")),
        "src/lib.rs not found under external root: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains("external.txt")),
        "non-matching files should be excluded by paths filter: {paths:?}"
    );
}

#[tokio::test]
async fn root_hidden_directory_works_without_hidden_flag() {
    // A hidden-named directory as root should still be entered at depth 0
    let parent = tempfile::tempdir().unwrap();
    let hidden_root = parent.path().join(".hidden_project");
    std::fs::create_dir_all(&hidden_root).unwrap();
    std::fs::write(hidden_root.join("visible.txt"), "data").unwrap();

    let cwd_dir = tempfile::tempdir().unwrap();
    let deps = deps_with_cwd(cwd_dir.path().to_path_buf());
    let root_arg = format!("\"{}\"", hidden_root.display());
    let frames = run_script(deps, &dump_raw_script(&format!("{{ root: {root_arg} }}"))).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"visible.txt"),
        "visible.txt missing from hidden root: {paths:?}"
    );
}

#[tokio::test]
async fn root_hidden_children_filtered_by_default() {
    let external = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(external.path().join(".secrets")).unwrap();
    std::fs::write(external.path().join(".secrets/key.pem"), "secret").unwrap();
    std::fs::write(external.path().join("readme.md"), "hello").unwrap();

    let cwd_dir = tempfile::tempdir().unwrap();
    let deps = deps_with_cwd(cwd_dir.path().to_path_buf());
    let root_arg = format!("\"{}\"", external.path().display());

    // Default: hidden children are filtered out
    let frames = run_script(deps, &dump_raw_script(&format!("{{ root: {root_arg} }}"))).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"readme.md"), "readme.md missing: {paths:?}");
    assert!(
        !paths.iter().any(|p| p.contains("key.pem")),
        "hidden child leaked through default filter: {paths:?}"
    );
}

#[tokio::test]
async fn root_gitignore_respected_in_external_tree() {
    let external = tempfile::tempdir().unwrap();
    std::fs::write(external.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(external.path().join("ignored.txt"), "secret").unwrap();
    std::fs::write(external.path().join("kept.txt"), "public").unwrap();

    let cwd_dir = tempfile::tempdir().unwrap();
    let deps = deps_with_cwd(cwd_dir.path().to_path_buf());
    let root_arg = format!("\"{}\"", external.path().display());
    let frames = run_script(deps, &dump_raw_script(&format!("{{ root: {root_arg} }}"))).await;
    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"kept.txt"), "kept.txt missing: {paths:?}");
    assert!(
        !paths.contains(&"ignored.txt"),
        "gitignored file leaked: {paths:?}"
    );
}

#[tokio::test]
async fn root_tilde_expansion() {
    // Create a temp directory, set HOME to it, then use ~/... as root
    let orig_home = std::env::var("HOME").ok();
    let home_tmp = tempfile::tempdir().unwrap();
    // SAFETY: test-only; we restore HOME below. Single-threaded test
    // (--test-threads=1) avoids data races on the process env.
    unsafe {
        std::env::set_var("HOME", home_tmp.path());
    }

    let project = home_tmp.path().join("test_project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("hello.rs"), "fn main() {}").unwrap();

    let cwd_dir = tempfile::tempdir().unwrap();
    let deps = deps_with_cwd(cwd_dir.path().to_path_buf());
    let frames = run_script(
        deps,
        &dump_raw_script(r#"{ root: "~/test_project", paths: ["**/*.rs"] }"#),
    )
    .await;

    // Restore HOME
    // SAFETY: restoring the original HOME; see above.
    match &orig_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }

    let v: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"hello.rs"),
        "hello.rs missing with tilde root: {paths:?}"
    );
}
