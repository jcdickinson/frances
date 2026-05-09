use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::trace;
use turso::{Builder, Connection};

use crate::migrations;
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

        trace!(path = %path.display(), "running schema migrations");
        migrations::run_all(
            &conn,
            &[
                &crate::tools::FILE_SCHEMA,
                &crate::history::SCHEMA,
                &crate::llm::session_provider::SCHEMA,
            ],
        )
        .await
        .context("run schema migrations")?;

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

#[cfg(test)]
pub(crate) mod test_support {
    use super::Database;
    use crate::session::{Paths, Session, SessionMeta};
    use std::ops::Deref;
    use tempfile::TempDir;

    /// A [`Database`] backed by a fresh `TempDir` that is removed when the
    /// `TempDb` drops. `db` is declared before `_dir` so the connection
    /// closes before the directory is unlinked.
    pub struct TempDb {
        db: Database,
        _dir: TempDir,
    }

    impl TempDb {
        pub async fn open() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let session = Session {
                paths: Paths {
                    state_root: dir.path().join("state"),
                    runtime_root: dir.path().join("runtime"),
                },
                id: "test".into(),
                dir: dir.path().to_path_buf(),
                runtime_dir: dir.path().join("runtime"),
                meta: SessionMeta {
                    version: 1,
                    id: "test".into(),
                    created: 0,
                    cwd: None,
                    reserved: None,
                },
            };
            let db = Database::open(&session).await.unwrap();
            Self { db, _dir: dir }
        }
    }

    impl Deref for TempDb {
        type Target = Database;
        fn deref(&self) -> &Database {
            &self.db
        }
    }
}
