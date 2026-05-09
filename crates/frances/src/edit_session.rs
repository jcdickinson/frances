use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Result, anyhow};
use frances_edit::{
    Anchor, AnchorParseError, AnchorStore, EditEngine, EditHints, EditOp, FileAnchorState, Pool,
    Truncated, WorkingFile, apply_ops, reconcile, render_diff_block, render_file,
};
use serde::Deserialize;
use thiserror::Error;

const DIFF_CONTEXT: usize = 2;
const ANCHOR_SEP: char = '§';

#[derive(Deserialize, Debug)]
pub struct EditInput {
    pub files: Vec<EditFileEntry>,
}

#[derive(Deserialize, Debug)]
pub struct EditFileEntry {
    pub path: PathBuf,
    pub edits: Vec<LlmEdit>,
}

/// Structured edit shape the model emits. The `anchor` and `end_anchor`
/// fields are full rendered anchor lines (`Word§content`) — the same string
/// `read_file` produced for that line. The validator splits on the first
/// `§` to recover the anchor word and validates the content (trimmed) against
/// the cached file.
///
/// `New` and `Overwrite` are whole-file operations: they replace the file's
/// content with `text` outright. They must be the only edit in their file's
/// `edits` array.
#[derive(Deserialize, Debug)]
#[serde(tag = "edit_type", rename_all = "snake_case")]
pub enum LlmEdit {
    Replace {
        anchor: String,
        end_anchor: String,
        text: String,
    },
    InsertAfter {
        anchor: String,
        text: String,
    },
    InsertBefore {
        anchor: String,
        text: String,
    },
    New {
        text: String,
    },
    Overwrite {
        text: String,
    },
}

impl LlmEdit {
    fn is_file_level(&self) -> bool {
        matches!(self, LlmEdit::New { .. } | LlmEdit::Overwrite { .. })
    }
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
        "edits in this batch overlap on lines {a_start}..={a_end} and {b_start}..={b_end}; split them into non-overlapping calls"
    )]
    OverlappingEdits {
        a_start: u32,
        a_end: u32,
        b_start: u32,
        b_end: u32,
    },
    #[error(
        "'new' and 'overwrite' must be the only edit for {path}; split unrelated edits into separate calls"
    )]
    FileLevelEditNotAlone { path: PathBuf },
    #[error(
        "{path} appears more than once in this call but uses 'new' or 'overwrite'; that path must be unique"
    )]
    FileLevelPathDuplicated { path: PathBuf },
    #[error(
        "cannot create {path} with 'new' because the file already exists; use 'overwrite' instead"
    )]
    NewFileExists { path: PathBuf },
}

pub struct EditSession<S: AnchorStore> {
    engine: EditEngine<S>,
    open_files: HashMap<PathBuf, WorkingFile>,
}

impl<S: AnchorStore> EditSession<S> {
    pub fn new(engine: EditEngine<S>) -> Self {
        Self {
            engine,
            open_files: HashMap::new(),
        }
    }

    /// Tool: read_file. Caller supplies the file's current content;
    /// we drift-reconcile against any cached anchor state, render, and cache
    /// the resulting WorkingFile for subsequent edits in this session.
    /// Returns the anchored render as plain text.
    pub async fn read_file(
        &mut self,
        path: PathBuf,
        lines: Vec<String>,
        mtime_ns: i64,
        size: u64,
    ) -> Result<String> {
        let working = self
            .engine
            .open(path.clone(), lines, mtime_ns, size)
            .await?;
        let rendered = render_file(&working.state, &working.lines);
        self.open_files.insert(path, working);
        Ok(rendered)
    }

    /// Tool: edit. For each file in `input`, dispatches on the kind of edit:
    /// file-level (`new`/`overwrite`) replace the whole file; line-level edits
    /// are validated against the cached anchored state, replayed into a
    /// draft, written via `on_draft`, then reconciled. Returns the
    /// concatenated diff blocks (one per file) as plain text.
    ///
    /// Line-level edits and `overwrite` require the path to be cached via
    /// `read_file`. `new` requires the file not to exist on disk.
    pub async fn edit<F>(&mut self, input: EditInput, mut on_draft: F) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        ensure_file_level_paths_unique(&input.files)?;

