use std::collections::HashSet;

use frances_anchors::hash_lines;
use similar::{Algorithm, DiffTag, capture_diff_slices};

use crate::anchor::Anchor;
use crate::pool::Pool;
use crate::state::{FileAnchorState, LineEntry, content_digest};

pub(crate) const DIFF_ALGORITHM: Algorithm = Algorithm::Patience;

#[derive(Debug, Clone, Default)]
pub struct EditHints {
    pub deleted_anchors: Vec<Anchor>,
}

#[derive(Debug, Clone)]
pub struct ReconcileOutcome {
    pub state: FileAnchorState,
    pub minted: Vec<(u32, Anchor)>,
    pub tombstoned: Vec<Anchor>,
}

pub fn reconcile(
    cached: &FileAnchorState,
    on_disk_lines: &[String],
    pool: &mut Pool,
    hints: Option<&EditHints>,
) -> ReconcileOutcome {
    let on_disk_hashes = hash_lines(on_disk_lines.iter().map(|s| s.as_str()));
    let mut cached_hashes = cached.line_hashes();

    // Hint application: poison the hashes of explicitly-deleted anchors so
    // similar can't match them to surviving lines with identical content.
    // The poison values are per-index so they're distinct from any real xxh3
    // output and from each other.
    if let Some(h) = hints {
        let excluded: HashSet<&Anchor> = h.deleted_anchors.iter().collect();
        for (i, le) in cached.lines.iter().enumerate() {
            if excluded.contains(&le.anchor) {
                cached_hashes[i] = poison(i);
            }
        }
    }

    let ops = capture_diff_slices(DIFF_ALGORITHM, &cached_hashes, &on_disk_hashes);

    let mut new_lines: Vec<LineEntry> = Vec::with_capacity(on_disk_lines.len());
    let mut minted: Vec<(u32, Anchor)> = Vec::new();
    let mut tombstoned: Vec<Anchor> = Vec::new();

    for op in ops {
        match op.tag() {
            DiffTag::Equal => {
                for old_idx in op.old_range() {
                    new_lines.push(cached.lines[old_idx].clone());
                }
            }
            DiffTag::Delete => {
                for old_idx in op.old_range() {
                    tombstoned.push(cached.lines[old_idx].anchor.clone());
                }
            }
            DiffTag::Insert => {
                for new_idx in op.new_range() {
                    let anchor = pool.mint();
                    let pos = new_lines.len() as u32;
                    new_lines.push(LineEntry {
                        hash: on_disk_hashes[new_idx],
                        anchor: anchor.clone(),
                    });
                    minted.push((pos, anchor));
                }
            }
            DiffTag::Replace => {
                for old_idx in op.old_range() {
                    tombstoned.push(cached.lines[old_idx].anchor.clone());
                }
                for new_idx in op.new_range() {
                    let anchor = pool.mint();
                    let pos = new_lines.len() as u32;
                    new_lines.push(LineEntry {
                        hash: on_disk_hashes[new_idx],
                        anchor: anchor.clone(),
                    });
                    minted.push((pos, anchor));
                }
            }
        }
    }

    let state = FileAnchorState {
        path: cached.path.clone(),
        mtime_ns: cached.mtime_ns,
        size: cached.size,
        content_digest: content_digest(&on_disk_hashes),
        lines: new_lines,
    };

    ReconcileOutcome {
        state,
        minted,
        tombstoned,
    }
}

fn poison(i: usize) -> u64 {
    u64::MAX.wrapping_sub(i as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_state(lines: &[&str]) -> (FileAnchorState, Pool) {
        let line_strs: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        let mut pool = Pool::from_used(HashSet::new());
        let hashes = hash_lines(line_strs.iter().map(|s| s.as_str()));
        let entries: Vec<LineEntry> = hashes
            .iter()
            .map(|&h| LineEntry {
                hash: h,
                anchor: pool.mint(),
            })
            .collect();
        let state = FileAnchorState {
            path: PathBuf::from("x"),
            mtime_ns: 0,
            size: 0,
            content_digest: content_digest(&hashes),
            lines: entries,
        };
        (state, pool)
    }

    fn line_vec(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn unchanged_preserves_anchors() {
        let (cached, mut pool) = make_state(&["a", "b", "c"]);
        let on_disk = line_vec(&["a", "b", "c"]);
        let out = reconcile(&cached, &on_disk, &mut pool, None);
        assert!(out.minted.is_empty());
        assert!(out.tombstoned.is_empty());
        for (a, b) in cached.lines.iter().zip(out.state.lines.iter()) {
            assert_eq!(a.anchor, b.anchor);
        }
    }

    #[test]
    fn pure_insertion_mints_only() {
        let (cached, mut pool) = make_state(&["a", "c"]);
        let on_disk = line_vec(&["a", "b", "c"]);
        let out = reconcile(&cached, &on_disk, &mut pool, None);
        assert_eq!(out.minted.len(), 1);
        assert!(out.tombstoned.is_empty());
        assert_eq!(out.state.lines[0].anchor, cached.lines[0].anchor);
        assert_eq!(out.state.lines[2].anchor, cached.lines[1].anchor);
    }

    #[test]
    fn pure_deletion_tombstones_only() {
        let (cached, mut pool) = make_state(&["a", "b", "c"]);
        let on_disk = line_vec(&["a", "c"]);
        let out = reconcile(&cached, &on_disk, &mut pool, None);
        assert!(out.minted.is_empty());
        assert_eq!(out.tombstoned.len(), 1);
        assert_eq!(out.tombstoned[0], cached.lines[1].anchor);
    }

    #[test]
    fn replacement_tombstones_and_mints() {
        let (cached, mut pool) = make_state(&["a", "old", "c"]);
        let on_disk = line_vec(&["a", "new", "c"]);
        let out = reconcile(&cached, &on_disk, &mut pool, None);
        assert_eq!(out.minted.len(), 1);
        assert_eq!(out.tombstoned.len(), 1);
        assert_eq!(out.tombstoned[0], cached.lines[1].anchor);
    }

    #[test]
    fn hints_force_tombstone_of_duplicate_line() {
        // cached: [A:foo, B:foo] — two lines with identical content
        // on_disk: [foo] — one of them survived
        // Without hints, similar might tombstone either A or B ambiguously.
        // With hints saying "A was deleted," B should survive.
        let (cached, mut pool) = make_state(&["foo", "foo"]);
        let anchor_a = cached.lines[0].anchor.clone();
        let anchor_b = cached.lines[1].anchor.clone();
        let on_disk = line_vec(&["foo"]);

        let hints = EditHints {
            deleted_anchors: vec![anchor_a.clone()],
        };
        let out = reconcile(&cached, &on_disk, &mut pool, Some(&hints));

        assert!(out.tombstoned.contains(&anchor_a));
        assert!(!out.tombstoned.contains(&anchor_b));
        assert_eq!(out.state.lines.len(), 1);
        assert_eq!(out.state.lines[0].anchor, anchor_b);
    }
}
