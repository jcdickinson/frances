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
//! `Some(text)` shows the text with a spinner, `None` hides it.
//!
//! `setTitle(text | null)` sets or clears the session title. It rides
//! the same surfaces channel; the driver persists it into session
//! metadata (unlike the footer, it outlives the workflow).

use std::sync::Arc;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Result as JsResult};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::SurfaceCmd;

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
/// clears it.
pub(crate) fn build_set_status<'js>(
    ctx: &Ctx<'js>,
    surfaces_tx: UnboundedSender<SurfaceCmd>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move |text: Opt<Option<String>>| {
        let cmd = match text.0.flatten() {
            Some(text) => SurfaceCmd::SetFooter { text },
            None => SurfaceCmd::ClearFooter,
        };
        let _ = surfaces_tx.send(cmd);
        Ok::<_, rquickjs::Error>(())
    })
}

/// Build the `_setTitle(text | null)` primitive. Same optional/nullable
/// argument shape as `setStatus`; the JS wrapper in
/// `assets/frances/v1/workflow.js` layers `getTitle()` on top by caching
/// the last value it sent.
pub(crate) fn build_set_title<'js>(
    ctx: &Ctx<'js>,
    surfaces_tx: UnboundedSender<SurfaceCmd>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move |text: Opt<Option<String>>| {
        let _ = surfaces_tx.send(SurfaceCmd::SetTitle {
            title: text.0.flatten(),
        });
        Ok::<_, rquickjs::Error>(())
    })
}
