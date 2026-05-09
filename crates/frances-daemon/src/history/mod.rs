use thiserror::Error;
use uuid::Uuid;

use crate::migrations::{EntitySchema, Migration};
use crate::store::Database;

mod messages;
mod sessions;
mod types;

pub use types::{Block, ChatSessionId, ChatSessionRow, OwnedHistoryInput, RowId, RowSeq};

/// Owns the conversation history. UUID is permanent — never edit.
pub static SCHEMA: EntitySchema = EntitySchema {
    entity: Uuid::from_u128(0x7ffee42d_48de_4090_8fc6_a25e66f33a02),
    migrations: &[Migration {
        name: "0001_init.sql",
        sql: include_str!("migrations/0001_init.sql"),
    }],
};

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("turso: {0}")]
    Turso(#[from] turso::Error),
    #[error("encode {what}: {source}")]
    Encode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("decode {what}: {source}")]
    Decode {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("chat_session {0} not found")]
    ChatSessionNotFound(ChatSessionId),
    #[error("expected text in {column}, got {found:?}")]
    NonTextColumn {
        column: &'static str,
        found: turso::Value,
    },
    #[error("primitive of type {kind:?} missing field {field:?}")]
    PrimitiveMissingField {
        kind: &'static str,
        field: &'static str,
    },
    #[error("unknown primitive type {0:?}")]
    UnknownPrimitiveType(String),
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    db: Database,
}

impl HistoryStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

pub(super) async fn next_seq(
    conn: &turso::Connection,
    session: ChatSessionId,
) -> std::result::Result<RowSeq, HistoryError> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM chat_messages WHERE chat_session_id = ?1",
            (session.0,),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .expect("COALESCE(MAX(seq), -1) + 1 always returns one row");
    Ok(RowSeq(row.get::<i64>(0)?))
}

pub(super) async fn last_insert_rowid(
    conn: &turso::Connection,
) -> std::result::Result<i64, HistoryError> {
    let mut rows = conn.query("SELECT last_insert_rowid()", ()).await?;
    let row = rows
        .next()
        .await?
        .expect("last_insert_rowid() always returns one row");
    Ok(row.get::<i64>(0)?)
}
