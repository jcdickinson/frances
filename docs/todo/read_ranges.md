# `file_read` with disjoint line ranges

Today `file_read` returns the entire file (anchored, line by line). The agent has no way to ask for a subset, so when a file is large or it only needs two distant regions, it shells out to `sed -n '460,640p;740,930p' path` or a `python3 -c` slicer. That bypasses the anchor system entirely — anything the agent then wants to edit forces a follow-up full `file_read` to re-establish the baseline.

## Proposal

Add an optional `ranges` parameter to `file_read`:

```
file_read { path, ranges?: [[start, end], ...], into? }
```

- `ranges` is a list of 1-indexed, inclusive `[start, end]` pairs.
- Returned output is line-anchored exactly as today (`Word§content`), with the requested ranges concatenated in document order. Insert a single separator line (e.g. `…§`) between non-adjacent ranges so the agent can see the gap without it being mistakable for an anchor.
- Omitted `ranges` = current whole-file behavior.
- Mutually exclusive with `into` (which already bypasses anchoring; ranges over a raw byte dump is incoherent).

## Anchors are still whole-file

The crucial invariant: even when the caller asks for a slice, the editor session anchors **every line of the file**, not just the returned ones. This is already what `read_file_inner` does (`crates/frances-workflow/src/modules/file.rs:159-165` — it passes the full `lines` vector to `sess.read_file`); we just don't expose any of the lines beyond the slice. The cost is per-line hashing, bounded by file size, and it means a subsequent `file_replace`/`file_insert_*` at a line *outside* the returned slice still resolves its anchor without a re-read.

Document this explicitly in `desc/file_read.md` so the agent understands that ranges are a *display* filter, not a baseline filter — reading lines 460-640 still licenses edits at line 200.

## Edge cases

- **Overlapping / adjacent ranges**: merge silently. Agents will sometimes emit redundant ranges and a hard error is friction.
- **Reverse ranges (`end < start`)**: hard error. Almost certainly a bug in the caller.
- **Past EOF**: truncate the offending range to EOF; not an error.
- **Total-line cap**: apply the existing cap to the union of ranges. If exceeded, error with the offending count so the agent can narrow.

## Critical files

- `crates/frances-workflow/src/modules/file.rs` — `read_file_inner`, the JS-side argument plumbing.
- `crates/frances-workflow/src/modules/desc/file_read.md` — describe the new arg and the "ranges don't shrink the anchor baseline" invariant.
- `crates/frances/src/edit_session.rs` — sanity-check that the existing baseline contract isn't expressed in terms of "what was last shown to the model".

## Out of scope

- Reading from variables — covered by `variable_get_jq.md`.
- Regex-bracketed slicing (`/start/,/end/p` style) — agent can still shell out for the rare case.
- Streaming / paginated reads of files larger than the cap. Same answer as today: ask for a range.
