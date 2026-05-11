// `frances:v1/workflow` — re-exports `exit` from the install-time stash.
// The stash is deleted from globalThis after all modules evaluate, so
// `__s` here ends up captured in this module's local scope only.

const __s = globalThis.__frances_v1_stash__;

export const exit = __s.exit;
