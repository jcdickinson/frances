//! `frances:v1/sections` — transcript + frame classes.
//!
//! The transcript is an append-only sequence of frames. The user holds
//! a frame object after `transcript.push(frame)` and may call
//! `frame.write(text)` to extend its content. Frames stay writeable
//! until they're explicitly closed; the host supports many open blocks
//! at once, so a long-running [`ShellOutputSection`] can keep streaming
//! while later [`MarkdownSection`]s are pushed alongside it.
//!
//! Each writeable frame exposes:
//!   - `frame.write(text)` — append, throws if `closed`.
//!   - `frame.writable` — WHATWG `WritableStream` over the same sink.
//!   - `frame.close()` — emit [`SectionTranscript::Close`], flip `closed`,
//!     return `this` so `new MarkdownSection(...).close()` chains.
//!   - `frame.autoclose` (default `true`) — when truthy, the writable's
//!     `close`/`abort` hook calls `frame.close()` so a finished pipe
//!     seals the frame automatically.
//!
//! Constructors also accept `{ ..., closed: true }` to pre-seal a
//! frame: `transcript.push` then emits a `Close` immediately after the
//! `Push`, which is the convenient way to write one-shot frames like
//! a greeting or an echoed user message.
//!
//! For v1 there is exactly one transcript (the live binding behind the
//! `transcript` import). The `Transcript` class is exported as a type
//! for future rotation work; users can't construct one in v1.
//!
//! Wire contract: the host receives a [`SectionTranscript::Set`] for each
//! frame (the first creates it; a later one re-upserts kind + metadata,
//! e.g. shell state going terminal), [`SectionTranscript::Append`] for each
//! text delta, and [`SectionTranscript::Close`] when a frame is sealed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use frances_edit::DiffOp;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, Opt, This};
use rquickjs::{Class, Ctx, Exception, Function, JsLifetime, Object, Result as JsResult, Value};

type Ctor<'js> = Constructor<'js>;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::{SectionId, SectionKind, SectionSpec, SectionTranscript, Source};

/// Shared state for the v1 frames surface. One per workflow invocation.
pub(crate) struct SectionsState {
    /// Monotonically-increasing frame id. Bumped by `transcript.push`.
    next_id: AtomicU64,
    /// Where push/append events go.
    tx: UnboundedSender<SectionTranscript>,
}

impl SectionsState {
    fn new(tx: UnboundedSender<SectionTranscript>) -> Arc<Self> {
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

pub(crate) type BuiltSections<'js> = (
    Class<'js, TranscriptHandle>,
    Ctor<'js>, // MarkdownSection
    Ctor<'js>, // ErrorSection
    Ctor<'js>, // JsonSection
    Ctor<'js>, // ShellOutputSection
    Ctor<'js>, // ReasoningSection
    Ctor<'js>, // ToolUseSection
    Ctor<'js>, // DiffSection
);

pub(crate) fn build_sections<'js>(
    ctx: &Ctx<'js>,
    tx: UnboundedSender<SectionTranscript>,
) -> JsResult<BuiltSections<'js>> {
    let state = SectionsState::new(tx);

    let transcript = Class::instance(
        ctx.clone(),
        TranscriptHandle {
            state: state.clone(),
        },
    )?;

    let md_ctor = build_markdown_ctor(ctx, state.clone())?;
    let err_ctor = build_error_ctor(ctx, state.clone())?;
    let json_ctor = build_json_ctor(ctx, state.clone())?;
    let shell_output_ctor = build_shell_output_ctor(ctx, state.clone())?;
    let thought_ctor = build_thought_ctor(ctx, state.clone())?;
    let tool_use_ctor = build_tool_use_ctor(ctx, state.clone())?;
    let diff_ctor = build_diff_ctor(ctx, state)?;

    Ok((
        transcript,
        md_ctor,
        err_ctor,
        json_ctor,
        shell_output_ctor,
        thought_ctor,
        tool_use_ctor,
        diff_ctor,
    ))
}

// ---------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------

