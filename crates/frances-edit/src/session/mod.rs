use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use regex::Regex;

use crate::loop_guard::LoopSet;
use crate::render::DiffRender;
use crate::{AnchorStore, EditEngine, LoopKey, WorkingFile, render_file};

mod anchored;
mod types;
mod whole_file;

use anchored::PinPosition;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

pub use types::{EditError, EditResult, LlmEdit, WriteMode};

const DIFF_CONTEXT: usize = 2;

pub struct EditSession<S: AnchorStore> {
    /// Shared anchor engine. Every method on it takes `&self`, so contexts
    /// share one engine freely; the anchor/tombstone state it owns is
    /// per-workspace, not per-context.
    engine: Arc<EditEngine<S>>,
    /// Per-context read cache: which files have been read here, plus the
    /// anchored snapshot each edit validates against. Empty in a fresh
    /// context, so an edit again requires a `read_file` in this context.
    open_files: HashMap<PathBuf, WorkingFile>,
    /// Per-context anti-repeat guard for reads and searches (cleared on edit).
    loop_set: LoopSet,
}

impl<S: AnchorStore> EditSession<S> {
    /// New read context over a shared anchor engine. The read cache and loop
    /// guard start empty, so "have I read this here?" tracks the live context
    /// rather than the whole workflow lifetime; the engine's persistent
    /// anchor state is shared across contexts.
    pub fn new(engine: Arc<EditEngine<S>>) -> Self {
        Self {
            engine,
            open_files: HashMap::new(),
            loop_set: LoopSet::default(),
        }
    }

    /// True if `key` matches an entry recorded since the last write.
    pub fn is_loop(&self, key: &LoopKey) -> bool {
        self.loop_set.contains(key)
    }

    /// Insert `key` into the loop-guard set.
    pub fn record_loop(&mut self, key: LoopKey) {
        self.loop_set.record(key);
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
    /// case) — `DiffRender` carries both the LLM-facing string and the
    /// structured ops shipped to the TUI.
    pub async fn edit<F>(&mut self, edit: LlmEdit, mut on_draft: F) -> EditResult<DiffRender>
    where
        F: FnMut(&Path, &[String], WriteMode) -> io::Result<(Vec<String>, i64, u64)>,
    {
        // Any edit mutates the workspace; previously-cached read/search
        // results are no longer the source of truth, so the loop-guard
        // set resets here. Cleared up front so a failed edit still
        // unblocks subsequent reads — failure is itself new information.
        self.loop_set.clear();
        match edit {
            LlmEdit::New { path, text } => self.apply_new(&path, &text, &mut on_draft).await,
            LlmEdit::Overwrite {
                path,
                text,
                bypass_anchor_guard,
            } => {
                self.apply_overwrite(&path, &text, bypass_anchor_guard, &mut on_draft)
                    .await
            }
            LlmEdit::ReplaceLines {
                path,
                anchor,
                end_anchor,
                text,
                bypass_anchor_guard,
            } => {
                self.apply_replace(
                    &path,
                    &anchor,
                    &end_anchor,
                    &text,
                    bypass_anchor_guard,
                    &mut on_draft,
                )
                .await
            }
            LlmEdit::ReplaceAll {
                path,
                find,
                replacement,
                count,
            } => {
                self.apply_replace_all(&path, &find, &replacement, count, &mut on_draft)
                    .await
            }
            LlmEdit::InsertAfter {
                path,
                anchor,
                text,
                bypass_anchor_guard,
            } => {
                self.apply_pin_insert(
                    &path,
                    &anchor,
                    &text,
                    PinPosition::After,
                    bypass_anchor_guard,
                    &mut on_draft,
                )
                .await
            }
            LlmEdit::InsertBefore {
                path,
                anchor,
                text,
                bypass_anchor_guard,
            } => {
                self.apply_pin_insert(
                    &path,
                    &anchor,
                    &text,
                    PinPosition::Before,
                    bypass_anchor_guard,
                    &mut on_draft,
                )
                .await
            }
        }
    }

    /// Commit accumulated edits (clears anchor tombstones).
    pub async fn commit_edits(&mut self) -> EditResult<()> {
        self.engine.commit_edits().await?;
        Ok(())
    }
}

/// Split a model-supplied `text` payload into a draft line array.
/// Callers split on `\n` exactly so the on-disk line count matches what the model wrote.
fn split_text_to_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

/// Detect "anchor paste-back" in an edit `text` payload: every non-blank
/// line begins with an anchor-shaped prefix like `Apple§` or
/// `Apple-Banana§`. Returns the comma-joined deduped anchor words (capped
/// at 20) when the pattern is uniform; `None` otherwise. A partial match
/// (one prefixed line among unprefixed ones) is evidence *against*
/// paste-back, so the gate requires uniformity across non-blank lines.
fn detect_anchor_pasteback(text: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let re = PATTERN.get_or_init(|| Regex::new(r"^([A-Z][\w-]*)§").unwrap());
    let mut non_blank_lines = 0usize;
    let mut matches = 0usize;
    let mut anchors: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        non_blank_lines += 1;
        if let Some(caps) = re.captures(line) {
            matches += 1;
            let word = caps.get(1).unwrap().as_str();
            if anchors.len() < 20 && !anchors.iter().any(|a| a == word) {
                anchors.push(word.to_owned());
            }
        }
    }
    if non_blank_lines > 0 && non_blank_lines == matches {
        Some(anchors.join(", "))
    } else {
        None
    }
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