        let mut blocks = Vec::new();
        for entry in input.files {
            let path = entry.path;

            if entry.edits.iter().any(LlmEdit::is_file_level) {
                if entry.edits.len() != 1 {
                    return Err(EditError::FileLevelEditNotAlone { path }.into());
                }
                let block = match entry.edits.into_iter().next().expect("len == 1") {
                    LlmEdit::New { text } => self.apply_new(&path, &text, &mut on_draft).await?,
                    LlmEdit::Overwrite { text } => {
                        self.apply_overwrite(&path, &text, &mut on_draft).await?
                    }
                    _ => unreachable!("guarded by is_file_level"),
                };
                blocks.push(format!("--- {} ---\n{}", path.display(), block));
                continue;
            }

            let working = self
                .open_files
                .get(&path)
                .ok_or_else(|| anyhow!("{} is not cached; call read_file first", path.display()))?
                .clone();

            let (ops, deleted) =
                validate_edits(&working.state, &working.lines, &entry.edits, &path)?;

            let draft = apply_ops(&working.state, &working.lines, &ops);

            let (post_lines, mtime_ns, size) = on_draft(&path, &draft)?;

            let used = self.engine.store().used_anchors(&path).await?;
            let mut pool = Pool::from_used(used);
            let hints = EditHints {
                deleted_anchors: deleted,
            };
            let outcome = reconcile(&working.state, &post_lines, &mut pool, Some(&hints));

            self.engine
                .commit(&path, &outcome.state, mtime_ns, size, &outcome.tombstoned)
                .await?;

            let block = render_diff_block(
                &working.state,
                &working.lines,
                &outcome.state,
                &post_lines,
                DIFF_CONTEXT,
            );

            self.open_files.insert(
                path.clone(),
                WorkingFile {
                    path: path.clone(),
                    state: outcome.state,
                    lines: post_lines,
                },
            );

            blocks.push(format!("--- {} ---\n{}", path.display(), block));
        }
        Ok(blocks.join("\n"))
    }

    /// Create a brand-new file. Fails if the file already exists on disk —
    /// the model must use `overwrite` for that case (which itself requires a
    /// fresh `read_file` so the model has actually seen the prior content).
    /// Mints fresh anchors for every line and renders a diff against an
    /// empty pre-state (all `+` lines).
    async fn apply_new<F>(&mut self, path: &Path, text: &str, on_draft: &mut F) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        if path.exists() {
            return Err(EditError::NewFileExists {
                path: path.to_path_buf(),
            }
            .into());
        }
        let draft = split_text_to_lines(text);
        let (post_lines, mtime_ns, size) = on_draft(path, &draft)?;
        let working = self
            .engine
            .open(path.to_path_buf(), post_lines, mtime_ns, size)
            .await?;
        let empty_pre = empty_state(path);
        let block = render_diff_block(
            &empty_pre,
            &[],
            &working.state,
            &working.lines,
            DIFF_CONTEXT,
        );
        self.open_files.insert(path.to_path_buf(), working);
        Ok(block)
    }

    /// Overwrite an existing, already-read file. Up-to-date read enforced via
    /// the cache: every cached entry was populated by `read_file` in the
    /// current session. Tombstones every prior anchor and mints fresh ones
    /// via the normal reconcile path.
    async fn apply_overwrite<F>(
        &mut self,
        path: &Path,
        text: &str,
        on_draft: &mut F,
    ) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        let working = self
            .open_files
            .get(path)
            .ok_or_else(|| {
                anyhow!(
                    "{} is not cached; call read_file before 'overwrite'",
                    path.display()
                )
            })?
            .clone();
        let draft = split_text_to_lines(text);
        let (post_lines, mtime_ns, size) = on_draft(path, &draft)?;
        let used = self.engine.store().used_anchors(path).await?;
        let mut pool = Pool::from_used(used);
        let hints = EditHints {
            deleted_anchors: working
                .state
                .lines
                .iter()
                .map(|le| le.anchor.clone())
                .collect(),
        };
        let outcome = reconcile(&working.state, &post_lines, &mut pool, Some(&hints));
        self.engine
            .commit(path, &outcome.state, mtime_ns, size, &outcome.tombstoned)
            .await?;
        let block = render_diff_block(
            &working.state,
            &working.lines,
            &outcome.state,
            &post_lines,
            DIFF_CONTEXT,
        );
        self.open_files.insert(
            path.to_path_buf(),
            WorkingFile {
                path: path.to_path_buf(),
                state: outcome.state,
                lines: post_lines,
            },
        );
        Ok(block)
    }

    /// End-of-turn cleanup. Caller invokes when assistant message's tool
    /// calls are fully processed.
    pub async fn end_turn(&mut self) -> Result<()> {
        self.engine.end_turn().await
    }
}

