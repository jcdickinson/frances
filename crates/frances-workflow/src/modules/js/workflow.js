// `frances:v1/workflow` — re-exports `exit` + `setStatus` from the
// install-time stash. The stash is deleted from globalThis after all
// modules evaluate, so the destructured bindings end up captured in this
// module's local scope only.
//
// `setStatus(text | null)` drives the TUI footer busy indicator:
// a string shows the text with a spinner, `null` hides it.

export const { exit, setStatus } = globalThis.__frances_v1_stash__;
