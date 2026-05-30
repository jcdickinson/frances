use std::borrow::Cow;

use async_trait::async_trait;
use frances_llm::HistoryStore as HistoryStoreTrait;
use frances_models_llm::chat::{
    BatchRow, ChatSessionId, HistoryBatch, HistoryError, ModelIntents, OwnedHistoryInput, RowId,
};
use serde_json::Value;
use tracing::trace;

use super::{TursoHistoryStore, next_seq, turso_err};

#[async_trait]
impl HistoryStoreTrait for TursoHistoryStore {
    async fn create_chat_session(
        &self,
        session_id: &str,
        model_intents: &[Cow<'static, str>],
    ) -> Result<ChatSessionId, HistoryError> {
        let conn = self.db().connect().await;
        let created_at = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .unwrap_or(i64::MAX);
        let intents_json =
            serde_json::to_string(model_intents).map_err(|source| HistoryError::Encode {
                what: "model_intents",
                source,
            })?;
        conn.execute(
            "INSERT INTO chat_sessions (session_id, model_intents, created_at) \
             VALUES (?1, jsonb(?2), ?3)",
            (session_id, intents_json, created_at),
        )
        .await
        .map_err(turso_err)?;
        Ok(ChatSessionId(conn.last_insert_rowid()))
    }

    async fn load_chat_session(
        &self,
        id: ChatSessionId,
    ) -> Result<frances_models_llm::chat::ChatSessionRow, HistoryError> {
        let conn = self.db().connect().await;
        let mut rows = conn
            .query(
                "SELECT session_id, json(model_intents) FROM chat_sessions WHERE id = ?1",
                (id.0,),
            )
            .await
            .map_err(turso_err)?;
        let row = rows
            .next()
            .await
            .map_err(turso_err)?
            .ok_or(HistoryError::ChatSessionNotFound(id))?;
        let session_id: String = row.get(0).map_err(turso_err)?;
        let intents_text: String = row.get(1).map_err(turso_err)?;
        let raw_intents: Vec<String> =
            serde_json::from_str(&intents_text).map_err(|source| HistoryError::Decode {
                what: "model_intents",
                source,
            })?;
        let model_intents: ModelIntents =
            Cow::Owned(raw_intents.into_iter().map(Cow::Owned).collect());
        Ok(frances_models_llm::chat::ChatSessionRow {
            id,
            session_id,
            model_intents,
        })
    }

    async fn loaded_history(&self, session: ChatSessionId) -> Result<Vec<Value>, HistoryError> {
        trace!(session = %session, "loading history payloads");

        let conn = self.db().connect().await;
        let mut rows = conn
            .query(
                "SELECT json(history) FROM chat_messages \
                 WHERE chat_session_id = ?1 AND history IS NOT NULL ORDER BY seq",
                (session.0,),
            )
            .await
            .map_err(turso_err)?;

        let mut payloads = Vec::new();
        while let Some(row) = rows.next().await.map_err(turso_err)? {
            let text = expect_text(row.get_value(0).map_err(turso_err)?, "history")?;
            let value: Value =
                serde_json::from_str(&text).map_err(|source| HistoryError::Decode {
                    what: "history payload",
                    source,
                })?;
            payloads.push(value);
        }

        Ok(payloads)
    }

    async fn append_history(
        &self,
        session: ChatSessionId,
        kind: &str,
        provider_id: &str,
        payloads: &[Value],
    ) -> Result<(), HistoryError> {
        trace!(
            session = %session,
            kind = kind,
            provider_id = provider_id,
            count = payloads.len(),
            "appending history rows"
        );

        let mut batch = HistoryBatch::default();
        for payload in payloads {
            batch.history(payload, kind, provider_id)?;
        }
        self.flush(session, batch).await
    }

    async fn flush(&self, session: ChatSessionId, batch: HistoryBatch) -> Result<(), HistoryError> {
        if batch.is_empty() {
            return Ok(());
        }

        let conn = self.db().connect().await;
        // One sequence read, then every row inserted under a single
        // transaction. The connection lock is held for the whole flush,
        // so reading `base` before `BEGIN` is race-free.
        let base = next_seq(&conn, session).await?;
        let tx = conn.unchecked_transaction().await.map_err(turso_err)?;
        for (i, row) in batch.rows.iter().enumerate() {
            let seq = base + i as i64;
            match row {
                BatchRow::Primitive { ty, json } => {
                    tx.execute(
                        "INSERT INTO chat_messages (chat_session_id, seq, type, primitive) \
                         VALUES (?1, ?2, ?3, jsonb(?4))",
                        (session.0, seq, *ty, json.as_str()),
                    )
                    .await
                    .map_err(turso_err)?;
                }
                BatchRow::History {
                    json,
                    kind,
                    provider_id,
                } => {
                    tx.execute(
                        "INSERT INTO chat_messages (chat_session_id, seq, type, history, kind, provider_id) \
                         VALUES (?1, ?2, 'history', jsonb(?3), ?4, ?5)",
                        (session.0, seq, json.as_str(), kind.as_str(), provider_id.as_str()),
                    )
                    .await
                    .map_err(turso_err)?;
                }
            }
        }
        tx.commit().await.map_err(turso_err)?;
        Ok(())
    }

    async fn checkpoint(&self, session: ChatSessionId) -> Result<RowId, HistoryError> {
        let conn = self.db().connect().await;
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(id), 0) FROM chat_messages WHERE chat_session_id = ?1",
                (session.0,),
            )
            .await
            .map_err(turso_err)?;
        let row = rows
            .next()
            .await
            .map_err(turso_err)?
            .expect("COALESCE(MAX(id), 0) always returns one row");
        let max_id: i64 = row.get(0).map_err(turso_err)?;
        Ok(RowId(max_id))
    }

    async fn rollback(&self, session: ChatSessionId, to: RowId) -> Result<(), HistoryError> {
        let conn = self.db().connect().await;
        conn.execute(
            "DELETE FROM chat_messages WHERE chat_session_id = ?1 AND id > ?2",
            (session.0, to.0),
        )
        .await
        .map_err(turso_err)?;
        Ok(())
    }
}

impl TursoHistoryStore {
    pub(super) async fn load_primitives(
        &self,
        session: ChatSessionId,
    ) -> Result<Vec<OwnedHistoryInput>, HistoryError> {
        let conn = self.db().connect().await;
        let mut rows = conn
            .query(
                "SELECT json(primitive) FROM chat_messages \
                 WHERE chat_session_id = ?1 AND primitive IS NOT NULL ORDER BY seq",
                (session.0,),
            )
            .await
            .map_err(turso_err)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(turso_err)? {
            let payload_text = expect_text(row.get_value(0).map_err(turso_err)?, "primitive")?;
            let owned: OwnedHistoryInput =
                serde_json::from_str(&payload_text).map_err(|source| HistoryError::Decode {
                    what: "primitive",
                    source,
                })?;
            out.push(owned);
        }

        Ok(out)
    }
}

/// Unwrap a turso column known to hold text, carrying the offending
/// [`turso::Value`] in the error rather than stringifying it eagerly —
/// it's rendered only at the `Display` boundary.
fn expect_text(value: turso::Value, column: &'static str) -> Result<String, HistoryError> {
    match value {
        turso::Value::Text(text) => Ok(text),
        found => Err(HistoryError::Backend(Box::new(NonText { column, found }))),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("expected text in {column}, got {found:?}")]
struct NonText {
    column: &'static str,
    found: turso::Value,
}
