//! `frances:v1/lifecycle` — graceful-shutdown hook.
//!
//! Exposes the user-facing `lifecycle` object (`{ shutdown: null }`).
//! Workflows opt in by assigning `lifecycle.shutdown = async () => {...}`.
//!
//! The module body (`js/lifecycle.js`) registers a *shutdown runner* on a
//! hidden global. The runtime invokes that runner when shutdown is
//! requested (a dehydrate from the host, or `workflow.exit()`), then
//! closes the inbox itself — it owns the `closed` / `closed_notify`
//! handles, so closing is a host concern, not the module's.

use rquickjs::{Ctx, Object, Result as JsResult};

/// Build the user-facing `lifecycle` object. Workflows assign
/// `lifecycle.shutdown`; the runtime reads it back (via the runner the
/// module registers) when winding the workflow down.
pub(crate) fn build_lifecycle_object<'js>(ctx: &Ctx<'js>) -> JsResult<Object<'js>> {
    let lifecycle = Object::new(ctx.clone())?;
    lifecycle.set("shutdown", rquickjs::Value::new_null(ctx.clone()))?;
    Ok(lifecycle)
}
