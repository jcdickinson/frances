//! `frances:v1/inbox` — async-iterable user-input stream.
//!
//! `next()` pulls from the input mpsc; when the buffer is empty it
//! pulses `parked` before suspending. `return()` and `workflow.exit()`
//! flip `closed`, breaking any in-flight `next()` with `{done: true}`.
//!
//! Same wiring as the previous `workflow.user.input` class, with the
//! message field renamed `message` → `content`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::runtime::UserInput;

pub(crate) fn build_inbox<'js>(
    ctx: &Ctx<'js>,
    rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
) -> JsResult<Class<'js, Inbox>> {
    Class::instance(
        ctx.clone(),
        Inbox {
            rx,
            closed,
            closed_notify,
            parked,
        },
    )
}

pub struct Inbox {
    rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
}

impl<'js> Trace<'js> for Inbox {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for Inbox {
    type Changed<'to> = Inbox;
}

impl<'js> JsClass<'js> for Inbox {
    const NAME: &'static str = "Inbox";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            PredefinedAtom::SymbolAsyncIterator,
            Function::new(ctx.clone(), |this: This<Class<'js, Inbox>>| {
                Ok::<_, rquickjs::Error>(this.0.clone())
            })?,
        )?;

        proto.set(
            PredefinedAtom::Next,
            Function::new(ctx.clone(), |this: This<Class<'js, Inbox>>| {
                let borrow = this.0.borrow();
                let rx = borrow.rx.clone();
                let closed = borrow.closed.clone();
                let closed_notify = borrow.closed_notify.clone();
                let parked = borrow.parked.clone();
                drop(borrow);
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    if closed.load(Ordering::Acquire) {
                        return IterResult::done();
                    }
                    let mut guard = rx.lock().await;
                    if closed.load(Ordering::Acquire) {
                        return IterResult::done();
                    }
                    if let Ok(value) = guard.try_recv() {
                        return IterResult::value(value);
                    }
                    parked.notify_one();
                    tokio::select! {
                        msg = guard.recv() => match msg {
                            Some(input) => IterResult::value(input),
                            None => IterResult::done(),
                        },
                        () = closed_notify.notified() => IterResult::done(),
                    }
                }))
            })?,
        )?;

        proto.set(
            PredefinedAtom::Return,
            Function::new(ctx.clone(), |this: This<Class<'js, Inbox>>| {
                let borrow = this.0.borrow();
                if !borrow.closed.swap(true, Ordering::AcqRel) {
                    borrow.closed_notify.notify_waiters();
                }
                Ok::<_, rquickjs::Error>(IterResult::done())
            })?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// `{ value, done }` for the JS iterator protocol.
struct IterResult {
    value: Option<UserInput>,
    done: bool,
}

impl IterResult {
    fn value(v: UserInput) -> Self {
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
