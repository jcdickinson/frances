use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use frances_edit::{Anchor, AnchorStore, FileAnchorState, LineEntry, StoreError, StoreResult};
use frances_storage::{EntitySchema, Migration};
use thiserror::Error;
use turso::Value;
use uuid::Uuid;

use crate::store::Database;

/// Owns file_meta, file_lines, file_tombstones — the anchor edit
/// state used by the editor. UUID is permanent.
pub static SCHEMA: EntitySchema<'static> = EntitySchema {
    entity: Uuid::from_u128(0x97acb11c_b9a1_4f71_af62_0368f2ca9913),
    migrations: Cow::Borrowed(&[Migration {
        name: Cow::Borrowed("0001_init.sql"),
        sql: Cow::Borrowed(include_str!("migrations/0001_init.sql")),
    }]),
};

#[derive(Debug, Error)]
enum AnchorRowError {
    #[error("non-UTF8 path: {0}")]
    NonUtf8Path(PathBuf),
    #[error("expected integer in {column}, got {found:?}")]
    NonIntegerColumn { column: &'static str, found: Value },
    #[error("expected blob in {column}, got {found:?}")]
    NonBlobColumn { column: &'static str, found: Value },
}

pub struct AnchorStoreImpl {
    db: Database,
}

impl AnchorStoreImpl {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

fn path_str(p: &Path) -> StoreResult<String> {
    p.to_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::new(AnchorRowError::NonUtf8Path(p.to_path_buf())))
}

fn col_i64(row: &turso::Row, idx: usize, column: &'static str) -> StoreResult<i64> {
    match row.get_value(idx).map_err(StoreError::new)? {
        Value::Integer(v) => Ok(v),
        other => Err(StoreError::new(AnchorRowError::NonIntegerColumn {
            column,
            found: other,
        })),
    }
}

fn col_blob(row: &turso::Row, idx: usize, column: &'static str) -> StoreResult<Vec<u8>> {
    match row.get_value(idx).map_err(StoreError::new)? {
        Value::Blob(v) => Ok(v),
        other => Err(StoreError::new(AnchorRowError::NonBlobColumn {
            column,
            found: other,
        })),
    }
}

#[async_trait]
impl AnchorStore for AnchorStoreImpl {
    async fn load(&self, path: &Path) -> StoreResult<Option<FileAnchorState>> {
        let conn = self.db.connect().await;
        let p = path_str(path)?;

        let mut rows = conn
            .query(
                "SELECT mtime_ns, size, content_digest FROM file_meta WHERE path = ?1",
                (p.clone(),),
            )
            .await
            .map_err(StoreError::new)?;
        let row = match rows.next().await.map_err(StoreError::new)? {
            Some(r) => r,
            None => return Ok(None),
        };
        let mtime_ns = col_i64(&row, 0, "mtime_ns")?;
        let size = col_i64(&row, 1, "size")? as u64;
        let content_digest = col_i64(&row, 2, "content_digest")? as u64;

        let mut rows = conn
            .query(
                "SELECT hash, anchor FROM file_lines WHERE path = ?1 ORDER BY line_no",
                (p,),
            )
            .await
            .map_err(StoreError::new)?;

        let mut lines: Vec<LineEntry> = Vec::new();
        while let Some(row) = rows.next().await.map_err(StoreError::new)? {
            let hash = col_i64(&row, 0, "hash")? as u64;
            let anchor_bytes = col_blob(&row, 1, "anchor")?;
            let anchor = Anchor::from_bytes(&anchor_bytes).map_err(StoreError::new)?;
            lines.push(LineEntry { hash, anchor });
        }

        Ok(Some(FileAnchorState {
            path: path.to_path_buf(),
            mtime_ns,
            size,
            content_digest,
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
        let conn = self.db.connect().await;
        conn.execute(
            "INSERT OR REPLACE INTO file_meta (path, mtime_ns, size, content_digest) VALUES (?1, ?2, ?3, ?4)",
            (path_str(path)?, mtime_ns, size as i64, content_digest as i64),
        )
        .await
        .map_err(StoreError::new)?;
        Ok(())
    }

    async fn replace_file_lines(&self, path: &Path, state: &FileAnchorState) -> StoreResult<()> {
        let conn = self.db.connect().await;
        let p = path_str(path)?;

        // Whole-file rewrite under one transaction: truncate, re-insert every
        // line via a single prepared statement, then write meta — collapsing
        // what used to be N+2 separately-committed statements into one commit.
        let tx = conn
            .unchecked_transaction()
            .await
            .map_err(StoreError::new)?;
        tx.execute("DELETE FROM file_lines WHERE path = ?1", (p.clone(),))
            .await
            .map_err(StoreError::new)?;
        if !state.lines.is_empty() {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO file_lines (path, line_no, hash, anchor) VALUES (?1, ?2, ?3, ?4)",
                )
                .await
                .map_err(StoreError::new)?;
            for (line_no, le) in state.lines.iter().enumerate() {
                stmt.execute((
                    p.clone(),
                    line_no as i64,
                    le.hash as i64,
                    le.anchor.to_bytes(),
                ))
                .await
                .map_err(StoreError::new)?;
                stmt.reset().map_err(StoreError::new)?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO file_meta (path, mtime_ns, size, content_digest) VALUES (?1, ?2, ?3, ?4)",
            (p, state.mtime_ns, state.size as i64, state.content_digest as i64),
        )
        .await
        .map_err(StoreError::new)?;
        tx.commit().await.map_err(StoreError::new)?;
        Ok(())
    }

    async fn used_anchors(&self, path: &Path) -> StoreResult<HashSet<Anchor>> {
        let conn = self.db.connect().await;
        let p = path_str(path)?;
        let mut rows = conn
            .query(
                "SELECT anchor FROM file_lines WHERE path = ?1
                 UNION
                 SELECT anchor FROM file_tombstones WHERE path = ?1",
                (p,),
            )
            .await
            .map_err(StoreError::new)?;
        let mut out = HashSet::new();
        while let Some(row) = rows.next().await.map_err(StoreError::new)? {
            let bytes = col_blob(&row, 0, "anchor")?;
            let anchor = Anchor::from_bytes(&bytes).map_err(StoreError::new)?;
            out.insert(anchor);
        }
        Ok(out)
    }

    async fn tombstone(&self, path: &Path, anchors: &[Anchor]) -> StoreResult<()> {
        let conn = self.db.connect().await;
        let p = path_str(path)?;
        let tx = conn
            .unchecked_transaction()
            .await
            .map_err(StoreError::new)?;
        let mut stmt = tx
            .prepare("INSERT OR IGNORE INTO file_tombstones (path, anchor) VALUES (?1, ?2)")
            .await
            .map_err(StoreError::new)?;
        for anchor in anchors {
            stmt.execute((p.clone(), anchor.to_bytes()))
                .await
                .map_err(StoreError::new)?;
            stmt.reset().map_err(StoreError::new)?;
        }
        tx.commit().await.map_err(StoreError::new)?;
        Ok(())
    }

    async fn clear_tombstones(&self) -> StoreResult<()> {
        let conn = self.db.connect().await;
        conn.execute("DELETE FROM file_tombstones", ())
            .await
            .map_err(StoreError::new)?;
        Ok(())
    }
}
