# Result entities for the silent tools

Tool calls no longer reach the transcript. `wrapTool` in
`assets/workflows/main.ts` used to push a `ToolUseSection` ("→ shell_run
jj show --stat") ahead of every handler; beside the result the tool
renders for itself that was the same information twice, so the marker —
and `ToolUseSection` with it — is gone.

That works for tools whose result is already a transcript row:

- `shell_run` — the shell entity (`[success] <cmd>` inline, openable as
  a tab).
- the `file_*` edit tools — a `DiffSection`.

It leaves the rest with **no trace at all**. A turn that only searches
and reads files currently looks like the assistant sat there silent:

- `file_read`
- `file_find_or_grep`
- `var_get` / `var_set` / `var_edit`
- `shell_set` / `shell_get`

## The fix is a result row, not the marker back

Each of these should produce something worth seeing on its own terms —
an entity (`frances:v1/entities`, referenced by an `EntityRefSection`)
or a one-shot section — rather than reinstating a generic "a tool ran"
line. Sketches, roughly in order of how much they'd help:

- **`file_read`** — an entity per read: path + ranges in the snapshot,
  the file text as the opened tab. Repeated reads of the same path are
  the common case, so consider one entity per path that updates rather
  than one per call.
- **`file_find_or_grep`** — an entity holding the match list; inline
  shows `/pattern/ in <paths> — N matches`, the tab shows the matches.
  It already builds the structured result the tab would render.
- **`var_*`** — small enough for a one-shot section: `name = <type
  summary>`, which is what the handler already returns as the tool
  result content.
- **`shell_set` / `shell_get`** — same shape as the variable tools,
  and arguably they belong *on* the shell entity (an env-var facet)
  rather than as rows of their own.

Whatever the shape, it must satisfy the emptiness rule the registry now
enforces: a kind that registers `isEmpty` and reports true renders no
row (gutter included). A result row that would be blank should say so
in its snapshot instead of rendering an empty article.
