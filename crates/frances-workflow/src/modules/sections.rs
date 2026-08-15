//! `frances:v1/sections` — transcript + frame classes.
//!
//! The transcript is an append-only sequence of frames. Every frame is
//! one-shot: `transcript.push(frame)` hands the host a finished section
//! and the frame object is inert afterwards. Anything that streams is
//! an entity (see `frances:v1/entities`), referenced from the
//! transcript by an `EntityRefSection`.
//!
//! For v1 there is exactly one transcript (the live binding behind the
//! `transcript` import). The `Transcript` class is exported as a type
//! for future rotation work; users can't construct one in v1.
//!
//! Wire contract: the host receives one [`SectionTranscript::Push`] per
//! pushed frame.

use super::{throw_js as throw_err, throw_js_type as throw_type};
use frances_edit::DiffOp;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, Opt, This};
use rquickjs::{Class, Ctx, Function, JsLifetime, Object, Result as JsResult, Value};

type Ctor<'js> = Constructor<'js>;
use tokio::sync::mpsc::UnboundedSender;

use crate::runtime::{SectionKind, SectionTranscript};

pub(crate) type BuiltSections<'js> = (
    Class<'js, TranscriptHandle>,
    Ctor<'js>, // ErrorSection
    Ctor<'js>, // JsonSection
    Ctor<'js>, // DiffSection
    Ctor<'js>, // EntityRefSection
);

pub(crate) fn build_sections<'js>(
    ctx: &Ctx<'js>,
    tx: UnboundedSender<SectionTranscript>,
) -> JsResult<BuiltSections<'js>> {
    let transcript = Class::instance(ctx.clone(), TranscriptHandle { tx })?;

    let err_ctor = build_error_ctor(ctx)?;
    let json_ctor = build_json_ctor(ctx)?;
    let diff_ctor = build_diff_ctor(ctx)?;
    let entity_ref_ctor = build_entity_ref_ctor(ctx)?;

    Ok((transcript, err_ctor, json_ctor, diff_ctor, entity_ref_ctor))
}

// ---------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------

/// `Transcript` — for v1 just `push(frame)`. Users get a singleton via
/// the `transcript` export; the constructor is intentionally absent.
pub struct TranscriptHandle {
    /// Where pushed sections go.
    tx: UnboundedSender<SectionTranscript>,
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
    let tx = handle.borrow().tx.clone();

    if let Some(err) = as_section::<ErrorSection>(&section) {
        let borrow = err.borrow();
        push_kind(
            &tx,
            SectionKind::Error {
                text: borrow.content.clone(),
            },
        );
        return Ok(());
    }
    if let Some(df) = as_section::<DiffSection>(&section) {
        let mut borrow = df.borrow_mut();
        let ops = std::mem::take(&mut borrow.ops);
        push_kind(&tx, SectionKind::Diff { lines: ops });
        return Ok(());
    }
    if let Some(json) = as_section::<JsonSection>(&section) {
        let borrow = json.borrow();
        push_kind(
            &tx,
            SectionKind::Json {
                tag: borrow.tag.clone(),
                value: borrow.value.clone(),
            },
        );
        return Ok(());
    }
    if let Some(er) = as_section::<EntityRefSection>(&section) {
        let borrow = er.borrow();
        push_kind(
            &tx,
            SectionKind::EntityRef {
                entity_id: borrow.entity_id,
            },
        );
        return Ok(());
    }
    throw_type(
        ctx,
        "transcript.push: expected an ErrorSection, JsonSection, DiffSection, or EntityRefSection",
    )
}

fn push_kind(tx: &UnboundedSender<SectionTranscript>, kind: SectionKind) {
    let _ = tx.send(SectionTranscript::Push(kind));
}

fn as_section<'js, C: JsClass<'js>>(v: &Value<'js>) -> Option<Class<'js, C>> {
    v.as_object().and_then(Class::<C>::from_object)
}

// ---------------------------------------------------------------------
// ErrorSection — one-shot error message
// ---------------------------------------------------------------------

/// `ErrorSection` — one-shot error message.
pub struct ErrorSection {
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
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------
// JsonSection — single tagged value, no write
// ---------------------------------------------------------------------

pub struct JsonSection {
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
        let proto = Object::new(ctx.clone())?;
        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------
// DiffSection — one-shot structured diff produced by a file-edit tool
// ---------------------------------------------------------------------

/// One-shot frame carrying a unified-diff payload. Constructed by the
/// JS file tools after a successful mutation; the runtime moves the ops
/// into a [`SectionKind::Diff`] section.
pub struct DiffSection {
    /// Drained on push — the runtime moves the ops into `SectionKind::Diff` rather than cloning.
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

fn build_diff_ctor<'js>(ctx: &Ctx<'js>) -> JsResult<Ctor<'js>> {
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
            Class::instance(ctx.clone(), DiffSection { ops })
        },
    )
}

// ---------------------------------------------------------------------
// EntityRefSection — one-shot pointer at an entity
// ---------------------------------------------------------------------

/// One-shot frame carrying nothing but an entity id (as minted by
/// `frances:v1/entities`' `createEntity`). The entity's snapshot renders
/// the ref; the transcript only records where it sits.
pub struct EntityRefSection {
    entity_id: uuid::Uuid,
}

impl<'js> Trace<'js> for EntityRefSection {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for EntityRefSection {
    type Changed<'to> = EntityRefSection;
}

impl<'js> JsClass<'js> for EntityRefSection {
    const NAME: &'static str = "EntityRefSection";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        Ok(Some(Object::new(ctx.clone())?))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

fn build_entity_ref_ctor<'js>(ctx: &Ctx<'js>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<EntityRefSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let Some(obj) = arg.as_object() else {
                return throw_type(&ctx, "new EntityRefSection: expected { id: string }");
            };
            let id: String = obj
                .get("id")
                .map_err(|_| throw_err(&ctx, "new EntityRefSection: missing string `id`"))?;
            let entity_id = id
                .parse()
                .map_err(|_| throw_err(&ctx, "new EntityRefSection: `id` is not an entity id"))?;
            Class::instance(ctx.clone(), EntityRefSection { entity_id })
        },
    )
}

// ---------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------

fn build_error_ctor<'js>(ctx: &Ctx<'js>) -> JsResult<Ctor<'js>> {
    Constructor::new_class::<ErrorSection, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>, arg: Opt<Value<'js>>| {
            let arg = arg.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            let content = parse_content_arg(&ctx, &arg, "ErrorSection")?;
            Class::instance(ctx.clone(), ErrorSection { content })
        },
    )
}

fn build_json_ctor<'js>(ctx: &Ctx<'js>) -> JsResult<Ctor<'js>> {
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
            // Values that don't JSON-encode are silently replaced with `null`.
            let json_str: String = ctx
                .json_stringify(value)?
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| "null".to_string());
            let parsed: serde_json::Value =
                serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);

            Class::instance(ctx.clone(), JsonSection { tag, value: parsed })
        },
    )
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
