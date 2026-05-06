use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use frances_edit::{Anchor, AnchorStore, FileAnchorState, LineEntry};
use turso::Value;

use crate::store::Database;

pub struct AnchorStoreImpl {
    db: Database,
}

impl AnchorStoreImpl {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("non-UTF8 path: {}", p.display()))
}

fn col_i64(row: &turso::Row, idx: usize) -> Result<i64> {
    match row.get_value(idx).context("column value")? {
        Value::Integer(v) => Ok(v),
        other => Err(anyhow!("expected integer, got {other:?}")),
    }
}

fn col_blob(row: &turso::Row, idx: usize) -> Result<Vec<u8>> {
    match row.get_value(idx).context("column value")? {
        Value::Blob(v) => Ok(v),
        other => Err(anyhow!("expected blob, got {other:?}")),
    }
}

#[async_trait]
impl AnchorStore for AnchorStoreImpl {
    async fn load(&self, path: &Path) -> Result<Option<FileAnchorState>> {
        let conn = self.db.connect();
        let p = path_str(path)?;

        let mut rows = conn
            .query(
                "SELECT mtime_ns, size, content_digest FROM file_meta WHERE path = ?1",
                (p.clone(),),
            )
            .await
            .context("query file_meta")?;
        let row = match rows.next().await.context("iterate file_meta")? {
            Some(r) => r,
            None => return Ok(None),
        };
        let mtime_ns = col_i64(&row, 0)?;
        let size = col_i64(&row, 1)? as u64;
        let content_digest = col_i64(&row, 2)? as u64;

        let mut rows = conn
            .query(
                "SELECT hash, anchor FROM file_lines WHERE path = ?1 ORDER BY line_no",
                (p,),
            )
            .await
            .context("query file_lines")?;

        let mut lines: Vec<LineEntry> = Vec::new();
        while let Some(row) = rows.next().await.context("iterate file_lines")? {
            let hash = col_i64(&row, 0)? as u64;
            let anchor_bytes = col_blob(&row, 1)?;
            let anchor = Anchor::from_bytes(&anchor_bytes)
                .map_err(|e| anyhow!("decode anchor blob: {e}"))?;
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
    ) -> Result<()> {
        let conn = self.db.connect();
        conn.execute(
            "INSERT OR REPLACE INTO file_meta (path, mtime_ns, size, content_digest) VALUES (?1, ?2, ?3, ?4)",
            (path_str(path)?, mtime_ns, size as i64, content_digest as i64),
        )
        .await
        .context("upsert file_meta")?;
        Ok(())
    }

    async fn upsert_lines(&self, path: &Path, lines: &[(u32, u64, Anchor)]) -> Result<()> {
        let conn = self.db.connect();
        let p = path_str(path)?;
        for (line_no, hash, anchor) in lines {
            conn.execute(
                "INSERT OR REPLACE INTO file_lines (path, line_no, hash, anchor) VALUES (?1, ?2, ?3, ?4)",
                (
                    p.clone(),
                    *line_no as i64,
                    *hash as i64,
                    anchor.to_bytes(),
                ),
            )
            .await
            .context("upsert file_lines")?;
        }
        Ok(())
    }

    async fn delete_lines(&self, path: &Path, line_nos: &[u32]) -> Result<()> {
        let conn = self.db.connect();
        let p = path_str(path)?;
        for n in line_nos {
            conn.execute(
                "DELETE FROM file_lines WHERE path = ?1 AND line_no = ?2",
                (p.clone(), *n as i64),
            )
            .await
            .context("delete file_lines")?;
        }
        Ok(())
    }

    async fn truncate_lines(&self, path: &Path) -> Result<()> {
        let conn = self.db.connect();
        conn.execute("DELETE FROM file_lines WHERE path = ?1", (path_str(path)?,))
            .await
            .context("truncate file_lines")?;
        Ok(())
    }

    async fn used_anchors(&self, path: &Path) -> Result<HashSet<Anchor>> {
        let conn = self.db.connect();
        let p = path_str(path)?;
        let mut rows = conn
            .query(
                "SELECT anchor FROM file_lines WHERE path = ?1
                 UNION
                 SELECT anchor FROM file_tombstones WHERE path = ?1",
                (p,),
            )
            .await
            .context("query used anchors")?;
        let mut out = HashSet::new();
        while let Some(row) = rows.next().await.context("iterate used anchors")? {
            let bytes = col_blob(&row, 0)?;
            let anchor =
                Anchor::from_bytes(&bytes).map_err(|e| anyhow!("decode anchor blob: {e}"))?;
            out.insert(anchor);
        }
        Ok(out)
    }

    async fn tombstone(&self, path: &Path, anchors: &[Anchor]) -> Result<()> {
        let conn = self.db.connect();
        let p = path_str(path)?;
        for anchor in anchors {
            conn.execute(
                "INSERT OR IGNORE INTO file_tombstones (path, anchor) VALUES (?1, ?2)",
                (p.clone(), anchor.to_bytes()),
            )
            .await
            .context("insert tombstone")?;
        }
        Ok(())
    }

    async fn clear_tombstones(&self) -> Result<()> {
        let conn = self.db.connect();
        conn.execute("DELETE FROM file_tombstones", ())
            .await
            .context("clear tombstones")?;
        Ok(())
    }

    async fn forget(&self, path: &Path) -> Result<()> {
        let conn = self.db.connect();
        let p = path_str(path)?;
        conn.execute("DELETE FROM file_meta WHERE path = ?1", (p.clone(),))
            .await
            .context("delete file_meta")?;
        conn.execute("DELETE FROM file_lines WHERE path = ?1", (p.clone(),))
            .await
            .context("delete file_lines")?;
        conn.execute("DELETE FROM file_tombstones WHERE path = ?1", (p,))
            .await
            .context("delete file_tombstones")?;
        Ok(())
    }
}
