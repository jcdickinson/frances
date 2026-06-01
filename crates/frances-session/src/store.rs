//! Session-runtime wiring around the workspace-shared [`Database`].
//!
//! [`Database`] itself lives in `frances-storage` so that crates
//! outside the session runtime (the workflow runtime, in particular) hold the
//! same lock when they touch the per-session turso connection. This
//! module just exposes the runtime's open-and-migrate flow plus an
//! in-memory variant for tests.

use frances_storage::MigrationError;
pub use frances_storage::{ActiveDatabase, Database};
use thiserror::Error;
use tracing::trace;

use crate::session::Session;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("turso: {0}")]
    Turso(#[from] turso::Error),
    #[error(transparent)]
    Migration(#[from] MigrationError),
}

/// Open the per-session database and run every session-runtime schema.
pub async fn open(session: &Session) -> std::result::Result<Database, DatabaseError> {
    let path = session.database_path();
    trace!(path = %path.display(), "opening turso database");
    let db = Database::open(path.to_string_lossy().into_owned()).await?;

    trace!(path = %path.display(), "running schema migrations");
    apply_migrations(&db).await?;
    Ok(db)
}

/// Build a fresh in-memory database with all session-runtime schemas applied.
#[cfg(test)]
pub(crate) async fn open_in_memory() -> std::result::Result<Database, DatabaseError> {
    let db = Database::open_in_memory().await?;
    apply_migrations(&db).await?;
    Ok(db)
}

async fn apply_migrations(db: &Database) -> std::result::Result<(), DatabaseError> {
    let conn = db.connect().await;
    frances_storage::run_all(
        &conn,
        &[
            &crate::anchor_store::SCHEMA,
            &crate::history::SCHEMA,
            &crate::llm::session_provider::SCHEMA,
            &crate::workflows::SCHEMA,
            &crate::scrollback::SCHEMA,
        ],
    )
    .await?;
    Ok(())
}
