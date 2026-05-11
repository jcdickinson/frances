//! `frances:v1/chat` — `ChatSession` for talking to the LLM.
//!
//! v1 status: the JS surface is fully present so workflows can be
//! written against it, but the runtime is not yet wired to the daemon's
//! `ChatSessionManager`. `stream()` therefore throws a clear "not yet
//! wired" error. The cross-crate plumbing is a focused follow-up — once
//! it lands, only this file changes (the JS shape is locked).
//!
//! Shape:
//!
//! ```js
//! const s = new ChatSession({ model_intents: ["summarize"] });
//! s.push({ role: "user", content: "hi" });
//! const r = await s.stream();
//! for await (const p of r.chunks) { /* … */ }
//! // assistant reply is auto-pushed onto `s` once the stream ends
//! ```
//!
//! Roles in v1: `"system"` and `"user"`. Pushing `"assistant"` throws —
//! assistant messages come from the model, the workflow doesn't
//! fabricate them. `"system"` may only be pushed before any `"user"`
//! message; once user input is in the conversation, the system block
//! is fixed.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::{Class, Ctx, Exception, Function, JsLifetime, Object, Result as JsResult, Value};

pub(crate) fn build_chat_session_ctor<'js>(ctx: &Ctx<'js>) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<ChatSession, _, _>(ctx.clone(), |ctx: Ctx<'js>, arg: Value<'js>| {
        let intents = parse_intents(&ctx, &arg)?;
        Class::instance(
            ctx.clone(),
            ChatSession {
                model_intents: intents,
                messages: Arc::new(StdMutex::new(Vec::new())),
            },
        )
    })
}

pub struct ChatSession {
    #[expect(
        dead_code,
        reason = "wired to backend by follow-up; kept on the type so the shape is locked"
    )]
    model_intents: Vec<String>,
    messages: Arc<StdMutex<Vec<ChatMessage>>>,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    #[expect(dead_code, reason = "consumed when stream() is wired to the backend")]
    pub content: String,
}

#[derive(Clone, Copy, Debug)]
pub enum ChatRole {
    System,
    User,
}

impl<'js> Trace<'js> for ChatSession {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ChatSession {
    type Changed<'to> = ChatSession;
}

impl<'js> JsClass<'js> for ChatSession {
    const NAME: &'static str = "ChatSession";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "push",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ChatSession>>, msg: Value<'js>| {
                    push_message(&ctx, &this.0, msg)
                },
            )?,
        )?;

        proto.set(
            "stream",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, _this: This<Class<'js, ChatSession>>| -> JsResult<()> {
                    Err(throw(
                        &ctx,
                        "ChatSession.stream: LLM backend is not yet wired into the workflow \
                         runtime (follow-up). The JS API shape is final; only the host wiring \
                         is pending.",
                    ))
                },
            )?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

fn parse_intents<'js>(ctx: &Ctx<'js>, arg: &Value<'js>) -> JsResult<Vec<String>> {
    let Some(obj) = arg.as_object() else {
        return Err(throw(
            ctx,
            "new ChatSession: expected { model_intents: string[] }",
        ));
    };
    let intents_val: Value<'js> = obj
        .get("model_intents")
        .map_err(|_| throw(ctx, "new ChatSession: missing `model_intents`"))?;
    let Some(arr) = intents_val.as_array() else {
        return Err(throw(
            ctx,
            "new ChatSession: `model_intents` must be an array of strings",
        ));
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr.iter::<String>() {
        let s = item.map_err(|_| {
            throw(
                ctx,
                "new ChatSession: every `model_intents` entry must be a string",
            )
        })?;
        out.push(s);
    }
    Ok(out)
}

fn push_message<'js>(
    ctx: &Ctx<'js>,
    session: &Class<'js, ChatSession>,
    msg: Value<'js>,
) -> JsResult<()> {
    let Some(obj) = msg.as_object() else {
        return Err(throw(ctx, "session.push: expected { role, content }"));
    };
    let role_str: String = obj
        .get("role")
        .map_err(|_| throw(ctx, "session.push: missing or non-string `role`"))?;
    let role = match role_str.as_str() {
        "user" => ChatRole::User,
        "system" => ChatRole::System,
        "assistant" => {
            return Err(throw(
                ctx,
                "session.push: role `assistant` is not pushable \
                 — assistant turns come from the model, not the workflow",
            ));
        }
        other => {
            return Err(throw(
                ctx,
                &format!("session.push: unknown role `{other}` (expected `system` or `user`)"),
            ));
        }
    };
    let content: String = obj
        .get("content")
        .map_err(|_| throw(ctx, "session.push: missing or non-string `content`"))?;

    let borrow = session.borrow();
    let mut messages = borrow
        .messages
        .lock()
        .expect("chat session messages poisoned");
    if matches!(role, ChatRole::System)
        && messages.iter().any(|m| !matches!(m.role, ChatRole::System))
    {
        return Err(throw(
            ctx,
            "session.push: role `system` is only valid before any user message has been pushed",
        ));
    }
    messages.push(ChatMessage { role, content });
    Ok(())
}

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
