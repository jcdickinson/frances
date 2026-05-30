//! `frances:v1/tools/file` — anchor-aware file editor primitive.
//!
//! `new Editor()` represents the session-scoped editor. Each
//! construction returns a handle backed by the *same* `EditSession`
//! held by the runtime, so all reads/edits across a workflow share the
//! anchor cache. The Rust side owns I/O (disk read, write, stat) and
//! delegates anchor work to `frances_edit::EditSession`.
//!
//! Methods on the JS side:
//!
//! - `readFile(path)` — read the file, drift-reconcile against the
//!   cached anchor state, return the anchored render. Throws on disk
//!   error or unknown anchor.
//! - `edit(value)` — apply one structured edit. `value` is a tagged
//!   object: `{ kind: "ReplaceLines"|"ReplaceAll"|"InsertAfter"|"InsertBefore"|"New"|
//!   "Overwrite", path, ... }`. Returns the diff block (or full
//!   anchored file for `New`).
//!
//! Paths are resolved against the latest invocation's cwd
//! (`WorkflowDeps::current_cwd`) on every call, so a new invocation cwd
//! takes effect immediately.
//!
//! Writes (`New`, `Overwrite`, anchor edits) `create_dir_all` for the
//! parent — idempotent; in practice it only matters for `New` since the
//! other ops require a prior successful `readFile`.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};
use twox_hash::XxHash3_64;

use frances_core::resolve_relative;
use frances_edit::{DiffOp, DiffRender, EditError, LlmEdit, LoopKey};

use super::throw_js as throw;
use crate::deps::{EditorFactory, WorkflowDeps};
use crate::io::WorkflowFs;

pub(crate) fn build_editor_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<EditorJs<D>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> JsResult<Class<'js, EditorJs<D>>> {
            Class::instance(ctx.clone(), EditorJs { deps: deps.clone() })
        },
    )
}

/// Builds the `{ file_read, file_replace_lines, ... }` descriptions object
/// for the stash. JS doesn't have verbatim string literals, so we keep
/// the LLM-facing markdown next to the module under `desc/` and inline
/// it via `include_str!` instead of fighting backtick escaping in
/// template literals.
pub(crate) fn build_descriptions<'js>(ctx: &Ctx<'js>) -> JsResult<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("file_read", include_str!("desc/file_read.md"))?;
    obj.set(
        "file_replace_lines",
        include_str!("desc/file_replace_lines.md"),
    )?;
    obj.set("file_replace_all", include_str!("desc/file_replace_all.md"))?;
    obj.set(
        "file_insert_after",
        include_str!("desc/file_insert_after.md"),
    )?;
    obj.set(
        "file_insert_before",
        include_str!("desc/file_insert_before.md"),
    )?;
    obj.set("file_new", include_str!("desc/file_new.md"))?;
    obj.set("file_overwrite", include_str!("desc/file_overwrite.md"))?;
    Ok(obj)
}

pub struct EditorJs<D: WorkflowDeps> {
    deps: D,
}

impl<'js, D: WorkflowDeps> Trace<'js> for EditorJs<D> {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js, D: WorkflowDeps> JsLifetime<'js> for EditorJs<D> {
    type Changed<'to> = EditorJs<D>;
}

