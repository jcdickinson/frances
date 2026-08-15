//! Per-workflow scrollback persistence.
//!
//! ## Writes
//!
//! [`persist_section`] writes one row per section the workflow pushes,
//! called from the emit path in `workflows::emit`. Sections are
//! one-shot, so a row is complete the moment it's written — there is no
//! open/close bookkeeping and nothing to reconcile if a workflow is
//! dehydrated mid-flight.
//!
//! ## Reads
//!
//! [`replay_to_channel`] queries every row for the given workflow
//! instance in `id` order and emits a [`ScrollbackFrame`] burst (each
//! wrapped in [`StreamFrame::Scrollback`]) on the supplied channel:
//!
//! 1. [`ScrollbackFrame::Reset`] — UI clears its in-memory
//!    scrollback and begins the burst.
//! 2. One [`ScrollbackFrame::Section`] per row, except error rows,
//!    which replay as [`ScrollbackFrame::Error`] to match how they
//!    were emitted live.
//! 3. [`ScrollbackFrame::End`] — UI returns to live mode.

use std::borrow::Cow;

use frances_core::now_ns;
use thiserror::Error;
use uuid::Uuid;

use frances_storage::{Database, EntitySchema, Migration};

use crate::Result;
use crate::events::{ScrollbackFrame, SectionKind, StreamFrame};
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

/// Insert one section row.
pub async fn persist_section(
    db: &Database,
    instance: Uuid,
    kind: &SectionKind,
) -> std::result::Result<(), ScrollbackError> {
    let payload_json = serde_json::to_string(kind).map_err(ScrollbackError::Encode)?;
    let instance_bytes = instance.as_bytes().to_vec();
    let now = now_ns();
    let conn = db.connect().await;
    conn.execute(
        "INSERT INTO scrollback_sections (instance_id, payload, created_at) \
         VALUES (?1, jsonb(?2), ?3)",
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
) -> std::result::Result<Vec<SectionKind>, ScrollbackError> {
    let conn = db.connect().await;
    let mut rows = conn
        .query(
            "SELECT json(payload) FROM scrollback_sections \
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
        out.push(serde_json::from_str(&payload_text).map_err(ScrollbackError::Decode)?);
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

    for kind in rows {
        let frame = match kind {
            SectionKind::Error { text } => ScrollbackFrame::Error(text),
            kind => ScrollbackFrame::Section(kind),
        };
        events.send(StreamFrame::Scrollback(frame));
    }

    events.send(StreamFrame::Scrollback(ScrollbackFrame::End));
    Ok(())
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

    /// Replay of one stored section is `[Reset, Section, End]`.
    #[tokio::test]
    async fn replay_frames_for_single_section_is_minimal() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_section(
            &db,
            instance,
            &SectionKind::Json {
                tag: "plan".into(),
                value: serde_json::json!({ "step": 1 }),
            },
        )
        .await
        .unwrap();

        let frames = collect_replay(&db, instance).await;
        assert_eq!(frames.len(), 3);
        match &frames[0] {
            StreamFrame::Scrollback(ScrollbackFrame::Reset { .. }) => {}
            other => panic!("expected Reset at [0], got {other:?}"),
        }
        match &frames[1] {
            StreamFrame::Scrollback(ScrollbackFrame::Section(SectionKind::Json {
                tag, ..
            })) => {
                assert_eq!(tag, "plan");
            }
            other => panic!("expected Section with Json kind at [1], got {other:?}"),
        }
        match &frames[2] {
            StreamFrame::Scrollback(ScrollbackFrame::End) => {}
            other => panic!("expected End at [2], got {other:?}"),
        }
    }

    /// An `EntityRef` row round-trips: persisted and replayed as a
    /// single self-describing section carrying the entity id.
    #[tokio::test]
    async fn entity_ref_roundtrip() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        persist_section(&db, instance, &SectionKind::EntityRef { entity_id })
            .await
            .unwrap();

        let frames = collect_replay(&db, instance).await;
        assert_eq!(frames.len(), 3);
        match &frames[1] {
            StreamFrame::Scrollback(ScrollbackFrame::Section(SectionKind::EntityRef {
                entity_id: got,
            })) => assert_eq!(*got, entity_id),
            other => panic!("expected EntityRef section at [1], got {other:?}"),
        }
    }

    /// Error rows replay as `Error` frames; everything else as sections,
    /// in insertion order.
    #[tokio::test]
    async fn replay_mixed_rows() {
        let db = fresh_db().await;
        let instance = Uuid::new_v4();
        persist_section(
            &db,
            instance,
            &SectionKind::Json {
                tag: "plan".into(),
                value: serde_json::json!({ "step": 1 }),
            },
        )
        .await
        .unwrap();
        persist_section(
            &db,
            instance,
            &SectionKind::Error {
                text: "boom".into(),
            },
        )
        .await
        .unwrap();
        persist_section(&db, instance, &SectionKind::Diff { lines: Vec::new() })
            .await
            .unwrap();

        let frames = collect_replay(&db, instance).await;
        assert!(matches!(
            frames.first(),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Reset { .. })),
        ));
        assert!(matches!(
            frames.get(1),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Section(
                SectionKind::Json { .. }
            ))),
        ));
        assert!(matches!(
            frames.get(2),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Error(_)))
        ));
        assert!(matches!(
            frames.get(3),
            Some(StreamFrame::Scrollback(ScrollbackFrame::Section(
                SectionKind::Diff { .. }
            ))),
        ));
        assert!(matches!(
            frames.get(4),
            Some(StreamFrame::Scrollback(ScrollbackFrame::End))
        ));
    }
}
