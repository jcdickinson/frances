//! File-touching tools: `file_read` plus the `file_*` edit family
//! (`file_replace`, `file_insert_after`, `file_insert_before`, `file_new`,
//! `file_overwrite`). One [`Tool`] impl exposes all six definitions and
//! routes by call name.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::Result;
use crate::edit_session::LlmEdit;
use crate::llm::{ToolCall, ToolDef, ToolFunction};
use crate::migrations::{EntitySchema, Migration};

/// Owns file_meta, file_lines, file_tombstones — the anchor edit
/// state that backs the `file_*` tools. UUID is permanent.
pub static SCHEMA: EntitySchema = EntitySchema {
    entity: Uuid::from_u128(0x97acb11c_b9a1_4f71_af62_0368f2ca9913),
    migrations: &[Migration {
        name: "0001_init.sql",
        sql: include_str!("file/migrations/0001_init.sql"),
    }],
};

use super::{
    Tool, ToolContext, ToolOutcome, ToolRegistryError, mtime_ns_from, resolve_path, split_lines,
};

const FILE_READ_DESC: &str = include_str!("desc/file_read.md");
const FILE_REPLACE_DESC: &str = include_str!("desc/file_replace.md");
const FILE_INSERT_AFTER_DESC: &str = include_str!("desc/file_insert_after.md");
const FILE_INSERT_BEFORE_DESC: &str = include_str!("desc/file_insert_before.md");
const FILE_NEW_DESC: &str = include_str!("desc/file_new.md");
const FILE_OVERWRITE_DESC: &str = include_str!("desc/file_overwrite.md");

#[derive(Debug, Error)]
pub enum FileToolError {
    #[error("unknown file tool: {0}")]
    UnknownTool(String),
    #[error("parse {tool} args: {source}")]
    ParseArgs {
        tool: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("read {path}: {source}")]
    ReadDisk {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("stat {path}: {source}")]
    Stat {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("write {path}: {source}")]
    WriteDisk {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("read-back {path}: {source}")]
    ReadBack {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Mtime(#[from] ToolRegistryError),
}

pub struct FileTools;

#[async_trait]
impl Tool for FileTools {
    async fn definitions(&self) -> Result<Vec<ToolDef>> {
        Ok(vec![
            ToolDef::Function(ToolFunction {
                name: "file_read".to_string(),
                description: FILE_READ_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "file_replace".to_string(),
                description: FILE_REPLACE_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "anchor": { "type": "string" },
                        "end_anchor": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["path", "anchor", "end_anchor", "text"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "file_insert_after".to_string(),
                description: FILE_INSERT_AFTER_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "anchor": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["path", "anchor", "text"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "file_insert_before".to_string(),
                description: FILE_INSERT_BEFORE_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "anchor": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["path", "anchor", "text"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "file_new".to_string(),
                description: FILE_NEW_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["path", "text"]
                }),
            }),
            ToolDef::Function(ToolFunction {
                name: "file_overwrite".to_string(),
                description: FILE_OVERWRITE_DESC.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["path", "text"]
                }),
            }),
        ])
    }

    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext<'_>) -> ToolOutcome {
        if call.name == "file_read" {
            return ToolOutcome::from_result(run_read(&call.arguments, ctx).await);
        }
        let edit = match parse_edit(&call.name, &call.arguments, ctx.cwd) {
            Ok(edit) => edit,
            Err(error) => return ToolOutcome::err(format!("{error}")),
        };
        ToolOutcome::from_result(apply_edit(ctx, edit).await)
    }
}

#[derive(serde::Deserialize)]
struct ReadArgs {
    path: PathBuf,
}

async fn run_read(args: &Value, ctx: &ToolContext<'_>) -> Result<String> {
    let args: ReadArgs =
        serde_json::from_value(args.clone()).map_err(|source| FileToolError::ParseArgs {
            tool: "file_read",
            source,
        })?;
    let path = resolve_path(ctx.cwd, &args.path);
    let (lines, mtime_ns, size) = read_file_from_disk(&path)?;

    let mut session = ctx.edit_session.lock().await;
    session.read_file(path, lines, mtime_ns, size).await
}

