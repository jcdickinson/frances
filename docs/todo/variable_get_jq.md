# `variable_get` gains an optional jq filter

`variable_get` today returns the whole stored value (`crates/frances-workflow/src/modules/desc/variable_get.md:1-7`). When a variable holds something large — captured shell output, a fetched payload, a serialised plan — the agent has two ways to look at a piece of it:

1. `variable_get` the whole thing and pay the full blob through the model.
2. `variable_assign` with a jq filter into a new destination, then `variable_get` that destination. Two calls plus a wasted variable name.

In practice the agent reaches for neither. Once a variable is exported to the shell session (e.g. as `$UI_RAW`), the easiest reach is `sed -n '...'` or `python3 -c '...'` against the env var. Observed in the wild: an agent slicing two line ranges out of `UI_RAW` via a python heredoc rather than `variable_assign` → `variable_get`. The capability existed, the affordance was hidden.

## Proposal

Add an optional `filter` to `variable_get`:

```
variable_get { name, filter? }
```

- The stored value is bound as `.` and the filter is evaluated exactly like `variable_assign` (single jq output required), but the result is **returned to the caller** instead of being written back to a destination.
- No `inputs` parameter: if the agent needs to combine multiple variables, that's an `assign`, not a `get`.
- Errors surface the jq compile / runtime error verbatim — same as `variable_assign`.

This is "variable_get with a lens". Cheaper than `assign → get` (one call, no destination littering) and generalises naturally over all stored shapes — strings, arrays, objects — because jq's slice and indexing operators work uniformly.

## Why not a `ranges` parameter

Initial sketch was `variable_get { name, ranges? }` mirroring `file_read`. Rejected: variables are JSON values, not necessarily text. `ranges` only makes sense for strings, and would have to error or no-op on objects/numbers/booleans. jq's `.[a:b]` handles the same intent for both strings and arrays, and the file_read text case is `split("\n") | .[a:b] | join("\n")` — verbose but discoverable and shape-correct.

## Documentation

Update `desc/variable_get.md` with at least one worked example showing the text-slice idiom — that's the affordance gap that drove the python reach. Suggested:

```
{ name: "UI_RAW",
  filter: "split(\"\n\") | to_entries | (.[459:640] + .[739:930]) | map(\"\(.key+1): \(.value)\") | join(\"\n\")" }
```

Mention explicitly that the variable is the `.` input — the agent will reach for `$name`-style references by analogy with `variable_assign` and get a confusing jq error otherwise.

## Critical files

- `crates/frances-workflow/src/modules/variable.rs` (or wherever `variable_get` lives — check `mod.rs` registration) — add the optional filter, reuse the jq evaluator from `variable_assign`.
- `crates/frances-workflow/src/modules/desc/variable_get.md` — document the new arg, the text-slice idiom, and the "value is `.`, not `$name`" gotcha.
- `crates/frances-workflow/src/modules/jaq.rs` — likely the shared jq evaluator; confirm it can be called read-only without a destination.

## Out of scope

- A separate `variable_slice` / range tool. jq subsumes it.
- Persisting the filter result back to the variable. That's what `variable_assign` is for.
