use std::collections::HashSet;
use std::str::FromStr;

use thiserror::Error;

use crate::anchor::{Anchor, AnchorParseError};
use crate::edit::EditOp;
use crate::state::FileAnchorState;
use crate::truncated::Truncated;

const SEP: char = '§';
const SEP_LEN: usize = 2; // §  is 2 bytes in UTF-8

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PatchParseError {
    #[error("line {line}: malformed: {detail}")]
    Malformed { line: usize, detail: String },
    #[error("line {line}: bad anchor: {source}")]
    BadAnchor {
        line: usize,
        #[source]
        source: AnchorParseError,
    },
    #[error("line {line}: anchor {anchor} not found in file")]
    AnchorNotFound { line: usize, anchor: Anchor },
    #[error("line {line}: anchor {anchor} excluded (tombstoned earlier this turn)")]
    ExcludedAnchor { line: usize, anchor: Anchor },
    #[error("line {line}: hunk has no context or delete line to anchor inserts")]
    UnpinnedInsert { line: usize },
    #[error("line {line}: insert line must have empty anchor (use `+§content`)")]
    InsertAnchorMustBeEmpty { line: usize },
    #[error(
        "line {line}: anchor {anchor} content mismatch (trimmed): expected {actual}, got {claimed}"
    )]
    ContentMismatch {
        line: usize,
        anchor: Anchor,
        actual: Truncated,
        claimed: Truncated,
    },
}

#[derive(Debug, Clone)]
pub struct ParsedPatch {
    pub ops: Vec<EditOp>,
    /// Anchors removed by these ops; caller folds into per-turn tombstone set.
    pub deleted: Vec<Anchor>,
}

pub fn parse_patch(
    input: &str,
    state: &FileAnchorState,
    file_lines: &[String],
    excluded: &HashSet<Anchor>,
) -> Result<ParsedPatch, PatchParseError> {
    let mut ops: Vec<EditOp> = Vec::new();
    let mut deleted: Vec<Anchor> = Vec::new();
    let mut hunk: Vec<HunkLine> = Vec::new();

    for (idx, raw) in input.lines().enumerate() {
        let line_no = idx + 1;

        if raw.is_empty() {
            if !hunk.is_empty() {
                process_hunk(&hunk, &mut ops, &mut deleted)?;
                hunk.clear();
            }
            continue;
        }

        let parsed = parse_line(raw, line_no)?;

        // Validate anchor existence + content for context/delete lines.
        match &parsed {
            HunkLine::Context {
                anchor, content, ..
            }
            | HunkLine::Delete {
                anchor, content, ..
            } => {
                if excluded.contains(anchor) {
                    return Err(PatchParseError::ExcludedAnchor {
                        line: line_no,
                        anchor: anchor.clone(),
                    });
                }
                let idx =
                    state
                        .find_anchor(anchor)
                        .ok_or_else(|| PatchParseError::AnchorNotFound {
                            line: line_no,
                            anchor: anchor.clone(),
                        })? as usize;
                let actual = &file_lines[idx];
                if actual.trim() != content.trim() {
                    return Err(PatchParseError::ContentMismatch {
                        line: line_no,
                        anchor: anchor.clone(),
                        actual: Truncated::new(actual.clone()),
                        claimed: Truncated::new(content.clone()),
                    });
                }
            }
            HunkLine::Insert { .. } => {}
        }

        hunk.push(parsed);
    }

    if !hunk.is_empty() {
        process_hunk(&hunk, &mut ops, &mut deleted)?;
    }

    Ok(ParsedPatch { ops, deleted })
}

#[derive(Debug)]
enum HunkLine {
    Context {
        line: usize,
        anchor: Anchor,
        content: String,
    },
    Delete {
        line: usize,
        anchor: Anchor,
        content: String,
    },
    Insert {
        line: usize,
        content: String,
    },
}

