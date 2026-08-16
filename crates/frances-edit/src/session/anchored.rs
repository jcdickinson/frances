use std::borrow::Cow;
use std::path::Path;
use std::str::FromStr;

use frances_anchors::hash_lines;
use regex::Regex;

use crate::render::{DiffRender, render_diff_block};
use crate::state::{LineEntry, content_digest};
use crate::{
    Anchor, AnchorStore, EditHints, EditOp, FileAnchorState, LineOrigin, Pool, Truncated,
    WorkingFile, apply_ops, reconcile,
};

use super::types::{EditError, EditResult, WriteMode};
use super::{DIFF_CONTEXT, DraftWriter, EditSession, detect_anchor_pasteback, split_text_to_lines};

const ANCHOR_SEP: char = '§';

/// Which side of the pinned anchor a `file_insert` lands on. Selects the
/// `EditOp::Insert{After,Before}` variant in one match — the two insert
/// paths are otherwise identical.
#[derive(Clone, Copy)]
pub(super) enum PinPosition {
    Before,
    After,
}

impl<S: AnchorStore> EditSession<S> {
    pub(super) async fn apply_replace<W: DraftWriter>(
        &mut self,
        path: &Path,
        anchor: &str,
        end_anchor: &str,
        text: &str,
        bypass_anchor_guard: bool,
        writer: &mut W,
    ) -> EditResult<DiffRender> {
        if !bypass_anchor_guard && let Some(anchors) = detect_anchor_pasteback(text) {
            return Err(EditError::AnchorPastebackDetected { anchors });
        }
        let working = self.cached_working(path)?;
        let (from_anchor, from_idx) = resolve_anchor(anchor, &working.state, &working.lines, path)?;
        let (to_anchor, to_idx) = resolve_anchor(end_anchor, &working.state, &working.lines, path)?;
        if to_idx < from_idx {
            return Err(EditError::BackwardsReplaceRange {
                start: from_idx,
                end: to_idx,
            });
        }
        let new_lines = split_text_to_lines(text);
        let tombstones: Vec<Anchor> = working.state.lines[from_idx as usize..=to_idx as usize]
            .iter()
            .map(|le| le.anchor.clone())
            .collect();
        let op = EditOp::Replace {
            from: from_anchor,
            to: to_anchor,
            lines: new_lines,
        };
        self.apply_line_edit(path, working, op, tombstones, writer)
            .await
    }

    pub(super) async fn apply_replace_all<W: DraftWriter>(
        &mut self,
        path: &Path,
        find: &str,
        replacement: &str,
        count: Option<usize>,
        writer: &mut W,
    ) -> EditResult<DiffRender> {
        let working = self.cached_working(path)?;
        let re = Regex::new(find)?;
        let content = working.lines.join("\n");

        // The count cap (`ReplaceAllCountExceeded`) needs the match total up
        // front to reject without writing. With no cap there's nothing to
        // enforce, so we skip the scan entirely: `replace_all` returns
        // `Cow::Borrowed` iff it made zero replacements, which is exactly the
        // "No changes" signal — one pass instead of count-then-replace.
        if let Some(limit) = count {
            let actual = re.find_iter(&content).count();
            if actual > limit {
                return Err(EditError::ReplaceAllCountExceeded { actual, limit });
            }
        }

        let replaced = re.replace_all(&content, replacement);
        if matches!(replaced, Cow::Borrowed(_)) {
            return Ok(DiffRender {
                text: format!("No changes: regex matched 0 times in {}", path.display()),
                ops: Vec::new(),
            });
        }

        let draft = split_text_to_lines(&replaced);
        self.apply_draft_edit(path, working, draft, None, writer)
            .await
    }

    pub(super) async fn apply_pin_insert<W: DraftWriter>(
        &mut self,
        path: &Path,
        anchor: &str,
        text: &str,
        position: PinPosition,
        bypass_anchor_guard: bool,
        writer: &mut W,
    ) -> EditResult<DiffRender> {
        if !bypass_anchor_guard && let Some(anchors) = detect_anchor_pasteback(text) {
            return Err(EditError::AnchorPastebackDetected { anchors });
        }
        let working = self.cached_working(path)?;
        let (pin, _) = resolve_anchor(anchor, &working.state, &working.lines, path)?;
        let lines = split_text_to_lines(text);
        let op = match position {
            PinPosition::After => EditOp::InsertAfter { pin, lines },
            PinPosition::Before => EditOp::InsertBefore { pin, lines },
        };
        self.apply_line_edit(path, working, op, Vec::new(), writer)
            .await
    }

