use std::io;
use std::path::Path;

use crate::render::{DiffRender, render_diff_block};
use crate::{AnchorStore, EditHints, FileAnchorState, Pool, WorkingFile, reconcile};

use super::types::{EditError, EditResult, WriteMode};
use super::{DIFF_CONTEXT, EditSession, detect_anchor_pasteback, split_text_to_lines};

impl<S: AnchorStore> EditSession<S> {
    /// Create a brand-new file. Fails if the file already exists on disk.
    /// Mints fresh anchors for every line and renders a diff against an
    /// empty pre-state (all `+` lines).
    pub(super) async fn apply_new<F>(
        &mut self,
        path: &Path,
        text: &str,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String], WriteMode) -> io::Result<(Vec<String>, i64, u64)>,
    {
        let draft = split_text_to_lines(text);
        // No `path.exists()` precheck — not race-free. The drafter opens with
        // `create_new` so a race surfaces as `AlreadyExists` and maps to the
        // typed error below.
        let (post_lines, mtime_ns, size) =
            on_draft(path, &draft, WriteMode::CreateNew).map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => EditError::NewFileExists {
                    path: path.to_path_buf(),
                },
                _ => EditError::Draft(e),
            })?;
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
    /// the cache. Tombstones every prior anchor and mints fresh ones via the
    /// normal reconcile path.
    pub(super) async fn apply_overwrite<F>(
        &mut self,
        path: &Path,
        text: &str,
        bypass_anchor_guard: bool,
        on_draft: &mut F,
    ) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String], WriteMode) -> io::Result<(Vec<String>, i64, u64)>,
    {
        if !bypass_anchor_guard && let Some(anchors) = detect_anchor_pasteback(text) {
            return Err(EditError::AnchorPastebackDetected { anchors });
        }
        let working = self
            .open_files
            .get(path)
            .ok_or_else(|| EditError::NotCachedForOverwrite {
                path: path.to_path_buf(),
            })?
            .clone();
        let draft = split_text_to_lines(text);
        let (post_lines, mtime_ns, size) = on_draft(path, &draft, WriteMode::Overwrite)?;
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
}

/// Empty pre-state for diffing newly-created files.
fn empty_state(path: &Path) -> FileAnchorState {
    FileAnchorState {
        path: path.to_path_buf(),
        mtime_ns: 0,
        size: 0,
        content_digest: 0,
        lines: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::test_support::{fresh_session, lines_of, no_format};
    use super::super::{EditError, LlmEdit};
    use super::*;

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

        assert!(block.text.contains("§alpha"));
        assert!(block.text.contains("§beta"));
        let plus_lines = block.text.lines().filter(|l| l.starts_with('+')).count();
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
        assert!(matches!(err, EditError::NewFileExists { .. }));
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
                    bypass_anchor_guard: false,
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
    async fn edit_overwrite_anchor_pasteback_rejected() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();

        let err = session
            .edit(
                LlmEdit::Overwrite {
                    path: path.clone(),
                    text: "Apple§foo\nBanana§bar".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::AnchorPastebackDetected { .. }));
        assert_eq!(session.open_files[&path].lines, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn edit_overwrite_anchor_pasteback_override_allows_through() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();

        session
            .edit(
                LlmEdit::Overwrite {
                    path: path.clone(),
                    text: "Apple§foo\nBanana§bar".into(),
                    bypass_anchor_guard: true,
                },
                no_format,
            )
            .await
            .unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["Apple§foo", "Banana§bar"]
        );
    }

    #[tokio::test]
    async fn edit_overwrite_without_read_errors() {
        let mut session = fresh_session();
        let err = session
            .edit(
                LlmEdit::Overwrite {
                    path: "/never-read".into(),
                    text: "x".into(),
                    bypass_anchor_guard: false,
                },
                no_format,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not cached"));
    }
}
