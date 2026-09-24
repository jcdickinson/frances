use crate::deps::{EditorSession, HarnessDeps};
use crate::io::HarnessFs;
use frances_core::resolve_relative;
use frances_edit::{DiffRender, DraftWriter, EditError, LlmEdit, LoopKey, LoopKind, WriteMode};
use std::fmt::Write;
use std::hash::Hasher;
use std::io;
use std::path::Path;
use twox_hash::XxHash3_64;
#[derive(Debug, thiserror::Error)]
pub enum FileToolError {
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
pub async fn read_raw_inner<D: HarnessDeps>(
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
pub struct ReadFileArgs {
    pub path: String,
    pub ranges: Option<Vec<[usize; 2]>>,
}

pub async fn read_file_inner<D: HarnessDeps>(
    deps: &D,
    session: &EditorSession<D>,
    args: ReadFileArgs,
) -> Result<String, FileToolError> {
    let requested = resolve_relative(Path::new(&args.path), deps.current_cwd().as_deref());
    let fs = deps.fs();
    let resolved = fs
        .canonicalize(&requested)
        .await
        .map_err(|source| FileToolError::Io {
            path: requested.display().to_string(),
            source,
        })?;
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
        .any(|root| resolved.starts_with(root));
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
pub(super) fn merge_ranges(
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
            if r[0] <= last[1].saturating_add(1) {
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
                writeln!(out, "{}:{}", i + 1, line).expect("writing to a String cannot fail");
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
                    writeln!(out, "{}:{}", line_num, lines[line_num - 1])
                        .expect("writing to a String cannot fail");
                }
            }
            Ok(out)
        }
    }
}

pub async fn edit_inner<D: HarnessDeps>(
    deps: &D,
    session: &EditorSession<D>,
    raw: serde_json::Value,
) -> Result<DiffRender, FileToolError> {
    let mut edit: LlmEdit = serde_json::from_value(raw).map_err(FileToolError::ParseEdit)?;
    resolve_edit_path(&mut edit, deps.current_cwd().as_deref());

    let requested = edit_path(&edit);
    let mut existing = requested.to_path_buf();
    let mut suffix = Vec::new();
    let resolved = loop {
        match deps.fs().canonicalize(&existing).await {
            Ok(mut path) => {
                for component in suffix.iter().rev() {
                    path.push(component);
                }
                break path;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(%error, ?existing, "resolving new file parent");
                let Some(name) = existing.file_name().map(std::ffi::OsStr::to_owned) else {
                    return Err(FileToolError::Io {
                        path: requested.display().to_string(),
                        source: error,
                    });
                };
                suffix.push(name);
                existing.pop();
            }
            Err(source) => {
                return Err(FileToolError::Io {
                    path: requested.display().to_string(),
                    source,
                });
            }
        }
    };
    if !deps
        .editable_roots()
        .iter()
        .any(|root| resolved.starts_with(root))
    {
        return Err(FileToolError::OutsideProject {
            path: requested.display().to_string(),
        });
    }
    *edit_path_mut(&mut edit) = resolved;

    let mut sess = session.lock().await;
    let writer = HarnessDraftWriter {
        fs: deps.fs().clone(),
    };
    Ok(sess.edit(edit, writer).await?)
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

fn edit_path_mut(edit: &mut LlmEdit) -> &mut std::path::PathBuf {
    match edit {
        LlmEdit::ReplaceLines { path, .. }
        | LlmEdit::ReplaceAll { path, .. }
        | LlmEdit::InsertAfter { path, .. }
        | LlmEdit::InsertBefore { path, .. }
        | LlmEdit::New { path, .. }
        | LlmEdit::Overwrite { path, .. } => path,
    }
}
fn resolve_edit_path(edit: &mut LlmEdit, cwd: Option<&Path>) {
    let path = edit_path_mut(edit);
    *path = resolve_relative(&*path, cwd);
}

/// Stat without reading content — used by the loop guard to check
/// "same file, same mtime+size?" before deciding to read.
async fn stat_file<F: HarnessFs>(fs: &F, path: &Path) -> io::Result<(i64, u64)> {
    let meta = fs.metadata(path).await?;
    Ok((meta.mtime_ns, meta.size))
}

/// Read content as lines. Caller has already stat'd the file to seed
/// the loop-guard key.
async fn read_file_lines<F: HarnessFs>(fs: &F, path: &Path) -> io::Result<Vec<String>> {
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

struct HarnessDraftWriter<F> {
    fs: F,
}

impl<F: HarnessFs> DraftWriter for HarnessDraftWriter<F> {
    async fn write(
        &mut self,
        path: &Path,
        draft: &[String],
        mode: WriteMode,
    ) -> io::Result<(Vec<String>, i64, u64)> {
        write_draft(&self.fs, path, draft, mode).await
    }
}

async fn write_draft<F: HarnessFs>(
    fs: &F,
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
        fs.create_dir_all(parent).await?;
    }
    let mut content = draft.join("\n");
    content.push('\n');
    match mode {
        // create_new is the atomic check-and-create: if the file appeared
        // since file_new's caller decided to create it, this fails with
        // AlreadyExists rather than clobbering it.
        WriteMode::CreateNew => fs.write_create_new(path, content.as_bytes()).await?,
        WriteMode::Overwrite => fs.write(path, content.as_bytes()).await?,
    }
    let meta = fs.metadata(path).await?;
    let post = fs.read_to_string(path).await?;
    Ok((split_lines(&post), meta.mtime_ns, meta.size))
}

fn split_lines(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s.split('\n').map(str::to_owned).collect();
    if lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// Run the edit commit (clears anchor tombstones on the shared engine).
pub async fn commit_inner<D: HarnessDeps>(session: &EditorSession<D>) -> Result<(), FileToolError> {
    let mut sess = session.lock().await;
    Ok(sess.commit_edits().await?)
}
