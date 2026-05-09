use crate::Result;

use super::{ChatSessionId, ChatSessionRow, HistoryError, HistoryStore, last_insert_rowid};

impl HistoryStore {
    pub async fn create_chat_session(
        &self,
        session_id: &str,
        model_intents: &[String],
    ) -> Result<ChatSessionId> {
        let conn = self.db.connect();
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
        .map_err(HistoryError::Turso)?;
        let id = last_insert_rowid(&conn).await?;
        Ok(ChatSessionId(id))
    }

    pub async fn load_chat_session(&self, id: ChatSessionId) -> Result<ChatSessionRow> {
        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "SELECT session_id, json(model_intents) FROM chat_sessions WHERE id = ?1",
                (id.0,),
            )
            .await
            .map_err(HistoryError::Turso)?;
        let row = rows
            .next()
            .await
            .map_err(HistoryError::Turso)?
            .ok_or(HistoryError::ChatSessionNotFound(id))?;
        let session_id: String = row.get(0).map_err(HistoryError::Turso)?;
        let intents_text: String = row.get(1).map_err(HistoryError::Turso)?;
        let model_intents: Vec<String> =
            serde_json::from_str(&intents_text).map_err(|source| HistoryError::Decode {
                what: "model_intents",
                source,
            })?;
        Ok(ChatSessionRow {
            id,
            session_id,
            model_intents,
        })
    }

    /// Returns the most recently pinned primary chat session, if any.
    /// The TUI's hardcoded turn workflow drives this session.
    pub async fn primary_chat_session(&self) -> Result<Option<ChatSessionId>> {
        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "SELECT chat_session_id FROM primary_chat_session ORDER BY id DESC LIMIT 1",
                (),
            )
            .await
            .map_err(HistoryError::Turso)?;
        match rows.next().await.map_err(HistoryError::Turso)? {
            Some(row) => Ok(Some(ChatSessionId(
                row.get::<i64>(0).map_err(HistoryError::Turso)?,
            ))),
            None => Ok(None),
        }
    }

    /// Append `id` as the new primary chat session. Earlier pins stay
    /// in the table as history; the latest row wins on read.
    pub async fn insert_primary_chat_session(&self, id: ChatSessionId) -> Result<()> {
        let conn = self.db.connect();
        conn.execute(
            "INSERT INTO primary_chat_session (chat_session_id) VALUES (?1)",
            (id.0,),
        )
        .await
        .map_err(HistoryError::Turso)?;
        Ok(())
    }
}
