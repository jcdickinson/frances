use frances_llm::{HistoryInput, Provider};
use serde_json::Value;
use tracing::trace;

use crate::Result;

use super::{
    Block, ChatSessionId, HistoryError, HistoryStore, OwnedHistoryInput, RowId, last_insert_rowid,
    next_seq,
};

impl HistoryStore {
    pub async fn append_primitive_user(&self, session: ChatSessionId, text: &str) -> Result<RowId> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive(session, "user", &primitive).await
    }

    pub async fn append_primitive_assistant(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive(session, "assistant", &primitive)
            .await
    }

    pub async fn append_primitive_tool_call(
        &self,
        session: ChatSessionId,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> Result<RowId> {
        let primitive = serde_json::json!({
            "id": id,
            "name": name,
            "arguments": arguments,
        });
        self.append_primitive(session, "tool_call", &primitive)
            .await
    }

    pub async fn append_primitive_tool_result(
        &self,
        session: ChatSessionId,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<RowId> {
        let primitive = serde_json::json!({
            "call_id": call_id,
            "content": content,
            "is_error": is_error,
        });
        self.append_primitive(session, "tool_result", &primitive)
            .await
    }

    /// Bulk-insert wire-tagged history rows. Each `payload` becomes one row
    /// with the supplied `(kind, provider_id)` tag; rows take consecutive
    /// per-session seq values.
    pub async fn append_history(
        &self,
        session: ChatSessionId,
        kind: &str,
        provider_id: &str,
        payloads: &[Value],
    ) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }

        trace!(
            session = %session,
            kind = kind,
            provider_id = provider_id,
            count = payloads.len(),
            "appending history rows"
        );

        let conn = self.db.connect();
        for payload in payloads {
            let seq = next_seq(&conn, session).await?;
            let payload_text =
                serde_json::to_string(payload).map_err(|source| HistoryError::Encode {
                    what: "history payload",
                    source,
                })?;
            conn.execute(
                "INSERT INTO chat_messages (chat_session_id, seq, type, history, kind, provider_id) \
                 VALUES (?1, ?2, 'history', jsonb(?3), ?4, ?5)",
                (session.0, seq.0, payload_text, kind, provider_id),
            )
            .await
            .map_err(HistoryError::Turso)?;
        }

        Ok(())
    }

    /// Wire JSON to send to the LLM, in seq order.
    pub async fn loaded_history(&self, session: ChatSessionId) -> Result<Vec<Value>> {
        trace!(session = %session, "loading history payloads");

        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "SELECT json(history) FROM chat_messages \
                 WHERE chat_session_id = ?1 AND history IS NOT NULL ORDER BY seq",
                (session.0,),
            )
            .await
            .map_err(HistoryError::Turso)?;

        let mut payloads = Vec::new();
        while let Some(row) = rows.next().await.map_err(HistoryError::Turso)? {
            let text = match row.get_value(0).map_err(HistoryError::Turso)? {
                turso::Value::Text(value) => value,
                other => {
                    return Err(HistoryError::NonTextColumn {
                        column: "history",
                        found: other,
                    }
                    .into());
                }
            };
            let value: Value =
                serde_json::from_str(&text).map_err(|source| HistoryError::Decode {
                    what: "history payload",
                    source,
                })?;
            payloads.push(value);
        }

        Ok(payloads)
    }

    /// Translate non-history rows for the (currently unwired) TUI replay path.
    pub async fn replay_for_tui(&self, session: ChatSessionId) -> Result<Vec<Block>> {
        let primitives = self.load_primitives(session).await?;
        Ok(primitives
            .into_iter()
            .map(|p| match p {
                OwnedHistoryInput::User { text } | OwnedHistoryInput::Assistant { text } => {
                    Block::Text { text }
                }
                OwnedHistoryInput::ToolCall {
                    id,
                    name,
                    arguments,
                } => Block::ToolUse {
                    id,
                    name,
                    input: arguments,
                },
                OwnedHistoryInput::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => Block::ToolResult {
                    tool_use_id: call_id,
                    content,
                    is_error,
                },
            })
            .collect())
    }

    /// Drop the wire-tagged history rows for `session` and re-forge from
    /// primitives under the supplied provider's wire shape. Currently
    /// unused — swap detection is future work.
    pub async fn purge_and_reforge<P: Provider + 'static>(
        &self,
        session: ChatSessionId,
        provider: &P,
        provider_id: &str,
    ) -> Result<()> {
        let conn = self.db.connect();
        conn.execute(
            "DELETE FROM chat_messages WHERE chat_session_id = ?1 AND type = 'history'",
            (session.0,),
        )
        .await
        .map_err(HistoryError::Turso)?;

        let primitives = self.load_primitives(session).await?;
        let inputs: Vec<HistoryInput<'_>> = primitives
            .iter()
            .map(OwnedHistoryInput::as_borrowed)
            .collect();
        let payloads = provider.forge_history(&inputs);
        self.append_history(session, provider.kind(), provider_id, &payloads)
            .await
    }

    async fn append_primitive(
        &self,
        session: ChatSessionId,
        ty: &str,
        primitive: &Value,
    ) -> Result<RowId> {
        let conn = self.db.connect();
        let seq = next_seq(&conn, session).await?;
        let payload_text =
            serde_json::to_string(primitive).map_err(|source| HistoryError::Encode {
                what: "primitive",
                source,
            })?;
        conn.execute(
            "INSERT INTO chat_messages (chat_session_id, seq, type, primitive) \
             VALUES (?1, ?2, ?3, jsonb(?4))",
            (session.0, seq.0, ty, payload_text),
        )
        .await
        .map_err(HistoryError::Turso)?;
        Ok(RowId(last_insert_rowid(&conn).await?))
    }

    async fn load_primitives(&self, session: ChatSessionId) -> Result<Vec<OwnedHistoryInput>> {
        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "SELECT type, json(primitive) FROM chat_messages \
                 WHERE chat_session_id = ?1 AND primitive IS NOT NULL ORDER BY seq",
                (session.0,),
            )
            .await
            .map_err(HistoryError::Turso)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(HistoryError::Turso)? {
            let ty = row.get::<String>(0).map_err(HistoryError::Turso)?;
            let payload_text = match row.get_value(1).map_err(HistoryError::Turso)? {
                turso::Value::Text(value) => value,
                other => {
                    return Err(HistoryError::NonTextColumn {
                        column: "primitive",
                        found: other,
                    }
                    .into());
                }
            };
            let payload: Value =
                serde_json::from_str(&payload_text).map_err(|source| HistoryError::Decode {
                    what: "primitive",
                    source,
                })?;

            let owned = match ty.as_str() {
                "user" => OwnedHistoryInput::User {
                    text: take_string(&payload, "user", "text")?,
                },
                "assistant" => OwnedHistoryInput::Assistant {
                    text: take_string(&payload, "assistant", "text")?,
                },
                "tool_call" => OwnedHistoryInput::ToolCall {
                    id: take_string(&payload, "tool_call", "id")?,
                    name: take_string(&payload, "tool_call", "name")?,
                    arguments: payload.get("arguments").cloned().ok_or(
                        HistoryError::PrimitiveMissingField {
                            kind: "tool_call",
                            field: "arguments",
                        },
                    )?,
                },
                "tool_result" => OwnedHistoryInput::ToolResult {
                    call_id: take_string(&payload, "tool_result", "call_id")?,
                    content: take_string(&payload, "tool_result", "content")?,
                    is_error: payload
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
                other => return Err(HistoryError::UnknownPrimitiveType(other.to_string()).into()),
            };
            out.push(owned);
        }

        Ok(out)
    }
}

fn take_string(
    value: &Value,
    kind: &'static str,
    key: &'static str,
) -> std::result::Result<String, HistoryError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(HistoryError::PrimitiveMissingField { kind, field: key })
}
