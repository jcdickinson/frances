// `frances:v1/io` — Timer + TimerError.
//
// `Timer` is a Rust-backed class re-exported from the install-time
// stash. `TimerError` is a plain JS class defined here — keeping the
// class body alongside the export feels more natural than evaling it
// from a Rust string, and the Rust reject path retrieves the
// constructor from this module's namespace after eval.

const __s = globalThis.__frances_v1_stash__;

export const Timer = __s.Timer;

export class TimerError extends Error {
  constructor(message = "timer rejected") {
    super(message);
    this.name = "TimerError";
  }
}
