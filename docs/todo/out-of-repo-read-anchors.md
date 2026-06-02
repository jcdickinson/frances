# Don't anchor reads of files outside the editable root

`Read` renders every file with line anchors (`Word§content`) and registers an
editing baseline, regardless of where the file lives. For files **outside the
project** — dependency source under `~/.cargo/registry/src`, system headers,
anything you read for reference but will never edit — that's pure waste.

`Read` already works on such paths: `resolve_relative`
(`crates/frances-core/src/path_util.rs:8`) passes absolute paths through
untouched, so `Read { path: "/home/jono/.cargo/.../foo.rs" }` opens fine today.
It just over-anchors the result.

## Why anchoring out-of-repo reads is wrong

Anchors are an *edit-engine* concern — they exist to give
`file_replace_lines`/`file_insert_*`/etc. stable edit coordinates and to
register a baseline. You will never (and should never) edit vendored dependency
source through the anchor engine; it's blown away on the next `cargo` resolve.
So anchoring an out-of-repo read buys nothing and costs:

1. **Tokens** — a `Word§` prefix on every line of an often 1–2k-line dependency
   file. On a file you're only reading, that bloat is itself pressure back
   toward shell `cat`/`grep -A`, undercutting the point of pushing agents to
   the tool.
2. **State** — it registers the path as "opened for editing" in the editor
   baseline, a false signal for a file that isn't yours.
3. **Anchor-pool churn** — line hashing + word allocation for no downstream
   edit.

There's already precedent that anchorless reading is legitimate: `Read`'s
`into:` mode does a raw, anchorless, non-baseline read (it just stashes to a
variable instead of printing). The missing capability is "anchorless read that
prints to the model."

## Proposed behaviour

For a path that resolves **outside the editable root**:

- Render plain (line numbers, no `Word§` anchors), don't register an edit
  baseline.
- Reject edit ops (`file_replace_lines` etc.) against it with a clear
  "read-only reference, outside project" error. If you can't edit it, the
  anchors have no reader — and rejecting makes the contract explicit rather
  than letting an edit silently target source that'll be clobbered.

## Open question — how to define "the editable root"

This is the real design decision and the reason this isn't already done. Need a
concrete predicate for in-repo (anchored + editable) vs out-of-repo (plain,
read-only):

- **Workspace root of cwd** — find the jj/git workspace root containing cwd;
  inside → editable, outside → plain. Auto-detected, matches "my project"
  intuition, costs a root lookup. Leaning this way.
- **cwd subtree** — anything under cwd is in-repo. Simplest, no VCS lookup, but
  wrong if you `cd` into a subdir or legitimately edit a sibling path.
- **Configured writable roots** — explicit allowlist. Most precise, most
  machinery; probably overkill unless we already need multi-root.

Tension with the project's simplicity ethos: this adds a path-classification
branch + a boundary definition to what's currently one uniform render path.
Worth it (the token/state savings are real and recurring), but keep the
predicate minimal — auto-detect, ask nothing of the model.

## When to pick this up

Alongside [search-outside-cwd](search-outside-cwd.md). That one lets the agent
*find* external source via `Search`; this one keeps *reading* it cheap and
honest. Doing the search fix without this just trades `grep` calls for
token-bloated anchored reads.
