//! Per-workflow SQL surface.
//!
//! Each workflow declares migrations under its [`Uuid`] in TOML; the
//! daemon applies them on first touch and hands back a [`WorkflowDb`]
//! handle. Tables live in the per-session turso database alongside the
//! daemon's own; no row-level partitioning — the workflow owns its
//! table names.
//!
//! `WorkflowDb` exposes a minimal async surface: `exec`, `query`,
//! `query_stream`, and `transaction`. Transactions are managed via raw
//! `BEGIN` / `COMMIT` / `ROLLBACK` statements rather than
//! [`turso::Transaction`] (whose `'conn` lifetime would force a
//! self-borrowing wrapper that's painful to ship through JS classes).
//! All workflows in a session share one [`turso::Connection`], so
//! exactly one transaction can be open at a time — `BEGIN` from a
//! second concurrent workflow surfaces a turso error.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use turso::{Connection, Value};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum WorkflowDbError {
    #[error("turso (entity {entity}): {source}")]
    Turso {
        entity: Uuid,
        #[source]
        source: turso::Error,
    },
    #[error(transparent)]
    Migration(#[from] frances_storage::MigrationError),
    #[error("transaction already settled")]
    TxSettled,
}

/// One result row, materialised by column name.
///
/// `columns` is an [`Arc`]'d header shared by every row of a single
/// result set so cloning a `Row` doesn't duplicate the column names.
#[derive(Debug, Clone)]
pub struct Row {
    pub columns: Arc<[String]>,
    pub values: Vec<Value>,
}

/// Outcome of an [`WorkflowDb::exec`] call.
#[derive(Debug, Clone, Copy)]
pub struct ExecResult {
    pub rows_affected: u64,
    pub last_insert_rowid: i64,
}

/// Workflow's window onto the per-session turso connection.
///
/// Cheap to clone — wraps a [`turso::Connection`] (itself an [`Arc`]).
/// Holds the workflow's [`Uuid`] for error reporting only; rows are
/// not partitioned by entity.
#[derive(Clone)]
pub struct WorkflowDb {
    conn: Connection,
    entity: Uuid,
}

impl WorkflowDb {
    pub fn new(conn: Connection, entity: Uuid) -> Self {
        Self { conn, entity }
    }

    pub fn entity(&self) -> Uuid {
        self.entity
    }

    pub async fn exec(&self, sql: &str, params: Vec<Value>) -> Result<ExecResult, WorkflowDbError> {
        let rows_affected = self
            .conn
            .execute(sql, params)
            .await
            .map_err(|e| self.wrap(e))?;
        let last_insert_rowid = self.conn.last_insert_rowid();
        Ok(ExecResult {
            rows_affected,
            last_insert_rowid,
        })
    }

