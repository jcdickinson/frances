//! LLM-callable tools: `read_file` and `edit`. The dispatcher resolves
//! tool calls against an `EditSession` shared with the daemon, returning
//! plain-text content suitable to feed back to the model as a tool result.
//!
//! Tool descriptions intentionally teach the anchor protocol once — runtime
//! outputs stay pure data per `docs/arch/anchors.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use frances_core::JsonRepair;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::anchor_store::AnchorStoreImpl;
use crate::edit_session::{EditError, EditInput, EditSession};
use crate::llm::{ToolCall, ToolDef, ToolFunction};

pub struct ToolContext<'a> {
    pub edit_session: &'a Mutex<EditSession<AnchorStoreImpl>>,
    pub cwd: Option<&'a Path>,
}

pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

pub fn definitions() -> &'static [ToolDef] {
    static DEFS: OnceLock<Vec<ToolDef>> = OnceLock::new();
    DEFS.get_or_init(|| {
        vec![
            ToolDef::Function(ToolFunction {
                name: "read_file".to_string(),
                description: READ_FILE_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "edit".to_string(),
                description: EDIT_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" },
                                    "edits": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "edit_type": {
                                                    "type": "string",
                                                    "enum": ["replace", "insert_after", "insert_before"]
                                                },
                                                "anchor": { "type": "string" },
                                                "end_anchor": { "type": "string" },
                                                "text": { "type": "string" }
                                            },
                                            "required": ["edit_type", "anchor", "text"]
                                        }
                                    }
                                },
                                "required": ["path", "edits"]
                            }
                        }
                    },
                    "required": ["files"]
                }),
            }),
        ]
    })
}

const READ_FILE_DESC: &str = "\
Read a file from disk and render it with line anchors. Each line is rendered as `Word§content` — a stable per-line anchor word (e.g. `Apple`, `BananaCarrot`), then `§`, then the line's content. Blank lines render as `Word§` with empty content. Anchors survive external edits and formatter runs. The rendered string of each line is exactly what you pass back as the `anchor` (and `end_anchor`) field of an `edit` call. Always call `read_file` for a path before calling `edit` on it — edit requires the file to be cached this turn. The path may be absolute or relative to the client's working directory.";

const EDIT_DESC: &str = "\
Edit one or more files by replacing, inserting after, or inserting before specific anchored lines. You must call `read_file` on each file first this turn so its anchors are cached.

Top-level shape — `files` is an ARRAY of file objects, NOT a string. Each file has an `edits` ARRAY:

{
  \"files\": [
    {
      \"path\": \"src/example.rs\",
      \"edits\": [
        { \"edit_type\": \"replace\", \"anchor\": \"...\", \"end_anchor\": \"...\", \"text\": \"...\" },
        { \"edit_type\": \"insert_after\", \"anchor\": \"...\", \"text\": \"...\" }
      ]
    }
  ]
}

Do NOT JSON-encode `files` or `edits` as a string. They are inline JSON arrays.

Per-edit fields:
  edit_type:  one of \"replace\", \"insert_after\", \"insert_before\"
  anchor:     full anchor line as `read_file` rendered it — \"Word§content\"
  end_anchor: only for replace; the rendered anchor line of the LAST line in the inclusive range
  text:       the new content. Use \\n for newlines. Multi-line is fine; do NOT include any anchors in text.

The anchor word must match a line in the latest `read_file` output for that path. The content after § must match the line's content (trimmed comparison). On mismatch, re-read the file and use the latest anchors.

Behaviour:
  replace        — replaces all lines from `anchor` through `end_anchor` (inclusive) with `text`.
  insert_after   — inserts `text` immediately after `anchor`.
  insert_before  — inserts `text` immediately before `anchor`.

Edits within a single call must not touch overlapping line ranges in the same file. If they do the call is rejected — split overlapping work into separate calls.

WORKED EXAMPLE. Suppose read_file on src/greet.py returned:

  Apple§def hello():
  Banana§    print(\"hi\")
  Cherry§
  Daisy§def goodbye():

To replace the print with two prints AND add a docstring before goodbye, the WHOLE tool call body is:

{
  \"files\": [
    {
      \"path\": \"src/greet.py\",
      \"edits\": [
        {
          \"edit_type\": \"replace\",
          \"anchor\":     \"Banana§    print(\\\"hi\\\")\",
          \"end_anchor\": \"Banana§    print(\\\"hi\\\")\",
          \"text\":       \"    print(\\\"hi there\\\")\\n    print(\\\"welcome\\\")\"
        },
        {
          \"edit_type\": \"insert_before\",
          \"anchor\": \"Daisy§def goodbye():\",
          \"text\":   \"# Says goodbye.\"
        }
      ]
    }
  ]
}

