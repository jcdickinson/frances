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
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use frances_core::Truncated;
use frances_models_llm::chat::{
    ChatCheckpoint, ChatError, ChatSession as ChatSessionTrait, ChatSessionBuilder,
    ChatSessionManager as ChatSessionManagerTrait, CompleteRequest, Demand, EnforceError,
    ModelIntents, OwnedHistoryInput, RowId,
};
use frances_models_llm::{HistoryInput, StreamEvent, ToolCall, ToolDef, ToolFunction};

use super::{get_or_undefined, throw_js as throw};
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
    usage_tx: UnboundedSender<frances_models_llm::Usage>,
) -> JsResult<(Constructor<'js>, Function<'js>)> {
    let ctor_usage_tx = usage_tx.clone();
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
                    usage_tx: ctor_usage_tx.clone(),
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
        |ctx: Ctx<'js>,
         this: This<Class<'js, ChatSessionJs<D>>>,
         opts: rquickjs::function::Opt<Value<'js>>|
         -> JsResult<Value<'js>> {
            let max_tool_calls = parse_stream_opts(&ctx, opts.0.as_ref())?;
            start_stream::<D>(&ctx, &this.0, max_tool_calls)
        },
    )?;

    Ok((ctor, inner_stream))
}

/// Build the standalone `complete` export: a one-shot, ephemeral LLM
/// call that bundles the constructor args (`intents`) with the request
/// args. Routes to the manager's `complete_enforced` when a tool call is
/// demanded (`toolChoice` names a tool, or `requireToolCall` forces any),
/// else plain `complete`. Returns a promise resolving to
/// `{ text, tool_calls }`. No streaming, no history, no cancellation
/// sentinel.
pub(crate) fn build_complete_fn<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Value<'js>| -> JsResult<Value<'js>> {
            let CompleteOpts {
                intents,
                messages,
                tools,
                demand,
                retries,
                max_tool_calls,
            } = parse_complete_opts(&ctx, &arg)?;
            let manager = deps.chat_session_manager().clone();
            let env = deps.current_env();

            let promised = Promised::from(async move {
                let session_id = uuid::Uuid::new_v4().to_string();
                let intents_ref: Vec<&str> = intents.iter().map(String::as_str).collect();
                let new_inputs: Vec<HistoryInput<'_>> = messages
                    .iter()
                    .map(OwnedHistoryInput::as_borrowed)
                    .collect();
                let req = CompleteRequest {
                    intents: &intents_ref,
                    session_id: &session_id,
                    env: env.as_ref(),
                    history: &[],
                    new_inputs: &new_inputs,
                    tools: &tools,
                    // `complete_enforced` drives tool_choice from the demand;
                    // the plain path forces nothing.
                    tool_choice: None,
                    cancel: CancellationToken::new(),
                    max_tool_calls,
                };
                let result = match demand {
                    Some(demand) => manager.complete_enforced(req, demand, retries).await,
                    // `complete` fails with `ChatError`; lift it into the
                    // shared `EnforceError` (it has `#[from] ChatError`).
                    None => manager.complete(req).await.map_err(EnforceError::from),
                };
                match result {
                    Ok(outcome) => CompletionResult::Completed(CompletedJs {
                        text: outcome.text,
                        tool_calls: outcome.tool_calls,
                        usage: None,
                    }),
                    Err(e) => CompletionResult::Failed(e),
                }
            });
            promised.into_js(&ctx)
        },
    )
}

struct CompleteOpts {
    intents: Vec<String>,
    messages: Vec<OwnedHistoryInput>,
    tools: Vec<ToolDef>,
    /// `Some` ⇒ enforce a tool call (route to `complete_enforced`).
    demand: Option<Demand>,
    retries: u8,
    max_tool_calls: Option<usize>,
}

