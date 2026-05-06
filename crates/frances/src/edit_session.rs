use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use frances_edit::{
    AnchorStore, EditEngine, EditHints, ParsedPatch, PatchParseError, Pool, WorkingFile, apply_ops,
    parse_patch, reconcile, render_diff_block, render_file,
};
use serde::Deserialize;

const DIFF_CONTEXT: usize = 2;

#[derive(Deserialize, Debug)]
pub struct EditInput {
    pub files: Vec<EditFileEntry>,
}

#[derive(Deserialize, Debug)]
pub struct EditFileEntry {
    pub path: PathBuf,
    pub patch: String,
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

    pub fn engine(&self) -> &EditEngine<S> {
        &self.engine
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

    /// Tool: edit. For each file in `input`, parses the patch against the
    /// cached state, replays it into a draft, hands the draft to `on_draft`
    /// (which writes to disk, runs any formatter, and returns the post-format
    /// content + metadata), then reconciles and commits. Returns the
    /// concatenated diff blocks (one per file) as plain text.
    ///
    /// Each path in `input` must already be cached via `read_file` — the
    /// session is filesystem-agnostic and will not load uncached files itself.
    pub async fn edit<F>(&mut self, input: EditInput, mut on_draft: F) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        let mut blocks = Vec::new();
        for entry in input.files {
            let path = entry.path;

            // Borrow the cached file rather than removing it: a parse failure
            // or formatter error must not destroy the cache, otherwise the
            // model's next retry hits a confusing "not cached" message.
            let working = self
                .open_files
                .get(&path)
                .ok_or_else(|| anyhow!("{} is not cached; call read_file first", path.display()))?
                .clone();

            let planned = plan_edit(&working, &entry.patch)?;

            let (post_lines, mtime_ns, size) = on_draft(&path, &planned.draft)?;

            let used = self.engine.store().used_anchors(&path).await?;
            let mut pool = Pool::from_used(used);
            let hints = EditHints {
                deleted_anchors: planned.parsed.deleted,
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

    /// End-of-turn cleanup. Caller invokes when assistant message's tool
    /// calls are fully processed.
    pub async fn end_turn(&mut self) -> Result<()> {
        self.engine.end_turn().await
    }
}

#[derive(Debug)]
pub struct PlannedEdit {
    pub parsed: ParsedPatch,
    pub draft: Vec<String>,
}

/// Pure: parse a patch against a working file and replay it into a draft.
/// No I/O, no commit. Useful for callers wanting finer-grained control than
/// `EditSession::edit` provides.
pub fn plan_edit(working: &WorkingFile, patch: &str) -> Result<PlannedEdit, PatchParseError> {
    let parsed = parse_patch(patch, &working.state, &working.lines, &HashSet::new())?;
    let draft = apply_ops(&working.state, &working.lines, &parsed.ops);
    Ok(PlannedEdit { parsed, draft })
}

/// Format a `PatchParseError` as a multi-line plain-text tool result error.
/// Designed for the model to read and self-correct.
pub fn format_error(err: &PatchParseError) -> String {
    match err {
        PatchParseError::ContentMismatch {
            line,
            anchor,
            actual,
            claimed,
        } => format!(
            "patch error: line {line}: anchor {anchor} content mismatch (trimmed):\n  expected: {actual}\n  got: {claimed}"
        ),
        PatchParseError::ExcludedAnchor { line, anchor } => format!(
            "patch error: line {line}: anchor {anchor} was tombstoned earlier this turn (you cannot reference it again)"
        ),
        other => format!("patch error: {other}"),
    }
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

    /// Mock `on_draft` for tests: pretends the formatter is a no-op (post = draft)
    /// and bumps mtime/size.
    fn no_format(_: &Path, draft: &[String]) -> Result<(Vec<String>, i64, u64)> {
        let size: u64 = draft.iter().map(|l| (l.len() + 1) as u64).sum();
        Ok((draft.to_vec(), 200, size))
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
        }
        assert!(session.open_files.contains_key(&path));
    }

    #[tokio::test]
    async fn plan_edit_pure_helper() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let working = session.open_files.get(&path).unwrap().clone();
        let anchor_a = working.state.lines[0].anchor.clone();
        let patch = format!(" {anchor_a}§a\n+§new\n");
        let planned = plan_edit(&working, &patch).unwrap();
        assert_eq!(planned.draft, vec!["a", "new", "b", "c"]);
        assert!(planned.parsed.deleted.is_empty());
    }

    #[tokio::test]
    async fn edit_round_trip_via_no_format_callback() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let anchor_b = session.open_files[&path].state.lines[1].anchor.clone();

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                patch: format!("-{anchor_b}§b\n+§B2\n"),
            }],
        };
        let blocks = session.edit(input, no_format).await.unwrap();

        assert!(blocks.contains(&format!("-{anchor_b}§b")));
        assert!(blocks.contains("§B2"));

        // The session cache should now reflect the post-edit state.
        let post = session.open_files.get(&path).unwrap();
        assert_eq!(post.lines, vec!["a", "B2", "c"]);
    }

    #[tokio::test]
    async fn edit_uncached_file_errors() {
        let mut session = fresh_session();
        let input = EditInput {
            files: vec![EditFileEntry {
                path: "/uncached".into(),
                patch: " From§a\n+§b\n".into(),
            }],
        };
        let err = session.edit(input, no_format).await.unwrap_err();
        assert!(err.to_string().contains("not cached"));
    }

    /// A parse failure on the patch must not destroy the cache — otherwise
    /// the model's next retry hits "not cached" and gets stuck.
    #[tokio::test]
    async fn edit_parse_error_preserves_cache() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();

        let bad = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                patch: "garbage with no sigil\n".into(),
            }],
        };
        let err = session.edit(bad, no_format).await.unwrap_err();
        assert!(err.to_string().contains("malformed"));

        // Cache survives. A well-formed retry against the same anchors works.
        let anchor_b = session.open_files[&path].state.lines[1].anchor.clone();
        let good = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                patch: format!("-{anchor_b}§b\n"),
            }],
        };
        session.edit(good, no_format).await.unwrap();
        assert_eq!(
            session.open_files[&path].lines,
            vec!["a".to_string(), "c".to_string()]
        );
    }

    #[tokio::test]
    async fn end_turn_clears_tombstones() {
        let mut session = fresh_session();
        let path = PathBuf::from("/x");
        session
            .read_file(path.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let anchor_a = session.open_files[&path].state.lines[0].anchor.clone();

        let input = EditInput {
            files: vec![EditFileEntry {
                path: path.clone(),
                patch: format!("-{anchor_a}§a\n+§A2\n"),
            }],
        };
        session.edit(input, no_format).await.unwrap();

        let used_before = session.engine.store().used_anchors(&path).await.unwrap();
        assert!(used_before.contains(&anchor_a)); // tombstoned but still in used set

        session.end_turn().await.unwrap();
        let used_after = session.engine.store().used_anchors(&path).await.unwrap();
        assert!(!used_after.contains(&anchor_a));
    }

    #[test]
    fn format_error_content_mismatch_is_multiline() {
        use frances_edit::Anchor;
        let err = PatchParseError::ContentMismatch {
            line: 5,
            anchor: Anchor::first(),
            actual: frances_edit::Truncated::new("def foo():"),
            claimed: frances_edit::Truncated::new("def bar():"),
        };
        let s = format_error(&err);
        assert!(s.contains("line 5"));
        assert!(s.contains("expected: def foo():"));
        assert!(s.contains("got: def bar():"));
    }

    #[test]
    fn format_error_other_is_one_line() {
        let err = PatchParseError::UnpinnedInsert { line: 3 };
        let s = format_error(&err);
        assert!(s.contains("line 3"));
        assert!(!s.contains('\n'));
    }
}