Returns one diff block per file with the new anchors for inserted lines.";

pub async fn dispatch(call: &ToolCall, ctx: ToolContext<'_>) -> ToolOutcome {
    let result = match call.name.as_str() {
        "read_file" => run_read_file(&call.arguments, &ctx).await,
        "edit" => run_edit(&call.arguments, &ctx).await,
        other => Err(anyhow!("unknown tool: {other}")),
    };

    match result {
        Ok(content) => ToolOutcome {
            content,
            is_error: false,
        },
        Err(error) => ToolOutcome {
            content: format!("{error:#}"),
            is_error: true,
        },
    }
}

#[derive(serde::Deserialize)]
struct ReadArgs {
    path: PathBuf,
}

async fn run_read_file(args: &Value, ctx: &ToolContext<'_>) -> Result<String> {
    let args: ReadArgs = serde_json::from_value(args.clone()).context("parse read_file args")?;
    let path = resolve_path(ctx.cwd, &args.path);

    let (lines, mtime_ns, size) =
        read_file_from_disk(&path).with_context(|| format!("read_file: {}", path.display()))?;

    let mut session = ctx.edit_session.lock().await;
    session
        .read_file(path, lines, mtime_ns, size)
        .await
        .context("read_file")
}

async fn run_edit(args: &Value, ctx: &ToolContext<'_>) -> Result<String> {
    // `JsonRepair` absorbs the qwen3-coder family quirk where array fields
    // (`files`, per-entry `edits`) arrive as JSON-encoded strings. On a clean
    // input the strict path runs and we're a passthrough.
    let raw = JsonRepair::<EditInput>::from_value(args.clone())
        .context("parse edit args")?
        .into_inner();

    let resolved_files = raw
        .files
        .into_iter()
        .map(|entry| crate::edit_session::EditFileEntry {
            path: resolve_path(ctx.cwd, &entry.path),
            edits: entry.edits,
        })
        .collect::<Vec<_>>();
    let input = EditInput {
        files: resolved_files,
    };

    let mut session = ctx.edit_session.lock().await;
    let outcome = session.edit(input, write_draft).await;

    match outcome {
        Ok(diff) => Ok(diff),
        Err(error) => {
            // Surface validator errors via their bespoke Display so the model
            // sees the same wording the type designed for it ("anchor not
            // found", "content mismatch", etc.) rather than anyhow's chain.
            if let Some(edit_error) = error.downcast_ref::<EditError>() {
                return Err(anyhow!(edit_error.to_string()));
            }
            Err(error)
        }
    }
}

fn resolve_path(cwd: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn read_file_from_disk(path: &Path) -> Result<(Vec<String>, i64, u64)> {
    let content = fs::read_to_string(path).context("read")?;
    let meta = fs::metadata(path).context("stat")?;
    let mtime_ns = mtime_ns_from(&meta)?;
    let size = meta.len();
    let lines = split_lines(&content);
    Ok((lines, mtime_ns, size))
}

fn write_draft(path: &Path, draft: &[String]) -> Result<(Vec<String>, i64, u64)> {
    let mut content = draft.join("\n");
    content.push('\n');
    fs::write(path, &content).with_context(|| format!("write {}", path.display()))?;

    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime_ns = mtime_ns_from(&meta)?;
    let size = meta.len();

    let post = fs::read_to_string(path).with_context(|| format!("read-back {}", path.display()))?;
    Ok((split_lines(&post), mtime_ns, size))
}

fn split_lines(s: &str) -> Vec<String> {
    s.split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .take_until_trailing_blank()
}

trait TakeUntilTrailingBlank {
    fn take_until_trailing_blank(self) -> Self;
}

impl TakeUntilTrailingBlank for Vec<String> {
    /// `"a\nb\n".split('\n')` yields `["a", "b", ""]`. The trailing empty
    /// element represents the final newline, not an actual blank line on disk.
    /// Strip it so the line count matches what's visible. A genuinely blank
    /// final line (file ending in `\n\n`) yields `["a", "b", "", ""]` — we
    /// strip only the last one, preserving the real blank.
    fn take_until_trailing_blank(mut self) -> Self {
        if self.last().map(String::is_empty).unwrap_or(false) {
            self.pop();
        }
        self
    }
}

fn mtime_ns_from(meta: &fs::Metadata) -> Result<i64> {
    let modified = meta.modified().context("modified time")?;
    let dur = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("modified before unix epoch")?;
    i64::try_from(dur.as_nanos()).context("mtime overflow")
}

pub fn assistant_payload(text: &str, tool_calls: &[ToolCall]) -> Value {
    let content = if text.is_empty() {
        Value::Null
    } else {
        Value::String(text.to_string())
    };
    let mut payload = json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                    }
                })
            })
            .collect();
        payload
            .as_object_mut()
            .expect("payload is object")
            .insert("tool_calls".to_string(), Value::Array(calls));
    }
    payload
}

