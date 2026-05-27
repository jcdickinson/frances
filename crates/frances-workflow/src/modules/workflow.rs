//! `frances:v1/workflow` — lifecycle (`exit`) + the busy-indicator
//! setter (`setStatus`).
//!
//! `exit()` requests graceful shutdown rather than killing the inbox
//! directly: it pulses `shutdown_notify`, and the `frances:v1/lifecycle`
//! module's background IIFE turns that into "run the user's shutdown
//! hook, then close the inbox." Workflows without a registered
//! `lifecycle.shutdown` handler still terminate promptly — the IIFE
//! closes the inbox unconditionally after the (absent) hook returns.
//!
//! `setStatus(text | null)` drives the TUI footer's busy indicator:
//! `Some(text)` shows the text with a spinner, `None` hides it. The
//! workflow owns this — the host no longer infers a "streaming" state
//! from token flow.

use std::sync::Arc;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Result as JsResult};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::HostFrame;

pub(crate) fn build_exit<'js>(
    ctx: &Ctx<'js>,
    shutdown_notify: Arc<Notify>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move || {
        shutdown_notify.notify_waiters();
        Ok::<_, rquickjs::Error>(())
    })
}

/// Build `setStatus(text | null)`. The argument is optional and
/// nullable: a string sets the indicator, `null`/`undefined`/omitted
/// clears it. Best-effort send — a dropped host receiver means the
/// session is winding down.
pub(crate) fn build_set_status<'js>(
    ctx: &Ctx<'js>,
    frames_tx: UnboundedSender<HostFrame>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move |text: Opt<Option<String>>| {
        let status = text.0.flatten();
        let _ = frames_tx.send(HostFrame::Status(status));
        Ok::<_, rquickjs::Error>(())
    })
}
