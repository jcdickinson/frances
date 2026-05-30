//! `frances:v1/storage` — per-workflow SQL surface.
//!
//! Layout mirrors `chat.rs` + `chat.js`. Rust exposes thin primitives
//! on two classes:
//!
//! - `Db` (singleton instance on the stash):
//!   `_exec(sql, params)` → `{ rowsAffected, lastInsertRowid }`
//!   `_query(sql, params)` → `Row[]`
//!   `_innerQueryStream(sql, params)` → raw async-iterable rows
//!   `_beginTransaction()` → `Tx` instance
//!
//! - `Tx` (returned by `db._beginTransaction()`):
//!   same row-returning primitives as `Db`, plus
//!   `_commit()`, `_rollback()`, `_settleDefault(success: bool)`.
//!
//! The JS module in `js/storage.js` wraps these in the user-facing
//! `db.exec/query/queryStream/transaction` shape (WHATWG
//! `ReadableStream` + `AbortSignal` for `queryStream`; callback
//! orchestration for `transaction`).

use std::sync::Arc;

use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Array, Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, TypedArray, Value,
};
use tokio::sync::Mutex as AsyncMutex;
use turso::Value as TursoValue;

use super::throw_js as throw;
use crate::storage::{ExecResult, Row, RowStream, WorkflowDb, WorkflowDbError, WorkflowTx};

/// Stamp a JS async method that clones one field off `this`, runs an async DB
/// call, and wraps the result in a typed `IntoJs` newtype. Collapses the
/// otherwise byte-identical `_exec`/`_query`/… towers on `JsDb` and `JsTx`.
/// Three shapes: `(sql, params)`, no JS args (`method()`), and one `bool` arg.
macro_rules! db_method {
    ($proto:expr, $ctx:expr, $name:literal, $class:ty, $field:ident.$method:ident => $wrap:ident) => {
        $proto.set(
            $name,
            Function::new(
                $ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, $class>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let conn = this.0.borrow().$field.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        $wrap(conn.$method(&sql, params).await)
                    }))
                },
            )?,
        )?;
    };
    ($proto:expr, $ctx:expr, $name:literal, $class:ty, $field:ident.$method:ident() => $wrap:ident) => {
        $proto.set(
            $name,
            Function::new($ctx.clone(), |this: This<Class<'js, $class>>| {
                let conn = this.0.borrow().$field.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move { $wrap(conn.$method().await) }))
            })?,
        )?;
    };
    ($proto:expr, $ctx:expr, $name:literal, $class:ty, $field:ident.$method:ident(bool) => $wrap:ident) => {
        $proto.set(
            $name,
            Function::new(
                $ctx.clone(),
                |this: This<Class<'js, $class>>, flag: bool| {
                    let conn = this.0.borrow().$field.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        $wrap(conn.$method(flag).await)
                    }))
                },
            )?,
        )?;
    };
}

/// Construct the singleton `Db` class instance + the private
/// `_innerQueryStream` function for the stash.
pub(crate) fn build_storage<'js>(
    ctx: &Ctx<'js>,
    db: Arc<WorkflowDb>,
) -> JsResult<Class<'js, JsDb>> {
    Class::instance(ctx.clone(), JsDb { db })
}

pub struct JsDb {
    db: Arc<WorkflowDb>,
}

impl<'js> Trace<'js> for JsDb {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for JsDb {
    type Changed<'to> = JsDb;
}

impl<'js> JsClass<'js> for JsDb {
    const NAME: &'static str = "Db";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        db_method!(proto, ctx, "_exec", JsDb, db.exec => WorkflowDbExec);
        db_method!(proto, ctx, "_query", JsDb, db.query => WorkflowDbQuery);
        db_method!(proto, ctx, "_innerQueryStream", JsDb, db.query_stream => WorkflowDbStream);
        db_method!(proto, ctx, "_beginTransaction", JsDb, db.begin() => WorkflowDbBegin);
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

pub struct JsTx {
    tx: WorkflowTx,
}

impl<'js> Trace<'js> for JsTx {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for JsTx {
    type Changed<'to> = JsTx;
}

impl<'js> JsClass<'js> for JsTx {
    const NAME: &'static str = "Tx";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        db_method!(proto, ctx, "_exec", JsTx, tx.exec => WorkflowDbExec);
        db_method!(proto, ctx, "_query", JsTx, tx.query => WorkflowDbQuery);
        db_method!(proto, ctx, "_innerQueryStream", JsTx, tx.query_stream => WorkflowDbStream);
        db_method!(proto, ctx, "_commit", JsTx, tx.commit() => WorkflowTxUnit);
        db_method!(proto, ctx, "_rollback", JsTx, tx.rollback() => WorkflowTxUnit);
        db_method!(proto, ctx, "_settleDefault", JsTx, tx.settle_default(bool) => WorkflowTxBool);
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Async-iterable wrapper around a [`RowStream`]. Yields rows as
/// `{ done, value }` objects per the JS iterator protocol, lifted by
/// `chat.js`-style stream wrapping in the JS module.
pub struct JsRowStream {
    inner: Arc<AsyncMutex<RowStream>>,
}

impl<'js> Trace<'js> for JsRowStream {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for JsRowStream {
    type Changed<'to> = JsRowStream;
}

impl<'js> JsClass<'js> for JsRowStream {
    const NAME: &'static str = "RowStream";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            PredefinedAtom::SymbolAsyncIterator,
            Function::new(ctx.clone(), |this: This<Class<'js, JsRowStream>>| {
                Ok::<_, rquickjs::Error>(this.0.clone())
            })?,
        )?;

