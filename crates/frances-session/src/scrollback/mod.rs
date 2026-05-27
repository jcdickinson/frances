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
//!   in-flight block — the runtime never gets to emit `BlockStop`, but
//!   the row goes in marked truncated so the replay can surface that
//!   to the user.
//! - **Error frames**: [`persist_error`] writes a row with
//!   `kind = 'error'` whenever the runtime emits a
//!   [`StreamFrame::Error`]. `truncated` is ignored for these.
//!
//! ## Reads
//!
//! [`replay_to_channel`] queries every row for the given workflow
//! instance in `id` order and emits a [`ScrollbackFrame`] burst (each
//! wrapped in [`StreamFrame::Scrollback`]) on the supplied channel:
//!
//! 1. [`ScrollbackFrame::Reset`] — TUI clears its in-memory scrollback
//!    and begins the burst.
//! 2. For each row: a single self-describing [`ScrollbackFrame::Block`]
//!    `{ id, kind, text }` (the first frame with an unseen id implicitly
//!    opens the block) followed by [`ScrollbackFrame::BlockStop`] or
//!    [`ScrollbackFrame::BlockTruncated`]. Error rows emit a single
//!    [`ScrollbackFrame::Error`].
//! 3. [`ScrollbackFrame::End`] — TUI returns to live mode.
//!
//! The replay uses its own block-id allocator (independent of
//! `EmitState`'s) — collisions across the boundary are harmless because
//! each replayed block opens and closes within the replay before the
//! next one starts, so the TUI's `BlockState` is `Idle` at
//! [`ScrollbackFrame::End`]. And committed blocks have no ids at all
//! (`crates/frances-tui/src/scrollback_container.rs`'s `committed`
//! field stores bare trait objects), so live frames after the replay
//! collide with nothing in scrollback either.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use frances_storage::{Database, EntitySchema, Migration};

use crate::Result;
use crate::events::{BlockId, BlockKind, ScrollbackFrame, StreamFrame};
use crate::runtime::EventsChannel;

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

/// On-disk JSON shape for `kind` = 'tool_use' rows. `detail` is the
/// optional human-readable suffix produced by the tool's
/// `describe(call)` method; it was added after the initial schema, so
/// older rows decode with `detail = None` via `#[serde(default)]`.
#[derive(Serialize, Deserialize)]
struct ToolUsePayload {
    name: Arc<str>,
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<Arc<str>>,
}

/// On-disk JSON shape for `kind` = 'shell_output' rows.
#[derive(Serialize, Deserialize)]
struct ShellOutputPayload {
    state: crate::events::ShellState,
    cmd: Arc<str>,
    text: String,
}

#[derive(Serialize, Deserialize)]
struct DiffPayload {
    lines: Vec<crate::events::DiffLine>,
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
    db: &Database,
    instance: Uuid,
    kind: &BlockKind,
    text: &str,
    truncated: bool,
) -> std::result::Result<(), ScrollbackError> {
    let (kind_text, payload_json) = encode_block(kind, text)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    let truncated_i = if truncated { 1 } else { 0 };
    let conn = db.connect().await;
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
    db: &Database,
    instance: Uuid,
    text: &str,
) -> std::result::Result<(), ScrollbackError> {
    let payload = ErrorPayload {
        text: text.to_owned(),
    };
    let payload_json = serde_json::to_string(&payload).map_err(ScrollbackError::Encode)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    let conn = db.connect().await;
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
    db: &Database,
    instance: Uuid,
) -> std::result::Result<Vec<StoredRow>, ScrollbackError> {
    let conn = db.connect().await;
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

/// Replay every stored row for `instance` into `events`, bracketed by
/// [`ScrollbackFrame::Reset`] and [`ScrollbackFrame::End`].
pub async fn replay_to_channel(
    events: &EventsChannel,
    db: &Database,
    instance: Uuid,
) -> Result<()> {
    let rows = load_for_instance(db, instance)
        .await
        .map_err(crate::Error::Scrollback)?;

    events.send(StreamFrame::Scrollback(ScrollbackFrame::Reset {
        instance_id: instance,
    }));

    // Local id allocator. Replayed block ids are independent of
    // `EmitState::next_block` — each replayed block opens and closes
    // inside the replay window before the next one starts, so the
    // TUI's `BlockState` is `Idle` at `ScrollbackFrame::End` and live
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
                // Stored rows always have a body (unmaterialised blocks
                // are never persisted), so we always send `text: Some(_)`
                // and the block materialises on the client.
                events.send(StreamFrame::Scrollback(ScrollbackFrame::Block {
                    id,
                    kind,
                    text: Some(text),
                }));
                if truncated {
                    events.send(StreamFrame::Scrollback(ScrollbackFrame::BlockTruncated {
                        id,
                    }));
                } else {
                    events.send(StreamFrame::Scrollback(ScrollbackFrame::BlockStop { id }));
                }
            }
            StoredRow::Error { text } => {
                events.send(StreamFrame::Scrollback(ScrollbackFrame::Error(text)));
            }
        }
    }

    events.send(StreamFrame::Scrollback(ScrollbackFrame::End));
    Ok(())
}

