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
    Array, Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult,
    TypedArray, Value,
};
use tokio::sync::Mutex as AsyncMutex;
use turso::Value as TursoValue;

use crate::storage::{ExecResult, Row, RowStream, WorkflowDb, WorkflowDbError, WorkflowTx};

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

        proto.set(
            "_exec",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, JsDb>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let db = this.0.borrow().db.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowDbExec(db.exec(&sql, params).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "_query",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, JsDb>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let db = this.0.borrow().db.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowDbQuery(db.query(&sql, params).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "_innerQueryStream",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, JsDb>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let db = this.0.borrow().db.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowDbStream(db.query_stream(&sql, params).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "_beginTransaction",
            Function::new(ctx.clone(), |this: This<Class<'js, JsDb>>| {
                let db = this.0.borrow().db.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    WorkflowDbBegin(db.begin().await)
                }))
            })?,
        )?;

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

        proto.set(
            "_exec",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, JsTx>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let tx = this.0.borrow().tx.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowDbExec(tx.exec(&sql, params).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "_query",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, JsTx>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let tx = this.0.borrow().tx.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowDbQuery(tx.query(&sql, params).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "_innerQueryStream",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, JsTx>>, sql: String, params: Value<'js>| {
                    let params = parse_params(&ctx, &params)?;
                    let tx = this.0.borrow().tx.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowDbStream(tx.query_stream(&sql, params).await)
                    }))
                },
            )?,
        )?;

        proto.set(
            "_commit",
            Function::new(ctx.clone(), |this: This<Class<'js, JsTx>>| {
                let tx = this.0.borrow().tx.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    WorkflowTxUnit(tx.commit().await)
                }))
            })?,
        )?;

        proto.set(
            "_rollback",
            Function::new(ctx.clone(), |this: This<Class<'js, JsTx>>| {
                let tx = this.0.borrow().tx.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    WorkflowTxUnit(tx.rollback().await)
                }))
            })?,
        )?;

        proto.set(
            "_settleDefault",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, JsTx>>, success: bool| {
                    let tx = this.0.borrow().tx.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        WorkflowTxBool(tx.settle_default(success).await)
                    }))
                },
            )?,
        )?;

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
        out.push(
            js_to_turso_value(ctx, &item)
                .map_err(|msg| throw(ctx, &format!("params[{i}]: {msg}")))?,
        );
    }
    Ok(out)
}

fn js_to_turso_value<'js>(_ctx: &Ctx<'js>, value: &Value<'js>) -> Result<TursoValue, String> {
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
        let s = s.to_string().map_err(|e| e.to_string())?;
        return Ok(TursoValue::Text(s));
    }
    if let Ok(bytes) = TypedArray::<u8>::from_value(value.clone()) {
        let slice: &[u8] = bytes.as_ref();
        return Ok(TursoValue::Blob(slice.to_vec()));
    }
    Err(
        "unsupported parameter type — only null, number, string, boolean, and Uint8Array are accepted"
            .to_owned(),
    )
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

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
