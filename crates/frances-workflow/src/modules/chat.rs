//! `frances:v1/chat` — `ChatSession` for talking to the LLM.
//!
//! The Rust side hands back a thin `{ events, completed }` shape; the
//! JS wrapper in `js/chat.js` lifts `events` into a WHATWG
//! `ReadableStream<StreamEvent>`, adds a lazy `text` `ReadableStream<string>`
//! and accepts an `AbortSignal`. Public shape from JS:
//!
//! ```js
//! const s = new ChatSession({ model_intents: ["summarize"] });
//! s.push({ role: "user", content: "hi" });
//! const r = await s.stream({ signal });
//! await r.text.pipeTo(frame.writable);
//! const final = await r.completed;     // { text, usage }
//! ```
//!
//! Roles in v1: `"system"` and `"user"`. Pushing `"assistant"` throws —
//! assistant messages come from the model. `"system"` may only be
//! pushed before any `"user"` message; after the first user push the
//! system slot is locked.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::oneshot;

use frances_models_llm::chat::{
    ChatError, ChatSession as ChatSessionTrait, ChatSessionBuilder,
    ChatSessionManager as ChatSessionManagerTrait, OwnedHistoryInput,
};
use frances_models_llm::wire::StreamEvent;

use crate::deps::WorkflowDeps;

type Session<D> = <<D as WorkflowDeps>::ChatSessionManager as ChatSessionManagerTrait>::Session;

/// Build the `ChatSession` constructor together with a private
/// "start raw stream" function. The constructor goes on the stash as
/// `ChatSession`; the raw-stream function goes on the stash as
/// `__chat_inner_stream` and is captured into closure by `chat.js`
/// before the stash is wiped, so the raw async-iterable event source
/// is never reachable from user code.
pub(crate) fn build_chat_session_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<(Constructor<'js>, Function<'js>)> {
    let ctor = Constructor::new_class::<ChatSessionJs<D>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Value<'js>| {
            let intents = parse_intents(&ctx, &arg)?;
            let builder = ChatSessionBuilder::new().with_model_intents(intents);
            let handle = deps.chat_session_manager().create(builder);
            Class::instance(
                ctx.clone(),
                ChatSessionJs::<D> {
                    handle,
                    deps: deps.clone(),
                    system_locked: AtomicBool::new(false),
                },
            )
        },
    )?;

    let inner_stream = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, this: This<Class<'js, ChatSessionJs<D>>>| -> JsResult<Value<'js>> {
            start_stream::<D>(&ctx, &this.0)
        },
    )?;

    Ok((ctor, inner_stream))
}

pub struct ChatSessionJs<D: WorkflowDeps> {
    handle: Session<D>,
    /// Captured at construction so each `stream()` call can resolve env
    /// vars (auth, etc.) against the latest client attach snapshot.
    deps: D,
    /// Flipped once the first `user` message is pushed. After that,
    /// `system` pushes throw.
    system_locked: AtomicBool,
}

impl<'js, D: WorkflowDeps> Trace<'js> for ChatSessionJs<D> {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js, D: WorkflowDeps> JsLifetime<'js> for ChatSessionJs<D> {
    type Changed<'to> = ChatSessionJs<D>;
}

impl<'js, D: WorkflowDeps> JsClass<'js> for ChatSessionJs<D> {
    const NAME: &'static str = "ChatSession";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        // `stream` is deliberately NOT on the prototype: the raw
        // async-iterable event source is private. `chat.js` installs
        // a WHATWG-wrapped `stream` here from the stash.
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "push",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ChatSessionJs<D>>>, msg: Value<'js>| {
                    push_message::<D>(&ctx, &this.0, msg)
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

fn push_message<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    session: &Class<'js, ChatSessionJs<D>>,
    msg: Value<'js>,
) -> JsResult<()> {
    let Some(obj) = msg.as_object() else {
        return Err(throw(ctx, "session.push: expected { role, content }"));
    };
    let role_str: String = obj
        .get("role")
        .map_err(|_| throw(ctx, "session.push: missing or non-string `role`"))?;
    let content: String = obj
        .get("content")
        .map_err(|_| throw(ctx, "session.push: missing or non-string `content`"))?;

    let borrow = session.borrow();
    let input = match role_str.as_str() {
        "user" => {
            borrow.system_locked.store(true, Ordering::Release);
            OwnedHistoryInput::User { text: content }
        }
        "system" => {
            if borrow.system_locked.load(Ordering::Acquire) {
                return Err(throw(
                    ctx,
                    "session.push: role `system` is only valid before any user message has been pushed",
                ));
            }
            OwnedHistoryInput::System { text: content }
        }
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
    borrow.handle.push(input);
    Ok(())
}

/// Synchronously kicks off a provider stream and returns an object
/// `{ events, completed }`.
///
/// `events` is an async iterable that yields stream events as they
/// arrive. `completed` is a Promise resolving to `{ text, usage }` once
/// `ChatSession::run` has settled. The rs-side `run` writes the
/// assistant primitive to history, so the next `s.stream()` call sees
/// the prior turn via `loaded_history`.
fn start_stream<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    session: &Class<'js, ChatSessionJs<D>>,
) -> JsResult<Value<'js>> {
    let (handle, env) = {
        let borrow = session.borrow();
        (borrow.handle.clone(), borrow.deps.current_env())
    };

    let (event_tx, event_rx) = mpsc::unbounded_channel::<StreamEvent>();
    let (completed_tx, completed_rx) = oneshot::channel::<Result<CompletedJs, ChatError>>();

    // Mirror the latest `Usage` event into the completion result so
    // `pipeTo` callers (who don't iterate `r.events`) still see it.
    let usage_capture: Arc<parking_lot::Mutex<Option<frances_models_llm::wire::Usage>>> =
        Arc::new(parking_lot::Mutex::new(None));

    tokio::spawn({
        let usage_capture = usage_capture.clone();
        async move {
            let tx_for_callback = event_tx.clone();
            let usage_for_callback = usage_capture.clone();
            let on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send> =
                Box::new(move |event| {
                    if let StreamEvent::Usage(u) = &event {
                        *usage_for_callback.lock() = Some(u.clone());
                    }
                    let _ = tx_for_callback.send(event);
                    Ok(())
                });
            let result = handle.run(env, Vec::new(), None, on_event).await;
            let usage = usage_capture.lock().take();
            drop(event_tx);
            let mapped = result.map(|outcome| CompletedJs {
                text: outcome.text,
                usage,
            });
            let _ = completed_tx.send(mapped);
        }
    });

    let events_class = Class::instance(
        ctx.clone(),
        JsEventStream {
            rx: Arc::new(AsyncMutex::new(event_rx)),
        },
    )?;

    let completed_promise = Promised::from(async move {
        match completed_rx.await {
            Ok(Ok(c)) => CompletionResult(Ok(c)),
            Ok(Err(e)) => CompletionResult(Err(format!("chat stream failed: {e}"))),
            Err(_) => CompletionResult(Err("chat stream task aborted".to_owned())),
        }
    });

    let obj = Object::new(ctx.clone())?;
    obj.set("events", events_class)?;
    obj.set("completed", completed_promise)?;
    Ok(obj.into_value())
}