/// Split a model-supplied `text` payload into a draft line array. Mirrors
/// `apply_ops`: callers split on `\n` exactly so the on-disk line count
/// matches what the model wrote.
fn split_text_to_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

/// A `new`/`overwrite` is a destructive whole-file op; allowing the same path
/// to appear elsewhere in the same call would let line-level edits run
/// against stale anchors (or after the file has been freshly minted) with no
/// useful semantics. Reject any path that uses a file-level edit and also
/// appears in another entry.
fn ensure_file_level_paths_unique(files: &[EditFileEntry]) -> Result<(), EditError> {
    let mut counts: HashMap<&Path, usize> = HashMap::with_capacity(files.len());
    for entry in files {
        *counts.entry(entry.path.as_path()).or_insert(0) += 1;
    }
    for entry in files {
        if entry.edits.iter().any(LlmEdit::is_file_level) && counts[entry.path.as_path()] > 1 {
            return Err(EditError::FileLevelPathDuplicated {
                path: entry.path.clone(),
            });
        }
    }
    Ok(())
}

/// Empty pre-state for diffing newly-created files. Only `lines` is read by
/// `render_diff_block`; the meta fields are placeholders.
fn empty_state(path: &Path) -> FileAnchorState {
    FileAnchorState {
        path: path.to_path_buf(),
        mtime_ns: 0,
        size: 0,
        content_digest: 0,
        lines: Vec::new(),
    }
}

/// Internal representation of an edit's affected line range, used for
/// overlap detection. Replace covers `[start, end]` inclusive; insertions
/// are zero-width "at" a single line index.
#[derive(Debug, Clone, Copy)]
enum AffectedRange {
    Replace { start: u32, end: u32 },
    InsertAt(u32),
}

impl AffectedRange {
    fn span(&self) -> (u32, u32) {
        match self {
            Self::Replace { start, end } => (*start, *end),
            Self::InsertAt(idx) => (*idx, *idx),
        }
    }
}

fn ranges_overlap(a: &AffectedRange, b: &AffectedRange) -> bool {
    match (a, b) {
        // Two inserts only conflict if they pin the same line; even then,
        // the model is asking for two batches at the same point — reject.
        (AffectedRange::InsertAt(x), AffectedRange::InsertAt(y)) => x == y,
        // An insert at idx conflicts with a replace [s..=e] iff s <= idx <= e.
        (AffectedRange::InsertAt(idx), AffectedRange::Replace { start, end })
        | (AffectedRange::Replace { start, end }, AffectedRange::InsertAt(idx)) => {
            *start <= *idx && *idx <= *end
        }
        // Two replaces overlap iff their inclusive ranges intersect.
        (
            AffectedRange::Replace {
                start: a_s,
                end: a_e,
            },
            AffectedRange::Replace {
                start: b_s,
                end: b_e,
            },
        ) => a_s <= b_e && b_s <= a_e,
    }
}

fn validate_edits(
    state: &FileAnchorState,
    lines: &[String],
    edits: &[LlmEdit],
    path: &Path,
) -> Result<(Vec<EditOp>, Vec<Anchor>), EditError> {
    let mut ops = Vec::with_capacity(edits.len());
    let mut deleted = Vec::new();
    let mut ranges: Vec<AffectedRange> = Vec::with_capacity(edits.len());

    for edit in edits {
        let (op, range, mut tombstones) = build_op(edit, state, lines, path)?;
        for prev in &ranges {
            if ranges_overlap(prev, &range) {
                let (a_s, a_e) = prev.span();
                let (b_s, b_e) = range.span();
                return Err(EditError::OverlappingEdits {
                    a_start: a_s,
                    a_end: a_e,
                    b_start: b_s,
                    b_end: b_e,
                });
            }
        }
        ranges.push(range);
        ops.push(op);
        deleted.append(&mut tombstones);
    }

    Ok((ops, deleted))
}

