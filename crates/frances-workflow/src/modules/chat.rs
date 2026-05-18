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
//! const final = await r.completed;     // { text, tool_calls, usage }
//!
//! // Transient: never reads or writes chat history.
//! const scratch = new ChatSession({ model_intents: ["classify"], ephemeral: true });
//! ```
//!
//! Constructor options:
//! - `model_intents` (required, string[]) — config keys walked when
//!   resolving a model for each call.
//! - `ephemeral` (optional, bool, default `false`) — when `true`, the
//!   session never touches `chat_sessions` / `chat_messages`. The
//!   provider sees only the in-memory `pending` queue drained since
//!   the last `stream()` call.
//!
//! Roles in v1: `"system"`, `"user"`, `"tool"`. Pushing `"assistant"`
//! throws — assistant messages come from the model. `"system"` may only
//! be pushed before any `"user"` message; after the first user push the
//! system slot is locked. `"tool"` carries `{ call_id, content, is_error }`
//! and queues a tool-result that the next `run()` includes in history.
//!
//! Tools are attached to the session via `chat.tools`, a plain JS array.
//! Each entry needs `{ name, description, parameters, handler }`. The
//! Rust side reads `name`/`description`/`parameters` at every `stream()`
//! call and forwards them to the provider; `handler` is JS-only and the
//! workflow's loop is responsible for invoking it.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Array, Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::oneshot;

use frances_models_llm::chat::{
    ChatError, ChatSession as ChatSessionTrait, ChatSessionBuilder,
    ChatSessionManager as ChatSessionManagerTrait, ModelIntents, OwnedHistoryInput,
};
use frances_models_llm::wire::{StreamEvent, ToolCall, ToolDef, ToolFunction};

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
        move |ctx: Ctx<'js>, arg: Value<'js>| -> JsResult<Class<'js, ChatSessionJs<D>>> {
            let opts = parse_chat_options(&ctx, &arg)?;
            let builder = ChatSessionBuilder::new()
                .with_model_intents(opts.intents)
                .with_ephemeral(opts.ephemeral);
            let handle = deps.chat_session_manager().create(builder);
            let instance = Class::instance(
                ctx.clone(),
                ChatSessionJs::<D> {
                    handle,
                    deps: deps.clone(),
                    system_locked: AtomicBool::new(false),
                },
            )?;
            // `tools` is a fresh JS array on every instance. Workflows
            // mutate it via `chat.tools.push({ ... })`; the Rust side
            // snapshots it at each `stream()` call.
            instance.set("tools", Array::new(ctx.clone())?)?;
            Ok(instance)
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

struct ChatOptions {
    intents: ModelIntents,
    ephemeral: bool,
}

fn parse_chat_options<'js>(ctx: &Ctx<'js>, arg: &Value<'js>) -> JsResult<ChatOptions> {
    let Some(obj) = arg.as_object() else {
        return Err(throw(
            ctx,
            "new ChatSession: expected { model_intents: string[], ephemeral?: bool }",
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
    let mut intents: Vec<Cow<'static, str>> = Vec::with_capacity(arr.len());
    for item in arr.iter::<String>() {
        let s = item.map_err(|_| {
            throw(
                ctx,
                "new ChatSession: every `model_intents` entry must be a string",
            )
        })?;
        intents.push(Cow::Owned(s));
    }

    // `ephemeral` is optional. Missing and `undefined` both mean
    // `false`; anything else (`0`, `"true"`, etc.) is rejected so the
    // caller doesn't get silent truthiness.
    let ephemeral_val: Value<'js> = obj
        .get("ephemeral")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    let ephemeral = if ephemeral_val.is_undefined() || ephemeral_val.is_null() {
        false
    } else {
        ephemeral_val
            .as_bool()
            .ok_or_else(|| throw(ctx, "new ChatSession: `ephemeral` must be a boolean"))?
    };

    Ok(ChatOptions {
        intents: Cow::Owned(intents),
        ephemeral,
    })
}

fn push_message<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    session: &Class<'js, ChatSessionJs<D>>,
    msg: Value<'js>,
) -> JsResult<()> {
    let Some(obj) = msg.as_object() else {
        return Err(throw(ctx, "session.push: expected an object"));
    };
    let role_str: String = obj
        .get("role")
        .map_err(|_| throw(ctx, "session.push: missing or non-string `role`"))?;

    let borrow = session.borrow();
    let input = match role_str.as_str() {
        "user" => {
            borrow.system_locked.store(true, Ordering::Release);
            let content: String = obj
                .get("content")
                .map_err(|_| throw(ctx, "session.push: missing or non-string `content`"))?;
            OwnedHistoryInput::User { text: content }
        }
        "system" => {
            if borrow.system_locked.load(Ordering::Acquire) {
                return Err(throw(
                    ctx,
                    "session.push: role `system` is only valid before any user message has been pushed",
                ));
            }
            let content: String = obj
                .get("content")
                .map_err(|_| throw(ctx, "session.push: missing or non-string `content`"))?;
            OwnedHistoryInput::System { text: content }
        }
        "tool" => {
            let call_id: String = obj.get("call_id").map_err(|_| {
                throw(
                    ctx,
                    "session.push: tool message missing or non-string `call_id`",
                )
            })?;
            let content: String = obj.get("content").map_err(|_| {
                throw(
                    ctx,
                    "session.push: tool message missing or non-string `content`",
                )
            })?;
            let is_error: bool = obj.get("is_error").map_err(|_| {
                throw(
                    ctx,
                    "session.push: tool message missing or non-boolean `is_error`",
                )
            })?;
            OwnedHistoryInput::ToolResult {
                call_id,
                content,
                is_error,
            }
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
                &format!(
                    "session.push: unknown role `{other}` (expected `system`, `user`, or `tool`)"
                ),
            ));
        }
    };
    borrow.handle.push(input);
    Ok(())
}