/// `Transcript` — for v1 just `push(frame)`. Users get a singleton via
/// the `transcript` export; the constructor is intentionally absent.
pub struct TranscriptHandle {
    state: Arc<SectionsState>,
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
                |ctx: Ctx<'js>, this: This<Class<'js, TranscriptHandle>>, section: Value<'js>| {
                    push_section(&ctx, &this.0, section)
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

fn push_section<'js>(
    ctx: &Ctx<'js>,
    handle: &Class<'js, TranscriptHandle>,
    section: Value<'js>,
) -> JsResult<()> {
    let state = handle.borrow().state.clone();

    if let Some(md) = as_section::<MarkdownSection>(&section) {
        let new_id = state.assign_id();
        let borrow = md.borrow();
        borrow.id.store(new_id, Ordering::Release);
        let section = SectionSpec {
            kind: SectionKind::Markdown {
                source: borrow.source,
            },
            seed: borrow.content.clone(),
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        // Pre-closed frame (either `{ ..., closed: true }` at
        // construction or `frame.close()` called before push) — seal
        // it on the wire now that it has an id. The TUI sees Push +
        // Close in the same batch and never paints the spinner over
        // it.
        if borrow.closed.load(Ordering::Acquire) {
            let _ = state.tx.send(SectionTranscript::Close {
                id: SectionId(new_id),
            });
        }
        return Ok(());
    }
    if let Some(err) = as_section::<ErrorSection>(&section) {
        let new_id = state.assign_id();
        err.borrow().id.store(new_id, Ordering::Release);
        let section = SectionSpec {
            kind: SectionKind::Error,
            seed: Some(err.borrow().content.clone()),
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        return Ok(());
    }
    if let Some(tu) = as_section::<ToolUseSection>(&section) {
        let borrow = tu.borrow();
        if borrow.hidden {
            return Ok(());
        }
        let new_id = state.assign_id();
        borrow.id.store(new_id, Ordering::Release);
        let section = SectionSpec {
            kind: SectionKind::ToolUse {
                name: borrow.name.clone(),
                detail: borrow.detail.clone(),
            },
            seed: None,
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        // One-shot: the runtime closes + persists this frame on its end
        // (see emit() for SectionKind::ToolUse). No SectionTranscript::Close from
        // the workflow side — keeps the JS API simple.
        return Ok(());
    }
    if let Some(df) = as_section::<DiffSection>(&section) {
        let mut borrow = df.borrow_mut();
        let new_id = state.assign_id();
        borrow.id.store(new_id, Ordering::Release);
        let ops = std::mem::take(&mut borrow.ops);
        let section = SectionSpec {
            kind: SectionKind::Diff { lines: ops },
            seed: None,
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        // One-shot — runtime seals on its side. Same shape as ToolUseSection.
        return Ok(());
    }
    if let Some(json) = as_section::<JsonSection>(&section) {
        let new_id = state.assign_id();
        json.borrow().id.store(new_id, Ordering::Release);
        let borrow = json.borrow();
        let section = SectionSpec {
            kind: SectionKind::Json {
                tag: borrow.tag.clone(),
                value: borrow.value.clone(),
            },
            seed: None,
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        return Ok(());
    }
    if let Some(sh) = as_section::<ShellOutputSection>(&section) {
        let new_id = state.assign_id();
        let borrow = sh.borrow();
        borrow.id.store(new_id, Ordering::Release);
        let section = SectionSpec {
            kind: SectionKind::ShellOutput {
                state: load_shell_state(&borrow.state_atom),
                cmd: borrow.cmd.clone(),
            },
            seed: Some(borrow.content.clone()),
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        if borrow.closed.load(Ordering::Acquire) {
            let _ = state.tx.send(SectionTranscript::Close {
                id: SectionId(new_id),
            });
        }
        return Ok(());
    }
    if let Some(th) = as_section::<ReasoningSection>(&section) {
        let new_id = state.assign_id();
        let borrow = th.borrow();
        borrow.id.store(new_id, Ordering::Release);
        // Body content rides as `seed` (typically empty — reasoning is
        // streamed in via `.write()`). State is whatever `done` reports
        // at push time so a pre-closed frame goes straight to `Done`.
        let reasoning_state = if borrow.done.load(Ordering::Acquire) {
            crate::runtime::ReasoningState::Done
        } else {
            crate::runtime::ReasoningState::Streaming
        };
        let section = SectionSpec {
            kind: SectionKind::Reasoning {
                state: reasoning_state,
            },
            seed: Some(borrow.content.clone()),
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(new_id),
            section,
        });
        if borrow.closed.load(Ordering::Acquire) {
            let _ = state.tx.send(SectionTranscript::Close {
                id: SectionId(new_id),
            });
        }
        return Ok(());
    }
    throw_type(
        ctx,
        "transcript.push: expected a MarkdownSection, ErrorSection, JsonSection, ShellOutputSection, ReasoningSection, or ToolUseSection",
    )
}

fn as_section<'js, C: JsClass<'js>>(v: &Value<'js>) -> Option<Class<'js, C>> {
    v.as_object().and_then(Class::<C>::from_object)
}

// ---------------------------------------------------------------------
// MarkdownSection / ErrorSection — writeable text frames
// ---------------------------------------------------------------------

pub struct MarkdownSection {
    state: Arc<SectionsState>,
    /// Set by `transcript.push`; 0 means "not yet pushed".
    id: AtomicU64,
    /// Initial content captured at construction. `None` when the
    /// workflow omitted `content` (or passed `undefined` / `null`) —
    /// the frame is pushed with no body, and the client defers measure
    /// and render until the first `write` materialises it. Appends go
    /// straight to the host channel; we don't reconstruct the full text
    /// here.
    content: Option<String>,
    /// Speaker for the frame. Drives the host-side sigil. Defaults to
    /// [`Source::Internal`] when the workflow omits `source` — that's the
    /// "no prefix" / chrome case (greetings, plan dumps, tag bodies).
    source: Source,
    /// Flipped by [`close_section`] (either explicit `.close()` or the
    /// writable's auto-close hook on the JS side). Subsequent writes
    /// throw; subsequent closes are no-ops.
    closed: AtomicBool,
}

impl<'js> Trace<'js> for MarkdownSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for MarkdownSection {
    type Changed<'to> = MarkdownSection;
}

impl<'js> JsClass<'js> for MarkdownSection {
    const NAME: &'static str = "MarkdownSection";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "write",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, MarkdownSection>>, delta: String| {
                    let b = this.0.borrow();
                    append_text(&ctx, &b.state, &b.id, &b.closed, delta)
                },
            )?,
        )?;
        proto.set(
            "close",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>,
                 this: This<Class<'js, MarkdownSection>>|
                 -> JsResult<Class<'js, MarkdownSection>> {
                    {
                        let b = this.0.borrow();
                        close_section(&ctx, &b.state, &b.id, &b.closed)?;
                    }
                    Ok(this.0.clone())
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// `ErrorSection` — one-shot error message. No `write` in v1; rendering
/// goes through `StreamFrame::Error` (a non-block message) on the host
/// side, so streaming-write semantics don't apply.
pub struct ErrorSection {
    #[expect(
        dead_code,
        reason = "kept on the type for parity with MarkdownSection; write support is a follow-up"
    )]
    state: Arc<SectionsState>,
    id: AtomicU64,
    content: String,
}

impl<'js> Trace<'js> for ErrorSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ErrorSection {
    type Changed<'to> = ErrorSection;
}

impl<'js> JsClass<'js> for ErrorSection {
    const NAME: &'static str = "ErrorSection";
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
    state: &Arc<SectionsState>,
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
    let _ = state.tx.send(SectionTranscript::Append {
        id: SectionId(frame_id),
        delta,
    });
    Ok(())
}

/// Mark the frame closed and (if it has been pushed) emit
/// [`SectionTranscript::Close`] exactly once. Idempotent: a second call is a
/// silent no-op. When called before `transcript.push`, just records
/// the intent — `transcript.push` notices the pre-set flag and emits
/// the close right after the push so `new MarkdownSection(...).close()`
/// chains the same way `new MarkdownSection({ ..., closed: true })`
/// does.
fn close_section<'js>(
    _ctx: &Ctx<'js>,
    state: &Arc<SectionsState>,
    id: &AtomicU64,
    closed: &AtomicBool,
) -> JsResult<()> {
    let frame_id = id.load(Ordering::Acquire);
    if frame_id == 0 {
        closed.store(true, Ordering::Release);
        return Ok(());
    }
    if !closed.swap(true, Ordering::AcqRel) {
        let _ = state.tx.send(SectionTranscript::Close {
            id: SectionId(frame_id),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------
// JsonSection — single tagged value, no write
// ---------------------------------------------------------------------

pub struct JsonSection {
    #[expect(
        dead_code,
        reason = "kept for future API parity with text frames; not read after construction"
    )]
    state: Arc<SectionsState>,
    id: AtomicU64,
    tag: String,
    value: serde_json::Value,
}

impl<'js> Trace<'js> for JsonSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for JsonSection {
    type Changed<'to> = JsonSection;
}

impl<'js> JsClass<'js> for JsonSection {
    const NAME: &'static str = "JsonSection";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        // Intentionally empty — no `write` here; JsonSection is set at
        // construction.
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------
// ToolUseSection — one-shot "→ tool_name" marker
// ---------------------------------------------------------------------

/// Hard cap on the length of the `detail` string returned by a tool's
/// `describe(call)`. The detail rides on every tool-call wire frame and
/// is shown inline in the TUI; an unbounded value could flood the
/// scrollback row or push the whole block off-screen. Anything past the
/// cap is truncated with a trailing `…`.
const TOOL_DETAIL_MAX: usize = 160;

pub struct ToolUseSection {
    #[expect(
        dead_code,
        reason = "kept for symmetry with the other frame types; never read after construction since ToolUseSection is one-shot"
    )]
    state: Arc<SectionsState>,
    id: AtomicU64,
    name: String,
    /// When `true`, `transcript.push` skips the frame entirely — no
    /// wire `Push` is emitted. Sourced from `tool.hidden` at
    /// construction so callers can pass the unmodified call+tool pair.
    hidden: bool,
    /// Optional human-readable suffix produced by `tool.describe(call)`
    /// (e.g. the file path + ranges for `file_read`). `None` when the
    /// tool didn't provide a `describe`, when it returned a non-string,
    /// or when it threw.
    detail: Option<String>,
}

impl<'js> Trace<'js> for ToolUseSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ToolUseSection {
    type Changed<'to> = ToolUseSection;
}

impl<'js> JsClass<'js> for ToolUseSection {
    const NAME: &'static str = "ToolUseSection";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        // No `write` / `close` — one-shot, sealed on the runtime side.
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Call `tool.describe(call)` if `tool` exposes a callable `describe`.
/// Returns the raw string the function produced, or `None` if there is
/// no `describe`, the property is not callable, it threw, or it
/// returned a non-string. Errors are swallowed by design — a broken
/// `describe` must never break the tool-call flow.
fn invoke_describe<'js>(tool: &Object<'js>, call: &Object<'js>) -> Option<String> {
    let describe = tool.get::<_, Function<'js>>("describe").ok()?;
    let result: Value<'js> = describe.call((call.clone(),)).ok()?;
    if result.is_string() {
        result.get::<String>().ok()
    } else {
        None
    }
}

/// Trim whitespace, collapse to `None` when empty, and truncate to
/// `TOOL_DETAIL_MAX` characters (replacing the tail with `…`). The cap
/// is enforced on Unicode scalar values, not bytes, so multi-byte
/// characters don't silently push the result past the limit.
fn normalise_detail(raw: String) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len().min(TOOL_DETAIL_MAX * 4));
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= TOOL_DETAIL_MAX {
            out.push('…');
            return Some(out);
        }
        out.push(ch);
    }
    Some(out)
}