fn build_op(
    edit: &LlmEdit,
    state: &FileAnchorState,
    lines: &[String],
    path: &Path,
) -> Result<(EditOp, AffectedRange, Vec<Anchor>), EditError> {
    match edit {
        LlmEdit::Replace {
            anchor,
            end_anchor,
            text,
        } => {
            let (from_anchor, from_idx) = resolve_anchor(anchor, state, lines, path)?;
            let (to_anchor, to_idx) = resolve_anchor(end_anchor, state, lines, path)?;
            if to_idx < from_idx {
                return Err(EditError::BackwardsReplaceRange {
                    start: from_idx,
                    end: to_idx,
                });
            }
            let new_lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
            let tombstones: Vec<Anchor> = state.lines[from_idx as usize..=to_idx as usize]
                .iter()
                .map(|le| le.anchor.clone())
                .collect();
            Ok((
                EditOp::Replace {
                    from: from_anchor,
                    to: to_anchor,
                    lines: new_lines,
                },
                AffectedRange::Replace {
                    start: from_idx,
                    end: to_idx,
                },
                tombstones,
            ))
        }
        LlmEdit::InsertAfter { anchor, text } => {
            let (pin, idx) = resolve_anchor(anchor, state, lines, path)?;
            let new_lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
            Ok((
                EditOp::InsertAfter {
                    pin,
                    lines: new_lines,
                },
                AffectedRange::InsertAt(idx),
                Vec::new(),
            ))
        }
        LlmEdit::InsertBefore { anchor, text } => {
            let (pin, idx) = resolve_anchor(anchor, state, lines, path)?;
            let new_lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
            Ok((
                EditOp::InsertBefore {
                    pin,
                    lines: new_lines,
                },
                AffectedRange::InsertAt(idx),
                Vec::new(),
            ))
        }
        LlmEdit::New { .. } | LlmEdit::Overwrite { .. } => {
            unreachable!("file-level edits are dispatched before validate_edits");
        }
    }
}

