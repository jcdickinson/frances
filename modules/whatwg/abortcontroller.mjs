// Hand-rolled AbortController/AbortSignal.
//
// The npm `abortcontroller-polyfill` package's ESM build inlines a
// CJS-style require shim and an EventTarget polyfill that doesn't load
// cleanly as a standalone module, so we ship our own small version
// here. Shape follows the WHATWG DOM spec for the parts user scripts
// are likely to reach.
//
// Intentionally minimal:
//   - AbortSignal does NOT extend EventTarget — it carries its own
//     listener set. Capture phase / bubbling don't apply.
//   - `AbortSignal.timeout(ms)` builds on the workflow host's private
//     sleep primitive (`_setSleep`) captured at module load time from
//     `globalThis.__frances_v1_stash__`. The runtime has no global
//     `setTimeout`; this is the canonical way to schedule a one-shot
//     wake.

import { DOMException } from "whatwg:dom";

const { _setSleep } = globalThis.__frances_v1_stash__;

class AbortSignal {
  constructor() {
    this._aborted = false;
    this._reason = undefined;
    this._listeners = new Set();
    this.onabort = null;
  }

  get aborted() { return this._aborted; }
  get reason() { return this._reason; }

  throwIfAborted() {
    if (this._aborted) throw this._reason;
  }

  addEventListener(type, listener) {
    if (type !== "abort" || typeof listener !== "function") return;
    this._listeners.add(listener);
  }

  removeEventListener(type, listener) {
    if (type !== "abort") return;
    this._listeners.delete(listener);
  }

  dispatchEvent(event) {
    if (event && event.type === "abort") this._fire(event);
    return true;
  }

  _doAbort(reason) {
    if (this._aborted) return;
    this._aborted = true;
    this._reason = reason !== undefined
      ? reason
      : new DOMException("signal is aborted without reason", "AbortError");
    this._fire({ type: "abort", target: this });
  }

  _fire(event) {
    if (typeof this.onabort === "function") {
      try { this.onabort.call(this, event); } catch (_) { /* swallow */ }
    }
    for (const l of [...this._listeners]) {
      try { l.call(this, event); } catch (_) { /* swallow */ }
    }
  }

  static abort(reason) {
    const s = new AbortSignal();
    s._doAbort(reason);
    return s;
  }

  static timeout(ms) {
    if (typeof ms !== "number" || !isFinite(ms) || ms < 0) {
      throw new TypeError(
        "AbortSignal.timeout: ms must be a finite, non-negative number",
      );
    }
    const signal = new AbortSignal();
    const token = _setSleep(ms);
    // Pin the token to the signal: the sleep primitive cancels itself
    // on Drop, so without a strong reference the local `token` binding
    // would be GC-eligible after this function returns and the timer
    // would never fire. If the signal itself is GC'd, the token goes
    // with it and cancellation is the right outcome.
    signal._timeoutToken = token;
    token.then((reason) => {
      signal._timeoutToken = null;
      if (reason === "fired") {
        signal._doAbort(new DOMException("signal timed out", "TimeoutError"));
      }
      // "closed"   — workflow tearing down; leave signal unaborted.
      // "cancelled" — only reachable if the signal was GC'd; no-op.
    });
    return signal;
  }

  static any(signals) {
    const out = new AbortSignal();
    const cleanups = [];
    const propagate = (reason) => {
      // Drop the source-side listeners first so we don't end up holding
      // the closures (and `out`) alive on every source signal forever.
      for (const c of cleanups) c();
      cleanups.length = 0;
      out._doAbort(reason);
    };
    for (const s of signals) {
      if (s.aborted) {
        propagate(s.reason);
        return out;
      }
      const listener = () => propagate(s.reason);
      s.addEventListener("abort", listener);
      cleanups.push(() => s.removeEventListener("abort", listener));
    }
    return out;
  }
}

class AbortController {
  constructor() {
    this._signal = new AbortSignal();
  }
  get signal() { return this._signal; }
  abort(reason) { this._signal._doAbort(reason); }
}

export { AbortController, AbortSignal };