/// Snapshot `chat.tools` into a `Vec<ToolDef>`. Validates each entry:
/// `name` and `description` strings, `parameters` an object, and no
/// duplicate names. The `handler` field is read by JS only; we don't
/// inspect it here.
fn snapshot_tools<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    session: &Class<'js, ChatSessionJs<D>>,
) -> JsResult<Vec<ToolDef>> {
    let tools_val: Value<'js> = session.get("tools").map_err(|_| {
        throw(
            ctx,
            "chat.stream: `chat.tools` missing — was the session constructed with `new ChatSession(...)`?",
        )
    })?;
    let Some(arr) = tools_val.as_array() else {
        return Err(throw(ctx, "chat.stream: `chat.tools` must be an array"));
    };

    let mut defs: Vec<ToolDef> = Vec::with_capacity(arr.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(arr.len());
    for (i, item) in arr.iter::<Value<'js>>().enumerate() {
        let item = item.map_err(|_| throw(ctx, &format!("chat.tools[{i}]: not readable")))?;
        let Some(obj) = item.as_object() else {
            return Err(throw(ctx, &format!("chat.tools[{i}]: expected an object")));
        };
        let name: String = obj.get("name").map_err(|_| {
            throw(
                ctx,
                &format!("chat.tools[{i}]: missing or non-string `name`"),
            )
        })?;
        if !seen.insert(name.clone()) {
            return Err(throw(
                ctx,
                &format!("chat.tools: duplicate tool name `{name}`"),
            ));
        }
        let description: String = obj.get("description").map_err(|_| {
            throw(
                ctx,
                &format!("chat.tools[{i}]: missing or non-string `description`"),
            )
        })?;
        let parameters_val: Value<'js> = obj.get("parameters").map_err(|_| {
            throw(
                ctx,
                &format!("chat.tools[{i}]: missing `parameters` (JSON schema object)"),
            )
        })?;
        if !parameters_val.is_object() {
            return Err(throw(
                ctx,
                &format!("chat.tools[{i}]: `parameters` must be an object"),
            ));
        }
        let parameters: JsonValue = from_json_value(&parameters_val).map_err(|e| {
            throw(
                ctx,
                &format!("chat.tools[{i}]: `parameters` not JSON-serialisable: {e}"),
            )
        })?;

        defs.push(ToolDef::Function(ToolFunction {
            name,
            description,
            parameters,
        }));
    }
    Ok(defs)
}

fn from_json_value<'js>(value: &Value<'js>) -> Result<JsonValue, String> {
    // `rquickjs` doesn't ship a JS-value → serde_json bridge by default,
    // so we walk the shape ourselves. JSON-shaped JS values only —
    // functions / symbols / undefined collapse to null per JSON convention.
    if let Some(s) = value.as_string() {
        let raw = s.to_string().map_err(|e| e.to_string())?;
        Ok(JsonValue::String(raw))
    } else if let Some(b) = value.as_bool() {
        Ok(JsonValue::Bool(b))
    } else if let Some(n) = value.as_int() {
        Ok(JsonValue::Number(n.into()))
    } else if let Some(n) = value.as_float() {
        serde_json::Number::from_f64(n)
            .map(JsonValue::Number)
            .ok_or_else(|| "non-finite number".to_owned())
    } else if value.is_null() || value.is_undefined() {
        Ok(JsonValue::Null)
    } else if let Some(arr) = value.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for v in arr.iter::<Value<'js>>() {
            let v = v.map_err(|e| e.to_string())?;
            out.push(from_json_value(&v)?);
        }
        Ok(JsonValue::Array(out))
    } else if let Some(obj) = value.as_object() {
        let mut out = serde_json::Map::new();
        for prop in obj.props::<String, Value<'js>>() {
            let (k, v) = prop.map_err(|e| e.to_string())?;
            out.insert(k, from_json_value(&v)?);
        }
        Ok(JsonValue::Object(out))
    } else {
        Ok(JsonValue::Null)
    }
}

