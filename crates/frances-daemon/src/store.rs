use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use frances_storage::MigrationError;
use thiserror::Error;
use tracing::trace;
use turso::{Builder, Connection};

use crate::session::Session;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("turso: {0}")]
    Turso(#[from] turso::Error),
    #[error(transparent)]
    Migration(#[from] MigrationError),
}

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
    pub async fn open(session: &Session) -> std::result::Result<Self, DatabaseError> {
        let path = session.database_path();
        trace!(path = %path.display(), "opening turso database");

        let database = Builder::new_local(&path.to_string_lossy()).build().await?;
        let conn = database.connect()?;

        trace!(path = %path.display(), "running schema migrations");
        frances_storage::run_all(
            conn,
            &[
                &crate::anchor_store::SCHEMA,
                &crate::history::SCHEMA,
                &crate::llm::session_provider::SCHEMA,
            ],
        )
        .await?;
        Ok(())
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
