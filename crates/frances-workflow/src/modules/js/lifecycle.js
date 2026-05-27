// `frances:v1/lifecycle` — graceful-shutdown hook.
//
// Workflows opt in by assigning a function:
//
//   import { lifecycle } from "frances:v1/lifecycle";
//   lifecycle.shutdown = async () => { /* save state, emit, ... */ };
//
// When the host requests shutdown (a dehydrate, or `workflow.exit()`),
// the runtime reads `lifecycle.shutdown` off this object, runs it, then
// closes the inbox so any `for await (const input of inbox)` loop in user
// code unwinds. The module just needs to export the object.

const __s = globalThis.__frances_v1_stash__;

export const lifecycle = __s.lifecycle;
