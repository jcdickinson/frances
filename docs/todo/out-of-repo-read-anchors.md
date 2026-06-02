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

## Decision — how "the editable root" is defined

Settled. The classification is "is this path under any registered **editable
root**?" — anchored + editable inside, plain + read-only outside.

- **Configurable markers, default `[".jj", ".git"]`.** Discovery walks up from
  the session's cwd and stops at the first ancestor containing any configured
  marker. That ancestor is the workspace root.
- **No marker found → the session's initial cwd is the root.** Keeps the safe
  bias: you can always edit where you started.
- **Discover once, from the session's primary cwd — don't chase
  `current_cwd()` per read.** You opened frances in project X; reading project Y
  as reference *should* be plain + read-only until you explicitly add it. That
  matches the future `/add-dir` model and is simpler than recomputing per read.

### Why this predicate (failure-mode asymmetry)

Misclassification is **not** symmetric, so the predicate biases toward
*editable* when uncertain:

- **False "outside"** (an in-repo file treated as external) → plain render *and
  edits rejected*. A hard failure on a file you're allowed to edit. Bad.
- **False "inside"** (an external file treated as in-repo) → over-anchoring,
  i.e. today's waste. Annoying, not broken.

The VCS workspace root is the right lower bound: everything in the repo stays
editable; only genuinely-outside paths flip to read-only. The dangerous
direction is structurally avoided. This is also why "cwd subtree" was rejected —
`cd` into a subdir would start *rejecting valid edits* on the rest of the repo.

### Plural now, no `/add-dir` machinery yet

`/add-dir` is on the roadmap, so make the primitive plural and build nothing
else:

- Session holds `editable_roots: Vec<PathBuf>`. Init pushes the discovered root
  as `roots[0]`.
- `is_editable(path)` = `roots.iter().any(|r| within(r, path))`.
- `/add-dir` later is just a `push`; the read/edit classification doesn't
  change.

Do **not** build now: the command itself, config-file loading of extra dirs, or
cross-session persistence of the set. No present reader — stays deleted until
`/add-dir` lands.

## Implementation notes

The mechanism is mostly already present — this is a classification branch, not a
new renderer.

- **Plain read = `read_raw_inner` + line numbering.** `read_raw_inner`
  (`crates/frances-workflow/src/modules/file.rs:248`) already does an anchorless
  disk read, loop-guarded via `sess.is_loop`/`record_loop`, with **no** baseline
  registration (it's what `into:` mode uses). The out-of-repo path reuses it,
  adds line numbers (no `Word§`), and honours `ranges`. Anchored in-repo reads
  (`read_file_inner` → `sess.read_file`) are unchanged.
- **Edit reject is UX, not load-bearing.** An out-of-repo path never registered
  a baseline, so an edit already fails with "unknown anchor / read before
  editing". The explicit `FileToolError::OutsideProject { path }` just makes the
  contract clear. Shared predicate between read and edit; lives next to
  `resolve_relative` in `crates/frances-core/src/path_util.rs`.
- **Canonicalize before the prefix check** — a raw `starts_with` is unsound
  against `..` and symlinks. Reads can canonicalize the target (it exists, we
  stat it first); `file_new` targets a non-existent path, so canonicalize the
  *parent* there.

## Shell integration — export the primary root

Seed `$FRANCES_ROOT` = `roots[0]` at shell spawn so the model can
`cd "$FRANCES_ROOT"` after wandering the persistent shell. There's precedent:
the shell already supports exported env (`set_var_inner`,
`crates/frances-workflow/src/modules/shell.rs:419`, emits `export NAME=…`).

Keep it **decoupled** from the read classification — it's a one-liner at spawn
time that happens to read the same `roots[0]`. The file/search tools resolve
against `current_cwd()`, not the shell's cwd, so the shell drifting never
affects reads/edits; this is purely a navigation convenience. Export only the
primary root, not the whole set (a `$FRANCES_ROOTS` list is YAGNI until
`/add-dir` exists).

## When to pick this up

Now. [search-outside-cwd](search-outside-cwd.md) is landing (the `root:` arg +
`~` expansion are in the working copy), so the agent can already *find* external
source via `Search`; without this, that just trades `grep` calls for
token-bloated anchored reads. This keeps *reading* external source cheap and
honest.
