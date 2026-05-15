//! `frances:v1/frames` — transcript + frame classes.
//!
//! The transcript is an append-only sequence of frames. The user holds a
//! frame object after `transcript.push(frame)` and may call
//! `frame.write(text)` to extend its content — **but only while the
//! frame is the most recently pushed one**. Pushing a new frame seals
//! the previous frame; writing to a sealed frame throws.
//!
//! For v1 there is exactly one transcript (the live binding behind the
//! `transcript` import). The `Transcript` class is exported as a type
//! for future rotation work; users can't construct one in v1.
//!
//! Wire contract: the host receives a [`HostFrame::Push`] for each new
//! frame and a [`HostFrame::Append`] for every text append. The host is
//! responsible for opening/closing wire blocks; this module only emits
//! semantic events.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::{Class, Ctx, Exception, Function, JsLifetime, Object, Result as JsResult, Value};

type Ctor<'js> = Constructor<'js>;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::{FrameId, FrameKind, FramePush, HostFrame};

/// Shared state for the v1 frames surface. One per workflow invocation.
pub(crate) struct FramesState {
    /// Monotonically-increasing frame id. Bumped by `transcript.push`.
    next_id: AtomicU64,
    /// Id of the currently-mutable frame. Equal to whatever was assigned
    /// most recently; older frames compare unequal and reject `write`.
    active_id: AtomicU64,
    /// Where push/append events go.
    tx: UnboundedSender<HostFrame>,
}

impl FramesState {
    fn new(tx: UnboundedSender<HostFrame>) -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(0),
            active_id: AtomicU64::new(0),
            tx,
        })
    }

    fn assign_id(&self) -> u64 {
        // Pre-increment so id == 0 means "never pushed".
        self.next_id.fetch_add(1, Ordering::AcqRel) + 1
    }
}

pub(crate) fn build_frames<'js>(
    ctx: &Ctx<'js>,
    tx: UnboundedSender<HostFrame>,
) -> JsResult<(
    Class<'js, TranscriptHandle>,
    Ctor<'js>,
    Ctor<'js>,
    Ctor<'js>,
)> {
    let state = FramesState::new(tx);

    let transcript = Class::instance(
        ctx.clone(),
        TranscriptHandle {
            state: state.clone(),
        },
    )?;

    let md_ctor = build_markdown_ctor(ctx, state.clone())?;
    let err_ctor = build_error_ctor(ctx, state.clone())?;
    let json_ctor = build_json_ctor(ctx, state)?;

    Ok((transcript, md_ctor, err_ctor, json_ctor))
}

// ---------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------

/// `Transcript` — for v1 just `push(frame)`. Users get a singleton via
/// the `transcript` export; the constructor is intentionally absent.
pub struct TranscriptHandle {
    state: Arc<FramesState>,
}

