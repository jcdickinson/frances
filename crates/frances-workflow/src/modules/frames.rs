//! `frances:v1/frames` — transcript + frame classes.
//!
//! The transcript is an append-only sequence of frames. The user holds
//! a frame object after `transcript.push(frame)` and may call
//! `frame.write(text)` to extend its content. Frames stay writeable
//! until they're explicitly closed; the host supports many open blocks
//! at once, so a long-running [`ShellOutputFrame`] can keep streaming
//! while later [`MarkdownFrame`]s are pushed alongside it.
//!
//! Each writeable frame exposes:
//!   - `frame.write(text)` — append, throws if `closed`.
//!   - `frame.writable` — WHATWG `WritableStream` over the same sink.
//!   - `frame.close()` — emit [`HostFrame::Close`], flip `closed`.
//!   - `frame.autoclose` (default `true`) — when truthy, the writable's
//!     `close`/`abort` hook calls `frame.close()` so a finished pipe
//!     seals the frame automatically.
//!
//! For v1 there is exactly one transcript (the live binding behind the
//! `transcript` import). The `Transcript` class is exported as a type
//! for future rotation work; users can't construct one in v1.
//!
//! Wire contract: the host receives a [`HostFrame::Push`] for each new
//! frame, [`HostFrame::Append`] for each text delta, [`HostFrame::UpdateKind`]
//! for in-place metadata transitions (e.g. shell state going terminal),
//! and [`HostFrame::Close`] when a frame is sealed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    /// Where push/append events go.
    tx: UnboundedSender<HostFrame>,
}

impl FramesState {
    fn new(tx: UnboundedSender<HostFrame>) -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(0),
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
    let json_ctor = build_json_ctor(ctx, state.clone())?;
    let shell_output_ctor = build_shell_output_ctor(ctx, state)?;

