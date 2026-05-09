use anyhow::{Context, Result, anyhow};
use frances_llm::{HistoryInput, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::trace;
use uuid::Uuid;

use crate::migrations::{EntitySchema, Migration};
use crate::store::Database;

/// Owns the conversation history. UUID is permanent — never edit.
pub static SCHEMA: EntitySchema = EntitySchema {
    entity: Uuid::from_u128(0x7ffee42d_48de_4090_8fc6_a25e66f33a02),
    migrations: &[Migration {
        name: "0001_init.sql",
        sql: include_str!("history/migrations/0001_init.sql"),
    }],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RowId(pub i64);

impl std::fmt::Display for RowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RowSeq(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatSessionId(pub i64);

impl std::fmt::Display for ChatSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Row-shape of a `chat_sessions` entry, returned from `load_chat_session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSessionRow {
    pub id: ChatSessionId,
    /// Opaque UUID; threaded through `ProviderRequest::session_id`.
    pub session_id: String,
    /// Ordered list of `models::<intent>` config keys the session walks
    /// when resolving a model for the next call.
    pub model_intents: Vec<String>,
}

/// A primitive row read back from storage; mirrors [`HistoryInput`] but
/// owns its strings so it can outlive the SQL row buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedHistoryInput {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl OwnedHistoryInput {
    pub fn as_borrowed(&self) -> HistoryInput<'_> {
        match self {
            Self::User { text } => HistoryInput::User { text },
            Self::Assistant { text } => HistoryInput::Assistant { text },
            Self::ToolCall {
                id,
                name,
                arguments,
            } => HistoryInput::ToolCall {
                id,
                name,
                arguments,
            },
            Self::ToolResult {
                call_id,
                content,
                is_error,
            } => HistoryInput::ToolResult {
                call_id,
                content,
                is_error: *is_error,
            },
        }
    }
}

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

#[derive(Debug, Clone)]
pub struct HistoryStore {
    db: Database,
}

impl HistoryStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    // -------------------------------------------------------------------
    // chat_sessions lifecycle
    // -------------------------------------------------------------------

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
        let intents_json = serde_json::to_string(model_intents).context("encode model_intents")?;
        conn.execute(
            "INSERT INTO chat_sessions (session_id, model_intents, created_at) \
             VALUES (?1, jsonb(?2), ?3)",
            (session_id, intents_json, created_at),
        )
        .await
        .context("insert chat_session")?;
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
            .context("query chat_session")?;
        let row = rows
            .next()
            .await
            .context("read chat_session row")?
            .ok_or_else(|| anyhow!("chat_session {id} not found"))?;
        let session_id: String = row.get(0).context("chat_session.session_id")?;
        let intents_text: String = row.get(1).context("chat_session.model_intents")?;
        let model_intents: Vec<String> =
            serde_json::from_str(&intents_text).context("decode model_intents")?;
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
            .context("query primary_chat_session")?;
        match rows.next().await.context("read primary_chat_session row")? {
            Some(row) => Ok(Some(ChatSessionId(
                row.get::<i64>(0)
                    .context("primary_chat_session.chat_session_id")?,
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
        .context("insert primary_chat_session")?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // chat_messages: primitive inserts
    // -------------------------------------------------------------------

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
            let payload_text = serde_json::to_string(payload).context("encode history payload")?;
            conn.execute(
                "INSERT INTO chat_messages (chat_session_id, seq, type, history, kind, provider_id) \
                 VALUES (?1, ?2, 'history', jsonb(?3), ?4, ?5)",
                (session.0, seq.0, payload_text, kind, provider_id),
            )
            .await
            .context("insert history row")?;
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
            .context("query history payloads")?;

        let mut payloads = Vec::new();
        while let Some(row) = rows.next().await.context("iterate history payloads")? {
            let text = match row.get_value(0).context("history column")? {
                turso::Value::Text(value) => value,
                other => return Err(anyhow!("unexpected history value: {other:?}")),
            };
            let value: Value = serde_json::from_str(&text).context("decode history payload")?;
            payloads.push(value);
        }

        Ok(payloads)
    }

    /// Translate non-history rows for the (currently unwired) TUI replay path.
    #[expect(
        dead_code,
        reason = "history-replay API; not yet wired into session resume"
    )]
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
    #[expect(
        dead_code,
        reason = "swap-time re-forge; provider-change detection is future work"
    )]
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
        .context("clear history rows")?;

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
        let payload_text = serde_json::to_string(primitive).context("encode primitive")?;
        conn.execute(
            "INSERT INTO chat_messages (chat_session_id, seq, type, primitive) \
             VALUES (?1, ?2, ?3, jsonb(?4))",
            (session.0, seq.0, ty, payload_text),
        )
        .await
        .with_context(|| format!("insert {ty} primitive"))?;
        last_insert_rowid(&conn).await.map(RowId)
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
            .context("query primitive rows")?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.context("iterate primitive rows")? {
            let ty = row.get::<String>(0).context("primitive type")?;
            let payload_text = match row.get_value(1).context("primitive payload")? {
                turso::Value::Text(value) => value,
                other => return Err(anyhow!("unexpected primitive value: {other:?}")),
            };
            let payload: Value =
                serde_json::from_str(&payload_text).context("decode primitive payload")?;

            let owned = match ty.as_str() {
                "user" => OwnedHistoryInput::User {
                    text: take_string(&payload, "text")?,
                },
                "assistant" => OwnedHistoryInput::Assistant {
                    text: take_string(&payload, "text")?,
                },
                "tool_call" => OwnedHistoryInput::ToolCall {
                    id: take_string(&payload, "id")?,
                    name: take_string(&payload, "name")?,
                    arguments: payload
                        .get("arguments")
                        .cloned()
                        .ok_or_else(|| anyhow!("tool_call primitive missing 'arguments'"))?,
                },
                "tool_result" => OwnedHistoryInput::ToolResult {
                    call_id: take_string(&payload, "call_id")?,
                    content: take_string(&payload, "content")?,
                    is_error: payload
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
                other => return Err(anyhow!("unexpected primitive type {other:?}")),
            };
            out.push(owned);
        }

        Ok(out)
    }
}

fn take_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("primitive missing string field {key:?}"))
}

async fn next_seq(conn: &turso::Connection, session: ChatSessionId) -> Result<RowSeq> {
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM chat_messages WHERE chat_session_id = ?1",
            (session.0,),
        )
        .await
        .context("query next seq")?;
    let row = rows
        .next()
        .await
        .context("read next seq row")?
        .ok_or_else(|| anyhow!("next seq query returned no rows"))?;
    Ok(RowSeq(row.get::<i64>(0).context("decode next seq")?))
}

async fn last_insert_rowid(conn: &turso::Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT last_insert_rowid()", ())
        .await
        .context("query last_insert_rowid")?;
    let row = rows
        .next()
        .await
        .context("read last_insert_rowid row")?
        .ok_or_else(|| anyhow!("last_insert_rowid query returned no rows"))?;
    row.get::<i64>(0).context("decode last_insert_rowid")
}
