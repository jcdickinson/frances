//! Entity persistence and publishing — the core half of the entity UI
//! protocol.
//!
//! An entity is an [`EntityEnvelope`] (typed: id, kind, lifecycle) plus
//! two opaque-JSON facets the core stores and forwards but never
//! inspects:
//!
//! - **snapshot** — small, latest-wins state. Must be sufficient to
//!   render the entity's collapsed/inline view; anything bigger belongs
//!   in the stream or an artifact.
//! - **stream** — optional append-only ordered items. Read only on
//!   subscription (a tab opening), never at transcript load.
//!
//! Plus **settle artifacts**: bounded derived blobs written once at
//! settle and point-read by `(entity_id, tag)` — e.g. a shell's
//! LLM-visible output digest.
//!
//! The [`EntityHub`] is the single publish point. Producers (the
//! workflow driver, the runtime's singleton writers) call its verbs;
//! frames flow out the events channel latest-wins for snapshots and
//! append-ordered for stream items. Kind-specific policy (caps,
//! teasers, compaction shape) lives entirely in producers — the hub is
//! a policy-free pipe.
//!
//! Appends for one entity must come from a single task (today: the
//! workflow driver), which makes seq order and persist order agree
//! without hub-side coordination.

use std::borrow::Cow;
use std::sync::Arc;

use dashmap::DashMap;
use frances_core::now_ns;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{trace, warn};
use uuid::Uuid;

use frances_models_ui::{EntityEnvelope, Lifecycle};
use frances_storage::{Database, EntitySchema, Migration};

use crate::events::StreamFrame;
use crate::runtime::EventsChannel;

/// Owns the per-session `entities` / `entity_stream` /
/// `entity_artifacts` tables. UUID is permanent; never edit.
pub static SCHEMA: EntitySchema<'static> = EntitySchema {
    entity: Uuid::from_u128(0x6b1f4c8d_2e7a_4f3b_9c05_d84a1e5f7c29),
    migrations: Cow::Borrowed(&[Migration {
        name: Cow::Borrowed("0001_init.sql"),
        sql: Cow::Borrowed(include_str!("migrations/0001_init.sql")),
    }]),
};

/// Well-known id of the singleton workspace entity (kind `workspace`).
pub const WORKSPACE_ENTITY_ID: Uuid = Uuid::from_u128(0x01f0a3b6_7d2c_4e19_8a5b_3c6d9e0f1a2b);
/// Well-known id of the singleton session entity (kind `session`).
pub const SESSION_ENTITY_ID: Uuid = Uuid::from_u128(0x02e1b4c7_8e3d_4f2a_9b6c_4d7e0f1a2b3c);

pub const WORKSPACE_KIND: &str = "workspace";
pub const SESSION_KIND: &str = "session";

/// Snapshot payload of the singleton workspace entity. Typed here (the
/// runtime is its producer); opaque JSON from the hub outward.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    /// Canonical workspace directories; first is the primary dir.
    pub directories: Vec<String>,
}

/// Snapshot payload of the singleton session entity.
#[derive(Debug, Clone, Default, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub title: Option<String>,
    pub usage: Option<frances_models_llm::Usage>,
    /// Footer busy-indicator text (the workflow's `setStatus`). Not
    /// meaningful after settle; a fresh session starts with `None`.
    pub busy: Option<String>,
}