/// Async-iterable wrapper around the stream-event receiver. Lifted from
/// `Inbox`'s `Symbol.asyncIterator` pattern; yields `StreamEvent`s
/// converted to discriminated-union JS objects.
pub struct JsEventStream {
    rx: Arc<AsyncMutex<UnboundedReceiver<StreamEvent>>>,
}

impl<'js> Trace<'js> for JsEventStream {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for JsEventStream {
    type Changed<'to> = JsEventStream;
}

impl<'js> JsClass<'js> for JsEventStream {
    const NAME: &'static str = "ChatEvents";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            PredefinedAtom::SymbolAsyncIterator,
            Function::new(ctx.clone(), |this: This<Class<'js, JsEventStream>>| {
                Ok::<_, rquickjs::Error>(this.0.clone())
            })?,
        )?;

        proto.set(
            PredefinedAtom::Next,
            Function::new(ctx.clone(), |this: This<Class<'js, JsEventStream>>| {
                let rx = this.0.borrow().rx.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    let mut guard = rx.lock().await;
                    match guard.recv().await {
                        Some(event) => IterResult::value(JsStreamEvent(event)),
                        None => IterResult::done(),
                    }
                }))
            })?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

struct JsStreamEvent(StreamEvent);

impl<'js> IntoJs<'js> for JsStreamEvent {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        match self.0 {
            StreamEvent::TextDelta(delta) => {
                obj.set("type", "text")?;
                obj.set("delta", delta)?;
            }
            StreamEvent::Usage(usage) => {
                obj.set("type", "usage")?;
                let u = Object::new(ctx.clone())?;
                u.set("promptTokens", usage.prompt_tokens)?;
                u.set("completionTokens", usage.completion_tokens)?;
                u.set("totalTokens", usage.total_tokens)?;
                u.set("cachedInputTokens", usage.cached_input_tokens)?;
                obj.set("usage", u)?;
            }
            // Provider-internal; never surfaces to JS.
            StreamEvent::History(_) | StreamEvent::ToolCall(_) => {
                obj.set("type", "ignored")?;
            }
        }
        Ok(obj.into_value())
    }
}

struct CompletionResult(Result<CompletedJs, String>);

impl<'js> IntoJs<'js> for CompletionResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(c) => c.into_js(ctx),
            Err(msg) => Err(throw(ctx, &msg)),
        }
    }
}

struct CompletedJs {
    text: String,
    usage: Option<frances_models_llm::wire::Usage>,
}

impl<'js> IntoJs<'js> for CompletedJs {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("text", self.text)?;
        if let Some(usage) = self.usage {
            let u = Object::new(ctx.clone())?;
            u.set("promptTokens", usage.prompt_tokens)?;
            u.set("completionTokens", usage.completion_tokens)?;
            u.set("totalTokens", usage.total_tokens)?;
            u.set("cachedInputTokens", usage.cached_input_tokens)?;
            obj.set("usage", u)?;
        }
        Ok(obj.into_value())
    }
}

struct IterResult {
    value: Option<JsStreamEvent>,
    done: bool,
}

impl IterResult {
    fn value(v: JsStreamEvent) -> Self {
        Self {
            value: Some(v),
            done: false,
        }
    }

    fn done() -> Self {
        Self {
            value: None,
            done: true,
        }
    }
}

impl<'js> IntoJs<'js> for IterResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("done", self.done)?;
        if let Some(v) = self.value {
            obj.set("value", v)?;
        }
        Ok(obj.into_value())
    }
}

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
