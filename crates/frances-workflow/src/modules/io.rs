//! `frances:v1/io` — IO primitives.
//!
//! v1 surface:
//!
//! - `Timer` — sleep/interval primitive that's directly awaitable.
//!
//! ```js
//! import { Timer } from "frances:v1/io";
//!
//! const t = new Timer(1000);            // fires once, ~1s from now
//! await t;
//!
//! const oneShot = new Timer({ delay: 250 });   // explicit one-shot
//! await oneShot;
//!
//! const tick = new Timer({ interval: 100 });   // repeats every 100ms
//! for (let i = 0; i < 3; i += 1) await tick;
//! tick.disable();                        // pause — schedule stops ticking
//! tick.enable();                         // resume with same schedule
//!
//! const wakeable = new Timer(60_000);
//! queueMicrotask(() => wakeable.fire());  // wake the await early
//! await wakeable;
//!
//! const failing = new Timer(60_000);
//! queueMicrotask(() => failing.reject(new Error("nope")));
//! try { await failing; } catch (e) { /* e is the Error above */ }
//!
//! // Read-only introspection:
//! tick.enabled;   // boolean
//! tick.delay;     // number or undefined
//! tick.interval;  // number or undefined
//! ```
//!
//! Constructor accepts:
//! - a number (milliseconds) — shorthand for `{ delay: <n> }`,
//! - `{ delay: number }` — fires once after the delay,
//! - `{ interval: number }` — repeats every `interval` ms (first fire
//!   also at `interval` ms), or
//! - `{ delay: number, interval: number }` — initial wait of `delay`,
//!   then fires every `interval` ms thereafter.
//!
//! Repeat is implied by the presence of `interval`. The object must
//! carry at least one of `delay` or `interval`.
//!
//! Semantics:
//!
//! - `await timer` suspends until the next firing: the scheduled time
//!   elapses, `fire()` is called, or the workflow is being torn down
//!   (`exit()`).
//! - For a one-shot timer (`delay`/number), awaiting again after the
//!   first firing resolves immediately.
//! - `disable()` / `enable(false)` pauses the timer. The schedule
//!   stops ticking, but awaits don't auto-resolve — they suspend
//!   until `fire()`, `enable()` / `set()` revives, or the workflow
//!   closes. Think "manual mode": auto-firing is off, manual firing
//!   still works.
//! - `enable()` / `enable(true)` revives a paused timer. Internally
//!   it just re-applies the existing `delay` / `interval` via `set`,
//!   so `fired_once` is cleared and pending awaits pick up the
//!   schedule.
//! - `fire()` wakes pending awaits in any non-rejected state. It does
//!   **not** implicitly re-enable a disabled timer — the next true
//!   scheduled tick still won't fire until you re-enable. On a
//!   one-shot `Active` timer it also marks the timer as having
//!   fired (so a subsequent `await` resolves instantly).
//! - `reject(reason?)` is terminal. Pending awaits reject with the
//!   reason; future awaits reject the same way; **all** further calls
//!   to `enable()` / `disable()` / `reject()` / `fire()` / `set()`
//!   throw. The reason is captured as a string at `reject()` time
//!   (Error → `.message`, anything else → `String(x)`). At throw time
//!   we construct a fresh `Error` carrying that message; pass
//!   `undefined` (or no argument) to reject with `undefined`.
//!   Identity isn't preserved across the await — we can't keep the
//!   original JS value alive without risking a runtime-lifetime leak.
//! - `set({ delay?, interval? })` reconfigures the schedule and
//!   re-enables the timer. Throws on `Rejected`. Pending awaits wake
//!   to pick up the new schedule.
//! - Read-only getters: `enabled`, `delay`, `interval`. They keep
//!   reporting the schedule's values even while paused — `disable()`
//!   doesn't forget the schedule.
//! - When the workflow closes, pending awaits resolve (graceful unwind),
//!   matching the way `inbox` returns `{done: true}` on close.
//!
//! The class itself is the thenable — there's no `wait()` method; the
//! engine's `await` does the right thing by calling `timer.then(...)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, Opt, This};
use rquickjs::object::Accessor;
use rquickjs::promise::Promised;
use rquickjs::{
    Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use tokio::sync::Notify;

use crate::modules::TimerErrorUserData;

pub(crate) fn build_timer_ctor<'js>(
    ctx: &Ctx<'js>,
    workflow_closed: Arc<AtomicBool>,
    workflow_closed_notify: Arc<Notify>,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<Timer, _, _>(ctx.clone(), move |ctx: Ctx<'js>, arg: Value<'js>| {
        let (delay, interval) = parse_timer_args(&ctx, &arg)?;
        Class::instance(
            ctx.clone(),
            Timer {
                schedule: Arc::new(Mutex::new(Schedule { delay, interval })),
                state: Arc::new(Mutex::new(TimerState::Active { fired_once: false })),
                state_notify: Arc::new(Notify::new()),
                fire_notify: Arc::new(Notify::new()),
                workflow_closed: workflow_closed.clone(),
                workflow_closed_notify: workflow_closed_notify.clone(),
            },
        )
    })
}

