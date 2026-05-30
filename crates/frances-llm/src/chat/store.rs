use std::borrow::Cow;

use async_trait::async_trait;
use serde_json::Value;

use frances_models_llm::chat::{ChatSessionId, ChatSessionRow, HistoryBatch, HistoryError, RowId};

/// Persistence boundary for chat sessions. The session-runtime impls this on its
/// turso-backed store; tests can mock it.
#[async_trait]
pub trait HistoryStore: Send + Sync + 'static {
    async fn create_chat_session(
        &self,
        session_id: &str,
        model_intents: &[Cow<'static, str>],
    ) -> Result<ChatSessionId, HistoryError>;

    async fn load_chat_session(&self, id: ChatSessionId) -> Result<ChatSessionRow, HistoryError>;

    async fn loaded_history(&self, session: ChatSessionId) -> Result<Vec<Value>, HistoryError>;

    async fn append_history(
        &self,
        session: ChatSessionId,
        kind: &str,
        provider_id: &str,
        payloads: &[Value],
    ) -> Result<(), HistoryError>;

    /// Persist a whole turn's worth of primitive and forged-history rows
    /// in one transaction: a single sequence read, then one insert per
    /// [`BatchRow`]. No-op on an empty batch.
    async fn flush(&self, session: ChatSessionId, batch: HistoryBatch) -> Result<(), HistoryError>;

    /// Highest persisted row id for `session` (`RowId(0)` when empty).
    /// Used as a rollback marker: rows appended after this id can be
    /// discarded by [`rollback`](Self::rollback). Default returns
    /// `RowId(0)` for stores that don't support truncation (test mocks).
    async fn checkpoint(&self, _session: ChatSessionId) -> Result<RowId, HistoryError> {
        Ok(RowId(0))
    }

    /// Delete every persisted message for `session` whose row id is
    /// greater than `to` (the marker from [`checkpoint`](Self::checkpoint)).
    /// Removes both primitive rows and forged-history rows in one pass.
    /// Default is a no-op for stores that don't support truncation.
    async fn rollback(&self, _session: ChatSessionId, _to: RowId) -> Result<(), HistoryError> {
        Ok(())
    }
}
