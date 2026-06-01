//! Per-entity SQL migration system, plus the shared [`Database`]
//! handle that owns the per-session [`turso::Connection`].
//!
//! ## Database
//!
//! [`Database`] wraps the underlying connection in an
//! [`AsyncMutex`](tokio::sync::Mutex) — turso's `Connection` returns
//! `Misuse("concurrent use forbidden")` if it sees overlapping calls
//! from cloned handles, so every caller in the session runtime, the workflow
//! runtime, the LLM history store, and so on goes through
//! [`Database::connect`] to acquire an [`ActiveDatabase`] guard. The
//! guard dereferences to `&Connection` and releases the lock on drop
//! (after a best-effort `cacheflush`). No raw `Connection` clones leave
//! the type — that's the invariant.
//!
//! ## Migrations
//!
//! Each subsystem ("thing") that owns tables — built-in tools, history,
//! session config, workflows loaded off disk — declares an
//! [`EntitySchema`]: a stable [`Uuid`] plus an ordered list of
//! [`Migration`]s, each with a human-readable name and a chunk of SQL.
//!
//! [`run`] enforces a single, strict invariant: the migrations already
//! recorded in `_migrations` for an entity must match the declared
//! prefix exactly — same name, same checksum, same order. Any drift
//! (renamed file, edited SQL, reordered, declared shrunk below
//! deployed) fails the load. There are no down migrations: forward
//! only. Each new migration's SQL and its `_migrations` row are
//! committed in a single transaction, so a partially-applied
//! migration can never be recorded as done.
//!
//! Modeled after sqlx's `_sqlx_migrations`, but multi-tenant via the
//! `entity` column so independent subsystems can evolve their schemas
//! without coordinating version numbers.

use std::borrow::Cow;
use std::hash::Hasher;
use std::ops::Deref;
use std::sync::Arc;

use frances_core::now_ns;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use turso::{Builder, Connection, Row, Value};
use twox_hash::XxHash3_64;
use uuid::Uuid;

/// Per-session SQL handle. Cheap to clone — the underlying
/// [`turso::Connection`] sits behind an [`AsyncMutex`] inside an [`Arc`],
/// so every clone shares the same lock.
///
/// All access goes through [`Database::connect`], which yields an
/// [`ActiveDatabase`] holding the lock for the duration of the
/// operation. The raw `Connection` is private to this type.
#[derive(Clone)]
pub struct Database {
    conn: Arc<AsyncMutex<Connection>>,
    path: Arc<str>,
}

impl Database {
    /// Open (or create) a turso database at `path`. Use `":memory:"`
    /// for an ephemeral instance.
    pub async fn open(path: impl Into<Arc<str>>) -> std::result::Result<Self, turso::Error> {
        let path: Arc<str> = path.into();
        let database = Builder::new_local(&path).build().await?;
        let conn = database.connect()?;
        Ok(Self {
            conn: Arc::new(AsyncMutex::new(conn)),
            path,
        })
    }

    /// Shortcut for [`Database::open`] with `":memory:"`. Useful in tests.
    pub async fn open_in_memory() -> std::result::Result<Self, turso::Error> {
        Self::open(":memory:").await
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Acquire the connection lock. The returned [`ActiveDatabase`]
    /// dereferences to `&Connection` and releases the lock (after a
    /// best-effort `cacheflush`) when dropped. Holding the guard across
    /// `await` points is the supported pattern.
    pub async fn connect(&self) -> ActiveDatabase {
        ActiveDatabase {
            guard: self.conn.clone().lock_owned().await,
        }
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &&*self.path)
            .finish()
    }
}

/// RAII guard returned by [`Database::connect`]. Holds the connection
/// mutex; drop releases it. Dereferences to `&Connection`.
pub struct ActiveDatabase {
    guard: OwnedMutexGuard<Connection>,
}

impl Deref for ActiveDatabase {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.guard
    }
}

impl Drop for ActiveDatabase {
    fn drop(&mut self) {
        if let Err(error) = self.guard.cacheflush() {
            tracing::warn!(%error, "cacheflush failed");
        }
    }
}

/// One forward-only migration. `name` is the filename (or any stable
/// label) and is shown in error messages; `sql` is its body.
///
/// Both fields are [`Cow<'static, str>`] so session-runtime-side schemas can
/// stay zero-copy on `include_str!` constants while workflow code
/// constructs migrations from bytes read at runtime.
#[derive(Clone)]
pub struct Migration {
    pub name: Cow<'static, str>,
    pub sql: Cow<'static, str>,
}

