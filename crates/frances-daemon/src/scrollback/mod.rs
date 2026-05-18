//! Per-workflow scrollback persistence.
//!
//! Two write paths feed the `scrollback_blocks` table; one read path
//! feeds the TUI replay.
//!
//! ## Writes
//!
//! - **Clean close** (`BlockStop` on the wire): [`persist_block`] with
//!   `truncated = false`. Called from the [`EmitState`]'s normal close
//!   path inside `workflows::emit`.
//! - **Dehydrate-interrupted close**: same call with `truncated = true`.
//!   Triggered when a workflow is pushed off the top with an open
//!   in-flight block — the daemon never gets to emit `BlockStop`, but
//!   the row goes in marked truncated so the replay can surface that
//!   to the user.
//! - **Error frames**: [`persist_error`] writes a row with
//!   `kind = 'error'` whenever the daemon emits a
//!   [`StreamFrame::Error`]. `truncated` is ignored for these.
//!
//! ## Reads
//!
//! [`replay_to_stream`] queries every row for the given workflow
//! instance in `id` order and emits a synthetic frame burst on the
//! supplied unix stream:
//!
//! 1. [`StreamFrame::ScrollbackReset`] — TUI clears its in-memory
//!    scrollback and enters replay mode.
//! 2. For each row: a single self-describing `BlockDelta { id, kind,
//!    text }` (the first delta with an unseen id implicitly opens the
//!    block) followed by `BlockStop` or `BlockTruncated`. Error rows
//!    emit a single `Error` frame.
//! 3. [`StreamFrame::ScrollbackReplayEnd`] — TUI returns to live mode.
//!
//! The replay uses its own block-id allocator (independent of
//! `EmitState`'s) — collisions across the boundary are harmless because
//! each replayed block opens and closes within the replay before the
//! next one starts, so the TUI's `BlockState` is `Idle` at
//! `ScrollbackReplayEnd`. And committed blocks have no ids at all
//! (`crates/frances-tui/src/scrollback_container.rs`'s `committed`
//! field stores bare trait objects), so live frames after the replay
//! collide with nothing in scrollback either.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::net::UnixStream;
use turso::Connection;
use uuid::Uuid;

use frances_storage::{EntitySchema, Migration};

use crate::Result;
use crate::protocol::{BlockId, BlockKind, StreamFrame};
use crate::transport::write_message;

/// Owns the per-session `scrollback_blocks` table. UUID is permanent;
/// never edit.
pub static SCHEMA: EntitySchema<'static> = EntitySchema {
    entity: Uuid::from_u128(0x2c8a7e91_4f0b_4d6a_b8d2_1a3e9f6c2b71),
    migrations: Cow::Borrowed(&[Migration {
        name: Cow::Borrowed("0001_init.sql"),
        sql: Cow::Borrowed(include_str!("migrations/0001_init.sql")),
    }]),
};

