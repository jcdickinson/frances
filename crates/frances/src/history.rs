use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::trace;

use crate::store::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub i64);

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageSeq(pub i64);

impl std::fmt::Display for MessageSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            _ => Err(anyhow!("unknown role {value:?}")),
        }
    }
}

/// Discriminated union of block payloads. The variant *is* the schema —
/// you can't construct a `Text` block with image data, or a `ToolUse` with
/// no name. Persisted as a single JSONB column (`blocks.payload`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    Image {
        description: String,
        data: serde_json::Value,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub seq: MessageSeq,
    pub role: Role,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    db: Database,
}

impl HistoryStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn append(
        &self,
        role: Role,
        blocks: Vec<Block>,
        openai_payload: serde_json::Value,
    ) -> Result<Message> {
        trace!(
            role = role.as_str(),
            blocks = blocks.len(),
            "appending history message"
        );

        let conn = self.db.connect();
        let seq = next_seq(&conn).await?;

        conn.execute(
            "INSERT INTO messages (seq, role) VALUES (?1, ?2)",
            (seq.0, role.as_str()),
        )
        .await
        .context("insert message")?;

        let message_id = last_insert_rowid(&conn).await?;

        for (index, block) in blocks.iter().enumerate() {
            let payload = serde_json::to_string(block).context("encode block")?;
            conn.execute(
                "INSERT INTO blocks (message_id, seq, payload) VALUES (?1, ?2, jsonb(?3))",
                (
                    message_id.0,
                    i64::try_from(index).context("block index overflow")?,
                    payload,
                ),
            )
            .await
            .with_context(|| format!("insert block {index}"))?;
        }

        let payload_text = serde_json::to_string(&openai_payload).context("encode payload")?;
        conn.execute(
            "INSERT INTO openai_messages (message_id, payload) VALUES (?1, jsonb(?2))",
            (message_id.0, payload_text),
        )
        .await
        .context("insert openai payload")?;

        Ok(Message {
            id: message_id,
            seq,
            role,
            blocks,
        })
    }

    pub async fn start_assistant(&self) -> Result<MessageId> {
        trace!("starting empty assistant message");

        let conn = self.db.connect();
        let seq = next_seq(&conn).await?;

        conn.execute(
            "INSERT INTO messages (seq, role) VALUES (?1, ?2)",
            (seq.0, Role::Assistant.as_str()),
        )
        .await
        .context("insert assistant placeholder")?;

        last_insert_rowid(&conn).await
    }

    pub async fn finish_assistant(
        &self,
        message_id: MessageId,
        blocks: Vec<Block>,
        openai_payload: serde_json::Value,
    ) -> Result<()> {
        trace!(
            %message_id,
            blocks = blocks.len(),
            "finishing assistant message"
        );

        let conn = self.db.connect();

        for (index, block) in blocks.iter().enumerate() {
            let payload = serde_json::to_string(block).context("encode block")?;
            conn.execute(
                "INSERT INTO blocks (message_id, seq, payload) VALUES (?1, ?2, jsonb(?3))",
                (
                    message_id.0,
                    i64::try_from(index).context("block index overflow")?,
                    payload,
                ),
            )
            .await
            .with_context(|| format!("insert block {index}"))?;
        }

        let payload_text = serde_json::to_string(&openai_payload).context("encode payload")?;
        conn.execute(
            "INSERT INTO openai_messages (message_id, payload) VALUES (?1, jsonb(?2))",
            (message_id.0, payload_text),
        )
        .await
        .context("insert openai payload")?;

        Ok(())
    }

    pub async fn openai_payloads(&self) -> Result<Vec<serde_json::Value>> {
        trace!("loading openai payloads");

        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "
                SELECT json(om.payload)
                FROM messages m
                JOIN openai_messages om ON om.message_id = m.id
                ORDER BY m.seq
                ",
                (),
            )
            .await
            .context("query openai payloads")?;

        let mut payloads = Vec::new();
        while let Some(row) = rows.next().await.context("iterate openai payloads")? {
            let text = match row.get_value(0).context("payload column")? {
                turso::Value::Text(value) => value,
                other => return Err(anyhow!("unexpected payload value: {other:?}")),
            };
            let value: serde_json::Value = serde_json::from_str(&text).context("decode payload")?;
            payloads.push(value);
        }

        Ok(payloads)
    }

    pub async fn append_response_chunks(
        &self,
        message_id: MessageId,
        chunks: &[serde_json::Value],
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        trace!(
            %message_id,
            chunks = chunks.len(),
            "persisting response chunks"
        );

        let conn = self.db.connect();
        for (index, chunk) in chunks.iter().enumerate() {
            let chunk_text = serde_json::to_string(chunk).context("encode chunk")?;
            conn.execute(
                "INSERT INTO openai_response_chunks (message_id, seq, chunk) VALUES (?1, ?2, jsonb(?3))",
                (
                    message_id.0,
                    i64::try_from(index).context("chunk index overflow")?,
                    chunk_text,
                ),
            )
            .await
            .with_context(|| format!("insert response chunk {index}"))?;
        }

        Ok(())
    }

    pub async fn messages(&self) -> Result<Vec<Message>> {
        trace!("loading history messages");

        let conn = self.db.connect();
        let mut rows = conn
            .query(
                "
                SELECT m.id, m.seq, m.role, json(b.payload)
                FROM messages m
                LEFT JOIN blocks b ON b.message_id = m.id
                ORDER BY m.seq, b.seq
                ",
                (),
            )
            .await
            .context("query messages")?;

        let mut messages: Vec<Message> = Vec::new();

        while let Some(row) = rows.next().await.context("iterate messages")? {
            let message_id = MessageId(row.get::<i64>(0).context("message id")?);
            let message_seq = MessageSeq(row.get::<i64>(1).context("message seq")?);
            let message_role = Role::parse(&row.get::<String>(2).context("message role")?)?;

            let needs_new = messages
                .last()
                .map(|message| message.id != message_id)
                .unwrap_or(true);

            if needs_new {
                messages.push(Message {
                    id: message_id,
                    seq: message_seq,
                    role: message_role,
                    blocks: Vec::new(),
                });
            }

            let Some(message) = messages.last_mut() else {
                continue;
            };

            let payload_value = row.get_value(3).context("block payload")?;
            if let turso::Value::Null = payload_value {
                continue;
            }

            let payload_text = match payload_value {
                turso::Value::Text(value) => value,
                other => return Err(anyhow!("unexpected block payload value: {other:?}")),
            };
            let block: Block =
                serde_json::from_str(&payload_text).context("decode block payload")?;
            message.blocks.push(block);
        }

        Ok(messages)
    }

    pub async fn clear(&self) -> Result<()> {
        trace!("clearing history tables");

        let conn = self.db.connect();
        conn.execute("DELETE FROM blocks", ())
            .await
            .context("clear blocks")?;
        conn.execute("DELETE FROM messages", ())
            .await
            .context("clear messages")?;
        Ok(())
    }
}

async fn next_seq(conn: &turso::Connection) -> Result<MessageSeq> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(seq), -1) + 1 FROM messages", ())
        .await
        .context("query next seq")?;
    let row = rows
        .next()
        .await
        .context("read next seq row")?
        .ok_or_else(|| anyhow!("next seq query returned no rows"))?;
    Ok(MessageSeq(row.get::<i64>(0).context("decode next seq")?))
}

async fn last_insert_rowid(conn: &turso::Connection) -> Result<MessageId> {
    let mut rows = conn
        .query("SELECT last_insert_rowid()", ())
        .await
        .context("query last_insert_rowid")?;
    let row = rows
        .next()
        .await
        .context("read last_insert_rowid row")?
        .ok_or_else(|| anyhow!("last_insert_rowid query returned no rows"))?;
    Ok(MessageId(
        row.get::<i64>(0).context("decode last_insert_rowid")?,
    ))
}
