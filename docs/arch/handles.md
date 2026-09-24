# Tool handles and the query layer

Forward-looking design — not yet implemented. Captures the conclusions of a design discussion about how search/list/read tools should hand results to the model without dumping them into context.

This is an optional tool-provider design, not a requirement for the host loop or
the [Model Content + Hooks Protocol](../model-content-hooks-protocol.md). Any future
handle store belongs to the provider that creates the handles. Server-owned
handles must not rely on Frances's JS variables or embedded runtime: those are
being removed. MCP resources and tools can expose retained results without
requiring the host to adopt this particular handle schema or query language.
Handles do not add tools dynamically; their reader/query tools must already be
selected in the model context's fixed tool set.

## Problem

The default tool shape is "run, return everything to the model." For small results that's fine. For anything larger — file listings, grep hits, file contents in bulk — it has two failure modes:

1. **Context bloat.** The full result lands in the conversation whether or not the model uses it. A 200-path `rg --files` answer costs the same context whether the next step needs all 200 paths or just five.
2. **Re-querying expensive operations.** Narrowing a result means running the underlying tool again. Cheap for `rg`; expensive for "read these 50 files," "list a deep directory tree," or anything that hit the network.

Bash env vars (`FILES=$(rg --files)`) partially solve (1) but only inside one shell process, only as flat strings, and the model has to remember to use the pattern. They don't solve (2) because there's no structured form to slice.

## Design

A **handle** is a typed, named, out-of-shell value produced by a tool. Tools that return potentially large results return a handle plus an inline summary; the full payload stays in the handle namespace until something explicitly consumes it.

### Handle namespace

- Lives outside any shell process. Survives across shell invocations and across tool calls of different kinds.
- Each handle has a **type** (`FileList`, `MatchList`, `FileContents`, `JsonDoc`, …) and a **name** (model-chosen or tool-assigned).
- Types carry schema. `MatchList` is `[{path, line, column, snippet}]`, not an opaque blob.

### Inline summary discipline

A handle that returns silently is a footgun. Every tool that produces a handle **must** also return inline:

- The handle name and type.
- A count.
- A small sample (top N entries) so the model can sanity-check the filter without materialising the whole thing.

Without the sample, the model wastes a turn inspecting the handle just to know whether the query was right.

### Shell injection: `include`

The shell tool takes an `include: ["name1", "name2", ...]` parameter. Each named handle is materialised into the shell's environment for that one call — typically as an env var or a temp file path the command can reference. There is **no in-string substitution syntax**; opt-in is per-call, explicit, and visible in the tool arguments.

```
shell {
  cmd: "xargs -a $files rg TODO",
  include: ["files"],
}
```

This keeps two namespaces cleanly separated: shell env (process-local, stringly-typed) and the handle namespace (durable, structured). The `include` list is the only bridge.

### Two-tier query layer

Manipulating handles needs query support, but a full scripting interpreter is overkill for what's actually wanted (filter, slice, project). Two tiers:

**Tier 1 — typed handle ops.** A small, named set of operations exposed as ordinary tools:

- `take` / `slice` — bounded subset
- `filter` — regex on a named field
- `pluck` — extract one field as a flat list (e.g. `MatchList` → `FileList` of paths)
- `count`
- `unique`
- `materialize` — flatten a handle into the conversation when the model actually wants to read it

These cover the common path with discoverable, single-purpose tools and no language at all.

**Tier 2 — jaq for the long tail.** When the built-ins don't fit, drop to a jq-style query against the handle's JSON shape. We use [`jaq`](https://github.com/01mf02/jaq), the pure-Rust jq implementation: faster than C jq, embeddable as a crate, no FFI.

```
.matches | map(select(.path | test("^src/parser"))) | .[0:20]
```

### Why not a Turing-complete interpreter

Embedding Lua / QuickJS / Starlark / Python for handle queries was considered and
rejected. The separate QuickJS workflow runtime currently exists but is being
removed under the new host architecture. A general interpreter for queries adds:

- **Costs.** Sandboxing review, embedding maintenance, error-surface translation, another language for users to learn.
- **Additional execution authority.** Querying retained data should not implicitly
  execute more tools. The Rust host controls execution; server hooks and the
  selected controller influence it through explicit protocol operations.

Any query implementation needs explicit resource and execution limits; choosing
jaq does not by itself guarantee termination or isolation. The optional query
surface is not a reason to preserve the embedded JS workflow runtime.

## Open questions

- Naming convention for handles (model-chosen names vs. auto-assigned `@matches_1`).
- Lifecycle: when does a handle get GC'd? On session end? Explicit `drop`? LRU under a memory cap?
- `describe @handle` as a standard introspection op so the model can recover context after a long gap.
- Whether handle types should be open (tool-defined) or a closed enum. Open is more flexible; closed makes the built-in tier 1 ops easier to type-check.
