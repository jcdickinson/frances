use std::borrow::Cow;

use frances_llm::HistoryStore as HistoryStoreTrait;
use frances_models_llm::chat::{ChatSessionId, HistoryError, OwnedHistoryInput};
use frances_storage::{EntitySchema, Migration};
use uuid::Uuid;

use crate::store::Database;

mod messages;

pub use messages::Block;

/// Owns the conversation history. UUID is permanent — never edit.
pub static SCHEMA: EntitySchema = EntitySchema {
    entity: Uuid::from_u128(0x7ffee42d_48de_4090_8fc6_a25e66f33a02),
    migrations: Cow::Borrowed(&[Migration {
        name: Cow::Borrowed("0001_init.sql"),
        sql: Cow::Borrowed(include_str!("migrations/0001_init.sql")),
    }]),
};

/// Turso-backed implementation of [`frances_llm::HistoryStore`].
/// Clone-by-value handle; the underlying `Database` already holds an
/// Arc-wrapped connection pool.
#[derive(Debug, Clone)]
pub struct TursoHistoryStore {
    db: Database,
}

impl TursoHistoryStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub(crate) fn db(&self) -> &Database {
        &self.db
    }

    /// Drop the wire-tagged history rows for `session` and re-forge from
    /// primitives under the supplied provider's wire shape. Currently
    /// unused — swap detection is future work. Lives as an inherent
    /// method rather than on the `HistoryStore` trait because it's
    /// TUI-only and pulls in the `Provider` trait.
    pub async fn purge_and_reforge<P: frances_llm::Provider + 'static>(
        &self,
        session: ChatSessionId,
        provider: &P,
        provider_id: &str,
    ) -> Result<(), HistoryError> {
        use frances_models_llm::wire::HistoryInput;

        let conn = self.db.connect();
        conn.execute(
            "DELETE FROM chat_messages WHERE chat_session_id = ?1 AND type = 'history'",
            (session.0,),
        )
        .await
        .map_err(turso_err)?;

        let primitives = self.load_primitives(session).await?;
        let inputs: Vec<HistoryInput<'_>> = primitives
            .iter()
            .map(OwnedHistoryInput::as_borrowed)
            .collect();
        let payloads = provider.forge_history(&inputs);
        <Self as HistoryStoreTrait>::append_history(
            self,
            session,
            provider.kind(),
            provider_id,
            &payloads,
        )
        .await
    }
}

pub(crate) fn turso_err(source: turso::Error) -> HistoryError {
    HistoryError::Backend(Box::new(TursoError(source)))
}

#[derive(Debug, thiserror::Error)]
#[error("turso: {0}")]
struct TursoError(#[source] turso::Error);

pub(super) async fn next_seq(
    conn: &turso::Connection,
    session: ChatSessionId,
) -> Result<i64, HistoryError> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM chat_messages WHERE chat_session_id = ?1",
            (session.0,),
        )
        .await
        .map_err(turso_err)?;
    let row = rows
        .next()
        .await
        .map_err(turso_err)?
        .expect("COALESCE(MAX(seq), -1) + 1 always returns one row");
    row.get::<i64>(0).map_err(turso_err)
}

pub(super) async fn last_insert_rowid(conn: &turso::Connection) -> Result<i64, HistoryError> {
    let mut rows = conn
        .query("SELECT last_insert_rowid()", ())
        .await
        .map_err(turso_err)?;
    let row = rows
        .next()
        .await
        .map_err(turso_err)?
        .expect("last_insert_rowid() always returns one row");
    row.get::<i64>(0).map_err(turso_err)
}
