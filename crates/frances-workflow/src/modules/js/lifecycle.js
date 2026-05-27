// `frances:v1/lifecycle` — graceful-shutdown hook.
//
// Workflows opt in by assigning a function:
//
//   import { lifecycle } from "frances:v1/lifecycle";
//   lifecycle.shutdown = async () => { /* save state, emit, ... */ };
//
// When the host requests shutdown (a dehydrate, or `workflow.exit()`),
// the runtime invokes the registered handler, then closes the inbox so
// any `for await (const input of inbox)` loop in user code unwinds. We
// expose the handler to the runtime through a hidden, non-enumerable
// global "runner"; closing the inbox is the runtime's job.

const __s = globalThis.__frances_v1_stash__;

export const lifecycle = __s.lifecycle;

Object.defineProperty(globalThis, "__frances_shutdown_runner", {
  value: async () => {
    if (typeof lifecycle.shutdown === "function") {
      try {
        await lifecycle.shutdown();
      } catch (_e) {
        // Best-effort. There's nowhere good to surface this — the
        // workflow is winding down anyway.
      }
    }
  },
  enumerable: false,
  configurable: true,
});