/// Parse `complete({ intents, input, tools?, requireToolCall?, toolChoice?, retries?, maxToolCalls? })`.
fn parse_complete_opts<'js>(ctx: &Ctx<'js>, arg: &Value<'js>) -> JsResult<CompleteOpts> {
    let Some(obj) = arg.as_object() else {
        return Err(throw(
            ctx,
            "complete: expected an options object { intents, input, tools?, requireToolCall?, toolChoice?, retries?, maxToolCalls? }",
        ));
    };

    // intents: string[]
    let intents_val: Value<'js> = obj
        .get("intents")
        .map_err(|_| throw(ctx, "complete: missing `intents`"))?;
    let Some(intents_arr) = intents_val.as_array() else {
        return Err(throw(
            ctx,
            "complete: `intents` must be an array of strings",
        ));
    };
    let mut intents: Vec<String> = Vec::with_capacity(intents_arr.len());
    for item in intents_arr.iter::<String>() {
        intents.push(
            item.map_err(|_| throw(ctx, "complete: every `intents` entry must be a string"))?,
        );
    }
    if intents.is_empty() {
        return Err(throw(ctx, "complete: `intents` must be non-empty"));
    }

    // input: { role, content }[]
    let input_val: Value<'js> = obj
        .get("input")
        .map_err(|_| throw(ctx, "complete: missing `input`"))?;
    let Some(input_arr) = input_val.as_array() else {
        return Err(throw(
            ctx,
            "complete: `input` must be an array of { role, content } messages",
        ));
    };
    let mut messages: Vec<OwnedHistoryInput> = Vec::with_capacity(input_arr.len());
    for (i, item) in input_arr.iter::<Value<'js>>().enumerate() {
        let item = item.map_err(|_| throw(ctx, &format!("complete: input[{i}] not readable")))?;
        messages.push(parse_message_obj(ctx, &item, i)?);
    }

    // tools?: [...]
    let tools_val: Value<'js> = get_or_undefined(ctx, obj, "tools");
    let tools = if tools_val.is_undefined() || tools_val.is_null() {
        Vec::new()
    } else {
        let Some(arr) = tools_val.as_array() else {
            return Err(throw(ctx, "complete: `tools` must be an array"));
        };
        parse_tool_defs(ctx, arr, "complete: tools")?
    };

    // requireToolCall? + toolChoice? → demand. A named `toolChoice` forces
    // that tool; `requireToolCall: true` forces any tool; neither ⇒ plain.
    let require_tool_call = get_optional_bool(ctx, obj, "requireToolCall")?.unwrap_or(false);
    let tool_choice_name = get_optional_string(ctx, obj, "toolChoice")?;
    let demand = match (tool_choice_name, require_tool_call) {
        (Some(name), _) => Some(Demand::Function(name)),
        (None, true) => Some(Demand::Required),
        (None, false) => None,
    };

    let retries = get_optional_u32(ctx, obj, "retries")?.unwrap_or(1) as u8;
    let max_tool_calls = get_optional_u32(ctx, obj, "maxToolCalls")?.map(|n| n as usize);

    Ok(CompleteOpts {
        intents,
        messages,
        tools,
        demand,
        retries,
        max_tool_calls,
    })
}

/// Parse one `{ role, content[, call_id, is_error] }` message into an
/// `OwnedHistoryInput`. `assistant` is rejected (model-only); other
/// unknown roles error.
fn parse_message_obj<'js>(
    ctx: &Ctx<'js>,
    msg: &Value<'js>,
    i: usize,
) -> JsResult<OwnedHistoryInput> {
    let Some(obj) = msg.as_object() else {
        return Err(throw(
            ctx,
            &format!("complete: input[{i}] must be an object"),
        ));
    };
    let role: String = obj
        .get("role")
        .map_err(|_| throw(ctx, &format!("complete: input[{i}] missing string `role`")))?;
    let content = |label: &str| -> JsResult<String> {
        obj.get("content").map_err(|_| {
            throw(
                ctx,
                &format!("complete: input[{i}] ({label}) missing string `content`"),
            )
        })
    };
    match role.as_str() {
        "user" => Ok(OwnedHistoryInput::User {
            text: content("user")?,
        }),
        "system" => Ok(OwnedHistoryInput::System {
            text: content("system")?,
        }),
        "tool" => {
            let call_id: String = obj.get("call_id").map_err(|_| {
                throw(
                    ctx,
                    &format!("complete: input[{i}] (tool) missing `call_id`"),
                )
            })?;
            let is_error: bool = obj.get("is_error").map_err(|_| {
                throw(
                    ctx,
                    &format!("complete: input[{i}] (tool) missing `is_error`"),
                )
            })?;
            Ok(OwnedHistoryInput::ToolResult {
                call_id,
                content: content("tool")?,
                is_error,
            })
        }
        "assistant" => Err(throw(
            ctx,
            &format!("complete: input[{i}] role `assistant` is model-only, not an input"),
        )),
        other => Err(throw(
            ctx,
            &format!("complete: input[{i}] unknown role `{other}` (expected system/user/tool)"),
        )),
    }
}

