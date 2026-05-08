//! DB-backed config provider — overrides per session.
//!
//! Reads from a `session_config` table on the per-session turso DB and
//! emits one [`ConfigEvent`] per row. Lives in `frances` (not in
//! `frances-config`) so the config crate stays independent of libsql.
//!
//! Schema is read-only this pass. Edit by raw SQL during testing; a
//! write API can be added later without changing the read path.

use anyhow::Context;
use async_trait::async_trait;
use frances_config::{ConfigEvent, ConfigProvider, EventSender, Path, ProviderError, Value};
use thiserror::Error;
use tracing::{debug, warn};

use crate::store::Database;

/// Reads `(path, kind, value)` rows from `session_config` on the
/// per-session DB and emits them as [`ConfigEvent`]s.
pub struct SessionConfigProvider {
    db: Database,
}

impl SessionConfigProvider {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ConfigProvider for SessionConfigProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        let rows = read_rows(&self.db)
            .await
            .map_err(|err| ProviderError::new(SessionConfigLoadError::from_anyhow(err)))?;

        let mut batch = Vec::with_capacity(rows.len());
        for row in rows {
            match row.into_event() {
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

struct RawRow {
    path: String,
    kind: String,
    value: String,
}

impl RawRow {
    fn into_event(self) -> Result<ConfigEvent, SessionConfigRowError> {
        let value = match self.kind.as_str() {
            "string" => Value::String(self.value.into()),
            "int" => Value::Int(self.value.parse::<i64>().map_err(|_| {
                SessionConfigRowError::BadValue {
                    path: self.path.clone(),
                    kind: self.kind.clone(),
                    value: self.value.clone(),
                }
            })?),
            "float" => Value::Float(self.value.parse::<f64>().map_err(|_| {
                SessionConfigRowError::BadValue {
                    path: self.path.clone(),
                    kind: self.kind.clone(),
                    value: self.value.clone(),
                }
            })?),
            "bool" => match self.value.as_str() {
                "true" | "1" => Value::Bool(true),
                "false" | "0" => Value::Bool(false),
                _ => {
                    return Err(SessionConfigRowError::BadValue {
                        path: self.path.clone(),
                        kind: self.kind.clone(),
                        value: self.value.clone(),
                    });
                }
            },
            other => {
                return Err(SessionConfigRowError::UnknownKind {
                    path: self.path,
                    kind: other.to_owned(),
                });
            }
        };
        Ok(ConfigEvent::new(Path::parse(&self.path), value))
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
}

#[derive(Debug, Error)]
#[error("session_config load failed: {0}")]
struct SessionConfigLoadError(String);

impl SessionConfigLoadError {
    fn from_anyhow(err: anyhow::Error) -> Self {
        Self(format!("{err:#}"))
    }
}

async fn read_rows(db: &Database) -> anyhow::Result<Vec<RawRow>> {
    let conn = db.connect();
    let mut rows = conn
        .query("SELECT path, kind, value FROM session_config", ())
        .await
        .context("query session_config")?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await.context("iterate session_config")? {
        let path = row.get::<String>(0).context("session_config.path column")?;
        let kind = row.get::<String>(1).context("session_config.kind column")?;
        let value = row
            .get::<String>(2)
            .context("session_config.value column")?;
        out.push(RawRow { path, kind, value });
    }
    Ok(out)
}
