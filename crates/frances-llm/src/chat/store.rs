use async_trait::async_trait;
use serde_json::Value;

use frances_models_llm::chat::{
    ChatSessionId, ChatSessionRow, HistoryError, OwnedHistoryInput, RowId,
};

/// Persistence boundary for chat sessions. The daemon impls this on its
/// turso-backed store; tests can mock it.
#[async_trait]
pub trait HistoryStore: Send + Sync + 'static {
    async fn create_chat_session(
        &self,
        session_id: &str,
        model_intents: &[String],
    ) -> Result<ChatSessionId, HistoryError>;

    async fn load_chat_session(&self, id: ChatSessionId) -> Result<ChatSessionRow, HistoryError>;

    async fn primary_chat_session(&self) -> Result<Option<ChatSessionId>, HistoryError>;

    async fn insert_primary_chat_session(&self, id: ChatSessionId) -> Result<(), HistoryError>;

    async fn loaded_history(&self, session: ChatSessionId) -> Result<Vec<Value>, HistoryError>;

    async fn append_history(
        &self,
        session: ChatSessionId,
        kind: &str,
        provider_id: &str,
        payloads: &[Value],
    ) -> Result<(), HistoryError>;

    async fn append_primitive_system(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId, HistoryError>;

    async fn append_primitive_user(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId, HistoryError>;

    async fn append_primitive_assistant(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId, HistoryError>;

    async fn append_primitive_tool_call(
        &self,
        session: ChatSessionId,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> Result<RowId, HistoryError>;

    async fn append_primitive_tool_result(
        &self,
        session: ChatSessionId,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<RowId, HistoryError>;

    async fn append_primitive(
        &self,
        session: ChatSessionId,
        input: &OwnedHistoryInput,
    ) -> Result<RowId, HistoryError> {
        match input {
            OwnedHistoryInput::System { text } => self.append_primitive_system(session, text).await,
            OwnedHistoryInput::User { text } => self.append_primitive_user(session, text).await,
            OwnedHistoryInput::Assistant { text } => {
                self.append_primitive_assistant(session, text).await
            }
            OwnedHistoryInput::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.append_primitive_tool_call(session, id, name, arguments)
                    .await
            }
            OwnedHistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                self.append_primitive_tool_result(session, call_id, content, *is_error)
                    .await
            }
        }
    }
}
