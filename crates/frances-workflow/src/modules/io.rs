//! `frances:v1/io` — IO primitives.
//!
//! Rust exposes a small, private sleep primitive consumed by JS-side
//! wrappers (the user-facing `Timer` in `js/io.js`, and — eventually —
//! AbortController utilities like `AbortSignal.timeout`). User code
//! never reaches this primitive: it lives only on the install-time
//! stash, which is deleted from `globalThis` once every virtual module
//! has captured its slot.
//!
//! Stash entries (private, not re-exported by any virtual module):
//!
//! - `_setSleep(ms: number) -> SleepToken`
//! - `_clearSleep(token: SleepToken) -> void`
//!
//! `SleepToken` is a Rust-backed JS class with two behaviours:
//!
//! 1. **Thenable.** `token.then(onF, onR)` resolves with a string
//!    describing how the sleep settled:
//!      - `"fired"`     — the requested duration elapsed naturally,
//!      - `"closed"`    — the workflow began tearing down before then,
//!      - `"cancelled"` — `_clearSleep` was called (or the token was
//!        dropped, see below).
//!
//!    Errors are not modelled by the primitive; consumers attach
//!    higher-level rejection semantics in JS.
//!
//! 2. **Drop cancels.** When the JS `SleepToken` is GC'd, the Rust
//!    `Drop` impl pulses `cancel`, so the spawned tokio task exits and
//!    no longer holds a pending sleep. Explicit `_clearSleep(token)`
//!    is the synchronous variant of the same operation.
//!
//! The primitive carries one shared piece of host state: the workflow's
//! `closed` flag plus its `Notify`. The spawned task observes both,
//! which is how pending sleeps surface as `"closed"` during graceful
//! teardown.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Opt, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use tokio::sync::Notify;

/// Builds the two stash functions plus the (registered, but
/// no-constructor) `SleepToken` JsClass. The caller stitches the
/// returned functions onto the v1 stash under `_setSleep` /
/// `_clearSleep`.
pub(crate) fn build_sleep_primitives<'js>(
    ctx: &Ctx<'js>,
    workflow_closed: Arc<AtomicBool>,
    workflow_closed_notify: Arc<Notify>,
) -> JsResult<(Function<'js>, Function<'js>)> {
    let closed = workflow_closed.clone();
    let closed_notify = workflow_closed_notify.clone();
    let set_sleep = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, ms: Value<'js>| -> JsResult<Class<'js, SleepToken>> {
            let n = ms
                .as_number()
                .ok_or_else(|| throw_err(&ctx, "_setSleep: ms must be a number"))?;
            if !n.is_finite() || n < 0.0 {
                return Err(throw_err(
                    &ctx,
                    "_setSleep: ms must be a finite, non-negative number of milliseconds",
                ));
            }
            let duration = Duration::from_millis(n.round() as u64);

            let inner = Arc::new(SleepTokenInner {
                result: Mutex::new(None),
                settled: Notify::new(),
                cancel: Notify::new(),
            });

            // Fast-path: if the workflow has already closed by the time
            // we're constructing the token, settle as "closed" without
            // ever spawning a task.
            if closed.load(Ordering::Acquire) {
                *inner.result.lock() = Some("closed");
            } else {
                spawn_sleep_task(
                    inner.clone(),
                    duration,
                    closed.clone(),
                    closed_notify.clone(),
                );
            }

            Class::instance(ctx, SleepToken { inner })
        },
    )?;

    let clear_sleep = Function::new(
        ctx.clone(),
        |_ctx: Ctx<'js>, token: Class<'js, SleepToken>| -> JsResult<()> {
            token.borrow().inner.cancel.notify_waiters();
            Ok(())
        },
    )?;

    Ok((set_sleep, clear_sleep))
}