impl<'js, D: WorkflowDeps> JsClass<'js> for EditorJs<D> {
    const NAME: &'static str = "Editor";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "readFile",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, EditorJs<D>>>, value: Value<'js>| {
                    let raw = super::rquickjs_to_json(&value);
                    let deps = this.0.borrow().deps.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        let result = match raw {
                            Ok(v) => match serde_json::from_value::<ReadFileArgs>(v.clone()) {
                                Ok(args) => read_file_inner(&deps, args).await,
                                Err(_) => {
                                    // Fallback for when the args is just a string (the path) which was the old behavior
                                    if let Some(path_str) = v.as_str() {
                                        read_file_inner(
                                            &deps,
                                            ReadFileArgs {
                                                path: path_str.to_string(),
                                                ranges: None,
                                            },
                                        )
                                        .await
                                    } else {
                                        Err(FileToolError::DecodeArgs(
                                            "parse readFile args: invalid arg shape".to_owned(),
                                        ))
                                    }
                                }
                            },
                            Err(msg) => Err(FileToolError::DecodeArgs(msg)),
                        };
                        EditorStringResult(result)
                    }))
                },
            )?,
        )?;

        proto.set(
            "readRaw",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, EditorJs<D>>>, path: String| {
                    let deps = this.0.borrow().deps.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        EditorStringResult(read_raw_inner(&deps, path).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "edit",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, EditorJs<D>>>, value: Value<'js>| {
                    let raw = super::rquickjs_to_json(&value);
                    let deps = this.0.borrow().deps.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        let result = match raw {
                            Ok(v) => edit_inner(&deps, v).await,
                            Err(msg) => Err(FileToolError::DecodeArgs(msg)),
                        };
                        EditorEditResult(result)
                    }))
                },
            )?,
        )?;

        // Commit accumulated edits (clears anchor tombstones). The
        // workflow calls this at its own reconciliation boundary — the
        // host no longer fires it automatically.
        proto.set(
            "commit",
            Function::new(ctx.clone(), |this: This<Class<'js, EditorJs<D>>>| {
                let deps = this.0.borrow().deps.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    EditorUnitResult(commit_inner(&deps).await)
                }))
            })?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Disk-only read. Returns the file as-is, with no `EditSession`
/// interaction for anchors — so the path is *not* registered for
/// editing and the caller doesn't get anchors. Used by `Read` when the
/// LLM asks for the content to land in a Frances variable instead of
/// in tool-result text. Still consults the session's loop guard so a
/// `readRaw` immediately following an identical `readRaw` on an
/// unchanged file trips the same guard as `file_read`.
/// Failures from the editor bridge's `*_inner` ops. Kept typed all the way to
/// the `IntoJs` boundary, which renders it via `Display` into a JS exception.
#[derive(Debug, thiserror::Error)]
enum FileToolError {
    /// Arg-shape decode failure — carries the message produced upstream
    /// (`rquickjs_to_json`, or a bad readFile arg shape).
    #[error("{0}")]
    DecodeArgs(String),
    #[error("parse edit: {0}")]
    ParseEdit(#[source] serde_json::Error),
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error(
        "loop guard: this exact read was just performed and {path} has not \
         changed since. you already have the result. do something different \
         — change the path, the ranges, or the tool, or move on."
    )]
    Loop { path: String },
    #[error("reverse range [{start}, {end}]")]
    ReverseRange { start: usize, end: usize },
    #[error("ranges are 1-indexed, got start=0")]
    RangeStartZero,
}

async fn read_raw_inner<D: WorkflowDeps>(deps: &D, path: String) -> Result<String, FileToolError> {
    let resolved = resolve_relative(Path::new(&path), deps.current_cwd().as_deref());
    let fs = deps.fs();
    let (mtime_ns, size) = stat_file(fs, &resolved)
        .await
        .map_err(|source| FileToolError::Io {
            path: resolved.display().to_string(),
            source,
        })?;
    let key = LoopKey::Read {
        args_hash: hash_read_raw_args(&path),
        mtime_ns,
        size,
    };

    let session = deps.editor_factory().session();
    let mut sess = session.lock().await;
    if sess.is_loop(&key) {
        return Err(FileToolError::Loop { path });
    }

    let content = fs
        .read_to_string(&resolved)
        .await
        .map_err(|source| FileToolError::Io {
            path: resolved.display().to_string(),
            source,
        })?;
    sess.record_loop(key);
    Ok(content)
}

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    ranges: Option<Vec<[usize; 2]>>,
}