/// Migrations owned by one subsystem, identified by a stable
/// [`Uuid`]. Generate the UUID once (any v4 will do) and treat it as
/// part of the public API of the subsystem — changing it orphans the
/// existing tables.
///
/// `migrations` is a [`Cow`] so the runtime's static schemas can stay
/// const-constructible (`Cow::Borrowed(&'static [..])`, instantiated
/// as `EntitySchema<'static>`) while workflow code loaded at runtime
/// can hand in either an owned [`Vec`] or a borrowed slice with a
/// shorter lifetime.
pub struct EntitySchema<'a> {
    pub entity: Uuid,
    pub migrations: Cow<'a, [Migration]>,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("turso: {0}")]
    Turso(#[from] turso::Error),
    #[error(
        "entity {entity}: database has {applied} migration(s) recorded but only {declared} declared in code; refusing to load"
    )]
    DeclaredShrunk {
        entity: Uuid,
        applied: usize,
        declared: usize,
    },
    #[error(
        "entity {entity}: migration at index {index} has version {found} on disk, expected {expected}"
    )]
    VersionMismatch {
        entity: Uuid,
        index: usize,
        expected: i64,
        found: i64,
    },
    #[error(
        "entity {entity}: migration {index} renamed: applied {applied:?}, declared {declared:?}"
    )]
    Renamed {
        entity: Uuid,
        index: usize,
        applied: String,
        declared: String,
    },
    #[error(
        "entity {entity}: migration {index} ({name}) checksum mismatch: applied {applied:#018x}, declared {declared:#018x}"
    )]
    ChecksumMismatch {
        entity: Uuid,
        index: usize,
        name: String,
        applied: u64,
        declared: u64,
    },
    #[error("expected integer {column}, got {found:?}")]
    NonIntegerColumn { column: &'static str, found: Value },
    #[error("expected text {column}, got {found:?}")]
    NonTextColumn { column: &'static str, found: Value },
}

pub type Result<T> = std::result::Result<T, MigrationError>;

/// xxh3-64 of the migration body, stored as a signed i64 in the
/// `checksum` column. Integrity check, not cryptographic.
fn checksum(sql: &str) -> i64 {
    let mut h = XxHash3_64::new();
    h.write(sql.as_bytes());
    h.finish() as i64
}

/// Create the tracking table if it doesn't already exist. Idempotent.
pub async fn ensure_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS _migrations (
            entity     BLOB    NOT NULL,
            version    INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            checksum   INTEGER NOT NULL,
            applied_at INTEGER NOT NULL,
            PRIMARY KEY (entity, version)
        );
        "#,
    )
    .await?;
    Ok(())
}

struct AppliedRow {
    version: i64,
    name: String,
    checksum: i64,
}

fn expect_i64(row: &Row, idx: usize, column: &'static str) -> Result<i64> {
    match row.get_value(idx)? {
        Value::Integer(v) => Ok(v),
        found => Err(MigrationError::NonIntegerColumn { column, found }),
    }
}

fn expect_text(row: &Row, idx: usize, column: &'static str) -> Result<String> {
    match row.get_value(idx)? {
        Value::Text(t) => Ok(t),
        found => Err(MigrationError::NonTextColumn { column, found }),
    }
}

async fn load_applied(conn: &Connection, entity: &[u8]) -> Result<Vec<AppliedRow>> {
    let mut rows = conn
        .query(
            "SELECT version, name, checksum FROM _migrations WHERE entity = ?1 ORDER BY version",
            (entity.to_vec(),),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(AppliedRow {
            version: expect_i64(&row, 0, "version")?,
            name: expect_text(&row, 1, "name")?,
            checksum: expect_i64(&row, 2, "checksum")?,
        });
    }
    Ok(out)
}

/// Apply `schema` to `conn`. Caller must have already invoked
/// [`ensure_table`].
///
/// Rejects the load (returns an error without applying anything) if
/// the recorded migrations diverge from the declared prefix in name,
/// checksum, or version. Otherwise applies declared migrations from
/// `applied.len()..` each inside its own transaction along with the
/// matching `_migrations` insert.
pub async fn run(conn: &Connection, schema: &EntitySchema<'_>) -> Result<()> {
    let entity_bytes = schema.entity.as_bytes().to_vec();
    let applied = load_applied(conn, &entity_bytes).await?;

    if applied.len() > schema.migrations.len() {
        return Err(MigrationError::DeclaredShrunk {
            entity: schema.entity,
            applied: applied.len(),
            declared: schema.migrations.len(),
        });
    }

    for (i, row) in applied.iter().enumerate() {
        let declared = &schema.migrations[i];
        let expected_version = i as i64;
        if row.version != expected_version {
            return Err(MigrationError::VersionMismatch {
                entity: schema.entity,
                index: i,
                expected: expected_version,
                found: row.version,
            });
        }
        if row.name != declared.name.as_ref() {
            return Err(MigrationError::Renamed {
                entity: schema.entity,
                index: i,
                applied: row.name.clone(),
                declared: declared.name.to_string(),
            });
        }
        let declared_sum = checksum(&declared.sql);
        if row.checksum != declared_sum {
            return Err(MigrationError::ChecksumMismatch {
                entity: schema.entity,
                index: i,
                name: declared.name.to_string(),
                applied: row.checksum as u64,
                declared: declared_sum as u64,
            });
        }
    }

    for (i, m) in schema.migrations.iter().enumerate().skip(applied.len()) {
        let tx = conn.unchecked_transaction().await?;
        tx.execute_batch(&m.sql).await?;
        tx.execute(
            "INSERT INTO _migrations (entity, version, name, checksum, applied_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                entity_bytes.clone(),
                i as i64,
                m.name.to_string(),
                checksum(&m.sql),
                now_ns(),
            ),
        )
        .await?;
        tx.commit().await?;
    }

    Ok(())
}

