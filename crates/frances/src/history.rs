use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::trace;

use crate::store::Store;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockType {
    Text,
    Thinking,
    Image,
    ToolUse,
    ToolResult,
}

impl BlockType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::Image => "image",
            Self::ToolUse => "tool_use",
            Self::ToolResult => "tool_result",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "text" => Ok(Self::Text),
            "thinking" => Ok(Self::Thinking),
            "image" => Ok(Self::Image),
            "tool_use" => Ok(Self::ToolUse),
            "tool_result" => Ok(Self::ToolResult),
            _ => Err(anyhow!("unknown block type {value:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub kind: BlockType,
    pub text: String,
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub seq: i64,
    pub role: Role,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    store: Store,
}

impl HistoryStore {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub async fn append(&self, role: Role, blocks: Vec<Block>) -> Result<Message> {
        trace!(
            role = role.as_str(),
            blocks = blocks.len(),
            "appending history message"
        );

        let conn = self.store.connect()?;
        let seq = next_seq(&conn).await?;

        conn.execute(
            "INSERT INTO messages (seq, role) VALUES (?1, ?2)",
            (seq, role.as_str()),
        )
        .await
        .context("insert message")?;

        let message_id = last_insert_rowid(&conn).await?;

        for (index, block) in blocks.iter().enumerate() {
            conn.execute(
                "INSERT INTO blocks (message_id, seq, type, text, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    message_id,
                    i64::try_from(index).context("block index overflow")?,
                    block.kind.as_str(),
                    block.text.as_str(),
                    block.data.clone(),
                ),
            )
            .await
            .with_context(|| format!("insert block {index}"))?;
        }

        Ok(Message {
            id: message_id,
            seq,
            role,
            blocks,
        })
    }

    pub async fn messages(&self) -> Result<Vec<Message>> {
        trace!("loading history messages");

        let conn = self.store.connect()?;
        let mut rows = conn
            .query(
                "
                SELECT m.id, m.seq, m.role, b.type, b.text, b.data
                FROM messages m
                LEFT JOIN blocks b ON b.message_id = m.id
                ORDER BY m.seq, b.seq
                ",
                (),
            )
            .await
            .context("query messages")?;

        let mut messages = Vec::new();

        while let Some(row) = rows.next().await.context("iterate messages")? {
            let message_id = row.get::<i64>(0).context("message id")?;
            let message_seq = row.get::<i64>(1).context("message seq")?;
            let message_role = Role::parse(&row.get::<String>(2).context("message role")?)?;

            let needs_new = messages
                .last()
                .map(|message: &Message| message.id != message_id)
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

            let kind_value = row.get_value(3).context("block type")?;
            if let turso::Value::Null = kind_value {
                continue;
            }

            let kind = match kind_value {
                turso::Value::Text(value) => BlockType::parse(&value)?,
                other => return Err(anyhow!("unexpected block type value: {other:?}")),
            };

            let text = match row.get_value(4).context("block text")? {
                turso::Value::Text(value) => value,
                turso::Value::Null => String::new(),
                other => return Err(anyhow!("unexpected block text value: {other:?}")),
            };

            let data = match row.get_value(5).context("block data")? {
                turso::Value::Null => None,
                turso::Value::Blob(bytes) => Some(bytes),
                other => return Err(anyhow!("unexpected block data value: {other:?}")),
            };

            message.blocks.push(Block { kind, text, data });
        }

        Ok(messages)
    }

    pub async fn clear(&self) -> Result<()> {
        trace!("clearing history tables");

        let conn = self.store.connect()?;
        conn.execute("DELETE FROM blocks", ())
            .await
            .context("clear blocks")?;
        conn.execute("DELETE FROM messages", ())
            .await
            .context("clear messages")?;
        Ok(())
    }
}

async fn next_seq(conn: &turso::Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT COALESCE(MAX(seq), -1) + 1 FROM messages", ())
        .await
        .context("query next seq")?;
    let row = rows
        .next()
        .await
        .context("read next seq row")?
        .ok_or_else(|| anyhow!("next seq query returned no rows"))?;
    row.get::<i64>(0).context("decode next seq")
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