fn parse_line(raw: &str, line: usize) -> Result<HunkLine, PatchParseError> {
    let bytes = raw.as_bytes();
    let sigil = bytes
        .first()
        .copied()
        .ok_or_else(|| PatchParseError::Malformed {
            line,
            detail: "empty line within hunk".into(),
        })?;
    let rest = match sigil {
        b' ' | b'-' | b'+' => &raw[1..],
        _ => {
            return Err(PatchParseError::Malformed {
                line,
                detail: format!("unknown sigil byte {sigil:#x}"),
            });
        }
    };

    let sep_pos = rest.find(SEP).ok_or_else(|| PatchParseError::Malformed {
        line,
        detail: format!("missing {SEP} separator"),
    })?;

    let anchor_str = &rest[..sep_pos];
    let content = &rest[sep_pos + SEP_LEN..];

    match sigil {
        b' ' => {
            let anchor = Anchor::from_str(anchor_str)
                .map_err(|source| PatchParseError::BadAnchor { line, source })?;
            Ok(HunkLine::Context {
                line,
                anchor,
                content: content.to_owned(),
            })
        }
        b'-' => {
            let anchor = Anchor::from_str(anchor_str)
                .map_err(|source| PatchParseError::BadAnchor { line, source })?;
            Ok(HunkLine::Delete {
                line,
                anchor,
                content: content.to_owned(),
            })
        }
        b'+' => {
            if !anchor_str.is_empty() {
                return Err(PatchParseError::InsertAnchorMustBeEmpty { line });
            }
            Ok(HunkLine::Insert {
                line,
                content: content.to_owned(),
            })
        }
        _ => unreachable!(),
    }
}

fn process_hunk(
    hunk: &[HunkLine],
    ops: &mut Vec<EditOp>,
    deleted: &mut Vec<Anchor>,
) -> Result<(), PatchParseError> {
    let has_pin = hunk
        .iter()
        .any(|l| matches!(l, HunkLine::Context { .. } | HunkLine::Delete { .. }));
    if !has_pin {
        let line = hunk
            .first()
            .map(|l| match l {
                HunkLine::Context { line, .. }
                | HunkLine::Delete { line, .. }
                | HunkLine::Insert { line, .. } => *line,
            })
            .unwrap_or(0);
        return Err(PatchParseError::UnpinnedInsert { line });
    }

    let mut pin: Option<Anchor> = None;
    let mut acc_deletes: Vec<Anchor> = Vec::new();
    let mut acc_inserts: Vec<String> = Vec::new();

    for entry in hunk {
        match entry {
            HunkLine::Context { anchor, .. } => {
                flush(&mut acc_deletes, &mut acc_inserts, &pin, ops, deleted);
                pin = Some(anchor.clone());
            }
            HunkLine::Delete { anchor, .. } => {
                if !acc_inserts.is_empty() {
                    // Strict: a delete after inserts closes the prior run.
                    flush(&mut acc_deletes, &mut acc_inserts, &pin, ops, deleted);
                }
                acc_deletes.push(anchor.clone());
                pin = Some(anchor.clone());
            }
            HunkLine::Insert { line, content } => {
                if pin.is_none() && acc_deletes.is_empty() {
                    return Err(PatchParseError::UnpinnedInsert { line: *line });
                }
                acc_inserts.push(content.clone());
            }
        }
    }

    flush(&mut acc_deletes, &mut acc_inserts, &pin, ops, deleted);
    Ok(())
}

