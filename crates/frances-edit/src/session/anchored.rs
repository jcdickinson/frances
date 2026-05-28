use std::io;
use std::path::Path;
use std::str::FromStr;

use regex::Regex;

use crate::render::{DiffRender, render_diff_block};
use crate::{
    Anchor, AnchorStore, EditHints, EditOp, FileAnchorState, Pool, Truncated, WorkingFile,
    apply_ops, reconcile,
};

use super::types::{EditError, EditResult};
use super::{DIFF_CONTEXT, EditSession, detect_anchor_pasteback, split_text_to_lines};

const ANCHOR_SEP: char = '§';

impl<S: AnchorStore> EditSession<S> {
    pub(super) async fn apply_replace<F>(
        &mut self,
        path: &Path,
        anchor: &str,
        end_anchor: &str,
        text: &str,
        bypass_anchor_guard: bool,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
    {
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
        self.apply_line_edit(path, working, op, tombstones, on_draft)
            .await
    }

    pub(super) async fn apply_replace_all<F>(
        &mut self,
        path: &Path,
        find: &str,
        replacement: &str,
        count: Option<usize>,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
    {
        let working = self.cached_working(path)?;
        let re = Regex::new(find)?;
        let content = working.lines.join("\n");
        let actual = re.find_iter(&content).count();
        if let Some(limit) = count
            && actual > limit
        {
            return Err(EditError::ReplaceAllCountExceeded { actual, limit });
        }
        if actual == 0 {
            return Ok(DiffRender {
                text: format!("No changes: regex matched 0 times in {}", path.display()),
                ops: Vec::new(),
            });
        }

        let replaced = re.replace_all(&content, replacement).into_owned();
        let draft = split_text_to_lines(&replaced);
        self.apply_draft_edit(path, working, draft, None, on_draft)
            .await
    }

    pub(super) async fn apply_insert_after<F>(
        &mut self,
        path: &Path,
        anchor: &str,
        text: &str,
        bypass_anchor_guard: bool,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
    {
        if !bypass_anchor_guard && let Some(anchors) = detect_anchor_pasteback(text) {
            return Err(EditError::AnchorPastebackDetected { anchors });
        }
        let working = self.cached_working(path)?;
        let (pin, _) = resolve_anchor(anchor, &working.state, &working.lines, path)?;
        let op = EditOp::InsertAfter {
            pin,
            lines: split_text_to_lines(text),
        };
        self.apply_line_edit(path, working, op, Vec::new(), on_draft)
            .await
    }

    pub(super) async fn apply_insert_before<F>(
        &mut self,
        path: &Path,
        anchor: &str,
        text: &str,
        bypass_anchor_guard: bool,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
    {
        if !bypass_anchor_guard && let Some(anchors) = detect_anchor_pasteback(text) {
            return Err(EditError::AnchorPastebackDetected { anchors });
        }
        let working = self.cached_working(path)?;
        let (pin, _) = resolve_anchor(anchor, &working.state, &working.lines, path)?;
        let op = EditOp::InsertBefore {
            pin,
            lines: split_text_to_lines(text),
        };
        self.apply_line_edit(path, working, op, Vec::new(), on_draft)
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
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
    {
        let ops = [op];
        let draft = apply_ops(&working.state, &working.lines, &ops);
        let hints = EditHints {
            deleted_anchors: tombstones,
        };
        self.apply_draft_edit(path, working, draft, Some(&hints), on_draft)
            .await
    }
    async fn apply_draft_edit<F>(
        &mut self,
        path: &Path,
        working: WorkingFile,
        draft: Vec<String>,
        hints: Option<&EditHints>,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
    {
        let (post_lines, mtime_ns, size) = on_draft(path, &draft)?;

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

        // Genuinely different content fails.
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
        // Same contract as edit_validation_failure_preserves_cache:
        // a guard rejection must not corrupt the session cache.
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

        // Retry with clean content succeeds.
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
                |_path, _draft| {
                    wrote = true;
                    no_format(_path, _draft)
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
                |_path, _draft| {
                    wrote = true;
                    no_format(_path, _draft)
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
