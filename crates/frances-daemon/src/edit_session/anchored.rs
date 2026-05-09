use std::path::Path;
use std::str::FromStr;

use frances_edit::{
    Anchor, AnchorStore, EditHints, EditOp, FileAnchorState, Pool, Truncated, WorkingFile,
    apply_ops, reconcile, render_diff_block,
};

use super::types::EditError;
use super::{DIFF_CONTEXT, EditSession, split_text_to_lines};
use crate::Result;

const ANCHOR_SEP: char = '§';

impl<S: AnchorStore> EditSession<S> {
    pub(super) async fn apply_replace<F>(
        &mut self,
        path: &Path,
        anchor: &str,
        end_anchor: &str,
        text: &str,
        on_draft: &mut F,
    ) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        let working = self.cached_working(path)?;
        let (from_anchor, from_idx) = resolve_anchor(anchor, &working.state, &working.lines, path)?;
        let (to_anchor, to_idx) = resolve_anchor(end_anchor, &working.state, &working.lines, path)?;
        if to_idx < from_idx {
            return Err(EditError::BackwardsReplaceRange {
                start: from_idx,
                end: to_idx,
            }
            .into());
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

    pub(super) async fn apply_insert_after<F>(
        &mut self,
        path: &Path,
        anchor: &str,
        text: &str,
        on_draft: &mut F,
    ) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
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
        on_draft: &mut F,
    ) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        let working = self.cached_working(path)?;
        let (pin, _) = resolve_anchor(anchor, &working.state, &working.lines, path)?;
        let op = EditOp::InsertBefore {
            pin,
            lines: split_text_to_lines(text),
        };
        self.apply_line_edit(path, working, op, Vec::new(), on_draft)
            .await
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

    use frances_edit::{AnchorStore, FakeStore};

    use super::super::test_support::{fresh_session, lines_of, no_format};
    use super::*;
    use crate::edit_session::LlmEdit;

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