fn get_optional_bool<'js>(ctx: &Ctx<'js>, obj: &Object<'js>, key: &str) -> JsResult<Option<bool>> {
    let v: Value<'js> = get_or_undefined(ctx, obj, key);
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    v.as_bool()
        .map(Some)
        .ok_or_else(|| throw(ctx, &format!("complete: `{key}` must be a boolean")))
}

fn get_optional_string<'js>(
    ctx: &Ctx<'js>,
    obj: &Object<'js>,
    key: &str,
) -> JsResult<Option<String>> {
    let v: Value<'js> = get_or_undefined(ctx, obj, key);
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let Some(s) = v.as_string() else {
        return Err(throw(ctx, &format!("complete: `{key}` must be a string")));
    };
    s.to_string()
        .map(Some)
        .map_err(|e| throw(ctx, &format!("complete: `{key}` conversion: {e}")))
}

fn get_optional_u32<'js>(ctx: &Ctx<'js>, obj: &Object<'js>, key: &str) -> JsResult<Option<u32>> {
    let v: Value<'js> = get_or_undefined(ctx, obj, key);
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let n = v
        .as_number()
        .ok_or_else(|| throw(ctx, &format!("complete: `{key}` must be a number")))?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
        return Err(throw(
            ctx,
            &format!("complete: `{key}` must be a non-negative integer"),
        ));
    }
    Ok(Some(n as u32))
}

pub struct ChatSessionJs<D: WorkflowDeps> {
    handle: Session<D>,
    /// Captured at construction so each `stream()` call can resolve env
    /// vars (auth, etc.) against the latest invocation's environment
    /// snapshot.
    deps: D,
    /// Side-channel for emitting `Usage` when the LLM stream reports
    /// token usage. Independent of the JS-visible event stream so
    /// workflows don't have to remember to forward it.
    usage_tx: UnboundedSender<frances_models_llm::Usage>,
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

        // `checkpoint()` snapshots the session's history position; the
        // returned token is handed back to `rollback(token)` to discard
        // everything appended since (e.g. a partial assistant round
        // whose tool calls never got results after an interrupt).
        proto.set(
            "checkpoint",
            Function::new(ctx.clone(), |this: This<Class<'js, ChatSessionJs<D>>>| {
                let handle = this.0.borrow().handle.clone();
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    CheckpointResult(handle.checkpoint().await)
                }))
            })?,
        )?;

        proto.set(
            "rollback",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ChatSessionJs<D>>>, token: Value<'js>| {
                    let cp = parse_checkpoint(&ctx, &token)?;
                    let handle = this.0.borrow().handle.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        UnitResult(handle.rollback(cp).await)
                    }))
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
    let ephemeral_val: Value<'js> = get_or_undefined(ctx, obj, "ephemeral");
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