fn spawn_sleep_task(
    inner: Arc<SleepTokenInner>,
    duration: Duration,
    workflow_closed: Arc<AtomicBool>,
    workflow_closed_notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        let cancel = inner.cancel.notified();
        let closed = workflow_closed_notify.notified();
        let sleep = tokio::time::sleep(duration);
        tokio::pin!(cancel);
        tokio::pin!(closed);
        tokio::pin!(sleep);

        // Register cancel + closed waiters before reading the closed
        // flag, so any pulse that races against us is held as a permit
        // and the select! sees it.
        cancel.as_mut().enable();
        closed.as_mut().enable();

        // If the workflow closed between the fast-path check and now,
        // surface "closed" immediately.
        if workflow_closed.load(Ordering::Acquire) {
            settle(&inner, "closed");
            return;
        }

        let reason: &'static str = tokio::select! {
            biased;
            () = &mut cancel => "cancelled",
            () = &mut closed => "closed",
            () = &mut sleep => "fired",
        };
        settle(&inner, reason);
    });
}

fn settle(inner: &SleepTokenInner, reason: &'static str) {
    let mut g = inner.result.lock();
    if g.is_none() {
        *g = Some(reason);
        inner.settled.notify_waiters();
    }
}

pub struct SleepToken {
    inner: Arc<SleepTokenInner>,
}

struct SleepTokenInner {
    /// Settled once, then immutable: `"fired" | "closed" | "cancelled"`.
    result: Mutex<Option<&'static str>>,
    /// Pulsed when `result` transitions from `None` to `Some(_)`.
    settled: Notify,
    /// Pulsed by `_clearSleep` (and by `Drop`). The spawned task
    /// observes this in its `select!`.
    cancel: Notify,
}

impl Drop for SleepToken {
    fn drop(&mut self) {
        // Idempotent: if the task has already settled, the pulse falls
        // on no registered waiters and is harmless.
        self.inner.cancel.notify_waiters();
    }
}

impl<'js> Trace<'js> for SleepToken {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for SleepToken {
    type Changed<'to> = SleepToken;
}

impl<'js> JsClass<'js> for SleepToken {
    const NAME: &'static str = "SleepToken";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "then",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>,
                 this: This<Class<'js, SleepToken>>,
                 on_fulfilled: Opt<Value<'js>>,
                 on_rejected: Opt<Value<'js>>| {
                    let on_f = on_fulfilled
                        .0
                        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                    let on_r = on_rejected
                        .0
                        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                    sleep_token_then(&ctx, &this.0, on_f, on_r)
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<rquickjs::function::Constructor<'js>>> {
        Ok(None)
    }
}

/// Implements the thenable protocol: builds an inner Promise whose
/// future waits for `inner.result` to be populated, then forwards the
/// caller's `onFulfilled` / `onRejected` through the inner promise.
fn sleep_token_then<'js>(
    ctx: &Ctx<'js>,
    this: &Class<'js, SleepToken>,
    on_fulfilled: Value<'js>,
    on_rejected: Value<'js>,
) -> JsResult<Value<'js>> {
    let inner = this.borrow().inner.clone();
    let promised = Promised::from(async move {
        loop {
            // Register before reading so any notify that fires between
            // our read and our await is held as a permit on the
            // Notified future.
            let n = inner.settled.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if let Some(r) = *inner.result.lock() {
                return ResultStr(r);
            }
            n.await;
        }
    });

    let inner_val: Value<'js> = promised.into_js(ctx)?;
    let Some(inner_obj) = inner_val.into_object() else {
        return Err(throw_err(
            ctx,
            "SleepToken: internal promise was not an object (rquickjs bug?)",
        ));
    };
    let inner_then: Function<'js> = inner_obj.get("then")?;
    inner_then.call((This(inner_obj), on_fulfilled, on_rejected))
}

/// Newtype so we can implement `IntoJs` for the resolution string.
/// Strings are passed by value through JS, so the &'static slice is
/// just a tag we copy at the boundary.
struct ResultStr(&'static str);

impl<'js> IntoJs<'js> for ResultStr {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        self.0.into_js(ctx)
    }
}

fn throw_err<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
