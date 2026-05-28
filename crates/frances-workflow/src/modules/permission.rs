//! `frances:v1/approval` — single async `approve()` function.
//!
//! JS surface (see `js/approval.js`):
//!
//! ```js
//! import { approve } from "frances:v1/approval";
//! const choice = await approve({
//!   prompt: "delete /tmp/foo?",
//!   toolCall: { id, name, arguments },  // optional
//!   allowAuto: false,                   // optional
//! });
//! // choice is one of:
//! //   { type: "yes",  details: string | null }
//! //   { type: "no",   details: string | null }
//! ```
//!
//! Rust does the work behind a single primitive `_approve(options)` on
//! the install stash: parse the options object, allocate a
//! `PermissionId` via the gateway, send a `PermissionAsk` on the
//! permissions channel so the runtime can forward it to the TUI, then await the gateway's
//! response oneshot. If the workflow shuts down first, the await
//! resolves to a synthetic `No { details: None }` so the JS body can
//! unwind cleanly without throwing.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use frances_models_llm::wire::ToolCall;
use rquickjs::promise::Promised;
use rquickjs::{Ctx, Exception, Function, IntoJs, Object, Result as JsResult, Value};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::permission::{PermissionRequest, PermissionResponse};

/// Build the `_approve` primitive that `frances:v1/approval` re-wraps.
/// Owns a sender into the permissions channel and the workflow `closed`
/// signal so a graceful shutdown surfaces as a benign result instead of
/// a hung promise.
pub(crate) fn build_approve_primitive<'js>(
    ctx: &Ctx<'js>,
    permissions_tx: UnboundedSender<PermissionRequest>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, options: Value<'js>| -> JsResult<Value<'js>> {
            let ApproveOptions {
                prompt,
                tool_call,
                allow_auto,
            } = parse_options(&ctx, &options)?;

            // The request carries its own reply slot — no gateway, no id
            // table. Whoever answers (auto-judge or TUI) resolves it.
            let (reply, rx) = oneshot::channel();

            // Best-effort emit; the receiver side is a tokio mpsc that
            // outlives the workflow body (owned by the runtime's drive
            // loop), so a closed channel here means the host is gone
            // and there's nothing to do but resolve the promise.
            let _ = permissions_tx.send(PermissionRequest {
                prompt,
                tool_call,
                allow_auto,
                reply,
            });

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
                        Ok(response) => ChoiceJs(response),
                        Err(_) => ChoiceJs(default_shutdown_choice()),
                    },
                }
            });
            promised.into_js(&ctx)
        },
    )
}

struct ApproveOptions {
    prompt: String,
    tool_call: Option<ToolCall>,
    allow_auto: bool,
}

fn parse_options<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<ApproveOptions> {
    let Some(obj) = value.as_object() else {
        return Err(throw(
            ctx,
            "approve: expected an options object: { prompt, toolCall?, allowAuto? }",
        ));
    };

    let prompt = match obj.get::<_, Value<'js>>("prompt")? {
        v if v.is_string() => v
            .as_string()
            .expect("checked above")
            .to_string()
            .map_err(|e| throw(ctx, &format!("approve: prompt conversion: {e}")))?,
        _ => {
            return Err(throw(ctx, "approve: `prompt` must be a string"));
        }
    };

    let tool_call = parse_tool_call(ctx, &obj.get::<_, Value<'js>>("toolCall")?)?;
    let allow_auto = parse_allow_auto(ctx, &obj.get::<_, Value<'js>>("allowAuto")?)?;

    Ok(ApproveOptions {
        prompt,
        tool_call,
        allow_auto,
    })
}

fn parse_tool_call<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<Option<ToolCall>> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let Some(obj) = value.as_object() else {
        return Err(throw(
            ctx,
            "approve: `toolCall` must be an object: { id, name, arguments }",
        ));
    };

    let id = string_field(ctx, obj, "id")?;
    let name = string_field(ctx, obj, "name")?;
    let arguments_value: Value<'js> = obj.get("arguments")?;
    let arguments = crate::modules::rquickjs_to_json(&arguments_value)
        .map_err(|e| throw(ctx, &format!("approve: `toolCall.arguments`: {e}")))?;

    Ok(Some(ToolCall {
        error: None,
        id,
        name,
        arguments,
    }))
}

fn string_field<'js>(ctx: &Ctx<'js>, obj: &Object<'js>, key: &str) -> JsResult<String> {
    let value: Value<'js> = obj.get(key)?;
    let Some(s) = value.as_string() else {
        return Err(throw(
            ctx,
            &format!("approve: `toolCall.{key}` must be a string"),
        ));
    };
    s.to_string()
}

fn parse_allow_auto<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> JsResult<bool> {
    if value.is_undefined() || value.is_null() {
        return Ok(false);
    }
    value
        .as_bool()
        .ok_or_else(|| throw(ctx, "approve: `allowAuto` must be a boolean if provided"))
}

fn default_shutdown_choice() -> PermissionResponse {
    // Graceful: matches how `inbox.next()` returns `done` on shutdown
    // rather than rejecting — JS callers can write a single
    // straight-line `await approve(...)` without try/catch.
    PermissionResponse::No { details: None }
}

struct ChoiceJs(PermissionResponse);

impl<'js> IntoJs<'js> for ChoiceJs {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        match self.0 {
            PermissionResponse::Yes { details } => {
                obj.set("type", "yes")?;
                set_optional_string(ctx, &obj, "details", details)?;
            }
            PermissionResponse::No { details } => {
                obj.set("type", "no")?;
                set_optional_string(ctx, &obj, "details", details)?;
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