fn build_tool_use_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ToolUseSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let Some(obj) = arg.as_object() else {
                return throw_type(
                    &ctx,
                    "new ToolUseSection: expected { call: { name: string }, tool? }",
                );
            };
            let call: Object<'js> = obj
                .get("call")
                .map_err(|_| throw_err(&ctx, "new ToolUseSection: missing object `call`"))?;
            let name: String = call.get("name").map_err(|_| {
                throw_err(
                    &ctx,
                    "new ToolUseSection: missing or non-string `call.name`",
                )
            })?;
            let tool: Option<Object<'js>> = match obj.get::<_, Value<'js>>("tool") {
                Ok(v) if v.is_object() => v.into_object(),
                _ => None,
            };
            let hidden = tool
                .as_ref()
                .and_then(|t| t.get::<_, bool>("hidden").ok())
                .unwrap_or(false);
            let detail = tool
                .as_ref()
                .and_then(|t| invoke_describe(t, &call))
                .and_then(normalise_detail);
            Class::instance(
                ctx.clone(),
                ToolUseSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    name,
                    hidden,
                    detail,
                },
            )
        },
    )
}

// ---------------------------------------------------------------------
// DiffSection — one-shot structured diff produced by a file-edit tool
// ---------------------------------------------------------------------

/// One-shot frame carrying a unified-diff payload. Constructed by the
/// JS file tools after a successful mutation; the runtime translates each
/// op into a wire `protocol::DiffLine` and emits a `BlockKind::Diff`
/// block. Like `ToolUseSection`, the runtime seals and persists the block
/// — there is no `write` / `close` on the JS side.
pub struct DiffSection {
    #[expect(
        dead_code,
        reason = "kept for symmetry with the other frame types; never read after construction since DiffSection is one-shot"
    )]
    state: Arc<SectionsState>,
    id: AtomicU64,
    /// Drained on push — the runtime moves the ops into the wire
    /// `SectionKind::Diff` rather than cloning, since a `DiffSection` is
    /// pushed exactly once.
    ops: Vec<DiffOp>,
}