fn read_file_from_disk(path: &Path) -> std::result::Result<(Vec<String>, i64, u64), FileToolError> {
    let content = fs::read_to_string(path).map_err(|source| FileToolError::ReadDisk {
        path: path.to_path_buf(),
        source,
    })?;
    let meta = fs::metadata(path).map_err(|source| FileToolError::Stat {
        path: path.to_path_buf(),
        source,
    })?;
    let mtime_ns = mtime_ns_from(&meta)?;
    let size = meta.len();
    let lines = split_lines(&content);
    Ok((lines, mtime_ns, size))
}

#[derive(serde::Deserialize)]
struct ReplaceArgs {
    path: PathBuf,
    anchor: String,
    end_anchor: String,
    text: String,
}

#[derive(serde::Deserialize)]
struct InsertArgs {
    path: PathBuf,
    anchor: String,
    text: String,
}

#[derive(serde::Deserialize)]
struct WholeFileArgs {
    path: PathBuf,
    text: String,
}

fn parse_edit(name: &str, args: &Value, cwd: Option<&Path>) -> Result<LlmEdit> {
    fn parse(tool: &'static str) -> impl FnOnce(serde_json::Error) -> FileToolError {
        move |source| FileToolError::ParseArgs { tool, source }
    }
    match name {
        "file_replace" => {
            let a: ReplaceArgs =
                serde_json::from_value(args.clone()).map_err(parse("file_replace"))?;
            Ok(LlmEdit::Replace {
                path: resolve_path(cwd, &a.path),
                anchor: a.anchor,
                end_anchor: a.end_anchor,
                text: a.text,
            })
        }
        "file_insert_after" => {
            let a: InsertArgs =
                serde_json::from_value(args.clone()).map_err(parse("file_insert_after"))?;
            Ok(LlmEdit::InsertAfter {
                path: resolve_path(cwd, &a.path),
                anchor: a.anchor,
                text: a.text,
            })
        }
        "file_insert_before" => {
            let a: InsertArgs =
                serde_json::from_value(args.clone()).map_err(parse("file_insert_before"))?;
            Ok(LlmEdit::InsertBefore {
                path: resolve_path(cwd, &a.path),
                anchor: a.anchor,
                text: a.text,
            })
        }
        "file_new" => {
            let a: WholeFileArgs =
                serde_json::from_value(args.clone()).map_err(parse("file_new"))?;
            Ok(LlmEdit::New {
                path: resolve_path(cwd, &a.path),
                text: a.text,
            })
        }
        "file_overwrite" => {
            let a: WholeFileArgs =
                serde_json::from_value(args.clone()).map_err(parse("file_overwrite"))?;
            Ok(LlmEdit::Overwrite {
                path: resolve_path(cwd, &a.path),
                text: a.text,
            })
        }
        other => Err(FileToolError::UnknownTool(other.to_string()).into()),
    }
}

async fn apply_edit(ctx: &ToolContext<'_>, edit: LlmEdit) -> Result<String> {
    let mut session = ctx.edit_session.lock().await;
    // Validator failures (`EditError`) flow back through the trait's
    // generic Display — that's exactly the wording designed for the model.
    // Other errors keep their wrapped chain.
    session.edit(edit, write_draft).await
}

