// `frances:v1/workflow` — re-exports `exit` from the install-time stash.
// The stash is deleted from globalThis after all modules evaluate, so
// the destructured `exit` ends up captured in this module's local scope only.

export const { exit } = globalThis.__frances_v1_stash__;
