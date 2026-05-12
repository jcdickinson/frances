//! `frances:v1/approval` — single async `approve()` function.
//!
//! JS surface (see `js/approval.js`):
//!
//! ```js
//! import { approve } from "frances:v1/approval";
//! const choice = await approve("delete /tmp/foo?");
//! // choice is one of:
//! //   { type: "yes",  details: string | null }
//! //   { type: "no",   details: string | null }
//! //   { type: "chat", content: string }
//! ```
//!
//! Rust does the work behind a single primitive `_approve(prompt)` on
//! the install stash: allocate an `ApprovalId` via the gateway, emit a
//! `HostFrame::Approval(request)` so the daemon can forward it to the
//! TUI, then await the gateway's response oneshot. If the workflow
//! shuts down first, the await resolves to a synthetic `Chat { "" }`
//! response so the JS body can unwind cleanly without throwing.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use rquickjs::promise::Promised;
use rquickjs::{Ctx, Exception, Function, IntoJs, Object, Result as JsResult, Value};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::approval::{ApprovalChoice, ApprovalGateway, ApprovalKind};
use crate::deps::WorkflowDeps;
use crate::runtime::HostFrame;

/// Build the `_approve` primitive that `frances:v1/approval` re-wraps.
/// Captures the gateway from deps, the frames channel, and the workflow
/// `closed` signal so a graceful shutdown surfaces as a benign result
/// instead of a hung promise.
pub(crate) fn build_approve_primitive<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
    frames_tx: UnboundedSender<HostFrame>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, prompt: Value<'js>| -> JsResult<Value<'js>> {
            let prompt_str = parse_prompt(&ctx, &prompt)?;

            // v1 only emits YesNo; future variants land here via opts
            // parsing without changing the JS API shape.
            let gateway = deps.approval_gateway().clone();
            let (request, rx) = gateway.allocate(prompt_str, ApprovalKind::YesNo);

            // Best-effort emit; the receiver side is a tokio mpsc that
            // outlives the workflow body (owned by the daemon's drive
            // loop), so a closed channel here means the host is gone
            // and there's nothing to do but resolve the promise.
            let _ = frames_tx.send(HostFrame::Approval(request));

            let closed = closed.clone();
            let closed_notify = closed_notify.clone();
            let promised = Promised::from(async move {
                // Fast path: workflow already shutting down.
                if closed.load(Ordering::Acquire) {
                    return ChoiceJs(default_shutdown_choice());
                }
                tokio::select! {
                    biased;
                    () = closed_notify.notified() => ChoiceJs(default_shutdown_choice()),
                    res = rx => match res {
                        Ok(choice) => ChoiceJs(choice),
                        Err(_) => ChoiceJs(default_shutdown_choice()),
                    },
                }
            });
            promised.into_js(&ctx)
        },
    )
}

fn parse_prompt<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<String> {
    if let Some(s) = value.as_string() {
        return s.to_string();
    }
    Err(throw(
        ctx,
        "approve: expected a string prompt as the first argument",
    ))
}

fn default_shutdown_choice() -> ApprovalChoice {
    // Graceful: matches how `inbox.next()` returns `done` on shutdown
    // rather than rejecting — JS callers can write a single
    // straight-line `await approve(...)` without try/catch.
    ApprovalChoice::Chat {
        content: String::new(),
    }
}

struct ChoiceJs(ApprovalChoice);

impl<'js> IntoJs<'js> for ChoiceJs {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        match self.0 {
            ApprovalChoice::Yes { details } => {
                obj.set("type", "yes")?;
                set_optional_string(ctx, &obj, "details", details)?;
            }
            ApprovalChoice::No { details } => {
                obj.set("type", "no")?;
                set_optional_string(ctx, &obj, "details", details)?;
            }
            ApprovalChoice::Chat { content } => {
                obj.set("type", "chat")?;
                obj.set("content", content)?;
            }
        }
        Ok(obj.into_value())
    }
}

fn set_optional_string<'js>(
    ctx: &Ctx<'js>,
    obj: &Object<'js>,
    key: &str,
    value: Option<String>,
) -> JsResult<()> {
    match value {
        Some(s) => obj.set(key, s)?,
        None => obj.set(key, Value::new_null(ctx.clone()))?,
    }
    Ok(())
}

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
