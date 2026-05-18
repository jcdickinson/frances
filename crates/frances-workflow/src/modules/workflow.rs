//! `frances:v1/workflow` — lifecycle (just `exit` in v1).
//!
//! `exit()` requests graceful shutdown rather than killing the inbox
//! directly: it pulses `shutdown_notify`, and the `frances:v1/lifecycle`
//! module's background IIFE turns that into "run the user's shutdown
//! hook, then close the inbox." Workflows without a registered
//! `lifecycle.shutdown` handler still terminate promptly — the IIFE
//! closes the inbox unconditionally after the (absent) hook returns.

use std::sync::Arc;

use rquickjs::{Ctx, Function, Result as JsResult};
use tokio::sync::Notify;

pub(crate) fn build_exit<'js>(
    ctx: &Ctx<'js>,
    shutdown_notify: Arc<Notify>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move || {
        shutdown_notify.notify_waiters();
        Ok::<_, rquickjs::Error>(())
    })
}
