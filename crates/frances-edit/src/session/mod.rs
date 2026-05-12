use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::{AnchorStore, EditEngine, WorkingFile, render_file};

mod anchored;
mod types;
mod whole_file;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

pub use types::{EditError, EditResult, LlmEdit};

const DIFF_CONTEXT: usize = 2;

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
    ) -> EditResult<String> {
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
    pub async fn edit<F>(&mut self, edit: LlmEdit, mut on_draft: F) -> EditResult<String>
    where
        F: FnMut(&Path, &[String]) -> io::Result<(Vec<String>, i64, u64)>,
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
                self.apply_replace(&path, &anchor, &end_anchor, &text, &mut on_draft)
                    .await
            }
            LlmEdit::InsertAfter { path, anchor, text } => {
                self.apply_insert_after(&path, &anchor, &text, &mut on_draft)
                    .await
            }
            LlmEdit::InsertBefore { path, anchor, text } => {
                self.apply_insert_before(&path, &anchor, &text, &mut on_draft)
                    .await
            }
        }
    }

    /// End-of-turn cleanup. Caller invokes when assistant message's tool
    /// calls are fully processed.
    pub async fn end_turn(&mut self) -> EditResult<()> {
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

#[cfg(test)]
mod tests {
    use super::test_support::{fresh_session, lines_of};
    use super::*;

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
}