/// In-process equivalent of [`replay_to_channel`] — produces the same
/// frame sequence as a `Vec<StreamFrame>` so it can be bundled into
/// an `AttachResponse`. The order matches the wire path exactly.
pub async fn replay_frames(
    db: &Database,
    instance: Uuid,
) -> std::result::Result<Vec<StreamFrame>, ScrollbackError> {
    let rows = load_for_instance(db, instance).await?;
    let mut out = Vec::with_capacity(rows.len() * 2 + 2);
    out.push(StreamFrame::Scrollback(ScrollbackFrame::Reset {
        instance_id: instance,
    }));
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
                out.push(StreamFrame::Scrollback(ScrollbackFrame::Block {
                    id,
                    kind,
                    text: Some(text),
                }));
                if truncated {
                    out.push(StreamFrame::Scrollback(ScrollbackFrame::BlockTruncated {
                        id,
                    }));
                } else {
                    out.push(StreamFrame::Scrollback(ScrollbackFrame::BlockStop { id }));
                }
            }
            StoredRow::Error { text } => {
                out.push(StreamFrame::Scrollback(ScrollbackFrame::Error(text)));
            }
        }
    }
    out.push(StreamFrame::Scrollback(ScrollbackFrame::End));
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
        BlockKind::ToolUse { name, detail } => (
            "tool_use",
            serde_json::to_value(ToolUsePayload {
                name: name.clone(),
                text: text.to_owned(),
                detail: detail.clone(),
            })
            .map_err(ScrollbackError::Encode)?,
        ),
        BlockKind::ShellOutput { state, cmd } => (
            "shell_output",
            serde_json::to_value(ShellOutputPayload {
                state: state.clone(),
                cmd: cmd.clone(),
                text: text.to_owned(),
            })
            .map_err(ScrollbackError::Encode)?,
        ),
        BlockKind::Diff { lines } => (
            "diff",
            serde_json::to_value(DiffPayload {
                lines: lines.clone(),
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
                kind: BlockKind::ToolUse {
                    name: p.name,

                    detail: p.detail,
                },
                text: p.text,
                truncated,
            })
        }
        "shell_output" => {
            let p: ShellOutputPayload =
                serde_json::from_str(payload_text).map_err(|source| ScrollbackError::Decode {
                    kind: kind.to_owned(),
                    source,
                })?;
            Ok(StoredRow::Block {
                kind: BlockKind::ShellOutput {
                    state: p.state,
                    cmd: p.cmd,
                },
                text: p.text,
                truncated,
            })
        }
        "diff" => {
            let p: DiffPayload =
                serde_json::from_str(payload_text).map_err(|source| ScrollbackError::Decode {
                    kind: kind.to_owned(),
                    source,
                })?;
            Ok(StoredRow::Block {
                kind: BlockKind::Diff { lines: p.lines },
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

    async fn fresh_db() -> Database {
        let db = Database::open_in_memory().await.unwrap();
        {
            let conn = db.connect().await;
            run_all(&conn, &[&SCHEMA]).await.unwrap();
        }
        db
    }

    #[tokio::test]
    async fn empty_instance_loads_nothing() {
        let db = fresh_db().await;
        let rows = load_for_instance(&db, Uuid::new_v4()).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn block_round_trips() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_block(
            &db,
            instance,
            &BlockKind::Text {
                sender: Some("user".into()),
            },
            "hello world",
            false,
        )
        .await
        .unwrap();
        let rows = load_for_instance(&db, instance).await.unwrap();
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
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_block(
            &db,
            instance,
            &BlockKind::ToolUse {
                name: "shell".into(),
                detail: None,
            },
            "ls /",
            true,
        )
        .await
        .unwrap();
        let rows = load_for_instance(&db, instance).await.unwrap();
        assert_eq!(
            rows,
            vec![StoredRow::Block {
                kind: BlockKind::ToolUse {
                    name: "shell".into(),
                    detail: None,
                },
                text: "ls /".into(),
                truncated: true,
            }]
        );
    }

    #[tokio::test]
    async fn shell_output_round_trips_each_state() {
        use crate::events::ShellState;
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        for (state, cmd, body) in [
            (ShellState::Running, "sleep 1", ""),
            (ShellState::Success, "true", ""),
            (ShellState::Exit(137), "sleep 9", "(killed)"),
        ] {
            persist_block(
                &db,
                instance,
                &BlockKind::ShellOutput {
                    state: state.clone(),
                    cmd: Arc::from(cmd),
                },
                body,
                false,
            )
            .await
            .unwrap();
        }
        let rows = load_for_instance(&db, instance).await.unwrap();
        let kinds: Vec<_> = rows
            .iter()
            .map(|r| match r {
                StoredRow::Block { kind, .. } => kind.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::ShellOutput {
                    state: ShellState::Running,
                    cmd: Arc::from("sleep 1"),
                },
                BlockKind::ShellOutput {
                    state: ShellState::Success,
                    cmd: Arc::from("true"),
                },
                BlockKind::ShellOutput {
                    state: ShellState::Exit(137),
                    cmd: Arc::from("sleep 9"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn error_round_trips() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_error(&db, instance, "boom").await.unwrap();
        let rows = load_for_instance(&db, instance).await.unwrap();
        assert_eq!(
            rows,
            vec![StoredRow::Error {
                text: "boom".into()
            }]
        );
    }

    #[tokio::test]
    async fn rows_are_scoped_per_instance() {
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        persist_block(&db, a, &BlockKind::Text { sender: None }, "a", false)
            .await
            .unwrap();
        persist_block(&db, b, &BlockKind::Text { sender: None }, "b", false)
            .await
            .unwrap();
        let rows_a = load_for_instance(&db, a).await.unwrap();
        let rows_b = load_for_instance(&db, b).await.unwrap();
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
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_block(
            &db,
            instance,
            &BlockKind::Text {
                sender: Some("user".into()),
            },
            "hi",
            false,
        )
        .await
        .unwrap();
        persist_error(&db, instance, "boom").await.unwrap();
        persist_block(
            &db,
            instance,
            &BlockKind::ToolUse {
                name: "shell".into(),
                detail: None,
            },
            "ls",
            true,
        )
        .await
        .unwrap();

        let frames = replay_frames(&db, instance).await.unwrap();
        // Reset + (Block+Stop) + Error + (Block+Truncated) + End
        assert_eq!(frames.len(), 1 + 2 + 1 + 2 + 1);
        assert!(matches!(
            frames.first(),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Reset { .. }))
        ));
        assert!(matches!(
            frames.last(),
            Some(StreamFrame::Scrollback(ScrollbackFrame::End))
        ));
        // Each block row produces exactly one self-describing block
        // frame (kind + full text in one frame) followed by stop /
        // truncated.
        assert!(matches!(
            frames.get(1),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Block {
                kind: BlockKind::Text { sender: Some(_) },
                ..
            })),
        ));
        assert!(matches!(
            frames.get(2),
            Some(StreamFrame::Scrollback(ScrollbackFrame::BlockStop { .. }))
        ));
        assert!(matches!(
            frames.get(3),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Error(_)))
        ));
        assert!(matches!(
            frames.get(4),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Block {
                kind: BlockKind::ToolUse { .. },
                ..
            })),
        ));
        assert!(matches!(
            frames.get(5),
            Some(StreamFrame::Scrollback(
                ScrollbackFrame::BlockTruncated { .. }
            )),
        ));
    }

    /// A single stored block produces exactly:
    /// `[Reset, Block { kind, text }, BlockStop, End]` (all wrapped in
    /// `StreamFrame::Scrollback`). No extra frames slip in.
    #[tokio::test]
    async fn replay_frames_for_single_block_is_minimal() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_block(
            &db,
            instance,
            &BlockKind::ToolUse {
                name: "shell".into(),
                detail: None,
            },
            "ls /",
            false,
        )
        .await
        .unwrap();

        let frames = replay_frames(&db, instance).await.unwrap();
        assert_eq!(frames.len(), 4);
        match &frames[0] {
            StreamFrame::Scrollback(ScrollbackFrame::Reset { .. }) => {}
            other => panic!("expected Reset at [0], got {other:?}"),
        }
        match &frames[1] {
            StreamFrame::Scrollback(ScrollbackFrame::Block {
                kind: BlockKind::ToolUse { name, .. },
                text,
                ..
            }) => {
                assert_eq!(&**name, "shell");
                assert_eq!(text.as_deref(), Some("ls /"));
            }
            other => panic!("expected Block with ToolUse kind at [1], got {other:?}"),
        }
        match &frames[2] {
            StreamFrame::Scrollback(ScrollbackFrame::BlockStop { .. }) => {}
            other => panic!("expected BlockStop at [2], got {other:?}"),
        }
        match &frames[3] {
            StreamFrame::Scrollback(ScrollbackFrame::End) => {}
            other => panic!("expected End at [3], got {other:?}"),
        }
    }
}