    Ok((transcript, md_ctor, err_ctor, json_ctor, shell_output_ctor))
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
    if let Some(sh) = as_frame::<ShellOutputFrame>(&frame) {
        let new_id = state.assign_id();
        let borrow = sh.borrow();
        borrow.id.store(new_id, Ordering::Release);
        let kind = FrameKind::ShellOutput {
            state: load_shell_state(&borrow.state_atom),
            content: borrow.content.clone(),
        };
        let _ = state.tx.send(HostFrame::Push(FramePush {
            id: FrameId(new_id),
            kind,
        }));
        return Ok(());
    }
    throw_type(
        ctx,
        "transcript.push: expected a MarkdownFrame, ErrorFrame, JsonFrame, or ShellOutputFrame",
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
    /// Initial content captured at construction. `None` when the
    /// workflow omitted `content` (or passed `undefined` / `null`) —
    /// the frame is pushed with no body, and the client defers measure
    /// and render until the first `write` materialises it. Appends go
    /// straight to the host channel; we don't reconstruct the full text
    /// here.
    content: Option<String>,
    /// Optional speaker label. `None` ⇒ the host renders no prefix.
    sender: Option<String>,
    /// Flipped by [`close_frame`] (either explicit `.close()` or the
    /// writable's auto-close hook on the JS side). Subsequent writes
    /// throw; subsequent closes are no-ops.
    closed: AtomicBool,
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
                    append_text(&ctx, &b.state, &b.id, &b.closed, delta)
                },
            )?,
        )?;
        proto.set(
            "close",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, MarkdownFrame>>| {
                    let b = this.0.borrow();
                    close_frame(&ctx, &b.state, &b.id, &b.closed)
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
    closed: &AtomicBool,
    delta: String,
) -> JsResult<()> {
    let frame_id = id.load(Ordering::Acquire);
    if frame_id == 0 {
        return throw_type(
            ctx,
            "frame.write: frame has not been pushed onto the transcript yet",
        );
    }
    if closed.load(Ordering::Acquire) {
        return throw_type(ctx, "frame.write: frame is closed");
    }
    let _ = state.tx.send(HostFrame::Append {
        id: FrameId(frame_id),
        delta,
    });
    Ok(())
}

/// Mark the frame closed and emit [`HostFrame::Close`] exactly once.
/// Idempotent: a second call is a silent no-op. Throws if called
/// before the frame has been pushed.
fn close_frame<'js>(
    ctx: &Ctx<'js>,
    state: &Arc<FramesState>,
    id: &AtomicU64,
    closed: &AtomicBool,
) -> JsResult<()> {
    let frame_id = id.load(Ordering::Acquire);
    if frame_id == 0 {
        return throw_type(
            ctx,
            "frame.close: frame has not been pushed onto the transcript yet",
        );
    }
    if !closed.swap(true, Ordering::AcqRel) {
        let _ = state.tx.send(HostFrame::Close {
            id: FrameId(frame_id),
        });
    }
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
// ShellOutputFrame — streaming shell-command output with mutable state
// ---------------------------------------------------------------------

/// Compact wire encoding for [`crate::runtime::ShellState`] so we can
/// store it in an `AtomicU64`. `set_shell_state` / `load_shell_state`
/// translate to and from this format; nothing outside this module
/// should care about the layout.
const SHELL_STATE_RUNNING: u64 = 0;
const SHELL_STATE_SUCCESS: u64 = 1;
/// Exit codes pack the i32 into the low 32 bits with a discriminator
/// in the high half (`2 << 32 | code as u32`).
const SHELL_STATE_EXIT_TAG: u64 = 2;

fn encode_shell_state(state: &crate::runtime::ShellState) -> u64 {
    match state {
        crate::runtime::ShellState::Running => SHELL_STATE_RUNNING,
        crate::runtime::ShellState::Success => SHELL_STATE_SUCCESS,
        crate::runtime::ShellState::Exit(n) => (SHELL_STATE_EXIT_TAG << 32) | (*n as u32 as u64),
    }
}

fn load_shell_state(atom: &AtomicU64) -> crate::runtime::ShellState {
    let raw = atom.load(Ordering::Acquire);
    let tag = raw >> 32;
    if tag == SHELL_STATE_EXIT_TAG {
        crate::runtime::ShellState::Exit(raw as u32 as i32)
    } else if raw == SHELL_STATE_SUCCESS {
        crate::runtime::ShellState::Success
    } else {
        crate::runtime::ShellState::Running
    }
}

pub struct ShellOutputFrame {
    state: Arc<FramesState>,
    id: AtomicU64,
    /// Initial body captured at construction. Mirrors `MarkdownFrame.content`.
    content: String,
    /// Encoded [`crate::runtime::ShellState`]. Mutated by
    /// `.setState()` / `.success()` / `.exit()`.
    state_atom: AtomicU64,
    /// Same close lifecycle as `MarkdownFrame`.
    closed: AtomicBool,
}

impl<'js> Trace<'js> for ShellOutputFrame {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ShellOutputFrame {
    type Changed<'to> = ShellOutputFrame;
}

impl<'js> JsClass<'js> for ShellOutputFrame {
    const NAME: &'static str = "ShellOutputFrame";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "write",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputFrame>>, delta: String| {
                    let b = this.0.borrow();
                    append_text(&ctx, &b.state, &b.id, &b.closed, delta)
                },
            )?,
        )?;
        proto.set(
            "close",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputFrame>>| {
                    let b = this.0.borrow();
                    close_frame(&ctx, &b.state, &b.id, &b.closed)
                },
            )?,
        )?;
        // `frame.success()` and `frame.exit(code)` set the new state
        // on the wire via `HostFrame::UpdateKind`. They do NOT close
        // the frame — JS-side auto-close (writable's close hook) or
        // an explicit `frame.close()` is still required to seal the
        // block. Keeping these orthogonal lets the workflow stream a
        // tail of output between "got exit code" and "EOF on stdout".
        proto.set(
            "success",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputFrame>>| {
                    let b = this.0.borrow();
                    set_shell_state(
                        &ctx,
                        &b.state,
                        &b.id,
                        &b.state_atom,
                        crate::runtime::ShellState::Success,
                    )
                },
            )?,
        )?;
        proto.set(
            "exit",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputFrame>>, code: i32| {
                    let b = this.0.borrow();
                    set_shell_state(
                        &ctx,
                        &b.state,
                        &b.id,
                        &b.state_atom,
                        crate::runtime::ShellState::Exit(code),
                    )
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Update the frame's state and emit [`HostFrame::UpdateKind`].
/// Throws if the frame hasn't been pushed yet.
fn set_shell_state<'js>(
    ctx: &Ctx<'js>,
    state: &Arc<FramesState>,
    id: &AtomicU64,
    state_atom: &AtomicU64,
    new_state: crate::runtime::ShellState,
) -> JsResult<()> {
    let frame_id = id.load(Ordering::Acquire);
    if frame_id == 0 {
        return throw_type(
            ctx,
            "shellOutput.setState: frame has not been pushed onto the transcript yet",
        );
    }
    state_atom.store(encode_shell_state(&new_state), Ordering::Release);
    // Content is empty here — the daemon's UpdateKind handler emits a
    // no-text BlockDelta carrying just the new kind.
    let kind = FrameKind::ShellOutput {
        state: new_state,
        content: String::new(),
    };
    let _ = state.tx.send(HostFrame::UpdateKind {
        id: FrameId(frame_id),
        kind,
    });
    Ok(())
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
                    closed: AtomicBool::new(false),
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

fn build_shell_output_ctor<'js>(ctx: &Ctx<'js>, state: Arc<FramesState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ShellOutputFrame, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Value<'js>| {
            let content = parse_shell_output_arg(&ctx, &arg)?;
            Class::instance(
                ctx.clone(),
                ShellOutputFrame {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    content,
                    state_atom: AtomicU64::new(SHELL_STATE_RUNNING),
                    closed: AtomicBool::new(false),
                },
            )
        },
    )
}

