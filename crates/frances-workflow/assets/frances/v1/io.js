// `frances:v1/io` — Timer + TimerError.
//
// Timer is a pure-JS class. It composes the private Rust sleep
// primitive (`_setSleep` / `_clearSleep`, captured from the install-time
// stash) into the full delay+interval+disable+enable+fire+reject+set
// semantics, and adds an `AbortSignal` option for cancellation.
//
// All of the timer's state lives here (active/disabled, fired-once,
// rejection, waiter queue, signal listener lifecycle); Rust only knows
// how to sleep, how to be cancelled, and how to surface workflow
// shutdown as a `"closed"` resolution on its token.

const { _setSleep, _clearSleep } = globalThis.__frances_v1_stash__;

export class TimerError extends Error {
  constructor(message = "timer rejected") {
    super(message);
    this.name = "TimerError";
  }
}

function validateMs(value, label, prefix) {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "number" || !isFinite(value) || value < 0) {
    throw new TypeError(
      `${prefix}: \`${label}\` must be a finite, non-negative number of milliseconds`,
    );
  }
  return value;
}

function parseSchedule(arg, prefix) {
  if (typeof arg === "number") {
    if (!isFinite(arg) || arg < 0) {
      throw new TypeError(
        `${prefix}: delay must be a finite, non-negative number of milliseconds`,
      );
    }
    return { delay: arg, interval: undefined };
  }
  if (arg !== null && typeof arg === "object" && !Array.isArray(arg)) {
    const delay = validateMs(arg.delay, "delay", prefix);
    const interval = validateMs(arg.interval, "interval", prefix);
    if (delay === undefined && interval === undefined) {
      throw new TypeError(
        `${prefix}: object form needs \`delay\`, \`interval\`, or both`,
      );
    }
    return { delay, interval };
  }
  throw new TypeError(
    `${prefix}: expected milliseconds (number) or \`{ delay?: number, interval?: number }\``,
  );
}

function isAbortSignalLike(s) {
  return (
    s !== null
    && typeof s === "object"
    && typeof s.aborted === "boolean"
    && typeof s.addEventListener === "function"
    && typeof s.removeEventListener === "function"
  );
}

const REJECTED_TERMINAL_MSG =
  "Timer: rejected timers are terminal — create a new Timer instead";

export class Timer {
  #delay;
  #interval;
  #enabled = true;
  #firedOnce = false;
  #rejected = false;
  #rejectionReason = undefined;
  // Each entry is { resolve, reject } from a pending `then()` call.
  #waiters = [];
  // Set by `fire()` when no waiter is queued; consumed by the next
  // `then()`. Mirrors Rust `Notify::notify_one` permit storage.
  #pendingFirePulse = false;
  // Current in-flight SleepToken from the primitive, or null.
  #sleepToken = null;
  // Removes the abort listener from the constructor-time signal.
  #signalCleanup = null;

  constructor(arg) {
    const isObj = arg !== null && typeof arg === "object" && !Array.isArray(arg);
    const signal = isObj ? arg.signal : undefined;
    const { delay, interval } = parseSchedule(arg, "new Timer");
    this.#delay = delay;
    this.#interval = interval;

    if (signal !== undefined) {
      if (!isAbortSignalLike(signal)) {
        throw new TypeError("new Timer: `signal` must be an AbortSignal");
      }
      if (signal.aborted) {
        this.#rejected = true;
        this.#rejectionReason = signal.reason;
      } else {
        const onAbort = () => this.#onAbort(signal.reason);
        signal.addEventListener("abort", onAbort);
        this.#signalCleanup = () => signal.removeEventListener("abort", onAbort);
      }
    }
  }

