//! Per-entity SQL migration system.
//!
//! Each subsystem ("thing") that owns tables — built-in tools, history,
//! session config, future workflows — declares an [`EntitySchema`]: a
//! stable [`Uuid`] plus an ordered slice of [`Migration`]s, each with a
//! human-readable name and a chunk of SQL.
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
//! without coordinating version numbers. (sqlx wasn't an option — it
//! doesn't talk to turso directly.)

use std::hash::Hasher;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use turso::{Connection, Value};
use twox_hash::XxHash3_64;
use uuid::Uuid;

/// One forward-only migration. `name` is the filename (or any stable
/// label) and is shown in error messages; `sql` is its body.
pub struct Migration {
    pub name: &'static str,
    pub sql: &'static str,
}

/// Migrations owned by one subsystem, identified by a stable
/// [`Uuid`]. Generate the UUID once (any v4 will do) and treat it as
/// part of the public API of the subsystem — changing it orphans the
/// existing tables.
pub struct EntitySchema {
    pub entity: Uuid,
    pub migrations: &'static [Migration],
}

/// xxh3-64 of the migration body, stored as a signed i64 in the
/// `checksum` column. Chosen because it's already a workspace dep and
/// stable across rust toolchains; this is integrity, not crypto.
fn checksum(sql: &str) -> i64 {
    let mut h = XxHash3_64::new();
    h.write(sql.as_bytes());
    h.finish() as i64
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
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
    .await
    .context("create _migrations table")?;
    Ok(())
}

struct AppliedRow {
    version: i64,
    name: String,
    checksum: i64,
}

async fn load_applied(conn: &Connection, entity: &[u8]) -> Result<Vec<AppliedRow>> {
    let mut rows = conn
        .query(
            "SELECT version, name, checksum FROM _migrations WHERE entity = ?1 ORDER BY version",
            (entity.to_vec(),),
        )
        .await
        .context("query _migrations")?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.context("iterate _migrations")? {
        let version = match row.get_value(0).context("version column")? {
            Value::Integer(v) => v,
            other => bail!("expected integer version, got {other:?}"),
        };
        let name = match row.get_value(1).context("name column")? {
            Value::Text(t) => t,
            other => bail!("expected text name, got {other:?}"),
        };
        let checksum = match row.get_value(2).context("checksum column")? {
            Value::Integer(v) => v,
            other => bail!("expected integer checksum, got {other:?}"),
        };
        out.push(AppliedRow {
            version,
            name,
            checksum,
        });
    }
    Ok(out)
}

/// Apply [`schema`] to `conn`. Caller must have already invoked
/// [`ensure_table`].
///
/// Rejects the load (returns an error without applying anything) if
/// the recorded migrations diverge from the declared prefix in name,
/// checksum, or version. Otherwise applies declared migrations from
/// `applied.len()..` each inside its own transaction along with the
/// matching `_migrations` insert.
pub async fn run(conn: &Connection, schema: &EntitySchema) -> Result<()> {
    let entity_bytes = schema.entity.as_bytes().to_vec();
    let applied = load_applied(conn, &entity_bytes).await?;

    if applied.len() > schema.migrations.len() {
        bail!(
            "entity {}: database has {} migration(s) recorded but only {} declared in code; refusing to load",
            schema.entity,
            applied.len(),
            schema.migrations.len(),
        );
    }

    for (i, row) in applied.iter().enumerate() {
        let declared = &schema.migrations[i];
        let expected_version = i as i64;
        if row.version != expected_version {
            bail!(
                "entity {}: migration at index {i} has version {} on disk, expected {expected_version}",
                schema.entity,
                row.version,
            );
        }
        if row.name != declared.name {
            bail!(
                "entity {}: migration {i} renamed: applied {:?}, declared {:?}",
                schema.entity,
                row.name,
                declared.name,
            );
        }
        let declared_sum = checksum(declared.sql);
        if row.checksum != declared_sum {
            bail!(
                "entity {}: migration {i} ({}) checksum mismatch: applied {:#018x}, declared {:#018x}",
                schema.entity,
                declared.name,
                row.checksum as u64,
                declared_sum as u64,
            );
        }
    }

    for (i, m) in schema.migrations.iter().enumerate().skip(applied.len()) {
        let tx = conn
            .unchecked_transaction()
            .await
            .context("begin migration tx")?;
        tx.execute_batch(m.sql)
            .await
            .with_context(|| format!("entity {}: migration {} ({})", schema.entity, i, m.name))?;
        tx.execute(
            "INSERT INTO _migrations (entity, version, name, checksum, applied_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                entity_bytes.clone(),
                i as i64,
                m.name.to_string(),
                checksum(m.sql),
                now_ns(),
            ),
        )
        .await
        .with_context(|| {
            format!(
                "entity {}: record migration {} ({})",
                schema.entity, i, m.name
            )
        })?;
        tx.commit().await.with_context(|| {
            format!(
                "entity {}: commit migration {} ({})",
                schema.entity, i, m.name
            )
        })?;
    }

    Ok(())
}