        proto.set(
            PredefinedAtom::Next,
            Function::new(ctx.clone(), |this: This<Class<'js, JsRowStream>>| {
                let inner = this.0.borrow().inner.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    let mut guard = inner.lock().await;
                    RowIterResult(guard.next().await)
                }))
            })?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

// -- IntoJs wrappers --------------------------------------------------------

struct WorkflowDbExec(Result<ExecResult, WorkflowDbError>);
struct WorkflowDbQuery(Result<Vec<Row>, WorkflowDbError>);
struct WorkflowDbStream(Result<RowStream, WorkflowDbError>);
struct WorkflowDbBegin(Result<WorkflowTx, WorkflowDbError>);
struct WorkflowTxUnit(Result<(), WorkflowDbError>);
struct WorkflowTxBool(Result<bool, WorkflowDbError>);
struct RowIterResult(Result<Option<Row>, WorkflowDbError>);

impl<'js> IntoJs<'js> for WorkflowDbExec {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(r) => {
                let obj = Object::new(ctx.clone())?;
                obj.set("rowsAffected", r.rows_affected as f64)?;
                obj.set("lastInsertRowid", r.last_insert_rowid as f64)?;
                Ok(obj.into_value())
            }
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

impl<'js> IntoJs<'js> for WorkflowDbQuery {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(rows) => {
                let arr = Array::new(ctx.clone())?;
                for (i, row) in rows.into_iter().enumerate() {
                    arr.set(i, row_into_js(ctx, &row)?)?;
                }
                Ok(arr.into_value())
            }
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

impl<'js> IntoJs<'js> for WorkflowDbStream {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(stream) => {
                let instance = Class::instance(
                    ctx.clone(),
                    JsRowStream {
                        inner: Arc::new(AsyncMutex::new(stream)),
                    },
                )?;
                Ok(instance.into_value())
            }
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

impl<'js> IntoJs<'js> for WorkflowDbBegin {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(tx) => {
                let instance = Class::instance(ctx.clone(), JsTx { tx })?;
                Ok(instance.into_value())
            }
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

impl<'js> IntoJs<'js> for WorkflowTxUnit {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

impl<'js> IntoJs<'js> for WorkflowTxBool {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(b) => Ok(Value::new_bool(ctx.clone(), b)),
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

impl<'js> IntoJs<'js> for RowIterResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(Some(row)) => {
                let obj = Object::new(ctx.clone())?;
                obj.set("done", false)?;
                obj.set("value", row_into_js(ctx, &row)?)?;
                Ok(obj.into_value())
            }
            Ok(None) => {
                let obj = Object::new(ctx.clone())?;
                obj.set("done", true)?;
                Ok(obj.into_value())
            }
            Err(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

// -- Param + value conversion ----------------------------------------------

fn parse_params<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<Vec<TursoValue>> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    let arr = value.as_array().ok_or_else(|| {
        throw(
            ctx,
            "params: expected an array of values (null, number, string, boolean, Uint8Array)",
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter::<Value<'js>>().enumerate() {
        let item = item?;
        out.push(js_to_turso_value(&item).map_err(|e| throw(ctx, &format!("params[{i}]: {e}")))?);
    }
    Ok(out)
}

/// A JS param value that can't be bound to a turso statement.
#[derive(Debug, thiserror::Error)]
enum ParamError {
    #[error("{0}")]
    StringConv(String),
    #[error(
        "unsupported parameter type — only null, number, string, boolean, and Uint8Array are accepted"
    )]
    Unsupported,
}

fn js_to_turso_value<'js>(value: &Value<'js>) -> Result<TursoValue, ParamError> {
    if value.is_null() || value.is_undefined() {
        return Ok(TursoValue::Null);
    }
    if let Some(b) = value.as_bool() {
        return Ok(TursoValue::Integer(if b { 1 } else { 0 }));
    }
    if let Some(i) = value.as_int() {
        return Ok(TursoValue::Integer(i.into()));
    }
    if let Some(f) = value.as_float() {
        // JS Number — pass integers as Integer when they fit, else Real.
        if f.is_finite() && f.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&f) {
            return Ok(TursoValue::Integer(f as i64));
        }
        return Ok(TursoValue::Real(f));
    }
    if let Some(s) = value.as_string() {
        let s = s
            .to_string()
            .map_err(|e| ParamError::StringConv(e.to_string()))?;
        return Ok(TursoValue::Text(s));
    }
    if let Ok(bytes) = TypedArray::<u8>::from_value(value.clone()) {
        let slice: &[u8] = bytes.as_ref();
        return Ok(TursoValue::Blob(slice.to_vec()));
    }
    Err(ParamError::Unsupported)
}

fn turso_value_into_js<'js>(ctx: &Ctx<'js>, value: &TursoValue) -> JsResult<Value<'js>> {
    match value {
        TursoValue::Null => Ok(Value::new_null(ctx.clone())),
        TursoValue::Integer(i) => {
            if let Ok(i32_val) = i32::try_from(*i) {
                Ok(Value::new_int(ctx.clone(), i32_val))
            } else {
                Ok(Value::new_float(ctx.clone(), *i as f64))
            }
        }
        TursoValue::Real(f) => Ok(Value::new_float(ctx.clone(), *f)),
        TursoValue::Text(s) => s.clone().into_js(ctx),
        TursoValue::Blob(b) => {
            let arr = TypedArray::<u8>::new_copy(ctx.clone(), b.as_slice())?;
            Ok(arr.into_value())
        }
    }
}

fn row_into_js<'js>(ctx: &Ctx<'js>, row: &Row) -> JsResult<Value<'js>> {
    let obj = Object::new(ctx.clone())?;
    for (i, name) in row.columns.iter().enumerate() {
        let v = turso_value_into_js(ctx, &row.values[i])?;
        obj.set(name.as_str(), v)?;
    }
    Ok(obj.into_value())
}
