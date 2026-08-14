//! `frances:v1/entities` host half — the workflow-side entity producer.
//!
//! `_createEntity(kind, snapshot)` mints an entity id, sends the
//! creating `Upsert`, and returns an [`EntityHandleJs`] whose methods
//! emit further [`EntityCmd`]s down the transcript channel (ordering
//! against sections is the point — see [`SectionTranscript::Entity`]).
//! Payloads are opaque JSON from here on; kind-specific shape lives in
//! the JS producer and the frontend's per-kind components.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, Opt, This};
use rquickjs::{Class, Ctx, Function, JsLifetime, Object, Result as JsResult, Value};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use super::{rquickjs_to_json, throw_js as throw_err};
use crate::runtime::{EntityCmd, SectionTranscript};

pub struct EntityHandleJs {
    entity_id: Uuid,
    kind: String,
    tx: UnboundedSender<SectionTranscript>,
    settled: Arc<AtomicBool>,
}

impl<'js> Trace<'js> for EntityHandleJs {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for EntityHandleJs {
    type Changed<'to> = EntityHandleJs;
}

impl<'js> JsClass<'js> for EntityHandleJs {
    const NAME: &'static str = "EntityHandle";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "updateSnapshot",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, EntityHandleJs>>, snapshot: Value<'js>| {
                    let handle = this.0.borrow();
                    let snapshot = json_arg(&ctx, &snapshot, "updateSnapshot")?;
                    if handle.settled.load(Ordering::Acquire) {
                        return Err(throw_err(&ctx, "entity.updateSnapshot: entity is settled"));
                    }
                    let _ = handle.tx.send(SectionTranscript::Entity(EntityCmd::Upsert {
                        entity_id: handle.entity_id,
                        kind: handle.kind.clone(),
                        snapshot,
                    }));
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.set(
            "append",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, EntityHandleJs>>, payload: Value<'js>| {
                    let handle = this.0.borrow();
                    let payload = json_arg(&ctx, &payload, "append")?;
                    if handle.settled.load(Ordering::Acquire) {
                        return Err(throw_err(&ctx, "entity.append: entity is settled"));
                    }
                    let _ = handle.tx.send(SectionTranscript::Entity(EntityCmd::Append {
                        entity_id: handle.entity_id,
                        payload,
                    }));
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.set(
            "settle",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>,
                 this: This<Class<'js, EntityHandleJs>>,
                 snapshot: Value<'js>,
                 opts: Opt<Object<'js>>| {
                    let handle = this.0.borrow();
                    let snapshot = json_arg(&ctx, &snapshot, "settle")?;
                    let artifacts = settle_artifacts(&ctx, opts.0.as_ref())?;
                    if handle.settled.swap(true, Ordering::AcqRel) {
                        return Err(throw_err(&ctx, "entity.settle: entity is already settled"));
                    }
                    let _ = handle.tx.send(SectionTranscript::Entity(EntityCmd::Settle {
                        entity_id: handle.entity_id,
                        snapshot,
                        artifacts,
                    }));
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Build the `_createEntity(kind, snapshot)` stash primitive.
pub(crate) fn build_create_entity<'js>(
    ctx: &Ctx<'js>,
    tx: UnboundedSender<SectionTranscript>,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, kind: String, snapshot: Value<'js>| {
            let snapshot = json_arg(&ctx, &snapshot, "createEntity")?;
            let entity_id = Uuid::new_v4();
            let _ = tx.send(SectionTranscript::Entity(EntityCmd::Upsert {
                entity_id,
                kind: kind.clone(),
                snapshot,
            }));

            let instance = Class::instance(
                ctx.clone(),
                EntityHandleJs {
                    entity_id,
                    kind,
                    tx: tx.clone(),
                    settled: Arc::new(AtomicBool::new(false)),
                },
            )?;
            // Plain data property — the id is immutable, no getter needed.
            instance.set("id", entity_id.to_string())?;
            Ok::<_, rquickjs::Error>(instance)
        },
    )
}

fn json_arg<'js>(ctx: &Ctx<'js>, value: &Value<'js>, verb: &str) -> JsResult<serde_json::Value> {
    rquickjs_to_json(value)
        .map_err(|error| throw_err(ctx, &format!("entity.{verb}: unsupported value: {error}")))
}

fn settle_artifacts<'js>(
    ctx: &Ctx<'js>,
    opts: Option<&Object<'js>>,
) -> JsResult<Vec<(String, serde_json::Value)>> {
    let Some(opts) = opts else {
        return Ok(Vec::new());
    };
    let artifacts: Value<'js> = opts.get("artifacts")?;
    if artifacts.is_undefined() || artifacts.is_null() {
        return Ok(Vec::new());
    }
    let Some(map) = artifacts.as_object() else {
        return Err(throw_err(
            ctx,
            "entity.settle: `artifacts` must be an object of tag → value",
        ));
    };
    let mut out = Vec::new();
    for entry in map.props::<String, Value<'js>>() {
        let (tag, value) = entry?;
        out.push((tag, json_arg(ctx, &value, "settle")?));
    }
    Ok(out)
}
