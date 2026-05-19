//! Per-session hashset that detects repeated read-style tool calls
//! when the world hasn't changed underneath them. The
//! [`EditSession`](crate::EditSession) owns the set; the read-style
//! tools (`file_read`, `readRaw`, `file_find_or_grep`) ask it whether
//! an incoming call is a loop and record the call on a miss. Any
//! write through `EditSession::edit` clears the set so a follow-up
//! read can never be misdiagnosed as a loop.
//!
//! Unbounded by design — we want to catch the model looping over a
//! horizon longer than three calls. The set only grows between
//! writes, and writes clear it; in practice a turn won't accumulate
//! enough distinct read keys to matter.

use std::collections::HashSet;

/// One read-style tool invocation in comparable form. Variants are
/// deliberately not interchangeable — a `Read` never matches a
/// `Search`, even with an identical `args_hash`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum LoopKey {
    /// `file_read` / `readRaw`. `mtime_ns` + `size` pin the on-disk
    /// state; `args_hash` pins the request shape (path + ranges).
    Read {
        args_hash: u64,
        mtime_ns: i64,
        size: u64,
    },
    /// `file_find_or_grep`. No filesystem pin — varying the args (or
    /// any edit clearing the set) is enough to break the loop.
    Search { args_hash: u64 },
}

#[derive(Default, Debug)]
pub(crate) struct LoopSet {
    entries: HashSet<LoopKey>,
}

impl LoopSet {
    pub(crate) fn contains(&self, key: &LoopKey) -> bool {
        self.entries.contains(key)
    }

    pub(crate) fn record(&mut self, key: LoopKey) {
        self.entries.insert(key);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_and_search_with_same_args_hash_do_not_collide() {
        let mut set = LoopSet::default();
        set.record(LoopKey::Read {
            args_hash: 42,
            mtime_ns: 100,
            size: 10,
        });
        assert!(!set.contains(&LoopKey::Search { args_hash: 42 }));
    }

    #[test]
    fn record_retains_all_entries_until_cleared() {
        let mut set = LoopSet::default();
        for i in 0..16u64 {
            set.record(LoopKey::Search { args_hash: i });
        }
        // No eviction — every entry is still present.
        for i in 0..16u64 {
            assert!(set.contains(&LoopKey::Search { args_hash: i }));
        }
    }

    #[test]
    fn clear_empties_the_set() {
        let mut set = LoopSet::default();
        set.record(LoopKey::Search { args_hash: 1 });
        set.clear();
        assert!(!set.contains(&LoopKey::Search { args_hash: 1 }));
    }
}
