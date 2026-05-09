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

    pub async fn append_primitive_user(&self, text: &str) -> Result<RowId> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive("user", &primitive).await
    }

    pub async fn append_primitive_assistant(&self, text: &str) -> Result<RowId> {
        let primitive = serde_json::json!({ "text": text });
        self.append_primitive("assistant", &primitive).await
    }

    pub async fn append_primitive_tool_call(
        &self,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> Result<RowId> {
        let primitive = serde_json::json!({
            "id": id,
            "name": name,
            "arguments": arguments,
        });
        self.append_primitive("tool_call", &primitive).await
    }

    pub async fn append_primitive_tool_result(
        &self,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<RowId> {
        let primitive = serde_json::json!({
            "call_id": call_id,
            "content": content,
            "is_error": is_error,
        });
        self.append_primitive("tool_result", &primitive).await
    }

    /// Bulk-insert wire-tagged history rows. Each `payload` becomes one row
    /// with the supplied `(kind, provider_id)` tag; rows take consecutive
    /// auto-incremented seq values.
    pub async fn append_history(
        &self,
        kind: &str,
        provider_id: &str,
        payloads: &[Value],
    ) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }

        trace!(
            kind = kind,
            provider_id = provider_id,
            count = payloads.len(),
            "appending history rows"
        );

        let conn = self.db.connect();
        for payload in payloads {
            let seq = next_seq(&conn).await?;
            let payload_text = serde_json::to_string(payload).context("encode history payload")?;
            conn.execute(
                "INSERT INTO rows (seq, type, history, kind, provider_id) \
                 VALUES (?1, 'history', jsonb(?2), ?3, ?4)",
                (seq.0, payload_text, kind, provider_id),
            )
            .await
            .context("insert history row")?;
        }

        Ok(())
    }

    /// Wire JSON to send to the LLM, in seq order.
    pub async fn loaded_history(&self) -> Result<Vec<Value>> {
        trace!("loading history payloads");

        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "SELECT json(history) FROM rows WHERE history IS NOT NULL ORDER BY seq",
                (),
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

    /// Translate non-history rows into `Block`s for the TUI replay path.
    /// Currently unused; resume flow is future work.
    #[expect(
        dead_code,
        reason = "history-replay API; not yet wired into session resume"
    )]
    pub async fn replay_for_tui(&self) -> Result<Vec<Block>> {
        let primitives = self.load_primitives().await?;
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

    /// Drop the wire-tagged history rows and re-forge from primitives under
    /// the supplied provider's wire shape. Currently unused — swap detection
    /// is future work.
    #[expect(
        dead_code,
        reason = "swap-time re-forge; provider-change detection is future work"
    )]
    pub async fn purge_and_reforge<P: Provider + 'static>(
        &self,
        provider: &P,
        provider_id: &str,
    ) -> Result<()> {
        let conn = self.db.connect();
        conn.execute("DELETE FROM rows WHERE type = 'history'", ())
            .await
            .context("clear history rows")?;

        let primitives = self.load_primitives().await?;
        let inputs: Vec<HistoryInput<'_>> = primitives
            .iter()
            .map(OwnedHistoryInput::as_borrowed)
            .collect();
        let payloads = provider.forge_history(&inputs);
        self.append_history(provider.kind(), provider_id, &payloads)
            .await
    }

    async fn append_primitive(&self, ty: &str, primitive: &Value) -> Result<RowId> {
        let conn = self.db.connect();
        let seq = next_seq(&conn).await?;
        let payload_text = serde_json::to_string(primitive).context("encode primitive")?;
        conn.execute(
            "INSERT INTO rows (seq, type, primitive) VALUES (?1, ?2, jsonb(?3))",
            (seq.0, ty, payload_text),
        )
        .await
        .with_context(|| format!("insert {ty} primitive"))?;
        last_insert_rowid(&conn).await
    }

    async fn load_primitives(&self) -> Result<Vec<OwnedHistoryInput>> {
        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "SELECT type, json(primitive) FROM rows \
                 WHERE primitive IS NOT NULL ORDER BY seq",
                (),
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

async fn next_seq(conn: &turso::Connection) -> Result<RowSeq> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(seq), -1) + 1 FROM rows", ())
        .await
        .context("query next seq")?;
    let row = rows
        .next()
        .await
        .context("read next seq row")?
        .ok_or_else(|| anyhow!("next seq query returned no rows"))?;
    Ok(RowSeq(row.get::<i64>(0).context("decode next seq")?))
}

async fn last_insert_rowid(conn: &turso::Connection) -> Result<RowId> {
    let mut rows = conn
        .query("SELECT last_insert_rowid()", ())
        .await
        .context("query last_insert_rowid")?;
    let row = rows
        .next()
        .await
        .context("read last_insert_rowid row")?
        .ok_or_else(|| anyhow!("last_insert_rowid query returned no rows"))?;
    Ok(RowId(
        row.get::<i64>(0).context("decode last_insert_rowid")?,
    ))
}
