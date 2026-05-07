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

    /// Tool: edit. For each file in `input`, validates each structured edit
    /// against the cached state, builds internal `EditOp`s, replays them into
    /// a draft, hands the draft to `on_draft` (which writes to disk, optionally
    /// runs a formatter, and returns the post-format content + metadata), then
    /// reconciles and commits. Returns the concatenated diff blocks (one per
    /// file) as plain text.
    ///
    /// Each path in `input` must already be cached via `read_file`.
    pub async fn edit<F>(&mut self, input: EditInput, mut on_draft: F) -> Result<String>
    where
        F: FnMut(&Path, &[String]) -> Result<(Vec<String>, i64, u64)>,
    {
        let mut blocks = Vec::new();
        for entry in input.files {
            let path = entry.path;

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

    /// End-of-turn cleanup. Caller invokes when assistant message's tool
    /// calls are fully processed.
    pub async fn end_turn(&mut self) -> Result<()> {
        self.engine.end_turn().await
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