impl<'js> Trace<'js> for TranscriptHandle {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for TranscriptHandle {
    type Changed<'to> = TranscriptHandle;
}

impl<'js> JsClass<'js> for TranscriptHandle {
    const NAME: &'static str = "Transcript";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "push",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, TranscriptHandle>>, frame: Value<'js>| {
                    push_frame(&ctx, &this.0, frame)
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

fn push_frame<'js>(
    ctx: &Ctx<'js>,
    handle: &Class<'js, TranscriptHandle>,
    frame: Value<'js>,
) -> JsResult<()> {
    let state = handle.borrow().state.clone();

    if let Some(md) = as_frame::<MarkdownFrame>(&frame) {
        let new_id = state.assign_id();
        let borrow = md.borrow();
        borrow.id.store(new_id, Ordering::Release);
        state.active_id.store(new_id, Ordering::Release);
        let kind = FrameKind::Markdown {
            content: borrow.content.clone(),
            sender: borrow.sender.clone(),
        };
        let _ = state.tx.send(HostFrame::Push(FramePush {
            id: FrameId(new_id),
            kind,
        }));
        return Ok(());
    }
    if let Some(err) = as_frame::<ErrorFrame>(&frame) {
        let new_id = state.assign_id();
        err.borrow().id.store(new_id, Ordering::Release);
        state.active_id.store(new_id, Ordering::Release);
        let kind = FrameKind::Error {
            content: err.borrow().content.clone(),
        };
        let _ = state.tx.send(HostFrame::Push(FramePush {
            id: FrameId(new_id),
            kind,
        }));
        return Ok(());
    }
    if let Some(json) = as_frame::<JsonFrame>(&frame) {
        let new_id = state.assign_id();
        json.borrow().id.store(new_id, Ordering::Release);
        state.active_id.store(new_id, Ordering::Release);
        let borrow = json.borrow();
        let kind = FrameKind::Json {
            tag: borrow.tag.clone(),
            value: borrow.value.clone(),
        };
        let _ = state.tx.send(HostFrame::Push(FramePush {
            id: FrameId(new_id),
            kind,
        }));
        return Ok(());
    }
    throw_type(
        ctx,
        "transcript.push: expected a MarkdownFrame, ErrorFrame, or JsonFrame",
    )
}

fn as_frame<'js, C: JsClass<'js>>(v: &Value<'js>) -> Option<Class<'js, C>> {
    v.as_object().and_then(Class::<C>::from_object)
}

// ---------------------------------------------------------------------
// MarkdownFrame / ErrorFrame — writeable text frames
// ---------------------------------------------------------------------

pub struct MarkdownFrame {
    state: Arc<FramesState>,
    /// Set by `transcript.push`; 0 means "not yet pushed".
    id: AtomicU64,
    /// Initial content captured at construction. Appends go straight to
    /// the host channel; we don't reconstruct the full text here.
    content: String,
    /// Optional speaker label. `None` ⇒ the host renders no prefix.
    sender: Option<String>,
}

impl<'js> Trace<'js> for MarkdownFrame {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for MarkdownFrame {
    type Changed<'to> = MarkdownFrame;
}

impl<'js> JsClass<'js> for MarkdownFrame {
    const NAME: &'static str = "MarkdownFrame";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "write",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, MarkdownFrame>>, delta: String| {
                    let b = this.0.borrow();
                    append_text(&ctx, &b.state, &b.id, delta)
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// `ErrorFrame` — one-shot error message. No `write` in v1; rendering
/// goes through `StreamFrame::Error` (a non-block message) on the host
/// side, so streaming-write semantics don't apply.
pub struct ErrorFrame {
    #[expect(
        dead_code,
        reason = "kept on the type for parity with MarkdownFrame; write support is a follow-up"
    )]
    state: Arc<FramesState>,
    id: AtomicU64,
    content: String,
}

impl<'js> Trace<'js> for ErrorFrame {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ErrorFrame {
    type Changed<'to> = ErrorFrame;
}

impl<'js> JsClass<'js> for ErrorFrame {
    const NAME: &'static str = "ErrorFrame";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        // No `write` — error frames are one-shot for v1.
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

fn append_text<'js>(
    ctx: &Ctx<'js>,
    state: &Arc<FramesState>,
    id: &AtomicU64,
    delta: String,
) -> JsResult<()> {
    let frame_id = id.load(Ordering::Acquire);
    let active = state.active_id.load(Ordering::Acquire);
    if frame_id == 0 {
        return throw_type(
            ctx,
            "frame.write: frame has not been pushed onto the transcript yet",
        );
    }
    if frame_id != active {
        return throw_type(
            ctx,
            "frame.write: this frame is no longer the active frame (a newer frame was pushed)",
        );
    }
    let _ = state.tx.send(HostFrame::Append {
        id: FrameId(frame_id),
        delta,
    });
    Ok(())
}

// ---------------------------------------------------------------------
// JsonFrame — single tagged value, no write
// ---------------------------------------------------------------------

pub struct JsonFrame {
    #[expect(
        dead_code,
        reason = "kept for future API parity with text frames; not read after construction"
    )]
    state: Arc<FramesState>,
    id: AtomicU64,
    tag: String,
    value: serde_json::Value,
}

impl<'js> Trace<'js> for JsonFrame {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for JsonFrame {
    type Changed<'to> = JsonFrame;
}

impl<'js> JsClass<'js> for JsonFrame {
    const NAME: &'static str = "JsonFrame";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        // Intentionally empty — no `write` here; JsonFrame is set at
        // construction.
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------
//
// We define our own constructor `Function`s so each one can close over
// the per-invocation `FramesState`. The frame classes' own `JsClass`
// impls return `None` from `constructor()` so user code can't do
// `new (constructor)()` without going through these.

fn build_markdown_ctor<'js>(ctx: &Ctx<'js>, state: Arc<FramesState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<MarkdownFrame, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Value<'js>| {
            let (content, sender) = parse_markdown_arg(&ctx, &arg)?;
            Class::instance(
                ctx.clone(),
                MarkdownFrame {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    content,
                    sender,
                },
            )
        },
    )
}

fn build_error_ctor<'js>(ctx: &Ctx<'js>, state: Arc<FramesState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ErrorFrame, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Value<'js>| {
            let content = parse_content_arg(&ctx, &arg, "ErrorFrame")?;
            Class::instance(
                ctx.clone(),
                ErrorFrame {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    content,
                },
            )
        },
    )
}

