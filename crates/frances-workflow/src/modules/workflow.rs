//! `frances:v1/workflow` — lifecycle (just `exit` in v1).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::{Ctx, Function, Result as JsResult};
use tokio::sync::Notify;

pub(crate) fn build_exit<'js>(
    ctx: &Ctx<'js>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
) -> JsResult<Function<'js>> {
    Function::new(ctx.clone(), move || {
        if !closed.swap(true, Ordering::AcqRel) {
            closed_notify.notify_waiters();
        }
        Ok::<_, rquickjs::Error>(())
    })
}