#[derive(Clone, Copy)]
struct Schedule {
    /// Wait for the first fire. `None` means no initial wait — first
    /// fire is governed by `interval`.
    delay: Option<Duration>,
    /// Period between subsequent fires. `None` means one-shot (only the
    /// initial fire ever happens).
    interval: Option<Duration>,
}

impl Schedule {
    /// How long the next `await` should wait, given whether the timer
    /// has already fired at least once.
    fn next_wait(&self, fired: bool) -> Duration {
        let chosen = if fired {
            self.interval.or(self.delay)
        } else {
            self.delay.or(self.interval)
        };
        chosen.expect("Schedule constructed without delay or interval")
    }

    fn repeats(&self) -> bool {
        self.interval.is_some()
    }
}

/// Lifecycle state.
///
/// - `Active { fired_once }` — the schedule ticks. For a one-shot
///   timer, `fired_once` flips to `true` after the first resolution
///   so subsequent awaits resolve instantly.
/// - `Disabled` — paused. Pending awaits suspend (no auto-firing).
///   `fire()` still wakes them; `enable()` / `set()` revives.
/// - `Rejected(Option<String>)` — terminal. All mutation methods
///   throw. `None` rejects with `undefined`; `Some(msg)` rejects with
///   `new Error(msg)`. We capture a string rather than the original
///   JS value because rquickjs `Persistent` doesn't reliably tear
///   down before the runtime — keeping a JS value alive in Rust risks
///   aborting at `JS_FreeRuntime`.
enum TimerState {
    Active { fired_once: bool },
    Disabled,
    Rejected(Option<String>),
}

pub struct Timer {
    /// Always present, so `.delay` / `.interval` keep reporting the
    /// schedule even while disabled. Updated only by `set()`.
    schedule: Arc<Mutex<Schedule>>,
    state: Arc<Mutex<TimerState>>,
    /// Pulsed on every state transition (including `set()`-driven
    /// schedule changes). Pending awaits re-read the state to decide
    /// how to proceed.
    state_notify: Arc<Notify>,
    /// Pulsed by `fire()`. Lives at the Timer level so `fire()` works
    /// while disabled (the disabled state has no schedule but still
    /// shares this notify with pending awaits).
    fire_notify: Arc<Notify>,
    workflow_closed: Arc<AtomicBool>,
    workflow_closed_notify: Arc<Notify>,
}

impl<'js> Trace<'js> for Timer {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> JsLifetime<'js> for Timer {
    type Changed<'to> = Timer;
}

