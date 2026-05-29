//! DB-backed config provider — overrides per session.
//!
//! Reads from a `session_config` table on the per-session turso DB and
//! emits one [`ConfigEvent`] per row. Lives in `frances` (not in
//! `frances-config`) so the config crate stays independent of turso.
//!
//! Writes go through [`SessionConfigWriter`], obtained from
//! [`SessionConfigProvider::writer`] after the provider has been driven
//! through `ConfigHandle::build`. The writer persists rows to the same
//! table and emits the events on this provider's layer in one call.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use frances_config::{ConfigEvent, ConfigProvider, EventSender, Path, ProviderError, Value};
use frances_storage::{EntitySchema, Migration};
use thiserror::Error;
use tracing::{debug, warn};
use twox_hash::XxHash3_64;
use uuid::Uuid;

use crate::store::Database;

/// Owns the `session_config` table. UUID is permanent.
pub static SCHEMA: EntitySchema<'static> = EntitySchema {
    entity: Uuid::from_u128(0x33578ba6_759b_42c5_8c7f_94932a153732),
    migrations: Cow::Borrowed(&[Migration {
        name: Cow::Borrowed("0001_init.sql"),
        sql: Cow::Borrowed(include_str!("session_provider/migrations/0001_init.sql")),
    }]),
};

/// Reads `(path, kind, value)` rows from `session_config` on the
/// per-session DB and emits them as [`ConfigEvent`]s. After `load()` has
/// run, [`writer`](Self::writer) hands out a [`SessionConfigWriter`] that
/// persists updates to the same table and emits them on this layer.
pub struct SessionConfigProvider {
    db: Database,
    sender: OnceLock<EventSender>,
}

impl SessionConfigProvider {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            sender: OnceLock::new(),
        }
    }

    /// Returns a writer that persists session-config updates and emits
    /// them on this provider's layer. Returns `None` until `load()` has
    /// captured an [`EventSender`] — i.e. until `ConfigHandle::build`
    /// has driven this provider.
    pub fn writer(&self) -> Option<SessionConfigWriter> {
        let sender = self.sender.get()?.clone();
        Some(SessionConfigWriter {
            db: self.db.clone(),
            sender,
        })
    }
}

#[async_trait]
impl ConfigProvider for SessionConfigProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        // Capture before consuming so writers can keep emitting after load.
        let _ = self.sender.set(events.clone());

        let rows = read_rows(&self.db).await.map_err(ProviderError::new)?;

        let mut batch = Vec::with_capacity(rows.len());
        for row in rows {
            match row.and_then(RawRow::into_event) {
                Ok(event) => batch.push(event),
                Err(error) => warn!(%error, "skipping malformed session_config row"),
            }
        }

        debug!(count = batch.len(), "session_config provider loaded");
        if !batch.is_empty() && events.send(batch).await.is_err() {
            // Receiver gone; nothing useful to do.
        }
        Ok(())
    }
}

/// Cloneable handle for persisting session-config updates.
///
/// `write` runs the SQL first; if it succeeds, the batch is emitted on
/// the provider's layer. If the channel is gone (handle dropped) the
/// emit is silently ignored — the row is in the DB and a future reload
/// would pick it up.
#[derive(Clone)]
pub struct SessionConfigWriter {
    db: Database,
    sender: EventSender,
}

impl SessionConfigWriter {
    /// Persist `events` to `session_config` and emit them on the DB layer.
    pub async fn write(&self, events: Vec<ConfigEvent>) -> Result<(), SessionConfigWriteError> {
        write_rows(&self.db, &events).await?;
        let _ = self.sender.send(events).await;
        Ok(())
    }
}

struct RawRow {
    path: Path,
    kind: String,
    value: String,
}

