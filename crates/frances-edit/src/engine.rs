use std::path::{Path, PathBuf};

use anyhow::Result;
use frances_anchors::hash_lines;

use crate::anchor::Anchor;
use crate::pool::Pool;
use crate::reconcile::reconcile;
use crate::state::{FileAnchorState, LineEntry, content_digest};
use crate::store::AnchorStore;

#[derive(Debug, Clone)]
pub struct WorkingFile {
    pub path: PathBuf,
    pub state: FileAnchorState,
    pub lines: Vec<String>,
}

pub struct EditEngine<S: AnchorStore> {
    store: S,
}

impl<S: AnchorStore> EditEngine<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Load anchored state for `path` and reconcile against the caller's
    /// already-read content. Drift ladder:
    ///   - mtime/size match → cached state is current, no work
    ///   - content_digest match → only meta touched, update meta and return cached
    ///   - else → reconcile against `on_disk_lines`, persist, return new state
    ///
    /// Cold path (no cached state): mint anchors for every line and persist.
    pub async fn open(
        &self,
        path: PathBuf,
        on_disk_lines: Vec<String>,
        mtime_ns: i64,
        size: u64,
    ) -> Result<WorkingFile> {
        let cached = self.store.load(&path).await?;

        let state = match cached {
            Some(c) if c.mtime_ns == mtime_ns && c.size == size => c,
            Some(c) => {
                let on_disk_hashes = hash_lines(on_disk_lines.iter().map(|s| s.as_str()));
                let on_disk_digest = content_digest(&on_disk_hashes);
                if c.content_digest == on_disk_digest {
                    self.store
                        .save_meta(&path, mtime_ns, size, on_disk_digest)
                        .await?;
                    FileAnchorState {
                        mtime_ns,
                        size,
                        ..c
                    }
                } else {
                    let mut pool = Pool::load(&self.store, &path).await?;
                    let r = reconcile(&c, &on_disk_lines, &mut pool, None);
                    let mut new_state = r.state;
                    new_state.mtime_ns = mtime_ns;
                    new_state.size = size;
                    self.persist_state(&path, &new_state).await?;
                    if !r.tombstoned.is_empty() {
                        self.store.tombstone(&path, &r.tombstoned).await?;
                    }
                    new_state
                }
            }
            None => {
                let on_disk_hashes = hash_lines(on_disk_lines.iter().map(|s| s.as_str()));
                let on_disk_digest = content_digest(&on_disk_hashes);
                let mut pool = Pool::load(&self.store, &path).await?;
                let entries: Vec<LineEntry> = on_disk_hashes
                    .iter()
                    .map(|&h| LineEntry {
                        hash: h,
                        anchor: pool.mint(),
                    })
                    .collect();
                let new_state = FileAnchorState {
                    path: path.clone(),
                    mtime_ns,
                    size,
                    content_digest: on_disk_digest,
                    lines: entries,
                };
                self.persist_state(&path, &new_state).await?;
                new_state
            }
        };

        Ok(WorkingFile {
            path,
            state,
            lines: on_disk_lines,
        })
    }

    /// Persist a final anchored state for `path`, replacing any existing
    /// rows. Records `tombstones` (typically `outcome.tombstoned` from a
    /// `reconcile` call) into the tombstones table.
    pub async fn commit(
        &self,
        path: &Path,
        state: &FileAnchorState,
        mtime_ns: i64,
        size: u64,
        tombstones: &[Anchor],
    ) -> Result<()> {
        let mut state_with_meta = state.clone();
        state_with_meta.mtime_ns = mtime_ns;
        state_with_meta.size = size;
        self.persist_state(path, &state_with_meta).await?;
        if !tombstones.is_empty() {
            self.store.tombstone(path, tombstones).await?;
        }
        Ok(())
    }

    /// Clear the tombstones table at the end of a turn.
    pub async fn end_turn(&self) -> Result<()> {
        self.store.clear_tombstones().await
    }

    async fn persist_state(&self, path: &Path, state: &FileAnchorState) -> Result<()> {
        self.store.truncate_lines(path).await?;
        let rows: Vec<(u32, u64, Anchor)> = state
            .lines
            .iter()
            .enumerate()
            .map(|(i, le)| (i as u32, le.hash, le.anchor.clone()))
            .collect();
        if !rows.is_empty() {
            self.store.upsert_lines(path, &rows).await?;
        }
        self.store
            .save_meta(path, state.mtime_ns, state.size, state.content_digest)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FakeStore;

    fn lines_of(s: &str) -> Vec<String> {
        s.lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn open_cold_path_mints_for_every_line() {
        let engine = EditEngine::new(FakeStore::new());
        let working = engine
            .open(PathBuf::from("/x"), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        assert_eq!(working.lines, vec!["a", "b", "c"]);
        assert_eq!(working.state.lines.len(), 3);
        assert_ne!(working.state.lines[0].anchor, working.state.lines[1].anchor);
    }

    #[tokio::test]
    async fn open_cached_when_mtime_size_match() {
        let engine = EditEngine::new(FakeStore::new());
        let p = PathBuf::from("/x");
        let w1 = engine
            .open(p.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let w2 = engine
            .open(p.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        for (a, b) in w1.state.lines.iter().zip(w2.state.lines.iter()) {
            assert_eq!(a.anchor, b.anchor);
        }
    }

    #[tokio::test]
    async fn open_digest_match_updates_meta_only() {
        let engine = EditEngine::new(FakeStore::new());
        let p = PathBuf::from("/x");
        let _ = engine
            .open(p.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        // Same content, different mtime/size — should keep anchors, update meta.
        let w2 = engine
            .open(p.clone(), lines_of("a\nb"), 200, 99)
            .await
            .unwrap();
        assert_eq!(w2.state.mtime_ns, 200);
        assert_eq!(w2.state.size, 99);
    }

    #[tokio::test]
    async fn open_drift_reconciles() {
        let engine = EditEngine::new(FakeStore::new());
        let p = PathBuf::from("/x");
        let w1 = engine
            .open(p.clone(), lines_of("a\nb\nc"), 100, 5)
            .await
            .unwrap();
        let anchor_a = w1.state.lines[0].anchor.clone();
        let anchor_b = w1.state.lines[1].anchor.clone();
        let anchor_c = w1.state.lines[2].anchor.clone();

        // Drift: 'b' was deleted externally
        let w2 = engine
            .open(p.clone(), lines_of("a\nc"), 200, 3)
            .await
            .unwrap();
        assert_eq!(w2.state.lines.len(), 2);
        assert_eq!(w2.state.lines[0].anchor, anchor_a);
        assert_eq!(w2.state.lines[1].anchor, anchor_c);
        let used = engine.store().used_anchors(&p).await.unwrap();
        assert!(used.contains(&anchor_b)); // tombstoned
    }

    #[tokio::test]
    async fn commit_persists_and_clears_with_end_turn() {
        let engine = EditEngine::new(FakeStore::new());
        let p = PathBuf::from("/x");
        let working = engine
            .open(p.clone(), lines_of("a\nb"), 100, 3)
            .await
            .unwrap();
        let anchor_a = working.state.lines[0].anchor.clone();

        // Build a new state where 'a' was replaced (test commit + tombstone)
        let mut pool = Pool::load(engine.store(), &p).await.unwrap();
        let new_lines = lines_of("A2\nb");
        let outcome = reconcile(&working.state, &new_lines, &mut pool, None);
        engine
            .commit(&p, &outcome.state, 200, 4, &outcome.tombstoned)
            .await
            .unwrap();

        let used_before = engine.store().used_anchors(&p).await.unwrap();
        assert!(used_before.contains(&anchor_a));

        engine.end_turn().await.unwrap();
        let used_after = engine.store().used_anchors(&p).await.unwrap();
        assert!(!used_after.contains(&anchor_a));
    }
}
