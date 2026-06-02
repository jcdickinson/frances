//! `frances:v1/tools/file` — anchor-aware file editor primitive.
//!
//! `new Editor()` mints a fresh read context: an empty read cache + loop
//! guard over the host's shared anchor engine. So anchor state persists
//! across contexts (the engine is shared), but "have I read this here?"
//! tracks the live context — a new `Editor` must `readFile` before it can
//! edit. The Rust side owns I/O (disk read, write, stat) and delegates anchor
//! work to `frances_edit::EditSession`. A `FileSearch` binds to an `Editor`
//! to share its loop guard.
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
use std::hash::Hasher;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};
use tokio::sync::Mutex as AsyncMutex;
use twox_hash::XxHash3_64;

use std::fmt::Write;

use frances_core::{is_within, resolve_relative};
use frances_edit::{DiffOp, DiffRender, EditError, LlmEdit, LoopKey, LoopKind, WriteMode};

use super::throw_js as throw;
use crate::deps::{EditorFactory, EditorSession, WorkflowDeps};
use crate::io::WorkflowFs;

pub(crate) fn build_editor_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<EditorJs<D>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> JsResult<Class<'js, EditorJs<D>>> {
            // Each `new Editor()` is a fresh read context: an empty read cache
            // + loop guard over the host's shared anchor engine.
            let session = Arc::new(AsyncMutex::new(deps.editor_factory().new_session()));
            Class::instance(
                ctx.clone(),
                EditorJs {
                    deps: deps.clone(),
                    session,
                },
            )
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
    /// This editor's per-context read session. `pub(crate)` so a `FileSearch`
    /// can bind to the same session (shared loop guard) at construction.
    pub(crate) session: EditorSession<D>,
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
                    let b = this.0.borrow();
                    let deps = b.deps.clone();
                    let session = b.session.clone();
                    drop(b);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        let result = match raw {
                            Ok(v) => match serde_json::from_value::<ReadFileArgs>(v.clone()) {
                                Ok(args) => read_file_inner(&deps, &session, args).await,
                                Err(_) => {
                                    if let Some(path_str) = v.as_str() {
                                        read_file_inner(
                                            &deps,
                                            &session,
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
                    let b = this.0.borrow();
                    let deps = b.deps.clone();
                    let session = b.session.clone();
                    drop(b);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        EditorStringResult(read_raw_inner(&deps, &session, path).await)
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
                    let b = this.0.borrow();
                    let deps = b.deps.clone();
                    let session = b.session.clone();
                    drop(b);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        let result = match raw {
                            Ok(v) => edit_inner(&deps, &session, v).await,
                            Err(msg) => Err(FileToolError::DecodeArgs(msg)),
                        };
                        EditorEditResult(result)
                    }))
                },
            )?,
        )?;

        // Commit accumulated edits (clears anchor tombstones). The
        // workflow calls this at its own reconciliation boundary.
        proto.set(
            "commit",
            Function::new(ctx.clone(), |this: This<Class<'js, EditorJs<D>>>| {
                let session = this.0.borrow().session.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    EditorUnitResult(commit_inner::<D>(&session).await)
                }))
            })?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

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
    #[error("path is outside the project and cannot be edited: {path}")]
    OutsideProject { path: String },
}

/// Disk-only read with no `EditSession` anchor interaction — the path is
/// not registered for editing and the caller gets no anchors.
async fn read_raw_inner<D: WorkflowDeps>(
    deps: &D,
    session: &EditorSession<D>,
    path: String,
) -> Result<String, FileToolError> {
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
    session: &EditorSession<D>,
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

    // Out-of-repo: plain line numbers, no anchor registration.
    let is_editable = deps
        .editable_roots()
        .iter()
        .any(|root| is_within(root, &resolved));
    if !is_editable {
        sess.record_loop(key);
        return render_plain(&lines, args.ranges.as_deref(), total_lines);
    }

    // In-repo: anchor-based read (unchanged).
    let full_rendered = sess.read_file(resolved, lines, mtime_ns, size).await?;
    sess.record_loop(key);

    if let Some(ranges) = args.ranges {
        let merged = merge_ranges(&ranges, total_lines)?;
        let mut output = String::new();
        let rendered_lines: Vec<_> = full_rendered.lines().collect();

        for (i, &[start, end]) in merged.iter().enumerate() {
            if i > 0 {
                output.push_str("…§\n");
            }
            for line in &rendered_lines[(start - 1)..end] {
                output.push_str(line);
                output.push('\n');
            }
        }
        Ok(output)
    } else {
        Ok(full_rendered)
    }
}

