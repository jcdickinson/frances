use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;
use thiserror::Error;

use crate::anchor::Anchor;
use crate::state::FileAnchorState;

/// Boxed error wrapper so different `AnchorStore` backends can fail with
/// different concrete error types without parameterising the trait.
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
    async fn upsert_lines(&self, path: &Path, lines: &[(u32, u64, Anchor)]) -> StoreResult<()>;
    async fn delete_lines(&self, path: &Path, line_nos: &[u32]) -> StoreResult<()>;
    async fn truncate_lines(&self, path: &Path) -> StoreResult<()>;
    async fn used_anchors(&self, path: &Path) -> StoreResult<HashSet<Anchor>>;
    async fn tombstone(&self, path: &Path, anchors: &[Anchor]) -> StoreResult<()>;
    async fn clear_tombstones(&self) -> StoreResult<()>;
    async fn forget(&self, path: &Path) -> StoreResult<()>;
}

#[cfg(any(test, feature = "test-utils"))]
pub use fake::FakeStore;

#[cfg(any(test, feature = "test-utils"))]
mod fake {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use async_trait::async_trait;

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
            let inner = self.inner.lock().unwrap();
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
            let mut inner = self.inner.lock().unwrap();
            inner
                .meta
                .insert(path.to_path_buf(), (mtime_ns, size, content_digest));
            Ok(())
        }

        async fn upsert_lines(&self, path: &Path, lines: &[(u32, u64, Anchor)]) -> StoreResult<()> {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner.lines.entry(path.to_path_buf()).or_default();
            for (line_no, hash, anchor) in lines {
                entry.insert(*line_no, (*hash, anchor.clone()));
            }
            Ok(())
        }

        async fn delete_lines(&self, path: &Path, line_nos: &[u32]) -> StoreResult<()> {
            let mut inner = self.inner.lock().unwrap();
            if let Some(m) = inner.lines.get_mut(path) {
                for n in line_nos {
                    m.remove(n);
                }
            }
            Ok(())
        }

        async fn truncate_lines(&self, path: &Path) -> StoreResult<()> {
            let mut inner = self.inner.lock().unwrap();
            inner.lines.remove(path);
            Ok(())
        }

        async fn used_anchors(&self, path: &Path) -> StoreResult<HashSet<Anchor>> {
            let inner = self.inner.lock().unwrap();
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
            let mut inner = self.inner.lock().unwrap();
            let set = inner.tombstones.entry(path.to_path_buf()).or_default();
            set.extend(anchors.iter().cloned());
            Ok(())
        }

        async fn clear_tombstones(&self) -> StoreResult<()> {
            let mut inner = self.inner.lock().unwrap();
            inner.tombstones.clear();
            Ok(())
        }

        async fn forget(&self, path: &Path) -> StoreResult<()> {
            let mut inner = self.inner.lock().unwrap();
            inner.meta.remove(path);
            inner.lines.remove(path);
            inner.tombstones.remove(path);
            Ok(())
        }
    }
}