fn write_draft(path: &Path, draft: &[String]) -> Result<(Vec<String>, i64, u64)> {
    let mut content = draft.join("\n");
    content.push('\n');
    fs::write(path, &content).map_err(|source| FileToolError::WriteDisk {
        path: path.to_path_buf(),
        source,
    })?;

    let meta = fs::metadata(path).map_err(|source| FileToolError::Stat {
        path: path.to_path_buf(),
        source,
    })?;
    let mtime_ns = mtime_ns_from(&meta).map_err(FileToolError::Mtime)?;
    let size = meta.len();

    let post = fs::read_to_string(path).map_err(|source| FileToolError::ReadBack {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((split_lines(&post), mtime_ns, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit_session::EditSession;
    use frances_edit::{AnchorStore, EditEngine, FakeStore};
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    fn fresh() -> (Mutex<EditSession<FakeStore>>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let session = EditSession::new(EditEngine::new(FakeStore::new()));
        (Mutex::new(session), dir)
    }

    /// Mirrors `run_read`, but generic over any [`AnchorStore`] so we can
    /// wire the [`FakeStore`]-backed session in unit tests.
    async fn run_read_with<S: AnchorStore + Send + Sync>(
        session: &Mutex<EditSession<S>>,
        cwd: Option<&Path>,
        args: Value,
    ) -> ToolOutcome {
        let args: ReadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("{e}")),
        };
        let path = resolve_path(cwd, &args.path);
        let (lines, mtime_ns, size) = match read_file_from_disk(&path) {
            Ok(t) => t,
            Err(e) => return ToolOutcome::err(format!("file_read: {}: {e}", path.display())),
        };
        let mut sess = session.lock().await;
        match sess.read_file(path, lines, mtime_ns, size).await {
            Ok(content) => ToolOutcome::ok(content),
            Err(e) => ToolOutcome::err(format!("{e}")),
        }
    }

    /// Test helper: same edit-session path as production, generic over the
    /// store so we can use [`FakeStore`].
    async fn dispatch_edit<S: AnchorStore + Send + Sync>(
        session: &Mutex<EditSession<S>>,
        edit: LlmEdit,
    ) -> ToolOutcome {
        let mut sess = session.lock().await;
        match sess.edit(edit, write_draft).await {
            Ok(diff) => ToolOutcome::ok(diff),
            Err(crate::Error::Edit(edit_error)) => ToolOutcome::err(edit_error.to_string()),
            Err(error) => ToolOutcome::err(format!("{error}")),
        }
    }

    /// Reads a file via the same [`FakeStore`]-backed session as the edits.
    async fn seed_anchors<S: AnchorStore + Send + Sync>(
        session: &Mutex<EditSession<S>>,
        path: &Path,
    ) -> String {
        let content = fs::read_to_string(path).unwrap();
        let meta = fs::metadata(path).unwrap();
        let mtime_ns = mtime_ns_from(&meta).unwrap();
        let size = meta.len();
        let lines = split_lines(&content);
        let mut sess = session.lock().await;
        sess.read_file(path.to_path_buf(), lines, mtime_ns, size)
            .await
            .unwrap()
    }

    fn anchor_line(read_output: &str, idx: usize) -> String {
        read_output.lines().nth(idx).unwrap().to_string()
    }

    #[tokio::test]
    async fn read_happy_path_returns_anchored_lines() {
        let (session, dir) = fresh();
        let path = dir.path().join("hello.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let outcome =
            run_read_with(&session, None, json!({ "path": path.to_str().unwrap() })).await;
        assert!(!outcome.is_error, "unexpected error: {}", outcome.content);
        let lines: Vec<&str> = outcome.content.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.contains('§'), "line missing anchor: {line:?}");
        }
    }

    #[tokio::test]
    async fn read_missing_file_is_error() {
        let (session, dir) = fresh();
        let path = dir.path().join("does-not-exist");
        let outcome =
            run_read_with(&session, None, json!({ "path": path.to_str().unwrap() })).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("does-not-exist"));
    }

    #[tokio::test]
    async fn replace_happy_path() {
        let (session, dir) = fresh();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\nc\n").unwrap();

        let read = seed_anchors(&session, &path).await;
        let line_b = anchor_line(&read, 1);

        let outcome = dispatch_edit(
            &session,
            LlmEdit::Replace {
                path: path.clone(),
                anchor: line_b.clone(),
                end_anchor: line_b,
                text: "B2".into(),
            },
        )
        .await;
        assert!(!outcome.is_error, "unexpected error: {}", outcome.content);

        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "a\nB2\nc\n");
        assert!(outcome.content.contains("§B2"));
    }

    #[tokio::test]
    async fn anchor_not_found_is_error() {
        let (session, dir) = fresh();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\n").unwrap();

        seed_anchors(&session, &path).await;

        let outcome = dispatch_edit(
            &session,
            LlmEdit::InsertAfter {
                path: path.clone(),
                anchor: "Wizard§a".into(),
                text: "X".into(),
            },
        )
        .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("not found"));
    }

    #[tokio::test]
    async fn content_mismatch_is_error() {
        let (session, dir) = fresh();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\n").unwrap();

        let read = seed_anchors(&session, &path).await;
        let real_b = anchor_line(&read, 1);
        let word_b = real_b.split('§').next().unwrap();
        let wrong = format!("{word_b}§not the real content");

        let outcome = dispatch_edit(
            &session,
            LlmEdit::Replace {
                path: path.clone(),
                anchor: wrong.clone(),
                end_anchor: wrong,
                text: "x".into(),
            },
        )
        .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("content mismatch"));
    }
}
