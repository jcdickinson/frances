use std::path::PathBuf;

use frances_edit::{AnchorParseError, Truncated};
use thiserror::Error;

/// One structured edit. The dispatcher in `tools::file` deserializes the
/// per-tool args struct (e.g. `file_replace` → `{ path, anchor, end_anchor,
/// text }`) and constructs the matching variant. `anchor` and `end_anchor`
/// are full rendered anchor lines (`Word§content`) — the same string
/// `file_read` produced for that line; the engine splits on the first `§` to
/// recover the anchor word and validates the content (trimmed) against the
/// cached file.
///
/// `New` and `Overwrite` are whole-file operations that replace the file's
/// content with `text` outright.
#[derive(Debug)]
pub enum LlmEdit {
    Replace {
        path: PathBuf,
        anchor: String,
        end_anchor: String,
        text: String,
    },
    InsertAfter {
        path: PathBuf,
        anchor: String,
        text: String,
    },
    InsertBefore {
        path: PathBuf,
        anchor: String,
        text: String,
    },
    New {
        path: PathBuf,
        text: String,
    },
    Overwrite {
        path: PathBuf,
        text: String,
    },
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
    #[error("{path} is not cached; call file_read before 'overwrite'")]
    NotCachedForOverwrite { path: PathBuf },
}