fn resolve_anchor(
    field: &str,
    state: &FileAnchorState,
    lines: &[String],
    path: &Path,
) -> Result<(Anchor, u32), EditError> {
    let (word, content) =
        field
            .split_once(ANCHOR_SEP)
            .ok_or_else(|| EditError::MalformedAnchor {
                field: field.to_string(),
            })?;
    let anchor = Anchor::from_str(word).map_err(|source| EditError::BadAnchorWord {
        word: word.to_string(),
        source,
    })?;
    let idx = state
        .find_anchor(&anchor)
        .ok_or_else(|| EditError::AnchorNotFound {
            word: word.to_string(),
            path: path.to_path_buf(),
        })?;
    let actual = &lines[idx as usize];
    if actual.trim() != content.trim() {
        return Err(EditError::ContentMismatch {
            word: word.to_string(),
            actual: Truncated::new(actual.clone()),
            claimed: Truncated::new(content.to_string()),
        });
    }
    Ok((anchor, idx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_edit::FakeStore;

    fn lines_of(s: &str) -> Vec<String> {
        s.lines().map(str::to_owned).collect()
    }

    fn fresh_session() -> EditSession<FakeStore> {
        EditSession::new(EditEngine::new(FakeStore::new()))
    }

    fn no_format(_: &Path, draft: &[String]) -> Result<(Vec<String>, i64, u64)> {
        let size: u64 = draft.iter().map(|l| (l.len() + 1) as u64).sum();
        Ok((draft.to_vec(), 200, size))
    }

    fn anchor_field(s: &EditSession<FakeStore>, path: &Path, idx: usize) -> String {
        let working = s.open_files.get(path).expect("cached");
        format!(
            "{}{}{}",
            working.state.lines[idx].anchor, ANCHOR_SEP, working.lines[idx]
        )
    }

    #[tokio::test]
    async fn read_file_renders_and_caches() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        let rendered = session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        assert_eq!(rendered.lines().count(), 3);
        for line in rendered.lines() {
            assert!(line.contains('§'));
            assert!(!line.starts_with(' '));
        }
        assert!(session.open_files.contains_key(&path));
    }

    #[tokio::test]
    async fn edit_replace_happy_path() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 1);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::Replace {
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "B2".into(),
                }],
            }],
        };
        let blocks = session.edit(input, no_format).await.unwrap();
        assert!(blocks.contains("§B2"));
        assert_eq!(session.open_files[&path].lines, vec!["a", "B2", "c"]);
    }

    #[tokio::test]
    async fn edit_insert_after_happy_path() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::InsertAfter {
                    anchor: target,
                    text: "X\nY".into(),
                }],
            }],
        };
        session.edit(input, no_format).await.unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a", "X", "Y", "b", "c"]
        );
    }

    #[tokio::test]
    async fn edit_insert_before_happy_path() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::InsertBefore {
                    anchor: target,
                    text: "X".into(),
                }],
            }],
        };
        session.edit(input, no_format).await.unwrap();
        assert_eq!(session.open_files[&path].lines, vec!["X", "a", "b", "c"]);
    }

    #[tokio::test]
    async fn edit_anchor_not_found_returns_named_error() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();

        let unused = unused_dict_word(&session, &path);
        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::InsertAfter {
                    anchor: format!("{unused}§a"),
                    text: "X".into(),
                }],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().expect("EditError downcast");
        assert!(matches!(downcast, EditError::AnchorNotFound { .. }));
    }

    /// Returns a real dict word that isn't currently used as an anchor in
    /// `path`. Robust to dict regeneration; lets the "anchor word valid but
    /// not in this file" tests run without hardcoding a specific entry.
    fn unused_dict_word<S: AnchorStore>(session: &EditSession<S>, path: &Path) -> &'static str {
        let used: std::collections::HashSet<String> = session
            .open_files
            .get(path)
            .expect("file is cached")
            .state
            .lines
            .iter()
            .map(|l| l.anchor.to_string())
            .collect();
        frances_anchors::WORDS
            .iter()
            .copied()
            .skip(frances_anchors::N_PADDING_WORDS)
            .find(|w| !used.contains(*w))
            .expect("dict has more data entries than the file uses")
    }

    #[tokio::test]
    async fn edit_content_mismatch_uses_trimmed() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\n  hello\nb"), 100, 9)
            .await
            .unwrap();
        let working = session.open_files.get(&path).unwrap();
        let anchor_word = working.state.lines[1].anchor.clone();

        // Trimmed match: file has "  hello", model says "hello". Should pass.
        let trimmed_match = format!("{anchor_word}§hello");
        let input_ok = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::Replace {
                    anchor: trimmed_match.clone(),
                    end_anchor: trimmed_match,
                    text: "world".into(),
                }],
            }],
        };
        session.edit(input_ok, no_format).await.unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a".to_string(), "world".to_string(), "b".to_string()]
        );

        // Genuinely different content fails.
        let working = session.open_files.get(&path).unwrap();
        let anchor_word = working.state.lines[1].anchor.clone();
        let wrong = format!("{anchor_word}§not the real content");
        let input_bad = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::Replace {
                    anchor: wrong.clone(),
                    end_anchor: wrong,
                    text: "x".into(),
                }],
            }],
        };
        let err = session.edit(input_bad, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(downcast, EditError::ContentMismatch { .. }));
    }

    #[tokio::test]
    async fn edit_malformed_anchor_field() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a"), 100, 1)
            .await
            .unwrap();

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::InsertAfter {
                    anchor: "no-section-sigil-here".into(),
                    text: "X".into(),
                }],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(downcast, EditError::MalformedAnchor { .. }));
    }

    #[tokio::test]
    async fn edit_overlapping_replace_replace_rejected() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc\nd"), 100, 7)
            .await
            .unwrap();
        let a = anchor_field(&session, &path, 0);
        let c = anchor_field(&session, &path, 2);
        let b = anchor_field(&session, &path, 1);
        let d = anchor_field(&session, &path, 3);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![
                    LlmEdit::Replace {
                        anchor: a,
                        end_anchor: c,
                        text: "X".into(),
                    },
                    LlmEdit::Replace {
                        anchor: b,
                        end_anchor: d,
                        text: "Y".into(),
                    },
                ],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(downcast, EditError::OverlappingEdits { .. }));
    }

    #[tokio::test]
    async fn edit_overlapping_replace_insert_inside_range_rejected() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let a = anchor_field(&session, &path, 0);
        let c = anchor_field(&session, &path, 2);
        let b = anchor_field(&session, &path, 1);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![
                    LlmEdit::Replace {
                        anchor: a,
                        end_anchor: c,
                        text: "X".into(),
                    },
                    LlmEdit::InsertAfter {
                        anchor: b,
                        text: "Y".into(),
                    },
                ],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(downcast, EditError::OverlappingEdits { .. }));
    }

    #[tokio::test]
    async fn edit_uncached_file_errors() {
        let mut session = fresh_session();
        let input = EditInput {
            files: vec![EditFileEntry {
                path: "/uncached".into(),
                edits: vec![LlmEdit::InsertAfter {
                    anchor: "Apple§a".into(),
                    text: "X".into(),
                }],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        assert!(err.to_string().contains("not cached"));
    }

    #[tokio::test]
    async fn edit_validation_failure_preserves_cache() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();

        let bad = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::InsertAfter {
                    anchor: "MissingWord§a".into(),
                    text: "X".into(),
                }],
            }],
        };
        let err = session.edit(bad, no_format).await.unwrap_err();
        assert!(err.downcast_ref::<EditError>().is_some());

        // Cache survives. A well-formed retry against the same anchors works.
        let target = anchor_field(&session, &path, 1);
        let good = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::Replace {
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "B2".into(),
                }],
            }],
        };
        session.edit(good, no_format).await.unwrap();
        assert_eq!(session.open_files[&path].lines, vec!["a", "B2", "c"]);
    }

    #[tokio::test]
    async fn edit_new_creates_file_and_caches_anchors() {
        let mut session = fresh_session();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brand_new.txt");

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::New {
                    text: "alpha\nbeta".into(),
                }],
            }],
        };
        let block = session.edit(input, no_format).await.unwrap();

        assert!(block.contains("§alpha"));
        assert!(block.contains("§beta"));
        // Diff vs empty pre-state ⇒ both lines emitted as `+`.
        assert_eq!(block.matches("\n+").count(), 2);

        let cached = session.open_files.get(&path).expect("cached after new");
        assert_eq!(cached.lines, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn edit_new_on_existing_file_errors() {
        let mut session = fresh_session();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.txt");
        std::fs::write(&path, "preexisting\n").unwrap();

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::New { text: "x".into() }],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(downcast, EditError::NewFileExists { .. }));
    }

    #[tokio::test]
    async fn edit_overwrite_replaces_content_and_tombstones_old() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let old_anchors: Vec<_> = session.open_files[&path]
            .state
            .lines
            .iter()
            .map(|le| le.anchor.clone())
            .collect();

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::Overwrite {
                    text: "x\ny\nz".into(),
                }],
            }],
        };
        session.edit(input, no_format).await.unwrap();

        assert_eq!(session.open_files[&path].lines, vec!["x", "y", "z"]);

        let used = session.engine.store().used_anchors(&path).await.unwrap();
        for old in &old_anchors {
            assert!(used.contains(old), "old anchor not tombstoned: {old}");
        }
    }

    #[tokio::test]
    async fn edit_overwrite_without_read_errors() {
        let mut session = fresh_session();
        let input = EditInput {
            files: vec![EditFileEntry {
                path: "/never-read".into(),
                edits: vec![LlmEdit::Overwrite { text: "x".into() }],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        assert!(err.to_string().contains("not cached"));
    }

    #[tokio::test]
    async fn edit_file_level_duplicate_path_rejected() {
        let mut session = fresh_session();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");

        let input = EditInput {
            files: vec![
                EditFileEntry {
                    path: path.clone(),
                    edits: vec![LlmEdit::New {
                        text: "first".into(),
                    }],
                },
                EditFileEntry {
                    path: path.clone(),
                    edits: vec![LlmEdit::Overwrite {
                        text: "second".into(),
                    }],
                },
            ],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(
            downcast,
            EditError::FileLevelPathDuplicated { .. }
        ));
    }

    #[tokio::test]
    async fn edit_file_level_mixed_with_line_edits_rejected() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![
                    LlmEdit::Overwrite {
                        text: "fresh".into(),
                    },
                    LlmEdit::InsertAfter {
                        anchor: target,
                        text: "X".into(),
                    },
                ],
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        let downcast = err.downcast_ref::<EditError>().unwrap();
        assert!(matches!(downcast, EditError::FileLevelEditNotAlone { .. }));
    }

    #[tokio::test]
    async fn end_turn_clears_tombstones() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                edits: vec![LlmEdit::Replace {
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "A2".into(),
                }],
            }],
        };
        session.edit(input, no_format).await.unwrap();

        let used_before = session.engine.store().used_anchors(&path).await.unwrap();
        assert_eq!(used_before.len(), 3); // a (tombstoned) + A2 + b

        session.end_turn().await.unwrap();
        let used_after = session.engine.store().used_anchors(&path).await.unwrap();
        assert_eq!(used_after.len(), 2); // tombstones gone; A2 + b remain
    }
}