#[derive(Debug, Error)]
pub enum EntityError {
    #[error("entity sql: {0}")]
    Turso(#[from] turso::Error),
    #[error("entity payload encode: {0}")]
    Encode(serde_json::Error),
    #[error("entity payload decode: {0}")]
    Decode(serde_json::Error),
    #[error("entity row: unexpected column shape for {column} (expected {expected})")]
    UnexpectedColumn {
        column: &'static str,
        expected: &'static str,
    },
}

type Result<T> = std::result::Result<T, EntityError>;

/// Frontend subscription state for one entity's stream.
enum SubState {
    /// Nobody is watching; appends persist but don't emit.
    None,
    /// A catch-up replay is in flight; live appends buffer here and the
    /// splice drains them (dropping any the replay already covered).
    CatchingUp {
        buffer: Vec<(u64, serde_json::Value)>,
    },
    /// Appends emit directly.
    Live,
}

struct EntityRecord {
    envelope: EntityEnvelope,
    snapshot: serde_json::Value,
    next_seq: u64,
    sub: SubState,
}

/// The single publish point for entity state. See module docs.
pub struct EntityHub {
    db: Database,
    events: EventsChannel,
    records: DashMap<Uuid, Arc<Mutex<EntityRecord>>>,
}

impl EntityHub {
    /// Open the hub over an already-migrated database: force-settle
    /// every row still marked Live (whatever produced it died with the
    /// previous process), then load all rows.
    pub async fn open(db: Database, events: EventsChannel) -> Result<Self> {
        let hub = Self {
            db,
            events,
            records: DashMap::new(),
        };

        {
            let conn = hub.db.connect().await;
            conn.execute(
                "UPDATE entities SET lifecycle = 1, updated_at = ?1 WHERE lifecycle = 0",
                (now_ns(),),
            )
            .await?;

            let mut rows = conn
                .query(
                    "SELECT entity_id, kind, json(snapshot) FROM entities ORDER BY created_at ASC",
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let entity_id = read_uuid(&row, 0, "entity_id")?;
                let kind = read_text(&row, 1, "kind")?;
                let snapshot: serde_json::Value =
                    serde_json::from_str(&read_text(&row, 2, "snapshot")?)
                        .map_err(EntityError::Decode)?;
                hub.records.insert(
                    entity_id,
                    Arc::new(Mutex::new(EntityRecord {
                        envelope: EntityEnvelope {
                            entity_id,
                            kind,
                            // Everything loaded from disk is settled by
                            // definition — its producer is gone.
                            lifecycle: Lifecycle::Settled,
                        },
                        snapshot,
                        next_seq: 0,
                        sub: SubState::None,
                    })),
                );
            }
        }

        Ok(hub)
    }

    /// Insert-or-update an entity's envelope + snapshot, persist it,
    /// and publish the latest-wins upsert.
    pub async fn upsert_snapshot(
        &self,
        envelope: EntityEnvelope,
        snapshot: serde_json::Value,
    ) -> Result<()> {
        let snapshot_json = serde_json::to_string(&snapshot).map_err(EntityError::Encode)?;
        let now = now_ns();
        {
            let conn = self.db.connect().await;
            conn.execute(
                "INSERT INTO entities (entity_id, kind, lifecycle, snapshot, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, jsonb(?4), ?5, ?5) \
                 ON CONFLICT (entity_id) DO UPDATE SET \
                 kind = ?2, lifecycle = ?3, snapshot = jsonb(?4), updated_at = ?5",
                (
                    envelope.entity_id.as_bytes().to_vec(),
                    envelope.kind.clone(),
                    lifecycle_int(envelope.lifecycle),
                    snapshot_json,
                    now,
                ),
            )
            .await?;
        }

        let record = self
            .records
            .entry(envelope.entity_id)
            .or_insert_with(|| {
                Arc::new(Mutex::new(EntityRecord {
                    envelope: envelope.clone(),
                    snapshot: serde_json::Value::Null,
                    next_seq: 0,
                    sub: SubState::None,
                }))
            })
            .clone();
        {
            let mut record = record.lock();
            record.envelope = envelope.clone();
            record.snapshot = snapshot.clone();
        }

        self.events
            .send(StreamFrame::EntityUpsert { envelope, snapshot });
        Ok(())
    }

    /// Append one item to an entity's stream. Fire-and-forget from the
    /// producer's perspective: unknown or settled entities trace and
    /// drop (the workflow may race teardown), only storage errors
    /// surface.
    pub async fn append(&self, entity_id: Uuid, payload: serde_json::Value) -> Result<()> {
        let Some(record) = self.records.get(&entity_id).map(|r| r.clone()) else {
            trace!(%entity_id, "append to unknown entity dropped");
            return Ok(());
        };

        let seq = {
            let mut record = record.lock();
            if record.envelope.lifecycle == Lifecycle::Settled {
                trace!(%entity_id, "append to settled entity dropped");
                return Ok(());
            }
            record.next_seq += 1;
            let seq = record.next_seq;
            match &mut record.sub {
                SubState::None => {}
                SubState::CatchingUp { buffer } => buffer.push((seq, payload.clone())),
                SubState::Live => self.events.send(StreamFrame::EntityStream {
                    entity_id,
                    seq,
                    payload: payload.clone(),
                }),
            }
            seq
        };

        let payload_json = serde_json::to_string(&payload).map_err(EntityError::Encode)?;
        let conn = self.db.connect().await;
        conn.execute(
            "INSERT INTO entity_stream (entity_id, seq, payload, created_at) \
             VALUES (?1, ?2, jsonb(?3), ?4)",
            (
                entity_id.as_bytes().to_vec(),
                seq as i64,
                payload_json,
                now_ns(),
            ),
        )
        .await?;
        Ok(())
    }

