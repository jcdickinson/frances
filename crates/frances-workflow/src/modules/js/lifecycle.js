// `frances:v1/lifecycle` — graceful-shutdown hook.
//
// Workflows opt in by assigning a function:
//
//   import { lifecycle } from "frances:v1/lifecycle";
//   lifecycle.shutdown = async () => { /* save state, emit, ... */ };
//
// The module body kicks off a background async IIFE that awaits the
// host's shutdown signal (either `workflow.exit()` or a dehydrate
// request from the runtime). When the signal fires, the IIFE runs the
// registered handler (if any) and then closes the inbox so any
// `for await (const input of inbox)` loop in user code unwinds.

const __s = globalThis.__frances_v1_stash__;
const __wait = __s._waitForShutdown;
const __close = __s._closeInbox;

export const lifecycle = __s.lifecycle;

(async () => {
  await __wait();
  try {
    if (typeof lifecycle.shutdown === "function") {
      await lifecycle.shutdown();
    }
  } catch (_e) {
    // Best-effort. There's nowhere good to surface this — the
    // workflow is winding down anyway.
  }
  __close();
})();