/// Parse `new ShellOutputFrame({ content? })`. `content` is optional;
/// if absent it defaults to an empty string (workflows usually push
/// the frame first, then stream output via `.writable`).
fn parse_shell_output_arg<'js>(ctx: &Ctx<'js>, arg: &Value<'js>) -> JsResult<String> {
    if arg.is_undefined() || arg.is_null() {
        return Ok(String::new());
    }
    let Some(obj) = arg.as_object() else {
        return throw_type(
            ctx,
            "new ShellOutputFrame: expected { content?: string } or no argument",
        );
    };
    let content_val: Value<'js> = obj
        .get("content")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    if content_val.is_undefined() || content_val.is_null() {
        return Ok(String::new());
    }
    if let Some(s) = content_val.as_string() {
        s.to_string()
            .map_err(|_| throw_err(ctx, "new ShellOutputFrame: `content` must be UTF-8"))
    } else {
        Err(throw_err(
            ctx,
            "new ShellOutputFrame: `content` must be a string when present",
        ))
    }
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

/// Parse `new MarkdownFrame({ content?, sender? })`. `content` and
/// `sender` are both optional; absent / `undefined` / `null` map to
/// `None`. Anything other than a string for either field throws.
fn parse_markdown_arg<'js>(
    ctx: &Ctx<'js>,
    arg: &Value<'js>,
) -> JsResult<(Option<String>, Option<String>)> {
    if arg.is_undefined() || arg.is_null() {
        return Ok((None, None));
    }
    let Some(obj) = arg.as_object() else {
        return Err(throw_err(
            ctx,
            "new MarkdownFrame: expected { content?: string, sender?: string } or no argument",
        ));
    };
    let content_val: Value<'js> = obj
        .get("content")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    let content = if content_val.is_undefined() || content_val.is_null() {
        None
    } else if let Some(s) = content_val.as_string() {
        Some(
            s.to_string()
                .map_err(|_| throw_err(ctx, "new MarkdownFrame: `content` must be UTF-8"))?,
        )
    } else {
        return Err(throw_err(
            ctx,
            "new MarkdownFrame: `content` must be a string when present",
        ));
    };
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