    /// Attach the frontend to an entity's stream. With `catch_up`,
    /// replays every persisted item then splices into the live feed
    /// gap-free; without, just starts tailing (the caller watched from
    /// the entity's birth — an inline live view).
    pub async fn subscribe(&self, entity_id: Uuid, catch_up: bool) -> Result<()> {
        let Some(record) = self.records.get(&entity_id).map(|r| r.clone()) else {
            trace!(%entity_id, "subscribe to unknown entity ignored");
            return Ok(());
        };

        if !catch_up {
            record.lock().sub = SubState::Live;
            return Ok(());
        }

        record.lock().sub = SubState::CatchingUp { buffer: Vec::new() };

        let mut last_replayed: u64 = 0;
        {
            let conn = self.db.connect().await;
            let mut rows = conn
                .query(
                    "SELECT seq, json(payload) FROM entity_stream \
                     WHERE entity_id = ?1 ORDER BY seq ASC",
                    (entity_id.as_bytes().to_vec(),),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let seq = read_u64(&row, 0, "seq")?;
                let payload: serde_json::Value =
                    serde_json::from_str(&read_text(&row, 1, "payload")?)
                        .map_err(EntityError::Decode)?;
                self.events.send(StreamFrame::EntityStream {
                    entity_id,
                    seq,
                    payload,
                });
                last_replayed = seq;
            }
        }

        let mut record = record.lock();
        if let SubState::CatchingUp { buffer } = std::mem::replace(&mut record.sub, SubState::Live)
        {
            for (seq, payload) in buffer {
                if seq > last_replayed {
                    self.events.send(StreamFrame::EntityStream {
                        entity_id,
                        seq,
                        payload,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn unsubscribe(&self, entity_id: Uuid) {
        if let Some(record) = self.records.get(&entity_id) {
            record.lock().sub = SubState::None;
        }
    }

    /// Settle an entity: final snapshot, lifecycle flip, artifacts, and
    /// (optionally) a compacted replacement for the persisted stream.
    /// The producer's one chance for kind-specific finalization.
    pub async fn settle(
        &self,
        entity_id: Uuid,
        snapshot: serde_json::Value,
        artifacts: Vec<(String, serde_json::Value)>,
        compacted_stream: Option<Vec<serde_json::Value>>,
    ) -> Result<()> {
        let Some(record) = self.records.get(&entity_id).map(|r| r.clone()) else {
            trace!(%entity_id, "settle of unknown entity ignored");
            return Ok(());
        };

        let envelope = {
            let mut record = record.lock();
            if record.envelope.lifecycle == Lifecycle::Settled {
                trace!(%entity_id, "double settle ignored");
                return Ok(());
            }
            record.envelope.lifecycle = Lifecycle::Settled;
            record.snapshot = snapshot.clone();
            record.envelope.clone()
        };

        let snapshot_json = serde_json::to_string(&snapshot).map_err(EntityError::Encode)?;
        let now = now_ns();
        {
            let conn = self.db.connect().await;
            conn.execute(
                "UPDATE entities SET lifecycle = 1, snapshot = jsonb(?2), updated_at = ?3 \
                 WHERE entity_id = ?1",
                (entity_id.as_bytes().to_vec(), snapshot_json, now),
            )
            .await?;

            for (tag, payload) in artifacts {
                let payload_json = serde_json::to_string(&payload).map_err(EntityError::Encode)?;
                conn.execute(
                    "INSERT INTO entity_artifacts (entity_id, tag, payload, created_at) \
                     VALUES (?1, ?2, jsonb(?3), ?4) \
                     ON CONFLICT (entity_id, tag) DO UPDATE SET payload = jsonb(?3)",
                    (entity_id.as_bytes().to_vec(), tag, payload_json, now),
                )
                .await?;
            }

            if let Some(items) = compacted_stream {
                conn.execute(
                    "DELETE FROM entity_stream WHERE entity_id = ?1",
                    (entity_id.as_bytes().to_vec(),),
                )
                .await?;
                for (index, payload) in items.into_iter().enumerate() {
                    let payload_json =
                        serde_json::to_string(&payload).map_err(EntityError::Encode)?;
                    conn.execute(
                        "INSERT INTO entity_stream (entity_id, seq, payload, created_at) \
                         VALUES (?1, ?2, jsonb(?3), ?4)",
                        (
                            entity_id.as_bytes().to_vec(),
                            (index + 1) as i64,
                            payload_json,
                            now,
                        ),
                    )
                    .await?;
                }
            }
        }

        self.events
            .send(StreamFrame::EntityUpsert { envelope, snapshot });
        Ok(())
    }

    /// Point-read one settle artifact.
    pub async fn read_artifact(
        &self,
        entity_id: Uuid,
        tag: &str,
    ) -> Result<Option<serde_json::Value>> {
        let conn = self.db.connect().await;
        let mut rows = conn
            .query(
                "SELECT json(payload) FROM entity_artifacts WHERE entity_id = ?1 AND tag = ?2",
                (entity_id.as_bytes().to_vec(), tag.to_owned()),
            )
            .await?;
        match rows.next().await? {
            Some(row) => {
                let payload = serde_json::from_str(&read_text(&row, 0, "payload")?)
                    .map_err(EntityError::Decode)?;
                Ok(Some(payload))
            }
            None => Ok(None),
        }
    }

    /// Generic repair: flip every Live entity to Settled (snapshot
    /// stays as last persisted). Used when producers go away wholesale
    /// — workflow finish/dehydrate teardown.
    pub async fn force_settle_all_live(&self) -> Result<()> {
        {
            let conn = self.db.connect().await;
            conn.execute(
                "UPDATE entities SET lifecycle = 1, updated_at = ?1 WHERE lifecycle = 0",
                (now_ns(),),
            )
            .await?;
        }

        for entry in self.records.iter() {
            let (envelope, snapshot) = {
                let mut record = entry.value().lock();
                if record.envelope.lifecycle == Lifecycle::Settled {
                    continue;
                }
                record.envelope.lifecycle = Lifecycle::Settled;
                (record.envelope.clone(), record.snapshot.clone())
            };
            self.events
                .send(StreamFrame::EntityUpsert { envelope, snapshot });
        }
        Ok(())
    }

    /// The attach snapshot: one upsert per known entity, queued into
    /// the events channel ahead of the scrollback replay burst.
    pub fn attach_publish_all(&self) {
        for entry in self.records.iter() {
            let (envelope, snapshot) = {
                let record = entry.value().lock();
                (record.envelope.clone(), record.snapshot.clone())
            };
            self.events
                .send(StreamFrame::EntityUpsert { envelope, snapshot });
        }
    }

    /// Clone an entity's current snapshot payload.
    pub fn snapshot(&self, entity_id: Uuid) -> Option<serde_json::Value> {
        self.records
            .get(&entity_id)
            .map(|record| record.lock().snapshot.clone())
    }

    /// Typed read-modify-write of the singleton session snapshot.
    /// Missing or undecodable state starts from default rather than
    /// erroring — the singleton is best-effort UI chrome.
    pub async fn update_session(&self, apply: impl FnOnce(&mut SessionSnapshot)) {
        let mut session: SessionSnapshot = self
            .snapshot(SESSION_ENTITY_ID)
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        apply(&mut session);
        self.upsert_singleton(SESSION_ENTITY_ID, SESSION_KIND, &session)
            .await;
    }

    /// Publish the singleton workspace snapshot.
    pub async fn set_workspace(&self, workspace: &WorkspaceSnapshot) {
        self.upsert_singleton(WORKSPACE_ENTITY_ID, WORKSPACE_KIND, workspace)
            .await;
    }

    /// Seeds a booting workflow's `getTitle` (via `WorkflowDeps`).
    pub fn session_title(&self) -> Option<String> {
        let session: SessionSnapshot = self
            .snapshot(SESSION_ENTITY_ID)
            .and_then(|value| serde_json::from_value(value).ok())?;
        session.title
    }

    async fn upsert_singleton(&self, id: Uuid, kind: &str, snapshot: &impl Serialize) {
        let value = match serde_json::to_value(snapshot) {
            Ok(value) => value,
            Err(error) => {
                warn!(%error, kind, "singleton snapshot encode failed");
                return;
            }
        };
        let envelope = EntityEnvelope {
            entity_id: id,
            kind: kind.to_owned(),
            lifecycle: Lifecycle::Live,
        };
        if let Err(error) = self.upsert_snapshot(envelope, value).await {
            warn!(%error, kind, "singleton snapshot upsert failed");
        }
    }
}

fn lifecycle_int(lifecycle: Lifecycle) -> i64 {
    match lifecycle {
        Lifecycle::Live => 0,
        Lifecycle::Settled => 1,
    }
}

fn read_text(row: &turso::Row, index: usize, column: &'static str) -> Result<String> {
    match row.get_value(index)? {
        turso::Value::Text(s) => Ok(s),
        _ => Err(EntityError::UnexpectedColumn {
            column,
            expected: "TEXT",
        }),
    }
}

fn read_u64(row: &turso::Row, index: usize, column: &'static str) -> Result<u64> {
    match row.get_value(index)? {
        turso::Value::Integer(n) if n >= 0 => Ok(n as u64),
        _ => Err(EntityError::UnexpectedColumn {
            column,
            expected: "non-negative INTEGER",
        }),
    }
}

fn read_uuid(row: &turso::Row, index: usize, column: &'static str) -> Result<Uuid> {
    match row.get_value(index)? {
        turso::Value::Blob(bytes) => {
            Uuid::from_slice(&bytes).map_err(|_| EntityError::UnexpectedColumn {
                column,
                expected: "16-byte BLOB",
            })
        }
        _ => Err(EntityError::UnexpectedColumn {
            column,
            expected: "BLOB",
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;

    async fn hub() -> (EntityHub, UnboundedReceiver<StreamFrame>) {
        let db = crate::store::open_in_memory().await.unwrap();
        let (events, rx) = EventsChannel::new();
        let hub = EntityHub::open(db, events).await.unwrap();
        (hub, rx)
    }

    fn live(id: Uuid, kind: &str) -> EntityEnvelope {
        EntityEnvelope {
            entity_id: id,
            kind: kind.to_owned(),
            lifecycle: Lifecycle::Live,
        }
    }

    fn drain(rx: &mut UnboundedReceiver<StreamFrame>) -> Vec<StreamFrame> {
        let mut frames = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        frames
    }

    fn stream_seqs(frames: &[StreamFrame]) -> Vec<u64> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                StreamFrame::EntityStream { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn upsert_replaces_snapshot() {
        let (hub, mut rx) = hub().await;
        let id = Uuid::new_v4();

        hub.upsert_snapshot(live(id, "shell"), json!({"cmd": "ls"}))
            .await
            .unwrap();
        hub.upsert_snapshot(live(id, "shell"), json!({"cmd": "ls -la"}))
            .await
            .unwrap();

        assert_eq!(hub.snapshot(id).unwrap(), json!({"cmd": "ls -la"}));
        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 2, "one upsert frame per publish");
    }

    #[tokio::test]
    async fn append_assigns_contiguous_seq_and_persists() {
        let (hub, mut rx) = hub().await;
        let id = Uuid::new_v4();
        hub.upsert_snapshot(live(id, "shell"), json!({}))
            .await
            .unwrap();

        for index in 0..3 {
            hub.append(id, json!({ "text": index })).await.unwrap();
        }

        // Nothing subscribed: no stream frames emitted.
        assert!(stream_seqs(&drain(&mut rx)).is_empty());

        // But everything persisted: a catch-up subscribe replays 1..=3.
        hub.subscribe(id, true).await.unwrap();
        assert_eq!(stream_seqs(&drain(&mut rx)), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn tail_subscribe_skips_history() {
        let (hub, mut rx) = hub().await;
        let id = Uuid::new_v4();
        hub.upsert_snapshot(live(id, "shell"), json!({}))
            .await
            .unwrap();
        hub.append(id, json!({"text": "old"})).await.unwrap();

        hub.subscribe(id, false).await.unwrap();
        drain(&mut rx);
        hub.append(id, json!({"text": "new"})).await.unwrap();

        assert_eq!(stream_seqs(&drain(&mut rx)), vec![2]);
    }

    /// Appends racing a catch-up replay must come out gap-free and
    /// dupe-free. Current-thread runtime interleaves the two tasks at
    /// their db await points, so appends land mid-replay.
    #[tokio::test]
    async fn subscribe_splice_is_gap_free() {
        let (hub, mut rx) = hub().await;
        let id = Uuid::new_v4();
        hub.upsert_snapshot(live(id, "shell"), json!({}))
            .await
            .unwrap();
        for index in 0..5 {
            hub.append(id, json!({ "text": index })).await.unwrap();
        }
        drain(&mut rx);

        tokio::join!(
            async {
                hub.subscribe(id, true).await.unwrap();
            },
            async {
                for index in 5..10 {
                    hub.append(id, json!({ "text": index })).await.unwrap();
                }
            }
        );

        let seqs = stream_seqs(&drain(&mut rx));
        assert_eq!(seqs, (1..=10).collect::<Vec<u64>>());
    }

    #[tokio::test]
    async fn settle_writes_artifacts_and_compacts() {
        let (hub, mut rx) = hub().await;
        let id = Uuid::new_v4();
        hub.upsert_snapshot(live(id, "shell"), json!({"state": "running"}))
            .await
            .unwrap();
        for index in 0..4 {
            hub.append(id, json!({ "text": index })).await.unwrap();
        }
        drain(&mut rx);

        hub.settle(
            id,
            json!({"state": "success"}),
            vec![("llm_digest".to_owned(), json!("exit 0"))],
            Some(vec![json!({"text": "0123"})]),
        )
        .await
        .unwrap();

        let frames = drain(&mut rx);
        assert!(matches!(
            &frames[..],
            [StreamFrame::EntityUpsert { envelope, .. }]
                if envelope.lifecycle == Lifecycle::Settled
        ));
        assert_eq!(
            hub.read_artifact(id, "llm_digest").await.unwrap(),
            Some(json!("exit 0"))
        );

        // Replay sees only the compacted stream.
        hub.subscribe(id, true).await.unwrap();
        assert_eq!(stream_seqs(&drain(&mut rx)), vec![1]);

        // Double settle is a no-op.
        hub.settle(id, json!({}), Vec::new(), None).await.unwrap();
        assert!(drain(&mut rx).is_empty());

        // Appends after settle are dropped.
        hub.append(id, json!({"text": "late"})).await.unwrap();
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn force_settle_flips_live_entities() {
        let (hub, mut rx) = hub().await;
        let id = Uuid::new_v4();
        hub.upsert_snapshot(live(id, "shell"), json!({"state": "running"}))
            .await
            .unwrap();
        drain(&mut rx);

        hub.force_settle_all_live().await.unwrap();

        let frames = drain(&mut rx);
        assert!(matches!(
            &frames[..],
            [StreamFrame::EntityUpsert { envelope, snapshot }]
                if envelope.lifecycle == Lifecycle::Settled
                    && snapshot == &json!({"state": "running"})
        ));
    }

    /// A hub opened over an existing db force-settles whatever the
    /// previous process left Live, and loads it for attach publishing.
    #[tokio::test]
    async fn open_force_settles_previous_lives() {
        let db = crate::store::open_in_memory().await.unwrap();
        let id = Uuid::new_v4();

        {
            let (events, _rx) = EventsChannel::new();
            let hub = EntityHub::open(db.clone(), events).await.unwrap();
            hub.upsert_snapshot(live(id, "shell"), json!({"state": "running"}))
                .await
                .unwrap();
        }

        let (events, mut rx) = EventsChannel::new();
        let hub = EntityHub::open(db, events).await.unwrap();
        hub.attach_publish_all();

        let frames = drain(&mut rx);
        assert!(matches!(
            &frames[..],
            [StreamFrame::EntityUpsert { envelope, .. }]
                if envelope.entity_id == id && envelope.lifecycle == Lifecycle::Settled
        ));
    }
}
