use async_trait::async_trait;
use frances_llm::HistoryStore as HistoryStoreTrait;
use frances_models_llm::chat::{ChatSessionId, HistoryError, OwnedHistoryInput, RowId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::trace;

use super::{TursoHistoryStore, last_insert_rowid, next_seq, turso_err};

/// Translation target for the (currently unwired) TUI replay path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[async_trait]
impl HistoryStoreTrait for TursoHistoryStore {
    async fn create_chat_session(
        &self,
        session_id: &str,
        model_intents: &[String],
    ) -> Result<ChatSessionId, HistoryError> {
        let conn = self.db().connect();
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
        let id = last_insert_rowid(&conn).await?;
        Ok(ChatSessionId(id))
    }

    async fn load_chat_session(
        &self,
        id: ChatSessionId,
    ) -> Result<frances_models_llm::chat::ChatSessionRow, HistoryError> {
        let conn = self.db().connect();
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
        let model_intents: Vec<String> =
            serde_json::from_str(&intents_text).map_err(|source| HistoryError::Decode {
                what: "model_intents",
                source,
            })?;
        Ok(frances_models_llm::chat::ChatSessionRow {
            id,
            session_id,
            model_intents,
        })
    }

    async fn primary_chat_session(&self) -> Result<Option<ChatSessionId>, HistoryError> {
        let conn = self.db().connect();
        let mut rows = conn
            .query(
                "SELECT chat_session_id FROM primary_chat_session ORDER BY id DESC LIMIT 1",
                (),
            )
            .await
            .map_err(turso_err)?;
        match rows.next().await.map_err(turso_err)? {
            Some(row) => Ok(Some(ChatSessionId(row.get::<i64>(0).map_err(turso_err)?))),
            None => Ok(None),
        }
    }

    async fn insert_primary_chat_session(&self, id: ChatSessionId) -> Result<(), HistoryError> {
        let conn = self.db().connect();
        conn.execute(
            "INSERT INTO primary_chat_session (chat_session_id) VALUES (?1)",
            (id.0,),
        )
        .await
        .map_err(turso_err)?;
        Ok(())
    }

    async fn loaded_history(&self, session: ChatSessionId) -> Result<Vec<Value>, HistoryError> {
        trace!(session = %session, "loading history payloads");

        let conn = self.db().connect();
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
            let text = match row.get_value(0).map_err(turso_err)? {
                turso::Value::Text(value) => value,
                other => {
                    return Err(HistoryError::Backend(Box::new(NonText {
                        column: "history",
                        found: format!("{other:?}"),
                    })));
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

    async fn append_history(
        &self,
        session: ChatSessionId,
        kind: &str,
        provider_id: &str,
        payloads: &[Value],
    ) -> Result<(), HistoryError> {
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

        let conn = self.db().connect();
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
                (session.0, seq, payload_text, kind, provider_id),
            )
            .await
            .map_err(turso_err)?;
        }

        Ok(())
    }

    async fn append_primitive_system(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId, HistoryError> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive_inner(session, "system", &primitive)
            .await
    }

    async fn append_primitive_user(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId, HistoryError> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive_inner(session, "user", &primitive)
            .await
    }

    async fn append_primitive_assistant(
        &self,
        session: ChatSessionId,
        text: &str,
    ) -> Result<RowId, HistoryError> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive_inner(session, "assistant", &primitive)
            .await
    }

    async fn append_primitive_tool_call(
        &self,
        session: ChatSessionId,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> Result<RowId, HistoryError> {
        let primitive = serde_json::json!({
            "id": id,
            "name": name,
            "arguments": arguments,
        });
        self.append_primitive_inner(session, "tool_call", &primitive)
            .await
    }

    async fn append_primitive_tool_result(
        &self,
        session: ChatSessionId,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<RowId, HistoryError> {
        let primitive = serde_json::json!({
            "call_id": call_id,
            "content": content,
            "is_error": is_error,
        });
        self.append_primitive_inner(session, "tool_result", &primitive)
            .await
    }
}

impl TursoHistoryStore {
    async fn append_primitive_inner(
        &self,
        session: ChatSessionId,
        ty: &str,
        primitive: &Value,
    ) -> Result<RowId, HistoryError> {
        let conn = self.db().connect();
        let seq = next_seq(&conn, session).await?;
        let payload_text =
            serde_json::to_string(primitive).map_err(|source| HistoryError::Encode {
                what: "primitive",
                source,
            })?;
        conn.execute(
            "INSERT INTO chat_messages (chat_session_id, seq, type, primitive) \
             VALUES (?1, ?2, ?3, jsonb(?4))",
            (session.0, seq, ty, payload_text),
        )
        .await
        .map_err(turso_err)?;
        Ok(RowId(last_insert_rowid(&conn).await?))
    }

    /// Translate non-history rows for the (currently unwired) TUI replay path.
    pub async fn replay_for_tui(&self, session: ChatSessionId) -> Result<Vec<Block>, HistoryError> {
        let primitives = self.load_primitives(session).await?;
        Ok(primitives
            .into_iter()
            .map(|p| match p {
                OwnedHistoryInput::System { text }
                | OwnedHistoryInput::User { text }
                | OwnedHistoryInput::Assistant { text } => Block::Text { text },
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

    pub(super) async fn load_primitives(
        &self,
        session: ChatSessionId,
    ) -> Result<Vec<OwnedHistoryInput>, HistoryError> {
        let conn = self.db().connect();
        let mut rows = conn
            .query(
                "SELECT type, json(primitive) FROM chat_messages \
                 WHERE chat_session_id = ?1 AND primitive IS NOT NULL ORDER BY seq",
                (session.0,),
            )
            .await
            .map_err(turso_err)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(turso_err)? {
            let ty = row.get::<String>(0).map_err(turso_err)?;
            let payload_text = match row.get_value(1).map_err(turso_err)? {
                turso::Value::Text(value) => value,
                other => {
                    return Err(HistoryError::Backend(Box::new(NonText {
                        column: "primitive",
                        found: format!("{other:?}"),
                    })));
                }
            };
            let payload: Value =
                serde_json::from_str(&payload_text).map_err(|source| HistoryError::Decode {
                    what: "primitive",
                    source,
                })?;

            let owned = match ty.as_str() {
                "system" => OwnedHistoryInput::System {
                    text: take_string(&payload, "system", "text")?,
                },
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
                other => return Err(HistoryError::UnknownPrimitiveType(other.to_string())),
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
) -> Result<String, HistoryError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(HistoryError::PrimitiveMissingField { kind, field: key })
}

#[derive(Debug, thiserror::Error)]
#[error("expected text in {column}, got {found}")]
struct NonText {
    column: &'static str,
    found: String,
}
