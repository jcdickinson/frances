// `frances:v1/inbox` — re-exports the async-iterable user-input stream.
//
// Items yielded by `inbox` are either a user message (`{ content }`) or
// the `INTERRUPT` sentinel (Esc in the UI). `INTERRUPT` is the
// process-wide registered symbol `Symbol.for("frances.interrupt")`, so
// `value === INTERRUPT` is a reliable identity check.

export const { inbox } = globalThis.__frances_v1_stash__;

export const INTERRUPT = Symbol.for("frances.interrupt");