fn flush(
    acc_deletes: &mut Vec<Anchor>,
    acc_inserts: &mut Vec<String>,
    pin: &Option<Anchor>,
    ops: &mut Vec<EditOp>,
    deleted: &mut Vec<Anchor>,
) {
    if acc_deletes.is_empty() && acc_inserts.is_empty() {
        return;
    }
    let lines = std::mem::take(acc_inserts);
    let dels: Vec<Anchor> = std::mem::take(acc_deletes);
    if dels.is_empty() {
        let pin_anchor = pin
            .clone()
            .expect("flush called with inserts but no pin (parser bug)");
        ops.push(EditOp::InsertAfter {
            pin: pin_anchor,
            lines,
        });
    } else {
        let from = dels.first().unwrap().clone();
        let to = dels.last().unwrap().clone();
        deleted.extend(dels);
        ops.push(EditOp::Replace { from, to, lines });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::Pool;
    use crate::state::{LineEntry, content_digest};
    use frances_anchors::hash_lines;
    use std::path::PathBuf;

    fn make_setup(lines: &[&str]) -> (FileAnchorState, Vec<String>, Vec<Anchor>) {
        let line_strs: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        let mut pool = Pool::from_used(HashSet::new());
        let hashes = hash_lines(line_strs.iter().map(|s| s.as_str()));
        let mut anchors = Vec::new();
        let entries: Vec<LineEntry> = hashes
            .iter()
            .map(|&h| {
                let a = pool.mint();
                anchors.push(a.clone());
                LineEntry { hash: h, anchor: a }
            })
            .collect();
        let state = FileAnchorState {
            path: PathBuf::from("/test"),
            mtime_ns: 0,
            size: 0,
            content_digest: content_digest(&hashes),
            lines: entries,
        };
        (state, line_strs, anchors)
    }

    #[test]
    fn pure_insert_after_pin() {
        let (state, lines, anchors) = make_setup(&["a", "b", "c"]);
        let patch = format!(" {}§a\n+§new\n", anchors[0]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        assert_eq!(parsed.ops.len(), 1);
        assert_eq!(parsed.deleted.len(), 0);
        match &parsed.ops[0] {
            EditOp::InsertAfter { pin, lines } => {
                assert_eq!(pin, &anchors[0]);
                assert_eq!(lines, &vec!["new".to_string()]);
            }
            _ => panic!("expected InsertAfter"),
        }
    }

    #[test]
    fn delete_only() {
        let (state, lines, anchors) = make_setup(&["a", "b", "c"]);
        let patch = format!("-{}§b\n", anchors[1]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        assert_eq!(parsed.ops.len(), 1);
        assert_eq!(parsed.deleted, vec![anchors[1].clone()]);
        match &parsed.ops[0] {
            EditOp::Replace { from, to, lines } => {
                assert_eq!(from, &anchors[1]);
                assert_eq!(to, &anchors[1]);
                assert!(lines.is_empty());
            }
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn delete_then_insert_coalesces_to_replace() {
        let (state, lines, anchors) = make_setup(&["a", "b", "c"]);
        let patch = format!("-{}§b\n+§B2\n", anchors[1]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        assert_eq!(parsed.ops.len(), 1);
        assert_eq!(parsed.deleted, vec![anchors[1].clone()]);
        match &parsed.ops[0] {
            EditOp::Replace { lines, .. } => assert_eq!(lines, &vec!["B2".to_string()]),
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn contiguous_deletes_then_inserts() {
        let (state, lines, anchors) = make_setup(&["a", "b", "c", "d"]);
        let patch = format!("-{}§b\n-{}§c\n+§X\n+§Y\n", anchors[1], anchors[2]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        assert_eq!(parsed.ops.len(), 1);
        assert_eq!(parsed.deleted.len(), 2);
        match &parsed.ops[0] {
            EditOp::Replace { from, to, lines } => {
                assert_eq!(from, &anchors[1]);
                assert_eq!(to, &anchors[2]);
                assert_eq!(lines.len(), 2);
            }
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn multi_hunk_blank_separated() {
        let (state, lines, anchors) = make_setup(&["a", "b", "c", "d"]);
        let patch = format!("-{}§a\n\n-{}§d\n", anchors[0], anchors[3]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        assert_eq!(parsed.ops.len(), 2);
        assert_eq!(parsed.deleted.len(), 2);
    }

    #[test]
    fn insert_blank_line_via_empty_content() {
        let (state, lines, anchors) = make_setup(&["a"]);
        let patch = format!(" {}§a\n+§\n", anchors[0]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        match &parsed.ops[0] {
            EditOp::InsertAfter { lines, .. } => assert_eq!(lines, &vec!["".to_string()]),
            _ => panic!(),
        }
    }

    #[test]
    fn literal_section_in_content() {
        let (state, lines, anchors) = make_setup(&["a§b"]);
        let patch = format!(" {}§a§b\n+§x§y\n", anchors[0]);
        let parsed = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
        match &parsed.ops[0] {
            EditOp::InsertAfter { lines, .. } => assert_eq!(lines, &vec!["x§y".to_string()]),
            _ => panic!(),
        }
    }

    #[test]
    fn trim_compare_passes_with_indent_diff() {
        let (state, lines, anchors) = make_setup(&["    return 1"]);
        let patch = format!(" {}§return 1\n+§after\n", anchors[0]);
        parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
    }

    #[test]
    fn trim_compare_passes_with_inverse_indent_diff() {
        let (state, lines, anchors) = make_setup(&["return 1"]);
        let patch = format!(" {}§    return 1\n+§after\n", anchors[0]);
        parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap();
    }

    #[test]
    fn trim_compare_fails_on_real_difference() {
        let (state, lines, anchors) = make_setup(&["return 2"]);
        let patch = format!(" {}§return 1\n+§after\n", anchors[0]);
        let err = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap_err();
        assert!(matches!(err, PatchParseError::ContentMismatch { .. }));
    }

    #[test]
    fn unpinned_insert_errors() {
        let (state, lines, _) = make_setup(&["a"]);
        let patch = "+§new\n";
        let err = parse_patch(patch, &state, &lines, &HashSet::new()).unwrap_err();
        assert!(matches!(err, PatchParseError::UnpinnedInsert { .. }));
    }

    #[test]
    fn insert_with_anchor_errors() {
        let (state, lines, anchors) = make_setup(&["a"]);
        let patch = format!("+{}§new\n", anchors[0]);
        let err = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap_err();
        assert!(matches!(
            err,
            PatchParseError::InsertAnchorMustBeEmpty { .. }
        ));
    }

    #[test]
    fn unknown_sigil_errors() {
        let (state, lines, _) = make_setup(&["a"]);
        let patch = "?Apple§a\n";
        let err = parse_patch(patch, &state, &lines, &HashSet::new()).unwrap_err();
        assert!(matches!(err, PatchParseError::Malformed { .. }));
    }

    #[test]
    fn missing_separator_errors() {
        let (state, lines, _) = make_setup(&["a"]);
        let patch = " Applea\n";
        let err = parse_patch(patch, &state, &lines, &HashSet::new()).unwrap_err();
        assert!(matches!(err, PatchParseError::Malformed { .. }));
    }

    #[test]
    fn excluded_anchor_errors() {
        let (state, lines, anchors) = make_setup(&["a", "b"]);
        let mut excluded = HashSet::new();
        excluded.insert(anchors[0].clone());
        let patch = format!(" {}§a\n+§new\n", anchors[0]);
        let err = parse_patch(&patch, &state, &lines, &excluded).unwrap_err();
        assert!(matches!(err, PatchParseError::ExcludedAnchor { .. }));
    }

    #[test]
    fn anchor_not_in_file_errors() {
        let (state, lines, _) = make_setup(&["a"]);
        // Use an anchor that's never minted in this file
        let mut other_pool = Pool::from_used(HashSet::new());
        for _ in 0..50 {
            other_pool.mint();
        }
        let unrelated = other_pool.mint();
        let patch = format!(" {unrelated}§a\n+§new\n");
        let err = parse_patch(&patch, &state, &lines, &HashSet::new()).unwrap_err();
        assert!(matches!(err, PatchParseError::AnchorNotFound { .. }));
    }
}