fn build_json_ctor<'js>(ctx: &Ctx<'js>, state: Arc<FramesState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<JsonFrame, _, _>(ctx.clone(), move |ctx: Ctx<'js>, arg: Value<'js>| {
        let Some(obj) = arg.as_object() else {
            return throw_type(&ctx, "new JsonFrame: expected { tag, value }");
        };
        let tag: String = obj
            .get("tag")
            .map_err(|_| throw_err(&ctx, "new JsonFrame: missing or non-string `tag`"))?;
        let value: Value<'js> = obj
            .get("value")
            .map_err(|_| throw_err(&ctx, "new JsonFrame: missing `value`"))?;

        // Round-trip the JS value through JSON to get a serde_json::Value.
        // Mirrors the old `workflow.frame.json` behaviour: silently drop
        // values that don't JSON-encode (use `null`).
        let json_str: String = ctx
            .json_stringify(value)?
            .and_then(|s| s.to_string().ok())
            .unwrap_or_else(|| "null".to_string());
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);

        Class::instance(
            ctx.clone(),
            JsonFrame {
                state: state.clone(),
                id: AtomicU64::new(0),
                tag,
                value: parsed,
            },
        )
    })
}

fn parse_content_arg<'js>(ctx: &Ctx<'js>, arg: &Value<'js>, name: &str) -> JsResult<String> {
    let Some(obj) = arg.as_object() else {
        return Err(throw_err(
            ctx,
            &format!("new {name}: expected {{ content: string }}"),
        ));
    };
    obj.get::<_, String>("content")
        .map_err(|_| throw_err(ctx, &format!("new {name}: `content` must be a string")))
}

/// Parse `new MarkdownFrame({ content, sender? })`. `sender` is
/// optional; if present it must be a string. Anything else throws.
fn parse_markdown_arg<'js>(ctx: &Ctx<'js>, arg: &Value<'js>) -> JsResult<(String, Option<String>)> {
    let Some(obj) = arg.as_object() else {
        return Err(throw_err(
            ctx,
            "new MarkdownFrame: expected { content: string, sender?: string }",
        ));
    };
    let content: String = obj
        .get("content")
        .map_err(|_| throw_err(ctx, "new MarkdownFrame: `content` must be a string"))?;
    let sender_val: Value<'js> = obj
        .get("sender")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    let sender =
        if sender_val.is_undefined() || sender_val.is_null() {
            None
        } else if let Some(s) = sender_val.as_string() {
            Some(s.to_string().map_err(|_| {
                throw_err(ctx, "new MarkdownFrame: `sender` must be a UTF-8 string")
            })?)
        } else {
            return Err(throw_err(
                ctx,
                "new MarkdownFrame: `sender` must be a string when present",
            ));
        };
    Ok((content, sender))
}

fn throw_type<'js, T>(ctx: &Ctx<'js>, message: &str) -> JsResult<T> {
    Err(throw_err(ctx, message))
}

fn throw_err<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    // Build a JS Error and throw it via rquickjs::Error::Exception.
    // `Exception::from_message` wraps the string with the right prototype.
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
