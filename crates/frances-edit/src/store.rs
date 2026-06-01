use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;
use thiserror::Error;

use crate::anchor::Anchor;
use crate::state::FileAnchorState;

/// Boxed error type for [`AnchorStore`] backends.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct StoreError(pub Box<dyn std::error::Error + Send + Sync + 'static>);

impl StoreError {
    pub fn new<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self(Box::new(err))
    }
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;

#[async_trait]
pub trait AnchorStore: Send + Sync {
    async fn load(&self, path: &Path) -> StoreResult<Option<FileAnchorState>>;
    async fn save_meta(
        &self,
        path: &Path,
        mtime_ns: i64,
        size: u64,
        content_digest: u64,
    ) -> StoreResult<()>;
    /// Replace all stored lines for `path` with `state.lines` and write
    /// `state`'s meta, atomically.
    async fn replace_file_lines(&self, path: &Path, state: &FileAnchorState) -> StoreResult<()>;
    async fn used_anchors(&self, path: &Path) -> StoreResult<HashSet<Anchor>>;
    async fn tombstone(&self, path: &Path, anchors: &[Anchor]) -> StoreResult<()>;
    async fn clear_tombstones(&self) -> StoreResult<()>;
}

#[cfg(any(test, feature = "test-utils"))]
pub use fake::FakeStore;

#[cfg(any(test, feature = "test-utils"))]
mod fake {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::path::{Path, PathBuf};

    use async_trait::async_trait;
    use parking_lot::Mutex;

    use super::{AnchorStore, StoreResult};
    use crate::anchor::Anchor;
    use crate::state::{FileAnchorState, LineEntry};

    #[derive(Default)]
    struct Inner {
        meta: HashMap<PathBuf, (i64, u64, u64)>, // (mtime_ns, size, content_digest)
        lines: HashMap<PathBuf, BTreeMap<u32, (u64, Anchor)>>,
        tombstones: HashMap<PathBuf, HashSet<Anchor>>,
    }

    pub struct FakeStore {
        inner: Mutex<Inner>,
    }

    impl Default for FakeStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeStore {
        pub fn new() -> Self {
            Self {
                inner: Mutex::new(Inner::default()),
            }
        }
    }

    #[async_trait]
    impl AnchorStore for FakeStore {
        async fn load(&self, path: &Path) -> StoreResult<Option<FileAnchorState>> {
            let inner = self.inner.lock();
            let meta = match inner.meta.get(path) {
                Some(m) => *m,
                None => return Ok(None),
            };
            let lines_map = inner.lines.get(path);
            let lines: Vec<LineEntry> = match lines_map {
                Some(m) => m
                    .values()
                    .map(|(hash, anchor)| LineEntry {
                        hash: *hash,
                        anchor: anchor.clone(),
                    })
                    .collect(),
                None => Vec::new(),
            };
            Ok(Some(FileAnchorState {
                path: path.to_path_buf(),
                mtime_ns: meta.0,
                size: meta.1,
                content_digest: meta.2,
                lines,
            }))
        }

        async fn save_meta(
            &self,
            path: &Path,
            mtime_ns: i64,
            size: u64,
            content_digest: u64,
        ) -> StoreResult<()> {
            let mut inner = self.inner.lock();
            inner
                .meta
                .insert(path.to_path_buf(), (mtime_ns, size, content_digest));
            Ok(())
        }

        async fn replace_file_lines(
            &self,
            path: &Path,
            state: &FileAnchorState,
        ) -> StoreResult<()> {
            let mut inner = self.inner.lock();
            inner.lines.remove(path);
            if !state.lines.is_empty() {
                let entry = inner.lines.entry(path.to_path_buf()).or_default();
                for (line_no, le) in state.lines.iter().enumerate() {
                    entry.insert(line_no as u32, (le.hash, le.anchor.clone()));
                }
            }
            inner.meta.insert(
                path.to_path_buf(),
                (state.mtime_ns, state.size, state.content_digest),
            );
            Ok(())
        }

        async fn used_anchors(&self, path: &Path) -> StoreResult<HashSet<Anchor>> {
            let inner = self.inner.lock();
            let mut used = HashSet::new();
            if let Some(m) = inner.lines.get(path) {
                for (_, anchor) in m.values() {
                    used.insert(anchor.clone());
                }
            }
            if let Some(t) = inner.tombstones.get(path) {
                used.extend(t.iter().cloned());
            }
            Ok(used)
        }

        async fn tombstone(&self, path: &Path, anchors: &[Anchor]) -> StoreResult<()> {
            let mut inner = self.inner.lock();
            let set = inner.tombstones.entry(path.to_path_buf()).or_default();
            set.extend(anchors.iter().cloned());
            Ok(())
        }

        async fn clear_tombstones(&self) -> StoreResult<()> {
            let mut inner = self.inner.lock();
            inner.tombstones.clear();
            Ok(())
        }
    }
}