async fn read_file_inner<D: WorkflowDeps>(
    deps: &D,
    args: ReadFileArgs,
) -> Result<String, FileToolError> {
    let resolved = resolve_relative(Path::new(&args.path), deps.current_cwd().as_deref());
    let fs = deps.fs();
    let (mtime_ns, size) = stat_file(fs, &resolved)
        .await
        .map_err(|source| FileToolError::Io {
            path: resolved.display().to_string(),
            source,
        })?;
    let key = LoopKey::Read {
        args_hash: hash_read_file_args(&args),
        mtime_ns,
        size,
    };

    let session: Arc<_> = deps.editor_factory().session();
    let mut sess = session.lock().await;
    if sess.is_loop(&key) {
        return Err(FileToolError::Loop { path: args.path });
    }

    let lines = read_file_lines(fs, &resolved)
        .await
        .map_err(|source| FileToolError::Io {
            path: resolved.display().to_string(),
            source,
        })?;
    let total_lines = lines.len();

    let full_rendered = sess.read_file(resolved, lines, mtime_ns, size).await?;
    sess.record_loop(key);

    if let Some(ranges) = args.ranges {
        // Validate each 1-indexed range and clamp its end to the file
        // length, then sort by start and merge overlapping/adjacent ones.
        let mut final_ranges = Vec::new();
        for [start, end] in ranges {
            if end < start {
                return Err(FileToolError::ReverseRange { start, end });
            }
            if start == 0 {
                return Err(FileToolError::RangeStartZero);
            }
            let actual_end = std::cmp::min(end, total_lines);
            if start > total_lines {
                continue; // completely out of bounds
            }
            final_ranges.push([start, actual_end]);
        }

        final_ranges.sort_unstable_by_key(|r| r[0]);
        let mut merged = Vec::new();
        for r in final_ranges {
            if merged.is_empty() {
                merged.push(r);
            } else {
                let last = merged.last_mut().unwrap();
                if r[0] <= last[1] + 1 {
                    last[1] = std::cmp::max(last[1], r[1]);
                } else {
                    merged.push(r);
                }
            }
        }

        let mut output = String::new();
        let rendered_lines: Vec<_> = full_rendered.lines().collect();

        for (i, [start, end]) in merged.iter().enumerate() {
            if i > 0 {
                output.push_str("…§\n");
            }
            for line in &rendered_lines[(*start - 1)..*end] {
                output.push_str(line);
                output.push('\n');
            }
        }
        Ok(output)
    } else {
        Ok(full_rendered)
    }
}

async fn edit_inner<D: WorkflowDeps>(
    deps: &D,
    raw: serde_json::Value,
) -> Result<DiffRender, FileToolError> {
    let mut edit: LlmEdit = serde_json::from_value(raw).map_err(FileToolError::ParseEdit)?;
    resolve_edit_path(&mut edit, deps.current_cwd().as_deref());
    let session = deps.editor_factory().session();
    let mut sess = session.lock().await;
    Ok(sess.edit(edit, write_draft).await?)
}

fn resolve_edit_path(edit: &mut LlmEdit, cwd: Option<&Path>) {
    let path = match edit {
        LlmEdit::ReplaceLines { path, .. }
        | LlmEdit::ReplaceAll { path, .. }
        | LlmEdit::InsertAfter { path, .. }
        | LlmEdit::InsertBefore { path, .. }
        | LlmEdit::New { path, .. }
        | LlmEdit::Overwrite { path, .. } => path,
    };
    *path = resolve_relative(path, cwd);
}

/// Stat without reading content — cheap, used by the loop guard so it
/// can answer "same file, same mtime+size?" before deciding to read.
async fn stat_file<F: WorkflowFs>(fs: &F, path: &Path) -> io::Result<(i64, u64)> {
    let meta = fs.metadata(path).await?;
    Ok((meta.mtime_ns, meta.size))
}

/// Read content as lines. Caller has typically already stat'd the
/// file (and used the result to seed the loop-guard key), so we don't
/// re-stat here.
async fn read_file_lines<F: WorkflowFs>(fs: &F, path: &Path) -> io::Result<Vec<String>> {
    let content = fs.read_to_string(path).await?;
    Ok(split_lines(&content))
}

fn hash_read_file_args(args: &ReadFileArgs) -> u64 {
    let mut buf = Vec::with_capacity(64);
    // Discriminator so file_read and readRaw on the same path don't
    // collide; their result shapes are different.
    buf.push(0u8);
    buf.extend_from_slice(args.path.as_bytes());
    buf.push(0);
    if let Some(ranges) = &args.ranges {
        for r in ranges {
            buf.extend_from_slice(&r[0].to_le_bytes());
            buf.extend_from_slice(&r[1].to_le_bytes());
        }
    }
    XxHash3_64::oneshot(&buf)
}

fn hash_read_raw_args(path: &str) -> u64 {
    let mut buf = Vec::with_capacity(path.len() + 1);
    buf.push(1u8);
    buf.extend_from_slice(path.as_bytes());
    XxHash3_64::oneshot(&buf)
}