    fn cached_working(&self, path: &Path) -> EditResult<WorkingFile> {
        self.open_files
            .get(path)
            .cloned()
            .ok_or_else(|| EditError::NotCached {
                path: path.to_path_buf(),
            })
    }

    /// Pipeline for line-level edits (`Insert*` / `Replace`). We know exactly
    /// what changed, so we apply the op by *direct anchor transform* — carried
    /// lines keep their anchor by identity, inserted lines mint, deleted lines
    /// drop — rather than diffing the pre-edit file against the result. Diffing
    /// across the whole edit re-pairs duplicate / blank lines far from the edit
    /// and churns their anchors (see anchors.md "direct transforms for our own
    /// edits"). We still reconcile, but only the *draft → written-back* delta,
    /// to absorb whatever the auto-formatter changed; when nothing reformats,
    /// that reconcile is an all-equal no-op.
    ///
    /// `deleted` is the pre-edit anchor list for lines the op removes (only
    /// `Replace` produces them); they're gone from the draft, so the formatter
    /// reconcile can't see them and we tombstone them explicitly.
    async fn apply_line_edit<W: DraftWriter>(
        &mut self,
        path: &Path,
        working: WorkingFile,
        op: EditOp,
        deleted: Vec<Anchor>,
        writer: &mut W,
    ) -> EditResult<DiffRender> {
        let ops = [op];
        let (draft_lines, origins) = apply_ops(&working.state, &working.lines, &ops);
        let (post_lines, mtime_ns, size) = writer
            .write(path, &draft_lines, WriteMode::Overwrite)
            .await?;

        let used = self.engine.store().used_anchors(path).await?;
        let mut pool = Pool::from_used(used);

        // Direct transform → the draft's anchored state. Hashes are recomputed
        // for every line so blank-ordinal salting tracks new positions without
        // forcing a re-anchor.
        let draft_hashes = hash_lines(draft_lines.iter().map(|s| s.as_str()));
        let entries: Vec<LineEntry> = origins
            .iter()
            .zip(&draft_hashes)
            .map(|(origin, &hash)| {
                let anchor = match origin {
                    LineOrigin::Carried(i) => working.state.lines[*i as usize].anchor.clone(),
                    LineOrigin::Inserted => pool.mint(),
                };
                LineEntry { hash, anchor }
            })
            .collect();
        let draft_state = FileAnchorState {
            path: path.to_path_buf(),
            mtime_ns,
            size,
            content_digest: content_digest(&draft_hashes),
            lines: entries,
        };

        // Reconcile only the formatter's edits (draft → post_lines).
        let outcome = reconcile(&draft_state, &post_lines, &mut pool, None);
        let mut tombstones = deleted;
        tombstones.extend(outcome.tombstoned.iter().cloned());

        self.engine
            .commit(path, &outcome.state, mtime_ns, size, &tombstones)
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
    async fn apply_draft_edit<W: DraftWriter>(
        &mut self,
        path: &Path,
        working: WorkingFile,
        draft: Vec<String>,
        hints: Option<&EditHints>,
        writer: &mut W,
    ) -> EditResult<DiffRender> {
        let (post_lines, mtime_ns, size) = writer.write(path, &draft, WriteMode::Overwrite).await?;

        let used = self.engine.store().used_anchors(path).await?;
        let mut pool = Pool::from_used(used);
        let outcome = reconcile(&working.state, &post_lines, &mut pool, hints);

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
}

fn resolve_anchor(
    field: &str,
    state: &FileAnchorState,
    lines: &[String],
    path: &Path,
) -> Result<(Anchor, u32), EditError> {
    if let Some((first_line, _)) = field.split_once('\n') {
        return Err(EditError::MultilineAnchor {
            first_line: first_line.to_string(),
        });
    }
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
    use std::path::PathBuf;

    use crate::{AnchorStore, FakeStore};

    use super::super::test_support::{fresh_session, lines_of, no_format};
    use super::super::{EditError, LlmEdit};
    use super::*;

    fn anchor_field(s: &EditSession<FakeStore>, path: &Path, idx: usize) -> String {
        let working = s.open_files.get(path).expect("cached");
        format!(
            "{}{}{}",
            working.state.lines[idx].anchor, ANCHOR_SEP, working.lines[idx]
        )
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
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "B2".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert!(block.text.contains("§B2"));
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
                    bypass_anchor_guard: false,
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
                    bypass_anchor_guard: false,
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
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::AnchorNotFound { .. }));
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
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: trimmed_match.clone(),
                    end_anchor: trimmed_match,
                    text: "world".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a".to_string(), "world".to_string(), "b".to_string()]
        );

