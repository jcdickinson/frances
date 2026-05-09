use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use frances_edit::{
    Anchor, AnchorParseError, AnchorStore, EditEngine, EditHints, EditOp, FileAnchorState, Pool,
    Truncated, WorkingFile, apply_ops, reconcile, render_diff_block, render_file,
};
use thiserror::Error;

use crate::Result;

const DIFF_CONTEXT: usize = 2;
const ANCHOR_SEP: char = '§';

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

    /// Tool: file_read. Caller supplies the file's current content;
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

    /// Apply one structured edit. Dispatches on variant:
    /// - `New` writes a fresh file (must not exist on disk).
    /// - `Overwrite` replaces a previously-read file's content (requires
    ///   prior `read_file` for the up-to-date-read safety net).
    /// - `Replace`/`InsertAfter`/`InsertBefore` resolve anchors against the
    ///   cached anchored state, replay one `EditOp` into a draft, write via
    ///   `on_draft`, then reconcile. Path must be cached via `read_file`.
    ///
    /// Returns the rendered diff block (or full anchored file in the `New`
    /// case) as plain text.
    pub async fn edit<F>(&mut self, edit: LlmEdit, mut on_draft: F) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        match edit {
            LlmEdit::New { path, text } => self.apply_new(&path, &text, &mut on_draft).await,
            LlmEdit::Overwrite { path, text } => {
                self.apply_overwrite(&path, &text, &mut on_draft).await
            }
            LlmEdit::Replace {
                path,
                anchor,
                end_anchor,
                text,
            } => {
                let working = self.cached_working(&path)?;
                let (from_anchor, from_idx) =
                    resolve_anchor(&anchor, &working.state, &working.lines, &path)?;
                let (to_anchor, to_idx) =
                    resolve_anchor(&end_anchor, &working.state, &working.lines, &path)?;
                if to_idx < from_idx {
                    return Err(EditError::BackwardsReplaceRange {
                        start: from_idx,
                        end: to_idx,
                    }
                    .into());
                }
                let new_lines = split_text_to_lines(&text);
                let tombstones: Vec<Anchor> = working.state.lines
                    [from_idx as usize..=to_idx as usize]
                    .iter()
                    .map(|le| le.anchor.clone())
                    .collect();
                let op = EditOp::Replace {
                    from: from_anchor,
                    to: to_anchor,
                    lines: new_lines,
                };
                self.apply_line_edit(&path, working, op, tombstones, &mut on_draft)
                    .await
            }
            LlmEdit::InsertAfter { path, anchor, text } => {
                let working = self.cached_working(&path)?;
                let (pin, _) = resolve_anchor(&anchor, &working.state, &working.lines, &path)?;
                let op = EditOp::InsertAfter {
                    pin,
                    lines: split_text_to_lines(&text),
                };
                self.apply_line_edit(&path, working, op, Vec::new(), &mut on_draft)
                    .await
            }
            LlmEdit::InsertBefore { path, anchor, text } => {
                let working = self.cached_working(&path)?;
                let (pin, _) = resolve_anchor(&anchor, &working.state, &working.lines, &path)?;
                let op = EditOp::InsertBefore {
                    pin,
                    lines: split_text_to_lines(&text),
                };
                self.apply_line_edit(&path, working, op, Vec::new(), &mut on_draft)
                    .await
            }
        }
    }

    fn cached_working(&self, path: &Path) -> Result<WorkingFile> {
        self.open_files.get(path).cloned().ok_or_else(|| {
            EditError::NotCached {
                path: path.to_path_buf(),
            }
            .into()
        })
    }

    /// Common pipeline for line-level edits: replay one `EditOp` into a
    /// draft, hand it to `on_draft`, reconcile against the cached state, and
    /// commit. `tombstones` is the pre-edit anchor list for any lines the
    /// op deletes (only `Replace` produces them).
    async fn apply_line_edit<F>(
        &mut self,
        path: &Path,
        working: WorkingFile,
        op: EditOp,
        tombstones: Vec<Anchor>,
        on_draft: &mut F,
    ) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        let ops = [op];
        let draft = apply_ops(&working.state, &working.lines, &ops);

        let (post_lines, mtime_ns, size) = on_draft(path, &draft)?;

        let used = self.engine.store().used_anchors(path).await?;
        let mut pool = Pool::from_used(used);
        let hints = EditHints {
            deleted_anchors: tombstones,
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
            .ok_or_else(|| EditError::NotCachedForOverwrite {
                path: path.to_path_buf(),
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
        self.engine.end_turn().await?;
        Ok(())
    }
}

/// Split a model-supplied `text` payload into a draft line array. Mirrors
/// `apply_ops`: callers split on `\n` exactly so the on-disk line count
/// matches what the model wrote.
fn split_text_to_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
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

        let block = session
            .edit(
                LlmEdit::Replace {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "B2".into(),
                },
                no_format,
            )
            .await
            .unwrap();
        assert!(block.contains("§B2"));
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

        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "X\nY".into(),
                },
                no_format,
            )
            .await
            .unwrap();
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

        session
            .edit(
                LlmEdit::InsertBefore {
                    path: path.clone(),
                    anchor: target,
                    text: "X".into(),
                },
                no_format,
            )
            .await
            .unwrap();
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
        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: format!("{unused}§a"),
                    text: "X".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Edit(EditError::AnchorNotFound { .. })
        ));
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
        session
            .edit(
                LlmEdit::Replace {
                    path: path.clone(),
                    anchor: trimmed_match.clone(),
                    end_anchor: trimmed_match,
                    text: "world".into(),
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a".to_string(), "world".to_string(), "b".to_string()]
        );

        // Genuinely different content fails.
        let working = session.open_files.get(&path).unwrap();
        let anchor_word = working.state.lines[1].anchor.clone();
        let wrong = format!("{anchor_word}§not the real content");
        let err = session
            .edit(
                LlmEdit::Replace {
                    path: path.clone(),
                    anchor: wrong.clone(),
                    end_anchor: wrong,
                    text: "x".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Edit(EditError::ContentMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn edit_malformed_anchor_field() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a"), 100, 1)
            .await
            .unwrap();

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: "no-section-sigil-here".into(),
                    text: "X".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Edit(EditError::MalformedAnchor { .. })
        ));
    }

    #[tokio::test]
    async fn edit_uncached_file_errors() {
        let mut session = fresh_session();
        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: "/uncached".into(),
                    anchor: "Apple§a".into(),
                    text: "X".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
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

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: "MissingWord§a".into(),
                    text: "X".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::Error::Edit(_)));

        // Cache survives. A well-formed retry against the same anchors works.
        let target = anchor_field(&session, &path, 1);
        session
            .edit(
                LlmEdit::Replace {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "B2".into(),
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(session.open_files[&path].lines, vec!["a", "B2", "c"]);
    }

    #[tokio::test]
    async fn edit_new_creates_file_and_caches_anchors() {
        let mut session = fresh_session();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brand_new.txt");

        let block = session
            .edit(
                LlmEdit::New {
                    path: path.clone(),
                    text: "alpha\nbeta".into(),
                },
                no_format,
            )
            .await
            .unwrap();

        assert!(block.contains("§alpha"));
        assert!(block.contains("§beta"));
        // Diff vs empty pre-state ⇒ every line emitted as `+`.
        let plus_lines = block.lines().filter(|l| l.starts_with('+')).count();
        assert_eq!(plus_lines, 2);

        let cached = session.open_files.get(&path).expect("cached after new");
        assert_eq!(cached.lines, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn edit_new_on_existing_file_errors() {
        let mut session = fresh_session();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.txt");
        std::fs::write(&path, "preexisting\n").unwrap();

        let err = session
            .edit(
                LlmEdit::New {
                    path: path.clone(),
                    text: "x".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Edit(EditError::NewFileExists { .. })
        ));
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

        session
            .edit(
                LlmEdit::Overwrite {
                    path: path.clone(),
                    text: "x\ny\nz".into(),
                },
                no_format,
            )
            .await
            .unwrap();

        assert_eq!(session.open_files[&path].lines, vec!["x", "y", "z"]);

        let used = session.engine.store().used_anchors(&path).await.unwrap();
        for old in &old_anchors {
            assert!(used.contains(old), "old anchor not tombstoned: {old}");
        }
    }

    #[tokio::test]
    async fn edit_overwrite_without_read_errors() {
        let mut session = fresh_session();
        let err = session
            .edit(
                LlmEdit::Overwrite {
                    path: "/never-read".into(),
                    text: "x".into(),
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not cached"));
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

        session
            .edit(
                LlmEdit::Replace {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "A2".into(),
                },
                no_format,
            )
            .await
            .unwrap();

        let used_before = session.engine.store().used_anchors(&path).await.unwrap();
        assert_eq!(used_before.len(), 3); // a (tombstoned) + A2 + b

        session.end_turn().await.unwrap();
        let used_after = session.engine.store().used_anchors(&path).await.unwrap();
        assert_eq!(used_after.len(), 2); // tombstones gone; A2 + b remain
    }
}