/// Validate, clamp, sort, and merge 1-indexed line ranges.
fn merge_ranges(
    ranges: &[[usize; 2]],
    total_lines: usize,
) -> Result<Vec<[usize; 2]>, FileToolError> {
    let mut final_ranges = Vec::new();
    for &[start, end] in ranges {
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
    Ok(merged)
}

/// Render file lines with `line_num:content` formatting for out-of-repo
/// reads (no anchors). When `ranges` is `None`, all lines are rendered.
/// When present, only the specified (merged) ranges are shown, separated
/// by `…`.
fn render_plain(
    lines: &[String],
    ranges: Option<&[[usize; 2]]>,
    total_lines: usize,
) -> Result<String, FileToolError> {
    match ranges {
        None => {
            let mut out = String::new();
            for (i, line) in lines.iter().enumerate() {
                let _ = writeln!(out, "{}:{}", i + 1, line);
            }
            Ok(out)
        }
        Some(ranges) => {
            let merged = merge_ranges(ranges, total_lines)?;
            if merged.is_empty() {
                return Ok(String::new());
            }
            let mut out = String::new();
            for (i, &[start, end]) in merged.iter().enumerate() {
                if i > 0 {
                    out.push_str("…\n");
                }
                for line_num in start..=end {
                    let _ = writeln!(out, "{}:{}", line_num, lines[line_num - 1]);
                }
            }
            Ok(out)
        }
    }
}

async fn edit_inner<D: WorkflowDeps>(
    deps: &D,
    session: &EditorSession<D>,
    raw: serde_json::Value,
) -> Result<DiffRender, FileToolError> {
    let mut edit: LlmEdit = serde_json::from_value(raw).map_err(FileToolError::ParseEdit)?;
    resolve_edit_path(&mut edit, deps.current_cwd().as_deref());

    // Reject edits on paths outside the project's editable roots.
    let resolved = edit_path(&edit);
    let is_editable = deps
        .editable_roots()
        .iter()
        .any(|root| is_within(root, resolved));
    if !is_editable {
        return Err(FileToolError::OutsideProject {
            path: resolved.display().to_string(),
        });
    }

    let mut sess = session.lock().await;
    Ok(sess.edit(edit, write_draft).await?)
}

/// Extract the resolved path from an `LlmEdit`.
fn edit_path(edit: &LlmEdit) -> &Path {
    match edit {
        LlmEdit::ReplaceLines { path, .. }
        | LlmEdit::ReplaceAll { path, .. }
        | LlmEdit::InsertAfter { path, .. }
        | LlmEdit::InsertBefore { path, .. }
        | LlmEdit::New { path, .. }
        | LlmEdit::Overwrite { path, .. } => Path::new(path),
    }
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

/// Stat without reading content — used by the loop guard to check
/// "same file, same mtime+size?" before deciding to read.
async fn stat_file<F: WorkflowFs>(fs: &F, path: &Path) -> io::Result<(i64, u64)> {
    let meta = fs.metadata(path).await?;
    Ok((meta.mtime_ns, meta.size))
}

/// Read content as lines. Caller has already stat'd the file to seed
/// the loop-guard key.
async fn read_file_lines<F: WorkflowFs>(fs: &F, path: &Path) -> io::Result<Vec<String>> {
    let content = fs.read_to_string(path).await?;
    Ok(split_lines(&content))
}

fn hash_read_file_args(args: &ReadFileArgs) -> u64 {
    let mut hasher = XxHash3_64::new();
    // Discriminator so file_read and readRaw on the same path don't
    // collide; their result shapes are different.
    hasher.write(&[LoopKind::ReadFile as u8]);
    hasher.write(args.path.as_bytes());
    hasher.write(&[0]);
    if let Some(ranges) = &args.ranges {
        for r in ranges {
            hasher.write(&r[0].to_le_bytes());
            hasher.write(&r[1].to_le_bytes());
        }
    }
    hasher.finish()
}

fn hash_read_raw_args(path: &str) -> u64 {
    let mut hasher = XxHash3_64::new();
    hasher.write(&[LoopKind::ReadRaw as u8]);
    hasher.write(path.as_bytes());
    hasher.finish()
}

fn write_draft(
    path: &Path,
    draft: &[String],
    mode: WriteMode,
) -> io::Result<(Vec<String>, i64, u64)> {
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
    match mode {
        // create_new is the atomic check-and-create: if the file appeared
        // since file_new's caller decided to create it, this fails with
        // AlreadyExists rather than clobbering it.
        WriteMode::CreateNew => {
            use io::Write;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            f.write_all(content.as_bytes())?;
        }
        WriteMode::Overwrite => fs::write(path, &content)?,
    }
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

/// Run the edit commit (clears anchor tombstones on the shared engine).
async fn commit_inner<D: WorkflowDeps>(session: &EditorSession<D>) -> Result<(), FileToolError> {
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

/// Promise payload that resolves to a string or rejects with an error message.
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
        let result = write_draft(&nested, &["hello".to_string()], WriteMode::Overwrite).unwrap();
        let (post, _mtime, size) = result;
        assert_eq!(post, vec!["hello"]);
        assert!(size > 0);
        assert!(nested.exists());
    }
}
