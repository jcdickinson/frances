use std::io;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;

use crate::{AnchorParseError, StoreError, Truncated};

/// One structured edit. Callers deserialize the matching variant from tool
/// args. `anchor` and `end_anchor` are full rendered anchor lines
/// (`Word§content`) — the same string `read_file` produced for that line; the
/// engine splits on the first `§` to recover the anchor word and validates the
/// content (trimmed) against the cached file.
///
/// `New` and `Overwrite` are whole-file operations that replace the file's
/// content with `text` outright.
///
/// Tagged JSON shape for cross-runtime deserialization (the JS workflow
/// side hands a `{ kind, ...fields }` object straight into
/// `serde_json::from_value`):
///
/// ```json
/// { "kind": "ReplaceLines", "path": "...", "anchor": "...", "end_anchor": "...", "text": "..." }
/// ```
#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmEdit {
    ReplaceLines {
        path: PathBuf,
        anchor: String,
        end_anchor: String,
        text: String,
        #[serde(default)]
        bypass_anchor_guard: bool,
    },
    ReplaceAll {
        path: PathBuf,
        find: String,
        replacement: String,
        count: Option<usize>,
    },
    InsertAfter {
        path: PathBuf,
        anchor: String,
        text: String,
        #[serde(default)]
        bypass_anchor_guard: bool,
    },
    InsertBefore {
        path: PathBuf,
        anchor: String,
        text: String,
        #[serde(default)]
        bypass_anchor_guard: bool,
    },
    New {
        path: PathBuf,
        text: String,
    },
    Overwrite {
        path: PathBuf,
        text: String,
        #[serde(default)]
        bypass_anchor_guard: bool,
    },
}

/// How the draft writer must open the target file. The check-and-create for
/// a brand-new file has to be atomic, and frances-edit is filesystem-agnostic
/// (the caller's `on_draft` owns the actual write), so the engine threads this
/// signal through instead of doing a racy `path.exists()` itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteMode {
    /// File must not already exist; open with `create_new(true)`.
    CreateNew,
    /// Replace the contents of a file that was already read this session.
    Overwrite,
}

#[derive(Error, Debug)]
pub enum EditError {
    #[error(
        "anchor word '{word}' not found in the latest read of {path}; re-read the file and try again"
    )]
    AnchorNotFound { word: String, path: PathBuf },
    #[error(
        "anchor '{word}' content mismatch (trimmed): file has {actual}, edit specified {claimed}"
    )]
    ContentMismatch {
        word: String,
        actual: Truncated,
        claimed: Truncated,
    },
    #[error("malformed anchor '{field}': expected '<Word>§<content>'")]
    MalformedAnchor { field: String },
    #[error(
        "anchor field spans multiple lines; pass exactly one rendered '<Word>§<content>' line (first line was '{first_line}')"
    )]
    MultilineAnchor { first_line: String },
    #[error(
        "edit `text` payload looks like pasted-back anchor renders: every \
         non-blank line begins with `Word§`. Anchors are assigned by the \
         engine — `text` should be the bare line content with no anchor \
         prefixes. You wrote: {anchors}. Remove the prefixes and resubmit. \
         (If these characters are genuinely intended as literal file \
         content, resubmit with `bypass_anchor_guard: true`.)"
    )]
    AnchorPastebackDetected { anchors: String },
    #[error("invalid anchor word '{word}': {source}")]
    BadAnchorWord {
        word: String,
        #[source]
        source: AnchorParseError,
    },
    #[error(
        "replace requires end_anchor.anchor word to be at or after anchor.anchor word in the file (got start={start} end={end})"
    )]
    BackwardsReplaceRange { start: u32, end: u32 },
    #[error(
        "cannot create {path} with 'new' because the file already exists; use 'overwrite' instead"
    )]
    NewFileExists { path: PathBuf },
    #[error("{path} is not cached; call file_read first")]
    NotCached { path: PathBuf },
    #[error(transparent)]
    Regex(#[from] regex::Error),
    #[error(
        "replace_all matched {actual} times, which exceeds count cap {limit}; no changes written"
    )]
    ReplaceAllCountExceeded { actual: usize, limit: usize },
    #[error("{path} is not cached; call file_read before 'overwrite'")]
    NotCachedForOverwrite { path: PathBuf },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("draft write failed: {0}")]
    Draft(#[from] io::Error),
}

pub type EditResult<T> = std::result::Result<T, EditError>;