#[derive(Debug, Error)]
pub enum ScrollbackError {
    #[error("scrollback sql: {0}")]
    Turso(#[from] turso::Error),
    #[error("scrollback transport: {0}")]
    Transport(#[from] crate::transport::TransportError),
    #[error("scrollback payload encode: {0}")]
    Encode(serde_json::Error),
    #[error("scrollback payload decode for kind {kind:?}: {source}")]
    Decode {
        kind: String,
        source: serde_json::Error,
    },
    #[error("scrollback row: unexpected column shape for {column} (expected {expected})")]
    UnexpectedColumn {
        column: &'static str,
        expected: &'static str,
    },
    #[error("scrollback row: unknown kind {0:?}")]
    UnknownKind(String),
}

/// Decoded row, ready for replay synthesis.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredRow {
    Block {
        kind: BlockKind,
        text: String,
        truncated: bool,
    },
    Error {
        text: String,
    },
}

/// On-disk JSON shape for `kind` = 'text' rows. `sender` mirrors the
/// wire-level `BlockKind::Text { sender: Option<Arc<str>> }`; `Arc<str>`
/// serializes through serde the same way `String` does, so the on-disk
/// JSON shape is identical to a `Option<String>` / `String` schema.
#[derive(Serialize, Deserialize)]
struct TextPayload {
    sender: Option<Arc<str>>,
    text: String,
}

/// On-disk JSON shape for `kind` = 'tool_use' rows.
#[derive(Serialize, Deserialize)]
struct ToolUsePayload {
    name: Arc<str>,
    text: String,
}

/// On-disk JSON shape for `kind` = 'tool_result' rows.
#[derive(Serialize, Deserialize)]
struct ToolResultPayload {
    tool_use_id: Arc<str>,
    is_error: bool,
    text: String,
}

/// On-disk JSON shape for `kind` = 'error' rows.
#[derive(Serialize, Deserialize)]
struct ErrorPayload {
    text: String,
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Insert a finished or truncated block row. `truncated = false`
/// corresponds to a clean `BlockStop`; `truncated = true` corresponds
/// to a workflow dehydrated mid-stream.
pub async fn persist_block(
    conn: &Connection,
    instance: Uuid,
    kind: &BlockKind,
    text: &str,
    truncated: bool,
) -> std::result::Result<(), ScrollbackError> {
    let (kind_text, payload_json) = encode_block(kind, text)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    let truncated_i = if truncated { 1 } else { 0 };
    conn.execute(
        "INSERT INTO scrollback_blocks (instance_id, kind, payload, truncated, created_at) \
         VALUES (?1, ?2, jsonb(?3), ?4, ?5)",
        (instance_bytes, kind_text, payload_json, truncated_i, now),
    )
    .await?;
    Ok(())
}

/// Insert an error row. The replay path emits a single
/// [`StreamFrame::Error`] for each such row.
pub async fn persist_error(
    conn: &Connection,
    instance: Uuid,
    text: &str,
) -> std::result::Result<(), ScrollbackError> {
    let payload = ErrorPayload {
        text: text.to_owned(),
    };
    let payload_json = serde_json::to_string(&payload).map_err(ScrollbackError::Encode)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    conn.execute(
        "INSERT INTO scrollback_blocks (instance_id, kind, payload, truncated, created_at) \
         VALUES (?1, 'error', jsonb(?2), 0, ?3)",
        (instance_bytes, payload_json, now),
    )
    .await?;
    Ok(())
}

/// Load every row for an instance in insertion order. The list maps 1:1
/// onto the replay frame burst.
pub async fn load_for_instance(
    conn: &Connection,
    instance: Uuid,
) -> std::result::Result<Vec<StoredRow>, ScrollbackError> {
    let mut rows = conn
        .query(
            "SELECT kind, json(payload), truncated FROM scrollback_blocks \
             WHERE instance_id = ?1 ORDER BY id ASC",
            (instance.as_bytes().to_vec(),),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let kind = match row.get_value(0)? {
            turso::Value::Text(t) => t,
            _ => {
                return Err(ScrollbackError::UnexpectedColumn {
                    column: "kind",
                    expected: "text",
                });
            }
        };
        let payload_text = match row.get_value(1)? {
            turso::Value::Text(t) => t,
            _ => {
                return Err(ScrollbackError::UnexpectedColumn {
                    column: "payload",
                    expected: "text (json)",
                });
            }
        };
        let truncated = matches!(row.get_value(2)?, turso::Value::Integer(n) if n != 0);
        out.push(decode_row(&kind, &payload_text, truncated)?);
    }
    Ok(out)
}

/// Replay every stored row for `instance` onto `stream`, bracketed by
/// [`StreamFrame::ScrollbackReset`] and
/// [`StreamFrame::ScrollbackReplayEnd`].
pub async fn replay_to_stream(
    stream: &mut UnixStream,
    conn: &Connection,
    instance: Uuid,
) -> Result<()> {
    let rows = load_for_instance(conn, instance)
        .await
        .map_err(crate::Error::Scrollback)?;

    write_message(
        stream,
        &StreamFrame::ScrollbackReset {
            instance_id: instance,
        },
    )
    .await?;

    // Local id allocator. Replayed block ids are independent of
    // `EmitState::next_block` — each replayed block opens and closes
    // inside the replay window before the next one starts, so the
    // TUI's `BlockState` is `Idle` at `ScrollbackReplayEnd` and live
    // frames after replay collide with nothing.
    let mut next_id: u64 = 1;
    for row in rows {
        match row {
            StoredRow::Block {
                kind,
                text,
                truncated,
            } => {
                let id = BlockId(next_id);
                next_id += 1;
                // The first (and only) delta carries the kind — the
                // TUI opens the block on the first delta for an unseen
                // id. We always emit one delta even when `text` is
                // empty so the TUI sees the implicit "open".
                write_message(stream, &StreamFrame::BlockDelta { id, kind, text }).await?;
                if truncated {
                    write_message(stream, &StreamFrame::BlockTruncated { id }).await?;
                } else {
                    write_message(stream, &StreamFrame::BlockStop { id }).await?;
                }
            }
            StoredRow::Error { text } => {
                write_message(stream, &StreamFrame::Error(text)).await?;
            }
        }
    }

    write_message(stream, &StreamFrame::ScrollbackReplayEnd).await?;
    Ok(())
}

/// In-process equivalent of [`replay_to_stream`] — produces the same
/// frame sequence as a `Vec<StreamFrame>` so it can be bundled into
/// an `AttachResponse`. The order matches the wire path exactly.
pub async fn replay_frames(
    conn: &Connection,
    instance: Uuid,
) -> std::result::Result<Vec<StreamFrame>, ScrollbackError> {
    let rows = load_for_instance(conn, instance).await?;
    let mut out = Vec::with_capacity(rows.len() * 2 + 2);
    out.push(StreamFrame::ScrollbackReset {
        instance_id: instance,
    });
    let mut next_id: u64 = 1;
    for row in rows {
        match row {
            StoredRow::Block {
                kind,
                text,
                truncated,
            } => {
                let id = BlockId(next_id);
                next_id += 1;
                out.push(StreamFrame::BlockDelta { id, kind, text });
                if truncated {
                    out.push(StreamFrame::BlockTruncated { id });
                } else {
                    out.push(StreamFrame::BlockStop { id });
                }
            }
            StoredRow::Error { text } => {
                out.push(StreamFrame::Error(text));
            }
        }
    }
    out.push(StreamFrame::ScrollbackReplayEnd);
    Ok(out)
}

fn encode_block(
    kind: &BlockKind,
    text: &str,
) -> std::result::Result<(&'static str, String), ScrollbackError> {
    let (kind_text, payload): (&'static str, Value) = match kind {
        BlockKind::Text { sender } => (
            "text",
            serde_json::to_value(TextPayload {
                sender: sender.clone(),
                text: text.to_owned(),
            })
            .map_err(ScrollbackError::Encode)?,
        ),
        BlockKind::ToolUse { name } => (
            "tool_use",
            serde_json::to_value(ToolUsePayload {
                name: name.clone(),
                text: text.to_owned(),
            })
            .map_err(ScrollbackError::Encode)?,
        ),
        BlockKind::ToolResult {
            tool_use_id,
            is_error,
        } => (
            "tool_result",
            serde_json::to_value(ToolResultPayload {
                tool_use_id: tool_use_id.clone(),
                is_error: *is_error,
                text: text.to_owned(),
            })
            .map_err(ScrollbackError::Encode)?,
        ),
    };
    let payload_json = serde_json::to_string(&payload).map_err(ScrollbackError::Encode)?;
    Ok((kind_text, payload_json))
}

fn decode_row(
    kind: &str,
    payload_text: &str,
    truncated: bool,
) -> std::result::Result<StoredRow, ScrollbackError> {
    match kind {
        "text" => {
            let p: TextPayload =
                serde_json::from_str(payload_text).map_err(|source| ScrollbackError::Decode {
                    kind: kind.to_owned(),
                    source,
                })?;
            Ok(StoredRow::Block {
                kind: BlockKind::Text { sender: p.sender },
                text: p.text,
                truncated,
            })
        }
        "tool_use" => {
            let p: ToolUsePayload =
                serde_json::from_str(payload_text).map_err(|source| ScrollbackError::Decode {
                    kind: kind.to_owned(),
                    source,
                })?;
            Ok(StoredRow::Block {
                kind: BlockKind::ToolUse { name: p.name },
                text: p.text,
                truncated,
            })
        }
        "tool_result" => {
            let p: ToolResultPayload =
                serde_json::from_str(payload_text).map_err(|source| ScrollbackError::Decode {
                    kind: kind.to_owned(),
                    source,
                })?;
            Ok(StoredRow::Block {
                kind: BlockKind::ToolResult {
                    tool_use_id: p.tool_use_id,
                    is_error: p.is_error,
                },
                text: p.text,
                truncated,
            })
        }
        "error" => {
            let p: ErrorPayload =
                serde_json::from_str(payload_text).map_err(|source| ScrollbackError::Decode {
                    kind: kind.to_owned(),
                    source,
                })?;
            Ok(StoredRow::Error { text: p.text })
        }
        other => Err(ScrollbackError::UnknownKind(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_storage::run_all;
    use turso::Builder;

    async fn fresh_conn() -> Connection {
        let conn = Builder::new_local(":memory:")
            .build()
            .await
            .unwrap()
            .connect()
            .unwrap();
        run_all(&conn, &[&SCHEMA]).await.unwrap();
        conn
    }

    #[tokio::test]
    async fn empty_instance_loads_nothing() {
        let conn = fresh_conn().await;
        let rows = load_for_instance(&conn, Uuid::new_v4()).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn block_round_trips() {
        let conn = fresh_conn().await;
        let instance = Uuid::new_v4();
        persist_block(
            &conn,
            instance,
            &BlockKind::Text {
                sender: Some("user".into()),
            },
            "hello world",
            false,
        )
        .await
        .unwrap();
        let rows = load_for_instance(&conn, instance).await.unwrap();
        assert_eq!(
            rows,
            vec![StoredRow::Block {
                kind: BlockKind::Text {
                    sender: Some("user".into())
                },
                text: "hello world".into(),
                truncated: false,
            }]
        );
    }

    #[tokio::test]
    async fn truncated_block_round_trips() {
        let conn = fresh_conn().await;
        let instance = Uuid::new_v4();
        persist_block(
            &conn,
            instance,
            &BlockKind::ToolUse {
                name: "shell".into(),
            },
            "ls /",
            true,
        )
        .await
        .unwrap();
        let rows = load_for_instance(&conn, instance).await.unwrap();
        assert_eq!(
            rows,
            vec![StoredRow::Block {
                kind: BlockKind::ToolUse {
                    name: "shell".into()
                },
                text: "ls /".into(),
                truncated: true,
            }]
        );
    }

    #[tokio::test]
    async fn error_round_trips() {
        let conn = fresh_conn().await;
        let instance = Uuid::new_v4();
        persist_error(&conn, instance, "boom").await.unwrap();
        let rows = load_for_instance(&conn, instance).await.unwrap();
        assert_eq!(
            rows,
            vec![StoredRow::Error {
                text: "boom".into()
            }]
        );
    }

    #[tokio::test]
    async fn rows_are_scoped_per_instance() {
        let conn = fresh_conn().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        persist_block(&conn, a, &BlockKind::Text { sender: None }, "a", false)
            .await
            .unwrap();
        persist_block(&conn, b, &BlockKind::Text { sender: None }, "b", false)
            .await
            .unwrap();
        let rows_a = load_for_instance(&conn, a).await.unwrap();
        let rows_b = load_for_instance(&conn, b).await.unwrap();
        assert_eq!(rows_a.len(), 1);
        assert_eq!(rows_b.len(), 1);
        match &rows_a[0] {
            StoredRow::Block { text, .. } => assert_eq!(text, "a"),
            other => panic!("unexpected {other:?}"),
        }
        match &rows_b[0] {
            StoredRow::Block { text, .. } => assert_eq!(text, "b"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_frames_synthesizes_expected_burst() {
        let conn = fresh_conn().await;
        let instance = Uuid::new_v4();
        persist_block(
            &conn,
            instance,
            &BlockKind::Text {
                sender: Some("user".into()),
            },
            "hi",
            false,
        )
        .await
        .unwrap();
        persist_error(&conn, instance, "boom").await.unwrap();
        persist_block(
            &conn,
            instance,
            &BlockKind::ToolUse {
                name: "shell".into(),
            },
            "ls",
            true,
        )
        .await
        .unwrap();

        let frames = replay_frames(&conn, instance).await.unwrap();
        // Reset + (Delta+Stop) + Error + (Delta+Truncated) + End
        assert_eq!(frames.len(), 1 + 2 + 1 + 2 + 1);
        assert!(matches!(
            frames.first(),
            Some(StreamFrame::ScrollbackReset { .. })
        ));
        assert!(matches!(
            frames.last(),
            Some(StreamFrame::ScrollbackReplayEnd)
        ));
        // Each block row produces exactly one self-describing delta
        // (kind + full text in one frame) followed by stop / truncated.
        assert!(matches!(
            frames.get(1),
            Some(StreamFrame::BlockDelta {
                kind: BlockKind::Text { sender: Some(_) },
                ..
            }),
        ));
        assert!(matches!(frames.get(2), Some(StreamFrame::BlockStop { .. })));
        assert!(matches!(frames.get(3), Some(StreamFrame::Error(_))));
        assert!(matches!(
            frames.get(4),
            Some(StreamFrame::BlockDelta {
                kind: BlockKind::ToolUse { .. },
                ..
            }),
        ));
        assert!(matches!(
            frames.get(5),
            Some(StreamFrame::BlockTruncated { .. }),
        ));
    }

    /// A single stored block produces exactly:
    /// `[ScrollbackReset, BlockDelta { kind, text }, BlockStop, ScrollbackReplayEnd]`.
    /// No extra frames slip in (no `BlockStart`).
    #[tokio::test]
    async fn replay_frames_for_single_block_is_minimal() {
        let conn = fresh_conn().await;
        let instance = Uuid::new_v4();
        persist_block(
            &conn,
            instance,
            &BlockKind::ToolUse {
                name: "shell".into(),
            },
            "ls /",
            false,
        )
        .await
        .unwrap();

        let frames = replay_frames(&conn, instance).await.unwrap();
        assert_eq!(frames.len(), 4);
        match &frames[0] {
            StreamFrame::ScrollbackReset { .. } => {}
            other => panic!("expected ScrollbackReset at [0], got {other:?}"),
        }
        match &frames[1] {
            StreamFrame::BlockDelta {
                kind: BlockKind::ToolUse { name },
                text,
                ..
            } => {
                assert_eq!(&**name, "shell");
                assert_eq!(text, "ls /");
            }
            other => panic!("expected BlockDelta with ToolUse kind at [1], got {other:?}"),
        }
        match &frames[2] {
            StreamFrame::BlockStop { .. } => {}
            other => panic!("expected BlockStop at [2], got {other:?}"),
        }
        match &frames[3] {
            StreamFrame::ScrollbackReplayEnd => {}
            other => panic!("expected ScrollbackReplayEnd at [3], got {other:?}"),
        }
    }
}
