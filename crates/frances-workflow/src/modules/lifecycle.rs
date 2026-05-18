//! `frances:v1/lifecycle` — graceful-shutdown hook.
//!
//! The host signals shutdown by pulsing a `Notify`. The JS module body
//! awaits that pulse, runs the workflow-supplied `lifecycle.shutdown`
//! function (if any), then flips the inbox `closed` flag — which the
//! existing inbox machinery surfaces as `{ done: true }` to any
//! `for await (const input of inbox)` loop in the user body.
//!
//! Three primitives stashed under `__frances_v1_stash__`:
//!
//! - `lifecycle` — the user-visible `{ shutdown: null }` object.
//!   Workflows assign `lifecycle.shutdown = async () => { ... }`; the
//!   IIFE reads it back when the signal fires.
//! - `_waitForShutdown()` — returns a Promise that resolves when the
//!   host calls `shutdown_notify.notify_waiters()`.
//! - `_closeInbox()` — flips `closed` and pulses `closed_notify`, same
//!   as `inbox.return()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::promise::Promised;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use tokio::sync::Notify;

pub(crate) fn build_lifecycle_primitives<'js>(
    ctx: &Ctx<'js>,
    shutdown_notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
) -> JsResult<(Object<'js>, Function<'js>, Function<'js>)> {
    let lifecycle = Object::new(ctx.clone())?;
    lifecycle.set("shutdown", rquickjs::Value::new_null(ctx.clone()))?;

    let wait_for_shutdown = {
        let signal = shutdown_notify.clone();
        Function::new(ctx.clone(), move || {
            let signal = signal.clone();
            Ok::<_, rquickjs::Error>(Promised::from(async move {
                // Register before checking would matter if there were a
                // way to read the "fired" state, but Notify has no such
                // read — `notified()` only sees future pulses unless a
                // permit is held. The contract is one-shot: the host
                // pulses exactly once and the IIFE awaits once.
                signal.notified().await;
            }))
        })?
    };

    let close_inbox = {
        let closed = closed.clone();
        let closed_notify = closed_notify.clone();
        Function::new(ctx.clone(), move || {
            if !closed.swap(true, Ordering::AcqRel) {
                closed_notify.notify_waiters();
            }
            Ok::<_, rquickjs::Error>(())
        })?
    };

    Ok((lifecycle, wait_for_shutdown, close_inbox))
}