impl<'js> Trace<'js> for DiffSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for DiffSection {
    type Changed<'to> = DiffSection;
}

impl<'js> JsClass<'js> for DiffSection {
    const NAME: &'static str = "DiffSection";
    type Mutable = rquickjs::class::Writable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        // No `write` / `close` — one-shot, sealed runtime-side.
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

fn parse_diff_ops<'js>(ctx: &Ctx<'js>, lines: Value<'js>) -> JsResult<Vec<DiffOp>> {
    let Some(arr) = lines.as_array() else {
        return throw_type(ctx, "new DiffSection: `lines` must be an array");
    };
    let mut ops = Vec::with_capacity(arr.len());
    for entry in arr.iter::<Value<'js>>() {
        let entry = entry?;
        let Some(obj) = entry.as_object() else {
            return throw_type(
                ctx,
                "new DiffSection: each `lines` entry must be { kind, text, line? }",
            );
        };
        let kind: String = obj
            .get("kind")
            .map_err(|_| throw_err(ctx, "new DiffSection: missing or non-string `kind`"))?;
        let text: String = obj
            .get("text")
            .map_err(|_| throw_err(ctx, "new DiffSection: missing or non-string `text`"))?;
        let op = match kind.as_str() {
            "context" => {
                let line: u32 = obj.get("line").map_err(|_| {
                    throw_err(
                        ctx,
                        "new DiffSection: context entries require `line: number`",
                    )
                })?;
                DiffOp::Context { text, line }
            }
            "added" => DiffOp::Added(text),
            "removed" => DiffOp::Removed(text),
            other => {
                return throw_type(
                    ctx,
                    &format!(
                        "new DiffSection: unknown `kind` {other:?}; expected \"context\", \"added\", or \"removed\""
                    ),
                );
            }
        };
        ops.push(op);
    }
    Ok(ops)
}