        let working = session.open_files.get(&path).unwrap();
        let anchor_word = working.state.lines[1].anchor.clone();
        let wrong = format!("{anchor_word}§not the real content");
        let err = session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: wrong.clone(),
                    end_anchor: wrong,
                    text: "x".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::ContentMismatch { .. }));
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
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::MalformedAnchor { .. }));
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
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not cached"));
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_rejected_uniform() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "Apple§foo\nBanana§bar".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        let EditError::AnchorPastebackDetected { anchors } = err else {
            panic!("expected AnchorPastebackDetected, got {err:?}");
        };
        assert!(anchors.contains("Apple"), "missing Apple in {anchors}");
        assert!(anchors.contains("Banana"), "missing Banana in {anchors}");
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_detector_is_structural() {
        // Invented anchor words (not in the file's live anchor set) still
        // trip the guard — the detector is structural, not membership-based.
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);
        let invented_a = unused_dict_word(&session, &path);

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: format!("{invented_a}§foo\n{invented_a}§bar"),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::AnchorPastebackDetected { .. }));
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_single_line_rejected() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "Apple§foo".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::AnchorPastebackDetected { .. }));
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_mixed_payload_passes() {
        // One coincidental `Word§` line among real content is evidence
        // *against* paste-back — the model is editing legitimately.
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "Apple§foo\nreal code here".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a", "Apple§foo", "real code here", "b"]
        );
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_blank_separators_dont_defeat() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "Apple§foo\n\nBanana§bar".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::AnchorPastebackDetected { .. }));
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_legit_content_passes() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "fn main() {}\n    let x = 1;".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a", "fn main() {}", "    let x = 1;", "b"]
        );
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_override_allows_through() {
        // With `bypass_anchor_guard: true`, anchor-shaped payload is
        // written verbatim — for the rare legitimate case of editing the
        // anchor engine itself, a test fixture, or prose.
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "Apple§foo\nBanana§bar".into(),
                    bypass_anchor_guard: true,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a", "Apple§foo", "Banana§bar", "b"]
        );
    }

    #[tokio::test]
    async fn edit_anchor_pasteback_preserves_cache() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        let err = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target.clone(),
                    text: "Apple§foo\nBanana§bar".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::AnchorPastebackDetected { .. }));

        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "real new line".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a", "real new line", "b", "c"]
        );
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
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::BadAnchorWord { .. }));

        // Cache survives. A well-formed retry against the same anchors works.
        let target = anchor_field(&session, &path, 1);
        session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "B2".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(session.open_files[&path].lines, vec!["a", "B2", "c"]);
    }

    #[tokio::test]
    async fn edit_replace_all_regex_happy_path() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("foo1\nfoo2\nbar"), 100, 14)
            .await
            .unwrap();

        let block = session
            .edit(
                LlmEdit::ReplaceAll {
                    path: path.clone(),
                    find: r"foo(\d)".into(),
                    replacement: "baz$1".into(),
                    count: None,
                },
                no_format,
            )
            .await
            .unwrap();

        assert!(block.text.contains("§baz1"), "block: {}", block.text);
        assert!(block.text.contains("§baz2"), "block: {}", block.text);
        assert_eq!(session.open_files[&path].lines, vec!["baz1", "baz2", "bar"]);
    }

    #[tokio::test]
    async fn edit_replace_all_zero_matches_is_noop() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("alpha\nbeta"), 100, 10)
            .await
            .unwrap();
        let before = session.open_files[&path].clone();
        let mut wrote = false;

        let msg = session
            .edit(
                LlmEdit::ReplaceAll {
                    path: path.clone(),
                    find: "gamma".into(),
                    replacement: "delta".into(),
                    count: None,
                },
                |path: &Path, draft: &[String], mode| {
                    wrote = true;
                    no_format(path, draft, mode)
                },
            )
            .await
            .unwrap();

        assert!(msg.text.contains("No changes"), "msg: {}", msg.text);
        assert!(!wrote, "zero-match replace_all must not write");
        assert_eq!(session.open_files[&path].lines, before.lines);
        assert_eq!(session.open_files[&path].state.lines, before.state.lines);
    }

    #[tokio::test]
    async fn edit_replace_all_count_cap_errors_without_writing() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\na\na"), 100, 5)
            .await
            .unwrap();
        let before = session.open_files[&path].clone();
        let mut wrote = false;

        let err = session
            .edit(
                LlmEdit::ReplaceAll {
                    path: path.clone(),
                    find: "a".into(),
                    replacement: "b".into(),
                    count: Some(2),
                },
                |path: &Path, draft: &[String], mode| {
                    wrote = true;
                    no_format(path, draft, mode)
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            EditError::ReplaceAllCountExceeded {
                actual: 3,
                limit: 2
            }
        ));
        assert!(err.to_string().contains("matched 3 times"));
        assert!(!wrote, "count-cap failure must not write");
        assert_eq!(session.open_files[&path].lines, before.lines);
    }

    #[tokio::test]
    async fn duplicate_content_lines_get_distinct_anchors() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(
                path.clone(),
                lines_of("a\n    #[test]\nb\n    #[test]\nc"),
                100,
                30,
            )
            .await
            .unwrap();

        let working = session.open_files.get(&path).unwrap();
        assert_ne!(working.state.lines[1].anchor, working.state.lines[3].anchor);
        assert_ne!(
            anchor_field(&session, &path, 1),
            anchor_field(&session, &path, 3)
        );
    }

    #[tokio::test]
    async fn replace_from_first_of_duplicate_lines_targets_first_instance() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(
                path.clone(),
                lines_of("a\n    #[test]\nb\n    #[test]\nc"),
                100,
                30,
            )
            .await
            .unwrap();

        let from = anchor_field(&session, &path, 1);
        let to = anchor_field(&session, &path, 2);
        let second_test_anchor = session.open_files[&path].state.lines[3].anchor.clone();

        session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: from,
                    end_anchor: to,
                    text: "X".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();

        assert_eq!(
            session.open_files[&path].lines,
            vec!["a", "X", "    #[test]", "c"]
        );
        assert_eq!(
            session.open_files[&path].state.lines[2].anchor,
            second_test_anchor
        );
    }

    #[tokio::test]
    async fn insert_after_first_of_identical_neighbors_lands_correctly() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("x\ndup\ndup\ny"), 100, 16)
            .await
            .unwrap();

        let first_dup = anchor_field(&session, &path, 1);
        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: first_dup,
                    text: "INS".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();

        assert_eq!(
            session.open_files[&path].lines,
            vec!["x", "dup", "INS", "dup", "y"]
        );
    }

    #[tokio::test]
    async fn content_mismatch_is_staleness_guard_not_ambiguity() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(
                path.clone(),
                lines_of("    #[test]\nfn a() {}\n    #[test]\nfn b() {}"),
                100,
                40,
            )
            .await
            .unwrap();

        let second_test = anchor_field(&session, &path, 2);
        assert!(second_test.ends_with("§    #[test]"));
        let word = second_test.split('§').next().unwrap().to_string();

        let err = session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: format!("{word}§    fn wrong() {{}}"),
                    end_anchor: format!("{word}§    fn wrong() {{}}"),
                    text: "x".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::ContentMismatch { .. }));

        session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: second_test.clone(),
                    end_anchor: second_test,
                    text: "    #[ignore]".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["    #[test]", "fn a() {}", "    #[ignore]", "fn b() {}"]
        );
    }

    #[tokio::test]
    async fn multiline_anchor_field_rejected_with_clear_error() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(
                path.clone(),
                lines_of("    #[test]\n    // comment\n    fn real() {}"),
                100,
                40,
            )
            .await
            .unwrap();

        let glued = format!(
            "{}\n{}",
            anchor_field(&session, &path, 0),
            anchor_field(&session, &path, 1)
        );
        let err = session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: glued,
                    end_anchor: anchor_field(&session, &path, 2),
                    text: "    #[ignore]".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();

        assert!(
            matches!(err, EditError::MultilineAnchor { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn edit_near_duplicate_blocks_does_not_reanchor_far_lines() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        // Two functions with byte-identical bodies: `    work();`, `    done();`
        // and `}` each appear twice, so their line hashes collide.
        let src = "fn a() {\n    work();\n    done();\n}\n\nfn b() {\n    work();\n    done();\n}";
        session
            .read_file(path.clone(), lines_of(src), 100, src.len() as u64)
            .await
            .unwrap();

        let pre: Vec<Anchor> = session.open_files[&path]
            .state
            .lines
            .iter()
            .map(|le| le.anchor.clone())
            .collect();

        // Insert into the FIRST function only.
        let target = anchor_field(&session, &path, 1); // `    work();` in fn a
        let block = session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "    extra();".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();

        // Post line array: extra() inserted at index 2; everything else shifts.
        let post = &session.open_files[&path].state.lines;
        assert_eq!(
            session.open_files[&path].lines,
            vec![
                "fn a() {",
                "    work();",
                "    extra();",
                "    done();",
                "}",
                "",
                "fn b() {",
                "    work();",
                "    done();",
                "}",
            ]
        );

        // Every original line must keep its original anchor. Post index i maps
        // to pre index i for i < 2, and i-1 for i > 2 (one line inserted at 2).
        let expected: Vec<&Anchor> = vec![
            &pre[0], &pre[1], /* 2 = minted */ &pre[2], &pre[3], &pre[4], &pre[5], &pre[6],
            &pre[7], &pre[8],
        ];
        let got: Vec<&Anchor> = post
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .map(|(_, le)| &le.anchor)
            .collect();
        assert_eq!(got, expected, "unchanged lines were re-anchored");

        // The diff block must not mention the untouched second function.
        assert!(
            !block.text.contains("fn b"),
            "diff names a far, unchanged line:\n{}",
            block.text
        );
    }

    #[tokio::test]
    async fn insert_with_blank_does_not_reanchor_downstream_blanks() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        // Isolated blanks at idx 1 and 3, between unique non-blank lines.
        session
            .read_file(path.clone(), lines_of("fn a\n\nfn b\n\nfn c"), 100, 16)
            .await
            .unwrap();

        let blank3 = session.open_files[&path].state.lines[3].anchor.clone();
        let fn_c = session.open_files[&path].state.lines[4].anchor.clone();

        // Insert a block that itself contains a blank line, bumping the
        // ordinal of every downstream blank.
        let target = anchor_field(&session, &path, 0); // `fn a`
        session
            .edit(
                LlmEdit::InsertAfter {
                    path: path.clone(),
                    anchor: target,
                    text: "x\n\ny".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();

        let post = &session.open_files[&path].state.lines;
        let lines = &session.open_files[&path].lines;
        assert_eq!(lines, &vec!["fn a", "x", "", "y", "", "fn b", "", "fn c"]);
        // `fn c` (unchanged, unique) keeps its anchor — sanity.
        assert_eq!(post[7].anchor, fn_c);
        // The blank originally at idx 3 is still present (now at idx 6) with
        // unchanged content. It must keep its anchor.
        assert_eq!(
            post[6].anchor, blank3,
            "downstream blank line was re-anchored by the whole-file reconcile"
        );
    }

    #[tokio::test]
    async fn replace_near_duplicate_block_does_not_reanchor_the_other_block() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        // Two byte-identical 3-line blocks, separated by a unique line.
        let src = "head\nlet x = 1;\nstep();\ndone();\nmid\nlet x = 1;\nstep();\ndone();\ntail";
        session
            .read_file(path.clone(), lines_of(src), 100, src.len() as u64)
            .await
            .unwrap();

        // Anchors of the SECOND block (idx 5,6,7) must be untouched.
        let second_block: Vec<Anchor> = session.open_files[&path].state.lines[5..8]
            .iter()
            .map(|le| le.anchor.clone())
            .collect();

        // Replace one line in the FIRST block.
        let target = anchor_field(&session, &path, 1); // first `let x = 1;`
        session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "let x = 2;".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();

        let post = &session.open_files[&path].state.lines;
        let got: Vec<Anchor> = post[5..8].iter().map(|le| le.anchor.clone()).collect();
        assert_eq!(
            got, second_block,
            "the untouched duplicate block was re-anchored"
        );
    }

    #[tokio::test]
    async fn commit_edits_clears_tombstones() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let target = anchor_field(&session, &path, 0);

        session
            .edit(
                LlmEdit::ReplaceLines {
                    path: path.clone(),
                    anchor: target.clone(),
                    end_anchor: target,
                    text: "A2".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap();

        let used_before = session.engine.store().used_anchors(&path).await.unwrap();
        assert_eq!(used_before.len(), 3); // a (tombstoned) + A2 + b

        session.commit_edits().await.unwrap();
        let used_after = session.engine.store().used_anchors(&path).await.unwrap();
        assert_eq!(used_after.len(), 2); // tombstones gone; A2 + b remain
    }
}
