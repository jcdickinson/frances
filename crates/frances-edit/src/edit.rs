use crate::anchor::Anchor;

#[derive(Debug, Clone)]
pub enum EditOp {
    /// Insert `lines` immediately after the line anchored at `pin` in the
    /// pre-turn line array.
    InsertAfter { pin: Anchor, lines: Vec<String> },
    /// Replace the contiguous run of pre-turn lines from anchor `from` to
    /// anchor `to` (inclusive) with `lines`. Empty `lines` = pure deletion.
    Replace {
        from: Anchor,
        to: Anchor,
        lines: Vec<String>,
    },
}

/// Pure replay: apply ops to `original_lines`, producing the new line array.
/// Anchors in ops must reference lines in `original_lines`'s anchor space —
/// the caller resolves anchor → original_index via the `FileAnchorState`
/// they passed to `parse_patch`.
///
/// Implementation note: ops are applied in order, with offset tracking so
/// that earlier splices shift the resolved positions of later ops.
pub fn apply_ops(
    original_state: &crate::state::FileAnchorState,
    original_lines: &[String],
    ops: &[EditOp],
) -> Vec<String> {
    let mut out: Vec<String> = original_lines.to_vec();
    // offsets[orig_idx] = current shift of that original index in `out`.
    let mut offsets: Vec<i64> = vec![0; original_lines.len()];

    for op in ops {
        match op {
            EditOp::InsertAfter { pin, lines } => {
                let orig = original_state
                    .find_anchor(pin)
                    .expect("apply_ops: anchor not found (parser should have caught this)")
                    as usize;
                let draft_idx = (orig as i64 + offsets[orig]) as usize + 1;
                let n = lines.len();
                splice_in(&mut out, draft_idx, lines);
                shift_after(&mut offsets, orig, n as i64);
            }
            EditOp::Replace { from, to, lines } => {
                let from_orig = original_state
                    .find_anchor(from)
                    .expect("apply_ops: anchor not found (parser should have caught this)")
                    as usize;
                let to_orig = original_state
                    .find_anchor(to)
                    .expect("apply_ops: anchor not found (parser should have caught this)")
                    as usize;
                let from_draft = (from_orig as i64 + offsets[from_orig]) as usize;
                let to_draft = (to_orig as i64 + offsets[to_orig]) as usize;
                let removed = to_draft - from_draft + 1;
                let added = lines.len();
                let delta = added as i64 - removed as i64;
                out.splice(from_draft..=to_draft, lines.iter().cloned());
                // Anything originally at position > to_orig shifts by delta.
                shift_after(&mut offsets, to_orig, delta);
            }
        }
    }

    out
}

fn splice_in(out: &mut Vec<String>, at: usize, lines: &[String]) {
    out.splice(at..at, lines.iter().cloned());
}

fn shift_after(offsets: &mut [i64], boundary_orig_idx: usize, delta: i64) {
    for offset in offsets.iter_mut().skip(boundary_orig_idx + 1) {
        *offset += delta;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::Pool;
    use crate::state::{FileAnchorState, LineEntry, content_digest};
    use frances_anchors::hash_lines;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn fresh_state(lines: &[&str]) -> (FileAnchorState, Vec<String>, Vec<Anchor>) {
        let line_strings: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        let mut pool = Pool::from_used(HashSet::new());
        let hashes = hash_lines(line_strings.iter().map(|s| s.as_str()));
        let mut anchors: Vec<Anchor> = Vec::new();
        let entries: Vec<LineEntry> = hashes
            .iter()
            .map(|&h| {
                let a = pool.mint();
                anchors.push(a.clone());
                LineEntry { hash: h, anchor: a }
            })
            .collect();
        let state = FileAnchorState {
            path: PathBuf::from("x"),
            mtime_ns: 0,
            size: 0,
            content_digest: content_digest(&hashes),
            lines: entries,
        };
        (state, line_strings, anchors)
    }

    #[test]
    fn insert_after_basic() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c"]);
        let ops = vec![EditOp::InsertAfter {
            pin: anchors[0].clone(),
            lines: vec!["X".into()],
        }];
        let out = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "X", "b", "c"]);
    }

    #[test]
    fn replace_range() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c", "d"]);
        let ops = vec![EditOp::Replace {
            from: anchors[1].clone(),
            to: anchors[2].clone(),
            lines: vec!["X".into()],
        }];
        let out = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "X", "d"]);
    }

    #[test]
    fn pure_delete() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c"]);
        let ops = vec![EditOp::Replace {
            from: anchors[1].clone(),
            to: anchors[1].clone(),
            lines: vec![],
        }];
        let out = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "c"]);
    }

    #[test]
    fn sequential_ops_offset_tracking() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c"]);
        // 1) insert X after a
        // 2) insert Y after b (b is at original index 1; should land between b and c)
        let ops = vec![
            EditOp::InsertAfter {
                pin: anchors[0].clone(),
                lines: vec!["X".into()],
            },
            EditOp::InsertAfter {
                pin: anchors[1].clone(),
                lines: vec!["Y".into()],
            },
        ];
        let out = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "X", "b", "Y", "c"]);
    }

    #[test]
    fn delete_then_insert_at_same_anchor_position() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c"]);
        // Replace a..=a with ["A1", "A2"], then insert after b (which has shifted)
        let ops = vec![
            EditOp::Replace {
                from: anchors[0].clone(),
                to: anchors[0].clone(),
                lines: vec!["A1".into(), "A2".into()],
            },
            EditOp::InsertAfter {
                pin: anchors[1].clone(),
                lines: vec!["X".into()],
            },
        ];
        let out = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["A1", "A2", "b", "X", "c"]);
    }
}
