//! Per-workflow scrollback persistence.
//!
//! ## Writes
//!
//! - **Clean close** (a `SectionClose` event): [`persist_section`]
//!   with `truncated = false`. Called from the `EmitState`'s normal
//!   close path inside `workflows::emit`.
//! - **Dehydrate-interrupted close**: same call with
//!   `truncated = true`. Triggered when a workflow is pushed off the
//!   top with an in-flight section.
//! - **Error sections**: [`persist_error`] writes a row whose payload
//!   has `kind = "error"` whenever the runtime emits a
//!   [`StreamFrame::Error`]. `truncated` is ignored for these.
//!
//! ## Reads
//!
//! [`replay_to_channel`] queries every row for the given workflow
//! instance in `id` order and emits a [`ScrollbackFrame`] burst (each
//! wrapped in [`StreamFrame::Scrollback`]) on the supplied channel:
//!
//! 1. [`ScrollbackFrame::Reset`] — TUI clears its in-memory
//!    scrollback and begins the burst.
//! 2. For each row: a single self-describing
//!    [`ScrollbackFrame::SectionAppend`] (the dispatcher constructs
//!    the section on first sight of the id) followed by
//!    [`ScrollbackFrame::SectionClose`] or
//!    [`ScrollbackFrame::SectionTruncated`]. Error rows emit a single
//!    [`ScrollbackFrame::Error`].
//! 3. [`ScrollbackFrame::End`] — TUI returns to live mode.
//!
//! Replay's id allocator is independent of live-side `SectionId`s:
//! each replayed section opens and closes within the burst before the
//! next one starts, so the TUI's section map is empty at
//! [`ScrollbackFrame::End`] and live ids after replay collide with
//! nothing.

use std::borrow::Cow;

use frances_core::now_ns;
use frances_edit::DiffOp;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use frances_storage::{Database, EntitySchema, Migration};

use crate::Result;
use crate::events::{
    ReasoningState, ScrollbackFrame, SectionId, SectionKind, ShellState, Source, StreamFrame,
};
use crate::runtime::EventsChannel;

/// Owns the per-session `scrollback_sections` table. UUID is permanent;
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
    #[error("scrollback payload decode: {0}")]
    Decode(serde_json::Error),
    #[error("scrollback row: unexpected column shape for {column} (expected {expected})")]
    UnexpectedColumn {
        column: &'static str,
        expected: &'static str,
    },
}

/// Decoded scrollback row. Either a real section (in which case
/// `kind`/`text` reconstruct the section on replay) or an
/// error-side-channel row (rendered as [`ScrollbackFrame::Error`]).
#[derive(Debug, Clone, PartialEq)]
pub enum StoredRow {
    Section {
        kind: SectionKind,
        text: String,
        truncated: bool,
    },
    Error {
        text: String,
    },
}

/// On-disk payload. The tag-internal serde derives mean the row's
/// JSON column directly identifies its variant — no parallel `kind`
/// column needed. Closing the `review-quality.md` "stringly `kind` +
/// JSON payload columns instead of `#[serde(tag)]`" finding.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RowPayload {
    Markdown {
        source: Source,
        text: String,
    },
    Error {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Json {
        tag: String,
        value: serde_json::Value,
    },
    ShellOutput {
        state: ShellState,
        cmd: String,
        text: String,
    },
    Reasoning {
        state: ReasoningState,
        text: String,
    },
    Diff {
        lines: Vec<DiffOp>,
    },
}

