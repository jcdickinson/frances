use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::trace;
use turso::{Builder, Connection};

use crate::session::Session;

#[derive(Clone)]
pub struct Database {
    conn: Connection,
    path: Arc<PathBuf>,
}

pub struct ActiveDatabase(Connection);

impl Deref for ActiveDatabase {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl Drop for ActiveDatabase {
    fn drop(&mut self) {
        if let Err(error) = self.0.cacheflush() {
            tracing::warn!(%error, "cacheflush failed");
        }
    }
}

impl Database {
    pub async fn open(session: &Session) -> Result<Self> {
        let path = session.database_path();
        trace!(path = %path.display(), "opening turso database");

        let database = Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .context("build turso database")?;
        let conn = database.connect().context("connect turso database")?;

        trace!(path = %path.display(), "initializing turso schema");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                seq INTEGER NOT NULL UNIQUE,
                role TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS blocks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                payload JSONB NOT NULL,
                FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_blocks_message_seq ON blocks(message_id, seq);

            CREATE TABLE IF NOT EXISTS openai_messages (
                message_id INTEGER PRIMARY KEY,
                payload JSONB NOT NULL,
                FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS openai_response_chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                chunk JSONB NOT NULL,
                FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_openai_response_chunks_message_seq
                ON openai_response_chunks(message_id, seq);

            CREATE TABLE IF NOT EXISTS file_meta (
                path           TEXT PRIMARY KEY,
                mtime_ns       INTEGER NOT NULL,
                size           INTEGER NOT NULL,
                content_digest INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS file_lines (
                path    TEXT    NOT NULL,
                line_no INTEGER NOT NULL,
                hash    INTEGER NOT NULL,
                anchor  BLOB    NOT NULL,
                PRIMARY KEY(path, line_no)
            );

            CREATE TABLE IF NOT EXISTS file_tombstones (
                path   TEXT NOT NULL,
                anchor BLOB NOT NULL,
                PRIMARY KEY(path, anchor)
            );
            "#,
        )
        .await
        .context("initialize turso schema")?;

        Ok(Self {
            conn,
            path: Arc::new(path),
        })
    }

    pub fn connect(&self) -> ActiveDatabase {
        ActiveDatabase(self.conn.clone())
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &*self.path)
            .finish()
    }
}
