# `file_replace_all` (and rename `file_replace` → `file_replace_range`)

`file_replace` replaces exactly one anchor-bracketed range (`crates/frances-workflow/src/modules/desc/file_replace.md:1-10`). For a genuinely cross-cutting change — `s/old_name/new_name/g`, swap a recurring import, normalise quoting across a file — the agent has to issue N calls, each requiring a fresh anchor pulled from the last `file_read`. The friction is high enough that the agent reaches for `sed -i 's/.../.../g' file` through the shell tool, which bypasses the anchor cache, bypasses the post-edit formatter pass, and produces an opaque diff. We then have to re-read the file just to get back into the anchor protocol.

The fix is to give the agent a sanctioned, unanchored, cross-cutting replace — and to rename `file_replace`, which has been a misnomer since it never actually replaced "all" of anything.

## Rename

| Today           | After this change      |
|-----------------|------------------------|
| `file_replace`  | `file_replace_range`   |
| (n/a)           | `file_replace_all`     |

`file_replace_range` is what the existing tool actually does: replace one contiguous range of lines, bracketed by anchors. The rename touches the JS export name, the desc filename, and the descriptions object in `crates/frances-workflow/src/modules/file.rs:56-65`. Worth doing in the same commit as the new tool — the new name makes the family read coherently:

- `file_replace_range` — precision, anchor-bracketed, one range.
- `file_replace_all`   — cross-cutting, regex, unanchored.

## `file_replace_all` proposal

```
file_replace_all { path, find, replacement, count? }
```

- `path`: file to edit. Must have been opened this turn via `file_read` (or just minted via `file_new` / `file_overwrite`), same prerequisite as the other edit tools — so the engine has a baseline to write through.
- `find`: a Rust `regex` crate pattern. No lookaround (not in `regex`); if we ever need it, swap to `fancy-regex` later — out of scope here.
- `replacement`: template string using `regex`'s replace syntax (`$1`, `${name}`, `$$` for literal `$`).
- `count`: optional maximum match count. If the regex matches more than `count` times, the call fails *without writing* and reports the actual count. This is a footgun protector — agents will sometimes paste a too-loose regex (`.`, `\w+`) and a hard cap turns a destructive surprise into an error message.

### Behaviour

1. Pull the cached file from the edit session (same lookup as `file_replace` today).
2. Compile `find` once. If invalid, return the `regex` error verbatim.
3. Run `Regex::replace_all` against the full file text.
4. If `count` is set and the match count exceeds it, return an error including the count; do not write.
5. If zero matches: succeed with a "no changes" result, no formatter run, no write. The agent shouldn't have to handle "succeeded but did nothing" as an error.
6. On a real change: run the project formatter, write to disk, re-anchor the file, return a diff block exactly like `file_replace_range` does.

### Deliberately not included

- **No anchor input.** The whole point is to escape the anchor model for the cross-cutting case. Re-anchoring happens *after* the write.
- **No address ranges, hold buffer, multiple commands.** That's the sed tarpit. If the agent needs "replace within lines 100-200 only", use `file_replace_range` with a wider bracket, or post-process. Resist the urge to grow this into a sed.
- **No `from` variable for the replacement.** `file_replace_range` has it because replacements are sometimes large; here they're regex templates and almost always short. Add it later if the pattern shows up.
- **No `dry_run`.** `count` covers the "did I match what I think I matched?" check; a dry-run mode duplicates that without giving more information.

## Critical files

- `crates/frances-workflow/src/modules/file.rs` — register the new export alongside `file_replace_range`, share the post-edit formatter + re-anchor tail with the existing path.
- `crates/frances-workflow/src/modules/desc/file_replace_all.md` (new) — describe the tool, the regex template syntax, and the `count` safety hatch. Include a worked example.
- `crates/frances-workflow/src/modules/desc/file_replace.md` → rename to `desc/file_replace_range.md`. Update the body's "Replace one contiguous range" leader to make the name self-evident.
- `crates/frances-workflow/src/modules/js/`  — wherever the JS bindings export the edit family; rename the symbol.
- `crates/frances/src/edit_session.rs` — confirm the write path can be invoked without an anchor pair as input.

## Out of scope

- Multi-file replace (one path per call; the agent can loop).
- Replacing into a variable instead of writing the file (we don't have `into` on `file_replace` either; symmetry can wait).
- Smarter conflict reporting against concurrent external edits — same drift-reconcile story as the rest of the edit family.
