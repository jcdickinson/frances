use crate::anchor::Anchor;

#[derive(Debug, Clone)]
pub enum EditOp {
    /// Insert `lines` immediately after the line anchored at `pin` in the
    /// pre-turn line array.
    InsertAfter { pin: Anchor, lines: Vec<String> },
    /// Insert `lines` immediately before the line anchored at `pin` in the
    /// pre-turn line array.
    InsertBefore { pin: Anchor, lines: Vec<String> },
    /// Replace the contiguous run of pre-turn lines from anchor `from` to
    /// anchor `to` (inclusive) with `lines`. Empty `lines` = pure deletion.
    Replace {
        from: Anchor,
        to: Anchor,
        lines: Vec<String>,
    },
}

/// Where an output line of [`apply_ops`] came from. Lets the caller carry the
/// original line's anchor by identity (no diff) and mint only for inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    /// Carried verbatim from this index in `original_state.lines`.
    Carried(u32),
    /// Freshly inserted by the edit.
    Inserted,
}

/// Apply ops to `original_lines`, producing the new line array alongside a
/// parallel [`LineOrigin`] for each output line. Anchors in ops must reference
/// lines in `original_lines`'s anchor space — the caller resolves anchor →
/// original_index via the `FileAnchorState` they passed to `resolve_anchor`.
pub fn apply_ops(
    original_state: &crate::state::FileAnchorState,
    original_lines: &[String],
    ops: &[EditOp],
) -> (Vec<String>, Vec<LineOrigin>) {
    let mut out: Vec<String> = original_lines.to_vec();
    let mut origins: Vec<LineOrigin> = (0..original_lines.len() as u32)
        .map(LineOrigin::Carried)
        .collect();
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
                splice_in(&mut out, &mut origins, draft_idx, lines);
                shift_after(&mut offsets, orig, n as i64);
            }
            EditOp::InsertBefore { pin, lines } => {
                let orig = original_state
                    .find_anchor(pin)
                    .expect("apply_ops: anchor not found (parser should have caught this)")
                    as usize;
                let draft_idx = (orig as i64 + offsets[orig]) as usize;
                let n = lines.len();
                splice_in(&mut out, &mut origins, draft_idx, lines);
                // pin itself shifts because the new lines land at its index.
                for offset in offsets.iter_mut().skip(orig) {
                    *offset += n as i64;
                }
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
                origins.splice(
                    from_draft..=to_draft,
                    std::iter::repeat_n(LineOrigin::Inserted, added),
                );
                // Anything originally at position > to_orig shifts by delta.
                shift_after(&mut offsets, to_orig, delta);
            }
        }
    }

    (out, origins)
}

fn splice_in(out: &mut Vec<String>, origins: &mut Vec<LineOrigin>, at: usize, lines: &[String]) {
    out.splice(at..at, lines.iter().cloned());
    origins.splice(
        at..at,
        std::iter::repeat_n(LineOrigin::Inserted, lines.len()),
    );
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
        let (out, _) = apply_ops(&state, &lines, &ops);
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
        let (out, _) = apply_ops(&state, &lines, &ops);
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
        let (out, _) = apply_ops(&state, &lines, &ops);
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
        let (out, _) = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "X", "b", "Y", "c"]);
    }

    #[test]
    fn insert_before_basic() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c"]);
        let ops = vec![EditOp::InsertBefore {
            pin: anchors[1].clone(),
            lines: vec!["X".into()],
        }];
        let (out, _) = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "X", "b", "c"]);
    }

    #[test]
    fn insert_before_at_first_line() {
        let (state, lines, anchors) = fresh_state(&["a", "b"]);
        let ops = vec![EditOp::InsertBefore {
            pin: anchors[0].clone(),
            lines: vec!["X".into(), "Y".into()],
        }];
        let (out, _) = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["X", "Y", "a", "b"]);
    }

    #[test]
    fn insert_before_then_insert_after_same_anchor() {
        let (state, lines, anchors) = fresh_state(&["a", "b", "c"]);
        let ops = vec![
            EditOp::InsertBefore {
                pin: anchors[1].clone(),
                lines: vec!["X".into()],
            },
            EditOp::InsertAfter {
                pin: anchors[1].clone(),
                lines: vec!["Y".into()],
            },
        ];
        let (out, _) = apply_ops(&state, &lines, &ops);
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
        let (out, _) = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["A1", "A2", "b", "X", "c"]);
    }

    #[test]
    fn origins_track_carried_and_inserted() {
        use LineOrigin::*;
        let (state, lines, anchors) = fresh_state(&["a", "b", "c", "d"]);
        // Replace b..=c with one line, then insert after d.
        let ops = vec![
            EditOp::Replace {
                from: anchors[1].clone(),
                to: anchors[2].clone(),
                lines: vec!["X".into()],
            },
            EditOp::InsertAfter {
                pin: anchors[3].clone(),
                lines: vec!["Y".into()],
            },
        ];
        let (out, origins) = apply_ops(&state, &lines, &ops);
        assert_eq!(out, vec!["a", "X", "d", "Y"]);
        // a carried from 0, X inserted, d carried from 3, Y inserted.
        assert_eq!(origins, vec![Carried(0), Inserted, Carried(3), Inserted]);
    }
}
