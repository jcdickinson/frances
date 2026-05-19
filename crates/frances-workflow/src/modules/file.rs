//! `frances:v1/tools/file` — anchor-aware file editor primitive.
//!
//! `new Editor()` represents the daemon's session-scoped editor. Each
//! construction returns a handle backed by the *same* `EditSession`
//! held by the daemon, so all reads/edits across a workflow share the
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
//! Paths are resolved against the daemon's most-recently-attached
//! client cwd (`WorkflowDeps::current_cwd`) on every call, so re-attach
//! with a new cwd takes effect immediately.
//!
//! Writes (`New`, `Overwrite`, anchor edits) `create_dir_all` for the
//! parent — idempotent; in practice it only matters for `New` since the
//! other ops require a prior successful `readFile`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};

use frances_edit::LlmEdit;

use crate::deps::{EditorFactory, WorkflowDeps};

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
                                        Err("parse readFile args: invalid arg shape".to_owned())
                                    }
                                }
                            },
                            Err(msg) => Err(msg),
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
                        EditorStringResult(read_raw_inner(&deps, path))
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
                            Err(msg) => Err(msg),
                        };
                        EditorStringResult(result)
                    }))
                },
            )?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Disk-only read. Returns the file as-is, with no `EditSession`
/// interaction — so the path is *not* registered for editing and the
/// caller doesn't get anchors. Used by `Read` when the LLM asks for the
/// content to land in a Frances variable instead of in tool-result
/// text.
fn read_raw_inner<D: WorkflowDeps>(deps: &D, path: String) -> Result<String, String> {
    let resolved = resolve_path(deps.current_cwd().as_deref(), Path::new(&path));
    fs::read_to_string(&resolved).map_err(|e| format!("{}: {e}", resolved.display()))
}

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    ranges: Option<Vec<[usize; 2]>>,
}

async fn read_file_inner<D: WorkflowDeps>(deps: &D, args: ReadFileArgs) -> Result<String, String> {
    let resolved = resolve_path(deps.current_cwd().as_deref(), Path::new(&args.path));
    let (lines, mtime_ns, size) =
        read_file_from_disk(&resolved).map_err(|e| format!("{}: {e}", resolved.display()))?;
    let total_lines = lines.len();
    let session: Arc<_> = deps.editor_factory().session();
    let mut sess = session.lock().await;

    let full_rendered = sess
        .read_file(resolved, lines, mtime_ns, size)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(ranges) = args.ranges {
        // filter out zero, wait 1-indexed so 0 is invalid anyway.
        // wait, let's normalize, sort, and merge overlapping.
        let mut final_ranges = Vec::new();
        for [start, end] in ranges {
            if end < start {
                return Err(format!("reverse range [{}, {}]", start, end));
            }
            if start == 0 {
                return Err("ranges are 1-indexed, got start=0".to_owned());
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

async fn edit_inner<D: WorkflowDeps>(deps: &D, raw: serde_json::Value) -> Result<String, String> {
    let mut edit: LlmEdit = serde_json::from_value(raw).map_err(|e| format!("parse edit: {e}"))?;
    resolve_edit_path(&mut edit, deps.current_cwd().as_deref());
    let session = deps.editor_factory().session();
    let mut sess = session.lock().await;
    sess.edit(edit, write_draft)
        .await
        .map_err(|e| e.to_string())
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
    *path = resolve_path(cwd, path);
}

fn read_file_from_disk(path: &Path) -> io::Result<(Vec<String>, i64, u64)> {
    let content = fs::read_to_string(path)?;
    let meta = fs::metadata(path)?;
    let mtime_ns = mtime_ns_from(&meta)?;
    let size = meta.len();
    Ok((split_lines(&content), mtime_ns, size))
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

fn resolve_path(cwd: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
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

/// Promise payload that resolves to a string or rejects with an error
/// message — matches `ShellOpResult`'s shape so the JS surface feels
/// the same.
struct EditorStringResult(Result<String, String>);

impl<'js> IntoJs<'js> for EditorStringResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(s) => s.into_js(ctx),
            Err(msg) => Err(throw(ctx, &msg)),
        }
    }
}

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_uses_cwd_for_relative() {
        let cwd = PathBuf::from("/tmp");
        assert_eq!(
            resolve_path(Some(&cwd), Path::new("foo.txt")),
            PathBuf::from("/tmp/foo.txt")
        );
    }

    #[test]
    fn resolve_path_keeps_absolute_paths_intact() {
        let cwd = PathBuf::from("/tmp");
        assert_eq!(
            resolve_path(Some(&cwd), Path::new("/etc/passwd")),
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