/// Parse the optional `{ maxToolCalls }` object that JS passes to
/// `_innerStream.call(chat, opts)`. Returns `None` when the option is
/// missing or the whole opts argument is undefined; throws a JS
/// `TypeError`-shaped exception for non-objects or invalid values.
fn parse_stream_opts<'js>(ctx: &Ctx<'js>, opts: Option<&Value<'js>>) -> JsResult<Option<usize>> {
    let Some(opts) = opts else { return Ok(None) };
    if opts.is_undefined() || opts.is_null() {
        return Ok(None);
    }
    let Some(obj) = opts.as_object() else {
        return Err(throw(
            ctx,
            "chat.stream: expected an options object or `undefined`",
        ));
    };
    let val: Value<'js> = get_or_undefined(ctx, obj, "maxToolCalls");
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    let n = val
        .as_number()
        .ok_or_else(|| throw(ctx, "chat.stream: `maxToolCalls` must be a number"))?;
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
        return Err(throw(
            ctx,
            "chat.stream: `maxToolCalls` must be a non-negative integer",
        ));
    }
    Ok(Some(n as usize))
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
            if is_error {
                tracing::warn!(
                    call_id = %call_id,
                    content = %Truncated::<100>::new(content.as_str()),
                    "tool call returned is_error",
                );
            }
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
    parse_tool_defs(ctx, arr, "chat.tools")
}