fn build_diff_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<DiffSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let Some(obj) = arg.as_object() else {
                return throw_type(&ctx, "new DiffSection: expected { lines: Array }");
            };
            let lines: Value<'js> = obj
                .get("lines")
                .map_err(|_| throw_err(&ctx, "new DiffSection: missing `lines`"))?;
            let ops = parse_diff_ops(&ctx, lines)?;
            Class::instance(
                ctx.clone(),
                DiffSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    ops,
                },
            )
        },
    )
}

// ---------------------------------------------------------------------
// ShellOutputSection — streaming shell-command output with mutable state
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

pub struct ShellOutputSection {
    state: Arc<SectionsState>,
    id: AtomicU64,
    /// Bash source that produced this output. Pinned on every wire
    /// frame so the TUI can render it as a header even when the body
    /// has been truncated.
    cmd: String,
    /// Initial body captured at construction. Mirrors `MarkdownSection.content`.
    content: String,
    /// Encoded [`crate::runtime::ShellState`]. Mutated by
    /// `.setState()` / `.success()` / `.exit()`.
    state_atom: AtomicU64,
    /// Same close lifecycle as `MarkdownSection`.
    closed: AtomicBool,
}

impl<'js> Trace<'js> for ShellOutputSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ShellOutputSection {
    type Changed<'to> = ShellOutputSection;
}