  get enabled() { return !this.#rejected && this.#enabled; }
  get delay() { return this.#delay; }
  get interval() { return this.#interval; }

  then(onFulfilled, onRejected) {
    if (this.#rejected) {
      return Promise.reject(this.#rejectionReason).then(onFulfilled, onRejected);
    }
    if (this.#pendingFirePulse) {
      this.#pendingFirePulse = false;
      return Promise.resolve(undefined).then(onFulfilled, onRejected);
    }
    if (this.#interval === undefined && this.#firedOnce) {
      return Promise.resolve(undefined).then(onFulfilled, onRejected);
    }
    let resolve, reject;
    const p = new Promise((r, j) => { resolve = r; reject = j; });
    this.#waiters.push({ resolve, reject });
    this.#ensureScheduled();
    return p.then(onFulfilled, onRejected);
  }

  disable() {
    this.#assertNotRejected();
    this.#enabled = false;
    this.#cancelToken();
  }

  enable(value = true) {
    this.#assertNotRejected();
    if (!value) {
      this.disable();
      return;
    }
    this.#firedOnce = false;
    this.#enabled = true;
    if (this.#waiters.length > 0) {
      this.#ensureScheduled();
    }
  }

  fire() {
    this.#assertNotRejected();
    if (this.#enabled) {
      this.#firedOnce = true;
    }
    if (this.#waiters.length > 0) {
      this.#cancelToken();
      this.#drainWaitersResolve();
    } else {
      this.#pendingFirePulse = true;
    }
  }

  reject(reason) {
    this.#assertNotRejected();
    this.#rejected = true;
    this.#rejectionReason = reason === undefined ? new TimerError() : reason;
    this.#cancelToken();
    this.#drainWaitersReject(this.#rejectionReason);
    this.#removeSignalListener();
  }

  set(arg) {
    this.#assertNotRejected();
    const { delay, interval } = parseSchedule(arg, "Timer.set");
    this.#delay = delay;
    this.#interval = interval;
    this.#firedOnce = false;
    this.#enabled = true;
    this.#cancelToken();
    if (this.#waiters.length > 0) {
      this.#ensureScheduled();
    }
  }

  #assertNotRejected() {
    if (this.#rejected) throw new Error(REJECTED_TERMINAL_MSG);
  }

  #drainWaitersResolve() {
    const ws = this.#waiters;
    this.#waiters = [];
    for (const w of ws) w.resolve(undefined);
  }

  #drainWaitersReject(reason) {
    const ws = this.#waiters;
    this.#waiters = [];
    for (const w of ws) w.reject(reason);
  }

  #cancelToken() {
    if (this.#sleepToken !== null) {
      _clearSleep(this.#sleepToken);
      this.#sleepToken = null;
    }
  }

  #ensureScheduled() {
    if (!this.#enabled || this.#rejected || this.#sleepToken !== null) return;
    let ms;
    if (this.#firedOnce) {
      if (this.#interval === undefined) return;  // one-shot already fired
      ms = this.#interval;
    } else {
      ms = this.#delay !== undefined ? this.#delay : this.#interval;
    }
    const token = _setSleep(ms);
    this.#sleepToken = token;
    token.then((reason) => this.#onSleepResolve(token, reason));
  }

  #onSleepResolve(token, reason) {
    // Stale callback: we superseded this token (via cancel, replace, or
    // reject) before its resolution landed.
    if (this.#sleepToken !== token) return;
    this.#sleepToken = null;
    if (reason === "fired") {
      this.#firedOnce = true;
      this.#drainWaitersResolve();
    } else if (reason === "closed") {
      // Graceful workflow shutdown: matches the existing inbox /
      // exit() behaviour where pending awaits resolve rather than throw.
      this.#drainWaitersResolve();
    }
    // "cancelled" is never reachable here — we always null out
    // #sleepToken before invoking _clearSleep, so the stale check
    // above swallows it.
  }

  #onAbort(reason) {
    if (this.#rejected) return;
    this.#rejected = true;
    this.#rejectionReason = reason;
    this.#cancelToken();
    this.#drainWaitersReject(reason);
    this.#removeSignalListener();
  }

  #removeSignalListener() {
    if (this.#signalCleanup !== null) {
      this.#signalCleanup();
      this.#signalCleanup = null;
    }
  }
}