impl RawRow {
    fn into_event(self) -> Result<ConfigEvent, SessionConfigRowError> {
        let display = self.path.to_string();
        let value = match self.kind.as_str() {
            "string" => Value::String(self.value.into()),
            "int" => Value::Int(self.value.parse::<i64>().map_err(|_| {
                SessionConfigRowError::BadValue {
                    path: display.clone(),
                    kind: self.kind.clone(),
                    value: self.value.clone(),
                }
            })?),
            "float" => Value::Float(self.value.parse::<f64>().map_err(|_| {
                SessionConfigRowError::BadValue {
                    path: display.clone(),
                    kind: self.kind.clone(),
                    value: self.value.clone(),
                }
            })?),
            "bool" => match self.value.as_str() {
                "true" | "1" => Value::Bool(true),
                "false" | "0" => Value::Bool(false),
                _ => {
                    return Err(SessionConfigRowError::BadValue {
                        path: display,
                        kind: self.kind.clone(),
                        value: self.value.clone(),
                    });
                }
            },
            other => {
                return Err(SessionConfigRowError::UnknownKind {
                    path: display,
                    kind: other.to_owned(),
                });
            }
        };
        Ok(ConfigEvent::new(self.path, value))
    }
}

#[derive(Debug, Error)]
enum SessionConfigRowError {
    #[error("session_config row at {path}: kind '{kind}' value '{value}' is not a valid {kind}")]
    BadValue {
        path: String,
        kind: String,
        value: String,
    },
    #[error("session_config row at {path}: unknown kind '{kind}' (expected string|int|float|bool)")]
    UnknownKind { path: String, kind: String },
    #[error("session_config row: malformed path JSON: {0}")]
    MalformedPath(String),
}

#[derive(Debug, Error)]
pub enum SessionConfigLoadError {
    #[error("session_config load: {0}")]
    Turso(#[from] turso::Error),
}

#[derive(Debug, Error)]
pub enum SessionConfigWriteError {
    #[error("session_config write: {0}")]
    Turso(#[from] turso::Error),
    #[error(
        "session_config write at {path}: {kind} values are not supported (only string|int|float|bool)"
    )]
    UnsupportedKind { path: String, kind: &'static str },
}

async fn read_rows(
    db: &Database,
) -> Result<Vec<Result<RawRow, SessionConfigRowError>>, SessionConfigLoadError> {
    let conn = db.connect().await;
    let mut rows = conn
        .query("SELECT json(path), kind, value FROM session_config", ())
        .await?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let path_json = row.get::<String>(0)?;
        let kind = row.get::<String>(1)?;
        let value = row.get::<String>(2)?;
        out.push(parse_path_json(&path_json).map(|path| RawRow { path, kind, value }));
    }
    Ok(out)
}

async fn write_rows(db: &Database, events: &[ConfigEvent]) -> Result<(), SessionConfigWriteError> {
    let conn = db.connect().await;
    for event in events {
        let hash = hash_path(&event.path);
        match value_row(&event.value) {
            Some((kind, value)) => {
                let path_json = path_to_json(&event.path).to_string();
                conn.execute(
                    "INSERT INTO session_config(path_hash, path, kind, value) \
                     VALUES(?1, jsonb(?2), ?3, ?4) \
                     ON CONFLICT(path_hash) DO UPDATE SET \
                         path = excluded.path, \
                         kind = excluded.kind, \
                         value = excluded.value",
                    (hash, path_json, kind.to_string(), value),
                )
                .await?;
            }
            None if event.value.is_null() => {
                conn.execute("DELETE FROM session_config WHERE path_hash = ?1", (hash,))
                    .await?;
            }
            None => {
                return Err(SessionConfigWriteError::UnsupportedKind {
                    path: event.path.to_string(),
                    kind: variant_name(&event.value),
                });
            }
        }
    }
    Ok(())
}

/// Stable 64-bit hash of a [`Path`] for use as the `session_config` primary
/// key. Lowercased so case-insensitive equality (per [`Value::String`]) maps
/// to the same row. Stored as `i64` via bit-cast.
fn hash_path(path: &Path) -> i64 {
    let canonical = path.to_string().to_ascii_lowercase();
    XxHash3_64::oneshot(canonical.as_bytes()) as i64
}