/// Convenience: ensure the tracking table and run every schema in
/// order. Bails on the first failing entity.
pub async fn run_all(conn: &Connection, schemas: &[&EntitySchema]) -> Result<()> {
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

    fn schema_v1() -> EntitySchema {
        static MIGS: &[Migration] = &[Migration {
            name: "0001_init.sql",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        }];
        EntitySchema {
            entity: TEST_ENTITY,
            migrations: MIGS,
        }
    }

    fn schema_v2() -> EntitySchema {
        static MIGS: &[Migration] = &[
            Migration {
                name: "0001_init.sql",
                sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
            },
            Migration {
                name: "0002_color.sql",
                sql: "ALTER TABLE widgets ADD COLUMN color TEXT;",
            },
        ];
        EntitySchema {
            entity: TEST_ENTITY,
            migrations: MIGS,
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

        static RENAMED: &[Migration] = &[Migration {
            name: "0001_renamed.sql",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        }];
        let renamed = EntitySchema {
            entity: TEST_ENTITY,
            migrations: RENAMED,
        };
        let err = run(&conn, &renamed).await.unwrap_err();
        assert!(err.to_string().contains("renamed"), "{err:#}");
    }

    #[tokio::test]
    async fn rejects_edited_migration() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v1()).await.unwrap();

        static EDITED: &[Migration] = &[Migration {
            name: "0001_init.sql",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT, color TEXT);",
        }];
        let edited = EntitySchema {
            entity: TEST_ENTITY,
            migrations: EDITED,
        };
        let err = run(&conn, &edited).await.unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err:#}");
    }

    #[tokio::test]
    async fn rejects_shrunk_declaration() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();
        run(&conn, &schema_v2()).await.unwrap();

        let err = run(&conn, &schema_v1()).await.unwrap_err();
        assert!(err.to_string().contains("refusing to load"), "{err:#}");
    }

    #[tokio::test]
    async fn failing_migration_rolls_back_record() {
        let conn = fresh_conn().await;
        ensure_table(&conn).await.unwrap();

        static BAD: &[Migration] = &[Migration {
            name: "0001_bad.sql",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY); NOT VALID SQL;",
        }];
        let bad = EntitySchema {
            entity: TEST_ENTITY,
            migrations: BAD,
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
        static OTHER_MIGS: &[Migration] = &[Migration {
            name: "0001_other.sql",
            sql: "CREATE TABLE gadgets (id INTEGER PRIMARY KEY);",
        }];
        let other = EntitySchema {
            entity: OTHER_ENTITY,
            migrations: OTHER_MIGS,
        };
        run(&conn, &other).await.unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM _migrations", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(matches!(row.get_value(0).unwrap(), Value::Integer(2)));
    }
}