/// Insert a finished or truncated section row. `truncated = false`
/// corresponds to a clean `SectionClose`; `truncated = true`
/// corresponds to a workflow dehydrated mid-stream.
pub async fn persist_section(
    db: &Database,
    instance: Uuid,
    kind: SectionKind,
    text: String,
    truncated: bool,
) -> std::result::Result<(), ScrollbackError> {
    let payload = encode_payload(kind, text);
    let payload_json = serde_json::to_string(&payload).map_err(ScrollbackError::Encode)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    let truncated_i = if truncated { 1 } else { 0 };
    let conn = db.connect().await;
    conn.execute(
        "INSERT INTO scrollback_sections (instance_id, payload, truncated, created_at) \
         VALUES (?1, jsonb(?2), ?3, ?4)",
        (instance_bytes, payload_json, truncated_i, now),
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
    let payload = RowPayload::Error {
        text: text.to_owned(),
    };
    let payload_json = serde_json::to_string(&payload).map_err(ScrollbackError::Encode)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    let conn = db.connect().await;
    conn.execute(
        "INSERT INTO scrollback_sections (instance_id, payload, truncated, created_at) \
         VALUES (?1, jsonb(?2), 0, ?3)",
        (instance_bytes, payload_json, now),
    )
    .await?;
    Ok(())
}

/// Load every row for an instance in insertion order. The list maps
/// 1:1 onto the replay frame burst.
pub async fn load_for_instance(
    db: &Database,
    instance: Uuid,
) -> std::result::Result<Vec<StoredRow>, ScrollbackError> {
    let conn = db.connect().await;
    let mut rows = conn
        .query(
            "SELECT json(payload), truncated FROM scrollback_sections \
             WHERE instance_id = ?1 ORDER BY id ASC",
            (instance.as_bytes().to_vec(),),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let payload_text = match row.get_value(0)? {
            turso::Value::Text(s) => s,
            _ => {
                return Err(ScrollbackError::UnexpectedColumn {
                    column: "payload",
                    expected: "TEXT (jsonb-rendered)",
                });
            }
        };
        let truncated = matches!(row.get_value(1)?, turso::Value::Integer(n) if n != 0);
        let payload: RowPayload =
            serde_json::from_str(&payload_text).map_err(ScrollbackError::Decode)?;
        out.push(decode_payload(payload, truncated));
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

    let mut next_id: u64 = 1;
    for row in rows {
        match row {
            StoredRow::Section {
                kind,
                text,
                truncated,
            } => {
                let id = SectionId(next_id);
                next_id += 1;
                events.send(StreamFrame::Scrollback(ScrollbackFrame::SectionAppend {
                    id,
                    kind,
                    delta: text,
                }));
                if truncated {
                    events.send(StreamFrame::Scrollback(ScrollbackFrame::SectionTruncated {
                        id,
                    }));
                } else {
                    events.send(StreamFrame::Scrollback(ScrollbackFrame::SectionClose {
                        id,
                    }));
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

fn encode_payload(kind: SectionKind, text: String) -> RowPayload {
    match kind {
        SectionKind::Markdown { source } => RowPayload::Markdown { source, text },
        SectionKind::Error => RowPayload::Error { text },
        SectionKind::ToolUse { name, detail } => RowPayload::ToolUse { name, detail },
        SectionKind::Json { tag, value } => RowPayload::Json { tag, value },
        SectionKind::ShellOutput { state, cmd } => RowPayload::ShellOutput { state, cmd, text },
        SectionKind::Reasoning { state } => RowPayload::Reasoning { state, text },
        SectionKind::Diff { lines } => RowPayload::Diff { lines },
    }
}

fn decode_payload(payload: RowPayload, truncated: bool) -> StoredRow {
    match payload {
        RowPayload::Markdown { source, text } => StoredRow::Section {
            kind: SectionKind::Markdown { source },
            text,
            truncated,
        },
        RowPayload::Error { text } => StoredRow::Error { text },
        RowPayload::ToolUse { name, detail } => StoredRow::Section {
            kind: SectionKind::ToolUse { name, detail },
            text: String::new(),
            truncated,
        },
        RowPayload::Json { tag, value } => {
            let body = serde_json::to_string(&value).unwrap_or_else(|_| "<unserializable>".into());
            let text = format!("[{tag}] {body}");
            StoredRow::Section {
                kind: SectionKind::Json { tag, value },
                text,
                truncated,
            }
        }
        RowPayload::ShellOutput { state, cmd, text } => StoredRow::Section {
            kind: SectionKind::ShellOutput { state, cmd },
            text,
            truncated,
        },
        RowPayload::Reasoning { state, text } => StoredRow::Section {
            kind: SectionKind::Reasoning { state },
            text,
            truncated,
        },
        RowPayload::Diff { lines } => StoredRow::Section {
            kind: SectionKind::Diff { lines },
            text: String::new(),
            truncated,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_storage::run_all;

    async fn fresh_db() -> Database {
        let db = Database::open_in_memory().await.unwrap();
        let conn = db.connect().await;
        run_all(&conn, &[&SCHEMA]).await.unwrap();
        db
    }

    async fn collect_replay(db: &Database, instance: Uuid) -> Vec<StreamFrame> {
        let (events, mut rx) = EventsChannel::new();
        replay_to_channel(&events, db, instance).await.unwrap();
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(frame);
        }
        out
    }

    /// Replay of one stored section is `[Reset, SectionAppend,
    /// SectionClose, End]`. Same shape as before the schema swap.
    #[tokio::test]
    async fn replay_frames_for_single_section_is_minimal() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_section(
            &db,
            instance,
            SectionKind::ToolUse {
                name: "shell".into(),
                detail: None,
            },
            String::new(),
            false,
        )
        .await
        .unwrap();

        let frames = collect_replay(&db, instance).await;
        assert_eq!(frames.len(), 4);
        match &frames[0] {
            StreamFrame::Scrollback(ScrollbackFrame::Reset { .. }) => {}
            other => panic!("expected Reset at [0], got {other:?}"),
        }
        match &frames[1] {
            StreamFrame::Scrollback(ScrollbackFrame::SectionAppend {
                kind: SectionKind::ToolUse { name, .. },
                ..
            }) => {
                assert_eq!(name, "shell");
            }
            other => panic!("expected SectionAppend with ToolUse kind at [1], got {other:?}"),
        }
        match &frames[2] {
            StreamFrame::Scrollback(ScrollbackFrame::SectionClose { .. }) => {}
            other => panic!("expected SectionClose at [2], got {other:?}"),
        }
        match &frames[3] {
            StreamFrame::Scrollback(ScrollbackFrame::End) => {}
            other => panic!("expected End at [3], got {other:?}"),
        }
    }

    /// Multi-row instance: text section + error + tool-use + truncated
    /// section, all of which round-trip through the new schema.
    #[tokio::test]
    async fn replay_mixed_rows() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_section(
            &db,
            instance,
            SectionKind::Markdown {
                source: Source::User,
            },
            "hi".into(),
            false,
        )
        .await
        .unwrap();
        persist_error(&db, instance, "boom").await.unwrap();
        persist_section(
            &db,
            instance,
            SectionKind::ToolUse {
                name: "shell".into(),
                detail: None,
            },
            String::new(),
            false,
        )
        .await
        .unwrap();
        persist_section(
            &db,
            instance,
            SectionKind::Markdown {
                source: Source::Assistant,
            },
            "partial".into(),
            true,
        )
        .await
        .unwrap();

        let frames = collect_replay(&db, instance).await;
        assert!(matches!(
            frames.first(),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Reset { .. })),
        ));
        assert!(matches!(
            frames.get(1),
            Some(StreamFrame::Scrollback(ScrollbackFrame::SectionAppend {
                kind: SectionKind::Markdown {
                    source: Source::User,
                },
                ..
            })),
        ));
        assert!(matches!(
            frames.get(2),
            Some(StreamFrame::Scrollback(
                ScrollbackFrame::SectionClose { .. }
            ))
        ));
        assert!(matches!(
            frames.get(3),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Error(_)))
        ));
        assert!(matches!(
            frames.get(4),
            Some(StreamFrame::Scrollback(ScrollbackFrame::SectionAppend {
                kind: SectionKind::ToolUse { .. },
                ..
            })),
        ));
        assert!(matches!(
            frames.get(5),
            Some(StreamFrame::Scrollback(
                ScrollbackFrame::SectionClose { .. }
            ))
        ));
        assert!(matches!(
            frames.get(6),
            Some(StreamFrame::Scrollback(ScrollbackFrame::SectionAppend {
                kind: SectionKind::Markdown {
                    source: Source::Assistant,
                },
                ..
            })),
        ));
        assert!(matches!(
            frames.get(7),
            Some(StreamFrame::Scrollback(
                ScrollbackFrame::SectionTruncated { .. }
            )),
        ));
    }
}