fn json_value_into_js<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> JsResult<Value<'js>> {
    match value {
        JsonValue::Null => Ok(Value::new_null(ctx.clone())),
        JsonValue::Bool(b) => Ok(Value::new_bool(ctx.clone(), *b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Ok(i) = i32::try_from(i) {
                    Ok(Value::new_int(ctx.clone(), i))
                } else {
                    Ok(Value::new_float(ctx.clone(), i as f64))
                }
            } else if let Some(f) = n.as_f64() {
                Ok(Value::new_float(ctx.clone(), f))
            } else {
                Ok(Value::new_null(ctx.clone()))
            }
        }
        JsonValue::String(s) => s.clone().into_js(ctx),
        JsonValue::Array(items) => {
            let arr = Array::new(ctx.clone())?;
            for (i, v) in items.iter().enumerate() {
                arr.set(i, json_value_into_js(ctx, v)?)?;
            }
            Ok(arr.into_value())
        }
        JsonValue::Object(map) => {
            let obj = Object::new(ctx.clone())?;
            for (k, v) in map {
                obj.set(k.as_str(), json_value_into_js(ctx, v)?)?;
            }
            Ok(obj.into_value())
        }
    }
}

/// Synchronously kicks off a provider stream and returns an object
/// `{ events, completed }`.
///
/// `events` is an async iterable that yields stream events as they
/// arrive. `completed` is a Promise resolving to `{ text, tool_calls, usage }`
/// once `ChatSession::run` has settled. The rs-side `run` writes the
/// assistant primitive to history, so the next `s.stream()` call sees
/// the prior turn via `loaded_history`.
fn start_stream<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    session: &Class<'js, ChatSessionJs<D>>,
) -> JsResult<Value<'js>> {
    let tool_defs = snapshot_tools::<D>(ctx, session)?;
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
            let result = handle.run(env, tool_defs, None, on_event).await;
            let usage = usage_capture.lock().take();
            drop(event_tx);
            let mapped = result.map(|outcome| CompletedJs {
                text: outcome.text,
                tool_calls: outcome.tool_calls,
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
                obj.set("usage", usage_into_js(ctx, &usage)?)?;
            }
            StreamEvent::ToolCall(call) => {
                obj.set("type", "tool_call")?;
                obj.set("id", call.id)?;
                obj.set("name", call.name)?;
                obj.set("arguments", json_value_into_js(ctx, &call.arguments)?)?;
            }
            // Provider-internal cache primitive; never surfaces to JS.
            StreamEvent::History(_) => {
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
    tool_calls: Vec<ToolCall>,
    usage: Option<frances_models_llm::wire::Usage>,
}

impl<'js> IntoJs<'js> for CompletedJs {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("text", self.text)?;

        let calls = Array::new(ctx.clone())?;
        for (i, call) in self.tool_calls.iter().enumerate() {
            let entry = Object::new(ctx.clone())?;
            entry.set("id", call.id.clone())?;
            entry.set("name", call.name.clone())?;
            entry.set("arguments", json_value_into_js(ctx, &call.arguments)?)?;
            calls.set(i, entry)?;
        }
        obj.set("tool_calls", calls)?;

        if let Some(usage) = self.usage {
            obj.set("usage", usage_into_js(ctx, &usage)?)?;
        }
        Ok(obj.into_value())
    }
}

fn usage_into_js<'js>(
    ctx: &Ctx<'js>,
    usage: &frances_models_llm::wire::Usage,
) -> JsResult<Object<'js>> {
    let u = Object::new(ctx.clone())?;
    u.set("promptTokens", usage.prompt_tokens)?;
    u.set("completionTokens", usage.completion_tokens)?;
    u.set("totalTokens", usage.total_tokens)?;
    u.set("cachedInputTokens", usage.cached_input_tokens)?;
    Ok(u)
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