    pub async fn query(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>, WorkflowDbError> {
        let mut stream = self.query_stream(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = stream.next().await? {
            out.push(row);
        }
        Ok(out)
    }

    pub async fn query_stream(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<RowStream, WorkflowDbError> {
        let rows = self
            .conn
            .query(sql, params)
            .await
            .map_err(|e| self.wrap(e))?;
        let columns: Arc<[String]> = rows.column_names().into();
        Ok(RowStream {
            rows,
            columns,
            entity: self.entity,
            tx: None,
        })
    }

    /// Begin a transaction. Subsequent SQL on the returned [`WorkflowTx`]
    /// runs inside it; the caller decides when to settle.
    pub async fn begin(&self) -> Result<WorkflowTx, WorkflowDbError> {
        self.conn
            .execute("BEGIN", ())
            .await
            .map_err(|e| self.wrap(e))?;
        Ok(WorkflowTx {
            conn: self.conn.clone(),
            entity: self.entity,
            inner: Arc::new(AsyncMutex::new(WorkflowTxInner { settled: false })),
        })
    }

    fn wrap(&self, source: turso::Error) -> WorkflowDbError {
        WorkflowDbError::Turso {
            entity: self.entity,
            source,
        }
    }
}

/// Live transaction bound to a single workflow.
///
/// Cloneable — every clone shares the same underlying `settled` flag
/// (`Arc<AsyncMutex<_>>`), so the JS-side `Transaction` proxy and the
/// runtime's default-settle code see the same state.
#[derive(Clone)]
pub struct WorkflowTx {
    conn: Connection,
    entity: Uuid,
    inner: Arc<AsyncMutex<WorkflowTxInner>>,
}

struct WorkflowTxInner {
    settled: bool,
}

impl WorkflowTx {
    pub fn entity(&self) -> Uuid {
        self.entity
    }

    pub async fn exec(&self, sql: &str, params: Vec<Value>) -> Result<ExecResult, WorkflowDbError> {
        let _guard = self.acquire().await?;
        let rows_affected = self
            .conn
            .execute(sql, params)
            .await
            .map_err(|e| self.wrap(e))?;
        let last_insert_rowid = self.conn.last_insert_rowid();
        Ok(ExecResult {
            rows_affected,
            last_insert_rowid,
        })
    }

    pub async fn query(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>, WorkflowDbError> {
        let mut stream = self.query_stream(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = stream.next().await? {
            out.push(row);
        }
        Ok(out)
    }

    pub async fn query_stream(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<RowStream, WorkflowDbError> {
        let _guard = self.acquire().await?;
        let rows = self
            .conn
            .query(sql, params)
            .await
            .map_err(|e| self.wrap(e))?;
        let columns: Arc<[String]> = rows.column_names().into();
        Ok(RowStream {
            rows,
            columns,
            entity: self.entity,
            tx: Some(self.inner.clone()),
        })
    }

    /// Mark the transaction committed. Errors with `TxSettled` if a
    /// previous explicit commit/rollback already ran.
    pub async fn commit(&self) -> Result<(), WorkflowDbError> {
        self.settle(true).await
    }

    pub async fn rollback(&self) -> Result<(), WorkflowDbError> {
        self.settle(false).await
    }

    /// Default settle from the runtime after the user callback returns.
    /// Returns `true` if this call actually ran the COMMIT/ROLLBACK,
    /// `false` if the user code already explicitly settled.
    pub async fn settle_default(&self, success: bool) -> Result<bool, WorkflowDbError> {
        let mut inner = self.inner.lock().await;
        if inner.settled {
            return Ok(false);
        }
        let sql = if success { "COMMIT" } else { "ROLLBACK" };
        self.conn.execute(sql, ()).await.map_err(|e| self.wrap(e))?;
        inner.settled = true;
        Ok(true)
    }

    async fn settle(&self, success: bool) -> Result<(), WorkflowDbError> {
        let mut inner = self.inner.lock().await;
        if inner.settled {
            return Err(WorkflowDbError::TxSettled);
        }
        let sql = if success { "COMMIT" } else { "ROLLBACK" };
        self.conn.execute(sql, ()).await.map_err(|e| self.wrap(e))?;
        inner.settled = true;
        Ok(())
    }

    /// Acquires the inner guard and verifies the tx hasn't been
    /// settled. The guard is held across the SQL call to serialise
    /// exec/query/commit/rollback within the tx — turso processes one
    /// statement at a time on the underlying connection, and this keeps
    /// the settled-check and the SQL atomic from JS's perspective.
    async fn acquire(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, WorkflowTxInner>, WorkflowDbError> {
        let guard = self.inner.lock().await;
        if guard.settled {
            return Err(WorkflowDbError::TxSettled);
        }
        Ok(guard)
    }

    fn wrap(&self, source: turso::Error) -> WorkflowDbError {
        WorkflowDbError::Turso {
            entity: self.entity,
            source,
        }
    }
}

/// Lazy result-set iterator. `next()` pulls one row from turso per call;
/// when bound to a transaction, also rejects further reads after the
/// tx settles.
pub struct RowStream {
    rows: turso::Rows,
    columns: Arc<[String]>,
    entity: Uuid,
    tx: Option<Arc<AsyncMutex<WorkflowTxInner>>>,
}

impl RowStream {
    pub fn columns(&self) -> &Arc<[String]> {
        &self.columns
    }

    pub async fn next(&mut self) -> Result<Option<Row>, WorkflowDbError> {
        if let Some(tx) = &self.tx
            && tx.lock().await.settled
        {
            return Err(WorkflowDbError::TxSettled);
        }
        let Some(row) = self.rows.next().await.map_err(|e| WorkflowDbError::Turso {
            entity: self.entity,
            source: e,
        })?
        else {
            return Ok(None);
        };
        let mut values = Vec::with_capacity(self.columns.len());
        for i in 0..self.columns.len() {
            values.push(row.get_value(i).map_err(|e| WorkflowDbError::Turso {
                entity: self.entity,
                source: e,
            })?);
        }
        Ok(Some(Row {
            columns: self.columns.clone(),
            values,
        }))
    }
}
