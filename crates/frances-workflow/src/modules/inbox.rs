//! `frances:v1/inbox` — async-iterable user-input stream.
//!
//! `next()` pulls from the input mpsc; when the buffer is empty it pulses
//! the test-harness `on_idle` signal (compiled only under test) before
//! suspending. `return()` and `workflow.exit()` close the shared
//! [`WorkflowClosed`], breaking any in-flight `next()` with `{done:
//! true}`.

use std::sync::Arc;

use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::closed::WorkflowClosed;
use crate::runtime::InboxItem;

/// Args for [`build_inbox`].
pub(crate) struct InboxArgs {
    pub rx: Arc<AsyncMutex<UnboundedReceiver<InboxItem>>>,
    pub closed: Arc<WorkflowClosed>,
    #[cfg(any(test, feature = "test-utils"))]
    pub on_idle: Arc<tokio::sync::Notify>,
}

pub(crate) fn build_inbox<'js>(ctx: &Ctx<'js>, args: InboxArgs) -> JsResult<Class<'js, Inbox>> {
    Class::instance(
        ctx.clone(),
        Inbox {
            rx: args.rx,
            closed: args.closed,
            #[cfg(any(test, feature = "test-utils"))]
            on_idle: args.on_idle,
        },
    )
}

pub struct Inbox {
    rx: Arc<AsyncMutex<UnboundedReceiver<InboxItem>>>,
    closed: Arc<WorkflowClosed>,
    #[cfg(any(test, feature = "test-utils"))]
    on_idle: Arc<tokio::sync::Notify>,
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
                #[cfg(any(test, feature = "test-utils"))]
                let on_idle = borrow.on_idle.clone();
                drop(borrow);
                Ok::<_, rquickjs::Error>(Promised::from(async move {
                    if closed.is_closed() {
                        return IterResult::done();
                    }
                    let mut guard = rx.lock().await;
                    if closed.is_closed() {
                        return IterResult::done();
                    }
                    if let Ok(value) = guard.try_recv() {
                        return IterResult::value(value);
                    }
                    #[cfg(any(test, feature = "test-utils"))]
                    on_idle.notify_one();
                    tokio::select! {
                        msg = guard.recv() => match msg {
                            Some(input) => IterResult::value(input),
                            None => IterResult::done(),
                        },
                        () = closed.closed() => IterResult::done(),
                    }
                }))
            })?,
        )?;

        proto.set(
            PredefinedAtom::Return,
            Function::new(ctx.clone(), |this: This<Class<'js, Inbox>>| {
                this.0.borrow().closed.close();
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
    value: Option<InboxItem>,
    done: bool,
}

impl IterResult {
    fn value(v: InboxItem) -> Self {
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