impl<'js> JsClass<'js> for ShellOutputSection {
    const NAME: &'static str = "ShellOutputSection";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "write",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputSection>>, delta: String| {
                    let b = this.0.borrow();
                    append_text(&ctx, &b.state, &b.id, &b.closed, delta)
                },
            )?,
        )?;
        proto.set(
            "close",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>,
                 this: This<Class<'js, ShellOutputSection>>|
                 -> JsResult<Class<'js, ShellOutputSection>> {
                    {
                        let b = this.0.borrow();
                        close_section(&ctx, &b.state, &b.id, &b.closed)?;
                    }
                    Ok(this.0.clone())
                },
            )?,
        )?;
        // `frame.success()` and `frame.exit(code)` set the new state
        // on the wire via a metadata-only `SectionTranscript::Set`. They do NOT close
        // the frame — JS-side auto-close (writable's close hook) or
        // an explicit `frame.close()` is still required to seal the
        // block. Keeping these orthogonal lets the workflow stream a
        // tail of output between "got exit code" and "EOF on stdout".
        proto.set(
            "success",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputSection>>| {
                    let b = this.0.borrow();
                    set_shell_state(
                        &ctx,
                        &b.state,
                        &b.id,
                        &b.state_atom,
                        &b.cmd,
                        crate::runtime::ShellState::Success,
                    )
                },
            )?,
        )?;
        proto.set(
            "exit",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ShellOutputSection>>, code: i32| {
                    let b = this.0.borrow();
                    set_shell_state(
                        &ctx,
                        &b.state,
                        &b.id,
                        &b.state_atom,
                        &b.cmd,
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

// ---------------------------------------------------------------------
// ReasoningSection — streaming model reasoning
// ---------------------------------------------------------------------

/// `ReasoningSection` — mirrors [`ShellOutputSection`] but for the model's
/// reasoning channel. `state` transitions `Streaming → Done` on close;
/// there is no body-after-state phase, so `.close()` performs both the
/// state transition and the seal in one call.
pub struct ReasoningSection {
    state: Arc<SectionsState>,
    id: AtomicU64,
    /// Initial body captured at construction. Mirrors `MarkdownSection.content`.
    content: String,
    /// Encoded [`crate::runtime::ReasoningState`]. `false` ⇒ Streaming,
    /// `true` ⇒ Done. Flipped by `.close()`.
    done: AtomicBool,
    /// Same close lifecycle as `MarkdownSection`.
    closed: AtomicBool,
}

impl<'js> Trace<'js> for ReasoningSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for ReasoningSection {
    type Changed<'to> = ReasoningSection;
}

impl<'js> JsClass<'js> for ReasoningSection {
    const NAME: &'static str = "ReasoningSection";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;
        proto.set(
            "write",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, ReasoningSection>>, delta: String| {
                    let b = this.0.borrow();
                    append_text(&ctx, &b.state, &b.id, &b.closed, delta)
                },
            )?,
        )?;
        // `close()` performs both the `Streaming → Done` state transition
        // (metadata-only `Set`) and the block seal (`Close`). Idempotent.
        proto.set(
            "close",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>,
                 this: This<Class<'js, ReasoningSection>>|
                 -> JsResult<Class<'js, ReasoningSection>> {
                    {
                        let b = this.0.borrow();
                        finish_thought(&ctx, &b.state, &b.id, &b.done, &b.closed)?;
                    }
                    Ok(this.0.clone())
                },
            )?,
        )?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Transition a thought frame to `Done` (metadata `Set`) and seal it
/// (`Close`) in one call. Idempotent on repeated invocation.
fn finish_thought<'js>(
    _ctx: &Ctx<'js>,
    state: &Arc<SectionsState>,
    id: &AtomicU64,
    done: &AtomicBool,
    closed: &AtomicBool,
) -> JsResult<()> {
    let frame_id = id.load(Ordering::Acquire);
    if frame_id == 0 {
        // Frame never pushed; record close-on-push intent.
        closed.store(true, Ordering::Release);
        done.store(true, Ordering::Release);
        return Ok(());
    }
    if !done.swap(true, Ordering::AcqRel) {
        // Metadata-only re-`Set` carrying the new state.
        let section = SectionSpec {
            kind: SectionKind::Reasoning {
                state: crate::runtime::ReasoningState::Done,
            },
            seed: None,
        };
        let _ = state.tx.send(SectionTranscript::Set {
            id: SectionId(frame_id),
            section,
        });
    }
    if !closed.swap(true, Ordering::AcqRel) {
        let _ = state.tx.send(SectionTranscript::Close {
            id: SectionId(frame_id),
        });
    }
    Ok(())
}

