Read a file from disk.

Args: `{ path, into? }`

  path: file to read (absolute or relative to the client's working directory).
  into: optional Frances variable name. When set, the file's RAW bytes are stored into that variable instead of being printed — no anchors, and the read does NOT count as an "opened for editing" read. Use this when you want to pipe the file's contents into `variable_assign` (e.g. for fromjson, jq-introspection) or hand it to `shell_set`. Subsequent edits against the same `path` will still require a real `file_read` (without `into`) so the editor sees the registered baseline.

Without `into`, the file is rendered with line anchors. Each line is `Word§content` — a stable per-line anchor word (e.g. `Apple`, `BananaCarrot`), then `§`, then the line's content. Blank lines render as `Word§` with empty content. Anchors survive external edits and formatter runs. The rendered string of each line is exactly what you pass back as the `anchor` (and `end_anchor`) field of an `edit` call. Always call `file_read` (without `into`) for a path before any `file_replace_lines` / `file_replace_all` / `file_insert_*` / `file_overwrite` on it.
