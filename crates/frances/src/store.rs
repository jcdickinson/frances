use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::trace;
use turso::{Builder, Connection, Database};

use crate::session::Paths;

#[derive(Clone)]
pub struct Store {
    database: Database,
    path: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish()
    }
}

impl Store {
    pub async fn open(paths: &Paths) -> Result<Self> {
        let path = paths.state_root.join("frances.db");
        trace!(path = %path.display(), "opening turso store");

        let database = Builder::new_local(&path.to_string_lossy())
            .build()
            .await
            .context("build turso database")?;

        let store = Self { database, path };
        store.initialize().await?;
        Ok(store)
    }

    pub fn connect(&self) -> Result<Connection> {
        trace!(path = %self.path.display(), "opening turso connection");
        self.database.connect().context("connect turso database")
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    async fn initialize(&self) -> Result<()> {
        let conn = self.connect()?;
        trace!(path = %self.path.display(), "initializing turso schema");
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
                type TEXT NOT NULL,
                text TEXT NOT NULL,
                data BLOB,
                FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_blocks_message_seq ON blocks(message_id, seq);
            "#,
        )
        .await
        .context("initialize turso schema")?;
        Ok(())
    }
}