/// Update the frame's state and emit a metadata-only [`SectionTranscript::Set`].
/// Throws if the frame hasn't been pushed yet.
fn set_shell_state<'js>(
    ctx: &Ctx<'js>,
    state: &Arc<SectionsState>,
    id: &AtomicU64,
    state_atom: &AtomicU64,
    cmd: &str,
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
    // Metadata-only re-`Set`: no body (`seed: None`), so the runtime
    // emits a no-text BlockDelta carrying just the new kind. `cmd` rides
    // along because the wire `BlockKind::ShellOutput` carries it on
    // every delta.
    let section = SectionSpec {
        kind: SectionKind::ShellOutput {
            state: new_state,
            cmd: cmd.to_owned(),
        },
        seed: None,
    };
    let _ = state.tx.send(SectionTranscript::Set {
        id: SectionId(frame_id),
        section,
    });
    Ok(())
}

// ---------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------
//
// We define our own constructor `Function`s so each one can close over
// the per-invocation `SectionsState`. The frame classes' own `JsClass`
// impls return `None` from `constructor()` so user code can't do
// `new (constructor)()` without going through these.

fn build_markdown_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<MarkdownSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let (content, source, closed) = parse_markdown_arg(&ctx, &arg)?;
            Class::instance(
                ctx.clone(),
                MarkdownSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    content,
                    source,
                    closed: AtomicBool::new(closed),
                },
            )
        },
    )
}

fn build_error_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ErrorSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let content = parse_content_arg(&ctx, &arg, "ErrorSection")?;
            Class::instance(
                ctx.clone(),
                ErrorSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    content,
                },
            )
        },
    )
}

fn build_json_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<JsonSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let Some(obj) = arg.as_object() else {
                return throw_type(&ctx, "new JsonSection: expected { tag, value }");
            };
            let tag: String = obj
                .get("tag")
                .map_err(|_| throw_err(&ctx, "new JsonSection: missing or non-string `tag`"))?;
            let value: Value<'js> = obj
                .get("value")
                .map_err(|_| throw_err(&ctx, "new JsonSection: missing `value`"))?;

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
                JsonSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    tag,
                    value: parsed,
                },
            )
        },
    )
}

fn build_shell_output_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ShellOutputSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let (cmd, content, closed) = parse_shell_output_arg(&ctx, &arg)?;
            Class::instance(
                ctx.clone(),
                ShellOutputSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    cmd,
                    content,
                    state_atom: AtomicU64::new(SHELL_STATE_RUNNING),
                    closed: AtomicBool::new(closed),
                },
            )
        },
    )
}

/// `new ReasoningSection()` — no constructor arguments. Reasoning frames
/// start empty in `Streaming` state and are filled via `.write()` from
/// the chat session's `r.reasoning` channel.
fn build_thought_ctor<'js>(ctx: &Ctx<'js>, state: Arc<SectionsState>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ReasoningSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, _arg: Opt<Value<'js>>| {
            Class::instance(
                ctx.clone(),
                ReasoningSection {
                    state: state.clone(),
                    id: AtomicU64::new(0),
                    content: String::new(),
                    done: AtomicBool::new(false),
                    closed: AtomicBool::new(false),
                },
            )
        },
    )
}