fn write_draft(path: &Path, draft: &[String]) -> io::Result<(Vec<String>, i64, u64)> {
    // file_new can target a path whose parent doesn't exist yet; other
    // ops already required a prior successful read so the parent is
    // there. create_dir_all is idempotent — safe in either case.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut content = draft.join("\n");
    content.push('\n');
    fs::write(path, &content)?;
    let meta = fs::metadata(path)?;
    let mtime_ns = mtime_ns_from(&meta)?;
    let size = meta.len();
    let post = fs::read_to_string(path)?;
    Ok((split_lines(&post), mtime_ns, size))
}

fn split_lines(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s.split('\n').map(str::to_owned).collect();
    if lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
    lines
}

fn mtime_ns_from(meta: &fs::Metadata) -> io::Result<i64> {
    let modified = meta.modified()?;
    let dur = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| io::Error::other(format!("mtime before epoch: {e}")))?;
    i64::try_from(dur.as_nanos()).map_err(|e| io::Error::other(format!("mtime overflow: {e}")))
}

/// Run the session-scoped edit commit (clears anchor tombstones).
async fn commit_inner<D: WorkflowDeps>(deps: &D) -> Result<(), FileToolError> {
    let session = deps.editor_factory().session();
    let mut sess = session.lock().await;
    Ok(sess.commit_edits().await?)
}

/// Promise payload that resolves to `undefined` or rejects with an
/// error message. Used by `editor.commit()`.
struct EditorUnitResult(Result<(), FileToolError>);

impl<'js> IntoJs<'js> for EditorUnitResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

/// Promise payload that resolves to a string or rejects with an error
/// message — matches `ShellOpResult`'s shape so the JS surface feels
/// the same.
struct EditorStringResult(Result<String, FileToolError>);

impl<'js> IntoJs<'js> for EditorStringResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(s) => s.into_js(ctx),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

/// Promise payload for `editor.edit()`. Resolves to
/// `{ text: string, diff: Array<{ kind, text, line? }> }` so the JS
/// caller can both return the LLM-facing string AND push a
/// `DiffSection` for the TUI. Rejects with an error message on failure.
struct EditorEditResult(Result<DiffRender, FileToolError>);

impl<'js> IntoJs<'js> for EditorEditResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(render) => {
                let obj = Object::new(ctx.clone())?;
                obj.set("text", render.text)?;
                obj.set("diff", diff_ops_to_js(ctx, &render.ops)?)?;
                Ok(obj.into_value())
            }
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

fn diff_ops_to_js<'js>(ctx: &Ctx<'js>, ops: &[DiffOp]) -> JsResult<Value<'js>> {
    let arr = rquickjs::Array::new(ctx.clone())?;
    for (i, op) in ops.iter().enumerate() {
        let entry = Object::new(ctx.clone())?;
        match op {
            DiffOp::Context { text, line } => {
                entry.set("kind", "context")?;
                entry.set("text", text.as_str())?;
                entry.set("line", *line)?;
            }
            DiffOp::Added(t) => {
                entry.set("kind", "added")?;
                entry.set("text", t.as_str())?;
            }
            DiffOp::Removed(t) => {
                entry.set("kind", "removed")?;
                entry.set("text", t.as_str())?;
            }
        }
        arr.set(i, entry)?;
    }
    Ok(arr.into_value())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn resolve_path_uses_cwd_for_relative() {
        let cwd = PathBuf::from("/tmp");
        assert_eq!(
            resolve_relative(Path::new("foo.txt"), Some(&cwd)),
            PathBuf::from("/tmp/foo.txt")
        );
    }

    #[test]
    fn resolve_path_keeps_absolute_paths_intact() {
        let cwd = PathBuf::from("/tmp");
        assert_eq!(
            resolve_relative(Path::new("/etc/passwd"), Some(&cwd)),
            PathBuf::from("/etc/passwd"),
        );
    }

    #[test]
    fn split_lines_strips_trailing_newline_only() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb\n\n"), vec!["a", "b", ""]);
        assert_eq!(split_lines(""), Vec::<String>::new());
    }

    #[test]
    fn write_draft_creates_missing_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c.txt");
        let result = write_draft(&nested, &["hello".to_string()]).unwrap();
        let (post, _mtime, size) = result;
        assert_eq!(post, vec!["hello"]);
        assert!(size > 0);
        assert!(nested.exists());
    }
}
