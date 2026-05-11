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
//   - AbortSignal.timeout is not implemented; the runtime has no
//     global setTimeout. Use `Timer` from "frances:v1/io" with
//     AbortController.abort instead.

import { DOMException } from "whatwg:dom";

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

  static any(signals) {
    const out = new AbortSignal();
    for (const s of signals) {
      if (s.aborted) { out._doAbort(s.reason); return out; }
      s.addEventListener("abort", () => out._doAbort(s.reason));
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