/// Parse `new ShellOutputSection({ cmd, content?, closed? })`. `cmd` is
/// required — it's the bash source that produced this output and the
/// TUI renders it as a header. `content` is optional; if absent it
/// defaults to an empty string (workflows usually push the frame first,
/// then stream output via `.writable`). `closed` mirrors the
/// MarkdownSection option: setting it to `true` pre-seals the frame so
/// `transcript.push` emits a `Close` right after the `Push`.
fn parse_shell_output_arg<'js>(
    ctx: &Ctx<'js>,
    arg: &Value<'js>,
) -> JsResult<(String, String, bool)> {
    let Some(obj) = arg.as_object() else {
        return throw_type(
            ctx,
            "new ShellOutputSection: expected { cmd: string, content?: string, closed?: bool }",
        );
    };
    let cmd_val: Value<'js> = obj
        .get("cmd")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    let cmd = if let Some(s) = cmd_val.as_string() {
        s.to_string()
            .map_err(|_| throw_err(ctx, "new ShellOutputSection: `cmd` must be UTF-8"))?
    } else {
        return Err(throw_err(
            ctx,
            "new ShellOutputSection: `cmd` is required and must be a string",
        ));
    };
    let content_val: Value<'js> = obj
        .get("content")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    let content = if content_val.is_undefined() || content_val.is_null() {
        String::new()
    } else if let Some(s) = content_val.as_string() {
        s.to_string()
            .map_err(|_| throw_err(ctx, "new ShellOutputSection: `content` must be UTF-8"))?
    } else {
        return Err(throw_err(
            ctx,
            "new ShellOutputSection: `content` must be a string when present",
        ));
    };
    let closed = parse_optional_bool(ctx, obj, "closed", "new ShellOutputSection: `closed`")?;
    Ok((cmd, content, closed))
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

/// Parse `new MarkdownSection({ content?, source?, closed? })`. `content`
/// is optional; absent / `undefined` / `null` map to `None`. `source`
/// is one of `"user"`, `"assistant"`, `"internal"`; absent → `Internal`.
/// `closed` is an optional bool defaulting to `false`; when `true` the
/// frame's `closed` flag is pre-set so `transcript.push` emits a `Close`
/// immediately after the `Push` — useful for one-shot frames like a
/// greeting or echoed user message. Anything else throws.
fn parse_markdown_arg<'js>(
    ctx: &Ctx<'js>,
    arg: &Value<'js>,
) -> JsResult<(Option<String>, Source, bool)> {
    if arg.is_undefined() || arg.is_null() {
        return Ok((None, Source::Internal, false));
    }
    let Some(obj) = arg.as_object() else {
        return Err(throw_err(
            ctx,
            "new MarkdownSection: expected { content?: string, source?: \"user\"|\"assistant\"|\"internal\" } or no argument",
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
                .map_err(|_| throw_err(ctx, "new MarkdownSection: `content` must be UTF-8"))?,
        )
    } else {
        return Err(throw_err(
            ctx,
            "new MarkdownSection: `content` must be a string when present",
        ));
    };
    let source_val: Value<'js> = obj
        .get("source")
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    let source = if source_val.is_undefined() || source_val.is_null() {
        Source::Internal
    } else if let Some(s) = source_val.as_string() {
        let s = s
            .to_string()
            .map_err(|_| throw_err(ctx, "new MarkdownSection: `source` must be UTF-8"))?;
        match s.as_str() {
            "user" => Source::User,
            "assistant" => Source::Assistant,
            "internal" => Source::Internal,
            other => {
                return Err(throw_err(
                    ctx,
                    &format!(
                        "new MarkdownSection: `source` must be \"user\", \"assistant\", or \"internal\" (got {other:?})"
                    ),
                ));
            }
        }
    } else {
        return Err(throw_err(
            ctx,
            "new MarkdownSection: `source` must be a string when present",
        ));
    };
    let closed = parse_optional_bool(ctx, obj, "closed", "new MarkdownSection: `closed`")?;
    Ok((content, source, closed))
}

/// Parse an optional bool field. Absent / `undefined` / `null` →
/// `false`; anything that isn't a bool throws with `field_label`.
fn parse_optional_bool<'js>(
    ctx: &Ctx<'js>,
    obj: &rquickjs::Object<'js>,
    key: &str,
    field_label: &str,
) -> JsResult<bool> {
    let val: Value<'js> = obj
        .get(key)
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
    if val.is_undefined() || val.is_null() {
        return Ok(false);
    }
    val.as_bool().ok_or_else(|| {
        throw_err(
            ctx,
            &format!("{field_label} must be a boolean when present"),
        )
    })
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