/// Parse a JS array of `{ name, description, parameters }` tool entries
/// into `Vec<ToolDef>`. `label` names the source for error messages
/// (`chat.tools` for the streaming path, `complete: tools` for the
/// one-shot export). The `handler` field is JS-only; not inspected here.
fn parse_tool_defs<'js>(ctx: &Ctx<'js>, arr: &Array<'js>, label: &str) -> JsResult<Vec<ToolDef>> {
    let mut defs: Vec<ToolDef> = Vec::with_capacity(arr.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(arr.len());
    for (i, item) in arr.iter::<Value<'js>>().enumerate() {
        let item = item.map_err(|_| throw(ctx, &format!("{label}[{i}]: not readable")))?;
        let Some(obj) = item.as_object() else {
            return Err(throw(ctx, &format!("{label}[{i}]: expected an object")));
        };
        let name: String = obj
            .get("name")
            .map_err(|_| throw(ctx, &format!("{label}[{i}]: missing or non-string `name`")))?;
        if !seen.insert(name.clone()) {
            return Err(throw(
                ctx,
                &format!("{label}: duplicate tool name `{name}`"),
            ));
        }
        let description: String = obj.get("description").map_err(|_| {
            throw(
                ctx,
                &format!("{label}[{i}]: missing or non-string `description`"),
            )
        })?;
        let parameters_val: Value<'js> = obj.get("parameters").map_err(|_| {
            throw(
                ctx,
                &format!("{label}[{i}]: missing `parameters` (JSON schema object)"),
            )
        })?;
        if !parameters_val.is_object() {
            return Err(throw(
                ctx,
                &format!("{label}[{i}]: `parameters` must be an object"),
            ));
        }
        let parameters: JsonValue = super::rquickjs_to_json(&parameters_val).map_err(|e| {
            throw(
                ctx,
                &format!("{label}[{i}]: `parameters` not JSON-serialisable: {e}"),
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
    max_tool_calls: Option<usize>,
) -> JsResult<Value<'js>> {
    let tool_defs = snapshot_tools::<D>(ctx, session)?;
    let (handle, env, usage_tx) = {
        let borrow = session.borrow();
        (
            borrow.handle.clone(),
            borrow.deps.current_env(),
            borrow.usage_tx.clone(),
        )
    };

    let (event_tx, event_rx) = mpsc::unbounded_channel::<StreamEvent>();
    let (completed_tx, completed_rx) = oneshot::channel::<Result<CompletedJs, ChatError>>();

    // Mirror the latest `Usage` event into the completion result so
    // `pipeTo` callers (who don't iterate `r.events`) still see it.
    let usage_capture: Arc<parking_lot::Mutex<Option<frances_models_llm::Usage>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();

    tokio::spawn({
        let usage_capture = usage_capture.clone();
        async move {
            let tx_for_callback = event_tx.clone();
            let usage_for_callback = usage_capture.clone();
            let usage_tx_for_callback = usage_tx;
            let on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send> =
                Box::new(move |event| {
                    if let StreamEvent::Usage(u) = &event {
                        *usage_for_callback.lock() = Some(u.clone());
                        // Side-channel to the host so the TUI footer
                        // updates. Best-effort: if the host is gone the
                        // workflow is shutting down anyway.
                        let _ = usage_tx_for_callback.send(u.clone());
                    }
                    let _ = tx_for_callback.send(event);
                    Ok(())
                });
            let result = handle
                .run(
                    env,
                    tool_defs,
                    None,
                    cancel_for_task,
                    max_tool_calls,
                    on_event,
                )
                .await;
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

    let cancel_class = Class::instance(ctx.clone(), CancelHandle { token: cancel })?;

    let completed_promise = Promised::from(async move {
        match completed_rx.await {
            Ok(Ok(c)) => CompletionResult::Completed(c),
            // `chat.js` recognizes the structurally-tagged cancellation
            // rejection and rethrows the user's abort reason, so all three
            // observables (`events`, `text`, `completed`) reject uniformly.
            Ok(Err(ChatError::Cancelled)) => CompletionResult::Cancelled,
            Ok(Err(e)) => CompletionResult::Failed(EnforceError::Provider(e)),
            // The driver task dropped its sender without sending — it
            // panicked or the runtime is shutting down. Synthesize a
            // provider-shaped error so the JS side still rejects.
            Err(_) => CompletionResult::Failed(EnforceError::Provider(ChatError::Provider {
                provider_id: "chat".to_owned(),
                source: "stream task aborted before completion".into(),
            })),
        }
    });

    let obj = Object::new(ctx.clone())?;
    obj.set("events", events_class)?;
    obj.set("completed", completed_promise)?;
    obj.set("cancel", cancel_class)?;
    Ok(obj.into_value())
}

/// JS-held handle that fires a `CancellationToken` either explicitly (via
/// `fire()`) or implicitly on GC. The Rust task spawned by `start_stream`
/// holds a clone of the same token; firing aborts its in-flight provider
/// stream. Mirrors the `SleepToken` precedent in
/// `crates/frances-workflow/src/modules/io.rs`.
pub struct CancelHandle {
    token: CancellationToken,
}

impl Drop for CancelHandle {
    fn drop(&mut self) {
        // Idempotent: a cancel after the stream has settled is a no-op.
        self.token.cancel();
    }
}

impl<'js> Trace<'js> for CancelHandle {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for CancelHandle {
    type Changed<'to> = CancelHandle;
}

impl<'js> JsClass<'js> for CancelHandle {
    const NAME: &'static str = "CancelHandle";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "fire",
            Function::new(ctx.clone(), |this: This<Class<'js, CancelHandle>>| {
                this.0.borrow().token.cancel();
                Ok::<_, rquickjs::Error>(())
            })?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
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
            StreamEvent::ReasoningDelta(delta) => {
                obj.set("type", "reasoning")?;
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

enum CompletionResult {
    Completed(CompletedJs),
    Cancelled,
    Failed(EnforceError),
}

impl<'js> IntoJs<'js> for CompletionResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self {
            CompletionResult::Completed(c) => c.into_js(ctx),
            CompletionResult::Cancelled => Err(throw_cancelled(ctx)),
            CompletionResult::Failed(e) => Err(throw(ctx, &e.to_string())),
        }
    }
}

struct CompletedJs {
    text: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<frances_models_llm::Usage>,
}

impl<'js> IntoJs<'js> for CompletedJs {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("text", self.text)?;

        // Each call carries an optional `error` (set by the chat layer when
        // its arguments failed the called tool's schema). Surfaced as
        // `error` + `expectedSchema` so the JS dispatch loop hands back an
        // error result for a bad call instead of invoking its handler.
        let calls = Array::new(ctx.clone())?;
        for (i, call) in self.tool_calls.iter().enumerate() {
            let entry = Object::new(ctx.clone())?;
            entry.set("id", call.id.clone())?;
            entry.set("name", call.name.clone())?;
            entry.set("arguments", json_value_into_js(ctx, &call.arguments)?)?;
            match &call.error {
                Some(e) => {
                    entry.set("error", e.message.clone())?;
                    entry.set(
                        "expectedSchema",
                        json_value_into_js(ctx, &e.expected_schema)?,
                    )?;
                }
                None => {
                    entry.set("error", Value::new_null(ctx.clone()))?;
                    entry.set("expectedSchema", Value::new_null(ctx.clone()))?;
                }
            }
            calls.set(i, entry)?;
        }
        obj.set("tool_calls", calls)?;

        if let Some(usage) = self.usage {
            obj.set("usage", usage_into_js(ctx, &usage)?)?;
        }
        Ok(obj.into_value())
    }
}

fn usage_into_js<'js>(ctx: &Ctx<'js>, usage: &frances_models_llm::Usage) -> JsResult<Object<'js>> {
    let u = Object::new(ctx.clone())?;
    u.set("promptTokens", usage.prompt_tokens)?;
    u.set("completionTokens", usage.completion_tokens)?;
    u.set("totalTokens", usage.total_tokens)?;
    u.set("cachedInputTokens", usage.cached_input_tokens)?;
    Ok(u)
}

/// Promise payload for `chat.checkpoint()`. Resolves to an opaque token
/// `{ persisted: number | null, pendingLen: number }` (round-tripped
/// back through `parse_checkpoint` in `rollback`), or rejects with the
/// error message.
struct CheckpointResult(Result<ChatCheckpoint, ChatError>);

impl<'js> IntoJs<'js> for CheckpointResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(cp) => {
                let obj = Object::new(ctx.clone())?;
                match cp.persisted {
                    Some(row) => obj.set("persisted", row.0)?,
                    None => obj.set("persisted", Value::new_null(ctx.clone()))?,
                }
                obj.set("pendingLen", cp.pending_len as i64)?;
                Ok(obj.into_value())
            }
            Err(e) => Err(throw(ctx, &format!("chat.checkpoint: {e}"))),
        }
    }
}

