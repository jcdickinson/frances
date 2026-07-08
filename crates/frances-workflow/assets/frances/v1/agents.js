// `frances:v1/agents` — instruction discovery primitives.
//
// Rust-backed async functions that discover agent instruction files
// (AGENTS.md / CLAUDE.md) in global, local, and nested scopes.
//
// Exports:
// - `discoverGlobalAgents()`  → Promise<Array<{path, content}> | null>
// - `discoverLocalAgents()`   → Promise<Array<{path, content}> | null>
// - `discoverNestedAgents()`  → Promise<Array<string> | null>
//
// Each function is a thin JS wrapper around a Rust-backed primitive
// captured from the install stash. Dedup (canonicalize + content-hash)
// is performed on the Rust side.

const __s = globalThis.__frances_v1_stash__;

export const discoverGlobalAgents = __s._discoverGlobalAgents;
export const discoverLocalAgents = __s._discoverLocalAgents;
export const discoverNestedAgents = __s._discoverNestedAgents;