pub fn tool_result_payload(tool_use_id: &str, content: &str) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_use_id,
        "content": content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit_session::EditFileEntry;
    use frances_edit::EditEngine;
    use tempfile::TempDir;

    fn fresh_ctx() -> (Mutex<EditSession<frances_edit::FakeStore>>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let session = EditSession::new(EditEngine::new(frances_edit::FakeStore::new()));
        (Mutex::new(session), dir)
    }

    /// dispatch() is generic over any AnchorStore via ToolContext, but the
    /// production type is AnchorStoreImpl. For tests we wire FakeStore in
    /// the same shape and exercise the dispatch logic directly via the
    /// run_* helpers (which take a Mutex<EditSession<S>>). To do that we
    /// inline a copy of the helpers' bodies, keeping the test scope tight.
    /// In a follow-up the dispatch layer could be parameterised over S;
    /// for now, the unit coverage here checks the parsing + I/O glue and
    /// the integration tests in edit_session.rs cover the engine path.
    async fn dispatch_read_file<S: frances_edit::AnchorStore + Send + Sync>(
        session: &Mutex<EditSession<S>>,
        cwd: Option<&Path>,
        args: Value,
    ) -> ToolOutcome {
        let args: ReadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome {
                    content: format!("{e:#}"),
                    is_error: true,
                };
            }
        };
        let path = resolve_path(cwd, &args.path);
        let (lines, mtime_ns, size) = match read_file_from_disk(&path) {
            Ok(t) => t,
            Err(e) => {
                return ToolOutcome {
                    content: format!("read_file: {}: {e:#}", path.display()),
                    is_error: true,
                };
            }
        };
        let mut sess = session.lock().await;
        match sess.read_file(path, lines, mtime_ns, size).await {
            Ok(content) => ToolOutcome {
                content,
                is_error: false,
            },
            Err(e) => ToolOutcome {
                content: format!("{e:#}"),
                is_error: true,
            },
        }
    }

    async fn dispatch_edit<S: frances_edit::AnchorStore + Send + Sync>(
        session: &Mutex<EditSession<S>>,
        cwd: Option<&Path>,
        args: Value,
    ) -> ToolOutcome {
        let raw = match JsonRepair::<EditInput>::from_value(args) {
            Ok(i) => i.into_inner(),
            Err(e) => {
                return ToolOutcome {
                    content: format!("{e:#}"),
                    is_error: true,
                };
            }
        };
        let input = EditInput {
            files: raw
                .files
                .into_iter()
                .map(|f| EditFileEntry {
                    path: resolve_path(cwd, &f.path),
                    edits: f.edits,
                })
                .collect(),
        };
        let mut sess = session.lock().await;
        match sess.edit(input, write_draft).await {
            Ok(diff) => ToolOutcome {
                content: diff,
                is_error: false,
            },
            Err(error) => {
                if let Some(edit_error) = error.downcast_ref::<EditError>() {
                    ToolOutcome {
                        content: edit_error.to_string(),
                        is_error: true,
                    }
                } else {
                    ToolOutcome {
                        content: format!("{error:#}"),
                        is_error: true,
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn read_file_happy_path_returns_anchored_lines() {
        let (session, dir) = fresh_ctx();
        let path = dir.path().join("hello.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let outcome =
            dispatch_read_file(&session, None, json!({ "path": path.to_str().unwrap() })).await;
        assert!(!outcome.is_error, "unexpected error: {}", outcome.content);
        let lines: Vec<&str> = outcome.content.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.contains('§'), "line missing anchor: {line:?}");
        }
    }

    #[tokio::test]
    async fn read_file_missing_file_is_error() {
        let (session, dir) = fresh_ctx();
        let path = dir.path().join("does-not-exist");
        let outcome =
            dispatch_read_file(&session, None, json!({ "path": path.to_str().unwrap() })).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("does-not-exist"));
    }

    /// Pulls the rendered anchor line for `idx` straight out of read_file
    /// output. This is exactly what the model gets and exactly what it must
    /// pass back as `anchor` / `end_anchor`.
    fn anchor_line(read_output: &str, idx: usize) -> String {
        read_output.lines().nth(idx).unwrap().to_string()
    }

    #[tokio::test]
    async fn edit_replace_happy_path() {
        let (session, dir) = fresh_ctx();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\nc\n").unwrap();

        let read = dispatch_read_file(&session, None, json!({ "path": path.to_str().unwrap() }))
            .await
            .content;
        let line_b = anchor_line(&read, 1);

        let outcome = dispatch_edit(
            &session,
            None,
            json!({
                "files": [{
                    "path": path.to_str().unwrap(),
                    "edits": [{
                        "edit_type": "replace",
                        "anchor": line_b,
                        "end_anchor": line_b,
                        "text": "B2"
                    }]
                }]
            }),
        )
        .await;
        assert!(!outcome.is_error, "unexpected error: {}", outcome.content);

        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "a\nB2\nc\n");
        assert!(outcome.content.contains("§B2"));
    }

    #[tokio::test]
    async fn edit_anchor_not_found_is_error() {
        let (session, dir) = fresh_ctx();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\n").unwrap();

        dispatch_read_file(&session, None, json!({ "path": path.to_str().unwrap() })).await;

        let outcome = dispatch_edit(
            &session,
            None,
            json!({
                "files": [{
                    "path": path.to_str().unwrap(),
                    "edits": [{
                        "edit_type": "insert_after",
                        "anchor": "Wizard§a",
                        "text": "X"
                    }]
                }]
            }),
        )
        .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("not found"));
    }

    /// Exercises the qwen3-coder family quirk: the model emits `files` (and
    /// each entry's `edits`) as a JSON-encoded string instead of an inline
    /// array. `JsonRepair` in `run_edit` should unwrap both layers and the
    /// edit should apply normally.
    #[tokio::test]
    async fn edit_with_stringified_files_and_edits_succeeds() {
        let (session, dir) = fresh_ctx();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\nc\n").unwrap();

        let read = dispatch_read_file(&session, None, json!({ "path": path.to_str().unwrap() }))
            .await
            .content;
        let line_b = anchor_line(&read, 1);

        let edits_str = serde_json::to_string(&json!([{
            "edit_type": "replace",
            "anchor": line_b,
            "end_anchor": line_b,
            "text": "B2"
        }]))
        .unwrap();
        let files_str = serde_json::to_string(&json!([{
            "path": path.to_str().unwrap(),
            "edits": edits_str
        }]))
        .unwrap();

        let outcome = dispatch_edit(&session, None, json!({ "files": files_str })).await;
        assert!(!outcome.is_error, "unexpected error: {}", outcome.content);

        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nB2\nc\n");
    }

    #[tokio::test]
    async fn edit_content_mismatch_is_error() {
        let (session, dir) = fresh_ctx();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\n").unwrap();

        let read = dispatch_read_file(&session, None, json!({ "path": path.to_str().unwrap() }))
            .await
            .content;
        // Pull the real anchor word, then attach the wrong content to it.
        let real_b = anchor_line(&read, 1);
        let word_b = real_b.split('§').next().unwrap();
        let wrong = format!("{word_b}§not the real content");

        let outcome = dispatch_edit(
            &session,
            None,
            json!({
                "files": [{
                    "path": path.to_str().unwrap(),
                    "edits": [{
                        "edit_type": "replace",
                        "anchor": wrong,
                        "end_anchor": wrong,
                        "text": "x"
                    }]
                }]
            }),
        )
        .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("content mismatch"));
    }

    #[test]
    fn assistant_payload_with_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({ "path": "x.rs" }),
        }];
        let payload = assistant_payload("hello", &calls);
        assert_eq!(payload["role"], "assistant");
        assert_eq!(payload["content"], "hello");
        let tcs = payload["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "call_1");
        assert_eq!(tcs[0]["type"], "function");
        assert_eq!(tcs[0]["function"]["name"], "read_file");
        let args_str = tcs[0]["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed, json!({ "path": "x.rs" }));
    }

    #[test]
    fn assistant_payload_empty_text_yields_null_content() {
        let payload = assistant_payload("", &[]);
        assert_eq!(payload["role"], "assistant");
        assert!(payload["content"].is_null());
        assert!(payload.get("tool_calls").is_none());
    }

    #[test]
    fn tool_result_payload_uses_openai_field_name() {
        let payload = tool_result_payload("call_1", "anchored content");
        assert_eq!(payload["role"], "tool");
        assert_eq!(payload["tool_call_id"], "call_1");
        assert_eq!(payload["content"], "anchored content");
    }

    #[test]
    fn split_lines_strips_trailing_newline_only() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb\n\n"), vec!["a", "b", ""]);
        assert_eq!(split_lines(""), Vec::<String>::new());
    }
}