/// Promise payload that resolves to `undefined` or rejects with the
/// error message. Used by `chat.rollback()`.
struct UnitResult(Result<(), ChatError>);

impl<'js> IntoJs<'js> for UnitResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(()) => Ok(Value::new_undefined(ctx.clone())),
            Err(e) => Err(throw(ctx, &format!("chat.rollback: {e}"))),
        }
    }
}

/// Parse the opaque token produced by `CheckpointResult` back into a
/// [`ChatCheckpoint`].
fn parse_checkpoint<'js>(ctx: &Ctx<'js>, token: &Value<'js>) -> JsResult<ChatCheckpoint> {
    let Some(obj) = token.as_object() else {
        return Err(throw(
            ctx,
            "chat.rollback: expected a checkpoint token from chat.checkpoint()",
        ));
    };
    let persisted_val: Value<'js> = obj
        .get("persisted")
        .map_err(|_| throw(ctx, "chat.rollback: token missing `persisted`"))?;
    let persisted = if persisted_val.is_null() || persisted_val.is_undefined() {
        None
    } else {
        let n: i64 = persisted_val
            .as_int()
            .map(i64::from)
            .or_else(|| persisted_val.as_float().map(|f| f as i64))
            .ok_or_else(|| throw(ctx, "chat.rollback: `persisted` must be a number or null"))?;
        Some(RowId(n))
    };
    let pending_len_val: Value<'js> = obj
        .get("pendingLen")
        .map_err(|_| throw(ctx, "chat.rollback: token missing `pendingLen`"))?;
    let pending_len = pending_len_val
        .as_int()
        .map(|i| i as usize)
        .or_else(|| pending_len_val.as_float().map(|f| f as usize))
        .ok_or_else(|| throw(ctx, "chat.rollback: `pendingLen` must be a number"))?;
    Ok(ChatCheckpoint {
        persisted,
        pending_len,
    })
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

/// Rejects with an `Error` carrying a `cancelled === true` flag so `chat.js`
/// recognizes a cancelled stream structurally rather than by message string.
fn throw_cancelled<'js>(ctx: &Ctx<'js>) -> rquickjs::Error {
    let exc = match Exception::from_message(ctx.clone(), "chat stream cancelled") {
        Ok(exc) => exc,
        Err(e) => return e,
    };
    if let Err(e) = exc.as_object().set("cancelled", true) {
        return e;
    }
    exc.throw()
}