/// Encode a [`Path`] as a JSON array of segments — strings stay strings,
/// integer segments become JSON numbers. Round-trips with [`parse_path_json`].
fn path_to_json(path: &Path) -> serde_json::Value {
    let segments: Vec<serde_json::Value> = path
        .iter()
        .map(|seg| match seg {
            Value::String(s) => serde_json::Value::String(s.to_string()),
            Value::Int(i) => serde_json::Value::Number((*i).into()),
            // Path::parse only ever produces String or Int segments, so any
            // other variant means a caller built a Path by hand with a
            // shape we don't support persisting.
            other => serde_json::Value::String(other.to_string()),
        })
        .collect();
    serde_json::Value::Array(segments)
}

fn parse_path_json(text: &str) -> Result<Path, SessionConfigRowError> {
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| SessionConfigRowError::MalformedPath(err.to_string()))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| SessionConfigRowError::MalformedPath("path is not a JSON array".into()))?;
    let mut segments: Vec<Value> = Vec::with_capacity(arr.len());
    for seg in arr {
        match seg {
            serde_json::Value::String(s) => segments.push(Value::String(Arc::from(s.as_str()))),
            serde_json::Value::Number(n) => {
                let i = n.as_i64().ok_or_else(|| {
                    SessionConfigRowError::MalformedPath(format!("non-integer numeric segment {n}"))
                })?;
                segments.push(Value::Int(i));
            }
            other => {
                return Err(SessionConfigRowError::MalformedPath(format!(
                    "unsupported segment {other}"
                )));
            }
        }
    }
    Ok(Path::from(segments))
}

/// Inverse of `RawRow::into_event` for the supported scalar kinds. Returns
/// `None` for `Value::Null` (the caller deletes the row) and for any
/// variant the schema does not support.
fn value_row(value: &Value) -> Option<(&'static str, String)> {
    match value {
        Value::String(s) => Some(("string", s.to_string())),
        Value::Int(i) => Some(("int", i.to_string())),
        Value::Float(f) => Some(("float", f.to_string())),
        Value::Bool(b) => Some(("bool", if *b { "true" } else { "false" }.to_owned())),
        Value::Null => None,
    }
}

fn variant_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::String(_) => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_config::ConfigHandle;
    use std::sync::Arc;

    async fn count_rows(db: &Database, path: &str) -> i64 {
        let conn = db.connect().await;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM session_config WHERE path_hash = ?1",
                (hash_path(&Path::parse(path)),),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get::<i64>(0).unwrap()
    }

    #[tokio::test]
    async fn writer_persists_and_emits() {
        let db = crate::store::open_in_memory().await.unwrap();

        let provider = Arc::new(SessionConfigProvider::new(db.clone()));
        let providers: Vec<Arc<dyn ConfigProvider>> = vec![provider.clone()];
        let handle = ConfigHandle::build(providers).await.unwrap();
        let writer = provider.writer().expect("load ran during build");

        writer
            .write(vec![ConfigEvent::new(
                Path::parse("llm::model"),
                Value::String("qwen".into()),
            )])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert_eq!(
            handle.snapshot().get("llm::model").value(),
            Some(&Value::String("qwen".into()))
        );
        assert_eq!(count_rows(&db, "llm::model").await, 1);

        writer
            .write(vec![ConfigEvent::unset(Path::parse("llm::model"))])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert!(handle.snapshot().get("llm::model").value().is_none());
        assert_eq!(count_rows(&db, "llm::model").await, 0);
    }

    #[test]
    fn hash_is_case_insensitive() {
        // Mirrors Value::String's case-insensitive Eq: differently-cased
        // paths must hash to the same row so writes upsert instead of
        // duplicating.
        assert_eq!(
            hash_path(&Path::parse("App::Name")),
            hash_path(&Path::parse("app::name"))
        );
        assert_eq!(
            hash_path(&Path::parse("LLM::Model")),
            hash_path(&Path::parse("llm::model"))
        );
    }

    #[test]
    fn path_json_round_trips() {
        let p = Path::parse("foo::42::bar");
        let json = path_to_json(&p).to_string();
        let parsed = parse_path_json(&json).unwrap();
        assert_eq!(parsed.to_string(), "foo::42::bar");
    }
}