/// Convenience: ensure the tracking table and run every schema in
/// order. Bails on the first failing entity.
pub async fn run_all(conn: &Connection, schemas: &[&EntitySchema<'_>]) -> Result<()> {
    ensure_table(conn).await?;
    for schema in schemas {
        run(conn, schema).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso::Builder;

    async fn fresh_conn() -> Connection {
        Builder::new_local(":memory:")
            .build()
            .await
            .unwrap()
            .connect()
            .unwrap()
    }

    const TEST_ENTITY: Uuid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);

    fn mig(name: &'static str, sql: &'static str) -> Migration {
        Migration {
            name: Cow::Borrowed(name),
            sql: Cow::Borrowed(sql),
        }
    }

    fn schema_v1() -> EntitySchema<'static> {
        EntitySchema {
            entity: TEST_ENTITY,
            migrations: vec![mig(
                "0001_init.sql",
                "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
            )]
            .into(),
        }
    }

    fn schema_v2() -> EntitySchema<'static> {
        EntitySchema {
            entity: TEST_ENTITY,
            migrations: vec![
                mig(
                    "0001_init.sql",
                    "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
                ),
                mig(
                    "0002_color.sql",
                    "ALTER TABLE widgets ADD COLUMN color TEXT;",
                ),
            ]
            .into(),
        }
    }

    #[tokio::test]
    async fn fresh_apply_creates_tables_and_records() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();

        conn.execute("INSERT INTO widgets (id, name) VALUES (1, 'a')", ())
            .await
            .unwrap();

        let mut rows = conn
            .query("SELECT version, name FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("one row");
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(0)));
        let Value::Text(name) = row.get_value(1).unwrap() else {
            panic!("name");
        };
        assert_eq!(name, "0001_init.sql");
        assert!(rows.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn second_run_is_noop() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(1)));
    }

    #[tokio::test]
    async fn extends_with_new_migration() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();
        run(&conn, &schema_v2()).await.unwrap();

        conn.execute(
            "INSERT INTO widgets (id, name, color) VALUES (1, 'a', 'red')",
            (),
        )
        .await
        .unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(2)));
    }

    #[tokio::test]
    async fn rejects_renamed_migration() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();

        let renamed = EntitySchema {
            entity: TEST_ENTITY,
            migrations: vec![mig(
                "0001_renamed.sql",
                "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
            )]
            .into(),
        };
        let err = run(&conn, &renamed).await.unwrap_err();
        assert!(matches!(err, MigrationError::Renamed { .. }), "{err:#}");
    }

    #[tokio::test]
    async fn rejects_edited_migration() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();

        let edited = EntitySchema {
            entity: TEST_ENTITY,
            migrations: vec![mig(
                "0001_init.sql",
                "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT, color TEXT);",
            )]
            .into(),
        };
        let err = run(&conn, &edited).await.unwrap_err();
        assert!(
            matches!(err, MigrationError::ChecksumMismatch { .. }),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn rejects_shrunk_declaration() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v2()).await.unwrap();

        let err = run(&conn, &schema_v1()).await.unwrap_err();
        assert!(
            matches!(err, MigrationError::DeclaredShrunk { .. }),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn failing_migration_rolls_back_record() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();

        let bad = EntitySchema {
            entity: TEST_ENTITY,
            migrations: vec![mig(
                "0001_bad.sql",
                "CREATE TABLE widgets (id INTEGER PRIMARY KEY); NOT VALID SQL;",
            )]
            .into(),
        };
        let _ = run(&conn, &bad).await;

        // _migrations row must NOT have been recorded if the SQL didn't
        // run cleanly — the executor commits both atomically.
        let mut rows = conn
            .query("SELECT COUNT(*) FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(0)));
    }

    #[tokio::test]
    async fn separate_entities_dont_clash() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();

        const OTHER_ENTITY: Uuid = Uuid::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef);
        let other = EntitySchema {
            entity: OTHER_ENTITY,
            migrations: vec![mig(
                "0001_other.sql",
                "CREATE TABLE gadgets (id INTEGER PRIMARY KEY);",
            )]
            .into(),
        };
        run(&conn, &other).await.unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(2)));
    }

    /// Workflow migrations are built from bytes loaded at runtime —
    /// proves an `EntitySchema` made entirely of owned `String`s
    /// applies identically to the borrowed session-runtime-side flavor.
    #[tokio::test]
    async fn owned_schema_applies() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();

        let schema = EntitySchema {
            entity: TEST_ENTITY,
            migrations: Cow::Owned(vec![Migration {
                name: Cow::Owned(String::from("0001_init.sql")),
                sql: Cow::Owned(String::from(
                    "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
                )),
            }]),
        };

        run(&conn, &schema).await.unwrap();
        // Re-run the same schema (still owned) — must be a clean no-op,
        // proving checksums match across the two owned constructions.
        run(&conn, &schema).await.unwrap();

        conn.execute("INSERT INTO widgets (id, name) VALUES (1, 'a')", ())
            .await
            .unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(1)));
    }
}