impl<'js> JsClass<'js> for Timer {
    const NAME: &'static str = "Timer";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "then",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>,
                 this: This<Class<'js, Timer>>,
                 on_fulfilled: Value<'js>,
                 on_rejected: Value<'js>| {
                    timer_then(&ctx, &this.0, on_fulfilled, on_rejected)
                },
            )?,
        )?;

        proto.set(
            "disable",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, Timer>>| {
                    let b = this.0.borrow();
                    let mut s = b.state.lock();
                    terminal_check(&ctx, &s)?;
                    *s = TimerState::Disabled;
                    // Notify while we still hold the lock — keeps the
                    // pulse synchronized with the state transition so
                    // pending awaits that registered under the same
                    // lock are guaranteed to see it.
                    b.state_notify.notify_waiters();
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.set(
            "enable",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, Timer>>, value: Opt<bool>| {
                    let enable = value.0.unwrap_or(true);
                    let b = this.0.borrow();
                    let mut s = b.state.lock();
                    terminal_check(&ctx, &s)?;
                    *s = if enable {
                        // Equivalent to `set(this.{delay,interval})` —
                        // re-applies the schedule, clearing `fired_once`.
                        TimerState::Active { fired_once: false }
                    } else {
                        TimerState::Disabled
                    };
                    b.state_notify.notify_waiters();
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.set(
            "reject",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, Timer>>, reason: Opt<Value<'js>>| {
                    let message = match reason.0 {
                        Some(v) => extract_reason_message(&ctx, &v)?,
                        None => None,
                    };
                    let b = this.0.borrow();
                    let mut s = b.state.lock();
                    terminal_check(&ctx, &s)?;
                    *s = TimerState::Rejected(message);
                    b.state_notify.notify_waiters();
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.set(
            "fire",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, Timer>>| {
                    let b = this.0.borrow();
                    let mut s = b.state.lock();
                    terminal_check(&ctx, &s)?;
                    // Manually firing an Active timer counts as "the
                    // timer fired" — flips `fired_once` so a one-shot
                    // short-circuits subsequent awaits, and so a
                    // `{delay, interval}` combo moves on to the
                    // `interval` phase. Disabled stays Disabled —
                    // `fire()` doesn't implicitly re-enable.
                    if let TimerState::Active { fired_once } = &mut *s {
                        *fired_once = true;
                    }
                    // `notify_one` (not `notify_waiters`) so the
                    // pulse is preserved as a permit if no await is
                    // registered yet — common when `fire()` runs in
                    // a microtask before the underlying Promised
                    // future has even polled once.
                    b.fire_notify.notify_one();
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.set(
            "set",
            Function::new(
                ctx.clone(),
                |ctx: Ctx<'js>, this: This<Class<'js, Timer>>, arg: Value<'js>| {
                    let (delay, interval) = parse_timer_args(&ctx, &arg)?;
                    let b = this.0.borrow();
                    let mut s = b.state.lock();
                    terminal_check(&ctx, &s)?;
                    *b.schedule.lock() = Schedule { delay, interval };
                    *s = TimerState::Active { fired_once: false };
                    b.state_notify.notify_waiters();
                    Ok::<_, rquickjs::Error>(())
                },
            )?,
        )?;

        proto.prop(
            "enabled",
            Accessor::from(|this: This<Class<'js, Timer>>| -> bool {
                matches!(*this.0.borrow().state.lock(), TimerState::Active { .. })
            }),
        )?;

        proto.prop(
            "delay",
            Accessor::from(|this: This<Class<'js, Timer>>| -> Option<f64> {
                this.0
                    .borrow()
                    .schedule
                    .lock()
                    .delay
                    .map(|d| d.as_millis() as f64)
            }),
        )?;

        proto.prop(
            "interval",
            Accessor::from(|this: This<Class<'js, Timer>>| -> Option<f64> {
                this.0
                    .borrow()
                    .schedule
                    .lock()
                    .interval
                    .map(|d| d.as_millis() as f64)
            }),
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

/// Implements the thenable protocol so `await timer` works. Builds an
/// inner Promise via `Promised` whose future waits for the next firing,
/// then forwards the await's own `resolve`/`reject` callbacks through
/// the inner promise's `.then`. The await resolves when the inner
/// promise settles.
fn timer_then<'js>(
    ctx: &Ctx<'js>,
    this: &Class<'js, Timer>,
    on_fulfilled: Value<'js>,
    on_rejected: Value<'js>,
) -> JsResult<Value<'js>> {
    let b = this.borrow();
    let schedule = b.schedule.clone();
    let state = b.state.clone();
    let state_notify = b.state_notify.clone();
    let fire_notify = b.fire_notify.clone();
    let workflow_closed = b.workflow_closed.clone();
    let workflow_closed_notify = b.workflow_closed_notify.clone();
    drop(b);

    let promised = Promised::from(async move {
        // The future loops so we can re-snapshot on state changes
        // (`disable`, `enable`, `set`, `reject`). Inside the loop we
        // (re-)read the current state, build the right set of wake
        // sources, and wait on them. A state-change wake just
        // continues the loop; anything else terminates.
        loop {
            let fire = fire_notify.notified();
            let state_change = state_notify.notified();
            let closed = workflow_closed_notify.notified();
            tokio::pin!(fire);
            tokio::pin!(state_change);
            tokio::pin!(closed);

            // Register waiters *under the state lock*, then snapshot.
            // `notify_waiters` only wakes already-registered waiters,
            // so any state transition or `fire()` that happens after
            // we drop the lock is guaranteed to find us registered.
            // (Mutators take the same state lock, so they can't slip
            // a notify in between our `enable` and the snapshot.)
            let phase = {
                let s = state.lock();
                fire.as_mut().enable();
                state_change.as_mut().enable();
                closed.as_mut().enable();
                if workflow_closed.load(Ordering::Acquire) {
                    return Outcome::Resolved;
                }
                phase_from_state(&s, &schedule)
            };
            let wait = match phase {
                Phase::Resolved => return Outcome::Resolved,
                Phase::Rejected(msg) => return Outcome::Rejected(msg),
                Phase::Wait(wait) => wait,
            };

            let woke = match wait {
                Some(d) => {
                    let sleep = tokio::time::sleep(d);
                    tokio::pin!(sleep);
                    tokio::select! {
                        biased;
                        () = &mut state_change => WokeVia::StateChange,
                        () = &mut closed => WokeVia::Closed,
                        () = &mut fire => WokeVia::Fired,
                        () = &mut sleep => WokeVia::Fired,
                    }
                }
                None => tokio::select! {
                    biased;
                    () = &mut state_change => WokeVia::StateChange,
                    () = &mut closed => WokeVia::Closed,
                    () = &mut fire => WokeVia::Fired,
                },
            };

            match woke {
                WokeVia::StateChange => continue,
                WokeVia::Closed => return Outcome::Resolved,
                WokeVia::Fired => {
                    // Mark Active timers as having fired at least
                    // once. For a `{delay, interval}` combo, this is
                    // what makes the next await use `interval` rather
                    // than `delay`; for a pure one-shot, it makes the
                    // next await short-circuit. State may have
                    // transitioned meanwhile; only update if Active.
                    let mut s = state.lock();
                    if let TimerState::Active { fired_once } = &mut *s {
                        *fired_once = true;
                    }
                    return Outcome::Resolved;
                }
            }
        }
    });

    let inner: Value<'js> = promised.into_js(ctx)?;
    let Some(inner_obj) = inner.into_object() else {
        return Err(throw_err(
            ctx,
            "Timer: internal promise was not an object (rquickjs bug?)",
        ));
    };
    let inner_then: Function<'js> = inner_obj.get("then")?;
    inner_then.call((This(inner_obj), on_fulfilled, on_rejected))
}

/// One iteration of the wait loop sees one of these.
enum Phase {
    Resolved,
    Rejected(Option<String>),
    /// `Some(duration)` is an active wait with an auto-fire deadline;
    /// `None` is a disabled-paused wait with no deadline (still
    /// reachable via `fire()`, state change, or workflow close).
    Wait(Option<Duration>),
}

enum WokeVia {
    StateChange,
    Closed,
    Fired,
}

fn phase_from_state(state: &TimerState, schedule: &Arc<Mutex<Schedule>>) -> Phase {
    match state {
        TimerState::Rejected(msg) => Phase::Rejected(msg.clone()),
        TimerState::Disabled => Phase::Wait(None),
        TimerState::Active { fired_once } => {
            let sched = schedule.lock();
            if !sched.repeats() && *fired_once {
                return Phase::Resolved;
            }
            Phase::Wait(Some(sched.next_wait(*fired_once)))
        }
    }
}

/// Throws if the timer is in the terminal `Rejected` state. Used by
/// every mutating method to enforce the rule.
fn terminal_check<'js>(ctx: &Ctx<'js>, state: &TimerState) -> JsResult<()> {
    if matches!(state, TimerState::Rejected(_)) {
        return Err(throw_err(
            ctx,
            "Timer: rejected timers are terminal — create a new Timer instead",
        ));
    }
    Ok(())
}

enum Outcome {
    Resolved,
    Rejected(Option<String>),
}

impl<'js> IntoJs<'js> for Outcome {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self {
            Outcome::Resolved => Ok(Value::new_undefined(ctx.clone())),
            Outcome::Rejected(msg) => {
                // Look up the shared `TimerError` constructor via
                // userdata (set by `install_v1` after the io module
                // evaluates). Cloning the inner `Persistent` and
                // restoring it gives us a usable `Function<'js>` we
                // can call as a constructor.
                let err = match ctx.userdata::<TimerErrorUserData>() {
                    Some(guard) => {
                        let persistent = guard.0.clone();
                        drop(guard);
                        let ctor: Constructor<'js> = persistent.restore(ctx)?;
                        // `TimerError` is a class, so it must be
                        // invoked via `new` — `construct`, not `call`.
                        match msg {
                            Some(m) => ctor.construct((m,))?,
                            None => ctor.construct(())?,
                        }
                    }
                    None => {
                        // Should-never path — userdata was cleared
                        // (or never set). Fall back to a plain Error
                        // with `.name = "TimerError"` so we still
                        // raise something useful.
                        let message = msg.unwrap_or_else(|| "timer rejected".to_string());
                        let exc = Exception::from_message(ctx.clone(), &message)?;
                        exc.set("name", "TimerError")?;
                        exc.into_value()
                    }
                };
                Err(ctx.throw(err))
            }
        }
    }
}

/// Pull a string out of the rejection reason. Errors yield their
/// `.message`; anything else is coerced via JS `String(x)`. Pass
/// `undefined` (or no argument) and we capture `None`, so the await
/// rejects with `undefined`.
fn extract_reason_message<'js>(ctx: &Ctx<'js>, reason: &Value<'js>) -> JsResult<Option<String>> {
    if reason.is_undefined() || reason.is_null() {
        return Ok(None);
    }
    if let Some(obj) = reason.as_object()
        && let Ok(msg) = obj.get::<_, String>("message")
    {
        return Ok(Some(msg));
    }
    // Fall back to JS coercion. `String(x)` invokes the engine's
    // built-in conversion, including `.toString()` on objects.
    let to_string: Function<'js> = ctx.globals().get("String")?;
    let s: String = to_string.call((reason.clone(),))?;
    Ok(Some(s))
}

fn parse_timer_args<'js>(
    ctx: &Ctx<'js>,
    arg: &Value<'js>,
) -> JsResult<(Option<Duration>, Option<Duration>)> {
    if let Some(n) = arg.as_number() {
        return Ok((Some(duration_from_ms(ctx, n, "delay")?), None));
    }
    if let Some(obj) = arg.as_object() {
        let delay = read_ms_field(ctx, obj, "delay")?;
        let interval = read_ms_field(ctx, obj, "interval")?;
        if delay.is_none() && interval.is_none() {
            return Err(throw_err(
                ctx,
                "new Timer: object form needs `delay`, `interval`, or both",
            ));
        }
        return Ok((delay, interval));
    }
    Err(throw_err(
        ctx,
        "new Timer: expected milliseconds (number) or `{ delay?: number, interval?: number }`",
    ))
}

fn read_ms_field<'js>(
    ctx: &Ctx<'js>,
    obj: &Object<'js>,
    field: &'static str,
) -> JsResult<Option<Duration>> {
    let raw: Option<Value<'js>> = obj
        .get(field)
        .map_err(|_| throw_err(ctx, &format!("new Timer: `{field}` lookup failed")))?;
    let Some(v) = raw else {
        return Ok(None);
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let n = v.as_number().ok_or_else(|| {
        throw_err(
            ctx,
            &format!("new Timer: `{field}` must be a number (milliseconds)"),
        )
    })?;
    Ok(Some(duration_from_ms(ctx, n, field)?))
}

fn duration_from_ms<'js>(ctx: &Ctx<'js>, ms: f64, field: &'static str) -> JsResult<Duration> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(throw_err(
            ctx,
            &format!("new Timer: `{field}` must be a finite, non-negative number of milliseconds"),
        ));
    }
    Ok(Duration::from_millis(ms.round() as u64))
}

fn throw_err<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}
